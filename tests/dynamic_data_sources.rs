use std::collections::HashSet;
use std::fs;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use pummel::config::Config;
use pummel::engine::{Engine, ExecutionOptions};
use pummel::error::Result;
use pummel::http::{Body, HttpClient, HttpHeaders, HttpStatus, Request, Response};
use pummel::metrics::RunStatus;
use pummel::scenario::{ScenarioBuilder, StepBuilder};
use tempfile::TempDir;

#[derive(Debug, Clone)]
struct SeenRequest {
    method: String,
    path: String,
    authorization: Option<String>,
    body: String,
    at: Instant,
}

#[derive(Default)]
struct CapturingClient {
    seen: Arc<Mutex<Vec<SeenRequest>>>,
}

impl CapturingClient {
    fn seen(&self) -> Arc<Mutex<Vec<SeenRequest>>> {
        self.seen.clone()
    }
}

#[async_trait]
impl HttpClient for CapturingClient {
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
            method: request.method().to_string(),
            path: request.url().path().to_string(),
            authorization,
            body: body.clone(),
            at: Instant::now(),
        });

        let response_body = if request.url().path() == "/login" {
            Body::Text(r#"{"token":"abc123"}"#.to_string())
        } else {
            Body::Text("ok".to_string())
        };
        Ok(Response::new(
            HttpStatus::OK,
            HttpHeaders::new(),
            response_body,
            Duration::from_millis(1),
        ))
    }

    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

fn temp_config(toml: &str, dir: &TempDir) -> Config {
    Config::from_toml_str(toml)
        .unwrap()
        .with_source_dir(dir.path())
}

async fn run_config(config: &Config) -> (pummel::metrics::TestResults, Vec<SeenRequest>) {
    let client = Arc::new(CapturingClient::default());
    let seen = client.seen();
    let client_for_factory = client.clone();

    let mut engine = Engine::new();
    engine.with_http_client_factory(move || Ok(client_for_factory.clone() as Arc<dyn HttpClient>));

    let results = engine.run(config).await.unwrap();
    let seen = seen.lock().unwrap().clone();
    (results, seen)
}

#[tokio::test]
async fn csv_fixture_loading_typed_columns_and_bad_configs() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("users.csv"),
        "username,age,active,tags\nalice,31,true,\"[\"\"admin\"\",\"\"beta\"\"]\"\nbob,29,false,\"[\"\"viewer\"\"]\"\n",
    )
    .unwrap();

    let config = temp_config(
        r#"
[global]
virtual_users = 2
duration_seconds = 0

[data_sources.users]
type = "csv"
path = "users.csv"
access = "per_vu"
exhaustion = "fail"

[data_sources.users.columns]
age = "integer"
active = "bool"
tags = "json"

[scenarios.s]
name = "S"
steps = ["typed"]

[steps.typed]
name = "Typed"
method = "POST"
url = "http://example.test/users"
json = '{"username":"{{data.users.username}}","age":{{data.users.age}},"active":{{data.users.active}},"tags":{{data.users.tags}}}'
"#,
        &dir,
    );
    let report = config.dynamic_lint_report().unwrap();
    assert_eq!(report.data_sources, 1);
    assert_eq!(report.dynamic_steps, 1);

    let (results, seen) = run_config(&config).await;
    assert_eq!(results.status, RunStatus::Completed);
    assert_eq!(results.total_requests, 2);

    let bodies: Vec<serde_json::Value> = seen
        .iter()
        .map(|request| serde_json::from_str(&request.body).unwrap())
        .collect();
    assert!(bodies.iter().any(|body| {
        body["username"] == "alice"
            && body["age"] == 31
            && body["active"] == true
            && body["tags"] == serde_json::json!(["admin", "beta"])
    }));
    assert!(bodies.iter().any(|body| {
        body["username"] == "bob"
            && body["age"] == 29
            && body["active"] == false
            && body["tags"] == serde_json::json!(["viewer"])
    }));

    fs::write(dir.path().join("bad.csv"), "username,age\nalice,old\n").unwrap();
    let bad_type = temp_config(
        r#"
[data_sources.users]
type = "csv"
path = "bad.csv"

[data_sources.users.columns]
age = "integer"

[scenarios.s]
name = "S"
steps = ["a"]

[steps.a]
name = "A"
url = "http://example.test/{{data.users.age}}"
"#,
        &dir,
    );
    assert!(bad_type.validate().is_err());

    let bad_path = temp_config(
        r#"
[data_sources.users]
type = "csv"
path = "missing.csv"

[scenarios.s]
name = "S"
steps = ["a"]

[steps.a]
name = "A"
url = "http://example.test/{{data.users.username}}"
"#,
        &dir,
    );
    assert!(bad_path.validate().is_err());
}

