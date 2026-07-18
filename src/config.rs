use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::{Method, Url, header};
use serde::{Deserialize, Serialize};
use serde_yaml_ng as serde_yaml;
use toml;

use crate::data::{
    DataAccessMode, DataExhaustion, DataSource, LoadedDataSources, validate_data_source_id,
    validate_json_path, validate_relative_json_path,
};
use crate::error::{Error, Result};
use crate::scenario::{
    BranchCondition, BranchOperator, DynamicBodyTemplate, DynamicRequestSpec, Extractor,
    LoadProfile, Scenario, ScenarioBuilder, Step,
};

/// Configuration for the load testing library
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Global settings
    #[serde(default)]
    pub global: GlobalConfig,

    /// HTTP client settings
    #[serde(default)]
    pub http: HttpConfig,

    /// Scenarios
    #[serde(default)]
    pub scenarios: HashMap<String, ScenarioConfig>,

    /// Steps
    #[serde(default)]
    pub steps: HashMap<String, StepConfig>,

    /// Dynamic data sources available to scenarios.
    #[serde(default)]
    pub data_sources: HashMap<String, DataSource>,

    /// Metrics settings
    #[serde(default)]
    pub metrics: MetricsConfig,

    /// Telemetry settings
    #[serde(default)]
    pub telemetry: TelemetryConfig,

    /// Pass/fail thresholds for gating a run (used by the CLI exit code).
    #[serde(default)]
    pub thresholds: ThresholdsConfig,

    /// Directory used to resolve relative data-source paths. Set by
    /// `from_toml`/`from_yaml`; string/programmatic configs default to cwd.
    #[serde(skip)]
    pub source_dir: Option<PathBuf>,
}

/// Summary produced by [`Config::dynamic_lint_report`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct DynamicLintReport {
    /// Number of configured data sources.
    pub data_sources: usize,
    /// Number of dynamic steps found across all scenarios.
    pub dynamic_steps: usize,
    /// Number of template expressions analyzed.
    pub templates: usize,
    /// Number of extractors analyzed.
    pub extractors: usize,
    /// Number of branch conditions analyzed.
    pub branches: usize,
}

impl Config {
    /// Create a new default configuration
    pub fn new() -> Self {
        Self::default()
    }

    /// Load configuration from a TOML file
    pub fn from_toml<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let content = fs::read_to_string(path)
            .map_err(|e| Error::config(format!("Failed to read config file: {e}")))?;

