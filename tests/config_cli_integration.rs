//! Independent verification that configuration and the public API surface are
//! trustworthy.
//!
//! These tests do not trust the implementers' own tests. Where a claim is
//! behavioral (redirects, HTTP/2 prior knowledge, config -> client wiring, CLI
//! exit codes, JSON output) they drive the real code end-to-end against a raw
//! in-process HTTP/1.1 server and/or the compiled `pummel` binary.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use async_trait::async_trait;

use pummel::config::{Config, GlobalConfig, HttpConfig};
use pummel::engine::Engine;
use pummel::error::{Error, Result as PummelResult};
use pummel::graph::{GraphFormat, GraphVisualizer};
use pummel::http::{
    Body, ClientSpec, DefaultHttpClient, HttpClient, HttpHeaders, HttpStatus, Request, Response,
};
use pummel::metrics::{RequestMetrics, TestResults};
use pummel::scenario::{Scenario, ScenarioBuilder, StepBuilder};
use pummel::telemetry::TelemetryExporter;

// ---------------------------------------------------------------------------
// Raw in-process HTTP/1.1 test server (no extra dependencies)
// ---------------------------------------------------------------------------

/// A reply the test server should write for a given request.
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn ok(body: &str) -> Self {
        Reply {
            status: 200,
            headers: vec![],
            body: body.to_string(),
        }
    }
    fn status(code: u16, body: &str) -> Self {
        Reply {
            status: code,
            headers: vec![],
            body: body.to_string(),
        }
    }
    fn redirect(location: &str) -> Self {
        Reply {
            status: 302,
            headers: vec![("Location".to_string(), location.to_string())],
            body: String::new(),
        }
    }
}

/// A minimal blocking HTTP/1.1 server running on a detached background thread.
/// It exists only for the lifetime of the test process.
struct TestServer {
    addr: SocketAddr,
    requests: Arc<AtomicUsize>,
}

impl TestServer {
    fn start<F>(handler: F) -> Self
    where
        F: Fn(&str, &str) -> Reply + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let handler = Arc::new(handler);

        let requests_bg = requests.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let handler = handler.clone();
                let requests = requests_bg.clone();
                thread::spawn(move || handle_conn(stream, &*handler, &requests));
            }
        });

        TestServer { addr, requests }
    }

    fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

fn handle_conn<F>(mut stream: TcpStream, handler: &F, requests: &AtomicUsize)
where
    F: Fn(&str, &str) -> Reply,
{
    // Read the request head (up to the blank line). Bodies are irrelevant to
    // these tests (all requests are GET), so we do not consume them.
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 65_536 {
                    break;
                }
            }
            Err(_) => return,
        }
    }

    let text = String::from_utf8_lossy(&buf);
    let first_line = text.lines().next().unwrap_or("");
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();

    requests.fetch_add(1, Ordering::SeqCst);

    let reply = handler(&method, &path);
    let reason = match reply.status {
        200 => "OK",
        302 => "Found",
        500 => "Internal Server Error",
        _ => "Status",
    };

    let mut response = format!("HTTP/1.1 {} {}\r\n", reply.status, reason);
    response.push_str(&format!("Content-Length: {}\r\n", reply.body.len()));
    response.push_str("Connection: close\r\n");
    for (k, v) in &reply.headers {
        response.push_str(&format!("{k}: {v}\r\n"));
    }
    response.push_str("\r\n");
    response.push_str(&reply.body);

    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

// ---------------------------------------------------------------------------
// HTTP config wiring — [http] config actually changes the constructed client
// ---------------------------------------------------------------------------

/// Seam: the config -> ClientSpec mapping carries verify_ssl / use_http2 /
/// connection_timeout / pool settings, and both spec variants build a client.
#[test]
fn proof1_client_spec_maps_http_config_fields() {
    let http = HttpConfig {
        connection_timeout_ms: 1234,
        pool_idle_timeout_seconds: 45,
        max_connections_per_host: 7,
        use_http2: true,
        verify_ssl: false,
        ..HttpConfig::default()
    };
    let global = GlobalConfig::default();

    let spec = ClientSpec::from((&http, &global));
    assert_eq!(spec.connect_timeout, Duration::from_millis(1234));
    assert_eq!(spec.pool_idle_timeout, Duration::from_secs(45));
    assert_eq!(spec.pool_max_idle_per_host, 7);
    assert!(spec.use_http2);
    assert!(!spec.verify_ssl);

    // The spec must actually build a working client (both follow/no-follow),
    // exercising danger_accept_invalid_certs + http2_prior_knowledge paths.
    assert!(DefaultHttpClient::from_spec(&spec).is_ok());
}

