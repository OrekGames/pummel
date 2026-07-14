//! Independent verification of load-model execution semantics.
//!
//! These tests do not trust the implementers: each drives the crate's *public*
//! API through the `with_http_client_factory` mock seam on a real tokio runtime
//! and asserts on directly observable behaviour (send counts, measured peak
//! request concurrency, factory-invocation counts, recorded failures, and
//! wall-clock termination). Every section pairs its positive assertion with a
//! sanity / negative check so a trivially-passing implementation is caught.
//! Durations are deliberately short (tens–hundreds of ms) so the suite is fast.
//!
//! Coverage:
//!   1. sustained load (iteration loop over `duration`)
//!   2. VU count = concurrency, NOT capped by a pool knob
//!   3. one shared client, not one-per-VU
//!   4. independent ready steps run concurrently within a VU
//!   5. step.timeout is enforced per attempt
//!   6. think-time is never slept past the deadline

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use pummel::engine::{Engine, ExecutionOptions};
use pummel::error::Result;
use pummel::http::{Body, HttpClient, HttpStatus, Request, Response};
use pummel::metrics::MetricsCollectorFactory;
use pummel::scenario::{ScenarioBuilder, StepBuilder};

/// A mock client that records total sends, tracks live in-flight concurrency
/// and the peak concurrency ever observed, optionally sleeps `delay` per send
/// (so overlap and timeouts are observable), and returns 200 OK so the default
/// validator passes and VUs keep iterating.
struct TrackingClient {
    sends: Arc<AtomicUsize>,
    in_flight: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    delay: Duration,
}

#[async_trait]
impl HttpClient for TrackingClient {
    async fn send(&self, _request: &Request) -> Result<Response> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
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

/// Shared bag of counters handed to a `TrackingClient` factory.
#[derive(Clone, Default)]
struct Counters {
    sends: Arc<AtomicUsize>,
    in_flight: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    factory_calls: Arc<AtomicUsize>,
}

impl Counters {
    fn sends(&self) -> usize {
        self.sends.load(Ordering::SeqCst)
    }
    fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }
    fn factory_calls(&self) -> usize {
        self.factory_calls.load(Ordering::SeqCst)
    }
}

/// Build an engine whose client factory produces `TrackingClient`s over shared
/// counters and counts how many times the factory itself is invoked.
fn engine_with(counters: &Counters, delay: Duration) -> Engine {
    let c = counters.clone();
    let mut engine = Engine::new();
    engine.with_http_client_factory(move || {
        c.factory_calls.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(TrackingClient {
            sends: c.sends.clone(),
            in_flight: c.in_flight.clone(),
            peak: c.peak.clone(),
            delay,
        }) as Arc<dyn HttpClient>)
    });
    engine.with_metrics_collector_factory(MetricsCollectorFactory::create_in_memory);
    engine
}

fn get(url: &str) -> Request {
    Request::get(url).build().unwrap()
}

fn base_options() -> ExecutionOptions {
    ExecutionOptions::builder()
        .virtual_users(1)
        .duration(Duration::from_secs(0))
        .ramp_up(Duration::from_secs(0))
        .think_time(Duration::from_secs(0))
        .max_concurrent_requests(0)
        .build()
}

// ===========================================================================
// Sustained load: the scenario iterates over `duration`,
// issuing far more than virtual_users * steps, and the run spans ~duration.
// Sanity companion: the duration-zero test issues exactly one pass, so
// the "many" in the sustained-load test genuinely comes from the iteration loop.
// ===========================================================================

#[tokio::test]
async fn proof1_sustained_load_iterates_over_duration() {
    let counters = Counters::default();
    let mut engine = engine_with(&counters, Duration::from_millis(2));

    let step = StepBuilder::new("s1", "S1", get("https://example.com/1"))
        .max_retries(0)
        .build();
    let scenario = ScenarioBuilder::new("s", "S")
        .step(step)
        .virtual_users(2)
        .duration(Duration::from_millis(300))
        .build()
        .unwrap();
    engine.add_scenario(scenario);

    let start = Instant::now();
    let results = engine.run_all(base_options()).await.unwrap();
    let elapsed = start.elapsed();

    let sends = counters.sends();
    // Old engine hard-cap was virtual_users*steps = 2. A ~300ms window at ~2ms
    // per send must produce dozens of iterations.
    assert!(
        sends > 20,
        "expected sustained iteration (>>2 sends); got {sends}"
    );
    assert_eq!(
        results.total_requests as usize, sends,
        "every send recorded exactly once"
    );
    assert!(
        results.requests_per_second > 0.0,
        "rps must reflect the real run window"
    );
    // ~duration wall clock: not sub-second-burst-then-return, not the 3600s
    // backstop.
    assert!(
        (Duration::from_millis(250)..Duration::from_secs(2)).contains(&elapsed),
        "run should span ~duration; took {elapsed:?}"
    );
}

