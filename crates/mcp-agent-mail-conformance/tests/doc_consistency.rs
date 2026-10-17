use regex::Regex;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const README_RELATIVE: &str = "README.md";
const AGENTS_RELATIVE: &str = "AGENTS.md";
const LIVE_DOCS: &[&str] = &[
    "README.md",
    "AGENTS.md",
    "docs/VISION.md",
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
    doctor_verbs: usize,
    robot_subcommands: usize,
    themes: usize,
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

/// Count the user-facing subcommands of one `am <family>` from the live clap
/// tree (the implicit `help` subcommand is not a verb).
fn live_cli_subcommand_count(family: &str) -> usize {
    use clap::CommandFactory;
    let root = mcp_agent_mail_cli::Cli::command();
    let family_cmd = root
        .get_subcommands()
        .find(|cmd| cmd.get_name() == family)
        .unwrap_or_else(|| panic!("`am {family}` is missing from the clap tree"));
    family_cmd
        .get_subcommands()
        .filter(|cmd| cmd.get_name() != "help")
        .count()
}

fn live_counts() -> LiveCounts {
    LiveCounts {
        tools: mcp_agent_mail_tools::TOOL_CLUSTER_MAP.len(),
        resources: collect_runtime_resources().len(),
        screens: mcp_agent_mail_server::tui_screens::ALL_SCREEN_IDS.len(),
        doctor_verbs: live_cli_subcommand_count("doctor"),
        robot_subcommands: live_cli_subcommand_count("robot"),
        themes: mcp_agent_mail_server::tui_theme::NAMED_THEME_COUNT,
    }
}

fn compile(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|e| panic!("invalid regex {pattern:?}: {e}"))
}

