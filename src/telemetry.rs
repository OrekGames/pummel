use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, error, info};

use crate::config;
use crate::error::{Error, Result};
use crate::metrics::{RequestMetrics, TestResults};

/// Telemetry exporter for the load testing library
#[async_trait]
pub trait TelemetryExporter: Send + Sync {
    /// Initialize the exporter
    async fn init(&self) -> Result<()>;

    /// Export request metrics
    async fn export_request(&self, metrics: &RequestMetrics) -> Result<()>;

    /// Export test results
    async fn export_results(&self, results: &TestResults) -> Result<()>;

    /// Shutdown the exporter
    async fn shutdown(&self) -> Result<()>;
}

/// Telemetry format
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TelemetryFormat {
    /// OpenTelemetry format (not yet implemented — selecting it errors)
    OpenTelemetry,
    /// Prometheus format (not yet implemented — selecting it errors)
    Prometheus,
    /// Newline-delimited JSON to stderr
    Json,
    /// Human-readable console output via the tracing logger
    Console,
    /// No-op (records/exports nothing)
    Noop,
}

impl fmt::Display for TelemetryFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TelemetryFormat::OpenTelemetry => write!(f, "opentelemetry"),
            TelemetryFormat::Prometheus => write!(f, "prometheus"),
            TelemetryFormat::Json => write!(f, "json"),
            TelemetryFormat::Console => write!(f, "console"),
            TelemetryFormat::Noop => write!(f, "noop"),
        }
    }
}

/// Configuration for building a telemetry exporter.
///
/// This is the exporter-side settings type consumed by
/// [`TelemetryExporterFactory::create`]. It is distinct from
/// [`crate::config::TelemetryConfig`] (the file-based `[telemetry]` section);
/// use the [`From`] bridge to convert the latter into this.
#[derive(Debug, Clone)]
pub struct ExporterConfig {
    /// Service name
    pub service_name: String,

    /// Endpoint URL
    pub endpoint: String,

    /// Format
    pub format: TelemetryFormat,

    /// Export timeout
    pub timeout: Duration,

    /// Additional attributes
    pub attributes: HashMap<String, String>,

    /// Request telemetry backpressure behavior: `drop` or `block`.
    pub backpressure: TelemetryBackpressure,

    /// Bounded request telemetry queue capacity.
    pub queue_capacity: usize,
}

impl Default for ExporterConfig {
    fn default() -> Self {
        Self {
            service_name: "pummel".to_string(),
            endpoint: "http://localhost:4317".to_string(),
            format: TelemetryFormat::Json,
            timeout: Duration::from_secs(10),
            attributes: HashMap::new(),
            backpressure: TelemetryBackpressure::Drop,
            queue_capacity: 1024,
        }
    }
}

/// Request telemetry queue backpressure behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TelemetryBackpressure {
    /// Drop request telemetry when the queue is full.
    Drop,
    /// Wait for queue capacity, applying backpressure to request generation.
    Block,
}

impl From<&config::TelemetryConfig> for ExporterConfig {
    /// Bridge the file-based `[telemetry]` section to exporter settings,
    /// mapping the `exporter` string to a [`TelemetryFormat`] and wiring the
    /// previously-dead `custom` map into exporter `attributes`.
    fn from(cfg: &config::TelemetryConfig) -> Self {
        let format = match cfg.exporter.to_lowercase().as_str() {
            "otlp" | "opentelemetry" => TelemetryFormat::OpenTelemetry,
            "prometheus" => TelemetryFormat::Prometheus,
            "console" => TelemetryFormat::Console,
            "noop" | "none" => TelemetryFormat::Noop,
            // `json` and any unrecognized value fall back to the one exporter
            // that is always available.
            _ => TelemetryFormat::Json,
        };

        Self {
            service_name: cfg.service_name.clone(),
            endpoint: cfg.endpoint.clone(),
            format,
            timeout: Duration::from_secs(10),
            attributes: cfg.custom.clone(),
            backpressure: match cfg.backpressure.to_lowercase().as_str() {
                "block" => TelemetryBackpressure::Block,
                _ => TelemetryBackpressure::Drop,
            },
            queue_capacity: cfg.queue_capacity,
        }
    }
}

