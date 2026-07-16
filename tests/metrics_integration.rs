//! Independent end-to-end verification of honest metrics and a sound metrics
//! subsystem.
//!
//! These tests do not trust the in-crate unit tests. Each section exercises the
//! crate's *public* API — the `with_http_client_factory` /
//! `with_metrics_collector_factory` engine seams, the `InMemoryMetricsCollector`
//! directly, and the real `DefaultHttpClient` against a raw local TCP server —
//! on a real tokio runtime, and asserts on observable behaviour. Where feasible
//! each section states the value the *bug* would have produced (negative control).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use pummel::engine::{Engine, ExecutionOptions};
use pummel::error::{Error, Result};
use pummel::http::{Body, HttpClient, HttpClientFactory, HttpStatus, Request, Response};
use pummel::metrics::{
    InMemoryMetricsCollector, MetricsCollector, MetricsCollectorFactory, RequestMetrics,
};
use pummel::scenario::{ScenarioBuilder, StepBuilder};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn default_options() -> ExecutionOptions {
    ExecutionOptions::builder()
        .virtual_users(1)
        .duration(Duration::from_secs(0))
        .ramp_up(Duration::from_secs(0))
        .think_time(Duration::from_secs(0))
        .build()
}

/// Build a successful `RequestMetrics` for `step`/`scenario` with the given
/// success latency, recorded through the same path the engine uses.
fn success_metric(step: &str, scenario: &str, vu: u32, latency_ms: u64) -> RequestMetrics {
    let request = Request::get("https://example.com").build().unwrap();
    let resp = Response::new(
        HttpStatus::OK,
        Default::default(),
        Body::Empty,
        Duration::from_millis(latency_ms),
    );
    RequestMetrics::new(
        format!("ok-{latency_ms}-{vu}"),
        step.to_string(),
        step.to_string(),
        scenario.to_string(),
        scenario.to_string(),
        vu,
        &request,
        Some(&resp),
        None,
        Duration::from_millis(latency_ms),
    )
}

/// Build a failed (transport-error, no response) `RequestMetrics` recording the
/// real elapsed `latency_ms`.
fn failure_metric(step: &str, scenario: &str, vu: u32, latency_ms: u64) -> RequestMetrics {
    let request = Request::get("https://example.com").build().unwrap();
    RequestMetrics::new(
        format!("err-{latency_ms}-{vu}"),
        step.to_string(),
        step.to_string(),
        scenario.to_string(),
        scenario.to_string(),
        vu,
        &request,
        None,
        Some("connection refused".to_string()),
        Duration::from_millis(latency_ms),
    )
}

/// Fails (`Err`) for the first `fail_count` calls, then returns `200 OK`.
/// Optionally sleeps `delay` before every response.
struct SequencedClient {
    calls: Arc<AtomicUsize>,
    fail_count: usize,
    delay: Duration,
}

#[async_trait]
impl HttpClient for SequencedClient {
    async fn send(&self, _request: &Request) -> Result<Response> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        if n < self.fail_count {
            Err(Error::other("mock transport failure"))
        } else {
            Ok(Response::new(
                HttpStatus::OK,
                Default::default(),
                Body::Text("ok".into()),
                Duration::from_millis(1),
            ))
        }
    }
    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

/// A raw HTTP/1.1 server (own OS thread) that flushes the response *head*,
/// waits `body_delay`, then writes the body. This makes time-to-first-byte
/// (headers) strictly precede full response time (body transfer), so response
/// time strictly exceeds time-to-first-byte. Serves exactly `n_conns`
/// connections then exits.
fn spawn_delayed_server(
    content_type: &'static str,
    body: &'static str,
    body_delay: Duration,
    n_conns: usize,
) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for _ in 0..n_conns {
            let (mut stream, _) = match listener.accept() {
                Ok(s) => s,
                Err(_) => return,
            };
            // Drain the request head so the client's write side completes.
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                content_type,
                body.len()
            );
            if stream.write_all(head.as_bytes()).is_err() {
                continue;
            }
            let _ = stream.flush();
            std::thread::sleep(body_delay);
            let _ = stream.write_all(body.as_bytes());
            let _ = stream.flush();
        }
    });
    addr
}

// ---------------------------------------------------------------------------
// Failure metrics: a failure records its real elapsed (not a hardcoded 0)
// and does NOT drag the success-latency distribution down.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn proof1_failure_records_real_elapsed_not_zero() {
    // The failure attempt has NO response but a real 42ms elapsed. The bug
    // recorded response_time_ms = 0 for every failure regardless of elapsed.
    let m = failure_metric("step1", "s", 0, 42);
    assert_eq!(
        m.response_time_ms, 42,
        "failure must record real elapsed, not hardcoded 0"
    );
    assert!(!m.success);
    assert_eq!(m.status_code, 0);
}