#[tokio::test]
async fn proof1b_sanity_duration_zero_runs_exactly_one_pass() {
    let counters = Counters::default();
    let mut engine = engine_with(&counters, Duration::from_millis(0));

    let step = StepBuilder::new("s1", "S1", get("https://example.com/1"))
        .max_retries(0)
        .build();
    let scenario = ScenarioBuilder::new("s", "S")
        .step(step)
        .virtual_users(3)
        .duration(Duration::from_secs(0)) // single-pass mode
        .build()
        .unwrap();
    engine.add_scenario(scenario);

    let results = engine.run_all(base_options()).await.unwrap();
    // Exactly virtual_users * steps, proving the "many" in the sustained-load
    // test comes from the iteration loop.
    assert_eq!(counters.sends(), 3);
    assert_eq!(results.total_requests, 3);
}

// ===========================================================================
// Virtual-user concurrency: VU count defines concurrency; it is not capped by a
// pool-sourced knob. N VUs each issuing one slow send overlap: measured peak
// in-flight == N even when a large `max_concurrent_requests` is configured.
// Sanity companion: with max_concurrent_requests = 1 the per-request semaphore
// serialises sends (peak == 1) — proving the knob caps concurrent requests,
// not VU lifetime, and that its default (0/large) leaves VU concurrency
// unthrottled.
// ===========================================================================

#[tokio::test]
async fn proof2_vu_concurrency_not_capped_by_pool_knob() {
    const N: u32 = 8;
    let counters = Counters::default();
    let mut engine = engine_with(&counters, Duration::from_millis(120));

    let step = StepBuilder::new("s1", "S1", get("https://example.com/1"))
        .max_retries(0)
        .build();
    let scenario = ScenarioBuilder::new("s", "S")
        .step(step)
        .virtual_users(N)
        .duration(Duration::from_secs(0)) // one slow send per VU
        .build()
        .unwrap();
    engine.add_scenario(scenario);

    let mut options = base_options();
    // A large "pool-ish" request cap must NOT throttle the N VUs below N.
    options.max_concurrent_requests = 1000;

    engine.run_all(options).await.unwrap();

    assert_eq!(counters.sends(), N as usize);
    assert_eq!(
        counters.peak(),
        N as usize,
        "all {N} VUs must be in-flight simultaneously (peak={})",
        counters.peak()
    );
}

#[tokio::test]
async fn proof2b_sanity_request_cap_serialises_requests() {
    const N: u32 = 6;
    let counters = Counters::default();
    let mut engine = engine_with(&counters, Duration::from_millis(15));

    let step = StepBuilder::new("s1", "S1", get("https://example.com/1"))
        .max_retries(0)
        .build();
    let scenario = ScenarioBuilder::new("s", "S")
        .step(step)
        .virtual_users(N)
        .duration(Duration::from_secs(0))
        .build()
        .unwrap();
    engine.add_scenario(scenario);

    let mut options = base_options();
    options.max_concurrent_requests = 1; // cap concurrent REQUESTS at 1

    engine.run_all(options).await.unwrap();

    assert_eq!(counters.sends(), N as usize);
    assert_eq!(
        counters.peak(),
        1,
        "a request cap of 1 must serialise sends even with {N} VUs (peak={})",
        counters.peak()
    );
}

// ===========================================================================
// Shared HTTP client: one shared client for the whole scenario — the factory
// is invoked once, not once per VU. Sanity companion: with per-user isolation
// enabled the factory is invoked once per VU.
// ===========================================================================

#[tokio::test]
async fn proof3_client_factory_invoked_once_shared() {
    const N: u32 = 25;
    let counters = Counters::default();
    let mut engine = engine_with(&counters, Duration::from_millis(0));

    let step = StepBuilder::new("s1", "S1", get("https://example.com/1"))
        .max_retries(0)
        .build();
    let scenario = ScenarioBuilder::new("s", "S")
        .step(step)
        .virtual_users(N)
        .duration(Duration::from_secs(0))
        .build()
        .unwrap();
    engine.add_scenario(scenario);

    engine.run_all(base_options()).await.unwrap();

    assert_eq!(counters.sends(), N as usize);
    assert_eq!(
        counters.factory_calls(),
        1,
        "one shared client must be built for all {N} VUs; got {} builds",
        counters.factory_calls()
    );
}

