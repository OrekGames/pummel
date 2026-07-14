# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-07-06

First substantive release, following a full code-review remediation that turned
`pummel` from a prototype into a real HTTP load generator that works both as a
standalone CLI and as an embeddable library.

> **Note on breaking changes.** `0.1.0` was never published to crates.io. The
> "Changed" and "Removed" entries below are breaking relative to earlier
> pre-release git checkouts; anyone who pinned a git revision should read them
> before upgrading. The public API now uses `#[non_exhaustive]` and builders so
> that future additions are non-breaking.

### Added

- **Sustained load generation.** Each virtual user now repeatedly executes the
  scenario for the configured `duration` (deadline = `start + ramp_up + duration`)
  instead of running the graph once. Adds optional open-loop arrival-rate pacing
  via `ExecutionOptions::target_rps` to mitigate coordinated omission.
- **Concurrent execution.** Independent ready steps within a virtual user run
  concurrently; a single HTTP client (and its connection pool) is shared across
  all virtual users of a scenario, with opt-in per-user isolation via
  `isolate_clients_per_user`.
- **In-flight request cap.** `ExecutionOptions::max_concurrent_requests` /
  `[http].max_concurrent_requests` bounds concurrent in-flight requests
  (`0` = unlimited), decoupled from the connection-pool size.
- **Enforced per-step timeouts.** `Step::timeout` is now honored per attempt.
- **Bounded streaming metrics.** A lock-free per-(scenario, step) aggregator with
  a latency histogram replaces unbounded per-request storage; memory is now
  independent of request count. Latency includes body download, exposes
  time-to-first-byte, records one sample per attempt, and keeps successful and
  failed latencies separate.
- **Configuration that drives behavior.** `[http]` settings (connect timeout,
  pools, HTTP/2, TLS verification) build the client; `[global]` headers,
  timeout, and load settings are inherited by scenarios/steps (scenario/step
  values override); `follow_redirects` is honored.
- **Config validation.** `#[serde(deny_unknown_fields)]` on all config structs
  (typos now error) plus semantic validation (e.g. `virtual_users = 0` is
  rejected).
- **Working telemetry.** The engine invokes the exporter lifecycle
  (`init` / per-attempt `export_request` / `export_results` / `shutdown`), gated
  on `[telemetry].enabled`. A real JSON exporter ships; unimplemented formats
  now error loudly.
- **CI-usable CLI.** Results JSON is written to stdout (pipeable); exit codes
  `0`/`1`/`2` with a `[thresholds]` surface (`max_error_rate`, `max_p90_ms`,
  `min_requests`); new `--dry-run`/`--validate`, `--users`/`--duration`/
  `--ramp-up`/`--target-rps`/`--max-error-rate` overrides, and `--graph` now
  visualizes without running the test.
- New public types: `ExecutionOptionsBuilder`, `ClientSpec`, `ThresholdsConfig`,
  `NoopMetricsCollector`, `telemetry::ExporterConfig`, `JsonTelemetryExporter`.
- MIT `LICENSE` file and complete package metadata (`license`, `repository`,
  `keywords`, `categories`, `rust-version = "1.96.1"`).
- Integration test suites covering the engine loop, sustained load, metrics
  fidelity, the config/API surface, the CLI, and a real localhost HTTP server;
  README examples are compile-tested as doctests. (30 tests → 117 tests + 4
  doctests.)

### Changed

- **`RequestBuilder::build()` now returns `Result<Request>`** and rejects
  invalid or relative-without-base URLs instead of silently substituting
  `http://localhost`.
- **`duration` now means sustained wall-clock load**, not a one-shot timeout.
- **Metrics collector:** the default is now the bounded in-memory streaming
  aggregator. `RequestMetrics::new` gained `elapsed`, `step_name`, and
  `scenario_name` parameters.
- **`ExecutionOptions`** is `#[non_exhaustive]`; construct it via
  `ExecutionOptions::builder()` / `default()`. Removed the unused
  `strict_dependencies` and `custom` fields; added `isolate_clients_per_user`
  and `target_rps`.
- **`ScenarioBuilder::step` returns `Self`** (was `Result<Self>`); errors now
  surface at `build()`. Out-of-order/forward step references are legal.
- **Config reshaping:** `ScenarioConfig` load fields and `StepConfig::timeout_ms`
  are now `Option<_>` (inherit from `[global]`); `Config` gained `thresholds`;
  `MetricsConfig` trimmed to `enabled`; `GlobalConfig`/`HttpConfig` lost `custom`;
  `HttpConfig` gained `max_concurrent_requests`.
- **`Error`** is `#[non_exhaustive]` with a new `Validation` variant; typed
  errors are preserved instead of being downgraded to `Error::Other`.
- `StepStatus`, `VirtualUserStatus`, and `telemetry::TelemetryFormat` are now
  `#[non_exhaustive]`.
- `telemetry::TelemetryConfig` was renamed to `telemetry::ExporterConfig`;
  `TelemetryExporterFactory::create` now takes `&ExporterConfig` and returns a
  `Result`.
- `Config::from_toml` / `from_yaml` accept `AsRef<Path>`.
- HTTP client construction moved to `HttpClientFactory::from_spec(ClientSpec)`
  (replacing `with_settings`).
- Default changes: `[telemetry].enabled` `true` → `false`; default exporter
  `otlp` → `json`; `max_concurrent_requests` default is `0` (unlimited).
- TLS backend switched to rustls (`reqwest` `default-features = false` +
  `rustls-tls`); `tokio` features narrowed off `full`.
- CLI logs now go to stderr so stdout carries only results.

### Fixed

- `global.base_url` is now applied to relative step URLs (previously ignored,
  sending load to a mangled host).
- A failed step with dependents no longer livelocks its virtual user; the
  duration timeout now aborts in-flight tasks, and scenarios are validated
  before running.
- Failed requests no longer record `0 ms` and no longer corrupt success-latency
  statistics; the percentile index off-by-one is fixed; retried attempts are now
  counted.
- `Engine::with_graph_visualizer` now uses the provided visualizer (was a silent
  no-op).
- Metrics report real step/scenario names instead of reusing IDs.
- `RequestBuilder` surfaces invalid header names/values and JSON serialization
  failures as errors instead of silently dropping them.
- Mermaid graph output escapes step IDs and names.
- `logging::init` no longer panics when a subscriber is already installed and now
  honors `RUST_LOG`.

### Removed

- `BatchedMetricsCollector` and `MetricsCollectorFactory::create_batched` (the
  self-declared-broken default; it leaked a background task and did more work
  than the collector it wrapped).
- `LoadTester` (use `Engine` directly).
- The unused `DependencyGraph` analytical methods (`get_topological_ordering`,
  `get_critical_path`, `get_dependencies`, `get_dependents`, `get_root_steps`,
  `get_leaf_steps`) and the unused `ScenarioExecutor` trait.
- Six unused dependencies (`rayon`, `fake`, `opentelemetry`, `opentelemetry-otlp`,
  `tower`, `dot`) and the native-tls/OpenSSL stack (lockfile: 303 → 259 packages).

### Security

- Replaced the unmaintained `serde_yaml` (RUSTSEC-2024-0320) with the maintained
  drop-in `serde_yaml_ng`.

[0.1.0]: https://github.com/OrekGames/pummel/releases/tag/v0.1.0
