use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, ValueEnum};

use pummel::config::ThresholdsConfig;
use pummel::metrics::RunStatus;
use pummel::prelude::*;

/// Process exit codes. The contract is stable and documented in the README:
/// callers (CI in particular) may rely on these.
mod exit {
    /// Run completed and all thresholds passed.
    pub const OK: u8 = 0;
    /// Usage / config / build error (surfaced as an `Err` from `run`).
    pub const ERROR: u8 = 1;
    /// A pass/fail threshold was breached.
    pub const THRESHOLD_BREACH: u8 = 2;
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
enum OutputFormat {
    /// Text output format
    Text,
    /// JSON output format
    Json,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
enum GraphOutputFormat {
    /// Mermaid graph format
    Mermaid,
    /// DOT graph format
    Dot,
}

/// Load Tester CLI - A high-throughput, multithreaded HTTP load testing tool
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Path to the configuration file (YAML or TOML)
    #[arg(short, long)]
    config: PathBuf,

    /// Output format for results
    #[arg(short, long, value_enum, default_value_t = OutputFormat::Text)]
    format: OutputFormat,

    /// Print the dependency graph and exit without running the load test
    #[arg(short, long)]
    graph: bool,

    /// Graph output format
    #[arg(short = 'G', long, value_enum, default_value_t = GraphOutputFormat::Mermaid)]
    graph_format: GraphOutputFormat,

    /// Validate the configuration (load, build, and check every scenario) then
    /// exit without generating load. Exit code is non-zero if the config is
    /// invalid, making this a config-check mode for CI.
    #[arg(long, visible_alias = "validate")]
    dry_run: bool,

    /// Override the global number of virtual users. Scenarios that set their own
    /// `virtual_users` still take precedence.
    #[arg(long)]
    users: Option<u32>,

    /// Override the global sustained-load duration, in seconds.
    #[arg(long)]
    duration: Option<u64>,

    /// Override the global ramp-up period, in seconds.
    #[arg(long)]
    ramp_up: Option<u64>,

    /// Aggregate request-attempt starts per second across active scenarios.
    /// Retries consume permits. Omit for think-time pacing between passes.
    #[arg(long)]
    target_rps: Option<f64>,

    /// Fail the run (exit code 2) if the error rate exceeds this value (0.0-1.0).
    /// Overrides `[thresholds] max_error_rate` from the config file.
    #[arg(long)]
    max_error_rate: Option<f64>,

    /// Enable verbose output
    #[arg(short, long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            let code = if err.use_stderr() {
                exit::ERROR
            } else {
                exit::OK
            };
            if let Err(print_err) = err.print() {
                eprintln!("error: {print_err}");
                return ExitCode::from(exit::ERROR);
            }
            return ExitCode::from(code);
        }
    };

    // Set up logging based on verbosity. Logs go to stderr, so stdout carries
    // only results output (see `print_json_results`).
    logging::init(cli.verbose);

    match run(cli).await {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            // Fatal errors go to stderr regardless of log filtering so a
            // `RUST_LOG=off` invocation still reports why it failed.
            eprintln!("error: {err}");
            ExitCode::from(exit::ERROR)
        }
    }
}