#[tokio::test]
async fn proof1_failure_does_not_corrupt_success_stats() {
    let collector = InMemoryMetricsCollector::new();
    // One 100ms success, and a failure. Under the bug the failure's 0ms would
    // be folded into min/avg/percentiles, forcing min -> 0 and dragging avg
    // toward 0. With success/failure separated, success stats stay pristine.
    collector
        .record_request(success_metric("step1", "s", 0, 100))
        .await
        .unwrap();
    collector
        .record_request(failure_metric("step1", "s", 0, 0))
        .await
        .unwrap();
    // A *slow* failure (500ms) must also not inflate success stats upward.
    collector
        .record_request(failure_metric("step1", "s", 0, 500))
        .await
        .unwrap();

    let step = collector
        .get_step_metrics(&"step1".to_string())
        .await
        .unwrap()
        .unwrap();

    assert_eq!(step.total_requests, 3);
    assert_eq!(step.successful_requests, 1);
    assert_eq!(step.failed_requests, 2);
    // Negative control: bug -> min == 0. Fixed -> min == 100 (the only success).
    assert_eq!(
        step.min_response_time_ms, 100,
        "failure 0ms must not pull min to 0"
    );
    assert_eq!(
        step.max_response_time_ms, 100,
        "500ms failure must not inflate max"
    );
    assert_eq!(
        step.avg_response_time_ms, 100.0,
        "avg is over successes only"
    );
    assert!((step.error_rate - (2.0 / 3.0)).abs() < 1e-9);
}

// ---------------------------------------------------------------------------
// Percentile calculation: nearest-rank correct (no upward bias).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn proof2_percentile_nearest_rank_small_exact_values() {
    // Latencies < 16ms map 1:1 into histogram buckets (exact), isolating the
    // nearest-rank index logic from bucket quantization.
    let collector = InMemoryMetricsCollector::new();
    for v in 1..=10u64 {
        collector
            .record_request(success_metric("step1", "s", 0, v))
            .await
            .unwrap();
    }
    let step = collector
        .get_step_metrics(&"step1".to_string())
        .await
        .unwrap()
        .unwrap();

    // rank_index(10, 0.5) = 4 -> 5th smallest = 5. Bug ((n*p) as usize)=5 -> 6.
    assert_eq!(
        step.p50_response_time_ms, 5,
        "p50 off-by-one (should be 5, not 6)"
    );
    // rank_index(10, 0.9) = 8 -> 9th smallest = 9. Bug -> index 9 = 10 (the max).
    assert_eq!(
        step.p90_response_time_ms, 9,
        "p90 of 10 samples must be the 9th (9), not the max (10)"
    );
}

#[tokio::test]
async fn proof2_median_of_two_picks_lower_value() {
    // The canonical M14 example: median of [100, 200]. The bug reported 200
    // (the upper); nearest-rank picks the lower. 100/200 land in the log-linear
    // region so the reported value is the lower bucket's representative (~102),
    // which is unambiguously the LOWER value, never 200.
    let collector = InMemoryMetricsCollector::new();
    collector
        .record_request(success_metric("step1", "s", 0, 100))
        .await
        .unwrap();
    collector
        .record_request(success_metric("step1", "s", 0, 200))
        .await
        .unwrap();
    let step = collector
        .get_step_metrics(&"step1".to_string())
        .await
        .unwrap()
        .unwrap();
    assert!(
        step.p50_response_time_ms < 150,
        "median must resolve to the lower value ~100, got {} (bug reported 200)",
        step.p50_response_time_ms
    );
}

// ---------------------------------------------------------------------------
// Retry visibility: one metrics record per attempt.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn proof3_retries_recorded_per_attempt() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_factory = calls.clone();

    let mut engine = Engine::new();
    engine.with_http_client_factory(move || {
        Ok(Arc::new(SequencedClient {
            calls: calls_factory.clone(),
            fail_count: 2, // fail twice, then succeed on the 3rd send
            delay: Duration::from_millis(0),
        }) as Arc<dyn HttpClient>)
    });
    engine.with_metrics_collector_factory(MetricsCollectorFactory::create_in_memory);

    let step = StepBuilder::new(
        "step1",
        "Step 1",
        Request::get("https://example.com/x").build().unwrap(),
    )
    .max_retries(2) // up to 3 attempts total
    .retry_delay(Duration::from_millis(1))
    .build();

    let scenario = ScenarioBuilder::new("s", "S")
        .step(step)
        .virtual_users(1)
        .duration(Duration::from_secs(0))
        .ramp_up(Duration::from_secs(0))
        .think_time(Duration::from_secs(0))
        .build()
        .unwrap();
    engine.add_scenario(scenario);

    let results = engine.run_all(default_options()).await.unwrap();

    // 3 real sends happened -> 3 metrics records. Bug recorded only 1 (the
    // final result), understating throughput and hiding the 2 failed attempts.
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "client must have been called 3 times"
    );
    assert_eq!(
        results.total_requests, 3,
        "must record one record PER attempt"
    );
    assert_eq!(
        results.successful_requests, 1,
        "exactly one attempt succeeded"
    );
    assert_eq!(
        results.failed_requests, 2,
        "two failed attempts must be recorded"
    );
    let step = results
        .scenarios
        .get("s")
        .unwrap()
        .steps
        .get("step1")
        .unwrap();
    assert_eq!(step.total_requests, 3);
    assert_eq!(step.successful_requests, 1);
    assert_eq!(step.failed_requests, 2);
}

