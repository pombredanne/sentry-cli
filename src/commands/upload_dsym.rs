//! Implements a command for uploading dsym files.
use std::fs;
use std::env;
use std::path::{Path, PathBuf};
use std::fs::File;
use std::io::{Write, Seek};
use std::ffi::OsStr;
use std::cell::RefCell;
use std::iter::Fuse;
use std::rc::Rc;
use std::collections::HashSet;

use clap::{App, Arg, ArgMatches};
use walkdir::{WalkDir, Iter as WalkDirIter};
use zip;
use uuid::Uuid;
use indicatif::{ProgressBar, ProgressStyle, style};

use prelude::*;
use api::{Api, DSymFile};
use utils::{ArgExt, TempFile, get_sha1_checksum,
            is_zip_file, validate_uuid, copy_with_progress,
            make_byte_progress_bar};
use config::Config;
use xcode;
use macho;

const BATCH_SIZE: usize = 12;

#[derive(Debug)]
enum DSymVar {
    FsFile(PathBuf),
    ZipFile(Rc<RefCell<Option<zip::ZipArchive<fs::File>>>>, usize),
}

#[derive(Debug)]
struct DSymRef {
    var: DSymVar,
    arc_name: String,
    checksum: String,
    size: u64,
    uuids: HashSet<Uuid>,
}

impl DSymRef {
    pub fn add_to_archive<W: Write + Seek>(&self, mut zip: &mut zip::ZipWriter<W>,
                                           pb: &ProgressBar) -> Result<()> {
        zip.start_file(self.arc_name.clone(), zip::write::FileOptions::default())?;
        match self.var {
            DSymVar::FsFile(ref p) => {
                copy_with_progress(pb, &mut File::open(&p)?, &mut zip)?;
            }
            DSymVar::ZipFile(ref rc, idx) => {
                let rc = rc.clone();
                let mut opt_archive = rc.borrow_mut();
                if let Some(ref mut archive) = *opt_archive {
                    let mut af = archive.by_index(idx)?;
                    copy_with_progress(pb, &mut af, &mut zip)?;
                } else {
                    panic!("zip file went away");
                }
            }
        }
        Ok(())
    }
}

struct BatchIter<'a> {
    path: PathBuf,
    wd_iter: Fuse<WalkDirIter>,
    open_zip: Rc<RefCell<Option<zip::ZipArchive<fs::File>>>>,
    open_zip_index: usize,
    uuids: Option<&'a HashSet<Uuid>>,
    allow_zips: bool,
    found_uuids: RefCell<&'a mut HashSet<Uuid>>,
}

impl<'a> BatchIter<'a> {
    pub fn new<P: AsRef<Path>>(path: P, uuids: Option<&'a HashSet<Uuid>>,
                               allow_zips: bool, found_uuids: &'a mut HashSet<Uuid>)
        -> BatchIter<'a>
    {
        BatchIter {
            path: path.as_ref().to_path_buf(),
            wd_iter: WalkDir::new(&path).into_iter().fuse(),
            open_zip: Rc::new(RefCell::new(None)),
            open_zip_index: !0,
            uuids: uuids,
            allow_zips: allow_zips,
            found_uuids: RefCell::new(found_uuids),
        }
    }

    fn found_all(&self) -> bool {
        if let Some(ref uuids) = self.uuids {
            self.found_uuids.borrow().is_superset(uuids)
        } else {
            false
        }
    }

    fn push_ref(&self, batch: &mut Vec<DSymRef>, dsym_ref: DSymRef) -> bool {
        let mut found_uuids = self.found_uuids.borrow_mut();
        let mut should_push = false;
        for uuid in &dsym_ref.uuids {
            if found_uuids.contains(uuid) {
                continue;
            }
            should_push = true;
            found_uuids.insert(*uuid);
        }
        if should_push {
            batch.push(dsym_ref);
        }
        batch.len() >= BATCH_SIZE
    }
}

impl<'a> Iterator for BatchIter<'a> {
    type Item = Result<Vec<DSymRef>>;

