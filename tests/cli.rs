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

/// Target CLI wording for common config mistakes (#36).
///
/// Emit path: `src/bin/cli.rs` `main` prints `error: {err}` after `load_config`
/// (`Config::from_toml` → `parse_config_error` in `src/config.rs`) or after
/// `Engine::apply_config` / `build_scenarios` (`invalid_request_url` in
/// `src/http.rs`, `Config::validate`).
///
/// These assert the *clearer* messages we want (field/path + what was wrong),
/// not today's serde dump + tacked-on hint. They fail on current main and go
/// green when Code Optimizer rewrites the emit path. Concrete before/after:
///
/// missing required field
///   before: Failed to parse TOML: TOML parse error … missing field `name`.
///           Required step fields are name and url…
///   after:  missing required field `name` at steps.a.name; each step needs `name` and `url`
///
/// bad URL
///   before: Request URL '/api' is not a valid absolute http(s) URL (relative
///           URL without a base). Use a full URL … or set [global] base_url …
///   after:  steps.a.url '/api' is not an absolute http(s) URL; set
///           [global].base_url or use a full https:// URL
///
/// invalid duration
///   before: Failed to parse TOML: … invalid type: string "30s", expected u64.
///           This field is an integer count, not a string…
///   after:  global.duration_seconds must be an integer number of seconds
///           (for example 30), not the string "30s"
///
/// unknown field
///   before: Failed to parse TOML: … unknown field `virtual_userz`, expected
///           one of `name`, `steps`, `virtual_users`, …
///   after:  unknown field `virtual_userz` at scenarios.smoke.virtual_userz;
///           did you mean `virtual_users`?
///
/// zero virtual users
///   before: Scenario 'smoke' virtual_users must be positive
///   after:  scenarios.smoke.virtual_users is 0; it must be a positive integer
#[test]
fn common_config_mistakes_name_field_path_and_reason() {
    struct Case {
        name: &'static str,
        toml: &'static str,
        needles: &'static [&'static str],
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
            needles: &["steps.a.name", "missing required field"],
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
            needles: &["steps.a.url", "/api", "[global].base_url"],
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
            needles: &["global.duration_seconds", "\"30s\"", "number of seconds"],
        },
        Case {
            name: "unknown field",
            toml: r#"
[scenarios.smoke]
name = "Smoke"
steps = []
virtual_userz = 50
"#,
            needles: &[
                "scenarios.smoke.virtual_userz",
                "unknown field",
                "did you mean",
                "virtual_users",
            ],
        },
        Case {
            name: "zero virtual users",
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
            needles: &[
                "scenarios.smoke.virtual_users",
                "virtual_users is 0",
                "positive integer",
            ],
        },
    ];

    for case in cases {
        let (code, stderr) = dry_run_stderr(case.toml);
        assert_eq!(
            code,
            Some(1),
            "{}: expected config-error exit 1; stderr: {stderr}",
            case.name
        );
        for needle in case.needles {
            assert!(
                stderr.contains(needle),
                "{}: stderr should contain {needle:?} (clearer field/path + reason); got: {stderr}",
                case.name
            );
        }
    }
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
