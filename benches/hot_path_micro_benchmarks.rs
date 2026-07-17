//! Microbenchmarks isolating remaining hot-path costs:
//! - regex compile-per-match vs cached `Regex`
//! - JSON serialize-per-send vs pre-serialized `Bytes`
//!
//! Both arms stay permanently so head-to-head comparisons do not need baselines.
//!
//! ```text
//! cargo bench --bench hot_path_micro_benchmarks -- --save-baseline before
//! cargo bench --bench hot_path_micro_benchmarks -- --baseline before
//! ```

use std::hint::black_box;

use bytes::Bytes;
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
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

criterion_group!(benches, bench_regex_match, bench_json_request_body);
criterion_main!(benches);
