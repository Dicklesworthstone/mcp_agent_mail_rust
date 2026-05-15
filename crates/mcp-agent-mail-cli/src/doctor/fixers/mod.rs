//! Per-FM detector/fixer pairs for the world-class `am doctor` surface.
//!
//! Pass-8 introduces the FM (failure-mode) production pattern: each
//! detector is a pure function that scans system state and returns a
//! `Finding` list; each fixer takes a `Finding` plus a `MutateContext`
//! and routes its mutations through the chokepoint.
//!
//! Today the module hosts one concrete fixer
//! (`stale_archive_lock::detect` + `::fix`) as the reference pattern.
//! Pass-9+ adds the remaining priority FMs identified by Phase 3
//! synthesis (see `__doctor_workspace/analysis/dependency_graph.json`).
//!
//! Per AGENTS.md:
//! - No file deletion. Use `Op::Rename` to quarantine.
//! - asupersync only. Fixers are synchronous; the doctor runs out of
//!   band of the request hot path.
//! - `#![forbid(unsafe_code)]`.

#![forbid(unsafe_code)]

pub mod dangling_doctor_latest;
pub mod empty_or_truncated_db;
pub mod known_bad_git_no_override;
pub mod missing_gitignore_entry;
pub mod stale_archive_lock;
pub mod stale_bearer_token_skew;
pub mod stale_head_or_ref_lock;
pub mod stale_listener_pid_hint;
pub mod wal_mode_disabled;
pub mod world_readable_storage_db;
pub mod world_readable_token_bak;
pub mod wrong_mcp_url_json;

use serde::Serialize;

/// `kill(pid, 0)` — POSIX liveness probe.
///
/// Shared helper for any fixer that needs to check whether a recorded
/// PID is still running. Returns `true` iff the process exists, including
/// when the caller lacks permission to signal it.
///
/// Caveat: `Pid::from_raw(0)` would refer to the calling process's
/// process group on POSIX, so PID 0 is rejected before probing. Tests
/// that want a guaranteed-dead PID should use `999_999_999` (above all
/// known `pid_max` values on Linux/macOS/BSD).
pub(crate) fn is_pid_alive(pid: u32) -> bool {
    use nix::unistd::Pid;

    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }

    pid_probe_result_is_alive(nix::sys::signal::kill(Pid::from_raw(pid), None))
}

fn pid_probe_result_is_alive(result: Result<(), nix::errno::Errno>) -> bool {
    use nix::errno::Errno;

    matches!(result, Ok(()) | Err(Errno::EPERM))
}

/// One finding from a detector. Serializable for inclusion in
/// `report.json::findings[]`.
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    /// Stable ID, e.g. `"fm-archive-state-files-stale-archive-lock-from-dead-pid"`.
    pub id: &'static str,
    /// Severity tier: `"P0"` | `"P1"` | `"P2"` | `"P3"`.
    pub severity: &'static str,
    /// Subsystem from the 11-category Phase 1 taxonomy.
    pub subsystem: &'static str,
    /// One-line human-readable title.
    pub title: String,
    /// 0.0-1.0; ≥0.95 means the detector is certain.
    pub confidence: f32,
    /// Structured evidence: file:line, sql query, hash, etc.
    pub evidence: serde_json::Value,
    /// Suggested remediation command (for capabilities-routing).
    pub remediation: FindingRemediation,
}

#[derive(Debug, Clone, Serialize)]
pub struct FindingRemediation {
    pub command: String,
    pub explain_command: String,
    pub auto_fixable: bool,
    pub estimated_actions: usize,
}

/// Outcome of a fix attempt — what mutate() actions were taken.
#[derive(Debug, Default)]
pub struct FixOutcome {
    pub actions_taken: usize,
    pub actions_skipped: usize,
    pub quarantined_paths: Vec<std::path::PathBuf>,
}

