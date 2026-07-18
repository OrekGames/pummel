use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use reqwest::{self, Method, StatusCode, Url, header, redirect};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{GlobalConfig, HttpConfig};
use crate::error::{Error, Result};

/// HTTP request method
pub type HttpMethod = Method;

/// HTTP status code
pub type HttpStatus = StatusCode;

/// HTTP headers
pub type HttpHeaders = header::HeaderMap;

/// HTTP request body
#[derive(Debug, Clone)]
pub enum Body {
    /// Empty body
    Empty,
    /// Text body
    Text(String),
    /// JSON body
    Json(Value),
    /// Binary body (refcounted; avoids copying response buffers on the hot path)
    Binary(Bytes),
}

impl fmt::Display for Body {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Body::Empty => write!(f, ""),
            Body::Text(text) => write!(f, "{text}"),
            Body::Json(json) => write!(f, "{json}"),
            Body::Binary(bytes) => write!(f, "<{} bytes>", bytes.len()),
        }
    }
}

/// HTTP request
#[derive(Debug, Clone)]
pub struct Request {
    /// HTTP method
    method: HttpMethod,
    /// URL
    url: Url,
    /// HTTP headers
    headers: HttpHeaders,
    /// Request body
    body: Body,
    /// Request timeout
    timeout: Duration,
    /// Follow redirects
    follow_redirects: bool,
    /// Custom data associated with this request
    metadata: HashMap<String, String>,
}

impl Request {
    /// Create a new GET request
    pub fn get<U: AsRef<str>>(url: U) -> RequestBuilder {
        RequestBuilder::new(Method::GET, url)
    }

    /// Create a new POST request
    pub fn post<U: AsRef<str>>(url: U) -> RequestBuilder {
        RequestBuilder::new(Method::POST, url)
    }

    /// Create a new PUT request
    pub fn put<U: AsRef<str>>(url: U) -> RequestBuilder {
        RequestBuilder::new(Method::PUT, url)
    }

    /// Create a new DELETE request
    pub fn delete<U: AsRef<str>>(url: U) -> RequestBuilder {
        RequestBuilder::new(Method::DELETE, url)
    }

    /// Create a new PATCH request
    pub fn patch<U: AsRef<str>>(url: U) -> RequestBuilder {
        RequestBuilder::new(Method::PATCH, url)
    }

    /// Create a new HEAD request
    pub fn head<U: AsRef<str>>(url: U) -> RequestBuilder {
        RequestBuilder::new(Method::HEAD, url)
    }

    /// Create a new OPTIONS request
    pub fn options<U: AsRef<str>>(url: U) -> RequestBuilder {
        RequestBuilder::new(Method::OPTIONS, url)
    }

    /// Creates a new request builder with the given method and URL.
    pub fn request<U: AsRef<str>>(method: HttpMethod, url: U) -> RequestBuilder {
        RequestBuilder::new(method, url)
    }

    /// Get the HTTP method
    pub fn method(&self) -> &HttpMethod {
        &self.method
    }

    /// Get the URL
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// Get the HTTP headers
    pub fn headers(&self) -> &HttpHeaders {
        &self.headers
    }

    /// Get the request body
    pub fn body(&self) -> &Body {
        &self.body
    }

    /// Get the request timeout
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Check if redirects should be followed
    pub fn follow_redirects(&self) -> bool {
        self.follow_redirects
    }

    /// Get the metadata
    pub fn metadata(&self) -> &HashMap<String, String> {
        &self.metadata
    }

    /// Get a metadata value
    pub fn get_metadata(&self, key: &str) -> Option<&String> {
        self.metadata.get(key)
    }
}

/// Builder for HTTP requests
pub struct RequestBuilder {
    method: HttpMethod,
    url: String,
    headers: HttpHeaders,
    body: Body,
    timeout: Duration,
    follow_redirects: bool,
    metadata: HashMap<String, String>,
    /// Errors accumulated by fluent setters (invalid header name/value,
    /// un-serializable JSON body). Surfaced by [`build`](RequestBuilder::build)
    /// so a misconfiguration fails loudly instead of silently dropping the
    /// header/body and sending a different request than the user specified.
    errors: Vec<Error>,
}

