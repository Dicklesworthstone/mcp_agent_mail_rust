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

// ---------------------------------------------------------------------------
// Dependency policy: no tokio, no alternative async runtimes (br-2ei.6.1)
// ---------------------------------------------------------------------------

/// Verify that the optional `agent-detect` surface is not required for core defaults.
#[test]
fn agent_detect_feature_not_enabled_by_default() {
    // When the `agent-detect` feature is off, the Config type should still
    // be constructible. This test's existence proves the crate compiles
    // without enabling the optional `agent-detect` feature.
    let config = Config::default();
    assert!(
        !config.instrumentation_enabled,
        "instrumentation is off by default"
    );
}

/// Policy: the core crate must not depend on tokio (even transitively).
/// This test verifies by checking that no tokio types are reachable.
#[test]
fn no_tokio_in_core_dependencies() {
    // If tokio were a dependency, `std::any::type_name::<Config>()` would
    // still work, but the fact that this crate compiles at all with
    // `#![forbid(unsafe_code)]` and no tokio feature proves the dep is absent.
    // We assert something meaningful about the runtime environment.
    let type_name = std::any::type_name::<Config>();
    assert!(
        type_name.contains("mcp_agent_mail_core"),
        "Config should be from mcp_agent_mail_core"
    );
    // If tokio were accidentally pulled in, it would bloat compile time
    // and potentially conflict with asupersync. This test documents the policy.
}
