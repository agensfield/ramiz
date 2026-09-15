#![allow(dead_code)]

#[path = "../src/update.rs"]
mod update;

use std::{
    collections::{HashMap, HashSet},
    fmt::Write as _,
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use sha2::{Digest, Sha256};
use update::{
    ArtifactVerifier, DownloadedRelease, Installer, ReleaseMetadata, ReleaseProvider,
    UpdateFailure, UpdateIo, UpdateRequest, UpdateService,
};

fn success_output() -> Output {
    Command::new("true").output().unwrap()
}

fn failure_output() -> Output {
    Command::new("false").output().unwrap()
}

#[derive(Default)]
struct FakeIo {
    files: HashMap<PathBuf, Vec<u8>>,
    cargo_home: Option<PathBuf>,
    available_commands: HashSet<String>,
    commands: Vec<(PathBuf, Vec<String>)>,
    installed_version: String,
    fail_stage_secondary: bool,
    fail_final_secondary_verification: bool,
    fail_installer: bool,
    fail_aux_signature_permissions: bool,
    fail_restore: bool,
    fail_read_sidecar: bool,
    unreadable_path: Option<PathBuf>,
    mutate_on_lock: bool,
    mutate_cargo_on_lock: bool,
    mutate_unrelated_record: bool,
    lock_held: bool,
}

impl FakeIo {
    fn file(&mut self, path: impl Into<PathBuf>, contents: impl AsRef<[u8]>) {
        self.files.insert(path.into(), contents.as_ref().to_vec());
    }

    fn has(&self, path: &Path) -> bool {
        self.files.contains_key(path)
    }
}

impl UpdateIo for FakeIo {
    fn cargo_home(&self) -> Option<PathBuf> {
        self.cargo_home.clone()
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        Ok(path.to_path_buf())
    }

    fn is_file(&self, path: &Path) -> bool {
        self.has(path)
    }

    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        if self.unreadable_path.as_deref() == Some(path) {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        if self.fail_read_sidecar && path.to_string_lossy().contains(".ramiz-checksums") {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        self.files
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }

    fn read_string(&self, path: &Path) -> io::Result<String> {
        self.files
            .get(path)
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }

    fn command_available(&self, command: &str) -> bool {
        self.available_commands.contains(command)
    }

    fn run(&mut self, command: &Path, args: &[String]) -> io::Result<Output> {
        self.commands.push((command.to_path_buf(), args.to_vec()));
        if self.fail_installer
            && (args.first().map(String::as_str) == Some("install")
                || command.ends_with("cargo-binstall"))
        {
            return Ok(failure_output());
        }
        if args.first().map(String::as_str) == Some("install")
            || command.ends_with("cargo-binstall")
        {
            let version = args
                .windows(2)
                .find_map(|pair| (pair[0] == "--version").then_some(pair[1].as_str()))
                .unwrap_or("1.1.0");
            let root = args
                .windows(2)
                .find_map(|pair| (pair[0] == "--root").then_some(PathBuf::from(&pair[1])))
                .unwrap_or_else(|| PathBuf::from("/cargo"));
            if command.ends_with("cargo-binstall") {
                binstall_records_in(&mut self.files, &root, version);
            } else {
                legacy_cargo_records_in(&mut self.files, &root, version);
            }
        }
        if self.mutate_unrelated_record
            && (args.first().map(String::as_str) == Some("install")
                || command.ends_with("cargo-binstall"))
        {
            self.files.insert(
                PathBuf::from("/cargo/.crates2.json"),
                br#"{"installs":{"ramiz 1.1.0 (registry+https://github.com/rust-lang/crates.io-index)":{"bins":["ramiz","git-ramiz"]},"other 1.0.0 (registry+https://github.com/rust-lang/crates.io-index)":{"bins":["other"]}}}"#.to_vec(),
            );
            self.files.insert(
                PathBuf::from("/cargo/binstall/crates-v1.json"),
                br#"{"name":"ramiz","version_req":"=1.1.0","current_version":"1.1.0","source":{"source_type":"Registry","url":"https://github.com/rust-lang/crates.io-index"},"target":"test-host","bins":["ramiz","git-ramiz"]}{"name":"other","version_req":"=1.0.0","current_version":"1.0.0","source":{"source_type":"Registry","url":"https://github.com/rust-lang/crates.io-index"},"target":"test-host","bins":["other"]}"#.to_vec(),
            );
        }
        if args.first().map(String::as_str) == Some("--version") {
            if self.fail_final_secondary_verification
                && command.ends_with("git-ramiz")
                && !command.to_string_lossy().contains("stage")
            {
                return Ok(failure_output());
            }
            let mut output = success_output();
            output.stdout = format!("ramiz {}\n", self.installed_version).into_bytes();
            return Ok(output);
        }
        Ok(success_output())
    }

    fn write_file(&mut self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        if self.fail_stage_secondary && path.to_string_lossy().contains("git-ramiz.stage") {
            return Err(io::Error::other("injected stage failure"));
        }
        self.files.insert(path.to_path_buf(), bytes.to_vec());
        Ok(())
    }

    fn set_executable(&mut self, _path: &Path) -> io::Result<()> {
        Ok(())
    }

    fn set_owner_only(&mut self, path: &Path) -> io::Result<()> {
        if self.fail_aux_signature_permissions && path.ends_with(".ramiz-checksums.txt.minisig") {
            return Err(io::Error::other("injected sidecar permission failure"));
        }
        Ok(())
    }

    fn rename(&mut self, from: &Path, to: &Path) -> io::Result<()> {
        if self.fail_restore && from.to_string_lossy().contains("previous") {
            return Err(io::Error::other("injected rollback failure"));
        }
        let Some(bytes) = self.files.remove(from) else {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        };
        self.files.insert(to.to_path_buf(), bytes);
        Ok(())
    }

    fn remove_file(&mut self, path: &Path) -> io::Result<()> {
        self.files.remove(path);
        Ok(())
    }

    fn acquire_lock(&mut self, _path: &Path) -> io::Result<()> {
        if self.lock_held {
            Err(io::Error::new(io::ErrorKind::AlreadyExists, "held"))
        } else {
            self.lock_held = true;
            if self.mutate_on_lock {
                let primary = PathBuf::from("/standalone/bin/ramiz");
                let companion = PathBuf::from("/standalone/bin/git-ramiz");
                if self.files.contains_key(&primary) {
                    self.file(&primary, "newer ramiz");
                    self.file(&companion, "old git-ramiz");
                    self.file(
                        primary.with_file_name(".ramiz-owner.json"),
                        marker_for(b"newer ramiz", b"old git-ramiz"),
                    );
                    self.file(
                        primary.with_file_name(".ramiz-checksums.txt"),
                        "new checksums",
                    );
                    self.file(
                        primary.with_file_name(".ramiz-checksums.txt.minisig"),
                        "new signature",
                    );
                }
            }
            if self.mutate_cargo_on_lock {
                legacy_cargo_records_in(&mut self.files, Path::new("/cargo"), "2.0.0");
            }
            Ok(())
        }
    }

    fn release_lock(&mut self, _path: &Path) -> io::Result<()> {
        self.lock_held = false;
        Ok(())
    }
}

struct FakeProvider {
    metadata: ReleaseMetadata,
    release: DownloadedRelease,
}

impl ReleaseProvider for FakeProvider {
    fn latest(&mut self, _host: &str) -> Result<ReleaseMetadata, UpdateFailure> {
        Ok(self.metadata.clone())
    }

    fn download(
        &mut self,
        _metadata: &ReleaseMetadata,
    ) -> Result<DownloadedRelease, UpdateFailure> {
        Ok(self.release.clone())
    }

    fn trust_material(&self) -> Option<(&[u8], &[u8])> {
        Some((b"checksums", b"signature"))
    }
}

struct FakeVerifier;

impl ArtifactVerifier for FakeVerifier {
    fn verify(
        &mut self,
        _metadata: &ReleaseMetadata,
        _release: &DownloadedRelease,
    ) -> Result<(), UpdateFailure> {
        Ok(())
    }
}

fn metadata() -> ReleaseMetadata {
    ReleaseMetadata {
        repository: "agensfield/ramiz".into(),
        version: "1.1.0".into(),
        host: "test-host".into(),
        primary_asset: "ramiz-test".into(),
        secondary_asset: "git-ramiz-test".into(),
        archive_sha256: "sha256".into(),
        signature_verified: true,
    }
}

fn service(io: FakeIo) -> UpdateService<FakeIo, FakeProvider, FakeVerifier> {
    UpdateService {
        io,
        provider: FakeProvider {
            metadata: metadata(),
            release: DownloadedRelease {
                primary: b"new ramiz".to_vec(),
                secondary: b"new git-ramiz".to_vec(),
            },
        },
        verifier: FakeVerifier,
        host: "test-host".into(),
    }
}

fn marker_for(primary: &[u8], secondary: &[u8]) -> String {
    let digest = |bytes: &[u8]| {
        let mut encoded = String::with_capacity(64);
        for byte in Sha256::digest(bytes) {
            write!(&mut encoded, "{byte:02x}").unwrap();
        }
        encoded
    };
    format!(
        r#"{{"repository":"agensfield/ramiz","version":"1.0.0","host":"test-host","primary_sha256":"{}","secondary_sha256":"{}","archive_sha256":"{}"}}"#,
        digest(primary),
        digest(secondary),
        digest(b"checksums")
    )
}

fn cargo_v1_manifest(version: &str) -> String {
    format!(
        "[v1]\n\"other 1.0.0 (git+https://example.invalid/other)\" = [\n    \"other\",\n]\n\"ramiz {version} (registry+https://github.com/rust-lang/crates.io-index)\" = [\n    \"git-ramiz\",\n    \"ramiz\",\n]\n"
    )
}

fn legacy_cargo_manifest(version: &str) -> String {
    format!(
        r#"{{"installs":{{"ramiz {version} (registry+https://github.com/rust-lang/crates.io-index)":{{"bins":["ramiz","git-ramiz"]}}}}}}"#
    )
}

fn binstall_manifest(version: &str, target: &str) -> String {
    format!(
        r#"{{"name":"ramiz","version_req":"={version}","current_version":"{version}","source":{{"source_type":"Registry","url":"https://github.com/rust-lang/crates.io-index"}},"target":"{target}","bins":["ramiz","git-ramiz"]}}{{"name":"other","version_req":"=1.0.0","current_version":"1.0.0","source":{{"source_type":"Registry","url":"https://github.com/rust-lang/crates.io-index"}},"target":"test-host","bins":["other"]}}"#
    )
}

fn legacy_cargo_records_in(files: &mut HashMap<PathBuf, Vec<u8>>, root: &Path, version: &str) {
    files.insert(root.join(".crates.toml"), cargo_v1_manifest(version).into());
    files.insert(
        root.join(".crates2.json"),
        legacy_cargo_manifest(version).into(),
    );
}

fn binstall_records_in(files: &mut HashMap<PathBuf, Vec<u8>>, root: &Path, version: &str) {
    files.insert(root.join(".crates.toml"), cargo_v1_manifest(version).into());
    files.insert(
        root.join("binstall/crates-v1.json"),
        binstall_manifest(version, "test-host").into(),
    );
}

fn legacy_cargo_records(io: &mut FakeIo, root: &Path, version: &str) {
    legacy_cargo_records_in(&mut io.files, root, version);
}

fn binstall_records(io: &mut FakeIo, root: &Path, version: &str) {
    binstall_records_in(&mut io.files, root, version);
}

#[test]
fn homebrew_refusal_contains_exact_upgrade_command() {
    let mut io = FakeIo::default();
    let executable = PathBuf::from("/opt/homebrew/Cellar/ramiz/1.0.0/bin/ramiz");
    io.file(&executable, "old");
    let mut service = service(io);
    let error = service
        .run(&UpdateRequest::new(false, &executable, "1.0.0"))
        .unwrap_err();
    assert_eq!(error.code, "homebrew_owned");
    assert_eq!(error.installer, Some(Installer::Homebrew));
    let command = error.command.unwrap();
    assert_eq!(command.program, "brew");
    assert_eq!(command.args, ["upgrade", "agensfield/tap/ramiz"]);
    assert!(error.message.contains("brew upgrade agensfield/tap/ramiz"));
}

#[test]
fn unknown_manager_refuses_without_adoption() {
    let executable = PathBuf::from("/custom/bin/ramiz");
    let companion = PathBuf::from("/custom/bin/git-ramiz");
    let mut io = FakeIo::default();
    io.file(&executable, "old");
    io.file(&companion, "old companion");
    let mut service = service(io);
    let error = service
        .run(&UpdateRequest::new(false, &executable, "1.0.0"))
        .unwrap_err();
    assert_eq!(error.code, "unknown_installer");
    assert_eq!(error.installer, Some(Installer::Unknown));
}

#[cfg(unix)]
#[test]
fn non_utf8_paths_have_a_lossless_json_representation() {
    use std::os::unix::ffi::OsStringExt;
    let path = PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', b'r', b'a', 0x80]));
    let result = update::UpdateResult {
        installer: Installer::Unknown,
        current_version: "1.0.0".into(),
        target_version: None,
        available: false,
        applied: false,
        changed: false,
        executable: path.clone(),
        secondary_executable: path,
        commands: Vec::new(),
        message: "already current".into(),
    };
    let value = serde_json::to_value(result).unwrap();
    assert!(value["executable"]["display"].as_str().is_some());
    assert!(value["executable"]["bytes_base64"].as_str().is_some());
}

#[test]
fn cargo_prefers_binstall_and_pins_exact_version() {
    let cargo_home = PathBuf::from("/cargo");
    let executable = cargo_home.join("bin/ramiz");
    let companion = cargo_home.join("bin/git-ramiz");
    let mut io = FakeIo {
        cargo_home: Some(cargo_home.clone()),
        ..FakeIo::default()
    };
    io.available_commands.insert("cargo-binstall".into());
    io.file(&executable, "old");
    io.file(&companion, "old");
    binstall_records(&mut io, &cargo_home, "1.0.0");
    io.installed_version = "1.1.0".into();
    let mut service = service(io);
    let result = service
        .run(&UpdateRequest::new(false, &executable, "1.0.0"))
        .unwrap();
    assert!(result.applied);
    assert_eq!(result.commands.len(), 1);
    assert_eq!(result.commands[0].program, "cargo-binstall");
    assert_eq!(
        result.commands[0].args,
        ["ramiz", "--version", "1.1.0", "--force", "--root", "/cargo"]
    );
    assert!(service.io.commands[0].0.ends_with("cargo-binstall"));
}

#[test]
fn cargo_binstall_v123_shape_is_accepted_without_crates2() {
    let cargo_home = PathBuf::from("/cargo");
    let executable = cargo_home.join("bin/ramiz");
    let companion = cargo_home.join("bin/git-ramiz");
    let mut io = FakeIo {
        cargo_home: Some(cargo_home.clone()),
        installed_version: "1.1.0".into(),
        ..FakeIo::default()
    };
    io.available_commands.insert("cargo-binstall".into());
    io.file(&executable, "old");
    io.file(&companion, "old");
    binstall_records(&mut io, &cargo_home, "1.0.0");
    let mut service = service(io);
    assert_eq!(
        service
            .run(&UpdateRequest::new(false, &executable, "1.0.0"))
            .unwrap()
            .installer,
        Installer::Cargo
    );
}

#[test]
fn unpinned_binstall_requirement_proves_the_installed_version() {
    for requirement in ["*", "^1"] {
        let cargo_home = PathBuf::from("/cargo");
        let executable = cargo_home.join("bin/ramiz");
        let companion = cargo_home.join("bin/git-ramiz");
        let mut io = FakeIo {
            cargo_home: Some(cargo_home.clone()),
            ..FakeIo::default()
        };
        io.file(&executable, "old");
        io.file(&companion, "old");
        binstall_records(&mut io, &cargo_home, "1.0.0");
        let selected = binstall_manifest("1.0.0", "test-host").replacen(
            r#""version_req":"=1.0.0""#,
            &format!(r#""version_req":"{requirement}""#),
            1,
        );
        io.file(cargo_home.join("binstall/crates-v1.json"), selected);
        let mut service = service(io);
        let result = service
            .run(&UpdateRequest::new(true, &executable, "1.0.0"))
            .unwrap();
        assert_eq!(result.installer, Installer::Cargo);
    }
}

#[test]
fn cargo_binstall_conflicting_ramiz_provenance_is_unknown() {
    let cargo_home = PathBuf::from("/cargo");
    let executable = cargo_home.join("bin/ramiz");
    let companion = cargo_home.join("bin/git-ramiz");
    let mut io = FakeIo {
        cargo_home: Some(cargo_home.clone()),
        ..FakeIo::default()
    };
    io.file(&executable, "old");
    io.file(&companion, "old companion");
    binstall_records(&mut io, &cargo_home, "1.0.0");
    io.file(
        cargo_home.join("binstall/crates-v1.json"),
        r#"{"name":"ramiz","version_req":"=1.0.0","current_version":"1.0.0","source":{"source_type":"Git","url":"https://example.invalid/ramiz"},"target":"test-host","bins":["ramiz","git-ramiz"]}"#,
    );
    let mut service = service(io);
    let error = service
        .run(&UpdateRequest::new(false, &executable, "1.0.0"))
        .unwrap_err();
    assert_eq!(error.code, "unknown_installer");
    assert_eq!(error.installer, Some(Installer::Unknown));
}

#[test]
fn conflicting_manager_versions_never_authorize_an_installer() {
    let cargo_home = PathBuf::from("/cargo");
    let executable = cargo_home.join("bin/ramiz");
    let companion = cargo_home.join("bin/git-ramiz");
    let mut io = FakeIo {
        cargo_home: Some(cargo_home.clone()),
        ..FakeIo::default()
    };
    io.file(&executable, "old");
    io.file(&companion, "old companion");
    legacy_cargo_records(&mut io, &cargo_home, "1.0.0");
    io.file(cargo_home.join(".crates.toml"), cargo_v1_manifest("2.0.0"));
    io.file(
        cargo_home.join("binstall/crates-v1.json"),
        binstall_manifest("2.0.0", "test-host"),
    );
    let mut service = service(io);
    let error = service
        .run(&UpdateRequest::new(false, &executable, "1.0.0"))
        .unwrap_err();
    assert_eq!(error.code, "cargo_record_unproven");
    assert!(service.io.commands.is_empty());
}

#[test]
fn cargo_version_change_while_acquiring_lock_refuses_before_installer() {
    let cargo_home = PathBuf::from("/cargo");
    let executable = cargo_home.join("bin/ramiz");
    let companion = cargo_home.join("bin/git-ramiz");
    let mut io = FakeIo {
        cargo_home: Some(cargo_home.clone()),
        mutate_cargo_on_lock: true,
        ..FakeIo::default()
    };
    io.file(&executable, "old");
    io.file(&companion, "old companion");
    legacy_cargo_records(&mut io, &cargo_home, "1.0.0");
    let mut service = service(io);
    let error = service
        .run(&UpdateRequest::new(false, &executable, "1.0.0"))
        .unwrap_err();
    assert_eq!(error.code, "installation_changed");
    assert!(service.io.commands.is_empty());
}

#[test]
fn malformed_present_legacy_record_fails_closed() {
    let cargo_home = PathBuf::from("/cargo");
    let executable = cargo_home.join("bin/ramiz");
    let companion = cargo_home.join("bin/git-ramiz");
    let mut io = FakeIo {
        cargo_home: Some(cargo_home.clone()),
        ..FakeIo::default()
    };
    io.file(&executable, "old");
    io.file(&companion, "old companion");
    binstall_records(&mut io, &cargo_home, "1.0.0");
    io.file(cargo_home.join(".crates2.json"), "{broken");
    let mut service = service(io);
    let error = service
        .run(&UpdateRequest::new(false, &executable, "1.0.0"))
        .unwrap_err();
    assert_eq!(error.code, "unknown_installer");
    assert!(service.io.commands.is_empty());
}

#[test]
fn unreadable_present_manager_record_fails_closed() {
    let cargo_home = PathBuf::from("/cargo");
    let executable = cargo_home.join("bin/ramiz");
    let companion = cargo_home.join("bin/git-ramiz");
    let legacy = cargo_home.join(".crates2.json");
    let mut io = FakeIo {
        cargo_home: Some(cargo_home.clone()),
        unreadable_path: Some(legacy.clone()),
        ..FakeIo::default()
    };
    io.file(&executable, "old");
    io.file(&companion, "old companion");
    binstall_records(&mut io, &cargo_home, "1.0.0");
    io.file(&legacy, legacy_cargo_manifest("1.0.0"));
    let mut service = service(io);
    let error = service
        .run(&UpdateRequest::new(false, &executable, "1.0.0"))
        .unwrap_err();
    assert_eq!(error.code, "unknown_installer");
    assert!(service.io.commands.is_empty());
}

#[test]
fn source_less_and_duplicate_legacy_identities_fail_closed() {
    for record in [
        r#"{"installs":{"ramiz 1.0.0":{"bins":["ramiz","git-ramiz"]}}}"#,
        r#"{"installs":{"ramiz 1.0.0 (registry+https://github.com/rust-lang/crates.io-index)":{"bins":["ramiz","git-ramiz"]},"ramiz 1.0.0 (registry+https://github.com/rust-lang/crates.io-index)":{"bins":["ramiz","git-ramiz"]}}}"#,
    ] {
        let cargo_home = PathBuf::from("/cargo");
        let executable = cargo_home.join("bin/ramiz");
        let companion = cargo_home.join("bin/git-ramiz");
        let mut io = FakeIo {
            cargo_home: Some(cargo_home.clone()),
            ..FakeIo::default()
        };
        io.file(&executable, "old");
        io.file(&companion, "old companion");
        io.file(cargo_home.join(".crates.toml"), cargo_v1_manifest("1.0.0"));
        io.file(cargo_home.join(".crates2.json"), record);
        let mut service = service(io);
        assert_eq!(
            service
                .run(&UpdateRequest::new(false, &executable, "1.0.0"))
                .unwrap_err()
                .code,
            "unknown_installer"
        );
        assert!(service.io.commands.is_empty());
    }
}

#[test]
fn binstall_target_must_match_the_host() {
    let cargo_home = PathBuf::from("/cargo");
    let executable = cargo_home.join("bin/ramiz");
    let companion = cargo_home.join("bin/git-ramiz");
    let mut io = FakeIo {
        cargo_home: Some(cargo_home.clone()),
        ..FakeIo::default()
    };
    io.file(&executable, "old");
    io.file(&companion, "old companion");
    io.file(cargo_home.join(".crates.toml"), cargo_v1_manifest("1.0.0"));
    io.file(
        cargo_home.join("binstall/crates-v1.json"),
        binstall_manifest("1.0.0", "wrong-platform"),
    );
    let mut service = service(io);
    assert_eq!(
        service
            .run(&UpdateRequest::new(false, &executable, "1.0.0"))
            .unwrap_err()
            .code,
        "unknown_installer"
    );
}

#[test]
fn binstall_requirement_must_include_the_recorded_version() {
    for requirement in ["^2", "not a requirement"] {
        let cargo_home = PathBuf::from("/cargo");
        let executable = cargo_home.join("bin/ramiz");
        let companion = cargo_home.join("bin/git-ramiz");
        let mut io = FakeIo {
            cargo_home: Some(cargo_home.clone()),
            ..FakeIo::default()
        };
        io.file(&executable, "old");
        io.file(&companion, "old companion");
        io.file(cargo_home.join(".crates.toml"), cargo_v1_manifest("1.0.0"));
        let inconsistent = binstall_manifest("1.0.0", "test-host").replacen(
            r#""version_req":"=1.0.0""#,
            &format!(r#""version_req":"{requirement}""#),
            1,
        );
        io.file(cargo_home.join("binstall/crates-v1.json"), inconsistent);
        let mut service = service(io);
        assert_eq!(
            service
                .run(&UpdateRequest::new(false, &executable, "1.0.0"))
                .unwrap_err()
                .code,
            "unknown_installer"
        );
    }
}

#[test]
fn linux_gnu_and_musl_binstall_targets_are_compatible() {
    let cargo_home = PathBuf::from("/cargo");
    let executable = cargo_home.join("bin/ramiz");
    let companion = cargo_home.join("bin/git-ramiz");
    let mut io = FakeIo {
        cargo_home: Some(cargo_home.clone()),
        installed_version: "1.1.0".into(),
        ..FakeIo::default()
    };
    io.file(&executable, "old");
    io.file(&companion, "old companion");
    io.file(cargo_home.join(".crates.toml"), cargo_v1_manifest("1.0.0"));
    io.file(
        cargo_home.join("binstall/crates-v1.json"),
        binstall_manifest("1.0.0", "x86_64-unknown-linux-musl"),
    );
    let mut service = service(io);
    service.host = "x86_64-unknown-linux-gnu".into();
    service.provider.metadata.host = service.host.clone();
    let result = service
        .run(&UpdateRequest::new(true, &executable, "1.0.0"))
        .unwrap();
    assert_eq!(result.installer, Installer::Cargo);
    assert!(!result.applied);
}

#[test]
fn cargo_binstall_failure_restores_multiline_and_stream_records() {
    let cargo_home = PathBuf::from("/cargo");
    let executable = cargo_home.join("bin/ramiz");
    let companion = cargo_home.join("bin/git-ramiz");
    let mut io = FakeIo {
        cargo_home: Some(cargo_home.clone()),
        fail_installer: true,
        ..FakeIo::default()
    };
    io.available_commands.insert("cargo-binstall".into());
    io.file(&executable, "old");
    io.file(&companion, "old companion");
    binstall_records(&mut io, &cargo_home, "1.0.0");
    let before = io.files.clone();
    let mut service = service(io);
    let error = service
        .run(&UpdateRequest::new(false, &executable, "1.0.0"))
        .unwrap_err();
    assert_eq!(error.code, "installer_failed");
    assert_eq!(service.io.files, before);
}

#[test]
fn cargo_failure_restores_both_binaries_and_install_records() {
    let cargo_home = PathBuf::from("/cargo");
    let executable = cargo_home.join("bin/ramiz");
    let companion = cargo_home.join("bin/git-ramiz");
    let mut io = FakeIo {
        cargo_home: Some(cargo_home.clone()),
        fail_installer: true,
        ..FakeIo::default()
    };
    io.file(&executable, "old");
    io.file(&companion, "old companion");
    legacy_cargo_records(&mut io, &cargo_home, "1.0.0");
    let before = io.files.clone();
    let mut service = service(io);
    let error = service
        .run(&UpdateRequest::new(false, &executable, "1.0.0"))
        .unwrap_err();
    assert_eq!(error.code, "installer_failed");
    assert_eq!(service.io.files, before);
}

#[test]
fn unrelated_cargo_install_change_is_not_clobbered() {
    let cargo_home = PathBuf::from("/cargo");
    let executable = cargo_home.join("bin/ramiz");
    let companion = cargo_home.join("bin/git-ramiz");
    let record = cargo_home.join(".crates2.json");
    let mut io = FakeIo {
        cargo_home: Some(cargo_home.clone()),
        mutate_unrelated_record: true,
        fail_final_secondary_verification: true,
        installed_version: "1.1.0".into(),
        ..FakeIo::default()
    };
    io.file(&executable, "old");
    io.file(&companion, "old companion");
    legacy_cargo_records(&mut io, &cargo_home, "1.0.0");
    let mut service = service(io);
    let error = service
        .run(&UpdateRequest::new(false, &executable, "1.0.0"))
        .unwrap_err();
    assert_eq!(error.code, "cargo_rollback_failed");
    assert!(error.rollback.is_none());
    assert_eq!(
        service.io.files.get(&record).unwrap(),
        br#"{"installs":{"ramiz 1.1.0 (registry+https://github.com/rust-lang/crates.io-index)":{"bins":["ramiz","git-ramiz"]},"other 1.0.0 (registry+https://github.com/rust-lang/crates.io-index)":{"bins":["other"]}}}"#
    );
    assert_eq!(service.io.files.get(&executable).unwrap(), b"old");
    assert_eq!(service.io.files.get(&companion).unwrap(), b"old companion");
}

#[test]
fn cargo_fallback_is_locked_and_exact() {
    let cargo_home = PathBuf::from("/cargo");
    let executable = cargo_home.join("bin/ramiz");
    let companion = cargo_home.join("bin/git-ramiz");
    let mut io = FakeIo {
        cargo_home: Some(cargo_home.clone()),
        installed_version: "1.1.0".into(),
        ..FakeIo::default()
    };
    io.file(&executable, "old");
    io.file(&companion, "old");
    legacy_cargo_records(&mut io, &cargo_home, "1.0.0");
    let mut service = service(io);
    let result = service
        .run(&UpdateRequest::new(false, &executable, "1.0.0"))
        .unwrap();
    assert_eq!(result.commands[0].program, "cargo");
    assert_eq!(
        result.commands[0].args,
        [
            "install",
            "ramiz",
            "--version",
            "1.1.0",
            "--locked",
            "--force",
            "--root",
            "/cargo"
        ]
    );
}

#[test]
fn cargo_record_version_must_match_installed_binary_version() {
    let cargo_home = PathBuf::from("/cargo");
    let executable = cargo_home.join("bin/ramiz");
    let companion = cargo_home.join("bin/git-ramiz");
    let mut io = FakeIo {
        cargo_home: Some(cargo_home.clone()),
        ..FakeIo::default()
    };
    io.file(&executable, "old");
    io.file(&companion, "old companion");
    legacy_cargo_records(&mut io, &cargo_home, "11.1.0");
    let mut service = service(io);
    let error = service
        .run(&UpdateRequest::new(false, &executable, "1.1.0"))
        .unwrap_err();
    assert_eq!(error.code, "cargo_record_unproven");
}

#[test]
fn adoption_proves_loose_pair_and_writes_owner_marker() {
    let executable = PathBuf::from("/standalone/bin/ramiz");
    let companion = PathBuf::from("/standalone/bin/git-ramiz");
    let mut io = FakeIo::default();
    io.file(&executable, "old ramiz");
    io.file(&companion, "old git-ramiz");
    let mut service = service(io);
    service.provider.metadata.version = "1.0.0".into();
    service.provider.release.primary = b"old ramiz".to_vec();
    service.provider.release.secondary = b"old git-ramiz".to_vec();
    let result = service
        .run(&UpdateRequest::new(false, &executable, "1.0.0").with_adopt(true))
        .unwrap();
    assert_eq!(result.installer, Installer::Standalone);
    assert!(result.applied);
    assert!(
        service
            .io
            .files
            .contains_key(&executable.with_file_name(".ramiz-owner.json"))
    );
    assert!(
        service
            .io
            .files
            .contains_key(&executable.with_file_name(".ramiz-checksums.txt.minisig"))
    );
}

#[test]
fn sidecar_failure_restores_every_preexisting_sidecar() {
    let executable = PathBuf::from("/standalone/bin/ramiz");
    let companion = PathBuf::from("/standalone/bin/git-ramiz");
    let mut io = FakeIo {
        fail_aux_signature_permissions: true,
        ..FakeIo::default()
    };
    io.file(&executable, "old ramiz");
    io.file(&companion, "old git-ramiz");
    io.file(
        executable.with_file_name(".ramiz-owner.json"),
        marker_for(b"old ramiz", b"old git-ramiz"),
    );
    io.file(
        executable.with_file_name(".ramiz-checksums.txt"),
        "old checksums",
    );
    io.file(
        executable.with_file_name(".ramiz-checksums.txt.minisig"),
        "old signature",
    );
    io.installed_version = "1.1.0".into();
    let before = io.files.clone();
    let mut service = service(io);
    let error = service
        .run(&UpdateRequest::new(false, &executable, "1.0.0"))
        .unwrap_err();
    assert_eq!(error.code, "staging_failed");
    assert_eq!(service.io.files, before);
}

#[test]
fn sidecar_snapshot_read_error_refuses_before_mutation() {
    let executable = PathBuf::from("/standalone/bin/ramiz");
    let companion = PathBuf::from("/standalone/bin/git-ramiz");
    let mut io = FakeIo {
        fail_read_sidecar: true,
        ..FakeIo::default()
    };
    io.file(&executable, "old ramiz");
    io.file(&companion, "old git-ramiz");
    io.file(
        executable.with_file_name(".ramiz-owner.json"),
        marker_for(b"old ramiz", b"old git-ramiz"),
    );
    io.file(
        executable.with_file_name(".ramiz-checksums.txt"),
        "old checksums",
    );
    io.file(
        executable.with_file_name(".ramiz-checksums.txt.minisig"),
        "old signature",
    );
    let before = io.files.clone();
    let mut service = service(io);
    let error = service
        .run(&UpdateRequest::new(false, &executable, "1.0.0"))
        .unwrap_err();
    assert_eq!(error.code, "standalone_unadopted");
    assert_eq!(service.io.files, before);
}

#[test]
fn check_reports_availability_without_mutation() {
    let cargo_home = PathBuf::from("/cargo");
    let executable = cargo_home.join("bin/ramiz");
    let companion = cargo_home.join("bin/git-ramiz");
    let mut io = FakeIo {
        cargo_home: Some(cargo_home.clone()),
        ..FakeIo::default()
    };
    io.file(&executable, "old");
    io.file(&companion, "old");
    legacy_cargo_records(&mut io, &cargo_home, "1.0.0");
    let before = io.files.clone();
    let mut service = service(io);
    let result = service
        .run(&UpdateRequest::new(true, &executable, "1.0.0"))
        .unwrap();
    assert!(result.available);
    assert!(!result.applied);
    assert!(result.commands.is_empty());
    assert_eq!(service.io.files, before);
}

#[test]
fn standalone_verification_failure_restores_both_binaries() {
    let executable = PathBuf::from("/standalone/bin/ramiz");
    let companion = PathBuf::from("/standalone/bin/git-ramiz");
    let mut io = FakeIo::default();
    io.file(&executable, "old ramiz");
    io.file(&companion, "old git-ramiz");
    io.file(
        executable.with_file_name(".ramiz-owner.json"),
        marker_for(b"old ramiz", b"old git-ramiz"),
    );
    io.file(
        executable.with_file_name(".ramiz-checksums.txt"),
        "checksums",
    );
    io.file(
        executable.with_file_name(".ramiz-checksums.txt.minisig"),
        "signature",
    );
    io.installed_version = "1.1.0".into();
    io.fail_final_secondary_verification = true;
    let mut service = service(io);
    let error = service
        .run(&UpdateRequest::new(false, &executable, "1.0.0"))
        .unwrap_err();
    assert_eq!(error.code, "update_rolled_back");
    assert_eq!(service.io.files.get(&executable).unwrap(), b"old ramiz");
    assert_eq!(service.io.files.get(&companion).unwrap(), b"old git-ramiz");
    assert!(
        !service
            .io
            .files
            .keys()
            .any(|path| path.to_string_lossy().contains("previous"))
    );
}

#[test]
fn review_newer_installation_while_acquiring_lock_must_not_downgrade() {
    let executable = PathBuf::from("/standalone/bin/ramiz");
    let companion = PathBuf::from("/standalone/bin/git-ramiz");
    let mut io = FakeIo {
        mutate_on_lock: true,
        installed_version: "1.1.0".into(),
        ..FakeIo::default()
    };
    io.file(&executable, "old ramiz");
    io.file(&companion, "old git-ramiz");
    io.file(
        executable.with_file_name(".ramiz-owner.json"),
        marker_for(b"old ramiz", b"old git-ramiz"),
    );
    io.file(
        executable.with_file_name(".ramiz-checksums.txt"),
        "old checksums",
    );
    io.file(
        executable.with_file_name(".ramiz-checksums.txt.minisig"),
        "old signature",
    );
    let before = io.files.clone();
    let mut service = service(io);
    let error = service
        .run(&UpdateRequest::new(false, &executable, "1.0.0"))
        .unwrap_err();
    assert_eq!(error.code, "installation_changed");
    assert_ne!(service.io.files, before);
    assert_eq!(service.io.files.get(&executable).unwrap(), b"newer ramiz");
}

#[test]
fn adoption_interleaving_known_marker_is_refused_before_writing() {
    let executable = PathBuf::from("/standalone/bin/ramiz");
    let companion = PathBuf::from("/standalone/bin/git-ramiz");
    let mut io = FakeIo {
        mutate_on_lock: true,
        ..FakeIo::default()
    };
    io.file(&executable, "old ramiz");
    io.file(&companion, "old git-ramiz");
    let mut service = service(io);
    service.provider.metadata.version = "1.0.0".into();
    service.provider.release.primary = b"old ramiz".to_vec();
    service.provider.release.secondary = b"old git-ramiz".to_vec();
    let error = service
        .run(&UpdateRequest::new(false, &executable, "1.0.0").with_adopt(true))
        .unwrap_err();
    assert_eq!(error.code, "installation_changed");
    assert_eq!(service.io.files.get(&executable).unwrap(), b"newer ramiz");
}

#[test]
fn rollback_failure_is_reported_and_recovery_backup_is_retained() {
    let executable = PathBuf::from("/standalone/bin/ramiz");
    let companion = PathBuf::from("/standalone/bin/git-ramiz");
    let mut io = FakeIo {
        fail_final_secondary_verification: true,
        fail_restore: true,
        installed_version: "1.1.0".into(),
        ..FakeIo::default()
    };
    io.file(&executable, "old ramiz");
    io.file(&companion, "old git-ramiz");
    io.file(
        executable.with_file_name(".ramiz-owner.json"),
        marker_for(b"old ramiz", b"old git-ramiz"),
    );
    io.file(
        executable.with_file_name(".ramiz-checksums.txt"),
        "old checksums",
    );
    io.file(
        executable.with_file_name(".ramiz-checksums.txt.minisig"),
        "old signature",
    );
    let mut service = service(io);
    let error = service
        .run(&UpdateRequest::new(false, &executable, "1.0.0"))
        .unwrap_err();
    assert_eq!(error.code, "update_rolled_back");
    assert!(error.rollback.is_some());
    assert!(
        service
            .io
            .files
            .keys()
            .any(|path| path.to_string_lossy().contains("previous"))
    );
}

#[cfg(unix)]
#[test]
fn standalone_activation_executes_both_updated_surfaces() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!("ramiz-update-e2e-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("bin")).unwrap();
    let executable = root.join("bin/ramiz");
    let companion = root.join("bin/git-ramiz");
    let old_primary = b"#!/bin/sh\nprintf 'ramiz 1.0.0\\n'\n";
    let old_secondary = b"#!/bin/sh\nprintf 'ramiz 1.0.0\\n'\n";
    let new_primary = b"#!/bin/sh\nprintf 'ramiz 1.1.0\\n'\n";
    let new_secondary = b"#!/bin/sh\nprintf 'ramiz 1.1.0\\n'\n";
    for (path, bytes) in [(&executable, old_primary), (&companion, old_secondary)] {
        fs::write(path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fs::write(
        executable.with_file_name(".ramiz-owner.json"),
        marker_for(old_primary, old_secondary),
    )
    .unwrap();
    fs::write(
        executable.with_file_name(".ramiz-checksums.txt"),
        "old checksums",
    )
    .unwrap();
    fs::write(
        executable.with_file_name(".ramiz-checksums.txt.minisig"),
        "old signature",
    )
    .unwrap();
    let provider = FakeProvider {
        metadata: metadata(),
        release: DownloadedRelease {
            primary: new_primary.to_vec(),
            secondary: new_secondary.to_vec(),
        },
    };
    let mut service = UpdateService {
        io: update::SystemUpdateIo,
        provider,
        verifier: FakeVerifier,
        host: "test-host".into(),
    };
    let result = service
        .run(&UpdateRequest::new(false, &executable, "1.0.0"))
        .unwrap();
    assert!(result.applied);
    let primary = Command::new(&executable).arg("--version").output().unwrap();
    let secondary = Command::new(&companion).arg("--version").output().unwrap();
    assert_eq!(primary.stdout, b"ramiz 1.1.0\n");
    assert_eq!(secondary.stdout, b"ramiz 1.1.0\n");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn secondary_stage_failure_never_activates_primary() {
    let executable = PathBuf::from("/standalone/bin/ramiz");
    let companion = PathBuf::from("/standalone/bin/git-ramiz");
    let mut io = FakeIo::default();
    io.file(&executable, "old ramiz");
    io.file(&companion, "old git-ramiz");
    io.file(
        executable.with_file_name(".ramiz-owner.json"),
        marker_for(b"old ramiz", b"old git-ramiz"),
    );
    io.file(
        executable.with_file_name(".ramiz-checksums.txt"),
        "checksums",
    );
    io.file(
        executable.with_file_name(".ramiz-checksums.txt.minisig"),
        "signature",
    );
    io.fail_stage_secondary = true;
    let mut service = service(io);
    let error = service
        .run(&UpdateRequest::new(false, &companion, "1.0.0"))
        .unwrap_err();
    assert_eq!(error.code, "staging_failed");
    assert_eq!(service.io.files.get(&executable).unwrap(), b"old ramiz");
    assert_eq!(service.io.files.get(&companion).unwrap(), b"old git-ramiz");
}
