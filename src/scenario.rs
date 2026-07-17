use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::data::DataSource;
use crate::error::{Error, Result};
use crate::http::{HttpMethod, Request, Response};

// Type alias for complex validator function type
type ValidatorFn = Arc<dyn Fn(&Response) -> bool + Send + Sync>;

/// Unique identifier for a step in a scenario
pub type StepId = String;

/// Unique identifier for a scenario
pub type ScenarioId = String;

/// Runtime state for one virtual user.
#[derive(Debug, Clone)]
pub struct VuContext {
    /// Virtual user id.
    pub id: u32,
    /// Current scenario id.
    pub scenario_id: ScenarioId,
    /// Current scenario iteration.
    pub iteration: u64,
    /// Per-VU variables extracted from prior responses.
    pub vars: HashMap<String, serde_json::Value>,
    /// Data-source rows bound for the current VU iteration.
    pub data: HashMap<String, serde_json::Value>,
}

impl VuContext {
    /// Create a new virtual-user context.
    pub fn new(id: u32, scenario_id: ScenarioId) -> Self {
        Self {
            id,
            scenario_id,
            iteration: 0,
            vars: HashMap::new(),
            data: HashMap::new(),
        }
    }

    /// Store a variable value in this VU's state.
    pub fn insert_var<S: Into<String>>(&mut self, name: S, value: serde_json::Value) {
        self.vars.insert(name.into(), value);
    }

    /// Resolve a user variable by either `name` or `var.name` syntax.
    pub fn get_var(&self, name: &str) -> Option<&serde_json::Value> {
        let name = name.strip_prefix("var.").unwrap_or(name);
        self.vars.get(name)
    }

    /// Replace data-source rows for the current VU iteration.
    pub fn set_data_rows(&mut self, data: HashMap<String, serde_json::Value>) {
        self.data = data;
    }

    /// Resolve a bound data-source row by id.
    pub fn get_data(&self, id: &str) -> Option<&serde_json::Value> {
        self.data.get(id)
    }
}

/// Dynamic request body template.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DynamicBodyTemplate {
    /// Render as plain text.
    Text(String),
    /// Render, then parse as JSON.
    Json(String),
}

/// Runtime-rendered request fields for dynamic scenarios.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct DynamicRequestSpec {
    /// HTTP method for the rendered request.
    pub method: HttpMethod,
    /// URL template rendered at send time.
    pub url_template: String,
    /// Header value templates rendered at send time.
    pub header_templates: HashMap<String, String>,
    /// Optional body template rendered at send time.
    pub body_template: Option<DynamicBodyTemplate>,
    /// Whether rendered requests should follow redirects.
    pub follow_redirects: bool,
}

impl DynamicRequestSpec {
    /// Create a dynamic request spec from an existing static request.
    pub fn from_request(request: &Request) -> Self {
        let mut header_templates = HashMap::new();
        for (name, value) in request.headers() {
            if let Ok(value) = value.to_str() {
                header_templates.insert(name.as_str().to_string(), value.to_string());
            }
        }

        let body_template = match request.body() {
            crate::http::Body::Empty => None,
            crate::http::Body::Text(text) => Some(DynamicBodyTemplate::Text(text.clone())),
            crate::http::Body::Json(value) => Some(DynamicBodyTemplate::Json(value.to_string())),
            // `RequestBuilder::json` stores pre-serialized UTF-8 bytes as Binary
            // with Content-Type application/json; treat that as a JSON template.
            crate::http::Body::Binary(bytes) => {
                let is_json = request
                    .headers()
                    .get("content-type")
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|value| value.starts_with("application/json"));
                if is_json {
                    String::from_utf8(bytes.to_vec())
                        .ok()
                        .map(DynamicBodyTemplate::Json)
                } else {
                    None
                }
            }
        };

        Self {
            method: request.method().clone(),
            url_template: request.url().to_string(),
            header_templates,
            body_template,
            follow_redirects: request.follow_redirects(),
        }
    }
}

/// Response extractor source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ExtractorSource {
    /// Extract from a response JSON body using a small dot path.
    JsonPath,
    /// Extract by applying a regex to the response body.
    BodyRegex,
    /// Extract the whole response header value.
    Header,
    /// Extract by applying a regex to a response header value.
    HeaderRegex,
    /// Extract the numeric HTTP status code.
    Status,
}

/// Extract a value from a response into the virtual-user state.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Extractor {
    /// Variable name to store.
    pub name: String,
    /// Extractor source.
    pub source: ExtractorSource,
    /// JSON path, regex pattern, or header name depending on source.
    pub selector: String,
    /// Optional header name for [`ExtractorSource::HeaderRegex`].
    pub header: Option<String>,
    /// Whether missing extraction fails the attempt.
    pub required: bool,
    /// Compiled `selector` for [`ExtractorSource::BodyRegex`] /
    /// [`ExtractorSource::HeaderRegex`]. Built once at construction so the
    /// send path does not recompile on every attempt.
    pub(crate) compiled_regex: Option<regex::Regex>,
}

