use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dashmap::{DashMap, DashSet};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::Result;
use crate::http::{Body, Request, Response};
use crate::scenario::{ScenarioId, StepId};

/// Count compact JSON wire bytes without allocating the serialized `String`.
/// Matches `Value`'s `Display` / `to_string()` length used previously for size.
fn json_wire_len(value: &Value) -> u64 {
    struct Counter(u64);
    impl Write for Counter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0 += buf.len() as u64;
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    // `Value` serialization cannot fail for the standard map/list/primitive shapes.
    let _ = serde_json::to_writer(&mut counter, value);
    counter.0
}

/// Response body size in bytes for metrics (no extra copies for text/binary).
fn response_body_len(body: &Body) -> u64 {
    match body {
        Body::Empty => 0,
        Body::Text(text) => text.len() as u64,
        Body::Binary(bytes) => bytes.len() as u64,
        Body::Json(value) => json_wire_len(value),
    }
}

/// Metrics for a single HTTP request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestMetrics {
    /// Unique identifier for this request
    pub id: String,

    /// Step ID that generated this request
    pub step_id: StepId,

    /// Human-readable name of the step that generated this request
    pub step_name: String,

    /// Scenario ID that contains the step
    pub scenario_id: ScenarioId,

    /// Human-readable name of the scenario that contains the step
    pub scenario_name: String,

    /// Virtual user ID that executed the request
    pub virtual_user_id: u32,

    /// Timestamp when the request was started
    pub timestamp: DateTime<Utc>,

    /// Timestamp when the request completed.
    pub completed_at: DateTime<Utc>,

    /// HTTP method
    pub method: String,

    /// URL
    pub url: String,

    /// HTTP status code
    pub status_code: u16,

    /// Whether the request was successful
    pub success: bool,

    /// Response time in milliseconds
    pub response_time_ms: u64,

    /// Connection time in milliseconds
    pub connection_time_ms: Option<u64>,

    /// Time to first byte in milliseconds
    pub ttfb_ms: Option<u64>,

    /// Request size in bytes
    pub request_size_bytes: Option<u64>,

    /// Response size in bytes
    pub response_size_bytes: Option<u64>,

    /// Error message if the request failed
    pub error: Option<String>,

    /// Custom labels for this request
    pub labels: HashMap<String, String>,
}

impl RequestMetrics {
    /// Create new request metrics.
    ///
    /// `elapsed` is the wall-clock time the caller measured around the send
    /// attempt. It is used as the recorded `response_time_ms` for BOTH
    /// successful and failed attempts, so failures record their real latency
    /// instead of a misleading `0` (real elapsed on failure). Time-to-first-byte and the
    /// response size are read from the response when one is present.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: String,
        step_id: StepId,
        step_name: String,
        scenario_id: ScenarioId,
        scenario_name: String,
        virtual_user_id: u32,
        request: &Request,
        response: Option<&Response>,
        error: Option<String>,
        elapsed: Duration,
    ) -> Self {
        // Record latency in ms once; derive start time from the same integer so
        // timestamp math stays consistent with `response_time_ms` and avoids
        // `Duration::from_std` chrono conversion on every attempt.
        let response_time_ms = elapsed.as_millis() as u64;
        let completed_at = Utc::now();
        let timestamp = match i64::try_from(response_time_ms) {
            Ok(ms) => completed_at - chrono::Duration::milliseconds(ms),
            Err(_) => completed_at,
        };
        // `as_str()` uses the cached serialization / static method name.
        let method = request.method().as_str().to_string();

        let mut parsed_url = request.url().clone();
        if parsed_url.has_host() {
            let _ = parsed_url.set_password(None);
        }
        let url = parsed_url.as_str().to_string();

        let (status_code, success, ttfb_ms, response_size_bytes) = if let Some(resp) = response {
            (
                resp.status().as_u16(),
                // A response is only a success if the transport-level status is
                // successful AND no error (e.g. a custom validator rejection)
                // was recorded. Otherwise a 2xx that fails validation would
                // pollute the success-latency distribution (real elapsed on failure).
                resp.is_success() && error.is_none(),
                resp.ttfb().map(|d| d.as_millis() as u64),
                Some(response_body_len(resp.body())),
            )
        } else {
            (0, false, None, None)
        };

        Self {
            id,
            step_id,
            step_name,
            scenario_id,
            scenario_name,
            virtual_user_id,
            timestamp,
            completed_at,
            method,
            url,
            status_code,
            success,
            response_time_ms,
            connection_time_ms: None,
            ttfb_ms,
            request_size_bytes: None,
            response_size_bytes,
            error,
            labels: HashMap::new(),
        }
    }

    /// Add a label to the metrics
    pub fn with_label<K, V>(&mut self, key: K, value: V) -> &mut Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        self.labels.insert(key.into(), value.into());
        self
    }
}

/// Terminal status of a test run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum RunStatus {
    /// All scheduled work completed normally.
    Completed,
    /// The run was cut short but partial metrics are available.
    Truncated { reason: String },
    /// Runtime execution failed but partial metrics may be available.
    Failed { reason: String },
}

