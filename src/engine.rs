use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rand::RngExt;
use tokio::sync::{Mutex, Semaphore};
use tokio::time::sleep;
use uuid::Uuid;

use crate::config::Config;
use crate::data::{
    LoadedDataSources, extract_json_path, extract_json_path_tokens, extract_relative_json_path,
};
use crate::error::{Error, Result};
use crate::graph::{DefaultGraphVisualizer, GraphFormat, GraphVisualizer};
use crate::http::{HttpClient, HttpClientFactory};
use crate::logging;
use crate::metrics::{
    MetricsCollector, MetricsCollectorFactory, RequestMetrics, RunStatus, TestResults,
};
use crate::scenario::{
    BranchOperator, DynamicBodyTemplate, DynamicRequestSpec, Extractor, ExtractorSource, LoadStage,
    Scenario, ScenarioId, Step, StepId, VuContext,
};
use crate::telemetry::{BoundedTelemetryExporter, TelemetryExporter};

// Type aliases for complex function pointer types
type HttpClientFactoryFn = Arc<dyn Fn() -> Result<Arc<dyn HttpClient>> + Send + Sync>;
type MetricsCollectorFactoryFn = Arc<dyn Fn() -> Arc<dyn MetricsCollector> + Send + Sync>;

/// Shared async request-attempt start-rate limiter.
///
/// Exposed for throughput benchmarks (`benches/throughput_benchmarks.rs`).
/// Not part of the stable public API.
#[derive(Debug)]
#[doc(hidden)]
pub struct RateLimiter {
    interval: Duration,
    next_start: Mutex<Instant>,
}

impl RateLimiter {
    /// Create a limiter that paces starts at `rate_per_second`, or `None` if the rate is invalid.
    #[doc(hidden)]
    pub fn new(rate_per_second: f64) -> Option<Arc<Self>> {
        if !rate_per_second.is_finite() || rate_per_second <= 0.0 {
            return None;
        }
        Some(Arc::new(Self {
            interval: Duration::from_secs_f64(1.0 / rate_per_second),
            next_start: Mutex::new(Instant::now()),
        }))
    }

    /// Reserve the next start slot, waiting until it is due (or until `deadline`).
    ///
    /// Returns `false` if the deadline is already reached or the reserved slot is at/after it.
    ///
    /// The mutex is only held while reserving the slot; sleep happens after the
    /// guard is dropped so concurrent virtual users are not serialized behind
    /// another task's pacing wait.
    #[doc(hidden)]
    pub async fn acquire_before_deadline(&self, deadline: Option<Instant>) -> bool {
        let sleep_for = {
            let mut next = self.next_start.lock().await;
            let now = Instant::now();
            if deadline.is_some_and(|deadline| now >= deadline) {
                return false;
            }
            let start_at = (*next).max(now);
            if deadline.is_some_and(|deadline| start_at >= deadline) {
                return false;
            }
            *next = start_at + self.interval;
            start_at.saturating_duration_since(now)
        };
        if !sleep_for.is_zero() {
            sleep(sleep_for).await;
            if deadline_expired(deadline) {
                return false;
            }
        }
        true
    }
}

fn deadline_expired(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|deadline| Instant::now() >= deadline)
}

async fn sleep_before_deadline(delay: Duration, deadline: Option<Instant>) -> bool {
    let Some(deadline) = deadline else {
        sleep(delay).await;
        return true;
    };
    let now = Instant::now();
    if now >= deadline {
        return false;
    }
    if now + delay >= deadline {
        sleep(deadline - now).await;
        return false;
    }
    sleep(delay).await;
    !deadline_expired(Some(deadline))
}

/// Options for executing a scenario.
///
/// This struct is `#[non_exhaustive]`: construct it with
/// [`ExecutionOptions::default`] (then mutate fields) or
/// [`ExecutionOptions::builder`] rather than a struct literal, so future fields
/// can be added without a semver break.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ExecutionOptions {
    /// Number of virtual users to simulate
    pub virtual_users: u32,

    /// Wall-clock duration of sustained load. Each virtual user repeatedly
    /// executes the whole scenario graph until this much steady-state time has
    /// elapsed (measured after its ramp-up delay), so the tool offers real
    /// sustained load for `duration`, not a single pass. `0` means run the
    /// scenario graph exactly once per VU (single-pass mode).
    pub duration: Duration,

    /// Ramp-up period (time to gradually stagger VU start times). This is
    /// additional to `duration`: a VU's steady-state deadline is
    /// `start + ramp_up + duration`, so every VU gets a full `duration` of load
    /// regardless of when its ramp slot begins.
    pub ramp_up: Duration,

    /// Think time between scenario iterations (simulates a user pausing between
    /// sessions). In closed-loop mode (the default, when `target_rps` is
    /// `None`) a VU sleeps this long between passes; it is never slept past the
    /// run deadline.
    pub think_time: Duration,

    /// Maximum number of simultaneously in-flight requests across ALL virtual
    /// users of a scenario. A permit is acquired around each individual send
    /// (including retries) and released immediately after, so this caps
    /// concurrent requests — NOT virtual-user concurrency or lifetime. `0`
    /// means unlimited, in which case the virtual-user count is the sole
    /// concurrency bound (the default).
    pub max_concurrent_requests: usize,

    /// Whether to abort the test on error
    pub abort_on_error: bool,

    /// Build a separate HTTP client (and connection pool / TLS cache / DNS
    /// resolver) per virtual user instead of sharing one client across all
    /// VUs of a scenario. Off by default: sharing a single internally-`Arc`'d
    /// client maximizes connection reuse. Enable only when per-user isolation
    /// (e.g. distinct cookie/session state) is actually required.
    pub isolate_clients_per_user: bool,

    /// Optional open-loop target arrival rate in requests/second. `None`
    /// (default) uses closed-loop pacing driven by `think_time`.
    pub target_rps: Option<f64>,
}

impl Default for ExecutionOptions {
    fn default() -> Self {
        Self {
            virtual_users: 1,
            duration: Duration::from_secs(60),
            ramp_up: Duration::from_secs(0),
            think_time: Duration::from_millis(0),
            max_concurrent_requests: 0,
            abort_on_error: false,
            isolate_clients_per_user: false,
            target_rps: None,
        }
    }
}

impl ExecutionOptions {
    /// Start building [`ExecutionOptions`] with a fluent builder.
    ///
    /// Because the struct is `#[non_exhaustive]`, external crates cannot use a
    /// struct literal; the builder (or [`ExecutionOptions::default`] plus field
    /// mutation) is the supported construction path.
    pub fn builder() -> ExecutionOptionsBuilder {
        ExecutionOptionsBuilder::default()
    }
}

/// Fluent builder for [`ExecutionOptions`].
///
/// Every unset field falls back to [`ExecutionOptions::default`]. Obtain one via
/// [`ExecutionOptions::builder`].
#[derive(Debug, Clone, Default)]
pub struct ExecutionOptionsBuilder {
    options: ExecutionOptions,
}

impl ExecutionOptionsBuilder {
    /// Set the number of virtual users to simulate.
    pub fn virtual_users(mut self, virtual_users: u32) -> Self {
        self.options.virtual_users = virtual_users;
        self
    }

    /// Set the sustained-load duration (`0` => single pass per VU).
    pub fn duration(mut self, duration: Duration) -> Self {
        self.options.duration = duration;
        self
    }

    /// Set the ramp-up period.
    pub fn ramp_up(mut self, ramp_up: Duration) -> Self {
        self.options.ramp_up = ramp_up;
        self
    }

    /// Set the think time between scenario iterations.
    pub fn think_time(mut self, think_time: Duration) -> Self {
        self.options.think_time = think_time;
        self
    }

    /// Set the maximum number of simultaneously in-flight requests (`0` =>
    /// unlimited).
    pub fn max_concurrent_requests(mut self, max_concurrent_requests: usize) -> Self {
        self.options.max_concurrent_requests = max_concurrent_requests;
        self
    }

    /// Set whether to abort the whole run on the first error.
    pub fn abort_on_error(mut self, abort_on_error: bool) -> Self {
        self.options.abort_on_error = abort_on_error;
        self
    }

    /// Set whether each virtual user builds its own isolated HTTP client.
    pub fn isolate_clients_per_user(mut self, isolate_clients_per_user: bool) -> Self {
        self.options.isolate_clients_per_user = isolate_clients_per_user;
        self
    }

    /// Set the optional open-loop target arrival rate (requests/second).
    pub fn target_rps(mut self, target_rps: Option<f64>) -> Self {
        self.options.target_rps = target_rps;
        self
    }

    /// Finish building and return the [`ExecutionOptions`].
    pub fn build(self) -> ExecutionOptions {
        self.options
    }
}

