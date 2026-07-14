//! Independent end-to-end verification of core engine behavior.
//!
//! Each test proves one defect fix through the crate's *public* API using the
//! `with_http_client_factory` mock seam and a real tokio runtime. These tests do
//! not trust the in-crate unit tests; they exercise the engine/config/http layers
//! from the outside and assert on observable behaviour (request URLs, returned
//! errors, wall-clock termination, and mock send counts).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use pummel::config::Config;
use pummel::engine::{Engine, ExecutionOptions};
use pummel::error::{Error, Result};
use pummel::http::{Body, HttpClient, HttpStatus, Request, Response};
use pummel::metrics::MetricsCollectorFactory;
use pummel::scenario::{Scenario, ScenarioBuilder, StepBuilder};

// ---------------------------------------------------------------------------
// Mock HTTP clients
// ---------------------------------------------------------------------------

/// Always fails, so every step exhausts its retries and is marked Failed.
struct FailingClient;

#[async_trait]
impl HttpClient for FailingClient {
    async fn send(&self, _request: &Request) -> Result<Response> {
        Err(Error::other("mock failure"))
    }
    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

/// Counts every `send` call and sleeps briefly, so a chain of steps keeps a
/// single VU busy well beyond a short duration timeout. Returns a 200 OK so the
/// default validator passes and the VU advances to the next dependent step.
struct CountingSlowClient {
    sends: Arc<AtomicUsize>,
    per_send: Duration,
}

#[async_trait]
impl HttpClient for CountingSlowClient {
    async fn send(&self, _request: &Request) -> Result<Response> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.per_send).await;
        Ok(Response::new(
            HttpStatus::OK,
            Default::default(),
            Body::Text("ok".into()),
            Duration::from_millis(1),
        ))
    }
    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// base_url + relative step path resolves to the correct absolute URL.
// ---------------------------------------------------------------------------

#[test]
fn proof1_base_url_joins_relative_path_to_absolute_url() {
    let toml = r#"
        [global]
        base_url = "https://api.example.com"

        [scenarios.s]
        name = "S"
        steps = ["step1"]

        [steps.step1]
        name = "Step 1"
        method = "GET"
        url = "/api/resource"
    "#;

    let config = Config::from_toml_str(toml).unwrap();
    let scenarios = config.build_scenarios().unwrap();
    let url = scenarios[0].get_step("step1").unwrap().request.url();

    // The relative path is joined onto base_url. The bug produced host="api"
    // (from "http:///api/resource") or a localhost placeholder.
    assert_eq!(url.scheme(), "https", "scheme must come from base_url");
    assert_eq!(
        url.host_str(),
        Some("api.example.com"),
        "host must be base_url's host, not 'api' and not 'localhost'"
    );
    assert_eq!(url.path(), "/api/resource", "full path must be preserved");
    assert_eq!(url.as_str(), "https://api.example.com/api/resource");
}

// ---------------------------------------------------------------------------
// invalid/empty URLs return Err; they are never silently rewritten to localhost.
// ---------------------------------------------------------------------------

#[test]
fn proof2_invalid_url_errors_instead_of_targeting_localhost() {
    for bad in [
        "",
        "not a url",
        "/api/resource",
        "www.example.com",
        "localhost:8080",
    ] {
        let built = Request::get(bad).build();
        assert!(
            built.is_err(),
            "target {bad:?} must be rejected, got {:?}",
            built.map(|r| r.url().as_str().to_string())
        );
    }

    // And a valid absolute URL still builds and is NOT rewritten to localhost.
    let ok = Request::get("https://real.example.com/path")
        .build()
        .unwrap();
    assert_eq!(ok.url().host_str(), Some("real.example.com"));
}

// ---------------------------------------------------------------------------
// a failed non-leaf dependency terminates promptly; it does
// not livelock the VU poll loop until the duration timeout.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn proof3_failed_dependency_terminates_promptly() {
    let mut engine = Engine::new();
    engine.with_http_client_factory(|| Ok(Arc::new(FailingClient) as Arc<dyn HttpClient>));
    engine.with_metrics_collector_factory(MetricsCollectorFactory::create_in_memory);

    let first = StepBuilder::new(
        "first",
        "First",
        Request::get("https://example.com/first").build().unwrap(),
    )
    .max_retries(0)
    .build();
    let second = StepBuilder::new(
        "second",
        "Second",
        Request::get("https://example.com/second").build().unwrap(),
    )
    .max_retries(0)
    .dependency("first")
    .build();

    let scenario = ScenarioBuilder::new("s", "S")
        .step(first)
        .step(second)
        .virtual_users(1)
        .duration(Duration::from_secs(0)) // 0 => 3600s internal timeout if it livelocks
        .ramp_up(Duration::from_secs(0))
        .think_time(Duration::from_secs(0))
        .build()
        .unwrap();
    engine.add_scenario(scenario);

    let start = Instant::now();
    let results = tokio::time::timeout(Duration::from_secs(5), engine.run_all(default_options()))
        .await
        .expect("run_all livelocked on a failed dependency (3600s hang)")
        .expect("run_all returned an error");
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(2),
        "run should terminate promptly, took {elapsed:?}"
    );

    // The dependency ran and failed; the dependent never ran (not stuck Waiting
    // and re-driven). Exactly one request (the failed 'first') was recorded.
    assert_eq!(results.total_requests, 1, "only 'first' should execute");
    assert_eq!(results.successful_requests, 0);
    assert!(
        results.failed_requests >= 1,
        "'first' must be recorded failed"
    );

    // 'second' must have produced no successful request (it was skipped, not run).
    let scenario_metrics = results
        .scenarios
        .get("s")
        .expect("scenario metrics present");
    if let Some(second_metrics) = scenario_metrics.steps.get("second") {
        assert_eq!(
            second_metrics.successful_requests, 0,
            "blocked dependent must not have succeeded"
        );
    }
}

