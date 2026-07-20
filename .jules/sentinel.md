## 2024-10-27 - [Config Parsing DoS via JSON Exhaustion]
**Vulnerability:** Memory/CPU exhaustion via deeply nested or excessively large static JSON strings in configuration file validation. The `Config::validate` function previously parsed these directly into a `serde_json::Value`.
**Learning:** `serde_json::from_str::<serde_json::Value>` forces allocation of the entire JSON syntax tree in memory. This is highly inefficient and creates a DoS vector during validation of static configuration inputs.
**Prevention:** Use `serde_json::from_str::<serde::de::IgnoredAny>` for JSON validation on hot paths or during config parsing when the structure itself isn't needed immediately, rather than parsing directly into `serde_json::Value`.
