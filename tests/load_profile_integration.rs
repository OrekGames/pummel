//! End-to-end coverage for open-loop think-time interaction and `LoadProfile`.
//!
//! Drives the public engine API through the mock HTTP client factory and asserts
//! on observable send counts, wall-clock duration, and aggregate `RunStatus`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use pummel::engine::{Engine, ExecutionOptions};
use pummel::error::Result;
use pummel::http::{Body, HttpClient, HttpStatus, Request, Response};
use pummel::metrics::{MetricsCollectorFactory, RunStatus};
use pummel::scenario::{LoadProfile, LoadStage, ScenarioBuilder, StepBuilder};

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

fn counting_engine(sends: Arc<AtomicUsize>, per_send: Duration) -> Engine {
    let sends_factory = sends.clone();
    let mut engine = Engine::new();
    engine.with_http_client_factory(move || {
        Ok(Arc::new(CountingClient {
            sends: sends_factory.clone(),
            per_send,
        }) as Arc<dyn HttpClient>)
    });
    engine.with_metrics_collector_factory(MetricsCollectorFactory::create_in_memory);
    engine
}

fn single_step(id: &str) -> pummel::scenario::Step {
    StepBuilder::new(
        id,
        id,
        Request::get(format!("https://example.com/{id}"))
            .build()
            .unwrap(),
    )
    .max_retries(0)
    .build()
}

fn base_options() -> ExecutionOptions {
    ExecutionOptions::builder()
        .virtual_users(1)
        .duration(Duration::from_secs(0))
        .ramp_up(Duration::from_secs(0))
        .think_time(Duration::from_secs(0))
        .build()
}

/// Open-loop load must ignore think time. A think_time larger than the run
/// window would otherwise end the VU after the first pass (closed-loop
/// deadline guard), starving open-loop arrival pacing.
#[tokio::test]
async fn open_loop_ignores_non_zero_think_time() {
    let sends = Arc::new(AtomicUsize::new(0));
    let mut engine = counting_engine(sends.clone(), Duration::from_millis(1));

    let scenario = ScenarioBuilder::new("s", "S")
        .step(single_step("s1"))
        .virtual_users(1)
        .duration(Duration::from_millis(400))
        .ramp_up(Duration::from_secs(0))
        // Far larger than the run window — must not gate open-loop iterations.
        .think_time(Duration::from_secs(5))
        .build()
        .unwrap();
    engine.add_scenario(scenario);

    let mut options = base_options();
    options.target_rps = Some(50.0);

    let start = Instant::now();
    let results = engine.run_all(options).await.unwrap();
    let elapsed = start.elapsed();
    let total = sends.load(Ordering::SeqCst);

    assert!(
        elapsed < Duration::from_secs(2),
        "open-loop run must not sleep think_time; took {elapsed:?}"
    );
    assert!(
        (5..=60).contains(&total),
        "target_rps=50 over 400ms should pace to ~20 sends despite think_time=5s; got {total}"
    );
    assert!(
        results.total_requests >= 5,
        "results should reflect paced open-loop traffic; got {}",
        results.total_requests
    );
}

/// Sequential load-profile stages run one after another (not in parallel).
#[tokio::test]
async fn load_profile_runs_stages_sequentially() {
    let sends = Arc::new(AtomicUsize::new(0));
    let mut engine = counting_engine(sends.clone(), Duration::from_millis(1));

    let profile = LoadProfile {
        stages: vec![
            LoadStage {
                name: Some("warmup".into()),
                duration_seconds: 1,
                virtual_users: Some(1),
                target_rps: Some(8.0),
                ramp_up_seconds: Some(0),
                think_time_ms: Some(0),
            },
            LoadStage {
                name: Some("steady".into()),
                duration_seconds: 1,
                virtual_users: Some(1),
                target_rps: Some(20.0),
                ramp_up_seconds: Some(0),
                think_time_ms: Some(0),
            },
        ],
    };

    let scenario = ScenarioBuilder::new("profiled", "Profiled")
        .step(single_step("s1"))
        .virtual_users(1)
        .duration(Duration::from_secs(0))
        .ramp_up(Duration::from_secs(0))
        .think_time(Duration::from_secs(0))
        .load_profile(profile)
        .build()
        .unwrap();
    engine.add_scenario(scenario);

    let start = Instant::now();
    let results = engine.run_all(base_options()).await.unwrap();
    let elapsed = start.elapsed();
    let total = sends.load(Ordering::SeqCst);

    // Two 1s stages should take roughly 2s of wall clock (plus small overhead).
    assert!(
        elapsed >= Duration::from_millis(1500),
        "stages must run sequentially; finished too fast: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "staged run should not linger far past ~2s; took {elapsed:?}"
    );
    // warmup ~8 + steady ~20 ≈ 28; allow wide bounds for scheduler jitter.
    assert!(
        (10..=80).contains(&total),
        "expected paced sends across both stages; got {total}"
    );
    assert!(
        !matches!(results.status, RunStatus::Failed { .. }),
        "staged open-loop run should not fail; got {:?}",
        results.status
    );
    assert_eq!(results.total_requests, total as u64);
}