/// Behavioral: `use_http2` (prior knowledge) demonstrably changes what the
/// constructed client puts on the wire. Against a plain HTTP/1.1 server, a
/// prior-knowledge HTTP/2 client fails while an HTTP/1.1 client succeeds —
/// proving the spec field reaches reqwest's builder, not just the struct.
#[tokio::test]
async fn proof1_use_http2_changes_client_behavior() {
    let server = TestServer::start(|_m, _p| Reply::ok("h1-ok"));
    let request = Request::get(server.url("/")).build().unwrap();

    let h1_spec = ClientSpec {
        use_http2: false,
        ..ClientSpec::default()
    };
    let h1 = DefaultHttpClient::from_spec(&h1_spec).unwrap();
    let h1_res = h1.send(&request).await;
    assert!(
        h1_res.is_ok() && h1_res.as_ref().unwrap().status() == HttpStatus::OK,
        "HTTP/1.1 client should succeed against an h1 server, got {h1_res:?}"
    );

    let h2_spec = ClientSpec {
        use_http2: true,
        ..ClientSpec::default()
    };
    let h2 = DefaultHttpClient::from_spec(&h2_spec).unwrap();
    let h2_res = h2.send(&request).await;
    assert!(
        h2_res.is_err(),
        "prior-knowledge HTTP/2 client must fail against an h1 server (proves use_http2 is wired), got {h2_res:?}"
    );
}

/// End-to-end: `[http] use_http2` flows through the real `Engine::run` config
/// path into the client the load test actually uses. Same h1 server: with
/// use_http2=true every request fails (error_rate 1.0); with false they succeed
/// (error_rate 0.0). This proves the [http] section is not dead config.
#[tokio::test]
async fn proof1_engine_run_honors_http_config_end_to_end() {
    let server = TestServer::start(|_m, _p| Reply::ok("ok"));

    let toml = format!(
        r#"
[global]
base_url = "{base}"
virtual_users = 1
duration_seconds = 0

[http]
use_http2 = true

[scenarios.s]
name = "S"
steps = ["a"]

[steps.a]
name = "A"
method = "GET"
url = "/"
"#,
        base = server.base()
    );

    let config = Config::from_toml_str(&toml).unwrap();
    let engine = Engine::new();
    let results = engine.run(&config).await.unwrap();
    assert!(
        results.total_requests >= 1 && results.error_rate == 1.0,
        "use_http2=true against h1 server should fail every request: {results:?}"
    );

    // Flip use_http2 off -> the very same config path now succeeds.
    let config_ok =
        Config::from_toml_str(&toml.replace("use_http2 = true", "use_http2 = false")).unwrap();
    let results_ok = Engine::new().run(&config_ok).await.unwrap();
    assert!(
        results_ok.total_requests >= 1 && results_ok.error_rate == 0.0,
        "use_http2=false should succeed against h1 server: {results_ok:?}"
    );
}

// ---------------------------------------------------------------------------
// Redirect following — follow_redirects is honored per request
// ---------------------------------------------------------------------------

#[tokio::test]
async fn proof2_follow_redirects_is_honored() {
    let server = TestServer::start(|_m, path| {
        if path == "/dest" {
            Reply::ok("final")
        } else {
            Reply::redirect("/dest")
        }
    });

    let client = DefaultHttpClient::new().unwrap();

    // follow=true -> ends at /dest with 200.
    let followed = Request::get(server.url("/"))
        .follow_redirects(true)
        .build()
        .unwrap();
    let resp = client.send(&followed).await.unwrap();
    assert_eq!(
        resp.status(),
        HttpStatus::OK,
        "with follow_redirects=true the client should land on the 200 destination"
    );

    // follow=false -> measures the 302 itself.
    let not_followed = Request::get(server.url("/"))
        .follow_redirects(false)
        .build()
        .unwrap();
    let resp = client.send(&not_followed).await.unwrap();
    assert_eq!(
        resp.status(),
        HttpStatus::FOUND,
        "with follow_redirects=false the client should surface the 302"
    );
}

