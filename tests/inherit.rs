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
