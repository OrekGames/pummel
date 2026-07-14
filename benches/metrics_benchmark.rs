use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use pummel::http::{Body, HttpStatus, Request, Response};
use pummel::metrics::{
    InMemoryMetricsCollector, MetricsCollector, MetricsCollectorFactory, RequestMetrics,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Runtime;
use uuid::Uuid;

// Helper function to create a test request
fn create_test_request() -> Request {
    Request::get("https://localhost/test").build().unwrap()
}

// Helper function to create a test response
fn create_test_response() -> Response {
    Response::new(
        HttpStatus::OK,
        Default::default(),
        Body::Text("Test response".into()),
        Duration::from_millis(10),
    )
}

// Helper function to create a test request metrics
fn create_test_metrics(
    step_id: &str,
    scenario_id: &str,
    virtual_user_id: u32,
    success: bool,
) -> RequestMetrics {
    let request = create_test_request();
    let response = if success {
        Some(create_test_response())
    } else {
        None
    };
    let error = if success {
        None
    } else {
        Some("Test error".to_string())
    };
    let elapsed = response
        .as_ref()
        .map(|r| r.response_time())
        .unwrap_or_default();

    RequestMetrics::new(
        Uuid::new_v4().to_string(),
        step_id.to_string(),
        step_id.to_string(),
        scenario_id.to_string(),
        scenario_id.to_string(),
        virtual_user_id,
        &request,
        response.as_ref(),
        error,
        elapsed,
    )
}

// Benchmark single-threaded recording of metrics
fn bench_single_threaded_recording(c: &mut Criterion) {
    let mut group = c.benchmark_group("metrics_single_threaded_recording");

    // Configure the group for faster benchmarks
    group.sample_size(10); // Reduce sample size
    group.measurement_time(Duration::from_secs(2)); // Shorter measurement time

    // Create a runtime for executing async functions
    let rt = Runtime::new().unwrap();

    // Test with different numbers of metrics (reduced range)
    for &num_metrics in [10, 100, 1000].iter() {
        // Benchmark InMemoryMetricsCollector
        group.bench_with_input(
            BenchmarkId::new("in_memory", num_metrics),
            &num_metrics,
            |b, &num_metrics| {
                b.iter(|| {
                    rt.block_on(async {
                        let collector = InMemoryMetricsCollector::new();

                        for i in 0..num_metrics {
                            let metrics = create_test_metrics(
                                "step_1",
                                "scenario_1",
                                1,
                                i % 2 == 0, // Alternate success/failure
                            );
                            collector.record_request(metrics).await.unwrap();
                        }
                    });
                });
            },
        );
    }

    group.finish();
}

// Benchmark multithreaded recording of metrics
fn bench_multi_threaded_recording(c: &mut Criterion) {
    let mut group = c.benchmark_group("metrics_multi_threaded_recording");

    // Configure the group for faster benchmarks
    group.sample_size(10); // Reduce sample size
    group.measurement_time(Duration::from_secs(2)); // Shorter measurement time

    // Create a runtime for executing async functions
    let rt = Runtime::new().unwrap();

    // Test with fewer threads
    for &num_threads in [2, 4].iter() {
        // Reduced number of metrics per thread
        const METRICS_PER_THREAD: usize = 100;

        // Benchmark InMemoryMetricsCollector
        group.bench_with_input(
            BenchmarkId::new("in_memory", num_threads),
            &num_threads,
            |b, &num_threads| {
                b.iter(|| {
                    rt.block_on(async {
                        let collector = Arc::new(InMemoryMetricsCollector::new());

                        let handles: Vec<_> = (0..num_threads)
                            .map(|thread_id| {
                                let collector = collector.clone();
                                tokio::spawn(async move {
                                    for i in 0..METRICS_PER_THREAD {
                                        let metrics = create_test_metrics(
                                            &format!("step_{}", i % 5 + 1),
                                            &format!("scenario_{}", thread_id % 2 + 1),
                                            thread_id as u32,
                                            i % 2 == 0, // Alternate success/failure
                                        );
                                        collector.record_request(metrics).await.unwrap();
                                    }
                                })
                            })
                            .collect();

                        for handle in handles {
                            handle.await.unwrap();
                        }
                    });
                });
            },
        );
    }

    group.finish();
}

// Benchmark burst recording of metrics
fn bench_burst_recording(c: &mut Criterion) {
    let mut group = c.benchmark_group("metrics_burst_recording");

    // Configure the group for faster benchmarks
    group.sample_size(10); // Reduce sample size
    group.measurement_time(Duration::from_secs(2)); // Shorter measurement time

    // Create a runtime for executing async functions
    let rt = Runtime::new().unwrap();

    // Test with smaller burst sizes
    for &burst_size in [10, 100, 1000].iter() {
        // Benchmark InMemoryMetricsCollector
        group.bench_with_input(
            BenchmarkId::new("in_memory", burst_size),
            &burst_size,
            |b, &burst_size| {
                b.iter(|| {
                    rt.block_on(async {
                        let collector = InMemoryMetricsCollector::new();

                        // Pre-create all metrics to simulate a burst
                        let metrics: Vec<_> = (0..burst_size)
                            .map(|i| {
                                create_test_metrics(
                                    &format!("step_{}", i % 5 + 1),
                                    "scenario_1",
                                    1,
                                    i % 2 == 0, // Alternate success/failure
                                )
                            })
                            .collect();

                        // Record all metrics in a burst
                        for metric in metrics {
                            collector.record_request(metric).await.unwrap();
                        }
                    });
                });
            },
        );
    }

    group.finish();
}

// Benchmark metrics retrieval
fn bench_metrics_retrieval(c: &mut Criterion) {
    let mut group = c.benchmark_group("metrics_retrieval");

    // Configure the group for faster benchmarks
    group.sample_size(10); // Reduce sample size
    group.measurement_time(Duration::from_secs(2)); // Shorter measurement time

    // Create a runtime for executing async functions
    let rt = Runtime::new().unwrap();

    // Test with smaller numbers of recorded metrics
    for &num_metrics in [10, 100, 1000].iter() {
        // Benchmark InMemoryMetricsCollector
        group.bench_with_input(
            BenchmarkId::new("in_memory", num_metrics),
            &num_metrics,
            |b, &num_metrics| {
                // Setup: record metrics
                let collector = InMemoryMetricsCollector::new();
                rt.block_on(async {
                    for i in 0..num_metrics {
                        let metrics = create_test_metrics(
                            &format!("step_{}", i % 5 + 1),
                            &format!("scenario_{}", i % 2 + 1),
                            i as u32 % 5,
                            i % 2 == 0, // Alternate success/failure
                        );
                        collector.record_request(metrics).await.unwrap();
                    }
                });

                // Benchmark retrieval operations
                b.iter(|| {
                    rt.block_on(async {
                        // Get test results (most comprehensive operation)
                        let _results = collector.get_test_results().await.unwrap();
                    });
                });
            },
        );
    }

    group.finish();
}

// Benchmark factory methods
fn bench_factory_methods(c: &mut Criterion) {
    let mut group = c.benchmark_group("metrics_factory_methods");

    // Configure the group for faster benchmarks
    group.sample_size(10); // Reduce sample size
    group.measurement_time(Duration::from_secs(1)); // Shorter measurement time

    // Create a runtime for executing async functions
    let rt = Runtime::new().unwrap();

    // Benchmark in-memory factory method - create once and use in benchmark
    group.bench_function("create_in_memory", |b| {
        // Setup phase - not measured
        b.iter_with_setup(
            || {
                // This setup function runs before each iteration but is not measured
                // No setup needed for in-memory collector
            },
            |_| {
                // This is the measured part
                rt.block_on(async {
                    let collector = MetricsCollectorFactory::create_in_memory();
                    // Do a simple operation to ensure the collector is used
                    let _ = collector.get_test_results().await.unwrap();
                })
            },
        );
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_single_threaded_recording,
    bench_multi_threaded_recording,
    bench_burst_recording,
    bench_metrics_retrieval,
    bench_factory_methods,
);
criterion_main!(benches);