/// Static registry entry for a per-FM fixer. Used by
/// `am doctor fixers` (enumeration) and `am doctor capabilities --json`
/// (machine-readable contract).
#[derive(Debug, Clone, Serialize)]
pub struct FixerSpec {
    /// Canonical FM id, e.g. `"fm-archive-state-files-stale-archive-lock-from-dead-pid"`.
    pub id: &'static str,
    pub severity: &'static str, // "P0" | "P1" | "P2" | "P3"
    pub subsystem: &'static str,
    /// One of: `"Op::Rename"`, `"Op::WriteFile"`, `"Op::AppendFile"`,
    /// `"Op::Chmod"`, `"Op::DbExec"`, `"Op::DbMigrate"`,
    /// `"Op::SymlinkAtomic"`, `"detect-only"`.
    pub op_pattern: &'static str,
    pub auto_fixable: bool,
    pub one_line_description: &'static str,
    /// Module path under `crates/mcp-agent-mail-cli/src/doctor/fixers/`
    /// for operator/agent navigation.
    pub source_module: &'static str,
}

/// Returns the canonical, alphabetically-sorted list of all FM-level
/// fixers in this build. Pass-14 baseline. Adding a new fixer means:
/// 1. Add its module to `pub mod` declarations above
/// 2. Add an entry here
/// 3. (No other wiring needed — `am doctor fixers` picks it up
///    automatically.)
pub fn registry() -> Vec<FixerSpec> {
    vec![
        FixerSpec {
            id: missing_gitignore_entry::FM_ID,
            severity: "P2",
            subsystem: "archive_state_files",
            op_pattern: "Op::AppendFile",
            auto_fixable: true,
            one_line_description: "Repo .gitignore missing `.doctor/` so per-run artifacts get committed",
            source_module: "doctor::fixers::missing_gitignore_entry",
        },
        FixerSpec {
            id: stale_archive_lock::FM_ID,
            severity: "P1",
            subsystem: "archive_state_files",
            op_pattern: "Op::Rename",
            auto_fixable: true,
            one_line_description: "Stale .git/index.lock whose holder PID is dead",
            source_module: "doctor::fixers::stale_archive_lock",
        },
        FixerSpec {
            id: stale_head_or_ref_lock::FM_ID,
            severity: "P2",
            subsystem: "archive_state_files",
            op_pattern: "Op::Rename",
            auto_fixable: true,
            one_line_description: "Stale .git/HEAD.lock / packed-refs.lock / refs/**/*.lock files",
            source_module: "doctor::fixers::stale_head_or_ref_lock",
        },
        FixerSpec {
            id: empty_or_truncated_db::FM_ID,
            severity: "P0",
            subsystem: "db_state_files",
            op_pattern: "detect-only",
            auto_fixable: false,
            one_line_description: "storage.sqlite3 is empty / truncated / fails PRAGMA quick_check (manual reconstruct required)",
            source_module: "doctor::fixers::empty_or_truncated_db",
        },
        FixerSpec {
            id: wal_mode_disabled::FM_ID,
            severity: "P1",
            subsystem: "db_state_files",
            op_pattern: "Op::DbExec",
            auto_fixable: true,
            one_line_description: "storage.sqlite3 has journal_mode != 'wal' (reader/writer lock contention)",
            source_module: "doctor::fixers::wal_mode_disabled",
        },
        FixerSpec {
            id: world_readable_storage_db::FM_ID,
            severity: "P0",
            subsystem: "db_state_files",
            op_pattern: "Op::Chmod",
            auto_fixable: true,
            one_line_description: "storage.sqlite3 has world/group-readable mode (leaks all message bodies)",
            source_module: "doctor::fixers::world_readable_storage_db",
        },
        FixerSpec {
            id: dangling_doctor_latest::FM_ID,
            severity: "P2",
            subsystem: "doctor_state_files",
            op_pattern: "Op::SymlinkAtomic",
            auto_fixable: true,
            one_line_description: ".doctor/latest symlink points at a non-existent runs/<id> directory",
            source_module: "doctor::fixers::dangling_doctor_latest",
        },
        FixerSpec {
            id: known_bad_git_no_override::FM_ID,
            severity: "P0",
            subsystem: "environment_toolchain",
            op_pattern: "detect-only",
            auto_fixable: false,
            one_line_description: "git 2.51.0 (segfault under multi-process load) with no AM_GIT_BINARY override",
            source_module: "doctor::fixers::known_bad_git_no_override",
        },
        FixerSpec {
            id: stale_bearer_token_skew::FM_ID,
            severity: "P1",
            subsystem: "mcp_config_files",
            op_pattern: "Op::WriteFile",
            auto_fixable: true,
            one_line_description: "MCP client JSON config has stale bearer token (rotated since config write)",
            source_module: "doctor::fixers::stale_bearer_token_skew",
        },
        FixerSpec {
            id: wrong_mcp_url_json::FM_ID,
            severity: "P1",
            subsystem: "mcp_config_files",
            op_pattern: "Op::WriteFile",
            auto_fixable: true,
            one_line_description: "MCP client JSON config has wrong mcp-agent-mail URL (port/host/scheme/path)",
            source_module: "doctor::fixers::wrong_mcp_url_json",
        },
        FixerSpec {
            id: stale_listener_pid_hint::FM_ID,
            severity: "P1",
            subsystem: "runtime_processes",
            op_pattern: "Op::Rename",
            auto_fixable: true,
            one_line_description: "Stale listener.pid hint file (dead PID or old mtime)",
            source_module: "doctor::fixers::stale_listener_pid_hint",
        },
        FixerSpec {
            id: world_readable_token_bak::FM_ID,
            severity: "P1",
            subsystem: "secrets_env_state",
            op_pattern: "Op::Chmod",
            auto_fixable: true,
            one_line_description: "Token-bearing .bak/.tmp/.orig backup with world/group-readable mode (target 0o600)",
            source_module: "doctor::fixers::world_readable_token_bak",
        },
    ]
}