#[tokio::test]
async fn proof3b_sanity_isolated_clients_built_per_vu() {
    const N: u32 = 5;
    let counters = Counters::default();
    let mut engine = engine_with(&counters, Duration::from_millis(0));

    let step = StepBuilder::new("s1", "S1", get("https://example.com/1"))
        .max_retries(0)
        .build();
    let scenario = ScenarioBuilder::new("s", "S")
        .step(step)
        .virtual_users(N)
        .duration(Duration::from_secs(0))
        .build()
        .unwrap();
    engine.add_scenario(scenario);

    let mut options = base_options();
    options.isolate_clients_per_user = true;

    engine.run_all(options).await.unwrap();

    assert_eq!(
        counters.factory_calls(),
        N as usize,
        "opt-in isolation must build one client per VU; got {} builds",
        counters.factory_calls()
    );
}

// ===========================================================================
// Intra-VU step concurrency: independent ready steps within one VU run concurrently.
// Two dependency-free steps with a slow mock overlap (peak in-flight == 2).
// Sanity companion: when the two steps are chained (B depends on A) they must
// never overlap (peak == 1), proving the concurrent overlap above is real
// intra-VU concurrency and not a dependency-ordering violation.
// ===========================================================================

#[tokio::test]
async fn proof4_independent_steps_run_concurrently() {
    let counters = Counters::default();
    let mut engine = engine_with(&counters, Duration::from_millis(120));

    let a = StepBuilder::new("a", "A", get("https://example.com/a"))
        .max_retries(0)
        .build();
    let b = StepBuilder::new("b", "B", get("https://example.com/b"))
        .max_retries(0)
        .build();
    // 1 VU, single pass, two INDEPENDENT root steps.
    let scenario = ScenarioBuilder::new("s", "S")
        .step(a)
        .step(b)
        .virtual_users(1)
        .duration(Duration::from_secs(0))
        .build()
        .unwrap();
    engine.add_scenario(scenario);

    engine.run_all(base_options()).await.unwrap();

    assert_eq!(counters.sends(), 2);
    assert_eq!(
        counters.peak(),
        2,
        "two independent steps in one VU must overlap (peak={})",
        counters.peak()
    );
}

#[tokio::test]
async fn proof4b_sanity_dependent_steps_are_serial() {
    let counters = Counters::default();
    let mut engine = engine_with(&counters, Duration::from_millis(60));

    let a = StepBuilder::new("a", "A", get("https://example.com/a"))
        .max_retries(0)
        .build();
    // b depends on a: they must run one-at-a-time.
    let b = StepBuilder::new("b", "B", get("https://example.com/b"))
        .max_retries(0)
        .dependency("a")
        .build();
    let scenario = ScenarioBuilder::new("s", "S")
        .step(a)
        .step(b)
        .virtual_users(1)
        .duration(Duration::from_secs(0))
        .build()
        .unwrap();
    engine.add_scenario(scenario);

    engine.run_all(base_options()).await.unwrap();

    assert_eq!(counters.sends(), 2);
    assert_eq!(
        counters.peak(),
        1,
        "a dependency chain must serialise (peak={})",
        counters.peak()
    );
}

// ===========================================================================
// Step timeout enforcement: step.timeout is enforced per attempt. A 50ms step
// timeout against a 500ms mock records a failed attempt with real elapsed
// ~50ms and returns promptly (not a ~500ms/30s hang). Sanity companion: a
// generous 500ms timeout against a fast mock succeeds, proving the timeout is a
// real cap and not an always-fail.
// ===========================================================================

