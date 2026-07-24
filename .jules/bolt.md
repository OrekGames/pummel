## 2024-07-24 - Pre-serialize static JSON configurations

**Learning:** Parsing static JSON strings into `serde_json::Value` syntax trees only to immediately serialize them back again in `RequestBuilder::json` incurs unnecessary allocation and serialization overhead on the hot path (as seen in `json_request_body/serialize_each_send` vs `json_request_body/pre_serialized_bytes`).

**Action:** When validating static JSON configurations, use `serde_json::from_str::<serde::de::IgnoredAny>` for zero-allocation structural validation. Then, map the unparsed string directly into a raw bytes request body (`RequestBuilder::binary(Bytes::from(json.clone()))`) and manually inject the `Content-Type: application/json` header.