        let mut config = Self::from_toml_str(&content)?;
        config.source_dir = Some(config_source_dir(path)?);
        Ok(config)
    }

    /// Load configuration from a TOML string
    pub fn from_toml_str(content: &str) -> Result<Self> {
        toml::from_str(content).map_err(|e| Error::config(format!("Failed to parse TOML: {e}")))
    }

    /// Save configuration to a TOML file
    pub fn to_toml<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let content = toml::to_string_pretty(self)
            .map_err(|e| Error::config(format!("Failed to serialize config: {e}")))?;

        fs::write(path, content)
            .map_err(|e| Error::config(format!("Failed to write config file: {e}")))
    }

    /// Load configuration from a YAML file
    pub fn from_yaml<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let content = fs::read_to_string(path)
            .map_err(|e| Error::config(format!("Failed to read config file: {e}")))?;

        let mut config = Self::from_yaml_str(&content)?;
        config.source_dir = Some(config_source_dir(path)?);
        Ok(config)
    }

    /// Load configuration from a YAML string
    pub fn from_yaml_str(content: &str) -> Result<Self> {
        serde_yaml::from_str(content)
            .map_err(|e| Error::config(format!("Failed to parse YAML: {e}")))
    }

    /// Save configuration to a YAML file
    pub fn to_yaml<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let content = serde_yaml::to_string(self)
            .map_err(|e| Error::config(format!("Failed to serialize config: {e}")))?;

        fs::write(path, content)
            .map_err(|e| Error::config(format!("Failed to write config file: {e}")))
    }

    /// Return this config with an explicit source directory for relative
    /// data-source paths.
    pub fn with_source_dir<P: Into<PathBuf>>(mut self, source_dir: P) -> Self {
        self.source_dir = Some(source_dir.into());
        self
    }

    /// Set the source directory for relative data-source paths.
    pub fn set_source_dir<P: Into<PathBuf>>(&mut self, source_dir: P) -> &mut Self {
        self.source_dir = Some(source_dir.into());
        self
    }

    /// Directory used to resolve relative data-source paths.
    pub fn data_source_base_dir(&self) -> Result<PathBuf> {
        match &self.source_dir {
            Some(path) => Ok(path.clone()),
            None => std::env::current_dir()
                .map_err(|e| Error::config(format!("Failed to resolve current directory: {e}"))),
        }
    }

    /// Analyze dynamic scenario references and fixture sources without running
    /// load.
    pub fn dynamic_lint_report(&self) -> Result<DynamicLintReport> {
        analyze_dynamic_config(self)
    }

    /// Validate the loaded configuration without generating load.
    pub fn validate(&self) -> Result<()> {
        if self.scenarios.is_empty() {
            return Err(Error::config(
                "Configuration must define at least one scenario",
            ));
        }
        if self.global.virtual_users == 0 {
            return Err(Error::config("global.virtual_users must be positive"));
        }

        validate_headers("global.headers", &self.global.headers)?;

        if let Some(max_error_rate) = self.thresholds.max_error_rate
            && (!max_error_rate.is_finite() || !(0.0..=1.0).contains(&max_error_rate))
        {
            return Err(Error::config(
                "thresholds.max_error_rate must be finite and between 0.0 and 1.0",
            ));
        }

        if self.telemetry.enabled {
            match self.telemetry.exporter.to_lowercase().as_str() {
                "json" | "console" | "noop" | "none" => {}
                other => {
                    return Err(Error::config(format!(
                        "Unsupported telemetry exporter '{other}'"
                    )));
                }
            }
        }
        match self.telemetry.backpressure.to_lowercase().as_str() {
            "drop" | "block" => {}
            other => {
                return Err(Error::config(format!(
                    "Unsupported telemetry backpressure '{other}'"
                )));
            }
        }
        if self.telemetry.queue_capacity == 0 {
            return Err(Error::config("telemetry.queue_capacity must be positive"));
        }

        for (scenario_id, scenario) in &self.scenarios {
            if scenario.steps.is_empty() {
                return Err(Error::config(format!(
                    "Scenario '{scenario_id}' must contain at least one step"
                )));
            }
            if matches!(scenario.virtual_users, Some(0)) {
                return Err(Error::config(format!(
                    "Scenario '{scenario_id}' virtual_users must be positive"
                )));
            }
            if let Some(profile) = &scenario.load_profile {
                validate_load_profile(scenario_id, profile)?;
            }
            for step_id in &scenario.steps {
                if !self.steps.contains_key(step_id) {
                    return Err(Error::config(format!(
                        "Step '{step_id}' referenced in scenario '{scenario_id}' not found"
                    )));
                }
            }
        }

        for (step_id, step) in &self.steps {
            validate_headers(&format!("steps.{step_id}.headers"), &step.headers)?;
            validate_template(&format!("steps.{step_id}.url"), &step.url)?;
            if let Some(body) = &step.body {
                validate_template(&format!("steps.{step_id}.body"), body)?;
            }
            if let Some(json) = &step.json {
                validate_template(&format!("steps.{step_id}.json"), json)?;
                if !contains_template(json) {
                    serde_json::from_str::<serde::de::IgnoredAny>(json).map_err(|e| {
                        Error::config(format!("Invalid JSON for step '{step_id}': {e}"))
                    })?;
                }
            }
            for (key, value) in self.global.headers.iter().chain(step.headers.iter()) {
                validate_template(&format!("steps.{step_id}.headers.{key}"), value)?;
            }
            for extractor in &step.extractors {
                extractor.to_runtime().map_err(|e| {
                    Error::config(format!("Invalid extractor on step '{step_id}': {e}"))
                })?;
            }
            if let Some(branch) = &step.branch {
                branch.to_runtime().map_err(|e| {
                    Error::config(format!("Invalid branch on step '{step_id}': {e}"))
                })?;
            }
        }

        self.dynamic_lint_report()?;

        // Building scenarios validates URL resolution, dependency graphs,
        // methods, dynamic runtime structs, and scenario/stage semantics.
        self.build_scenarios().map(|_| ())
    }

    /// Build scenarios from the configuration
    pub fn build_scenarios(&self) -> Result<Vec<Scenario>> {
        let mut scenarios = Vec::new();
        let data_source_base_dir = self.data_source_base_dir()?;

        for (id, config) in &self.scenarios {
            let mut builder = ScenarioBuilder::new(id, &config.name);

            // Resolve each load parameter with scenario-overrides-global
            // semantics: a value specified on the scenario (`Some`) wins;
            // otherwise the `[global]` default applies. GlobalConfig keeps
            // concrete defaults, so a value unspecified everywhere still falls
            // to a sane default.
            let virtual_users = config.virtual_users.unwrap_or(self.global.virtual_users);
            let duration_seconds = config
                .duration_seconds
                .unwrap_or(self.global.duration_seconds);
            let ramp_up_seconds = config
                .ramp_up_seconds
                .unwrap_or(self.global.ramp_up_seconds);
            let think_time_ms = config.think_time_ms.unwrap_or(self.global.think_time_ms);

            // Set scenario properties
            builder = builder
                .virtual_users(virtual_users)
                .duration(Duration::from_secs(duration_seconds))
                .ramp_up(Duration::from_secs(ramp_up_seconds))
                .think_time(Duration::from_millis(think_time_ms));

            // Add metadata
            for (key, value) in &config.metadata {
                builder = builder.metadata(key, value);
            }
            if let Some(load_profile) = &config.load_profile {
                builder = builder.load_profile(load_profile.clone());
            }
            let scenario_data_refs = referenced_data_sources_for_scenario(self, id, config)?;
            if !scenario_data_refs.is_empty() {
                builder = builder.data_source_base_dir(data_source_base_dir.clone());
                let mut source_ids: Vec<_> = scenario_data_refs.into_iter().collect();
                source_ids.sort();
                for source_id in source_ids {
                    let data_source = self.data_sources.get(&source_id).ok_or_else(|| {
                        Error::config(format!(
                            "Scenario '{id}' references missing data source '{source_id}'"
                        ))
                    })?;
                    builder = builder.data_source(source_id, data_source.clone());
                }
            }

            // Add steps
            for step_id in &config.steps {
                if let Some(step_config) = self.steps.get(step_id) {
                    let step = self.build_step(step_id, step_config)?;
                    builder = builder.step(step);
                } else {
                    return Err(Error::config(format!(
                        "Step '{step_id}' referenced in scenario '{id}' not found"
                    )));
                }
            }

            // Build the scenario
            let scenario = builder.build()?;
            scenarios.push(scenario);
        }

        Ok(scenarios)
    }

    /// Build a step from the configuration
    fn build_step(&self, id: &str, config: &StepConfig) -> Result<Step> {
        // Resolve the effective request URL against the global base URL.
        // With an empty base_url (the default) the step URL is used as-is and
        // must itself be an absolute URL; otherwise it is joined onto base_url
        // using RFC 3986 semantics (an absolute step URL overrides the base).
        let url_has_template = contains_template(&config.url);
        let effective_url = if self.global.base_url.is_empty() {
            config.url.clone()
        } else if url_has_template {
            join_url_template(&self.global.base_url, &config.url)?
        } else {
            let base = Url::parse(&self.global.base_url).map_err(|e| {
                Error::config(format!("Invalid base_url '{}': {e}", self.global.base_url))
            })?;
            base.join(&config.url)
                .map_err(|e| {
                    Error::config(format!(
                        "Failed to join base_url '{}' with step '{}' url '{}': {e}",
                        self.global.base_url, id, config.url
                    ))
                })?
                .to_string()
        };

        let method = parse_method(&config.method, id)?;

        let needs_dynamic = url_has_template
            || self.global.headers.values().any(|v| contains_template(v))
            || config.headers.values().any(|v| contains_template(v))
            || config.body.as_ref().is_some_and(|v| contains_template(v))
            || config.json.as_ref().is_some_and(|v| contains_template(v))
            || !config.extractors.is_empty()
            || config.branch.is_some();

        let static_url = if needs_dynamic && contains_template(&effective_url) {
            "http://example.invalid/".to_string()
        } else {
            effective_url.clone()
        };

        // Create the request
        let request = crate::http::Request::request(method.clone(), &static_url);

        // Resolve the effective timeout: the step value wins when specified,
        // otherwise the `[global]` default applies (step overrides global default).
        let timeout_ms = config.timeout_ms.unwrap_or(self.global.timeout_ms);

        // Add headers, body, etc.
        let request = request
            .timeout(Duration::from_millis(timeout_ms))
            .follow_redirects(config.follow_redirects);

        // Apply the global default headers first, then per-step headers so a
        // step header overrides a global one on key collision (step overrides global default). These
        // are the authoritative per-request headers (they also seed the shared
        // client's default headers via ClientSpec, but applying them here lets
        // per-step overrides work).
        let mut request = request;
        if !needs_dynamic {
            for (key, value) in &self.global.headers {
                request = request.header(key, value);
            }
            for (key, value) in &config.headers {
                request = request.header(key, value);
            }
        }

        // Add body if present
        let request = if needs_dynamic {
            request
        } else if let Some(body) = &config.body {
            request.text(body)
        } else if let Some(json) = &config.json {
            // Parse the JSON string into a serde_json::Value
            let json_value: serde_json::Value = serde_json::from_str(json)
                .map_err(|e| Error::config(format!("Invalid JSON for step '{id}': {e}")))?;
            request.json(&json_value)
        } else {
            request
        };

        // Build the request
        let request = request.build()?;

        // Create the step
        let mut step = Step::new(id, &config.name, request);

        // Add dependencies
        for dep in &config.dependencies {
            step.add_dependency(dep);
        }

        // Set properties
        step.with_max_retries(config.max_retries)
            .with_retry_delay(Duration::from_millis(config.retry_delay_ms))
            .with_timeout(Duration::from_millis(timeout_ms))
            .with_weight(config.weight);

        // Add metadata
        for (key, value) in &config.metadata {
            step.with_metadata(key, value);
        }

        if needs_dynamic {
            let mut header_templates = self.global.headers.clone();
            for (key, value) in &config.headers {
                header_templates.insert(key.clone(), value.clone());
            }
            let body_template = if let Some(body) = &config.body {
                Some(DynamicBodyTemplate::Text(body.clone()))
            } else {
                config
                    .json
                    .as_ref()
                    .map(|json| DynamicBodyTemplate::Json(json.clone()))
            };
            step.with_dynamic_request(DynamicRequestSpec {
                method,
                url_template: effective_url,
                header_templates,
                body_template,
                follow_redirects: config.follow_redirects,
            });

            for extractor in &config.extractors {
                step.with_extractor(extractor.to_runtime()?);
            }
            if let Some(branch) = &config.branch {
                step.with_branch(branch.to_runtime()?);
            }
        }

        Ok(step)
    }
}

/// Global configuration settings
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlobalConfig {
    /// Base URL for all requests
    #[serde(default)]
    pub base_url: String,

    /// Default timeout for all requests in milliseconds
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,

    /// Default think time between scenario iterations in milliseconds (a pause
    /// between sessions, never slept past the run deadline).
    #[serde(default)]
    pub think_time_ms: u64,

    /// Default number of virtual users
    #[serde(default = "default_virtual_users")]
    pub virtual_users: u32,

    /// Wall-clock duration of sustained load, in seconds. Each virtual user
    /// repeatedly executes the scenario graph until this much steady-state time
    /// has elapsed (after its ramp-up delay), so the test offers real sustained
    /// load for this long rather than a single pass. `0` runs the graph exactly
    /// once per VU.
    #[serde(default = "default_duration_seconds")]
    pub duration_seconds: u64,

    /// Default ramp-up period in seconds. This is additional to
    /// `duration_seconds`: it staggers VU start times, and each VU still gets a
    /// full `duration_seconds` of steady load after its ramp slot.
    #[serde(default)]
    pub ramp_up_seconds: u64,

    /// Default headers for all requests
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            timeout_ms: default_timeout_ms(),
            think_time_ms: 0,
            virtual_users: default_virtual_users(),
            duration_seconds: default_duration_seconds(),
            ramp_up_seconds: 0,
            headers: HashMap::new(),
        }
    }
}