#[tokio::test]
async fn json_fixture_loading_from_root_object_and_array() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("object.json"),
        r#"{"username":"solo","count":2,"active":true,"note":null}"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("array.json"),
        r#"{"users":[{"username":"alice"},{"username":"bob"}]}"#,
    )
    .unwrap();

    let config = temp_config(
        r#"
[global]
virtual_users = 1
duration_seconds = 0

[data_sources.object_user]
type = "json"
path = "object.json"

[data_sources.array_users]
type = "json"
path = "array.json"
root = "$.users"

[scenarios.s]
name = "S"
steps = ["object", "array"]

[steps.object]
name = "Object"
method = "POST"
url = "http://example.test/object"
json = '{"username":"{{data.object_user.username}}","count":{{data.object_user.count}},"active":{{data.object_user.active}},"note":{{data.object_user.note}}}'

[steps.array]
name = "Array"
method = "POST"
url = "http://example.test/array"
json = '{"username":"{{data.array_users.username}}"}'
"#,
        &dir,
    );

    let (results, seen) = run_config(&config).await;
    assert_eq!(results.status, RunStatus::Completed);
    assert_eq!(results.total_requests, 2);
    let bodies: Vec<serde_json::Value> = seen
        .iter()
        .map(|request| serde_json::from_str(&request.body).unwrap())
        .collect();
    assert!(bodies.iter().any(|body| {
        body["username"] == "solo"
            && body["count"] == 2
            && body["active"] == true
            && body["note"].is_null()
    }));
    assert!(bodies.iter().any(|body| body["username"] == "alice"));
}

#[tokio::test]
async fn per_vu_partitioning_and_insufficient_rows() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("users.csv"), "username\nalice\nbob\n").unwrap();

    let config = temp_config(
        r#"
[global]
virtual_users = 2
duration_seconds = 0

[data_sources.users]
type = "csv"
path = "users.csv"
access = "per_vu"
exhaustion = "fail"

[scenarios.s]
name = "S"
steps = ["a"]

[steps.a]
name = "A"
url = "http://example.test/{{data.users.username}}"
"#,
        &dir,
    );
    let (results, seen) = run_config(&config).await;
    assert_eq!(results.status, RunStatus::Completed);
    let paths: HashSet<String> = seen.into_iter().map(|request| request.path).collect();
    assert_eq!(
        paths,
        HashSet::from(["/alice".to_string(), "/bob".to_string()])
    );

    let insufficient = temp_config(
        r#"
[global]
virtual_users = 3
duration_seconds = 0

[data_sources.users]
type = "csv"
path = "users.csv"
access = "per_vu"
exhaustion = "fail"

[scenarios.s]
name = "S"
steps = ["a"]

[steps.a]
name = "A"
url = "http://example.test/{{data.users.username}}"
"#,
        &dir,
    );
    assert!(insufficient.validate().is_err());
}

#[tokio::test]
async fn sequential_reuses_row_within_iteration_and_seeded_random_repeats() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("users.csv"),
        "username\nalice\nbob\ncarol\ndave\n",
    )
    .unwrap();

    let sequential = temp_config(
        r#"
[global]
virtual_users = 1
duration_seconds = 0

[data_sources.users]
type = "csv"
path = "users.csv"
access = "sequential"
exhaustion = "fail"

[scenarios.s]
name = "S"
steps = ["first", "second"]

[steps.first]
name = "First"
url = "http://example.test/seq/{{data.users.username}}/first"

[steps.second]
name = "Second"
url = "http://example.test/seq/{{data.users.username}}/second"
dependencies = ["first"]
"#,
        &dir,
    );
    let (_, seen) = run_config(&sequential).await;
    let paths: HashSet<String> = seen.into_iter().map(|request| request.path).collect();
    assert_eq!(
        paths,
        HashSet::from([
            "/seq/alice/first".to_string(),
            "/seq/alice/second".to_string()
        ])
    );

    let random = temp_config(
        r#"
[global]
virtual_users = 4
duration_seconds = 0

[data_sources.users]
type = "csv"
path = "users.csv"
access = "random"
exhaustion = "wrap"
seed = 42

[scenarios.s]
name = "S"
steps = ["a"]

[steps.a]
name = "A"
url = "http://example.test/r/{{vu.id}}/{{data.users.username}}"
"#,
        &dir,
    );
    let (_, first_seen) = run_config(&random).await;
    let (_, second_seen) = run_config(&random).await;
    let mut first_paths: Vec<String> = first_seen.into_iter().map(|request| request.path).collect();
    let mut second_paths: Vec<String> = second_seen
        .into_iter()
        .map(|request| request.path)
        .collect();
    first_paths.sort();
    second_paths.sort();
    assert_eq!(first_paths, second_paths);
}

