use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};

#[derive(Debug)]
pub struct GitFailure {
    pub operation: &'static str,
    pub output: Output,
}

impl std::fmt::Display for GitFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let stderr = String::from_utf8_lossy(&self.output.stderr);
        let stdout = String::from_utf8_lossy(&self.output.stdout);
        match (stdout.trim(), stderr.trim()) {
            ("", "") => write!(formatter, "{} failed", self.operation),
            (stdout, "") => write!(formatter, "{} failed; stdout: {stdout}", self.operation),
            ("", stderr) => write!(formatter, "{} failed; stderr: {stderr}", self.operation),
            (stdout, stderr) => write!(
                formatter,
                "{} failed; stdout: {stdout}; stderr: {stderr}",
                self.operation
            ),
        }
    }
}

pub fn checked(cwd: &Path, operation: &'static str, args: &[&OsStr]) -> Result<Output, GitFailure> {
    let output = command(cwd, args)
        .output()
        .map_err(|error| synthetic_failure(operation, error))?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(GitFailure { operation, output })
    }
}

pub fn checked_with_input(
    cwd: &Path,
    operation: &'static str,
    args: &[&OsStr],
    input: &[u8],
) -> Result<Output, GitFailure> {
    let mut child = command(cwd, args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| synthetic_failure(operation, error))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let input = input.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let output = child
        .wait_with_output()
        .map_err(|error| synthetic_failure(operation, error))?;
    let write_result = writer.join().map_err(|_| {
        synthetic_failure(
            operation,
            std::io::Error::other("Git stdin writer panicked"),
        )
    })?;
    if output.status.success() {
        write_result
            .map(|()| output)
            .map_err(|error| synthetic_failure(operation, error))
    } else {
        Err(GitFailure { operation, output })
    }
}

fn command(cwd: &Path, args: &[&OsStr]) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0");
    for variable in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_PREFIX",
        "GIT_NAMESPACE",
        "GIT_CEILING_DIRECTORIES",
    ] {
        command.env_remove(variable);
    }
    command
}

fn synthetic_failure(operation: &'static str, error: std::io::Error) -> GitFailure {
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt;

    GitFailure {
        operation,
        output: Output {
            status: std::process::ExitStatus::from_raw(1),
            stdout: Vec::new(),
            stderr: error.to_string().into_bytes(),
        },
    }
}

pub fn require_version(cwd: &Path) -> Result<String, GitFailure> {
    let output = checked(cwd, "git --version", &[OsStr::new("--version")])?;
    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let version = value.strip_prefix("git version ").unwrap_or(&value);
    let mut numbers = version
        .split('.')
        .filter_map(|part| part.parse::<u32>().ok());
    let major = numbers.next().unwrap_or(0);
    let minor = numbers.next().unwrap_or(0);
    if (major, minor) < (2, 42) {
        let mut output = output;
        output.status = failure_status();
        output.stderr = format!("Ramiz requires Git 2.42 or newer; found {version}").into_bytes();
        return Err(GitFailure {
            operation: "git version check",
            output,
        });
    }
    Ok(version.to_owned())
}

fn failure_status() -> std::process::ExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(1)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(1)
    }
}

pub fn repo_root(cwd: &Path) -> Result<Option<PathBuf>, GitFailure> {
    let output = checked(
        cwd,
        "git repository discovery",
        &[OsStr::new("rev-parse"), OsStr::new("--show-toplevel")],
    );
    match output {
        Ok(output) => Ok(Some(PathBuf::from(trim_line(&output.stdout)))),
        Err(_error) if is_bare(cwd).unwrap_or(false) => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn common_dir(cwd: &Path) -> Result<PathBuf, GitFailure> {
    let output = checked(
        cwd,
        "git common directory discovery",
        &[
            OsStr::new("rev-parse"),
            OsStr::new("--path-format=absolute"),
            OsStr::new("--git-common-dir"),
        ],
    )?;
    Ok(PathBuf::from(trim_line(&output.stdout)))
}

pub fn index_features(cwd: &Path) -> Result<(bool, bool), GitFailure> {
    let sparse = optional_text(
        cwd,
        "git sparse checkout lookup",
        &[
            OsStr::new("config"),
            OsStr::new("--bool"),
            OsStr::new("core.sparseCheckout"),
        ],
    )?
    .is_some_and(|value| value == "true");
    let split = optional_text(
        cwd,
        "git split index lookup",
        &[OsStr::new("rev-parse"), OsStr::new("--shared-index-path")],
    )?
    .is_some_and(|value| !value.is_empty());
    Ok((sparse, split))
}

fn optional_text(
    cwd: &Path,
    operation: &'static str,
    args: &[&OsStr],
) -> Result<Option<String>, GitFailure> {
    let output = command(cwd, args)
        .output()
        .map_err(|error| synthetic_failure(operation, error))?;
    if output.status.success() {
        Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        ))
    } else if output.status.code() == Some(1) {
        Ok(None)
    } else {
        Err(GitFailure { operation, output })
    }
}