/// HTTP client configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    /// Maximum number of connections per host
    #[serde(default = "default_max_connections_per_host")]
    pub max_connections_per_host: usize,

    /// Maximum number of simultaneously in-flight requests across all virtual
    /// users of a scenario. Decoupled from `max_connections_per_host` (a
    /// connection-pool knob): this bounds concurrent requests via a per-send
    /// permit. `0` (the default) means unlimited, so the virtual-user count is
    /// the sole concurrency bound.
    #[serde(default = "default_max_concurrent_requests")]
    pub max_concurrent_requests: usize,

    /// Connection timeout in milliseconds
    #[serde(default = "default_connection_timeout_ms")]
    pub connection_timeout_ms: u64,

    /// Pool idle timeout in seconds
    #[serde(default = "default_pool_idle_timeout_seconds")]
    pub pool_idle_timeout_seconds: u64,

    /// Whether to use HTTP/2
    #[serde(default = "default_use_http2")]
    pub use_http2: bool,

    /// Whether to verify SSL certificates
    #[serde(default = "default_verify_ssl")]
    pub verify_ssl: bool,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            max_connections_per_host: default_max_connections_per_host(),
            max_concurrent_requests: default_max_concurrent_requests(),
            connection_timeout_ms: default_connection_timeout_ms(),
            pool_idle_timeout_seconds: default_pool_idle_timeout_seconds(),
            use_http2: default_use_http2(),
            verify_ssl: default_verify_ssl(),
        }
    }
}

/// Scenario configuration
///
/// The load-parameter fields are `Option`: `None` (unspecified) inherits the
/// matching `[global]` value; `Some` overrides it. This makes the documented
/// "scenario overrides global when specified" behavior real.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioConfig {
    /// Name of the scenario
    pub name: String,

    /// Steps in this scenario
    #[serde(default)]
    pub steps: Vec<String>,

    /// Number of virtual users. `None` inherits `[global] virtual_users`.
    #[serde(default)]
    pub virtual_users: Option<u32>,

    /// Wall-clock duration of sustained load for this scenario, in seconds.
    /// Each virtual user repeats the scenario graph until this much steady-state
    /// time elapses (after its ramp-up delay). `0` runs the graph exactly once
    /// per VU. `None` inherits `[global] duration_seconds`.
    #[serde(default)]
    pub duration_seconds: Option<u64>,

    /// Ramp-up period in seconds. Additional to `duration_seconds`: it staggers
    /// VU start times without eating into each VU's steady-state budget. `None`
    /// inherits `[global] ramp_up_seconds`.
    #[serde(default)]
    pub ramp_up_seconds: Option<u64>,

    /// Think time between scenario iterations in milliseconds (a pause between
    /// sessions, never slept past the run deadline). `None` inherits
    /// `[global] think_time_ms`.
    #[serde(default)]
    pub think_time_ms: Option<u64>,

    /// Custom metadata
    #[serde(default)]
    pub metadata: HashMap<String, String>,

    /// Optional staged load profile. When omitted, the scenario's existing
    /// load fields form the single default stage.
    #[serde(default)]
    pub load_profile: Option<LoadProfile>,
}

/// Step configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepConfig {
    /// Name of the step
    pub name: String,

    /// HTTP method
    #[serde(default = "default_http_method")]
    pub method: String,

    /// URL
    pub url: String,

    /// HTTP headers
    #[serde(default)]
    pub headers: HashMap<String, String>,

    /// Request body as text
    #[serde(default)]
    pub body: Option<String>,

    /// Request body as JSON
    #[serde(default)]
    pub json: Option<String>,

    /// Dependencies on other steps
    #[serde(default)]
    pub dependencies: Vec<String>,

    /// Maximum number of retries
    #[serde(default)]
    pub max_retries: u32,

    /// Delay between retries in milliseconds
    #[serde(default = "default_retry_delay_ms")]
    pub retry_delay_ms: u64,

    /// Timeout in milliseconds. `None` inherits `[global] timeout_ms`.
    #[serde(default)]
    pub timeout_ms: Option<u64>,

    /// Scheduling/launch priority among simultaneously-ready steps (higher runs
    /// first). Does NOT change how often a step runs: every ready step runs
    /// exactly once per scenario iteration.
    #[serde(default = "default_weight")]
    pub weight: u32,

    /// Whether to follow redirects
    #[serde(default = "default_follow_redirects")]
    pub follow_redirects: bool,

    /// Custom metadata
    #[serde(default)]
    pub metadata: HashMap<String, String>,

    /// Response extractors that populate per-VU variables.
    #[serde(default)]
    pub extractors: Vec<ExtractorConfig>,

    /// Optional branch condition for conditional step execution.
    #[serde(default)]
    pub branch: Option<BranchConfig>,
}

/// Config-file representation of a response extractor.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractorConfig {
    /// Variable name to store.
    pub name: String,
    /// JSON dot path, e.g. `$.token` or `$.items[0].id`.
    #[serde(default)]
    pub json_path: Option<String>,
    /// Regex pattern. Uses capture group 1 when present, otherwise whole match.
    #[serde(default)]
    pub regex: Option<String>,
    /// Response header name for header extraction.
    #[serde(default)]
    pub header: Option<String>,
    /// Extract the numeric status code.
    #[serde(default)]
    pub status: bool,
    /// Whether missing extraction fails the attempt.
    #[serde(default = "default_true")]
    pub required: bool,
}

impl ExtractorConfig {
    fn to_runtime(&self) -> Result<Extractor> {
        if self.name.trim().is_empty() {
            return Err(Error::config("extractor name cannot be empty"));
        }

        let selected = self.json_path.is_some() as u8
            + self.status as u8
            + match (&self.regex, &self.header) {
                (Some(_), Some(_)) => 1,
                (Some(_), None) => 1,
                (None, Some(_)) => 1,
                (None, None) => 0,
            };
        if selected != 1 {
            return Err(Error::config(
                "extractor must specify exactly one source: json_path, regex, header, or status",
            ));
        }

        let mut extractor = if let Some(path) = &self.json_path {
            validate_json_path(path)?;
            Extractor::json_path(&self.name, path)
        } else if self.status {
            Extractor::status(&self.name)
        } else if let Some(regex) = &self.regex {
            regex::Regex::new(regex)
                .map_err(|e| Error::config(format!("invalid extractor regex: {e}")))?;
            if let Some(header_name) = &self.header {
                validate_header_name(header_name)?;
                Extractor::header_regex(&self.name, header_name, regex)
            } else {
                Extractor::body_regex(&self.name, regex)
            }
        } else if let Some(header_name) = &self.header {
            validate_header_name(header_name)?;
            Extractor::header(&self.name, header_name)
        } else {
            return Err(Error::config("extractor source missing"));
        };
        extractor.required = self.required;
        Ok(extractor)
    }
}

/// Config-file representation of a branch condition.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BranchConfig {
    /// Variable name to inspect.
    pub variable: String,
    /// Branch operator.
    pub condition: BranchOperator,
    /// Expected value for comparison operators.
    #[serde(default)]
    pub value: Option<String>,
}

impl BranchConfig {
    fn to_runtime(&self) -> Result<BranchCondition> {
        if self.variable.trim().is_empty() {
            return Err(Error::config("branch variable cannot be empty"));
        }
        match self.condition {
            BranchOperator::Exists => Ok(BranchCondition::exists(&self.variable)),
            BranchOperator::Equals => {
                let value = self
                    .value
                    .as_ref()
                    .ok_or_else(|| Error::config("branch condition 'equals' requires a value"))?;
                Ok(BranchCondition::equals(&self.variable, value))
            }
            BranchOperator::NotEquals => {
                let value = self.value.as_ref().ok_or_else(|| {
                    Error::config("branch condition 'not_equals' requires a value")
                })?;
                Ok(BranchCondition::not_equals(&self.variable, value))
            }
            BranchOperator::GreaterThan => {
                let value = validate_numeric_branch_value(self.condition, &self.value)?;
                Ok(BranchCondition::greater_than(&self.variable, value))
            }
            BranchOperator::GreaterThanOrEqual => {
                let value = validate_numeric_branch_value(self.condition, &self.value)?;
                Ok(BranchCondition::greater_than_or_equal(
                    &self.variable,
                    value,
                ))
            }
            BranchOperator::LessThan => {
                let value = validate_numeric_branch_value(self.condition, &self.value)?;
                Ok(BranchCondition::less_than(&self.variable, value))
            }
            BranchOperator::LessThanOrEqual => {
                let value = validate_numeric_branch_value(self.condition, &self.value)?;
                Ok(BranchCondition::less_than_or_equal(&self.variable, value))
            }
            BranchOperator::MatchesRegex => {
                let value = self.value.as_ref().ok_or_else(|| {
                    Error::config("branch condition 'matches_regex' requires a value")
                })?;
                regex::Regex::new(value)
                    .map_err(|e| Error::config(format!("invalid branch regex: {e}")))?;
                Ok(BranchCondition::matches_regex(&self.variable, value))
            }
        }
    }
}

