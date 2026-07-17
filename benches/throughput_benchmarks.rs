//! Throughput-focused Criterion benches for hot-path changes:
//! - rate limiter contention under many concurrent waiters
//! - semaphore vs rate-limiter acquire ordering (permanent head-to-head)
//! - response body materialization (`to_vec` vs `bytes::Bytes`)
//!
//! Baseline workflow:
//! ```text
//! cargo bench --bench throughput_benchmarks -- --save-baseline before
//! # apply fixes, then either:
//! cargo bench --bench throughput_benchmarks -- --baseline before
//! # or save a named after baseline (cannot combine with --baseline):
//! cargo bench --bench throughput_benchmarks -- --save-baseline after
//! ```

use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use futures::future::join_all;
use pummel::engine::RateLimiter;
use pummel::http::Body;
use tokio::runtime::Builder;
use tokio::sync::Semaphore;
use tokio::task::yield_now;

fn bench_rate_limiter_contention(c: &mut Criterion) {
    let mut group = c.benchmark_group("rate_limiter_contention");
    // Keep wall time practical: sleep-under-lock baselines are slow at high task counts.
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(5));
    group.warm_up_time(Duration::from_secs(1));

    // High enough that interval is small; contention (not pacing math) dominates.
    const TARGET_RPS: f64 = 50_000.0;
    const ACQUIRES_PER_TASK: u64 = 64;

    let rt = Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("tokio runtime");

    for &tasks in &[32usize, 128, 512] {
        let total_acquires = tasks as u64 * ACQUIRES_PER_TASK;
        group.throughput(Throughput::Elements(total_acquires));
        group.bench_with_input(BenchmarkId::new("tasks", tasks), &tasks, |b, &tasks| {
            b.iter(|| {
                rt.block_on(async {
                    let limiter = RateLimiter::new(TARGET_RPS).expect("valid rate");
                    let completed = Arc::new(AtomicU64::new(0));
                    let mut handles = Vec::with_capacity(tasks);
                    for _ in 0..tasks {
                        let limiter = Arc::clone(&limiter);
                        let completed = Arc::clone(&completed);
                        handles.push(tokio::spawn(async move {
                            for _ in 0..ACQUIRES_PER_TASK {
                                let ok = limiter.acquire_before_deadline(None).await;
                                assert!(ok, "acquire without deadline must succeed");
                                tokio::task::yield_now().await;
                                completed.fetch_add(1, Ordering::Relaxed);
                            }
                        }));
                    }
                    join_all(handles).await;
                    completed.load(Ordering::Relaxed)
                })
            });
        });
    }

    group.finish();
}

/// Head-to-head: hold the in-flight semaphore during rate wait vs rate-then-sem.
///
/// Both arms stay permanently so the comparison does not depend on baselines.
fn bench_semaphore_rate_ordering(c: &mut Criterion) {
    let mut group = c.benchmark_group("semaphore_rate_ordering");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(5));
    group.warm_up_time(Duration::from_secs(1));

    const TARGET_RPS: f64 = 50_000.0;
    const TASKS: usize = 128;
    const MAX_IN_FLIGHT: usize = 8;

    let rt = Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("tokio runtime");

    group.throughput(Throughput::Elements(TASKS as u64));

    group.bench_function("sem_then_rate", |b| {
        b.iter(|| {
            rt.block_on(async {
                let limiter = RateLimiter::new(TARGET_RPS).expect("valid rate");
                let semaphore = Arc::new(Semaphore::new(MAX_IN_FLIGHT));
                let mut handles = Vec::with_capacity(TASKS);
                for _ in 0..TASKS {
                    let limiter = Arc::clone(&limiter);
                    let semaphore = Arc::clone(&semaphore);
                    handles.push(tokio::spawn(async move {
                        // Old engine order: concurrency permit held across rate sleep.
                        let _permit = semaphore.acquire().await.expect("sem");
                        let ok = limiter.acquire_before_deadline(None).await;
                        yield_now().await;
                        ok
                    }));
                }
                let mut granted = 0u64;
                for handle in handles {
                    if handle.await.expect("join") {
                        granted += 1;
                    }
                }
                black_box(granted)
            })
        });
    });

    group.bench_function("rate_then_sem", |b| {
        b.iter(|| {
            rt.block_on(async {
                let limiter = RateLimiter::new(TARGET_RPS).expect("valid rate");
                let semaphore = Arc::new(Semaphore::new(MAX_IN_FLIGHT));
                let mut handles = Vec::with_capacity(TASKS);
                for _ in 0..TASKS {
                    let limiter = Arc::clone(&limiter);
                    let semaphore = Arc::clone(&semaphore);
                    handles.push(tokio::spawn(async move {
                        // Fixed order: pace first, then take an in-flight slot.
                        let ok = limiter.acquire_before_deadline(None).await;
                        let _permit = semaphore.acquire().await.expect("sem");
                        yield_now().await;
                        ok
                    }));
                }
                let mut granted = 0u64;
                for handle in handles {
                    if handle.await.expect("join") {
                        granted += 1;
                    }
                }
                black_box(granted)
            })
        });
    });

    group.finish();
}

fn bench_response_body_materialize(c: &mut Criterion) {
    let mut group = c.benchmark_group("response_body_materialize");
    group.sample_size(40);
    group.measurement_time(Duration::from_secs(3));
    group.warm_up_time(Duration::from_secs(1));

    for &size in &[1024usize, 64 * 1024, 1024 * 1024] {
        let payload = Bytes::from(vec![0u8; size]);
        group.throughput(Throughput::Bytes(size as u64));

        // Old DefaultHttpClient path: Bytes -> Vec -> Body::Binary, then clone.
        group.bench_with_input(BenchmarkId::new("to_vec", size), &payload, |b, payload| {
            b.iter(|| {
                let body = Body::Binary(payload.to_vec().into());
                let cloned = body.clone();
                let len = match (&body, &cloned) {
                    (Body::Binary(a), Body::Binary(b)) => a.len() + b.len(),
                    _ => 0,
                };
                black_box(len)
            });
        });

        // New path: store Bytes directly (cheap clone via refcount).
        group.bench_with_input(BenchmarkId::new("bytes", size), &payload, |b, payload| {
            b.iter(|| {
                let body = Body::Binary(payload.clone());
                let cloned = body.clone();
                let len = match (&body, &cloned) {
                    (Body::Binary(a), Body::Binary(b)) => a.len() + b.len(),
                    _ => 0,
                };
                black_box(len)
            });
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_rate_limiter_contention,
    bench_semaphore_rate_ordering,
    bench_response_body_materialize,
);
criterion_main!(benches);