impl PartialEq for Extractor {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.source == other.source
            && self.selector == other.selector
            && self.header == other.header
            && self.required == other.required
    }
}

impl Eq for Extractor {}

impl Extractor {
    /// Create a JSON-path extractor.
    pub fn json_path<N: Into<String>, P: Into<String>>(name: N, path: P) -> Self {
        Self {
            name: name.into(),
            source: ExtractorSource::JsonPath,
            selector: path.into(),
            header: None,
            required: true,
            compiled_regex: None,
        }
    }

    /// Create a body-regex extractor.
    pub fn body_regex<N: Into<String>, P: Into<String>>(name: N, regex: P) -> Self {
        let selector = regex.into();
        let compiled_regex = regex::Regex::new(&selector).ok();
        Self {
            name: name.into(),
            source: ExtractorSource::BodyRegex,
            selector,
            header: None,
            required: true,
            compiled_regex,
        }
    }

    /// Create a whole-header extractor.
    pub fn header<N: Into<String>, H: Into<String>>(name: N, header: H) -> Self {
        Self {
            name: name.into(),
            source: ExtractorSource::Header,
            selector: header.into(),
            header: None,
            required: true,
            compiled_regex: None,
        }
    }

    /// Create a header-regex extractor.
    pub fn header_regex<N: Into<String>, H: Into<String>, P: Into<String>>(
        name: N,
        header: H,
        regex: P,
    ) -> Self {
        let selector = regex.into();
        let compiled_regex = regex::Regex::new(&selector).ok();
        Self {
            name: name.into(),
            source: ExtractorSource::HeaderRegex,
            selector,
            header: Some(header.into()),
            required: true,
            compiled_regex,
        }
    }

    /// Create a status-code extractor.
    pub fn status<N: Into<String>>(name: N) -> Self {
        Self {
            name: name.into(),
            source: ExtractorSource::Status,
            selector: String::new(),
            header: None,
            required: true,
            compiled_regex: None,
        }
    }

    /// Mark this extractor optional or required.
    pub fn required(mut self, required: bool) -> Self {
        self.required = required;
        self
    }
}

/// Branch condition operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum BranchOperator {
    /// Run only when the variable exists.
    Exists,
    /// Run only when the variable stringifies equal to the expected value.
    Equals,
    /// Run only when the variable exists and stringifies differently.
    NotEquals,
    /// Run only when the variable is numerically greater than the expected value.
    GreaterThan,
    /// Run only when the variable is numerically greater than or equal to the expected value.
    GreaterThanOrEqual,
    /// Run only when the variable is numerically less than the expected value.
    LessThan,
    /// Run only when the variable is numerically less than or equal to the expected value.
    LessThanOrEqual,
    /// Run only when the variable stringifies to a value matching the regex.
    MatchesRegex,
}

/// Conditional step execution.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct BranchCondition {
    /// Variable name to inspect.
    pub variable: String,
    /// Condition operation.
    pub operator: BranchOperator,
    /// Expected value for equals / not_equals.
    pub value: Option<String>,
    /// Compiled regex for [`BranchOperator::MatchesRegex`].
    pub(crate) compiled_regex: Option<regex::Regex>,
}

impl PartialEq for BranchCondition {
    fn eq(&self, other: &Self) -> bool {
        self.variable == other.variable
            && self.operator == other.operator
            && self.value == other.value
    }
}

impl Eq for BranchCondition {}

impl BranchCondition {
    /// Create an `exists` branch condition.
    pub fn exists<S: Into<String>>(variable: S) -> Self {
        Self {
            variable: variable.into(),
            operator: BranchOperator::Exists,
            value: None,
            compiled_regex: None,
        }
    }

    /// Create an equality branch condition.
    pub fn equals<S: Into<String>, V: Into<String>>(variable: S, value: V) -> Self {
        Self {
            variable: variable.into(),
            operator: BranchOperator::Equals,
            value: Some(value.into()),
            compiled_regex: None,
        }
    }

    /// Create an inequality branch condition.
    pub fn not_equals<S: Into<String>, V: Into<String>>(variable: S, value: V) -> Self {
        Self {
            variable: variable.into(),
            operator: BranchOperator::NotEquals,
            value: Some(value.into()),
            compiled_regex: None,
        }
    }

