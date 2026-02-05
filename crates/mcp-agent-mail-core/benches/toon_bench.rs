//! Criterion benchmarks for TOON hot paths.
//!
//! Covers: format resolution, stats parsing, encoder invocation, envelope construction.
//! Uses deterministic stub encoders for offline reproducibility.

use std::collections::HashMap;
use std::path::PathBuf;

use criterion::{Criterion, black_box, criterion_group, criterion_main};

use mcp_agent_mail_core::config::Config;
use mcp_agent_mail_core::toon::{
    apply_resource_format, apply_toon_format, apply_tool_format, parse_toon_stats,
    resolve_encoder, resolve_output_format, run_encoder,
};

fn stub_encoder_path() -> String {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path.push("scripts");
    path.push("toon_stub_encoder.sh");
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
        ..Config::default()
    }
}

// ---------------------------------------------------------------------------
// Format resolution (pure CPU, no I/O)
// ---------------------------------------------------------------------------

fn bench_format_resolution(c: &mut Criterion) {
    let config = Config::default();

    c.bench_function("resolve_format_explicit_toon", |b| {
        b.iter(|| resolve_output_format(black_box(Some("toon")), &config));
    });

    c.bench_function("resolve_format_explicit_json", |b| {
        b.iter(|| resolve_output_format(black_box(Some("json")), &config));
    });

    c.bench_function("resolve_format_none_implicit", |b| {
        b.iter(|| resolve_output_format(black_box(None), &config));
    });

    c.bench_function("resolve_format_mime_alias", |b| {
        b.iter(|| resolve_output_format(black_box(Some("application/toon")), &config));
    });

    let config_default = Config {
        output_format_default: Some("toon".to_string()),
        ..Config::default()
    };
    c.bench_function("resolve_format_config_default", |b| {
        b.iter(|| resolve_output_format(black_box(None), &config_default));
    });

    c.bench_function("resolve_format_auto_alias", |b| {
        b.iter(|| resolve_output_format(black_box(Some("auto")), &config));
    });
}

// ---------------------------------------------------------------------------
// Stats parsing (pure CPU, string scanning)
// ---------------------------------------------------------------------------

fn bench_stats_parsing(c: &mut Criterion) {
    let full_stats = "Token estimates: ~42 (JSON) -> ~18 (TOON)\nSaved ~24 tokens (-57.1%)\n";
    let tokens_only = "Token estimates: ~50 (JSON) -> ~30 (TOON)\n";
    let noisy = "info: loading config\nToken estimates: ~200 (JSON) -> ~80 (TOON)\nSaved ~120 tokens (-60.0%)\ninfo: done\n";
    let empty = "";

    c.bench_function("parse_stats_full", |b| {
        b.iter(|| parse_toon_stats(black_box(full_stats)));
    });

    c.bench_function("parse_stats_tokens_only", |b| {
        b.iter(|| parse_toon_stats(black_box(tokens_only)));
    });

    c.bench_function("parse_stats_noisy", |b| {
        b.iter(|| parse_toon_stats(black_box(noisy)));
    });

    c.bench_function("parse_stats_empty", |b| {
        b.iter(|| parse_toon_stats(black_box(empty)));
    });
}

// ---------------------------------------------------------------------------
// Encoder resolution (pure CPU, no I/O)
// ---------------------------------------------------------------------------

fn bench_encoder_resolution(c: &mut Criterion) {
    let config = Config::default();
    let config_custom = Config {
        toon_bin: Some("/usr/local/bin/tru --experimental --verbose".to_string()),
        ..Config::default()
    };

    c.bench_function("resolve_encoder_default", |b| {
        b.iter(|| resolve_encoder(black_box(&config)));
    });

    c.bench_function("resolve_encoder_custom", |b| {
        b.iter(|| resolve_encoder(black_box(&config_custom)));
    });
}

// ---------------------------------------------------------------------------
// Run encoder (subprocess I/O — measures end-to-end latency)
// ---------------------------------------------------------------------------

