//! Pass-27 contract: the agent handbook surfaced by `am doctor
//! robot-docs` must reference every verb and per-FM workflow the
//! doctor surface currently supports.
//!
//! Pre-pass-27 the handbook still listed "10 Verbs" while passes 14,
//! 16, 17, 23, 24 had grown the surface to 14. Agents calling cold
//! got an incomplete picture.
//!
//! This test pins the verb list: adding a new verb to `lib.rs`
//! without updating `robot_docs.rs::HANDBOOK_TEXT` fails CI here.

#![forbid(unsafe_code)]

use mcp_agent_mail_cli::doctor::robot_docs::handbook;

const REQUIRED_VERBS: &[&str] = &[
    "am doctor",
    "am doctor --fix",
    "am doctor --dry-run --fix",
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
fn handbook_verb_count_matches_table_header() {
    // The verb table header pins the count. If we add another verb,
    // the header must be updated too — otherwise operators see "14
    // Verbs" listing 15 rows, which is confusing.
    let text = handbook();
    assert!(
        text.contains("## The 14 Verbs"),
        "handbook header must say `## The 14 Verbs` (pass-27 contract)"
    );
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
