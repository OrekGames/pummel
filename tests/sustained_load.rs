//! Independent end-to-end verification that the engine sustains load over time.
//!
//! The engine must behave as an actual sustained load generator, not a
//! single-pass burst.
//!
//! Each test drives the crate's *public* API through the
//! `with_http_client_factory` mock seam on a real tokio runtime and asserts on
//! observable behaviour (recorded request counts, wall-clock termination).
//! Durations are deliberately short (a few hundred ms) so the suite stays fast.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use pummel::engine::{Engine, ExecutionOptions};
use pummel::error::Result;
use pummel::http::{Body, HttpClient, HttpStatus, Request, Response};
use pummel::metrics::MetricsCollectorFactory;
use pummel::scenario::{ScenarioBuilder, StepBuilder};

/// Counts every `send`, sleeps `per_send`, then returns 200 OK so the default
/// validator passes and the VU keeps iterating.
struct CountingClient {
    sends: Arc<AtomicUsize>,
    per_send: Duration,
}

#[async_trait]
impl HttpClient for CountingClient {
    async fn send(&self, _request: &Request) -> Result<Response> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        if !self.per_send.is_zero() {
            tokio::time::sleep(self.per_send).await;
        }
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

fn base_options() -> ExecutionOptions {
    ExecutionOptions::builder()
        .virtual_users(1)
        .duration(Duration::from_secs(0))
        .ramp_up(Duration::from_secs(0))
        .think_time(Duration::from_secs(0))
        .build()
}

// ---------------------------------------------------------------------------
// duration > 0 sustains load — a single VU on a single
// step issues MANY more requests than virtual_users * steps (the old hard cap).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sustained_load_repeats_scenario_over_duration() {
    let sends = Arc::new(AtomicUsize::new(0));
    let sends_factory = sends.clone();

    let mut engine = Engine::new();
    engine.with_http_client_factory(move || {
        Ok(Arc::new(CountingClient {
            sends: sends_factory.clone(),
            per_send: Duration::from_millis(2),
        }) as Arc<dyn HttpClient>)
    });
    engine.with_metrics_collector_factory(MetricsCollectorFactory::create_in_memory);

    let step = StepBuilder::new(
        "s1",
        "S1",
        Request::get("https://example.com/1").build().unwrap(),
    )
    .max_retries(0)
    .build();

    // 2 VUs * 1 step. The OLD engine issued exactly 2 requests total regardless
    // of duration; the sustained loop issues many more over a 300ms window.
    let scenario = ScenarioBuilder::new("s", "S")
        .step(step)
        .virtual_users(2)
        .duration(Duration::from_millis(300))
        .ramp_up(Duration::from_secs(0))
        .think_time(Duration::from_secs(0))
        .build()
        .unwrap();
    engine.add_scenario(scenario);

    let start = Instant::now();
    let results = engine.run_all(base_options()).await.unwrap();
    let elapsed = start.elapsed();

    let total = sends.load(Ordering::SeqCst);
    assert!(
        total > 20,
        "sustained load must issue many more than virtual_users*steps (=2); got {total}"
    );
    assert_eq!(
        results.total_requests as usize, total,
        "every send must be recorded exactly once"
    );
    assert_eq!(results.successful_requests, results.total_requests);
    // requests_per_second reflects the real recorded run window (no code change
    // needed once the loop sustains load).
    assert!(
        results.requests_per_second > 0.0,
        "rps must be meaningful over the real run window"
    );
    // The run terminates near the deadline, not 3600s and not immediately.
    assert!(
        elapsed >= Duration::from_millis(250) && elapsed < Duration::from_secs(2),
        "run should span roughly the configured duration, took {elapsed:?}"
    );
}