/// Bounded background dispatcher for per-request telemetry.
pub struct BoundedTelemetryExporter {
    inner: Arc<dyn TelemetryExporter>,
    backpressure: TelemetryBackpressure,
    capacity: usize,
    sender: Mutex<Option<mpsc::Sender<RequestMetrics>>>,
    worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
    warned_full: AtomicBool,
}

impl BoundedTelemetryExporter {
    /// Create a bounded dispatcher around an exporter.
    pub fn new(
        inner: Arc<dyn TelemetryExporter>,
        backpressure: TelemetryBackpressure,
        capacity: usize,
    ) -> Self {
        Self {
            inner,
            backpressure,
            capacity: capacity.max(1),
            sender: Mutex::new(None),
            worker: Mutex::new(None),
            warned_full: AtomicBool::new(false),
        }
    }

    /// Create the default drop-on-full dispatcher.
    pub fn default_drop(inner: Arc<dyn TelemetryExporter>) -> Self {
        Self::new(inner, TelemetryBackpressure::Drop, 1024)
    }
}

#[async_trait]
impl TelemetryExporter for BoundedTelemetryExporter {
    async fn init(&self) -> Result<()> {
        self.inner.init().await?;

        let (tx, mut rx) = mpsc::channel::<RequestMetrics>(self.capacity);
        let inner = self.inner.clone();
        let handle = tokio::spawn(async move {
            while let Some(metrics) = rx.recv().await {
                if let Err(err) = inner.export_request(&metrics).await {
                    error!("Failed to export request telemetry: {err}");
                }
            }
        });

        *self.sender.lock().await = Some(tx);
        *self.worker.lock().await = Some(handle);
        Ok(())
    }

    async fn export_request(&self, metrics: &RequestMetrics) -> Result<()> {
        let sender = self.sender.lock().await.clone();
        let Some(sender) = sender else {
            return Ok(());
        };

        match self.backpressure {
            TelemetryBackpressure::Drop => match sender.try_send(metrics.clone()) {
                Ok(()) => Ok(()),
                Err(mpsc::error::TrySendError::Full(_)) => {
                    if !self.warned_full.swap(true, Ordering::Relaxed) {
                        error!("Telemetry queue full; dropping request telemetry");
                    }
                    Ok(())
                }
                Err(mpsc::error::TrySendError::Closed(_)) => Ok(()),
            },
            TelemetryBackpressure::Block => sender
                .send(metrics.clone())
                .await
                .map_err(|e| Error::telemetry(format!("telemetry queue closed: {e}"))),
        }
    }

    async fn export_results(&self, results: &TestResults) -> Result<()> {
        self.inner.export_results(results).await
    }

    async fn shutdown(&self) -> Result<()> {
        self.sender.lock().await.take();
        if let Some(handle) = self.worker.lock().await.take()
            && let Err(err) = handle.await
        {
            error!("Telemetry worker failed: {err}");
        }
        self.inner.shutdown().await
    }
}

/// No-op telemetry exporter that does nothing
pub struct NoopTelemetryExporter;

impl Default for NoopTelemetryExporter {
    fn default() -> Self {
        Self::new()
    }
}