/// Status of a step execution
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StepStatus {
    /// Step is waiting for dependencies to complete
    Waiting,
    /// Step is ready to be executed
    Ready,
    /// Step is currently executing
    Executing,
    /// Step has completed successfully
    Completed,
    /// Step was skipped by a branch condition
    Skipped,
    /// Step has failed
    Failed(Cow<'static, str>),
}

/// Terminal result of executing one step (all attempts) in a pass.
///
/// Produced by the static `execute_step` (which never borrows `self`) so a whole
/// ready-set can run concurrently; the driving pass then applies the status
/// update from each outcome after the batch resolves.
struct StepOutcome {
    /// Id of the step this outcome is for.
    step_id: StepId,
    /// Whether the step ultimately succeeded (a validated response was received).
    success: bool,
    /// Whether the step was skipped by a false branch condition.
    skipped: bool,
    /// Whether execution stopped before sending because the run deadline was reached.
    truncated: bool,
    /// The error to surface when `abort_on_error` is set. `None` on success.
    error: Option<Error>,
}

impl StepOutcome {
    fn truncated(step_id: StepId) -> Self {
        Self {
            step_id,
            success: false,
            skipped: false,
            truncated: true,
            error: None,
        }
    }
}

/// Status of a virtual user
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum VirtualUserStatus {
    /// Virtual user is waiting to start
    Waiting,
    /// Virtual user is active
    Active,
    /// Virtual user has completed
    Completed,
    /// Virtual user has failed
    Failed(String),
}

fn value_to_template_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn resolve_template_expr(ctx: &VuContext, step_id: &str, expr: &str) -> Result<String> {
    let expr = expr.trim();
    match expr {
        "vu.id" => Ok(ctx.id.to_string()),
        "scenario.id" => Ok(ctx.scenario_id.clone()),
        "step.id" => Ok(step_id.to_string()),
        "iteration" => Ok(ctx.iteration.to_string()),
        "uuid" => Ok(Uuid::new_v4().to_string()),
        "random.u64" => Ok(rand::rng().random::<u64>().to_string()),
        _ if expr.starts_with("random.int:") => {
            let mut parts = expr.split(':');
            let _ = parts.next();
            let min = parts
                .next()
                .ok_or_else(|| Error::config("random.int requires min"))?
                .parse::<i64>()
                .map_err(|e| Error::config(format!("invalid random.int min: {e}")))?;
            let max = parts
                .next()
                .ok_or_else(|| Error::config("random.int requires max"))?
                .parse::<i64>()
                .map_err(|e| Error::config(format!("invalid random.int max: {e}")))?;
            if min > max {
                return Err(Error::config("random.int min must be <= max"));
            }
            Ok(rand::rng().random_range(min..=max).to_string())
        }
        _ if expr.starts_with("data.") => {
            let (source_id, path) = parse_data_expr(expr)?;
            let row = ctx.get_data(source_id).ok_or_else(|| {
                Error::validation(format!(
                    "Missing data source row '{source_id}' for template expression '{expr}'"
                ))
            })?;
            let value = extract_relative_json_path(row, path).ok_or_else(|| {
                Error::validation(format!(
                    "Missing data path '{path}' in source '{source_id}' for template expression '{expr}'"
                ))
            })?;
            Ok(value_to_template_string(&value))
        }
        _ => ctx
            .get_var(expr)
            .map(value_to_template_string)
            .ok_or_else(|| Error::validation(format!("Missing template variable '{expr}'"))),
    }
}

fn parse_data_expr(expr: &str) -> Result<(&str, &str)> {
    let rest = expr
        .strip_prefix("data.")
        .ok_or_else(|| Error::config("data expression must start with 'data.'"))?;
    rest.split_once('.').ok_or_else(|| {
        Error::config(format!(
            "data expression '{expr}' must use data.<source>.<path>"
        ))
    })
}

fn render_template(ctx: &VuContext, step_id: &str, template: &str) -> Result<String> {
    let mut rendered = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        rendered.push_str(&rest[..start]);
        let after_start = &rest[start + 2..];
        let end = after_start
            .find("}}")
            .ok_or_else(|| Error::config("Unclosed template expression"))?;
        let expr = after_start[..end].trim();
        if expr.is_empty() {
            return Err(Error::config("Empty template expression"));
        }
        rendered.push_str(&resolve_template_expr(ctx, step_id, expr)?);
        rest = &after_start[end + 2..];
    }
    if rest.contains("}}") {
        return Err(Error::config("Unopened template expression"));
    }
    rendered.push_str(rest);
    Ok(rendered)
}

fn branch_matches(
    ctx: &VuContext,
    step_id: &str,
    branch: &crate::scenario::BranchCondition,
) -> bool {
    let value = resolve_branch_value(ctx, step_id, &branch.variable);
    match branch.operator {
        BranchOperator::Exists => value.is_some(),
        BranchOperator::Equals => match (value, &branch.value) {
            (Some(value), Some(expected)) => value_to_template_string(&value) == *expected,
            _ => false,
        },
        BranchOperator::NotEquals => match (value, &branch.value) {
            (Some(value), Some(expected)) => value_to_template_string(&value) != *expected,
            _ => false,
        },
        BranchOperator::GreaterThan => {
            numeric_branch_cmp(value.as_ref(), &branch.value, |a, b| a > b)
        }
        BranchOperator::GreaterThanOrEqual => {
            numeric_branch_cmp(value.as_ref(), &branch.value, |a, b| a >= b)
        }
        BranchOperator::LessThan => numeric_branch_cmp(value.as_ref(), &branch.value, |a, b| a < b),
        BranchOperator::LessThanOrEqual => {
            numeric_branch_cmp(value.as_ref(), &branch.value, |a, b| a <= b)
        }
        BranchOperator::MatchesRegex => match value {
            Some(value) => {
                let haystack = value_to_template_string(&value);
                if let Some(regex) = branch.compiled_regex.as_ref() {
                    regex.is_match(&haystack)
                } else if let Some(pattern) = &branch.value {
                    regex::Regex::new(pattern)
                        .map(|regex| regex.is_match(&haystack))
                        .unwrap_or(false)
                } else {
                    false
                }
            }
            None => false,
        },
    }
}

fn resolve_branch_value(
    ctx: &VuContext,
    step_id: &str,
    variable: &str,
) -> Option<serde_json::Value> {
    match variable {
        "vu.id" => Some(serde_json::Value::Number(ctx.id.into())),
        "scenario.id" => Some(serde_json::Value::String(ctx.scenario_id.clone())),
        "step.id" => Some(serde_json::Value::String(step_id.to_string())),
        "iteration" => Some(serde_json::Value::Number(ctx.iteration.into())),
        "uuid" => Some(serde_json::Value::String(Uuid::new_v4().to_string())),
        "random.u64" => Some(serde_json::Value::String(
            rand::rng().random::<u64>().to_string(),
        )),
        _ if variable.starts_with("random.int:") => resolve_template_expr(ctx, step_id, variable)
            .ok()
            .map(serde_json::Value::String),
        _ if variable.starts_with("data.") => {
            let (source_id, path) = parse_data_expr(variable).ok()?;
            let row = ctx.get_data(source_id)?;
            extract_relative_json_path(row, path)
        }
        _ => ctx.get_var(variable).cloned(),
    }
}

fn numeric_branch_cmp(
    actual: Option<&serde_json::Value>,
    expected: &Option<String>,
    cmp: impl FnOnce(f64, f64) -> bool,
) -> bool {
    let Some(actual) = actual.and_then(value_as_f64) else {
        return false;
    };
    let Some(expected) = expected
        .as_ref()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite())
    else {
        return false;
    };
    cmp(actual, expected)
}

fn value_as_f64(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::Number(number) => number.as_f64().filter(|value| value.is_finite()),
        serde_json::Value::String(value) => {
            value.parse::<f64>().ok().filter(|value| value.is_finite())
        }
        _ => None,
    }
}

fn render_request(
    step: &Step,
    spec: &DynamicRequestSpec,
    ctx: &VuContext,
) -> Result<crate::http::Request> {
    let url = render_template(ctx, &step.id, &spec.url_template)?;
    let mut builder = crate::http::Request::request(spec.method.clone(), url)
        .timeout(step.timeout)
        .follow_redirects(spec.follow_redirects);

    for (key, value) in &spec.header_templates {
        builder = builder.header(key, render_template(ctx, &step.id, value)?);
    }

    if let Some(body) = &spec.body_template {
        match body {
            DynamicBodyTemplate::Text(template) => {
                builder = builder.text(render_template(ctx, &step.id, template)?);
            }
            DynamicBodyTemplate::Json(template) => {
                let rendered = render_template(ctx, &step.id, template)?;
                // We just need to validate it is a valid JSON before sending it as raw_json.
                // We use IgnoredAny to skip building a full Value tree.
                serde_json::from_str::<serde::de::IgnoredAny>(&rendered).map_err(|e| {
                    Error::validation(format!(
                        "Rendered JSON body for step '{}' is invalid: {e}",
                        step.id
                    ))
                })?;
                builder = builder.raw_json(rendered);
            }
        }
    }

    builder.build()
}