    fn next(&mut self) -> Option<Result<Vec<DSymRef>>> {
        let pb = ProgressBar::new_spinner();
        pb.set_style(ProgressStyle::default_spinner()
            .tick_chars("/|\\- ")
            .template("{spinner} Looking for symbols... {msg:.dim}"));

        let mut batch = vec![];

        macro_rules! uuid_match {
            ($load:expr) => {
                match $load {
                    Ok(uuids) => {
                        if let Some(ref expected_uuids) = self.uuids {
                            if !uuids.is_disjoint(expected_uuids) {
                                Some(uuids)
                            } else {
                                None
                            }
                        } else if !uuids.is_empty() {
                            Some(uuids)
                        } else {
                            None
                        }
                    }
                    Err(err) => {
                        if let &ErrorKind::NoMacho = err.kind() {
                            None
                        } else {
                            return Some(Err(err));
                        }
                    }
                }
            }
        }

        let mut show_zip_continue = true;
        while !self.found_all() {
            if self.open_zip_index == !0 {
                *self.open_zip.borrow_mut() = None;
            }

            if self.open_zip_index != !0 {
                let mut archive_ptr = self.open_zip.borrow_mut();
                let mut archive = archive_ptr.as_mut().unwrap();
                if show_zip_continue {
                    show_zip_continue = false;
                }
                if self.open_zip_index >= archive.len() {
                    self.open_zip_index = !0;
                    if batch.len() != 0 {
                        break;
                    }
                } else {
                    if let Some(uuids) = uuid_match!(macho::get_uuids_for_reader(
                            iter_try!(archive.by_index(self.open_zip_index))))
                    {
                        let mut f = iter_try!(archive.by_index(self.open_zip_index));
                        let name = Path::new("DebugSymbols").join(f.name());
                        if self.push_ref(&mut batch, DSymRef {
                            var: DSymVar::ZipFile(self.open_zip.clone(), self.open_zip_index),
                            arc_name: name.to_string_lossy().into_owned(),
                            checksum: iter_try!(get_sha1_checksum(&mut f)),
                            size: f.size(),
                            uuids: uuids,
                        }) {
                            break;
                        }
                    }
                    self.open_zip_index += 1;
                }
            } else if let Some(dent_res) = self.wd_iter.next() {
                let dent = iter_try!(dent_res);
                let md = iter_try!(dent.metadata());
                if md.is_file() {
                    if let Some(fname) = dent.path().file_name().and_then(|x| x.to_str()) {
                        pb.set_message(fname);
                    }
                    if self.allow_zips && is_zip_file(iter_try!(fs::File::open(&dent.path()))) {
                        show_zip_continue = false;
                        let f = iter_try!(fs::File::open(dent.path()));
                        if let Ok(archive) = zip::ZipArchive::new(f) {
                            *self.open_zip.borrow_mut() = Some(archive);
                            self.open_zip_index = 0;
                            // whenever we switch the zip we need to yield because we
                            // might have references to an earlier zip
                            if batch.len() > 0 {
                                break;
                            }
                        }
                    } else if let Some(uuids) = uuid_match!(macho::get_uuids_for_path(
                            dent.path())) {
                        let name = Path::new("DebugSymbols")
                            .join(dent.path().strip_prefix(&self.path).unwrap());
                        if self.push_ref(&mut batch, DSymRef {
                            var: DSymVar::FsFile(dent.path().to_path_buf()),
                            arc_name: name.to_string_lossy().into_owned(),
                            checksum: iter_try!(get_sha1_checksum(
                                &mut iter_try!(fs::File::open(dent.path())))),
                            size: md.len(),
                            uuids: uuids,
                        }) {
                            break;
                        }
                    }
                }
            } else {
                break;
            }
        }

        pb.finish_and_clear();
        if batch.len() == 0 {
            None
        } else {
            Some(Ok(batch))
        }
    }
}

fn find_missing_files(api: &mut Api,
                      refs: Vec<DSymRef>,
                      org: &str,
                      project: &str)
                      -> Result<Vec<DSymRef>> {
    debug!("Checking for missing debug symbols: {:#?}", &refs);
    let missing = {
        let checksums: Vec<_> = refs.iter().map(|ref x| x.checksum.as_str()).collect();
        api.find_missing_dsym_checksums(org, project, &checksums)?
    };
    let mut rv = vec![];
    for r in refs.into_iter() {
        if missing.contains(&r.checksum) {
            rv.push(r)
        }
    }
    debug!("Missing debug symbols: {:#?}", &rv);
    Ok(rv)
}

fn zip_up_missing(refs: &[DSymRef]) -> Result<TempFile> {
    println!("{} Compressing {} missing debug symbol files", style("[2/3]").dim(),
             style(refs.len()).yellow());
    let total_bytes = refs.iter().map(|x| x.size).sum();
    let pb = make_byte_progress_bar(total_bytes);
    let tf = TempFile::new()?;
    let mut zip = zip::ZipWriter::new(tf.open());
    for ref r in refs {
        r.add_to_archive(&mut zip, &pb)?;
    }
    pb.finish_and_clear();
    Ok(tf)
}

fn upload_dsyms(api: &mut Api,
                refs: &[DSymRef],
                org: &str,
                project: &str)
                -> Result<Vec<DSymFile>> {
    let tf = zip_up_missing(refs)?;
    println!("{} Uploading debug symbol files", style("[3/3]").dim());
    Ok(api.upload_dsyms(org, project, tf.path())?)
}

fn get_paths_from_env() -> Result<Vec<PathBuf>> {
    let mut rv = vec![];
    if let Some(base_path) = env::var_os("DWARF_DSYM_FOLDER_PATH") {
        debug!("Getting path from DWARF_DSYM_FOLDER_PATH: {}",
               Path::new(&base_path).display());
        for entry in WalkDir::new(base_path) {
            let entry = entry?;
            if entry.path().extension() == Some(OsStr::new("dSYM")) &&
               fs::metadata(entry.path())?.is_dir() {
                rv.push(entry.path().to_path_buf());
            }
        }
    }
    Ok(rv)
}

pub fn make_app<'a, 'b: 'a>(app: App<'a, 'b>) -> App<'a, 'b> {
    app.about("uploads debug symbols to a project")
        .org_project_args()
        .arg(Arg::with_name("paths")
            .value_name("PATH")
            .help("The path to the debug symbols")
            .multiple(true)
            .number_of_values(1)
            .index(1))
        .arg(Arg::with_name("uuids")
             .value_name("UUID")
             .long("uuid")
             .help("Finds debug symbols by UUID.")
             .validator(validate_uuid)
             .multiple(true)
             .number_of_values(1))
        .arg(Arg::with_name("require_all")
             .long("require-all")
             .help("When combined with --uuid this will error if not all \
                    UUIDs could be found."))
        .arg(Arg::with_name("derived_data")
             .long("derived-data")
             .help("Search for debug symbols in derived data."))
        .arg(Arg::with_name("no_zips")
             .long("no-zips")
             .help("Do not recursive into .zip files"))
        .arg(Arg::with_name("info_plist")
             .long("info-plist")
             .value_name("PATH")
             .help("Optional path to the Info.plist.  We will try to find this \
                    automatically if run from xcode.  Providing this information \
                    will associate the debug symbols with a specific ITC application \
                    and build in Sentry."))
        .arg(Arg::with_name("no_reprocessing")
             .long("no-reprocessing")
             .help("Does not trigger reprocessing after upload"))
        .arg(Arg::with_name("force_foreground")
             .long("force-foreground")
             .help("By default the upload process will when triggered from Xcode \
                    detach and continue in the background.  When an error happens \
                    a dialog is shown.  If this parameter is passed Xcode will wait \
                    for the process to finish before the build finishes and output \
                    will be shown in the Xcode build output."))
}