/// Inputs to `dispatch_only`. Each FM module pulls only the fields it
/// needs — `dispatch_only` is a `match` on FM id, not a trait, because
/// the six concrete fixers have heterogeneous input shapes and a
/// premature trait would just bury the differences.
#[derive(Debug, Clone)]
pub struct DispatchInputs {
    /// Repository root (used as a default scope-anchor and for default
    /// glob expansion).
    pub repo_root: std::path::PathBuf,
    /// `<storage_root>/projects/*/` archive roots for stale-lock scans.
    /// Caller is responsible for enumerating; an empty slice short-circuits
    /// the relevant FMs to "no findings."
    pub archive_roots: Vec<std::path::PathBuf>,
    /// PID-hint candidate paths for the listener-pid-hint FM (typically
    /// `<storage_root>/listener.pid` plus an operator override).
    pub pid_hint_candidates: Vec<std::path::PathBuf>,
    /// Candidate token-bearing backup files for the chmod FM.
    pub token_backup_candidates: Vec<std::path::PathBuf>,
    /// Candidate MCP client JSON configs to scan for stale URLs.
    pub mcp_config_candidates: Vec<std::path::PathBuf>,
    /// Canonical MCP URL the configs are expected to point at, e.g.
    /// `http://127.0.0.1:8765/mcp/`. Required only for the wrong-url FM.
    pub canonical_mcp_url: Option<String>,
    /// Canonical HTTP bearer token from server config. `None`
    /// skips the `stale_bearer_token_skew` FM. Empty string is
    /// treated as "unconfigured" by the detector (never flag).
    pub canonical_bearer_token: Option<String>,
    /// Inputs for the known-bad-git detect-only FM (system git path,
    /// version string, AM_GIT_BINARY override). `None` skips the FM.
    pub git_detect: Option<known_bad_git_no_override::DetectInputs>,
    /// Path to the repo `.gitignore` for the
    /// `missing_gitignore_entry` FM. `None` skips the FM. Typically
    /// `<repo_root>/.gitignore`.
    pub gitignore_target: Option<std::path::PathBuf>,
    /// Candidate SQLite database file paths for the
    /// `world_readable_storage_db` FM. Typically just
    /// `<storage_root>/storage.sqlite3` (or whatever
    /// `DbPoolConfig::database_url` resolves to). Empty slice
    /// skips the FM.
    pub db_file_candidates: Vec<std::path::PathBuf>,
    /// Path to the `<repo>/.doctor/latest` symlink for the
    /// `dangling_doctor_latest` FM. `None` skips the FM.
    pub doctor_latest_target: Option<std::path::PathBuf>,
    /// Optional override for the per-FM mtime-based staleness threshold.
    ///
    /// `None` (the production default) means each stale-* FM uses its
    /// own canonical `DEFAULT_STALE_SECONDS` const (e.g. 300s for
    /// archive-lock, 120s for ref-lock, 600s for listener-pid-hint).
    /// `Some(secs)` forces a single override across all three. Tests
    /// use `Some(0)` to fire detectors immediately, but production
    /// callers should leave this at `None` so each FM gets the right
    /// canonical default — pass-19 closed a drift bug where the handler
    /// hardcoded `stale_archive_lock::DEFAULT_STALE_SECONDS` (300s) and
    /// applied it to ref-lock (canonical 120s) and listener-pid (600s)
    /// alike.
    pub stale_seconds_override: Option<u64>,
}