fn response_body_text(response: &crate::http::Response) -> Result<String> {
    response.text()
}

fn regex_capture(regex: &regex::Regex, haystack: &str) -> Option<String> {
    regex.captures(haystack).map(|captures| {
        captures
            .get(1)
            .or_else(|| captures.get(0))
            .map(|m| m.as_str().to_string())
            .unwrap_or_default()
    })
}

fn run_extractor_regex(extractor: &Extractor, haystack: &str) -> Result<Option<String>> {
    if let Some(regex) = extractor.compiled_regex.as_ref() {
        return Ok(regex_capture(regex, haystack));
    }
    let regex = regex::Regex::new(&extractor.selector)
        .map_err(|e| Error::validation(format!("invalid extractor regex: {e}")))?;
    Ok(regex_capture(&regex, haystack))
}

fn run_extractor(
    extractor: &Extractor,
    response: &crate::http::Response,
) -> Result<Option<serde_json::Value>> {
    match extractor.source {
        ExtractorSource::JsonPath => {
            let value: serde_json::Value = response.json()?;
            if let Some(tokens) = extractor.compiled_json_path.as_deref() {
                Ok(extract_json_path_tokens(&value, tokens))
            } else {
                Ok(extract_json_path(&value, &extractor.selector))
            }
        }
        ExtractorSource::BodyRegex => {
            let body = response_body_text(response)?;
            Ok(run_extractor_regex(extractor, &body)?.map(serde_json::Value::String))
        }
        ExtractorSource::Header => {
            let value = response
                .headers()
                .get(&extractor.selector)
                .and_then(|value| value.to_str().ok())
                .map(|value| serde_json::Value::String(value.to_string()));
            Ok(value)
        }
        ExtractorSource::HeaderRegex => {
            let header = extractor.header.as_deref().unwrap_or_default();
            let header_value = response
                .headers()
                .get(header)
                .and_then(|value| value.to_str().ok());
            match header_value {
                Some(value) => {
                    Ok(run_extractor_regex(extractor, value)?.map(serde_json::Value::String))
                }
                None => Ok(None),
            }
        }
        ExtractorSource::Status => Ok(Some(serde_json::Value::Number(serde_json::Number::from(
            response.status().as_u16(),
        )))),
    }
}

fn apply_stage_options(options: &ExecutionOptions, stage: &LoadStage) -> ExecutionOptions {
    let mut stage_options = options.clone();
    if let Some(virtual_users) = stage.virtual_users {
        stage_options.virtual_users = virtual_users;
    }
    stage_options.duration = Duration::from_secs(stage.duration_seconds);
    if let Some(ramp_up_seconds) = stage.ramp_up_seconds {
        stage_options.ramp_up = Duration::from_secs(ramp_up_seconds);
    }
    if let Some(think_time_ms) = stage.think_time_ms {
        stage_options.think_time = Duration::from_millis(think_time_ms);
    }
    if options.target_rps.is_none() {
        stage_options.target_rps = stage.target_rps;
    }
    stage_options
}

fn combine_run_status(current: RunStatus, next: RunStatus) -> RunStatus {
    match (current, next) {
        (RunStatus::Failed { reason }, _) => RunStatus::Failed { reason },
        (_, RunStatus::Failed { reason }) => RunStatus::Failed { reason },
        (RunStatus::Truncated { reason }, _) => RunStatus::Truncated { reason },
        (_, RunStatus::Truncated { reason }) => RunStatus::Truncated { reason },
        (RunStatus::Completed, RunStatus::Completed) => RunStatus::Completed,
    }
}

/// Context for a virtual user
struct VirtualUserContext {
    /// Virtual user ID
    id: u32,

    /// Scenario being executed
    scenario: Arc<Scenario>,

    /// HTTP client
    http_client: Arc<dyn HttpClient>,

    /// Metrics collector
    metrics_collector: Arc<dyn MetricsCollector>,

    /// Optional telemetry exporter. When present, one `export_request` callback
    /// is fired per send attempt (cloned Arc, like `metrics_collector`); `None`
    /// skips the callback entirely to keep it off the hot path.
    telemetry_exporter: Option<Arc<dyn TelemetryExporter>>,

    /// Status of each step
    step_statuses: HashMap<StepId, StepStatus>,

    /// Reused buffer for ready-set StepIds within a DAG pass (avoids fresh Vec
    /// capacity churn across ready-waves).
    ready_buf: Vec<StepId>,

    /// Execution options
    options: ExecutionOptions,

    /// Optional in-flight-request cap shared across all VUs of the scenario. A
    /// permit is acquired around each individual send (see `execute_step`) and
    /// released immediately, so it caps concurrent requests rather than VU
    /// lifetime. `None` means unlimited.
    semaphore: Option<Arc<Semaphore>>,

    /// Optional request-attempt start-rate limiter shared across all VUs in the
    /// active scenario/stage.
    rate_limiter: Option<Arc<RateLimiter>>,

    /// Per-VU dynamic scenario state.
    vu_context: Arc<Mutex<VuContext>>,

    /// Loaded data sources shared by all VUs for this scenario run.
    data_sources: Arc<LoadedDataSources>,

    /// End time
    end_time: Option<Instant>,

    /// Status
    status: VirtualUserStatus,
}

impl VirtualUserContext {
    /// Create a new virtual user context
    #[allow(clippy::too_many_arguments)]
    fn new(
        id: u32,
        scenario: Arc<Scenario>,
        http_client: Arc<dyn HttpClient>,
        metrics_collector: Arc<dyn MetricsCollector>,
        telemetry_exporter: Option<Arc<dyn TelemetryExporter>>,
        options: ExecutionOptions,
        semaphore: Option<Arc<Semaphore>>,
        rate_limiter: Option<Arc<RateLimiter>>,
        data_sources: Arc<LoadedDataSources>,
    ) -> Self {
        let mut step_statuses = HashMap::new();

        // Initialize all steps as waiting
        for step in scenario.get_steps() {
            step_statuses.insert(step.id.clone(), StepStatus::Waiting);
        }

        // Mark steps with no dependencies as ready
        for step in scenario.get_root_steps() {
            step_statuses.insert(step.id.clone(), StepStatus::Ready);
        }

        Self {
            id,
            scenario: scenario.clone(),
            http_client,
            metrics_collector,
            telemetry_exporter,
            step_statuses,
            ready_buf: Vec::new(),
            options,
            semaphore,
            rate_limiter,
            vu_context: Arc::new(Mutex::new(VuContext::new(id, scenario.id.clone()))),
            data_sources,
            end_time: None,
            status: VirtualUserStatus::Waiting,
        }
    }

    /// Re-initialize all step statuses for a fresh DAG pass: every step back to
    /// `Waiting`, then root (dependency-free) steps to `Ready`.
    ///
    /// The sustained-load loop executes the scenario graph once per
    /// iteration, so each iteration must start from a clean status map;
    /// otherwise steps left `Completed`/`Failed` by the previous pass would
    /// never run again.
    fn reset_statuses(&mut self) {
        for status in self.step_statuses.values_mut() {
            *status = StepStatus::Waiting;
        }
        for step_id in &self.scenario.root_step_ids {
            if let Some(status) = self.step_statuses.get_mut(step_id) {
                *status = StepStatus::Ready;
            }
        }
    }

    /// Collect the ids of every step that is currently `Ready`, in launch order.
    ///
    /// All returned steps are mutually independent by construction: a dependent
    /// step only becomes `Ready` after its dependency reaches `Completed` (which
    /// happens in a later pass iteration), so the whole set can be executed
    /// concurrently without violating any dependency.
    ///
    /// Order is deterministic: higher `weight` first (launch priority among
    /// simultaneously-ready steps — weight does NOT change how often a step
    /// runs), ties broken by step id. Ids (cheap `String` clones) are returned
    /// rather than `&Step` so the caller can drop the `self` borrow before the
    /// concurrent sends.
    fn take_ready_steps(&mut self) -> Vec<StepId> {
        self.ready_buf.clear();

        // Walk statuses (typically fewer Ready entries than total steps in deep
        // DAGs) instead of scanning every scenario step.
        for (step_id, status) in &self.step_statuses {
            if matches!(status, StepStatus::Ready) {
                self.ready_buf.push(step_id.clone());
            }
        }

        self.ready_buf.sort_unstable_by(|a, b| {
            let wa = self
                .scenario
                .steps
                .get(a)
                .map(|s| s.weight)
                .unwrap_or_default();
            let wb = self
                .scenario
                .steps
                .get(b)
                .map(|s| s.weight)
                .unwrap_or_default();
            wb.cmp(&wa).then_with(|| a.cmp(b))
        });

        std::mem::take(&mut self.ready_buf)
    }

