//! Installer-aware self-update primitives.
//!
//! The release transport is deliberately injected.  A production caller must
//! provide a `ReleaseProvider` which talks only to the official GitHub release
//! endpoint and an `ArtifactVerifier` which checks the archive checksum and
//! signature/attestation before returning extracted binaries.  Keeping those
//! concerns outside this module makes the transaction deterministic and keeps
//! tests away from the network.

#![allow(clippy::result_large_err)]

use std::{
    collections::BTreeSet,
    env, fmt, fs,
    io::{self, Cursor, Read},
    path::{Component, Path, PathBuf},
    process::{Command, Output},
    sync::{Mutex, OnceLock},
    time::Duration,
};

use serde::{Deserialize, Serialize, Serializer, ser::SerializeMap};
use sha2::{Digest, Sha256};

pub const REPOSITORY: &str = "agensfield/ramiz";
pub const HOMEBREW_COMMAND: &str = "brew upgrade agensfield/tap/ramiz";
pub const MINISIGN_PUBLIC_KEY: &str = "untrusted comment: minisign public key DBDB291231A7A8D0\nRWTQqKcxEinb25UAhVtTVlhpVt+R7u8I3fVmbnl35gb3OO1dH6q7hR6d";

#[cfg(unix)]
static HELD_UPDATE_LOCK: OnceLock<Mutex<Option<fs::File>>> = OnceLock::new();

#[derive(Clone, Debug)]
pub struct UpdateRequest {
    pub check: bool,
    pub adopt: bool,
    pub executable: PathBuf,
    pub current_version: String,
}

impl UpdateRequest {
    pub fn new(
        check: bool,
        executable: impl Into<PathBuf>,
        current_version: impl Into<String>,
    ) -> Self {
        Self {
            check,
            adopt: false,
            executable: executable.into(),
            current_version: current_version.into(),
        }
    }