impl RequestBuilder {
    /// Create a new request builder
    pub fn new<U: AsRef<str>>(method: HttpMethod, url: U) -> Self {
        Self {
            method,
            url: url.as_ref().to_string(),
            headers: HttpHeaders::new(),
            body: Body::Empty,
            timeout: Duration::from_secs(30),
            follow_redirects: true,
            metadata: HashMap::new(),
            errors: Vec::new(),
        }
    }

    /// Set a header.
    ///
    /// An invalid header name or value is recorded as an error (surfaced by
    /// [`build`](RequestBuilder::build)) rather than silently dropped, so a
    /// config-supplied `Authorization` value with a stray control character
    /// fails the build instead of producing a stream of 401s.
    pub fn header<K, V>(mut self, key: K, value: V) -> Self
    where
        K: AsRef<str>,
        V: AsRef<str>,
    {
        match (
            header::HeaderName::from_bytes(key.as_ref().as_bytes()),
            header::HeaderValue::from_str(value.as_ref()),
        ) {
            (Ok(name), Ok(val)) => {
                self.headers.insert(name, val);
            }
            _ => {
                self.errors.push(Error::config(format!(
                    "Invalid header '{}: {}'",
                    key.as_ref(),
                    value.as_ref()
                )));
            }
        }
        self
    }

    /// Set multiple headers
    pub fn headers(mut self, headers: HttpHeaders) -> Self {
        self.headers = headers;
        self
    }

    /// Set the request body as text
    pub fn text<T: Into<String>>(mut self, text: T) -> Self {
        self.body = Body::Text(text.into());
        self
    }

    /// Set the request body from pre-serialized or validated JSON bytes.
    pub fn raw_json<B: Into<Bytes>>(mut self, body: B) -> Self {
        self.body = Body::Binary(body.into());
        if !self.headers.contains_key(header::CONTENT_TYPE) {
            let value = header::HeaderValue::from_static("application/json");
            self.headers.insert(header::CONTENT_TYPE, value);
        }
        self
    }

    /// Set the request body as JSON.
    ///
    /// The body is serialized once here into a refcounted [`Bytes`] buffer
    /// (`Body::Binary`) so each send clones bytes instead of re-running
    /// `serde_json`. A serialization failure (e.g. a custom `Serialize` impl
    /// that errors, or a map with non-string keys) is recorded as an error
    /// surfaced by [`build`](RequestBuilder::build) rather than silently
    /// leaving the previous (empty) body in place.
    pub fn json<T: Serialize>(mut self, json: &T) -> Self {
        match serde_json::to_vec(json) {
            Ok(bytes) => {
                self.body = Body::Binary(Bytes::from(bytes));
                if !self.headers.contains_key(header::CONTENT_TYPE) {
                    let value = header::HeaderValue::from_static("application/json");
                    self.headers.insert(header::CONTENT_TYPE, value);
                }
            }
            Err(err) => {
                self.errors
                    .push(Error::config(format!("Invalid JSON request body: {err}")));
            }
        }
        self
    }

    /// Set the request body as binary
    pub fn binary<B: Into<Bytes>>(mut self, bytes: B) -> Self {
        self.body = Body::Binary(bytes.into());
        self
    }

    /// Set the request timeout
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set whether to follow redirects
    pub fn follow_redirects(mut self, follow: bool) -> Self {
        self.follow_redirects = follow;
        self
    }

    /// Add metadata to this request
    pub fn metadata<K, V>(mut self, key: K, value: V) -> Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// Build the request.
    ///
    /// The target must be an absolute `http`/`https` URL. Inputs that lack a
    /// scheme or host (e.g. `www.example.com`, `localhost:8080`, `/api/path`,
    /// or an empty string) are rejected with an error rather than being
    /// silently rewritten to a placeholder.
    pub fn build(self) -> Result<Request> {
        // Surface the first accumulated setter error (invalid header / JSON
        // body) before constructing the request, so a malformed request fails
        // loudly instead of silently omitting the offending header/body.
        if let Some(err) = self.errors.into_iter().next() {
            return Err(err);
        }

        let url = Url::parse(&self.url)
            .map_err(|e| Error::config(format!("Invalid request URL '{}': {e}", self.url)))?;
        if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
            return Err(Error::config(format!(
                "Request URL '{}' must be an absolute http(s) URL",
                self.url
            )));
        }