/// Drive a run end-to-end, returning the intended process exit code.
async fn run(cli: Cli) -> Result<u8> {
    let config_path = cli.config.as_path();
    logging::info!("Loading configuration from {}", config_path.display());
    let mut config = load_config(config_path)?;

    // Apply CLI overrides to the resolved [global] values BEFORE building
    // scenarios, so the overrides flow through the global->scenario inheritance
    // in `build_scenarios`. Scenarios that specify their own value still win.
    if let Some(users) = cli.users {
        config.global.virtual_users = users;
    }
    if let Some(duration) = cli.duration {
        config.global.duration_seconds = duration;
    }
    if let Some(ramp_up) = cli.ramp_up {
        config.global.ramp_up_seconds = ramp_up;
    }
    // The CLI flag overrides any `[thresholds] max_error_rate` from the file.
    if let Some(max_error_rate) = cli.max_error_rate {
        config.thresholds.max_error_rate = Some(max_error_rate);
    }
    if let Some(target_rps) = cli.target_rps
        && (!target_rps.is_finite() || target_rps <= 0.0)
    {
        return Err(Error::config("target_rps must be finite and positive"));
    }

    let mut engine = Engine::new();
    engine.apply_config(&config)?;

    // Build (and thereby validate: cycles, dependency existence, virtual_users
    // >= 1) every scenario. An invalid config fails here with a non-zero exit.
    logging::info!("Building scenarios from configuration...");
    let scenarios = config.build_scenarios()?;

    // --dry-run / --validate: config is valid, report a summary and stop.
    if cli.dry_run {
        print_dry_run_summary(&scenarios);
        return Ok(exit::OK);
    }

    for scenario in scenarios {
        logging::info!("Adding scenario: {}", scenario.name);
        engine.add_scenario(scenario);
    }

    // --graph: visualize and exit without running the load test.
    // `apply_config` already ran above, so an enabled-but-invalid telemetry
    // section still fails before visualization (graph does not skip factory wiring).
    if cli.graph {
        let graph_format = match cli.graph_format {
            GraphOutputFormat::Mermaid => GraphFormat::Mermaid,
            GraphOutputFormat::Dot => GraphFormat::Dot,
        };
        let graph = engine.visualize_graph(graph_format)?;
        println!("{graph}");
        return Ok(exit::OK);
    }

    // Build execution options. The per-scenario load parameters (virtual_users,
    // duration, ramp_up, think_time) are taken from each built scenario inside
    // `run_all`, so only the scenario-independent knobs are set here.
    let options = ExecutionOptions::builder()
        .max_concurrent_requests(config.http.max_concurrent_requests)
        .target_rps(cli.target_rps)
        .build();

    logging::info!("Starting load test...");
    let results = engine.run_all(options).await?;

    // Print results first (to stdout), then gate on thresholds.
    match cli.format {
        OutputFormat::Text => print_text_results(&results),
        OutputFormat::Json => print_json_results(&results)?,
    }

    match &results.status {
        RunStatus::Completed => {}
        RunStatus::Truncated { reason } => {
            eprintln!("run truncated: {reason}");
            return Ok(exit::ERROR);
        }
        RunStatus::Failed { reason } => {
            eprintln!("run failed: {reason}");
            return Ok(exit::ERROR);
        }
        _ => {
            eprintln!("run ended with an unsupported status");
            return Ok(exit::ERROR);
        }
    }

    let breaches = evaluate_thresholds(&results, &config.thresholds);
    if breaches.is_empty() {
        Ok(exit::OK)
    } else {
        for breach in &breaches {
            eprintln!("threshold breach: {breach}");
        }
        Ok(exit::THRESHOLD_BREACH)
    }
}

/// Evaluate the configured pass/fail thresholds against a completed run,
/// returning a human-readable message for each breach (empty => all passed).
fn evaluate_thresholds(results: &TestResults, thresholds: &ThresholdsConfig) -> Vec<String> {
    let mut breaches = Vec::new();

    if let Some(max) = thresholds.max_error_rate
        && results.error_rate > max
    {
        breaches.push(format!(
            "error rate {:.4} exceeds max_error_rate {max:.4}",
            results.error_rate
        ));
    }
    if let Some(max) = thresholds.max_p90_ms
        && results.p90_response_time_ms > max
    {
        breaches.push(format!(
            "p90 response time {}ms exceeds max_p90_ms {max}ms",
            results.p90_response_time_ms
        ));
    }
    if let Some(min) = thresholds.min_requests
        && results.total_requests < min
    {
        breaches.push(format!(
            "total requests {} below min_requests {min}",
            results.total_requests
        ));
    }

    breaches
}

fn load_config(path: &Path) -> Result<Config> {
    let extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");

    match extension.to_lowercase().as_str() {
        "yaml" | "yml" => Config::from_yaml(path),
        "toml" => Config::from_toml(path),
        _ => Err(Error::config(format!(
            "Unsupported config file extension '{extension}' for '{}'. \
             Use a .yaml, .yml, or .toml file with --config",
            path.display()
        ))),
    }
}

/// Print a summary of the built scenarios for `--dry-run`. Goes to stdout so it
/// can be captured/parsed independently of the stderr logs.
fn print_dry_run_summary(scenarios: &[Scenario]) {
    println!("Configuration is valid. {} scenario(s):", scenarios.len());
    for scenario in scenarios {
        println!(
            "  - {} ({}): {} user(s), duration {}s, ramp-up {}s, {} step(s)",
            scenario.id,
            scenario.name,
            scenario.virtual_users,
            scenario.duration.as_secs(),
            scenario.ramp_up.as_secs(),
            scenario.steps.len(),
        );
    }
}

