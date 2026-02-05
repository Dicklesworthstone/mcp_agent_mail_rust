#![forbid(unsafe_code)]
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::significant_drop_tightening
)]

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use fastmcp::{Budget, CallToolParams, Cx};
use fastmcp_core::SessionState;
use mcp_agent_mail_conformance::Fixtures;
use serde_json::Value;
use std::hint::black_box;
use std::sync::Once;
use tempfile::TempDir;

fn fixtures_path() -> std::path::PathBuf {
    // `CARGO_MANIFEST_DIR` is `crates/mcp-agent-mail` for this bench crate.
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json")
}

fn seed_fixtures(fixtures: &Fixtures) {
    static SEEDED: Once = Once::new();

    SEEDED.call_once(|| {
        // Reuse the conformance fixtures to seed a realistic DB state.
        // This ensures benchmarks remain aligned with parity expectations.
        let config = mcp_agent_mail_core::Config::from_env();
        let router = mcp_agent_mail_server::build_server(&config).into_router();
        let cx = Cx::for_testing();
        let budget = Budget::INFINITE;
        let mut req_id: u64 = 1;

        for (tool_name, tool_fixture) in &fixtures.tools {
            for case in &tool_fixture.cases {
                let args = match &case.input {
                    Value::Null => None,
                    Value::Object(map) if map.is_empty() => None,
                    other => Some(other.clone()),
                };
                let params = CallToolParams {
                    name: tool_name.clone(),
                    arguments: args,
                    meta: None,
                };

                let _ = router
                    .handle_tools_call(
                        &cx,
                        req_id,
                        params,
                        &budget,
                        SessionState::new(),
                        None,
                        None,
                    )
                    .expect("tool call should succeed during seeding");
                req_id += 1;
            }
        }
    });
}

fn bench_tools(c: &mut Criterion) {
    // Ensure DB is initialized before anything touches the pool cache.
    let tmp = TempDir::new().expect("tempdir");
    let original_cwd = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(tmp.path()).expect("chdir to tempdir");

    // Load fixtures via absolute path (bench runs in tempdir so relative paths won't work).
    let fixtures = Fixtures::load(fixtures_path()).expect("fixtures");
    seed_fixtures(&fixtures);

    let config = mcp_agent_mail_core::Config::from_env();
    let router = mcp_agent_mail_server::build_server(&config).into_router();
    let cx = Cx::for_testing();
    let budget = Budget::INFINITE;

    let mut group = c.benchmark_group("mcp_agent_mail_tools");

    // Bench high-frequency operations across tool clusters.
    // Format: (tool_name, case_name)
    let targets: &[(&str, &str)] = &[
        // Health
        ("health_check", "default"),
        // Identity cluster
        ("ensure_project", "abs_path_backend"),
        ("register_agent", "green_castle"),
        // Messaging cluster
        ("fetch_inbox", "gc_inbox_with_bodies"),
        ("search_messages", "search_hello"),
        ("summarize_thread", "summarize_thread_root"),
        // File reservations cluster
        ("file_reservation_paths", "reserve_src_glob"),
        // Macros cluster
        ("macro_start_session", "macro_start_session_basic"),
    ];

    for (tool_name, case_name) in targets {
        let fixture = fixtures
            .tools
            .get(*tool_name)
            .unwrap_or_else(|| panic!("missing tool fixture: {tool_name}"));
        let case = fixture
            .cases
            .iter()
            .find(|c| c.name == *case_name)
            .unwrap_or_else(|| panic!("missing case {case_name} for tool {tool_name}"));

        let args = match &case.input {
            Value::Null => None,
            Value::Object(map) if map.is_empty() => None,
            other => Some(other.clone()),
        };

        let params = CallToolParams {
            name: tool_name.to_string(),
            arguments: args,
            meta: None,
        };

        group.throughput(Throughput::Elements(1));
        group.bench_with_input(
            BenchmarkId::new(*tool_name, *case_name),
            &params,
            |b, params| {
                let mut req_id: u64 = 1;
                b.iter(|| {
                    let out = router
                        .handle_tools_call(
                            &cx,
                            req_id,
                            params.clone(),
                            &budget,
                            SessionState::new(),
                            None,
                            None,
                        )
                        .expect("tool call");
                    req_id = req_id.wrapping_add(1);
                    black_box(out);
                });
            },
        );
    }

    group.finish();

    std::env::set_current_dir(original_cwd).expect("restore cwd");
    drop(tmp);
}

criterion_group!(benches, bench_tools);
criterion_main!(benches);