    pub fn with_adopt(mut self, adopt: bool) -> Self {
        self.adopt = adopt;
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Installer {
    Homebrew,
    Cargo,
    Standalone,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
}

impl CommandSpec {
    fn cargo_binstall(version: &str, root: &Path) -> Self {
        Self {
            program: "cargo-binstall".into(),
            args: vec![
                "ramiz".into(),
                "--version".into(),
                version.into(),
                "--force".into(),
                "--root".into(),
                root.to_string_lossy().into_owned(),
            ],
        }
    }

    fn cargo_install(version: &str, root: &Path) -> Self {
        Self {
            program: "cargo".into(),
            args: vec![
                "install".into(),
                "ramiz".into(),
                "--version".into(),
                version.into(),
                "--locked".into(),
                "--force".into(),
                "--root".into(),
                root.to_string_lossy().into_owned(),
            ],
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UpdateResult {
    pub installer: Installer,
    pub current_version: String,
    pub target_version: Option<String>,
    pub available: bool,
    pub applied: bool,
    pub changed: bool,
    #[serde(serialize_with = "serialize_path")]
    pub executable: PathBuf,
    #[serde(serialize_with = "serialize_path")]
    pub secondary_executable: PathBuf,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<CommandSpec>,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UpdateFailure {
    pub code: &'static str,
    pub message: String,
    pub installer: Option<Installer>,
    #[serde(serialize_with = "serialize_path")]
    pub executable: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<CommandSpec>,
    pub applied: bool,
    pub changed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rollback: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cleanup: Option<String>,
}

impl UpdateFailure {
    fn new(code: &'static str, message: impl Into<String>, executable: &Path) -> Self {
        Self {
            code,
            message: message.into(),
            installer: None,
            executable: executable.to_path_buf(),
            command: None,
            applied: false,
            changed: false,
            rollback: None,
            cleanup: None,
        }
    }

    fn with_installer(mut self, installer: Installer) -> Self {
        self.installer = Some(installer);
        self
    }

    fn with_command(mut self, command: CommandSpec) -> Self {
        self.command = Some(command);
        self
    }

    fn with_rollback(mut self, detail: impl Into<String>) -> Self {
        self.rollback = Some(detail.into());
        self
    }

    fn with_cleanup(mut self, detail: impl Into<String>) -> Self {
        self.cleanup = Some(detail.into());
        self
    }

    fn applied(mut self) -> Self {
        self.applied = true;
        self.changed = true;
        self
    }
}

impl fmt::Display for UpdateFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for UpdateFailure {}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StandaloneMarker {
    pub repository: String,
    pub version: String,
    pub host: String,
    pub primary_sha256: String,
    pub secondary_sha256: String,
    pub archive_sha256: String,
}

#[derive(Clone, Debug)]
pub struct ReleaseMetadata {
    pub repository: String,
    pub version: String,
    pub host: String,
    pub primary_asset: String,
    pub secondary_asset: String,
    pub archive_sha256: String,
    pub signature_verified: bool,
}

#[derive(Clone, Debug)]
pub struct DownloadedRelease {
    /// The verifier must return the two extracted executable bytes only after
    /// archive checksum and signature/attestation checks have succeeded.
    pub primary: Vec<u8>,
    pub secondary: Vec<u8>,
}

pub trait ReleaseProvider {
    fn latest(&mut self, host: &str) -> Result<ReleaseMetadata, UpdateFailure>;
    fn download(&mut self, metadata: &ReleaseMetadata) -> Result<DownloadedRelease, UpdateFailure>;

    fn trust_material(&self) -> Option<(&[u8], &[u8])> {
        None
    }

    fn validate_local_material(
        &mut self,
        _checksums: &[u8],
        _signature: &[u8],
        _version: &str,
        _host: &str,
    ) -> Result<(), UpdateFailure> {
        Ok(())
    }

    /// Fetch an immutable release tag. Providers should override this for
    /// standalone provenance checks; the fallback keeps test providers small.
    fn exact_release(
        &mut self,
        version: &str,
        host: &str,
    ) -> Result<ReleaseMetadata, UpdateFailure> {
        let metadata = self.latest(host)?;
        if metadata.version == version {
            Ok(metadata)
        } else {
            Err(UpdateFailure::new(
                "release_version_unavailable",
                format!("release {version} is unavailable"),
                Path::new("ramiz"),
            ))
        }
    }
}

pub trait ArtifactVerifier {
    fn verify(
        &mut self,
        metadata: &ReleaseMetadata,
        release: &DownloadedRelease,
    ) -> Result<(), UpdateFailure>;
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
    assets: Vec<GithubAsset>,
}

#[derive(Clone, Debug, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Deserialize)]
struct ReleaseSource {
    repository: String,
    version: String,
    target: String,
    commit: String,
}

#[derive(Clone, Debug)]
struct GithubReleaseState {
    archive: GithubAsset,
    checksum: Vec<u8>,
    signature: Vec<u8>,
}

/// Official GitHub release transport. The API base is configurable solely so
/// deterministic tests can point it at a local HTTP fixture; the default is
/// the fixed `api.github.com/repos/agensfield/ramiz` endpoint.
pub struct GithubReleaseProvider {
    agent: ureq::Agent,
    api_base: String,
    public_key: String,
    state: Option<GithubReleaseState>,
    #[cfg(test)]
    fixture_mode: bool,
}

impl GithubReleaseProvider {
    pub fn new() -> Self {
        Self::with_api_base_and_key(
            "https://api.github.com/repos/agensfield/ramiz",
            MINISIGN_PUBLIC_KEY,
        )
    }

    fn with_api_base_and_key(api_base: impl Into<String>, public_key: impl Into<String>) -> Self {
        Self {
            agent: ureq::Agent::config_builder()
                .https_only(true)
                .max_redirects(3)
                .save_redirect_history(true)
                .timeout_global(Some(Duration::from_secs(60)))
                .timeout_connect(Some(Duration::from_secs(15)))
                .timeout_recv_body(Some(Duration::from_secs(60)))
                .build()
                .new_agent(),
            api_base: api_base.into().trim_end_matches('/').into(),
            public_key: public_key.into(),
            state: None,
            #[cfg(test)]
            fixture_mode: false,
        }
    }

    /// Construct a provider for the in-process acceptance fixture.
    ///
    /// This is compiled only into tests. Production callers cannot select an
    /// API base, disable HTTPS, or replace the pinned public key.
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn test_fixture(api_base: impl Into<String>, public_key: impl Into<String>) -> Self {
        Self {
            agent: ureq::Agent::config_builder()
                .https_only(false)
                .max_redirects(3)
                .save_redirect_history(true)
                .timeout_global(Some(Duration::from_secs(60)))
                .timeout_connect(Some(Duration::from_secs(15)))
                .timeout_recv_body(Some(Duration::from_secs(60)))
                .build()
                .new_agent(),
            api_base: api_base.into().trim_end_matches('/').into(),
            public_key: public_key.into(),
            state: None,
            fixture_mode: true,
        }
    }

    fn ensure_asset(&self, url: &str) -> Result<(), UpdateFailure> {
        #[cfg(test)]
        if self.fixture_mode {
            return Ok(());
        }
        ensure_official_asset(url)
    }

    fn ensure_api(&self, url: &str) -> Result<(), UpdateFailure> {
        #[cfg(test)]
        if self.fixture_mode {
            return Ok(());
        }
        ensure_official_api(url)
    }

    fn get_bytes(&self, url: &str) -> Result<Vec<u8>, UpdateFailure> {
        let mut response = self
            .agent
            .get(url)
            .header("User-Agent", "ramiz-updater")
            .header("Accept", "application/octet-stream")
            .call()
            .map_err(|error| {
                UpdateFailure::new(
                    "release_download_failed",
                    error.to_string(),
                    Path::new("ramiz"),
                )
            })?;
        if let Some(history) = ureq::ResponseExt::get_redirect_history(&response) {
            for url in history {
                self.ensure_asset(&url.to_string())?;
            }
        }
        response
            .body_mut()
            .with_config()
            .limit(MAX_DOWNLOAD_BYTES)
            .read_to_vec()
            .map_err(|error| {
                UpdateFailure::new(
                    "release_download_failed",
                    error.to_string(),
                    Path::new("ramiz"),
                )
            })
    }

    fn get_json(&self, url: &str) -> Result<GithubRelease, UpdateFailure> {
        let mut response = self
            .agent
            .get(url)
            .header("User-Agent", "ramiz-updater")
            .header("Accept", "application/vnd.github+json")
            .call()
            .map_err(|error| {
                UpdateFailure::new(
                    "release_metadata_failed",
                    error.to_string(),
                    Path::new("ramiz"),
                )
            })?;
        if let Some(history) = ureq::ResponseExt::get_redirect_history(&response) {
            for url in history {
                self.ensure_api(&url.to_string())?;
            }
        }
        response
            .body_mut()
            .with_config()
            .limit(MAX_METADATA_BYTES)
            .read_json()
            .map_err(|error| {
                UpdateFailure::new(
                    "release_metadata_failed",
                    error.to_string(),
                    Path::new("ramiz"),
                )
            })
    }
}

const MAX_METADATA_BYTES: u64 = 2 * 1024 * 1024;
const MAX_DOWNLOAD_BYTES: u64 = 256 * 1024 * 1024;
const MAX_BINARY_BYTES: u64 = 128 * 1024 * 1024;
const MAX_ARCHIVE_MEMBERS: usize = 32;
const MAX_ARCHIVE_BYTES: u64 = 300 * 1024 * 1024;

impl Default for GithubReleaseProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl ReleaseProvider for GithubReleaseProvider {
    fn latest(&mut self, host: &str) -> Result<ReleaseMetadata, UpdateFailure> {
        let release = self.get_json(&format!("{}/releases/latest", self.api_base))?;
        self.metadata_for_release(release, host)
    }

    fn exact_release(
        &mut self,
        version: &str,
        host: &str,
    ) -> Result<ReleaseMetadata, UpdateFailure> {
        if !safe_version(version) || semver::Version::parse(version).is_err() {
            return Err(UpdateFailure::new(
                "invalid_release_metadata",
                "invalid release version",
                Path::new("ramiz"),
            ));
        }
        let release = self.get_json(&format!("{}/releases/tags/v{version}", self.api_base))?;
        self.metadata_for_release(release, host)
    }

    fn download(&mut self, metadata: &ReleaseMetadata) -> Result<DownloadedRelease, UpdateFailure> {
        let state = self.state.as_ref().ok_or_else(|| {
            UpdateFailure::new(
                "release_state_missing",
                "release metadata must be checked before download",
                Path::new("ramiz"),
            )
        })?;
        let expected = archive_name(&metadata.version, &metadata.host).ok_or_else(|| {
            UpdateFailure::new(
                "unsupported_host",
                "unsupported release host",
                Path::new("ramiz"),
            )
        })?;
        if state.archive.name != expected {
            return Err(UpdateFailure::new(
                "release_state_mismatch",
                "release metadata and downloaded asset do not match",
                Path::new("ramiz"),
            ));
        }
        let archive = self.get_bytes(&state.archive.browser_download_url)?;
        PinnedMinisignVerifier::new(&self.public_key)?
            .verify_archive(&archive, &metadata.archive_sha256)?;
        extract_pair(&archive, metadata)
            .map_err(|message| UpdateFailure::new("archive_invalid", message, Path::new("ramiz")))
    }

    fn trust_material(&self) -> Option<(&[u8], &[u8])> {
        self.state
            .as_ref()
            .map(|state| (state.checksum.as_slice(), state.signature.as_slice()))
    }

    fn validate_local_material(
        &mut self,
        checksums: &[u8],
        signature: &[u8],
        version: &str,
        host: &str,
    ) -> Result<(), UpdateFailure> {
        let archive = archive_name(version, host).ok_or_else(|| {
            UpdateFailure::new(
                "unsupported_host",
                "unsupported release host",
                Path::new("ramiz"),
            )
        })?;
        PinnedMinisignVerifier::new(&self.public_key)?
            .verify_checksums(checksums, signature, &archive)
            .map(|_| ())
    }
}

impl GithubReleaseProvider {
    fn metadata_for_release(
        &mut self,
        release: GithubRelease,
        host: &str,
    ) -> Result<ReleaseMetadata, UpdateFailure> {
        if release.prerelease || release.draft {
            return Err(UpdateFailure::new(
                "release_not_stable",
                "GitHub returned a draft or prerelease release",
                Path::new("ramiz"),
            ));
        }
        let mut asset_names = BTreeSet::new();
        if release
            .assets
            .iter()
            .any(|asset| !asset_names.insert(asset.name.as_str()))
        {
            return Err(UpdateFailure::new(
                "release_asset_invalid",
                "GitHub release contains duplicate asset names",
                Path::new("ramiz"),
            ));
        }
        let version = release
            .tag_name
            .strip_prefix('v')
            .unwrap_or(&release.tag_name)
            .to_owned();
        if !safe_version(&version) || semver::Version::parse(&version).is_err() {
            return Err(UpdateFailure::new(
                "invalid_release_metadata",
                "GitHub returned an invalid release tag",
                Path::new("ramiz"),
            ));
        }
        let archive_name = archive_name(&version, host).ok_or_else(|| {
            UpdateFailure::new(
                "unsupported_host",
                format!("no Ramiz release artifact for {host}"),
                Path::new("ramiz"),
            )
        })?;
        for name in [
            archive_name.as_str(),
            "checksums.txt",
            "checksums.txt.minisig",
            "attestation.jsonl",
        ] {
            if release
                .assets
                .iter()
                .filter(|asset| asset.name == name)
                .count()
                != 1
            {
                return Err(UpdateFailure::new(
                    "release_asset_invalid",
                    format!("GitHub release must contain exactly one {name}"),
                    Path::new("ramiz"),
                ));
            }
        }
        let archive = release
            .assets
            .iter()
            .find(|asset| asset.name == archive_name)
            .cloned()
            .ok_or_else(|| {
                UpdateFailure::new(
                    "release_asset_missing",
                    format!("GitHub release is missing {archive_name}"),
                    Path::new("ramiz"),
                )
            })?;
        let checksum = release
            .assets
            .iter()
            .find(|asset| asset.name == "checksums.txt")
            .cloned()
            .ok_or_else(|| {
                UpdateFailure::new(
                    "release_asset_missing",
                    "GitHub release is missing checksums.txt",
                    Path::new("ramiz"),
                )
            })?;
        let signature = release
            .assets
            .iter()
            .find(|asset| asset.name == "checksums.txt.minisig")
            .cloned()
            .ok_or_else(|| {
                UpdateFailure::new(
                    "release_asset_missing",
                    "GitHub release is missing checksums.txt.minisig",
                    Path::new("ramiz"),
                )
            })?;
        let attestation = release
            .assets
            .iter()
            .find(|asset| asset.name == "attestation.jsonl")
            .cloned()
            .ok_or_else(|| {
                UpdateFailure::new(
                    "release_asset_missing",
                    "GitHub release is missing attestation.jsonl",
                    Path::new("ramiz"),
                )
            })?;
        self.ensure_asset(&archive.browser_download_url)?;
        self.ensure_asset(&checksum.browser_download_url)?;
        self.ensure_asset(&signature.browser_download_url)?;
        self.ensure_asset(&attestation.browser_download_url)?;
        let checksum_bytes = self.get_bytes(&checksum.browser_download_url)?;
        let signature_bytes = self.get_bytes(&signature.browser_download_url)?;
        let digest = PinnedMinisignVerifier::new(&self.public_key)?.verify_checksums(
            &checksum_bytes,
            &signature_bytes,
            &archive_name,
        )?;
        self.state = Some(GithubReleaseState {
            archive,
            checksum: checksum_bytes,
            signature: signature_bytes,
        });
        Ok(ReleaseMetadata {
            repository: REPOSITORY.into(),
            version,
            host: host.into(),
            primary_asset: "ramiz".into(),
            secondary_asset: "git-ramiz".into(),
            archive_sha256: digest,
            signature_verified: true,
        })
    }
}

/// Verifies the repository's pinned Minisign key and SHA-256 manifest. The
/// release provider invokes these checks before exposing an archive, while
/// `verify_archive` is repeated immediately before extraction.
pub struct PinnedMinisignVerifier {
    public_key: String,
}

impl PinnedMinisignVerifier {
    fn new(public_key: impl Into<String>) -> Result<Self, UpdateFailure> {
        let public_key = public_key.into();
        minisign_verify::PublicKey::decode(&public_key).map_err(|error| {
            UpdateFailure::new(
                "invalid_minisign_key",
                error.to_string(),
                Path::new("ramiz"),
            )
        })?;
        Ok(Self { public_key })
    }

    pub fn pinned() -> Result<Self, UpdateFailure> {
        Self::new(MINISIGN_PUBLIC_KEY)
    }

    /// Construct a verifier for the in-process acceptance fixture.
    ///
    /// The production CLI can only use [`Self::pinned`].
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn test_fixture(public_key: impl Into<String>) -> Result<Self, UpdateFailure> {
        Self::new(public_key)
    }

    pub fn verify_checksums(
        &self,
        checksums: &[u8],
        signature: &[u8],
        archive_name: &str,
    ) -> Result<String, UpdateFailure> {
        let checksums_text = std::str::from_utf8(checksums).map_err(|error| {
            UpdateFailure::new(
                "checksum_manifest_invalid",
                error.to_string(),
                Path::new("ramiz"),
            )
        })?;
        let signature_text = std::str::from_utf8(signature).map_err(|error| {
            UpdateFailure::new("minisign_invalid", error.to_string(), Path::new("ramiz"))
        })?;
        let key = minisign_verify::PublicKey::decode(&self.public_key).map_err(|error| {
            UpdateFailure::new(
                "invalid_minisign_key",
                error.to_string(),
                Path::new("ramiz"),
            )
        })?;
        let parsed = minisign_verify::Signature::decode(signature_text).map_err(|error| {
            UpdateFailure::new("minisign_invalid", error.to_string(), Path::new("ramiz"))
        })?;
        key.verify(checksums, &parsed, false).map_err(|error| {
            UpdateFailure::new("minisign_invalid", error.to_string(), Path::new("ramiz"))
        })?;
        let mut selected = None;
        for line in checksums_text
            .lines()
            .filter(|line| !line.trim().is_empty())
        {
            let Some((digest, name)) = line.split_once("  ") else {
                return Err(UpdateFailure::new(
                    "checksum_manifest_invalid",
                    "malformed checksum entry",
                    Path::new("ramiz"),
                ));
            };
            if digest.len() != 64
                || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                || name.is_empty()
            {
                return Err(UpdateFailure::new(
                    "checksum_manifest_invalid",
                    "malformed checksum entry",
                    Path::new("ramiz"),
                ));
            }
            if name == archive_name && selected.replace(digest.to_ascii_lowercase()).is_some() {
                return Err(UpdateFailure::new(
                    "checksum_manifest_invalid",
                    "duplicate checksum entry",
                    Path::new("ramiz"),
                ));
            }
        }
        selected.ok_or_else(|| {
            UpdateFailure::new(
                "checksum_missing",
                format!("checksums.txt does not cover {archive_name}"),
                Path::new("ramiz"),
            )
        })
    }

    pub fn verify_archive(&self, archive: &[u8], expected_hex: &str) -> Result<(), UpdateFailure> {
        if expected_hex.len() != 64 || !expected_hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(UpdateFailure::new(
                "checksum_manifest_invalid",
                "invalid SHA-256 digest",
                Path::new("ramiz"),
            ));
        }
        let digest = Sha256::digest(archive);
        if hex_digest(&digest) != expected_hex.to_ascii_lowercase() {
            return Err(UpdateFailure::new(
                "checksum_mismatch",
                "downloaded archive SHA-256 does not match checksums.txt",
                Path::new("ramiz"),
            ));
        }
        Ok(())
    }
}

impl ArtifactVerifier for PinnedMinisignVerifier {
    fn verify(
        &mut self,
        metadata: &ReleaseMetadata,
        release: &DownloadedRelease,
    ) -> Result<(), UpdateFailure> {
        if !metadata.signature_verified {
            return Err(UpdateFailure::new(
                "artifact_trust_failed",
                "release trust was not established",
                Path::new("ramiz"),
            ));
        }
        if release.primary.is_empty() || release.secondary.is_empty() {
            return Err(UpdateFailure::new(
                "archive_invalid",
                "release archive did not contain both binaries",
                Path::new("ramiz"),
            ));
        }
        Ok(())
    }
}

fn archive_name(version: &str, host: &str) -> Option<String> {
    let (platform, arch) = match host {
        "aarch64-apple-darwin" => ("darwin", "arm64"),
        "x86_64-apple-darwin" => ("darwin", "amd64"),
        "aarch64-unknown-linux-musl" => ("linux", "arm64"),
        "x86_64-unknown-linux-musl" => ("linux", "amd64"),
        _ => return None,
    };
    Some(format!("ramiz_{version}_{platform}_{arch}.tar.gz"))
}

fn ensure_official_asset(url: &str) -> Result<(), UpdateFailure> {
    if url.starts_with("https://github.com/agensfield/ramiz/releases/download/")
        || (url.starts_with("https://objects.githubusercontent.com/")
            && url.contains("github-production-release-asset"))
        || url.starts_with("https://release-assets.githubusercontent.com/")
    {
        Ok(())
    } else {
        Err(UpdateFailure::new(
            "untrusted_release_source",
            "release asset URL is not an official Agensfield GitHub URL",
            Path::new("ramiz"),
        ))
    }
}

fn ensure_official_api(url: &str) -> Result<(), UpdateFailure> {
    if url.starts_with("https://api.github.com/repos/agensfield/ramiz/") {
        Ok(())
    } else {
        Err(UpdateFailure::new(
            "untrusted_release_source",
            "release metadata URL is outside the official GitHub repository",
            Path::new("ramiz"),
        ))
    }
}

fn extract_pair(archive: &[u8], metadata: &ReleaseMetadata) -> Result<DownloadedRelease, String> {
    let decoder = flate2::read::GzDecoder::new(Cursor::new(archive));
    let mut archive = tar::Archive::new(decoder);
    let mut primary = None;
    let mut secondary = None;
    let mut source = None;
    let mut seen = std::collections::BTreeSet::new();
    let mut member_count = 0;
    let mut total_size = 0u64;
    let entries = archive.entries().map_err(|error| error.to_string())?;
    for entry in entries {
        let mut entry = entry.map_err(|error| error.to_string())?;
        member_count += 1;
        total_size = total_size
            .checked_add(entry.size())
            .ok_or_else(|| "archive size accounting overflowed".to_owned())?;
        if member_count > MAX_ARCHIVE_MEMBERS || total_size > MAX_ARCHIVE_BYTES {
            return Err("archive exceeds member or decompressed size limits".into());
        }
        let path = entry
            .path()
            .map_err(|error| error.to_string())?
            .into_owned();
        if !safe_archive_path(&path) {
            return Err(format!("archive contains unsafe path {}", path.display()));
        }
        if !seen.insert(path.clone()) {
            return Err(format!(
                "archive contains duplicate path {}",
                path.display()
            ));
        }
        if !entry.header().entry_type().is_file() {
            return Err(format!(
                "archive contains non-file member {}",
                path.display()
            ));
        }
        if entry.size() > MAX_BINARY_BYTES {
            return Err(format!("archive member {} is too large", path.display()));
        }
        if path == Path::new(&metadata.primary_asset) {
            let mut bytes = Vec::with_capacity(entry.size() as usize);
            entry
                .read_to_end(&mut bytes)
                .map_err(|error| error.to_string())?;
            primary = Some(bytes);
        } else if path == Path::new(&metadata.secondary_asset) {
            let mut bytes = Vec::with_capacity(entry.size() as usize);
            entry
                .read_to_end(&mut bytes)
                .map_err(|error| error.to_string())?;
            secondary = Some(bytes);
        } else if path == Path::new("SOURCE.json") {
            let mut bytes = Vec::with_capacity(entry.size() as usize);
            entry
                .read_to_end(&mut bytes)
                .map_err(|error| error.to_string())?;
            source = Some(bytes);
        }
    }
    let source = source.ok_or_else(|| "archive does not contain SOURCE.json".to_owned())?;
    let source: ReleaseSource =
        serde_json::from_slice(&source).map_err(|error| error.to_string())?;
    if source.repository != format!("https://github.com/{REPOSITORY}")
        || source.version != metadata.version
        || source.target != metadata.host
        || source.commit.len() != 40
        || !source.commit.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("archive SOURCE.json provenance does not match the release".into());
    }
    match (primary, secondary) {
        (Some(primary), Some(secondary)) => Ok(DownloadedRelease { primary, secondary }),
        _ => Err("archive does not contain both requested binaries".into()),
    }
}

fn safe_archive_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0xf) as usize] as char);
    }
    output
}