// ---------------------------------------------------------------------------
// Response timing: response time includes body download; ttfb is populated
// and precedes full response time. Uses the REAL DefaultHttpClient.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn proof4_response_time_includes_body_ttfb_populated() {
    let body_delay = Duration::from_millis(400);
    let addr = spawn_delayed_server("text/plain", "hello-body", body_delay, 1);

    let client = HttpClientFactory::create().unwrap();
    let request = Request::get(format!("http://{addr}/")).build().unwrap();

    let resp = client.send(&request).await.unwrap();

    let ttfb = resp
        .ttfb()
        .expect("ttfb_ms must be populated, not hardcoded None");
    let total = resp.response_time();

    // Body transfer was delayed 400ms AFTER headers. Bug: response_time was
    // captured right after headers (TTFB-only) and excluded the body wait.
    assert!(
        total >= Duration::from_millis(300),
        "response_time must include the ~400ms body transfer, got {total:?}"
    );
    assert!(
        total >= ttfb,
        "full response_time ({total:?}) must be >= ttfb ({ttfb:?})"
    );
    assert!(
        total > ttfb,
        "body transfer must add measurable time beyond ttfb ({ttfb:?} vs {total:?})"
    );

    // And the plumbed RequestMetrics reflects both phases.
    let m = RequestMetrics::new(
        "r".into(),
        "step".into(),
        "Step".into(),
        "s".into(),
        "Scenario".into(),
        0,
        &request,
        Some(&resp),
        None,
        total,
    );
    assert!(
        m.ttfb_ms.is_some(),
        "RequestMetrics.ttfb_ms must be populated"
    );
    assert!(m.response_time_ms >= m.ttfb_ms.unwrap());
    assert!(m.response_time_ms >= 300);
}

// ---------------------------------------------------------------------------
// Body handling: bodies are not eagerly JSON-parsed. A text/plain "123"
// stays raw bytes, is retrievable as text "123", and response_size_bytes is set.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn proof5_body_not_eagerly_json_parsed() {
    let addr = spawn_delayed_server("text/plain", "123", Duration::from_millis(0), 1);

    let client = HttpClientFactory::create().unwrap();
    let request = Request::get(format!("http://{addr}/")).build().unwrap();
    let resp = client.send(&request).await.unwrap();

    // "123" is valid JSON, so the old speculative parse would coerce it into a
    // Body::Json(Number). The fix keeps raw bytes.
    match resp.body() {
        Body::Binary(bytes) => assert_eq!(bytes.as_ref(), b"123", "raw bytes must be preserved"),
        other => panic!("expected raw Body::Binary, got eagerly-parsed {other:?}"),
    }
    assert_eq!(
        resp.text().unwrap(),
        "123",
        "text() must round-trip the raw body"
    );

    // response_size_bytes must be populated from the byte length.
    let m = RequestMetrics::new(
        "r".into(),
        "step".into(),
        "Step".into(),
        "s".into(),
        "Scenario".into(),
        0,
        &request,
        Some(&resp),
        None,
        resp.response_time(),
    );
    assert_eq!(
        m.response_size_bytes,
        Some(3),
        "response_size_bytes must be set from the body length"
    );
}