/// Metrics configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsConfig {
    /// Whether to collect metrics. When `false`, the engine installs a no-op
    /// collector so a run records nothing and returns empty results.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
        }
    }
}

/// Pass/fail thresholds evaluated after a run to gate the CLI exit code. Every
/// field is optional; `None` disables that check.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ThresholdsConfig {
    /// Maximum tolerated error rate (0.0–1.0). A run above this fails.
    #[serde(default)]
    pub max_error_rate: Option<f64>,

    /// Maximum tolerated p90 response time in milliseconds.
    #[serde(default)]
    pub max_p90_ms: Option<u64>,

    /// Minimum number of requests the run must have issued.
    #[serde(default)]
    pub min_requests: Option<u64>,
}

/// Telemetry configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryConfig {
    /// Whether to export telemetry. Opt-in: defaults to `false` so a plain run
    /// does not attach an exporter (per-request export adds an await to the hot
    /// path). Set to `true` to have the engine build an exporter from
    /// `exporter`/`endpoint`/`service_name`/`custom`.
    #[serde(default)]
    pub enabled: bool,

    /// Exporter type. Implemented: `json` (newline-delimited JSON to stderr),
    /// `console`, `noop`. `otlp`/`prometheus` are accepted but currently error
    /// at build time (not yet implemented).
    #[serde(default = "default_exporter_type")]
    pub exporter: String,

    /// Exporter endpoint
    #[serde(default)]
    pub endpoint: String,

    /// Service name
    #[serde(default = "default_service_name")]
    pub service_name: String,

    /// Custom telemetry settings
    #[serde(default)]
    pub custom: HashMap<String, String>,

    /// Request telemetry backpressure behavior: `drop` or `block`.
    #[serde(default = "default_telemetry_backpressure")]
    pub backpressure: String,

    /// Bounded request telemetry queue capacity.
    #[serde(default = "default_telemetry_queue_capacity")]
    pub queue_capacity: usize,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            exporter: default_exporter_type(),
            endpoint: String::new(),
            service_name: default_service_name(),
            custom: HashMap::new(),
            backpressure: default_telemetry_backpressure(),
            queue_capacity: default_telemetry_queue_capacity(),
        }
    }
}

/// Builder for creating configurations
pub struct ConfigBuilder {
    config: Config,
}

impl Default for ConfigBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigBuilder {
    /// Create a new config builder
    pub fn new() -> Self {
        Self {
            config: Config::default(),
        }
    }

    /// Set the base URL
    pub fn base_url<S: Into<String>>(mut self, url: S) -> Self {
        self.config.global.base_url = url.into();
        self
    }

    /// Set the default timeout
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.config.global.timeout_ms = timeout.as_millis() as u64;
        self
    }

    /// Set the default think time
    pub fn think_time(mut self, think_time: Duration) -> Self {
        self.config.global.think_time_ms = think_time.as_millis() as u64;
        self
    }

    /// Set the default number of virtual users
    pub fn virtual_users(mut self, virtual_users: u32) -> Self {
        self.config.global.virtual_users = virtual_users;
        self
    }

    /// Set the default test duration
    pub fn duration(mut self, duration: Duration) -> Self {
        self.config.global.duration_seconds = duration.as_secs();
        self
    }

    /// Set the default ramp-up period
    pub fn ramp_up(mut self, ramp_up: Duration) -> Self {
        self.config.global.ramp_up_seconds = ramp_up.as_secs();
        self
    }

    /// Add a default header
    pub fn header<K, V>(mut self, key: K, value: V) -> Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        self.config.global.headers.insert(key.into(), value.into());
        self
    }

    /// Add a scenario
    pub fn scenario(mut self, id: &str, config: ScenarioConfig) -> Self {
        self.config.scenarios.insert(id.to_string(), config);
        self
    }

    /// Add a step
    pub fn step(mut self, id: &str, config: StepConfig) -> Self {
        self.config.steps.insert(id.to_string(), config);
        self
    }

    /// Add a dynamic data source.
    pub fn data_source(mut self, id: &str, data_source: DataSource) -> Self {
        self.config.data_sources.insert(id.to_string(), data_source);
        self
    }

    /// Set the base directory for relative data-source paths.
    pub fn source_dir<P: Into<PathBuf>>(mut self, source_dir: P) -> Self {
        self.config.source_dir = Some(source_dir.into());
        self
    }

    /// Set HTTP client settings
    pub fn http(mut self, http: HttpConfig) -> Self {
        self.config.http = http;
        self
    }

    /// Set metrics settings
    pub fn metrics(mut self, metrics: MetricsConfig) -> Self {
        self.config.metrics = metrics;
        self
    }

    /// Set telemetry settings
    pub fn telemetry(mut self, telemetry: TelemetryConfig) -> Self {
        self.config.telemetry = telemetry;
        self
    }

    /// Build the configuration
    pub fn build(self) -> Config {
        self.config
    }
}

fn config_source_dir(path: &Path) -> Result<PathBuf> {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => Ok(parent.to_path_buf()),
        _ => std::env::current_dir()
            .map_err(|e| Error::config(format!("Failed to resolve current directory: {e}"))),
    }
}

fn analyze_dynamic_config(config: &Config) -> Result<DynamicLintReport> {
    let base_dir = config.data_source_base_dir()?;
    for id in config.data_sources.keys() {
        validate_data_source_id(id)?;
    }
    let loaded_sources = LoadedDataSources::load(&config.data_sources, &base_dir)?;
    let mut report = DynamicLintReport {
        data_sources: config.data_sources.len(),
        ..DynamicLintReport::default()
    };

    for (scenario_id, scenario) in &config.scenarios {
        let mut scenario_data_refs = std::collections::HashSet::new();

        for step_id in &scenario.steps {
            let step = config.steps.get(step_id).ok_or_else(|| {
                Error::config(format!(
                    "Step '{step_id}' referenced in scenario '{scenario_id}' not found"
                ))
            })?;
            let extractor_vars =
                available_extractor_vars_for_step(config, scenario_id, scenario, step_id)?;
            let mut dynamic_step = false;

            report.templates += analyze_template_value(
                &mut scenario_data_refs,
                &extractor_vars,
                &loaded_sources,
                &format!("steps.{step_id}.url"),
                &step.url,
            )?;
            dynamic_step |= contains_template(&step.url);

            if let Some(body) = &step.body {
                report.templates += analyze_template_value(
                    &mut scenario_data_refs,
                    &extractor_vars,
                    &loaded_sources,
                    &format!("steps.{step_id}.body"),
                    body,
                )?;
                dynamic_step |= contains_template(body);
            }
            if let Some(json) = &step.json {
                report.templates += analyze_template_value(
                    &mut scenario_data_refs,
                    &extractor_vars,
                    &loaded_sources,
                    &format!("steps.{step_id}.json"),
                    json,
                )?;
                dynamic_step |= contains_template(json);
            }
            for (key, value) in config.global.headers.iter().chain(step.headers.iter()) {
                report.templates += analyze_template_value(
                    &mut scenario_data_refs,
                    &extractor_vars,
                    &loaded_sources,
                    &format!("steps.{step_id}.headers.{key}"),
                    value,
                )?;
                dynamic_step |= contains_template(value);
            }

            for extractor in &step.extractors {
                extractor.to_runtime().map_err(|e| {
                    Error::config(format!("Invalid extractor on step '{step_id}': {e}"))
                })?;
                report.extractors += 1;
                dynamic_step = true;
            }

            if let Some(branch) = &step.branch {
                branch.to_runtime().map_err(|e| {
                    Error::config(format!("Invalid branch on step '{step_id}': {e}"))
                })?;
                validate_branch_variable(
                    &mut scenario_data_refs,
                    &extractor_vars,
                    &loaded_sources,
                    scenario_id,
                    step_id,
                    &branch.variable,
                )?;
                report.branches += 1;
                dynamic_step = true;
            }

            if dynamic_step {
                report.dynamic_steps += 1;
            }
        }

        validate_per_vu_capacity(config, scenario, &loaded_sources, &scenario_data_refs)?;
    }

    Ok(report)
}

