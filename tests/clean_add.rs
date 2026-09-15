#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

struct Fixture(PathBuf);

impl Fixture {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("ramiz-{label}-{}-{nonce}", std::process::id()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
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

fn git_text(cwd: &Path, args: &[&str]) -> String {
    String::from_utf8(git(cwd, args).stdout)
        .unwrap()
        .trim()
        .to_owned()
}

fn init_repo(root: &Path) -> PathBuf {
    let repo = root.join("repo");
    git(root, &["init", "-q", "-b", "main", repo.to_str().unwrap()]);
    git(&repo, &["config", "user.name", "Ramiz Test"]);
    git(&repo, &["config", "user.email", "ramiz@example.invalid"]);
    repo
}

fn ramiz(cwd: &Path, args: &[&str]) -> Output {
    ramiz_command(cwd, args).output().unwrap()
}

fn ramiz_command(cwd: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ramiz"));
    command.current_dir(cwd).args(args);
    command
}

fn cow_available(cwd: &Path) -> bool {
    let output = ramiz(cwd, &["doctor", "--json"]);
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    value["data"]["available"] == true
}

#[test]
fn clean_add_retains_only_verified_untransformed_files() {
    let fixture = Fixture::new("clean-add");
    let repo = init_repo(fixture.path());
    if !cow_available(&repo) {
        return;
    }

    fs::write(
        repo.join(".gitattributes"),
        "filtered.txt aaa= filter=ramiz-test\n",
    )
    .unwrap();
    fs::write(repo.join("clean.txt"), "clean\n").unwrap();
    fs::write(repo.join("dirty.txt"), "committed\n").unwrap();
    fs::write(repo.join("filtered.txt"), "blob\n").unwrap();
    fs::write(repo.join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();
    fs::set_permissions(repo.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    symlink("clean.txt", repo.join("link")).unwrap();
    git(
        &repo,
        &[
            "config",
            "filter.ramiz-test.clean",
            "sed s/materialized/blob/",
        ],
    );
    git(
        &repo,
        &[
            "config",
            "filter.ramiz-test.smudge",
            "sed s/blob/materialized/",
        ],
    );
    git(&repo, &["config", "filter.ramiz-test.required", "true"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "initial"]);
    git(&repo, &["checkout", "-qb", "target"]);
    fs::write(repo.join("target-only.txt"), "target\n").unwrap();
    git(&repo, &["add", "target-only.txt"]);
    git(&repo, &["commit", "-qm", "target"]);
    git(&repo, &["checkout", "-q", "main"]);
    fs::write(repo.join("dirty.txt"), "donor dirty\n").unwrap();
    let hook = repo.join(".git/hooks/post-checkout");
    fs::write(
        &hook,
        "#!/bin/sh\nprintf 'clean-hook-out\\n'\nprintf 'clean-hook-err\\n' >&2\n",
    )
    .unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();

    let destination = fixture.path().join("worktree");
    let output = ramiz(
        &repo,
        &[
            "add",
            "--json",
            "--require-cow",
            "-b",
            "feature",
            destination.to_str().unwrap(),
            "target",
        ],
    );
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["schema"], "ramiz.cli/v1");
    assert_eq!(envelope["command"], "add");
    assert!(envelope["data"]["cow_files"].as_u64().unwrap() >= 1);
    assert!(envelope["data"]["git_files"].as_u64().unwrap() >= 2);
    assert_eq!(envelope["data"]["hook_output"]["stdout"], "");
    assert_eq!(
        envelope["data"]["hook_output"]["stderr"],
        "Y2xlYW4taG9vay1vdXQKY2xlYW4taG9vay1lcnIK"
    );

    assert_eq!(git_text(&destination, &["status", "--porcelain=v1"]), "");
    assert_eq!(
        fs::read_to_string(destination.join("dirty.txt")).unwrap(),
        "committed\n"
    );
    assert_eq!(
        fs::read_to_string(destination.join("filtered.txt")).unwrap(),
        "materialized\n"
    );
    assert_eq!(
        fs::read_to_string(destination.join("target-only.txt")).unwrap(),
        "target\n"
    );
    assert_eq!(
        fs::read_link(destination.join("link")).unwrap(),
        Path::new("clean.txt")
    );
    assert_ne!(
        fs::metadata(destination.join("run.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0
    );
    assert_eq!(
        git_text(&destination, &["write-tree"]),
        git_text(&repo, &["rev-parse", "target^{tree}"])
    );

    fs::write(destination.join("clean.txt"), "destination\n").unwrap();
    assert_eq!(
        fs::read_to_string(repo.join("clean.txt")).unwrap(),
        "clean\n"
    );
    fs::write(repo.join("clean.txt"), "donor\n").unwrap();
    assert_eq!(
        fs::read_to_string(destination.join("clean.txt")).unwrap(),
        "destination\n"
    );
}

#[test]
fn required_filter_failure_rolls_back_owned_worktree_and_branch() {
    let fixture = Fixture::new("filter-rollback");
    let repo = init_repo(fixture.path());
    if !cow_available(&repo) {
        return;
    }
    fs::write(repo.join(".gitattributes"), "broken.txt filter=broken\n").unwrap();
    fs::write(repo.join("broken.txt"), "content\n").unwrap();
    git(&repo, &["config", "filter.broken.clean", "cat"]);
    git(&repo, &["config", "filter.broken.smudge", "false"]);
    git(&repo, &["config", "filter.broken.required", "true"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "initial"]);

    let destination = fixture.path().join("worktree");
    let output = ramiz(
        &repo,
        &[
            "add",
            "--json",
            "--require-cow",
            "-b",
            "rolled-back",
            destination.to_str().unwrap(),
        ],
    );
    assert!(!output.status.success());
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["command"], "add");
    assert_eq!(envelope["error"]["code"], "checkout_failed");
    assert!(!destination.exists());
    let branch = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["show-ref", "--verify", "--quiet", "refs/heads/rolled-back"])
        .status()
        .unwrap();
    assert!(!branch.success());
    assert_eq!(
        git_text(&repo, &["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        1
    );
}

#[test]
fn failing_post_checkout_preserves_created_worktree_and_hook_writes() {
    let fixture = Fixture::new("hook-boundary");
    let repo = init_repo(fixture.path());
    if !cow_available(&repo) {
        return;
    }
    fs::write(repo.join("file.txt"), "content\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "initial"]);
    let hook = repo.join(".git/hooks/post-checkout");
    fs::write(
        &hook,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" > hook.args\npwd > hook.cwd\nprintf x >> hook.count\nprintf 'hook-out\\n'\nprintf 'hook-err\\n' >&2\nexit 9\n",
    )
    .unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();

    let destination = fixture.path().join("worktree");
    let output = ramiz(
        &repo,
        &[
            "add",
            "--json",
            "--require-cow",
            "-b",
            "hooked",
            destination.to_str().unwrap(),
        ],
    );
    assert!(!output.status.success());
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["error"]["code"], "post_checkout_failed");
    assert_eq!(envelope["error"]["worktree_created"], true);
    assert_eq!(envelope["error"]["hook_output"]["stdout"], "");
    assert_eq!(
        envelope["error"]["hook_output"]["stderr"],
        "aG9vay1vdXQKaG9vay1lcnIK"
    );
    assert_eq!(
        fs::read_to_string(destination.join("hook.count")).unwrap(),
        "x"
    );
    assert_eq!(
        fs::read_to_string(destination.join("hook.cwd"))
            .unwrap()
            .trim(),
        fs::canonicalize(&destination).unwrap().to_str().unwrap()
    );
    let args = fs::read_to_string(destination.join("hook.args")).unwrap();
    let fields: Vec<&str> = args.split_whitespace().collect();
    assert_eq!(fields.len(), 3);
    assert!(fields[0].chars().all(|character| character == '0'));
    assert_eq!(fields[0].len(), fields[1].len());
    assert_eq!(fields[2], "1");
    assert_eq!(
        git_text(&destination, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "hooked"
    );
}

#[test]
fn inherited_git_index_environment_cannot_redirect_destination_plumbing() {
    let fixture = Fixture::new("environment-isolation");
    let repo = init_repo(fixture.path());
    if !cow_available(&repo) {
        return;
    }
    fs::write(repo.join("staged.txt"), "one\n").unwrap();
    git(&repo, &["add", "staged.txt"]);
    git(&repo, &["commit", "-qm", "initial"]);
    fs::write(repo.join("staged.txt"), "two\n").unwrap();
    git(&repo, &["add", "staged.txt"]);
    let index = repo.join(".git/index");
    let before = fs::read(&index).unwrap();

    let destination = fixture.path().join("worktree");
    let output = ramiz_command(
        &repo,
        &[
            "add",
            "--json",
            "--require-cow",
            "--detach",
            destination.to_str().unwrap(),
            "HEAD",
        ],
    )
    .env("GIT_INDEX_FILE", &index)
    .env("GIT_DIR", fixture.path().join("nonexistent-git-dir"))
    .env("GIT_WORK_TREE", fixture.path().join("wrong-worktree"))
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(&index).unwrap(), before);
    assert_eq!(
        git_text(&repo, &["diff", "--cached", "--name-only"]),
        "staged.txt"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn clean_clone_uses_checkout_umask_and_drops_donor_xattrs() {
    use std::os::unix::process::CommandExt;

    let fixture = Fixture::new("metadata-normalization");
    let repo = init_repo(fixture.path());
    if !cow_available(&repo) {
        return;
    }
    fs::write(repo.join("plain"), "plain\n").unwrap();
    fs::write(repo.join("exec"), "exec\n").unwrap();
    fs::set_permissions(repo.join("exec"), fs::Permissions::from_mode(0o755)).unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "initial"]);
    let status = Command::new("xattr")
        .args([
            "-w",
            "user.ramiz-review",
            "leak",
            repo.join("plain").to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(status.success());

    let destination = fixture.path().join("worktree");
    let mut command = ramiz_command(
        &repo,
        &[
            "add",
            "--json",
            "--require-cow",
            "-b",
            "metadata",
            destination.to_str().unwrap(),
        ],
    );
    // SAFETY: pre_exec runs in the child after fork and only sets that child's umask.
    unsafe {
        command.pre_exec(|| {
            libc::umask(0o077);
            Ok(())
        });
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(
        fs::metadata(destination.join("plain"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(destination.join("exec"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let xattr = Command::new("xattr")
        .args([
            "-p",
            "user.ramiz-review",
            destination.join("plain").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        !xattr.status.success(),
        "donor xattr leaked into clean worktree"
    );
}

#[test]
fn bare_repository_without_donor_uses_checkout_backend() {
    let fixture = Fixture::new("bare-checkout");
    let source = init_repo(fixture.path());
    fs::write(source.join("file"), "content\n").unwrap();
    git(&source, &["add", "file"]);
    git(&source, &["commit", "-qm", "initial"]);
    let bare = fixture.path().join("bare.git");
    git(
        fixture.path(),
        &[
            "clone",
            "--bare",
            "-q",
            source.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    let destination = fixture.path().join("worktree");
    let output = ramiz(
        &bare,
        &[
            "add",
            "--json",
            "-b",
            "bare-work",
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
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["data"]["backend"], "checkout");
    assert_eq!(fs::read(destination.join("file")).unwrap(), b"content\n");
    assert_eq!(git_text(&destination, &["status", "--porcelain=v1"]), "");
}

#[test]
fn sparse_checkout_forces_checkout_backend_and_strict_mode_refuses_before_mutation() {
    let fixture = Fixture::new("sparse-fallback");
    let repo = init_repo(fixture.path());
    fs::create_dir(repo.join("included")).unwrap();
    fs::write(repo.join("included/file"), "included\n").unwrap();
    fs::write(repo.join("outside"), "outside\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "initial"]);
    git(&repo, &["sparse-checkout", "init", "--cone"]);
    git(&repo, &["sparse-checkout", "set", "included"]);

    let refused = fixture.path().join("strict");
    let output = ramiz(
        &repo,
        &[
            "add",
            "--json",
            "--require-cow",
            "-b",
            "strict-sparse",
            refused.to_str().unwrap(),
        ],
    );
    assert!(!output.status.success());
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["error"]["code"], "cow_unsupported_index");
    assert!(!refused.exists());
    let branch = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            "refs/heads/strict-sparse",
        ])
        .status()
        .unwrap();
    assert!(!branch.success());

    let destination = fixture.path().join("fallback");
    let output = ramiz(
        &repo,
        &[
            "add",
            "--json",
            "-b",
            "fallback-sparse",
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
    assert_eq!(envelope["data"]["backend"], "checkout");
    assert!(
        envelope["warnings"][0]
            .as_str()
            .unwrap()
            .contains("sparse checkout")
    );
    assert_eq!(git_text(&destination, &["status", "--porcelain=v1"]), "");
}

#[test]
fn split_index_forces_checkout_backend() {
    let fixture = Fixture::new("split-fallback");
    let repo = init_repo(fixture.path());
    fs::write(repo.join("file"), "content\n").unwrap();
    git(&repo, &["add", "file"]);
    git(&repo, &["commit", "-qm", "initial"]);
    git(&repo, &["update-index", "--split-index"]);
    let destination = fixture.path().join("fallback");
    let output = ramiz(
        &repo,
        &[
            "add",
            "--json",
            "-b",
            "fallback-split",
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
    assert_eq!(envelope["data"]["backend"], "checkout");
    assert!(
        envelope["warnings"][0]
            .as_str()
            .unwrap()
            .contains("split index")
    );
}

#[test]
fn unborn_creation_uses_native_symbolic_head_and_skips_checkout_hook() {
    let fixture = Fixture::new("unborn");
    let repo = init_repo(fixture.path());
    let hook = repo.join(".git/hooks/post-checkout");
    fs::write(&hook, "#!/bin/sh\nprintf called > hook-called\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();

    let destination = fixture.path().join("unborn-work");
    let output = ramiz(&repo, &["add", "--json", destination.to_str().unwrap()]);
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(envelope["data"]["head"].is_null());
    assert_eq!(
        git_text(&destination, &["symbolic-ref", "HEAD"]),
        "refs/heads/unborn-work"
    );
    assert_eq!(git_text(&destination, &["status", "--porcelain=v1"]), "");
    assert!(!destination.join("hook-called").exists());
    assert!(
        envelope["warnings"][0]
            .as_str()
            .unwrap()
            .contains("does not run post-checkout")
    );
}

#[test]
fn unborn_detach_refuses_before_destination_creation() {
    let fixture = Fixture::new("unborn-detach");
    let repo = init_repo(fixture.path());
    let destination = fixture.path().join("detached");
    let output = ramiz(
        &repo,
        &["add", "--json", "--detach", destination.to_str().unwrap()],
    );
    assert!(!output.status.success());
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["error"]["code"], "invalid_unborn_target");
    assert!(!destination.exists());
}

#[test]
fn handled_interrupt_after_registration_rolls_back_owned_state() {
    let fixture = Fixture::new("interrupt");
    let repo = init_repo(fixture.path());
    if !cow_available(&repo) {
        return;
    }
    let filter = fixture.path().join("slow-filter.sh");
    fs::write(&filter, "#!/bin/sh\nsleep 1\ncat\n").unwrap();
    fs::set_permissions(&filter, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        repo.join(".gitattributes"),
        "slow.txt filter=slow required\n",
    )
    .unwrap();
    fs::write(repo.join("slow.txt"), "content\n").unwrap();
    git(
        &repo,
        &["config", "filter.slow.clean", filter.to_str().unwrap()],
    );
    git(
        &repo,
        &["config", "filter.slow.smudge", filter.to_str().unwrap()],
    );
    git(&repo, &["config", "filter.slow.required", "true"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "initial"]);

    let destination = fixture.path().join("worktree");
    let child = ramiz_command(
        &repo,
        &[
            "add",
            "--json",
            "--require-cow",
            "-b",
            "interrupted",
            destination.to_str().unwrap(),
        ],
    )
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .spawn()
    .unwrap();
    for _ in 0..200 {
        if destination.join(".git").exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        destination.join(".git").exists(),
        "Ramiz never reached registration"
    );
    // SAFETY: child.id() identifies the live Ramiz subprocess owned by this test.
    assert_eq!(
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) },
        0
    );
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["error"]["code"], "interrupted");
    assert!(!destination.exists());
    let branch = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["show-ref", "--verify", "--quiet", "refs/heads/interrupted"])
        .status()
        .unwrap();
    assert!(!branch.success());
}

#[test]
fn branch_shorthand_existing_branch_and_detached_forms_match_git() {
    let fixture = Fixture::new("grammar");
    let repo = init_repo(fixture.path());
    fs::write(repo.join("file"), "content\n").unwrap();
    git(&repo, &["add", "file"]);
    git(&repo, &["commit", "-qm", "initial"]);
    git(&repo, &["branch", "existing"]);

    let shorthand = fixture.path().join("shorthand");
    let output = ramiz(&repo, &["add", "--json", shorthand.to_str().unwrap()]);
    assert!(output.status.success());
    assert_eq!(
        git_text(&shorthand, &["symbolic-ref", "--short", "HEAD"]),
        "shorthand"
    );

    let existing = fixture.path().join("existing-worktree");
    let output = ramiz(
        &repo,
        &["add", "--json", existing.to_str().unwrap(), "existing"],
    );
    assert!(output.status.success());
    assert_eq!(
        git_text(&existing, &["symbolic-ref", "--short", "HEAD"]),
        "existing"
    );

    let detached = fixture.path().join("detached");
    let output = ramiz(
        &repo,
        &[
            "add",
            "--json",
            "--detach",
            detached.to_str().unwrap(),
            "HEAD",
        ],
    );
    assert!(output.status.success());
    let symbolic = Command::new("git")
        .arg("-C")
        .arg(&detached)
        .args(["symbolic-ref", "--quiet", "HEAD"])
        .status()
        .unwrap();
    assert!(!symbolic.success());

    let duplicate = fixture.path().join("duplicate");
    let output = ramiz(
        &repo,
        &["add", "--json", duplicate.to_str().unwrap(), "existing"],
    );
    assert!(!output.status.success());
    assert!(!duplicate.exists());
}

#[test]
fn registered_missing_destination_refuses_with_git_recovery_guidance() {
    let fixture = Fixture::new("registered-missing");
    let repo = init_repo(fixture.path());
    fs::write(repo.join("file"), "content\n").unwrap();
    git(&repo, &["add", "file"]);
    git(&repo, &["commit", "-qm", "initial"]);
    let destination = fixture.path().join("missing");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "missing-native",
            destination.to_str().unwrap(),
        ],
    );
    fs::remove_dir_all(&destination).unwrap();
    let output = ramiz(
        &repo,
        &[
            "add",
            "--json",
            "-b",
            "must-not-create",
            destination.to_str().unwrap(),
        ],
    );
    assert!(!output.status.success());
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["error"]["code"], "registered_destination_missing");
    assert!(
        envelope["error"]["message"]
            .as_str()
            .unwrap()
            .contains("git worktree repair")
    );
    assert!(!destination.exists());
}

#[test]
fn sha256_repository_uses_matching_hook_object_ids() {
    let fixture = Fixture::new("sha256");
    let repo = fixture.path().join("repo");
    git(
        fixture.path(),
        &[
            "init",
            "--object-format=sha256",
            "-q",
            repo.to_str().unwrap(),
        ],
    );
    git(&repo, &["config", "user.name", "Ramiz Test"]);
    git(&repo, &["config", "user.email", "ramiz@example.invalid"]);
    fs::write(repo.join("file"), "content\n").unwrap();
    git(&repo, &["add", "file"]);
    git(&repo, &["commit", "-qm", "initial"]);
    let hook = repo.join(".git/hooks/post-checkout");
    fs::write(&hook, "#!/bin/sh\nprintf '%s\\n' \"$*\" > hook.args\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    let destination = fixture.path().join("worktree");
    let output = ramiz(
        &repo,
        &[
            "add",
            "--json",
            "-b",
            "sha256-work",
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
    assert_eq!(envelope["data"]["head"].as_str().unwrap().len(), 64);
    let args = fs::read_to_string(destination.join("hook.args")).unwrap();
    let fields: Vec<&str> = args.split_whitespace().collect();
    assert_eq!(fields[0].len(), 64);
    assert_eq!(fields[1].len(), 64);
    assert!(fields[0].chars().all(|character| character == '0'));
}

#[test]
#[cfg(target_os = "linux")]
fn non_utf8_tracked_paths_survive_clean_materialization() {
    use std::os::unix::ffi::OsStringExt;

    let fixture = Fixture::new("non-utf8");
    let repo = init_repo(fixture.path());
    let name = std::ffi::OsString::from_vec(vec![b'f', b'i', b'l', b'e', b'-', 0x80]);
    fs::write(repo.join(&name), "content\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "non-utf8"]);
    let destination = fixture
        .path()
        .join(std::ffi::OsString::from_vec(vec![b'w', b't', b'-', 0x81]));
    let output = ramiz_command(&repo, &["add", "--json", "-b", "non-utf8"])
        .arg(&destination)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        !envelope["data"]["path"]["bytes_base64"]
            .as_str()
            .unwrap()
            .is_empty()
    );
    assert_eq!(fs::read(destination.join(name)).unwrap(), b"content\n");
    assert_eq!(git_text(&destination, &["status", "--porcelain=v1"]), "");
}
