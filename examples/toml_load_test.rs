use pummel::config::Config;
use pummel::engine::Engine;
use pummel::logging;
use std::path::Path;

/// This example demonstrates how to load a load test configuration from a TOML file
/// and run it using the load-tester library.
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging with default settings
    logging::init(true);

    // Path to the TOML configuration file
    let config_path = Path::new("examples/config_example.toml");

    // Load the configuration from the TOML file
    logging::info!("Loading configuration from {}", config_path.display());
    let config = Config::from_toml(config_path)?;

    let engine = Engine::new();

    // Run the load test
    logging::info!("Starting load test...");
    let results = engine.run(&config).await?;

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
