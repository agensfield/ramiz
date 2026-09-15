use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(target_os = "macos")]
use std::ffi::CString;
#[cfg(target_os = "linux")]
use std::fs::File;

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    #[cfg(target_os = "macos")]
    Apfs,
    #[cfg(target_os = "linux")]
    LinuxReflink,
}

impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            #[cfg(target_os = "macos")]
            Self::Apfs => "apfs",
            #[cfg(target_os = "linux")]
            Self::LinuxReflink => "linux-reflink",
        }
    }
}

pub fn probe(directory: &Path) -> Result<Backend, String> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    let source = directory.join(format!(
        ".ramiz-probe-{}-{nonce}.source",
        std::process::id()
    ));
    let target = directory.join(format!(
        ".ramiz-probe-{}-{nonce}.target",
        std::process::id()
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&source)
            .map_err(|error| format!("create probe in {}: {error}", directory.display()))?;
        file.write_all(b"ramiz-cow-probe")
            .map_err(|error| format!("write probe: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("sync probe: {error}"))?;
        clone_file(&source, &target)
    })();
    let mut cleanup = Vec::new();
    for path in [&target, &source] {
        if let Err(error) = fs::remove_file(path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                cleanup.push(format!("remove {}: {error}", path.display()));
            }
        }
    }
    if cleanup.is_empty() {
        result
    } else {
        Err(format!("probe cleanup failed: {}", cleanup.join("; ")))
    }
}

pub fn clone_file(source: &Path, target: &Path) -> Result<Backend, String> {
    #[cfg(target_os = "macos")]
    {
        clone_file_macos(source, target)?;
        return Ok(Backend::Apfs);
    }
    #[cfg(target_os = "linux")]
    {
        clone_file_linux(source, target)?;
        return Ok(Backend::LinuxReflink);
    }
    #[allow(unreachable_code)]
    Err("copy-on-write cloning is unsupported on this platform".into())
}

pub fn normalize_clean_file(path: &Path, executable: bool) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    remove_xattrs(path)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        // Ramiz is single-threaded while materializing. Read and immediately
        // restore the process umask so retained files get ordinary checkout
        // permissions instead of donor permissions.
        let mask = unsafe { libc::umask(0) };
        unsafe { libc::umask(mask) };
        let requested = if executable { 0o777 } else { 0o666 };
        fs::set_permissions(path, fs::Permissions::from_mode(requested & !(mask as u32)))
            .map_err(|error| format!("normalize permissions on {}: {error}", path.display()))?;
    }

    let now = SystemTime::now();
    fs::File::open(path)
        .and_then(|file| file.set_times(fs::FileTimes::new().set_accessed(now).set_modified(now)))
        .map_err(|error| format!("normalize timestamps on {}: {error}", path.display()))?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn remove_xattrs(path: &Path) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;

    unsafe extern "C" {
        fn listxattr(
            path: *const libc::c_char,
            namebuf: *mut libc::c_char,
            size: libc::size_t,
            options: libc::c_int,
        ) -> libc::ssize_t;
        fn removexattr(
            path: *const libc::c_char,
            name: *const libc::c_char,
            options: libc::c_int,
        ) -> libc::c_int;
    }

    let path_c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| format!("path contains NUL: {}", path.display()))?;
    // SAFETY: path_c is valid for each call and no pointer is retained.
    let size = unsafe { listxattr(path_c.as_ptr(), std::ptr::null_mut(), 0, 0) };
    if size < 0 {
        return Err(format!(
            "list xattrs on {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    if size == 0 {
        return Ok(());
    }
    let mut names = vec![0u8; size as usize];
    // SAFETY: names has exactly the advertised capacity and path_c remains valid.
    let read = unsafe { listxattr(path_c.as_ptr(), names.as_mut_ptr().cast(), names.len(), 0) };
    if read < 0 {
        return Err(format!(
            "read xattrs on {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    names.truncate(read as usize);
    for name in names
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        let name_c = CString::new(name).expect("xattr names cannot contain interior NUL bytes");
        // SAFETY: both C strings outlive the call and no pointer is retained.
        if unsafe { removexattr(path_c.as_ptr(), name_c.as_ptr(), 0) } != 0 {
            return Err(format!(
                "remove xattr {:?} from {}: {}",
                String::from_utf8_lossy(name),
                path.display(),
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn clone_file_macos(source: &Path, target: &Path) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;

    unsafe extern "C" {
        fn clonefile(
            source: *const libc::c_char,
            target: *const libc::c_char,
            flags: libc::c_int,
        ) -> libc::c_int;
    }

    const CLONE_NOOWNERCOPY: libc::c_int = 0x0002;
    let source_c = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| "source path contains NUL".to_string())?;
    let target_c = CString::new(target.as_os_str().as_bytes())
        .map_err(|_| "target path contains NUL".to_string())?;
    // SAFETY: both C strings outlive the call and clonefile retains neither pointer.
    let result = unsafe { clonefile(source_c.as_ptr(), target_c.as_ptr(), CLONE_NOOWNERCOPY) };
    if result == 0 {
        Ok(())
    } else {
        Err(format!(
            "clonefile {} -> {}: {}",
            source.display(),
            target.display(),
            std::io::Error::last_os_error()
        ))
    }
}

#[cfg(target_os = "linux")]
fn clone_file_linux(source: &Path, target: &Path) -> Result<(), String> {
    use std::os::fd::AsRawFd;

    const FICLONE: libc::Ioctl = 0x4004_9409;
    let source_file =
        File::open(source).map_err(|error| format!("open {}: {error}", source.display()))?;
    let target_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(target)
        .map_err(|error| format!("create {}: {error}", target.display()))?;
    // SAFETY: both descriptors are valid for the duration of ioctl.
    let result = unsafe { libc::ioctl(target_file.as_raw_fd(), FICLONE, source_file.as_raw_fd()) };
    if result == 0 {
        Ok(())
    } else {
        let error = std::io::Error::last_os_error();
        let _ = fs::remove_file(target);
        Err(format!(
            "FICLONE {} -> {}: {error}",
            source.display(),
            target.display()
        ))
    }
}

pub fn existing_directory(path: &Path) -> Result<PathBuf, String> {
    let mut candidate = path;
    loop {
        match fs::canonicalize(candidate) {
            Ok(path) if path.is_dir() => return Ok(path),
            Ok(_) => return Err(format!("{} is not a directory", candidate.display())),
            Err(_) => {
                candidate = candidate
                    .parent()
                    .ok_or_else(|| format!("no existing parent for {}", path.display()))?;
            }
        }
    }
}