    /// Create a numeric greater-than branch condition.
    pub fn greater_than<S: Into<String>, V: Into<String>>(variable: S, value: V) -> Self {
        Self {
            variable: variable.into(),
            operator: BranchOperator::GreaterThan,
            value: Some(value.into()),
            compiled_regex: None,
        }
    }

    /// Create a numeric greater-than-or-equal branch condition.
    pub fn greater_than_or_equal<S: Into<String>, V: Into<String>>(variable: S, value: V) -> Self {
        Self {
            variable: variable.into(),
            operator: BranchOperator::GreaterThanOrEqual,
            value: Some(value.into()),
            compiled_regex: None,
        }
    }

    /// Create a numeric less-than branch condition.
    pub fn less_than<S: Into<String>, V: Into<String>>(variable: S, value: V) -> Self {
        Self {
            variable: variable.into(),
            operator: BranchOperator::LessThan,
            value: Some(value.into()),
            compiled_regex: None,
        }
    }

    /// Create a numeric less-than-or-equal branch condition.
    pub fn less_than_or_equal<S: Into<String>, V: Into<String>>(variable: S, value: V) -> Self {
        Self {
            variable: variable.into(),
            operator: BranchOperator::LessThanOrEqual,
            value: Some(value.into()),
            compiled_regex: None,
        }
    }

    /// Create a regex branch condition.
    pub fn matches_regex<S: Into<String>, V: Into<String>>(variable: S, pattern: V) -> Self {
        let value = pattern.into();
        let compiled_regex = regex::Regex::new(&value).ok();
        Self {
            variable: variable.into(),
            operator: BranchOperator::MatchesRegex,
            value: Some(value),
            compiled_regex,
        }
    }
}

/// Sequential load profile for a scenario.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadProfile {
    /// Stages to execute sequentially.
    #[serde(default)]
    pub stages: Vec<LoadStage>,
}

/// One stage in a scenario load profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadStage {
    /// Optional stage name.
    #[serde(default)]
    pub name: Option<String>,
    /// Stage duration in seconds.
    pub duration_seconds: u64,
    /// Optional virtual-user override for this stage.
    #[serde(default)]
    pub virtual_users: Option<u32>,
    /// Optional per-scenario request-attempt start rate for this stage.
    #[serde(default)]
    pub target_rps: Option<f64>,
    /// Optional ramp-up override for this stage.
    #[serde(default)]
    pub ramp_up_seconds: Option<u64>,
    /// Optional think-time override for this stage.
    #[serde(default)]
    pub think_time_ms: Option<u64>,
}

/// A step in a test scenario
#[derive(Clone)]
pub struct Step {
    /// Unique identifier for this step
    pub id: StepId,

    /// Human-readable name for this step
    pub name: String,

    /// HTTP request to be executed
    pub request: Request,

    /// IDs of steps that must complete before this step can execute
    pub dependencies: HashSet<StepId>,

    /// Custom validation function for the response
    pub validator: Option<ValidatorFn>,

    /// Maximum number of times to retry this step if it fails
    pub max_retries: u32,

    /// Delay between retries
    pub retry_delay: Duration,

    /// Timeout for this step
    pub timeout: Duration,

    /// Scheduling/launch priority among simultaneously-ready steps (higher runs
    /// first). This does NOT change how often a step runs: every ready step runs
    /// exactly once per scenario iteration. It only orders the launch of steps
    /// that become ready together within one pass.
    pub weight: u32,

    /// Custom data associated with this step
    pub metadata: HashMap<String, String>,

    /// Optional runtime-rendered request fields and dynamic step behavior.
    pub dynamic_request: Option<DynamicRequestSpec>,

    /// Response extractors that populate per-VU state.
    pub extractors: Vec<Extractor>,

    /// Optional branch condition. A false condition skips the step.
    pub branch: Option<BranchCondition>,
}

impl fmt::Debug for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Step")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("request", &self.request)
            .field("dependencies", &self.dependencies)
            .field("validator", &format_args!("<function>"))
            .field("max_retries", &self.max_retries)
            .field("retry_delay", &self.retry_delay)
            .field("timeout", &self.timeout)
            .field("weight", &self.weight)
            .field("metadata", &self.metadata)
            .field("dynamic_request", &self.dynamic_request)
            .field("extractors", &self.extractors)
            .field("branch", &self.branch)
            .finish()
    }
}

