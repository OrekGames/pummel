//! Microbenchmarks for data-path hot costs:
//! - JSON-path parse-per-extract vs cached tokens
//! - deep `Value` clone vs `Arc::clone` for fixture row bind
//!
//! Both arms stay permanently so head-to-head comparisons do not need baselines.
//!
//! ```text
//! cargo bench --bench data_path_benchmarks -- --save-baseline before
//! cargo bench --bench data_path_benchmarks -- --baseline before
//! ```

use std::hint::black_box;
use std::sync::Arc;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use serde_json::{Value, json};

#[derive(Clone)]
enum JsonPathToken {
    Field(String),
    Index(usize),
}

/// Minimal parse mirroring `data::parse_json_path` field/index subset.
fn parse_json_path(path: &str) -> Vec<JsonPathToken> {
    let bytes = path.as_bytes();
    let mut index = 1; // skip '$'
    let mut tokens = Vec::new();
    while index < bytes.len() {
        match bytes[index] {
            b'.' => {
                index += 1;
                let start = index;
                while index < bytes.len() && !matches!(bytes[index], b'.' | b'[') {
                    index += 1;
                }
                tokens.push(JsonPathToken::Field(path[start..index].to_string()));
            }
            b'[' => {
                index += 1;
                let start = index;
                while index < bytes.len() && bytes[index] != b']' {
                    index += 1;
                }
                let n: usize = path[start..index].parse().expect("index");
                tokens.push(JsonPathToken::Index(n));
                index += 1; // ']'
            }
            _ => panic!("unexpected path syntax"),
        }
    }
    tokens
}

fn extract_tokens<'a>(value: &'a Value, tokens: &[JsonPathToken]) -> Option<&'a Value> {
    let mut current = value;
    for token in tokens {
        match token {
            JsonPathToken::Field(field) => current = current.get(field)?,
            JsonPathToken::Index(i) => current = current.as_array()?.get(*i)?,
        }
    }
    Some(current)
}

fn bench_json_path_extract(c: &mut Criterion) {
    let mut group = c.benchmark_group("json_path_extract");
    group.throughput(Throughput::Elements(1));

    const PATH: &str = "$.user.roles[1]";
    let payload = json!({
        "user": {
            "id": 42,
            "roles": ["reader", "admin", "ops"],
            "meta": {"region": "us-east-1", "tier": "gold"}
        },
        "ok": true
    });

    group.bench_function("parse_each_time", |b| {
        b.iter(|| {
            let tokens = parse_json_path(black_box(PATH));
            let extracted = extract_tokens(black_box(&payload), &tokens).cloned();
            black_box(extracted)
        });
    });

    let cached = parse_json_path(PATH);
    group.bench_function("cached_tokens", |b| {
        b.iter(|| {
            let extracted = extract_tokens(black_box(&payload), black_box(&cached)).cloned();
            black_box(extracted)
        });
    });

    group.finish();
}

fn bench_fixture_row_bind(c: &mut Criterion) {
    let mut group = c.benchmark_group("fixture_row_bind");
    group.throughput(Throughput::Elements(1));

    let row = json!({
        "username": "alice",
        "token": "x".repeat(128),
        "profile": {
            "email": "alice@example.com",
            "tags": ["a", "b", "c", "d"],
            "prefs": {"theme": "dark", "notify": true}
        }
    });

    group.bench_function("clone_value", |b| {
        b.iter(|| {
            let bound = black_box(&row).clone();
            black_box(bound)
        });
    });

    let arc_row = Arc::new(row);
    group.bench_function("clone_arc", |b| {
        b.iter(|| {
            let bound = Arc::clone(black_box(&arc_row));
            black_box(bound)
        });
    });

    group.finish();
}

criterion_group!(benches, bench_json_path_extract, bench_fixture_row_bind);
criterion_main!(benches);