        Ok(Request {
            method: self.method,
            url,
            headers: self.headers,
            body: self.body,
            timeout: self.timeout,
            follow_redirects: self.follow_redirects,
            metadata: self.metadata,
        })
    }
}

/// HTTP response
#[derive(Debug, Clone)]
pub struct Response {
    /// HTTP status code
    status: HttpStatus,
    /// HTTP headers
    headers: HttpHeaders,
    /// Response body
    body: Body,
    /// Time taken to receive the full response (headers + body transfer)
    response_time: Duration,
    /// Time to first byte (headers received), if measured
    ttfb: Option<Duration>,
    /// Custom data associated with this response
    metadata: HashMap<String, String>,
}

impl Response {
    /// Create a new response
    ///
    /// `response_time` should reflect the full response including body
    /// transfer. Time-to-first-byte is left unset (`None`); use
    /// [`Response::new_with_timing`] to record it.
    pub fn new(
        status: HttpStatus,
        headers: HttpHeaders,
        body: Body,
        response_time: Duration,
    ) -> Self {
        Self {
            status,
            headers,
            body,
            response_time,
            ttfb: None,
            metadata: HashMap::new(),
        }
    }

    /// Create a new response recording both the full response time and the
    /// time-to-first-byte (headers received).
    pub fn new_with_timing(
        status: HttpStatus,
        headers: HttpHeaders,
        body: Body,
        response_time: Duration,
        ttfb: Option<Duration>,
    ) -> Self {
        Self {
            status,
            headers,
            body,
            response_time,
            ttfb,
            metadata: HashMap::new(),
        }
    }

    /// Get the HTTP status code
    pub fn status(&self) -> HttpStatus {
        self.status
    }

    /// Check if the response status is successful (2xx)
    pub fn is_success(&self) -> bool {
        self.status.is_success()
    }

    /// Check if the response status is a client error (4xx)
    pub fn is_client_error(&self) -> bool {
        self.status.is_client_error()
    }

    /// Check if the response status is a server error (5xx)
    pub fn is_server_error(&self) -> bool {
        self.status.is_server_error()
    }

    /// Get the HTTP headers
    pub fn headers(&self) -> &HttpHeaders {
        &self.headers
    }

    /// Get the response body
    pub fn body(&self) -> &Body {
        &self.body
    }

    /// Get the response time (headers + body transfer)
    pub fn response_time(&self) -> Duration {
        self.response_time
    }

    /// Get the time-to-first-byte (headers received), if it was measured.
    pub fn ttfb(&self) -> Option<Duration> {
        self.ttfb
    }

    /// Get the metadata
    pub fn metadata(&self) -> &HashMap<String, String> {
        &self.metadata
    }

    /// Get a metadata value
    pub fn get_metadata(&self, key: &str) -> Option<&String> {
        self.metadata.get(key)
    }

    /// Add metadata to this response
    pub fn with_metadata<K, V>(&mut self, key: K, value: V) -> &mut Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// Parse the response body as JSON
    pub fn json<T: for<'de> Deserialize<'de>>(&self) -> Result<T> {
        match &self.body {
            Body::Json(value) => serde_json::from_value(value.clone()).map_err(Error::from),
            Body::Text(text) => serde_json::from_str(text).map_err(Error::from),
            Body::Binary(bytes) => serde_json::from_slice(bytes).map_err(Error::from),
            Body::Empty => Err(Error::other("Empty response body")),
        }
    }

    /// Get the response body as text
    pub fn text(&self) -> Result<String> {
        match &self.body {
            Body::Text(text) => Ok(text.clone()),
            Body::Json(value) => Ok(value.to_string()),
            Body::Binary(bytes) => String::from_utf8(bytes.to_vec())
                .map_err(|e| Error::other(format!("Failed to decode response body: {e}"))),
            Body::Empty => Ok(String::new()),
        }
    }

