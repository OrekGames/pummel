use std::io;
use thiserror::Error;

/// Custom error type for the load testing library.
///
/// This enum is `#[non_exhaustive]`: matching it from outside the crate must
/// include a wildcard arm, so future variants can be added without a semver
/// break. The `#[from]` transport variants (`Toml`, `Http`, `Json`, `Io`)
/// deliberately expose their source error types to embedders; bumping those
/// dependencies' major versions is an accepted 0.1 tradeoff.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// Error when parsing configuration
    #[error("Configuration error: {0}")]
    Config(String),

    /// Error when parsing TOML
    #[error("TOML parsing error: {0}")]
    Toml(#[from] toml::de::Error),

    /// Error when making HTTP requests
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// Error when serializing or deserializing JSON
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// IO error
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    /// Error when building or executing scenarios
    #[error("Scenario error: {0}")]
    Scenario(String),

    /// Error when a response fails validation (a request was made and answered,
    /// but the response did not satisfy the step's validator / success check).
    ///
    /// Kept distinct from transport errors ([`Error::Http`]) so an embedding
    /// framework can tell "the server answered wrongly" from "the request never
    /// completed".
    #[error("Validation error: {0}")]
    Validation(String),

    /// Error when building or visualizing dependency graph
    #[error("Graph error: {0}")]
    Graph(String),

    /// Error when collecting or exporting metrics
    #[error("Metrics error: {0}")]
    Metrics(String),

    /// Error when setting up or using OpenTelemetry
    #[error("Telemetry error: {0}")]
    Telemetry(String),

    /// Error when running the load test engine
    #[error("Engine error: {0}")]
    Engine(String),

    /// Timeout error
    #[error("Timeout error: {0}")]
    Timeout(String),

    /// Generic error
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Create a new configuration error
    pub fn config<S: Into<String>>(msg: S) -> Self {
        Error::Config(msg.into())
    }

    /// Create a new scenario error
    pub fn scenario<S: Into<String>>(msg: S) -> Self {
        Error::Scenario(msg.into())
    }

    /// Create a new response-validation error
    pub fn validation<S: Into<String>>(msg: S) -> Self {
        Error::Validation(msg.into())
    }

    /// Create a new graph error
    pub fn graph<S: Into<String>>(msg: S) -> Self {
        Error::Graph(msg.into())
    }

    /// Create a new metrics error
    pub fn metrics<S: Into<String>>(msg: S) -> Self {
        Error::Metrics(msg.into())
    }

    /// Create a new telemetry error
    pub fn telemetry<S: Into<String>>(msg: S) -> Self {
        Error::Telemetry(msg.into())
    }

    /// Create a new engine error
    pub fn engine<S: Into<String>>(msg: S) -> Self {
        Error::Engine(msg.into())
    }

    /// Create a new timeout error
    pub fn timeout<S: Into<String>>(msg: S) -> Self {
        Error::Timeout(msg.into())
    }

    /// Create a new generic error
    pub fn other<S: Into<String>>(msg: S) -> Self {
        Error::Other(msg.into())
    }
}

/// Custom result type for the load testing library
pub type Result<T> = std::result::Result<T, Error>;
