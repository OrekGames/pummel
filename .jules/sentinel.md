## 2024-05-24 - [CRITICAL] Prevent credential leakage in logs and metrics
**Vulnerability:** URL objects containing Basic Authentication credentials (e.g. `https://user:password@example.com`) could be exposed in metrics and logs, leaking sensitive passwords.
**Learning:** `reqwest::Url` provides a `set_password(None)` method that should be used on cloned URL instances before serializing or logging them in observability data.
**Prevention:** Always redact credentials from `reqwest::Url` objects using `set_password(None)` before including them in metrics, graphs, or logs.
