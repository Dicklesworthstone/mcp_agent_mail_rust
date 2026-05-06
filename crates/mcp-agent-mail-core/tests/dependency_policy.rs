use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;

const FORBIDDEN_ASYNC_STACK: &[&str] = &[
    "async-std",
    "axum",
    "fastembed",
    "h2",
    "hf-hub",
    "hyper",
    "hyper-rustls",
    "hyper-tls",
    "hyper-util",
    "reqwest",
    "smol",
    "tokio",
    "tokio-rustls",
    "tokio-util",
    "tower",
    "tower-http",
    "tower-service",
];

fn workspace_manifest() -> PathBuf {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    crate_dir
        .parent()
        .and_then(|crates_dir| crates_dir.parent())
        .expect("core crate should live under <workspace>/crates/mcp-agent-mail-core")
        .join("Cargo.toml")
}

fn resolved_package_names(extra_args: &[&str]) -> BTreeSet<String> {
    let output = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args([
            "metadata",
            "--locked",
            "--format-version=1",
            "--manifest-path",
        ])
        .arg(workspace_manifest())
        .args(extra_args)
        .output()
        .expect("run cargo metadata");

    assert!(
        output.status.success(),
        "cargo metadata failed\nstatus: {}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("cargo metadata should emit json");
    let package_names_by_id: BTreeMap<&str, &str> = metadata["packages"]
        .as_array()
        .expect("metadata packages should be an array")
        .iter()
        .map(|package| {
            (
                package["id"].as_str().expect("package id should be string"),
                package["name"]
                    .as_str()
                    .expect("package name should be string"),
            )
        })
        .collect();

    metadata["resolve"]["nodes"]
        .as_array()
        .expect("metadata resolve nodes should be an array")
        .iter()
        .filter_map(|node| node["id"].as_str())
        .filter_map(|id| package_names_by_id.get(id).copied())
        .map(str::to_owned)
        .collect()
}

fn assert_graph_excludes_forbidden_async_stack(label: &str, extra_args: &[&str]) {
    let names = resolved_package_names(extra_args);
    let violations: Vec<&str> = FORBIDDEN_ASYNC_STACK
        .iter()
        .copied()
        .filter(|name| names.contains(*name))
        .collect();

    assert!(
        violations.is_empty(),
        "{label} dependency graph contains forbidden async/network packages: {violations:?}"
    );
}

#[test]
fn default_workspace_graph_excludes_forbidden_async_stack() {
    assert_graph_excludes_forbidden_async_stack("default", &[]);
}

#[test]
fn all_features_workspace_graph_excludes_forbidden_async_stack() {
    assert_graph_excludes_forbidden_async_stack("all-features", &["--all-features"]);
}