    /// Get the response body as bytes
    pub fn bytes(&self) -> Result<Vec<u8>> {
        match &self.body {
            Body::Text(text) => Ok(text.as_bytes().to_vec()),
            Body::Json(value) => Ok(value.to_string().as_bytes().to_vec()),
            Body::Binary(bytes) => Ok(bytes.to_vec()),
            Body::Empty => Ok(Vec::new()),
        }
    }
}

/// HTTP client for making requests
#[async_trait]
pub trait HttpClient: Send + Sync {
    /// Send a request and return the response
    async fn send(&self, request: &Request) -> Result<Response>;

    /// Close the client and release resources
    async fn close(&self) -> Result<()>;
}

/// Immutable description of how to build an HTTP client (connection pool, TLS,
/// protocol, and default headers).
///
/// A single spec drives both of a [`DefaultHttpClient`]'s internal reqwest
/// clients so their settings can never drift. Reqwest's redirect policy is
/// client-level, so the follow / no-follow distinction is NOT part of the spec:
/// it is applied by [`DefaultHttpClient::from_spec`] when it builds the two
/// clients from the same spec.
#[derive(Debug, Clone)]
pub struct ClientSpec {
    /// Connection (TCP + TLS handshake) timeout.
    pub connect_timeout: Duration,
    /// How long an idle connection is kept in the pool.
    pub pool_idle_timeout: Duration,
    /// Maximum idle connections kept per host in the pool.
    pub pool_max_idle_per_host: usize,
    /// Assume HTTP/2 prior knowledge (no ALPN negotiation) when `true`.
    pub use_http2: bool,
    /// Verify TLS certificates. `false` accepts invalid certs (self-signed
    /// staging hosts) via `danger_accept_invalid_certs`.
    pub verify_ssl: bool,
    /// Default headers applied to every request by the client. Per-request /
    /// per-step headers still take precedence.
    pub default_headers: HttpHeaders,
}

impl Default for ClientSpec {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            pool_idle_timeout: Duration::from_secs(30),
            pool_max_idle_per_host: 10,
            // Off by default: prior-knowledge HTTP/2 skips ALPN and breaks
            // plain HTTP/1.1 servers, so the no-config default negotiates
            // normally. Config can opt in via `[http] use_http2`.
            use_http2: false,
            verify_ssl: true,
            default_headers: HttpHeaders::new(),
        }
    }
}

impl ClientSpec {
    /// Build a client spec from config, validating global default headers.
    pub fn try_from_config(http: &HttpConfig, global: &GlobalConfig) -> Result<Self> {
        let mut default_headers = HttpHeaders::new();
        for (key, value) in &global.headers {
            let name = header::HeaderName::from_bytes(key.as_bytes())
                .map_err(|e| Error::config(format!("Invalid global header name '{key}': {e}")))?;
            if contains_template(value) {
                continue;
            }
            let val = header::HeaderValue::from_str(value).map_err(|e| {
                Error::config(format!("Invalid global header value for '{key}': {e}"))
            })?;
            default_headers.insert(name, val);
        }

        Ok(Self {
            connect_timeout: Duration::from_millis(http.connection_timeout_ms),
            pool_idle_timeout: Duration::from_secs(http.pool_idle_timeout_seconds),
            pool_max_idle_per_host: http.max_connections_per_host,
            use_http2: http.use_http2,
            verify_ssl: http.verify_ssl,
            default_headers,
        })
    }
}

fn contains_template(value: &str) -> bool {
    value.contains("{{") || value.contains("}}")
}

impl From<(&HttpConfig, &GlobalConfig)> for ClientSpec {
    /// Build a client spec from the `[http]` section, folding the `[global]`
    /// default headers in as the client's default headers.
    fn from((http, global): (&HttpConfig, &GlobalConfig)) -> Self {
        Self::try_from_config(http, global)
            .expect("ClientSpec::from requires valid config headers; use try_from_config")
    }
}

/// Default HTTP client implementation using reqwest.
///
/// Reqwest's redirect policy is fixed per client, so a single client cannot vary
/// redirect-following per request. This holds TWO clients built from the same
/// [`ClientSpec`] — one that follows redirects (capped at 10 hops) and one that
/// does not — and [`send`](DefaultHttpClient::send) selects between them by the
/// request's `follow_redirects` flag. Both clients are internally `Arc`'d and
/// cloned cheaply into every virtual user, preserving connection sharing.
pub struct DefaultHttpClient {
    /// Client that follows redirects (up to 10 hops).
    client_follow: reqwest::Client,
    /// Client that does not follow redirects (measures the 3xx itself).
    client_no_follow: reqwest::Client,
}

