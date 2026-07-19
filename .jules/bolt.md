## 2025-02-19 - [Avoid `serde_json::Value` Allocation]
**Learning:** Parsing JSON strings into a `serde_json::Value` (a full AST/DOM representation) and then serializing them back to bytes just to validate syntax and send as a request body is a significant anti-pattern for performance-sensitive paths like load generators.
**Action:** Use `serde_json::from_str::<serde::de::IgnoredAny>(json)` to perform zero-allocation syntax validation on JSON strings when the data is just being passed through or validated, and pass the string directly as a text body instead of serializing a `Value`.
