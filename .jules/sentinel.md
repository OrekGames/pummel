## 2025-02-18 - [Security Warning for TLS Verification]
**Vulnerability:** The HTTP client configuration (`verify_ssl = false`) allows disabling TLS certificate verification without an explicit runtime warning.
**Learning:** This is an intentional feature designed to allow testing against staging servers with self-signed certificates.
**Prevention:** To prevent accidental usage in production environments without breaking the intended functionality, a loud runtime warning has been added whenever the `verify_ssl = false` configuration is actively used.
