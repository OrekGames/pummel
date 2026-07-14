use async_trait::async_trait;
use chrono::Utc;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use pummel::engine::{Engine, ExecutionOptions};
use pummel::http::{Body, HttpClient, HttpStatus, Request, Response};
use pummel::metrics::{
    MetricsCollector, RequestMetrics, RunStatus, ScenarioMetrics, StepMetrics, TestResults,
};
use pummel::scenario::{Scenario, ScenarioBuilder, StepBuilder};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Runtime;

// Mock HTTP client for benchmarking that returns immediately
struct MockHttpClient;

#[async_trait]
impl HttpClient for MockHttpClient {
    async fn send(&self, _request: &Request) -> pummel::error::Result<Response> {
        Ok(Response::new(
            HttpStatus::OK,
            Default::default(),
            Body::Text("Mock response".into()),
            Duration::from_millis(1),
        ))
    }

    async fn close(&self) -> pummel::error::Result<()> {
        Ok(())
    }
}

// Mock metrics collector that does minimal work
struct MockMetricsCollector;

#[async_trait]
impl MetricsCollector for MockMetricsCollector {
    async fn record_request(&self, _metrics: RequestMetrics) -> pummel::error::Result<()> {
        Ok(())
    }

    async fn get_step_metrics(
        &self,
        _step_id: &String,
    ) -> pummel::error::Result<Option<StepMetrics>> {
        Ok(None)
    }

    async fn get_scenario_metrics(
        &self,
        _scenario_id: &String,
    ) -> pummel::error::Result<Option<ScenarioMetrics>> {
        Ok(None)
    }

    async fn get_test_results(&self) -> pummel::error::Result<TestResults> {
        Ok(TestResults {
            status: RunStatus::Completed,
            scenarios: HashMap::new(),
            total_requests: 0,
            successful_requests: 0,
            failed_requests: 0,
            avg_response_time_ms: 0.0,
            p90_response_time_ms: 0,
            requests_per_second: 0.0,
            error_rate: 0.0,
            start_time: Utc::now(),
            end_time: Utc::now(),
            duration_seconds: 0.0,
            total_virtual_users: 0,
        })
    }

    async fn reset(&self) -> pummel::error::Result<()> {
        Ok(())
    }

    async fn flush(&self) -> pummel::error::Result<()> {
        Ok(())
    }
}

// Helper function to create a test scenario with a specified number of steps
fn create_test_scenario(num_steps: usize, with_dependencies: bool, num_users: u32) -> Scenario {
    let mut builder = ScenarioBuilder::new(
        format!("bench_scenario_{num_steps}"),
        format!("Benchmark Scenario with {num_steps} steps"),
    );

    // Add the first step
    let first_step = StepBuilder::new(
        "step_1",
        "Step 1",
        Request::get("https://localhost/1").build().unwrap(),
    )
    .max_retries(0)
    .timeout(Duration::from_secs(1))
    .build();

    builder = builder.step(first_step);

    // Add remaining steps
    for i in 2..=num_steps {
        let mut step_builder = StepBuilder::new(
            format!("step_{i}"),
            format!("Step {i}"),
            Request::get(format!("https://localhost/{i}"))
                .build()
                .unwrap(),
        )
        .max_retries(0)
        .timeout(Duration::from_secs(1));

        // Add dependency on the previous step if requested
        if with_dependencies {
            step_builder = step_builder.dependency(format!("step_{}", i - 1));
        }

        builder = builder.step(step_builder.build());
    }

    // M38: keep the load model instantaneous so the benchmark measures the
    // engine's dispatch / VU-spawn work, not the tokio timer wheel. `run_all`
    // overrides ExecutionOptions.{virtual_users,duration,ramp_up,think_time}
    // with these scenario values (engine.rs), so a non-zero ramp/think/duration
    // here would silently dominate the measured region. duration(0) selects
    // single-pass mode (one DAG pass per VU, no deadline sleeps); ramp_up(0)
    // and think_time(0) remove all inter-VU / inter-pass sleeps.
    builder
        .virtual_users(num_users)
        .duration(Duration::from_secs(0))
        .ramp_up(Duration::from_secs(0))
        .think_time(Duration::from_secs(0))
        .build()
        .unwrap()
}

