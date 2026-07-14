use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use pummel::engine::{Engine, ExecutionOptions};
use pummel::error::{Error, Result};
use pummel::http::{Body, HttpClient, HttpHeaders, HttpStatus, Request, Response};
use pummel::metrics::RunStatus;
use pummel::prelude::{BranchCondition, Extractor};
use pummel::scenario::{ScenarioBuilder, StepBuilder};

#[derive(Debug, Clone)]
struct SeenRequest {
    path: String,
    authorization: Option<String>,
    body: String,
}

#[derive(Default)]
struct DynamicClient {
    seen: Arc<Mutex<Vec<SeenRequest>>>,
}

#[async_trait]
impl HttpClient for DynamicClient {
    async fn send(&self, request: &Request) -> Result<Response> {
        let authorization = request
            .headers()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let body = match request.body() {
            Body::Text(text) => text.clone(),
            Body::Json(value) => value.to_string(),
            Body::Binary(bytes) => String::from_utf8_lossy(bytes).to_string(),
            Body::Empty => String::new(),
        };
        self.seen.lock().unwrap().push(SeenRequest {
            path: request.url().path().to_string(),
            authorization,
            body,
        });

        if request.url().path() == "/login" {
            Ok(Response::new(
                HttpStatus::OK,
                HttpHeaders::new(),
                Body::Text(r#"{"token":"abc123"}"#.to_string()),
                Duration::from_millis(1),
            ))
        } else {
            Ok(Response::new(
                HttpStatus::OK,
                HttpHeaders::new(),
                Body::Text("ok".to_string()),
                Duration::from_millis(1),
            ))
        }
    }

    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn dynamic_json_extraction_renders_later_header_and_body() {
    let client = Arc::new(DynamicClient::default());
    let seen = client.seen.clone();

    let mut engine = Engine::new();
    let client_for_factory = client.clone();
    engine.with_http_client_factory(move || Ok(client_for_factory.clone() as Arc<dyn HttpClient>));

    let login = StepBuilder::new(
        "login",
        "Login",
        Request::post("http://example.test/login").build().unwrap(),
    )
    .extractor(Extractor::json_path("token", "$.token"))
    .build();
    let authed = StepBuilder::new(
        "authed",
        "Authed",
        Request::post("http://example.test/authed").build().unwrap(),
    )
    .dependency("login")
    .header_template("Authorization", "Bearer {{token}}")
    .text_body_template("token={{var.token}}")
    .build();

    let scenario = ScenarioBuilder::new("s", "S")
        .step(login)
        .step(authed)
        .duration(Duration::from_secs(0))
        .build()
        .unwrap();
    engine.add_scenario(scenario);

    let results = engine.run_all(ExecutionOptions::default()).await.unwrap();
    assert_eq!(results.status, RunStatus::Completed);
    assert_eq!(results.total_requests, 2);

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    let authed = seen
        .iter()
        .find(|request| request.path == "/authed")
        .unwrap();
    assert_eq!(authed.authorization.as_deref(), Some("Bearer abc123"));
    assert_eq!(authed.body, "token=abc123");
}

#[tokio::test]
async fn false_branch_skips_step_and_unblocks_dependents() {
    let client = Arc::new(DynamicClient::default());
    let seen = client.seen.clone();

    let mut engine = Engine::new();
    let client_for_factory = client.clone();
    engine.with_http_client_factory(move || Ok(client_for_factory.clone() as Arc<dyn HttpClient>));

    let skipped = StepBuilder::new(
        "maybe",
        "Maybe",
        Request::get("http://example.test/maybe").build().unwrap(),
    )
    .branch(BranchCondition::exists("flag"))
    .build();
    let after = StepBuilder::new(
        "after",
        "After",
        Request::get("http://example.test/after").build().unwrap(),
    )
    .dependency("maybe")
    .build();

    let scenario = ScenarioBuilder::new("s", "S")
        .step(skipped)
        .step(after)
        .duration(Duration::from_secs(0))
        .build()
        .unwrap();
    engine.add_scenario(scenario);

    let results = engine.run_all(ExecutionOptions::default()).await.unwrap();
    assert_eq!(results.status, RunStatus::Completed);
    assert_eq!(results.total_requests, 1);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path, "/after");
}

struct FailingTimedClient {
    starts: Arc<Mutex<Vec<Instant>>>,
}

#[async_trait]
impl HttpClient for FailingTimedClient {
    async fn send(&self, _request: &Request) -> Result<Response> {
        self.starts.lock().unwrap().push(Instant::now());
        Err(Error::other("fail"))
    }

    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn target_rps_limits_each_retry_attempt_start() {
    let starts = Arc::new(Mutex::new(Vec::new()));
    let starts_for_factory = starts.clone();

    let mut engine = Engine::new();
    engine.with_http_client_factory(move || {
        Ok(Arc::new(FailingTimedClient {
            starts: starts_for_factory.clone(),
        }) as Arc<dyn HttpClient>)
    });

    let step = StepBuilder::new(
        "retry",
        "Retry",
        Request::get("http://example.test/retry").build().unwrap(),
    )
    .max_retries(2)
    .retry_delay(Duration::from_millis(0))
    .build();
    let scenario = ScenarioBuilder::new("s", "S")
        .step(step)
        .duration(Duration::from_secs(0))
        .build()
        .unwrap();
    engine.add_scenario(scenario);

    let options = ExecutionOptions::builder()
        .target_rps(Some(10.0))
        .duration(Duration::from_secs(0))
        .build();
    let results = engine.run_all(options).await.unwrap();
    assert_eq!(results.total_requests, 3);

    let starts = starts.lock().unwrap();
    assert_eq!(starts.len(), 3);
    assert!(starts[1].duration_since(starts[0]) >= Duration::from_millis(80));
    assert!(starts[2].duration_since(starts[1]) >= Duration::from_millis(80));
}

struct SlowOkClient;

#[async_trait]
impl HttpClient for SlowOkClient {
    async fn send(&self, _request: &Request) -> Result<Response> {
        tokio::time::sleep(Duration::from_millis(25)).await;
        Ok(Response::new(
            HttpStatus::OK,
            HttpHeaders::new(),
            Body::Text("ok".to_string()),
            Duration::from_millis(25),
        ))
    }

    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn duration_cutting_a_pass_returns_truncated_partial_results() {
    let mut engine = Engine::new();
    engine.with_http_client_factory(|| Ok(Arc::new(SlowOkClient) as Arc<dyn HttpClient>));

    let mut builder = ScenarioBuilder::new("s", "S");
    for i in 0..20 {
        let mut step = StepBuilder::new(
            format!("step_{i}"),
            format!("Step {i}"),
            Request::get(format!("http://example.test/{i}"))
                .build()
                .unwrap(),
        );
        if i > 0 {
            step = step.dependency(format!("step_{}", i - 1));
        }
        builder = builder.step(step.build());
    }
    let scenario = builder
        .duration(Duration::from_millis(100))
        .build()
        .unwrap();
    engine.add_scenario(scenario);

    let results = engine.run_all(ExecutionOptions::default()).await.unwrap();
    assert!(matches!(results.status, RunStatus::Truncated { .. }));
    assert!(results.total_requests > 0);
    assert!(results.total_requests < 20);
}

#[test]
fn config_validate_rejects_semantic_errors() {
    let bad_threshold = pummel::config::Config::from_toml_str(
        r#"
[thresholds]
max_error_rate = nan

[scenarios.s]
name = "S"
steps = ["a"]

[steps.a]
name = "A"
url = "http://example.test/"
"#,
    )
    .unwrap();
    assert!(bad_threshold.validate().is_err());

    let bad_telemetry = pummel::config::Config::from_toml_str(
        r#"
[telemetry]
enabled = true
exporter = "prometheus"

[scenarios.s]
name = "S"
steps = ["a"]

[steps.a]
name = "A"
url = "http://example.test/"
"#,
    )
    .unwrap();
    assert!(bad_telemetry.validate().is_err());
}