// ---------------------------------------------------------------------------
// Global config inheritance — [global] takes effect (inherit + override)
// ---------------------------------------------------------------------------

#[test]
fn proof3_global_inherited_and_overridden() {
    let toml = r#"
[global]
base_url = "https://example.com"
virtual_users = 5
duration_seconds = 30
timeout_ms = 4321

[global.headers]
"X-Global" = "g"

[scenarios.inherits]
name = "Inherits"
steps = ["s"]

[scenarios.overrides]
name = "Overrides"
steps = ["s"]
virtual_users = 9

[steps.s]
name = "S"
method = "GET"
url = "/a"
"#;

    let config = Config::from_toml_str(toml).unwrap();
    let scenarios = config.build_scenarios().unwrap();

    let inherits = scenarios.iter().find(|s| s.id == "inherits").unwrap();
    // Omitted scenario value inherits [global].
    assert_eq!(inherits.virtual_users, 5);
    assert_eq!(inherits.duration, Duration::from_secs(30));
    // Global default header is applied to the built request...
    let step = inherits.get_step("s").unwrap();
    assert_eq!(step.request.headers().get("X-Global").unwrap(), "g");
    // ...and global timeout_ms is inherited by the step.
    assert_eq!(step.request.timeout(), Duration::from_millis(4321));

    // A scenario value overrides [global].
    let overrides = scenarios.iter().find(|s| s.id == "overrides").unwrap();
    assert_eq!(overrides.virtual_users, 9);
    // Unspecified fields still inherit.
    assert_eq!(overrides.duration, Duration::from_secs(30));
}

// ---------------------------------------------------------------------------
// Config validation — deny_unknown_fields + semantic validation
// ---------------------------------------------------------------------------

#[test]
fn proof4_unknown_field_is_a_hard_error() {
    let toml = r#"
[scenarios.s]
name = "S"
steps = []
virtual_userz = 50
"#;
    let parsed = Config::from_toml_str(toml);
    assert!(
        parsed.is_err(),
        "a typo'd config key must be rejected, not silently ignored"
    );
}

#[test]
fn proof4_zero_virtual_users_is_rejected() {
    let toml = r#"
[scenarios.s]
name = "S"
steps = ["s"]
virtual_users = 0

[steps.s]
name = "S"
method = "GET"
url = "https://example.com/a"
"#;
    let config = Config::from_toml_str(toml).unwrap();
    assert!(
        config.build_scenarios().is_err(),
        "virtual_users = 0 must be rejected rather than run a zero-request no-op"
    );
}

// ---------------------------------------------------------------------------
// Telemetry callbacks — exporter receives callbacks during a run
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Counts {
    init: AtomicUsize,
    export_request: AtomicUsize,
    export_results: AtomicUsize,
    shutdown: AtomicUsize,
}

struct CountingExporter {
    counts: Arc<Counts>,
}

