## 2024-07-18 - Avoid Parsing and Immediately Serializing JSON Payloads Again
**Learning:** Found an inefficiency where a dynamic JSON request body was first parsed entirely into a `serde_json::Value` only to be immediately serialized to bytes via `.json(&value)`.
**Action:** Changed the builder to utilize a new `raw_json` method taking pre-serialized or statically rendered JSON bytes. We can validate valid JSON by decoding without allocating using `serde_json::from_str::<serde::de::IgnoredAny>(&rendered)` to avoid the extra allocation/de-allocation of a tree structure in the loop.
