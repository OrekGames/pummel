//! Head-to-head benches for metrics hot-path recording:
//! - full [`RequestMetrics`] construction + `record_request` (legacy engine path)
//! - slim [`AttemptSummary`] + `record_attempt_summary` (default collector, no telemetry)
//! - construct+noop vs skip (metrics disabled, no telemetry)
//!
//! Both arms stay permanently so comparisons do not depend on baselines.
//!
//! ```text
//! cargo bench --bench metrics_record_path_benchmarks -- --save-baseline before
//! cargo bench --bench metrics_record_path_benchmarks -- --baseline before | tee /tmp/bench_compare.txt
//! ```

use std::hint::black_box;
use std::time::Duration;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use pummel::http::{Body, HttpStatus, Request, Response};
use pummel::metrics::{
    AttemptSummary, InMemoryMetricsCollector, MetricsCollector, NoopMetricsCollector,
    RequestMetrics,
};
use tokio::runtime::Builder;
use uuid::Uuid;

fn sample_request() -> Request {
    Request::get("https://localhost/api/v1/items?limit=50")
        .build()
        .expect("request")
}

fn sample_response() -> Response {
    Response::new(
        HttpStatus::OK,
        Default::default(),
        Body::Binary(bytes::Bytes::from_static(
            br#"{"ok":true,"items":[1,2,3],"msg":"hello"}"#,
        )),
        Duration::from_millis(12),
    )
}

fn bench_full_vs_summary(c: &mut Criterion) {
    let mut group = c.benchmark_group("metrics_record_path");
    group.throughput(Throughput::Elements(1));
    group.sample_size(40);
    group.measurement_time(Duration::from_secs(3));
    group.warm_up_time(Duration::from_secs(1));

    let rt = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    let request = sample_request();
    let response = sample_response();
    let elapsed = Duration::from_millis(12);

    group.bench_function("full_request_metrics", |b| {
        b.iter(|| {
            rt.block_on(async {
                let collector = InMemoryMetricsCollector::new();
                let metrics = RequestMetrics::new(
                    Uuid::new_v4().to_string(),
                    "step_get".to_string(),
                    "Get Items".to_string(),
                    "scenario_main".to_string(),
                    "Main Scenario".to_string(),
                    7,
                    black_box(&request),
                    Some(black_box(&response)),
                    None,
                    elapsed,
                );
                collector
                    .record_request(black_box(metrics))
                    .await
                    .expect("record");
                black_box(collector)
            })
        });
    });

    group.bench_function("attempt_summary", |b| {
        b.iter(|| {
            rt.block_on(async {
                let collector = InMemoryMetricsCollector::new();
                let summary = AttemptSummary {
                    scenario_id: "scenario_main",
                    step_id: "step_get",
                    step_name: "Get Items",
                    scenario_name: "Main Scenario",
                    virtual_user_id: 7,
                    success: true,
                    elapsed,
                };
                collector
                    .record_attempt_summary(black_box(summary))
                    .await
                    .expect("record");
                black_box(collector)
            })
        });
    });

    group.finish();
}

fn bench_noop_construct_vs_skip(c: &mut Criterion) {
    let mut group = c.benchmark_group("metrics_noop_path");
    group.throughput(Throughput::Elements(1));
    group.sample_size(40);
    group.measurement_time(Duration::from_secs(3));
    group.warm_up_time(Duration::from_secs(1));

    let rt = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    let request = sample_request();
    let response = sample_response();
    let elapsed = Duration::from_millis(12);

    // Legacy: still build RequestMetrics and await the noop collector.
    group.bench_function("construct_and_noop_record", |b| {
        b.iter(|| {
            rt.block_on(async {
                let collector = NoopMetricsCollector::new();
                assert!(!collector.records_requests());
                let metrics = RequestMetrics::new(
                    Uuid::new_v4().to_string(),
                    "step_get".to_string(),
                    "Get Items".to_string(),
                    "scenario_main".to_string(),
                    "Main Scenario".to_string(),
                    7,
                    black_box(&request),
                    Some(black_box(&response)),
                    None,
                    elapsed,
                );
                collector
                    .record_request(black_box(metrics))
                    .await
                    .expect("record");
                black_box(())
            })
        });
    });

    // Fixed: when records_requests is false and there is no telemetry, skip.
    group.bench_function("skip_when_unneeded", |b| {
        b.iter(|| {
            let collector = NoopMetricsCollector::new();
            if collector.records_requests() {
                unreachable!("noop must not record");
            }
            black_box(())
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_full_vs_summary,
    bench_noop_construct_vs_skip
);
criterion_main!(benches);