/// Metrics for a step in a scenario
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepMetrics {
    /// Step ID
    pub step_id: StepId,

    /// Step name
    pub name: String,

    /// Total number of requests
    pub total_requests: u64,

    /// Number of successful requests
    pub successful_requests: u64,

    /// Number of failed requests
    pub failed_requests: u64,

    /// Minimum response time in milliseconds
    pub min_response_time_ms: u64,

    /// Maximum response time in milliseconds
    pub max_response_time_ms: u64,

    /// Average response time in milliseconds
    pub avg_response_time_ms: f64,

    /// 50th percentile response time in milliseconds
    pub p50_response_time_ms: u64,

    /// 90th percentile response time in milliseconds
    pub p90_response_time_ms: u64,

    /// 95th percentile response time in milliseconds
    pub p95_response_time_ms: u64,

    /// 99th percentile response time in milliseconds
    pub p99_response_time_ms: u64,

    /// Requests per second
    pub requests_per_second: f64,

    /// Error rate (0.0 to 1.0)
    pub error_rate: f64,

    /// Start time
    pub start_time: DateTime<Utc>,

    /// End time
    pub end_time: DateTime<Utc>,

    /// Duration in seconds
    pub duration_seconds: f64,
}

/// Metrics for a scenario
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioMetrics {
    /// Scenario ID
    pub scenario_id: ScenarioId,

    /// Scenario name
    pub name: String,

    /// Metrics for each step
    pub steps: HashMap<StepId, StepMetrics>,

    /// Total number of requests
    pub total_requests: u64,

    /// Number of successful requests
    pub successful_requests: u64,

    /// Number of failed requests
    pub failed_requests: u64,

    /// Average response time in milliseconds
    pub avg_response_time_ms: f64,

    /// 90th percentile response time in milliseconds
    pub p90_response_time_ms: u64,

    /// Requests per second
    pub requests_per_second: f64,

    /// Error rate (0.0 to 1.0)
    pub error_rate: f64,

    /// Start time
    pub start_time: DateTime<Utc>,

    /// End time
    pub end_time: DateTime<Utc>,

    /// Duration in seconds
    pub duration_seconds: f64,

    /// Number of virtual users
    pub virtual_users: u32,
}

/// Overall test results
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResults {
    /// Run terminal status.
    pub status: RunStatus,

    /// Metrics for each scenario
    pub scenarios: HashMap<ScenarioId, ScenarioMetrics>,

    /// Total number of requests
    pub total_requests: u64,

    /// Number of successful requests
    pub successful_requests: u64,

    /// Number of failed requests
    pub failed_requests: u64,

    /// Average response time in milliseconds
    pub avg_response_time_ms: f64,

    /// 90th percentile response time in milliseconds
    pub p90_response_time_ms: u64,

    /// Requests per second
    pub requests_per_second: f64,

    /// Error rate (0.0 to 1.0)
    pub error_rate: f64,

    /// Start time
    pub start_time: DateTime<Utc>,

    /// End time
    pub end_time: DateTime<Utc>,

    /// Duration in seconds
    pub duration_seconds: f64,

    /// Total number of virtual users
    pub total_virtual_users: u32,
}

impl TestResults {
    /// Create a new empty test results
    pub fn new() -> Self {
        Self {
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
        }
    }
}

impl Default for TestResults {
    fn default() -> Self {
        Self::new()
    }
}

/// Trait for collecting metrics
#[async_trait]
pub trait MetricsCollector: Send + Sync {
    /// Record a request
    async fn record_request(&self, metrics: RequestMetrics) -> Result<()>;

    /// Get metrics for a step
    async fn get_step_metrics(&self, step_id: &StepId) -> Result<Option<StepMetrics>>;

    /// Get metrics for a scenario
    async fn get_scenario_metrics(
        &self,
        scenario_id: &ScenarioId,
    ) -> Result<Option<ScenarioMetrics>>;

    /// Get overall test results
    async fn get_test_results(&self) -> Result<TestResults>;

    /// Reset all metrics
    async fn reset(&self) -> Result<()>;

    /// Flush any pending metrics to ensure they are recorded.
    ///
    /// The default streaming collector aggregates synchronously at record time,
    /// so there is never anything pending; this is a deterministic no-op kept on
    /// the trait for collectors that buffer.
    async fn flush(&self) -> Result<()>;
}

// ---------------------------------------------------------------------------
// Bounded latency histogram
// ---------------------------------------------------------------------------

/// Number of sub-buckets per power-of-two octave. A quantile carries at most
/// `2^-SUB_BITS` relative error (~6% at 4 bits); exact counts/min/max/avg come
/// from atomics, not the histogram, so exact-value assertions stay exact.
const SUB_BITS: u32 = 4;
/// Sub-buckets per octave (`2^SUB_BITS`).
const SUB_COUNT: u64 = 1 << SUB_BITS;
/// Total number of buckets: enough to hold any `u64` millisecond value.
/// `bucket_index(u64::MAX) == 975`, so 976 buckets cover the full range.
const NUM_BUCKETS: usize = 976;