fn is_bare(cwd: &Path) -> Result<bool, GitFailure> {
    let output = checked(
        cwd,
        "git bare repository discovery",
        &[OsStr::new("rev-parse"), OsStr::new("--is-bare-repository")],
    )?;
    Ok(trim_line(&output.stdout) == OsStr::new("true"))
}

fn trim_line(bytes: &[u8]) -> OsString {
    let end = bytes
        .iter()
        .rposition(|byte| !matches!(byte, b'\r' | b'\n'))
        .map_or(0, |index| index + 1);
    #[cfg(unix)]
    {
        OsString::from_vec(bytes[..end].to_vec())
    }
    #[cfg(not(unix))]
    {
        OsString::from(String::from_utf8_lossy(&bytes[..end]).into_owned())
    }
}

pub fn ref_oid(cwd: &Path, reference: &str) -> Result<Option<String>, GitFailure> {
    let expression = format!("{reference}^{{commit}}");
    let output = command(
        cwd,
        &[
            OsStr::new("rev-parse"),
            OsStr::new("--verify"),
            OsStr::new("--quiet"),
            OsStr::new(&expression),
        ],
    )
    .output()
    .map_err(|error| synthetic_failure("git ref lookup", error))?;
    if output.status.success() {
        Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        ))
    } else if output.status.code() == Some(1) {
        Ok(None)
    } else {
        Err(GitFailure {
            operation: "git ref lookup",
            output,
        })
    }
}