#[tokio::test]
async fn data_driven_login_profile_flow_uses_fixture_and_extracted_token() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("users.csv"),
        "username,password\nalice,secret\n",
    )
    .unwrap();

    let config = temp_config(
        r#"
[global]
virtual_users = 1
duration_seconds = 0

[data_sources.users]
type = "csv"
path = "users.csv"
access = "per_vu"
exhaustion = "fail"

[scenarios.auth]
name = "Auth"
steps = ["login", "profile"]

[steps.login]
name = "Login"
method = "POST"
url = "http://example.test/login"
json = '{"username":"{{data.users.username}}","password":"{{data.users.password}}"}'

[[steps.login.extractors]]
name = "token"
json_path = "$.token"
required = true

[steps.profile]
name = "Profile"
url = "http://example.test/profile/{{data.users.username}}"
dependencies = ["login"]

[steps.profile.headers]
Authorization = "Bearer {{token}}"
"#,
        &dir,
    );

    let (results, seen) = run_config(&config).await;
    assert_eq!(results.status, RunStatus::Completed);
    assert_eq!(results.total_requests, 2);
    let login = seen
        .iter()
        .find(|request| request.path == "/login")
        .unwrap();
    let login_body: serde_json::Value = serde_json::from_str(&login.body).unwrap();
    assert_eq!(login_body["username"], "alice");
    assert_eq!(login_body["password"], "secret");
    let profile = seen
        .iter()
        .find(|request| request.path == "/profile/alice")
        .unwrap();
    assert_eq!(profile.authorization.as_deref(), Some("Bearer abc123"));
}

#[tokio::test]
async fn numeric_and_regex_branch_conditions_validate_and_execute() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("users.csv"), "username,age\nalice,21\n").unwrap();

    let config = temp_config(
        r#"
[global]
virtual_users = 1
duration_seconds = 0

[data_sources.users]
type = "csv"
path = "users.csv"

[data_sources.users.columns]
age = "integer"

[scenarios.s]
name = "S"
steps = ["adult", "regex"]

[steps.adult]
name = "Adult"
url = "http://example.test/adult"

[steps.adult.branch]
variable = "data.users.age"
condition = "greater_than"
value = "18"

[steps.regex]
name = "Regex"
url = "http://example.test/regex"

[steps.regex.branch]
variable = "data.users.username"
condition = "matches_regex"
value = "^ali"
"#,
        &dir,
    );
    let (results, seen) = run_config(&config).await;
    assert_eq!(results.status, RunStatus::Completed);
    let paths: HashSet<String> = seen.into_iter().map(|request| request.path).collect();
    assert_eq!(
        paths,
        HashSet::from(["/adult".to_string(), "/regex".to_string()])
    );

    let invalid_number = temp_config(
        r#"
[data_sources.users]
type = "csv"
path = "users.csv"

[scenarios.s]
name = "S"
steps = ["a"]

[steps.a]
name = "A"
url = "http://example.test/a"

[steps.a.branch]
variable = "data.users.age"
condition = "less_than"
value = "young"
"#,
        &dir,
    );
    assert!(invalid_number.validate().is_err());

    let invalid_regex = temp_config(
        r#"
[data_sources.users]
type = "csv"
path = "users.csv"

[scenarios.s]
name = "S"
steps = ["a"]

[steps.a]
name = "A"
url = "http://example.test/a"

[steps.a.branch]
variable = "data.users.username"
condition = "matches_regex"
value = "["
"#,
        &dir,
    );
    assert!(invalid_regex.validate().is_err());
}

#[test]
fn missing_dynamic_references_fail_validation_before_run() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("users.csv"), "username\nalice\n").unwrap();

    let missing_source = temp_config(
        r#"
[scenarios.s]
name = "S"
steps = ["a"]

[steps.a]
name = "A"
url = "http://example.test/{{data.users.username}}"
"#,
        &dir,
    );
    assert!(missing_source.validate().is_err());

    let missing_path = temp_config(
        r#"
[data_sources.users]
type = "csv"
path = "users.csv"

[scenarios.s]
name = "S"
steps = ["a"]

[steps.a]
name = "A"
url = "http://example.test/{{data.users.password}}"
"#,
        &dir,
    );
    assert!(missing_path.validate().is_err());

    let missing_var = temp_config(
        r#"
[scenarios.s]
name = "S"
steps = ["a"]

[steps.a]
name = "A"
url = "http://example.test/{{token}}"
"#,
        &dir,
    );
    assert!(missing_var.validate().is_err());

    let unknown_branch = temp_config(
        r#"
[scenarios.s]
name = "S"
steps = ["a"]

[steps.a]
name = "A"
url = "http://example.test/a"

[steps.a.branch]
variable = "flag"
condition = "exists"
"#,
        &dir,
    );
    assert!(unknown_branch.validate().is_err());
}