// ---------------------------------------------------------------------------
// when the duration timeout fires, spawned VU tasks are
// actually aborted -> no further sends occur after run_all returns.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn proof4_timeout_aborts_vu_tasks_no_more_sends_after_return() {
    let sends = Arc::new(AtomicUsize::new(0));
    let sends_for_factory = sends.clone();

    let mut engine = Engine::new();
    engine.with_http_client_factory(move || {
        Ok(Arc::new(CountingSlowClient {
            sends: sends_for_factory.clone(),
            per_send: Duration::from_millis(30),
        }) as Arc<dyn HttpClient>)
    });
    engine.with_metrics_collector_factory(MetricsCollectorFactory::create_in_memory);

    // A long chain: ~100 steps * 30ms = ~3s of work, but the duration timeout is
    // 1s. Without abort the VU keeps sending after run_all returns.
    let mut builder = ScenarioBuilder::new("s", "S");
    let mut first = StepBuilder::new(
        "step_1",
        "Step 1",
        Request::get("https://example.com/1").build().unwrap(),
    )
    .max_retries(0)
    .build();
    // ensure first has no deps
    let _ = &mut first;
    builder = builder.step(first);
    for i in 2..=100 {
        let step = StepBuilder::new(
            format!("step_{i}"),
            format!("Step {i}"),
            Request::get(format!("https://example.com/{i}"))
                .build()
                .unwrap(),
        )
        .max_retries(0)
        .dependency(format!("step_{}", i - 1))
        .build();
        builder = builder.step(step);
    }
    let scenario = builder
        .virtual_users(1)
        .duration(Duration::from_secs(1)) // 1s real timeout fires mid-run
        .ramp_up(Duration::from_secs(0))
        .think_time(Duration::from_secs(0))
        .build()
        .unwrap();
    engine.add_scenario(scenario);

    let _ = engine.run_all(default_options()).await.unwrap();

    let after_return = sends.load(Ordering::SeqCst);
    assert!(after_return > 0, "the VU should have sent some requests");
    assert!(
        after_return < 100,
        "the run must have been truncated by the timeout, got {after_return} sends"
    );

    // Give any un-aborted task time to keep sending, then confirm it did not.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after_wait = sends.load(Ordering::SeqCst);
    assert_eq!(
        after_return, after_wait,
        "VU tasks kept sending after run_all returned: {after_return} -> {after_wait} (not aborted)"
    );
}

// ---------------------------------------------------------------------------
// run_all rejects an invalid graph built by bypassing
// ScenarioBuilder, rather than hanging VUs until timeout.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn proof5_invalid_scenario_rejected_self_dependency() {
    let mut engine = Engine::new();
    engine.with_http_client_factory(|| Ok(Arc::new(FailingClient) as Arc<dyn HttpClient>));
    engine.with_metrics_collector_factory(MetricsCollectorFactory::create_in_memory);

    // Bypass ScenarioBuilder validation by inserting a self-dependent step
    // directly into the public `steps` map (a cycle: s1 -> s1).
    let mut scenario = Scenario::new("bad", "Bad");
    let step = StepBuilder::new(
        "s1",
        "S1",
        Request::get("https://example.com").build().unwrap(),
    )
    .dependency("s1")
    .build();
    scenario.steps.insert(step.id.clone(), step);
    engine.add_scenario(scenario);

    let result = tokio::time::timeout(Duration::from_secs(5), engine.run_all(default_options()))
        .await
        .expect("run_all hung on an invalid graph instead of rejecting it");
    assert!(
        result.is_err(),
        "cyclic/self-dependent scenario must be rejected"
    );
}

#[tokio::test]
async fn proof5_invalid_scenario_rejected_missing_dependency() {
    let mut engine = Engine::new();
    engine.with_http_client_factory(|| Ok(Arc::new(FailingClient) as Arc<dyn HttpClient>));
    engine.with_metrics_collector_factory(MetricsCollectorFactory::create_in_memory);

    let mut scenario = Scenario::new("bad2", "Bad2");
    let step = StepBuilder::new(
        "s1",
        "S1",
        Request::get("https://example.com").build().unwrap(),
    )
    .dependency("ghost")
    .build();
    scenario.steps.insert(step.id.clone(), step);
    engine.add_scenario(scenario);

    let result = engine.run_all(default_options()).await;
    assert!(
        result.is_err(),
        "scenario with a missing dependency must be rejected"
    );
}

// ---------------------------------------------------------------------------

fn default_options() -> ExecutionOptions {
    ExecutionOptions::builder()
        .virtual_users(1)
        .duration(Duration::from_secs(0))
        .ramp_up(Duration::from_secs(0))
        .think_time(Duration::from_secs(0))
        .build()
}