    /// Check if all dependencies of a step are completed
    fn are_dependencies_completed(&self, step: &Step) -> bool {
        for dep_id in &step.dependencies {
            if let Some(status) = self.step_statuses.get(dep_id) {
                if !matches!(status, StepStatus::Completed | StepStatus::Skipped) {
                    return false;
                }
            } else {
                return false;
            }
        }

        true
    }

    /// Update step statuses after a step completes
    fn update_step_statuses(&mut self, step_id: &StepId, success: bool, skipped: bool) {
        // Update the status of the completed step
        if let Some(status) = self.step_statuses.get_mut(step_id) {
            if skipped {
                *status = StepStatus::Skipped;
            } else if success {
                *status = StepStatus::Completed;
            } else {
                *status = StepStatus::Failed(Cow::Borrowed("Step execution failed"));
            }
        }

        // Promote waiting dependents via reverse adjacency (O(out-degree)).
        let Some(dependent_ids) = self.scenario.dependents.get(step_id) else {
            return;
        };
        // Clone ids so we can mutably borrow `self.step_statuses` / `self.scenario`.
        let dependent_ids = dependent_ids.clone();

        for dep_step_id in dependent_ids {
            let Some(step) = self.scenario.steps.get(&dep_step_id) else {
                continue;
            };
            if !matches!(
                self.step_statuses.get(&dep_step_id),
                Some(StepStatus::Waiting)
            ) {
                continue;
            }
            if self.are_dependencies_completed(step)
                && let Some(status) = self.step_statuses.get_mut(&dep_step_id)
            {
                *status = StepStatus::Ready;
            }
        }
    }

    /// Execute a single step to completion (retries included) WITHOUT borrowing
    /// `self`, returning a terminal outcome.
    ///
    /// This is a static associated fn (not `&mut self`) so that a whole ready-set
    /// of independent steps can be launched concurrently in one pass
    /// (concurrent independent ready steps): the caller clones the shared `Arc`s and `Copy` scalars it
    /// needs and hands them in, keeping `&mut self` off the concurrent sends. The
    /// step's status is left untouched here; the caller applies the terminal
    /// status update (from the returned `StepOutcome`) after the whole batch
    /// resolves, so dependents are promoted only once their dependency is
    /// actually `Completed`.
    ///
    /// Per-attempt recording and real-elapsed-on-failure are
    /// preserved: one `RequestMetrics` is buffered per send attempt and flushed
    /// to the collector before returning. A record error is logged rather than
    /// propagated so the step still becomes terminal in the caller's status pass
    /// (the Tier-1 no-strand guarantee: every launched step yields exactly one
    /// outcome).
    #[allow(clippy::too_many_arguments)]
    async fn execute_step(
        http_client: Arc<dyn HttpClient>,
        metrics_collector: Arc<dyn MetricsCollector>,
        telemetry_exporter: Option<Arc<dyn TelemetryExporter>>,
        scenario: Arc<Scenario>,
        step_id: StepId,
        vu_id: u32,
        semaphore: Option<Arc<Semaphore>>,
        rate_limiter: Option<Arc<RateLimiter>>,
        vu_context: Arc<Mutex<VuContext>>,
        deadline: Option<Instant>,
    ) -> StepOutcome {
        // Re-borrow the step from the shared scenario. A missing step is a
        // terminal failure rather than a panic so the batch still completes.
        let step = match scenario.get_step(&step_id) {
            Some(step) => step,
            None => {
                return StepOutcome {
                    success: false,
                    skipped: false,
                    truncated: false,
                    error: Some(Error::other(format!(
                        "Step '{step_id}' not found in scenario"
                    ))),
                    step_id,
                };
            }
        };

        if deadline_expired(deadline) {
            return StepOutcome::truncated(step_id);
        }

        if let Some(branch) = &step.branch {
            let ctx = vu_context.lock().await;
            if !branch_matches(&ctx, &step.id, branch) {
                return StepOutcome {
                    step_id,
                    success: true,
                    skipped: true,
                    truncated: false,
                    error: None,
                };
            }
        }

        // Base request ID; each attempt gets a unique `-{attempt}` suffix so
        // every send is a distinct metrics record.
        let request_id = Uuid::new_v4().to_string();

        // Execute the request with retries using exponential backoff. We record
        // ONE metrics record per send attempt: a step that fails twice
        // then succeeds sent three real requests and must report three, so that
        // throughput, error rate, and target load reflect actual traffic. The
        // records are buffered here (bounded by `max_retries + 1`) and flushed
        // to the collector after the step status is made terminal.
        let mut attempts: Vec<RequestMetrics> = Vec::new();
        let mut last_error = None;
        let mut response = None;
        let mut truncated = false;
        let base_delay = step.retry_delay;
        let max_delay = Duration::from_secs(30); // Maximum delay of 30 seconds

        for attempt in 0..=step.max_retries {
            if attempt > 0 {
                // Calculate exponential backoff with jitter
                let shift = (attempt - 1).min(63);
                let exp_factor = 1u128 << shift;
                let delay_millis = base_delay.as_millis().saturating_mul(exp_factor);
                let max_delay_millis = max_delay.as_millis();
                let capped_delay_millis = delay_millis.min(max_delay_millis);

                // Add jitter (random variation) to prevent thundering herd problem
                let jitter_factor = rand::rng().random_range(0.8..1.2);
                let jittered_delay_millis = ((capped_delay_millis as f64 * jitter_factor) as u128)
                    .min(u64::MAX as u128) as u64;

                // Wait before retrying, but never past the shared run deadline.
                if !sleep_before_deadline(Duration::from_millis(jittered_delay_millis), deadline)
                    .await
                {
                    truncated = true;
                    break;
                }

                // Log retry attempt
                logging::info!(
                    "Retrying step '{}' (attempt {}/{})",
                    step.id,
                    attempt,
                    step.max_retries
                );
            }

            if deadline_expired(deadline) {
                truncated = true;
                break;
            }

            let request = if let Some(spec) = &step.dynamic_request {
                let ctx = vu_context.lock().await;
                match render_request(step, spec, &ctx) {
                    Ok(request) => request,
                    Err(err) => {
                        last_error = Some(err);
                        break;
                    }
                }
            } else {
                step.request.clone()
            };

            // Send the request, measuring wall-clock elapsed for this attempt so
            // that a failure (which returns `Err` with no timing) still records
            // its real latency instead of a misleading 0.
            // Acquire an in-flight-request permit (if a cap is configured)
            // around THIS individual send only, so the semaphore bounds
            // concurrent requests rather than VU lifetime. The permit
            // is dropped as soon as the send returns.
            //
            // Enforce the step's own timeout per attempt: wrap the
            // send in `tokio::time::timeout(step.timeout, ...)`. This is the
            // outer per-attempt cap the public builder configures; it coexists
            // with reqwest's separate inner request timeout. A timeout is
            // treated as a failed attempt (with real elapsed) and falls through
            // to the retry/backoff logic like any other failure.
            let (elapsed, send_result) = {
                // Pace before taking an in-flight permit so concurrency slots
                // are not held across rate-limiter sleeps.
                let rate_permit_acquired = match &rate_limiter {
                    Some(rate_limiter) => rate_limiter.acquire_before_deadline(deadline).await,
                    None => true,
                };
                if !rate_permit_acquired {
                    truncated = true;
                    break;
                }
                if deadline_expired(deadline) {
                    truncated = true;
                    break;
                }
                let _permit = match &semaphore {
                    Some(sem) => Some(sem.acquire().await.unwrap()),
                    None => None,
                };
                // Time only the send itself, starting AFTER the in-flight-request
                // and rate permits are acquired, so queue wait does not inflate
                // recorded latency or let it exceed step.timeout.
                let attempt_start = Instant::now();
                let result = tokio::time::timeout(step.timeout, http_client.send(&request)).await;
                (attempt_start.elapsed(), result)
            };
            let attempt_id = format!("{request_id}-{attempt}");

            match send_result {
                Ok(Ok(resp)) => {
                    let validated = step.validate(&resp);
                    let mut error = if validated {
                        None
                    } else {
                        Some(format!("Response validation failed for step '{}'", step.id))
                    };
                    if validated {
                        let mut extracted = Vec::new();
                        for extractor in &step.extractors {
                            match run_extractor(extractor, &resp) {
                                Ok(Some(value)) => {
                                    extracted.push((extractor.name.clone(), value));
                                }
                                Ok(None) if !extractor.required => {}
                                Ok(None) => {
                                    error = Some(format!(
                                        "Required extractor '{}' did not match for step '{}'",
                                        extractor.name, step.id
                                    ));
                                    break;
                                }
                                Err(err) => {
                                    error = Some(err.to_string());
                                    break;
                                }
                            }
                        }
                        if error.is_none() && !extracted.is_empty() {
                            let mut ctx = vu_context.lock().await;
                            for (name, value) in extracted {
                                ctx.insert_var(name, value);
                            }
                        }
                    }
                    let attempt_success = validated && error.is_none();
                    // Record this attempt (response present so size/ttfb/status
                    // are captured) before moving `resp` into `response`.
                    attempts.push(RequestMetrics::new(
                        attempt_id,
                        step.id.clone(),
                        step.name.clone(),
                        scenario.id.clone(),
                        scenario.name.clone(),
                        vu_id,
                        &request,
                        Some(&resp),
                        error.clone(),
                        elapsed,
                    ));

                    if attempt_success {
                        response = Some(resp);
                        break;
                    } else {
                        // A response arrived but failed the step's validator:
                        // surface it as a structured Validation error so an
                        // embedder can tell it apart from a transport failure.
                        last_error = Some(Error::validation(error.unwrap_or_else(|| {
                            format!("Response validation failed for step '{}'", step.id)
                        })));
                    }
                }
                Ok(Err(err)) => {
                    // Transport error: no response, but real elapsed is recorded.
                    attempts.push(RequestMetrics::new(
                        attempt_id,
                        step.id.clone(),
                        step.name.clone(),
                        scenario.id.clone(),
                        scenario.name.clone(),
                        vu_id,
                        &request,
                        None,
                        Some(err.to_string()),
                        elapsed,
                    ));
                    last_error = Some(err);
                }
                Err(_elapsed) => {
                    // Step timeout fired. Record a failed attempt
                    // with real elapsed (~= step.timeout) and status 0, then let
                    // the retry loop treat it like any other failure.
                    let msg = format!("step '{}' timed out after {:?}", step.id, step.timeout);
                    attempts.push(RequestMetrics::new(
                        attempt_id,
                        step.id.clone(),
                        step.name.clone(),
                        scenario.id.clone(),
                        scenario.name.clone(),
                        vu_id,
                        &request,
                        None,
                        Some(msg.clone()),
                        elapsed,
                    ));
                    last_error = Some(Error::timeout(msg));
                }
            }
        }

        // Flush one record per attempt (moved into the collector, no clones). A
        // record error is logged rather than propagated: the caller applies the
        // terminal status update from the returned outcome regardless, so a
        // failed record can never strand the step (Tier-1 no-strand guarantee).
        let success = response.is_some();
        for metrics in attempts {
            // Fire the telemetry callback (if an exporter is attached) BEFORE
            // moving the record into the collector. Log-and-continue on error,
            // mirroring the record_request no-strand guarantee.
            if let Some(exporter) = &telemetry_exporter
                && let Err(err) = exporter.export_request(&metrics).await
            {
                logging::error!("Failed to export telemetry for step '{step_id}': {err}");
            }
            if let Err(err) = metrics_collector.record_request(metrics).await {
                logging::error!("Failed to record metrics for step '{step_id}': {err}");
            }
        }

        let error = if success || truncated {
            None
        } else {
            Some(
                last_error
                    .unwrap_or_else(|| Error::other(format!("Failed to execute step '{step_id}'"))),
            )
        };

        StepOutcome {
            step_id,
            success,
            skipped: false,
            truncated,
            error,
        }
    }

