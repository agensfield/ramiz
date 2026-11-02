#[path = "../src/fsclone.rs"]
#[allow(dead_code)]
mod fsclone;
#[path = "../src/inherit.rs"]
mod inherit;

use std::{
    collections::BTreeSet,
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

static FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "ramiz-inherit-{}-{}",
            std::process::id(),
            FIXTURE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::create_dir(path.join("donor")).unwrap();
        fs::create_dir(path.join("destination")).unwrap();
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn inherits_internal_hardlinks_and_isolates_donor() {
    let f = Fixture::new();
    let donor = f.0.join("donor");
    let destination = f.0.join("destination");
    fs::write(donor.join("a"), b"original").unwrap();
    fs::set_permissions(donor.join("a"), fs::Permissions::from_mode(0o6755)).unwrap();
    fs::hard_link(donor.join("a"), donor.join("b")).unwrap();
    symlink("a", donor.join("relative")).unwrap();
    fs::write(donor.join(".git"), b"excluded").unwrap();
    let snapshot = inherit::snapshot(&donor).unwrap();
    let target_symlinks = BTreeSet::new();
    let report =
        inherit::materialize(&snapshot, &destination, &target_symlinks, true, false).unwrap();
    inherit::finalize_directories(&snapshot, &destination, &target_symlinks).unwrap();
    assert_eq!(report.hardlinks, 1);
    assert_eq!(
        fs::metadata(destination.join("a")).unwrap().ino(),
        fs::metadata(destination.join("b")).unwrap().ino()
    );
    assert_ne!(
        fs::metadata(destination.join("a")).unwrap().ino(),
        fs::metadata(donor.join("a")).unwrap().ino()
    );
    assert_eq!(
        fs::metadata(destination.join("a")).unwrap().mode() & 0o7777,
        0o755
    );
    assert_eq!(
        fs::read_link(destination.join("relative")).unwrap(),
        PathBuf::from("a")
    );
    assert!(!destination.join(".git").exists());
    fs::write(destination.join("a"), b"destination").unwrap();
    assert_eq!(fs::read(destination.join("b")).unwrap(), b"destination");
    assert_eq!(fs::read(donor.join("a")).unwrap(), b"original");
    fs::write(donor.join("a"), b"donor changed").unwrap();
    assert!(inherit::verify_unchanged(&snapshot).is_err());
    assert_eq!(fs::read(destination.join("a")).unwrap(), b"destination");
}

#[test]
fn rejects_nested_repository_and_indirect_donor_link_before_copy() {
    let f = Fixture::new();
    let donor = f.0.join("donor");
    let destination = f.0.join("destination");
    fs::create_dir(donor.join("nested")).unwrap();
    fs::write(donor.join("nested/.git"), b"gitdir: elsewhere").unwrap();
    assert!(
        inherit::snapshot(&donor)
            .unwrap_err()
            .contains("nested repository")
    );
    fs::remove_file(donor.join("nested/.git")).unwrap();
    symlink(&donor, donor.join("bridge")).unwrap();
    symlink("bridge/missing", donor.join("indirect")).unwrap();
    let snapshot = inherit::snapshot(&donor).unwrap();
    let error =
        inherit::materialize(&snapshot, &destination, &BTreeSet::new(), true, false).unwrap_err();
    assert!(error.contains("points into donor"));
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
}

#[cfg(target_os = "macos")]
#[test]
fn preserves_existing_directory_backup_marker_without_inventing_policy() {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let fixture = Fixture::new();
    let donor = fixture.0.join("donor");
    let destination = fixture.0.join("destination");
    fs::create_dir(donor.join("marked")).unwrap();
    fs::create_dir(donor.join("ordinary")).unwrap();
    fs::write(donor.join("marked/file"), b"unique data").unwrap();
    let marker = c"com.apple.metadata:com_apple_backup_excludeItem";
    let source = CString::new(donor.join("marked").as_os_str().as_bytes()).unwrap();
    let value = b"existing-donor-intent";
    assert_eq!(
        unsafe {
            libc::setxattr(
                source.as_ptr(),
                marker.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
                libc::XATTR_NOFOLLOW,
            )
        },
        0
    );
    let snapshot = inherit::snapshot(&donor).unwrap();
    inherit::materialize(&snapshot, &destination, &BTreeSet::new(), true, false).unwrap();
    inherit::verify_unchanged(&snapshot).unwrap();
    let target = CString::new(destination.join("marked").as_os_str().as_bytes()).unwrap();
    let mut output = [0u8; 64];
    let size = unsafe {
        libc::getxattr(
            target.as_ptr(),
            marker.as_ptr(),
            output.as_mut_ptr().cast(),
            output.len(),
            0,
            libc::XATTR_NOFOLLOW,
        )
    };
    assert_eq!(size as usize, value.len());
    assert_eq!(&output[..size as usize], value);
    let ordinary = CString::new(destination.join("ordinary").as_os_str().as_bytes()).unwrap();
    assert_eq!(
        unsafe {
            libc::getxattr(
                ordinary.as_ptr(),
                marker.as_ptr(),
                std::ptr::null_mut(),
                0,
                0,
                libc::XATTR_NOFOLLOW,
            )
        },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ENOATTR)
    );
    assert_eq!(
        fs::read(destination.join("marked/file")).unwrap(),
        b"unique data"
    );
}