#[tokio::test]
async fn static_request_builder_scenarios_run_without_dynamic_rendering() {
    let client = Arc::new(CapturingClient::default());
    let seen = client.seen();
    let client_for_factory = client.clone();

    let mut engine = Engine::new();
    engine.with_http_client_factory(move || Ok(client_for_factory.clone() as Arc<dyn HttpClient>));

    let step = StepBuilder::new(
        "static",
        "Static",
        Request::get("http://example.test/static").build().unwrap(),
    )
    .build();
    let scenario = ScenarioBuilder::new("s", "S")
        .step(step)
        .duration(Duration::from_secs(0))
        .build()
        .unwrap();
    engine.add_scenario(scenario);

    let results = engine.run_all(ExecutionOptions::default()).await.unwrap();
    assert_eq!(results.status, RunStatus::Completed);
    assert_eq!(results.total_requests, 1);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path, "/static");
}

#[tokio::test]
async fn scenarios_bind_only_referenced_data_sources() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("users.csv"), "username\nalice\nbob\n").unwrap();
    fs::write(dir.path().join("unused.csv"), "username\nsolo\n").unwrap();

    let static_config = temp_config(
        r#"
[global]
virtual_users = 3
duration_seconds = 0

[data_sources.unused]
type = "csv"
path = "unused.csv"
access = "per_vu"
exhaustion = "fail"

[scenarios.static]
name = "Static"
steps = ["a"]

[steps.a]
name = "A"
url = "http://example.test/static"
"#,
        &dir,
    );
    static_config.validate().unwrap();
    let static_scenarios = static_config.build_scenarios().unwrap();
    assert!(static_scenarios[0].data_sources.is_empty());
    let (results, seen) = run_config(&static_config).await;
    assert_eq!(results.status, RunStatus::Completed);
    assert_eq!(results.total_requests, 3);
    assert!(seen.iter().all(|request| request.path == "/static"));

    let dynamic_config = temp_config(
        r#"
[global]
virtual_users = 2
duration_seconds = 0

[data_sources.users]
type = "csv"
path = "users.csv"
access = "per_vu"
exhaustion = "fail"

[data_sources.unused]
type = "csv"
path = "unused.csv"
access = "per_vu"
exhaustion = "fail"

[scenarios.dynamic]
name = "Dynamic"
steps = ["a"]

[steps.a]
name = "A"
url = "http://example.test/users/{{data.users.username}}"
"#,
        &dir,
    );
    dynamic_config.validate().unwrap();
    let dynamic_scenarios = dynamic_config.build_scenarios().unwrap();
    let sources: HashSet<_> = dynamic_scenarios[0].data_sources.keys().cloned().collect();
    assert_eq!(sources, HashSet::from(["users".to_string()]));

    let (results, seen) = run_config(&dynamic_config).await;
    assert_eq!(results.status, RunStatus::Completed);
    assert_eq!(results.total_requests, 2);
    let paths: HashSet<_> = seen.into_iter().map(|request| request.path).collect();
    assert_eq!(
        paths,
        HashSet::from(["/users/alice".to_string(), "/users/bob".to_string()])
    );
}

