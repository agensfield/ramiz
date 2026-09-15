#![cfg(unix)]

use std::{
    fs,
    os::unix::{
        fs::{MetadataExt, PermissionsExt},
        net::UnixListener,
    },
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};

static FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "ramiz-inherit-add-{}-{}",
            std::process::id(),
            FIXTURE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn git(cwd: &Path, args: &[&str]) -> Output {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn git_bytes(cwd: &Path, args: &[&str]) -> Vec<u8> {
    git(cwd, args).stdout
}

fn ramiz(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ramiz"))
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap()
}

fn init_repo(root: &Path) -> PathBuf {
    let repo = root.join("repo");
    git(root, &["init", "-q", "-b", "main", repo.to_str().unwrap()]);
    git(&repo, &["config", "user.name", "Ramiz Test"]);
    git(&repo, &["config", "user.email", "ramiz@example.invalid"]);
    repo
}

#[test]
fn inheritance_preserves_head_index_worktree_and_ignored_state() {
    let fixture = Fixture::new();
    let repo = init_repo(&fixture.0);
    fs::write(repo.join(".gitignore"), "build/\ncache-*\nlive.sock\n").unwrap();
    fs::write(repo.join("abc.txt"), "A\n").unwrap();
    fs::write(repo.join("deleted.txt"), "tracked\n").unwrap();
    fs::write(repo.join("tracked-target"), "target\n").unwrap();
    std::os::unix::fs::symlink("tracked-target", repo.join("tracked-link")).unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "initial"]);

    fs::write(repo.join("abc.txt"), "B\n").unwrap();
    git(&repo, &["add", "abc.txt"]);
    fs::write(repo.join("abc.txt"), "C\n").unwrap();
    git(&repo, &["rm", "-q", "deleted.txt"]);
    fs::write(repo.join("deleted.txt"), "recreated untracked\n").unwrap();
    fs::write(repo.join("intent.txt"), "intent\n").unwrap();
    git(&repo, &["add", "-N", "intent.txt"]);
    fs::write(repo.join("untracked.txt"), "untracked\n").unwrap();
    fs::create_dir(repo.join("build")).unwrap();
    fs::write(repo.join("build/out"), "ignored\n").unwrap();
    fs::write(repo.join("cache-a"), "cache\n").unwrap();
    fs::hard_link(repo.join("cache-a"), repo.join("cache-b")).unwrap();
    std::os::unix::fs::symlink("untracked.txt", repo.join("relative-link")).unwrap();
    let _socket = UnixListener::bind(repo.join("live.sock")).unwrap();

    let index_path = PathBuf::from(
        String::from_utf8(git_bytes(
            &repo,
            &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        ))
        .unwrap()
        .trim(),
    );
    let index_before = fs::read(&index_path).unwrap();
    let status_before = git_bytes(
        &repo,
        &["status", "--porcelain=v2", "-z", "--untracked-files=all"],
    );

    let destination = fixture.0.join("worktree");
    let output = ramiz(
        &repo,
        &[
            "add",
            "--inherit",
            "--json",
            "-b",
            "inherited",
            destination.to_str().unwrap(),
        ],
    );
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["schema"], "ramiz.cli/v1");
    assert_eq!(envelope["command"], "add");
    assert!(envelope["data"]["cow_files"].as_u64().unwrap() >= 1);
    assert_eq!(envelope["data"]["hardlinks"], 1);
    assert!(
        envelope["data"]["skipped_special"]
            .as_array()
            .unwrap()
            .iter()
            .any(|path| path["display"] == "live.sock")
    );

    assert_eq!(fs::read(destination.join("abc.txt")).unwrap(), b"C\n");
    assert_eq!(git_bytes(&destination, &["show", ":abc.txt"]), b"B\n");
    assert_eq!(
        fs::read(destination.join("deleted.txt")).unwrap(),
        b"recreated untracked\n"
    );
    assert_eq!(
        fs::read(destination.join("build/out")).unwrap(),
        b"ignored\n"
    );
    assert_eq!(
        fs::metadata(destination.join("cache-a")).unwrap().ino(),
        fs::metadata(destination.join("cache-b")).unwrap().ino()
    );
    assert_ne!(
        fs::metadata(destination.join("cache-a")).unwrap().ino(),
        fs::metadata(repo.join("cache-a")).unwrap().ino()
    );
    assert_eq!(
        fs::read_link(destination.join("tracked-link")).unwrap(),
        Path::new("tracked-target")
    );
    assert_eq!(
        fs::read_link(destination.join("relative-link")).unwrap(),
        Path::new("untracked.txt")
    );
    assert!(!destination.join("live.sock").exists());
    assert_eq!(
        git_bytes(
            &destination,
            &["status", "--porcelain=v2", "-z", "--untracked-files=all"],
        ),
        status_before
    );
    assert_eq!(fs::read(&index_path).unwrap(), index_before);

    fs::write(destination.join("build/out"), "destination\n").unwrap();
    assert_eq!(fs::read(repo.join("build/out")).unwrap(), b"ignored\n");
}

#[test]
fn inheritance_target_mismatch_refuses_before_branch_or_destination_creation() {
    let fixture = Fixture::new();
    let repo = init_repo(&fixture.0);
    fs::write(repo.join("file"), "one\n").unwrap();
    git(&repo, &["add", "file"]);
    git(&repo, &["commit", "-qm", "one"]);
    fs::write(repo.join("file"), "two\n").unwrap();
    git(&repo, &["commit", "-qam", "two"]);
    let destination = fixture.0.join("worktree");
    let output = ramiz(
        &repo,
        &[
            "add",
            "--inherit",
            "--json",
            "-b",
            "wrong-target",
            destination.to_str().unwrap(),
            "HEAD~1",
        ],
    );
    assert!(!output.status.success());
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["error"]["code"], "inheritance_target_mismatch");
    assert!(!destination.exists());
    let branch = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["show-ref", "--verify", "--quiet", "refs/heads/wrong-target"])
        .status()
        .unwrap();
    assert!(!branch.success());
}