// ---------------------------------------------------------------------------
// think_time is never slept past the deadline. A
// think_time larger than the whole run must NOT extend the run — the VU stops
// after its last in-window pass instead of sleeping past the deadline.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn think_time_not_slept_past_deadline() {
    let sends = Arc::new(AtomicUsize::new(0));
    let sends_factory = sends.clone();

    let mut engine = Engine::new();
    engine.with_http_client_factory(move || {
        Ok(Arc::new(CountingClient {
            sends: sends_factory.clone(),
            per_send: Duration::from_millis(1),
        }) as Arc<dyn HttpClient>)
    });
    engine.with_metrics_collector_factory(MetricsCollectorFactory::create_in_memory);

    let step = StepBuilder::new(
        "s1",
        "S1",
        Request::get("https://example.com/1").build().unwrap(),
    )
    .max_retries(0)
    .build();

    // think_time (5s) dwarfs duration (200ms): after the first pass, sleeping
    // think_time would land far past the deadline, so the loop must break
    // instead. The run returns quickly (well under think_time) with ~1 pass.
    let scenario = ScenarioBuilder::new("s", "S")
        .step(step)
        .virtual_users(1)
        .duration(Duration::from_millis(200))
        .ramp_up(Duration::from_secs(0))
        .think_time(Duration::from_secs(5))
        .build()
        .unwrap();
    engine.add_scenario(scenario);

    let start = Instant::now();
    let _ = engine.run_all(base_options()).await.unwrap();
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(1),
        "run must not sleep think_time past the deadline, took {elapsed:?}"
    );
    let total = sends.load(Ordering::SeqCst);
    assert!(
        (1..=2).contains(&total),
        "with think_time > duration only the first pass(es) run; got {total}"
    );
}

// ---------------------------------------------------------------------------
// target_rps paces iteration start times so offered load is decoupled from response latency and stays well below the
// unlimited closed-loop rate over the same window.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn open_loop_target_rps_paces_offered_load() {
    let sends = Arc::new(AtomicUsize::new(0));
    let sends_factory = sends.clone();

    let mut engine = Engine::new();
    engine.with_http_client_factory(move || {
        Ok(Arc::new(CountingClient {
            sends: sends_factory.clone(),
            per_send: Duration::from_millis(1),
        }) as Arc<dyn HttpClient>)
    });
    engine.with_metrics_collector_factory(MetricsCollectorFactory::create_in_memory);

    let step = StepBuilder::new(
        "s1",
        "S1",
        Request::get("https://example.com/1").build().unwrap(),
    )
    .max_retries(0)
    .build();

    // 1 VU, 400ms window, target 50 rps => interval 20ms => ~20 iterations.
    // Unlimited closed-loop over the same window would issue hundreds.
    let scenario = ScenarioBuilder::new("s", "S")
        .step(step)
        .virtual_users(1)
        .duration(Duration::from_millis(400))
        .ramp_up(Duration::from_secs(0))
        .think_time(Duration::from_secs(0))
        .build()
        .unwrap();
    engine.add_scenario(scenario);

    let mut options = base_options();
    options.target_rps = Some(50.0);

    let _ = engine.run_all(options).await.unwrap();

    let total = sends.load(Ordering::SeqCst);
    assert!(
        (5..=60).contains(&total),
        "target_rps=50 over 400ms should pace to ~20 sends (bounded well below \
         the unlimited closed-loop rate); got {total}"
    );
}

// ---------------------------------------------------------------------------
// Guard: duration == 0 still means EXACTLY ONE pass (single-pass mode), the
// invariant the single-pass exact-count tests depend on.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn duration_zero_runs_exactly_one_pass() {
    let sends = Arc::new(AtomicUsize::new(0));
    let sends_factory = sends.clone();

    let mut engine = Engine::new();
    engine.with_http_client_factory(move || {
        Ok(Arc::new(CountingClient {
            sends: sends_factory.clone(),
            per_send: Duration::from_millis(0),
        }) as Arc<dyn HttpClient>)
    });
    engine.with_metrics_collector_factory(MetricsCollectorFactory::create_in_memory);

    let step = StepBuilder::new(
        "s1",
        "S1",
        Request::get("https://example.com/1").build().unwrap(),
    )
    .max_retries(0)
    .build();

    let scenario = ScenarioBuilder::new("s", "S")
        .step(step)
        .virtual_users(3)
        .duration(Duration::from_secs(0)) // single-pass mode
        .ramp_up(Duration::from_secs(0))
        .think_time(Duration::from_secs(0))
        .build()
        .unwrap();
    engine.add_scenario(scenario);

    let results = engine.run_all(base_options()).await.unwrap();

    // 3 VUs * 1 step, exactly once each: no looping when duration == 0.
    assert_eq!(sends.load(Ordering::SeqCst), 3);
    assert_eq!(results.total_requests, 3);
}
