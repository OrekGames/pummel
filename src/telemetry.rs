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

    /// Close the request queue and wait for the background worker to finish.
    ///
    /// Idempotent: a second call is a no-op once the sender/worker are gone.
    /// Used by [`export_results`](TelemetryExporter::export_results) so the
    /// aggregate line cannot race ahead of still-queued request telemetry, and
    /// by [`shutdown`](TelemetryExporter::shutdown) so a post-export shutdown
    /// remains safe.
    async fn drain_request_queue(&self) {
        // Dropping the sender ends the worker's `recv` loop after it finishes
        // any items already in the channel.
        self.sender.lock().await.take();
        if let Some(handle) = self.worker.lock().await.take()
            && let Err(err) = handle.await
        {
            error!("Telemetry worker failed: {err}");
        }
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
        // Drain queued request lines before writing the aggregate so NDJSON
        // (and any ordered sink) never emits results ahead of in-flight requests.
        self.drain_request_queue().await;
        self.inner.export_results(results).await
    }

    async fn shutdown(&self) -> Result<()> {
        // Safe after `export_results`: drain is idempotent when the queue is
        // already closed and the worker has joined.
        self.drain_request_queue().await;
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
    /// Implemented formats return a real exporter. `OpenTelemetry`/`Prometheus`
    /// return an [`Error::telemetry`] for direct/programmatic factory use.
    /// File-based `[telemetry]` configs with those exporters are rejected
    /// earlier by [`crate::config::Config::validate`] when telemetry is enabled.
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
    use crate::http::Request;
    use crate::metrics::RequestMetricsParams;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;
    use tokio::sync::Notify;

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

    /// Mock exporter whose `export_request` waits on a gate so queue/backpressure
    /// and drain ordering can be asserted deterministically.
    struct GatedMockExporter {
        gate_open: AtomicBool,
        notify: Notify,
        events: StdMutex<Vec<&'static str>>,
        /// Increments as soon as the worker enters `export_request` (before gate).
        entered: std::sync::atomic::AtomicUsize,
        request_count: std::sync::atomic::AtomicUsize,
        results_count: std::sync::atomic::AtomicUsize,
        shutdown_count: std::sync::atomic::AtomicUsize,
    }

    impl GatedMockExporter {
        fn new(gate_open: bool) -> Arc<Self> {
            Arc::new(Self {
                gate_open: AtomicBool::new(gate_open),
                notify: Notify::new(),
                events: StdMutex::new(Vec::new()),
                entered: std::sync::atomic::AtomicUsize::new(0),
                request_count: std::sync::atomic::AtomicUsize::new(0),
                results_count: std::sync::atomic::AtomicUsize::new(0),
                shutdown_count: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        fn open_gate(&self) {
            self.gate_open.store(true, Ordering::SeqCst);
            self.notify.notify_waiters();
        }

        fn events(&self) -> Vec<&'static str> {
            self.events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }

        async fn wait_entered(&self, n: usize) {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
            while self.entered.load(Ordering::SeqCst) < n {
                if tokio::time::Instant::now() >= deadline {
                    panic!(
                        "timed out waiting for worker to enter export_request ({}/{})",
                        self.entered.load(Ordering::SeqCst),
                        n
                    );
                }
                tokio::task::yield_now().await;
            }
        }
    }

    #[async_trait]
    impl TelemetryExporter for GatedMockExporter {
        async fn init(&self) -> Result<()> {
            Ok(())
        }

        async fn export_request(&self, _metrics: &RequestMetrics) -> Result<()> {
            self.entered.fetch_add(1, Ordering::SeqCst);
            while !self.gate_open.load(Ordering::SeqCst) {
                self.notify.notified().await;
            }
            self.request_count.fetch_add(1, Ordering::SeqCst);
            self.events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push("request");
            Ok(())
        }

        async fn export_results(&self, _results: &TestResults) -> Result<()> {
            self.results_count.fetch_add(1, Ordering::SeqCst);
            self.events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push("results");
            Ok(())
        }

        async fn shutdown(&self) -> Result<()> {
            self.shutdown_count.fetch_add(1, Ordering::SeqCst);
            self.events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push("shutdown");
            Ok(())
        }
    }

    fn sample_metrics(id: &str) -> RequestMetrics {
        let request = Request::get("https://example.com").build().unwrap();
        RequestMetrics::new(RequestMetricsParams {
            id: id.to_string(),
            step_id: "step1".to_string(),
            step_name: "Step 1".to_string(),
            scenario_id: "scenario1".to_string(),
            scenario_name: "Scenario 1".to_string(),
            virtual_user_id: 0,
            request: &request,
            response: None,
            error: None,
            elapsed: Duration::from_millis(1),
        })
    }

    #[tokio::test]
    async fn export_results_drains_queued_requests_before_aggregate() {
        let mock = GatedMockExporter::new(false);
        let bounded = BoundedTelemetryExporter::new(mock.clone(), TelemetryBackpressure::Drop, 8);
        bounded.init().await.unwrap();

        for i in 0..3 {
            bounded
                .export_request(&sample_metrics(&format!("r{i}")))
                .await
                .unwrap();
        }
        mock.wait_entered(1).await;

        // Release the worker, then export results: drain must finish every
        // queued request before the aggregate callback runs.
        mock.open_gate();
        bounded.export_results(&TestResults::new()).await.unwrap();

        let events = mock.events();
        assert_eq!(
            mock.request_count.load(Ordering::SeqCst),
            3,
            "all queued requests must export"
        );
        assert_eq!(mock.results_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            events,
            vec!["request", "request", "request", "results"],
            "aggregate must follow drained requests, got {events:?}"
        );

        // Shutdown after export_results must remain safe (idempotent drain).
        bounded.shutdown().await.unwrap();
        assert_eq!(mock.shutdown_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            mock.events(),
            vec!["request", "request", "request", "results", "shutdown"]
        );
    }

    #[tokio::test]
    async fn drop_backpressure_discards_when_queue_full() {
        let mock = GatedMockExporter::new(false);
        let capacity = 2usize;
        let bounded =
            BoundedTelemetryExporter::new(mock.clone(), TelemetryBackpressure::Drop, capacity);
        bounded.init().await.unwrap();

        // First send is taken by the blocked worker; the next `capacity` fill
        // the channel; one more must be dropped without error.
        bounded
            .export_request(&sample_metrics("held"))
            .await
            .unwrap();
        mock.wait_entered(1).await;
        for i in 0..capacity {
            bounded
                .export_request(&sample_metrics(&format!("q{i}")))
                .await
                .unwrap();
        }
        assert!(
            bounded
                .export_request(&sample_metrics("overflow"))
                .await
                .is_ok(),
            "Drop mode must return Ok when the queue is full"
        );
        assert!(
            bounded.warned_full.load(Ordering::Relaxed),
            "queue-full Drop path should set the one-shot warned flag"
        );

        mock.open_gate();
        bounded.export_results(&TestResults::new()).await.unwrap();
        // held + capacity queued items; overflow dropped.
        assert_eq!(
            mock.request_count.load(Ordering::SeqCst),
            1 + capacity,
            "overflow request must be dropped"
        );
        bounded.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn block_backpressure_waits_for_capacity() {
        let mock = GatedMockExporter::new(false);
        let capacity = 1usize;
        let bounded =
            BoundedTelemetryExporter::new(mock.clone(), TelemetryBackpressure::Block, capacity);
        bounded.init().await.unwrap();

        bounded
            .export_request(&sample_metrics("held"))
            .await
            .unwrap();
        mock.wait_entered(1).await;
        bounded
            .export_request(&sample_metrics("queued"))
            .await
            .unwrap();

        let blocked = Arc::new(AtomicBool::new(true));
        let blocked_flag = blocked.clone();
        let bounded = Arc::new(bounded);
        let send_task = {
            let bounded = bounded.clone();
            tokio::spawn(async move {
                bounded
                    .export_request(&sample_metrics("blocked"))
                    .await
                    .unwrap();
                blocked_flag.store(false, Ordering::SeqCst);
            })
        };

        // While the queue is full and the worker is gated, the Block send must
        // still be waiting.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            blocked.load(Ordering::SeqCst),
            "Block mode must wait when the queue is full"
        );

        mock.open_gate();
        tokio::time::timeout(Duration::from_secs(2), send_task)
            .await
            .expect("blocked send should complete after drain capacity frees")
            .unwrap();
        assert!(!blocked.load(Ordering::SeqCst));

        bounded.export_results(&TestResults::new()).await.unwrap();
        assert_eq!(mock.request_count.load(Ordering::SeqCst), 3);
        bounded.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_drains_when_export_results_skipped() {
        let mock = GatedMockExporter::new(true);
        let bounded = BoundedTelemetryExporter::new(mock.clone(), TelemetryBackpressure::Drop, 4);
        bounded.init().await.unwrap();
        bounded
            .export_request(&sample_metrics("solo"))
            .await
            .unwrap();
        bounded.shutdown().await.unwrap();

        assert_eq!(mock.request_count.load(Ordering::SeqCst), 1);
        assert_eq!(mock.results_count.load(Ordering::SeqCst), 0);
        assert_eq!(mock.shutdown_count.load(Ordering::SeqCst), 1);
        assert_eq!(mock.events(), vec!["request", "shutdown"]);
    }
}