fn serialize_path<S>(path: &Path, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = path.as_os_str().as_bytes();
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("display", &path.to_string_lossy())?;
        map.serialize_entry("bytes_base64", &base64_encode(bytes))?;
        map.end()
    }
    #[cfg(not(unix))]
    {
        let display = path.to_string_lossy();
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("display", &display)?;
        map.serialize_entry("bytes_base64", &base64_encode(display.as_bytes()))?;
        map.end()
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0] as usize;
        let second = chunk.get(1).copied().unwrap_or(0) as usize;
        let third = chunk.get(2).copied().unwrap_or(0) as usize;
        output.push(ALPHABET[first >> 2] as char);
        output.push(ALPHABET[((first & 3) << 4) | (second >> 4)] as char);
        output.push(if chunk.len() > 1 {
            ALPHABET[((second & 15) << 2) | (third >> 6)] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            ALPHABET[third & 63] as char
        } else {
            '='
        });
    }
    output
}

/// All effects used by update application are behind this trait.  The real
/// implementation is `SystemUpdateIo`; tests can model failures at each
/// individual rename/write/verification boundary.
pub trait UpdateIo {
    fn cargo_home(&self) -> Option<PathBuf>;
    fn cargo_install_root(&self) -> Option<PathBuf> {
        self.cargo_home()
    }
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;
    fn is_file(&self, path: &Path) -> bool;
    fn path_exists(&self, path: &Path) -> bool {
        self.is_file(path)
    }
    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "binary reads are unavailable",
        ))
    }
    fn read_string(&self, path: &Path) -> io::Result<String>;
    fn command_available(&self, command: &str) -> bool;
    fn run(&mut self, command: &Path, args: &[String]) -> io::Result<Output>;
    fn write_file(&mut self, path: &Path, bytes: &[u8]) -> io::Result<()>;
    fn create_staged_file(&mut self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.write_file(path, bytes)
    }
    fn set_owner_only(&mut self, path: &Path) -> io::Result<()> {
        let _ = path;
        Ok(())
    }
    fn set_executable(&mut self, path: &Path) -> io::Result<()>;
    fn rename(&mut self, from: &Path, to: &Path) -> io::Result<()>;
    fn remove_file(&mut self, path: &Path) -> io::Result<()>;
    fn acquire_lock(&mut self, path: &Path) -> io::Result<()>;
    fn release_lock(&mut self, path: &Path) -> io::Result<()>;
}

