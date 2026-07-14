use pummel::graph::GraphFormat;
use pummel::logging;
use pummel::prelude::*;
use pummel::telemetry::TelemetryExporterFactory;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging with verbose settings
    logging::init(true);

    // Create a simple scenario with two steps
    let step1 = StepBuilder::new(
        "get_homepage",
        "Get Homepage",
        Request::get("https://www.example.com").build()?,
    )
    .max_retries(3)
    .timeout(Duration::from_secs(5))
    .build();

    let step2 = StepBuilder::new(
        "get_about",
        "Get About Page",
        Request::get("https://www.example.com/about").build()?,
    )
    .dependency("get_homepage") // This step depends on the first step
    .max_retries(3)
    .timeout(Duration::from_secs(5))
    .build();

    let scenario = ScenarioBuilder::new("example_scenario", "Example Scenario")
        .step(step1)
        .step(step2)
        .virtual_users(2) // Use 2 virtual users
        .duration(Duration::from_secs(3)) // Run briefly so a manual run is quick
        .ramp_up(Duration::from_secs(1)) // Ramp up over 1 second
        .think_time(Duration::from_millis(200)) // Wait 200ms between requests
        .build()
        .unwrap();

    // Create an engine
    let mut engine = Engine::new();

    // Add the scenario to the engine
    engine.add_scenario(scenario);

    // Add a console telemetry exporter
    engine.with_telemetry_exporter(TelemetryExporterFactory::create_console());

    // Visualize the dependency graph
    let graph = engine.visualize_graph(GraphFormat::Mermaid)?;
    logging::info!("Dependency Graph:\n{graph}");

    // Create execution options
    let options = ExecutionOptions::builder()
        .virtual_users(2)
        .duration(Duration::from_secs(3))
        .ramp_up(Duration::from_secs(1))
        .think_time(Duration::from_millis(200))
        .max_concurrent_requests(10)
        .build();

    // Run the load test
    logging::info!("Starting load test...");
    let results = engine.run_all(options).await?;

    // Print the results
    logging::info!("-------------------------------------------------------");
    logging::info!("Load Test Results:");
    logging::info!("Total Requests:.............{}", results.total_requests);
    logging::info!(
        "Successful Requests:........{}",
        results.successful_requests
    );
    logging::info!("Failed Requests:............{}", results.failed_requests);
    logging::info!(
        "Average Response Time:......{:.2}ms",
        results.avg_response_time_ms
    );
    logging::info!(
        "P90 Response Time:..........{}ms",
        results.p90_response_time_ms
    );
    logging::info!(
        "Requests Per Second:........{:.2}",
        results.requests_per_second
    );
    logging::info!(
        "Error Rate:.................{:.2}%",
        results.error_rate * 100.0
    );
    logging::info!(
        "Duration:...................{:.2}s",
        results.duration_seconds
    );
    logging::info!("-------------------------------------------------------");
    Ok(())
}