impl DefaultHttpClient {
    /// Create a new default HTTP client using [`ClientSpec::default`].
    pub fn new() -> Result<Self> {
        Self::from_spec(&ClientSpec::default())
    }

    /// Create a new default HTTP client from an explicit [`ClientSpec`].
    ///
    /// Builds both the follow-redirects and no-follow clients from the same
    /// spec so their pool / TLS / protocol / header settings cannot drift.
    pub fn from_spec(spec: &ClientSpec) -> Result<Self> {
        if !spec.verify_ssl {
            crate::logging::warn!(
                "TLS certificate verification is disabled (verify_ssl = false); use only with trusted staging hosts"
            );
        }

        let client_follow = Self::build_client(spec, redirect::Policy::limited(10))?;
        let client_no_follow = Self::build_client(spec, redirect::Policy::none())?;
        Ok(Self {
            client_follow,
            client_no_follow,
        })
    }

    /// Build one reqwest client from `spec` with the given redirect policy.
    fn build_client(spec: &ClientSpec, policy: redirect::Policy) -> Result<reqwest::Client> {
        let mut builder = reqwest::Client::builder()
            .connect_timeout(spec.connect_timeout)
            .pool_idle_timeout(Some(spec.pool_idle_timeout))
            .pool_max_idle_per_host(spec.pool_max_idle_per_host)
            .default_headers(spec.default_headers.clone())
            .redirect(policy);

        if spec.use_http2 {
            builder = builder.http2_prior_knowledge();
        }
        if !spec.verify_ssl {
            builder = builder.danger_accept_invalid_certs(true);
        }

        builder.build().map_err(Error::from)
    }
}

#[async_trait]
impl HttpClient for DefaultHttpClient {
    async fn send(&self, request: &Request) -> Result<Response> {
        let start_time = std::time::Instant::now();

        // Select the client whose redirect policy matches the request. This
        // keeps the per-request/per-step `follow_redirects` flag honest: with
        // it disabled the 3xx response itself is measured instead of the
        // redirected-to page.
        let client = if request.follow_redirects() {
            &self.client_follow
        } else {
            &self.client_no_follow
        };

        // Build the reqwest request
        let mut req_builder = client
            .request(request.method().clone(), request.url().clone())
            .headers(request.headers().clone())
            .timeout(request.timeout());

        // Set the request body
        req_builder = match request.body() {
            Body::Empty => req_builder,
            Body::Text(text) => req_builder.body(text.clone()),
            Body::Json(json) => req_builder.json(json),
            Body::Binary(bytes) => req_builder.body(bytes.clone()),
        };

        // Send the request. For reqwest this resolves once the response
        // headers have arrived, so this marks time-to-first-byte.
        let resp = req_builder.send().await.map_err(Error::from)?;
        let ttfb = start_time.elapsed();

        // Get the status and headers
        let status = resp.status();
        let headers = resp.headers().clone();

        // Read the response body. We keep the raw bytes rather than
        // speculatively deserializing every payload as JSON: parsing on the
        // hot path is the largest per-request CPU cost and is lossy (text
        // like "123"/"null" would be coerced into a JSON body). Callers parse
        // on demand via Response::json()/text().
        let body = if status == StatusCode::NO_CONTENT {
            Body::Empty
        } else {
            let bytes = resp.bytes().await.map_err(Error::from)?;
            if bytes.is_empty() {
                Body::Empty
            } else {
                // Keep the refcounted buffer from reqwest; avoid an extra heap copy.
                Body::Binary(bytes)
            }
        };

        // Full response time now includes body transfer, not just headers.
        let response_time = start_time.elapsed();

        // Create the response
        Ok(Response::new_with_timing(
            status,
            headers,
            body,
            response_time,
            Some(ttfb),
        ))
    }

    async fn close(&self) -> Result<()> {
        // reqwest client doesn't need explicit closing
        Ok(())
    }
}

