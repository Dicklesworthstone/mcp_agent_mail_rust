//! Pass-27 contract: the agent handbook surfaced by `am doctor
//! robot-docs` must reference every verb and per-FM workflow the
//! doctor surface currently supports.
//!
//! Pre-pass-27 the handbook still listed "10 Verbs" while passes 14,
//! 16, 17, 23, 24 had grown the surface to 14. Agents calling cold
//! got an incomplete picture. A hand-pinned count drifted again (15
//! listed against 28 real verbs), and its copy-paste recipes used
//! spellings clap rejects (`am doctor --json`, `am doctor --fix`), so
//! the verb list and the recipes are now checked against the parser
//! itself.

#![forbid(unsafe_code)]

use clap::{CommandFactory, Parser};
use mcp_agent_mail_cli::Cli;
use mcp_agent_mail_cli::doctor::robot_docs::handbook;

const REQUIRED_VERBS: &[&str] = &[
    "am doctor check --json",
    "am doctor fix --dry-run",
    "am doctor fix --yes",
    "am doctor fix --only",
    "am doctor fix --list",
    "am doctor undo",
    "am doctor capabilities",
    "am doctor fixers",
    "am doctor explain",
    "am doctor robot-docs",
    "am doctor health",
    "am doctor ls",
    "am doctor triage",
    "am doctor selftest",
    "am doctor locks",
    "am doctor drain",
];

const REQUIRED_TOPICS: &[&str] = &[
    "mutate()",       // chokepoint mention
    "backups/seq_",   // per-mutation seq-backup layout
    "actions.jsonl",  // hash-witnessed action log
    "AGENTS.md",      // RULE 1 / RULE 2 absolutes
    ".doctor/runs/",  // per-run artifact layout
    ".doctor/latest", // canonical symlink
    "schema_version", // contract versioning
    "<fm-id>",        // the per-FM verb signature
];

#[test]
fn handbook_lists_every_doctor_verb() {
    let text = handbook();
    for verb in REQUIRED_VERBS {
        assert!(
            text.contains(verb),
            "handbook missing required verb mention: `{verb}` — robot_docs.rs is out of sync with the lib.rs verb list"
        );
    }
}

#[test]
fn handbook_names_every_verb_clap_accepts() {
    let text = handbook();
    let cli = Cli::command();
    let doctor = cli
        .find_subcommand("doctor")
        .expect("`am doctor` is a subcommand");
    let verbs: Vec<&str> = doctor
        .get_subcommands()
        .map(clap::Command::get_name)
        .filter(|name| *name != "help")
        .collect();
    assert!(verbs.len() > 20, "doctor verbs went missing: {verbs:?}");
    for verb in verbs {
        assert!(
            text.contains(&format!("`am doctor {verb}")),
            "handbook never names `am doctor {verb}` — add it to a verb table"
        );
    }
}

/// Every `am ...` line in the handbook's fenced shell blocks is meant to be
/// pasted, so each must parse. Pipes and trailing comments are not part of
/// the command; `<placeholders>` stand in for one argument each.
#[test]
fn handbook_shell_recipes_parse() {
    let mut in_shell_block = false;
    let mut checked = 0;
    for line in handbook().lines() {
        let trimmed = line.trim();
        if let Some(fence) = trimmed.strip_prefix("```") {
            in_shell_block = !in_shell_block && fence == "bash";
            continue;
        }
        if !in_shell_block || !trimmed.starts_with("am ") {
            continue;
        }
        let command = trimmed.split('|').next().unwrap_or_default();
        let command = command.split(" #").next().unwrap_or_default().trim();
        let argv: Vec<&str> = command.split_whitespace().collect();
        if let Err(err) = Cli::try_parse_from(&argv) {
            panic!("handbook recipe `{command}` does not parse: {err}");
        }
        checked += 1;
    }
    assert!(checked >= 20, "only {checked} recipe lines found");
}

#[test]
fn handbook_shell_recipe_check_rejects_the_old_spellings() {
    // The negative control for the parse check above: the spellings the
    // handbook used to publish are usage errors.
    for old in [
        "am doctor --json",
        "am doctor --dry-run --fix",
        "am doctor --fix --only fm-x --yes",
        "am doctor --quick --json",
        "am doctor",
    ] {
        let argv: Vec<&str> = old.split_whitespace().collect();
        assert!(
            Cli::try_parse_from(&argv).is_err(),
            "`{old}` unexpectedly parses"
        );
    }
}

#[test]
fn handbook_covers_load_bearing_topics() {
    let text = handbook();
    for topic in REQUIRED_TOPICS {
        assert!(
            text.contains(topic),
            "handbook missing required topic: `{topic}` — agents reading cold won't learn this concept"
        );
    }
}

#[test]
fn handbook_mentions_per_fm_workflow_recipe() {
    let text = handbook();
    // Pass-27 added the per-FM verb recipe as a numbered section.
    // The recipe is the recommended path for agents — it must be
    // present and findable.
    assert!(
        text.contains("Per-FM surface") || text.contains("Per-FM verbs") || text.contains("### 6."),
        "handbook missing the per-FM workflow recipe (pass-27)"
    );
}