impl NoopTelemetryExporter {
    /// Create a new no-op telemetry exporter
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl TelemetryExporter for NoopTelemetryExporter {
    async fn init(&self) -> Result<()> {
        // Do nothing
        Ok(())
    }

    async fn export_request(&self, _metrics: &RequestMetrics) -> Result<()> {
        // Do nothing
        Ok(())
    }

    async fn export_results(&self, _results: &TestResults) -> Result<()> {
        // Do nothing
        Ok(())
    }

    async fn shutdown(&self) -> Result<()> {
        // Do nothing
        Ok(())
    }
}

/// Console telemetry exporter that prints metrics to the console
pub struct ConsoleTelemetryExporter;

impl Default for ConsoleTelemetryExporter {
    fn default() -> Self {
        Self::new()
    }
}

impl ConsoleTelemetryExporter {
    /// Create a new console telemetry exporter
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl TelemetryExporter for ConsoleTelemetryExporter {
    async fn init(&self) -> Result<()> {
        info!("Initializing console telemetry exporter");
        Ok(())
    }

    async fn export_request(&self, metrics: &RequestMetrics) -> Result<()> {
        // Keep this as println since it's user-facing output
        info!(
            "Request: {} {} - Status: {} - Response time: {}ms - Success: {}",
            metrics.method,
            metrics.url,
            metrics.status_code,
            metrics.response_time_ms,
            metrics.success
        );

        // Also log it for structured logging
        debug!(
            method = %metrics.method,
            url = %metrics.url,
            status = metrics.status_code,
            response_time_ms = metrics.response_time_ms,
            success = metrics.success,
            "Request metrics"
        );

        Ok(())
    }

    async fn export_results(&self, results: &TestResults) -> Result<()> {
        // Keep these as println since they're user-facing output
        info!("Test Results:");
        info!("  Total requests: {}", results.total_requests);
        info!("  Successful requests: {}", results.successful_requests);
        info!("  Failed requests: {}", results.failed_requests);
        info!(
            "  Average response time: {:.2}ms",
            results.avg_response_time_ms
        );
        info!("  P90 response time: {}ms", results.p90_response_time_ms);
        info!("  Requests per second: {:.2}", results.requests_per_second);
        info!("  Error rate: {:.2}%", results.error_rate * 100.0);
        info!("  Duration: {:.2}s", results.duration_seconds);
        info!("  Virtual users: {}", results.total_virtual_users);

        // Also log it for structured logging
        info!(
            total_requests = results.total_requests,
            successful_requests = results.successful_requests,
            failed_requests = results.failed_requests,
            avg_response_time_ms = results.avg_response_time_ms,
            p90_response_time_ms = results.p90_response_time_ms,
            requests_per_second = results.requests_per_second,
            error_rate = results.error_rate,
            duration_seconds = results.duration_seconds,
            total_virtual_users = results.total_virtual_users,
            "Test results"
        );

        Ok(())
    }

    async fn shutdown(&self) -> Result<()> {
        info!("Shutting down console telemetry exporter");
        Ok(())
    }
}

/// Telemetry exporter that serializes metrics as newline-delimited JSON to
/// STDERR.
///
/// Each request metric and the final results object is written as a single JSON
/// line to **stderr** — never stdout, which is reserved for `--format json`
/// results (see the CLI). This makes the `json` telemetry format a real,
/// machine-consumable stream (`2>telemetry.ndjson`) rather than a facade.
pub struct JsonTelemetryExporter;

impl Default for JsonTelemetryExporter {
    fn default() -> Self {
        Self::new()
    }
}

impl JsonTelemetryExporter {
    /// Create a new JSON telemetry exporter
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl TelemetryExporter for JsonTelemetryExporter {
    async fn init(&self) -> Result<()> {
        Ok(())
    }

    async fn export_request(&self, metrics: &RequestMetrics) -> Result<()> {
        // Log-and-continue on serialization failure: telemetry must never
        // strand a run (mirrors the metrics no-strand guarantee).
        match serde_json::to_string(metrics) {
            Ok(line) => eprintln!("{line}"),
            Err(err) => error!("Failed to serialize request metrics to JSON: {err}"),
        }
        Ok(())
    }

    async fn export_results(&self, results: &TestResults) -> Result<()> {
        match serde_json::to_string(results) {
            Ok(line) => eprintln!("{line}"),
            Err(err) => error!("Failed to serialize test results to JSON: {err}"),
        }
        Ok(())
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

/// Factory for creating telemetry exporters
pub struct TelemetryExporterFactory;

impl TelemetryExporterFactory {
    /// Create a telemetry exporter for the requested format.
    ///
    /// Implemented formats return a real exporter; `OpenTelemetry`/`Prometheus`
    /// return an [`Error::telemetry`] so a config-driven choice fails loudly
    /// instead of silently no-opping (explicit exporter choice fails loudly).
    pub fn create(config: &ExporterConfig) -> Result<Arc<dyn TelemetryExporter>> {
        match config.format {
            TelemetryFormat::Json => Ok(Arc::new(JsonTelemetryExporter::new())),
            TelemetryFormat::Console => Ok(Arc::new(ConsoleTelemetryExporter::new())),
            TelemetryFormat::Noop => Ok(Arc::new(NoopTelemetryExporter::new())),
            TelemetryFormat::OpenTelemetry | TelemetryFormat::Prometheus => Err(Error::telemetry(
                "otlp/prometheus exporter not implemented; use json, console, or noop",
            )),
        }
    }

    /// Create a new JSON telemetry exporter
    pub fn create_json() -> Arc<dyn TelemetryExporter> {
        Arc::new(JsonTelemetryExporter::new())
    }

    /// Create a new console telemetry exporter
    pub fn create_console() -> Arc<dyn TelemetryExporter> {
        Arc::new(ConsoleTelemetryExporter::new())
    }

    /// Create a new no-op telemetry exporter
    pub fn create_noop() -> Arc<dyn TelemetryExporter> {
        Arc::new(NoopTelemetryExporter::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exporter_config_default() {
        let config = ExporterConfig::default();
        assert_eq!(config.service_name, "pummel");
        assert_eq!(config.endpoint, "http://localhost:4317");
        assert_eq!(config.format, TelemetryFormat::Json);
    }

    #[test]
    fn test_exporter_config_from_file_config() {
        let file_cfg = config::TelemetryConfig {
            exporter: "console".to_string(),
            service_name: "svc".to_string(),
            custom: HashMap::from([("region".to_string(), "us".to_string())]),
            ..config::TelemetryConfig::default()
        };

        let spec = ExporterConfig::from(&file_cfg);
        assert_eq!(spec.format, TelemetryFormat::Console);
        assert_eq!(spec.service_name, "svc");
        assert_eq!(
            spec.attributes.get("region").map(String::as_str),
            Some("us")
        );
    }

    #[test]
    fn test_factory_create_implemented_formats() {
        for format in [
            TelemetryFormat::Json,
            TelemetryFormat::Console,
            TelemetryFormat::Noop,
        ] {
            let config = ExporterConfig {
                format,
                ..ExporterConfig::default()
            };
            assert!(TelemetryExporterFactory::create(&config).is_ok());
        }
    }

    #[test]
    fn test_factory_create_unimplemented_formats_error() {
        for format in [TelemetryFormat::OpenTelemetry, TelemetryFormat::Prometheus] {
            let config = ExporterConfig {
                format,
                ..ExporterConfig::default()
            };
            assert!(TelemetryExporterFactory::create(&config).is_err());
        }
    }

    #[tokio::test]
    async fn test_noop_exporter() {
        let exporter = NoopTelemetryExporter::new();
        assert!(exporter.init().await.is_ok());
        assert!(exporter.shutdown().await.is_ok());
    }

    #[tokio::test]
    async fn test_console_exporter() {
        let exporter = ConsoleTelemetryExporter::new();
        assert!(exporter.init().await.is_ok());
        assert!(exporter.shutdown().await.is_ok());
    }

    #[tokio::test]
    async fn test_json_exporter() {
        let exporter = JsonTelemetryExporter::new();
        assert!(exporter.init().await.is_ok());
        assert!(exporter.shutdown().await.is_ok());
    }
}
