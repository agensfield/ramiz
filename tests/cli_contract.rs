use std::process::Command;

#[test]
fn json_usage_failure_is_one_versioned_envelope() {
    let output = Command::new(env!("CARGO_BIN_EXE_ramiz"))
        .args([
            "add",
            "--json",
            "--inherit",
            "--require-cow",
            "--allow-copy",
            "destination",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stderr.is_empty());
    assert_eq!(
        output.stdout.iter().filter(|byte| **byte == b'\n').count(),
        1
    );
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["schema"], "ramiz.cli/v1");
    assert_eq!(envelope["ok"], false);
    assert_eq!(envelope["command"], "add");
    assert_eq!(envelope["error"]["code"], "cli_usage");
}

#[test]
fn both_invocation_binaries_report_the_same_version() {
    let direct = Command::new(env!("CARGO_BIN_EXE_ramiz"))
        .arg("--version")
        .output()
        .unwrap();
    let git_extension = Command::new(env!("CARGO_BIN_EXE_git-ramiz"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(direct.status.success());
    assert!(git_extension.status.success());
    assert_eq!(direct.stdout, git_extension.stdout);
    assert_eq!(
        String::from_utf8(direct.stdout).unwrap().trim(),
        format!("ramiz {}", env!("CARGO_PKG_VERSION"))
    );

    let extension_dir = std::path::Path::new(env!("CARGO_BIN_EXE_git-ramiz"))
        .parent()
        .unwrap();
    let inherited_path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![extension_dir.to_path_buf()];
    paths.extend(std::env::split_paths(&inherited_path));
    let dispatched = Command::new("git")
        .args(["ramiz", "--version"])
        .env("PATH", std::env::join_paths(paths).unwrap())
        .output()
        .unwrap();
    assert!(
        dispatched.status.success(),
        "{}",
        String::from_utf8_lossy(&dispatched.stderr)
    );
    assert_eq!(dispatched.stdout, git_extension.stdout);
}

#[test]
fn update_failure_preserves_structured_cli_state() {
    let output = Command::new(env!("CARGO_BIN_EXE_ramiz"))
        .args(["update", "--json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stderr.is_empty());
    assert_eq!(
        output.stdout.iter().filter(|byte| **byte == b'\n').count(),
        1
    );
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["schema"], "ramiz.cli/v1");
    assert_eq!(envelope["ok"], false);
    assert_eq!(envelope["command"], "update");
    assert_eq!(envelope["error"]["code"], "unknown_installer");
    assert_eq!(envelope["error"]["update"]["installer"], "unknown");
    assert_eq!(envelope["error"]["update"]["applied"], false);
    assert_eq!(envelope["error"]["update"]["changed"], false);
    assert!(envelope["error"]["update"]["executable"]["display"].is_string());
    assert!(envelope["error"]["update"]["executable"]["bytes_base64"].is_string());
}