fn bench_run_encoder(c: &mut Criterion) {
    let config = stub_config();
    let config_stats = stub_config_with_stats();
    let small_payload = r#"{"id":1}"#;
    let medium_payload = serde_json::to_string(&serde_json::json!({
        "id": 1,
        "name": "BlueLake",
        "program": "codex",
        "model": "gpt-5",
        "task_description": "Port the notification system",
        "messages": [
            {"id": 1, "subject": "Welcome", "body": "Hello BlueLake, you have been registered."},
            {"id": 2, "subject": "Task assigned", "body": "Please work on the notification system."},
            {"id": 3, "subject": "Update", "body": "Deadline extended by 24 hours."}
        ]
    }))
    .unwrap();

    let mut group = c.benchmark_group("run_encoder");
    group.sample_size(20); // subprocess benchmarks are slower

    group.bench_function("small_payload", |b| {
        b.iter(|| run_encoder(&config, black_box(small_payload)));
    });

    group.bench_function("medium_payload", |b| {
        b.iter(|| run_encoder(&config, black_box(&medium_payload)));
    });

    group.bench_function("small_with_stats", |b| {
        b.iter(|| run_encoder(&config_stats, black_box(small_payload)));
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// apply_toon_format (envelope construction + optional encoder)
// ---------------------------------------------------------------------------

fn bench_apply_format(c: &mut Criterion) {
    let config_json = Config::default();
    let config_toon = stub_config();
    let config_toon_stats = stub_config_with_stats();
    let payload = serde_json::json!({
        "id": 1, "slug": "backend", "human_key": "/backend",
        "created_at": "2026-01-01T00:00:00Z"
    });

    c.bench_function("apply_format_json_passthrough", |b| {
        b.iter(|| apply_toon_format(&payload, black_box(Some("json")), &config_json));
    });

    c.bench_function("apply_format_none_implicit", |b| {
        b.iter(|| apply_toon_format(&payload, black_box(None), &config_json));
    });

    let mut group = c.benchmark_group("apply_format_toon");
    group.sample_size(20);

    group.bench_function("toon_no_stats", |b| {
        b.iter(|| apply_toon_format(&payload, black_box(Some("toon")), &config_toon));
    });

    group.bench_function("toon_with_stats", |b| {
        b.iter(|| apply_toon_format(&payload, black_box(Some("toon")), &config_toon_stats));
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// apply_tool_format and apply_resource_format (string-level wrappers)
// ---------------------------------------------------------------------------

fn bench_tool_and_resource_format(c: &mut Criterion) {
    let config = stub_config();
    let json = r#"{"id":1,"subject":"Test message","body":"Hello world"}"#;

    c.bench_function("tool_format_json_passthrough", |b| {
        b.iter(|| apply_tool_format(black_box(json), Some("json"), &config));
    });

    let mut group = c.benchmark_group("tool_resource_format_toon");
    group.sample_size(20);

    group.bench_function("tool_format_toon", |b| {
        b.iter(|| apply_tool_format(black_box(json), Some("toon"), &config));
    });

    let params: HashMap<String, String> =
        [("format".to_string(), "toon".to_string())].into_iter().collect();
    group.bench_function("resource_format_toon", |b| {
        b.iter(|| apply_resource_format(black_box(json), &params, &config));
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// JSON serialization overhead (baseline for envelope cost)
// ---------------------------------------------------------------------------

fn bench_json_overhead(c: &mut Criterion) {
    let payload = serde_json::json!({
        "id": 1, "name": "BlueLake", "program": "codex", "model": "gpt-5",
        "task_description": "Port notification system",
        "inception_ts": "2026-01-01T00:00:00Z",
        "last_active_ts": "2026-01-01T12:00:00Z",
        "project_id": 42
    });

    c.bench_function("json_serialize_payload", |b| {
        b.iter(|| serde_json::to_string(black_box(&payload)));
    });

    let json_str = serde_json::to_string(&payload).unwrap();
    c.bench_function("json_parse_payload", |b| {
        b.iter(|| serde_json::from_str::<serde_json::Value>(black_box(&json_str)));
    });
}

criterion_group!(
    benches,
    bench_format_resolution,
    bench_stats_parsing,
    bench_encoder_resolution,
    bench_run_encoder,
    bench_apply_format,
    bench_tool_and_resource_format,
    bench_json_overhead,
);
criterion_main!(benches);
