//! Integration coverage for the `pummel` binary.
//!
//! These exercise the CLI paths that do not require a live HTTP target:
//! config loading/validation, `--dry-run`, `--graph`, and the exit-code
//! contract (0 = pass, 1 = usage/config error, 2 = threshold breach).

use std::io::Write;
use std::process::Command;

use tempfile::NamedTempFile;

/// Path to the compiled CLI binary (Cargo provides this env var to tests).
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_pummel")
}

/// Write `contents` to a temp file with the given extension so the CLI's
/// format detection (by extension) works.
fn config_file(extension: &str, contents: &str) -> NamedTempFile {
    let mut file = tempfile::Builder::new()
        .suffix(&format!(".{extension}"))
        .tempfile()
        .unwrap();
    file.write_all(contents.as_bytes()).unwrap();
    file.flush().unwrap();
    file
}

const VALID_TOML: &str = r#"
[global]
base_url = "https://example.com"
virtual_users = 3

[scenarios.smoke]
name = "Smoke"
steps = ["home"]

[steps.home]
name = "Home"
method = "GET"
url = "/"
"#;

#[test]
fn dry_run_valid_config_exits_zero_with_summary() {
    let cfg = config_file("toml", VALID_TOML);
    let out = Command::new(bin())
        .arg("--config")
        .arg(cfg.path())
        .arg("--dry-run")
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "expected exit 0, got {:?}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Configuration is valid"),
        "summary missing from stdout: {stdout}"
    );
    // Global virtual_users (3) is inherited by the scenario that omits it.
    assert!(
        stdout.contains("3 user(s)"),
        "resolved users wrong: {stdout}"
    );
}

#[test]
fn validate_alias_works() {
    let cfg = config_file("toml", VALID_TOML);
    let out = Command::new(bin())
        .arg("--config")
        .arg(cfg.path())
        .arg("--validate")
        .output()
        .unwrap();
    assert!(out.status.success());
}

#[test]
fn users_override_is_applied_to_dry_run_summary() {
    let cfg = config_file("toml", VALID_TOML);
    let out = Command::new(bin())
        .arg("--config")
        .arg(cfg.path())
        .arg("--dry-run")
        .arg("--users")
        .arg("42")
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("42 user(s)"),
        "--users override not applied: {stdout}"
    );
}

#[test]
fn zero_virtual_users_is_rejected() {
    let toml = r#"
[scenarios.smoke]
name = "Smoke"
steps = ["home"]
virtual_users = 0

[steps.home]
name = "Home"
method = "GET"
url = "https://example.com/"
"#;
    let cfg = config_file("toml", toml);
    let out = Command::new(bin())
        .arg("--config")
        .arg(cfg.path())
        .arg("--dry-run")
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected usage/config error exit 1; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn unknown_config_field_is_rejected() {
    // deny_unknown_fields: a typo must be a hard error, not a silent no-op.
    let toml = r#"
[scenarios.smoke]
name = "Smoke"
steps = []
virtual_userz = 50
"#;
    let cfg = config_file("toml", toml);
    let out = Command::new(bin())
        .arg("--config")
        .arg(cfg.path())
        .arg("--dry-run")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn graph_visualizes_and_exits_without_running() {
    let cfg = config_file("toml", VALID_TOML);
    let out = Command::new(bin())
        .arg("--config")
        .arg(cfg.path())
        .arg("--graph")
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Mermaid output (the default graph format) starts with "graph TD".
    assert!(
        stdout.contains("graph TD"),
        "graph missing on stdout: {stdout}"
    );
    // The run itself never started, so no results are printed.
    assert!(!stdout.contains("Load Test Results"));
}

#[test]
fn unsupported_extension_is_rejected() {
    let cfg = config_file("txt", "not a config");
    let out = Command::new(bin())
        .arg("--config")
        .arg(cfg.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
}
