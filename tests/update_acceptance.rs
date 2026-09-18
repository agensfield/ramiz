#![allow(dead_code)]

#[path = "../src/update.rs"]
mod update;

use std::{
    collections::HashMap,
    env,
    fmt::Write as _,
    fs,
    io::{Cursor, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::Command,
    thread::{self, JoinHandle},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use flate2::{Compression, write::GzEncoder};
use minisign::{PublicKey, SecretKey, SecretKeyBox, sign};
use sha2::{Digest, Sha256};
use update::{
    ArtifactVerifier, DownloadedRelease, GithubReleaseProvider, PinnedMinisignVerifier,
    ReleaseMetadata, ReleaseProvider, SystemUpdateIo, UpdateFailure, UpdateRequest, UpdateService,
};

const COMMIT: &str = "0000000000000000000000000000000000000000";
const TEST_PUBLIC_KEY: &str = include_str!("fixtures/update-test.pub");
const TEST_SECRET_KEY: &str = include_str!("fixtures/update-test.key");
const CANDIDATE_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const HOST: &str = "aarch64-apple-darwin";
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
const HOST: &str = "x86_64-apple-darwin";
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const HOST: &str = "aarch64-unknown-linux-musl";
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const HOST: &str = "x86_64-unknown-linux-musl";

struct HttpFixture {
    base: String,
    address: SocketAddr,
    thread: Option<JoinHandle<()>>,
}

impl HttpFixture {
    fn new<F>(build_files: F, requests: usize) -> Self
    where
        F: FnOnce(&str) -> HashMap<String, Vec<u8>>,
    {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let base = format!("http://{address}/repos/agensfield/ramiz");
        let files = build_files(&base);
        let thread = thread::spawn(move || {
            for _ in 0..requests {
                let (mut stream, _) = listener.accept().unwrap();
                let request = request_path(&mut stream);
                let path = request
                    .strip_prefix("/repos/agensfield/ramiz")
                    .unwrap_or(&request);
                let (status, content_type, body) = match files.get(path) {
                    Some(body) => ("200 OK", content_type(path), body.as_slice()),
                    None => ("404 Not Found", "text/plain", b"missing".as_slice()),
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: {content_type}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
                stream.write_all(body).unwrap();
            }
        });
        Self {
            base,
            address,
            thread: Some(thread),
        }
    }
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            while !thread.is_finished() {
                if let Ok(mut stream) = TcpStream::connect(self.address) {
                    let _ = stream.write_all(
                        b"GET /__fixture_shutdown__ HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                    );
                }
            }
            thread.join().unwrap();
        }
    }
}

fn request_path(stream: &mut TcpStream) -> String {
    let mut request = Vec::new();
    let mut byte = [0; 1];
    while !request.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        request.push(byte[0]);
    }
    let line = request.split(|byte| *byte == b'\n').next().unwrap();
    let line = String::from_utf8_lossy(line);
    line.split_whitespace().nth(1).unwrap().to_owned()
}

fn content_type(path: &str) -> &'static str {
    if path.ends_with(".json") || path.contains("/releases/") {
        "application/json"
    } else {
        "application/octet-stream"
    }
}

fn digest(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        write!(&mut encoded, "{byte:02x}").unwrap();
    }
    encoded
}

fn sign_checksums(checksums: &[u8], public: &PublicKey, secret: &SecretKey) -> Vec<u8> {
    sign(
        Some(public),
        secret,
        Cursor::new(checksums),
        Some("ramiz updater acceptance fixture"),
        Some("signature from test-only fixture key"),
    )
    .unwrap()
    .to_bytes()
}

fn fixture_keypair() -> (PublicKey, SecretKey) {
    let public = PublicKey::from_box(TEST_PUBLIC_KEY.to_owned().into()).unwrap();
    let secret = SecretKeyBox::from_string(TEST_SECRET_KEY)
        .unwrap()
        .into_unencrypted_secret_key()
        .unwrap();
    (public, secret)
}

