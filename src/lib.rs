//! # Load Tester
//!
//! A high-throughput, multithreaded HTTP load testing library.
//!
//! This library allows users to define test scenarios in a declarative fashion,
//! either in code or via TOML configuration. It builds a dependency graph which
//! can be visualized and captures request metrics related to the performance of
//! the load test. Metrics can be streamed to a pluggable [`telemetry`]
//! exporter (a real newline-delimited JSON exporter ships in the box; embedders
//! can implement their own).
//!
//! # Examples
//!
//! ## Building and running a scenario in code
//!
//! Mirrors the "Basic Example" in the README, so that a `cargo test --doc`
//! failure signals README/API drift.
//!
//! ```no_run
//! use pummel::prelude::*;
//! use std::time::Duration;
//!
//! #[tokio::main]
//! async fn main() -> Result<()> {
//!     let step = StepBuilder::new(
//!         "get_homepage",
//!         "Get Homepage",
//!         Request::get("https://example.com").build()?,
//!     )
//!     .max_retries(3)
//!     .timeout(Duration::from_secs(5))
//!     .build();
//!
//!     let scenario = ScenarioBuilder::new("example_scenario", "Example Scenario")
//!         .step(step)
//!         .virtual_users(5)
//!         .duration(Duration::from_secs(30))
//!         .ramp_up(Duration::from_secs(5))
//!         .think_time(Duration::from_millis(500))
//!         .build()?;
//!
//!     let mut engine = Engine::new();
//!     engine.add_scenario(scenario);
//!     engine.with_telemetry_exporter(TelemetryExporterFactory::create_console());
//!
//!     let options = ExecutionOptions::builder()
//!         .virtual_users(5)
//!         .duration(Duration::from_secs(30))
//!         .ramp_up(Duration::from_secs(5))
//!         .think_time(Duration::from_millis(500))
//!         .max_concurrent_requests(10)
//!         .build();
//!
//!     let results = engine.run_all(options).await?;
//!     println!("Total Requests: {}", results.total_requests);
//!     Ok(())
//! }
//! ```
//!
//! ## Running from a configuration file
//!
//! Mirrors the README "Using Configuration Files" snippet. `Config::from_toml`
//! parses TOML instead.
//!
//! ```no_run
//! use pummel::prelude::*;
//!
//! #[tokio::main]
//! async fn main() -> Result<()> {
//!     let config = Config::from_yaml("config.yaml")?;
//!     let mut engine = Engine::new();
//!     let results = engine.run(&config).await?;
//!     println!("Test completed with {} requests", results.total_requests);
//!     Ok(())
//! }
//! ```
//!
//! ## Embedding in a test that asserts on the results
//!
//! The advertised "embed pummel in your own tests" workflow: run a scenario and
//! assert on the [`TestResults`](crate::metrics::TestResults) instead of merely
//! logging them.
//!
//! ```no_run
//! use pummel::prelude::*;
//! use std::time::Duration;
//!
//! # async fn embed() -> Result<()> {
//! let step = StepBuilder::new(
//!     "health",
//!     "Health Check",
//!     Request::get("http://127.0.0.1:8080/health").build()?,
//! )
//! .build();
//!
//! let scenario = ScenarioBuilder::new("smoke", "Smoke Test")
//!     .step(step)
//!     .virtual_users(4)
//!     .duration(Duration::from_secs(2))
//!     .build()?;
//!
//! let mut engine = Engine::new();
//! engine.add_scenario(scenario);
//! let results = engine.run_all(ExecutionOptions::builder().build()).await?;
//!
//! // Fail the test if the service degraded under load.
//! assert_eq!(results.error_rate, 0.0, "requests failed under load");
//! assert!(results.p90_response_time_ms < 500, "p90 latency regressed");
//! # Ok(())
//! # }
//! ```

pub mod config;
pub mod data;
pub mod engine;
pub mod error;
pub mod graph;
pub mod http;
pub mod logging;
pub mod metrics;
pub mod scenario;
pub mod telemetry;

pub use error::{Error, Result};

/// Re-export commonly used types and traits
pub mod prelude {
    pub use crate::config::{Config, ConfigBuilder, DynamicLintReport};
    pub use crate::data::{
        CsvColumnType, DataAccessMode, DataExhaustion, DataSource, DataSourceKind,
    };
    pub use crate::engine::{Engine, ExecutionOptions, ExecutionOptionsBuilder};
    pub use crate::error::{Error, Result};
    pub use crate::graph::{DependencyGraph, GraphFormat, GraphVisualizer};
    pub use crate::http::{HttpClient, Request, Response};
    pub use crate::logging;
    pub use crate::metrics::{MetricsCollector, MetricsCollectorFactory, RunStatus, TestResults};
    pub use crate::scenario::{
        BranchCondition, BranchOperator, DynamicBodyTemplate, DynamicRequestSpec, Extractor,
        ExtractorSource, LoadProfile, LoadStage, Scenario, ScenarioBuilder, Step, StepBuilder,
        VuContext,
    };
    pub use crate::telemetry::{TelemetryExporter, TelemetryExporterFactory};
}

/// Version of the library
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version() {
        // VERSION should be a valid semver string like "0.1.0"
        assert!(VERSION.len() >= 5); // At least "0.0.0"
        assert!(VERSION.contains('.'));
    }
}
