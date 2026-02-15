// Note: unsafe required for env::set_var in Rust 2024
#![allow(unsafe_code)]

//! Conformance tests verifying Rust resource descriptions match the Python reference.
//! Each Python resource docstring becomes the MCP resource description.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// A unified (uri, description) from both resources and resource templates.
struct ResourceEntry {
    uri: String,
    description: Option<String>,
}

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

struct EnvVarGuard {
    previous: Vec<(String, Option<String>)>,
}

impl EnvVarGuard {
    fn set(vars: &[(&str, &str)]) -> Self {
        let mut previous = Vec::new();
        for (key, value) in vars {
            let old = std::env::var(*key).ok();
            previous.push(((*key).to_string(), old));
            unsafe {
                std::env::set_var(key, value);
            }
        }
        mcp_agent_mail_core::Config::reset_cached();
        Self { previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        for (key, value) in self.previous.drain(..) {
            match value {
                Some(v) => unsafe {
                    std::env::set_var(&key, v);
                },
                None => unsafe {
                    std::env::remove_var(&key);
                },
            }
        }
        mcp_agent_mail_core::Config::reset_cached();
    }
}

fn collect_all_resources() -> Vec<ResourceEntry> {
    let _lock = env_lock().lock().unwrap_or_else(|p| p.into_inner());
    let _guard = EnvVarGuard::set(&[
        ("WORKTREES_ENABLED", "true"),
        ("TOOL_FILTER_PROFILE", "full"),
    ]);
    let config = mcp_agent_mail_core::Config::from_env();
    let router = mcp_agent_mail_server::build_server(&config).into_router();

    let mut entries = Vec::new();

    // Static resources (no path params)
    for r in router.resources() {
        entries.push(ResourceEntry {
            uri: r.uri.clone(),
            description: r.description.clone(),
        });
    }

    // Resource templates (with path params like {agent}, {slug})
    for t in router.resource_templates() {
        entries.push(ResourceEntry {
            uri: t.uri_template.clone(),
            description: t.description.clone(),
        });
    }

    entries
}

/// Expected description prefixes for key Python-matching resources.
fn expected_description_prefixes() -> HashMap<&'static str, &'static str> {
    let mut m = HashMap::new();
    m.insert(
        "resource://config/environment",
        "Inspect the server's current environment and HTTP settings.",
    );
    m.insert(
        "resource://identity/{project}",
        "Inspect identity resolution for a given project path.",
    );
    m.insert(
        "resource://tooling/directory",
        "Provide a clustered view of exposed MCP tools to combat option overload.",
    );
    m.insert(
        "resource://tooling/schemas",
        "Expose JSON-like parameter schemas for tools/macros to prevent drift.",
    );
    m.insert(
        "resource://tooling/metrics",
        "Expose aggregated tool call/error counts for analysis.",
    );
    m.insert(
        "resource://tooling/locks",
        "Return lock metadata from the shared archive storage.",
    );
    m.insert(
        "resource://projects",
        "List all projects known to the server in creation order.",
    );
    m.insert(
        "resource://project/{slug}",
        "Fetch a project and its agents by project slug or human key.",
    );
    m.insert(
        "resource://agents/{project_key}",
        "List all registered agents in a project for easy agent discovery.",
    );
    m.insert(
        "resource://file_reservations/{slug}",
        "List file_reservations for a project, optionally filtering to active-only.",
    );
    m.insert(
        "resource://message/{message_id}",
        "Read a single message by id within a project.",
    );
    m.insert(
        "resource://thread/{thread_id}",
        "List messages for a thread within a project.",
    );
    m.insert(
        "resource://inbox/{agent}",
        "Read an agent's inbox for a project.",
    );
    m.insert(
        "resource://views/urgent-unread/{agent}",
        "Convenience view listing urgent and high-importance messages that are unread",
    );
    m.insert(
        "resource://views/ack-required/{agent}",
        "Convenience view listing messages requiring acknowledgement",
    );
    m.insert(
        "resource://views/acks-stale/{agent}",
        "List ack-required messages older than a TTL",
    );
    m.insert(
        "resource://views/ack-overdue/{agent}",
        "List messages requiring acknowledgement older than ttl_minutes",
    );
    m.insert(
        "resource://mailbox/{agent}",
        "List recent messages in an agent's mailbox with lightweight Git commit context.",
    );
    m.insert(
        "resource://mailbox-with-commits/{agent}",
        "List recent messages in an agent's mailbox with commit metadata including diff summaries.",
    );
    m.insert(
        "resource://outbox/{agent}",
        "List messages sent by the agent, enriched with commit metadata",
    );
    m.insert(
        "resource://product/{key}",
        "Inspect product and list linked projects.",
    );
    m
}

#[test]
fn resource_descriptions_match_python_prefixes() {
    let all = collect_all_resources();
    let expected = expected_description_prefixes();

    eprintln!(
        "Checking {} resource description prefixes against {} entries (resources + templates)",
        expected.len(),
        all.len()
    );

    let mut matched = 0;
    let mut mismatches: Vec<String> = Vec::new();

    for (uri_pattern, expected_prefix) in &expected {
        // Find entry by exact URI or URI template match (skip query variants)
        let entry = all.iter().find(|e| {
            let uri = &e.uri;
            uri == uri_pattern && !uri.contains('?')
        });

        match entry {
            Some(e) => {
                let desc = e.description.as_deref().unwrap_or("");
                if desc.starts_with(expected_prefix) {
                    matched += 1;
                } else {
                    let mismatch_idx = desc
                        .chars()
                        .zip(expected_prefix.chars())
                        .position(|(a, b)| a != b)
                        .unwrap_or(desc.len().min(expected_prefix.len()));
                    mismatches.push(format!(
                        "{uri_pattern}: description mismatch at char {mismatch_idx}\n  expected start: {expected_prefix}\n  actual start:   {}",
                        &desc[..desc.len().min(120)]
                    ));
                }
            }
            None => {
                mismatches.push(format!("{uri_pattern}: resource not found"));
            }
        }
    }

    if !mismatches.is_empty() {
        panic!(
            "Resource description parity failures ({}/{}):\n{}",
            mismatches.len(),
            expected.len(),
            mismatches.join("\n\n")
        );
    }

    eprintln!(
        "All {matched}/{} resource description prefixes match Python",
        expected.len()
    );
}

#[test]
fn agents_resource_description_contains_notes_section() {
    let all = collect_all_resources();
    let agents_entry = all
        .iter()
        .find(|e| e.uri == "resource://agents/{project_key}")
        .expect("agents resource template should exist");

    let desc = agents_entry.description.as_deref().unwrap_or("");

    assert!(
        desc.contains("When to use"),
        "agents description should include 'When to use' section"
    );
    assert!(
        desc.contains("Notes"),
        "agents description should include 'Notes' section"
    );
    assert!(
        desc.contains("Agent names are NOT the same as your program name"),
        "agents description should include agent name warning"
    );
    assert!(
        desc.contains("project isolation is enforced"),
        "agents description should mention project isolation"
    );
}

#[test]
fn file_reservations_description_contains_why_section() {
    let all = collect_all_resources();
    let entry = all
        .iter()
        .find(|e| e.uri == "resource://file_reservations/{slug}")
        .expect("file_reservations resource template should exist");

    let desc = entry.description.as_deref().unwrap_or("");

    assert!(
        desc.contains("Why this exists"),
        "file_reservations description should include 'Why this exists' section"
    );
    assert!(
        desc.contains("edit intent"),
        "file_reservations description should mention edit intent"
    );
}
