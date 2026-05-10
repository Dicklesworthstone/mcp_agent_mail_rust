//! `am doctor capabilities --json` — machine-readable contract.
//!
//! Returns the doctor's contract: detectors, fixers, exit codes, env vars,
//! per-run artifact schema. Stable across `doctor_version` minor bumps;
//! agents only care about `doctor_contract_version`.
//!
//! Schema follows OUTPUT-SCHEMA.md and CLI-SURFACE.md from
//! `world-class-doctor-mode-for-cli-tools`.

#![forbid(unsafe_code)]

use serde::Serialize;
use serde_json::json;
use std::path::PathBuf;

use super::runs::{DOCTOR_CONTRACT_VERSION, DOCTOR_VERSION};

#[derive(Debug, Serialize)]
pub struct CapabilitiesReport {
    pub schema_version: &'static str,
    pub tool: &'static str,
    pub tool_version: String,
    pub doctor_version: &'static str,
    pub doctor_contract_version: &'static str,
    pub platform: Platform,
    pub subsystems: Vec<&'static str>,
    pub detectors: Vec<Detector>,
    pub fixers: Vec<Fixer>,
    pub manual_remediations: Vec<ManualRemediation>,
    pub exit_codes: serde_json::Map<String, serde_json::Value>,
    pub env_vars: serde_json::Map<String, serde_json::Value>,
    pub write_scopes: Vec<PathBuf>,
    pub run_artifact_layout: RunArtifactLayout,
    pub report_schema: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Platform {
    pub os: &'static str,
    pub arch: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct Detector {
    pub id: String,
    pub subsystem: &'static str,
    pub severity: &'static str,
    pub description: &'static str,
    pub estimated_cost_ms: u64,
    pub online_required: bool,
    /// Whether this detector is also available in `--quick` (fast pre-commit).
    pub quick_mode_eligible: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Fixer {
    pub id: String,
    pub preconditions: Vec<&'static str>,
    pub writes_to: Vec<&'static str>,
    pub ops: Vec<&'static str>,
    pub reversible: bool,
    pub idempotent: bool,
    pub estimated_cost_ms: u64,
    pub requires_yes: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ManualRemediation {
    pub id: String,
    pub instruction: String,
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct RunArtifactLayout {
    pub root: &'static str,
    pub per_run_dir: &'static str,
    pub files: Vec<&'static str>,
    pub backups_dir: &'static str,
    pub latest_symlink: &'static str,
    pub history_jsonl: &'static str,
}

/// The 11 subsystems Phase 1 archaeology identified.
pub const SUBSYSTEMS: [&str; 11] = [
    "db_state_files",
    "archive_state_files",
    "runtime_processes",
    "mcp_config_files",
    "secrets_env_state",
    "guard_install",
    "environment_toolchain",
    "share_export_state",
    "atc_learning_state",
    "search_index_state",
    "identity_contacts_state",
];

/// Build the full capabilities report for the running binary.
///
/// Phase 4 pass-1 wires the existing am doctor checks (which the live
/// binary already runs) plus the new world-class-mode meta-detectors.
/// Future passes register the 82 spec'd detectors+fixers individually
/// from `analysis/repair_specs/`.
pub fn build_report(tool_version: String, write_scopes: Vec<PathBuf>) -> CapabilitiesReport {
    let mut exit_codes = serde_json::Map::new();
    for (code, label) in [
        ("0", "success_or_healthy"),
        ("1", "findings_present_no_fix"),
        ("2", "fix_partial"),
        ("3", "fix_failed_rolled_back"),
        ("4", "refused_unsafe"),
        ("5", "concurrency_lost"),
        ("6", "online_required"),
        ("64", "usage_error"),
        ("66", "no_input"),
        ("73", "cant_create"),
        ("74", "io_error"),
    ] {
        exit_codes.insert(code.to_string(), json!(label));
    }

    let mut env_vars = serde_json::Map::new();
    for (k, v) in [
        ("AM_INTERFACE_MODE", "Must be 'cli' for am doctor"),
        (
            "AM_DOCTOR_BACKUPS_DIR",
            "Override default .doctor/ location",
        ),
        (
            "AM_GIT_BINARY",
            "Alternate git binary (e.g., for known-bad git 2.51.0 mitigation)",
        ),
        (
            "AM_GIT_FLOCK_TIMEOUT_SECS",
            "Per-archive git serialization lock timeout, default 60",
        ),
        ("STORAGE_ROOT", "Archive root override"),
        ("DATABASE_URL", "SQLite DB location override"),
        (
            "HTTP_BEARER_TOKEN",
            "Active bearer token (read for token-rotation fixers)",
        ),
        ("NO_COLOR", "Disable ANSI"),
        (
            "AM_E2E_FORCE_LEGACY",
            "MUST NOT be set when running am doctor (refused with exit 4)",
        ),
        (
            "ALLOW_EPHEMERAL_PROJECTS_IN_DEFAULT_STORAGE",
            "Permit /tmp-style project roots in default storage",
        ),
    ] {
        env_vars.insert(k.to_string(), json!(v));
    }

    CapabilitiesReport {
        schema_version: "1.0",
        tool: "am",
        tool_version,
        doctor_version: DOCTOR_VERSION,
        doctor_contract_version: DOCTOR_CONTRACT_VERSION,
        platform: Platform {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
        },
        subsystems: SUBSYSTEMS.to_vec(),
        detectors: build_detector_list(),
        fixers: build_fixer_list(),
        manual_remediations: build_manual_remediation_list(),
        exit_codes,
        env_vars,
        write_scopes,
        run_artifact_layout: RunArtifactLayout {
            root: ".doctor/",
            per_run_dir: ".doctor/runs/<ISO8601>__<run-id>/",
            files: vec![
                "report.json",
                "report.md",
                "scorecard.json",
                "actions.jsonl",
                "stderr.log",
                "stdout.json",
                "undo.sh",
            ],
            backups_dir: ".doctor/runs/<run-id>/backups/",
            latest_symlink: ".doctor/latest -> runs/<run-id>",
            history_jsonl: ".doctor/scorecard_history.jsonl",
        },
        report_schema: "https://github.com/Dicklesworthstone/mcp_agent_mail_rust/blob/main/docs/SPEC-doctor-report.md",
    }
}

/// Existing-surface detectors (the 35 checks `am doctor check` already runs)
/// plus the new meta-detectors. Phase 4 pass-2 wires the per-FM detectors
/// from `analysis/repair_specs/` here individually.
fn build_detector_list() -> Vec<Detector> {
    vec![
        // Live operational
        det(
            "server_port",
            "runtime_processes",
            "P1",
            "Port 8765 owner verification",
            50,
            false,
            true,
        ),
        det(
            "server_process_cpu",
            "runtime_processes",
            "P2",
            "Runaway CPU on listener",
            100,
            false,
            false,
        ),
        det(
            "server_http_health",
            "runtime_processes",
            "P1",
            "/health probe (offline-only when --online not passed)",
            200,
            false,
            false,
        ),
        det(
            "server_jsonrpc_health",
            "runtime_processes",
            "P1",
            "JSON-RPC health_check probe",
            300,
            false,
            false,
        ),
        det(
            "database",
            "db_state_files",
            "P0",
            "SQLite reachable",
            30,
            false,
            true,
        ),
        det(
            "db_file_sanity",
            "db_state_files",
            "P0",
            "PRAGMA quick_check + file size sane",
            80,
            false,
            true,
        ),
        det(
            "pool_init",
            "db_state_files",
            "P0",
            "Pool initializes without panic",
            50,
            false,
            true,
        ),
        det(
            "storage_root_writable",
            "archive_state_files",
            "P1",
            "Storage root writable",
            20,
            false,
            true,
        ),
        det(
            "archive_db_parity",
            "archive_state_files",
            "P1",
            "DB and Git archive in agreement",
            200,
            false,
            false,
        ),
        det(
            "foreign_key_integrity",
            "db_state_files",
            "P1",
            "PRAGMA foreign_key_check",
            100,
            false,
            false,
        ),
        // Archive hygiene
        det(
            "archive_hygiene",
            "archive_state_files",
            "P2",
            "Group-bucketed issue counts",
            500,
            false,
            false,
        ),
        det(
            "storage_root",
            "archive_state_files",
            "P1",
            "Storage root present + slug present",
            30,
            false,
            true,
        ),
        det(
            "storage_root_disk_space",
            "archive_state_files",
            "P2",
            "Free space above watermark",
            20,
            false,
            true,
        ),
        det(
            "storage_root_git_repo",
            "archive_state_files",
            "P0",
            "git/ exists at storage root",
            30,
            false,
            true,
        ),
        det(
            "storage_root_git_index_lock",
            "archive_state_files",
            "P1",
            "No stale .git/index.lock",
            30,
            false,
            true,
        ),
        // Environment & config
        det(
            "git_binary_path",
            "environment_toolchain",
            "P1",
            "git resolvable",
            30,
            false,
            true,
        ),
        det(
            "installed_agents",
            "environment_toolchain",
            "P3",
            "Detected agent CLIs",
            50,
            false,
            false,
        ),
        det(
            "binary_resolution",
            "environment_toolchain",
            "P1",
            "am binary on PATH and reachable",
            30,
            false,
            true,
        ),
        det(
            "legacy_python_alias",
            "environment_toolchain",
            "P2",
            "Old Python mcp_agent_mail shadowing",
            30,
            false,
            true,
        ),
        det(
            "stale_python_processes",
            "runtime_processes",
            "P0",
            "Stale Python servers",
            100,
            false,
            false,
        ),
        det(
            "path_order",
            "environment_toolchain",
            "P2",
            "~/.local/bin before /usr/local/bin",
            20,
            false,
            true,
        ),
        det(
            "binary_version",
            "environment_toolchain",
            "P3",
            "am PATH/install provenance matches source version",
            50,
            false,
            false,
        ),
        det(
            "server_binary_version",
            "environment_toolchain",
            "P3",
            "mcp-agent-mail PATH/install provenance matches source version",
            50,
            false,
            false,
        ),
        det(
            "timestamp_format",
            "db_state_files",
            "P0",
            "DB timestamps in expected format (i64 vs TEXT)",
            100,
            false,
            false,
        ),
        det(
            "wal_mode",
            "db_state_files",
            "P1",
            "SQLite in WAL mode",
            30,
            false,
            true,
        ),
        det(
            "schema_version",
            "db_state_files",
            "P0",
            "Schema version current",
            30,
            false,
            true,
        ),
        det(
            "fts5",
            "db_state_files",
            "P2",
            "FTS5 module loadable",
            30,
            false,
            true,
        ),
        det(
            "guard_hooks",
            "guard_install",
            "P1",
            "Pre-commit hook installed",
            50,
            false,
            true,
        ),
        det(
            "mcp_config",
            "mcp_config_files",
            "P1",
            "Agent configs point at am",
            100,
            false,
            false,
        ),
        det(
            "mcp_config_token",
            "mcp_config_files",
            "P0",
            "Bearer token matches",
            50,
            false,
            false,
        ),
        det(
            "update_available",
            "environment_toolchain",
            "P3",
            "Newer release available",
            1000,
            true,
            false,
        ),
        det(
            "beads_issue_awareness",
            "environment_toolchain",
            "P3",
            ".beads/ integration",
            30,
            false,
            true,
        ),
    ]
}

#[allow(clippy::too_many_arguments)]
fn det(
    id: &str,
    subsystem: &'static str,
    severity: &'static str,
    description: &'static str,
    estimated_cost_ms: u64,
    online_required: bool,
    quick_mode_eligible: bool,
) -> Detector {
    Detector {
        id: id.to_string(),
        subsystem,
        severity,
        description,
        estimated_cost_ms,
        online_required,
        quick_mode_eligible,
    }
}

/// Existing-surface fixers (the operations `am doctor fix` / `am doctor
/// repair` / `am doctor reconstruct` perform). Phase 4 pass-2 routes these
/// through `mutate()` and adds per-FM fixers from the spec corpus.
fn build_fixer_list() -> Vec<Fixer> {
    vec![
        Fixer {
            id: "legacy_python_alias_repair".to_string(),
            preconditions: vec!["lock_acquired", "config_env_writable"],
            writes_to: vec!["~/.config/mcp-agent-mail/config.env"],
            ops: vec!["WriteFile"],
            reversible: true,
            idempotent: true,
            estimated_cost_ms: 50,
            requires_yes: false,
        },
        Fixer {
            id: "mcp_config_repair".to_string(),
            preconditions: vec!["lock_acquired", "agent_configs_present"],
            writes_to: vec![
                "~/.codex/config.toml",
                "~/.claude/.mcp.json",
                "~/.gemini/settings.json",
                "~/.cursor/mcp.json",
            ],
            ops: vec!["WriteFile"],
            reversible: true,
            idempotent: true,
            estimated_cost_ms: 100,
            requires_yes: false,
        },
        Fixer {
            id: "storage_root_git_index_lock_remove".to_string(),
            preconditions: vec!["lock_acquired", "lock_age_gt_5min", "no_live_writer_pid"],
            writes_to: vec!["<STORAGE_ROOT>/projects/<slug>/.git/index.lock"],
            ops: vec!["Rename"],
            reversible: true,
            idempotent: true,
            estimated_cost_ms: 30,
            requires_yes: false,
        },
        Fixer {
            id: "guard_install".to_string(),
            preconditions: vec!["lock_acquired", "git_repo_present"],
            writes_to: vec!["<repo>/.git/hooks/pre-commit"],
            ops: vec!["WriteFile", "Chmod"],
            reversible: true,
            idempotent: true,
            estimated_cost_ms: 50,
            requires_yes: false,
        },
        Fixer {
            id: "wal_mode_enable".to_string(),
            preconditions: vec!["lock_acquired", "db_writable"],
            writes_to: vec!["<DATABASE_URL>"],
            ops: vec!["DbExec"],
            reversible: false, // PRAGMA journal_mode=WAL is a one-way change
            idempotent: true,
            estimated_cost_ms: 100,
            requires_yes: false,
        },
        Fixer {
            id: "database_repair".to_string(),
            preconditions: vec!["lock_acquired", "db_writable", "backup_dir_writable"],
            writes_to: vec!["<DATABASE_URL>"],
            ops: vec!["WriteFile", "DbExec"],
            reversible: true,
            idempotent: true,
            estimated_cost_ms: 5000,
            requires_yes: false,
        },
        Fixer {
            id: "database_reconstruct".to_string(),
            preconditions: vec!["lock_acquired", "archive_intact", "db_quarantine_writable"],
            writes_to: vec!["<DATABASE_URL>"],
            ops: vec!["Rename", "WriteFile", "DbMigrate"],
            reversible: true,
            idempotent: false, // generates new run-id; can't be re-run cleanly
            estimated_cost_ms: 30000,
            requires_yes: true,
        },
        Fixer {
            id: "server_runtime_stop_unhealthy".to_string(),
            preconditions: vec!["lock_acquired", "no_active_writes"],
            writes_to: vec!["<.doctor/runs/<id>/killed_pids.txt>"],
            ops: vec!["AppendFile"], // Records killed PIDs; signal is not a mutate Op
            reversible: false,       // killing a process is not undoable
            idempotent: true,
            estimated_cost_ms: 5000,
            requires_yes: true,
        },
    ]
}

fn build_manual_remediation_list() -> Vec<ManualRemediation> {
    vec![
        ManualRemediation {
            id: "fm-environment_toolchain-known-bad-git-no-override".to_string(),
            instruction: "Set AM_GIT_BINARY=/path/to/safe/git in $XDG_CONFIG_HOME/mcp-agent-mail/config.env".to_string(),
            reason: "git 2.51.0 segfaults under multi-process concurrency. The doctor cannot install git binaries.".to_string(),
        },
        ManualRemediation {
            id: "fm-environment_toolchain-path-order-shadows-am".to_string(),
            instruction: "Add ~/.local/bin to your PATH before /usr/bin in your shell rc file. Re-run `am doctor`.".to_string(),
            reason: "Doctor refuses to edit user shell rc files (see safety_envelope.md).".to_string(),
        },
        ManualRemediation {
            id: "fm-secrets_env_state-committed-env-file-in-repo".to_string(),
            instruction: "Remove .env from your tracked git history (BFG / git-filter-repo). Then add to .gitignore. The doctor refuses to alter the git index.".to_string(),
            reason: "Token rotation does not fix already-committed secrets; git history scrub is required.".to_string(),
        },
    ]
}
