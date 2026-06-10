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

pub mod agent_profile_anomalies;
pub mod am_git_binary_missing;
pub mod archive_db_drift_anomalies;
pub mod archive_identity_artifact_mismatches;
pub mod archive_loose_object_bloat;
pub mod archive_message_artifact_anomalies;
pub mod archive_message_dir_structure_anomalies;
pub mod codex_startup_timeout;
pub mod committed_env_file_in_repo;
pub mod dangling_doctor_latest;
pub mod duplicate_canonical_message_ids;
pub mod empty_or_truncated_db;
pub mod guard_chain_runner_missing_or_stale;
pub mod guard_foreign_runner_overwrite;
pub mod guard_hooks_path_divergence;
pub mod guard_plugin_not_executable;
pub mod guard_plugin_symlink_replacement;
pub mod identity_build_slot_lease_expired;
pub mod inbox_stats_divergence;
pub mod integrity_page_malformed;
pub mod jwt_enabled_without_keys;
pub mod known_bad_git_no_override;
pub mod legacy_fts_residue;
pub mod login_shell_path_leak;
pub mod malformed_message_frontmatter;
pub mod mcp_duplicate_aliased_server_entries;
pub mod missing_gitignore_entry;
pub mod missing_head_or_broken_git_shape;
pub mod missing_or_malformed_project_json;
pub mod orphan_foreign_key_rows;
pub mod path_order_shadows_am;
pub mod port_bound_by_foreign_process;
pub mod quarantined_bak_files;
pub mod reservation_db_archive_parity;
pub mod retained_autocommit_leak;
pub mod runtime_pid_hint_symlink_toctou;
pub mod schema_version_mismatch;
pub mod share_half_finished_bundle;
pub mod share_scrub_manifest_mismatch;
pub mod share_verify_live_failed_deploy;
pub mod sqlite_sidecar_symlink;
pub mod stale_am_git_binary_cache;
pub mod stale_archive_lock;
pub mod stale_bearer_token_skew;
pub mod stale_head_or_ref_lock;
pub mod stale_listener_pid_hint;
pub mod stale_python_launcher_entry;
pub mod stale_python_server_shadow;
pub mod suspicious_ephemeral_archive_root;
pub mod text_timestamp_contamination;
pub mod unexpected_symlink_in_archive;
pub mod wal_mode_disabled;
pub mod wal_shm_sidecar_drift;
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
#[cfg(unix)]
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

/// Windows PID-liveness probe.
///
/// The real Win32 probe — `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION,
/// …)` + `GetExitCodeProcess` (alive iff exit code == `STILL_ACTIVE`) —
/// requires `unsafe` FFI. This crate is built under a crate-root
/// `#![forbid(unsafe_code)]`, which an inner `#[allow(unsafe_code)]`
/// cannot downgrade (`E0453`), so we cannot call those Win32 entry points
/// here.
///
/// We therefore fall back to a *conservative* answer: assume the PID is
/// alive. Every caller uses `is_pid_alive` to decide whether a recorded
/// PID still owns a lock / pid-hint before reclaiming it; answering `true`
/// means the doctor will NEVER reclaim a lock out from under a process it
/// cannot positively confirm is dead. The cost is that a genuinely-stale
/// lock left by a crashed Windows process won't be auto-cleaned by these
/// FMs — strictly the safe direction (no data loss, no live-process
/// disruption). PID 0 is still rejected to match the Unix guard.
#[cfg(not(unix))]
pub(crate) fn is_pid_alive(pid: u32) -> bool {
    pid != 0
}

/// Canonical Rust-binary path used by the `stale_python_launcher_entry`
/// detector to recognize "correctly-configured" entries vs. legacy
/// Python launchers. Defaults to `~/.local/bin/mcp-agent-mail`,
/// matching the installer's canonical location. Tests can override
/// by constructing `DetectInputs` directly.
pub(crate) fn default_rust_binary_path() -> std::path::PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".local").join("bin").join("mcp-agent-mail"))
        .unwrap_or_else(|| std::path::PathBuf::from("/usr/local/bin/mcp-agent-mail"))
}

/// For the `sqlite_sidecar_symlink` detector: expand each main-DB
/// candidate into a tagged `Candidate` triple (main + -wal + -shm).
/// Pass-35-review Codex F3 / Gemini F3 (P2): the role is now
/// passed explicitly, so an operator whose main DB filename
/// happens to end with `-wal`/`-shm` doesn't get misclassified.
pub(crate) fn expand_db_candidates_with_sidecars(
    db_paths: &[std::path::PathBuf],
) -> Vec<sqlite_sidecar_symlink::Candidate> {
    use sqlite_sidecar_symlink::Candidate;
    let mut out = Vec::with_capacity(db_paths.len() * 3);
    for db in db_paths {
        out.push(Candidate::main_db(db.clone()));
        if let Some(parent) = db.parent()
            && let Some(name) = db.file_name()
        {
            let base = name.to_string_lossy();
            out.push(Candidate::wal(parent.join(format!("{base}-wal"))));
            out.push(Candidate::shm(parent.join(format!("{base}-shm"))));
        }
    }
    out
}

/// Build a SQLite URI filename for a read-only immutable probe.
///
/// SQLite parses `?` and `#` as URI delimiters when `SQLITE_OPEN_URI`
/// is set, so candidate DB paths must be percent-encoded before
/// appending `?immutable=1`. Leaving a raw path here causes doctor DB
/// detectors to silently skip perfectly valid mailbox roots whose path
/// contains URI metacharacters.
pub(crate) fn sqlite_immutable_uri(db_path: &std::path::Path) -> String {
    let mut uri = String::from("file:");
    for byte in path_bytes_for_sqlite_uri(db_path) {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'.' | b'-' | b'_' | b'~' | b':' => {
                uri.push(byte as char)
            }
            other => {
                use std::fmt::Write as _;
                write!(&mut uri, "%{other:02X}").expect("writing to String cannot fail");
            }
        }
    }
    uri.push_str("?immutable=1");
    uri
}

/// Open a SQLite database for detector-only inspection without creating
/// sidecars, replaying WAL/journals, taking writer locks, or creating a
/// missing file.
///
/// SQLite's plain read-only mode can still create or require `-shm` files for
/// WAL databases. The URI `immutable=1` flag tells SQLite the file cannot
/// change under this connection, which is the contract doctor detectors need:
/// observe bytes and schema, never perturb the state being diagnosed.
#[allow(clippy::result_large_err)]
pub(crate) fn open_immutable_sqlite(
    db_path: &std::path::Path,
) -> sqlmodel_core::Result<sqlmodel_sqlite::SqliteConnection> {
    let uri = sqlite_immutable_uri(db_path);
    let mut flags = sqlmodel_sqlite::OpenFlags::read_only();
    flags.uri = true;
    let config = sqlmodel_sqlite::SqliteConfig::file(uri).flags(flags);
    sqlmodel_sqlite::SqliteConnection::open(&config)
}

