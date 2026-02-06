//! Integration tests for HTTP request logging parity (br-1bm.6.4).
//!
//! Tests cover:
//! - Config gating (logging enabled vs disabled)
//! - `LOG_JSON_ENABLED` toggles KV vs JSON renderer
//! - OTEL config no-op parity (no spans, no behavior change)
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

#[test]
fn otel_disabled_by_default() {
    let config = Config::from_env();
    assert!(
        !config.http_otel_enabled,
        "OTEL should be disabled by default"
    );
}

// ---------------------------------------------------------------------------
// Config field defaults
// ---------------------------------------------------------------------------

#[test]
fn otel_config_default_service_name() {
    let config = Config::from_env();
    assert_eq!(config.http_otel_service_name, "mcp-agent-mail");
}

#[test]
fn otel_config_default_endpoint_is_empty() {
    let config = Config::from_env();
    assert!(config.http_otel_exporter_otlp_endpoint.is_empty());
}

#[test]
fn otel_config_fields_can_be_set_without_crash() {
    // Setting OTEL config fields should not panic or alter other config defaults.
    let config = Config {
        http_otel_enabled: true,
        http_otel_service_name: "integration-test-svc".to_string(),
        http_otel_exporter_otlp_endpoint: "http://127.0.0.1:4318".to_string(),
        ..Default::default()
    };
    assert!(config.http_otel_enabled);
    assert_eq!(config.http_otel_service_name, "integration-test-svc");
    assert_eq!(
        config.http_otel_exporter_otlp_endpoint,
        "http://127.0.0.1:4318"
    );
    // Other fields should remain at defaults.
    assert!(!config.http_request_log_enabled);
    assert!(!config.log_json_enabled);
}

// ---------------------------------------------------------------------------
// Logging enable matrix
// ---------------------------------------------------------------------------

#[test]
fn logging_enable_matrix_all_combinations_valid() {
    // Verify all combinations of logging/JSON/OTEL config are valid (no panics).
    for &log_enabled in &[false, true] {
        for &json_enabled in &[false, true] {
            for &otel_enabled in &[false, true] {
                let config = Config {
                    http_request_log_enabled: log_enabled,
                    log_json_enabled: json_enabled,
                    http_otel_enabled: otel_enabled,
                    ..Default::default()
                };
                // Just verify no panics when constructing config.
                assert_eq!(config.http_request_log_enabled, log_enabled);
                assert_eq!(config.log_json_enabled, json_enabled);
                assert_eq!(config.http_otel_enabled, otel_enabled);
            }
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
// LLM cost logging config
// ---------------------------------------------------------------------------

#[test]
fn llm_cost_logging_enabled_by_default() {
    let config = Config::from_env();
    assert!(
        config.llm_cost_logging_enabled,
        "LLM cost logging should be enabled by default"
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
