use regex::Regex;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const README_RELATIVE: &str = "README.md";
const AGENTS_RELATIVE: &str = "AGENTS.md";
const LIVE_DOCS: &[&str] = &[
    "README.md",
    "AGENTS.md",
    "VISION.md",
    "docs/OPERATOR_RUNBOOK.md",
    "docs/OPERATOR_COOKBOOK.md",
    "docs/RELEASE_CHECKLIST.md",
    "docs/SPEC-interface-mode-switch.md",
    "docs/SPEC-meta-command-allowlist.md",
];
const STALE_SEARCH_PHRASES: &[&str] = &["Tantivy Lexical", "ad-hoc SQL fallback"];

#[derive(Debug, Clone, Copy)]
struct LiveCounts {
    tools: usize,
    resources: usize,
    screens: usize,
}

#[derive(Debug)]
struct ClaimPattern {
    label: &'static str,
    regex: Regex,
    expected: usize,
    source_of_truth: &'static str,
}

#[derive(Debug)]
struct CountMatch {
    line_no: usize,
    found: usize,
    line_text: String,
}

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn workspace_root() -> PathBuf {
    crate_root()
        .parent()
        .and_then(Path::parent)
        .expect("crate should have a workspace root")
        .to_path_buf()
}

fn read_file(path: impl AsRef<Path>) -> String {
    let path = path.as_ref();
    fs::read_to_string(path).unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
}

fn normalize_runtime_resource_uri(uri: &str) -> String {
    uri.strip_suffix("?{query}").unwrap_or(uri).to_string()
}

fn collect_runtime_resources() -> BTreeSet<String> {
    let mut config = mcp_agent_mail_core::Config::from_env();
    config.tool_filter.enabled = false;
    config.worktrees_enabled = true;

    let router = mcp_agent_mail_server::build_server(&config).into_router();
    let mut resources = BTreeSet::new();
    for resource in router.resources() {
        resources.insert(normalize_runtime_resource_uri(&resource.uri));
    }
    for template in router.resource_templates() {
        resources.insert(normalize_runtime_resource_uri(&template.uri_template));
    }
    resources
}

fn live_counts() -> LiveCounts {
    LiveCounts {
        tools: mcp_agent_mail_tools::TOOL_CLUSTER_MAP.len(),
        resources: collect_runtime_resources().len(),
        screens: mcp_agent_mail_server::tui_screens::ALL_SCREEN_IDS.len(),
    }
}

fn compile(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|e| panic!("invalid regex {pattern:?}: {e}"))
}

fn find_count_match(doc: &str, regex: &Regex) -> Option<CountMatch> {
    for (idx, line) in doc.lines().enumerate() {
        let captures = match regex.captures(line) {
            Some(captures) => captures,
            None => continue,
        };
        let found = captures["count"].parse::<usize>().unwrap_or_else(|e| {
            panic!(
                "failed to parse count {:?} on line {} with {:?}: {e}",
                &captures["count"],
                idx + 1,
                regex.as_str()
            )
        });
        return Some(CountMatch {
            line_no: idx + 1,
            found,
            line_text: line.trim().to_string(),
        });
    }
    None
}

