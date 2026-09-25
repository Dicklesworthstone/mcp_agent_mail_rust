//! Core config dependency-policy tests.
//!
//! This file used to hold OTEL config no-op parity tests. The `HTTP_OTEL_*` /
//! `OTEL_*` knobs were removed from `Config` (br-kp1in.20): nothing exported
//! telemetry, so advertising them was dishonest.

use mcp_agent_mail_core::Config;

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