/// HTTP client factory
pub struct HttpClientFactory;

impl HttpClientFactory {
    /// Create a new HTTP client
    pub fn create() -> Result<Arc<dyn HttpClient>> {
        Ok(Arc::new(DefaultHttpClient::new()?))
    }

    /// Create a new HTTP client from an explicit [`ClientSpec`].
    pub fn from_spec(spec: ClientSpec) -> Result<Arc<dyn HttpClient>> {
        Ok(Arc::new(DefaultHttpClient::from_spec(&spec)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_builder() {
        let request = Request::get("https://localhost")
            .header("User-Agent", "load-tester")
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();

        assert_eq!(request.method(), &Method::GET);
        assert_eq!(request.url().as_str(), "https://localhost/");
        assert_eq!(request.timeout(), Duration::from_secs(10));
        assert!(request.follow_redirects());
    }

    #[test]
    fn test_request_with_json() {
        let json = serde_json::json!({
            "name": "Test",
            "value": 123
        });

        let request = Request::post("https://localhost/api")
            .json(&json)
            .build()
            .unwrap();

        assert_eq!(request.method(), &Method::POST);
        match request.body() {
            Body::Binary(bytes) => {
                let body: serde_json::Value = serde_json::from_slice(bytes).unwrap();
                assert_eq!(body["name"], "Test");
                assert_eq!(body["value"], 123);
            }
            _ => panic!("Expected pre-serialized JSON Binary body"),
        }

        let headers = request.headers();
        assert_eq!(
            headers.get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
    }

    #[test]
    fn test_build_rejects_invalid_targets() {
        // Empty / unparseable / relative / missing-scheme targets must error
        // instead of being silently rewritten to a placeholder host.
        for bad in ["", "not a url", "/api/resource", "www.example.com"] {
            assert!(
                Request::get(bad).build().is_err(),
                "expected error for target {bad:?}"
            );
        }
        // "localhost:8080" parses with scheme "localhost" and no host: reject it.
        assert!(Request::get("localhost:8080").build().is_err());
    }

    #[test]
    fn test_client_spec_from_config_maps_fields() {
        use crate::config::{GlobalConfig, HttpConfig};

        let http = HttpConfig {
            connection_timeout_ms: 1500,
            pool_idle_timeout_seconds: 45,
            max_connections_per_host: 7,
            use_http2: true,
            verify_ssl: false,
            ..HttpConfig::default()
        };

        let mut global = GlobalConfig::default();
        global
            .headers
            .insert("X-Api-Key".to_string(), "secret".to_string());

        let spec = ClientSpec::from((&http, &global));
        assert_eq!(spec.connect_timeout, Duration::from_millis(1500));
        assert_eq!(spec.pool_idle_timeout, Duration::from_secs(45));
        assert_eq!(spec.pool_max_idle_per_host, 7);
        assert!(spec.use_http2);
        assert!(!spec.verify_ssl);
        assert_eq!(spec.default_headers.get("X-Api-Key").unwrap(), "secret");

        // Both client variants (follow / no-follow) build from one spec.
        assert!(DefaultHttpClient::from_spec(&spec).is_ok());
    }

    #[test]
    fn test_build_surfaces_invalid_header() {
        // A header value with a control character is rejected by
        // HeaderValue::from_str; build() must surface it, not drop the header.
        let result = Request::get("https://localhost")
            .header("Authorization", "Bearer bad\nvalue")
            .build();
        assert!(
            result.is_err(),
            "invalid header value should fail the build"
        );
    }

    #[test]
    fn test_response_methods() {
        let mut headers = HttpHeaders::new();
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );

        let json = serde_json::json!({
            "result": "success",
            "count": 42
        });

        let response = Response::new(
            StatusCode::OK,
            headers,
            Body::Json(json),
            Duration::from_millis(123),
        );

        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.is_success());
        assert!(!response.is_client_error());
        assert!(!response.is_server_error());
        assert_eq!(response.response_time(), Duration::from_millis(123));

        match response.body() {
            Body::Json(body) => {
                assert_eq!(body["result"], "success");
                assert_eq!(body["count"], 42);
            }
            _ => panic!("Expected JSON body"),
        }
    }
}