#[derive(Default)]
pub struct SystemUpdateIo;

impl UpdateIo for SystemUpdateIo {
    fn cargo_home(&self) -> Option<PathBuf> {
        env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
    }

    fn cargo_install_root(&self) -> Option<PathBuf> {
        env::var_os("CARGO_INSTALL_ROOT")
            .map(PathBuf::from)
            .or_else(|| {
                let home = self.cargo_home()?;
                for config in [home.join("config.toml"), home.join("config")] {
                    let Ok(contents) = fs::read_to_string(&config) else {
                        continue;
                    };
                    let mut in_install = false;
                    for line in contents.lines() {
                        let line = line.split('#').next().unwrap_or_default().trim();
                        if line.starts_with('[') && line.ends_with(']') {
                            in_install = line == "[install]";
                            continue;
                        }
                        if in_install {
                            if let Some(value) = line.strip_prefix("root =") {
                                let value = value.trim().trim_matches('"');
                                if !value.is_empty() {
                                    return Some(PathBuf::from(value));
                                }
                            }
                        }
                    }
                }
                None
            })
            .or_else(|| self.cargo_home())
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        fs::canonicalize(path)
    }

    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }

    fn path_exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        fs::read(path)
    }

    fn read_string(&self, path: &Path) -> io::Result<String> {
        fs::read_to_string(path)
    }

    fn command_available(&self, command: &str) -> bool {
        env::var_os("PATH")
            .into_iter()
            .flat_map(|path| env::split_paths(&path).collect::<Vec<_>>())
            .map(|directory| directory.join(command))
            .any(|candidate| candidate.is_file())
    }

    fn run(&mut self, command: &Path, args: &[String]) -> io::Result<Output> {
        Command::new(command).args(args).output()
    }

    fn write_file(&mut self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        use std::fs::OpenOptions;
        use std::io::Write;
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(path)?;
        file.write_all(bytes)
    }

    fn create_staged_file(&mut self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        use std::fs::OpenOptions;
        use std::io::Write;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(path)?;
        file.write_all(bytes)
    }

    fn set_executable(&mut self, path: &Path) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(path)?.permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions)?;
        }
        Ok(())
    }

    fn set_owner_only(&mut self, path: &Path) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(path)?.permissions();
            permissions.set_mode(0o600);
            fs::set_permissions(path, permissions)?;
        }
        Ok(())
    }

    fn rename(&mut self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
    }

    fn remove_file(&mut self, path: &Path) -> io::Result<()> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn acquire_lock(&mut self, path: &Path) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::{
                fs::OpenOptions,
                os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
            };
            let held = HELD_UPDATE_LOCK.get_or_init(|| Mutex::new(None));
            let mut held = held
                .lock()
                .map_err(|_| io::Error::other("update lock state poisoned"))?;
            if held.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "update lock already held",
                ));
            }
            let mut options = OpenOptions::new();
            options
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
            let file = options.open(path)?;
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
            *held = Some(file);
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "advisory locking is unavailable",
            ))
        }
    }

    fn release_lock(&mut self, path: &Path) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let held = HELD_UPDATE_LOCK.get_or_init(|| Mutex::new(None));
            let mut held = held
                .lock()
                .map_err(|_| io::Error::other("update lock state poisoned"))?;
            let Some(file) = held.take() else {
                return Ok(());
            };
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
            drop(file);
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
            let _ = path;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Ok(())
        }
    }
}

pub struct UpdateService<I, P, V> {
    pub io: I,
    pub provider: P,
    pub verifier: V,
    pub host: String,
}

