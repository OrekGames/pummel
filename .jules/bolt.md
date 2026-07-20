## 2024-03-24 - [Avoid parse-and-serialize roundtrips for pre-rendered JSON]
**Learning:** Parsing a pre-rendered JSON string into serde_json::Value and re-serializing it introduces unnecessary allocations and latency on the hot path.
**Action:** Use serde_json::from_str::<serde::de::IgnoredAny> for fast validation without allocation, and send the original string with .text() along with an explicit Content-Type header.