#[async_trait]
impl TelemetryExporter for CountingExporter {
    async fn init(&self) -> PummelResult<()> {
        self.counts.init.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn export_request(&self, _m: &RequestMetrics) -> PummelResult<()> {
        self.counts.export_request.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn export_results(&self, _r: &TestResults) -> PummelResult<()> {
        self.counts.export_results.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn shutdown(&self) -> PummelResult<()> {
        self.counts.shutdown.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// HTTP client that always returns 200 with no body, in-process (no network).
struct OkClient;

#[async_trait]
impl HttpClient for OkClient {
    async fn send(&self, _request: &Request) -> PummelResult<Response> {
        Ok(Response::new(
            HttpStatus::OK,
            HttpHeaders::new(),
            Body::Empty,
            Duration::from_millis(1),
        ))
    }
    async fn close(&self) -> PummelResult<()> {
        Ok(())
    }
}

#[tokio::test]
async fn proof5_telemetry_exporter_receives_callbacks() {
    let counts = Arc::new(Counts::default());
    let exporter = Arc::new(CountingExporter {
        counts: counts.clone(),
    });

    let mut engine = Engine::new();
    engine.with_http_client_factory(|| Ok(Arc::new(OkClient) as Arc<dyn HttpClient>));
    engine.with_telemetry_exporter(exporter);

    let request = Request::get("https://example.com/a").build().unwrap();
    let step = StepBuilder::new("a", "A", request).build();
    let scenario = ScenarioBuilder::new("s", "S")
        .step(step)
        .virtual_users(3)
        .duration(Duration::from_secs(0))
        .build()
        .unwrap();
    engine.add_scenario(scenario);

    let _results = engine
        .run_all(pummel::engine::ExecutionOptions::default())
        .await
        .unwrap();

    assert_eq!(
        counts.init.load(Ordering::SeqCst),
        1,
        "init should fire once"
    );
    assert!(
        counts.export_request.load(Ordering::SeqCst) >= 3,
        "export_request must fire per attempt (>=3 for 3 VUs), got {}",
        counts.export_request.load(Ordering::SeqCst)
    );
    assert_eq!(
        counts.export_results.load(Ordering::SeqCst),
        1,
        "export_results should fire once"
    );
    assert_eq!(
        counts.shutdown.load(Ordering::SeqCst),
        1,
        "shutdown should fire once"
    );
}

// ---------------------------------------------------------------------------
// Logging initialization — logging::init is idempotent (no panic on second call)
// ---------------------------------------------------------------------------

#[test]
fn proof6_logging_init_twice_does_not_panic() {
    // The documented pattern (init then init) must not crash.
    pummel::logging::init(false);
    pummel::logging::init(true);
    // try_init surfaces the "already installed" signal without panicking.
    let _ = pummel::logging::try_init(false);
}

// ---------------------------------------------------------------------------
// CLI behavior — JSON output, threshold exit code, dry-run does not run load
// ---------------------------------------------------------------------------

fn cli_bin() -> &'static str {
    env!("CARGO_BIN_EXE_pummel")
}

fn write_config(contents: &str) -> tempfile::NamedTempFile {
    let mut f = tempfile::Builder::new().suffix(".toml").tempfile().unwrap();
    f.write_all(contents.as_bytes()).unwrap();
    f.flush().unwrap();
    f
}

fn single_pass_config(base: &str) -> String {
    format!(
        r#"
[global]
base_url = "{base}"
virtual_users = 1
duration_seconds = 0

[scenarios.s]
name = "S"
steps = ["a"]

[steps.a]
name = "A"
method = "GET"
url = "/"
"#
    )
}

#[test]
fn proof7_json_output_is_clean_parseable_json() {
    let server = TestServer::start(|_m, _p| Reply::ok("ok"));
    let cfg = write_config(&single_pass_config(&server.base()));

    let out = Command::new(cli_bin())
        .arg("--config")
        .arg(cfg.path())
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "clean run should exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8(out.stdout).unwrap();
    // No ANSI escapes and no log prefixes leaked onto stdout.
    assert!(
        !stdout.contains('\u{1b}'),
        "stdout must not contain ANSI escape codes: {stdout:?}"
    );
    let trimmed = stdout.trim_start();
    assert!(
        trimmed.starts_with('{'),
        "stdout must start with raw JSON, got: {stdout:?}"
    );

    // The whole of stdout must parse as a single JSON object.
    let value: serde_json::Value = serde_json::from_str(trimmed)
        .unwrap_or_else(|e| panic!("stdout is not valid JSON ({e}): {stdout:?}"));
    assert!(value.get("total_requests").is_some());
    assert!(
        value["total_requests"].as_u64().unwrap() >= 1,
        "single pass should record at least one request: {value}"
    );
}

#[test]
fn proof7_threshold_breach_exits_nonzero() {
    // Server fails every request -> error_rate 1.0. --max-error-rate 0.0 => breach.
    let server = TestServer::start(|_m, _p| Reply::status(500, "boom"));
    let cfg = write_config(&single_pass_config(&server.base()));

    let out = Command::new(cli_bin())
        .arg("--config")
        .arg(cfg.path())
        .arg("--max-error-rate")
        .arg("0.0")
        .output()
        .unwrap();

    assert_eq!(
        out.status.code(),
        Some(2),
        "a run breaching the failure threshold must exit with the threshold code; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn proof7_dry_run_does_not_run_load() {
    let server = TestServer::start(|_m, _p| Reply::ok("ok"));
    let cfg = write_config(&single_pass_config(&server.base()));

    let out = Command::new(cli_bin())
        .arg("--config")
        .arg(cfg.path())
        .arg("--dry-run")
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "dry-run of a valid config should exit 0"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Configuration is valid"),
        "dry-run should print a validation summary: {stdout}"
    );
    // Give any (erroneously) spawned load a beat to hit the server, then assert
    // nothing did.
    thread::sleep(Duration::from_millis(200));
    assert_eq!(
        server.count(),
        0,
        "--dry-run must not send any load to the target"
    );
}

// ---------------------------------------------------------------------------
// Graph visualizer hook — with_graph_visualizer uses the custom visualizer
// ---------------------------------------------------------------------------

struct SentinelVisualizer;

impl GraphVisualizer for SentinelVisualizer {
    fn visualize(&self, _scenario: &Scenario, _format: GraphFormat) -> PummelResult<String> {
        Ok("SENTINEL-VIZ".to_string())
    }
    fn clone_box(&self) -> Box<dyn GraphVisualizer> {
        Box::new(SentinelVisualizer)
    }
}

#[test]
fn proof8_custom_graph_visualizer_is_used() {
    let mut engine = Engine::new();
    let request = Request::get("https://example.com").build().unwrap();
    let step = StepBuilder::new("s", "S", request).build();
    let scenario = ScenarioBuilder::new("sc", "SC").step(step).build().unwrap();
    engine.add_scenario(scenario);
    engine.with_graph_visualizer(Box::new(SentinelVisualizer));

    // The custom visualizer output is returned instead of a default.
    assert_eq!(
        engine.visualize_graph(GraphFormat::Dot).unwrap(),
        "SENTINEL-VIZ"
    );
    // And it survives an Engine clone (used internally by run()).
    assert_eq!(
        engine
            .clone()
            .visualize_graph(GraphFormat::Mermaid)
            .unwrap(),
        "SENTINEL-VIZ"
    );
}

// ---------------------------------------------------------------------------
// Builder ergonomics — step returns Self; errors surface at build
// ---------------------------------------------------------------------------

#[test]
fn proof9_scenario_builder_step_returns_self_forward_ref_ok() {
    // `.step()` returns Self (no unwrap): a forward reference (b depends on a,
    // inserted before a) builds fine because validation is deferred to build().
    let a = StepBuilder::new(
        "a",
        "A",
        Request::get("https://example.com/a").build().unwrap(),
    )
    .build();
    let b = StepBuilder::new(
        "b",
        "B",
        Request::get("https://example.com/b").build().unwrap(),
    )
    .dependency("a")
    .build();

    let scenario = ScenarioBuilder::new("sc", "SC")
        .step(b) // forward reference to "a", inserted first
        .step(a)
        .build();
    assert!(
        scenario.is_ok(),
        "out-of-order .step() then build() must succeed: {:?}",
        scenario.err()
    );
}

#[test]
fn proof9_scenario_builder_surfaces_missing_dependency_at_build() {
    let s = StepBuilder::new(
        "s",
        "S",
        Request::get("https://example.com/s").build().unwrap(),
    )
    .dependency("ghost")
    .build();

    let result = ScenarioBuilder::new("sc", "SC").step(s).build();
    assert!(
        matches!(result, Err(Error::Scenario(_))),
        "a dependency on a non-existent step must surface at build(): {result:?}"
    );
}

#[test]
fn proof9_request_builder_surfaces_invalid_header() {
    let result = Request::get("https://example.com")
        .header("Authorization", "Bearer bad\nvalue")
        .build();
    assert!(
        result.is_err(),
        "an invalid header value must fail the build, not be silently dropped"
    );
}

#[test]
fn proof9_request_builder_surfaces_json_serialization_failure() {
    // A Serialize impl that always errors must surface at build(), not leave a
    // silently-empty body.
    struct BadJson;
    impl serde::Serialize for BadJson {
        fn serialize<S: serde::Serializer>(&self, _s: S) -> std::result::Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("boom"))
        }
    }

    let result = Request::post("https://example.com").json(&BadJson).build();
    assert!(
        result.is_err(),
        "a JSON serialization failure must surface at build()"
    );
}