fn print_text_results(results: &TestResults) {
    print!("{}", format_text_results(results));
}

fn run_status_label(status: &RunStatus) -> String {
    match status {
        RunStatus::Completed => "completed".to_string(),
        RunStatus::Truncated { reason } => format!("truncated ({reason})"),
        RunStatus::Failed { reason } => format!("failed ({reason})"),
        _ => "unknown".to_string(),
    }
}

/// Human-readable default CLI summary. JSON (`--format json`) remains the
/// machine-readable dump of the full [`TestResults`] struct.
fn format_text_results(results: &TestResults) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Load Test Results: {}",
        run_status_label(&results.status)
    );
    let _ = writeln!(out, "Virtual users: {}", results.total_virtual_users);
    let _ = writeln!(out, "Duration: {:.2}s", results.duration_seconds);
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Requests: {} total, {} ok, {} failed ({:.2}% errors)",
        results.total_requests,
        results.successful_requests,
        results.failed_requests,
        results.error_rate * 100.0
    );
    let _ = writeln!(out, "Throughput: {:.2} req/s", results.requests_per_second);
    let _ = writeln!(
        out,
        "Latency: avg {:.2}ms, p90 {}ms",
        results.avg_response_time_ms, results.p90_response_time_ms
    );

    if results.scenarios.is_empty() {
        return out;
    }

    let mut scenario_ids: Vec<_> = results.scenarios.keys().cloned().collect();
    scenario_ids.sort();
    let _ = writeln!(out);
    for id in scenario_ids {
        let scenario = &results.scenarios[&id];
        let _ = writeln!(
            out,
            "Scenario {id} ({}): {} user(s), {} req, {} failed, p90 {}ms, {:.2} req/s",
            scenario.name,
            scenario.virtual_users,
            scenario.total_requests,
            scenario.failed_requests,
            scenario.p90_response_time_ms,
            scenario.requests_per_second
        );
        let mut step_ids: Vec<_> = scenario.steps.keys().cloned().collect();
        step_ids.sort();
        for step_id in step_ids {
            let step = &scenario.steps[&step_id];
            let _ = writeln!(
                out,
                "  Step {step_id} ({}): {} req, {} failed, avg {:.2}ms, p50 {}ms p90 {}ms p95 {}ms p99 {}ms",
                step.name,
                step.total_requests,
                step.failed_requests,
                step.avg_response_time_ms,
                step.p50_response_time_ms,
                step.p90_response_time_ms,
                step.p95_response_time_ms,
                step.p99_response_time_ms
            );
        }
    }
    out
}