fn archive(version: &str, primary: &[u8], secondary: &[u8]) -> Vec<u8> {
    let mut compressed = GzEncoder::new(Vec::new(), Compression::best());
    {
        let mut archive = tar::Builder::new(&mut compressed);
        for (name, contents, mode) in [("ramiz", primary, 0o755), ("git-ramiz", secondary, 0o755)] {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(mode);
            header.set_mtime(0);
            header.set_uid(0);
            header.set_gid(0);
            header.set_cksum();
            archive.append_data(&mut header, name, contents).unwrap();
        }
        let source = format!(
            "{{\"commit\":\"{COMMIT}\",\"repository\":\"https://github.com/agensfield/ramiz\",\"source\":\"https://github.com/agensfield/ramiz/tree/{COMMIT}\",\"target\":\"{HOST}\",\"version\":\"{version}\"}}\n"
        );
        let mut header = tar::Header::new_gnu();
        header.set_size(source.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        header.set_cksum();
        archive
            .append_data(&mut header, "SOURCE.json", source.as_bytes())
            .unwrap();
        archive.finish().unwrap();
    }
    compressed.finish().unwrap()
}

fn release_json(base: &str, version: &str) -> Vec<u8> {
    let archive_name = archive_name(version);
    let origin = base
        .strip_suffix("/repos/agensfield/ramiz")
        .expect("fixture base includes the GitHub API path");
    serde_json::json!({
        "tag_name": format!("v{version}"),
        "prerelease": false,
        "draft": false,
        "assets": [
            {"name": archive_name, "browser_download_url": format!("{origin}/assets/{version}/archive")},
            {"name": "checksums.txt", "browser_download_url": format!("{origin}/assets/{version}/checksums")},
            {"name": "checksums.txt.minisig", "browser_download_url": format!("{origin}/assets/{version}/signature")},
            {"name": "attestation.jsonl", "browser_download_url": format!("{origin}/assets/{version}/attestation")}
        ]
    })
    .to_string()
    .into_bytes()
}

fn archive_name(version: &str) -> String {
    let suffix = match HOST {
        "aarch64-apple-darwin" => "darwin_arm64",
        "x86_64-apple-darwin" => "darwin_amd64",
        "aarch64-unknown-linux-musl" => "linux_arm64",
        "x86_64-unknown-linux-musl" => "linux_amd64",
        _ => unreachable!("unsupported acceptance host"),
    };
    format!("ramiz_{version}_{suffix}.tar.gz")
}

fn fixture_files(
    base: &str,
    predecessor: &[u8],
    candidate_primary: &[u8],
    candidate_secondary: &[u8],
    public_key: &PublicKey,
    secret_key: &SecretKey,
) -> HashMap<String, Vec<u8>> {
    let old_archive = archive("0.9.0", predecessor, predecessor);
    let new_archive = archive(CANDIDATE_VERSION, candidate_primary, candidate_secondary);
    let mut files = HashMap::new();
    for (version, archive) in [("0.9.0", old_archive), (CANDIDATE_VERSION, new_archive)] {
        let name = archive_name(version);
        let checksums = format!("{}  {name}\n", digest(&archive)).into_bytes();
        let signature = sign_checksums(&checksums, public_key, secret_key);
        files.insert(format!("/assets/{version}/archive"), archive);
        files.insert(format!("/assets/{version}/checksums"), checksums);
        files.insert(format!("/assets/{version}/signature"), signature);
        files.insert(format!("/assets/{version}/attestation"), b"{}\n".to_vec());
    }
    files.insert("/releases/tags/v0.9.0".into(), release_json(base, "0.9.0"));
    files.insert(
        "/releases/latest".into(),
        release_json(base, CANDIDATE_VERSION),
    );
    files
}

#[cfg(unix)]
fn predecessor_pair(root: &Path) -> (PathBuf, PathBuf, Vec<u8>) {
    let primary = root.join("ramiz");
    let secondary = root.join("git-ramiz");
    let script = b"#!/bin/sh\nprintf 'ramiz 0.9.0\\n'\n";
    fs::write(&primary, script).unwrap();
    fs::write(&secondary, script).unwrap();
    fs::set_permissions(&primary, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&secondary, fs::Permissions::from_mode(0o755)).unwrap();
    (primary, secondary, script.to_vec())
}

#[cfg(unix)]
fn rollback_candidate_pair() -> (Vec<u8>, Vec<u8>) {
    let primary = format!(
        r####"#!/bin/sh
if [ "$1" = "--version" ]; then
    printf 'ramiz {}\n'
else
    exit 0
fi
"####,
        CANDIDATE_VERSION
    )
    .into_bytes();
    let secondary = format!(
        r####"#!/bin/sh
case "$0" in
    *.stage*)
        printf 'ramiz {}\n'
        ;;
    *)
        printf 'ramiz {}\n'
        exit 42
        ;;
esac
"####,
        CANDIDATE_VERSION, CANDIDATE_VERSION
    )
    .into_bytes();
    (primary, secondary)
}