fn referenced_data_sources_for_scenario(
    config: &Config,
    scenario_id: &str,
    scenario: &ScenarioConfig,
) -> Result<HashSet<String>> {
    let mut data_refs = HashSet::new();
    for step_id in &scenario.steps {
        let step = config
            .steps
            .get(step_id)
            .ok_or_else(|| Error::config(format!("Step '{step_id}' not found")))?;

        collect_data_refs_from_template(
            &mut data_refs,
            &format!("steps.{step_id}.url"),
            &step.url,
        )?;
        if let Some(body) = &step.body {
            collect_data_refs_from_template(
                &mut data_refs,
                &format!("steps.{step_id}.body"),
                body,
            )?;
        }
        if let Some(json) = &step.json {
            collect_data_refs_from_template(
                &mut data_refs,
                &format!("steps.{step_id}.json"),
                json,
            )?;
        }
        for (key, value) in config.global.headers.iter().chain(step.headers.iter()) {
            collect_data_refs_from_template(
                &mut data_refs,
                &format!("steps.{step_id}.headers.{key}"),
                value,
            )?;
        }
        if let Some(branch) = &step.branch {
            collect_data_ref_from_expr(
                &mut data_refs,
                &format!("branch on step '{step_id}' in scenario '{scenario_id}'"),
                &branch.variable,
            )?;
        }
    }
    Ok(data_refs)
}

fn collect_data_refs_from_template(
    data_refs: &mut HashSet<String>,
    label: &str,
    value: &str,
) -> Result<()> {
    for expr in template_expressions(label, value)? {
        collect_data_ref_from_expr(data_refs, label, &expr)?;
    }
    Ok(())
}

fn collect_data_ref_from_expr(
    data_refs: &mut HashSet<String>,
    label: &str,
    expr: &str,
) -> Result<bool> {
    let Some((source_id, _path)) = parse_data_reference(label, expr)? else {
        return Ok(false);
    };
    data_refs.insert(source_id.to_string());
    Ok(true)
}

fn available_extractor_vars_for_step(
    config: &Config,
    scenario_id: &str,
    scenario: &ScenarioConfig,
    step_id: &str,
) -> Result<HashSet<String>> {
    let step = config.steps.get(step_id).ok_or_else(|| {
        Error::config(format!(
            "Step '{step_id}' referenced in scenario '{scenario_id}' not found"
        ))
    })?;
    let scenario_step_ids: HashSet<&str> = scenario.steps.iter().map(String::as_str).collect();
    let mut vars = HashSet::new();
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();

    for dep_id in &step.dependencies {
        collect_available_extractors_from_dependency(
            config,
            scenario_id,
            &scenario_step_ids,
            dep_id,
            &mut visiting,
            &mut visited,
            &mut vars,
        )?;
    }

    Ok(vars)
}

fn collect_available_extractors_from_dependency(
    config: &Config,
    scenario_id: &str,
    scenario_step_ids: &HashSet<&str>,
    step_id: &str,
    visiting: &mut HashSet<String>,
    visited: &mut HashSet<String>,
    vars: &mut HashSet<String>,
) -> Result<()> {
    if visited.contains(step_id) {
        return Ok(());
    }
    if !scenario_step_ids.contains(step_id) {
        return Err(Error::config(format!(
            "Step '{step_id}' is referenced as a dependency in scenario '{scenario_id}' but is not part of that scenario"
        )));
    }
    if !visiting.insert(step_id.to_string()) {
        return Err(Error::config(format!(
            "Dependency cycle detected in scenario '{scenario_id}' involving step '{step_id}'"
        )));
    }

    let step = config.steps.get(step_id).ok_or_else(|| {
        Error::config(format!(
            "Step '{step_id}' referenced in scenario '{scenario_id}' not found"
        ))
    })?;
    for dep_id in &step.dependencies {
        collect_available_extractors_from_dependency(
            config,
            scenario_id,
            scenario_step_ids,
            dep_id,
            visiting,
            visited,
            vars,
        )?;
    }

    visiting.remove(step_id);
    visited.insert(step_id.to_string());

    if step.branch.is_none() {
        for extractor in &step.extractors {
            if extractor.name.trim().is_empty() {
                return Err(Error::config(format!(
                    "extractor on step '{step_id}' has an empty name"
                )));
            }
            if extractor.required {
                vars.insert(extractor.name.clone());
            }
        }
    }

    Ok(())
}

fn analyze_template_value(
    data_refs: &mut HashSet<String>,
    extractor_vars: &HashSet<String>,
    loaded_sources: &LoadedDataSources,
    label: &str,
    value: &str,
) -> Result<usize> {
    let expressions = template_expressions(label, value)?;
    for expr in &expressions {
        validate_template_expr(data_refs, extractor_vars, loaded_sources, label, expr)?;
    }
    Ok(expressions.len())
}

fn validate_template_expr(
    data_refs: &mut HashSet<String>,
    extractor_vars: &HashSet<String>,
    loaded_sources: &LoadedDataSources,
    label: &str,
    expr: &str,
) -> Result<()> {
    if validate_builtin_expr(expr)? {
        return Ok(());
    }
    if validate_data_expr(data_refs, loaded_sources, label, expr)? {
        return Ok(());
    }
    let variable = expr.strip_prefix("var.").unwrap_or(expr);
    if extractor_vars.contains(variable) {
        return Ok(());
    }
    Err(Error::config(format!(
        "Template expression '{{{{{expr}}}}}' in {label} references unknown variable '{variable}'"
    )))
}

fn validate_branch_variable(
    data_refs: &mut HashSet<String>,
    extractor_vars: &HashSet<String>,
    loaded_sources: &LoadedDataSources,
    scenario_id: &str,
    step_id: &str,
    variable: &str,
) -> Result<()> {
    if validate_builtin_expr(variable)? {
        return Ok(());
    }
    if validate_data_expr(data_refs, loaded_sources, "branch variable", variable)? {
        return Ok(());
    }
    let variable_name = variable.strip_prefix("var.").unwrap_or(variable);
    if extractor_vars.contains(variable_name) {
        return Ok(());
    }
    Err(Error::config(format!(
        "Branch on step '{step_id}' in scenario '{scenario_id}' references unknown variable '{variable}'"
    )))
}

fn validate_data_expr(
    data_refs: &mut HashSet<String>,
    loaded_sources: &LoadedDataSources,
    label: &str,
    expr: &str,
) -> Result<bool> {
    let Some((source_id, path)) = parse_data_reference(label, expr)? else {
        return Ok(false);
    };
    if !loaded_sources.has_source(source_id) {
        return Err(Error::config(format!(
            "Data template expression '{expr}' in {label} references missing data source '{source_id}'"
        )));
    }
    if !loaded_sources.path_exists_for_every_row(source_id, path) {
        return Err(Error::config(format!(
            "Data template expression '{expr}' in {label} references path '{path}' that is missing from one or more rows in data source '{source_id}'"
        )));
    }
    data_refs.insert(source_id.to_string());
    Ok(true)
}

fn parse_data_reference<'a>(label: &str, expr: &'a str) -> Result<Option<(&'a str, &'a str)>> {
    let Some(rest) = expr.strip_prefix("data.") else {
        return Ok(None);
    };
    let (source_id, path) = rest.split_once('.').ok_or_else(|| {
        Error::config(format!(
            "Data template expression '{expr}' in {label} must use data.<source>.<path>"
        ))
    })?;
    validate_data_source_id(source_id)?;
    validate_relative_json_path(path)?;
    Ok(Some((source_id, path)))
}