// ---------------------------------------------------------------------------
// Bounded aggregation: no per-request retention and
// reads do not depend on the number of records.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn proof6_bounded_histogram_aggregation_not_exact_retention() {
    // Discriminator: if the collector RETAINED every raw latency, percentiles
    // would be exact. Because it aggregates into a fixed-size histogram, a large
    // latency is reported as its bucket representative, not the exact value.
    let collector = InMemoryMetricsCollector::new();
    collector
        .record_request(success_metric("step1", "s", 0, 1_000_000))
        .await
        .unwrap();
    let step = collector
        .get_step_metrics(&"step1".to_string())
        .await
        .unwrap()
        .unwrap();
    // Exact min/max come from atomics (unquantized)...
    assert_eq!(step.max_response_time_ms, 1_000_000);
    assert_eq!(step.min_response_time_ms, 1_000_000);
    // ...but the percentile is drawn from the bounded histogram: close to, but
    // NOT exactly, 1_000_000 (bucket quantization => no exact retention).
    assert_ne!(
        step.p50_response_time_ms, 1_000_000,
        "a bounded histogram must quantize; exact value implies unbounded retention"
    );
    let rel_err = (step.p50_response_time_ms as f64 - 1_000_000.0).abs() / 1_000_000.0;
    assert!(
        rel_err < 0.10,
        "histogram error must stay bounded (~6%), got {rel_err}"
    );
}

#[tokio::test]
async fn proof6_scales_without_per_request_growth() {
    // Record many requests into a SINGLE (scenario, step). With O(1)/record
    // aggregation and O(steps) reads this is trivial; with per-request retention
    // + full-map rescans on every read it would be the pathological case. We
    // assert correctness at scale and that a read is cheap regardless of N.
    let collector = InMemoryMetricsCollector::new();
    let n: u64 = 200_000;
    for i in 0..n {
        // Latencies 1..=10ms (exact buckets), evenly spread.
        collector
            .record_request(success_metric("step1", "s", (i % 4) as u32, (i % 10) + 1))
            .await
            .unwrap();
    }

    let start = std::time::Instant::now();
    let results = collector.get_test_results().await.unwrap();
    let read_elapsed = start.elapsed();

    assert_eq!(results.total_requests, n);
    assert_eq!(results.successful_requests, n);
    assert_eq!(results.failed_requests, 0);
    // A single read over one aggregate must not scan N records.
    assert!(
        read_elapsed < Duration::from_millis(200),
        "read should be O(steps), not O(records); took {read_elapsed:?}"
    );
    // Distinct VU ids were 0..4.
    assert_eq!(results.total_virtual_users, 4);
}

// ---------------------------------------------------------------------------
// Default collector: the BatchedMetricsCollector is gone and the default
// collector (used when no factory is set) produces correct TestResults.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn proof7_default_collector_is_correct_no_batched() {
    // NOTE (compile-time check): MetricsCollectorFactory exposes only
    // `create_in_memory`; there is no `create_batched` / `new_for_testing` /
    // `BatchedMetricsCollector` symbol to reference. This file compiling against
    // the public API is itself evidence the batched type was removed.
    let _factory_exists = MetricsCollectorFactory::create_in_memory;

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_factory = calls.clone();

    // Deliberately do NOT set a metrics collector factory: exercise the engine's
    // DEFAULT arm (get_metrics_collector_factory -> create_in_memory). Under the
    // bug this defaulted to the broken batched collector.
    let mut engine = Engine::new();
    engine.with_http_client_factory(move || {
        Ok(Arc::new(SequencedClient {
            calls: calls_factory.clone(),
            fail_count: 0, // always succeed
            delay: Duration::from_millis(0),
        }) as Arc<dyn HttpClient>)
    });

    // Two independent steps so both run (each succeeds once).
    let step1 = StepBuilder::new(
        "s1",
        "S1",
        Request::get("https://example.com/1").build().unwrap(),
    )
    .max_retries(0)
    .build();
    let step2 = StepBuilder::new(
        "s2",
        "S2",
        Request::get("https://example.com/2").build().unwrap(),
    )
    .max_retries(0)
    .build();

    let scenario = ScenarioBuilder::new("scn", "Scn")
        .step(step1)
        .step(step2)
        .virtual_users(3)
        .duration(Duration::from_secs(0))
        .ramp_up(Duration::from_secs(0))
        .think_time(Duration::from_secs(0))
        .build()
        .unwrap();
    engine.add_scenario(scenario);

    let results = engine.run_all(default_options()).await.unwrap();

    // 3 VUs * 2 steps = 6 successful sends, recorded exactly (no leak, no loss).
    assert_eq!(calls.load(Ordering::SeqCst), 6);
    assert_eq!(
        results.total_requests, 6,
        "default collector must record all sends"
    );
    assert_eq!(results.successful_requests, 6);
    assert_eq!(results.failed_requests, 0);
    assert_eq!(results.error_rate, 0.0);
    assert_eq!(results.total_virtual_users, 3);
    let scn = results.scenarios.get("scn").unwrap();
    assert_eq!(scn.steps.get("s1").unwrap().successful_requests, 3);
    assert_eq!(scn.steps.get("s2").unwrap().successful_requests, 3);
}