fn validate_claims(doc_label: &str, doc: &str, patterns: &[ClaimPattern]) -> Result<(), String> {
    let mut errors = Vec::new();

    for pattern in patterns {
        match find_count_match(doc, &pattern.regex) {
            Some(count_match) if count_match.found == pattern.expected => {}
            Some(count_match) => errors.push(format!(
                "{doc_label}:{}: {} drifted: found {}, expected {} from {}; line: {}",
                count_match.line_no,
                pattern.label,
                count_match.found,
                pattern.expected,
                pattern.source_of_truth,
                count_match.line_text
            )),
            None => errors.push(format!(
                "{doc_label}: missing {} matcher /{}/. Update the doc wording or this guard, but keep it aligned with {}.",
                pattern.label,
                pattern.regex.as_str(),
                pattern.source_of_truth
            )),
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

fn validate_stale_phrases() -> Result<(), String> {
    let root = workspace_root();
    let mut errors = Vec::new();

    for relative in LIVE_DOCS {
        let path = root.join(relative);
        let doc = read_file(&path);
        for needle in STALE_SEARCH_PHRASES {
            for (idx, line) in doc.lines().enumerate() {
                if line.contains(needle) {
                    errors.push(format!(
                        "{}:{}: stale phrase {:?} reintroduced; replace it with current Search V3/frankensearch wording. line: {}",
                        relative,
                        idx + 1,
                        needle,
                        line.trim()
                    ));
                }
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

fn validate_readme(doc: &str, counts: LiveCounts) -> Result<(), String> {
    validate_claims(
        README_RELATIVE,
        doc,
        &[
            ClaimPattern {
                label: "README hero tool count",
                regex: compile(r"with (?P<count>\d+) tools and \d+ resources"),
                expected: counts.tools,
                source_of_truth: "mcp_agent_mail_tools::TOOL_CLUSTER_MAP",
            },
            ClaimPattern {
                label: "README feature table tool count",
                regex: compile(r"\|\s+\*\*(?P<count>\d+) MCP Tools\*\*\s+\|"),
                expected: counts.tools,
                source_of_truth: "mcp_agent_mail_tools::TOOL_CLUSTER_MAP",
            },
            ClaimPattern {
                label: "README tool section heading",
                regex: compile(r"^## The (?P<count>\d+) MCP Tools$"),
                expected: counts.tools,
                source_of_truth: "mcp_agent_mail_tools::TOOL_CLUSTER_MAP",
            },
            ClaimPattern {
                label: "README hero resource count",
                regex: compile(r"with \d+ tools and (?P<count>\d+) resources"),
                expected: counts.resources,
                source_of_truth: "mcp_agent_mail_server::build_server(...).into_router() resource/template inventory",
            },
            ClaimPattern {
                label: "README feature table resource count",
                regex: compile(r"\|\s+\*\*(?P<count>\d+) MCP Resources\*\*\s+\|"),
                expected: counts.resources,
                source_of_truth: "mcp_agent_mail_server::build_server(...).into_router() resource/template inventory",
            },
            ClaimPattern {
                label: "README FAQ resource count",
                regex: compile(r"all (?P<count>\d+) MCP resources"),
                expected: counts.resources,
                source_of_truth: "mcp_agent_mail_server::build_server(...).into_router() resource/template inventory",
            },
            ClaimPattern {
                label: "README hero screen count",
                regex: compile(r"interactive (?P<count>\d+)-screen TUI"),
                expected: counts.screens,
                source_of_truth: "mcp_agent_mail_server::tui_screens::ALL_SCREEN_IDS",
            },
            ClaimPattern {
                label: "README feature table screen count",
                regex: compile(r"\|\s+\*\*(?P<count>\d+)-Screen TUI\*\*\s+\|"),
                expected: counts.screens,
                source_of_truth: "mcp_agent_mail_server::tui_screens::ALL_SCREEN_IDS",
            },
            ClaimPattern {
                label: "README TUI overview screen count",
                regex: compile(r"The interactive TUI has (?P<count>\d+) screens"),
                expected: counts.screens,
                source_of_truth: "mcp_agent_mail_server::tui_screens::ALL_SCREEN_IDS",
            },
            ClaimPattern {
                label: "README workspace tree screen count",
                regex: compile(r"TUI \((?P<count>\d+) screens\)"),
                expected: counts.screens,
                source_of_truth: "mcp_agent_mail_server::tui_screens::ALL_SCREEN_IDS",
            },
        ],
    )
}

fn validate_agents_md(doc: &str, counts: LiveCounts) -> Result<(), String> {
    validate_claims(
        AGENTS_RELATIVE,
        doc,
        &[
            ClaimPattern {
                label: "AGENTS hero tool count",
                regex: compile(r"with (?P<count>\d+) tools and \d+ resources"),
                expected: counts.tools,
                source_of_truth: "mcp_agent_mail_tools::TOOL_CLUSTER_MAP",
            },
            ClaimPattern {
                label: "AGENTS tools crate row",
                regex: compile(r"\| `mcp-agent-mail-tools` \| `src/` \| (?P<count>\d+) MCP tool implementations"),
                expected: counts.tools,
                source_of_truth: "mcp_agent_mail_tools::TOOL_CLUSTER_MAP",
            },
            ClaimPattern {
                label: "AGENTS tool heading",
                regex: compile(r"^### (?P<count>\d+) MCP Tools \(9 Clusters\)$"),
                expected: counts.tools,
                source_of_truth: "mcp_agent_mail_tools::TOOL_CLUSTER_MAP",
            },
            ClaimPattern {
                label: "AGENTS hero resource count",
                regex: compile(r"with \d+ tools and (?P<count>\d+) resources"),
                expected: counts.resources,
                source_of_truth: "mcp_agent_mail_server::build_server(...).into_router() resource/template inventory",
            },
            ClaimPattern {
                label: "AGENTS conformance category resource count",
                regex: compile(
                    r"34 Python-parity tools \+ 3 Rust-native, (?P<count>\d+) resources",
                ),
                expected: counts.resources,
                source_of_truth: "mcp_agent_mail_server::build_server(...).into_router() resource/template inventory",
            },
            ClaimPattern {
                label: "AGENTS conformance fixture paragraph resource count",
                regex: compile(r"across 34 Python-parity tools and (?P<count>\d+) resources"),
                expected: counts.resources,
                source_of_truth: "mcp_agent_mail_server::build_server(...).into_router() resource/template inventory",
            },
            ClaimPattern {
                label: "AGENTS key files screen count",
                regex: compile(r"TUI operations console \((?P<count>\d+) screens\)"),
                expected: counts.screens,
                source_of_truth: "mcp_agent_mail_server::tui_screens::ALL_SCREEN_IDS",
            },
            ClaimPattern {
                label: "AGENTS TUI heading",
                regex: compile(r"^### (?P<count>\d+)-Screen TUI$"),
                expected: counts.screens,
                source_of_truth: "mcp_agent_mail_server::tui_screens::ALL_SCREEN_IDS",
            },
        ],
    )
}

#[test]
fn readme_counts_match_live_inventory() {
    let counts = live_counts();
    let readme = read_file(workspace_root().join(README_RELATIVE));
    if let Err(err) = validate_readme(&readme, counts) {
        panic!("{err}");
    }
}

#[test]
fn agents_md_counts_match_live_inventory() {
    let counts = live_counts();
    let agents = read_file(workspace_root().join(AGENTS_RELATIVE));
    if let Err(err) = validate_agents_md(&agents, counts) {
        panic!("{err}");
    }
}

#[test]
fn live_docs_reject_stale_search_naming() {
    if let Err(err) = validate_stale_phrases() {
        panic!("{err}");
    }
}

#[test]
#[ignore = "Demonstrates that an intentional README count mutation is caught by the guard"]
fn intentionally_mutated_readme_is_rejected() {
    let counts = live_counts();
    let readme = read_file(workspace_root().join(README_RELATIVE));
    let hero_phrase = compile(r"with \d+ tools and \d+ resources");
    let replacement = format!(
        "with {} tools and {} resources",
        counts.tools.saturating_sub(1),
        counts.resources
    );
    let mutated = hero_phrase.replace(&readme, replacement).to_string();
    assert_ne!(
        mutated, readme,
        "expected to mutate the README hero summary for the negative test"
    );

    let err = validate_readme(&mutated, counts).expect_err("mutated README should fail");
    assert!(
        err.contains("README.md"),
        "negative test should report the affected file: {err}"
    );
    assert!(
        err.contains("README hero tool count"),
        "negative test should report the failing claim: {err}"
    );
}