impl Step {
    /// Create a new step with the given ID, name, and request
    pub fn new<I: Into<String>, N: Into<String>>(id: I, name: N, request: Request) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            request,
            dependencies: HashSet::new(),
            validator: None,
            max_retries: 0,
            retry_delay: Duration::from_millis(100),
            timeout: Duration::from_secs(30),
            weight: 1,
            metadata: HashMap::new(),
            dynamic_request: None,
            extractors: Vec::new(),
            branch: None,
        }
    }

    /// Add a dependency to this step
    pub fn add_dependency<S: Into<String>>(&mut self, step_id: S) -> &mut Self {
        self.dependencies.insert(step_id.into());
        self
    }

    /// Set the validator function for this step
    pub fn with_validator<F>(&mut self, validator: F) -> &mut Self
    where
        F: Fn(&Response) -> bool + Send + Sync + 'static,
    {
        self.validator = Some(Arc::new(validator));
        self
    }

    /// Set the maximum number of retries for this step
    pub fn with_max_retries(&mut self, max_retries: u32) -> &mut Self {
        self.max_retries = max_retries;
        self
    }

    /// Set the retry delay for this step
    pub fn with_retry_delay(&mut self, retry_delay: Duration) -> &mut Self {
        self.retry_delay = retry_delay;
        self
    }

    /// Set the timeout for this step
    pub fn with_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.timeout = timeout;
        self
    }

    /// Set the weight for this step.
    ///
    /// Weight is scheduling/launch priority among simultaneously-ready steps
    /// (higher launches first); it does NOT change how often a step runs — every
    /// ready step runs exactly once per scenario iteration.
    pub fn with_weight(&mut self, weight: u32) -> &mut Self {
        self.weight = weight;
        self
    }

    /// Add metadata to this step
    pub fn with_metadata<K: Into<String>, V: Into<String>>(
        &mut self,
        key: K,
        value: V,
    ) -> &mut Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// Set the dynamic request spec for this step.
    pub fn with_dynamic_request(&mut self, dynamic_request: DynamicRequestSpec) -> &mut Self {
        self.dynamic_request = Some(dynamic_request);
        self
    }

    /// Set the dynamic URL template, preserving the static request's other
    /// request fields.
    pub fn with_url_template<S: Into<String>>(&mut self, url_template: S) -> &mut Self {
        let mut spec = self
            .dynamic_request
            .clone()
            .unwrap_or_else(|| DynamicRequestSpec::from_request(&self.request));
        spec.url_template = url_template.into();
        self.dynamic_request = Some(spec);
        self
    }

    /// Add or replace a dynamic header template.
    pub fn with_header_template<K: Into<String>, V: Into<String>>(
        &mut self,
        key: K,
        value: V,
    ) -> &mut Self {
        let mut spec = self
            .dynamic_request
            .clone()
            .unwrap_or_else(|| DynamicRequestSpec::from_request(&self.request));
        spec.header_templates.insert(key.into(), value.into());
        self.dynamic_request = Some(spec);
        self
    }

    /// Set a dynamic text body template.
    pub fn with_text_body_template<S: Into<String>>(&mut self, template: S) -> &mut Self {
        let mut spec = self
            .dynamic_request
            .clone()
            .unwrap_or_else(|| DynamicRequestSpec::from_request(&self.request));
        spec.body_template = Some(DynamicBodyTemplate::Text(template.into()));
        self.dynamic_request = Some(spec);
        self
    }

    /// Set a dynamic JSON body template.
    pub fn with_json_body_template<S: Into<String>>(&mut self, template: S) -> &mut Self {
        let mut spec = self
            .dynamic_request
            .clone()
            .unwrap_or_else(|| DynamicRequestSpec::from_request(&self.request));
        spec.body_template = Some(DynamicBodyTemplate::Json(template.into()));
        self.dynamic_request = Some(spec);
        self
    }

    /// Add a response extractor.
    pub fn with_extractor(&mut self, extractor: Extractor) -> &mut Self {
        self.extractors.push(extractor);
        self
    }

    /// Set the branch condition for this step.
    pub fn with_branch(&mut self, branch: BranchCondition) -> &mut Self {
        self.branch = Some(branch);
        self
    }

    /// Validate a response using this step's validator
    pub fn validate(&self, response: &Response) -> bool {
        match &self.validator {
            Some(validator) => validator(response),
            None => response.status().is_success(),
        }
    }
}

/// Builder for creating steps
pub struct StepBuilder {
    step: Step,
}

impl StepBuilder {
    /// Create a new step builder with the given ID, name, and request
    pub fn new<I: Into<String>, N: Into<String>>(id: I, name: N, request: Request) -> Self {
        Self {
            step: Step::new(id, name, request),
        }
    }

    /// Add a dependency to this step
    pub fn dependency<S: Into<String>>(mut self, step_id: S) -> Self {
        self.step.add_dependency(step_id);
        self
    }

    /// Set the validator function for this step
    pub fn validator<F>(mut self, validator: F) -> Self
    where
        F: Fn(&Response) -> bool + Send + Sync + 'static,
    {
        self.step.with_validator(validator);
        self
    }