// Helper function to create a test engine with a scenario
fn create_test_engine(num_steps: usize, with_dependencies: bool, num_users: u32) -> Engine {
    let scenario = create_test_scenario(num_steps, with_dependencies, num_users);
    let mut engine = Engine::new();
    engine.add_scenario(scenario);

    // Configure the engine to use our mock HTTP client
    engine.with_http_client_factory(|| Ok(Arc::new(MockHttpClient)));

    // Configure the engine to use our mock metrics collector
    engine.with_metrics_collector_factory(|| Arc::new(MockMetricsCollector));

    engine
}

// Benchmark scenario creation
fn bench_scenario_creation(c: &mut Criterion) {
    let mut group = c.benchmark_group("scenario_creation");

    for size in [5, 10, 20, 50].iter() {
        group.bench_with_input(BenchmarkId::new("linear", size), size, |b, &size| {
            b.iter(|| create_test_scenario(size, false, 1));
        });

        group.bench_with_input(
            BenchmarkId::new("with_dependencies", size),
            size,
            |b, &size| {
                b.iter(|| create_test_scenario(size, true, 1));
            },
        );
    }
    group.finish();
}

// Benchmark engine initialization
fn bench_engine_initialization(c: &mut Criterion) {
    let mut group = c.benchmark_group("engine_initialization");

    for size in [1, 5, 10, 20].iter() {
        group.bench_with_input(
            BenchmarkId::new("engine_with_scenarios", size),
            size,
            |b, &size| {
                b.iter(|| create_test_engine(size, true, 1));
            },
        );
    }

    group.finish();
}

// Benchmark run_all method (async) with different numbers of steps
fn bench_run_all(c: &mut Criterion) {
    let mut group = c.benchmark_group("run_all");

    // Create a runtime for executing async functions
    let rt = Runtime::new().unwrap();

    // Create scenarios with different numbers of steps
    for size in [1, 3, 5].iter() {
        let engine = create_test_engine(*size, true, 1);
        // virtual_users/duration/ramp_up/think_time are overridden by the
        // scenario's (all-zero) values in run_all, so only the honored knob
        // (max_concurrent_requests) is set here.
        let options = ExecutionOptions::builder()
            .max_concurrent_requests(10)
            .build();

        group.bench_with_input(BenchmarkId::new("steps", size), size, |b, _| {
            b.iter(|| {
                rt.block_on(async {
                    let _ = engine.run_all(options.clone()).await;
                });
            });
        });
    }

    group.finish();
}

// Benchmark run_all method (async) with different numbers of virtual users
fn bench_run_all_virtual_users(c: &mut Criterion) {
    let mut group = c.benchmark_group("run_all_virtual_users");

    // Configure the group for larger benchmarks
    group.sample_size(10); // Reduce sample size for large benchmarks
    group.measurement_time(Duration::from_secs(5)); // Shorter measurement time

    // Create a runtime for executing async functions
    let rt = Runtime::new().unwrap();

    // Fixed number of steps for all benchmarks
    const NUM_STEPS: usize = 3;

    // Test with different numbers of virtual users
    for &users in [1, 10, 50, 100, 500, 1000, 5000, 10000].iter() {
        // Create an engine with the appropriate number of virtual users
        let engine = create_test_engine(NUM_STEPS, true, users);

        // virtual_users/duration/ramp_up/think_time are overridden by the
        // scenario (single-pass, zero sleeps); only max_concurrent_requests is
        // honored by run_all, so scale just that with the user count.
        let options = ExecutionOptions::builder()
            .max_concurrent_requests(users as usize * 2)
            .build();

        group.bench_with_input(BenchmarkId::new("users", users), &users, |b, _| {
            b.iter(|| {
                rt.block_on(async {
                    let _ = engine.run_all(options.clone()).await;
                });
            });
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_scenario_creation,
    bench_engine_initialization,
    bench_run_all,
    bench_run_all_virtual_users,
);
criterion_main!(benches);
