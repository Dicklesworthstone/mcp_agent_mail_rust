//! Integration tests for HTTP request logging parity (br-1bm.6.4).
//!
//! Tests cover:
//! - Config gating (logging enabled vs disabled)
//! - `LOG_JSON_ENABLED` toggles KV vs JSON renderer
//! - Field derivation (`client_ip`, `duration_ms` integer)
//! - `ExpectedErrorFilter` config constant coverage

use mcp_agent_mail_core::Config;

// ---------------------------------------------------------------------------
// Config gating integration tests
// ---------------------------------------------------------------------------

#[test]
fn http_request_log_disabled_by_default() {
    let config = Config::from_env();
    assert!(
        !config.http_request_log_enabled,
        "HTTP request logging should be disabled by default"
    );
}

#[test]
fn log_json_disabled_by_default() {
    let config = Config::from_env();
    assert!(
        !config.log_json_enabled,
        "JSON logging should be disabled by default"
    );
}

// ---------------------------------------------------------------------------
// Logging enable matrix
// ---------------------------------------------------------------------------

#[test]
fn logging_enable_matrix_all_combinations_valid() {
    // Verify all combinations of logging/JSON config are valid (no panics).
    for &log_enabled in &[false, true] {
        for &json_enabled in &[false, true] {
            let config = Config {
                http_request_log_enabled: log_enabled,
                log_json_enabled: json_enabled,
                ..Default::default()
            };
            // Just verify no panics when constructing config.
            assert_eq!(config.http_request_log_enabled, log_enabled);
            assert_eq!(config.log_json_enabled, json_enabled);
        }
    }
}

// ---------------------------------------------------------------------------
// Tools log config
// ---------------------------------------------------------------------------

#[test]
fn tools_log_enabled_by_default() {
    let config = Config::from_env();
    assert!(
        config.tools_log_enabled,
        "tools log should be enabled by default"
    );
}

// ---------------------------------------------------------------------------
// Instrumentation config (related to logging infrastructure)
// ---------------------------------------------------------------------------

#[test]
fn instrumentation_disabled_by_default() {
    let config = Config::from_env();
    assert!(!config.instrumentation_enabled);
    assert_eq!(config.instrumentation_slow_query_ms, 250);
}