    /// Set the maximum number of retries for this step
    pub fn max_retries(mut self, max_retries: u32) -> Self {
        self.step.with_max_retries(max_retries);
        self
    }

    /// Set the retry delay for this step
    pub fn retry_delay(mut self, retry_delay: Duration) -> Self {
        self.step.with_retry_delay(retry_delay);
        self
    }

    /// Set the timeout for this step
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.step.with_timeout(timeout);
        self
    }

    /// Set the weight for this step.
    ///
    /// Weight is scheduling/launch priority among simultaneously-ready steps
    /// (higher launches first); it does NOT change how often a step runs — every
    /// ready step runs exactly once per scenario iteration.
    pub fn weight(mut self, weight: u32) -> Self {
        self.step.with_weight(weight);
        self
    }

    /// Add metadata to this step
    pub fn metadata<K: Into<String>, V: Into<String>>(mut self, key: K, value: V) -> Self {
        self.step.with_metadata(key, value);
        self
    }

    /// Set the dynamic request spec for this step.
    pub fn dynamic_request(mut self, dynamic_request: DynamicRequestSpec) -> Self {
        self.step.with_dynamic_request(dynamic_request);
        self
    }

    /// Set the dynamic URL template.
    pub fn url_template<S: Into<String>>(mut self, url_template: S) -> Self {
        self.step.with_url_template(url_template);
        self
    }

    /// Add or replace a dynamic header template.
    pub fn header_template<K: Into<String>, V: Into<String>>(mut self, key: K, value: V) -> Self {
        self.step.with_header_template(key, value);
        self
    }

    /// Set a dynamic text body template.
    pub fn text_body_template<S: Into<String>>(mut self, template: S) -> Self {
        self.step.with_text_body_template(template);
        self
    }

    /// Set a dynamic JSON body template.
    pub fn json_body_template<S: Into<String>>(mut self, template: S) -> Self {
        self.step.with_json_body_template(template);
        self
    }

    /// Add a response extractor.
    pub fn extractor(mut self, extractor: Extractor) -> Self {
        self.step.with_extractor(extractor);
        self
    }

    /// Set a branch condition.
    pub fn branch(mut self, branch: BranchCondition) -> Self {
        self.step.with_branch(branch);
        self
    }

    /// Build the step
    pub fn build(self) -> Step {
        self.step
    }
}

/// A test scenario consisting of multiple steps
#[derive(Debug, Clone)]
pub struct Scenario {
    /// Unique identifier for this scenario
    pub id: ScenarioId,

    /// Human-readable name for this scenario
    pub name: String,

    /// Steps in this scenario
    pub steps: HashMap<StepId, Step>,

    /// Cached ids of root steps (no dependencies), rebuilt when steps change.
    /// Used by the engine hot path to avoid rescanning `steps` every DAG pass.
    pub(crate) root_step_ids: Vec<StepId>,

    /// Reverse adjacency: dependency id → steps that list it in `dependencies`.
    /// Used to promote waiting dependents in O(out-degree) instead of O(steps).
    pub(crate) dependents: HashMap<StepId, Vec<StepId>>,

    /// Number of virtual users to simulate
    pub virtual_users: u32,

    /// Duration of the test
    pub duration: Duration,

    /// Ramp-up period (time to gradually increase load)
    pub ramp_up: Duration,

    /// Think time between requests (simulates user behavior)
    pub think_time: Duration,

    /// Custom data associated with this scenario
    pub metadata: HashMap<String, String>,

    /// Optional sequential staged load profile.
    pub load_profile: Option<LoadProfile>,

    /// Dynamic data sources available to this scenario.
    pub data_sources: HashMap<String, DataSource>,

    /// Base directory used to resolve relative data-source paths.
    pub data_source_base_dir: Option<PathBuf>,
}