impl<I, P, V> UpdateService<I, P, V>
where
    I: UpdateIo,
    P: ReleaseProvider,
    V: ArtifactVerifier,
{
    pub fn run(&mut self, request: &UpdateRequest) -> Result<UpdateResult, UpdateFailure> {
        let paths = BinaryPair::from_executable(&request.executable);
        let installer = discover_installer(&self.io, &paths.primary, &paths.secondary);
        if request.check {
            let (metadata, available) = self.latest(request, &paths, installer)?;
            let mut result = result_for(
                request,
                &paths,
                installer,
                Some(metadata.version),
                available,
            );
            result.message = if available {
                "update available"
            } else {
                "already current"
            }
            .into();
            return Ok(result);
        }
        if request.adopt && installer != Installer::Unknown {
            return Err(UpdateFailure::new(
                "adopt_known_manager",
                "cannot adopt an installation already proven to be owned by a package manager",
                &paths.primary,
            )
            .with_installer(installer));
        }
        match installer {
            Installer::Homebrew => Err(UpdateFailure::new(
                "homebrew_owned",
                format!(
                    "Homebrew owns {}; update it with `{HOMEBREW_COMMAND}`",
                    paths.primary.display()
                ),
                &paths.primary,
            )
            .with_installer(Installer::Homebrew)
            .with_command(CommandSpec {
                program: "brew".into(),
                args: vec!["upgrade".into(), "agensfield/tap/ramiz".into()],
            })),
            Installer::Cargo => self.run_cargo(request, paths),
            Installer::Standalone => self.run_standalone(request, paths),
            Installer::Unknown => {
                if request.adopt {
                    self.adopt_standalone(request, paths)
                } else {
                    Err(UpdateFailure::new(
                        "unknown_installer",
                        format!(
                            "cannot update {}; installer ownership is unknown; update it through its manager",
                            paths.primary.display()
                        ),
                        &paths.primary,
                    )
                    .with_installer(Installer::Unknown))
                }
            }
        }
    }

    fn adopt_standalone(
        &mut self,
        request: &UpdateRequest,
        paths: BinaryPair,
    ) -> Result<UpdateResult, UpdateFailure> {
        if self.io.path_exists(&manifest_path(&paths.primary))
            || self.io.path_exists(&signature_path(&paths.primary))
            || self.io.path_exists(&marker_path(&paths.primary))
        {
            return Err(UpdateFailure::new(
                "adoption_state_conflict",
                "refusing adoption because standalone ownership files already exist without a valid marker",
                &paths.primary,
            ));
        }
        if self.proves_standalone(&request.current_version, &paths) {
            let lock = host_lock_path();
            self.io.acquire_lock(&lock).map_err(|error| {
                UpdateFailure::new("update_locked", error.to_string(), &paths.primary)
            })?;
            if discover_installer(&self.io, &paths.primary, &paths.secondary) != Installer::Unknown
                || !self.proves_standalone(&request.current_version, &paths)
            {
                let _ = self.io.release_lock(&lock);
                return Err(UpdateFailure::new(
                    "installation_changed",
                    "installation ownership or executable pair changed while acquiring the update lock",
                    &paths.primary,
                ));
            }
            let Some((checksums, signature)) = self.provider.trust_material() else {
                let _ = self.io.release_lock(&lock);
                return Err(UpdateFailure::new(
                    "trust_material_missing",
                    "signed release manifest is unavailable",
                    &paths.primary,
                ));
            };
            let marker = match marker_for(
                &self.io,
                &paths,
                request.current_version.clone(),
                &self.host,
                checksums,
            ) {
                Ok(marker) => marker,
                Err(error) => {
                    let _ = self.io.release_lock(&lock);
                    return Err(error);
                }
            };
            let result =
                write_adoption_material(&mut self.io, &paths, &marker, checksums, signature);
            let unlock = self.io.release_lock(&lock);
            if let Err(error) = unlock {
                return Err(UpdateFailure::new(
                    "lock_release_failed",
                    error.to_string(),
                    &paths.primary,
                ));
            }
            result.map(|_| {
                let mut result = result_for(
                    request,
                    &paths,
                    Installer::Standalone,
                    Some(request.current_version.clone()),
                    false,
                );
                result.applied = true;
                result.changed = true;
                result.message = "standalone installation adopted".into();
                result
            })
        } else {
            Err(UpdateFailure::new(
                "standalone_proof_failed",
                "the installed pair does not match the signed release for its version",
                &paths.primary,
            ))
        }
    }

    fn proves_standalone(&mut self, current_version: &str, paths: &BinaryPair) -> bool {
        if !self.io.is_file(&paths.primary) || !self.io.is_file(&paths.secondary) {
            return false;
        }
        let Ok(metadata) = self.provider.exact_release(current_version, &self.host) else {
            return false;
        };
        if metadata.repository != REPOSITORY
            || metadata.version != current_version
            || metadata.host != self.host
            || !metadata.signature_verified
        {
            return false;
        }
        let Ok(release) = self.provider.download(&metadata) else {
            return false;
        };
        if self.verifier.verify(&metadata, &release).is_err() {
            return false;
        }
        self.io.read_file(&paths.primary).ok().as_deref() == Some(release.primary.as_slice())
            && self.io.read_file(&paths.secondary).ok().as_deref()
                == Some(release.secondary.as_slice())
    }

    fn latest(
        &mut self,
        request: &UpdateRequest,
        paths: &BinaryPair,
        installer: Installer,
    ) -> Result<(ReleaseMetadata, bool), UpdateFailure> {
        let metadata = self
            .provider
            .latest(&self.host)
            .map_err(|error| error.with_installer(installer))?;
        if metadata.repository != REPOSITORY {
            return Err(UpdateFailure::new(
                "untrusted_release_source",
                format!(
                    "release metadata is from {}, expected {REPOSITORY}",
                    metadata.repository
                ),
                &paths.primary,
            )
            .with_installer(installer));
        }
        if !safe_version(&metadata.version)
            || metadata.host != self.host
            || metadata.primary_asset.trim().is_empty()
            || metadata.secondary_asset.trim().is_empty()
        {
            return Err(UpdateFailure::new(
                "invalid_release_metadata",
                "release metadata has no matching version or host",
                &paths.primary,
            )
            .with_installer(installer));
        }
        let target = semver::Version::parse(&metadata.version).map_err(|error| {
            UpdateFailure::new(
                "invalid_release_metadata",
                format!("invalid release version: {error}"),
                &paths.primary,
            )
            .with_installer(installer)
        })?;
        let current = semver::Version::parse(&request.current_version).map_err(|error| {
            UpdateFailure::new(
                "invalid_current_version",
                format!("invalid current version: {error}"),
                &paths.primary,
            )
            .with_installer(installer)
        })?;
        let available = target > current;
        Ok((metadata, available))
    }

    fn run_cargo(
        &mut self,
        request: &UpdateRequest,
        paths: BinaryPair,
    ) -> Result<UpdateResult, UpdateFailure> {
        let installer = Installer::Cargo;
        let Some(cargo_home) = self.io.cargo_home() else {
            return Err(UpdateFailure::new(
                "cargo_root_unproven",
                "Cargo installation root is unavailable",
                &paths.primary,
            )
            .with_installer(installer));
        };
        let install_root = self.io.cargo_install_root().unwrap_or(cargo_home.clone());
        let root = install_root.join("bin");
        let primary = self
            .io
            .canonicalize(&paths.primary)
            .unwrap_or_else(|_| paths.primary.clone());
        let expected_primary = root.join(paths.primary.file_name().unwrap_or_default());
        let expected_primary = self
            .io
            .canonicalize(&expected_primary)
            .unwrap_or(expected_primary);
        if primary != expected_primary {
            return Err(UpdateFailure::new(
                "cargo_root_unproven",
                format!(
                    "{} is outside the Cargo installation root",
                    paths.primary.display()
                ),
                &paths.primary,
            )
            .with_installer(installer));
        }
        if !cargo_record_present(&self.io, &install_root, Some(&request.current_version)) {
            return Err(UpdateFailure::new(
                "cargo_record_unproven",
                "Cargo installation record does not prove that Ramiz owns this executable",
                &paths.primary,
            )
            .with_installer(installer));
        }
        if !self.io.is_file(&paths.primary) || !self.io.is_file(&paths.secondary) {
            return Err(UpdateFailure::new(
                "binary_pair_missing",
                "Cargo updates require both ramiz and git-ramiz executables",
                &paths.primary,
            )
            .with_installer(installer));
        }
        let (metadata, available) = self.latest(request, &paths, installer)?;
        let mut result = result_for(
            request,
            &paths,
            installer,
            Some(metadata.version.clone()),
            available,
        );
        if request.check || !available {
            result.message = if available {
                "update available"
            } else {
                "already current"
            }
            .into();
            return Ok(result);
        }

        let command = if self.io.command_available("cargo-binstall") {
            CommandSpec::cargo_binstall(&metadata.version, &install_root)
        } else {
            CommandSpec::cargo_install(&metadata.version, &install_root)
        };
        let lock = host_lock_path();
        self.io.acquire_lock(&lock).map_err(|error| {
            UpdateFailure::new(
                "update_locked",
                format!("another Ramiz update is running: {error}"),
                &paths.primary,
            )
            .with_installer(installer)
        })?;
        if discover_installer(&self.io, &paths.primary, &paths.secondary) != installer {
            let _ = self.io.release_lock(&lock);
            return Err(UpdateFailure::new(
                "installation_changed",
                "Cargo ownership changed while acquiring the update lock",
                &paths.primary,
            )
            .with_installer(installer));
        }
        let cargo_backup = match CargoBackup::capture(&self.io, &paths, &install_root) {
            Ok(backup) => backup,
            Err(error) => {
                let _ = self.io.release_lock(&lock);
                return Err(error.with_installer(installer));
            }
        };
        let operation = (|| {
            let output = self
                .io
                .run(Path::new(&command.program), &command.args)
                .map_err(|error| {
                    UpdateFailure::new("installer_failed", error.to_string(), &paths.primary)
                        .with_installer(installer)
                        .with_command(command.clone())
                })?;
            if !output.status.success() {
                return Err(UpdateFailure::new(
                    "installer_failed",
                    process_failure(&output),
                    &paths.primary,
                )
                .with_installer(installer)
                .with_command(command.clone()));
            }
            verify_binaries(&mut self.io, &paths, &metadata.version).map_err(|error| {
                error
                    .with_installer(installer)
                    .with_command(command.clone())
            })?;
            result.applied = true;
            result.changed = true;
            result.commands.push(command.clone());
            result.message = "update applied".into();
            Ok(result)
        })();
        let rollback_error = if operation.is_err() {
            cargo_backup.restore(&mut self.io).err()
        } else {
            None
        };
        let unlock = self.io.release_lock(&lock);
        match (operation, unlock) {
            (Ok(result), Ok(())) => Ok(result),
            (Ok(_), Err(error)) => {
                Err(
                    UpdateFailure::new("lock_release_failed", error.to_string(), &paths.primary)
                        .with_installer(installer),
                )
            }
            (Err(error), _) => {
                if let Some(rollback_error) = rollback_error {
                    return Err(UpdateFailure::new(
                        "cargo_rollback_failed",
                        format!("{error}; failed to restore the previous Cargo installation: {rollback_error}"),
                        &paths.primary,
                    )
                    .with_installer(installer));
                }
                Err(error)
            }
        }
    }

    fn run_standalone(
        &mut self,
        request: &UpdateRequest,
        paths: BinaryPair,
    ) -> Result<UpdateResult, UpdateFailure> {
        let installer = Installer::Standalone;
        if !self.io.is_file(&marker_path(&paths.primary))
            || !self.io.is_file(&manifest_path(&paths.primary))
            || !self.io.is_file(&signature_path(&paths.primary))
        {
            return Err(UpdateFailure::new(
                "standalone_unadopted",
                "standalone updates require a local adoption marker and signed manifest",
                &paths.primary,
            )
            .with_installer(installer));
        }
        let marker = self
            .io
            .read_string(&marker_path(&paths.primary))
            .ok()
            .and_then(|contents| serde_json::from_str::<StandaloneMarker>(&contents).ok())
            .ok_or_else(|| {
                UpdateFailure::new(
                    "standalone_unadopted",
                    "standalone owner marker is invalid",
                    &paths.primary,
                )
            })?;
        if marker.repository != REPOSITORY
            || marker.host != self.host
            || marker.version != request.current_version
        {
            return Err(UpdateFailure::new(
                "standalone_unadopted",
                "standalone owner marker does not match this host or repository",
                &paths.primary,
            )
            .with_installer(installer));
        }
        let checksums = self
            .io
            .read_file(&manifest_path(&paths.primary))
            .map_err(|error| {
                UpdateFailure::new("standalone_unadopted", error.to_string(), &paths.primary)
            })?;
        let signature = self
            .io
            .read_file(&signature_path(&paths.primary))
            .map_err(|error| {
                UpdateFailure::new("standalone_unadopted", error.to_string(), &paths.primary)
            })?;
        self.provider
            .validate_local_material(&checksums, &signature, &marker.version, &marker.host)
            .map_err(|error| error.with_installer(installer))?;
        let (metadata, available) = self.latest(request, &paths, installer)?;
        let mut result = result_for(
            request,
            &paths,
            installer,
            Some(metadata.version.clone()),
            available,
        );
        if request.check || !available {
            result.message = if available {
                "update available"
            } else {
                "already current"
            }
            .into();
            return Ok(result);
        }
        if metadata.archive_sha256.trim().is_empty() || !metadata.signature_verified {
            return Err(UpdateFailure::new(
                "artifact_trust_failed",
                "release must provide a SHA-256 checksum and verified Minisign signature",
                &paths.primary,
            )
            .with_installer(installer));
        }
        let lock = host_lock_path();
        self.io.acquire_lock(&lock).map_err(|error| {
            UpdateFailure::new(
                "update_locked",
                format!("another Ramiz update is running: {error}"),
                &paths.primary,
            )
            .with_installer(installer)
        })?;
        if discover_installer(&self.io, &paths.primary, &paths.secondary) != installer {
            let _ = self.io.release_lock(&lock);
            return Err(UpdateFailure::new(
                "installation_changed",
                "standalone ownership changed while acquiring the update lock",
                &paths.primary,
            )
            .with_installer(installer));
        }
        if !standalone_snapshot_unchanged(&self.io, &paths, &marker, &checksums, &signature)
            || self
                .provider
                .validate_local_material(&checksums, &signature, &marker.version, &marker.host)
                .is_err()
        {
            let _ = self.io.release_lock(&lock);
            return Err(UpdateFailure::new(
                "installation_changed",
                "standalone installation changed while acquiring the update lock",
                &paths.primary,
            )
            .with_installer(installer));
        }
        let release = match self
            .provider
            .download(&metadata)
            .map_err(|error| error.with_installer(installer))
        {
            Ok(release) => release,
            Err(error) => {
                let _ = self.io.release_lock(&lock);
                return Err(error);
            }
        };
        if let Err(error) = self
            .verifier
            .verify(&metadata, &release)
            .map_err(|error| error.with_installer(installer))
        {
            let _ = self.io.release_lock(&lock);
            return Err(error);
        }
        let trust_material = self
            .provider
            .trust_material()
            .map(|(checksums, signature)| (checksums.to_vec(), signature.to_vec()))
            .ok_or_else(|| {
                UpdateFailure::new(
                    "trust_material_missing",
                    "signed release manifest is unavailable",
                    &paths.primary,
                )
            });
        let trust_material = match trust_material {
            Ok(material) => material,
            Err(error) => {
                let _ = self.io.release_lock(&lock);
                return Err(error);
            }
        };
        let transaction =
            match PairTransaction::capture(&self.io, paths.clone(), metadata.version.clone()) {
                Ok(transaction) => transaction,
                Err(error) => {
                    let _ = self.io.release_lock(&lock);
                    return Err(error.with_installer(installer));
                }
            };
        let result =
            self.activate_standalone(request, transaction, &release, &trust_material, result);
        let unlock_result = self.io.release_lock(&lock);
        match (result, unlock_result) {
            (Ok(result), Ok(())) => Ok(result),
            (Ok(_), Err(error)) => {
                Err(
                    UpdateFailure::new("lock_release_failed", error.to_string(), &paths.primary)
                        .with_installer(installer),
                )
            }
            (Err(error), _) => Err(error),
        }
    }

    fn activate_standalone(
        &mut self,
        _request: &UpdateRequest,
        mut transaction: PairTransaction,
        release: &DownloadedRelease,
        trust_material: &(Vec<u8>, Vec<u8>),
        mut result: UpdateResult,
    ) -> Result<UpdateResult, UpdateFailure> {
        let installer = Installer::Standalone;
        let paths = transaction.paths.clone();
        if !self.io.is_file(&paths.primary) || !self.io.is_file(&paths.secondary) {
            return Err(UpdateFailure::new(
                "binary_pair_missing",
                "standalone updates require both ramiz and git-ramiz executables",
                &paths.primary,
            )
            .with_installer(installer));
        }
        self.io
            .create_staged_file(&transaction.stage_primary, &release.primary)
            .map_err(|error| {
                UpdateFailure::new("staging_failed", error.to_string(), &paths.primary)
                    .with_installer(installer)
            })?;
        if let Err(error) = self
            .io
            .create_staged_file(&transaction.stage_secondary, &release.secondary)
        {
            let _ = self.io.remove_file(&transaction.stage_primary);
            return Err(
                UpdateFailure::new("staging_failed", error.to_string(), &paths.secondary)
                    .with_installer(installer),
            );
        }
        for stage in [&transaction.stage_primary, &transaction.stage_secondary] {
            if let Err(error) = self.io.set_executable(stage) {
                let cleanup = self.cleanup_stages(&transaction);
                let mut failure = UpdateFailure::new("staging_failed", error.to_string(), stage)
                    .with_installer(installer);
                if let Err(detail) = cleanup {
                    failure = failure.with_cleanup(detail);
                }
                return Err(failure);
            }
        }
        if let Err(error) = verify_binaries(
            &mut self.io,
            &transaction.staged_pair(),
            &transaction.version,
        ) {
            let cleanup = self.cleanup_stages(&transaction);
            let mut error = error.with_installer(installer);
            if let Err(detail) = cleanup {
                error = error.with_cleanup(detail);
            }
            return Err(error);
        }

        let mut aux = AuxTransaction::capture(
            &self.io,
            &paths,
            &transaction.version,
            &self.host,
            release,
            trust_material,
        )
        .map_err(|error| error.with_installer(installer))?;
        if let Err(error) = aux.apply(&mut self.io) {
            let cleanup = self.cleanup_stages(&transaction);
            let mut error = error.with_installer(installer);
            if let Err(detail) = cleanup {
                error = error.with_cleanup(detail);
            }
            return Err(error);
        }

        if let Err(error) = transaction.move_old_to_backup(&mut self.io) {
            let rollback = transaction.rollback(&mut self.io);
            let aux_rollback = aux.rollback(&mut self.io);
            let mut failure =
                UpdateFailure::new("activation_failed", error.to_string(), &paths.primary)
                    .with_installer(installer);
            if let Err(detail) = rollback {
                failure = failure.with_rollback(detail);
            }
            if let Err(detail) = aux_rollback {
                failure = failure.with_cleanup(detail);
            }
            return Err(failure);
        }
        if let Err(error) = transaction.activate(&mut self.io) {
            let rollback = transaction.rollback(&mut self.io);
            let aux_rollback = aux.rollback(&mut self.io);
            let mut failure = UpdateFailure::new(
                "update_rolled_back",
                format!("activation failed and the previous executable pair was restored: {error}"),
                &paths.primary,
            )
            .with_installer(installer);
            if let Err(detail) = rollback {
                failure = failure.with_rollback(detail);
            }
            if let Err(detail) = aux_rollback {
                failure = failure.with_cleanup(detail);
            }
            return Err(failure);
        }
        if let Err(error) = verify_binaries(&mut self.io, &paths, &transaction.version) {
            let rollback = transaction.rollback(&mut self.io);
            let aux_rollback = aux.rollback(&mut self.io);
            let mut failure = UpdateFailure::new(
                "update_rolled_back",
                format!(
                    "verification failed and the previous executable pair was restored: {error}"
                ),
                &paths.primary,
            )
            .with_installer(installer);
            if let Err(detail) = rollback {
                failure = failure.with_rollback(detail);
            }
            if let Err(detail) = aux_rollback {
                failure = failure.with_cleanup(detail);
            }
            return Err(failure);
        }
        if let Err(cleanup) = transaction.cleanup_backups(&mut self.io) {
            return Err(UpdateFailure::new(
                "update_cleanup_failed",
                "update applied but cleanup of preserved files failed",
                &paths.primary,
            )
            .with_installer(installer)
            .with_cleanup(cleanup)
            .applied());
        }
        result.applied = true;
        result.changed = true;
        result.message = "update applied".into();
        Ok(result)
    }

    fn cleanup_stages(&mut self, transaction: &PairTransaction) -> Result<(), String> {
        let mut errors = Vec::new();
        if let Err(error) = self.io.remove_file(&transaction.stage_primary) {
            errors.push(format!("{}: {error}", transaction.stage_primary.display()));
        }
        if let Err(error) = self.io.remove_file(&transaction.stage_secondary) {
            errors.push(format!(
                "{}: {error}",
                transaction.stage_secondary.display()
            ));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

fn result_for(
    request: &UpdateRequest,
    paths: &BinaryPair,
    installer: Installer,
    target_version: Option<String>,
    available: bool,
) -> UpdateResult {
    UpdateResult {
        installer,
        current_version: request.current_version.clone(),
        target_version,
        available,
        applied: false,
        changed: false,
        executable: paths.primary.clone(),
        secondary_executable: paths.secondary.clone(),
        commands: Vec::new(),
        message: String::new(),
    }
}

#[derive(Clone, Debug)]
struct BinaryPair {
    primary: PathBuf,
    secondary: PathBuf,
}

impl BinaryPair {
    fn from_executable(executable: &Path) -> Self {
        let name = executable
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("ramiz");
        let primary = if name == "git-ramiz" {
            executable.with_file_name("ramiz")
        } else {
            executable.to_path_buf()
        };
        let secondary = if name == "git-ramiz" {
            executable.to_path_buf()
        } else {
            executable.with_file_name("git-ramiz")
        };
        Self { primary, secondary }
    }
}

fn discover_installer<I: UpdateIo>(io: &I, executable: &Path, secondary: &Path) -> Installer {
    let resolved = io
        .canonicalize(executable)
        .unwrap_or_else(|_| executable.to_path_buf());
    if is_homebrew_path(&resolved) {
        return Installer::Homebrew;
    }
    if cargo_owned(io, executable, &resolved) {
        return Installer::Cargo;
    }
    for receipt in receipt_paths(executable) {
        if let Ok(contents) = io.read_string(&receipt) {
            if let Ok(marker) = serde_json::from_str::<StandaloneMarker>(&contents) {
                if marker_proves_installation(io, &marker, executable, secondary) {
                    return Installer::Standalone;
                }
            }
        }
    }
    Installer::Unknown
}

fn marker_proves_installation<I: UpdateIo>(
    io: &I,
    marker: &StandaloneMarker,
    primary: &Path,
    secondary: &Path,
) -> bool {
    if marker.repository != REPOSITORY
        || !safe_version(&marker.version)
        || !io.is_file(&manifest_path(primary))
        || !io.is_file(&signature_path(primary))
    {
        return false;
    }
    let Ok(primary_bytes) = io.read_file(primary) else {
        return false;
    };
    let Ok(secondary_bytes) = io.read_file(secondary) else {
        return false;
    };
    hex_digest(&Sha256::digest(primary_bytes)) == marker.primary_sha256
        && hex_digest(&Sha256::digest(secondary_bytes)) == marker.secondary_sha256
}

fn standalone_snapshot_unchanged<I: UpdateIo>(
    io: &I,
    paths: &BinaryPair,
    marker: &StandaloneMarker,
    checksums: &[u8],
    signature: &[u8],
) -> bool {
    let current_marker = io
        .read_string(&marker_path(&paths.primary))
        .ok()
        .and_then(|contents| serde_json::from_str::<StandaloneMarker>(&contents).ok());
    let Some(current_marker) = current_marker else {
        return false;
    };
    if current_marker.repository != marker.repository
        || current_marker.version != marker.version
        || current_marker.host != marker.host
        || io.read_file(&manifest_path(&paths.primary)).ok().as_deref() != Some(checksums)
        || io
            .read_file(&signature_path(&paths.primary))
            .ok()
            .as_deref()
            != Some(signature)
    {
        return false;
    }
    marker_proves_installation(io, &current_marker, &paths.primary, &paths.secondary)
}

fn is_homebrew_path(path: &Path) -> bool {
    let known_root = [
        Path::new("/opt/homebrew/Cellar"),
        Path::new("/usr/local/Cellar"),
        Path::new("/home/linuxbrew/.linuxbrew/Cellar"),
    ]
    .iter()
    .any(|root| path.starts_with(root));
    if !known_root {
        return false;
    }
    let mut components = path.components();
    while let Some(component) = components.next() {
        if matches!(component, Component::Normal(name) if name == "Cellar")
            && matches!(components.next(), Some(Component::Normal(name)) if name == "ramiz")
        {
            return true;
        }
    }
    false
}

fn cargo_owned<I: UpdateIo>(io: &I, executable: &Path, resolved: &Path) -> bool {
    let Some(cargo_home) = io.cargo_home() else {
        return false;
    };
    let install_root = io.cargo_install_root().unwrap_or(cargo_home);
    let expected = install_root
        .join("bin")
        .join(executable.file_name().unwrap_or_default());
    let expected = io.canonicalize(&expected).unwrap_or(expected);
    (resolved == expected || executable == expected)
        && cargo_record_present(io, &install_root, None)
}

fn cargo_record_present<I: UpdateIo>(io: &I, install_root: &Path, version: Option<&str>) -> bool {
    let Ok(record) = io.read_string(&install_root.join(".crates2.json")) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&record) else {
        return false;
    };
    json_has_cargo_install(&value, version)
}

fn json_has_cargo_install(value: &serde_json::Value, version: Option<&str>) -> bool {
    match value {
        serde_json::Value::Object(object) => object.iter().any(|(key, value)| {
            if version.map_or_else(
                || key.starts_with("ramiz "),
                |version| {
                    key.as_str() == format!("ramiz {version}")
                        || key.starts_with(&format!("ramiz {version} ("))
                },
            ) {
                !key.contains("git+")
                    && !key.contains("path+")
                    && !key.contains("private")
                    && cargo_record_entry_ok(value)
            } else {
                json_has_cargo_install(value, version)
            }
        }),
        serde_json::Value::Array(values) => values
            .iter()
            .any(|value| json_has_cargo_install(value, version)),
        _ => false,
    }
}

fn unrelated_cargo_json_equal(baseline: &[u8], current: &[u8]) -> bool {
    let Ok(mut baseline) = serde_json::from_slice::<serde_json::Value>(baseline) else {
        return false;
    };
    let Ok(mut current) = serde_json::from_slice::<serde_json::Value>(current) else {
        return false;
    };
    strip_ramiz_entries(&mut baseline);
    strip_ramiz_entries(&mut current);
    baseline == current
}

fn strip_ramiz_entries(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            object.retain(|key, _| !key.starts_with("ramiz "));
            for value in object.values_mut() {
                strip_ramiz_entries(value);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                strip_ramiz_entries(value);
            }
        }
        _ => {}
    }
}