fn print_json_results(results: &TestResults) -> Result<()> {
    // Raw JSON to stdout (no logger, no ANSI, no timestamps) so
    // `pummel --format json | jq` works. Serialization failure is surfaced
    // as an error rather than swallowed into a placeholder string.
    let json = serde_json::to_string_pretty(results)
        .map_err(|e| Error::other(format!("Failed to serialize results to JSON: {e}")))?;
    println!("{json}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn results_with(error_rate: f64, p90: u64, total: u64) -> TestResults {
        TestResults {
            error_rate,
            p90_response_time_ms: p90,
            total_requests: total,
            ..Default::default()
        }
    }

    #[test]
    fn no_thresholds_never_breaches() {
        let results = results_with(1.0, 100_000, 0);
        let thresholds = ThresholdsConfig::default();
        assert!(evaluate_thresholds(&results, &thresholds).is_empty());
    }

    #[test]
    fn max_error_rate_breach_is_reported() {
        let results = results_with(0.5, 0, 100);
        let thresholds = ThresholdsConfig {
            max_error_rate: Some(0.1),
            ..Default::default()
        };
        let breaches = evaluate_thresholds(&results, &thresholds);
        assert_eq!(breaches.len(), 1);
        assert!(breaches[0].contains("error rate"));
    }

    #[test]
    fn error_rate_at_threshold_passes() {
        // The check is strictly-greater-than, so being exactly at the limit is a
        // pass (a 10% cap admits a 10% error rate).
        let results = results_with(0.1, 0, 100);
        let thresholds = ThresholdsConfig {
            max_error_rate: Some(0.1),
            ..Default::default()
        };
        assert!(evaluate_thresholds(&results, &thresholds).is_empty());
    }

    #[test]
    fn p90_and_min_requests_breaches_are_reported_together() {
        let results = results_with(0.0, 500, 5);
        let thresholds = ThresholdsConfig {
            max_error_rate: None,
            max_p90_ms: Some(200),
            min_requests: Some(10),
        };
        let breaches = evaluate_thresholds(&results, &thresholds);
        assert_eq!(breaches.len(), 2);
        assert!(breaches.iter().any(|b| b.contains("p90")));
        assert!(breaches.iter().any(|b| b.contains("total requests")));
    }

    #[test]
    fn all_thresholds_within_bounds_pass() {
        let results = results_with(0.01, 150, 1000);
        let thresholds = ThresholdsConfig {
            max_error_rate: Some(0.05),
            max_p90_ms: Some(200),
            min_requests: Some(100),
        };
        assert!(evaluate_thresholds(&results, &thresholds).is_empty());
    }

    fn sample_step(id: &str, name: &str) -> pummel::metrics::StepMetrics {
        let now = chrono::Utc::now();
        pummel::metrics::StepMetrics {
            step_id: id.to_string(),
            name: name.to_string(),
            total_requests: 10,
            successful_requests: 9,
            failed_requests: 1,
            min_response_time_ms: 5,
            max_response_time_ms: 40,
            avg_response_time_ms: 12.5,
            p50_response_time_ms: 10,
            p90_response_time_ms: 20,
            p95_response_time_ms: 25,
            p99_response_time_ms: 35,
            requests_per_second: 10.0,
            error_rate: 0.1,
            start_time: now,
            end_time: now,
            duration_seconds: 1.0,
        }
    }

    fn sample_scenario(
        id: &str,
        name: &str,
        steps: Vec<pummel::metrics::StepMetrics>,
    ) -> pummel::metrics::ScenarioMetrics {
        let now = chrono::Utc::now();
        let mut step_map = std::collections::HashMap::new();
        for step in steps {
            step_map.insert(step.step_id.clone(), step);
        }
        pummel::metrics::ScenarioMetrics {
            scenario_id: id.to_string(),
            name: name.to_string(),
            steps: step_map,
            total_requests: 10,
            successful_requests: 9,
            failed_requests: 1,
            avg_response_time_ms: 12.5,
            p90_response_time_ms: 20,
            requests_per_second: 10.0,
            error_rate: 0.1,
            start_time: now,
            end_time: now,
            duration_seconds: 1.0,
            virtual_users: 3,
        }
    }

    #[test]
    fn text_results_include_status_users_and_step_percentiles() {
        let mut results = results_with(0.1, 20, 10);
        results.successful_requests = 9;
        results.failed_requests = 1;
        results.avg_response_time_ms = 12.5;
        results.requests_per_second = 10.0;
        results.duration_seconds = 1.0;
        results.total_virtual_users = 3;
        results.scenarios.insert(
            "zeta".to_string(),
            sample_scenario("zeta", "Zeta", vec![sample_step("home", "Home")]),
        );
        results.scenarios.insert(
            "alpha".to_string(),
            sample_scenario("alpha", "Alpha", vec![sample_step("echo", "Echo")]),
        );

        let text = format_text_results(&results);
        assert!(
            text.starts_with("Load Test Results: completed\n"),
            "status line missing: {text}"
        );
        assert!(text.contains("Virtual users: 3"), "{text}");
        assert!(
            text.contains("Requests: 10 total, 9 ok, 1 failed (10.00% errors)"),
            "{text}"
        );
        assert!(text.contains("Latency: avg 12.50ms, p90 20ms"), "{text}");
        assert!(
            text.contains("  Step echo (Echo): 10 req, 1 failed, avg 12.50ms, p50 10ms p90 20ms p95 25ms p99 35ms"),
            "{text}"
        );
        let alpha = text.find("Scenario alpha").expect("alpha scenario");
        let zeta = text.find("Scenario zeta").expect("zeta scenario");
        assert!(alpha < zeta, "scenarios should be sorted by id: {text}");
    }

    #[test]
    fn text_results_include_truncated_reason() {
        let results = TestResults {
            status: RunStatus::Truncated {
                reason: "deadline reached".to_string(),
            },
            ..Default::default()
        };
        let text = format_text_results(&results);
        assert!(
            text.contains("Load Test Results: truncated (deadline reached)"),
            "{text}"
        );
    }
}
