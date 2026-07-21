## 2024-07-21 - URL Credential Leakage in Telemetry
**Vulnerability:** Telemetry and metrics logging was recording the full requested URL, which might contain inline credentials (e.g., `https://user:password@example.com/...`), potentially exposing secrets in log output.
**Learning:** `reqwest::Url` has a `set_password(None)` method that can easily be used to redact credentials from the URL before serializing it for logs or telemetry.
**Prevention:** Redact embedded passwords from `reqwest::Url` objects before recording them in metrics or log output.
