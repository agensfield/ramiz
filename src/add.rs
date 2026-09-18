use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{CString, OsStr, OsString},
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
};

use serde::Serialize;

use crate::{cancellation, cli::AddArgs, error::RamizError, fsclone, git, inherit, output};

#[derive(Serialize)]
struct AddData {
    path: output::EncodedPath,
    head: Option<String>,
    backend: &'static str,
    cow_files: usize,
    git_files: usize,
    copied_files: usize,
    hardlinks: usize,
    skipped_special: Vec<output::EncodedPath>,
    inherited_symlinks: Vec<output::EncodedPath>,
    donor_links: Vec<output::EncodedPath>,
    hook_output: HookData,
}

#[derive(Default, Serialize)]
struct HookData {
    encoding: &'static str,
    stdout: String,
    stderr: String,
}

struct CleanCandidate {
    path: OsString,
    oid: String,
    source: PathBuf,
    target: PathBuf,
    executable: bool,
}

enum CloneOutcome {
    Retained(usize),
    Checkout(usize),
    Fatal(usize, String),
}

pub fn run(args: AddArgs) -> Result<(), RamizError> {
    if args.inherit {
        return run_inherit(args);
    }
    run_clean(args)
}

fn run_clean(args: AddArgs) -> Result<(), RamizError> {
    let cwd = std::env::current_dir()
        .map_err(|error| RamizError::new("current_directory", error.to_string(), args.json))?;
    git::require_version(&cwd).map_err(|error| git_error("git_version", error, args.json))?;
    let repo_root =
        git::repo_root(&cwd).map_err(|error| git_error("not_a_repository", error, args.json))?;
    if git::ref_oid(&cwd, "HEAD")
        .map_err(|error| git_error("head_preflight", error, args.json))?
        .is_none()
    {
        if git::has_any_commit(&cwd)
            .map_err(|error| git_error("head_preflight", error, args.json))?
        {
            return Err(RamizError::new(
                "invalid_head",
                "HEAD does not resolve although the repository contains commits",
                args.json,
            ));
        }
        return run_unborn(args, cwd, repo_root);
    }
    let donor = match &args.from {
        Some(path) => Some(
            git::repo_root(path)
                .map_err(|error| git_error("invalid_donor", error, args.json))?
                .ok_or_else(|| {
                    RamizError::new(
                        "invalid_donor",
                        "a bare repository cannot be a donor",
                        args.json,
                    )
                })?,
        ),
        None => repo_root.clone(),
    };
    let source_common = git::common_dir(&cwd)
        .map_err(|error| git_error("repository_discovery", error, args.json))?;
    if let Some(donor) = &donor {
        let donor_common =
            git::common_dir(donor).map_err(|error| git_error("invalid_donor", error, args.json))?;
        if canonical_or_self(&source_common) != canonical_or_self(&donor_common) {
            return Err(RamizError::new(
                "different_repository",
                "donor belongs to a different Git common repository",
                args.json,
            ));
        }
    }

    let destination = resolve_destination(&cwd, &args.path)
        .map_err(|message| RamizError::new("invalid_destination", message, args.json))?;
    validate_destination(
        &destination,
        repo_root.as_deref(),
        donor.as_deref(),
        Some(&source_common),
        args.json,
    )?;
    let existed_empty = destination.is_dir();
    let probe_directory = fsclone::existing_directory(&destination)
        .map_err(|message| RamizError::new("invalid_destination", message, args.json))?;
    let (sparse, split) = match &donor {
        Some(donor) => git::index_features(donor)
            .map_err(|error| git_error("index_feature_detection", error, args.json))?,
        None => (false, false),
    };
    let mut warnings = Vec::new();
    let backend = if sparse || split {
        let reason = match (sparse, split) {
            (true, true) => "sparse checkout and split index are enabled",
            (true, false) => "sparse checkout is enabled",
            (false, true) => "split index is enabled",
            (false, false) => unreachable!(),
        };
        if args.require_cow {
            return Err(RamizError::new("cow_unsupported_index", reason, args.json));
        }
        warnings.push(format!("copy-on-write skipped: {reason}"));
        None
    } else if donor.is_none() {
        if args.require_cow {
            return Err(RamizError::new(
                "missing_donor",
                "a bare repository requires --from for copy-on-write creation",
                args.json,
            ));
        }
        warnings.push("copy-on-write skipped: bare repository has no donor".into());
        None
    } else {
        match fsclone::probe(&probe_directory) {
            Ok(backend) => Some(backend),
            Err(reason) if args.require_cow => {
                return Err(RamizError::new("cow_unavailable", reason, args.json));
            }
            Err(reason) => {
                warnings.push(format!("copy-on-write unavailable: {reason}"));
                None
            }
        }
    };

    if git::worktree_registered(&cwd, &destination)
        .map_err(|error| git_error("destination_preflight", error, args.json))?
    {
        return Err(RamizError::new(
            "registered_destination_missing",
            format!(
                "{} is already registered as a Git worktree; use `git worktree repair` or `git worktree prune`",
                destination.display()
            ),
            args.json,
        ));
    }

    let prepared = prepare_branch(&cwd, &destination, &args)?;

    let mut transaction = Transaction {
        repository: cwd.clone(),
        destination: destination.clone(),
        existed_empty,
        branch_ref: prepared.owned_ref.clone(),
        created_oid: prepared.owned_oid.clone(),
        registered: false,
        rollback_enabled: true,
    };

    if let Err(error) = native_add(&cwd, &destination, &args, prepared.checkout_ref.as_deref()) {
        let cleanup = transaction.rollback();
        let mut failure = git_error("worktree_add_failed", error, args.json);
        if let Some(cleanup) = cleanup {
            failure = failure.with_cleanup(cleanup);
        }
        return Err(failure);
    }
    transaction.registered = true;
    check_cancelled(args.json, &mut transaction)?;
    let target = git::head_oid(&destination).map_err(|error| {
        fail_with_rollback("target_resolution", error, args.json, &mut transaction)
    })?;
    if let Some(expected) = &prepared.expected_target {
        if expected != &target {
            let error = git::GitFailure {
                operation: "git target stability check",
                output: std::process::Output {
                    status: failure_status(),
                    stdout: Vec::new(),
                    stderr: format!("target moved from {expected} to {target} during registration")
                        .into_bytes(),
                },
            };
            return Err(fail_with_rollback(
                "target_changed",
                error,
                args.json,
                &mut transaction,
            ));
        }
    }

    let entries = git::read_target_index(&destination, &target).map_err(|error| {
        fail_with_rollback("index_materialization", error, args.json, &mut transaction)
    })?;
    let (cow_files, git_paths) = materialize_clean_files(
        entries,
        donor.as_deref(),
        &destination,
        backend,
        args.json,
        &mut transaction,
    )?;

    git::checkout_paths(&destination, &git_paths).map_err(|error| {
        fail_with_rollback("checkout_failed", error, args.json, &mut transaction)
    })?;
    check_cancelled(args.json, &mut transaction)?;
    git::refresh_and_verify_clean(&destination).map_err(|error| {
        fail_with_rollback("verification_failed", error, args.json, &mut transaction)
    })?;

    transaction.rollback_enabled = false;
    let hook = match git::run_post_checkout(&destination, &target) {
        Ok(output) => output,
        Err(error) => {
            if !args.json {
                forward_hook_output(&error.output);
            }
            let hook_stdout = output::base64(&error.output.stdout);
            let hook_stderr = output::base64(&error.output.stderr);
            return Err(RamizError::new(
                "post_checkout_failed",
                format!(
                    "post-checkout hook failed with exit status {}",
                    error
                        .output
                        .status
                        .code()
                        .map_or_else(|| "signal".to_string(), |code| code.to_string())
                ),
                args.json,
            )
            .worktree_created()
            .with_hook_output(hook_stdout, hook_stderr));
        }
    };
    if !args.json {
        forward_hook_output(&hook);
    }

    emit_success(
        &args,
        AddData {
            path: output::EncodedPath::new(&destination),
            head: Some(target),
            backend: backend.map_or("checkout", |backend| backend.as_str()),
            cow_files,
            git_files: git_paths.len(),
            copied_files: 0,
            hardlinks: 0,
            skipped_special: Vec::new(),
            inherited_symlinks: Vec::new(),
            donor_links: Vec::new(),
            hook_output: HookData {
                encoding: "base64",
                stdout: output::base64(&hook.stdout),
                stderr: output::base64(&hook.stderr),
            },
        },
        &warnings,
    );
    Ok(())
}