pub fn execute<'a>(matches: &ArgMatches<'a>, config: &Config) -> Result<()> {
    let zips = !matches.is_present("no_zips");
    let mut paths = match matches.values_of("paths") {
        Some(paths) => paths.map(|x| PathBuf::from(x)).collect(),
        None => get_paths_from_env()?,
    };
    if_chain! {
        if matches.is_present("derived_data");
        if let Some(path) = env::home_dir().map(|x| x.join("Library/Developer/Xcode/DerivedData"));
        if path.is_dir();
        then {
            paths = vec![path];
        }
    }
    let find_uuids = matches.values_of("uuids").map(|uuids| {
        uuids.map(|s| Uuid::parse_str(s).unwrap()).collect::<HashSet<_>>()
    });
    let mut found_uuids: HashSet<Uuid> = HashSet::new();
    let info_plist = match matches.value_of("info_plist") {
        Some(path) => Some(xcode::InfoPlist::from_path(path)?),
        None => xcode::InfoPlist::discover_from_env()?,
    };

    if paths.len() == 0 {
        println!("Warning: no paths were provided.");
    }

    let (org, project) = config.get_org_and_project(matches)?;
    let mut api = Api::new(config);
    let mut total_uploaded = 0;

    xcode::MayDetach::wrap("Debug symbol upload", |md| {
        // Optionally detach if run from xcode
        if !matches.is_present("force_foreground") {
            md.may_detach()?;
        }

        let mut batch_num = 0;
        let mut all_dsym_checksums = vec![];
        for path in paths.into_iter() {
            debug!("Scanning {}", path.display());
            for batch_res in BatchIter::new(path, find_uuids.as_ref(), zips,
                                            &mut found_uuids) {
                if batch_num > 0 {
                    println!("");
                }
                batch_num += 1;
                let batch = batch_res?;
                println!("{}", style(format!("Batch {}", batch_num)).bold());
                for dsym_ref in batch.iter() {
                    all_dsym_checksums.push(dsym_ref.checksum.clone());
                }
                println!("{} Found {} debug symbol files. Checking for missing symbols on server",
                         style("[1/3]").dim(), style(batch.len()).yellow());
                let missing = find_missing_files(&mut api, batch, &org, &project)?;
                if missing.len() == 0 {
                    println!("{} Nothing to compress, all symbols are on the server",
                             style("[2/3]").dim());
                    println!("{} Nothing to upload", style("[3/3]").dim());
                    continue;
                }
                let rv = upload_dsyms(&mut api, &missing, &org, &project)?;
                if rv.len() > 0 {
                    total_uploaded += rv.len();
                    println!("Newly uploaded debug symbols:");
                    for df in rv {
                        println!("  {} ({}; {})",
                                 style(&df.uuid).dim(),
                                 &df.object_name,
                                 df.cpu_name);
                    }
                }
            }
        }

        // associate the dsyms with the info plist data if available
        if let Some(ref info_plist) = info_plist {
            println!("Associating dsyms with {}", info_plist);
            match api.associate_dsyms(&org, &project, info_plist, all_dsym_checksums)? {
                None => {
                    println!("Server does not support dsym associations. Ignoring.");
                }
                Some(resp) => {
                    if resp.associated_dsyms.len() == 0 {
                        println!("No new debug symbols to associate.");
                    } else {
                        println!("Associated {} debug symbols with the build.",
                                 style(resp.associated_dsyms.len()).yellow());
                    }
                }
            }
        }

        if total_uploaded > 0 {
            println!("Uploaded a total of {} debug symbols",
                     style(total_uploaded).yellow());
        }

        // If wanted trigger reprocessing
        if !matches.is_present("no_reprocessing") {
            if !api.trigger_reprocessing(&org, &project)? {
                println!("Server does not support reprocessing. Not triggering.");
            }
        } else {
            println!("Skipped reprocessing.");
        }

        // did we miss anything?
        if let Some(ref find_uuids) = find_uuids {
            let missing: HashSet<_> = find_uuids.difference(&found_uuids).collect();
            if matches.is_present("require_all") && !missing.is_empty() {
                println!("");
                println_stderr!("{}", style("error: not all requested dsyms could be found.").red());
                println_stderr!("The following symbols are still missing:");
                for uuid in &missing {
                    println!("  {}", uuid);
                }
                return Err(ErrorKind::QuietExit(1).into());
            }
        }

        Ok(())
    })
}