/// Map a latency (ms) to its histogram bucket.
///
/// Values below `SUB_COUNT` map 1:1 (exact). Larger values use a log-linear
/// mapping: `SUB_COUNT` sub-buckets within each power-of-two octave. The
/// mapping is monotonic non-decreasing in `v`, which the percentile walk
/// relies on.
fn bucket_index(v: u64) -> usize {
    if v < SUB_COUNT {
        return v as usize;
    }
    let exp = 63 - v.leading_zeros() as u64; // floor(log2(v)); >= SUB_BITS
    let sub = (v >> (exp - SUB_BITS as u64)) - SUB_COUNT; // 0..SUB_COUNT-1
    let octave = exp - SUB_BITS as u64; // >= 0
    let idx = SUB_COUNT + octave * SUB_COUNT + sub;
    (idx as usize).min(NUM_BUCKETS - 1)
}

/// Representative (midpoint) latency value for a bucket, used when reporting a
/// percentile drawn from the histogram.
fn bucket_representative(idx: usize) -> u64 {
    let idx = idx as u64;
    if idx < SUB_COUNT {
        return idx;
    }
    let octave = (idx - SUB_COUNT) / SUB_COUNT;
    let sub = (idx - SUB_COUNT) % SUB_COUNT;
    let exp = octave + SUB_BITS as u64;
    let shift = exp - SUB_BITS as u64;
    let lo = (SUB_COUNT + sub) << shift;
    let width = 1u64 << shift;
    lo + width / 2
}

/// 0-based nearest-rank index for percentile `p` over `n` samples.
///
/// Uses `ceil(n*p) - 1` so that, e.g., the median of two samples reports the
/// lower one and p90 of ten samples reports the 9th, not the max (fixes the
/// previous `(n*p) as usize` upward bias that was copy-pasted three times).
fn rank_index(n: u64, p: f64) -> u64 {
    ((n as f64 * p).ceil() as u64).max(1) - 1
}

/// Compute a quantile from a flat array of per-bucket counts (success latencies
/// only). `total` is the number of samples represented by `counts`.
fn quantile_from_counts(counts: &[u64], total: u64, p: f64) -> u64 {
    if total == 0 {
        return 0;
    }
    let target = rank_index(total, p);
    let mut cumulative = 0u64;
    for (i, &c) in counts.iter().enumerate() {
        if c == 0 {
            continue;
        }
        cumulative += c;
        if cumulative > target {
            return bucket_representative(i);
        }
    }
    0
}

/// Lock-free bounded histogram over success latencies (milliseconds).
struct LatencyHistogram {
    buckets: Vec<AtomicU64>,
}

impl LatencyHistogram {
    fn new() -> Self {
        let mut buckets = Vec::with_capacity(NUM_BUCKETS);
        for _ in 0..NUM_BUCKETS {
            buckets.push(AtomicU64::new(0));
        }
        Self { buckets }
    }

    fn record(&self, value_ms: u64) {
        self.buckets[bucket_index(value_ms)].fetch_add(1, Ordering::Relaxed);
    }

    /// Snapshot the current bucket counts into a freshly allocated array.
    fn snapshot(&self) -> Vec<u64> {
        self.buckets
            .iter()
            .map(|b| b.load(Ordering::Relaxed))
            .collect()
    }

    /// Add this histogram's counts into an accumulator (for scenario/global
    /// merges). `acc` must have `NUM_BUCKETS` elements.
    fn add_into(&self, acc: &mut [u64]) {
        for (a, b) in acc.iter_mut().zip(self.buckets.iter()) {
            *a += b.load(Ordering::Relaxed);
        }
    }
}

// ---------------------------------------------------------------------------
// Per-(scenario, step) streaming aggregate
// ---------------------------------------------------------------------------

/// Bounded, lock-free running aggregate for a single (scenario, step) key.
///
/// Success and failure latencies are kept SEPARATE: only successful requests
/// feed `success_*`/`hist`, so a single timeout can never drag the reported
/// min/avg/percentiles down or push min to 0 (real elapsed on failure). There is no per-request
/// retention, so nothing grows unboundedly and nothing needs reclaiming
/// (bounded aggregation, no per-request retention).
struct StepAggregate {
    total: AtomicU64,
    success: AtomicU64,
    fail: AtomicU64,
    success_sum_ms: AtomicU64,
    success_min_ms: AtomicU64,
    success_max_ms: AtomicU64,
    /// First request start and last request completion timestamps (chrono
    /// millis) for duration + rps.
    first_start_ts_ms: AtomicI64,
    last_completed_ts_ms: AtomicI64,
    /// Distinct virtual-user ids seen. Kept as an explicit (bounded) set rather
    /// than `max + 1` so sparse/non-zero ids are counted exactly; the number of
    /// distinct VUs is bounded by the configured virtual user count.
    vu_ids: DashSet<u32>,
    hist: LatencyHistogram,
    /// Human-readable step/scenario names, captured (write-once) from the first
    /// request recorded for this aggregate so the built metrics carry the real
    /// names rather than falling back to the ids.
    step_name: OnceLock<String>,
    scenario_name: OnceLock<String>,
}

