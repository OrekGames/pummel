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

fn mock_resolve_expr(expr: &str) -> String {
    match expr {
        "vu.id" => "1".to_string(),
        "scenario.id" => "my_scenario".to_string(),
        "step.id" => "my_step".to_string(),
        _ => "unknown".to_string(),
    }
}

fn render_template_original(template: &str) -> String {
    let mut rendered = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        rendered.push_str(&rest[..start]);
        let after_start = &rest[start + 2..];
        let end = after_start.find("}}").unwrap();
        let expr = after_start[..end].trim();
        rendered.push_str(&mock_resolve_expr(expr));
        rest = &after_start[end + 2..];
    }
    rendered.push_str(rest);
    rendered
}

#[derive(Clone)]
enum TemplateSegment {
    Literal(String),
    Expression(String),
}

use lru::LruCache;
use std::cell::RefCell;
use std::num::NonZeroUsize;
use std::rc::Rc;

thread_local! {
    static TEMPLATE_CACHE: RefCell<LruCache<String, Rc<Vec<TemplateSegment>>>> =
        RefCell::new(LruCache::new(NonZeroUsize::new(1024).unwrap()));
}

fn render_template_optimized(template: &str) -> String {
    let segments = TEMPLATE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(segments) = cache.get(template) {
            return Rc::clone(segments);
        }

        let mut parsed = Vec::new();
        let mut rest = template;
        while let Some(start) = rest.find("{{") {
            if start > 0 {
                parsed.push(TemplateSegment::Literal(rest[..start].to_string()));
            }
            let after_start = &rest[start + 2..];
            let end = after_start.find("}}").unwrap();
            let expr = after_start[..end].trim();
            parsed.push(TemplateSegment::Expression(expr.to_string()));
            rest = &after_start[end + 2..];
        }
        if !rest.is_empty() {
            parsed.push(TemplateSegment::Literal(rest.to_string()));
        }

        let segments = Rc::new(parsed);
        cache.put(template.to_string(), Rc::clone(&segments));
        segments
    });

    let mut rendered = String::with_capacity(template.len());
    for segment in segments.iter() {
        match segment {
            TemplateSegment::Literal(text) => rendered.push_str(text),
            TemplateSegment::Expression(expr) => rendered.push_str(&mock_resolve_expr(expr)),
        }
    }
    rendered
}

fn bench_template_render(c: &mut Criterion) {
    let mut group = c.benchmark_group("template_render");
    group.throughput(Throughput::Elements(1));

    let template = "Hello {{vu.id}}, this is {{scenario.id}} at {{step.id}}! Here is some extra text to make it longer.";

    group.bench_function("original_parse_each_time", |b| {
        b.iter(|| {
            let rendered = render_template_original(black_box(template));
            black_box(rendered)
        });
    });

    // prime the cache
    render_template_optimized(template);

    group.bench_function("optimized_cached_segments", |b| {
        b.iter(|| {
            let rendered = render_template_optimized(black_box(template));
            black_box(rendered)
        });
    });

    group.finish();
}

criterion_group!(benches, bench_dynamic_json_body, bench_template_render);
criterion_main!(benches);