pub fn resolve_commit(cwd: &Path, expression: &str) -> Result<String, GitFailure> {
    let expression = format!("{expression}^{{commit}}");
    let output = checked(
        cwd,
        "git commit resolution",
        &[
            OsStr::new("rev-parse"),
            OsStr::new("--verify"),
            OsStr::new(&expression),
        ],
    )?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

pub fn create_ref(cwd: &Path, reference: &str, target: &str) -> Result<(), GitFailure> {
    let zero = "0".repeat(target.len());
    checked(
        cwd,
        "git branch creation",
        &[
            OsStr::new("update-ref"),
            OsStr::new("-m"),
            OsStr::new("branch: Created from ramiz add"),
            OsStr::new(reference),
            OsStr::new(target),
            OsStr::new(&zero),
        ],
    )?;
    Ok(())
}

pub fn head_oid(cwd: &Path) -> Result<String, GitFailure> {
    let output = checked(
        cwd,
        "git target resolution",
        &[OsStr::new("rev-parse"), OsStr::new("HEAD^{commit}")],
    )?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

pub fn has_any_commit(cwd: &Path) -> Result<bool, GitFailure> {
    let output = checked(
        cwd,
        "git commit inventory",
        &[
            OsStr::new("rev-list"),
            OsStr::new("--all"),
            OsStr::new("--max-count=1"),
        ],
    )?;
    Ok(!output.stdout.is_empty())
}

pub fn symbolic_head(cwd: &Path) -> Result<Option<String>, GitFailure> {
    optional_text(
        cwd,
        "git symbolic HEAD lookup",
        &[
            OsStr::new("symbolic-ref"),
            OsStr::new("--quiet"),
            OsStr::new("HEAD"),
        ],
    )
}

pub fn git_path(cwd: &Path, name: &str) -> Result<PathBuf, GitFailure> {
    let output = checked(
        cwd,
        "git path lookup",
        &[
            OsStr::new("rev-parse"),
            OsStr::new("--path-format=absolute"),
            OsStr::new("--git-path"),
            OsStr::new(name),
        ],
    )?;
    Ok(PathBuf::from(trim_line(&output.stdout)))
}

pub fn readonly_status(cwd: &Path) -> Result<Vec<u8>, GitFailure> {
    let mut process = command(
        cwd,
        &[
            OsStr::new("status"),
            OsStr::new("--porcelain=v2"),
            OsStr::new("-z"),
            OsStr::new("--untracked-files=all"),
        ],
    );
    process.env("GIT_OPTIONAL_LOCKS", "0");
    let output = process
        .output()
        .map_err(|error| synthetic_failure("git donor status", error))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(GitFailure {
            operation: "git donor status",
            output,
        })
    }
}

pub fn logical_index(cwd: &Path) -> Result<Vec<u8>, GitFailure> {
    let stages = checked(
        cwd,
        "git staged index inspection",
        &[
            OsStr::new("ls-files"),
            OsStr::new("--stage"),
            OsStr::new("-z"),
        ],
    )?
    .stdout;
    let flags = checked(
        cwd,
        "git index flag inspection",
        &[OsStr::new("ls-files"), OsStr::new("-v"), OsStr::new("-z")],
    )?
    .stdout;
    let mut result = stages;
    result.push(0xff);
    result.extend(flags);
    Ok(result)
}

pub fn disable_index_caches(cwd: &Path) -> Result<(), GitFailure> {
    checked(
        cwd,
        "git inherited index normalization",
        &[
            OsStr::new("update-index"),
            OsStr::new("--no-fsmonitor"),
            OsStr::new("--no-untracked-cache"),
        ],
    )?;
    Ok(())
}

pub fn install_index(bytes: &[u8], destination: &Path) -> Result<(), std::io::Error> {
    let file_name = destination
        .file_name()
        .unwrap_or_else(|| OsStr::new("index"));
    let temporary = destination.with_file_name(format!(
        ".{}.ramiz-{}",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, destination)?;
        if let Some(parent) = destination.parent() {
            File::open(parent)?.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

pub fn blob(cwd: &Path, oid: &str) -> Result<Vec<u8>, GitFailure> {
    let output = checked(
        cwd,
        "git blob read",
        &[OsStr::new("cat-file"), OsStr::new("blob"), OsStr::new(oid)],
    )?;
    Ok(output.stdout)
}

#[derive(Debug)]
pub struct IndexEntry {
    pub mode: u32,
    pub oid: String,
    pub stage: u8,
    pub path: OsString,
}

pub fn read_target_index(cwd: &Path, target: &str) -> Result<Vec<IndexEntry>, GitFailure> {
    checked(
        cwd,
        "git read-tree",
        &[
            OsStr::new("read-tree"),
            OsStr::new("--reset"),
            OsStr::new(target),
        ],
    )?;
    let output = checked(
        cwd,
        "git index listing",
        &[
            OsStr::new("ls-files"),
            OsStr::new("--stage"),
            OsStr::new("-z"),
        ],
    )?;
    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .map(parse_index_entry)
        .collect()
}

fn parse_index_entry(record: &[u8]) -> Result<IndexEntry, GitFailure> {
    let tab = record
        .iter()
        .position(|byte| *byte == b'\t')
        .ok_or_else(|| parse_failure("missing index path separator"))?;
    let header =
        std::str::from_utf8(&record[..tab]).map_err(|_| parse_failure("non-UTF-8 index header"))?;
    let mut fields = header.split(' ');
    let mode = u32::from_str_radix(fields.next().unwrap_or_default(), 8)
        .map_err(|_| parse_failure("invalid index mode"))?;
    let oid = fields.next().unwrap_or_default().to_owned();
    let stage = fields
        .next()
        .unwrap_or_default()
        .parse::<u8>()
        .map_err(|_| parse_failure("invalid index stage"))?;
    #[cfg(unix)]
    let path = OsString::from_vec(record[tab + 1..].to_vec());
    #[cfg(not(unix))]
    let path = OsString::from(String::from_utf8_lossy(&record[tab + 1..]).into_owned());
    Ok(IndexEntry {
        mode,
        oid,
        stage,
        path,
    })
}

fn parse_failure(message: &str) -> GitFailure {
    GitFailure {
        operation: "git index parse",
        output: Output {
            status: failure_status(),
            stdout: Vec::new(),
            stderr: message.as_bytes().to_vec(),
        },
    }
}

pub fn path_has_transform(cwd: &Path, path: &OsStr) -> Result<bool, GitFailure> {
    let mut input = os_bytes(path).to_vec();
    input.push(0);
    let output = checked_with_input(
        cwd,
        "git attribute lookup",
        &[
            OsStr::new("check-attr"),
            OsStr::new("--cached"),
            OsStr::new("-z"),
            OsStr::new("-a"),
            OsStr::new("--stdin"),
        ],
        &input,
    )?;
    let mut fields: Vec<&[u8]> = output.stdout.split(|byte| *byte == 0).collect();
    if fields.last().is_some_and(|field| field.is_empty()) {
        fields.pop();
    }
    if fields.len() % 3 != 0
        || fields
            .chunks_exact(3)
            .any(|triple| triple[0] != os_bytes(path))
    {
        return Err(parse_failure("malformed git check-attr -z response"));
    }
    let transformed = fields.chunks_exact(3).any(|triple| {
        matches!(
            triple[1],
            b"filter" | b"text" | b"eol" | b"working-tree-encoding" | b"ident"
        ) && !matches!(triple[2], b"unspecified" | b"unset")
    });
    if transformed {
        return Ok(true);
    }
    for key in ["core.autocrlf", "core.eol"] {
        let output = command(
            cwd,
            &[OsStr::new("config"), OsStr::new("--get"), OsStr::new(key)],
        )
        .output()
        .map_err(|error| synthetic_failure("git config lookup", error))?;
        if output.status.success() {
            let value = String::from_utf8_lossy(&output.stdout);
            if !matches!(value.trim(), "" | "false" | "input" | "native") {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub fn hash_file(cwd: &Path, path: &Path) -> Result<String, GitFailure> {
    let mut child = command(
        cwd,
        &[
            OsStr::new("hash-object"),
            OsStr::new("--no-filters"),
            OsStr::new("--stdin"),
        ],
    )
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .map_err(|error| synthetic_failure("git hash-object", error))?;
    let mut file = File::open(path).map_err(|error| synthetic_failure("git hash-object", error))?;
    std::io::copy(&mut file, child.stdin.as_mut().expect("piped stdin"))
        .map_err(|error| synthetic_failure("git hash-object", error))?;
    drop(child.stdin.take());
    let output = child
        .wait_with_output()
        .map_err(|error| synthetic_failure("git hash-object", error))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(GitFailure {
            operation: "git hash-object",
            output,
        })
    }
}

pub fn checkout_paths(cwd: &Path, paths: &[OsString]) -> Result<(), GitFailure> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut input = Vec::new();
    for path in paths {
        input.extend_from_slice(os_bytes(path));
        input.push(0);
    }
    checked_with_input(
        cwd,
        "git checkout-index",
        &[
            OsStr::new("checkout-index"),
            OsStr::new("-u"),
            OsStr::new("-z"),
            OsStr::new("--stdin"),
        ],
        &input,
    )?;
    Ok(())
}

pub fn refresh_and_verify_clean(cwd: &Path) -> Result<(), GitFailure> {
    let output = checked(
        cwd,
        "git worktree verification",
        &[
            OsStr::new("status"),
            OsStr::new("--porcelain=v1"),
            OsStr::new("-z"),
            OsStr::new("--untracked-files=all"),
        ],
    )?;
    if output.stdout.is_empty() {
        Ok(())
    } else {
        let status = String::from_utf8_lossy(&output.stdout).replace('\0', "\\0");
        Err(GitFailure {
            operation: "git worktree verification",
            output: Output {
                status: failure_status(),
                stdout: output.stdout,
                stderr: format!("materialized worktree does not match the target index: {status}")
                    .into_bytes(),
            },
        })
    }
}

pub fn run_post_checkout(cwd: &Path, target: &str) -> Result<Output, GitFailure> {
    let null_ref = "0".repeat(target.len());
    checked(
        cwd,
        "git post-checkout hook",
        &[
            OsStr::new("hook"),
            OsStr::new("run"),
            OsStr::new("--ignore-missing"),
            OsStr::new("post-checkout"),
            OsStr::new("--"),
            OsStr::new(&null_ref),
            OsStr::new(target),
            OsStr::new("1"),
        ],
    )
}

pub fn worktree_registered(cwd: &Path, destination: &Path) -> Result<bool, GitFailure> {
    let output = checked(
        cwd,
        "git worktree ownership check",
        &[
            OsStr::new("worktree"),
            OsStr::new("list"),
            OsStr::new("--porcelain"),
            OsStr::new("-z"),
        ],
    )?;
    for field in output.stdout.split(|byte| *byte == 0) {
        let Some(path) = field.strip_prefix(b"worktree ") else {
            continue;
        };
        #[cfg(unix)]
        let candidate = PathBuf::from(OsString::from_vec(path.to_vec()));
        #[cfg(not(unix))]
        let candidate = PathBuf::from(String::from_utf8_lossy(path).into_owned());
        if canonical_or_self(&candidate) == canonical_or_self(destination) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn branch_checked_out(cwd: &Path, reference: &str) -> Result<bool, GitFailure> {
    let output = checked(
        cwd,
        "git branch ownership check",
        &[
            OsStr::new("worktree"),
            OsStr::new("list"),
            OsStr::new("--porcelain"),
            OsStr::new("-z"),
        ],
    )?;
    let expected = format!("branch {reference}");
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .any(|field| field == expected.as_bytes()))
}

fn canonical_or_self(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub fn delete_ref_if(cwd: &Path, reference: &str, expected_oid: &str) -> Result<(), GitFailure> {
    checked(
        cwd,
        "git branch rollback",
        &[
            OsStr::new("update-ref"),
            OsStr::new("-d"),
            OsStr::new(reference),
            OsStr::new(expected_oid),
        ],
    )?;
    Ok(())
}

#[cfg(unix)]
fn os_bytes(value: &OsStr) -> &[u8] {
    value.as_bytes()
}

#[cfg(not(unix))]
fn os_bytes(value: &OsStr) -> &[u8] {
    value.to_str().unwrap_or_default().as_bytes()
}