fn validate_builtin_expr(expr: &str) -> Result<bool> {
    match expr {
        "vu.id" | "scenario.id" | "step.id" | "iteration" | "uuid" | "random.u64" => Ok(true),
        _ if expr.starts_with("random.int:") => {
            let parts: Vec<&str> = expr.split(':').collect();
            if parts.len() != 3 {
                return Err(Error::config(
                    "random.int templates must use random.int:<min>:<max>",
                ));
            }
            let min = parts[1]
                .parse::<i64>()
                .map_err(|e| Error::config(format!("invalid random.int min: {e}")))?;
            let max = parts[2]
                .parse::<i64>()
                .map_err(|e| Error::config(format!("invalid random.int max: {e}")))?;
            if min > max {
                return Err(Error::config("random.int min must be <= max"));
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn validate_numeric_branch_value(
    operator: BranchOperator,
    value: &Option<String>,
) -> Result<&String> {
    let value = value.as_ref().ok_or_else(|| {
        Error::config(format!(
            "branch condition '{}' requires a value",
            branch_operator_name(operator)
        ))
    })?;
    let parsed = value.parse::<f64>().map_err(|e| {
        Error::config(format!(
            "branch condition '{}' requires a numeric value: {e}",
            branch_operator_name(operator)
        ))
    })?;
    if !parsed.is_finite() {
        return Err(Error::config(format!(
            "branch condition '{}' requires a finite numeric value",
            branch_operator_name(operator)
        )));
    }
    Ok(value)
}

fn branch_operator_name(operator: BranchOperator) -> &'static str {
    match operator {
        BranchOperator::Exists => "exists",
        BranchOperator::Equals => "equals",
        BranchOperator::NotEquals => "not_equals",
        BranchOperator::GreaterThan => "greater_than",
        BranchOperator::GreaterThanOrEqual => "greater_than_or_equal",
        BranchOperator::LessThan => "less_than",
        BranchOperator::LessThanOrEqual => "less_than_or_equal",
        BranchOperator::MatchesRegex => "matches_regex",
    }
}

fn validate_per_vu_capacity(
    config: &Config,
    scenario: &ScenarioConfig,
    loaded_sources: &LoadedDataSources,
    data_refs: &std::collections::HashSet<String>,
) -> Result<()> {
    let required_rows = scenario_max_virtual_users(config, scenario);
    for source_id in data_refs {
        let Some(source) = config.data_sources.get(source_id) else {
            continue;
        };
        if source.access == DataAccessMode::PerVu && source.exhaustion == DataExhaustion::Fail {
            let row_count = loaded_sources.row_count(source_id).unwrap_or_default();
            if row_count < required_rows as usize {
                return Err(Error::config(format!(
                    "data source '{source_id}' uses per_vu/fail but has {row_count} row(s) for {required_rows} virtual user(s)"
                )));
            }
        }
    }
    Ok(())
}

fn scenario_max_virtual_users(config: &Config, scenario: &ScenarioConfig) -> u32 {
    let scenario_users = scenario
        .virtual_users
        .unwrap_or(config.global.virtual_users);
    let mut max_users = scenario_users;
    if let Some(profile) = &scenario.load_profile {
        for stage in &profile.stages {
            max_users = max_users.max(stage.virtual_users.unwrap_or(scenario_users));
        }
    }
    max_users
}

fn parse_method(method: &str, step_id: &str) -> Result<Method> {
    if method.eq_ignore_ascii_case("GET") {
        return Ok(Method::GET);
    }
    if method.eq_ignore_ascii_case("POST") {
        return Ok(Method::POST);
    }
    if method.eq_ignore_ascii_case("PUT") {
        return Ok(Method::PUT);
    }
    if method.eq_ignore_ascii_case("DELETE") {
        return Ok(Method::DELETE);
    }
    if method.eq_ignore_ascii_case("PATCH") {
        return Ok(Method::PATCH);
    }
    if method.eq_ignore_ascii_case("HEAD") {
        return Ok(Method::HEAD);
    }
    if method.eq_ignore_ascii_case("OPTIONS") {
        return Ok(Method::OPTIONS);
    }

    if method.bytes().any(|byte| byte.is_ascii_lowercase()) {
        return Err(Error::config(format!(
            "Custom HTTP method '{}' for step '{}' must be uppercase",
            method, step_id
        )));
    }

    Method::from_bytes(method.as_bytes()).map_err(|_| {
        Error::config(format!(
            "Invalid HTTP method '{}' for step '{}'",
            method, step_id
        ))
    })
}

fn contains_template(value: &str) -> bool {
    value.contains("{{") || value.contains("}}")
}

fn validate_template(label: &str, value: &str) -> Result<()> {
    template_expressions(label, value).map(|_| ())
}

fn template_expressions(label: &str, value: &str) -> Result<Vec<String>> {
    let mut expressions = Vec::new();
    let mut rest = value;
    while let Some(start) = rest.find("{{") {
        let after_start = &rest[start + 2..];
        let end = after_start
            .find("}}")
            .ok_or_else(|| Error::config(format!("Unclosed template expression in {label}")))?;
        let name = after_start[..end].trim();
        if name.is_empty() {
            return Err(Error::config(format!(
                "Empty template expression in {label}"
            )));
        }
        expressions.push(name.to_string());
        rest = &after_start[end + 2..];
    }
    if rest.contains("}}") {
        return Err(Error::config(format!(
            "Unopened template expression in {label}"
        )));
    }
    Ok(expressions)
}

fn validate_header_name(name: &str) -> Result<()> {
    header::HeaderName::from_bytes(name.as_bytes())
        .map(|_| ())
        .map_err(|e| Error::config(format!("invalid header name '{name}': {e}")))
}

fn validate_headers(label: &str, headers: &HashMap<String, String>) -> Result<()> {
    for (key, value) in headers {
        validate_header_name(key).map_err(|e| Error::config(format!("{label}: {e}")))?;
        if !contains_template(value) {
            header::HeaderValue::from_str(value).map_err(|e| {
                Error::config(format!("{label}: invalid value for header '{key}': {e}"))
            })?;
        }
    }
    Ok(())
}

fn validate_load_profile(scenario_id: &str, profile: &LoadProfile) -> Result<()> {
    if profile.stages.is_empty() {
        return Err(Error::config(format!(
            "Scenario '{scenario_id}' load_profile.stages must not be empty"
        )));
    }
    for stage in &profile.stages {
        if stage.duration_seconds == 0 {
            return Err(Error::config(format!(
                "Scenario '{scenario_id}' load stage duration_seconds must be positive"
            )));
        }
        if matches!(stage.virtual_users, Some(0)) {
            return Err(Error::config(format!(
                "Scenario '{scenario_id}' load stage virtual_users must be positive"
            )));
        }
        if let Some(target_rps) = stage.target_rps
            && (!target_rps.is_finite() || target_rps <= 0.0)
        {
            return Err(Error::config(format!(
                "Scenario '{scenario_id}' load stage target_rps must be finite and positive"
            )));
        }
    }
    Ok(())
}

fn join_url_template(base_url: &str, url: &str) -> Result<String> {
    if url.starts_with("http://") || url.starts_with("https://") {
        return Ok(url.to_string());
    }
    let base = Url::parse(base_url)
        .map_err(|e| Error::config(format!("Invalid base_url '{base_url}': {e}")))?;
    let mut joined = base.to_string();
    if !joined.ends_with('/') && !url.starts_with('/') {
        joined.push('/');
    }
    if joined.ends_with('/') && url.starts_with('/') {
        joined.pop();
    }
    joined.push_str(url);
    Ok(joined)
}

// Default values for configuration
fn default_timeout_ms() -> u64 {
    30000
}

fn default_virtual_users() -> u32 {
    1
}

fn default_duration_seconds() -> u64 {
    60
}

fn default_max_connections_per_host() -> usize {
    100
}

fn default_max_concurrent_requests() -> usize {
    0
}

fn default_connection_timeout_ms() -> u64 {
    5000
}

fn default_pool_idle_timeout_seconds() -> u64 {
    30
}

fn default_use_http2() -> bool {
    false
}

fn default_verify_ssl() -> bool {
    true
}

fn default_http_method() -> String {
    "GET".to_string()
}

fn default_retry_delay_ms() -> u64 {
    100
}

fn default_weight() -> u32 {
    1
}

fn default_follow_redirects() -> bool {
    true
}

fn default_true() -> bool {
    true
}

fn default_exporter_type() -> String {
    // Default to the one exporter that is actually implemented out of the box,
    // so an explicit `otlp`/`prometheus` choice errors only when the user asks
    // for it rather than by default (explicit exporter choice fails loudly).
    "json".to_string()
}

fn default_service_name() -> String {
    "load-tester".to_string()
}

fn default_telemetry_backpressure() -> String {
    "drop".to_string()
}

fn default_telemetry_queue_capacity() -> usize {
    1024
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_config_builder() {
        let config = ConfigBuilder::new()
            .base_url("https://example.com")
            .timeout(Duration::from_secs(10))
            .virtual_users(10)
            .duration(Duration::from_secs(300))
            .header("User-Agent", "load-tester")
            .build();

        assert_eq!(config.global.base_url, "https://example.com");
        assert_eq!(config.global.timeout_ms, 10000);
        assert_eq!(config.global.virtual_users, 10);
        assert_eq!(config.global.duration_seconds, 300);
        assert_eq!(
            config.global.headers.get("User-Agent").unwrap(),
            "load-tester"
        );
    }

    #[test]
    fn test_toml_serialization() {
        let config = ConfigBuilder::new()
            .base_url("https://example.com")
            .virtual_users(10)
            .build();

        let file = NamedTempFile::new().unwrap();
        config.to_toml(file.path()).unwrap();

        let loaded_config = Config::from_toml(file.path()).unwrap();
        assert_eq!(loaded_config.global.base_url, "https://example.com");
        assert_eq!(loaded_config.global.virtual_users, 10);
    }

    #[test]
    fn test_from_toml_str() {
        let toml_str = r#"
            [global]
            base_url = "https://example.com"
            virtual_users = 10
            
            [scenarios.login]
            name = "Login Scenario"
            steps = ["login"]
            virtual_users = 5
            
            [steps.login]
            name = "Login Step"
            method = "POST"
            url = "/login"
            json = '{"username": "test", "password": "password"}'
        "#;

        let config = Config::from_toml_str(toml_str).unwrap();
        assert_eq!(config.global.base_url, "https://example.com");
        assert_eq!(config.global.virtual_users, 10);

        let scenario = config.scenarios.get("login").unwrap();
        assert_eq!(scenario.name, "Login Scenario");
        // Specified on the scenario => Some(5), overriding global.
        assert_eq!(scenario.virtual_users, Some(5));
        assert_eq!(scenario.steps, vec!["login"]);

        let step = config.steps.get("login").unwrap();
        assert_eq!(step.name, "Login Step");
        assert_eq!(step.method, "POST");
        assert_eq!(step.url, "/login");
        assert_eq!(
            step.json,
            Some(r#"{"username": "test", "password": "password"}"#.to_string())
        );
    }

    #[test]
    fn test_build_scenarios() {
        let toml_str = r#"
            [global]
            base_url = "https://example.com"
            
            [scenarios.simple]
            name = "Simple Scenario"
            steps = ["step1", "step2"]
            virtual_users = 5
            
            [steps.step1]
            name = "Step 1"
            method = "GET"
            url = "/api/resource"
            
            [steps.step2]
            name = "Step 2"
            method = "POST"
            url = "/api/resource"
            dependencies = ["step1"]
            json = '{"data": "value"}'
        "#;

        let config = Config::from_toml_str(toml_str).unwrap();
        let scenarios = config.build_scenarios().unwrap();

        assert_eq!(scenarios.len(), 1);
        let scenario = &scenarios[0];
        assert_eq!(scenario.id, "simple");
        assert_eq!(scenario.name, "Simple Scenario");
        assert_eq!(scenario.virtual_users, 5);
        assert_eq!(scenario.steps.len(), 2);

        let step1 = scenario.get_step("step1").unwrap();
        assert_eq!(step1.name, "Step 1");
        assert_eq!(step1.request.method().as_str(), "GET");
        // The relative step URL is joined onto base_url, preserving the host
        // and the full path (previously mangled to host="api"/path="/resource").
        assert_eq!(step1.request.url().host_str(), Some("example.com"));
        assert_eq!(step1.request.url().path(), "/api/resource");
        assert!(step1.dependencies.is_empty());

        let step2 = scenario.get_step("step2").unwrap();
        assert_eq!(step2.name, "Step 2");
        assert_eq!(step2.request.method().as_str(), "POST");
        assert_eq!(step2.request.url().host_str(), Some("example.com"));
        assert_eq!(step2.request.url().path(), "/api/resource");
        assert!(step2.dependencies.contains("step1"));
    }

    #[test]
    fn test_build_step_absolute_url_overrides_base_url() {
        let toml_str = r#"
            [global]
            base_url = "https://example.com"

            [scenarios.simple]
            name = "Simple Scenario"
            steps = ["step1"]

            [steps.step1]
            name = "Step 1"
            method = "GET"
            url = "https://other.example.org/foo"
        "#;

        let config = Config::from_toml_str(toml_str).unwrap();
        let scenarios = config.build_scenarios().unwrap();
        let step1 = scenarios[0].get_step("step1").unwrap();
        // An absolute step URL wins over the base URL (RFC 3986 join).
        assert_eq!(step1.request.url().host_str(), Some("other.example.org"));
        assert_eq!(step1.request.url().path(), "/foo");
    }

    #[test]
    fn test_build_step_empty_base_url_requires_absolute() {
        // With no base_url, a relative step URL is invalid and must error
        // loudly rather than being rewritten to a garbage host.
        let toml_str = r#"
            [scenarios.simple]
            name = "Simple Scenario"
            steps = ["step1"]

            [steps.step1]
            name = "Step 1"
            method = "GET"
            url = "/api/resource"
        "#;

        let config = Config::from_toml_str(toml_str).unwrap();
        assert!(config.build_scenarios().is_err());
    }

    #[test]
    fn test_scenario_inherits_global_load_settings() {
        // A scenario that omits load settings inherits [global]; one that
        // specifies them overrides global load settings.
        let toml_str = r#"
            [global]
            base_url = "https://example.com"
            virtual_users = 5
            duration_seconds = 30
            ramp_up_seconds = 2
            think_time_ms = 7

            [scenarios.inherits]
            name = "Inherits"
            steps = ["s"]

            [scenarios.overrides]
            name = "Overrides"
            steps = ["s"]
            virtual_users = 3
            duration_seconds = 0

            [steps.s]
            name = "S"
            method = "GET"
            url = "/a"
        "#;

        let config = Config::from_toml_str(toml_str).unwrap();
        let scenarios = config.build_scenarios().unwrap();

        let inherits = scenarios.iter().find(|s| s.id == "inherits").unwrap();
        assert_eq!(inherits.virtual_users, 5);
        assert_eq!(inherits.duration, Duration::from_secs(30));
        assert_eq!(inherits.ramp_up, Duration::from_secs(2));
        assert_eq!(inherits.think_time, Duration::from_millis(7));

        let overrides = scenarios.iter().find(|s| s.id == "overrides").unwrap();
        assert_eq!(overrides.virtual_users, 3);
        assert_eq!(overrides.duration, Duration::from_secs(0));
        // Unspecified fields still inherit global.
        assert_eq!(overrides.ramp_up, Duration::from_secs(2));
        assert_eq!(overrides.think_time, Duration::from_millis(7));
    }

    #[test]
    fn test_step_inherits_global_timeout_and_headers() {
        // A step that omits timeout_ms inherits [global] timeout_ms; global
        // headers are applied to the built request, with a per-step header
        // overriding on collision (step overrides global default).
        let toml_str = r#"
            [global]
            base_url = "https://example.com"
            timeout_ms = 1234

            [global.headers]
            "X-Global" = "g"
            "X-Override" = "from-global"

            [scenarios.s]
            name = "S"
            steps = ["inherit", "explicit"]

            [steps.inherit]
            name = "Inherit"
            method = "GET"
            url = "/a"

            [steps.explicit]
            name = "Explicit"
            method = "GET"
            url = "/b"
            timeout_ms = 999
            [steps.explicit.headers]
            "X-Override" = "from-step"
        "#;

        let config = Config::from_toml_str(toml_str).unwrap();
        let scenarios = config.build_scenarios().unwrap();
        let scenario = &scenarios[0];

        let inherit = scenario.get_step("inherit").unwrap();
        assert_eq!(inherit.timeout, Duration::from_millis(1234));
        assert_eq!(inherit.request.timeout(), Duration::from_millis(1234));
        assert_eq!(inherit.request.headers().get("X-Global").unwrap(), "g");
        assert_eq!(
            inherit.request.headers().get("X-Override").unwrap(),
            "from-global"
        );

        let explicit = scenario.get_step("explicit").unwrap();
        // Explicit step timeout wins over global.
        assert_eq!(explicit.timeout, Duration::from_millis(999));
        // Global header still applied, but a per-step header overrides it.
        assert_eq!(explicit.request.headers().get("X-Global").unwrap(), "g");
        assert_eq!(
            explicit.request.headers().get("X-Override").unwrap(),
            "from-step"
        );
    }

    #[test]
    fn test_deny_unknown_fields_rejects_typo() {
        // A typo'd scenario key must be a hard parse error rather than a
        // silently-ignored field that runs at the wrong load (typo'd key is a hard parse error).
        let toml_str = r#"
            [scenarios.s]
            name = "S"
            steps = []
            virtual_userz = 50
        "#;
        assert!(Config::from_toml_str(toml_str).is_err());
    }

    #[test]
    fn test_zero_virtual_users_is_rejected() {
        // virtual_users = 0 must fail loudly instead of running a zero-request
        // no-op that reports success (zero virtual_users fails loudly).
        let toml_str = r#"
            [scenarios.s]
            name = "S"
            steps = ["s"]
            virtual_users = 0

            [steps.s]
            name = "S"
            method = "GET"
            url = "https://example.com/a"
        "#;
        let config = Config::from_toml_str(toml_str).unwrap();
        assert!(config.build_scenarios().is_err());
    }

    #[test]
    fn test_build_step_empty_base_url_absolute_ok() {
        let toml_str = r#"
            [scenarios.simple]
            name = "Simple Scenario"
            steps = ["step1"]

            [steps.step1]
            name = "Step 1"
            method = "GET"
            url = "https://example.com/api/resource"
        "#;

        let config = Config::from_toml_str(toml_str).unwrap();
        let scenarios = config.build_scenarios().unwrap();
        let step1 = scenarios[0].get_step("step1").unwrap();
        assert_eq!(step1.request.url().host_str(), Some("example.com"));
        assert_eq!(step1.request.url().path(), "/api/resource");
    }

    // -----------------------------------------------------------------------
    // YAML parsing coverage. The README leads with YAML and the
    // CLI dispatches `.yaml`/`.yml` through `Config::from_yaml_str`, yet every
    // other config test above exercises only TOML or the builder.
    // -----------------------------------------------------------------------

    #[test]
    fn test_from_yaml_str() {
        let yaml_str = r#"
global:
  base_url: "https://example.com"
  virtual_users: 10

scenarios:
  login:
    name: "Login Scenario"
    steps: ["login"]
    virtual_users: 5

steps:
  login:
    name: "Login Step"
    method: "POST"
    url: "/login"
    json: '{"username": "test", "password": "password"}'
"#;

        let config = Config::from_yaml_str(yaml_str).unwrap();
        assert_eq!(config.global.base_url, "https://example.com");
        assert_eq!(config.global.virtual_users, 10);

        let scenario = config.scenarios.get("login").unwrap();
        assert_eq!(scenario.name, "Login Scenario");
        assert_eq!(scenario.virtual_users, Some(5));
        assert_eq!(scenario.steps, vec!["login"]);

        let step = config.steps.get("login").unwrap();
        assert_eq!(step.name, "Login Step");
        assert_eq!(step.method, "POST");
        assert_eq!(step.url, "/login");
        assert_eq!(
            step.json,
            Some(r#"{"username": "test", "password": "password"}"#.to_string())
        );

        // The parsed YAML must also build into runnable scenarios.
        let scenarios = config.build_scenarios().unwrap();
        assert_eq!(scenarios.len(), 1);
        let built = scenarios[0].get_step("login").unwrap();
        assert_eq!(built.request.method().as_str(), "POST");
        assert_eq!(built.request.url().host_str(), Some("example.com"));
        assert_eq!(built.request.url().path(), "/login");
    }

    #[test]
    fn test_yaml_and_toml_parse_equivalently() {
        // The same configuration expressed in YAML and TOML must deserialize to
        // the same values, guarding against serde format divergences (enum
        // representation, field naming) on the documented primary path.
        let yaml_str = r#"
global:
  base_url: "https://example.com"
  virtual_users: 7
  duration_seconds: 42

scenarios:
  simple:
    name: "Simple Scenario"
    steps: ["step1", "step2"]
    virtual_users: 3

steps:
  step1:
    name: "Step 1"
    method: "GET"
    url: "/api/resource"
  step2:
    name: "Step 2"
    method: "POST"
    url: "/api/resource"
    dependencies: ["step1"]
"#;
        let toml_str = r#"
            [global]
            base_url = "https://example.com"
            virtual_users = 7
            duration_seconds = 42

            [scenarios.simple]
            name = "Simple Scenario"
            steps = ["step1", "step2"]
            virtual_users = 3

            [steps.step1]
            name = "Step 1"
            method = "GET"
            url = "/api/resource"

            [steps.step2]
            name = "Step 2"
            method = "POST"
            url = "/api/resource"
            dependencies = ["step1"]
        "#;

        let from_yaml = Config::from_yaml_str(yaml_str).unwrap();
        let from_toml = Config::from_toml_str(toml_str).unwrap();

        assert_eq!(from_yaml.global.base_url, from_toml.global.base_url);
        assert_eq!(
            from_yaml.global.virtual_users,
            from_toml.global.virtual_users
        );
        assert_eq!(
            from_yaml.global.duration_seconds,
            from_toml.global.duration_seconds
        );

        let y = from_yaml.scenarios.get("simple").unwrap();
        let t = from_toml.scenarios.get("simple").unwrap();
        assert_eq!(y.name, t.name);
        assert_eq!(y.steps, t.steps);
        assert_eq!(y.virtual_users, t.virtual_users);

        // The built scenarios must be indistinguishable as well.
        let ys = from_yaml.build_scenarios().unwrap();
        let ts = from_toml.build_scenarios().unwrap();
        assert_eq!(ys[0].steps.len(), ts[0].steps.len());
        let y2 = ys[0].get_step("step2").unwrap();
        let t2 = ts[0].get_step("step2").unwrap();
        assert_eq!(y2.request.method().as_str(), t2.request.method().as_str());
        assert!(y2.dependencies.contains("step1"));
        assert!(t2.dependencies.contains("step1"));
    }

    #[test]
    fn test_yaml_deny_unknown_fields_rejects_typo() {
        // `#[serde(deny_unknown_fields)]` must reject misspelled keys via the
        // YAML path too, so config typos fail loudly instead of being ignored.
        let yaml_str = r#"
global:
  base_url: "https://example.com"
  virtual_uzers: 10
"#;
        assert!(Config::from_yaml_str(yaml_str).is_err());
    }

    #[test]
    fn test_yaml_round_trip_via_to_string() {
        // A config serialized to YAML must parse back into an equivalent config,
        // exercising both `serde_yaml::to_string` and `from_str`.
        let toml_str = r#"
            [global]
            base_url = "https://example.com"
            virtual_users = 4

            [scenarios.s]
            name = "S"
            steps = ["a"]

            [steps.a]
            name = "A"
            method = "GET"
            url = "/a"
        "#;
        let original = Config::from_toml_str(toml_str).unwrap();

        let yaml = serde_yaml::to_string(&original).unwrap();
        let reparsed = Config::from_yaml_str(&yaml).unwrap();

        assert_eq!(reparsed.global.base_url, original.global.base_url);
        assert_eq!(reparsed.global.virtual_users, original.global.virtual_users);
        assert_eq!(
            reparsed.scenarios.get("s").unwrap().name,
            original.scenarios.get("s").unwrap().name
        );
        assert_eq!(
            reparsed.steps.get("a").unwrap().url,
            original.steps.get("a").unwrap().url
        );
    }
}
