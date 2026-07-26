## 2025-02-14 - Credential leakage in metrics and console output
**Vulnerability:** The codebase passes unredacted URLs directly from the request object to metrics collectors and console logs. Specifically, `request.url().as_str().to_string()` is recorded in `metrics.rs`, which is then logged verbatim in `telemetry.rs` (`export_request` and potentially elsewhere). This leaks basic auth credentials embedded in URLs.
**Learning:** In Pummel, `reqwest::Url` objects may contain credentials from configurations. When exporting or logging URL strings, passwords must be explicitly stripped via `.set_password(None)`.
**Prevention:** Always redact sensitive components from `Url` objects before converting them to strings for telemetry, logging, or displaying to users.