/// Stage fields override the scenario baseline when parent `target_rps` is unset.
#[tokio::test]
async fn load_profile_stage_overrides_apply() {
    let sends = Arc::new(AtomicUsize::new(0));
    let mut engine = counting_engine(sends.clone(), Duration::from_millis(1));

    let profile = LoadProfile {
        stages: vec![LoadStage {
            name: Some("override".into()),
            duration_seconds: 1,
            virtual_users: Some(2),
            target_rps: Some(10.0),
            ramp_up_seconds: Some(0),
            // Non-zero think time must still be ignored under stage open-loop.
            think_time_ms: Some(5_000),
        }],
    };

    // Scenario baseline is closed-loop / single VU / zero duration; the stage
    // must supply the effective load parameters.
    let scenario = ScenarioBuilder::new("override", "Override")
        .step(single_step("s1"))
        .virtual_users(1)
        .duration(Duration::from_secs(0))
        .ramp_up(Duration::from_secs(0))
        .think_time(Duration::from_secs(0))
        .load_profile(profile)
        .build()
        .unwrap();
    engine.add_scenario(scenario);

    let start = Instant::now();
    let results = engine.run_all(base_options()).await.unwrap();
    let elapsed = start.elapsed();
    let total = sends.load(Ordering::SeqCst);

    assert!(
        elapsed < Duration::from_secs(3),
        "stage think_time_ms must not extend an open-loop stage; took {elapsed:?}"
    );
    assert!(
        (4..=40).contains(&total),
        "stage target_rps=10 over 1s should pace to ~10 sends; got {total}"
    );
    assert_eq!(results.total_virtual_users, 2);
}

/// Closed-loop stages that exit via the think-time deadline guard complete
/// cleanly; the profile aggregate stays `Completed`.
#[tokio::test]
async fn load_profile_aggregate_status_completed() {
    let sends = Arc::new(AtomicUsize::new(0));
    let mut engine = counting_engine(sends.clone(), Duration::from_millis(0));

    // Large think_time relative to the stage window forces a clean stop after
    // the first pass (never mid-pass truncation).
    let profile = LoadProfile {
        stages: vec![
            LoadStage {
                name: Some("a".into()),
                duration_seconds: 1,
                virtual_users: Some(1),
                target_rps: None,
                ramp_up_seconds: Some(0),
                think_time_ms: Some(5_000),
            },
            LoadStage {
                name: Some("b".into()),
                duration_seconds: 1,
                virtual_users: Some(1),
                target_rps: None,
                ramp_up_seconds: Some(0),
                think_time_ms: Some(5_000),
            },
        ],
    };

    let scenario = ScenarioBuilder::new("done", "Done")
        .step(single_step("s1"))
        .virtual_users(1)
        .duration(Duration::from_secs(0))
        .ramp_up(Duration::from_secs(0))
        .think_time(Duration::from_secs(0))
        .load_profile(profile)
        .build()
        .unwrap();
    engine.add_scenario(scenario);

    let start = Instant::now();
    let results = engine.run_all(base_options()).await.unwrap();
    let elapsed = start.elapsed();

    assert_eq!(results.status, RunStatus::Completed);
    assert_eq!(sends.load(Ordering::SeqCst), 2);
    assert_eq!(results.total_requests, 2);
    assert!(
        elapsed < Duration::from_secs(2),
        "think-time deadline guard must end each stage promptly; took {elapsed:?}"
    );
}

/// Aggregate run status across stages prefers Truncated over Completed when any
/// stage is cut short mid-pass by the rate limiter.
#[tokio::test]
async fn load_profile_aggregate_status_prefers_truncated() {
    let sends = Arc::new(AtomicUsize::new(0));
    let mut engine = counting_engine(sends.clone(), Duration::from_millis(0));

    let profile = LoadProfile {
        stages: vec![
            LoadStage {
                name: Some("ok".into()),
                duration_seconds: 1,
                virtual_users: Some(1),
                target_rps: None,
                ramp_up_seconds: Some(0),
                // Clean Completed for stage 1.
                think_time_ms: Some(5_000),
            },
            LoadStage {
                name: Some("cut_short".into()),
                // 1s window with 1 rps: first attempt starts, waiting for the
                // next permit exceeds the deadline → Truncated.
                duration_seconds: 1,
                virtual_users: Some(1),
                target_rps: Some(1.0),
                ramp_up_seconds: Some(0),
                think_time_ms: Some(0),
            },
        ],
    };

    let scenario = ScenarioBuilder::new("agg", "Aggregate")
        .step(single_step("s1"))
        .virtual_users(1)
        .duration(Duration::from_secs(0))
        .ramp_up(Duration::from_secs(0))
        .think_time(Duration::from_secs(0))
        .load_profile(profile)
        .build()
        .unwrap();
    engine.add_scenario(scenario);

    let results = engine.run_all(base_options()).await.unwrap();

    assert!(
        matches!(results.status, RunStatus::Truncated { .. }),
        "combined status should be Truncated when any stage truncates; got {:?}",
        results.status
    );
    assert!(
        results.total_requests >= 2,
        "both stages should contribute traffic before truncation; got {}",
        results.total_requests
    );
}