impl Scenario {
    /// Create a new scenario with the given ID and name
    pub fn new<I: Into<String>, N: Into<String>>(id: I, name: N) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            steps: HashMap::new(),
            root_step_ids: Vec::new(),
            dependents: HashMap::new(),
            virtual_users: 1,
            duration: Duration::from_secs(60),
            ramp_up: Duration::from_secs(0),
            think_time: Duration::from_millis(0),
            metadata: HashMap::new(),
            load_profile: None,
            data_sources: HashMap::new(),
            data_source_base_dir: None,
        }
    }

    /// Rebuild root-id and reverse-adjacency caches from `steps`.
    ///
    /// Call after any mutation of `steps` that bypasses [`Self::add_step`] / builder
    /// finalize. Safe to call repeatedly.
    pub fn rebuild_scheduling_cache(&mut self) {
        self.root_step_ids.clear();
        self.dependents.clear();

        for step in self.steps.values() {
            if step.dependencies.is_empty() {
                self.root_step_ids.push(step.id.clone());
            }
            for dep_id in &step.dependencies {
                self.dependents
                    .entry(dep_id.clone())
                    .or_default()
                    .push(step.id.clone());
            }
        }

        // Stable order keeps ready/reset behavior deterministic across runs.
        self.root_step_ids.sort_unstable();
        for children in self.dependents.values_mut() {
            children.sort_unstable();
        }
    }

    /// Add a step to this scenario
    pub fn add_step(&mut self, step: Step) -> Result<&mut Self> {
        // Validate that all dependencies exist
        for dep_id in &step.dependencies {
            if !self.steps.contains_key(dep_id) && dep_id != &step.id {
                return Err(Error::scenario(format!(
                    "Step '{}' depends on non-existent step '{}'",
                    step.id, dep_id
                )));
            }
        }

        self.steps.insert(step.id.clone(), step);
        self.rebuild_scheduling_cache();
        Ok(self)
    }

    /// Set the number of virtual users for this scenario
    pub fn with_virtual_users(&mut self, virtual_users: u32) -> &mut Self {
        self.virtual_users = virtual_users;
        self
    }

    /// Set the duration of the test
    pub fn with_duration(&mut self, duration: Duration) -> &mut Self {
        self.duration = duration;
        self
    }

    /// Set the ramp-up period
    pub fn with_ramp_up(&mut self, ramp_up: Duration) -> &mut Self {
        self.ramp_up = ramp_up;
        self
    }

    /// Set the think time
    pub fn with_think_time(&mut self, think_time: Duration) -> &mut Self {
        self.think_time = think_time;
        self
    }

    /// Add metadata to this scenario
    pub fn with_metadata<K: Into<String>, V: Into<String>>(
        &mut self,
        key: K,
        value: V,
    ) -> &mut Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// Set the staged load profile for this scenario.
    pub fn with_load_profile(&mut self, load_profile: LoadProfile) -> &mut Self {
        self.load_profile = Some(load_profile);
        self
    }

    /// Add a dynamic data source to this scenario.
    pub fn with_data_source<S: Into<String>>(
        &mut self,
        id: S,
        data_source: DataSource,
    ) -> &mut Self {
        self.data_sources.insert(id.into(), data_source);
        self
    }

    /// Set the base directory for resolving relative data-source paths.
    pub fn with_data_source_base_dir<P: Into<PathBuf>>(&mut self, base_dir: P) -> &mut Self {
        self.data_source_base_dir = Some(base_dir.into());
        self
    }

    /// Get a step by ID
    pub fn get_step(&self, id: &str) -> Option<&Step> {
        self.steps.get(id)
    }

    /// Get all steps in this scenario
    pub fn get_steps(&self) -> Vec<&Step> {
        self.steps.values().collect()
    }

    /// Get the root steps (steps with no dependencies)
    pub fn get_root_steps(&self) -> Vec<&Step> {
        if !self.root_step_ids.is_empty() || self.steps.is_empty() {
            return self
                .root_step_ids
                .iter()
                .filter_map(|id| self.steps.get(id))
                .collect();
        }
        // Fallback when `steps` was mutated without rebuilding the cache.
        self.steps
            .values()
            .filter(|step| step.dependencies.is_empty())
            .collect()
    }

    /// Get the leaf steps (steps that no other steps depend on)
    pub fn get_leaf_steps(&self) -> Vec<&Step> {
        let mut leaf_steps = HashSet::new();

        // Start with all steps
        for step in self.steps.values() {
            leaf_steps.insert(&step.id);
        }

        // Remove steps that are dependencies of other steps
        for step in self.steps.values() {
            for dep_id in &step.dependencies {
                leaf_steps.remove(dep_id);
            }
        }

        // Return the remaining steps
        leaf_steps
            .iter()
            .filter_map(|id| self.steps.get(*id))
            .collect()
    }

    /// Validate the scenario for consistency
    pub fn validate(&self) -> Result<()> {
        // Reject a zero virtual-user count: it would otherwise run to
        // completion sending zero requests and report exit-0 "success",
        // silently masking a misconfigured (or global-inherited 0) load level
        // (zero virtual_users fails loudly). This single chokepoint covers both the config path
        // (build_scenarios) and the embedder path (run_all validates every
        // scenario). `duration_seconds == 0` is intentionally NOT rejected: it
        // is the documented single-pass mode.
        if self.virtual_users == 0 {
            return Err(Error::scenario(format!(
                "Scenario '{}' must have at least 1 virtual user",
                self.id
            )));
        }

        if self.steps.is_empty() {
            return Err(Error::scenario(format!(
                "Scenario '{}' must contain at least one step",
                self.id
            )));
        }

        if let Some(profile) = &self.load_profile {
            if profile.stages.is_empty() {
                return Err(Error::scenario(format!(
                    "Scenario '{}' load profile must contain at least one stage",
                    self.id
                )));
            }
            for stage in &profile.stages {
                if stage.duration_seconds == 0 {
                    return Err(Error::scenario(format!(
                        "Scenario '{}' load stage duration_seconds must be positive",
                        self.id
                    )));
                }
                if matches!(stage.virtual_users, Some(0)) {
                    return Err(Error::scenario(format!(
                        "Scenario '{}' load stage virtual_users must be positive",
                        self.id
                    )));
                }
                if let Some(target_rps) = stage.target_rps
                    && (!target_rps.is_finite() || target_rps <= 0.0)
                {
                    return Err(Error::scenario(format!(
                        "Scenario '{}' load stage target_rps must be finite and positive",
                        self.id
                    )));
                }
            }
        }

        // Check for cycles in the dependency graph
        self.check_cycles()?;

        // Check that all steps have valid dependencies
        for step in self.steps.values() {
            for dep_id in &step.dependencies {
                if !self.steps.contains_key(dep_id) {
                    return Err(Error::scenario(format!(
                        "Step '{}' depends on non-existent step '{}'",
                        step.id, dep_id
                    )));
                }
            }
        }

        Ok(())
    }

    /// Check for cycles in the dependency graph
    fn check_cycles(&self) -> Result<()> {
        let mut visited = HashSet::new();
        let mut rec_stack = HashSet::new();

        for step_id in self.steps.keys() {
            if !visited.contains(step_id)
                && self.is_cyclic(step_id, &mut visited, &mut rec_stack)?
            {
                return Err(Error::scenario(
                    "Cycle detected in scenario dependency graph".to_string(),
                ));
            }
        }

        Ok(())
    }

    /// Helper function for cycle detection
    fn is_cyclic(
        &self,
        step_id: &str,
        visited: &mut HashSet<String>,
        rec_stack: &mut HashSet<String>,
    ) -> Result<bool> {
        visited.insert(step_id.to_string());
        rec_stack.insert(step_id.to_string());

        if let Some(step) = self.steps.get(step_id) {
            for dep_id in &step.dependencies {
                if !visited.contains(dep_id) {
                    if self.is_cyclic(dep_id, visited, rec_stack)? {
                        return Ok(true);
                    }
                } else if rec_stack.contains(dep_id) {
                    return Ok(true);
                }
            }
        } else {
            return Err(Error::scenario(format!(
                "Step '{step_id}' not found in scenario"
            )));
        }

        rec_stack.remove(step_id);
        Ok(false)
    }
}

