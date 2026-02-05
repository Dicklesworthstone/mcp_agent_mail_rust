//! Integration tests for TOON output format.
//!
//! These tests exercise the full TOON pipeline (format resolution → encoder
//! invocation → envelope construction) using a deterministic stub encoder
//! (`scripts/toon_stub_encoder.sh`).
//!
//! All tests are offline — they do not require `tru` installed.

use std::collections::HashMap;
use std::path::PathBuf;

use mcp_agent_mail_core::config::Config;
use mcp_agent_mail_core::toon::{
    apply_resource_format, apply_toon_format, apply_tool_format, looks_like_toon_rust_encoder,
    resolve_output_format, run_encoder, validate_encoder,
};

/// Path to the stub encoder script.
fn stub_encoder_path() -> String {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/
    path.pop(); // workspace root
    path.push("scripts");
    path.push("toon_stub_encoder.sh");
    assert!(path.exists(), "stub encoder not found at {path:?}");
    path.to_string_lossy().to_string()
}

/// Path to a stub encoder that always fails on --encode.
fn failing_stub_encoder_path() -> String {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/
    path.pop(); // workspace root
    path.push("scripts");
    path.push("toon_stub_encoder_fail.sh");
    assert!(path.exists(), "failing stub encoder not found at {path:?}");
    path.to_string_lossy().to_string()
}

fn stub_config() -> Config {
    Config {
        toon_bin: Some(stub_encoder_path()),
        toon_stats_enabled: false,
        output_format_default: None,
        ..Config::default()
    }
}

fn stub_config_with_stats() -> Config {
    Config {
        toon_bin: Some(stub_encoder_path()),
        toon_stats_enabled: true,
        output_format_default: None,
        ..Config::default()
    }
}

// ---------------------------------------------------------------------------
// Encoder validation with stub
// ---------------------------------------------------------------------------

#[test]
fn stub_encoder_passes_validation() {
    let parts = vec![stub_encoder_path()];
    let result = validate_encoder(&parts);
    assert!(result.is_ok(), "stub encoder should pass validation: {result:?}");
}

#[test]
fn stub_encoder_looks_like_toon_rust() {
    let path = stub_encoder_path();
    let result = looks_like_toon_rust_encoder(&path);
    assert!(result.is_ok());
    assert!(result.unwrap(), "stub encoder should look like toon_rust");
}

#[test]
fn nonexistent_encoder_fails_validation() {
    let parts = vec!["/nonexistent/tru_binary".to_string()];
    let result = validate_encoder(&parts);
    assert!(result.is_err());
    let err = result.unwrap_err();
    // Nonexistent binary: either "not found" or "does not look like toon_rust"
    assert!(
        err.contains("not found") || err.contains("not look like"),
        "unexpected error: {err}"
    );
}

#[test]
fn toon_basename_rejected() {
    // A binary named exactly "toon" should be rejected (Node.js protection)
    let result = looks_like_toon_rust_encoder("toon");
    match result {
        Ok(false) => {} // expected: rejected based on basename
        Ok(true) => panic!("should reject 'toon' basename"),
        Err(_) => {} // also acceptable: binary not found
    }
}

#[test]
fn toon_exe_basename_rejected() {
    let result = looks_like_toon_rust_encoder("toon.exe");
    match result {
        Ok(false) => {}
        Ok(true) => panic!("should reject 'toon.exe' basename"),
        Err(_) => {}
    }
}

// ---------------------------------------------------------------------------
// run_encoder with stub
// ---------------------------------------------------------------------------

