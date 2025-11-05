// Examples of how to use logging in routini code
//
// Run with: cargo run --example logging_examples
// Or with different log levels: RUST_LOG=debug cargo run --example logging_examples

use tracing::{debug, error, info, trace, warn};

fn main() {
    // Initialize tracing for the example
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .init();

    println!("=== Routini Logging Examples ===\n");

    println!("1. Basic logging:");
    basic_logging_example();
    println!();

    println!("2. Contextual logging:");
    contextual_logging_example("127.0.0.1:8080", 15);
    println!();

    println!("3. Error logging:");
    error_logging_example("127.0.0.1:8080", "Connection refused");
    println!();

    println!("4. Trace-level logging:");
    trace_logging_example();
    println!();

    println!("5. Span example:");
    span_example("/api/users");
    println!();

    println!("6. Performance-sensitive logging:");
    performance_sensitive_logging("127.0.0.1:8080", true);
    println!();

    println!("=== Examples Complete ===");
}

/// Example of basic logging
pub fn basic_logging_example() {
    // Simple messages
    info!("Server starting");
    debug!("Configuration loaded");

    // With structured fields
    info!(port = 3500, backends = 40, "Proxy server initialized");
}

/// Example of logging with context
pub fn contextual_logging_example(backend_addr: &str, latency_ms: u64) {
    // Log with structured data that can be queried
    info!(
        backend = %backend_addr,  // %: Display formatting
        latency_ms = latency_ms,
        status = 200,
        "Request completed successfully"
    );

    // Debug logs with more detail
    debug!(
        backend = %backend_addr,
        latency_ms = latency_ms,
        route = "/api/users",
        method = "GET",
        "Request details"
    );
}

/// Example of error logging
pub fn error_logging_example(backend: &str, error: &str) {
    // Error with context
    error!(
        backend = %backend,
        error = %error,
        "Failed to connect to backend"
    );

    // Warning when something is wrong but not critical
    warn!(
        backend = %backend,
        retry_count = 3,
        "Backend connection unstable"
    );
}

/// Example of trace-level logging for deep debugging
pub fn trace_logging_example() {
    trace!("Entering function");

    // Very detailed information
    trace!(
        step = "backend_selection",
        candidates = 5,
        "Starting backend selection algorithm"
    );

    trace!("Exiting function");
}

/// Example of conditional logging with spans
pub fn span_example(route: &str) {
    // Create a span for a logical unit of work
    let span = tracing::info_span!(
        "request_processing",
        route = %route,
    );

    // All logs within this guard will include the span context
    let _guard = span.enter();

    info!("Processing request");
    debug!("Selecting backend");
    info!("Request forwarded");
}

/// Example of logging in hot paths
pub fn performance_sensitive_logging(backend: &str, should_log_debug: bool) {
    // Always log critical info
    info!(backend = %backend, "Request forwarded");

    // Only compute expensive strings if debug is enabled
    if should_log_debug {
        debug!(
            backend = %backend,
            details = %format!("Expensive computation: {}", expensive_operation()),
            "Detailed request info"
        );
    }
}

fn expensive_operation() -> String {
    // Simulate expensive formatting
    "computed value".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_logging_examples() {
        // Initialize tracing for tests
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();

        basic_logging_example();
        contextual_logging_example("127.0.0.1:8080", 15);
        error_logging_example("127.0.0.1:8080", "Connection refused");
        trace_logging_example();
        span_example("/api/users");
        performance_sensitive_logging("127.0.0.1:8080", true);
    }
}
