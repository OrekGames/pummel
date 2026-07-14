# Pummel

A high-throughput, multithreaded HTTP load testing library and CLI tool for Rust.

## Features

- **Flexible Scenario Definition**: Create complex load testing scenarios with multiple steps and dependencies
- **Concurrent Execution**: Run tests with multiple virtual users in parallel
- **Comprehensive Metrics**: Collect detailed metrics on response times, throughput, and error rates
- **Dependency Graph Visualization**: Generate and visualize dependency graphs of your test scenarios
- **Multiple Configuration Formats**: Support for both YAML and TOML configuration files
- **CLI Tool**: Run load tests from the command line with rich output options
- **Telemetry Integration**: Pluggable telemetry exporters (newline-delimited JSON, console, or your own `TelemetryExporter`) for streaming per-request and summary metrics. OpenTelemetry/Prometheus exporters are not yet implemented and error if selected.
- **Dynamic Scenarios**: Bind CSV/JSON fixture rows, render request templates, extract response values, and branch later steps from runtime state.
- **Extensible Architecture**: Easily extend with custom HTTP clients and metrics collectors

## Installation

**Preferred for Rust users:** install from [crates.io](https://crates.io/crates/pummel):

```bash
cargo install pummel --locked
```

> The first crates.io publish and checksum-verified GitHub Release are forthcoming
> until tag `v0.1.0` exists. Until then, build from source.

### Library

Add the library to your `Cargo.toml`:

```toml
[dependencies]
pummel = "0.1.0"
```

### Automated binary installers

Installers discover the latest stable GitHub Release, download the platform
archive and `checksums-sha256.txt`, verify the exact-filename SHA-256, and install
the binary. No `minisign` (or other signing tool) is required.

```bash
curl -fsSL https://raw.githubusercontent.com/OrekGames/pummel/main/scripts/install.sh | bash
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/OrekGames/pummel/main/scripts/install.ps1 | iex
```

See the [Installation Documentation](docs/installation.md) for manual verification
steps and the maintainer release checklist.

### Build from Source

Build the CLI tool from source:

```bash
git clone https://github.com/OrekGames/pummel.git
cd pummel
cargo build --release
```

The CLI binary will be available at `target/release/pummel`.

## CLI Usage

The `pummel` tool allows you to run load tests using configuration files.

### Basic Usage

```bash
# Run a load test with YAML configuration
./target/release/pummel --config examples/config_example.yaml

# Run a load test with TOML configuration
./target/release/pummel --config examples/config_example.toml

# Print the dependency graph and exit (does NOT run the test)
./target/release/pummel --config examples/config_example.yaml --graph

# Output results as clean JSON on stdout (logs go to stderr), pipeable to jq
./target/release/pummel --config examples/config_example.yaml --format json | jq

# Validate a config in CI without generating load (non-zero exit if invalid)
./target/release/pummel --config examples/config_example.yaml --dry-run

# Override load parameters from the command line
./target/release/pummel --config examples/config_example.yaml --users 100 --duration 60 --ramp-up 10

# Gate a CI run: fail (exit code 2) if the error rate exceeds 1%
./target/release/pummel --config examples/config_example.yaml --max-error-rate 0.01

# Enable verbose logging (logs are written to stderr)
./target/release/pummel --config examples/config_example.yaml --verbose
```

### CLI Options

```
Options:
  -c, --config <CONFIG>              Path to the configuration file (YAML or TOML)
  -f, --format <FORMAT>              Output format for results [default: text] [possible values: text, json]
  -g, --graph                        Print the dependency graph and exit without running the test
  -G, --graph-format <GRAPH_FORMAT>  Graph output format [default: mermaid] [possible values: mermaid, dot]
      --dry-run                      Validate the config and exit without generating load [aliases: --validate]
      --users <USERS>                Override the global number of virtual users
      --duration <DURATION>          Override the global sustained-load duration, in seconds
      --ramp-up <RAMP_UP>            Override the global ramp-up period, in seconds
      --target-rps <TARGET_RPS>      Aggregate request-attempt starts per second across active scenarios
      --max-error-rate <RATE>        Fail the run (exit code 2) if the error rate exceeds this value (0.0-1.0)
  -v, --verbose                      Enable verbose output
  -h, --help                         Print help
  -V, --version                      Print version
```

### Output and exit codes

Results are written to **stdout** (raw JSON with `--format json`, human-readable text
otherwise) while all logs go to **stderr**, so `--format json | jq` and `2>/dev/null`
both work. The process exit code follows a stable contract for CI gating:

| Code | Meaning |
| ---- | ------- |
| `0`  | Run completed and all configured thresholds passed |
| `1`  | Usage/config/build error, or a truncated/failed run with partial results |
| `2`  | A pass/fail threshold was breached |

Thresholds can be set in a `[thresholds]` config section (`max_error_rate`,
`max_p90_ms`, `min_requests`) and `--max-error-rate` overrides the config value.

`--target-rps` is an aggregate cap for request-attempt starts across the whole
run. Retries consume permits too. When multiple scenarios are active, the CLI
splits the aggregate rate evenly across them; stage-level `target_rps` values
are used only when the CLI override is omitted.

## Configuration Format

### YAML Configuration

```yaml
# Global settings that apply to all scenarios
global:
  virtual_users: 5
  duration_seconds: 30
  ramp_up_seconds: 0
  think_time_ms: 0

scenarios:
  example_scenario:
    name: "Example Scenario"
    steps:
      - "get_homepage"
      - "get_about"
    # Override global settings if needed
    virtual_users: 10

steps:
  get_homepage:
    name: "Get Homepage"
    method: "GET"
    url: "https://www.example.com/"
    timeout_ms: 5000
    max_retries: 3
    follow_redirects: true
  
  get_about:
    name: "Get About Page"
    method: "GET"
    url: "https://www.example.com/about"
    timeout_ms: 5000
    max_retries: 3
    follow_redirects: true
    dependencies:
      - "get_homepage"  # This step depends on the first step
```

### TOML Configuration

```toml
[global]
virtual_users = 5
duration_seconds = 30
ramp_up_seconds = 0
think_time_ms = 0

[scenarios.example_scenario]
name = "Example Scenario"
steps = ["get_homepage", "get_about"]
virtual_users = 10

[steps.get_homepage]
name = "Get Homepage"
method = "GET"
url = "https://www.example.com/"
timeout_ms = 5000
max_retries = 3
follow_redirects = true

[steps.get_about]
name = "Get About Page"
method = "GET"
url = "https://www.example.com/about"
timeout_ms = 5000
max_retries = 3
follow_redirects = true
dependencies = ["get_homepage"]
```

### Dynamic Requests, Fixture Data, and Staged Profiles

Step URLs, headers, text bodies, and JSON bodies can use `{{name}}` or
`{{var.name}}` templates populated from per-virtual-user state. Built-ins
include `{{vu.id}}`, `{{scenario.id}}`, `{{step.id}}`, `{{iteration}}`,
`{{uuid}}`, `{{random.u64}}`, and `{{random.int:min:max}}`. Data sources use
`{{data.<source>.<path>}}` templates and are defined at the top level.

```toml
[data_sources.users]
type = "csv"
path = "fixtures/users.csv"
access = "per_vu"
exhaustion = "fail"

[scenarios.auth_flow]
name = "Authenticated Flow"
steps = ["login", "profile"]

[[scenarios.auth_flow.load_profile.stages]]
name = "warmup"
duration_seconds = 10
virtual_users = 2
target_rps = 5

[[scenarios.auth_flow.load_profile.stages]]
name = "steady"
duration_seconds = 60
virtual_users = 20
target_rps = 50

[steps.login]
name = "Login"
method = "POST"
url = "https://api.example.com/login"
json = '{"username":"{{data.users.username}}","password":"{{data.users.password}}"}'

[[steps.login.extractors]]
name = "token"
json_path = "$.token"
required = true

[steps.profile]
name = "Profile"
method = "GET"
url = "https://api.example.com/profile/{{data.users.username}}"
dependencies = ["login"]

[steps.profile.headers]
Authorization = "Bearer {{token}}"
```

Extractors support JSON dot paths such as `$.token` and `$.items[0].id`, body
regexes, header extraction, header regexes, and status-code extraction. Branch
conditions (`exists`, `equals`, `not_equals`, numeric comparisons, and
`matches_regex`) can skip a step without emitting a request while still
satisfying dependencies.

See [Dynamic Scenarios](docs/dynamic-scenarios.md) for fixture sources,
template stringification, row access modes, validation, and the supported
JSON-path subset.

## Library Usage

### Basic Example

```rust
use pummel::prelude::*;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    logging::init(true);

    // Create steps
    let step1 = StepBuilder::new(
        "get_homepage",
        "Get Homepage",
        Request::get("https://example.com").build()?,
    )
    .max_retries(3)
    .timeout(Duration::from_secs(5))
    .build();

    let step2 = StepBuilder::new(
        "get_about",
        "Get About Page",
        Request::get("https://example.com/about").build()?,
    )
    .dependency("get_homepage")
    .max_retries(3)
    .timeout(Duration::from_secs(5))
    .build();

    // Create scenario
    let scenario = ScenarioBuilder::new("example_scenario", "Example Scenario")
        .step(step1)
        .step(step2)
        .virtual_users(5)
        .duration(Duration::from_secs(30))
        .ramp_up(Duration::from_secs(5))
        .think_time(Duration::from_millis(500))
        .build()?;

    // Create and configure engine
    let mut engine = Engine::new();
    engine.add_scenario(scenario);
    engine.with_telemetry_exporter(TelemetryExporterFactory::create_console());

    // Run the load test. ExecutionOptions is #[non_exhaustive]; build it with
    // the fluent builder (or ExecutionOptions::default() + field mutation).
    let options = ExecutionOptions::builder()
        .virtual_users(5)
        .duration(Duration::from_secs(30))
        .ramp_up(Duration::from_secs(5))
        .think_time(Duration::from_millis(500))
        .max_concurrent_requests(10)
        .build();

    let results = engine.run_all(options).await?;

    // Print results
    println!("Total Requests: {}", results.total_requests);
    println!("Successful Requests: {}", results.successful_requests);
    println!("Average Response Time: {:.2}ms", results.avg_response_time_ms);
    println!("Requests Per Second: {:.2}", results.requests_per_second);

    Ok(())
}
```

### Using Configuration Files

`Engine` is the single entry point. `Engine::run` loads the `[http]`,
`[metrics]`, and `[telemetry]` config sections and wires the client, collector,
and exporter automatically.

```rust
use pummel::prelude::*;

#[tokio::main]
async fn main() -> Result<()> {
    // Load from YAML (or Config::from_toml for TOML)
    let config = Config::from_yaml("config.yaml")?;

    let mut engine = Engine::new();
    let results = engine.run(&config).await?;

    println!("Test completed with {} requests", results.total_requests);
    Ok(())
}
```

## Examples

See the `examples/` directory for complete working examples:

- `simple_load_test.rs` - Basic programmatic usage
- `toml_load_test.rs` - Using TOML configuration
- `config_example.yaml` - YAML configuration example
- `config_example.toml` - TOML configuration example
- `dynamic_login.toml` - Data-driven dynamic scenario example
- `fixtures/users.csv` - Fixture data for dynamic examples

## License

This project is licensed under the MIT License.
