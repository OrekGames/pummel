## 2024-05-18 - [Avoid parsing JSON into Value when only validating]
**Vulnerability:** In `src/config.rs`, validating that a string is a valid JSON without evaluating it parsed it into `serde_json::Value`, allocating a full syntax tree for every validation which could cause memory bloat and Denial of Service (DoS) for large JSON strings on hot paths.
**Learning:** `serde_json::from_str::<serde_json::Value>` fully allocates the JSON into memory.
**Prevention:** Use `serde_json::from_str::<serde::de::IgnoredAny>` for validating JSON structure efficiently without allocations.