    /// Execute the scenario graph exactly once (one DAG pass).
    ///
    /// Each iteration executes the ENTIRE current ready-set CONCURRENTLY, so
    /// independent DAG branches overlap instead of running one-at-a-time — the
    /// dependency graph now has real throughput meaning. The pass is
    /// a 3-phase cycle that never holds `&mut self` across the sends:
    ///
    /// * PHASE 1 (`&mut self`, no await): collect the ready-set (mutually
    ///   independent by construction), stop if it is empty (all terminal, or the
    ///   Tier-1 terminal break when a failed dependency leaves nothing ready),
    ///   and mark each `Ready -> Executing`.
    /// * PHASE 2 (owned, awaits, no `self` borrow): launch one `execute_step`
    ///   future per ready id over cloned `Arc`s and join them all.
    /// * PHASE 3 (`&mut self`): apply each outcome's terminal status update
    ///   (promoting now-ready dependents) AFTER the whole batch resolves, so a
    ///   dependent is only promoted once its dependency is actually `Completed`;
    ///   then, if `abort_on_error`, surface the first failure.
    ///
    /// `deadline` bounds a single long pass so it cannot overrun the shared run
    /// window: before dispatching each ready-set we check the clock and stop if
    /// the deadline has passed (deadline check before dispatch — a 100-step chain under a 1s deadline
    /// self-terminates instead of running to completion).
    async fn run_one_pass(&mut self, deadline: Option<Instant>) -> Result<bool> {
        loop {
            // Intra-pass deadline check: keep a long sequential chain from
            // running past the shared run window.
            if let Some(dl) = deadline
                && Instant::now() >= dl
            {
                return Ok(true);
            }

            // PHASE 1: collect the ready-set. Empty => every step is terminal, or
            // the remaining steps are blocked by a failed dependency (the Tier-1
            // terminal break) — either way this pass is done.
            let ready = self.take_ready_steps();
            let ready_cap = ready.len();
            if ready.is_empty() {
                break;
            }

            // Mark the whole ready-set Executing before launching so it is not
            // re-collected; the terminal status is applied in PHASE 3.
            for step_id in &ready {
                if let Some(status) = self.step_statuses.get_mut(step_id) {
                    *status = StepStatus::Executing;
                }
            }

            // PHASE 2: run every ready step concurrently over cloned Arcs, so no
            // `self` borrow is held across the awaits. Collect the futures first
            // to drop the borrow used to clone, then join.
            let futures: Vec<_> = ready
                .into_iter()
                .map(|step_id| {
                    Self::execute_step(
                        self.http_client.clone(),
                        self.metrics_collector.clone(),
                        self.telemetry_exporter.clone(),
                        self.scenario.clone(),
                        step_id,
                        self.id,
                        self.semaphore.clone(),
                        self.rate_limiter.clone(),
                        self.vu_context.clone(),
                        deadline,
                    )
                })
                .collect();
            let outcomes = futures::future::join_all(futures).await;

            // Preserve ready-buffer capacity across waves in this pass.
            if self.ready_buf.capacity() < ready_cap {
                self.ready_buf.reserve(ready_cap);
            }

            // PHASE 3: apply terminal status updates for the whole batch, then
            // (if configured) abort on the first failure. Updates are applied
            // after the batch so dependents are promoted only once their
            // dependency is Completed.
            let mut abort_err = None;
            let mut truncated = false;
            for outcome in outcomes {
                if outcome.truncated {
                    truncated = true;
                    continue;
                }
                self.update_step_statuses(&outcome.step_id, outcome.success, outcome.skipped);
                if !outcome.success && self.options.abort_on_error && abort_err.is_none() {
                    abort_err = outcome.error;
                }
            }
            if truncated {
                return Ok(true);
            }
            if let Some(err) = abort_err {
                self.status = VirtualUserStatus::Failed(err.to_string());
                return Err(err);
            }
        }

        Ok(false)
    }

    /// Run the virtual user as a sustained-load generator.
    ///
    /// Instead of executing the scenario exactly once, the VU repeats the whole
    /// DAG pass until the shared `deadline`, so the configured `duration`
    /// produces real sustained wall-clock load rather than a single sub-second
    /// burst.
    ///
    /// `deadline` semantics:
    /// * `Some(t)` — sustained mode: keep iterating until `now >= t`. The
    ///   deadline is `start + ramp_up + duration` (computed once in
    ///   `run_scenario`), so a VU that begins at its ramp slot still gets a full
    ///   `duration` of steady work after ramp-up.
    /// * `None` — single-pass mode (`duration == 0`): execute exactly one pass
    ///   and return. This preserves the exact per-attempt / per-VU request
    ///   counts the Tier-1/2A verification tests assert on.
    ///
    /// Pacing between iterations (never past the deadline):
    /// * closed-loop (default) — sleep `think_time` between passes;
    /// * open-loop (`target_rps`) — schedule iteration *start* times at a fixed
    ///   cadence decoupled from response latency, mitigating coordinated
    ///   omission.
    async fn run(&mut self, deadline: Option<Instant>) -> Result<bool> {
        self.status = VirtualUserStatus::Active;

        let mut iteration = 0u64;
        let mut truncated = false;

        loop {
            // (1) Fresh DAG pass.
            self.reset_statuses();
            let data_rows = self.data_sources.bind_iteration(self.id, iteration)?;
            {
                let mut ctx = self.vu_context.lock().await;
                ctx.iteration = iteration;
                ctx.set_data_rows(data_rows);
            }

            // (2) Execute one pass.
            match self.run_one_pass(deadline).await {
                Ok(true) => {
                    truncated = true;
                    break;
                }
                Ok(false) => {}
                Err(err) => {
                    self.end_time = Some(Instant::now());
                    self.status = VirtualUserStatus::Failed(err.to_string());
                    return Err(err);
                }
            }

            // A pass over an instantly-resolving client (e.g. an in-process
            // mock) may perform no await; yield once per iteration so this VU
            // cannot starve co-located tasks or timers on the same worker
            // until the deadline.
            tokio::task::yield_now().await;

            // (3) Single-pass mode: exactly one iteration (keeps exact-count
            //     verification tests green).
            let deadline = match deadline {
                Some(dl) => dl,
                None => break,
            };

            // (4) Deadline reached => stop generating load.
            let now = Instant::now();
            if now >= deadline {
                break;
            }

            // (5) Pace before the next iteration, never sleeping past the
            //     deadline.
            if self.options.think_time.as_millis() > 0 {
                // Closed-loop: think between sessions, but not past the deadline.
                if now + self.options.think_time >= deadline {
                    break;
                }
                sleep(self.options.think_time).await;
            }
            iteration = iteration.saturating_add(1);
        }

        self.end_time = Some(Instant::now());
        self.status = VirtualUserStatus::Completed;

        Ok(truncated)
    }
}