impl StepAggregate {
    fn new() -> Self {
        Self {
            total: AtomicU64::new(0),
            success: AtomicU64::new(0),
            fail: AtomicU64::new(0),
            success_sum_ms: AtomicU64::new(0),
            success_min_ms: AtomicU64::new(u64::MAX),
            success_max_ms: AtomicU64::new(0),
            first_start_ts_ms: AtomicI64::new(i64::MAX),
            last_completed_ts_ms: AtomicI64::new(i64::MIN),
            vu_ids: DashSet::new(),
            hist: LatencyHistogram::new(),
            step_name: OnceLock::new(),
            scenario_name: OnceLock::new(),
        }
    }

    fn record(
        &self,
        success: bool,
        latency_ms: u64,
        started_ms: i64,
        completed_ms: i64,
        vu_id: u32,
    ) {
        self.total.fetch_add(1, Ordering::Relaxed);
        if success {
            self.success.fetch_add(1, Ordering::Relaxed);
            self.success_sum_ms.fetch_add(latency_ms, Ordering::Relaxed);
            self.success_min_ms.fetch_min(latency_ms, Ordering::Relaxed);
            self.success_max_ms.fetch_max(latency_ms, Ordering::Relaxed);
            self.hist.record(latency_ms);
        } else {
            self.fail.fetch_add(1, Ordering::Relaxed);
        }
        self.first_start_ts_ms
            .fetch_min(started_ms, Ordering::Relaxed);
        self.last_completed_ts_ms
            .fetch_max(completed_ms, Ordering::Relaxed);
        self.vu_ids.insert(vu_id);
    }
}

/// Convert a chrono-millis timestamp to a `DateTime<Utc>`, falling back to now
/// for the sentinel values used before any request is recorded.
fn ts_to_datetime(ms: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(ms).unwrap_or_else(Utc::now)
}

/// In-memory streaming metrics collector.
///
/// Stores a small, bounded running aggregate per (scenario, step) rather than
/// retaining every request, so memory is O(scenarios × steps) regardless of how
/// long the test runs. All record-time updates are lock-free atomics; reads are
/// O(steps × buckets) and never rescan a request log.
#[derive(Clone)]
pub struct InMemoryMetricsCollector {
    /// Running aggregates keyed by (scenario id, step id). Wrapped in `Arc` so
    /// all clones of the collector share the same data.
    steps: Arc<DashMap<(ScenarioId, StepId), Arc<StepAggregate>>>,
}

impl InMemoryMetricsCollector {
    /// Create a new in-memory metrics collector.
    ///
    /// Unlike the previous batched collector this does not spawn any background
    /// task, so it can be constructed outside a tokio runtime.
    pub fn new() -> Self {
        Self {
            steps: Arc::new(DashMap::new()),
        }
    }

    /// Build `StepMetrics` from a single aggregate. Success-only latency stats;
    /// counts include failures.
    fn build_step_metrics(step_id: &StepId, agg: &StepAggregate) -> StepMetrics {
        let total = agg.total.load(Ordering::Relaxed);
        let success = agg.success.load(Ordering::Relaxed);
        let fail = agg.fail.load(Ordering::Relaxed);
        let success_sum = agg.success_sum_ms.load(Ordering::Relaxed);

        let (min, max, avg) = if success > 0 {
            (
                agg.success_min_ms.load(Ordering::Relaxed),
                agg.success_max_ms.load(Ordering::Relaxed),
                success_sum as f64 / success as f64,
            )
        } else {
            (0, 0, 0.0)
        };

        let counts = agg.hist.snapshot();
        let p50 = quantile_from_counts(&counts, success, 0.50);
        let p90 = quantile_from_counts(&counts, success, 0.90);
        let p95 = quantile_from_counts(&counts, success, 0.95);
        let p99 = quantile_from_counts(&counts, success, 0.99);

        let start = ts_to_datetime(agg.first_start_ts_ms.load(Ordering::Relaxed));
        let end = ts_to_datetime(agg.last_completed_ts_ms.load(Ordering::Relaxed));
        let duration = (end - start).num_milliseconds() as f64 / 1000.0;
        let requests_per_second = if duration > 0.0 {
            total as f64 / duration
        } else {
            0.0
        };
        let error_rate = if total > 0 {
            fail as f64 / total as f64
        } else {
            0.0
        };

        StepMetrics {
            step_id: step_id.clone(),
            name: agg
                .step_name
                .get()
                .cloned()
                .unwrap_or_else(|| step_id.clone()),
            total_requests: total,
            successful_requests: success,
            failed_requests: fail,
            min_response_time_ms: min,
            max_response_time_ms: max,
            avg_response_time_ms: avg,
            p50_response_time_ms: p50,
            p90_response_time_ms: p90,
            p95_response_time_ms: p95,
            p99_response_time_ms: p99,
            requests_per_second,
            error_rate,
            start_time: start,
            end_time: end,
            duration_seconds: duration,
        }
    }

