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

fn run_cli(args: &[&str]) -> std::process::Output {
    Command::new(bin()).args(args).output().unwrap()
}

/// Run `pummel --config <file> --dry-run` and return stderr.
fn dry_run_stderr(toml: &str) -> (Option<i32>, String) {
    let cfg = config_file("toml", toml);
    let out = Command::new(bin())
        .arg("--config")
        .arg(cfg.path())
        .arg("--dry-run")
        .output()
        .unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
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
fn missing_config_exits_one() {
    let out = run_cli(&[]);

    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--config"),
        "missing-argument details absent from stderr"
    );
}

#[test]
fn unknown_option_exits_one() {
    let out = run_cli(&["--definitely-unknown"]);

    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("unexpected argument"),
        "unknown-option details absent from stderr"
    );
}

#[test]
fn invalid_value_exits_one() {
    let out = run_cli(&["--config", "unused.toml", "--format", "xml"]);

    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("invalid value"),
        "invalid-value details absent from stderr"
    );
}

#[test]
fn help_exits_zero() {
    let out = run_cli(&["--help"]);

    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("Usage:"),
        "help text absent from stdout"
    );
}

#[test]
fn version_exits_zero() {
    let out = run_cli(&["--version"]);

    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(env!("CARGO_PKG_VERSION")),
        "version text absent from stdout"
    );
}

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

/// Trimmed primary CLI line: `src/bin/cli.rs` prints `error: {err}` and
/// `Error::Config` displays `Configuration error: {0}`. Tracing may also write
/// to stderr, so callers must extract this line rather than match the whole
/// buffer.
fn primary_config_error_line(stderr: &str) -> &str {
    stderr
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("error: Configuration error:"))
        .unwrap_or("")
}

fn assert_primary_config_error(name: &str, code: Option<i32>, stderr: &str, expected: &str) {
    assert_eq!(
        code,
        Some(1),
        "{name}: expected config-error exit 1; stderr: {stderr}"
    );
    assert_eq!(
        primary_config_error_line(stderr),
        expected,
        "{name}: primary stderr line mismatch; full stderr: {stderr}"
    );
    assert!(
        !stderr.contains("Failed to parse"),
        "{name}: recognized config errors must not dump parser text; stderr: {stderr}"
    );
}

/// Target CLI wording for common config mistakes (#36).
///
/// Contract: schema errors are `reason at path; optional hint`; value errors
/// are `path reason; optional hint`. The complete primary line is
/// `error: Configuration error: <payload>` — not unordered substring needles.
///
/// Pins that fail until Code Optimizer lands the remaining rewrites:
/// - bad URL hint uses `global.base_url`, not `[global].base_url`
/// - indexed stage-zero uses `scenarios.smoke.load_profile.stages[0].virtual_users`
/// - YAML missing-field uses the same primary line as TOML (no serde dump)
#[test]
fn common_config_mistakes_name_field_path_and_reason() {
    struct Case {
        name: &'static str,
        toml: &'static str,
        primary: &'static str,
    }

    let cases = [
        Case {
            name: "missing required field",
            toml: r#"
[scenarios.s]
name = "S"
steps = ["a"]

[steps.a]
url = "https://example.com/"
"#,
            primary: "error: Configuration error: missing required field `name` at steps.a.name; each step needs `name` and `url`",
        },
        Case {
            name: "bad URL",
            toml: r#"
[scenarios.s]
name = "S"
steps = ["a"]

[steps.a]
name = "A"
method = "GET"
url = "/api"
"#,
            primary: "error: Configuration error: steps.a.url '/api' is not an absolute http(s) URL; set global.base_url or use a full https:// URL",
        },
        Case {
            name: "invalid duration",
            toml: r#"
[global]
duration_seconds = "30s"

[scenarios.s]
name = "S"
steps = ["a"]

[steps.a]
name = "A"
method = "GET"
url = "https://example.com/"
"#,
            primary: r#"error: Configuration error: global.duration_seconds must be an integer number of seconds (for example 30), not the string "30s""#,
        },
        Case {
            name: "unknown field",
            toml: r#"
[scenarios.smoke]
name = "Smoke"
steps = []
virtual_userz = 50
"#,
            primary: "error: Configuration error: unknown field `virtual_userz` at scenarios.smoke.virtual_userz; did you mean `virtual_users`?",
        },
        Case {
            name: "zero scenario virtual users",
            toml: r#"
[scenarios.smoke]
name = "Smoke"
steps = ["home"]
virtual_users = 0

[steps.home]
name = "Home"
method = "GET"
url = "https://example.com/"
"#,
            primary: "error: Configuration error: scenarios.smoke.virtual_users is 0; it must be a positive integer",
        },
        Case {
            name: "zero global virtual users",
            toml: r#"
[global]
virtual_users = 0

[scenarios.smoke]
name = "Smoke"
steps = ["home"]

[steps.home]
name = "Home"
method = "GET"
url = "https://example.com/"
"#,
            primary: "error: Configuration error: global.virtual_users is 0; it must be a positive integer",
        },
        Case {
            name: "zero indexed load-stage virtual users",
            toml: r#"
[scenarios.smoke]
name = "Smoke"
steps = ["home"]

[scenarios.smoke.load_profile]
stages = [{ duration_seconds = 1, virtual_users = 0 }]

[steps.home]
name = "Home"
method = "GET"
url = "https://example.com/"
"#,
            primary: "error: Configuration error: scenarios.smoke.load_profile.stages[0].virtual_users is 0; it must be a positive integer",
        },
    ];

    for case in cases {
        let (code, stderr) = dry_run_stderr(case.toml);
        assert_primary_config_error(case.name, code, &stderr, case.primary);
    }

    // Same missing-field payload via YAML. Cheap parity pin; must not lock in
    // the serde dump `from_yaml_str` still emits today.
    let yaml = r#"
scenarios:
  s:
    name: S
    steps: ["a"]
steps:
  a:
    url: "https://example.com/"
"#;
    let cfg = config_file("yaml", yaml);
    let out = Command::new(bin())
        .arg("--config")
        .arg(cfg.path())
        .arg("--dry-run")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_primary_config_error(
        "missing required field (yaml)",
        out.status.code(),
        &stderr,
        "error: Configuration error: missing required field `name` at steps.a.name; each step needs `name` and `url`",
    );
}

#[test]
fn unsupported_telemetry_exporter_lists_supported_values() {
    let toml = r#"
[telemetry]
enabled = true
exporter = "otlp"

[scenarios.s]
name = "S"
steps = ["a"]

[steps.a]
name = "A"
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
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("json, console, noop, none") && stderr.contains("otlp"),
        "telemetry error should list supported exporters: {stderr}"
    );
}

#[test]
fn missing_config_file_includes_path() {
    let out = run_cli(&["--config", "does-not-exist-pummel.toml", "--dry-run"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("does-not-exist-pummel.toml") && stderr.contains("--config"),
        "missing file error should include the path: {stderr}"
    );
}