#[test]
#[cfg(unix)]
fn signed_standalone_bootstrap_updates_the_real_pair_and_rejects_corruption() {
    let root = std::env::temp_dir().join(format!("ramiz-update-acceptance-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let (primary, secondary, predecessor) = predecessor_pair(&root);
    let candidate_primary = fs::read(env!("CARGO_BIN_EXE_ramiz")).unwrap();
    let candidate_secondary = fs::read(env!("CARGO_BIN_EXE_git-ramiz")).unwrap();
    assert!(!candidate_primary.is_empty() && !candidate_secondary.is_empty());
    assert_eq!(
        archive(CANDIDATE_VERSION, &candidate_primary, &candidate_secondary),
        archive(CANDIDATE_VERSION, &candidate_primary, &candidate_secondary),
        "fixture archive packing must be reproducible"
    );
    let (public, secret) = fixture_keypair();
    let public_key = public.to_box().unwrap().to_string();
    assert_ne!(public_key, update::MINISIGN_PUBLIC_KEY);

    // The server is deliberately assembled after the signed bytes so every
    // response is immutable for this acceptance run.
    let fixture = HttpFixture::new(
        |base| {
            fixture_files(
                base,
                &predecessor,
                &candidate_primary,
                &candidate_secondary,
                &public,
                &secret,
            )
        },
        12,
    );
    let provider = GithubReleaseProvider::test_fixture(fixture.base.clone(), public_key.clone());
    let verifier = PinnedMinisignVerifier::test_fixture(public_key).unwrap();
    let mut service = UpdateService {
        io: SystemUpdateIo,
        provider,
        verifier,
        host: HOST.into(),
    };

    let adopted = service
        .run(&UpdateRequest::new(false, &primary, "0.9.0").with_adopt(true))
        .unwrap();
    assert!(adopted.applied);
    assert_eq!(adopted.message, "standalone installation adopted");
    let manifest = primary.with_file_name(".ramiz-checksums.txt");
    let signature = primary.with_file_name(".ramiz-checksums.txt.minisig");
    let marker = primary.with_file_name(".ramiz-owner.json");
    let adopted_state = [
        fs::read(&primary).unwrap(),
        fs::read(&secondary).unwrap(),
        fs::read(&manifest).unwrap(),
        fs::read(&signature).unwrap(),
        fs::read(&marker).unwrap(),
    ];
    assert!(String::from_utf8_lossy(&adopted_state[4]).contains("0.9.0"));

    // Corrupting the local verification material must fail before any pair or
    // sidecar mutation, preserving the adopted predecessor byte-for-byte.
    fs::write(&signature, b"corrupted minisign material\n").unwrap();
    let error = service
        .run(&UpdateRequest::new(false, &primary, "0.9.0"))
        .unwrap_err();
    assert_eq!(error.code, "minisign_invalid");
    assert_eq!(fs::read(&primary).unwrap(), adopted_state[0]);
    assert_eq!(fs::read(&secondary).unwrap(), adopted_state[1]);
    assert_eq!(fs::read(&manifest).unwrap(), adopted_state[2]);
    assert_eq!(
        fs::read(&signature).unwrap(),
        b"corrupted minisign material\n"
    );
    assert_eq!(fs::read(&marker).unwrap(), adopted_state[4]);
    fs::write(&signature, adopted_state[3].as_slice()).unwrap();

    let updated = service
        .run(&UpdateRequest::new(false, &primary, "0.9.0"))
        .unwrap();
    assert!(updated.applied);
    assert_eq!(updated.installer, update::Installer::Standalone);
    assert_eq!(fs::read(&primary).unwrap(), candidate_primary);
    assert_eq!(fs::read(&secondary).unwrap(), candidate_secondary);
    assert!(String::from_utf8_lossy(&fs::read(&marker).unwrap()).contains(CANDIDATE_VERSION));
    assert_eq!(
        Command::new(&primary)
            .arg("--version")
            .output()
            .unwrap()
            .stdout,
        format!("ramiz {CANDIDATE_VERSION}\n").as_bytes()
    );
    assert_eq!(
        Command::new(&secondary)
            .arg("--version")
            .output()
            .unwrap()
            .stdout,
        format!("ramiz {CANDIDATE_VERSION}\n").as_bytes()
    );
    drop(service);
    drop(fixture);
    fs::remove_dir_all(root).unwrap();
}

#[test]
#[cfg(unix)]
fn signed_standalone_rollback_restores_pair_and_sidecars_after_activation_failure() {
    let root = std::env::temp_dir().join(format!(
        "ramiz-update-rollback-acceptance-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let (primary, secondary, predecessor) = predecessor_pair(&root);
    let (candidate_primary, candidate_secondary) = rollback_candidate_pair();
    let (public, secret) = fixture_keypair();
    let public_key = public.to_box().unwrap().to_string();
    let fixture = HttpFixture::new(
        |base| {
            fixture_files(
                base,
                &predecessor,
                &candidate_primary,
                &candidate_secondary,
                &public,
                &secret,
            )
        },
        12,
    );
    let provider = GithubReleaseProvider::test_fixture(fixture.base.clone(), public_key.clone());
    let verifier = PinnedMinisignVerifier::test_fixture(public_key).unwrap();
    let mut service = UpdateService {
        io: SystemUpdateIo,
        provider,
        verifier,
        host: HOST.into(),
    };

    let adopted = service
        .run(&UpdateRequest::new(false, &primary, "0.9.0").with_adopt(true))
        .unwrap();
    assert!(adopted.applied);
    let manifest = primary.with_file_name(".ramiz-checksums.txt");
    let signature = primary.with_file_name(".ramiz-checksums.txt.minisig");
    let marker = primary.with_file_name(".ramiz-owner.json");
    let adopted_state = [
        fs::read(&primary).unwrap(),
        fs::read(&secondary).unwrap(),
        fs::read(&manifest).unwrap(),
        fs::read(&signature).unwrap(),
        fs::read(&marker).unwrap(),
    ];

    let error = service
        .run(&UpdateRequest::new(false, &primary, "0.9.0"))
        .unwrap_err();
    assert_eq!(error.code, "update_rolled_back");
    assert!(error.message.contains("verification failed"));
    assert_eq!(fs::read(&primary).unwrap(), adopted_state[0]);
    assert_eq!(fs::read(&secondary).unwrap(), adopted_state[1]);
    assert_eq!(fs::read(&manifest).unwrap(), adopted_state[2]);
    assert_eq!(fs::read(&signature).unwrap(), adopted_state[3]);
    assert_eq!(fs::read(&marker).unwrap(), adopted_state[4]);
    assert!(
        !root
            .join(format!("ramiz.previous.ramiz-update-{CANDIDATE_VERSION}"))
            .exists()
    );
    assert!(
        !root
            .join(format!(
                "git-ramiz.previous.ramiz-update-{CANDIDATE_VERSION}"
            ))
            .exists()
    );
    drop(service);
    drop(fixture);
    fs::remove_dir_all(root).unwrap();
}

struct CargoAcceptanceProvider;

impl ReleaseProvider for CargoAcceptanceProvider {
    fn latest(&mut self, _host: &str) -> Result<ReleaseMetadata, UpdateFailure> {
        Ok(ReleaseMetadata {
            repository: update::REPOSITORY.into(),
            version: CANDIDATE_VERSION.into(),
            host: HOST.into(),
            primary_asset: "ramiz".into(),
            secondary_asset: "git-ramiz".into(),
            archive_sha256: "release-only-cargo-acceptance".into(),
            signature_verified: true,
        })
    }

    fn download(
        &mut self,
        _metadata: &ReleaseMetadata,
    ) -> Result<DownloadedRelease, UpdateFailure> {
        panic!("Cargo acceptance must use Cargo for installation")
    }
}

struct CargoAcceptanceVerifier;

impl ArtifactVerifier for CargoAcceptanceVerifier {
    fn verify(
        &mut self,
        _metadata: &ReleaseMetadata,
        _release: &DownloadedRelease,
    ) -> Result<(), UpdateFailure> {
        Ok(())
    }
}

struct EnvironmentRestore {
    cargo_home: Option<std::ffi::OsString>,
    cargo_install_root: Option<std::ffi::OsString>,
    path: Option<std::ffi::OsString>,
}

impl Drop for EnvironmentRestore {
    fn drop(&mut self) {
        // SAFETY: This ignored acceptance test runs as a single process and
        // restores each process-global variable immediately on scope exit.
        unsafe {
            restore_env("CARGO_HOME", self.cargo_home.take());
            restore_env("CARGO_INSTALL_ROOT", self.cargo_install_root.take());
            restore_env("PATH", self.path.take());
        }
    }
}

unsafe fn restore_env(name: &str, value: Option<std::ffi::OsString>) {
    if let Some(value) = value {
        unsafe { env::set_var(name, value) };
    } else {
        unsafe { env::remove_var(name) };
    }
}

#[test]
#[ignore = "release-only: requires the candidate version to exist on crates.io"]
fn cargo_release_updates_the_actual_previous_public_release() {
    const PREVIOUS_VERSION: &str = "1.0.1";
    let root = std::env::temp_dir().join(format!(
        "ramiz-cargo-release-acceptance-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let cargo_home = root.join("cargo-home");
    fs::create_dir_all(&cargo_home).unwrap();
    let restore = EnvironmentRestore {
        cargo_home: env::var_os("CARGO_HOME"),
        cargo_install_root: env::var_os("CARGO_INSTALL_ROOT"),
        path: env::var_os("PATH"),
    };
    // Force the service's real command selection to cargo, while preserving
    // the rest of the toolchain PATH needed by Cargo itself.
    let path = restore.path.clone().unwrap_or_default();
    let filtered_path = env::join_paths(
        env::split_paths(&path).filter(|directory| !directory.join("cargo-binstall").is_file()),
    )
    .unwrap();
    // SAFETY: EnvironmentRestore restores these scoped test overrides.
    unsafe {
        env::set_var("CARGO_HOME", &cargo_home);
        env::set_var("CARGO_INSTALL_ROOT", &root);
        env::set_var("PATH", filtered_path);
    }
    let install = Command::new("cargo")
        .args([
            "+1.85.0",
            "install",
            "ramiz",
            "--version",
            PREVIOUS_VERSION,
            "--locked",
            "--force",
            "--root",
            root.to_str().unwrap(),
        ])
        .env("CARGO_HOME", &cargo_home)
        .output()
        .unwrap();
    assert!(
        install.status.success(),
        "Cargo 1.85 could not install public Ramiz {PREVIOUS_VERSION}: {}",
        String::from_utf8_lossy(&install.stderr)
    );
    let primary = root.join("bin/ramiz");
    let secondary = root.join("bin/git-ramiz");
    assert_eq!(
        Command::new(&primary)
            .arg("--version")
            .output()
            .unwrap()
            .stdout,
        format!("ramiz {PREVIOUS_VERSION}\n").as_bytes()
    );
    assert_eq!(
        Command::new(&secondary)
            .arg("--version")
            .output()
            .unwrap()
            .stdout,
        format!("ramiz {PREVIOUS_VERSION}\n").as_bytes()
    );
    let record = root.join(".crates2.json");
    assert!(
        fs::read_to_string(&record)
            .unwrap()
            .contains(PREVIOUS_VERSION)
    );
    assert!(
        fs::read_to_string(root.join(".crates.toml"))
            .unwrap()
            .contains(PREVIOUS_VERSION)
    );
    let mut service = UpdateService {
        io: SystemUpdateIo,
        provider: CargoAcceptanceProvider,
        verifier: CargoAcceptanceVerifier,
        host: HOST.into(),
    };
    let result = service
        .run(&UpdateRequest::new(false, &primary, PREVIOUS_VERSION))
        .unwrap();
    assert!(result.applied);
    assert_eq!(result.commands.len(), 1);
    assert_eq!(result.commands[0].program, "cargo");
    assert_eq!(
        fs::read(&primary).unwrap(),
        fs::read(root.join("bin/ramiz")).unwrap()
    );
    assert_eq!(
        fs::read(&secondary).unwrap(),
        fs::read(root.join("bin/git-ramiz")).unwrap()
    );
    let installed_version = Command::new(&primary).arg("--version").output().unwrap();
    assert_eq!(
        installed_version.stdout,
        format!("ramiz {CANDIDATE_VERSION}\n").as_bytes()
    );
    let installed_companion = Command::new(&secondary).arg("--version").output().unwrap();
    assert_eq!(
        installed_companion.stdout,
        format!("ramiz {CANDIDATE_VERSION}\n").as_bytes()
    );
    let records = fs::read_to_string(&record).unwrap();
    assert!(records.contains(&format!("ramiz {CANDIDATE_VERSION}")));
    assert!(records.contains("git-ramiz"));
    drop(service);
    drop(restore);
    fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "release-only: downloads the previous public GitHub release"]
fn binstall_release_recognizes_the_actual_previous_public_installation() {
    const PREVIOUS_VERSION: &str = "1.0.1";
    let root = std::env::temp_dir().join(format!(
        "ramiz-binstall-release-acceptance-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let cargo_home = root.join("cargo-home");
    fs::create_dir_all(&cargo_home).unwrap();
    let restore = EnvironmentRestore {
        cargo_home: env::var_os("CARGO_HOME"),
        cargo_install_root: env::var_os("CARGO_INSTALL_ROOT"),
        path: env::var_os("PATH"),
    };
    let inherited_path = restore.path.clone().unwrap_or_default();
    let mut paths = vec![root.join("bin")];
    paths.extend(env::split_paths(&inherited_path));
    // SAFETY: EnvironmentRestore restores these scoped test overrides.
    unsafe {
        env::set_var("CARGO_HOME", &cargo_home);
        env::set_var("CARGO_INSTALL_ROOT", &root);
        env::set_var("PATH", env::join_paths(paths).unwrap());
    }
    let install = Command::new("cargo-binstall")
        .args([
            "--no-confirm",
            "--force",
            "--targets",
            HOST,
            "--root",
            root.to_str().unwrap(),
            &format!("ramiz@{PREVIOUS_VERSION}"),
        ])
        .output()
        .unwrap();
    assert!(
        install.status.success(),
        "cargo-binstall could not install public Ramiz {PREVIOUS_VERSION}: {}",
        String::from_utf8_lossy(&install.stderr)
    );
    let primary = root.join("bin/ramiz");
    let secondary = root.join("bin/git-ramiz");
    assert!(!root.join(".crates2.json").exists());
    assert!(
        fs::read_to_string(root.join("binstall/crates-v1.json"))
            .unwrap()
            .contains(PREVIOUS_VERSION)
    );
    let mut service = UpdateService {
        io: SystemUpdateIo,
        provider: CargoAcceptanceProvider,
        verifier: CargoAcceptanceVerifier,
        host: HOST.into(),
    };
    let result = service
        .run(&UpdateRequest::new(true, &primary, PREVIOUS_VERSION))
        .unwrap();
    assert_eq!(result.installer, update::Installer::Cargo);
    assert!(result.available);
    assert!(!result.applied);
    assert!(result.commands.is_empty());
    assert_eq!(
        Command::new(&primary)
            .arg("--version")
            .output()
            .unwrap()
            .stdout,
        format!("ramiz {PREVIOUS_VERSION}\n").as_bytes()
    );
    assert_eq!(
        Command::new(&secondary)
            .arg("--version")
            .output()
            .unwrap()
            .stdout,
        format!("ramiz {PREVIOUS_VERSION}\n").as_bytes()
    );
    assert!(
        fs::read_to_string(root.join(".crates.toml"))
            .unwrap()
            .contains(PREVIOUS_VERSION)
    );
    assert!(
        fs::read_to_string(root.join("binstall/crates-v1.json"))
            .unwrap()
            .contains(PREVIOUS_VERSION)
    );
    drop(service);
    drop(restore);
    fs::remove_dir_all(root).unwrap();
}
