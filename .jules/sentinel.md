## 2024-07-24 - Credentials leakage in metrics URL reporting
**Vulnerability:** URL objects embedding credentials (e.g., https://user:pass@domain.com/) were passed directly to `RequestMetrics::new()` in `src/metrics.rs`, causing the plain text credentials to be serialized in logs and telemetry outputs.
**Learning:** Using `reqwest::Url`'s `.as_str()` directly for observability exports sensitive information if developers embed Basic Auth via URLs.
**Prevention:** Always ensure embedded passwords are redacted using `url.set_password(None)` before emitting URLs to logs, telemetry, or metrics objects.