fn find_count_match(doc: &str, regex: &Regex) -> Option<CountMatch> {
    for (idx, line) in doc.lines().enumerate() {
        let Some(captures) = regex.captures(line) else {
            continue;
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
            ClaimPattern {
                label: "README feature table robot subcommand count",
                regex: compile(
                    r"\|\s+\*\*Robot Mode\*\*\s+\|\s+(?P<count>\d+) agent-optimized CLI subcommands",
                ),
                expected: counts.robot_subcommands,
                source_of_truth: "clap tree: mcp_agent_mail_cli::Cli::command() -> `robot` subcommands",
            },
            ClaimPattern {
                label: "README robot section heading",
                regex: compile(r"^### (?P<count>\d+) Subcommands$"),
                expected: counts.robot_subcommands,
                source_of_truth: "clap tree: mcp_agent_mail_cli::Cli::command() -> `robot` subcommands",
            },
            ClaimPattern {
                label: "README family detail doctor verb count",
                regex: compile(r"^\| `doctor` \((?P<count>\d+) verbs\) \|"),
                expected: counts.doctor_verbs,
                source_of_truth: "clap tree: mcp_agent_mail_cli::Cli::command() -> `doctor` subcommands",
            },
            ClaimPattern {
                label: "README command families doctor verb count",
                regex: compile(r"`doctor \.\.\.` \((?P<count>\d+) verbs:"),
                expected: counts.doctor_verbs,
                source_of_truth: "clap tree: mcp_agent_mail_cli::Cli::command() -> `doctor` subcommands",
            },
            ClaimPattern {
                label: "README TUI theme count",
                regex: compile(r"\*\*Themes:\*\* (?P<count>\d+) named palettes"),
                expected: counts.themes,
                source_of_truth: "mcp_agent_mail_server::tui_theme::NAMED_THEME_COUNT",
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
                regex: compile(
                    r"\| `mcp-agent-mail-tools` \| `src/` \| (?P<count>\d+) MCP tool implementations",
                ),
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
                regex: compile(r"38 tools\) plus 7 Rust-native tools and (?P<count>\d+) resources"),
                expected: counts.resources,
                source_of_truth: "mcp_agent_mail_server::build_server(...).into_router() resource/template inventory",
            },
            ClaimPattern {
                label: "AGENTS conformance fixture paragraph resource count",
                regex: compile(r"across 37 captured behavior tools and (?P<count>\d+) resources"),
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
            ClaimPattern {
                label: "AGENTS doctor verbs heading",
                regex: compile(r"^### Verbs \((?P<count>\d+) verbs;"),
                expected: counts.doctor_verbs,
                source_of_truth: "clap tree: mcp_agent_mail_cli::Cli::command() -> `doctor` subcommands",
            },
            ClaimPattern {
                label: "AGENTS robot command reference heading",
                regex: compile(r"^#### Command Reference \((?P<count>\d+) subcommands\)$"),
                expected: counts.robot_subcommands,
                source_of_truth: "clap tree: mcp_agent_mail_cli::Cli::command() -> `robot` subcommands",
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

/// Split a documented command into shell words: whitespace separates words
/// outside quotes, and the quotes are removed. No other expansion.
/// Each word records whether any of it was quoted: a quoted `#`, `|` or `>`
/// is an argument, never a comment, pipe, or redirection.
fn shell_words(command: &str) -> Vec<(String, bool)> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quoted = false;
    let mut quote = None;
    for c in command.chars() {
        match quote {
            Some(open) if c == open => quote = None,
            Some(_) => word.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                in_word = true;
                quoted = true;
            }
            None if c.is_whitespace() => {
                if in_word {
                    words.push((std::mem::take(&mut word), quoted));
                    in_word = false;
                    quoted = false;
                }
            }
            None => {
                word.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push((word, quoted));
    }
    words
}

/// Validate `am` command examples that start with one of `prefixes` against
/// the shipped CLI, reporting every command clap rejects.
///
/// Recipes can include quoted arguments, trailing redirection, a pipeline, a
/// comment, or backslash line continuations. Parse their argv; never execute
/// a documented command while validating documentation. A `<placeholder>` or
/// a shell variable such as `"$MESSAGE_ID"` stands for one argument, checked
/// as `1` so it satisfies a string, path, or numeric value alike. An example
/// that elides arguments with `...` cannot be checked and is skipped.
fn validate_cli_recipes(doc: &str, prefixes: &[&str]) -> Result<usize, String> {
    use clap::CommandFactory;

    let lines: Vec<&str> = doc.lines().collect();
    let mut checked = 0;
    let mut rejected = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line_number = index + 1;
        let mut line = lines[index].trim().to_string();
        index += 1;
        if !prefixes.iter().any(|prefix| line.starts_with(prefix)) {
            continue;
        }
        while let Some(head) = line.strip_suffix('\\') {
            let Some(next) = lines.get(index) else {
                break;
            };
            line = format!("{} {}", head.trim_end(), next.trim());
            index += 1;
        }
        let words = shell_words(&line);
        if words.iter().any(|(word, quoted)| !quoted && word == "...") {
            continue;
        }
        let placeholder = |word: &str| {
            (word.len() > 2 && word.starts_with('<') && word.ends_with('>')) || word.contains('$')
        };
        let args: Vec<_> = words
            .iter()
            .take_while(|(word, quoted)| {
                *quoted
                    || (word != "\\"
                        && (placeholder(word) || !word.starts_with(['|', '>', '<', '#']))
                        && !word.starts_with("2>"))
            })
            .map(|(word, _)| {
                if placeholder(word) {
                    "1"
                } else {
                    word.as_str()
                }
            })
            .collect();
        match mcp_agent_mail_cli::Cli::command().try_get_matches_from(args) {
            Ok(_) => checked += 1,
            // `--help` and `--version` are valid requests clap answers early.
            Err(error)
                if matches!(
                    error.kind(),
                    clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
                ) =>
            {
                checked += 1;
            }
            Err(error) => rejected.push(format!(
                "line {line_number}: `{line}`: {}",
                error.to_string().lines().next().unwrap_or_default()
            )),
        }
    }
    if rejected.is_empty() {
        Ok(checked)
    } else {
        Err(format!("invalid `am` recipes:\n{}", rejected.join("\n")))
    }
}

const DOCTOR_AND_ROBOT: &[&str] = &["am doctor ", "am robot "];
const EVERY_AM_COMMAND: &[&str] = &["am "];

#[test]
fn release_doctor_recipes_parse_with_live_cli() {
    for relative in ["docs/RELEASE_CHECKLIST.md", "docs/ROLLOUT_PLAYBOOK.md"] {
        let doc = read_file(workspace_root().join(relative));
        let count = validate_cli_recipes(&doc, DOCTOR_AND_ROBOT)
            .unwrap_or_else(|error| panic!("{relative}: {error}"));
        assert!(count > 0, "{relative}: no doctor recipes were checked");

        // Reproduce the obsolete placement of --json on the doctor root.
        let mutated = doc.replace("am doctor check --json", "am doctor --json");
        assert_ne!(mutated, doc, "{relative}: negative control did not mutate");
        assert!(
            validate_cli_recipes(&mutated, DOCTOR_AND_ROBOT).is_err(),
            "{relative}: obsolete doctor syntax escaped the CLI parser"
        );
    }
}

/// The docs agents copy `am` commands from. Each once published a spelling
/// clap rejects (`am doctor --json`, `--reconstruct-from-archive`,
/// `health --format json`, `fix-orphan-refs --dry-run`, `robot atc --toon`,
/// `guard check <project>`, `archive create`, `share export` without
/// `--output`).
#[test]
fn agent_facing_cli_recipes_parse_with_live_cli() {
    let mut failures = Vec::new();
    for relative in [
        "README.md",
        "AGENTS.md",
        "docs/OPERATOR_COOKBOOK.md",
        "docs/OPERATOR_RUNBOOK.md",
        "docs/OPERATOR_VERIFICATION_RUNBOOK.md",
        "docs/RECOVERY_RUNBOOK.md",
        "docs/RUNBOOK-atc-rollback.md",
        "docs/MIGRATION_GUIDE.md",
    ] {
        let doc = read_file(workspace_root().join(relative));
        match validate_cli_recipes(&doc, EVERY_AM_COMMAND) {
            Ok(0) => failures.push(format!("{relative}: no am recipes were checked")),
            Ok(_) => {}
            Err(error) => failures.push(format!("{relative}: {error}")),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    // Placeholders count as one argument, and the old spellings still fail
    // through them.
    let check = |recipe: &str| validate_cli_recipes(recipe, EVERY_AM_COMMAND);
    assert_eq!(check("am doctor fix --only <fm-id> --yes\n"), Ok(1));
    assert!(check("am doctor --fix --only <fm-id> --yes\n").is_err());
    assert!(check("am doctor fix-orphan-refs --all --dry-run \\\n").is_err());
    assert!(check("am robot atc --summary-only --toon | grep x\n").is_err());
    assert!(check("am guard check my-proj\n").is_err());
    assert!(check("am archive create my-proj\n").is_err());
    // Every rejected line is reported, not just the first.
    let both = check("am archive create a\nam guard check b\n").unwrap_err();
    assert!(both.contains("line 1") && both.contains("line 2"), "{both}");
    // A quoted `#` or `|` is an argument, not a comment or pipe; an unquoted
    // one still ends the command.
    assert_eq!(
        check("am mail send -p p --from A --to B --subject \"#12 | x\" --body b\n"),
        Ok(1)
    );
    assert_eq!(check("am doctor check --json | jq '.healthy'\n"), Ok(1));
}