fn cargo_record_entry_ok(value: &serde_json::Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    if let Some(source) = object.get("source").and_then(serde_json::Value::as_str) {
        if !source.is_empty() && source != "registry+https://github.com/rust-lang/crates.io-index" {
            return false;
        }
    }
    let Some(bins) = object.get("bins").and_then(serde_json::Value::as_array) else {
        return false;
    };
    let bins: BTreeSet<&str> = bins.iter().filter_map(serde_json::Value::as_str).collect();
    bins.contains("ramiz") && bins.contains("git-ramiz")
}

fn receipt_paths(executable: &Path) -> [PathBuf; 1] {
    [executable.with_file_name(".ramiz-owner.json")]
}

fn marker_path(executable: &Path) -> PathBuf {
    executable.with_file_name(".ramiz-owner.json")
}

fn manifest_path(executable: &Path) -> PathBuf {
    executable.with_file_name(".ramiz-checksums.txt")
}

fn signature_path(executable: &Path) -> PathBuf {
    executable.with_file_name(".ramiz-checksums.txt.minisig")
}

fn marker_for<I: UpdateIo>(
    io: &I,
    paths: &BinaryPair,
    version: String,
    host: &str,
    checksums: &[u8],
) -> Result<StandaloneMarker, UpdateFailure> {
    let primary = io.read_file(&paths.primary).map_err(|error| {
        UpdateFailure::new("standalone_proof_failed", error.to_string(), &paths.primary)
    })?;
    let secondary = io.read_file(&paths.secondary).map_err(|error| {
        UpdateFailure::new(
            "standalone_proof_failed",
            error.to_string(),
            &paths.secondary,
        )
    })?;
    Ok(StandaloneMarker {
        repository: REPOSITORY.into(),
        version,
        host: host.into(),
        primary_sha256: hex_digest(&Sha256::digest(primary)),
        secondary_sha256: hex_digest(&Sha256::digest(secondary)),
        archive_sha256: hex_digest(&Sha256::digest(checksums)),
    })
}