fn materialize_clean_files(
    entries: Vec<git::IndexEntry>,
    donor: Option<&Path>,
    destination: &Path,
    backend: Option<fsclone::Backend>,
    json: bool,
    transaction: &mut Transaction,
) -> Result<(usize, Vec<OsString>), RamizError> {
    let (Some(donor), Some(_backend)) = (donor, backend) else {
        return Ok((0, entries.into_iter().map(|entry| entry.path).collect()));
    };

    let mut git_paths = Vec::new();
    let mut candidates = Vec::new();
    for entry in entries {
        check_cancelled(json, transaction)?;
        if entry.stage != 0 || !matches!(entry.mode, 0o100644 | 0o100755) {
            git_paths.push(entry.path);
            continue;
        }
        let source = donor.join(&entry.path);
        let source_meta = match fs::symlink_metadata(&source) {
            Ok(meta) if meta.is_file() => meta,
            _ => {
                git_paths.push(entry.path);
                continue;
            }
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let donor_executable = source_meta.permissions().mode() & 0o111 != 0;
            if (entry.mode == 0o100755) != donor_executable {
                git_paths.push(entry.path);
                continue;
            }
        }
        candidates.push(CleanCandidate {
            target: destination.join(&entry.path),
            source,
            executable: entry.mode == 0o100755,
            path: entry.path,
            oid: entry.oid,
        });
    }

    check_cancelled(json, transaction)?;
    let candidate_paths: Vec<OsString> = candidates
        .iter()
        .map(|candidate| candidate.path.clone())
        .collect();
    let transformed = git::transformed_paths(destination, &candidate_paths)
        .map_err(|error| fail_with_rollback("attribute_lookup", error, json, transaction))?;
    let mut hash_candidates = Vec::new();
    for candidate in candidates {
        if transformed.contains(&candidate.path) {
            git_paths.push(candidate.path);
        } else {
            hash_candidates.push(candidate);
        }
    }

    check_cancelled(json, transaction)?;
    let source_paths: Vec<PathBuf> = hash_candidates
        .iter()
        .map(|candidate| candidate.source.clone())
        .collect();
    let source_hashes = hash_clean_files(
        destination,
        &source_paths,
        "donor_verification",
        json,
        transaction,
    )?;
    let mut clone_candidates = Vec::new();
    for (candidate, source_oid) in hash_candidates.into_iter().zip(source_hashes) {
        if source_oid == candidate.oid {
            clone_candidates.push(candidate);
        } else {
            git_paths.push(candidate.path);
        }
    }

    let umask = fsclone::capture_umask();
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let (sender, receiver) = mpsc::channel();
    let worker_count = clone_candidates.len().min(4);
    let panicked = thread::scope(|scope| {
        let mut workers = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            let sender = sender.clone();
            let next = &next;
            let stop = &stop;
            let candidates = &clone_candidates;
            workers.push(scope.spawn(move || {
                loop {
                    if stop.load(Ordering::Relaxed) || cancellation::requested() {
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(candidate) = candidates.get(index) else {
                        break;
                    };
                    #[cfg(debug_assertions)]
                    clean_worker_test_gate();
                    if cancellation::requested() {
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                    if let Some(parent) = candidate.target.parent() {
                        if let Err(error) = fs::create_dir_all(parent) {
                            stop.store(true, Ordering::Relaxed);
                            let _ = sender.send(CloneOutcome::Fatal(
                                index,
                                format!("create {}: {error}", parent.display()),
                            ));
                            break;
                        }
                    }
                    if fsclone::clone_file(&candidate.source, &candidate.target).is_err()
                        || fsclone::normalize_clean_file(
                            &candidate.target,
                            candidate.executable,
                            umask,
                        )
                        .is_err()
                    {
                        let _ = fs::remove_file(&candidate.target);
                        let _ = sender.send(CloneOutcome::Checkout(index));
                    } else {
                        let _ = sender.send(CloneOutcome::Retained(index));
                    }
                }
            }));
        }
        workers
            .into_iter()
            .fold(false, |panicked, worker| worker.join().is_err() || panicked)
    });
    drop(sender);
    let mut outcomes: Vec<CloneOutcome> = receiver.into_iter().collect();
    outcomes.sort_by_key(|outcome| match outcome {
        CloneOutcome::Retained(index)
        | CloneOutcome::Checkout(index)
        | CloneOutcome::Fatal(index, _) => *index,
    });
    if panicked {
        return Err(fail_message(
            "materialization_failed",
            "clean materialization worker panicked",
            json,
            transaction,
        ));
    }
    check_cancelled(json, transaction)?;
    if let Some(message) = outcomes.iter().find_map(|outcome| match outcome {
        CloneOutcome::Fatal(_, message) => Some(message.clone()),
        _ => None,
    }) {
        return Err(fail_message(
            "materialization_failed",
            message,
            json,
            transaction,
        ));
    }
    if outcomes.len() != clone_candidates.len() {
        return Err(fail_message(
            "materialization_failed",
            "clean materialization workers returned incomplete results",
            json,
            transaction,
        ));
    }

    let mut retained = Vec::new();
    for outcome in outcomes {
        match outcome {
            CloneOutcome::Retained(index) => retained.push(index),
            CloneOutcome::Checkout(index) => {
                git_paths.push(clone_candidates[index].path.clone());
            }
            CloneOutcome::Fatal(_, _) => unreachable!(),
        }
    }
    let target_paths: Vec<PathBuf> = retained
        .iter()
        .map(|index| clone_candidates[*index].target.clone())
        .collect();
    let target_hashes = hash_clean_files(
        destination,
        &target_paths,
        "clone_verification",
        json,
        transaction,
    )?;
    let mut cow_files = 0;
    for (index, cloned_oid) in retained.into_iter().zip(target_hashes) {
        let candidate = &clone_candidates[index];
        if cloned_oid == candidate.oid {
            cow_files += 1;
        } else {
            let _ = fs::remove_file(&candidate.target);
            git_paths.push(candidate.path.clone());
        }
    }
    Ok((cow_files, git_paths))
}

fn hash_clean_files(
    destination: &Path,
    paths: &[PathBuf],
    failure_code: &'static str,
    json: bool,
    transaction: &mut Transaction,
) -> Result<Vec<String>, RamizError> {
    match git::hash_files(destination, paths) {
        Ok(hashes) => Ok(hashes),
        Err(git::HashFilesError::Git(error)) => {
            Err(fail_with_rollback(failure_code, error, json, transaction))
        }
        Err(git::HashFilesError::Cancelled) => Err(fail_message(
            "interrupted",
            "creation interrupted",
            json,
            transaction,
        )),
    }
}

fn run_unborn(args: AddArgs, cwd: PathBuf, repo_root: Option<PathBuf>) -> Result<(), RamizError> {
    if args.detach || args.start_point.is_some() {
        return Err(RamizError::new(
            "invalid_unborn_target",
            "an unborn repository has no commit for --detach or <start-point>",
            args.json,
        ));
    }
    let common = git::common_dir(&cwd)
        .map_err(|error| git_error("repository_discovery", error, args.json))?;
    let donor = match &args.from {
        Some(path) => {
            let donor = git::repo_root(path)
                .map_err(|error| git_error("invalid_donor", error, args.json))?
                .ok_or_else(|| {
                    RamizError::new(
                        "invalid_donor",
                        "a bare repository cannot be a donor",
                        args.json,
                    )
                })?;
            let donor_common = git::common_dir(&donor)
                .map_err(|error| git_error("invalid_donor", error, args.json))?;
            if canonical_or_self(&common) != canonical_or_self(&donor_common) {
                return Err(RamizError::new(
                    "different_repository",
                    "donor belongs to a different Git common repository",
                    args.json,
                ));
            }
            Some(donor)
        }
        None => repo_root.clone(),
    };
    let destination = resolve_destination(&cwd, &args.path)
        .map_err(|message| RamizError::new("invalid_destination", message, args.json))?;
    validate_destination(
        &destination,
        repo_root.as_deref(),
        donor.as_deref(),
        Some(&common),
        args.json,
    )?;
    if git::worktree_registered(&cwd, &destination)
        .map_err(|error| git_error("destination_preflight", error, args.json))?
    {
        return Err(RamizError::new(
            "registered_destination_missing",
            format!(
                "{} is already registered as a Git worktree",
                destination.display()
            ),
            args.json,
        ));
    }
    let existed_empty = destination.is_dir();
    let mut warnings =
        vec!["unborn HEAD follows native Git behavior and does not run post-checkout".to_string()];
    let backend = if donor.is_some() {
        let probe_directory = fsclone::existing_directory(&destination)
            .map_err(|message| RamizError::new("invalid_destination", message, args.json))?;
        match fsclone::probe(&probe_directory) {
            Ok(backend) => Some(backend),
            Err(reason) if args.require_cow => {
                return Err(RamizError::new("cow_unavailable", reason, args.json));
            }
            Err(reason) => {
                warnings.push(format!("copy-on-write unavailable: {reason}"));
                None
            }
        }
    } else {
        if args.require_cow {
            return Err(RamizError::new(
                "missing_donor",
                "a bare unborn repository requires --from for --require-cow",
                args.json,
            ));
        }
        None
    };

    let mut command = vec![
        OsString::from("worktree"),
        OsString::from("add"),
        OsString::from("--quiet"),
        OsString::from("--no-guess-remote"),
    ];
    if let Some(branch) = &args.branch {
        command.push(OsString::from("-b"));
        command.push(OsString::from(branch));
    }
    command.push(destination.as_os_str().to_owned());
    let borrowed: Vec<&OsStr> = command.iter().map(OsString::as_os_str).collect();
    if let Err(error) = git::checked(&cwd, "git unborn worktree add", &borrowed) {
        let created = git::worktree_registered(&cwd, &destination).unwrap_or(false);
        let failure = git_error("worktree_add_failed", error, args.json);
        return Err(if created {
            failure.worktree_created()
        } else {
            failure
        });
    }
    let mut transaction = Transaction {
        repository: cwd.clone(),
        destination: destination.clone(),
        existed_empty,
        branch_ref: None,
        created_oid: None,
        registered: true,
        rollback_enabled: true,
    };
    check_cancelled(args.json, &mut transaction)?;
    let registered = git::worktree_registered(&cwd, &destination).map_err(|error| {
        fail_with_rollback("unborn_verification", error, args.json, &mut transaction)
    })?;
    let symbolic = git::symbolic_head(&destination).map_err(|error| {
        fail_with_rollback("unborn_verification", error, args.json, &mut transaction)
    })?;
    let index = git::git_path(&destination, "index").map_err(|error| {
        fail_with_rollback("unborn_verification", error, args.json, &mut transaction)
    })?;
    let status = git::readonly_status(&destination).map_err(|error| {
        fail_with_rollback("unborn_verification", error, args.json, &mut transaction)
    })?;
    if !registered || symbolic.is_none() || !index.is_file() || !status.is_empty() {
        return Err(fail_message(
            "unborn_verification_failed",
            "native Git did not leave a valid empty unborn worktree",
            args.json,
            &mut transaction,
        ));
    }
    transaction.rollback_enabled = false;
    emit_success(
        &args,
        AddData {
            path: output::EncodedPath::new(&destination),
            head: None,
            backend: backend.map_or("checkout", |backend| backend.as_str()),
            cow_files: 0,
            git_files: 0,
            copied_files: 0,
            hardlinks: 0,
            skipped_special: Vec::new(),
            inherited_symlinks: Vec::new(),
            donor_links: Vec::new(),
            hook_output: HookData {
                encoding: "base64",
                stdout: String::new(),
                stderr: String::new(),
            },
        },
        &warnings,
    );
    Ok(())
}

fn run_inherit(args: AddArgs) -> Result<(), RamizError> {
    let cwd = std::env::current_dir()
        .map_err(|error| RamizError::new("current_directory", error.to_string(), args.json))?;
    git::require_version(&cwd).map_err(|error| git_error("git_version", error, args.json))?;
    let repo_root = git::repo_root(&cwd)
        .map_err(|error| git_error("not_a_repository", error, args.json))?
        .ok_or_else(|| {
            RamizError::new(
                "inheritance_requires_worktree",
                "complete inheritance requires a non-bare invoking worktree",
                args.json,
            )
        })?;
    let donor = match &args.from {
        Some(path) => git::repo_root(path)
            .map_err(|error| git_error("invalid_donor", error, args.json))?
            .ok_or_else(|| {
                RamizError::new(
                    "invalid_donor",
                    "a bare repository cannot be an inheritance donor",
                    args.json,
                )
            })?,
        None => repo_root.clone(),
    };
    let source_common = git::common_dir(&cwd)
        .map_err(|error| git_error("repository_discovery", error, args.json))?;
    let donor_common =
        git::common_dir(&donor).map_err(|error| git_error("invalid_donor", error, args.json))?;
    if canonical_or_self(&source_common) != canonical_or_self(&donor_common) {
        return Err(RamizError::new(
            "different_repository",
            "donor belongs to a different Git common repository",
            args.json,
        ));
    }

    let destination = resolve_destination(&cwd, &args.path)
        .map_err(|message| RamizError::new("invalid_destination", message, args.json))?;
    validate_destination(
        &destination,
        Some(&repo_root),
        Some(&donor),
        Some(&source_common),
        args.json,
    )?;
    if destination.starts_with(&donor) || donor.starts_with(&destination) {
        return Err(RamizError::new(
            "inheritance_destination_overlap",
            "complete inheritance requires a destination outside the donor tree",
            args.json,
        ));
    }
    if git::worktree_registered(&cwd, &destination)
        .map_err(|error| git_error("destination_preflight", error, args.json))?
    {
        return Err(RamizError::new(
            "registered_destination_missing",
            format!(
                "{} is already registered as a Git worktree; use `git worktree repair` or `git worktree prune`",
                destination.display()
            ),
            args.json,
        ));
    }
    let (sparse, split) = git::index_features(&donor)
        .map_err(|error| git_error("index_feature_detection", error, args.json))?;
    if sparse || split {
        return Err(RamizError::new(
            "inheritance_unsupported_index",
            "complete inheritance does not support sparse checkout or split index",
            args.json,
        ));
    }

    let donor_head = git::head_oid(&donor)
        .map_err(|error| git_error("inheritance_unborn_donor", error, args.json))?;
    let donor_symbolic_head =
        git::symbolic_head(&donor).map_err(|error| git_error("donor_head", error, args.json))?;
    let intended = intended_target(&cwd, &destination, &args)?;
    if intended != donor_head {
        return Err(RamizError::new(
            "inheritance_target_mismatch",
            format!("inheritance target {intended} does not equal donor HEAD {donor_head}"),
            args.json,
        ));
    }

    let donor_index_path = git::git_path(&donor, "index")
        .map_err(|error| git_error("donor_index", error, args.json))?;
    let donor_index = fs::read(&donor_index_path)
        .map_err(|error| RamizError::new("donor_index", error.to_string(), args.json))?;
    let donor_status = git::readonly_status(&donor)
        .map_err(|error| git_error("donor_status", error, args.json))?;
    let donor_logical_index =
        git::logical_index(&donor).map_err(|error| git_error("donor_index", error, args.json))?;
    let donor_filesystem = inherit::snapshot(&donor)
        .map_err(|error| RamizError::new("inheritance_preflight", error, args.json))?;
    let probe_directory = fsclone::existing_directory(&destination)
        .map_err(|message| RamizError::new("invalid_destination", message, args.json))?;
    let mut warnings = Vec::new();
    let probed_backend = match fsclone::probe(&probe_directory) {
        Ok(backend) => Some(backend),
        Err(reason) if args.allow_copy => {
            warnings.push(format!(
                "copy-on-write unavailable; physical copy authorized: {reason}"
            ));
            None
        }
        Err(reason) => return Err(RamizError::new("cow_unavailable", reason, args.json)),
    };

    let prepared = prepare_branch(&cwd, &destination, &args)?;
    let mut transaction = Transaction {
        repository: cwd.clone(),
        destination: destination.clone(),
        existed_empty: destination.is_dir(),
        branch_ref: prepared.owned_ref.clone(),
        created_oid: prepared.owned_oid.clone(),
        registered: false,
        rollback_enabled: true,
    };
    if let Err(error) = native_add(&cwd, &destination, &args, prepared.checkout_ref.as_deref()) {
        let cleanup = transaction.rollback();
        let mut failure = git_error("worktree_add_failed", error, args.json);
        if let Some(cleanup) = cleanup {
            failure = failure.with_cleanup(cleanup);
        }
        return Err(failure);
    }
    transaction.registered = true;
    check_cancelled(args.json, &mut transaction)?;
    let target = git::head_oid(&destination).map_err(|error| {
        fail_with_rollback("target_resolution", error, args.json, &mut transaction)
    })?;
    if target != donor_head {
        return Err(fail_message(
            "inheritance_target_changed",
            format!("inheritance target moved from {donor_head} to {target}"),
            args.json,
            &mut transaction,
        ));
    }

    let target_entries = git::read_target_index(&destination, &target).map_err(|error| {
        fail_with_rollback("index_materialization", error, args.json, &mut transaction)
    })?;
    let mut target_symlinks = BTreeSet::new();
    let mut target_link_data = BTreeMap::new();
    for entry in target_entries {
        if entry.stage == 0 && entry.mode == 0o120000 {
            let path = PathBuf::from(&entry.path);
            let data = git::blob(&destination, &entry.oid).map_err(|error| {
                fail_with_rollback("target_symlink_read", error, args.json, &mut transaction)
            })?;
            target_symlinks.insert(path.clone());
            target_link_data.insert(path, data);
        }
    }
    validate_target_symlink_ancestors(&donor, &target_symlinks).map_err(|error| {
        fail_message(
            "target_symlink_ancestor",
            error,
            args.json,
            &mut transaction,
        )
    })?;

    let report = inherit::materialize(
        &donor_filesystem,
        &destination,
        &target_symlinks,
        args.allow_copy,
        args.allow_donor_links,
    )
    .map_err(|error| {
        fail_message(
            "inheritance_materialization_failed",
            error,
            args.json,
            &mut transaction,
        )
    })?;
    check_cancelled(args.json, &mut transaction)?;
    inheritance_test_gate();
    warnings.extend(report.warnings.iter().cloned());

    let destination_index = git::git_path(&destination, "index").map_err(|error| {
        fail_with_rollback("destination_index", error, args.json, &mut transaction)
    })?;
    git::install_index(&donor_index, &destination_index).map_err(|error| {
        io_failure(
            "inheritance_index_install",
            error,
            args.json,
            &mut transaction,
        )
    })?;
    git::disable_index_caches(&destination).map_err(|error| {
        fail_with_rollback(
            "inheritance_index_normalization",
            error,
            args.json,
            &mut transaction,
        )
    })?;
    materialize_target_symlinks(&destination, &target_link_data).map_err(|error| {
        io_failure(
            "target_symlink_materialization",
            error,
            args.json,
            &mut transaction,
        )
    })?;
    inherit::finalize_directories(&donor_filesystem, &destination, &target_symlinks).map_err(
        |error| {
            fail_message(
                "inheritance_metadata_failed",
                error,
                args.json,
                &mut transaction,
            )
        },
    )?;

    inherit::verify_unchanged(&donor_filesystem)
        .map_err(|error| fail_message("donor_changed", error, args.json, &mut transaction))?;
    let donor_index_after = fs::read(&donor_index_path)
        .map_err(|error| io_failure("donor_verification", error, args.json, &mut transaction))?;
    let donor_status_after = git::readonly_status(&donor).map_err(|error| {
        fail_with_rollback("donor_verification", error, args.json, &mut transaction)
    })?;
    let donor_logical_after = git::logical_index(&donor).map_err(|error| {
        fail_with_rollback("donor_verification", error, args.json, &mut transaction)
    })?;
    let donor_head_after = git::head_oid(&donor).map_err(|error| {
        fail_with_rollback("donor_verification", error, args.json, &mut transaction)
    })?;
    let donor_symbolic_after = git::symbolic_head(&donor).map_err(|error| {
        fail_with_rollback("donor_verification", error, args.json, &mut transaction)
    })?;
    if donor_index_after != donor_index
        || donor_status_after != donor_status
        || donor_logical_after != donor_logical_index
        || donor_head_after != donor_head
        || donor_symbolic_after != donor_symbolic_head
    {
        return Err(fail_message(
            "donor_changed",
            "donor Git/index state changed during inheritance",
            args.json,
            &mut transaction,
        ));
    }
    let destination_logical = git::logical_index(&destination).map_err(|error| {
        fail_with_rollback(
            "inheritance_verification",
            error,
            args.json,
            &mut transaction,
        )
    })?;
    let destination_status = git::readonly_status(&destination).map_err(|error| {
        fail_with_rollback(
            "inheritance_verification",
            error,
            args.json,
            &mut transaction,
        )
    })?;
    if destination_logical != donor_logical_index || destination_status != donor_status {
        return Err(fail_message(
            "inheritance_verification_failed",
            "inherited index or working state differs from the donor",
            args.json,
            &mut transaction,
        ));
    }
    check_cancelled(args.json, &mut transaction)?;

    transaction.rollback_enabled = false;
    let hook = match git::run_post_checkout(&destination, &target) {
        Ok(output) => output,
        Err(error) => {
            if !args.json {
                forward_hook_output(&error.output);
            }
            let hook_stdout = output::base64(&error.output.stdout);
            let hook_stderr = output::base64(&error.output.stderr);
            return Err(RamizError::new(
                "post_checkout_failed",
                format!(
                    "post-checkout hook failed with exit status {}",
                    error
                        .output
                        .status
                        .code()
                        .map_or_else(|| "signal".to_string(), |code| code.to_string())
                ),
                args.json,
            )
            .worktree_created()
            .with_hook_output(hook_stdout, hook_stderr));
        }
    };
    if !args.json {
        forward_hook_output(&hook);
    }
    let backend = if report.copied_files > 0 && report.cow_files == 0 {
        "copy"
    } else {
        probed_backend.map_or("copy", |backend| backend.as_str())
    };
    emit_success(
        &args,
        AddData {
            path: output::EncodedPath::new(&destination),
            head: Some(target),
            backend,
            cow_files: report.cow_files,
            git_files: target_link_data.len(),
            copied_files: report.copied_files,
            hardlinks: report.hardlinks,
            skipped_special: report
                .skipped_special
                .iter()
                .map(|path| output::EncodedPath::new(path))
                .collect(),
            inherited_symlinks: report
                .symlinks
                .iter()
                .map(|path| output::EncodedPath::new(path))
                .collect(),
            donor_links: report
                .donor_links
                .iter()
                .map(|path| output::EncodedPath::new(path))
                .collect(),
            hook_output: HookData {
                encoding: "base64",
                stdout: output::base64(&hook.stdout),
                stderr: output::base64(&hook.stderr),
            },
        },
        &warnings,
    );
    Ok(())
}

fn intended_target(cwd: &Path, destination: &Path, args: &AddArgs) -> Result<String, RamizError> {
    if let Some(branch) = &args.branch {
        let reference = format!("refs/heads/{branch}");
        if git::ref_oid(cwd, &reference)
            .map_err(|error| git_error("branch_preflight", error, args.json))?
            .is_some()
        {
            return Err(RamizError::new(
                "branch_exists",
                format!("branch '{branch}' already exists"),
                args.json,
            ));
        }
        return git::resolve_commit(cwd, args.start_point.as_deref().unwrap_or("HEAD"))
            .map_err(|error| git_error("target_resolution", error, args.json));
    }
    if args.detach {
        return git::resolve_commit(cwd, args.start_point.as_deref().unwrap_or("HEAD"))
            .map_err(|error| git_error("target_resolution", error, args.json));
    }
    if let Some(start) = &args.start_point {
        return git::resolve_commit(cwd, start)
            .map_err(|error| git_error("target_resolution", error, args.json));
    }
    let branch = destination
        .file_name()
        .ok_or_else(|| {
            RamizError::new(
                "invalid_destination",
                "destination has no basename",
                args.json,
            )
        })?
        .to_string_lossy();
    let reference = format!("refs/heads/{branch}");
    match git::ref_oid(cwd, &reference)
        .map_err(|error| git_error("branch_preflight", error, args.json))?
    {
        Some(oid) => Ok(oid),
        None => {
            git::head_oid(cwd).map_err(|error| git_error("target_resolution", error, args.json))
        }
    }
}

#[cfg(unix)]
fn materialize_target_symlinks(
    destination: &Path,
    links: &BTreeMap<PathBuf, Vec<u8>>,
) -> Result<(), std::io::Error> {
    use std::os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::ffi::OsStrExt,
    };

    for (relative, data) in links {
        let mut directory = fs::File::open(destination)?;
        let parent = relative.parent().unwrap_or_else(|| Path::new(""));
        for component in parent.components() {
            let Component::Normal(name) = component else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("unsafe target symlink path {}", relative.display()),
                ));
            };
            let name = CString::new(name.as_bytes()).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL")
            })?;
            // SAFETY: directory is a live descriptor, name is a valid C string,
            // and openat retains neither. O_NOFOLLOW prevents ancestor escape.
            let mut fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
                // SAFETY: the same descriptor-relative path is used, so mkdirat
                // cannot be redirected through a symlinked ancestor.
                if unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o777) } != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // SAFETY: same invariants as the first openat call.
                fd = unsafe {
                    libc::openat(
                        directory.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
            }
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: fd is newly owned and valid after successful openat.
            directory = unsafe { fs::File::from(OwnedFd::from_raw_fd(fd)) };
        }
        let leaf = relative.file_name().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "symlink path has no leaf")
        })?;
        let leaf = CString::new(leaf.as_bytes()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL")
        })?;
        let target = CString::new(data.as_slice()).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "symlink target contains NUL",
            )
        })?;
        // SAFETY: both strings and the parent descriptor live through the call;
        // symlinkat creates only the leaf relative to the no-follow traversal.
        if unsafe { libc::symlinkat(target.as_ptr(), directory.as_raw_fd(), leaf.as_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

fn validate_target_symlink_ancestors(
    donor: &Path,
    links: &BTreeSet<PathBuf>,
) -> Result<(), String> {
    for link in links {
        let mut ancestor = PathBuf::new();
        for component in link.parent().unwrap_or_else(|| Path::new("")).components() {
            let Component::Normal(name) = component else {
                return Err(format!("unsafe target symlink path {}", link.display()));
            };
            ancestor.push(name);
            match fs::symlink_metadata(donor.join(&ancestor)) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                Ok(_) => {
                    return Err(format!(
                        "target symlink {} has non-directory donor ancestor {}",
                        link.display(),
                        ancestor.display()
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => return Err(error.to_string()),
            }
        }
    }
    Ok(())
}

fn emit_success(args: &AddArgs, data: AddData, warnings: &[String]) {
    if args.json {
        output::success("add", data, warnings);
    } else {
        println!(
            "created {} at {} via {} ({} CoW, {} Git-materialized files)",
            data.path.display,
            data.head.as_deref().unwrap_or("unborn"),
            data.backend,
            data.cow_files,
            data.git_files
        );
        for warning in warnings {
            eprintln!("warning: {warning}");
        }
    }
}

fn native_add(
    cwd: &Path,
    destination: &Path,
    args: &AddArgs,
    checkout_ref: Option<&str>,
) -> Result<(), git::GitFailure> {
    let mut owned = vec![
        OsString::from("worktree"),
        OsString::from("add"),
        OsString::from("--no-checkout"),
    ];
    owned.push(OsString::from("--no-guess-remote"));
    if args.detach {
        owned.push(OsString::from("--detach"));
    }
    owned.push(destination.as_os_str().to_owned());
    if let Some(reference) = checkout_ref {
        owned.push(OsString::from(reference));
    } else if let Some(start) = &args.start_point {
        owned.push(OsString::from(start));
    }
    let borrowed: Vec<&OsStr> = owned.iter().map(OsString::as_os_str).collect();
    git::checked(cwd, "git worktree add", &borrowed).map(|_| ())
}

struct PreparedBranch {
    checkout_ref: Option<String>,
    expected_target: Option<String>,
    owned_ref: Option<String>,
    owned_oid: Option<String>,
}

fn prepare_branch(
    cwd: &Path,
    destination: &Path,
    args: &AddArgs,
) -> Result<PreparedBranch, RamizError> {
    if args.detach {
        let expression = args.start_point.as_deref().unwrap_or("HEAD");
        let target = git::resolve_commit(cwd, expression)
            .map_err(|error| git_error("target_resolution", error, args.json))?;
        return Ok(PreparedBranch {
            checkout_ref: args.start_point.clone(),
            expected_target: Some(target),
            owned_ref: None,
            owned_oid: None,
        });
    }

    let branch = if let Some(branch) = &args.branch {
        Some((branch.clone(), true))
    } else if args.start_point.is_none() {
        Some((
            destination
                .file_name()
                .ok_or_else(|| {
                    RamizError::new(
                        "invalid_destination",
                        "destination has no branch basename",
                        args.json,
                    )
                })?
                .to_string_lossy()
                .into_owned(),
            false,
        ))
    } else {
        None
    };

    let Some((branch, explicit_new)) = branch else {
        let expression = args.start_point.as_deref().expect("start point exists");
        let target = git::resolve_commit(cwd, expression)
            .map_err(|error| git_error("target_resolution", error, args.json))?;
        return Ok(PreparedBranch {
            checkout_ref: Some(expression.to_owned()),
            expected_target: Some(target),
            owned_ref: None,
            owned_oid: None,
        });
    };

    let reference = format!("refs/heads/{branch}");
    if let Some(oid) = git::ref_oid(cwd, &reference)
        .map_err(|error| git_error("branch_preflight", error, args.json))?
    {
        if explicit_new {
            return Err(RamizError::new(
                "branch_exists",
                format!("branch '{branch}' already exists"),
                args.json,
            ));
        }
        return Ok(PreparedBranch {
            checkout_ref: Some(branch),
            expected_target: Some(oid),
            owned_ref: None,
            owned_oid: None,
        });
    }

    let target_expression = args.start_point.as_deref().unwrap_or("HEAD");
    let target = git::resolve_commit(cwd, target_expression)
        .map_err(|error| git_error("target_resolution", error, args.json))?;
    git::create_ref(cwd, &reference, &target)
        .map_err(|error| git_error("branch_creation_failed", error, args.json))?;
    Ok(PreparedBranch {
        checkout_ref: Some(branch),
        expected_target: Some(target.clone()),
        owned_ref: Some(reference),
        owned_oid: Some(target),
    })
}

fn validate_destination(
    destination: &Path,
    repository: Option<&Path>,
    donor: Option<&Path>,
    common: Option<&Path>,
    json: bool,
) -> Result<(), RamizError> {
    if repository.is_some_and(|root| canonical_or_self(root) == canonical_or_self(destination))
        || donor.is_some_and(|root| canonical_or_self(root) == canonical_or_self(destination))
        || common.is_some_and(|root| destination.starts_with(canonical_or_self(root)))
    {
        return Err(RamizError::new(
            "unsafe_destination",
            "destination cannot be the repository or donor root",
            json,
        ));
    }
    match fs::symlink_metadata(destination) {
        Ok(meta) if !meta.is_dir() => Err(RamizError::new(
            "destination_exists",
            "destination exists and is not a directory",
            json,
        )),
        Ok(_)
            if fs::read_dir(destination)
                .map_err(|error| RamizError::new("destination_read", error.to_string(), json))?
                .next()
                .is_some() =>
        {
            Err(RamizError::new(
                "destination_not_empty",
                "destination exists and is not empty",
                json,
            ))
        }
        _ => Ok(()),
    }
}

fn resolve_destination(cwd: &Path, requested: &Path) -> Result<PathBuf, String> {
    let absolute = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        cwd.join(requested)
    };
    let mut existing = absolute.as_path();
    let mut tail = Vec::new();
    while !existing.exists() {
        let name = existing
            .file_name()
            .ok_or_else(|| format!("invalid destination {}", requested.display()))?;
        tail.push(name.to_owned());
        existing = existing
            .parent()
            .ok_or_else(|| format!("no existing parent for {}", requested.display()))?;
    }
    let mut resolved = fs::canonicalize(existing)
        .map_err(|error| format!("resolve {}: {error}", existing.display()))?;
    for component in tail.into_iter().rev() {
        resolved.push(component);
    }
    if resolved
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err("destination contains unresolved parent components".into());
    }
    Ok(resolved)
}

fn canonical_or_self(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn git_error(code: &'static str, error: git::GitFailure, json: bool) -> RamizError {
    RamizError::new(code, error.to_string(), json)
}

fn fail_with_rollback(
    code: &'static str,
    error: git::GitFailure,
    json: bool,
    transaction: &mut Transaction,
) -> RamizError {
    let mut failure = git_error(code, error, json);
    if let Some(cleanup) = transaction.rollback() {
        failure = failure.with_cleanup(cleanup);
    }
    failure
}

fn io_failure(
    code: &'static str,
    error: std::io::Error,
    json: bool,
    transaction: &mut Transaction,
) -> RamizError {
    let mut failure = RamizError::new(code, error.to_string(), json);
    if let Some(cleanup) = transaction.rollback() {
        failure = failure.with_cleanup(cleanup);
    }
    failure
}

fn fail_message(
    code: &'static str,
    message: impl Into<String>,
    json: bool,
    transaction: &mut Transaction,
) -> RamizError {
    let mut failure = RamizError::new(code, message, json);
    if let Some(cleanup) = transaction.rollback() {
        failure = failure.with_cleanup(cleanup);
    }
    failure
}

fn check_cancelled(json: bool, transaction: &mut Transaction) -> Result<(), RamizError> {
    if cancellation::requested() {
        return Err(fail_message(
            "interrupted",
            "creation interrupted",
            json,
            transaction,
        ));
    }
    Ok(())
}

#[cfg(debug_assertions)]
fn clean_worker_test_gate() {
    let (Ok(marker), Ok(release)) = (
        std::env::var("RAMIZ_TEST_CLEAN_WORKER_MARKER"),
        std::env::var("RAMIZ_TEST_CLEAN_WORKER_RELEASE"),
    ) else {
        return;
    };
    if fs::write(&marker, b"ready").is_err() {
        return;
    }
    while !Path::new(&release).exists() && !cancellation::requested() {
        thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[cfg(debug_assertions)]
fn inheritance_test_gate() {
    let (Ok(marker), Ok(release)) = (
        std::env::var("RAMIZ_TEST_INHERIT_MARKER"),
        std::env::var("RAMIZ_TEST_INHERIT_RELEASE"),
    ) else {
        return;
    };
    if fs::write(&marker, b"ready").is_err() {
        return;
    }
    for _ in 0..1_000 {
        if Path::new(&release).exists() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[cfg(not(debug_assertions))]
fn inheritance_test_gate() {}

fn forward_hook_output(output: &std::process::Output) {
    let _ = std::io::stdout().write_all(&output.stdout);
    let _ = std::io::stderr().write_all(&output.stderr);
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

struct Transaction {
    repository: PathBuf,
    destination: PathBuf,
    existed_empty: bool,
    branch_ref: Option<String>,
    created_oid: Option<String>,
    registered: bool,
    rollback_enabled: bool,
}

impl Transaction {
    fn rollback(&mut self) -> Option<String> {
        if !self.rollback_enabled {
            return None;
        }
        self.rollback_enabled = false;
        let mut failures = Vec::new();
        let mut removed = false;
        if self.registered {
            let result = git::checked(
                &self.repository,
                "git worktree rollback",
                &[
                    OsStr::new("worktree"),
                    OsStr::new("remove"),
                    OsStr::new("--force"),
                    self.destination.as_os_str(),
                ],
            );
            match result {
                Ok(_) => removed = true,
                Err(error) => failures.push(error.to_string()),
            }
        }
        if self.existed_empty && removed && !self.destination.exists() {
            if let Err(error) = fs::create_dir_all(&self.destination) {
                failures.push(format!(
                    "restore empty {}: {error}",
                    self.destination.display()
                ));
            }
        }
        if let (Some(reference), Some(oid)) = (&self.branch_ref, &self.created_oid) {
            match git::branch_checked_out(&self.repository, reference) {
                Ok(false) => {
                    if let Err(error) = git::delete_ref_if(&self.repository, reference, oid) {
                        failures.push(error.to_string());
                    }
                }
                Ok(true) => failures.push(format!(
                    "owned branch {reference} is checked out; refusing to delete it"
                )),
                Err(error) => failures.push(error.to_string()),
            }
        }
        if failures.is_empty() {
            None
        } else {
            Some(format!(
                "{}; recover with `git worktree repair` and inspect `{}`",
                failures.join("; "),
                self.destination.display()
            ))
        }
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        let _ = self.rollback();
    }
}