/// Builder for creating scenarios
///
/// This builder provides a fluent API for creating scenarios. Steps are added
/// without eager validation, so method calls chain without an `unwrap` after
/// each [`step`](Self::step); all errors (cycles, missing dependencies, an
/// empty virtual-user count) surface from the single [`build`](Self::build)
/// call at the end.
pub struct ScenarioBuilder {
    scenario: Scenario,
}

impl ScenarioBuilder {
    /// Create a new scenario builder with the given ID and name
    pub fn new<I: Into<String>, N: Into<String>>(id: I, name: N) -> Self {
        Self {
            scenario: Scenario::new(id, name),
        }
    }

    /// Add a step to this scenario.
    ///
    /// Steps are inserted directly with NO eager dependency check, so forward
    /// references are legal: `.step(b_depends_on_a).step(a)` builds fine. All
    /// validation (cycles, dependency existence, `virtual_users >= 1`) is
    /// deferred to [`build`](Self::build), which is the single place errors
    /// surface. This is why `step` returns `Self` rather than `Result<Self>` —
    /// there is nothing to fail here, so callers chain without `unwrap`.
    pub fn step(mut self, step: Step) -> Self {
        self.scenario.steps.insert(step.id.clone(), step);
        self
    }

    /// Set the number of virtual users for this scenario
    pub fn virtual_users(mut self, virtual_users: u32) -> Self {
        self.scenario.with_virtual_users(virtual_users);
        self
    }

    /// Set the duration of the test
    pub fn duration(mut self, duration: Duration) -> Self {
        self.scenario.with_duration(duration);
        self
    }

    /// Set the ramp-up period
    pub fn ramp_up(mut self, ramp_up: Duration) -> Self {
        self.scenario.with_ramp_up(ramp_up);
        self
    }

    /// Set the think time
    pub fn think_time(mut self, think_time: Duration) -> Self {
        self.scenario.with_think_time(think_time);
        self
    }

