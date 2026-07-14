use tracing_subscriber::util::{SubscriberInitExt, TryInitError};
use tracing_subscriber::{EnvFilter, fmt};

// Re-export tracing macros for convenience
pub use tracing::{Level, debug, error, info, instrument, span, trace, warn};

/// Default log directives in normal mode. Both the library target and the CLI
/// binary target are named `pummel`, which is handled by specifying it explicitly.
/// Everything else falls back to `warn`.
const DEFAULT_DIRECTIVES: &str = "pummel=info,warn";

/// Default log directives in verbose mode.
const VERBOSE_DIRECTIVES: &str = "pummel=trace,debug";

/// Initialize the logging system with the specified verbosity level.
///
/// This is infallible and idempotent: if a global tracing subscriber has
/// already been installed (e.g. by an embedding application, or by a previous
/// call), the request is silently ignored rather than panicking. Use
/// [`try_init`] if you need to observe whether installation succeeded.
///
/// The `RUST_LOG` environment variable, when set, takes precedence over the
/// `verbose` flag; otherwise `verbose` selects between the normal and verbose
/// default directives. All log output is written to **stderr**, leaving stdout
/// free for machine-readable results (e.g. `--format json`).
///
/// # Arguments
///
/// * `verbose` - Whether to enable verbose logging
///
/// # Example
///
/// ```no_run
/// use pummel::logging;
///
/// // Initialize with default settings
/// logging::init(false);
///
/// // A second call is a silent no-op (the first subscriber stays installed).
/// logging::init(true);
/// ```
pub fn init(verbose: bool) {
    // Discard the "a global default has already been set" error so embedders
    // with their own subscriber (and repeated `#[tokio::test]`s) don't crash.
    let _ = try_init(verbose);
    debug!("Logging initialized with verbose={}", verbose);
}

/// Initialize the logging system, returning an error if a global subscriber is
/// already installed.
///
/// Same behavior as [`init`] but surfaces the install result for callers that
/// want the signal (e.g. to know whether they own the subscriber).
///
/// `RUST_LOG` takes precedence when set; otherwise `verbose` selects the
/// default directives. Output goes to stderr.
pub fn try_init(verbose: bool) -> Result<(), TryInitError> {
    let default_directives = if verbose {
        VERBOSE_DIRECTIVES
    } else {
        DEFAULT_DIRECTIVES
    };

    // Honor RUST_LOG when set, else fall back to the verbose/normal directives.
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_directives));

    fmt::Subscriber::builder()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(true)
        .with_thread_names(false)
        .with_file(false)
        .with_line_number(false)
        .with_ansi(true)
        // Write logs to stderr so stdout is reserved for results output.
        .with_writer(std::io::stderr)
        .finish()
        .try_init()
}