/// Engine for executing load tests
#[derive(Default)]
pub struct Engine {
    /// Scenarios to execute
    scenarios: HashMap<ScenarioId, Arc<Scenario>>,

    /// HTTP client factory
    http_client_factory: Option<HttpClientFactoryFn>,

    /// Metrics collector factory
    metrics_collector_factory: Option<MetricsCollectorFactoryFn>,

    /// Graph visualizer
    graph_visualizer: Option<Box<dyn GraphVisualizer>>,

    /// Telemetry exporter
    telemetry_exporter: Option<Arc<dyn TelemetryExporter>>,
}

impl Engine {
    /// Create a new engine
    pub fn new() -> Self {
        Self {
            scenarios: HashMap::new(),
            http_client_factory: None,
            metrics_collector_factory: None,
            graph_visualizer: None,
            telemetry_exporter: None,
        }
    }

    /// Add a scenario to the engine
    pub fn add_scenario(&mut self, scenario: Scenario) -> &mut Self {
        self.scenarios
            .insert(scenario.id.clone(), Arc::new(scenario));
        self
    }

    /// Set the HTTP client factory
    pub fn with_http_client_factory<F>(&mut self, factory: F) -> &mut Self
    where
        F: Fn() -> Result<Arc<dyn HttpClient>> + Send + Sync + 'static,
    {
        self.http_client_factory = Some(Arc::new(factory));
        self
    }

    /// Set the metrics collector factory
    pub fn with_metrics_collector_factory<F>(&mut self, factory: F) -> &mut Self
    where
        F: Fn() -> Arc<dyn MetricsCollector> + Send + Sync + 'static,
    {
        self.metrics_collector_factory = Some(Arc::new(factory));
        self
    }

    /// Set the graph visualizer
    pub fn with_graph_visualizer(&mut self, visualizer: Box<dyn GraphVisualizer>) -> &mut Self {
        self.graph_visualizer = Some(visualizer);
        self
    }

    /// Set the telemetry exporter
    pub fn with_telemetry_exporter(&mut self, exporter: Arc<dyn TelemetryExporter>) -> &mut Self {
        self.telemetry_exporter = Some(Arc::new(BoundedTelemetryExporter::default_drop(exporter)));
        self
    }

    /// Apply fallible config-derived runtime wiring.
    pub fn apply_config(&mut self, config: &Config) -> Result<()> {
        config.validate()?;

        if self.http_client_factory.is_none() {
            let spec = crate::http::ClientSpec::try_from_config(&config.http, &config.global)?;
            self.with_http_client_factory(move || {
                crate::http::HttpClientFactory::from_spec(spec.clone())
            });
        }

        if !config.metrics.enabled && self.metrics_collector_factory.is_none() {
            self.with_metrics_collector_factory(MetricsCollectorFactory::create_noop);
        }

        if config.telemetry.enabled && self.telemetry_exporter.is_none() {
            let spec = crate::telemetry::ExporterConfig::from(&config.telemetry);
            let exporter = crate::telemetry::TelemetryExporterFactory::create(&spec)?;
            self.telemetry_exporter = Some(Arc::new(BoundedTelemetryExporter::new(
                exporter,
                spec.backpressure,
                spec.queue_capacity,
            )));
        }

        Ok(())
    }

    /// Get the HTTP client factory
    fn get_http_client_factory(&self) -> HttpClientFactoryFn {
        match &self.http_client_factory {
            Some(factory) => factory.clone(),
            None => Arc::new(HttpClientFactory::create),
        }
    }

    /// Get the metrics collector factory
    fn get_metrics_collector_factory(&self) -> MetricsCollectorFactoryFn {
        match &self.metrics_collector_factory {
            Some(factory) => factory.clone(),
            None => Arc::new(MetricsCollectorFactory::create_in_memory),
        }
    }

