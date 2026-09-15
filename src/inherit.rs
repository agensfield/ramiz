//! Complete filesystem inheritance. The caller owns registration and rollback.
//!
//! Before `snapshot`, pin donor HEAD and capture the donor index/Git state with
//! GIT_OPTIONAL_LOCKS=0. Refuse actual sparse/split indexes or normalize a private
//! copy with Git, never the donor index. Copy that stable index to the destination
//! and disable/rebuild fsmonitor, untracked and stat caches with Git before use.
//! Compare HEAD/index/Git state again alongside `verify_unchanged` before hooks.
//! Tracked target symlinks are excluded here: materialize them using Git afterward.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::CString,
    fs::{self, Metadata, OpenOptions},
    io,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, PermissionsExt, symlink},
    },
    path::{Component, Path, PathBuf},
};

use crate::fsclone;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    dev: u64,
    ino: u64,
    mode: u32,
    size: u64,
    links: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    target: Option<PathBuf>,
}

impl Entry {
    fn read(path: &Path) -> io::Result<Self> {
        let m = fs::symlink_metadata(path)?;
        Ok(Self {
            dev: m.dev(),
            ino: m.ino(),
            mode: m.mode(),
            size: m.size(),
            links: m.nlink(),
            mtime: (m.mtime(), m.mtime_nsec()),
            ctime: (m.ctime(), m.ctime_nsec()),
            target: if m.file_type().is_symlink() {
                Some(fs::read_link(path)?)
            } else {
                None
            },
        })
    }
}

/// Metadata-only mutation detection, deliberately not a transactional snapshot.
#[derive(Debug)]
pub struct Snapshot {
    donor: PathBuf,
    entries: BTreeMap<PathBuf, Entry>,
}