#[test]
fn run_encoder_stub_success() {
    let config = stub_config();
    let result = run_encoder(&config, r#"{"id":1,"subject":"Test"}"#);
    assert!(result.is_ok(), "run_encoder should succeed: {}", result.err().map(|e| e.to_error_string()).unwrap_or_default());
    let success = result.unwrap();
    assert!(success.encoded.contains("~stub_toon_output"));
    assert!(success.encoded.contains("payload_length:"));
    assert!(success.stats.is_none(), "stats should be None when not enabled");
    assert!(success.stats_raw.is_none());
}

#[test]
fn run_encoder_stub_with_stats() {
    let config = stub_config_with_stats();
    let result = run_encoder(&config, r#"{"id":1}"#);
    assert!(result.is_ok());
    let success = result.unwrap();
    assert!(success.encoded.contains("~stub_toon_output"));
    let stats = success.stats.expect("stats should be parsed");
    assert_eq!(stats.json_tokens, 25);
    assert_eq!(stats.toon_tokens, 12);
    assert_eq!(stats.saved_tokens, Some(13));
    assert!((stats.saved_percent.unwrap() - (-52.0)).abs() < 0.01);
}

#[test]
fn run_encoder_stub_failure() {
    let config = Config {
        toon_bin: Some(failing_stub_encoder_path()),
        toon_stats_enabled: false,
        ..Config::default()
    };
    let result = run_encoder(&config, r#"{"id":1}"#);
    assert!(result.is_err());
    let err = result.unwrap_err();
    let msg = err.to_error_string();
    assert!(msg.contains("exited with"), "expected non-zero exit error: {msg}");
    assert!(err.stderr().is_some());
    assert!(err.stderr().unwrap().contains("simulated encoder failure"));
}

#[test]
fn run_encoder_nonexistent_binary() {
    let config = Config {
        toon_bin: Some("/nonexistent/tru_binary".to_string()),
        toon_stats_enabled: false,
        ..Config::default()
    };
    let result = run_encoder(&config, r#"{"id":1}"#);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_error_string();
    assert!(msg.contains("not found") || msg.contains("not look like"), "unexpected error: {msg}");
}

// ---------------------------------------------------------------------------
// Tool output envelope (mirrors test_tool_format_toon_envelope)
// ---------------------------------------------------------------------------

#[test]
fn tool_format_toon_envelope_with_stats() {
    let config = stub_config_with_stats();
    let payload = serde_json::json!({
        "status": "ok",
        "project": "backend"
    });
    let envelope = apply_toon_format(&payload, Some("toon"), &config)
        .expect("should not error")
        .expect("should return Some for toon format");

    assert_eq!(envelope.format, "toon");
    assert!(envelope.data.is_string(), "toon-encoded data should be a string");

    assert_eq!(envelope.meta.requested, Some("toon".to_string()));
    assert_eq!(envelope.meta.source, "param");
    assert!(envelope.meta.encoder.is_some());
    assert!(envelope.meta.toon_error.is_none());

    let stats = envelope.meta.toon_stats.expect("stats should be present");
    assert_eq!(stats.json_tokens, 25);
    assert_eq!(stats.toon_tokens, 12);
}

#[test]
fn tool_format_toon_envelope_without_stats() {
    let config = stub_config();
    let payload = serde_json::json!({"status": "ok"});
    let envelope = apply_toon_format(&payload, Some("toon"), &config)
        .unwrap()
        .unwrap();

    assert_eq!(envelope.format, "toon");
    assert!(envelope.data.is_string());
    assert!(envelope.meta.toon_stats.is_none());
    assert!(envelope.meta.toon_stats_raw.is_none());
}

// ---------------------------------------------------------------------------
// Resource output envelope (mirrors test_resource_format_toon_envelope)
// ---------------------------------------------------------------------------

#[test]
fn resource_format_toon_envelope() {
    let config = stub_config();
    let json_str = r#"[{"slug":"backend","human_key":"/backend"}]"#;
    let mut params = HashMap::new();
    params.insert("format".to_string(), "toon".to_string());

    let result = apply_resource_format(json_str, &params, &config).unwrap();
    let envelope: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(envelope["format"], "toon");
    assert!(envelope["data"].is_string());
    assert_eq!(envelope["meta"]["requested"], "toon");
    assert_eq!(envelope["meta"]["source"], "param");
}

// ---------------------------------------------------------------------------
// Resource query param format=toon is honored
// ---------------------------------------------------------------------------

#[test]
fn resource_format_query_param_honored() {
    let config = stub_config();
    let json_str = r#"{"environment":"local"}"#;
    let mut params = HashMap::new();
    params.insert("format".to_string(), "toon".to_string());

    let result = apply_resource_format(json_str, &params, &config).unwrap();
    let envelope: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(envelope["format"], "toon");
    assert!(envelope["data"].is_string());
}

#[test]
fn resource_format_no_param_passthrough() {
    let config = stub_config();
    let json_str = r#"{"environment":"local"}"#;
    let params: HashMap<String, String> = HashMap::new();

    let result = apply_resource_format(json_str, &params, &config).unwrap();
    // Should be unchanged (no format param = implicit json = passthrough)
    assert_eq!(result, json_str);
}

// ---------------------------------------------------------------------------
// Fallback on encoder error (mirrors test_toon_fallback_on_encoder_error)
// ---------------------------------------------------------------------------

#[test]
fn fallback_on_encoder_error_returns_json_with_toon_error() {
    let config = Config {
        toon_bin: Some(failing_stub_encoder_path()),
        toon_stats_enabled: false,
        ..Config::default()
    };
    let payload = serde_json::json!({"status": "ok"});
    let envelope = apply_toon_format(&payload, Some("toon"), &config)
        .unwrap()
        .unwrap();

    assert_eq!(envelope.format, "json");
    assert_eq!(envelope.data, payload);
    assert_eq!(envelope.meta.requested, Some("toon".to_string()));
    assert!(envelope.meta.toon_error.is_some());
    assert!(envelope.meta.encoder.is_none());
}

#[test]
fn fallback_preserves_original_data() {
    let config = Config {
        toon_bin: Some("/nonexistent/tru_binary".to_string()),
        ..Config::default()
    };
    let payload = serde_json::json!({
        "id": 42,
        "subject": "Important",
        "body": "Hello world"
    });
    let envelope = apply_toon_format(&payload, Some("toon"), &config)
        .unwrap()
        .unwrap();

    assert_eq!(envelope.format, "json");
    // Verify all original fields are preserved in fallback
    assert_eq!(envelope.data["id"], 42);
    assert_eq!(envelope.data["subject"], "Important");
    assert_eq!(envelope.data["body"], "Hello world");
}

// ---------------------------------------------------------------------------
// apply_tool_format string-level integration
// ---------------------------------------------------------------------------

#[test]
fn apply_tool_format_toon_returns_envelope_string() {
    let config = stub_config();
    let json = r#"{"id":1,"subject":"Test"}"#;
    let result = apply_tool_format(json, Some("toon"), &config).unwrap();

    let envelope: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(envelope["format"], "toon");
    assert!(envelope["data"].is_string());
    assert_eq!(envelope["meta"]["source"], "param");
}

#[test]
fn apply_tool_format_json_returns_original() {
    let config = stub_config();
    let json = r#"{"id":1}"#;
    let result = apply_tool_format(json, Some("json"), &config).unwrap();
    assert_eq!(result, json);
}

#[test]
fn apply_tool_format_none_returns_original() {
    let config = stub_config();
    let json = r#"{"id":1}"#;
    let result = apply_tool_format(json, None, &config).unwrap();
    assert_eq!(result, json);
}

#[test]
fn apply_tool_format_with_config_default_toon() {
    let config = Config {
        toon_bin: Some(stub_encoder_path()),
        toon_stats_enabled: false,
        output_format_default: Some("toon".to_string()),
        ..Config::default()
    };
    let json = r#"{"id":1}"#;
    // No explicit format → falls to config default "toon"
    let result = apply_tool_format(json, None, &config).unwrap();
    let envelope: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(envelope["format"], "toon");
    assert_eq!(envelope["meta"]["source"], "default");
}

// ---------------------------------------------------------------------------
// Format resolution edge cases
// ---------------------------------------------------------------------------

#[test]
fn format_resolution_case_insensitive() {
    let config = Config::default();
    let d = resolve_output_format(Some("TOON"), &config).unwrap();
    assert_eq!(d.resolved, "toon");
    assert_eq!(d.source, "param");
}

#[test]
fn format_resolution_whitespace_trimmed() {
    let config = Config::default();
    let d = resolve_output_format(Some("  toon  "), &config).unwrap();
    assert_eq!(d.resolved, "toon");
}

#[test]
fn format_resolution_mime_text_toon() {
    let config = Config::default();
    let d = resolve_output_format(Some("text/toon"), &config).unwrap();
    assert_eq!(d.resolved, "toon");
}

#[test]
fn format_resolution_invalid_default_rejected() {
    let config = Config {
        output_format_default: Some("yaml".to_string()),
        ..Config::default()
    };
    let err = resolve_output_format(None, &config).unwrap_err();
    assert!(err.contains("Invalid format"));
}

#[test]
fn format_resolution_default_auto_means_implicit_json() {
    let config = Config {
        output_format_default: Some("auto".to_string()),
        ..Config::default()
    };
    let d = resolve_output_format(None, &config).unwrap();
    assert_eq!(d.resolved, "json");
    assert_eq!(d.source, "implicit");
}

// ---------------------------------------------------------------------------
// E2E-ish offline test: multi-tool sequence with format=toon
// (mirrors test_toon_format_e2e.py)
// ---------------------------------------------------------------------------

#[test]
fn e2e_toon_format_multi_tool_sequence() {
    let config = stub_config_with_stats();

    // Simulate health_check response
    let health = serde_json::json!({"status": "ok", "version": "1.0.0"});
    let health_env = apply_toon_format(&health, Some("toon"), &config)
        .unwrap()
        .unwrap();
    assert_eq!(health_env.format, "toon");
    assert!(health_env.data.is_string());

    // Simulate ensure_project response
    let project = serde_json::json!({
        "id": 1, "slug": "backend", "human_key": "/backend",
        "created_at": "2026-01-01T00:00:00Z"
    });
    let project_env = apply_toon_format(&project, Some("toon"), &config)
        .unwrap()
        .unwrap();
    assert_eq!(project_env.format, "toon");
    assert!(project_env.meta.toon_stats.is_some());

    // Simulate register_agent response
    let agent = serde_json::json!({
        "id": 1, "name": "BlueLake", "program": "codex", "model": "gpt-5",
        "task_description": "", "inception_ts": "2026-01-01T00:00:00Z",
        "last_active_ts": "2026-01-01T00:00:00Z", "project_id": 1
    });
    let agent_env = apply_toon_format(&agent, Some("toon"), &config)
        .unwrap()
        .unwrap();
    assert_eq!(agent_env.format, "toon");

    // Simulate inbox resource with format=toon query param
    let inbox_json = r#"[{"id":1,"subject":"Welcome","from":"System","importance":"normal"}]"#;
    let mut params = HashMap::new();
    params.insert("format".to_string(), "toon".to_string());
    let inbox_result = apply_resource_format(inbox_json, &params, &config).unwrap();
    let inbox_env: serde_json::Value = serde_json::from_str(&inbox_result).unwrap();
    assert_eq!(inbox_env["format"], "toon");

    // Write structured log artifact (JSON)
    let log = serde_json::json!({
        "test": "e2e_toon_format_multi_tool_sequence",
        "steps": [
            {"tool": "health_check", "format": health_env.format, "has_stats": health_env.meta.toon_stats.is_some()},
            {"tool": "ensure_project", "format": project_env.format, "has_stats": project_env.meta.toon_stats.is_some()},
            {"tool": "register_agent", "format": agent_env.format, "has_stats": agent_env.meta.toon_stats.is_some()},
            {"resource": "inbox", "format": inbox_env["format"]}
        ]
    });

    // Write log to temp file
    let log_dir = std::env::temp_dir().join("mcp_agent_mail_toon_tests");
    std::fs::create_dir_all(&log_dir).unwrap();
    let log_path = log_dir.join("e2e_toon_log.json");
    std::fs::write(&log_path, serde_json::to_string_pretty(&log).unwrap()).unwrap();

    // Verify log artifact was written
    let contents = std::fs::read_to_string(&log_path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&contents).unwrap();
    assert_eq!(parsed["steps"].as_array().unwrap().len(), 4);
    for step in parsed["steps"].as_array().unwrap() {
        assert_eq!(step["format"], "toon");
    }

    // Cleanup
    let _ = std::fs::remove_file(&log_path);
}

// ---------------------------------------------------------------------------
// Envelope serialization contract
// ---------------------------------------------------------------------------

#[test]
fn successful_envelope_has_no_error_fields() {
    let config = stub_config();
    let payload = serde_json::json!({"id": 1});
    let envelope = apply_toon_format(&payload, Some("toon"), &config)
        .unwrap()
        .unwrap();

    let json_str = serde_json::to_string(&envelope).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

    // On success: toon_error and toon_stderr should be absent (skip_serializing_if)
    assert!(parsed["meta"].get("toon_error").is_none());
    assert!(parsed["meta"].get("toon_stderr").is_none());
    // encoder should be present
    assert!(parsed["meta"]["encoder"].is_string());
}

#[test]
fn fallback_envelope_has_error_no_encoder() {
    let config = Config {
        toon_bin: Some(failing_stub_encoder_path()),
        ..Config::default()
    };
    let payload = serde_json::json!({"id": 1});
    let envelope = apply_toon_format(&payload, Some("toon"), &config)
        .unwrap()
        .unwrap();

    let json_str = serde_json::to_string(&envelope).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

    // On fallback: toon_error present, encoder absent
    assert!(parsed["meta"]["toon_error"].is_string());
    assert!(parsed["meta"].get("encoder").is_none());
    // toon_stderr may or may not be present depending on error type
}

#[test]
fn envelope_data_is_string_on_success() {
    let config = stub_config();
    let payload = serde_json::json!({"nested": {"deep": [1,2,3]}});
    let envelope = apply_toon_format(&payload, Some("toon"), &config)
        .unwrap()
        .unwrap();
    assert!(envelope.data.is_string(), "encoded data should be a string, not object");
}

#[test]
fn envelope_data_is_object_on_fallback() {
    let config = Config {
        toon_bin: Some("/nonexistent/tru_binary".to_string()),
        ..Config::default()
    };
    let payload = serde_json::json!({"nested": {"deep": [1,2,3]}});
    let envelope = apply_toon_format(&payload, Some("toon"), &config)
        .unwrap()
        .unwrap();
    assert!(envelope.data.is_object(), "fallback data should be the original object");
    assert_eq!(envelope.data["nested"]["deep"][1], 2);
}