#[test]
fn extractor_variables_must_come_from_required_unbranched_dependencies() {
    let same_step = Config::from_toml_str(
        r#"
[scenarios.s]
name = "S"
steps = ["login"]

[steps.login]
name = "Login"
url = "http://example.test/{{token}}"

[[steps.login.extractors]]
name = "token"
json_path = "$.token"
"#,
    )
    .unwrap();
    assert!(same_step.validate().is_err());

    let independent_or_later = Config::from_toml_str(
        r#"
[scenarios.s]
name = "S"
steps = ["profile", "login"]

[steps.profile]
name = "Profile"
url = "http://example.test/{{token}}"

[steps.login]
name = "Login"
url = "http://example.test/login"

[[steps.login.extractors]]
name = "token"
json_path = "$.token"
"#,
    )
    .unwrap();
    assert!(independent_or_later.validate().is_err());

    let transitive_dependency = Config::from_toml_str(
        r#"
[scenarios.s]
name = "S"
steps = ["login", "middle", "profile"]

[steps.login]
name = "Login"
url = "http://example.test/login"

[[steps.login.extractors]]
name = "token"
json_path = "$.token"
required = true

[steps.middle]
name = "Middle"
url = "http://example.test/middle"
dependencies = ["login"]

[steps.profile]
name = "Profile"
url = "http://example.test/profile"
dependencies = ["middle"]

[steps.profile.headers]
Authorization = "Bearer {{token}}"
"#,
    )
    .unwrap();
    transitive_dependency.validate().unwrap();

    let optional_extractor = Config::from_toml_str(
        r#"
[scenarios.s]
name = "S"
steps = ["login", "profile"]

[steps.login]
name = "Login"
url = "http://example.test/login"

[[steps.login.extractors]]
name = "token"
json_path = "$.token"
required = false

[steps.profile]
name = "Profile"
url = "http://example.test/profile/{{token}}"
dependencies = ["login"]
"#,
    )
    .unwrap();
    assert!(optional_extractor.validate().is_err());

    let branch_conditioned_extractor = Config::from_toml_str(
        r#"
[scenarios.s]
name = "S"
steps = ["login", "profile"]

[steps.login]
name = "Login"
url = "http://example.test/login"

[steps.login.branch]
variable = "vu.id"
condition = "exists"

[[steps.login.extractors]]
name = "token"
json_path = "$.token"
required = true

[steps.profile]
name = "Profile"
url = "http://example.test/profile/{{token}}"
dependencies = ["login"]
"#,
    )
    .unwrap();
    assert!(branch_conditioned_extractor.validate().is_err());
}

#[tokio::test]
async fn config_http_methods_are_normalized_or_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let lowercase_standard = temp_config(
        r#"
[global]
duration_seconds = 0

[scenarios.s]
name = "S"
steps = ["a"]

[steps.a]
name = "A"
method = "post"
url = "http://example.test/post"
"#,
        &dir,
    );
    lowercase_standard.validate().unwrap();
    let (_results, seen) = run_config(&lowercase_standard).await;
    assert_eq!(seen[0].method, "POST");

    let lowercase_custom = Config::from_toml_str(
        r#"
[scenarios.s]
name = "S"
steps = ["a"]

[steps.a]
name = "A"
method = "propfind"
url = "http://example.test/custom"
"#,
    )
    .unwrap();
    assert!(lowercase_custom.validate().is_err());
}

#[tokio::test]
async fn rate_limited_sends_stop_at_deadline_and_report_truncated() {
    let client = Arc::new(CapturingClient::default());
    let seen = client.seen();
    let client_for_factory = client.clone();

    let mut engine = Engine::new();
    engine.with_http_client_factory(move || Ok(client_for_factory.clone() as Arc<dyn HttpClient>));

    let step = StepBuilder::new(
        "rate",
        "Rate",
        Request::get("http://example.test/rate").build().unwrap(),
    )
    .build();
    let scenario = ScenarioBuilder::new("s", "S")
        .step(step)
        .virtual_users(1)
        .duration(Duration::from_millis(100))
        .think_time(Duration::from_secs(0))
        .build()
        .unwrap();
    engine.add_scenario(scenario);

    let start = Instant::now();
    let options = ExecutionOptions::builder().target_rps(Some(1.0)).build();
    let results = engine.run_all(options).await.unwrap();
    let elapsed = start.elapsed();

    assert!(matches!(results.status, RunStatus::Truncated { .. }));
    assert_eq!(results.total_requests, 1);
    assert!(
        elapsed < Duration::from_millis(500),
        "run slept for a rate permit past the deadline: {elapsed:?}"
    );

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].at.duration_since(start) < Duration::from_millis(500));
}

#[test]
fn run_status_json_uses_flat_reason_shape() {
    let failed = RunStatus::Failed {
        reason: "boom".to_string(),
    };
    let failed_json = serde_json::to_value(&failed).unwrap();
    assert_eq!(
        failed_json,
        serde_json::json!({"kind": "failed", "reason": "boom"})
    );
    assert_eq!(
        serde_json::from_value::<RunStatus>(failed_json).unwrap(),
        failed
    );

    let truncated = RunStatus::Truncated {
        reason: "deadline".to_string(),
    };
    let truncated_json = serde_json::to_value(&truncated).unwrap();
    assert_eq!(
        truncated_json,
        serde_json::json!({"kind": "truncated", "reason": "deadline"})
    );
    assert_eq!(
        serde_json::from_value::<RunStatus>(truncated_json).unwrap(),
        truncated
    );
}