#[tokio::test]
async fn proof5_step_timeout_enforced() {
    let counters = Counters::default();
    // Mock would take 500ms; step timeout is 50ms.
    let mut engine = engine_with(&counters, Duration::from_millis(500));

    let step = StepBuilder::new("slow", "Slow", get("https://example.com/slow"))
        .max_retries(0)
        .timeout(Duration::from_millis(50))
        .build();
    let scenario = ScenarioBuilder::new("s", "S")
        .step(step)
        .virtual_users(1)
        .duration(Duration::from_secs(0))
        .build()
        .unwrap();
    engine.add_scenario(scenario);

    let start = Instant::now();
    let results = engine.run_all(base_options()).await.unwrap();
    let elapsed = start.elapsed();

    // Exactly one attempt was sent and it is recorded as a FAILURE.
    assert_eq!(results.total_requests, 1);
    assert_eq!(results.failed_requests, 1);
    assert_eq!(results.successful_requests, 0);
    // Real elapsed ~= the 50ms timeout: bounded on BOTH sides. The lower bound
    // (>= ~40ms) proves the attempt actually waited roughly its timeout (real
    // elapsed, not an instant/zero-time fail); the upper bound (< 300ms) proves
    // it did NOT wait the 500ms mock nor the 30s default. Note the failure's
    // latency is intentionally kept out of `avg_response_time_ms` by the
    // aggregator (failures don't pollute the success distribution), so
    // wall-clock is the observable signal of the recorded real elapsed here.
    assert!(
        (Duration::from_millis(40)..Duration::from_millis(300)).contains(&elapsed),
        "timeout must fire near its 50ms cap (real elapsed), not instantly and \
         not after the 500ms mock; took {elapsed:?}"
    );
    assert_eq!(
        results.avg_response_time_ms, 0.0,
        "an all-failure run contributes nothing to the success-latency average"
    );
}

#[tokio::test]
async fn proof5b_sanity_generous_timeout_succeeds() {
    let counters = Counters::default();
    // Mock takes 30ms; step timeout is a generous 500ms => success.
    let mut engine = engine_with(&counters, Duration::from_millis(30));

    let step = StepBuilder::new("ok", "Ok", get("https://example.com/ok"))
        .max_retries(0)
        .timeout(Duration::from_millis(500))
        .build();
    let scenario = ScenarioBuilder::new("s", "S")
        .step(step)
        .virtual_users(1)
        .duration(Duration::from_secs(0))
        .build()
        .unwrap();
    engine.add_scenario(scenario);

    let results = engine.run_all(base_options()).await.unwrap();
    assert_eq!(results.total_requests, 1);
    assert_eq!(results.successful_requests, 1);
    assert_eq!(results.failed_requests, 0);
}

// ===========================================================================
// Think-time deadline behavior: think-time is never slept past the deadline — a
// think_time far larger than the whole run must not extend it. The run returns
// promptly (well under think_time). Sanity companion: the same window with
// think_time = 0 issues many more passes, proving the short count above is the
// deadline cutting think-time, not a stalled loop.
// ===========================================================================

#[tokio::test]
async fn proof6_think_time_not_slept_past_deadline() {
    let counters = Counters::default();
    let mut engine = engine_with(&counters, Duration::from_millis(1));

    let step = StepBuilder::new("s1", "S1", get("https://example.com/1"))
        .max_retries(0)
        .build();
    // think_time (5s) dwarfs duration (200ms): after the first in-window pass
    // the loop must break rather than sleep think_time.
    let scenario = ScenarioBuilder::new("s", "S")
        .step(step)
        .virtual_users(1)
        .duration(Duration::from_millis(200))
        .think_time(Duration::from_secs(5))
        .build()
        .unwrap();
    engine.add_scenario(scenario);

    let start = Instant::now();
    engine.run_all(base_options()).await.unwrap();
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(1),
        "run must return at the deadline, not after a 5s think; took {elapsed:?}"
    );
    let sends = counters.sends();
    assert!(
        (1..=3).contains(&sends),
        "think_time > duration => only the first pass(es) run; got {sends}"
    );
}

#[tokio::test]
async fn proof6b_sanity_zero_think_time_sustains_many_passes() {
    let counters = Counters::default();
    let mut engine = engine_with(&counters, Duration::from_millis(1));

    let step = StepBuilder::new("s1", "S1", get("https://example.com/1"))
        .max_retries(0)
        .build();
    let scenario = ScenarioBuilder::new("s", "S")
        .step(step)
        .virtual_users(1)
        .duration(Duration::from_millis(200))
        .think_time(Duration::from_secs(0))
        .build()
        .unwrap();
    engine.add_scenario(scenario);

    engine.run_all(base_options()).await.unwrap();
    // Same 200ms window, no think-time: many iterations, proving the short
    // count above is the deadline suppressing think-time (not a broken loop).
    assert!(
        counters.sends() > 20,
        "zero think_time over 200ms should sustain many passes; got {}",
        counters.sends()
    );
}