    /// Build `ScenarioMetrics` from all step aggregates belonging to a scenario.
    fn build_scenario_metrics(
        scenario_id: &ScenarioId,
        entries: &[(StepId, Arc<StepAggregate>)],
    ) -> Option<ScenarioMetrics> {
        if entries.is_empty() {
            return None;
        }

        // Recover the scenario's human-readable name from the first aggregate
        // that captured it, falling back to the id.
        let scenario_name = entries
            .iter()
            .find_map(|(_, agg)| agg.scenario_name.get().cloned())
            .unwrap_or_else(|| scenario_id.clone());

        let mut steps = HashMap::new();
        let mut total = 0u64;
        let mut success = 0u64;
        let mut fail = 0u64;
        let mut success_sum = 0u64;
        let mut merged = vec![0u64; NUM_BUCKETS];
        let mut first_ts = i64::MAX;
        let mut last_ts = i64::MIN;
        let mut vu_ids: std::collections::HashSet<u32> = std::collections::HashSet::new();

        for (step_id, agg) in entries {
            total += agg.total.load(Ordering::Relaxed);
            success += agg.success.load(Ordering::Relaxed);
            fail += agg.fail.load(Ordering::Relaxed);
            success_sum += agg.success_sum_ms.load(Ordering::Relaxed);
            agg.hist.add_into(&mut merged);
            first_ts = first_ts.min(agg.first_start_ts_ms.load(Ordering::Relaxed));
            last_ts = last_ts.max(agg.last_completed_ts_ms.load(Ordering::Relaxed));
            for id in agg.vu_ids.iter() {
                vu_ids.insert(*id);
            }

            steps.insert(step_id.clone(), Self::build_step_metrics(step_id, agg));
        }

        if total == 0 {
            return None;
        }

        let avg = if success > 0 {
            success_sum as f64 / success as f64
        } else {
            0.0
        };
        let p90 = quantile_from_counts(&merged, success, 0.90);

        let start = ts_to_datetime(first_ts);
        let end = ts_to_datetime(last_ts);
        let duration = (end - start).num_milliseconds() as f64 / 1000.0;
        let requests_per_second = if duration > 0.0 {
            total as f64 / duration
        } else {
            0.0
        };
        let error_rate = fail as f64 / total as f64;
        let virtual_users = vu_ids.len() as u32;

        Some(ScenarioMetrics {
            scenario_id: scenario_id.clone(),
            name: scenario_name,
            steps,
            total_requests: total,
            successful_requests: success,
            failed_requests: fail,
            avg_response_time_ms: avg,
            p90_response_time_ms: p90,
            requests_per_second,
            error_rate,
            start_time: start,
            end_time: end,
            duration_seconds: duration,
            virtual_users,
        })
    }

    /// Snapshot the aggregate map, grouped by scenario id.
    fn snapshot_by_scenario(&self) -> HashMap<ScenarioId, Vec<(StepId, Arc<StepAggregate>)>> {
        let mut by_scenario: HashMap<ScenarioId, Vec<(StepId, Arc<StepAggregate>)>> =
            HashMap::new();
        for entry in self.steps.iter() {
            let (scenario_id, step_id) = entry.key();
            by_scenario
                .entry(scenario_id.clone())
                .or_default()
                .push((step_id.clone(), entry.value().clone()));
        }
        by_scenario
    }
}

#[async_trait]
impl MetricsCollector for InMemoryMetricsCollector {
    async fn record_request(&self, metrics: RequestMetrics) -> Result<()> {
        // Read the primitive fields, then move the owned key strings into the
        // map key so nothing is cloned on the hot path.
        let success = metrics.success;
        let latency_ms = metrics.response_time_ms;
        let started_ms = metrics.timestamp.timestamp_millis();
        let completed_ms = metrics.completed_at.timestamp_millis();
        let vu_id = metrics.virtual_user_id;
        let step_name = metrics.step_name;
        let scenario_name = metrics.scenario_name;
        let key = (metrics.scenario_id, metrics.step_id);

        let agg = self
            .steps
            .entry(key)
            .or_insert_with(|| Arc::new(StepAggregate::new()))
            .clone();
        // Capture the human-readable names once, from the first request seen.
        agg.step_name.get_or_init(|| step_name);
        agg.scenario_name.get_or_init(|| scenario_name);
        agg.record(success, latency_ms, started_ms, completed_ms, vu_id);
        Ok(())
    }

    async fn get_step_metrics(&self, step_id: &StepId) -> Result<Option<StepMetrics>> {
        // A step id can in principle be reused across scenarios; the trait only
        // gives us the step id, so we resolve the first matching key (same
        // limitation as the previous implementation).
        for entry in self.steps.iter() {
            if entry.key().1 == *step_id {
                return Ok(Some(Self::build_step_metrics(step_id, entry.value())));
            }
        }
        Ok(None)
    }