fn write_adoption_material<I: UpdateIo>(
    io: &mut I,
    paths: &BinaryPair,
    marker: &StandaloneMarker,
    checksums: &[u8],
    signature: &[u8],
) -> Result<(), UpdateFailure> {
    let manifest = manifest_path(&paths.primary);
    let detached = signature_path(&paths.primary);
    let owner = marker_path(&paths.primary);
    io.write_file(&manifest, checksums).map_err(|error| {
        UpdateFailure::new("adoption_failed", error.to_string(), &paths.primary)
    })?;
    if let Err(error) = io.write_file(&detached, signature) {
        let _ = io.remove_file(&manifest);
        return Err(UpdateFailure::new(
            "adoption_failed",
            error.to_string(),
            &paths.primary,
        ));
    }
    if let Err(error) = io.write_file(
        &owner,
        &serde_json::to_vec(marker).map_err(|error| {
            UpdateFailure::new("adoption_failed", error.to_string(), &paths.primary)
        })?,
    ) {
        let _ = io.remove_file(&manifest);
        let _ = io.remove_file(&detached);
        return Err(UpdateFailure::new(
            "adoption_failed",
            error.to_string(),
            &paths.primary,
        ));
    }
    for path in [&manifest, &detached, &owner] {
        if let Err(error) = io.set_owner_only(path) {
            let _ = io.remove_file(&manifest);
            let _ = io.remove_file(&detached);
            let _ = io.remove_file(&owner);
            return Err(UpdateFailure::new(
                "adoption_failed",
                error.to_string(),
                &paths.primary,
            ));
        }
    }
    Ok(())
}

fn verify_binaries<I: UpdateIo>(
    io: &mut I,
    paths: &BinaryPair,
    version: &str,
) -> Result<(), UpdateFailure> {
    for binary in [&paths.primary, &paths.secondary] {
        let output = io.run(binary, &["--version".into()]).map_err(|error| {
            UpdateFailure::new("verification_failed", error.to_string(), binary)
        })?;
        let expected = format!("ramiz {version}");
        if !output.status.success() || String::from_utf8_lossy(&output.stdout).trim() != expected {
            return Err(UpdateFailure::new(
                "verification_failed",
                format!("{} did not report version {version}", binary.display()),
                binary,
            ));
        }
        let output = io.run(binary, &["--help".into()]).map_err(|error| {
            UpdateFailure::new("verification_failed", error.to_string(), binary)
        })?;
        if !output.status.success() {
            return Err(UpdateFailure::new(
                "verification_failed",
                format!("{} failed its help smoke check", binary.display()),
                binary,
            ));
        }
    }
    Ok(())
}

fn process_failure(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.trim().is_empty() {
        format!("installer exited with {}", output.status)
    } else {
        format!("installer failed: {}", stderr.trim())
    }
}

fn safe_version(version: &str) -> bool {
    !version.is_empty()
        && version.bytes().any(|byte| byte.is_ascii_digit())
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
}

fn host_lock_path() -> PathBuf {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no arguments or memory-safety preconditions.
        let user = unsafe { libc::geteuid() };
        PathBuf::from(format!("/tmp/ramiz-update-{user}.lock"))
    }
    #[cfg(not(unix))]
    {
        env::temp_dir().join("ramiz-update.lock")
    }
}