    /// Run a scenario
    async fn run_scenario(
        &self,
        scenario: Arc<Scenario>,
        options: ExecutionOptions,
        metrics_collector: Option<Arc<dyn MetricsCollector>>,
    ) -> Result<RunStatus> {
        // Create a metrics collector or use the provided one
        let metrics_collector =
            metrics_collector.unwrap_or_else(|| (self.get_metrics_collector_factory())());

        // In-flight-request cap: a per-send permit shared across all
        // VUs, NOT a per-VU-lifetime gate. `0` => unlimited (no semaphore), so
        // by default the virtual-user count is the sole concurrency bound and
        // VU concurrency is never throttled by a connection-pool knob.
        let request_semaphore: Option<Arc<Semaphore>> = if options.max_concurrent_requests > 0 {
            Some(Arc::new(Semaphore::new(options.max_concurrent_requests)))
        } else {
            None
        };
        let rate_limiter = options.target_rps.and_then(RateLimiter::new);

        let loaded_data_sources = if scenario.data_sources.is_empty() {
            Arc::new(LoadedDataSources::default())
        } else {
            let base_dir = match &scenario.data_source_base_dir {
                Some(base_dir) => base_dir.clone(),
                None => std::env::current_dir().map_err(|e| {
                    Error::config(format!("Failed to resolve current directory: {e}"))
                })?,
            };
            LoadedDataSources::load(&scenario.data_sources, &base_dir)?
        };

        // Build ONE shared HTTP client for the whole scenario:
        // the factory is invoked exactly once here and the internally-`Arc`'d
        // client is cloned into every VU task, so all VUs share a single
        // connection pool / TLS cache / DNS resolver. Per-user isolation is
        // opt-in via `isolate_clients_per_user`, in which case each task builds
        // its own client. Surfacing a factory error once here is cleaner than
        // one failure per task.
        let shared_client: Option<Arc<dyn HttpClient>> = if options.isolate_clients_per_user {
            None
        } else {
            Some((self.get_http_client_factory())()?)
        };

        // One shared clock for the whole scenario. Every VU is bounded by the
        // same steady-state deadline so early VUs self-terminate together with
        // the last-ramped VU (sustained load with aligned deadlines).
        //
        // deadline = start + ramp_up + duration (NOT start + duration): the
        // per-VU ramp sleep runs BEFORE any work, so adding ramp_up here gives a
        // VU that starts at its ramp slot a full `duration` of steady load
        // instead of losing the ramp period from its budget.
        //
        // `duration == 0` => `None` => single-pass mode (each VU runs the graph
        // exactly once), which the exact-count verification tests rely on.
        let start = Instant::now();
        let deadline: Option<Instant> = if options.duration.as_millis() > 0 {
            Some(start + options.ramp_up + options.duration)
        } else {
            None
        };

        // Create virtual users
        let mut handles = Vec::new();

        // Clone the shared telemetry exporter Arc once per VU (like the metrics
        // collector) so every VU fires the same exporter's callbacks.
        let telemetry_exporter = self.telemetry_exporter.clone();

        for i in 0..options.virtual_users {
            let scenario_clone = scenario.clone();
            let metrics_collector_clone = metrics_collector.clone();
            let telemetry_exporter_clone = telemetry_exporter.clone();
            let options_clone = options.clone();
            let request_semaphore_clone = request_semaphore.clone();
            let rate_limiter_clone = rate_limiter.clone();
            let data_sources_clone = loaded_data_sources.clone();
            let shared_client_clone = shared_client.clone();
            let http_client_factory = self.get_http_client_factory();

            // Calculate delay for this user based on ramp-up period
            let delay = if options.ramp_up.as_millis() > 0 && options.virtual_users > 1 {
                let user_delay = options.ramp_up.as_millis() / (options.virtual_users as u128 - 1);
                Duration::from_millis((user_delay * i as u128) as u64)
            } else {
                Duration::from_millis(0)
            };

            // Spawn a task for this virtual user
            let handle = tokio::spawn(async move {
                // Wait for ramp-up delay. This runs at task start BEFORE any
                // work and is no longer followed by a lifetime-permit wait, so
                // a VU starts at its computed ramp slot (the old semaphore made
                // a delayed VU start long after its slot).
                if delay.as_millis() > 0 {
                    sleep(delay).await;
                }

                // Use the shared client unless per-user isolation was requested,
                // in which case build a dedicated client for this VU.
                let http_client = match shared_client_clone {
                    Some(client) => client,
                    None => match http_client_factory() {
                        Ok(client) => client,
                        Err(err) => return Err(err),
                    },
                };

                // Create a virtual user context
                let mut context = VirtualUserContext::new(
                    i,
                    scenario_clone,
                    http_client,
                    metrics_collector_clone,
                    telemetry_exporter_clone,
                    options_clone,
                    request_semaphore_clone,
                    rate_limiter_clone,
                    data_sources_clone,
                );

                context.run(deadline).await
            });

            handles.push(handle);
        }

        // Safety timeout backstop. Primary bounding is now the in-loop deadline
        // check inside each VU; this outer timeout only aborts a genuinely hung
        // send. It must cover the full legitimate window
        // (ramp_up + duration + one step timeout of grace for a hung final
        // send), otherwise it would prematurely cut off VUs still doing real
        // steady-state work. `duration == 0` keeps the 1h single-pass backstop.
        let timeout = if options.duration.as_millis() > 0 {
            let grace = scenario
                .get_steps()
                .into_iter()
                .map(|step| step.timeout)
                .max()
                .unwrap_or_default()
                .min(Duration::from_secs(30));
            options.ramp_up + options.duration + grace
        } else {
            Duration::from_secs(3600) // 1 hour default
        };

        // Capture abort handles so the spawned VU tasks can be stopped if the
        // duration timeout fires (otherwise they keep sending requests and
        // writing metrics after this function returns).
        let abort_handles: Vec<tokio::task::AbortHandle> =
            handles.iter().map(|h| h.abort_handle()).collect();

        let results = tokio::time::timeout(timeout, futures::future::join_all(handles)).await;
        let mut status = RunStatus::Completed;

        // Check for errors
        match results {
            Ok(join_results) => {
                for join_result in join_results {
                    match join_result {
                        Ok(Ok(truncated)) => {
                            if truncated {
                                status = RunStatus::Truncated {
                                    reason: format!(
                                        "Scenario '{}' reached its deadline mid-pass",
                                        scenario.id
                                    ),
                                };
                            }
                        }
                        Ok(Err(err)) => {
                            if options.abort_on_error {
                                return Err(err);
                            }
                            logging::warn!("VU in scenario '{}' failed: {err}", scenario.id);
                            status = RunStatus::Failed {
                                reason: format!("VU in scenario '{}' failed: {err}", scenario.id),
                            };
                        }
                        Err(join_err) => {
                            logging::error!(
                                "VU task in scenario '{}' panicked/cancelled: {join_err}",
                                scenario.id
                            );
                            if options.abort_on_error {
                                return Err(Error::engine(format!(
                                    "Virtual user task failed: {join_err}"
                                )));
                            }
                            status = RunStatus::Failed {
                                reason: format!(
                                    "VU task in scenario '{}' failed: {join_err}",
                                    scenario.id
                                ),
                            };
                        }
                    }
                }
            }
            Err(_elapsed) => {
                // The duration timeout fired before all VUs finished. Abort the
                // leaked tasks so they stop generating load, and surface the fact
                // that this run was truncated rather than completed.
                for handle in &abort_handles {
                    handle.abort();
                }
                logging::warn!(
                    "Scenario '{}' exceeded duration {:?}; aborted {} virtual user task(s)",
                    scenario.id,
                    timeout,
                    abort_handles.len()
                );
                status = RunStatus::Truncated {
                    reason: format!("Scenario '{}' exceeded {:?}", scenario.id, timeout),
                };
            }
        }

        Ok(status)
    }

    async fn run_scenario_with_profile(
        &self,
        scenario: Arc<Scenario>,
        options: ExecutionOptions,
        metrics_collector: Arc<dyn MetricsCollector>,
    ) -> Result<RunStatus> {
        let Some(profile) = &scenario.load_profile else {
            return self
                .run_scenario(scenario, options, Some(metrics_collector))
                .await;
        };

        let mut combined = RunStatus::Completed;
        for stage in &profile.stages {
            let stage_options = apply_stage_options(&options, stage);
            let status = self
                .run_scenario(
                    scenario.clone(),
                    stage_options,
                    Some(metrics_collector.clone()),
                )
                .await?;
            combined = combine_run_status(combined, status);
        }
        Ok(combined)
    }

    /// Run all scenarios with the given configuration
    pub async fn run(&self, config: &Config) -> Result<TestResults> {
        config.validate()?;
        // Build scenarios from the configuration
        let scenarios = config.build_scenarios()?;

        // Clone the engine (the manual Clone impl already carries the telemetry
        // exporter and factory overrides) and add the built scenarios.
        let mut engine = self.clone();

        for scenario in scenarios {
            engine.add_scenario(scenario);
        }
        engine.apply_config(config)?;

        // Create execution options from the configuration
        let options = ExecutionOptions {
            virtual_users: config.global.virtual_users,
            duration: Duration::from_secs(config.global.duration_seconds),
            ramp_up: Duration::from_secs(config.global.ramp_up_seconds),
            think_time: Duration::from_millis(config.global.think_time_ms),
            // Wire the in-flight-request cap from its own dedicated knob, NOT
            // from the connection-pool size. `0` => unlimited.
            max_concurrent_requests: config.http.max_concurrent_requests,
            abort_on_error: false,
            isolate_clients_per_user: false,
            target_rps: None,
        };

        // Run all scenarios
        engine.run_all(options).await
    }

    /// Run all scenarios with the given options
    pub async fn run_all(&self, options: ExecutionOptions) -> Result<TestResults> {
        if let Some(target_rps) = options.target_rps
            && (!target_rps.is_finite() || target_rps <= 0.0)
        {
            return Err(Error::config("target_rps must be finite and positive"));
        }

        // Validate every scenario before running so that an invalid, cyclic, or
        // self-dependent graph (constructable by embedders that bypass
        // ScenarioBuilder) fails loudly instead of hanging VUs until timeout.
        for scenario in self.scenarios.values() {
            scenario.validate().map_err(|e| {
                Error::scenario(format!("Scenario '{}' is invalid: {e}", scenario.id))
            })?;
        }

        // Telemetry lifecycle: if an exporter is attached, initialize
        // it before spawning scenarios. Per-request `export_request` callbacks
        // fire inside `execute_step`; the aggregate `export_results` + shutdown
        // run after results are computed (below). When no exporter is attached
        // this is a complete no-op — no telemetry awaits touch the run at all.
        // Config-level `telemetry.enabled` gating happens at attachment time
        // (`Engine::run` only attaches an exporter when enabled), so an attached
        // exporter is always meant to be invoked.
        if let Some(exporter) = &self.telemetry_exporter {
            exporter.init().await?;
        }

        // Create a metrics collector for the overall test
        let metrics_collector = (self.get_metrics_collector_factory())();

        // Create a vector to hold the futures for all scenario executions
        let mut scenario_futures = Vec::new();
        let scenario_count = self.scenarios.len().max(1);

        // Prepare futures for each scenario
        for scenario in self.scenarios.values() {
            // Create options for this scenario
            let split_target_rps = options
                .target_rps
                .map(|target_rps| target_rps / scenario_count as f64);
            let scenario_options = ExecutionOptions {
                virtual_users: scenario.virtual_users,
                duration: scenario.duration,
                ramp_up: scenario.ramp_up,
                think_time: scenario.think_time,
                target_rps: split_target_rps,
                ..options.clone()
            };

            // Create a future for running this scenario
            let engine_ref = self.clone();
            let scenario_clone = scenario.clone();
            let metrics_collector_clone = metrics_collector.clone();

            let future = tokio::spawn(async move {
                engine_ref
                    .run_scenario_with_profile(
                        scenario_clone,
                        scenario_options,
                        metrics_collector_clone,
                    )
                    .await
            });

            scenario_futures.push(future);
        }

        // Execute all scenarios in parallel and wait for them to complete
        let results = futures::future::join_all(scenario_futures).await;

        // Check for errors
        let mut run_status = RunStatus::Completed;
        for result in results {
            match result {
                Ok(Ok(status)) => {
                    run_status = combine_run_status(run_status, status);
                }
                Ok(Err(err)) => {
                    if options.abort_on_error {
                        return Err(err);
                    }
                    run_status = combine_run_status(
                        run_status,
                        RunStatus::Failed {
                            reason: err.to_string(),
                        },
                    );
                }
                Err(err) => {
                    if options.abort_on_error {
                        return Err(Error::other(format!("Task error: {err}")));
                    }
                    run_status = combine_run_status(
                        run_status,
                        RunStatus::Failed {
                            reason: format!("Task error: {err}"),
                        },
                    );
                }
            }
        }

        // Flush any pending metrics to ensure they are all recorded
        // This is particularly important for batched collectors
        metrics_collector.flush().await?;

        // Get the overall test results
        let mut results = metrics_collector.get_test_results().await?;
        results.status = run_status;

        // Telemetry lifecycle: export the aggregate results and shut the
        // exporter down. Log-and-continue on error so a telemetry failure never
        // discards the already-computed results.
        if let Some(exporter) = &self.telemetry_exporter {
            if let Err(err) = exporter.export_results(&results).await {
                logging::error!("Failed to export telemetry results: {err}");
            }
            if let Err(err) = exporter.shutdown().await {
                logging::error!("Failed to shut down telemetry exporter: {err}");
            }
        }

        Ok(results)
    }