#[derive(Debug, Default)]
pub struct Report {
    pub cow_files: usize,
    pub copied_files: usize,
    pub hardlinks: usize,
    pub skipped_special: Vec<PathBuf>,
    pub symlinks: Vec<PathBuf>,
    pub donor_links: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

pub fn snapshot(donor: &Path) -> Result<Snapshot, String> {
    let donor = fs::canonicalize(donor).map_err(|e| e.to_string())?;
    let mut entries = BTreeMap::new();
    scan(&donor, Path::new(""), &mut entries).map_err(|e| e.to_string())?;
    Ok(Snapshot { donor, entries })
}

fn scan(root: &Path, relative: &Path, entries: &mut BTreeMap<PathBuf, Entry>) -> io::Result<()> {
    let path = root.join(relative);
    let entry = Entry::read(&path)?;
    let directory = entry.mode as libc::mode_t & libc::S_IFMT == libc::S_IFDIR;
    entries.insert(relative.to_owned(), entry);
    if directory {
        if !relative.as_os_str().is_empty()
            && (fs::symlink_metadata(path.join(".git")).is_ok()
                || (path.join("HEAD").is_file()
                    && path.join("objects").is_dir()
                    && path.join("refs").is_dir()))
        {
            return Err(io::Error::other(format!(
                "nested repository: {}",
                relative.display()
            )));
        }
        for child in fs::read_dir(path)? {
            let child = child?;
            if relative.as_os_str().is_empty() && child.file_name() == ".git" {
                continue;
            }
            scan(root, &relative.join(child.file_name()), entries)?;
        }
    }
    Ok(())
}

pub fn verify_unchanged(before: &Snapshot) -> Result<(), String> {
    let after = snapshot(&before.donor)?;
    if before.entries != after.entries {
        return Err("donor filesystem changed during inheritance".into());
    }
    Ok(())
}

/// Destination must be newly registered and contain only its Git-created .git.
/// Any error requires caller-owned rollback. `target_symlinks` are raw relative
/// target-tree paths, not a lossy UTF-8 listing or a donor index interpretation.
pub fn materialize(
    before: &Snapshot,
    destination: &Path,
    target_symlinks: &BTreeSet<PathBuf>,
    allow_copy: bool,
    allow_donor_links: bool,
) -> Result<Report, String> {
    let destination = fs::canonicalize(destination).map_err(|e| e.to_string())?;
    if destination.starts_with(&before.donor) || before.donor.starts_with(&destination) {
        return Err("inheritance destination and donor must be disjoint".into());
    }
    for item in fs::read_dir(&destination).map_err(|e| e.to_string())? {
        if item.map_err(|e| e.to_string())?.file_name() != ".git" {
            return Err("inheritance destination is not empty apart from .git".into());
        }
    }
    // Check all inherited symlinks before any materialization. Resolve relative
    // links as they will behave at the destination, not in their original tree.
    let mut report = Report::default();
    for (relative, entry) in &before.entries {
        if target_symlinks.contains(relative) {
            continue;
        }
        if let Some(raw) = &entry.target {
            let target = destination.join(relative);
            let resolved = resolve_link_target(
                target.parent().unwrap().join(raw),
                &destination,
                &before.donor,
            )
            .map_err(|e| format!("resolve symlink {}: {e}", relative.display()))?;
            if resolved.starts_with(&before.donor) {
                if !allow_donor_links {
                    return Err(format!(
                        "inherited symlink points into donor: {}",
                        relative.display()
                    ));
                }
                report.donor_links.push(relative.clone());
            }
        }
    }
    let mut hardlinks = BTreeMap::<(u64, u64), PathBuf>::new();
    for (relative, entry) in &before.entries {
        if relative.as_os_str().is_empty()
            || target_symlinks
                .iter()
                .any(|link| relative.starts_with(link))
        {
            continue;
        }
        let source = before.donor.join(relative);
        let target = destination.join(relative);
        if Entry::read(&source).map_err(|e| e.to_string())? != *entry {
            return Err(format!(
                "donor changed before copying {}",
                relative.display()
            ));
        }
        match entry.mode as libc::mode_t & libc::S_IFMT {
            libc::S_IFDIR => fs::create_dir(&target).map_err(|e| e.to_string())?,
            libc::S_IFREG => {
                if let Some(first) = hardlinks.get(&(entry.dev, entry.ino)) {
                    fs::hard_link(first, &target).map_err(|e| e.to_string())?;
                    report.hardlinks += 1;
                } else {
                    match fsclone::clone_file(&source, &target) {
                        Ok(_) => report.cow_files += 1,
                        Err(error) if allow_copy => {
                            // Never overwrite an unexplained partial clone.
                            let mut output = OpenOptions::new()
                                .write(true)
                                .create_new(true)
                                .open(&target)
                                .map_err(|e| format!("{error}; copy: {e}"))?;
                            let mut input = fs::File::open(&source).map_err(|e| e.to_string())?;
                            io::copy(&mut input, &mut output).map_err(|e| e.to_string())?;
                            report.copied_files += 1;
                            report.warnings.push(format!(
                                "physical copy {}: sparse layout and extended metadata may differ",
                                relative.display()
                            ));
                        }
                        Err(error) => return Err(error),
                    }
                    hardlinks.insert((entry.dev, entry.ino), target.clone());
                }
                let metadata = fs::symlink_metadata(&source).map_err(|e| e.to_string())?;
                preserve_metadata(&target, &metadata, false).map_err(|e| e.to_string())?;
            }
            libc::S_IFLNK => {
                symlink(entry.target.as_ref().unwrap(), &target).map_err(|e| e.to_string())?;
                let metadata = fs::symlink_metadata(&source).map_err(|e| e.to_string())?;
                preserve_metadata(&target, &metadata, true).map_err(|e| e.to_string())?;
                report.symlinks.push(relative.clone());
            }
            _ => report.skipped_special.push(relative.clone()),
        }
    }
    report.warnings.push(
        "directory/symlink ACLs and xattrs are not copied; platform metadata may differ".into(),
    );
    #[cfg(target_os = "linux")]
    report
        .warnings
        .push("FICLONE preserves data extents only; file ACLs and xattrs are not copied".into());
    verify_unchanged(before)?;
    Ok(report)
}

/// Finalize directories only after the caller has materialized target-owned
/// symlinks. This preserves read-only modes and donor timestamps without
/// letting later entry creation disturb them.
pub fn finalize_directories(
    before: &Snapshot,
    destination: &Path,
    target_symlinks: &BTreeSet<PathBuf>,
) -> Result<(), String> {
    for (relative, entry) in before.entries.iter().rev() {
        if entry.mode as libc::mode_t & libc::S_IFMT == libc::S_IFDIR
            && !target_symlinks
                .iter()
                .any(|link| relative.starts_with(link))
        {
            let metadata =
                fs::symlink_metadata(before.donor.join(relative)).map_err(|e| e.to_string())?;
            preserve_metadata(&destination.join(relative), &metadata, false)
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn preserve_metadata(target: &Path, metadata: &Metadata, link: bool) -> io::Result<()> {
    if !link {
        fs::set_permissions(target, fs::Permissions::from_mode(metadata.mode() & 0o1777))?;
    }
    let times = [
        libc::timespec {
            tv_sec: metadata.atime(),
            tv_nsec: metadata.atime_nsec() as _,
        },
        libc::timespec {
            tv_sec: metadata.mtime(),
            tv_nsec: metadata.mtime_nsec() as _,
        },
    ];
    let path = CString::new(target.as_os_str().as_bytes())?;
    // SAFETY: path and the two-element times array remain valid for this call.
    if unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            path.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// Canonicalize existing prefixes, retaining dangling suffixes. Resolves each
// symlink before processing '..', unlike lexical normalization of the whole path.
fn resolve_link_target(path: PathBuf, destination: &Path, donor: &Path) -> io::Result<PathBuf> {
    fn walk(
        path: &Path,
        budget: &mut usize,
        destination: &Path,
        donor: &Path,
    ) -> io::Result<PathBuf> {
        let mut resolved = PathBuf::new();
        for component in path.components() {
            match component {
                Component::RootDir => resolved.push("/"),
                Component::CurDir => (),
                Component::ParentDir => {
                    resolved.pop();
                }
                Component::Normal(part) => {
                    resolved.push(part);
                    let lookup = match resolved.strip_prefix(destination) {
                        Ok(relative) => donor.join(relative),
                        Err(_) => resolved.clone(),
                    };
                    match fs::symlink_metadata(&lookup) {
                        Ok(m) if m.file_type().is_symlink() => {
                            if *budget == 0 {
                                return Err(io::Error::other("symlink resolution loop"));
                            }
                            *budget -= 1;
                            let raw = fs::read_link(&lookup)?;
                            resolved.pop();
                            resolved = walk(&resolved.join(raw), budget, destination, donor)?;
                        }
                        Ok(_) => (),
                        Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                        Err(e) => return Err(e),
                    }
                }
                Component::Prefix(_) => unreachable!("Unix paths"),
            }
        }
        Ok(resolved)
    }
    walk(&path, &mut 40, destination, donor)
}
