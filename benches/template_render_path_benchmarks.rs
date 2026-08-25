//! Dynamic JSON body path after template render.
//!
//! Production `render_request` already validates with `IgnoredAny` and keeps
//! `Body::Binary(Bytes::from(rendered))`. The parse-then-reserialize arm is a
//! permanent historical regression guard, not the live hot path.
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

/// Historical path: parse to Value, then re-serialize.
fn path_parse_and_reserialize(rendered: &str) -> Bytes {
    let value: serde_json::Value = serde_json::from_str(rendered).expect("valid json");
    Bytes::from(serde_json::to_vec(&value).expect("serialize"))
}

/// Production path: validate JSON, keep rendered UTF-8 bytes (no Value round-trip).
fn path_validate_and_binary(rendered: String) -> Bytes {
    serde_json::from_str::<IgnoredAny>(&rendered).expect("valid json");
    Bytes::from(rendered)
}

fn bench_dynamic_json_body(c: &mut Criterion) {
    let mut group = c.benchmark_group("dynamic_json_body_historical_vs_production");
    group.throughput(Throughput::Elements(1));

    let rendered = sample_rendered_json();

    group.bench_function("historical_parse_value_then_reserialize", |b| {
        b.iter(|| {
            let body = path_parse_and_reserialize(black_box(rendered.as_str()));
            black_box(body.len())
        });
    });

    group.bench_function("production_validate_then_binary_bytes", |b| {
        b.iter(|| {
            let body = path_validate_and_binary(black_box(rendered.clone()));
            black_box(body.len())
        });
    });

    group.finish();
}

use pummel::engine::render_template;
use pummel::scenario::VuContext;

fn bench_template_render(c: &mut Criterion) {
    let mut group = c.benchmark_group("template_render");
    group.throughput(Throughput::Elements(1));

    let ctx = VuContext::new(1, "my_scenario".to_string());
    let step_id = "my_step";

    let template_single = "Hello {{vu.id}}, this is {{scenario.id}} at {{step.id}}! Here is some extra text to make it longer.";
    let _ = render_template(&ctx, step_id, template_single).unwrap();

    group.bench_function("production_render_cached", |b| {
        b.iter(|| {
            let rendered =
                render_template(black_box(&ctx), black_box(step_id), black_box(template_single)).unwrap();
            black_box(rendered)
        });
    });

    let template_multi = "{{vu.id}}/{{scenario.id}}/{{step.id}}";
    let _ = render_template(&ctx, step_id, template_multi).unwrap();

    group.bench_function("production_render_cached_multi", |b| {
        b.iter(|| {
            let rendered =
                render_template(black_box(&ctx), black_box(step_id), black_box(template_multi)).unwrap();
            black_box(rendered)
        });
    });

    group.finish();
}

criterion_group!(benches, bench_dynamic_json_body, bench_template_render);
criterion_main!(benches);