    /// Visualize the dependency graph of all scenarios.
    ///
    /// Uses the custom visualizer installed via
    /// [`with_graph_visualizer`](Engine::with_graph_visualizer) when present,
    /// falling back to a default otherwise. (Previously a stored custom
    /// visualizer was silently ignored.)
    pub fn visualize_graph(&self, format: GraphFormat) -> Result<String> {
        let default = DefaultGraphVisualizer::new();
        let visualizer: &dyn GraphVisualizer = self.graph_visualizer.as_deref().unwrap_or(&default);

        // If there's only one scenario, visualize it
        if self.scenarios.len() == 1 {
            let scenario = self.scenarios.values().next().unwrap();
            return visualizer.visualize(scenario, format);
        }

        // Otherwise, create a combined visualization
        let mut result = String::new();

        for (id, scenario) in &self.scenarios {
            let graph = visualizer.visualize(scenario, format)?;
            result.push_str(&format!("# Scenario: {id}\n\n{graph}\n\n"));
        }

        Ok(result)
    }
}

impl Clone for Engine {
    fn clone(&self) -> Self {
        Self {
            scenarios: self.scenarios.clone(),
            http_client_factory: self.http_client_factory.clone(),
            metrics_collector_factory: self.metrics_collector_factory.clone(),
            // Preserve a stored custom visualizer across clone via clone_box
            // (dyn-clone) instead of substituting a default.
            graph_visualizer: self.graph_visualizer.as_ref().map(|v| v.clone_box()),
            telemetry_exporter: self.telemetry_exporter.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{Request, Response};
    use crate::scenario::{Scenario, ScenarioBuilder, StepBuilder};
    use async_trait::async_trait;

    /// HTTP client that always fails, so every step exhausts its retries.
    struct FailingHttpClient;

    #[async_trait]
    impl HttpClient for FailingHttpClient {
        async fn send(&self, _request: &Request) -> Result<Response> {
            Err(Error::other("mock request failure"))
        }

        async fn close(&self) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_engine_creation() {
        let engine = Engine::new();
        assert_eq!(engine.scenarios.len(), 0);
    }

    #[tokio::test]
    async fn test_add_scenario() {
        let mut engine = Engine::new();

        let request = Request::get("https://example.com").build().unwrap();
        let step = StepBuilder::new("step1", "Step 1", request).build();

        let scenario = ScenarioBuilder::new("scenario1", "Test Scenario")
            .step(step)
            .build()
            .unwrap();

        engine.add_scenario(scenario);

        assert_eq!(engine.scenarios.len(), 1);
        assert!(engine.scenarios.contains_key("scenario1"));
    }

    #[tokio::test]
    async fn test_visualize_graph() {
        let mut engine = Engine::new();

        let request = Request::get("https://example.com").build().unwrap();
        let step = StepBuilder::new("step1", "Step 1", request).build();

        let scenario = ScenarioBuilder::new("scenario1", "Test Scenario")
            .step(step)
            .build()
            .unwrap();

        engine.add_scenario(scenario);

        let dot = engine.visualize_graph(GraphFormat::Dot).unwrap();
        assert!(dot.contains("digraph"));

        let mermaid = engine.visualize_graph(GraphFormat::Mermaid).unwrap();
        assert!(mermaid.contains("graph TD"));

        let json = engine.visualize_graph(GraphFormat::Json).unwrap();
        assert!(json.contains("\"scenario\""));
    }

    /// A custom `GraphVisualizer` installed via
    /// `with_graph_visualizer` must actually be used (previously silently
    /// replaced by the default) AND survive `Engine::clone` (via `clone_box`).
    #[tokio::test]
    async fn test_custom_graph_visualizer_is_honored_and_clones() {
        /// A visualizer that ignores the scenario and returns a sentinel string.
        struct SentinelVisualizer;
        impl GraphVisualizer for SentinelVisualizer {
            fn visualize(&self, _scenario: &Scenario, _format: GraphFormat) -> Result<String> {
                Ok("SENTINEL-VIZ".to_string())
            }
            fn clone_box(&self) -> Box<dyn GraphVisualizer> {
                Box::new(SentinelVisualizer)
            }
        }

        let mut engine = Engine::new();
        let request = Request::get("https://example.com").build().unwrap();
        let step = StepBuilder::new("step1", "Step 1", request).build();
        let scenario = ScenarioBuilder::new("scenario1", "Test Scenario")
            .step(step)
            .build()
            .unwrap();
        engine.add_scenario(scenario);
        engine.with_graph_visualizer(Box::new(SentinelVisualizer));

        // The stored custom visualizer is used, not a fresh default.
        let out = engine.visualize_graph(GraphFormat::Dot).unwrap();
        assert_eq!(out, "SENTINEL-VIZ");

        // And it survives cloning the engine.
        let cloned = engine.clone();
        assert_eq!(
            cloned.visualize_graph(GraphFormat::Dot).unwrap(),
            "SENTINEL-VIZ"
        );
    }

    /// A failed dependency must not livelock the VU loop. With
    /// `duration = 0` the old code would sleep-poll for the full 3600s internal
    /// timeout; the terminal-exit fix makes the VU finish as soon as no step is
    /// ready. The outer timeout asserts `run_all` returns promptly.
    #[tokio::test]
    async fn test_failed_dependency_does_not_livelock() {
        let mut engine = Engine::new();
        engine.with_http_client_factory(|| Ok(Arc::new(FailingHttpClient) as Arc<dyn HttpClient>));
        engine.with_metrics_collector_factory(MetricsCollectorFactory::create_in_memory);

        // second depends on first; first will fail, so second can never become
        // ready and must not spin forever.
        let first = StepBuilder::new(
            "first",
            "First",
            Request::get("https://example.com/first").build().unwrap(),
        )
        .max_retries(0)
        .build();
        let second = StepBuilder::new(
            "second",
            "Second",
            Request::get("https://example.com/second").build().unwrap(),
        )
        .max_retries(0)
        .dependency("first")
        .build();

        let scenario = ScenarioBuilder::new("scenario1", "Test Scenario")
            .step(first)
            .step(second)
            .virtual_users(1)
            .duration(Duration::from_secs(0)) // 0 => 3600s internal timeout
            .build()
            .unwrap();

        engine.add_scenario(scenario);

        let result =
            tokio::time::timeout(Duration::from_secs(5), engine.run_all(options_no_wait()))
                .await
                .expect("run_all livelocked on a failed dependency");
        assert!(result.is_ok());
    }

    /// An invalid graph constructed by bypassing ScenarioBuilder must
    /// be rejected before running instead of hanging VUs until the timeout.
    #[tokio::test]
    async fn test_invalid_scenario_is_rejected() {
        let mut engine = Engine::new();
        engine.with_metrics_collector_factory(MetricsCollectorFactory::create_in_memory);

        // Bypass ScenarioBuilder/add_step validation by inserting directly.
        let mut scenario = Scenario::new("bad", "Bad Scenario");
        let step = StepBuilder::new(
            "s1",
            "S1",
            Request::get("https://example.com").build().unwrap(),
        )
        .dependency("ghost") // non-existent dependency
        .build();
        scenario.steps.insert(step.id.clone(), step);

        engine.add_scenario(scenario);

        let result = engine.run_all(options_no_wait()).await;
        assert!(result.is_err(), "invalid scenario should be rejected");
    }

    fn options_no_wait() -> ExecutionOptions {
        ExecutionOptions {
            virtual_users: 1,
            duration: Duration::from_secs(0),
            ramp_up: Duration::from_secs(0),
            think_time: Duration::from_secs(0),
            ..ExecutionOptions::default()
        }
    }
}
