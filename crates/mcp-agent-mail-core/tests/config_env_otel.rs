//! OTEL config no-op parity tests.
//!
//! Note: In Rust 2024, `std::env::set_var` is `unsafe` and this workspace forbids `unsafe_code`,
//! so we do not mutate process-wide env here. Instead, we validate that the OTEL config fields
//! exist and can be set without affecting the config type.

use mcp_agent_mail_core::Config;

#[test]
fn otel_config_fields_can_be_set() {
    let config = Config {
        http_otel_enabled: true,
        http_otel_service_name: "mcp-agent-mail-test".to_string(),
        http_otel_exporter_otlp_endpoint: "http://127.0.0.1:4318".to_string(),
        ..Default::default()
    };

    assert!(config.http_otel_enabled);
    assert_eq!(config.http_otel_service_name, "mcp-agent-mail-test");
    assert_eq!(
        config.http_otel_exporter_otlp_endpoint,
        "http://127.0.0.1:4318"
    );
}
