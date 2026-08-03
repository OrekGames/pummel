//! Microbenchmarks isolating remaining hot-path costs:
//! - regex compile-per-match vs cached `Regex`
//! - JSON serialize-per-send vs pre-serialized `Bytes`
//! - HTTP send-path Request clone + reqwest materialize clones
//!
//! Both arms stay permanently so head-to-head comparisons do not need baselines.
//!
//! ```text
//! cargo bench --bench hot_path_micro_benchmarks -- --save-baseline before
//! cargo bench --bench hot_path_micro_benchmarks -- --baseline before
//! ```

use std::hint::black_box;
use std::time::Duration;

use bytes::Bytes;
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use pummel::http::{Body, Request};
use serde_json::json;

const REGEX_PATTERN: &str = r#""token"\s*:\s*"([^"]+)""#;
const HAYSTACK: &str = r#"{"ok":true,"token":"abc123xyz","nested":{"id":42},"msg":"hello world"}"#;

fn bench_regex_match(c: &mut Criterion) {
    let mut group = c.benchmark_group("regex_match");
    group.throughput(Throughput::Elements(1));

    group.bench_function("compile_each_time", |b| {
        b.iter(|| {
            let regex = regex::Regex::new(REGEX_PATTERN).expect("valid pattern");
            let captured = regex
                .captures(black_box(HAYSTACK))
                .and_then(|caps| caps.get(1).map(|m| m.as_str().to_string()));
            black_box(captured)
        });
    });

    let cached = regex::Regex::new(REGEX_PATTERN).expect("valid pattern");
    group.bench_function("cached", |b| {
        b.iter(|| {
            let captured = cached
                .captures(black_box(HAYSTACK))
                .and_then(|caps| caps.get(1).map(|m| m.as_str().to_string()));
            black_box(captured)
        });
    });

    group.finish();
}

fn bench_json_request_body(c: &mut Criterion) {
    let mut group = c.benchmark_group("json_request_body");
    group.throughput(Throughput::Elements(1));

    let payload = json!({
        "user": "alice",
        "roles": ["admin", "ops"],
        "meta": {"region": "us-east-1", "attempt": 1},
        "payload": "x".repeat(256),
    });

    group.bench_function("serialize_each_send", |b| {
        b.iter(|| {
            // Mirrors RequestBuilder::json + reqwest .json(): Value then bytes every send.
            let value = serde_json::to_value(black_box(&payload)).expect("to_value");
            let bytes = serde_json::to_vec(&value).expect("to_vec");
            black_box(bytes.len())
        });
    });

    let pre_serialized = Bytes::from(serde_json::to_vec(&payload).expect("to_vec"));
    group.bench_function("pre_serialized_bytes", |b| {
        b.iter(|| {
            // Hot path after fix: clone refcounted Bytes into the request builder.
            let body = pre_serialized.clone();
            black_box(body.len())
        });
    });

    group.finish();
}

/// Representative static-step request: several headers + 1 KiB pre-serialized body.
fn sample_send_request() -> Request {
    let body = Bytes::from(vec![b'x'; 1024]);
    Request::post("https://api.example.com/v1/load-test/ingest?vu=1")
        .header("User-Agent", "pummel-bench/0.1")
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header(
            "Authorization",
            "Bearer bench-token-abcdefghijklmnopqrstuvwxyz",
        )
        .header("X-Request-Id", "00000000-0000-0000-0000-000000000001")
        .header("X-Forwarded-For", "203.0.113.10")
        .header("X-Custom-A", "value-a")
        .header("X-Custom-B", "value-b")
        .binary(body)
        .timeout(Duration::from_secs(30))
        .build()
        .expect("valid sample request")
}

/// Mirrors `DefaultHttpClient::send` clones into reqwest (~method/url/headers/body).
fn materialize_send_clones(request: &Request) {
    let method = request.method().clone();
    let url = request.url().clone();
    let headers = request.headers().clone();
    let timeout = request.timeout();
    let body = match request.body() {
        Body::Empty => None,
        Body::Text(text) => Some(Bytes::from(text.clone())),
        Body::Json(json) => Some(Bytes::from(serde_json::to_vec(json).expect("json"))),
        Body::Binary(bytes) => Some(bytes.clone()),
    };
    black_box((method, url, headers, timeout, body));
}

fn bench_http_send_clones(c: &mut Criterion) {
    let mut group = c.benchmark_group("http_send_clones");
    group.throughput(Throughput::Elements(1));

    let request = sample_send_request();

    // Engine static path: `step.request.clone()` every attempt.
    group.bench_function("request_clone", |b| b.iter(|| black_box(request.clone())));

    // Send path only: clones method/URL/headers/body for reqwest.
    group.bench_function("send_materialize", |b| {
        b.iter(|| materialize_send_clones(black_box(&request)))
    });

    // Combined per-attempt cost for static steps (engine clone + send materialize).
    group.bench_function("clone_plus_materialize", |b| {
        b.iter(|| {
            let owned = request.clone();
            materialize_send_clones(black_box(&owned));
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_regex_match,
    bench_json_request_body,
    bench_http_send_clones
);
criterion_main!(benches);