    async fn get_scenario_metrics(
        &self,
        scenario_id: &ScenarioId,
    ) -> Result<Option<ScenarioMetrics>> {
        let entries: Vec<(StepId, Arc<StepAggregate>)> = self
            .steps
            .iter()
            .filter(|e| e.key().0 == *scenario_id)
            .map(|e| (e.key().1.clone(), e.value().clone()))
            .collect();
        Ok(Self::build_scenario_metrics(scenario_id, &entries))
    }

    async fn get_test_results(&self) -> Result<TestResults> {
        let by_scenario = self.snapshot_by_scenario();

        let mut scenarios = HashMap::new();
        let mut total = 0u64;
        let mut success = 0u64;
        let mut fail = 0u64;
        let mut success_sum = 0u64;
        let mut merged = vec![0u64; NUM_BUCKETS];
        let mut first_ts = i64::MAX;
        let mut last_ts = i64::MIN;
        let mut total_virtual_users = 0u32;

        for (scenario_id, entries) in by_scenario {
            // Fold into the global aggregate from the raw atomics.
            for (_step_id, agg) in &entries {
                total += agg.total.load(Ordering::Relaxed);
                success += agg.success.load(Ordering::Relaxed);
                fail += agg.fail.load(Ordering::Relaxed);
                success_sum += agg.success_sum_ms.load(Ordering::Relaxed);
                agg.hist.add_into(&mut merged);
                first_ts = first_ts.min(agg.first_start_ts_ms.load(Ordering::Relaxed));
                last_ts = last_ts.max(agg.last_completed_ts_ms.load(Ordering::Relaxed));
            }

            if let Some(scenario_metrics) = Self::build_scenario_metrics(&scenario_id, &entries) {
                total_virtual_users += scenario_metrics.virtual_users;
                scenarios.insert(scenario_id, scenario_metrics);
            }
        }

        let avg = if success > 0 {
            success_sum as f64 / success as f64
        } else {
            0.0
        };
        let p90 = quantile_from_counts(&merged, success, 0.90);
        let error_rate = if total > 0 {
            fail as f64 / total as f64
        } else {
            0.0
        };

        let (start, end) = if total > 0 {
            (ts_to_datetime(first_ts), ts_to_datetime(last_ts))
        } else {
            let now = Utc::now();
            (now, now)
        };
        let duration = (end - start).num_milliseconds() as f64 / 1000.0;
        let requests_per_second = if duration > 0.0 {
            total as f64 / duration
        } else {
            0.0
        };

        Ok(TestResults {
            status: RunStatus::Completed,
            scenarios,
            total_requests: total,
            successful_requests: success,
            failed_requests: fail,
            avg_response_time_ms: avg,
            p90_response_time_ms: p90,
            requests_per_second,
            error_rate,
            start_time: start,
            end_time: end,
            duration_seconds: duration,
            total_virtual_users,
        })
    }

    async fn reset(&self) -> Result<()> {
        self.steps.clear();
        Ok(())
    }

    async fn flush(&self) -> Result<()> {
        // Aggregation happens synchronously at record time, so there is never
        // anything pending to flush.
        Ok(())
    }
}

impl Default for InMemoryMetricsCollector {
    fn default() -> Self {
        Self::new()
    }
}

/// Metrics collector that records nothing.
///
/// Installed by the engine when `[metrics] enabled = false`, so a run performs
/// no aggregation and returns empty [`TestResults`]. Every method is a cheap
/// deterministic no-op, so it adds no per-request overhead on the hot path.
#[derive(Clone, Default)]
pub struct NoopMetricsCollector;

impl NoopMetricsCollector {
    /// Create a new no-op metrics collector.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl MetricsCollector for NoopMetricsCollector {
    async fn record_request(&self, _metrics: RequestMetrics) -> Result<()> {
        Ok(())
    }

    async fn get_step_metrics(&self, _step_id: &StepId) -> Result<Option<StepMetrics>> {
        Ok(None)
    }

    async fn get_scenario_metrics(
        &self,
        _scenario_id: &ScenarioId,
    ) -> Result<Option<ScenarioMetrics>> {
        Ok(None)
    }

    async fn get_test_results(&self) -> Result<TestResults> {
        Ok(TestResults::new())
    }

    async fn reset(&self) -> Result<()> {
        Ok(())
    }

    async fn flush(&self) -> Result<()> {
        Ok(())
    }
}

/// Factory for creating metrics collectors
pub struct MetricsCollectorFactory;

impl MetricsCollectorFactory {
    /// Create a new in-memory metrics collector
    pub fn create_in_memory() -> Arc<dyn MetricsCollector> {
        Arc::new(InMemoryMetricsCollector::new())
    }