#[cfg(unix)]
fn path_bytes_for_sqlite_uri(path: &std::path::Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_bytes_for_sqlite_uri(path: &std::path::Path) -> Vec<u8> {
    path.to_string_lossy().replace('\\', "/").into_bytes()
}

#[cfg(unix)]
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
/// 3. Add matching `dispatch_only` and `detect_only` branches below so
///    `am doctor fix --only <fm-id>` and `am doctor fix --list` can
///    actually run the detector.
pub fn registry() -> Vec<FixerSpec> {
    vec![
        FixerSpec {
            id: agent_profile_anomalies::FM_ID,
            severity: "P1",
            subsystem: "archive_state_files",
            op_pattern: "detect-only",
            auto_fixable: false,
            one_line_description: "Agent profile dirs are orphaned (parent project missing/unrecognized) OR have missing/unparseable `profile.json` (manual triage)",
            source_module: "doctor::fixers::agent_profile_anomalies",
        },
        FixerSpec {
            id: archive_db_drift_anomalies::FM_ID,
            severity: "P1",
            subsystem: "archive_state_files",
            op_pattern: "detect-only",
            auto_fixable: false,
            one_line_description: "Archive-vs-DB drift: archive project identities with no DB row, OR archive message-count diverges from DB count (manual: pick authoritative side; reconstruct DB from archive or restore/rebuild archive from backup/DB evidence)",
            source_module: "doctor::fixers::archive_db_drift_anomalies",
        },
        FixerSpec {
            id: archive_identity_artifact_mismatches::FM_ID,
            severity: "P1",
            subsystem: "archive_state_files",
            op_pattern: "detect-only",
            auto_fixable: false,
            one_line_description: "DB rows for agent profiles / file reservations reference archive artifacts that are missing or disagree with DB fields (pre-commit guard impact; manual: pick authoritative side; reconstruct DB from archive or restore/rebuild artifacts)",
            source_module: "doctor::fixers::archive_identity_artifact_mismatches",
        },
        FixerSpec {
            id: archive_message_artifact_anomalies::FM_ID,
            severity: "P1",
            subsystem: "archive_state_files",
            op_pattern: "detect-only",
            auto_fixable: false,
            one_line_description: "DB rows reference canonical message files / per-agent mailbox copies that are missing or out of sync on disk (data-loss signal; manual: pick authoritative side; reconstruct DB from archive or restore/rebuild archive artifacts)",
            source_module: "doctor::fixers::archive_message_artifact_anomalies",
        },
        FixerSpec {
            id: archive_message_dir_structure_anomalies::FM_ID,
            severity: "P2",
            subsystem: "archive_state_files",
            op_pattern: "detect-only",
            auto_fixable: false,
            one_line_description: "Archive `messages/YYYY/MM/` tree has invalid date directories OR non-.md files (breaks FTS V3 + archive replay; manual: rename/quarantine)",
            source_module: "doctor::fixers::archive_message_dir_structure_anomalies",
        },
        FixerSpec {
            id: duplicate_canonical_message_ids::FM_ID,
            severity: "P0",
            subsystem: "archive_state_files",
            op_pattern: "detect-only",
            auto_fixable: false,
            one_line_description: "Two or more archive .md files resolve to the same positive message_id — breaks thread reconstruction + ack accounting (manual triage + quarantine)",
            source_module: "doctor::fixers::duplicate_canonical_message_ids",
        },
        FixerSpec {
            id: archive_loose_object_bloat::FM_ID,
            severity: "P3",
            subsystem: "archive_state_files",
            op_pattern: "detect-only",
            auto_fixable: false,
            one_line_description: "Per-project git archives have excessive loose objects or no pack files (performance/inode pressure; manual: run `am doctor pack-archive`)",
            source_module: "doctor::fixers::archive_loose_object_bloat",
        },
        FixerSpec {
            id: malformed_message_frontmatter::FM_ID,
            severity: "P1",
            subsystem: "archive_state_files",
            op_pattern: "detect-only",
            auto_fixable: false,
            one_line_description: "Archive .md files with missing / unparseable / invalid-id / incomplete JSON frontmatter — breaks canonical-id mapping + FTS V3 indexing (manual: inspect, fix, reparse)",
            source_module: "doctor::fixers::malformed_message_frontmatter",
        },
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
            id: missing_head_or_broken_git_shape::FM_ID,
            severity: "P0",
            subsystem: "archive_state_files",
            op_pattern: "detect-only",
            auto_fixable: false,
            one_line_description: "One or more `<storage_root>/projects/<slug>/.git/HEAD` files are missing / empty / symlinked / dangling — archive replay broken; manual: `am doctor reconstruct`",
            source_module: "doctor::fixers::missing_head_or_broken_git_shape",
        },
        FixerSpec {
            id: missing_or_malformed_project_json::FM_ID,
            severity: "P1",
            subsystem: "archive_state_files",
            op_pattern: "Op::WriteFile",
            auto_fixable: true,
            one_line_description: "Mailbox archive contains projects whose `project.json` is missing OR has malformed JSON / missing required fields — partial auto-fix rewrites Invalid entries when DB-aware scan supplied `canonical_human_key`; Missing entries + Invalid-without-canonical stay manual (operator-supplied truth)",
            source_module: "doctor::fixers::missing_or_malformed_project_json",
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
            id: suspicious_ephemeral_archive_root::FM_ID,
            severity: "P3",
            subsystem: "archive_state_files",
            op_pattern: "detect-only",
            auto_fixable: false,
            one_line_description: "Mailbox archive contains project entries rooted at ephemeral paths (/tmp, /var/tmp, tmp-XXXX) — leakage from test runs; manual review + archive-normalize",
            source_module: "doctor::fixers::suspicious_ephemeral_archive_root",
        },
        FixerSpec {
            id: unexpected_symlink_in_archive::FM_ID,
            severity: "P1",
            subsystem: "archive_state_files",
            op_pattern: "Op::Rename",
            auto_fixable: true,
            one_line_description: "Mailbox archive contains unexpected symlinks (possible filesystem tampering / migration artifact) — auto-fix quarantines each via symlink-aware Op::Rename (moved, never dereferenced; removes exfil vector, preserves link for forensics; reversible via `am doctor undo`)",
            source_module: "doctor::fixers::unexpected_symlink_in_archive",
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
            id: inbox_stats_divergence::FM_ID,
            severity: "P1",
            subsystem: "db_state_files",
            op_pattern: "Op::DbExec",
            auto_fixable: true,
            one_line_description: "inbox_stats materialized aggregate drifts from ground-truth message_recipients counts (rebuild via Op::DbExec)",
            source_module: "doctor::fixers::inbox_stats_divergence",
        },
        FixerSpec {
            id: integrity_page_malformed::FM_ID,
            severity: "P0",
            subsystem: "db_state_files",
            op_pattern: "detect-only",
            auto_fixable: false,
            one_line_description: "PRAGMA integrity_check reports malformed pages (slow check; opt-in via --only; recovery via `am doctor reconstruct`)",
            source_module: "doctor::fixers::integrity_page_malformed",
        },
        FixerSpec {
            id: legacy_fts_residue::FM_ID,
            severity: "P2",
            subsystem: "db_state_files",
            op_pattern: "Op::DbExec",
            auto_fixable: true,
            one_line_description: "storage.sqlite3 retains legacy FTS5 tables/triggers/views after Search V3 migration — auto-fix drops them via Op::DbExec in dependency order (TRIGGER → VIEW → TABLE); reversible via `am doctor undo` (whole-DB-file backup)",
            source_module: "doctor::fixers::legacy_fts_residue",
        },
        FixerSpec {
            id: orphan_foreign_key_rows::FM_ID,
            severity: "P1",
            subsystem: "db_state_files",
            op_pattern: "Op::DbExec",
            auto_fixable: true,
            one_line_description: "PRAGMA foreign_key_check reports orphan child rows (stale file_reservations/file_reservation_releases auto-fix via DbExec quarantine; message history preserved)",
            source_module: "doctor::fixers::orphan_foreign_key_rows",
        },
        FixerSpec {
            id: reservation_db_archive_parity::FM_ID,
            severity: "P1",
            subsystem: "db_state_files",
            op_pattern: "detect-only",
            auto_fixable: false,
            one_line_description: "File reservation SQLite rows and stable archive JSON artifacts disagree on holder, released_ts, active status, or thread provenance (pre-commit guard over/under-block risk; manual reconcile until per-field mutate repair is implemented)",
            source_module: "doctor::fixers::reservation_db_archive_parity",
        },
        FixerSpec {
            id: retained_autocommit_leak::FM_ID,
            severity: "P0",
            subsystem: "db_state_files",
            op_pattern: "detect-only",
            auto_fixable: false,
            one_line_description: "pool init SQL is missing `PRAGMA autocommit_retain = OFF` (durable visibility silently degraded; rebuild required)",
            source_module: "doctor::fixers::retained_autocommit_leak",
        },
        FixerSpec {
            id: schema_version_mismatch::FM_ID,
            // Severity is dynamic (P0 for ForwardMigrate, P1 for
            // Newer); the registry surface picks the higher
            // (P0) as the documented baseline. Findings still
            // carry the precise per-direction severity.
            severity: "P0",
            subsystem: "db_state_files",
            op_pattern: "detect-only",
            auto_fixable: false,
            one_line_description: "DB user_version != compiled SCHEMA_VERSION (forward migration via `am serve` restart; newer-on-disk requires binary upgrade)",
            source_module: "doctor::fixers::schema_version_mismatch",
        },
        FixerSpec {
            id: sqlite_sidecar_symlink::FM_ID,
            severity: "P0",
            subsystem: "db_state_files",
            op_pattern: "detect-only",
            auto_fixable: false,
            one_line_description: "SQLite -wal/-shm sidecar is a symlink (WAL writes redirected; manual quarantine)",
            source_module: "doctor::fixers::sqlite_sidecar_symlink",
        },
        FixerSpec {
            id: text_timestamp_contamination::FM_ID,
            severity: "P0",
            subsystem: "db_state_files",
            op_pattern: "detect-only",
            auto_fixable: false,
            one_line_description: "TEXT-typed timestamps from Python writer poison i64 columns (boot migration / reconstruct required)",
            source_module: "doctor::fixers::text_timestamp_contamination",
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
            id: wal_shm_sidecar_drift::FM_ID,
            severity: "P0",
            subsystem: "db_state_files",
            op_pattern: "detect-only",
            auto_fixable: false,
            one_line_description: "WAL/SHM sidecars drift from main DB (asymmetric, header-only, stale, or boot-quarantine pile-up)",
            source_module: "doctor::fixers::wal_shm_sidecar_drift",
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
            id: am_git_binary_missing::FM_ID,
            severity: "P0",
            subsystem: "environment_toolchain",
            op_pattern: "detect-only",
            auto_fixable: false,
            one_line_description: "AM_GIT_BINARY override points at a missing / non-file / non-executable path",
            source_module: "doctor::fixers::am_git_binary_missing",
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
            id: login_shell_path_leak::FM_ID,
            severity: "P2",
            subsystem: "environment_toolchain",
            op_pattern: "detect-only",
            auto_fixable: false,
            one_line_description: "$HOME/.local/bin missing from PATH in non-interactive / login shell contexts (am works in terminal but fails for cron, ssh, systemd; manual shell rc edit)",
            source_module: "doctor::fixers::login_shell_path_leak",
        },
        FixerSpec {
            id: path_order_shadows_am::FM_ID,
            severity: "P1",
            subsystem: "environment_toolchain",
            op_pattern: "detect-only",
            auto_fixable: false,
            one_line_description: "Multiple distinct `am` binaries on PATH (first-match wins; ensure ~/.local/bin precedes others)",
            source_module: "doctor::fixers::path_order_shadows_am",
        },
        FixerSpec {
            id: stale_am_git_binary_cache::FM_ID,
            severity: "P2",
            subsystem: "environment_toolchain",
            op_pattern: "detect-only",
            auto_fixable: false,
            one_line_description: "Cached git binary path/SHA drifted from live disk state (binary swapped after cache validation; manual: restart serve or wait 24h TTL)",
            source_module: "doctor::fixers::stale_am_git_binary_cache",
        },
        FixerSpec {
            id: guard_chain_runner_missing_or_stale::FM_ID,
            severity: "P1",
            subsystem: "guard_install",
            op_pattern: "detect-only",
            auto_fixable: false,
            one_line_description: "Agent-mail plugin is installed at `hooks.d/<hook>/` but the chain runner is missing OR lacks our sentinel — `git commit` silently bypasses the guard; manual: re-run `am install-precommit-guard` (auto-fix needs the installer's render fn pub'd)",
            source_module: "doctor::fixers::guard_chain_runner_missing_or_stale",
        },
        FixerSpec {
            id: guard_foreign_runner_overwrite::FM_ID,
            severity: "P0",
            subsystem: "guard_install",
            op_pattern: "detect-only",
            auto_fixable: false,
            one_line_description: "Foreign hook manager (husky / lefthook / pre-commit-framework) clobbered the agent-mail chain runner at the active pre-commit — guard is COMPLETELY absent from `git commit`; auto-fix via Op::Rename foreign-aside + re-install deferred",
            source_module: "doctor::fixers::guard_foreign_runner_overwrite",
        },
        FixerSpec {
            id: guard_hooks_path_divergence::FM_ID,
            severity: "P1",
            subsystem: "guard_install",
            op_pattern: "detect-only",
            auto_fixable: false,
            one_line_description: "Agent-mail chain-runner is installed at a hooks dir that is NOT the currently active dir (`core.hooksPath` changed after install) — `git commit` silently bypasses the guard; auto-fix via Op::Rename to quarantine deferred",
            source_module: "doctor::fixers::guard_hooks_path_divergence",
        },
        FixerSpec {
            id: guard_plugin_not_executable::FM_ID,
            severity: "P1",
            subsystem: "guard_install",
            op_pattern: "Op::Chmod",
            auto_fixable: true,
            one_line_description: "Pre-commit / pre-push guard hook(s) lack user-exec bit (POSIX): `git commit` silently bypasses the agent-mail guard. Auto-fix chmods each entry to 0o755 via the chokepoint; reversible via `am doctor undo`",
            source_module: "doctor::fixers::guard_plugin_not_executable",
        },
        FixerSpec {
            id: guard_plugin_symlink_replacement::FM_ID,
            severity: "P1",
            subsystem: "guard_install",
            op_pattern: "detect-only",
            auto_fixable: false,
            one_line_description: "Agent-mail guard path(s) under the hooks dir are symlinks (installer always writes regular files) — possible filesystem tampering / attacker aliasing; auto-fix via Op::Rename symlink-aside + reinstall deferred",
            source_module: "doctor::fixers::guard_plugin_symlink_replacement",
        },
        FixerSpec {
            id: identity_build_slot_lease_expired::FM_ID,
            severity: "P2",
            subsystem: "identity_contacts_state",
            op_pattern: "Op::WriteFile",
            auto_fixable: true,
            one_line_description: "Build-slot lease JSON files are expired but unreleased, leaving ghost occupied slots — auto-fix rewrites `released_ts` to `now_iso` via Op::WriteFile through the chokepoint (UPDATE-only — never deletes; reversible via `am doctor undo`)",
            source_module: "doctor::fixers::identity_build_slot_lease_expired",
        },
        FixerSpec {
            id: codex_startup_timeout::FM_ID,
            severity: "P1",
            subsystem: "mcp_config_files",
            op_pattern: "Op::WriteFile",
            auto_fixable: true,
            one_line_description: "Codex config.toml missing or too-short startup_timeout_sec (boot races mcp-agent-mail cold start) — auto-fix sets startup_timeout_sec on the existing mcp_agent_mail entry via format-preserving toml_edit; entry-absent stays manual (am setup)",
            source_module: "doctor::fixers::codex_startup_timeout",
        },
        FixerSpec {
            id: mcp_duplicate_aliased_server_entries::FM_ID,
            severity: "P2",
            subsystem: "mcp_config_files",
            op_pattern: "Op::WriteFile",
            auto_fixable: true,
            one_line_description: "JSON/JSONC MCP config files contain more than one agent-mail server entry across supported container/alias keys — partial auto-fix rewrites strict `.json` configs when canonical `(mcpServers, mcp-agent-mail)` is present; `.jsonc`/`.json5` configs and canonical-missing configs stay manual",
            source_module: "doctor::fixers::mcp_duplicate_aliased_server_entries",
        },
        FixerSpec {
            id: quarantined_bak_files::FM_ID,
            severity: "P1",
            subsystem: "mcp_config_files",
            op_pattern: "Op::Chmod",
            auto_fixable: true,
            one_line_description: "Timestamped MCP config backups (`*.<YYYYMMDD>_<HHMMSS>.bak`) with token-shape content + world/group-readable mode — auto-fix chmods each to 0o600 via the chokepoint (defense-in-depth; reversible via `am doctor undo`)",
            source_module: "doctor::fixers::quarantined_bak_files",
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
            id: stale_python_launcher_entry::FM_ID,
            severity: "P0",
            subsystem: "mcp_config_files",
            op_pattern: "Op::WriteFile",
            auto_fixable: true,
            one_line_description: "MCP client config still uses Python launcher for mcp_agent_mail — auto-fix swaps the launcher command to the canonical Rust binary (stdio preserved) in `.json`/`.toml` configs; `.jsonc`/`.json5` stay manual",
            source_module: "doctor::fixers::stale_python_launcher_entry",
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
            id: runtime_pid_hint_symlink_toctou::FM_ID,
            severity: "P1",
            subsystem: "runtime_processes",
            op_pattern: "detect-only",
            auto_fixable: false,
            one_line_description: "listener.pid or its parent directory shape exposes symlink/permission TOCTOU signals (manual security triage)",
            source_module: "doctor::fixers::runtime_pid_hint_symlink_toctou",
        },
        FixerSpec {
            id: port_bound_by_foreign_process::FM_ID,
            severity: "P0",
            subsystem: "runtime_processes",
            op_pattern: "detect-only",
            auto_fixable: false,
            one_line_description: "Configured HTTP_HOST:HTTP_PORT is held by a foreign process (am serve-http would fail to bind)",
            source_module: "doctor::fixers::port_bound_by_foreign_process",
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
            id: stale_python_server_shadow::FM_ID,
            severity: "P1",
            subsystem: "runtime_processes",
            op_pattern: "detect-only",
            auto_fixable: false,
            one_line_description: "listener.pid PID is a live Python interpreter (detect-only — auto-fixing the lock would cause the race it prevents; manual `kill <pid>` required)",
            source_module: "doctor::fixers::stale_python_server_shadow",
        },
        FixerSpec {
            id: committed_env_file_in_repo::FM_ID,
            severity: "P0",
            subsystem: "secrets_env_state",
            op_pattern: "Op::Chmod",
            auto_fixable: true,
            one_line_description: "Token-shape .env file committed to git OR present untracked with wide permissions (tracked lane is detect-only; untracked lane chmods to 0o600)",
            source_module: "doctor::fixers::committed_env_file_in_repo",
        },
        FixerSpec {
            id: jwt_enabled_without_keys::FM_ID,
            severity: "P0",
            subsystem: "secrets_env_state",
            op_pattern: "detect-only",
            auto_fixable: false,
            one_line_description: "HTTP_JWT_ENABLED=true but JWT verifier keys are missing or algorithm family is wrong (every request 401s)",
            source_module: "doctor::fixers::jwt_enabled_without_keys",
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
        FixerSpec {
            id: share_half_finished_bundle::FM_ID,
            severity: "P1",
            subsystem: "share_export_state",
            op_pattern: "Op::Rename",
            auto_fixable: true,
            one_line_description: "Share-export temp dirs or partial bundles remain after a crash — auto-fix quarantines each debris directory via directory Op::Rename into `<run-dir>/quarantine/share-debris/` (never deletes; reversible via `am doctor undo`)",
            source_module: "doctor::fixers::share_half_finished_bundle",
        },
        FixerSpec {
            id: share_scrub_manifest_mismatch::FM_ID,
            severity: "P1",
            subsystem: "share_export_state",
            op_pattern: "detect-only",
            auto_fixable: false,
            one_line_description: "Published share bundle manifest attachment counts disagree with on-disk attachments (manual: re-export, do not patch counts in place)",
            source_module: "doctor::fixers::share_scrub_manifest_mismatch",
        },
        FixerSpec {
            id: share_verify_live_failed_deploy::FM_ID,
            severity: "P2",
            subsystem: "share_export_state",
            op_pattern: "detect-only",
            auto_fixable: false,
            one_line_description: "Share bundle has stale verify-live failure report (manual: re-run verify-live and redeploy or roll back remote target)",
            source_module: "doctor::fixers::share_verify_live_failed_deploy",
        },
    ]
}

/// Inputs to `dispatch_only`. Each FM module pulls only the fields it
/// needs — `dispatch_only` is a `match` on FM id, not a trait, because
/// the concrete fixers have heterogeneous input shapes and a premature
/// trait would just bury the differences.
#[derive(Debug, Clone)]
pub struct DispatchInputs {
    /// Repository root (used as a default scope-anchor and for default
    /// glob expansion).
    pub repo_root: std::path::PathBuf,
    /// `<storage_root>/projects/*/` archive roots for stale-lock scans.
    /// Caller is responsible for enumerating; an empty slice short-circuits
    /// the relevant FMs to "no findings."
    pub archive_roots: Vec<std::path::PathBuf>,
    /// The mailbox storage root (typically
    /// `~/.mcp_agent_mail_git_mailbox_repo/` or the value of
    /// `STORAGE_ROOT`). Used by
    /// `suspicious_ephemeral_archive_root` which feeds this path
    /// directly to `scan_archive_anomalies(root)` — the helper
    /// appends `projects/` internally, so passing a project-dir
    /// (an entry from `archive_roots`) would make the scanner
    /// look in `<project_dir>/projects/` and silently find
    /// nothing. `None` skips the FM (pass-35AA review F1 fix).
    pub storage_root: Option<std::path::PathBuf>,
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
    /// Inputs for the AM_GIT_BINARY-points-at-missing-file detect-only
    /// FM. `None` skips the FM.
    pub am_git_binary_detect: Option<am_git_binary_missing::DetectInputs>,
    /// JWT config inputs for the
    /// `jwt_enabled_without_keys` FM. `None` skips it.
    pub jwt_detect: Option<jwt_enabled_without_keys::DetectInputs>,
    /// Inputs for the port-bind probe FM
    /// (`port_bound_by_foreign_process`). `None` skips the FM.
    pub port_bind_probe: Option<port_bound_by_foreign_process::DetectInputs>,
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
    /// Test-only override for the
    /// `missing_or_malformed_project_json` FM. `None` (the
    /// production default) makes the dispatcher build inputs
    /// from `storage_root` + `db_aware_archive_report`. Tests
    /// pass `Some(DetectInputs { report_override: Some(synthetic_report), .. })`
    /// to inject a hand-built `ArchiveAnomalyReport` so the
    /// round-trip test doesn't need a real DB + archive.
    ///
    /// Note: unlike most `Option<DetectInputs>` fields on
    /// `DispatchInputs`, `None` here does NOT skip the FM — the
    /// dispatcher falls back to the production-default inputs.
    /// The override is purely a test-injection hook.
    pub missing_project_json_detect_override:
        Option<missing_or_malformed_project_json::DetectInputs>,
    /// Inputs for the quarantined-bak-files FM
    /// (`quarantined_bak_files`). `None` skips the FM
    /// (`MissingInput`). Production callers pass
    /// `Some(DetectInputs::default())` to invoke the canonical
    /// MCP-config-dir walk via
    /// `mcp_config::detect_mcp_config_locations_default()`. Tests
    /// pass `Some(DetectInputs { dir_overrides: Some(...) })` to
    /// scope the walk to a tempdir.
    pub quarantined_bak_detect: Option<quarantined_bak_files::DetectInputs>,
}

fn db_aware_archive_report(
    inputs: &DispatchInputs,
) -> Option<mcp_agent_mail_db::archive_anomaly::ArchiveAnomalyReport> {
    let storage_root = inputs.storage_root.as_ref()?;
    let db_path = inputs.db_file_candidates.first()?;
    Some(mcp_agent_mail_db::archive_anomaly::scan_archive_anomalies_with_db(storage_root, db_path))
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
    } else if fm_id == suspicious_ephemeral_archive_root::FM_ID {
        // Use the dedicated `storage_root` DispatchInputs field
        // (pass-35AA review F1, Codex). archive_roots entries
        // are per-project directories — `scan_archive_anomalies`
        // appends `projects/` internally so feeding it a
        // project-dir made it scan the wrong subtree silently.
        let se_inputs = suspicious_ephemeral_archive_root::DetectInputs {
            storage_root_override: inputs.storage_root.clone(),
            report_override: None,
        };
        let findings = suspicious_ephemeral_archive_root::detect(&se_inputs);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = suspicious_ephemeral_archive_root::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == missing_head_or_broken_git_shape::FM_ID {
        let findings = missing_head_or_broken_git_shape::detect(&inputs.archive_roots);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = missing_head_or_broken_git_shape::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == agent_profile_anomalies::FM_ID {
        let ap_inputs = agent_profile_anomalies::DetectInputs {
            storage_root_override: inputs.storage_root.clone(),
            report_override: None,
        };
        let findings = agent_profile_anomalies::detect(&ap_inputs);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = agent_profile_anomalies::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == archive_db_drift_anomalies::FM_ID {
        let ad_inputs = archive_db_drift_anomalies::DetectInputs {
            storage_root_override: inputs.storage_root.clone(),
            report_override: db_aware_archive_report(inputs),
        };
        let findings = archive_db_drift_anomalies::detect(&ad_inputs);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = archive_db_drift_anomalies::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == archive_identity_artifact_mismatches::FM_ID {
        let aiam_inputs = archive_identity_artifact_mismatches::DetectInputs {
            storage_root_override: inputs.storage_root.clone(),
            report_override: db_aware_archive_report(inputs),
        };
        let findings = archive_identity_artifact_mismatches::detect(&aiam_inputs);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = archive_identity_artifact_mismatches::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == archive_message_artifact_anomalies::FM_ID {
        let ama_inputs = archive_message_artifact_anomalies::DetectInputs {
            storage_root_override: inputs.storage_root.clone(),
            report_override: db_aware_archive_report(inputs),
        };
        let findings = archive_message_artifact_anomalies::detect(&ama_inputs);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = archive_message_artifact_anomalies::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == archive_message_dir_structure_anomalies::FM_ID {
        let amds_inputs = archive_message_dir_structure_anomalies::DetectInputs {
            storage_root_override: inputs.storage_root.clone(),
            report_override: None,
        };
        let findings = archive_message_dir_structure_anomalies::detect(&amds_inputs);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = archive_message_dir_structure_anomalies::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == duplicate_canonical_message_ids::FM_ID {
        let dc_inputs = duplicate_canonical_message_ids::DetectInputs {
            storage_root_override: inputs.storage_root.clone(),
            report_override: None,
        };
        let findings = duplicate_canonical_message_ids::detect(&dc_inputs);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = duplicate_canonical_message_ids::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == archive_loose_object_bloat::FM_ID {
        let findings =
            archive_loose_object_bloat::detect(&inputs.archive_roots, &Default::default());
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = archive_loose_object_bloat::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == malformed_message_frontmatter::FM_ID {
        let mf_inputs = malformed_message_frontmatter::DetectInputs {
            storage_root_override: inputs.storage_root.clone(),
            report_override: None,
        };
        let findings = malformed_message_frontmatter::detect(&mf_inputs);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = malformed_message_frontmatter::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == missing_or_malformed_project_json::FM_ID {
        // Use the DB-aware archive scan when both storage_root and
        // a DB path are available — this populates
        // `InvalidProjectMetadata::canonical_human_key` from the
        // DB row, which is the source of truth needed for any
        // future auto-fix reconstruction of `project.json`. Falls
        // back to the archive-only scan when no DB is configured
        // (e.g., an offline mailbox inspection).
        //
        // `missing_project_json_detect_override` is the test-only
        // injection point that lets the round-trip test supply a
        // synthetic `ArchiveAnomalyReport` without needing a real
        // DB + archive.
        let mp_inputs = inputs
            .missing_project_json_detect_override
            .clone()
            .unwrap_or_else(|| missing_or_malformed_project_json::DetectInputs {
                storage_root_override: inputs.storage_root.clone(),
                report_override: db_aware_archive_report(inputs),
            });
        let findings = missing_or_malformed_project_json::detect(&mp_inputs);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = missing_or_malformed_project_json::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == unexpected_symlink_in_archive::FM_ID {
        let us_inputs = unexpected_symlink_in_archive::DetectInputs {
            storage_root_override: inputs.storage_root.clone(),
            report_override: None,
        };
        let findings = unexpected_symlink_in_archive::detect(&us_inputs);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = unexpected_symlink_in_archive::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
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
    } else if fm_id == port_bound_by_foreign_process::FM_ID {
        let port_inputs = inputs
            .port_bind_probe
            .as_ref()
            .ok_or(DispatchError::MissingInput {
                fm_id: port_bound_by_foreign_process::FM_ID,
                field: "port_bind_probe",
            })?;
        let findings = port_bound_by_foreign_process::detect(port_inputs);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = port_bound_by_foreign_process::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == stale_python_server_shadow::FM_ID {
        let findings = stale_python_server_shadow::detect(&inputs.pid_hint_candidates);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = stale_python_server_shadow::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
            outcome.quarantined_paths.extend(result.quarantined_paths);
        }
    } else if fm_id == jwt_enabled_without_keys::FM_ID {
        let jwt_inputs = inputs
            .jwt_detect
            .as_ref()
            .ok_or(DispatchError::MissingInput {
                fm_id: jwt_enabled_without_keys::FM_ID,
                field: "jwt_detect",
            })?;
        let findings = jwt_enabled_without_keys::detect(jwt_inputs);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = jwt_enabled_without_keys::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == committed_env_file_in_repo::FM_ID {
        let findings = committed_env_file_in_repo::detect(&inputs.repo_root);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = committed_env_file_in_repo::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == guard_chain_runner_missing_or_stale::FM_ID {
        let findings = guard_chain_runner_missing_or_stale::detect(&inputs.repo_root);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            // Detect-only — fix is a no-op.
            let result = guard_chain_runner_missing_or_stale::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == guard_foreign_runner_overwrite::FM_ID {
        let findings = guard_foreign_runner_overwrite::detect(&inputs.repo_root);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            // Detect-only — fix is a no-op.
            let result = guard_foreign_runner_overwrite::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == guard_hooks_path_divergence::FM_ID {
        let findings = guard_hooks_path_divergence::detect(&inputs.repo_root);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            // Detect-only — fix is a no-op.
            let result = guard_hooks_path_divergence::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == guard_plugin_not_executable::FM_ID {
        let findings = guard_plugin_not_executable::detect(&inputs.repo_root);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            // Detect-only — fix is a no-op.
            let result = guard_plugin_not_executable::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == guard_plugin_symlink_replacement::FM_ID {
        let findings = guard_plugin_symlink_replacement::detect(&inputs.repo_root);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            // Detect-only — fix is a no-op.
            let result = guard_plugin_symlink_replacement::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == identity_build_slot_lease_expired::FM_ID {
        let findings = identity_build_slot_lease_expired::detect(
            inputs.storage_root.as_deref(),
            &Default::default(),
        );
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = identity_build_slot_lease_expired::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
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
    } else if fm_id == path_order_shadows_am::FM_ID {
        // Reads PATH from process env; no DispatchInputs field
        // needed.
        let pa_inputs = path_order_shadows_am::DetectInputs::default();
        let findings = path_order_shadows_am::detect(&pa_inputs);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = path_order_shadows_am::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == login_shell_path_leak::FM_ID {
        // Reads dirs::home_dir() + spawns shell subprocesses;
        // no DispatchInputs field needed for production.
        let ls_inputs = login_shell_path_leak::DetectInputs::default();
        let findings = login_shell_path_leak::detect(&ls_inputs);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = login_shell_path_leak::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == stale_am_git_binary_cache::FM_ID {
        // Reads the process-wide git_binary cache via
        // peek_cached_resolution(); no DispatchInputs field
        // needed for production.
        let sg_inputs = stale_am_git_binary_cache::DetectInputs::default();
        let findings = stale_am_git_binary_cache::detect(&sg_inputs);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = stale_am_git_binary_cache::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == am_git_binary_missing::FM_ID {
        let am_inputs =
            inputs
                .am_git_binary_detect
                .as_ref()
                .ok_or(DispatchError::MissingInput {
                    fm_id: am_git_binary_missing::FM_ID,
                    field: "am_git_binary_detect",
                })?;
        let findings = am_git_binary_missing::detect(am_inputs);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            // Detect-only: fix is a no-op.
            let result = am_git_binary_missing::fix(ctx, f)?;
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
    } else if fm_id == sqlite_sidecar_symlink::FM_ID {
        // Expand each db candidate into its sidecar paths so the
        // detector lstats `storage.sqlite3`, `storage.sqlite3-wal`,
        // and `storage.sqlite3-shm` together. Same db_file_candidates
        // wiring that world_readable_storage_db uses.
        let paths = expand_db_candidates_with_sidecars(&inputs.db_file_candidates);
        let findings = sqlite_sidecar_symlink::detect(&paths);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = sqlite_sidecar_symlink::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
            outcome.quarantined_paths.extend(result.quarantined_paths);
        }
    } else if fm_id == schema_version_mismatch::FM_ID {
        let findings = schema_version_mismatch::detect(&inputs.db_file_candidates);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = schema_version_mismatch::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == inbox_stats_divergence::FM_ID {
        let findings = inbox_stats_divergence::detect(&inputs.db_file_candidates);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = inbox_stats_divergence::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == integrity_page_malformed::FM_ID {
        let findings = integrity_page_malformed::detect(&inputs.db_file_candidates);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = integrity_page_malformed::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == legacy_fts_residue::FM_ID {
        let findings = legacy_fts_residue::detect(&inputs.db_file_candidates);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            // Auto-fix via Op::DbExec: dependency-ordered DROP of
            // the residual fts_* objects. Reversible via the
            // chokepoint's whole-DB-file backup.
            let result = legacy_fts_residue::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == orphan_foreign_key_rows::FM_ID {
        let findings = orphan_foreign_key_rows::detect(&inputs.db_file_candidates);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            // Auto-fix is intentionally narrow: stale file_reservations
            // and release-ledger sidecars are quarantined via DbExec;
            // message-recipient history remains detect-only inside the FM.
            let result = orphan_foreign_key_rows::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == reservation_db_archive_parity::FM_ID {
        let findings = reservation_db_archive_parity::detect(
            inputs.storage_root.as_deref(),
            &inputs.db_file_candidates,
        );
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = reservation_db_archive_parity::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == retained_autocommit_leak::FM_ID {
        // Inspects mcp_agent_mail_db::schema constants; no
        // DispatchInputs field needed for production.
        let rl_inputs = retained_autocommit_leak::DetectInputs::default();
        let findings = retained_autocommit_leak::detect(&rl_inputs);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            // Detect-only — fix is a no-op.
            let result = retained_autocommit_leak::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == codex_startup_timeout::FM_ID {
        // `detect_mcp_config_locations_default` is a pure helper
        // that reads no env state beyond `dirs::home_dir()` + CWD;
        // we don't need a dedicated DispatchInputs field.
        let locations = mcp_agent_mail_core::mcp_config::detect_mcp_config_locations_default();
        let findings = codex_startup_timeout::detect(&locations);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = codex_startup_timeout::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == mcp_duplicate_aliased_server_entries::FM_ID {
        let findings = mcp_duplicate_aliased_server_entries::detect(&inputs.mcp_config_candidates);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = mcp_duplicate_aliased_server_entries::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == quarantined_bak_files::FM_ID {
        // `None` skips the FM via `MissingInput`. Production
        // callers pass `Some(DetectInputs::default())` to invoke
        // the canonical MCP-config-dir walk; tests inject
        // `Some(DetectInputs { dir_overrides: Some(...) })` to
        // scope the walk to a tempdir.
        let qb_inputs =
            inputs
                .quarantined_bak_detect
                .as_ref()
                .ok_or(DispatchError::MissingInput {
                    fm_id: quarantined_bak_files::FM_ID,
                    field: "quarantined_bak_detect",
                })?;
        let findings = quarantined_bak_files::detect(qb_inputs);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = quarantined_bak_files::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == stale_python_launcher_entry::FM_ID {
        let locations = mcp_agent_mail_core::mcp_config::detect_mcp_config_locations_default();
        let inputs = stale_python_launcher_entry::DetectInputs {
            locations,
            rust_binary_path: default_rust_binary_path(),
        };
        let findings = stale_python_launcher_entry::detect(&inputs);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = stale_python_launcher_entry::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == text_timestamp_contamination::FM_ID {
        let findings = text_timestamp_contamination::detect(&inputs.db_file_candidates);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            // Detect-only — fix is a no-op.
            let result = text_timestamp_contamination::fix(ctx, f)?;
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
    } else if fm_id == wal_shm_sidecar_drift::FM_ID {
        let inputs = wal_shm_sidecar_drift::DetectInputs::new(inputs.db_file_candidates.clone());
        let findings = wal_shm_sidecar_drift::detect(&inputs);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = wal_shm_sidecar_drift::fix(ctx, f)?;
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
    } else if fm_id == runtime_pid_hint_symlink_toctou::FM_ID {
        let findings = runtime_pid_hint_symlink_toctou::detect(&inputs.pid_hint_candidates);
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = runtime_pid_hint_symlink_toctou::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == stale_bearer_token_skew::FM_ID {
        let canonical =
            inputs
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
    } else if fm_id == share_half_finished_bundle::FM_ID {
        let findings =
            share_half_finished_bundle::detect(Some(&inputs.repo_root), &Default::default());
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = share_half_finished_bundle::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == share_scrub_manifest_mismatch::FM_ID {
        let findings = share_scrub_manifest_mismatch::detect(Some(&inputs.repo_root));
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = share_scrub_manifest_mismatch::fix(ctx, f)?;
            outcome.actions_taken += result.actions_taken;
            outcome.actions_skipped += result.actions_skipped;
        }
    } else if fm_id == share_verify_live_failed_deploy::FM_ID {
        let findings =
            share_verify_live_failed_deploy::detect(Some(&inputs.repo_root), &Default::default());
        outcome.findings_count = findings.len();
        for f in &findings {
            outcome.findings.push(f.to_finding());
            let result = share_verify_live_failed_deploy::fix(ctx, f)?;
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
    } else if fm_id == suspicious_ephemeral_archive_root::FM_ID {
        let se_inputs = suspicious_ephemeral_archive_root::DetectInputs {
            storage_root_override: inputs.storage_root.clone(),
            report_override: None,
        };
        suspicious_ephemeral_archive_root::detect(&se_inputs)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == missing_head_or_broken_git_shape::FM_ID {
        missing_head_or_broken_git_shape::detect(&inputs.archive_roots)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == agent_profile_anomalies::FM_ID {
        let ap_inputs = agent_profile_anomalies::DetectInputs {
            storage_root_override: inputs.storage_root.clone(),
            report_override: None,
        };
        agent_profile_anomalies::detect(&ap_inputs)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == archive_db_drift_anomalies::FM_ID {
        let ad_inputs = archive_db_drift_anomalies::DetectInputs {
            storage_root_override: inputs.storage_root.clone(),
            report_override: db_aware_archive_report(inputs),
        };
        archive_db_drift_anomalies::detect(&ad_inputs)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == archive_identity_artifact_mismatches::FM_ID {
        let aiam_inputs = archive_identity_artifact_mismatches::DetectInputs {
            storage_root_override: inputs.storage_root.clone(),
            report_override: db_aware_archive_report(inputs),
        };
        archive_identity_artifact_mismatches::detect(&aiam_inputs)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == archive_message_artifact_anomalies::FM_ID {
        let ama_inputs = archive_message_artifact_anomalies::DetectInputs {
            storage_root_override: inputs.storage_root.clone(),
            report_override: db_aware_archive_report(inputs),
        };
        archive_message_artifact_anomalies::detect(&ama_inputs)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == archive_message_dir_structure_anomalies::FM_ID {
        let amds_inputs = archive_message_dir_structure_anomalies::DetectInputs {
            storage_root_override: inputs.storage_root.clone(),
            report_override: None,
        };
        archive_message_dir_structure_anomalies::detect(&amds_inputs)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == duplicate_canonical_message_ids::FM_ID {
        let dc_inputs = duplicate_canonical_message_ids::DetectInputs {
            storage_root_override: inputs.storage_root.clone(),
            report_override: None,
        };
        duplicate_canonical_message_ids::detect(&dc_inputs)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == archive_loose_object_bloat::FM_ID {
        archive_loose_object_bloat::detect(&inputs.archive_roots, &Default::default())
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == malformed_message_frontmatter::FM_ID {
        let mf_inputs = malformed_message_frontmatter::DetectInputs {
            storage_root_override: inputs.storage_root.clone(),
            report_override: None,
        };
        malformed_message_frontmatter::detect(&mf_inputs)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == missing_or_malformed_project_json::FM_ID {
        let mp_inputs = inputs
            .missing_project_json_detect_override
            .clone()
            .unwrap_or_else(|| missing_or_malformed_project_json::DetectInputs {
                storage_root_override: inputs.storage_root.clone(),
                report_override: db_aware_archive_report(inputs),
            });
        missing_or_malformed_project_json::detect(&mp_inputs)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == unexpected_symlink_in_archive::FM_ID {
        let us_inputs = unexpected_symlink_in_archive::DetectInputs {
            storage_root_override: inputs.storage_root.clone(),
            report_override: None,
        };
        unexpected_symlink_in_archive::detect(&us_inputs)
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
    } else if fm_id == port_bound_by_foreign_process::FM_ID {
        let port_inputs = inputs
            .port_bind_probe
            .as_ref()
            .ok_or(DispatchError::MissingInput {
                fm_id: port_bound_by_foreign_process::FM_ID,
                field: "port_bind_probe",
            })?;
        port_bound_by_foreign_process::detect(port_inputs)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == stale_python_server_shadow::FM_ID {
        stale_python_server_shadow::detect(&inputs.pid_hint_candidates)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == jwt_enabled_without_keys::FM_ID {
        let jwt_inputs = inputs
            .jwt_detect
            .as_ref()
            .ok_or(DispatchError::MissingInput {
                fm_id: jwt_enabled_without_keys::FM_ID,
                field: "jwt_detect",
            })?;
        jwt_enabled_without_keys::detect(jwt_inputs)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == committed_env_file_in_repo::FM_ID {
        committed_env_file_in_repo::detect(&inputs.repo_root)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == guard_chain_runner_missing_or_stale::FM_ID {
        guard_chain_runner_missing_or_stale::detect(&inputs.repo_root)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == guard_foreign_runner_overwrite::FM_ID {
        guard_foreign_runner_overwrite::detect(&inputs.repo_root)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == guard_hooks_path_divergence::FM_ID {
        guard_hooks_path_divergence::detect(&inputs.repo_root)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == guard_plugin_not_executable::FM_ID {
        guard_plugin_not_executable::detect(&inputs.repo_root)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == guard_plugin_symlink_replacement::FM_ID {
        guard_plugin_symlink_replacement::detect(&inputs.repo_root)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == identity_build_slot_lease_expired::FM_ID {
        identity_build_slot_lease_expired::detect(
            inputs.storage_root.as_deref(),
            &Default::default(),
        )
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
    } else if fm_id == path_order_shadows_am::FM_ID {
        path_order_shadows_am::detect(&path_order_shadows_am::DetectInputs::default())
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == login_shell_path_leak::FM_ID {
        login_shell_path_leak::detect(&login_shell_path_leak::DetectInputs::default())
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == stale_am_git_binary_cache::FM_ID {
        stale_am_git_binary_cache::detect(&stale_am_git_binary_cache::DetectInputs::default())
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == am_git_binary_missing::FM_ID {
        let am_inputs =
            inputs
                .am_git_binary_detect
                .as_ref()
                .ok_or(DispatchError::MissingInput {
                    fm_id: am_git_binary_missing::FM_ID,
                    field: "am_git_binary_detect",
                })?;
        am_git_binary_missing::detect(am_inputs)
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
    } else if fm_id == sqlite_sidecar_symlink::FM_ID {
        let paths = expand_db_candidates_with_sidecars(&inputs.db_file_candidates);
        sqlite_sidecar_symlink::detect(&paths)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == schema_version_mismatch::FM_ID {
        schema_version_mismatch::detect(&inputs.db_file_candidates)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == inbox_stats_divergence::FM_ID {
        inbox_stats_divergence::detect(&inputs.db_file_candidates)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == integrity_page_malformed::FM_ID {
        integrity_page_malformed::detect(&inputs.db_file_candidates)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == legacy_fts_residue::FM_ID {
        legacy_fts_residue::detect(&inputs.db_file_candidates)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == orphan_foreign_key_rows::FM_ID {
        orphan_foreign_key_rows::detect(&inputs.db_file_candidates)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == reservation_db_archive_parity::FM_ID {
        reservation_db_archive_parity::detect(
            inputs.storage_root.as_deref(),
            &inputs.db_file_candidates,
        )
        .iter()
        .map(|f| f.to_finding())
        .collect()
    } else if fm_id == retained_autocommit_leak::FM_ID {
        retained_autocommit_leak::detect(&retained_autocommit_leak::DetectInputs::default())
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == codex_startup_timeout::FM_ID {
        let locations = mcp_agent_mail_core::mcp_config::detect_mcp_config_locations_default();
        codex_startup_timeout::detect(&locations)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == mcp_duplicate_aliased_server_entries::FM_ID {
        mcp_duplicate_aliased_server_entries::detect(&inputs.mcp_config_candidates)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == quarantined_bak_files::FM_ID {
        let qb_inputs =
            inputs
                .quarantined_bak_detect
                .as_ref()
                .ok_or(DispatchError::MissingInput {
                    fm_id: quarantined_bak_files::FM_ID,
                    field: "quarantined_bak_detect",
                })?;
        quarantined_bak_files::detect(qb_inputs)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == stale_python_launcher_entry::FM_ID {
        let locations = mcp_agent_mail_core::mcp_config::detect_mcp_config_locations_default();
        let inputs = stale_python_launcher_entry::DetectInputs {
            locations,
            rust_binary_path: default_rust_binary_path(),
        };
        stale_python_launcher_entry::detect(&inputs)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == text_timestamp_contamination::FM_ID {
        text_timestamp_contamination::detect(&inputs.db_file_candidates)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == wal_mode_disabled::FM_ID {
        wal_mode_disabled::detect(&inputs.db_file_candidates)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == wal_shm_sidecar_drift::FM_ID {
        let detect_inputs =
            wal_shm_sidecar_drift::DetectInputs::new(inputs.db_file_candidates.clone());
        wal_shm_sidecar_drift::detect(&detect_inputs)
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
    } else if fm_id == runtime_pid_hint_symlink_toctou::FM_ID {
        runtime_pid_hint_symlink_toctou::detect(&inputs.pid_hint_candidates)
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == stale_bearer_token_skew::FM_ID {
        let canonical =
            inputs
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
    } else if fm_id == share_half_finished_bundle::FM_ID {
        share_half_finished_bundle::detect(Some(&inputs.repo_root), &Default::default())
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == share_scrub_manifest_mismatch::FM_ID {
        share_scrub_manifest_mismatch::detect(Some(&inputs.repo_root))
            .iter()
            .map(|f| f.to_finding())
            .collect()
    } else if fm_id == share_verify_live_failed_deploy::FM_ID {
        share_verify_live_failed_deploy::detect(Some(&inputs.repo_root), &Default::default())
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
    use sqlmodel_sqlite::SqliteConnection;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn sqlite_immutable_uri_escapes_uri_delimiters_in_paths() {
        let uri = sqlite_immutable_uri(std::path::Path::new("/tmp/agent mail?#%/storage.sqlite3"));
        assert_eq!(
            uri,
            "file:/tmp/agent%20mail%3F%23%25/storage.sqlite3?immutable=1"
        );
    }

    #[test]
    fn open_immutable_sqlite_does_not_create_wal_sidecars() {
        let td = TempDir::new().unwrap();
        let db = td.path().join("storage.sqlite3");
        let wal = td.path().join("storage.sqlite3-wal");
        let shm = td.path().join("storage.sqlite3-shm");

        let seed = SqliteConnection::open_file(db.to_string_lossy().into_owned()).unwrap();
        seed.execute_raw("PRAGMA journal_mode=WAL;").unwrap();
        seed.execute_raw("CREATE TABLE t (id INTEGER PRIMARY KEY);")
            .unwrap();
        seed.execute_raw("INSERT INTO t (id) VALUES (1);").unwrap();
        seed.execute_raw("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        drop(seed);

        let before_dir = sorted_dir_entries(td.path());
        let before_wal = wal.exists();
        let before_shm = shm.exists();
        let conn = open_immutable_sqlite(&db).expect("immutable open");
        let rows = conn
            .query_sync("SELECT COUNT(*) AS n FROM t", &[])
            .expect("immutable read");
        let n = rows
            .first()
            .and_then(|row| row.get_named::<i64>("n").ok())
            .expect("count row");
        drop(conn);

        assert_eq!(n, 1);
        assert_eq!(
            before_dir,
            sorted_dir_entries(td.path()),
            "immutable detector open must not create sibling files"
        );
        assert_eq!(
            before_wal,
            wal.exists(),
            "immutable detector open must not create or remove WAL sidecars"
        );
        assert_eq!(
            before_shm,
            shm.exists(),
            "immutable detector open must not create or remove SHM sidecars"
        );
    }

    fn sorted_dir_entries(path: &std::path::Path) -> Vec<String> {
        let mut entries: Vec<String> = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        entries
    }

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
        assert!(r.len() >= 13, "registry has fewer fixers than expected");
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
    fn every_registry_entry_has_detect_and_dispatch_arms() {
        let temp = tempfile::tempdir().expect("tempdir");
        let inputs = DispatchInputs {
            repo_root: temp.path().to_path_buf(),
            archive_roots: Vec::new(),
            storage_root: Some(temp.path().to_path_buf()),
            pid_hint_candidates: Vec::new(),
            token_backup_candidates: Vec::new(),
            mcp_config_candidates: Vec::new(),
            canonical_mcp_url: None,
            canonical_bearer_token: None,
            git_detect: None,
            am_git_binary_detect: None,
            jwt_detect: None,
            port_bind_probe: None,
            gitignore_target: None,
            db_file_candidates: Vec::new(),
            doctor_latest_target: None,
            stale_seconds_override: None,
            missing_project_json_detect_override: None,
            quarantined_bak_detect: None,
        };
        let run_dir =
            crate::doctor::runs::scaffold_run_dir(temp.path(), "test_run").expect("run dir");
        let actions = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join("actions.jsonl"))
            .expect("actions file");
        let ctx = crate::doctor::mutate::MutateContext {
            run_id: "test_run".into(),
            run_dir,
            capabilities: crate::doctor::mutate::Capabilities {
                write_scopes: vec![temp.path().to_path_buf()],
            },
            actions_file: std::sync::Mutex::new(actions),
            fixer_id: "registry-arm-test".into(),
            repo_root: temp.path().to_path_buf(),
            dry_run: true,
            start: std::time::Instant::now(),
            extra_locks: Vec::new(),
        };

        for spec in registry() {
            match detect_only(spec.id, &inputs) {
                Ok(_) | Err(DispatchError::MissingInput { .. }) => {}
                Err(DispatchError::UnknownFm(id)) => {
                    panic!("registry id {id} is missing a detect_only branch")
                }
                Err(DispatchError::Mutate(err)) => {
                    panic!(
                        "detect_only for {} unexpectedly reached mutate: {err}",
                        spec.id
                    )
                }
            }
            match dispatch_only(spec.id, &ctx, &inputs) {
                Ok(_) | Err(DispatchError::MissingInput { .. }) => {}
                Err(DispatchError::UnknownFm(id)) => {
                    panic!("registry id {id} is missing a dispatch_only branch")
                }
                Err(DispatchError::Mutate(err)) => {
                    panic!(
                        "dispatch_only for {} unexpectedly reached mutate: {err}",
                        spec.id
                    )
                }
            }
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