#[test]
fn inheritance_preserves_unmerged_index_stages_and_conflict_bytes() {
    let fixture = Fixture::new();
    let repo = init_repo(&fixture.0);
    fs::write(repo.join("conflict.txt"), "base\n").unwrap();
    git(&repo, &["add", "conflict.txt"]);
    git(&repo, &["commit", "-qm", "base"]);
    git(&repo, &["checkout", "-qb", "other"]);
    fs::write(repo.join("conflict.txt"), "other\n").unwrap();
    git(&repo, &["commit", "-qam", "other"]);
    git(&repo, &["checkout", "-q", "main"]);
    fs::write(repo.join("conflict.txt"), "main\n").unwrap();
    git(&repo, &["commit", "-qam", "main"]);
    let merge = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["merge", "other"])
        .output()
        .unwrap();
    assert!(!merge.status.success());
    let stages_before = git_bytes(&repo, &["ls-files", "--stage", "-z"]);
    let conflict_before = fs::read(repo.join("conflict.txt")).unwrap();

    let destination = fixture.0.join("worktree");
    let output = ramiz(
        &repo,
        &[
            "add",
            "--inherit",
            "--json",
            "--detach",
            destination.to_str().unwrap(),
            "HEAD",
        ],
    );
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        git_bytes(&destination, &["ls-files", "--stage", "-z"]),
        stages_before
    );
    assert_eq!(
        fs::read(destination.join("conflict.txt")).unwrap(),
        conflict_before
    );
}

#[test]
fn tracked_symlink_refuses_inherited_symlink_ancestor_without_outside_write() {
    let fixture = Fixture::new();
    let repo = init_repo(&fixture.0);
    fs::create_dir(repo.join("dir")).unwrap();
    fs::write(repo.join("target"), "target\n").unwrap();
    std::os::unix::fs::symlink("../target", repo.join("dir/link")).unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "tracked link"]);
    fs::remove_file(repo.join("dir/link")).unwrap();
    fs::remove_dir(repo.join("dir")).unwrap();
    let outside = fixture.0.join("outside");
    fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, repo.join("dir")).unwrap();

    let destination = fixture.0.join("worktree");
    let output = ramiz(
        &repo,
        &[
            "add",
            "--inherit",
            "--json",
            "-b",
            "contained",
            destination.to_str().unwrap(),
        ],
    );
    assert!(!output.status.success());
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["error"]["code"], "target_symlink_ancestor");
    assert!(!outside.join("link").exists());
    assert!(!destination.exists());
}

#[test]
fn tracked_symlink_lands_before_read_only_directory_metadata_is_finalized() {
    let fixture = Fixture::new();
    let repo = init_repo(&fixture.0);
    let directory = repo.join("dir");
    fs::create_dir(&directory).unwrap();
    fs::write(repo.join("target"), "target\n").unwrap();
    std::os::unix::fs::symlink("../target", directory.join("link")).unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "tracked link"]);
    let expected_time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
    fs::File::open(&directory)
        .unwrap()
        .set_times(
            fs::FileTimes::new()
                .set_accessed(expected_time)
                .set_modified(expected_time),
        )
        .unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o555)).unwrap();
    let donor_metadata = fs::metadata(&directory).unwrap();

    let destination = fixture.0.join("worktree");
    let output = ramiz(
        &repo,
        &[
            "add",
            "--inherit",
            "--json",
            "-b",
            "read-only-dir",
            destination.to_str().unwrap(),
        ],
    );
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let inherited = fs::metadata(destination.join("dir")).unwrap();
    assert_eq!(inherited.permissions().mode() & 0o777, 0o555);
    assert_eq!(
        (inherited.mtime(), inherited.mtime_nsec()),
        (donor_metadata.mtime(), donor_metadata.mtime_nsec())
    );
    assert_eq!(
        fs::read_link(destination.join("dir/link")).unwrap(),
        Path::new("../target")
    );
}

#[test]
fn donor_head_move_with_identical_tree_aborts_and_rolls_back() {
    let fixture = Fixture::new();
    let repo = init_repo(&fixture.0);
    fs::write(repo.join("file"), "content\n").unwrap();
    git(&repo, &["add", "file"]);
    git(&repo, &["commit", "-qm", "initial"]);
    let destination = fixture.0.join("worktree");
    let marker = fixture.0.join("ready");
    let release = fixture.0.join("release");
    let child = Command::new(env!("CARGO_BIN_EXE_ramiz"))
        .current_dir(&repo)
        .args([
            "add",
            "--inherit",
            "--json",
            "-b",
            "head-race",
            destination.to_str().unwrap(),
        ])
        .env("RAMIZ_TEST_INHERIT_MARKER", &marker)
        .env("RAMIZ_TEST_INHERIT_RELEASE", &release)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    for _ in 0..1_000 {
        if marker.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        marker.exists(),
        "inheritance never reached the controlled interleaving"
    );
    git(
        &repo,
        &["commit", "--allow-empty", "-qm", "same tree, new HEAD"],
    );
    fs::write(&release, "continue").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["error"]["code"], "donor_changed");
    assert!(!destination.exists());
    let branch = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["show-ref", "--verify", "--quiet", "refs/heads/head-race"])
        .status()
        .unwrap();
    assert!(!branch.success());
}