fn snapshot_optional<I: UpdateIo>(io: &I, path: &Path) -> Result<Option<Vec<u8>>, io::Error> {
    match io.read_file(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

struct CargoBackup {
    binaries: [(PathBuf, Option<Vec<u8>>); 2],
    records: [(PathBuf, Option<Vec<u8>>); 2],
}

impl CargoBackup {
    fn capture<I: UpdateIo>(
        io: &I,
        paths: &BinaryPair,
        install_root: &Path,
    ) -> Result<Self, UpdateFailure> {
        if snapshot_optional(io, &install_root.join(".crates2.json"))
            .map_err(|error| {
                UpdateFailure::new("cargo_snapshot_failed", error.to_string(), install_root)
            })?
            .is_none()
        {
            return Err(UpdateFailure::new(
                "cargo_record_unproven",
                "Cargo .crates2.json record is required",
                install_root,
            ));
        }
        let primary = snapshot_optional(io, &paths.primary).map_err(|error| {
            UpdateFailure::new("cargo_snapshot_failed", error.to_string(), &paths.primary)
        })?;
        let secondary = snapshot_optional(io, &paths.secondary).map_err(|error| {
            UpdateFailure::new("cargo_snapshot_failed", error.to_string(), &paths.secondary)
        })?;
        let record_paths = [
            install_root.join(".crates2.json"),
            install_root.join(".crates.toml"),
        ];
        let records = record_paths.map(|path| {
            let contents = snapshot_optional(io, &path).map_err(|error| {
                UpdateFailure::new("cargo_snapshot_failed", error.to_string(), &path)
            })?;
            Ok((path, contents))
        });
        Ok(Self {
            binaries: [
                (paths.primary.clone(), primary),
                (paths.secondary.clone(), secondary),
            ],
            records: [records[0].clone()?, records[1].clone()?],
        })
    }

    fn restore<I: UpdateIo>(&self, io: &mut I) -> Result<(), io::Error> {
        for (path, bytes) in &self.binaries {
            match bytes {
                Some(bytes) => {
                    io.write_file(path, bytes)?;
                    io.set_executable(path)?;
                }
                None => io.remove_file(path)?,
            }
        }
        if let (Some(Some(baseline)), Some(current)) = (
            self.records
                .iter()
                .find(|(path, _)| path.ends_with(".crates2.json"))
                .map(|(_, bytes)| bytes),
            snapshot_optional(
                io,
                &self
                    .records
                    .iter()
                    .find(|(path, _)| path.ends_with(".crates2.json"))
                    .unwrap()
                    .0,
            )?,
        ) {
            if !unrelated_cargo_json_equal(baseline, &current) {
                return Err(io::Error::other(
                    "unrelated Cargo installation changed while updating",
                ));
            }
        }
        for (path, bytes) in &self.records {
            match bytes {
                Some(bytes) => io.write_file(path, bytes)?,
                None => io.remove_file(path)?,
            }
        }
        Ok(())
    }
}

struct AuxTransaction {
    paths: [PathBuf; 3],
    old: [Option<Vec<u8>>; 3],
    new: [Vec<u8>; 3],
    mutated: [bool; 3],
}

impl AuxTransaction {
    fn capture<I: UpdateIo>(
        io: &I,
        paths: &BinaryPair,
        version: &str,
        host: &str,
        release: &DownloadedRelease,
        trust_material: &(Vec<u8>, Vec<u8>),
    ) -> Result<Self, UpdateFailure> {
        let marker = StandaloneMarker {
            repository: REPOSITORY.into(),
            version: version.into(),
            host: host.into(),
            primary_sha256: hex_digest(&Sha256::digest(&release.primary)),
            secondary_sha256: hex_digest(&Sha256::digest(&release.secondary)),
            archive_sha256: hex_digest(&Sha256::digest(&trust_material.0)),
        };
        let aux_paths = [
            manifest_path(&paths.primary),
            signature_path(&paths.primary),
            marker_path(&paths.primary),
        ];
        let old = [
            snapshot_optional(io, &aux_paths[0]).map_err(|error| {
                UpdateFailure::new("staging_snapshot_failed", error.to_string(), &aux_paths[0])
            })?,
            snapshot_optional(io, &aux_paths[1]).map_err(|error| {
                UpdateFailure::new("staging_snapshot_failed", error.to_string(), &aux_paths[1])
            })?,
            snapshot_optional(io, &aux_paths[2]).map_err(|error| {
                UpdateFailure::new("staging_snapshot_failed", error.to_string(), &aux_paths[2])
            })?,
        ];
        Ok(Self {
            paths: aux_paths,
            old,
            new: [
                trust_material.0.clone(),
                trust_material.1.clone(),
                serde_json::to_vec(&marker).unwrap_or_default(),
            ],
            mutated: [false, false, false],
        })
    }

    fn apply<I: UpdateIo>(&mut self, io: &mut I) -> Result<(), UpdateFailure> {
        for index in 0..self.paths.len() {
            self.mutated[index] = true;
            if let Err(error) = io.write_file(&self.paths[index], &self.new[index]) {
                let cleanup = self.rollback(io);
                let mut failure =
                    UpdateFailure::new("staging_failed", error.to_string(), &self.paths[index]);
                if let Err(detail) = cleanup {
                    failure = failure.with_cleanup(detail);
                }
                return Err(failure);
            }
            if let Err(error) = io.set_owner_only(&self.paths[index]) {
                let cleanup = self.rollback(io);
                let mut failure =
                    UpdateFailure::new("staging_failed", error.to_string(), &self.paths[index]);
                if let Err(detail) = cleanup {
                    failure = failure.with_cleanup(detail);
                }
                return Err(failure);
            }
        }
        Ok(())
    }

    fn rollback<I: UpdateIo>(&self, io: &mut I) -> Result<(), String> {
        let mut errors = Vec::new();
        for ((path, old), mutated) in self.paths.iter().zip(self.old.iter()).zip(self.mutated) {
            if !mutated {
                continue;
            }
            if let Err(error) = io.remove_file(path) {
                errors.push(format!("{}: remove: {error}", path.display()));
            }
            if let Some(bytes) = old {
                if let Err(error) = io.write_file(path, bytes) {
                    errors.push(format!("{}: restore: {error}", path.display()));
                }
                if let Err(error) = io.set_owner_only(path) {
                    errors.push(format!("{}: permissions: {error}", path.display()));
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

struct PairTransaction {
    paths: BinaryPair,
    version: String,
    stage_primary: PathBuf,
    stage_secondary: PathBuf,
    backup_primary: PathBuf,
    backup_secondary: PathBuf,
    primary_backed_up: bool,
    secondary_backed_up: bool,
    primary_activated: bool,
    secondary_activated: bool,
    old_primary: Vec<u8>,
    old_secondary: Vec<u8>,
}

impl PairTransaction {
    fn capture<I: UpdateIo>(
        io: &I,
        paths: BinaryPair,
        version: String,
    ) -> Result<Self, UpdateFailure> {
        let old_primary = io.read_file(&paths.primary).map_err(|error| {
            UpdateFailure::new("staging_snapshot_failed", error.to_string(), &paths.primary)
        })?;
        let old_secondary = io.read_file(&paths.secondary).map_err(|error| {
            UpdateFailure::new(
                "staging_snapshot_failed",
                error.to_string(),
                &paths.secondary,
            )
        })?;
        let suffix = format!(".ramiz-update-{version}");
        Ok(Self {
            stage_primary: paths.primary.with_extension(format!("stage{suffix}")),
            stage_secondary: paths.secondary.with_extension(format!("stage{suffix}")),
            backup_primary: paths.primary.with_extension(format!("previous{suffix}")),
            backup_secondary: paths.secondary.with_extension(format!("previous{suffix}")),
            paths,
            version,
            primary_backed_up: false,
            secondary_backed_up: false,
            primary_activated: false,
            secondary_activated: false,
            old_primary,
            old_secondary,
        })
    }

    fn staged_pair(&self) -> BinaryPair {
        BinaryPair {
            primary: self.stage_primary.clone(),
            secondary: self.stage_secondary.clone(),
        }
    }

    fn move_old_to_backup<I: UpdateIo>(&mut self, io: &mut I) -> io::Result<()> {
        if io.path_exists(&self.backup_primary) || io.path_exists(&self.backup_secondary) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "update backup already exists",
            ));
        }
        io.rename(&self.paths.primary, &self.backup_primary)?;
        self.primary_backed_up = true;
        io.rename(&self.paths.secondary, &self.backup_secondary)?;
        self.secondary_backed_up = true;
        Ok(())
    }

    fn activate<I: UpdateIo>(&mut self, io: &mut I) -> io::Result<()> {
        io.rename(&self.stage_primary, &self.paths.primary)?;
        self.primary_activated = true;
        io.rename(&self.stage_secondary, &self.paths.secondary)?;
        self.secondary_activated = true;
        Ok(())
    }

    fn rollback<I: UpdateIo>(&mut self, io: &mut I) -> Result<(), String> {
        let mut errors = Vec::new();
        if self.primary_activated {
            if let Err(error) = io.remove_file(&self.paths.primary) {
                errors.push(format!("{}: remove: {error}", self.paths.primary.display()));
            }
        }
        if self.secondary_activated {
            if let Err(error) = io.remove_file(&self.paths.secondary) {
                errors.push(format!(
                    "{}: remove: {error}",
                    self.paths.secondary.display()
                ));
            }
        }
        if self.primary_backed_up {
            if let Err(error) = io.rename(&self.backup_primary, &self.paths.primary) {
                errors.push(format!(
                    "{}: restore: {error}",
                    self.paths.primary.display()
                ));
            }
        }
        if self.secondary_backed_up {
            if let Err(error) = io.rename(&self.backup_secondary, &self.paths.secondary) {
                errors.push(format!(
                    "{}: restore: {error}",
                    self.paths.secondary.display()
                ));
            }
        }
        match io.read_file(&self.paths.primary) {
            Ok(bytes) if bytes == self.old_primary => {}
            Ok(_) => errors.push(format!(
                "{}: restored bytes differ",
                self.paths.primary.display()
            )),
            Err(error) => errors.push(format!(
                "{}: verify restore: {error}",
                self.paths.primary.display()
            )),
        }
        match io.read_file(&self.paths.secondary) {
            Ok(bytes) if bytes == self.old_secondary => {}
            Ok(_) => errors.push(format!(
                "{}: restored bytes differ",
                self.paths.secondary.display()
            )),
            Err(error) => errors.push(format!(
                "{}: verify restore: {error}",
                self.paths.secondary.display()
            )),
        }
        if let Err(error) = io.remove_file(&self.stage_primary) {
            errors.push(format!(
                "{}: cleanup: {error}",
                self.stage_primary.display()
            ));
        }
        if let Err(error) = io.remove_file(&self.stage_secondary) {
            errors.push(format!(
                "{}: cleanup: {error}",
                self.stage_secondary.display()
            ));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    fn cleanup_backups<I: UpdateIo>(&self, io: &mut I) -> Result<(), String> {
        let mut errors = Vec::new();
        for path in [
            &self.backup_primary,
            &self.backup_secondary,
            &self.stage_primary,
            &self.stage_secondary,
        ] {
            if let Err(error) = io.remove_file(path) {
                errors.push(format!("{}: {error}", path.display()));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

/// The system-facing constructor uses the official GitHub endpoint and the
/// public key pinned in `release/minisign.pub`.
pub fn system_service()
-> UpdateService<SystemUpdateIo, GithubReleaseProvider, PinnedMinisignVerifier> {
    UpdateService {
        io: SystemUpdateIo,
        provider: GithubReleaseProvider::new(),
        verifier: PinnedMinisignVerifier::pinned()
            .expect("the pinned Ramiz Minisign public key must remain valid"),
        host: host_triple(),
    }
}

fn host_triple() -> String {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin".into()
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "x86_64-apple-darwin".into()
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "aarch64-unknown-linux-musl".into()
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "x86_64-unknown-linux-musl".into()
    } else {
        format!("{}-{}", env::consts::ARCH, env::consts::OS)
    }
}
