// Note: unsafe required for env::set_var in Rust 2024
#![allow(unsafe_code)]

//! Supported resource-inventory and Rust-owned description checks.
//!
//! Historical Python resource prose is not a product contract. Rust may improve
//! wording freely, while these tests keep the supported resource inventory and
//! important operator guidance from disappearing accidentally.

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

const SUPPORTED_RESOURCE_URIS: &[&str] = &[
    "resource://config/environment",
    "resource://identity/{project}",
    "resource://tooling/directory",
    "resource://tooling/schemas",
    "resource://tooling/metrics",
    "resource://tooling/locks",
    "resource://projects",
    "resource://project/{slug}",
    "resource://agents/{project_key}",
    "resource://file_reservations/{slug}",
    "resource://message/{message_id}",
    "resource://thread/{thread_id}",
    "resource://inbox/{agent}",
    "resource://views/urgent-unread/{agent}",
    "resource://views/ack-required/{agent}",
    "resource://views/acks-stale/{agent}",
    "resource://views/ack-overdue/{agent}",
    "resource://mailbox/{agent}",
    "resource://mailbox-with-commits/{agent}",
    "resource://outbox/{agent}",
    "resource://product/{key}",
];

#[test]
fn supported_resources_have_rust_owned_descriptions() {
    let all = collect_all_resources();

    eprintln!(
        "Checking {} supported resources against {} entries (resources + templates)",
        SUPPORTED_RESOURCE_URIS.len(),
        all.len()
    );

    let mut matched = 0;
    let mut mismatches: Vec<String> = Vec::new();

    for uri_pattern in SUPPORTED_RESOURCE_URIS {
        // Find entry by exact URI or URI template match (skip query variants)
        let entry = all.iter().find(|e| {
            let uri = &e.uri;
            uri == *uri_pattern && !uri.contains('?')
        });

        match entry {
            Some(e) => {
                let desc = e.description.as_deref().unwrap_or("");
                if desc.trim().is_empty() {
                    mismatches.push(format!("{uri_pattern}: Rust description is empty"));
                } else {
                    matched += 1;
                }
            }
            None => {
                mismatches.push(format!("{uri_pattern}: resource not found"));
            }
        }
    }

    if !mismatches.is_empty() {
        panic!(
            "Supported resource description failures ({}/{}):\n{}",
            mismatches.len(),
            SUPPORTED_RESOURCE_URIS.len(),
            mismatches.join("\n\n")
        );
    }

    eprintln!(
        "All {matched}/{} supported resources have Rust-owned descriptions",
        SUPPORTED_RESOURCE_URIS.len()
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