    /// Add metadata to this scenario
    pub fn metadata<K: Into<String>, V: Into<String>>(mut self, key: K, value: V) -> Self {
        self.scenario.with_metadata(key, value);
        self
    }

    /// Set the staged load profile for this scenario.
    pub fn load_profile(mut self, load_profile: LoadProfile) -> Self {
        self.scenario.with_load_profile(load_profile);
        self
    }

    /// Add a dynamic data source to this scenario.
    pub fn data_source<S: Into<String>>(mut self, id: S, data_source: DataSource) -> Self {
        self.scenario.with_data_source(id, data_source);
        self
    }

    /// Set the base directory for resolving relative data-source paths.
    pub fn data_source_base_dir<P: Into<PathBuf>>(mut self, base_dir: P) -> Self {
        self.scenario.with_data_source_base_dir(base_dir);
        self
    }

    /// Build the scenario, validating it (cycles, dependency existence, and a
    /// non-zero virtual-user count). This is the single point where a
    /// misconfigured scenario — including forward references that never resolve
    /// — is rejected.
    pub fn build(mut self) -> Result<Scenario> {
        self.scenario.validate()?;
        self.scenario.rebuild_scheduling_cache();
        Ok(self.scenario)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Request;

    #[test]
    fn test_step_builder() {
        let request = Request::get("https://example.com").build().unwrap();
        let step = StepBuilder::new("step1", "Step 1", request)
            .dependency("step0")
            .max_retries(3)
            .timeout(Duration::from_secs(10))
            .build();

        assert_eq!(step.id, "step1");
        assert_eq!(step.name, "Step 1");
        assert_eq!(step.max_retries, 3);
        assert_eq!(step.timeout, Duration::from_secs(10));
        assert!(step.dependencies.contains("step0"));
    }

    #[test]
    fn test_scenario_builder() {
        let request1 = Request::get("https://localhost/1").build().unwrap();
        let request2 = Request::get("https://localhost/2").build().unwrap();

        let step1 = StepBuilder::new("step1", "Step 1", request1).build();
        let step2 = StepBuilder::new("step2", "Step 2", request2)
            .dependency("step1")
            .build();

        let scenario = ScenarioBuilder::new("scenario1", "Scenario 1")
            .step(step1)
            .step(step2)
            .virtual_users(10)
            .duration(Duration::from_secs(60))
            .build()
            .unwrap();

        assert_eq!(scenario.id, "scenario1");
        assert_eq!(scenario.name, "Scenario 1");
        assert_eq!(scenario.virtual_users, 10);
        assert_eq!(scenario.duration, Duration::from_secs(60));
        assert_eq!(scenario.steps.len(), 2);

        let root_steps = scenario.get_root_steps();
        assert_eq!(root_steps.len(), 1);
        assert_eq!(root_steps[0].id, "step1");
        assert_eq!(scenario.root_step_ids, vec!["step1".to_string()]);
        assert_eq!(
            scenario.dependents.get("step1").map(|d| d.as_slice()),
            Some(["step2".to_string()].as_slice())
        );

        let leaf_steps = scenario.get_leaf_steps();
        assert_eq!(leaf_steps.len(), 1);
        assert_eq!(leaf_steps[0].id, "step2");
    }

    #[test]
    fn test_validate_rejects_zero_virtual_users() {
        let request = Request::get("https://example.com").build().unwrap();
        let step = StepBuilder::new("s", "S", request).build();
        let mut scenario = Scenario::new("z", "Zero");
        scenario.add_step(step).unwrap();
        scenario.with_virtual_users(0);

        let err = scenario.validate().unwrap_err();
        assert!(matches!(err, Error::Scenario(msg) if msg.contains("at least 1 virtual user")));
    }

    #[test]
    fn test_cycle_detection() {
        let request1 = Request::get("https://localhost/1").build().unwrap();
        let request2 = Request::get("https://localhost/2").build().unwrap();
        let request3 = Request::get("https://localhost/3").build().unwrap();

        let step1 = StepBuilder::new("step1", "Step 1", request1)
            .dependency("step3")
            .build();
        let step2 = StepBuilder::new("step2", "Step 2", request2)
            .dependency("step1")
            .build();
        let step3 = StepBuilder::new("step3", "Step 3", request3)
            .dependency("step2")
            .build();

        let result = ScenarioBuilder::new("scenario1", "Scenario 1")
            .step(step1)
            .step(step2)
            .step(step3)
            .build();

        assert!(result.is_err());
        match result {
            Err(Error::Scenario(msg)) => {
                assert!(msg.contains("Cycle detected in scenario dependency graph"));
            }
            _ => panic!("Expected scenario error"),
        }
    }
}
