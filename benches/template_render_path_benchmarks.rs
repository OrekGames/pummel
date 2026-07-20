//! Head-to-head: dynamic JSON body path after template render.
//!
//! Current hot path in `render_request`:
//!   rendered String → `serde_json::from_str::<Value>` → `RequestBuilder::json`
//!   (which `serde_json::to_vec`s again into `Body::Binary`)
//!
//! Candidate:
//!   rendered String → validate JSON → `Body::Binary(Bytes::from(rendered))`
//!   (skip Value keep + re-serialize)
//!
//! Both arms stay permanently so the comparison survives without baselines.
//!
//! ```text
//! cargo bench --bench template_render_path_benchmarks -- --save-baseline before
//! cargo bench --bench template_render_path_benchmarks -- --baseline before
//! ```

use std::hint::black_box;

use bytes::Bytes;
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use serde::de::IgnoredAny;

/// Representative rendered JSON body (post-template), ~medium request payload.
fn sample_rendered_json() -> String {
    serde_json::json!({
        "user_id": "user-42",
        "session": "sess-abc-xyz",
        "roles": ["admin", "ops", "reader"],
        "meta": {
            "region": "us-east-1",
            "attempt": 3,
            "flags": {"beta": true, "debug": false}
        },
        "payload": "x".repeat(512),
        "items": [
            {"id": 1, "name": "alpha", "qty": 10},
            {"id": 2, "name": "beta", "qty": 20},
            {"id": 3, "name": "gamma", "qty": 30}
        ]
    })
    .to_string()
}

/// Current production path: parse to Value, then re-serialize (as `builder.json` does).
fn path_parse_and_reserialize(rendered: &str) -> Bytes {
    let value: serde_json::Value = serde_json::from_str(rendered).expect("valid json");
    Bytes::from(serde_json::to_vec(&value).expect("serialize"))
}

/// Candidate path: validate JSON, keep rendered UTF-8 bytes (no Value round-trip).
fn path_validate_and_binary(rendered: String) -> Bytes {
    serde_json::from_str::<IgnoredAny>(&rendered).expect("valid json");
    Bytes::from(rendered)
}

fn bench_dynamic_json_body(c: &mut Criterion) {
    let mut group = c.benchmark_group("dynamic_json_body");
    group.throughput(Throughput::Elements(1));

    let rendered = sample_rendered_json();

    group.bench_function("parse_value_then_reserialize", |b| {
        b.iter(|| {
            let body = path_parse_and_reserialize(black_box(rendered.as_str()));
            black_box(body.len())
        });
    });

    group.bench_function("validate_then_binary_bytes", |b| {
        b.iter(|| {
            let body = path_validate_and_binary(black_box(rendered.clone()));
            black_box(body.len())
        });
    });

    group.finish();
}

criterion_group!(benches, bench_dynamic_json_body);
criterion_main!(benches);