    /// Create a new no-op metrics collector (records nothing).
    pub fn create_noop() -> Arc<dyn MetricsCollector> {
        Arc::new(NoopMetricsCollector::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{Request, Response};
    use reqwest::StatusCode;
    use std::time::Duration;

    #[test]
    fn test_bucket_mapping_is_monotonic_and_bounded() {
        // Boundary continuity between the linear and log-linear regions.
        assert_eq!(bucket_index(0), 0);
        assert_eq!(bucket_index(15), 15);
        assert_eq!(bucket_index(16), 16);
        assert_eq!(bucket_index(31), 31);
        assert_eq!(bucket_index(32), 32);
        // Never exceeds the fixed bucket count.
        assert!(bucket_index(u64::MAX) < NUM_BUCKETS);
        // Monotonic non-decreasing over a wide range.
        let mut prev = 0;
        for v in 0..10_000u64 {
            let idx = bucket_index(v);
            assert!(idx >= prev);
            prev = idx;
        }
    }

    #[test]
    fn test_rank_index_nearest_rank() {
        // Median of two samples must pick the lower (rank 0), not the upper.
        assert_eq!(rank_index(2, 0.5), 0);
        // p90 of ten samples is the 9th (rank 8), not the max (rank 9).
        assert_eq!(rank_index(10, 0.9), 8);
        assert_eq!(rank_index(1, 0.99), 0);
    }

    #[test]
    fn test_json_wire_len_matches_to_string() {
        let value = serde_json::json!({
            "ok": true,
            "n": 42,
            "items": ["a", "b"],
        });
        assert_eq!(json_wire_len(&value), value.to_string().len() as u64);
    }

    #[tokio::test]
    async fn test_record_and_retrieve_metrics() {
        let collector = InMemoryMetricsCollector::new();

        // Create a request and response
        let request = Request::get("https://example.com").build().unwrap();
        let response = Response::new(
            StatusCode::OK,
            reqwest::header::HeaderMap::new(),
            crate::http::Body::Empty,
            Duration::from_millis(100),
        );

        // Record a request
        let metrics = RequestMetrics::new(
            "req1".to_string(),
            "step1".to_string(),
            "Step 1".to_string(),
            "scenario1".to_string(),
            "Scenario 1".to_string(),
            1,
            &request,
            Some(&response),
            None,
            Duration::from_millis(100),
        );

        collector.record_request(metrics).await.unwrap();

        // Get step metrics
        let step_metrics = collector
            .get_step_metrics(&"step1".to_string())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(step_metrics.step_id, "step1");
        assert_eq!(step_metrics.total_requests, 1);
        assert_eq!(step_metrics.successful_requests, 1);
        assert_eq!(step_metrics.failed_requests, 0);
        assert_eq!(step_metrics.min_response_time_ms, 100);

        // Get scenario metrics
        let scenario_metrics = collector
            .get_scenario_metrics(&"scenario1".to_string())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(scenario_metrics.scenario_id, "scenario1");
        assert_eq!(scenario_metrics.total_requests, 1);
        assert_eq!(scenario_metrics.successful_requests, 1);
        assert_eq!(scenario_metrics.failed_requests, 0);

        // Get test results
        let results = collector.get_test_results().await.unwrap();

        assert_eq!(results.total_requests, 1);
        assert_eq!(results.successful_requests, 1);
        assert_eq!(results.failed_requests, 0);
        assert_eq!(results.total_virtual_users, 1);
    }

    #[tokio::test]
    async fn test_multiple_requests() {
        let collector = InMemoryMetricsCollector::new();

        // Create requests and responses
        let request1 = Request::get("https://localhost/1").build().unwrap();
        let response1 = Response::new(
            StatusCode::OK,
            reqwest::header::HeaderMap::new(),
            crate::http::Body::Empty,
            Duration::from_millis(100),
        );

        let request2 = Request::get("https://localhost/2").build().unwrap();
        let response2 = Response::new(
            StatusCode::BAD_REQUEST,
            reqwest::header::HeaderMap::new(),
            crate::http::Body::Empty,
            Duration::from_millis(200),
        );

        // Record requests
        let metrics1 = RequestMetrics::new(
            "req1".to_string(),
            "step1".to_string(),
            "Step 1".to_string(),
            "scenario1".to_string(),
            "Scenario 1".to_string(),
            1,
            &request1,
            Some(&response1),
            None,
            Duration::from_millis(100),
        );

        let metrics2 = RequestMetrics::new(
            "req2".to_string(),
            "step2".to_string(),
            "Step 2".to_string(),
            "scenario1".to_string(),
            "Scenario 1".to_string(),
            1,
            &request2,
            Some(&response2),
            Some("Bad request".to_string()),
            Duration::from_millis(200),
        );

        collector.record_request(metrics1).await.unwrap();
        collector.record_request(metrics2).await.unwrap();

        // Get test results
        let results = collector.get_test_results().await.unwrap();

        assert_eq!(results.total_requests, 2);
        assert_eq!(results.successful_requests, 1);
        assert_eq!(results.failed_requests, 1);
        assert_eq!(results.error_rate, 0.5);

        // Check scenario metrics
        let scenario = results.scenarios.get("scenario1").unwrap();
        assert_eq!(scenario.total_requests, 2);
        assert_eq!(scenario.successful_requests, 1);
        assert_eq!(scenario.failed_requests, 1);

        // Check step metrics
        let step1 = scenario.steps.get("step1").unwrap();
        assert_eq!(step1.total_requests, 1);
        assert_eq!(step1.successful_requests, 1);
        assert_eq!(step1.failed_requests, 0);

        let step2 = scenario.steps.get("step2").unwrap();
        assert_eq!(step2.total_requests, 1);
        assert_eq!(step2.successful_requests, 0);
        assert_eq!(step2.failed_requests, 1);
        // A failed request must not pollute the success latency distribution:
        // with no successes, min/avg/percentiles are 0.
        assert_eq!(step2.min_response_time_ms, 0);
        assert_eq!(step2.avg_response_time_ms, 0.0);
        assert_eq!(step2.p90_response_time_ms, 0);
    }

    #[tokio::test]
    async fn test_failure_does_not_corrupt_success_latency() {
        let collector = InMemoryMetricsCollector::new();
        let request = Request::get("https://example.com").build().unwrap();
        let ok = Response::new(
            StatusCode::OK,
            reqwest::header::HeaderMap::new(),
            crate::http::Body::Empty,
            Duration::from_millis(100),
        );

        // One 100ms success and one failed transport attempt recorded with 0ms
        // (no response). The success min must remain 100, not collapse to 0.
        collector
            .record_request(RequestMetrics::new(
                "ok".to_string(),
                "step1".to_string(),
                "Step 1".to_string(),
                "scenario1".to_string(),
                "Scenario 1".to_string(),
                0,
                &request,
                Some(&ok),
                None,
                Duration::from_millis(100),
            ))
            .await
            .unwrap();
        collector
            .record_request(RequestMetrics::new(
                "err".to_string(),
                "step1".to_string(),
                "Step 1".to_string(),
                "scenario1".to_string(),
                "Scenario 1".to_string(),
                0,
                &request,
                None,
                Some("connection refused".to_string()),
                Duration::from_millis(0),
            ))
            .await
            .unwrap();

        let step = collector
            .get_step_metrics(&"step1".to_string())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(step.total_requests, 2);
        assert_eq!(step.successful_requests, 1);
        assert_eq!(step.failed_requests, 1);
        assert_eq!(step.min_response_time_ms, 100);
        assert_eq!(step.avg_response_time_ms, 100.0);
        assert_eq!(step.error_rate, 0.5);
    }

    #[tokio::test]
    async fn test_validation_failure_on_2xx_counts_as_failure() {
        // A server returns HTTP 200 but a custom validator rejects the body,
        // so an `error` is recorded alongside a successful status. The attempt
        // must count as a failure and must not pollute the success latency
        // distribution (real elapsed on failure).
        let collector = InMemoryMetricsCollector::new();
        let request = Request::get("https://example.com").build().unwrap();
        let ok_body = Response::new(
            StatusCode::OK,
            reqwest::header::HeaderMap::new(),
            crate::http::Body::Empty,
            Duration::from_millis(100),
        );

        let metrics = RequestMetrics::new(
            "rejected".to_string(),
            "step1".to_string(),
            "Step 1".to_string(),
            "scenario1".to_string(),
            "Scenario 1".to_string(),
            0,
            &request,
            Some(&ok_body),
            Some("validation failed".to_string()),
            Duration::from_millis(500),
        );
        assert!(
            !metrics.success,
            "2xx that fails validation must not be a success"
        );
        collector.record_request(metrics).await.unwrap();

        let step = collector
            .get_step_metrics(&"step1".to_string())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(step.total_requests, 1);
        assert_eq!(step.successful_requests, 0);
        assert_eq!(step.failed_requests, 1);
        assert_eq!(step.error_rate, 1.0);
    }

    #[test]
    fn test_request_metrics_redacts_password() {
        let request = Request::get("https://user:secretpass@example.com/api?q=1")
            .build()
            .unwrap();
        let metrics = RequestMetrics::new(
            "req1".to_string(),
            "step1".to_string(),
            "Step 1".to_string(),
            "scenario1".to_string(),
            "Scenario 1".to_string(),
            1,
            &request,
            None,
            None,
            Duration::from_millis(100),
        );
        assert_eq!(metrics.url, "https://user@example.com/api?q=1");
    }

    #[tokio::test]
    async fn test_reset_metrics() {
        let collector = InMemoryMetricsCollector::new();

        // Create a request and response
        let request = Request::get("https://example.com").build().unwrap();
        let response = Response::new(
            StatusCode::OK,
            reqwest::header::HeaderMap::new(),
            crate::http::Body::Empty,
            Duration::from_millis(100),
        );

        // Record a request
        let metrics = RequestMetrics::new(
            "req1".to_string(),
            "step1".to_string(),
            "Step 1".to_string(),
            "scenario1".to_string(),
            "Scenario 1".to_string(),
            1,
            &request,
            Some(&response),
            None,
            Duration::from_millis(100),
        );

        collector.record_request(metrics).await.unwrap();

        // Reset metrics
        collector.reset().await.unwrap();

        // Get test results
        let results = collector.get_test_results().await.unwrap();

        assert_eq!(results.total_requests, 0);
        assert!(results.scenarios.is_empty());
    }
}