/// Outcome of `dispatch_only`: aggregated counts plus serializable
/// findings (so callers can embed them in `report.json`).
#[derive(Debug, Default, Serialize)]
pub struct DispatchOutcome {
    pub fm_id: String,
    pub findings_count: usize,
    pub actions_taken: usize,
    pub actions_skipped: usize,
    pub quarantined_paths: Vec<std::path::PathBuf>,
    pub findings: Vec<Finding>,
}

#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error("unknown FM id {0}; run `am doctor fixers` to see valid ids")]
    UnknownFm(String),
    #[error("missing required input for {fm_id}: {field}")]
    MissingInput {
        fm_id: &'static str,
        field: &'static str,
    },
    #[error(transparent)]
    Mutate(#[from] crate::doctor::mutate::MutateError),
}

/// Dispatch a single registered FM's detect+fix through `mutate()`.
///
/// Resolves `fm_id` against the registry and invokes the matching
/// module's `detect()` + `fix()` with inputs drawn from `DispatchInputs`.
/// Detect-only FMs (e.g., `known_bad_git_no_override`) skip the `fix()`
/// call and report only the findings.
///
/// The chokepoint enforces backups, scope, locks, atomicity, and
/// reversibility; this dispatcher is purely a router.
pub fn dispatch_only(
    fm_id: &str,
    ctx: &crate::doctor::mutate::MutateContext,
    inputs: &DispatchInputs,
) -> Result<DispatchOutcome, DispatchError> {
    let mut outcome = DispatchOutcome {
        fm_id: fm_id.to_string(),
        ..Default::default()
    };

    if fm_id == stale_archive_lock::FM_ID {
        let stale_secs = inputs
            .stale_seconds_override
            .unwrap_or(stale_archive_lock::DEFAULT_STALE_SECONDS);
        let findings = stale_archive_lock::detect(&inputs.archive_roots, stale_secs);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = stale_archive_lock::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
            outcome.quarantined_paths.extend(result.quarantined_paths);
        }
    } else if fm_id == stale_head_or_ref_lock::FM_ID {
        let stale_secs = inputs
            .stale_seconds_override
            .unwrap_or(stale_head_or_ref_lock::DEFAULT_STALE_SECONDS);
        let findings = stale_head_or_ref_lock::detect(&inputs.archive_roots, stale_secs);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = stale_head_or_ref_lock::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
            outcome.quarantined_paths.extend(result.quarantined_paths);
        }
    } else if fm_id == stale_listener_pid_hint::FM_ID {
        let stale_secs = inputs
            .stale_seconds_override
            .unwrap_or(stale_listener_pid_hint::DEFAULT_STALE_SECONDS);
        let findings = stale_listener_pid_hint::detect(&inputs.pid_hint_candidates, stale_secs);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = stale_listener_pid_hint::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
            outcome.quarantined_paths.extend(result.quarantined_paths);
        }
    } else if fm_id == known_bad_git_no_override::FM_ID {
        let git_inputs = inputs
            .git_detect
            .as_ref()
            .ok_or(DispatchError::MissingInput {
                fm_id: known_bad_git_no_override::FM_ID,
                field: "git_detect",
            })?;
        let findings = known_bad_git_no_override::detect(git_inputs);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            // Detect-only: fix is a no-op (returns actions_skipped: 1).
            let result = known_bad_git_no_override::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == world_readable_token_bak::FM_ID {
        let findings = world_readable_token_bak::detect(&inputs.token_backup_candidates);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = world_readable_token_bak::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == world_readable_storage_db::FM_ID {
        let findings = world_readable_storage_db::detect(&inputs.db_file_candidates);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = world_readable_storage_db::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == wal_mode_disabled::FM_ID {
        let findings = wal_mode_disabled::detect(&inputs.db_file_candidates);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = wal_mode_disabled::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == empty_or_truncated_db::FM_ID {
        let findings = empty_or_truncated_db::detect(&inputs.db_file_candidates);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            // Detect-only — fix is a no-op.
            let result = empty_or_truncated_db::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == dangling_doctor_latest::FM_ID {
        let latest = inputs
            .doctor_latest_target
            .as_deref()
            .ok_or(DispatchError::MissingInput {
                fm_id: dangling_doctor_latest::FM_ID,
                field: "doctor_latest_target",
            })?;
        let findings = dangling_doctor_latest::detect(latest);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = dangling_doctor_latest::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == wrong_mcp_url_json::FM_ID {
        let canonical = inputs
            .canonical_mcp_url
            .as_deref()
            .ok_or(DispatchError::MissingInput {
                fm_id: wrong_mcp_url_json::FM_ID,
                field: "canonical_mcp_url",
            })?;
        let findings = wrong_mcp_url_json::detect(canonical, &inputs.mcp_config_candidates);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = wrong_mcp_url_json::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == stale_bearer_token_skew::FM_ID {
        let canonical = inputs
            .canonical_bearer_token
            .as_deref()
            .ok_or(DispatchError::MissingInput {
                fm_id: stale_bearer_token_skew::FM_ID,
                field: "canonical_bearer_token",
            })?;
        let findings = stale_bearer_token_skew::detect(canonical, &inputs.mcp_config_candidates);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = stale_bearer_token_skew::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == missing_gitignore_entry::FM_ID {
        let gitignore = inputs
            .gitignore_target
            .as_deref()
            .ok_or(DispatchError::MissingInput {
                fm_id: missing_gitignore_entry::FM_ID,
                field: "gitignore_target",
            })?;
        let findings = missing_gitignore_entry::detect(gitignore);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = missing_gitignore_entry::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else {
        return Err(DispatchError::UnknownFm(fm_id.to_string()));
    }

    Ok(outcome)
}

/// Outcome of `detect_only`: findings plus the inferred action-count
/// that a full `dispatch_only` would have planned. Used by
/// `am doctor fix --only <fm-id> --list` (pass-16) to preview work
/// without invoking the `mutate()` chokepoint at all.
#[derive(Debug, Default, Serialize)]
pub struct DetectOutcome {
    pub fm_id: String,
    pub findings_count: usize,
    /// Each finding's `remediation.estimated_actions` summed. For
    /// detect-only FMs this is 0.
    pub actions_planned: usize,
    pub findings: Vec<Finding>,
}

/// Pure-detection variant of `dispatch_only`. Calls only `detect()`,
/// never `fix()`. Skips the `mutate()` chokepoint entirely — no
/// run-dir scaffolding, no `actions.jsonl` lines, no advisory locks.
///
/// Used by `am doctor fix --only <fm-id> --list`: an operator can
/// preview the findings (and the inferred action plan) before
/// committing to a real `--fix` run. Roughly an order of magnitude
/// cheaper than `--dry-run` for FMs whose `fix()` does substantial
/// pre-mutate work (JSON re-parse, etc.).
pub fn detect_only(fm_id: &str, inputs: &DispatchInputs) -> Result<DetectOutcome, DispatchError> {
    let mut outcome = DetectOutcome {
        fm_id: fm_id.to_string(),
        ..Default::default()
    };

    let raw_findings: Vec<Finding> = if fm_id == stale_archive_lock::FM_ID {
        let stale_secs = inputs
            .stale_seconds_override
            .unwrap_or(stale_archive_lock::DEFAULT_STALE_SECONDS);
        stale_archive_lock::detect(&inputs.archive_roots, stale_secs)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == stale_head_or_ref_lock::FM_ID {
        let stale_secs = inputs
            .stale_seconds_override
            .unwrap_or(stale_head_or_ref_lock::DEFAULT_STALE_SECONDS);
        stale_head_or_ref_lock::detect(&inputs.archive_roots, stale_secs)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == stale_listener_pid_hint::FM_ID {
        let stale_secs = inputs
            .stale_seconds_override
            .unwrap_or(stale_listener_pid_hint::DEFAULT_STALE_SECONDS);
        stale_listener_pid_hint::detect(&inputs.pid_hint_candidates, stale_secs)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == known_bad_git_no_override::FM_ID {
        let git_inputs = inputs
            .git_detect
            .as_ref()
            .ok_or(DispatchError::MissingInput {
                fm_id: known_bad_git_no_override::FM_ID,
                field: "git_detect",
            })?;
        known_bad_git_no_override::detect(git_inputs)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == world_readable_token_bak::FM_ID {
        world_readable_token_bak::detect(&inputs.token_backup_candidates)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == world_readable_storage_db::FM_ID {
        world_readable_storage_db::detect(&inputs.db_file_candidates)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == wal_mode_disabled::FM_ID {
        wal_mode_disabled::detect(&inputs.db_file_candidates)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == empty_or_truncated_db::FM_ID {
        empty_or_truncated_db::detect(&inputs.db_file_candidates)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == dangling_doctor_latest::FM_ID {
        let latest = inputs
            .doctor_latest_target
            .as_deref()
            .ok_or(DispatchError::MissingInput {
                fm_id: dangling_doctor_latest::FM_ID,
                field: "doctor_latest_target",
            })?;
        dangling_doctor_latest::detect(latest)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == wrong_mcp_url_json::FM_ID {
        let canonical = inputs
            .canonical_mcp_url
            .as_deref()
            .ok_or(DispatchError::MissingInput {
                fm_id: wrong_mcp_url_json::FM_ID,
                field: "canonical_mcp_url",
            })?;
        wrong_mcp_url_json::detect(canonical, &inputs.mcp_config_candidates)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == stale_bearer_token_skew::FM_ID {
        let canonical = inputs
            .canonical_bearer_token
            .as_deref()
            .ok_or(DispatchError::MissingInput {
                fm_id: stale_bearer_token_skew::FM_ID,
                field: "canonical_bearer_token",
            })?;
        stale_bearer_token_skew::detect(canonical, &inputs.mcp_config_candidates)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == missing_gitignore_entry::FM_ID {
        let gitignore = inputs
            .gitignore_target
            .as_deref()
            .ok_or(DispatchError::MissingInput {
                fm_id: missing_gitignore_entry::FM_ID,
                field: "gitignore_target",
            })?;
        missing_gitignore_entry::detect(gitignore)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else {
        return Err(DispatchError::UnknownFm(fm_id.to_string()));
    };

    outcome.findings_count = raw_findings.len();
    outcome.actions_planned = raw_findings
        .iter()
        .map(|f| f.remediation.estimated_actions)
        .sum();
    outcome.findings = raw_findings;
    Ok(outcome)
}

/// Successful per-FM entry in the all-fixer detect-only report.
#[derive(Debug, Serialize)]
pub struct DetectAllFmOutcome {
    pub fm_id: String,
    pub severity: &'static str,
    pub subsystem: &'static str,
    pub op_pattern: &'static str,
    pub findings_count: usize,
    pub actions_planned: usize,
    pub findings: Vec<Finding>,
}

/// Per-FM detector skipped because the caller could not supply an
/// input required by that detector.
#[derive(Debug, Serialize)]
pub struct DetectAllSkipped {
    pub fm_id: String,
    pub severity: &'static str,
    pub subsystem: &'static str,
    pub reason: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missing_field: Option<&'static str>,
}

/// Aggregated detect-only report for every registered FM-level fixer.
#[derive(Debug, Serialize)]
pub struct DetectAllOutcome {
    pub fm_count: usize,
    pub total_findings: usize,
    pub total_actions_planned: usize,
    pub per_fm: Vec<DetectAllFmOutcome>,
    pub skipped: Vec<DetectAllSkipped>,
}

/// Run every registered FM detector and aggregate the agent-facing
/// `am doctor fix --list` report without invoking any fixer.
pub fn detect_all(inputs: &DispatchInputs) -> Result<DetectAllOutcome, DispatchError> {
    let specs = registry();
    let mut outcome = DetectAllOutcome {
        fm_count: specs.len(),
        total_findings: 0,
        total_actions_planned: 0,
        per_fm: Vec::with_capacity(specs.len()),
        skipped: Vec::new(),
    };

    for spec in &specs {
        match detect_only(spec.id, inputs) {
            Ok(detected) => {
                outcome.total_findings += detected.findings_count;
                outcome.total_actions_planned += detected.actions_planned;
                outcome.per_fm.push(DetectAllFmOutcome {
                    fm_id: spec.id.to_string(),
                    severity: spec.severity,
                    subsystem: spec.subsystem,
                    op_pattern: spec.op_pattern,
                    findings_count: detected.findings_count,
                    actions_planned: detected.actions_planned,
                    findings: detected.findings,
                });
            }
            Err(DispatchError::MissingInput { fm_id, field }) => {
                outcome.skipped.push(DetectAllSkipped {
                    fm_id: fm_id.to_string(),
                    severity: spec.severity,
                    subsystem: spec.subsystem,
                    reason: "missing_input",
                    missing_field: Some(field),
                });
            }
            Err(DispatchError::UnknownFm(id)) => {
                outcome.skipped.push(DetectAllSkipped {
                    fm_id: id,
                    severity: spec.severity,
                    subsystem: spec.subsystem,
                    reason: "internal_dispatcher_did_not_recognize_registry_id",
                    missing_field: None,
                });
            }
            Err(DispatchError::Mutate(me)) => return Err(DispatchError::Mutate(me)),
        }
    }

    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::errno::Errno;

    #[test]
    fn pid_probe_result_treats_permission_denied_as_alive() {
        assert!(pid_probe_result_is_alive(Ok(())));
        assert!(pid_probe_result_is_alive(Err(Errno::EPERM)));
        assert!(!pid_probe_result_is_alive(Err(Errno::ESRCH)));
    }

    #[test]
    fn is_pid_alive_rejects_posix_special_or_unrepresentable_values() {
        assert!(!is_pid_alive(0));
        assert!(!is_pid_alive(u32::MAX));
    }

    #[test]
    fn registry_is_non_empty_and_alphabetically_sorted() {
        // Pass-14: every FM-level fixer must register a FixerSpec.
        let r = registry();
        assert!(r.len() >= 12, "registry has fewer fixers than expected");
        // Alphabetical sort by id helps `am doctor fixers` produce
        // stable output (operators rely on this for diffing).
        let ids: Vec<&str> = r.iter().map(|s| s.id).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(
            ids, sorted,
            "registry entries must be alphabetically sorted by id"
        );
    }

    #[test]
    fn registry_entries_use_canonical_op_patterns() {
        // Op patterns must be one of the 7 canonical variants OR detect-only.
        let allowed: &[&str] = &[
            "Op::WriteFile",
            "Op::AppendFile",
            "Op::Rename",
            "Op::Chmod",
            "Op::DbExec",
            "Op::DbMigrate",
            "Op::SymlinkAtomic",
            "detect-only",
        ];
        for spec in registry() {
            assert!(
                allowed.contains(&spec.op_pattern),
                "fixer {} has non-canonical op_pattern {}",
                spec.id,
                spec.op_pattern,
            );
            assert!(
                ["P0", "P1", "P2", "P3"].contains(&spec.severity),
                "fixer {} has non-canonical severity {}",
                spec.id,
                spec.severity,
            );
            // Detect-only fixers must have auto_fixable=false; all others
            // must have auto_fixable=true.
            let expected = spec.op_pattern != "detect-only";
            assert_eq!(
                spec.auto_fixable, expected,
                "fixer {} auto_fixable={} but op_pattern={}",
                spec.id, spec.auto_fixable, spec.op_pattern,
            );
        }
    }

    #[test]
    fn registry_serializes_to_json() {
        let r = registry();
        let s = serde_json::to_string_pretty(&r).expect("serialize");
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert!(v.is_array());
        let first = &v[0];
        assert!(first.get("id").is_some());
        assert!(first.get("severity").is_some());
        assert!(first.get("op_pattern").is_some());
        assert!(first.get("auto_fixable").is_some());
    }
}
