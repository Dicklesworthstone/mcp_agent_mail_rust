//! Legacy Python installation detection and migration/import commands.
//!
//! Command surface:
//! - `am legacy detect`
//! - `am legacy import`
//! - `am legacy status`
//! - `am upgrade`

#![forbid(unsafe_code)]

use crate::{CliError, CliResult, SetupCommand, handle_setup, output};
use chrono::Utc;
use clap::{Args, Subcommand};
use mcp_agent_mail_core::Config;
use mcp_agent_mail_core::config::{read_env_authority_text, user_env_authority_candidates};
use mcp_agent_mail_core::disk::{
    is_sqlite_memory_database_url, sqlite_file_path_from_database_url,
};
use mcp_agent_mail_db::schema;
use mcp_agent_mail_db::{CanonicalDbConn, DbConn};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

#[derive(Args, Debug)]
pub struct LegacyArgs {
    #[command(subcommand)]
    pub action: LegacyCommand,
}

#[derive(Subcommand, Debug)]
pub enum LegacyCommand {
    /// Detect legacy Python installation markers and likely data locations.
    Detect {
        /// Root directory to inspect (default: current directory).
        #[arg(long)]
        search_root: Option<PathBuf>,
        /// Output format: table, json, or toon.
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Import/migrate a legacy Python installation into Rust-native schema.
    Import {
        /// Auto-discover legacy paths using marker detection + precedence rules.
        #[arg(long, default_value_t = false)]
        auto: bool,
        /// Root directory to inspect for `.env` and legacy markers.
        #[arg(long)]
        search_root: Option<PathBuf>,
        /// Explicit source sqlite database path.
        #[arg(long)]
        db: Option<PathBuf>,
        /// Explicit source storage root path.
        #[arg(long)]
        storage_root: Option<PathBuf>,
        /// Optional target DB path. Imports always migrate a new copy.
        #[arg(long)]
        target_db: Option<PathBuf>,
        /// Optional target storage root. Imports always migrate a new copy.
        #[arg(long)]
        target_storage_root: Option<PathBuf>,
        /// Show planned operations without making any changes.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        /// Skip interactive confirmation prompt.
        #[arg(long, default_value_t = false)]
        yes: bool,
        /// Output format: table, json, or toon.
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Show status/history of legacy import receipts.
    Status {
        /// Root directory used for env precedence.
        #[arg(long)]
        search_root: Option<PathBuf>,
        /// Explicit storage root (where receipts are stored).
        #[arg(long)]
        storage_root: Option<PathBuf>,
        /// Output format: table, json, or toon.
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

#[derive(Args, Debug)]
pub struct UpgradeArgs {
    /// Root directory to inspect for legacy markers and env files.
    #[arg(long)]
    pub search_root: Option<PathBuf>,
    /// Show operations without making changes.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
    /// Skip interactive confirmation prompt.
    #[arg(long, default_value_t = false)]
    pub yes: bool,
    /// Output format: table, json, or toon.
    #[arg(long, value_parser)]
    pub format: Option<output::CliOutputFormat>,
    /// Output JSON (shorthand for --format json).
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ConfidenceLevel {
    None,
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum MarkerSeverity {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyMarker {
    id: String,
    severity: MarkerSeverity,
    detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ResolvedSource {
    Explicit,
    ProcessEnv,
    ProjectEnv,
    UserEnv,
    Default,
}

impl ResolvedSource {
    const fn label(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::ProcessEnv => "env",
            Self::ProjectEnv => ".env",
            Self::UserEnv => "user-env",
            Self::Default => "default",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ResolvedPathInfo {
    path: String,
    source: ResolvedSource,
    exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    raw_value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct ResolvedPath {
    path: PathBuf,
    source: ResolvedSource,
    exists: bool,
    raw_value: Option<String>,
}

#[derive(Debug, Clone)]
struct EnvAuthoritySnapshot {
    path: PathBuf,
    values: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default)]
struct LegacyEnvSnapshot {
    project: Option<EnvAuthoritySnapshot>,
    user: Option<EnvAuthoritySnapshot>,
}

#[derive(Debug, Clone)]
struct ResolvedLegacyAuthorities {
    database: ResolvedPath,
    storage: ResolvedPath,
    env: LegacyEnvSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyDbSignature {
    open_ok: bool,
    core_tables_present: bool,
    legacy_trigger_count: usize,
    datetime_like_column_count: usize,
    migrations_table_present: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyDetectReport {
    search_root: String,
    detected: bool,
    confidence: ConfidenceLevel,
    score: u32,
    database: ResolvedPathInfo,
    storage_root: ResolvedPathInfo,
    markers: Vec<LegacyMarker>,
    #[serde(skip_serializing_if = "Option::is_none")]
    db_signature: Option<LegacyDbSignature>,
    recommended_action: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ImportMode {
    Copy,
}

#[derive(Debug)]
struct ImportPlan {
    mode: ImportMode,
    search_root: PathBuf,
    source_db: PathBuf,
    source_snapshot: Option<LegacySourceSnapshot>,
    source_storage_root: PathBuf,
    target_db: PathBuf,
    target_storage_root: PathBuf,
    operations: Vec<String>,
}

#[derive(Debug)]
struct LegacySourceSnapshot {
    original_path: PathBuf,
    snapshot_path: PathBuf,
    _snapshot_dir: tempfile::TempDir,
}

impl LegacySourceSnapshot {
    fn capture(original_path: &Path) -> CliResult<Self> {
        let snapshot_dir =
            crate::canonical_snapshot_tempdir("legacy-source-snapshot-", "legacy source capture")?;
        let snapshot_path = snapshot_dir.path().join("source.sqlite3");
        materialize_legacy_source_snapshot(original_path, &snapshot_path)?;
        Ok(Self {
            original_path: original_path.to_path_buf(),
            snapshot_path,
            _snapshot_dir: snapshot_dir,
        })
    }

    fn original_path(&self) -> &Path {
        &self.original_path
    }

    fn snapshot_path(&self) -> &Path {
        &self.snapshot_path
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum LegacyImportMailboxLockKind {
    StorageRoot,
    Sqlite,
}

#[derive(Debug)]
struct LegacyImportMailboxLocks {
    _guards: Vec<mcp_agent_mail_server::MailboxActivityLockGuard>,
}

fn legacy_import_lock_specs(plan: &ImportPlan) -> Vec<(LegacyImportMailboxLockKind, PathBuf)> {
    let mut specs = Vec::new();

    // The source is strictly read-only: creating activity-lock metadata beside
    // it would violate that contract. Fresh targets also remain lock-free until
    // the import creates them, avoiding stale lock artifacts on a failed run.
    if plan.target_storage_root.exists() {
        specs.push((
            LegacyImportMailboxLockKind::StorageRoot,
            plan.target_storage_root.clone(),
        ));
    }
    if plan.target_db.exists() {
        specs.push((LegacyImportMailboxLockKind::Sqlite, plan.target_db.clone()));
    }

    specs
}

fn acquire_legacy_import_mailbox_locks(plan: &ImportPlan) -> CliResult<LegacyImportMailboxLocks> {
    let mut specs = legacy_import_lock_specs(plan);
    specs.sort();
    specs.dedup();

    let mut guards = Vec::with_capacity(specs.len());
    for (kind, path) in specs {
        let guard = match kind {
            LegacyImportMailboxLockKind::StorageRoot => {
                crate::acquire_cli_mailbox_activity_lock_for_storage_root(
                    &path,
                    mcp_agent_mail_server::MailboxActivityLockMode::Exclusive,
                )?
            }
            LegacyImportMailboxLockKind::Sqlite => {
                crate::acquire_cli_mailbox_activity_lock_for_sqlite_path(
                    &path,
                    mcp_agent_mail_server::MailboxActivityLockMode::Exclusive,
                )?
            }
        };
        if let Some(guard) = guard {
            guards.push(guard);
        }
    }

    Ok(LegacyImportMailboxLocks { _guards: guards })
}

/// Current receipt schema version.
///
/// - v1: success-only receipts (no `outcome`/`failure_reason` fields).
/// - v2: adds `outcome` ("succeeded"/"failed") and `failure_reason` so failed
///   imports leave an auditable trail for `am legacy status`. The reader
///   tolerates v1 receipts by defaulting `outcome` to "succeeded".
const LEGACY_IMPORT_RECEIPT_VERSION: u32 = 2;

const LEGACY_IMPORT_OUTCOME_SUCCEEDED: &str = "succeeded";
const LEGACY_IMPORT_OUTCOME_FAILED: &str = "failed";

fn default_receipt_outcome() -> String {
    LEGACY_IMPORT_OUTCOME_SUCCEEDED.to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyImportReceipt {
    receipt_version: u32,
    /// "succeeded" or "failed". v1 receipts lack this field; default preserves
    /// their (success-only) semantics on read.
    #[serde(default = "default_receipt_outcome")]
    outcome: String,
    /// Present only on failure receipts: the error that aborted the import.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    failure_reason: Option<String>,
    created_at: String,
    mode: ImportMode,
    search_root: String,
    source_db: String,
    source_storage_root: String,
    target_db: String,
    target_storage_root: String,
    migrated_migration_ids: Vec<String>,
    integrity_check_ok: bool,
    core_table_counts: BTreeMap<String, i64>,
    setup_refresh_ok: bool,
    warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ImportDryRunReport {
    mode: ImportMode,
    search_root: String,
    source_db: String,
    source_storage_root: String,
    target_db: String,
    target_storage_root: String,
    operations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyStatusReport {
    storage_root: String,
    receipts_dir: String,
    receipt_count: usize,
    latest_receipt: Option<LegacyImportReceipt>,
}

#[derive(Debug, Clone, Serialize)]
struct UpgradeReport {
    search_root: String,
    legacy_detected: bool,
    confidence: ConfidenceLevel,
    action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    import_receipt: Option<LegacyImportReceipt>,
}

pub fn handle_legacy(args: LegacyArgs) -> CliResult<()> {
    match args.action {
        LegacyCommand::Detect {
            search_root,
            format,
            json,
        } => handle_legacy_detect(search_root, format, json),
        LegacyCommand::Import {
            auto,
            search_root,
            db,
            storage_root,
            target_db,
            target_storage_root,
            dry_run,
            yes,
            format,
            json,
        } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            let opts = ImportOptions {
                auto,
                search_root,
                db,
                storage_root,
                target_db,
                target_storage_root,
                dry_run,
                yes,
            };
            run_legacy_import(opts, fmt)
        }
        LegacyCommand::Status {
            search_root,
            storage_root,
            format,
            json,
        } => handle_legacy_status(search_root, storage_root, format, json),
    }
}

pub fn handle_upgrade(args: UpgradeArgs) -> CliResult<()> {
    let fmt = output::CliOutputFormat::resolve(args.format, args.json);
    let root = resolve_search_root(args.search_root);
    let detect = build_detect_report(&root, None, None)?;

    let mut report = UpgradeReport {
        search_root: root.display().to_string(),
        legacy_detected: detect.detected,
        confidence: detect.confidence,
        action: String::new(),
        import_receipt: None,
    };

    if !detect.detected {
        report.action = if args.dry_run {
            "dry-run: no legacy install detected; would run setup refresh".to_string()
        } else {
            run_setup_refresh_once(Some(root.clone()))?;
            "no legacy install detected; setup refresh completed".to_string()
        };
        output::emit_output(&report, fmt, || {
            ftui_runtime::ftui_println!("Upgrade summary");
            ftui_runtime::ftui_println!("- Search root: {}", report.search_root);
            ftui_runtime::ftui_println!("- Legacy detected: no");
            ftui_runtime::ftui_println!("- Action: {}", report.action);
        });
        return Ok(());
    }

    let import_opts = ImportOptions {
        auto: true,
        search_root: Some(root),
        db: None,
        storage_root: None,
        target_db: None,
        target_storage_root: None,
        dry_run: args.dry_run,
        yes: args.yes,
    };
    let plan = build_import_plan(&import_opts)?;

    if args.dry_run {
        report.action = "dry-run: legacy detected; would copy-import + setup refresh".into();
        output::emit_output(&report, fmt, || {
            ftui_runtime::ftui_println!("Upgrade summary");
            ftui_runtime::ftui_println!("- Search root: {}", report.search_root);
            ftui_runtime::ftui_println!("- Legacy detected: yes ({:?})", report.confidence);
            for op in &plan.operations {
                ftui_runtime::ftui_println!("  - {op}");
            }
        });
        return Ok(());
    }

    if !import_opts.yes {
        if !crate::output::is_stdin_tty() {
            return Err(CliError::Other(
                "refusing to run non-interactively without --yes".to_string(),
            ));
        }
        if !confirm_with_prompt("Proceed with legacy import + upgrade?", false)? {
            return Err(CliError::ExitCode(1));
        }
    }

    let receipt = execute_import(plan, true)?;
    report.action = "legacy import completed and setup refresh attempted".to_string();
    report.import_receipt = Some(receipt);
    output::emit_output(&report, fmt, || {
        ftui_runtime::ftui_println!("Upgrade summary");
        ftui_runtime::ftui_println!("- Search root: {}", report.search_root);
        ftui_runtime::ftui_println!("- Legacy detected: yes ({:?})", report.confidence);
        ftui_runtime::ftui_println!("- Action: {}", report.action);
        if let Some(r) = &report.import_receipt {
            ftui_runtime::ftui_println!("- Receipt: {}", r.created_at);
            ftui_runtime::ftui_println!("- Target DB: {}", r.target_db);
            ftui_runtime::ftui_println!(
                "- Integrity: {}",
                if r.integrity_check_ok { "ok" } else { "failed" }
            );
        }
    });
    Ok(())
}

fn handle_legacy_detect(
    search_root: Option<PathBuf>,
    format: Option<output::CliOutputFormat>,
    json: bool,
) -> CliResult<()> {
    let fmt = output::CliOutputFormat::resolve(format, json);
    let root = resolve_search_root(search_root);
    let report = build_detect_report(&root, None, None)?;
    output::emit_output(&report, fmt, || {
        ftui_runtime::ftui_println!("Legacy detection report");
        ftui_runtime::ftui_println!("- Search root: {}", report.search_root);
        ftui_runtime::ftui_println!(
            "- Detected: {} ({:?}, score {})",
            if report.detected { "yes" } else { "no" },
            report.confidence,
            report.score
        );
        ftui_runtime::ftui_println!(
            "- Database: {} [{}] {}",
            report.database.path,
            report.database.source.label(),
            if report.database.exists {
                "(exists)"
            } else {
                "(missing)"
            }
        );
        ftui_runtime::ftui_println!(
            "- Storage root: {} [{}] {}",
            report.storage_root.path,
            report.storage_root.source.label(),
            if report.storage_root.exists {
                "(exists)"
            } else {
                "(missing)"
            }
        );
        if let Some(sig) = &report.db_signature {
            ftui_runtime::ftui_println!(
                "- DB signature: core_tables={} legacy_triggers={} datetime_cols={} migrations_table={}",
                sig.core_tables_present,
                sig.legacy_trigger_count,
                sig.datetime_like_column_count,
                sig.migrations_table_present
            );
        }
        if !report.markers.is_empty() {
            ftui_runtime::ftui_println!("- Markers:");
            for marker in &report.markers {
                let path = marker.path.clone().unwrap_or_else(|| "-".to_string());
                ftui_runtime::ftui_println!(
                    "  - [{}] {} ({path})",
                    format!("{:?}", marker.severity),
                    marker.detail
                );
            }
        }
        ftui_runtime::ftui_println!("- Recommended: {}", report.recommended_action);
    });
    Ok(())
}

fn handle_legacy_status(
    search_root: Option<PathBuf>,
    storage_root_override: Option<PathBuf>,
    format: Option<output::CliOutputFormat>,
    json: bool,
) -> CliResult<()> {
    let fmt = output::CliOutputFormat::resolve(format, json);
    let root = resolve_search_root(search_root);
    let storage = match storage_root_override {
        Some(path) => normalize_input_path(&path.to_string_lossy(), &root),
        None => resolve_storage_root(&root, None)?.path,
    };
    let report = collect_status_report(&storage)?;
    let receipts_dir = PathBuf::from(&report.receipts_dir);
    if report.receipt_count == 0 {
        output::emit_output(&report, fmt, || {
            ftui_runtime::ftui_println!(
                "No legacy import receipts found under {}.",
                receipts_dir.display()
            );
        });
        return Ok(());
    }
    output::emit_output(&report, fmt, || {
        ftui_runtime::ftui_println!("Legacy import status");
        ftui_runtime::ftui_println!("- Storage root: {}", report.storage_root);
        ftui_runtime::ftui_println!("- Receipts dir: {}", report.receipts_dir);
        ftui_runtime::ftui_println!("- Receipt count: {}", report.receipt_count);
        if let Some(latest) = &report.latest_receipt {
            ftui_runtime::ftui_println!("- Latest: {}", latest.created_at);
            ftui_runtime::ftui_println!("- Outcome: {}", latest.outcome);
            if let Some(reason) = &latest.failure_reason {
                ftui_runtime::ftui_println!("- Failure reason: {reason}");
            }
            ftui_runtime::ftui_println!("- Mode: {:?}", latest.mode);
            ftui_runtime::ftui_println!("- Target DB: {}", latest.target_db);
            ftui_runtime::ftui_println!(
                "- Integrity: {}",
                if latest.integrity_check_ok {
                    "ok"
                } else {
                    "failed"
                }
            );
        }
    });
    Ok(())
}

fn collect_status_report(storage: &Path) -> CliResult<LegacyStatusReport> {
    let receipts_dir = storage.join("legacy_import_receipts");
    if !receipts_dir.exists() {
        return Ok(LegacyStatusReport {
            storage_root: storage.display().to_string(),
            receipts_dir: receipts_dir.display().to_string(),
            receipt_count: 0,
            latest_receipt: None,
        });
    }

    let mut receipts: Vec<LegacyImportReceipt> = Vec::new();
    for entry in fs::read_dir(&receipts_dir)?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|v| v.to_str()) != Some("json") {
            continue;
        }
        let text = match fs::read_to_string(&path) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let parsed = match serde_json::from_str::<LegacyImportReceipt>(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        receipts.push(parsed);
    }
    receipts.sort_by(|a, b| b.created_at.cmp(&a.created_at));

    Ok(LegacyStatusReport {
        storage_root: storage.display().to_string(),
        receipts_dir: receipts_dir.display().to_string(),
        receipt_count: receipts.len(),
        latest_receipt: receipts.first().cloned(),
    })
}

#[derive(Debug, Clone)]
struct ImportOptions {
    auto: bool,
    search_root: Option<PathBuf>,
    db: Option<PathBuf>,
    storage_root: Option<PathBuf>,
    target_db: Option<PathBuf>,
    target_storage_root: Option<PathBuf>,
    dry_run: bool,
    yes: bool,
}

fn run_legacy_import(opts: ImportOptions, fmt: output::CliOutputFormat) -> CliResult<()> {
    let plan = build_import_plan(&opts)?;

    if opts.dry_run {
        let report = ImportDryRunReport {
            mode: plan.mode,
            search_root: plan.search_root.display().to_string(),
            source_db: plan.source_db.display().to_string(),
            source_storage_root: plan.source_storage_root.display().to_string(),
            target_db: plan.target_db.display().to_string(),
            target_storage_root: plan.target_storage_root.display().to_string(),
            operations: plan.operations.clone(),
        };
        output::emit_output(&report, fmt, || {
            ftui_runtime::ftui_println!("Legacy import dry-run");
            ftui_runtime::ftui_println!("- Mode: {:?}", report.mode);
            for op in &report.operations {
                ftui_runtime::ftui_println!("  - {op}");
            }
        });
        return Ok(());
    }

    if !opts.yes {
        if !crate::output::is_stdin_tty() {
            return Err(CliError::Other(
                "refusing to run non-interactively without --yes".to_string(),
            ));
        }
        if !confirm_with_prompt("Proceed with legacy import now?", false)? {
            return Err(CliError::ExitCode(1));
        }
    }

    let receipt = execute_import(plan, true)?;
    output::emit_output(&receipt, fmt, || {
        ftui_runtime::ftui_println!("Legacy import complete");
        ftui_runtime::ftui_println!("- Created at: {}", receipt.created_at);
        ftui_runtime::ftui_println!("- Mode: {:?}", receipt.mode);
        ftui_runtime::ftui_println!("- Target DB: {}", receipt.target_db);
        ftui_runtime::ftui_println!("- Target storage: {}", receipt.target_storage_root);
        ftui_runtime::ftui_println!(
            "- Integrity check: {}",
            if receipt.integrity_check_ok {
                "ok"
            } else {
                "failed"
            }
        );
        if !receipt.warnings.is_empty() {
            ftui_runtime::ftui_println!("- Warnings:");
            for warning in &receipt.warnings {
                ftui_runtime::ftui_println!("  - {warning}");
            }
        }
    });
    Ok(())
}

fn build_import_plan(opts: &ImportOptions) -> CliResult<ImportPlan> {
    let root = resolve_search_root(opts.search_root.clone());
    let resolved =
        resolve_legacy_authorities(&root, opts.db.as_deref(), opts.storage_root.as_deref())?;
    let source_db = resolved.database.path.clone();
    let source_storage = resolved.storage.path.clone();
    if !source_db.exists() {
        return Err(CliError::InvalidArgument(format!(
            "source DB missing: {}",
            source_db.display()
        )));
    }
    if !source_db.is_file() {
        return Err(CliError::InvalidArgument(format!(
            "source DB must be a file path: {}",
            source_db.display()
        )));
    }
    require_storage_directory(&source_storage, "source storage root", false)?;

    let mode = ImportMode::Copy;
    let target_db = opts
        .target_db
        .clone()
        .map(|v| normalize_input_path(&v.to_string_lossy(), &root))
        .unwrap_or_else(|| default_copy_target_db(&source_db));
    let target_storage = opts
        .target_storage_root
        .clone()
        .map(|v| normalize_input_path(&v.to_string_lossy(), &root))
        .unwrap_or_else(|| default_copy_target_storage(&source_storage));

    if source_db == target_db {
        return Err(CliError::InvalidArgument(
            "legacy import requires a target DB path different from source DB".to_string(),
        ));
    }
    if fs::symlink_metadata(&target_db).is_ok() {
        return Err(CliError::InvalidArgument(format!(
            "legacy import requires target DB path that does not already exist: {}",
            target_db.display()
        )));
    }
    if source_storage == target_storage {
        return Err(CliError::InvalidArgument(
            "legacy import requires target storage root different from source storage root"
                .to_string(),
        ));
    }
    require_storage_directory(&target_storage, "target storage root", true)?;
    if paths_overlap(&source_storage, &target_storage) {
        return Err(CliError::InvalidArgument(
            "legacy import requires target storage root to be outside source storage root"
                .to_string(),
        ));
    }

    // A real import captures one coherent source generation before the prompt
    // or any target mutation. Detection, both source checks, and the target
    // backup all consume this retained private image. Dry-run remains a pure
    // plan: its detector may inspect a short-lived snapshot, but the plan does
    // not retain or claim an executable source generation.
    let source_snapshot = if opts.dry_run {
        None
    } else {
        Some(LegacySourceSnapshot::capture(&source_db)?)
    };
    let detect = build_detect_report_from_resolved(&root, &resolved, source_snapshot.as_ref())?;
    if opts.auto && !detect.detected {
        return Err(CliError::InvalidArgument(
            "no legacy installation detected; run `am legacy detect` to inspect details"
                .to_string(),
        ));
    }

    let mut operations = Vec::new();
    operations.push(format!("resolve source DB: {}", source_db.display()));
    operations.push(format!(
        "resolve source storage root: {}",
        source_storage.display()
    ));
    operations.push(
        "capture one WAL-aware, transactionally coherent private source snapshot and use it for \
         legacy signature detection, both source checks, and target backup"
            .to_string(),
    );
    operations.push(format!(
        "copy source DB to target DB with a canonical SQLite online backup: {}",
        target_db.display()
    ));
    operations.push(format!(
        "copy source storage root to target storage root: {}",
        target_storage.display()
    ));
    operations.push("run schema::migrate_to_latest against target DB only".to_string());
    operations.push("verify source and target DB readability before recording success".to_string());
    operations.push("run target integrity_check and core-table sanity queries".to_string());
    operations.push("write JSON receipt under target storage root".to_string());
    operations.push("refresh agent MCP config via setup run".to_string());

    Ok(ImportPlan {
        mode,
        search_root: root,
        source_db,
        source_snapshot,
        source_storage_root: source_storage,
        target_db,
        target_storage_root: target_storage,
        operations,
    })
}

fn execute_import(plan: ImportPlan, should_refresh_setup: bool) -> CliResult<LegacyImportReceipt> {
    // Import only locks existing target paths. Source paths stay entirely
    // read-only, including during detection and the SQLite online backup.
    let _mailbox_locks = acquire_legacy_import_mailbox_locks(&plan)?;
    let now = Utc::now();
    let timestamp = now.format("%Y%m%dT%H%M%SZ").to_string();
    let source_snapshot = plan.source_snapshot.as_ref().ok_or_else(|| {
        CliError::Other(
            "legacy import execution requires the retained source snapshot omitted by dry-run"
                .to_string(),
        )
    })?;

    // Preflight before any target artifact exists: failures here return the
    // error unchanged (there is nothing to stage aside and no target storage
    // root that this run owns to hold a failure receipt).
    verify_source_canonical_sqlite_readable(source_snapshot)?;
    ensure_target_storage_root_usable(&plan.target_storage_root)?;

    match execute_import_body(&plan, should_refresh_setup, &now) {
        Ok(receipt) => {
            write_receipt(&plan.target_storage_root, &receipt, &timestamp)?;
            Ok(receipt)
        }
        Err(err) => Err(handle_failed_import(&plan, &err, &now, &timestamp)),
    }
}

/// Import steps that create/modify target artifacts. Any `Err` from here means
/// a partially created target may exist; `execute_import` stages it aside and
/// records a failure receipt so the run is auditable and retryable.
fn execute_import_body(
    plan: &ImportPlan,
    should_refresh_setup: bool,
    now: &chrono::DateTime<Utc>,
) -> CliResult<LegacyImportReceipt> {
    let mut warnings = Vec::new();
    let source_snapshot = plan.source_snapshot.as_ref().ok_or_else(|| {
        CliError::Other("legacy import body lost its retained source snapshot".to_string())
    })?;

    copy_db_via_sqlite_backup(source_snapshot, &plan.target_db)?;
    copy_dir_recursive(&plan.source_storage_root, &plan.target_storage_root)?;

    let migrated_ids = migrate_sqlite_db(&plan.target_db)?;
    let integrity_ok = integrity_check_ok(&plan.target_db)?;
    if !integrity_ok {
        return Err(CliError::Other(format!(
            "integrity_check failed after migration for {}",
            plan.target_db.display()
        )));
    }
    let core_counts = query_core_table_counts(&plan.target_db)?;
    verify_source_canonical_sqlite_readable(source_snapshot)?;
    verify_canonical_sqlite_readable(&plan.target_db, "target DB")?;
    verify_runtime_sqlite_readable(&plan.target_db, "target DB")?;

    let setup_ok = if should_refresh_setup {
        match run_setup_refresh_once(Some(plan.search_root.clone())) {
            Ok(()) => true,
            Err(err) => {
                warnings.push(format!("setup refresh failed: {err}"));
                false
            }
        }
    } else {
        true
    };

    Ok(LegacyImportReceipt {
        receipt_version: LEGACY_IMPORT_RECEIPT_VERSION,
        outcome: LEGACY_IMPORT_OUTCOME_SUCCEEDED.to_string(),
        failure_reason: None,
        created_at: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        mode: plan.mode,
        search_root: plan.search_root.display().to_string(),
        source_db: plan.source_db.display().to_string(),
        source_storage_root: plan.source_storage_root.display().to_string(),
        target_db: plan.target_db.display().to_string(),
        target_storage_root: plan.target_storage_root.display().to_string(),
        migrated_migration_ids: migrated_ids,
        integrity_check_ok: integrity_ok,
        core_table_counts: core_counts,
        setup_refresh_ok: setup_ok,
        warnings,
    })
}

/// A target storage root is usable when it does not exist, or contains at most
/// the `legacy_import_receipts` directory (left behind by a previous failed
/// attempt whose partial artifacts were staged aside). Anything else is
/// refused so an unrelated directory is never merged into.
fn ensure_target_storage_root_usable(target_storage_root: &Path) -> CliResult<()> {
    if !require_storage_directory(target_storage_root, "target storage root", true)? {
        return Ok(());
    }
    for entry in fs::read_dir(target_storage_root)? {
        let entry = entry?;
        if entry.file_name() == "legacy_import_receipts" {
            continue;
        }
        return Err(CliError::InvalidArgument(format!(
            "target storage root {} already exists and is not empty; choose a different path",
            target_storage_root.display()
        )));
    }
    Ok(())
}

/// Failure path for `execute_import`: stage the partially created target DB
/// (plus `-wal`/`-shm` sidecars) aside as `<target>.failed-<UTC ts>` siblings
/// so the original target path is free for a retry, write a failure receipt so
/// `am legacy status` can report the attempt, and return the original error
/// annotated with the staged and receipt paths.
///
/// Staging uses rename (never deletion), and only touches the target DB this
/// same run just created — source paths are never moved or modified.
fn handle_failed_import(
    plan: &ImportPlan,
    original: &CliError,
    now: &chrono::DateTime<Utc>,
    timestamp: &str,
) -> CliError {
    let failure_reason = original.to_string();
    let mut warnings = Vec::new();
    let staged = stage_failed_target_db_aside(&plan.target_db, timestamp, &mut warnings);

    let staged_note = if staged.is_empty() {
        "no partial target DB was created".to_string()
    } else {
        format!(
            "partial target DB staged aside at {}",
            staged
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };

    let receipt = LegacyImportReceipt {
        receipt_version: LEGACY_IMPORT_RECEIPT_VERSION,
        outcome: LEGACY_IMPORT_OUTCOME_FAILED.to_string(),
        failure_reason: Some(failure_reason.clone()),
        created_at: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        mode: plan.mode,
        search_root: plan.search_root.display().to_string(),
        source_db: plan.source_db.display().to_string(),
        source_storage_root: plan.source_storage_root.display().to_string(),
        target_db: plan.target_db.display().to_string(),
        target_storage_root: plan.target_storage_root.display().to_string(),
        migrated_migration_ids: Vec::new(),
        integrity_check_ok: false,
        core_table_counts: BTreeMap::new(),
        setup_refresh_ok: false,
        warnings: {
            let mut all = warnings;
            if !staged.is_empty() {
                all.push(format!("{staged_note} (rename, not deletion)"));
            }
            all
        },
    };
    let receipt_note = match write_receipt(&plan.target_storage_root, &receipt, timestamp) {
        Ok(path) => format!("failure receipt written to {}", path.display()),
        Err(receipt_err) => format!("failure receipt could not be written: {receipt_err}"),
    };

    CliError::Other(format!(
        "legacy import failed: {failure_reason}; {staged_note}; {receipt_note}; \
         the original target paths are free again, so the same command can be retried \
         once the cause is fixed"
    ))
}

/// Rename the partially created target DB and its SQLite sidecars aside as
/// `<target>.failed-<ts>` (sidecars become `<target>.failed-<ts>-wal` /
/// `<target>.failed-<ts>-shm`, keeping them associated with the staged DB).
/// Missing files are skipped; rename errors are reported as warnings rather
/// than masking the original import error.
fn stage_failed_target_db_aside(
    target_db: &Path,
    timestamp: &str,
    warnings: &mut Vec<String>,
) -> Vec<PathBuf> {
    let base = format!("{}.failed-{timestamp}", target_db.display());
    let mut staged = Vec::new();
    for suffix in ["", "-wal", "-shm"] {
        let candidate = if suffix.is_empty() {
            target_db.to_path_buf()
        } else {
            PathBuf::from(format!("{}{suffix}", target_db.display()))
        };
        if fs::symlink_metadata(&candidate).is_err() {
            continue;
        }
        let mut dest = PathBuf::from(format!("{base}{suffix}"));
        let mut counter = 1_u32;
        while fs::symlink_metadata(&dest).is_ok() {
            dest = PathBuf::from(format!("{base}-{counter}{suffix}"));
            counter = counter.saturating_add(1);
            if counter > 1000 {
                break;
            }
        }
        match fs::rename(&candidate, &dest) {
            Ok(()) => staged.push(dest),
            Err(err) => warnings.push(format!(
                "failed to stage partial target artifact {} aside: {err}",
                candidate.display()
            )),
        }
    }
    staged
}

fn run_setup_refresh_once(project_dir: Option<PathBuf>) -> CliResult<()> {
    let config = Config::from_env();
    let cwd = project_dir.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    handle_setup(SetupCommand::Run {
        agent: None,
        dry_run: false,
        yes: true,
        token: None,
        port: config.http_port,
        host: config.http_host,
        path: config.http_path,
        project_dir: Some(cwd),
        format: None,
        json: false,
        no_user_config: false,
        no_hooks: false,
    })
}

fn migrate_sqlite_db(path: &Path) -> CliResult<Vec<String>> {
    use asupersync::runtime::RuntimeBuilder;

    let conn = DbConn::open_file(path.display().to_string())
        .map_err(|e| CliError::Other(format!("cannot open sqlite DB {}: {e}", path.display())))?;
    conn.execute_raw(schema::PRAGMA_DB_INIT_BASE_SQL)
        .map_err(|e| CliError::Other(format!("failed to apply base init PRAGMAs: {e}")))?;

    let cx = asupersync::Cx::for_request();
    let rt = RuntimeBuilder::current_thread()
        .build()
        .map_err(|e| CliError::Other(format!("failed to build runtime: {e}")))?;
    match rt.block_on(async { schema::migrate_to_latest_base(&cx, &conn).await }) {
        asupersync::Outcome::Ok(ids) => {
            schema::enforce_runtime_fts_cleanup(&conn)
                .map_err(|e| CliError::Other(format!("runtime FTS cleanup failed: {e}")))?;
            let _ = conn.execute_raw("PRAGMA journal_mode = WAL;");
            Ok(ids)
        }
        asupersync::Outcome::Err(e) => Err(CliError::Other(format!("migration failed: {e}"))),
        asupersync::Outcome::Cancelled(r) => {
            Err(CliError::Other(format!("migration cancelled: {r:?}")))
        }
        asupersync::Outcome::Panicked(p) => {
            Err(CliError::Other(format!("migration panicked: {p}")))
        }
    }
}

fn integrity_check_ok(path: &Path) -> CliResult<bool> {
    let conn = DbConn::open_file(path.display().to_string())
        .map_err(|e| CliError::Other(format!("cannot open sqlite DB {}: {e}", path.display())))?;
    let rows = conn
        .query_sync("PRAGMA integrity_check", &[])
        .map_err(|e| CliError::Other(format!("integrity_check query failed: {e}")))?;
    let value = rows
        .first()
        .and_then(|r| r.get_named::<String>("integrity_check").ok())
        .unwrap_or_default();
    if value == "ok" {
        return Ok(true);
    }

    // #153 defect 1: the one-shot migration gate failed closed on a
    // canonical-clean DB. The bespoke (frankensqlite) engine diverges from
    // canonical SQLite on shapes canonical accepts — most notably the
    // `agents(project_id, name COLLATE NOCASE)` unique index it false-flags
    // (#151/#152). The runtime integrity guard already reconciles such a
    // verdict against canonical SQLite (`reconcile_with_canonical` in
    // mcp-agent-mail-db::pool); the migration gate did not, so a freshly
    // migrated DB that canonical `PRAGMA integrity_check`/`quick_check` both
    // accept was reported as a hard failure (no receipt, operator steered away
    // from a migration that in fact succeeded).
    //
    // Mirror the runtime contract here: a non-`ok` bespoke verdict is only a
    // failure if canonical SQLite also rejects the file. Drop the bespoke
    // connection first so the canonical engine opens the file cleanly.
    drop(conn);
    match mcp_agent_mail_db::pool::sqlite_compatibility_read_path_is_healthy(path) {
        Ok(true) => {
            tracing::warn!(
                path = %path.display(),
                primary_verdict = %value,
                "legacy import integrity gate: bespoke engine rejected the migrated DB but canonical SQLite accepts it; treating as healthy"
            );
            Ok(true)
        }
        Ok(false) => Ok(false),
        Err(e) => {
            // Canonical second opinion could not run. Fail closed: preserve the
            // bespoke rejection rather than invent health we cannot confirm.
            tracing::warn!(
                path = %path.display(),
                primary_verdict = %value,
                canonical_error = %e,
                "legacy import integrity gate: bespoke engine rejected the migrated DB and canonical fallback could not run"
            );
            Ok(false)
        }
    }
}

fn query_core_table_counts(path: &Path) -> CliResult<BTreeMap<String, i64>> {
    let conn = DbConn::open_file(path.display().to_string())
        .map_err(|e| CliError::Other(format!("cannot open sqlite DB {}: {e}", path.display())))?;
    let mut out = BTreeMap::new();
    for table in [
        "projects",
        "agents",
        "messages",
        "message_recipients",
        "file_reservations",
        "agent_links",
    ] {
        let sql = format!("SELECT COUNT(*) AS c FROM {table}");
        let rows = conn
            .query_sync(&sql, &[])
            .map_err(|e| CliError::Other(format!("count query failed for {table}: {e}")))?;
        let count = rows
            .first()
            .and_then(|r| r.get_named::<i64>("c").ok())
            .unwrap_or(0);
        out.insert(table.to_string(), count);
    }
    Ok(out)
}

fn write_receipt(
    target_storage_root: &Path,
    receipt: &LegacyImportReceipt,
    timestamp: &str,
) -> CliResult<PathBuf> {
    let dir = target_storage_root.join("legacy_import_receipts");
    fs::create_dir_all(&dir)?;
    let mut path = dir.join(format!("legacy_import_{timestamp}.json"));
    if path.exists() {
        let mut suffix = 1_u32;
        loop {
            let candidate = dir.join(format!("legacy_import_{timestamp}_{suffix}.json"));
            if !candidate.exists() {
                path = candidate;
                break;
            }
            suffix = suffix
                .checked_add(1)
                .ok_or_else(|| CliError::Other("too many legacy import receipts".to_string()))?;
        }
    }
    let content = serde_json::to_string_pretty(receipt)
        .map_err(|e| CliError::Other(format!("failed to serialize receipt: {e}")))?;
    fs::write(&path, format!("{content}\n"))?;
    Ok(path)
}

fn build_detect_report(
    search_root: &Path,
    explicit_db: Option<&Path>,
    explicit_storage_root: Option<&Path>,
) -> CliResult<LegacyDetectReport> {
    let resolved =
        resolve_legacy_authorities(search_root, explicit_db, explicit_storage_root)?;
    build_detect_report_from_resolved(search_root, &resolved, None)
}

fn build_detect_report_from_resolved(
    search_root: &Path,
    resolved: &ResolvedLegacyAuthorities,
    source_snapshot: Option<&LegacySourceSnapshot>,
) -> CliResult<LegacyDetectReport> {
    let db_resolved = &resolved.database;
    let storage_resolved = &resolved.storage;

    let mut markers = Vec::new();
    if let Some(marker) = detect_pyproject_marker(search_root) {
        markers.push(marker);
    }
    if let Some(marker) = detect_legacy_script_marker(search_root) {
        markers.push(marker);
    }
    if search_root.join("uv.lock").exists() {
        markers.push(LegacyMarker {
            id: "uv_lock".to_string(),
            severity: MarkerSeverity::Low,
            detail: "uv.lock present (legacy Python packaging footprint)".to_string(),
            path: Some(search_root.join("uv.lock").display().to_string()),
        });
    }
    if search_root.join(".venv").exists() {
        markers.push(LegacyMarker {
            id: "venv".to_string(),
            severity: MarkerSeverity::Low,
            detail: ".venv directory present".to_string(),
            path: Some(search_root.join(".venv").display().to_string()),
        });
    }
    if let Some(marker) = detect_env_marker(&resolved.env) {
        markers.push(marker);
    }
    if db_resolved.exists {
        markers.push(LegacyMarker {
            id: "db_exists".to_string(),
            severity: MarkerSeverity::Medium,
            detail: "resolved database file exists".to_string(),
            path: Some(db_resolved.path.display().to_string()),
        });
    }
    if storage_resolved.exists {
        markers.push(LegacyMarker {
            id: "storage_exists".to_string(),
            severity: MarkerSeverity::Medium,
            detail: "resolved storage root exists".to_string(),
            path: Some(storage_resolved.path.display().to_string()),
        });
    }

    let db_signature = if !db_resolved.exists {
        None
    } else if let Some(snapshot) = source_snapshot {
        if snapshot.original_path() != db_resolved.path.as_path() {
            return Err(CliError::Other(format!(
                "legacy detector snapshot source {} does not match resolved database {}",
                snapshot.original_path().display(),
                db_resolved.path.display()
            )));
        }
        Some(inspect_db_signature(snapshot))
    } else {
        match LegacySourceSnapshot::capture(&db_resolved.path) {
            Ok(snapshot) => Some(inspect_db_signature(&snapshot)),
            Err(error) => Some(LegacyDbSignature {
                open_ok: false,
                core_tables_present: false,
                legacy_trigger_count: 0,
                datetime_like_column_count: 0,
                migrations_table_present: false,
                notes: vec![format!("failed to capture coherent sqlite source: {error}")],
            }),
        }
    };
    if let Some(sig) = &db_signature {
        if sig.legacy_trigger_count > 0 {
            markers.push(LegacyMarker {
                id: "legacy_fts_triggers".to_string(),
                severity: MarkerSeverity::High,
                detail: format!(
                    "legacy FTS triggers detected (count={})",
                    sig.legacy_trigger_count
                ),
                path: Some(db_resolved.path.display().to_string()),
            });
        }
        if sig.datetime_like_column_count > 0 {
            markers.push(LegacyMarker {
                id: "datetime_columns".to_string(),
                severity: MarkerSeverity::High,
                detail: format!(
                    "legacy DATETIME/TEXT timestamp columns detected (count={})",
                    sig.datetime_like_column_count
                ),
                path: Some(db_resolved.path.display().to_string()),
            });
        }
        if sig.core_tables_present && !sig.migrations_table_present {
            markers.push(LegacyMarker {
                id: "missing_migrations_table".to_string(),
                severity: MarkerSeverity::Medium,
                detail: "core tables present but migration tracking table missing".to_string(),
                path: Some(db_resolved.path.display().to_string()),
            });
        }
    }

    // Bug #87: If the installed `am` binary is a compiled native executable
    // (ELF, Mach-O, PE) rather than a Python script, the markers we collected
    // are artefacts of the *Rust* installation, not a legacy Python one.
    // Clear the markers so we don't falsely offer a migration prompt.
    //
    // Important: only clear when we *positively find* a native binary.
    // If no binary is found at all, keep markers — there may be orphaned
    // Python artifacts (database, pyproject.toml) worth migrating even
    // though the Python binary itself was removed.
    if !markers.is_empty()
        && let Some(binary_path) = find_installed_am_binary()
        && !is_likely_python_binary(&binary_path)
    {
        markers.clear();
    }

    let score: u32 = markers
        .iter()
        .map(|m| match m.severity {
            MarkerSeverity::Low => 1,
            MarkerSeverity::Medium => 2,
            MarkerSeverity::High => 3,
        })
        .sum();

    let strong_signal = db_signature.as_ref().is_some_and(|sig| {
        sig.core_tables_present
            && (sig.legacy_trigger_count > 0 || sig.datetime_like_column_count > 0)
    });
    let confidence = if strong_signal || score >= 9 {
        ConfidenceLevel::High
    } else if score >= 5 {
        ConfidenceLevel::Medium
    } else if score >= 2 {
        ConfidenceLevel::Low
    } else {
        ConfidenceLevel::None
    };
    let detected = confidence != ConfidenceLevel::None;

    let recommended_action = if detected {
        "am legacy import --auto --yes".to_string()
    } else {
        "No strong legacy markers detected; run `am legacy detect --json` for details.".to_string()
    };

    Ok(LegacyDetectReport {
        search_root: search_root.display().to_string(),
        detected,
        confidence,
        score,
        database: ResolvedPathInfo {
            path: db_resolved.path.display().to_string(),
            source: db_resolved.source,
            exists: db_resolved.exists,
            raw_value: db_resolved.raw_value.clone(),
            error: None,
        },
        storage_root: ResolvedPathInfo {
            path: storage_resolved.path.display().to_string(),
            source: storage_resolved.source,
            exists: storage_resolved.exists,
            raw_value: storage_resolved.raw_value.clone(),
            error: None,
        },
        markers,
        db_signature,
        recommended_action,
    })
}

/// Check whether a binary at `path` is likely a Python script (shebang with
/// "python") rather than a compiled native binary (ELF, Mach-O, PE).
///
/// Returns `true` only when positive evidence of Python is found.  For native
/// executables or any unreadable/missing file the function returns `false`.
fn is_likely_python_binary(path: &Path) -> bool {
    let mut file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut header = [0u8; 64];
    let n = match file.read(&mut header) {
        Ok(n) => n,
        Err(_) => return false,
    };
    if n < 2 {
        return false;
    }

    // Shebang — check if the interpreter line references Python.
    if header[0] == b'#' && header[1] == b'!' {
        let line = std::str::from_utf8(&header[..n]).unwrap_or("");
        return line.to_ascii_lowercase().contains("python");
    }

    // ELF magic: 0x7f 'E' 'L' 'F'
    if n >= 4 && header[..4] == [0x7f, b'E', b'L', b'F'] {
        return false;
    }

    // PE (Windows) magic: 'M' 'Z'
    if header[0] == b'M' && header[1] == b'Z' {
        return false;
    }

    // Mach-O magic (32-bit and 64-bit, both endiannesses)
    if n >= 4 {
        let magic = &header[..4];
        if magic == [0xcf, 0xfa, 0xed, 0xfe]   // MH_MAGIC_64 (little-endian)
            || magic == [0xfe, 0xed, 0xfa, 0xcf] // MH_MAGIC_64 (big-endian)
            || magic == [0xce, 0xfa, 0xed, 0xfe] // MH_MAGIC (little-endian)
            || magic == [0xfe, 0xed, 0xfa, 0xce]
        // MH_MAGIC (big-endian)
        {
            return false;
        }
    }

    // Unknown format — assume not Python.
    false
}

/// Find the installed `am` binary.
///
/// We first check whether the currently-running process IS the `am` CLI
/// (by looking at the executable's file name).  If it is, we return its
/// path directly.  Otherwise we fall back to a PATH lookup via `which`.
fn find_installed_am_binary() -> Option<PathBuf> {
    #[cfg(test)]
    {
        None
    }

    #[cfg(not(test))]
    {
        // If the currently-running executable is `am`, use it directly.
        if let Ok(exe) = std::env::current_exe()
            && exe
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| name == "am")
        {
            return Some(exe);
        }
        // Fallback: look up `am` in PATH via `which`.
        if let Ok(output) = std::process::Command::new("which").arg("am").output()
            && output.status.success()
        {
            let path_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path_str.is_empty() {
                let p = PathBuf::from(path_str);
                if p.exists() {
                    return Some(p);
                }
            }
        }
        None
    }
}

fn detect_pyproject_marker(search_root: &Path) -> Option<LegacyMarker> {
    let pyproject = search_root.join("pyproject.toml");
    if !pyproject.exists() {
        return None;
    }
    let text = fs::read_to_string(&pyproject).ok()?;
    if text.contains("name = \"mcp-agent-mail\"")
        || text.contains("name='mcp-agent-mail'")
        || text.contains("mcp_agent_mail")
    {
        return Some(LegacyMarker {
            id: "pyproject_package".to_string(),
            severity: MarkerSeverity::High,
            detail: "pyproject.toml contains mcp-agent-mail package marker".to_string(),
            path: Some(pyproject.display().to_string()),
        });
    }
    None
}

fn detect_legacy_script_marker(search_root: &Path) -> Option<LegacyMarker> {
    let marker = search_root.join("scripts").join("run_server_with_token.sh");
    if marker.exists() {
        return Some(LegacyMarker {
            id: "legacy_run_script".to_string(),
            severity: MarkerSeverity::High,
            detail: "legacy Python run helper script present".to_string(),
            path: Some(marker.display().to_string()),
        });
    }
    None
}

fn detect_env_marker(snapshot: &LegacyEnvSnapshot) -> Option<LegacyMarker> {
    let project = snapshot.project.as_ref()?;
    let map = &project.values;
    let legacy_db = map
        .get("DATABASE_URL")
        .is_some_and(|value| value.contains("sqlite+aiosqlite:///"));
    let legacy_storage = map
        .get("STORAGE_ROOT")
        .is_some_and(|value| value.contains(".mcp_agent_mail_git_mailbox_repo"));

    if legacy_db || legacy_storage {
        return Some(LegacyMarker {
            id: "legacy_env_defaults".to_string(),
            severity: MarkerSeverity::High,
            detail: "project .env contains legacy Python DATABASE_URL/STORAGE_ROOT markers"
                .to_string(),
            path: Some(project.path.display().to_string()),
        });
    }
    None
}

fn inspect_db_signature(snapshot: &LegacySourceSnapshot) -> LegacyDbSignature {
    // Immutable SQLite is safe only because the type retains a private,
    // transactionally materialized source generation. It must never accept an
    // arbitrary live pathname, where immutable mode would ignore the WAL.
    let conn = match open_private_immutable_source_snapshot(snapshot) {
        Ok(conn) => conn,
        Err(error) => {
            return LegacyDbSignature {
                open_ok: false,
                core_tables_present: false,
                legacy_trigger_count: 0,
                datetime_like_column_count: 0,
                migrations_table_present: false,
                notes: vec![format!("failed to open retained source snapshot: {error}")],
            };
        }
    };

    let mut notes = Vec::new();
    let table_rows = conn
        .query_sync("SELECT name FROM sqlite_master WHERE type='table'", &[])
        .unwrap_or_default();
    let table_names: std::collections::BTreeSet<String> = table_rows
        .iter()
        .filter_map(|r| r.get_named::<String>("name").ok())
        .collect();
    let core_tables = [
        "projects",
        "agents",
        "messages",
        "message_recipients",
        "file_reservations",
        "agent_links",
    ];
    let core_tables_present = core_tables.iter().all(|name| table_names.contains(*name));
    let migrations_table_present = table_names.contains("mcp_agent_mail_migrations");

    let trigger_rows = conn
        .query_sync(
            "SELECT name FROM sqlite_master WHERE type='trigger' \
             AND name IN ('fts_messages_ai','fts_messages_ad','fts_messages_au')",
            &[],
        )
        .unwrap_or_default();
    let legacy_trigger_count = trigger_rows.len();

    let mut datetime_like_column_count = 0usize;
    for table in [
        "projects",
        "agents",
        "messages",
        "file_reservations",
        "products",
        "product_project_links",
    ] {
        let pragma_sql = format!("PRAGMA table_info({table})");
        let cols = conn.query_sync(&pragma_sql, &[]).unwrap_or_default();
        for col in cols {
            let col_name: String = col.get_named("name").unwrap_or_default();
            let col_type: String = col.get_named("type").unwrap_or_default();
            let is_ts_column = matches!(
                col_name.as_str(),
                "created_at"
                    | "created_ts"
                    | "inception_ts"
                    | "last_active_ts"
                    | "updated_ts"
                    | "expires_ts"
                    | "released_ts"
                    | "confirmed_ts"
                    | "dismissed_ts"
                    | "evaluated_ts"
                    | "read_ts"
                    | "ack_ts"
            );
            if is_ts_column {
                let upper = col_type.to_ascii_uppercase();
                if upper.contains("DATE") || upper.contains("TEXT") {
                    datetime_like_column_count += 1;
                }
            }
        }
    }

    if core_tables_present {
        notes.push("core legacy tables present".to_string());
    }
    if legacy_trigger_count > 0 {
        notes.push("legacy Python FTS triggers present".to_string());
    }
    if datetime_like_column_count > 0 {
        notes.push("legacy DATETIME/TEXT timestamp columns present".to_string());
    }

    LegacyDbSignature {
        open_ok: true,
        core_tables_present,
        legacy_trigger_count,
        datetime_like_column_count,
        migrations_table_present,
        notes,
    }
}

#[cfg(test)]
fn resolve_database_path(search_root: &Path, explicit: Option<&Path>) -> CliResult<ResolvedPath> {
    let process_value = if explicit.is_none() {
        process_env_value_for_legacy("DATABASE_URL")?
    } else {
        None
    };
    let needs_database_authority = explicit.is_none() && process_value.is_none();
    let snapshot =
        capture_legacy_env_snapshot(search_root, needs_database_authority, false)?;
    resolve_database_path_from_snapshot(
        search_root,
        explicit,
        process_value.as_deref(),
        &snapshot,
    )
}

fn resolve_database_path_from_snapshot(
    search_root: &Path,
    explicit: Option<&Path>,
    process_value: Option<&str>,
    snapshot: &LegacyEnvSnapshot,
) -> CliResult<ResolvedPath> {
    if let Some(path) = explicit {
        let raw = path.to_string_lossy();
        require_nonblank_legacy_authority("DATABASE_URL", &raw)?;
        let normalized = normalize_input_path(&raw, search_root);
        return Ok(ResolvedPath {
            exists: normalized.exists(),
            path: normalized,
            source: ResolvedSource::Explicit,
            raw_value: Some(path.display().to_string()),
        });
    }

    if let Some(value) = process_value {
        return parse_database_value(value, search_root, ResolvedSource::ProcessEnv);
    }

    if let Some(value) = snapshot
        .project
        .as_ref()
        .and_then(|authority| authority.values.get("DATABASE_URL"))
    {
        return parse_database_value(value, search_root, ResolvedSource::ProjectEnv);
    }

    if let Some(value) = snapshot
        .user
        .as_ref()
        .and_then(|authority| authority.values.get("DATABASE_URL"))
    {
        return parse_database_value(value, search_root, ResolvedSource::UserEnv);
    }

    parse_database_value(
        "sqlite+aiosqlite:///./storage.sqlite3",
        search_root,
        ResolvedSource::Default,
    )
}

fn resolve_storage_root(search_root: &Path, explicit: Option<&Path>) -> CliResult<ResolvedPath> {
    let process_value = if explicit.is_none() {
        process_env_value_for_legacy("STORAGE_ROOT")?
    } else {
        None
    };
    let needs_storage_authority = explicit.is_none() && process_value.is_none();
    let snapshot =
        capture_legacy_env_snapshot(search_root, false, needs_storage_authority)?;
    resolve_storage_root_from_snapshot(
        search_root,
        explicit,
        process_value.as_deref(),
        &snapshot,
    )
}

fn resolve_storage_root_from_snapshot(
    search_root: &Path,
    explicit: Option<&Path>,
    process_value: Option<&str>,
    snapshot: &LegacyEnvSnapshot,
) -> CliResult<ResolvedPath> {
    if let Some(path) = explicit {
        let raw = path.to_string_lossy();
        require_nonblank_legacy_authority("STORAGE_ROOT", &raw)?;
        let normalized = normalize_input_path(&raw, search_root);
        return Ok(ResolvedPath {
            exists: normalized.exists(),
            path: normalized,
            source: ResolvedSource::Explicit,
            raw_value: Some(path.display().to_string()),
        });
    }

    if let Some(value) = process_value {
        return parse_storage_value(value, search_root, ResolvedSource::ProcessEnv);
    }

    if let Some(value) = snapshot
        .project
        .as_ref()
        .and_then(|authority| authority.values.get("STORAGE_ROOT"))
    {
        return parse_storage_value(value, search_root, ResolvedSource::ProjectEnv);
    }

    if let Some(value) = snapshot
        .user
        .as_ref()
        .and_then(|authority| authority.values.get("STORAGE_ROOT"))
    {
        return parse_storage_value(value, search_root, ResolvedSource::UserEnv);
    }

    let value = "~/.mcp_agent_mail_git_mailbox_repo";
    parse_storage_value(value, search_root, ResolvedSource::Default)
}

fn resolve_legacy_authorities(
    search_root: &Path,
    explicit_database: Option<&Path>,
    explicit_storage: Option<&Path>,
) -> CliResult<ResolvedLegacyAuthorities> {
    let process_database = if explicit_database.is_none() {
        process_env_value_for_legacy("DATABASE_URL")?
    } else {
        None
    };
    let process_storage = if explicit_storage.is_none() {
        process_env_value_for_legacy("STORAGE_ROOT")?
    } else {
        None
    };
    let needs_database_authority = explicit_database.is_none() && process_database.is_none();
    let needs_storage_authority = explicit_storage.is_none() && process_storage.is_none();
    let env = capture_legacy_env_snapshot(
        search_root,
        needs_database_authority,
        needs_storage_authority,
    )?;
    let database = resolve_database_path_from_snapshot(
        search_root,
        explicit_database,
        process_database.as_deref(),
        &env,
    )?;
    let storage = resolve_storage_root_from_snapshot(
        search_root,
        explicit_storage,
        process_storage.as_deref(),
        &env,
    )?;
    Ok(ResolvedLegacyAuthorities {
        database,
        storage,
        env,
    })
}

fn process_env_value_for_legacy(key: &str) -> CliResult<Option<String>> {
    match std::env::var(key) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(CliError::InvalidArgument(format!(
            "process environment authority {key} is not valid UTF-8"
        ))),
    }
}

fn resolve_legacy_database_url_path(db_path: &Path, search_root: &Path) -> PathBuf {
    let db_path_text = db_path.to_string_lossy();
    if db_path.is_absolute() {
        return db_path.to_path_buf();
    }

    let joined = normalize_input_path(&db_path_text, search_root);
    if joined.exists() {
        return joined;
    }

    let explicit_relative = db_path_text.starts_with("./") || db_path_text.starts_with("../");
    if !explicit_relative {
        let absolute_candidate = Path::new("/").join(db_path);
        if absolute_candidate.exists() {
            return absolute_candidate;
        }
    }

    joined
}

fn parse_database_value(
    value: &str,
    search_root: &Path,
    source: ResolvedSource,
) -> CliResult<ResolvedPath> {
    require_nonblank_legacy_authority("DATABASE_URL", value)?;
    if is_sqlite_memory_database_url(value) {
        return Err(CliError::InvalidArgument(
            "in-memory DATABASE_URL is not supported for legacy import".to_string(),
        ));
    }

    let path = if value.contains("://") {
        let db_path = sqlite_file_path_from_database_url(value).ok_or_else(|| {
            CliError::InvalidArgument(format!(
                "unsupported DATABASE_URL scheme for import: {value}"
            ))
        })?;
        resolve_legacy_database_url_path(&db_path, search_root)
    } else {
        normalize_input_path(value, search_root)
    };
    Ok(ResolvedPath {
        exists: path.exists(),
        path,
        source,
        raw_value: Some(value.to_string()),
    })
}

fn parse_storage_value(
    value: &str,
    search_root: &Path,
    source: ResolvedSource,
) -> CliResult<ResolvedPath> {
    require_nonblank_legacy_authority("STORAGE_ROOT", value)?;
    let path = normalize_input_path(value, search_root);
    Ok(ResolvedPath {
        exists: path.exists(),
        path,
        source,
        raw_value: Some(value.to_string()),
    })
}

fn require_nonblank_legacy_authority(key: &str, value: &str) -> CliResult<()> {
    if value.trim().is_empty() {
        return Err(CliError::InvalidArgument(format!(
            "legacy {key} authority must not be empty or whitespace"
        )));
    }
    Ok(())
}

fn parse_env_file_map(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let kv_line = trimmed
            .strip_prefix("export")
            .filter(|rest| rest.starts_with(char::is_whitespace))
            .map(str::trim_start)
            .unwrap_or(trimmed);
        let Some((k, v)) = kv_line.split_once('=') else {
            continue;
        };
        let key = k.trim().to_string();
        if key.is_empty() {
            continue;
        }
        let mut val = v.trim().to_string();
        if ((val.starts_with('"') && val.ends_with('"'))
            || (val.starts_with('\'') && val.ends_with('\'')))
            && val.len() >= 2
        {
            val = val[1..val.len() - 1].to_string();
        }
        out.insert(key, val);
    }
    out
}

fn capture_legacy_env_snapshot(
    search_root: &Path,
    needs_database_authority: bool,
    needs_storage_authority: bool,
) -> CliResult<LegacyEnvSnapshot> {
    if !needs_database_authority && !needs_storage_authority {
        return Ok(LegacyEnvSnapshot::default());
    }
    let user_candidates = user_env_authority_candidates();
    capture_legacy_env_snapshot_with_reader(
        search_root,
        &user_candidates,
        needs_database_authority,
        needs_storage_authority,
        read_env_authority_text,
    )
}

fn capture_legacy_env_snapshot_with_reader(
    search_root: &Path,
    user_candidates: &[PathBuf],
    needs_database_authority: bool,
    needs_storage_authority: bool,
    mut read_authority: impl FnMut(&Path) -> std::io::Result<Option<String>>,
) -> CliResult<LegacyEnvSnapshot> {
    if !needs_database_authority && !needs_storage_authority {
        return Ok(LegacyEnvSnapshot::default());
    }
    let project_path = search_root.join(".env");
    let project_text = read_legacy_env_authority(&project_path, &mut read_authority)?;
    let project_values = project_text
        .as_deref()
        .map(parse_env_file_map)
        .unwrap_or_default();
    let include_user_authority = (needs_database_authority
        && !project_values.contains_key("DATABASE_URL"))
        || (needs_storage_authority && !project_values.contains_key("STORAGE_ROOT"));

    let mut selected_user: Option<(usize, String)> = None;
    if include_user_authority {
        for (index, path) in user_candidates.iter().enumerate() {
            if let Some(text) = read_legacy_env_authority(path, &mut read_authority)? {
                selected_user = Some((index, text));
                break;
            }
        }
    }

    // A single import plan must never combine values obtained on separate,
    // drifting path lookups. Re-probe every authority that influenced
    // precedence before returning the parsed snapshot. The low-level reader
    // independently binds each present file and its parent while reading it.
    require_unchanged_env_authority(
        &project_path,
        project_text.as_deref(),
        &mut read_authority,
    )?;
    let user_probe_len = if include_user_authority {
        selected_user
            .as_ref()
            .map_or(user_candidates.len(), |(index, _)| index + 1)
    } else {
        0
    };
    for (index, path) in user_candidates.iter().take(user_probe_len).enumerate() {
        let expected = selected_user
            .as_ref()
            .filter(|(selected_index, _)| *selected_index == index)
            .map(|(_, text)| text.as_str());
        require_unchanged_env_authority(path, expected, &mut read_authority)?;
    }

    let project = project_text.map(|_| EnvAuthoritySnapshot {
        path: project_path,
        values: project_values,
    });
    let user = selected_user.map(|(index, text)| EnvAuthoritySnapshot {
        path: user_candidates[index].clone(),
        values: parse_env_file_map(&text),
    });
    Ok(LegacyEnvSnapshot { project, user })
}

fn read_legacy_env_authority(
    path: &Path,
    read_authority: &mut impl FnMut(&Path) -> std::io::Result<Option<String>>,
) -> CliResult<Option<String>> {
    read_authority(path).map_err(|error| {
        CliError::InvalidArgument(format!(
            "refusing unsafe legacy environment authority {}: {error}",
            path.display()
        ))
    })
}

fn require_unchanged_env_authority(
    path: &Path,
    expected: Option<&str>,
    read_authority: &mut impl FnMut(&Path) -> std::io::Result<Option<String>>,
) -> CliResult<()> {
    let observed = read_legacy_env_authority(path, read_authority)?;
    if observed.as_deref() != expected {
        return Err(CliError::InvalidArgument(format!(
            "legacy environment authority {} changed while capturing one import snapshot",
            path.display()
        )));
    }
    Ok(())
}

fn resolve_search_root(search_root: Option<PathBuf>) -> PathBuf {
    let root = search_root.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    root.canonicalize().unwrap_or(root)
}

fn normalize_input_path(raw: &str, base: &Path) -> PathBuf {
    let expanded = expand_tilde(raw);
    if expanded.is_absolute() {
        expanded
    } else {
        base.join(expanded)
    }
}

fn normalize_path_for_overlap(path: &Path) -> PathBuf {
    normalize_lexical_path(&crate::canonicalize_existing_prefix(path))
}

fn normalize_lexical_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(component.as_os_str());
                }
            }
            Component::Normal(segment) => out.push(segment),
        }
    }
    out
}

fn paths_overlap(a: &Path, b: &Path) -> bool {
    let a = normalize_path_for_overlap(a);
    let b = normalize_path_for_overlap(b);
    a.starts_with(&b) || b.starts_with(&a)
}

fn expand_tilde(raw: &str) -> PathBuf {
    if raw == "~" {
        return home_dir().unwrap_or_else(|| PathBuf::from(raw));
    }
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(raw)
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        dirs::home_dir()
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

fn default_copy_target_db(source_db: &Path) -> PathBuf {
    let stem = source_db
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("storage");
    source_db.with_file_name(format!("{stem}.rust-copy.sqlite3"))
}

fn default_copy_target_storage(source_storage: &Path) -> PathBuf {
    let name = source_storage
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("storage_root");
    source_storage.with_file_name(format!("{name}-rust-copy"))
}

fn open_canonical_read_only(path: &Path) -> CliResult<CanonicalDbConn> {
    let path_text = path.to_string_lossy().into_owned();
    let config = mcp_agent_mail_db::sqlmodel_sqlite::SqliteConfig::file(path_text)
        .flags(mcp_agent_mail_db::sqlmodel_sqlite::OpenFlags::read_only());
    CanonicalDbConn::open(&config).map_err(|error| {
        CliError::Other(format!(
            "cannot open SQLite DB read-only {}: {error}",
            path.display()
        ))
    })
}

/// Encode a filesystem path for use inside a SQLite URI filename.
///
/// SQLite URI filenames treat `?` as the query separator and `#` as a fragment
/// marker, and `%` introduces percent-escapes, so those three characters must
/// be percent-encoded when they appear in the path itself.
fn sqlite_uri_encode_path(path: &Path) -> String {
    let mut encoded = String::new();
    for ch in path.to_string_lossy().chars() {
        match ch {
            '%' => encoded.push_str("%25"),
            '?' => encoded.push_str("%3F"),
            '#' => encoded.push_str("%23"),
            other => encoded.push(other),
        }
    }
    encoded
}

fn legacy_source_has_franken_namespace_artifact(path: &Path) -> CliResult<bool> {
    for suffix in ["-fsqlite-ns-gate", "-fsqlite-ns-use"] {
        let sidecar = mcp_agent_mail_core::disk::sqlite_sidecar_path(path, suffix);
        match fs::symlink_metadata(&sidecar) {
            Ok(_) => return Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(CliError::Other(format!(
                    "cannot inspect legacy source namespace artifact {}: {error}",
                    sidecar.display()
                )));
            }
        }
    }
    Ok(false)
}

fn backup_guarded_offline_canonical_source_into_snapshot(
    source: &Path,
    destination: &Path,
) -> CliResult<()> {
    let context = "legacy offline source capture";
    let destination_text = crate::sqlite_snapshot_path_text(destination, context, "destination")?;
    crate::prepare_sqlite_snapshot_destination(destination, context)?;
    let conn =
        mcp_agent_mail_db::pool::open_guarded_read_only_canonical_sqlite_file(source, context)
            .map_err(|error| {
                CliError::Other(format!(
                    "cannot open offline legacy source {} with guarded canonical SQLite: {error}",
                    source.display()
                ))
            })?;
    // The online backup API reads through this one guarded read transaction,
    // including committed WAL frames, and creates only the private destination.
    // The source connection remains engine-enforced read-only throughout.
    conn.backup_to_path(destination_text).map_err(|error| {
        CliError::Other(format!(
            "cannot materialize WAL-aware legacy snapshot from {} to {}: {error}",
            source.display(),
            destination.display()
        ))
    })?;
    Ok(())
}

fn materialize_legacy_source_snapshot(source: &Path, destination: &Path) -> CliResult<()> {
    if legacy_source_has_franken_namespace_artifact(source)? {
        // Any namespace occupant makes canonical access ambiguous. The guarded
        // same-engine path either binds the complete current generation or
        // fails closed; it never falls back across engines.
        crate::vacuum_live_franken_sqlite_into_snapshot(
            source,
            destination,
            "legacy live Franken source capture",
        )
    } else {
        // Namespace absence selects the explicit offline/canonical authority.
        // The guarded true-read-only connection sees a complete WAL generation
        // when one is available and fails closed on unstable/suspect families.
        backup_guarded_offline_canonical_source_into_snapshot(source, destination)
    }
}

/// Open only a retained private source snapshot in SQLite immutable mode.
///
/// The typed argument prevents production callers from applying immutable mode
/// to a live database pathname. SQLite immutable mode intentionally ignores
/// WAL state; that is correct here because capture already folded one coherent
/// source transaction into this private standalone image.
fn open_private_immutable_source_snapshot(
    snapshot: &LegacySourceSnapshot,
) -> CliResult<CanonicalDbConn> {
    let uri = format!(
        "file:{}?immutable=1",
        sqlite_uri_encode_path(snapshot.snapshot_path())
    );
    let flags = {
        let mut flags = mcp_agent_mail_db::sqlmodel_sqlite::OpenFlags::read_only();
        flags.uri = true;
        flags
    };
    let config = mcp_agent_mail_db::sqlmodel_sqlite::SqliteConfig::file(uri).flags(flags);
    CanonicalDbConn::open(&config).map_err(|error| {
        CliError::Other(format!(
            "cannot open retained legacy source snapshot {} read-only: {error}",
            snapshot.snapshot_path().display()
        ))
    })
}

fn verify_canonical_quick_check(conn: &CanonicalDbConn, path: &Path, label: &str) -> CliResult<()> {
    let rows = conn
        .query_sync("PRAGMA quick_check", &[])
        .map_err(|error| {
            CliError::Other(format!(
                "{label} canonical SQLite quick_check failed for {}: {error}",
                path.display()
            ))
        })?;
    let value = rows
        .first()
        .and_then(|row| row.get_named::<String>("quick_check").ok())
        .unwrap_or_default();
    if value != "ok" {
        return Err(CliError::Other(format!(
            "{label} canonical SQLite quick_check is not ok for {}: {value}",
            path.display()
        )));
    }
    Ok(())
}

/// Verify a TARGET database (a fresh artifact this run created) is readable.
/// Uses a plain read-only open; side effects on our own target are harmless
/// and a non-immutable open sees any WAL content the migration left behind.
fn verify_canonical_sqlite_readable(path: &Path, label: &str) -> CliResult<()> {
    let conn = open_canonical_read_only(path)?;
    verify_canonical_quick_check(&conn, path, label)
}

/// Verify the one retained SOURCE generation used by this import.
fn verify_source_canonical_sqlite_readable(snapshot: &LegacySourceSnapshot) -> CliResult<()> {
    let conn = open_private_immutable_source_snapshot(snapshot)?;
    verify_canonical_quick_check(&conn, snapshot.original_path(), "source DB snapshot")
}

fn verify_runtime_sqlite_readable(path: &Path, label: &str) -> CliResult<()> {
    let conn = DbConn::open_file_read_only(path.display().to_string()).map_err(|error| {
        CliError::Other(format!(
            "{label} runtime SQLite read-only open failed for {}: {error}",
            path.display()
        ))
    })?;
    conn.query_sync("SELECT COUNT(*) AS c FROM sqlite_master", &[])
        .map_err(|error| {
            CliError::Other(format!(
                "{label} runtime SQLite read failed for {}: {error}",
                path.display()
            ))
        })?;
    Ok(())
}

fn copy_db_via_sqlite_backup(
    source_snapshot: &LegacySourceSnapshot,
    target_db: &Path,
) -> CliResult<()> {
    if fs::symlink_metadata(target_db).is_ok() {
        return Err(CliError::InvalidArgument(format!(
            "target database path must not already exist: {}",
            target_db.display()
        )));
    }

    if let Some(parent) = target_db.parent() {
        fs::create_dir_all(parent)?;
    }

    // All live/offline authority decisions and WAL observation happened once
    // during capture. Canonical SQLite now sees only the retained private inode,
    // so no source-side TOCTOU or mixed-engine descriptor close remains.
    let source = open_private_immutable_source_snapshot(source_snapshot)?;
    source
        .backup_to_path(target_db.to_string_lossy().as_ref())
        .map_err(|error| {
            CliError::Other(format!(
                "canonical SQLite backup from {} to {} failed: {error}",
                source_snapshot.original_path().display(),
                target_db.display()
            ))
        })?;
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> CliResult<()> {
    require_storage_directory(src, "source storage directory", false)?;
    if !require_storage_directory(dst, "target storage directory", true)? {
        fs::create_dir_all(dst)?;
        // Revalidate the created path before placing any source entry under
        // it. This catches an immediately substituted symlink/reparse point;
        // descriptor-relative copying remains part of the open capability-VFS
        // redesign rather than being implied by this path-based guard.
        require_storage_directory(dst, "target storage directory", false)?;
    }
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let target = dst.join(entry.file_name());
        let metadata = fs::symlink_metadata(&path)?;
        if storage_metadata_is_link_like(&metadata) {
            if path.is_dir() {
                return Err(CliError::InvalidArgument(format!(
                    "symlinked directories are not supported during recursive copy: {}",
                    path.display()
                )));
            }
            if path.is_file() {
                return Err(CliError::InvalidArgument(format!(
                    "symlinked files are not supported during recursive copy: {}",
                    path.display()
                )));
            }
            return Err(CliError::InvalidArgument(format!(
                "broken symlink encountered during recursive copy: {}",
                path.display()
            )));
        }
        if metadata.file_type().is_dir() {
            copy_dir_recursive(&path, &target)?;
        } else if metadata.file_type().is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&path, &target)?;
        } else {
            return Err(CliError::InvalidArgument(format!(
                "unsupported special file encountered during recursive copy: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

fn require_storage_directory(
    path: &Path,
    label: &str,
    allow_missing: bool,
) -> CliResult<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(false);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(CliError::InvalidArgument(format!(
                "{label} missing: {}",
                path.display()
            )));
        }
        Err(error) => return Err(error.into()),
    };
    if storage_metadata_is_link_like(&metadata) {
        return Err(CliError::InvalidArgument(format!(
            "{label} must not be a symlink or reparse point: {}",
            path.display()
        )));
    }
    if !metadata.file_type().is_dir() {
        return Err(CliError::InvalidArgument(format!(
            "{label} must be a directory: {}",
            path.display()
        )));
    }
    Ok(true)
}

fn storage_metadata_is_link_like(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;

        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn confirm_with_prompt(prompt: &str, default: bool) -> CliResult<bool> {
    let suffix = if default { "[Y/n]" } else { "[y/N]" };
    ftui_runtime::ftui_println!("{prompt} {suffix}");
    std::io::stdout().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let input = input.trim().to_ascii_lowercase();
    if input.is_empty() {
        return Ok(default);
    }
    if input == "y" || input == "yes" {
        return Ok(true);
    }
    if input == "n" || input == "no" {
        return Ok(false);
    }
    Ok(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    struct LegacyTestChild {
        child: Option<std::process::Child>,
        stop_path: PathBuf,
    }

    #[cfg(target_os = "linux")]
    impl LegacyTestChild {
        fn spawn(test_name: &str, env_key: &str, root: &Path) -> Self {
            let child = std::process::Command::new(
                std::env::current_exe().expect("resolve current test executable"),
            )
            .arg(test_name)
            .arg("--exact")
            .arg("--nocapture")
            .env(env_key, root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn legacy source child fixture");
            Self {
                child: Some(child),
                stop_path: root.join("stop"),
            }
        }

        fn wait_for_marker(&mut self, marker: &Path, label: &str) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            loop {
                if marker.exists() {
                    return;
                }
                if let Some(status) = self
                    .child
                    .as_mut()
                    .expect("child still present")
                    .try_wait()
                    .expect("probe child status")
                {
                    panic!("legacy source child exited before {label}: {status}");
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "timed out waiting for legacy source child {label}"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }

        fn finish(mut self, witness: &str) {
            fs::write(&self.stop_path, b"stop").expect("signal legacy source child to stop");
            let output = self
                .child
                .take()
                .expect("child still present")
                .wait_with_output()
                .expect("wait for legacy source child");
            assert!(
                output.status.success(),
                "legacy source child failed: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).contains(witness),
                "legacy source child filter ran no causal fixture: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for LegacyTestChild {
        fn drop(&mut self) {
            let _ = fs::write(&self.stop_path, b"stop");
            if let Some(child) = self.child.as_mut() {
                let _ = child.wait();
            }
        }
    }

    fn sample_receipt(created_at: &str, target_db: &str) -> LegacyImportReceipt {
        let mut counts = BTreeMap::new();
        counts.insert("messages".to_string(), 1);
        LegacyImportReceipt {
            receipt_version: LEGACY_IMPORT_RECEIPT_VERSION,
            outcome: LEGACY_IMPORT_OUTCOME_SUCCEEDED.to_string(),
            failure_reason: None,
            created_at: created_at.to_string(),
            mode: ImportMode::Copy,
            search_root: "/tmp/project".to_string(),
            source_db: "/tmp/storage.sqlite3".to_string(),
            source_storage_root: "/tmp/storage-root".to_string(),
            target_db: target_db.to_string(),
            target_storage_root: "/tmp/storage-root".to_string(),
            migrated_migration_ids: vec!["20260216_add_indexes".to_string()],
            integrity_check_ok: true,
            core_table_counts: counts,
            setup_refresh_ok: true,
            warnings: vec![],
        }
    }

    #[test]
    fn parse_env_file_map_parses_key_values() {
        let map = parse_env_file_map(
            "DATABASE_URL=sqlite+aiosqlite:///./storage.sqlite3\nSTORAGE_ROOT=~/.mcp_agent_mail_git_mailbox_repo\n",
        );
        assert_eq!(
            map.get("DATABASE_URL").unwrap(),
            "sqlite+aiosqlite:///./storage.sqlite3"
        );
        assert_eq!(
            map.get("STORAGE_ROOT").unwrap(),
            "~/.mcp_agent_mail_git_mailbox_repo"
        );
    }

    #[test]
    fn parse_env_file_map_parses_export_prefix() {
        let map = parse_env_file_map(
            "export DATABASE_URL=sqlite+aiosqlite:///./storage.sqlite3\nexport STORAGE_ROOT=~/mailbox\n",
        );
        assert_eq!(
            map.get("DATABASE_URL").unwrap(),
            "sqlite+aiosqlite:///./storage.sqlite3"
        );
        assert_eq!(map.get("STORAGE_ROOT").unwrap(), "~/mailbox");
    }

    #[test]
    fn parse_env_file_map_parses_export_with_tabs() {
        let map = parse_env_file_map(
            "export\tDATABASE_URL=sqlite+aiosqlite:///./tabbed.sqlite3\n",
        );
        assert_eq!(
            map.get("DATABASE_URL").unwrap(),
            "sqlite+aiosqlite:///./tabbed.sqlite3"
        );
    }

    #[test]
    fn resolved_legacy_paths_consume_one_retained_project_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let project_env = tmp.path().join(".env");
        fs::write(
            &project_env,
            "DATABASE_URL=sqlite+aiosqlite:///./original.sqlite3\nSTORAGE_ROOT=./original-storage\n",
        )
        .unwrap();

        let snapshot = capture_legacy_env_snapshot_with_reader(
            tmp.path(),
            &[],
            true,
            true,
            read_env_authority_text,
        )
        .expect("capture one project env snapshot");
        fs::write(
            &project_env,
            "DATABASE_URL=sqlite+aiosqlite:///./changed.sqlite3\nSTORAGE_ROOT=./changed-storage\n",
        )
        .unwrap();

        let database =
            resolve_database_path_from_snapshot(tmp.path(), None, None, &snapshot).unwrap();
        let storage =
            resolve_storage_root_from_snapshot(tmp.path(), None, None, &snapshot).unwrap();
        assert_eq!(database.source, ResolvedSource::ProjectEnv);
        assert_eq!(database.path, tmp.path().join("original.sqlite3"));
        assert_eq!(storage.source, ResolvedSource::ProjectEnv);
        assert_eq!(storage.path, tmp.path().join("original-storage"));
    }

    #[test]
    fn explicit_and_process_values_remain_above_env_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let snapshot = LegacyEnvSnapshot {
            project: Some(EnvAuthoritySnapshot {
                path: tmp.path().join(".env"),
                values: parse_env_file_map(
                    "DATABASE_URL=sqlite:///project.sqlite3\nSTORAGE_ROOT=./project-storage\n",
                ),
            }),
            user: None,
        };
        let explicit_db = tmp.path().join("explicit.sqlite3");

        let database = resolve_database_path_from_snapshot(
            tmp.path(),
            Some(&explicit_db),
            Some("sqlite:///process.sqlite3"),
            &snapshot,
        )
        .unwrap();
        let storage = resolve_storage_root_from_snapshot(
            tmp.path(),
            None,
            Some("./process-storage"),
            &snapshot,
        )
        .unwrap();
        assert_eq!(database.source, ResolvedSource::Explicit);
        assert_eq!(database.path, explicit_db);
        assert_eq!(storage.source, ResolvedSource::ProcessEnv);
        assert_eq!(storage.path, tmp.path().join("process-storage"));
    }

    #[test]
    fn blank_project_authority_fails_closed_without_user_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let snapshot = LegacyEnvSnapshot {
            project: Some(EnvAuthoritySnapshot {
                path: tmp.path().join(".env"),
                values: parse_env_file_map("DATABASE_URL=\nSTORAGE_ROOT='   '\n"),
            }),
            user: Some(EnvAuthoritySnapshot {
                path: tmp.path().join("user.env"),
                values: parse_env_file_map(
                    "DATABASE_URL=sqlite:///fallback.sqlite3\nSTORAGE_ROOT=./fallback-storage\n",
                ),
            }),
        };

        let database_error =
            resolve_database_path_from_snapshot(tmp.path(), None, None, &snapshot)
                .expect_err("blank project DATABASE_URL must not fall through");
        let storage_error =
            resolve_storage_root_from_snapshot(tmp.path(), None, None, &snapshot)
                .expect_err("blank project STORAGE_ROOT must not fall through");
        assert!(database_error.to_string().contains("DATABASE_URL authority"));
        assert!(storage_error.to_string().contains("STORAGE_ROOT authority"));
    }

    #[test]
    fn legacy_env_snapshot_rejects_project_leaf_content_drift() {
        let tmp = tempfile::tempdir().unwrap();
        let project_env = tmp.path().join(".env");
        fs::write(&project_env, "DATABASE_URL=sqlite:///first.sqlite3\n").unwrap();
        let changed = std::cell::Cell::new(false);

        let error = capture_legacy_env_snapshot_with_reader(
            tmp.path(),
            &[],
            true,
            false,
            |path| {
                let observed = read_env_authority_text(path)?;
                if path == project_env && observed.is_some() && !changed.replace(true) {
                    fs::write(path, "DATABASE_URL=sqlite:///second.sqlite3\n")?;
                }
                Ok(observed)
            },
        )
        .expect_err("a drifting project authority must be rejected");
        assert!(error.to_string().contains("changed while capturing"));
    }

    #[test]
    fn legacy_env_snapshot_rejects_invalid_utf8_and_oversized_project_authorities() {
        let invalid_root = tempfile::tempdir().unwrap();
        fs::write(invalid_root.path().join(".env"), [0xff, 0xfe]).unwrap();
        let invalid_error = capture_legacy_env_snapshot_with_reader(
            invalid_root.path(),
            &[],
            true,
            false,
            read_env_authority_text,
        )
        .expect_err("invalid UTF-8 must be rejected");
        assert!(invalid_error.to_string().contains("valid UTF-8"));

        let oversized_root = tempfile::tempdir().unwrap();
        let oversized_len = usize::try_from(
            mcp_agent_mail_core::config::ENV_AUTHORITY_FILE_MAX_BYTES + 1,
        )
        .unwrap();
        fs::write(oversized_root.path().join(".env"), vec![b'x'; oversized_len]).unwrap();
        let oversized_error = capture_legacy_env_snapshot_with_reader(
            oversized_root.path(),
            &[],
            true,
            false,
            read_env_authority_text,
        )
        .expect_err("oversized env authority must be rejected");
        assert!(oversized_error.to_string().contains("exceeding"));
    }

    #[test]
    fn legacy_env_snapshot_rejects_higher_user_authority_appearing_during_selection() {
        let tmp = tempfile::tempdir().unwrap();
        let user_dir = tmp.path().join("user-config");
        fs::create_dir(&user_dir).unwrap();
        let primary = user_dir.join("config.env");
        let fallback = user_dir.join("fallback.env");
        fs::write(&fallback, "STORAGE_ROOT=./fallback\n").unwrap();
        let appeared = std::cell::Cell::new(false);

        let error = capture_legacy_env_snapshot_with_reader(
            tmp.path(),
            &[primary.clone(), fallback.clone()],
            false,
            true,
            |path| {
                let observed = read_env_authority_text(path)?;
                if path == fallback && observed.is_some() && !appeared.replace(true) {
                    fs::write(&primary, "STORAGE_ROOT=./higher\n")?;
                }
                Ok(observed)
            },
        )
        .expect_err("an appearing higher-priority authority must stop fallback");
        assert!(error.to_string().contains("changed while capturing"));
    }

    #[cfg(unix)]
    #[test]
    fn legacy_env_snapshot_rejects_project_leaf_symlink() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("real-project.env");
        fs::write(&target, "DATABASE_URL=sqlite:///redirected.sqlite3\n").unwrap();
        symlink(&target, tmp.path().join(".env")).unwrap();

        let error = capture_legacy_env_snapshot_with_reader(
            tmp.path(),
            &[],
            true,
            false,
            read_env_authority_text,
        )
        .expect_err("a symlinked project authority must be rejected");
        assert!(error.to_string().contains("unsafe legacy environment authority"));
    }

    #[cfg(unix)]
    #[test]
    fn legacy_env_snapshot_rejects_project_parent_swap() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let moved_project = tmp.path().join("project-bound");
        fs::create_dir(&project).unwrap();
        let project_env = project.join(".env");
        fs::write(&project_env, "DATABASE_URL=sqlite:///source.sqlite3\n").unwrap();
        let swapped = std::cell::Cell::new(false);

        let error = capture_legacy_env_snapshot_with_reader(
            &project,
            &[],
            true,
            false,
            |path| {
                let observed = read_env_authority_text(path)?;
                if path == project_env && observed.is_some() && !swapped.replace(true) {
                    fs::rename(&project, &moved_project)?;
                    symlink(&moved_project, &project)?;
                }
                Ok(observed)
            },
        )
        .expect_err("a swapped project parent must be rejected");
        assert!(error.to_string().contains("unsafe legacy environment authority"));
    }

    #[cfg(unix)]
    #[test]
    fn legacy_env_snapshot_rejects_user_leaf_symlink_without_fallback() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let user_dir = tmp.path().join("user-config");
        fs::create_dir(&user_dir).unwrap();
        let target = user_dir.join("real.env");
        let primary = user_dir.join("config.env");
        let fallback = user_dir.join("fallback.env");
        fs::write(&target, "STORAGE_ROOT=./redirected\n").unwrap();
        fs::write(&fallback, "STORAGE_ROOT=./fallback\n").unwrap();
        symlink(&target, &primary).unwrap();

        let error = capture_legacy_env_snapshot_with_reader(
            tmp.path(),
            &[primary, fallback],
            false,
            true,
            read_env_authority_text,
        )
        .expect_err("an unsafe higher-priority user authority must stop fallback");
        assert!(error.to_string().contains("unsafe legacy environment authority"));
    }

    #[cfg(unix)]
    #[test]
    fn legacy_env_snapshot_rejects_user_parent_swap() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let user_dir = tmp.path().join("user-config");
        let moved_user_dir = tmp.path().join("user-config-bound");
        fs::create_dir(&user_dir).unwrap();
        let user_env = user_dir.join("config.env");
        fs::write(&user_env, "STORAGE_ROOT=./source-storage\n").unwrap();
        let swapped = std::cell::Cell::new(false);

        let error = capture_legacy_env_snapshot_with_reader(
            tmp.path(),
            std::slice::from_ref(&user_env),
            false,
            true,
            |path| {
                let observed = read_env_authority_text(path)?;
                if path == user_env && observed.is_some() && !swapped.replace(true) {
                    fs::rename(&user_dir, &moved_user_dir)?;
                    symlink(&moved_user_dir, &user_dir)?;
                }
                Ok(observed)
            },
        )
        .expect_err("a swapped user authority parent must be rejected");
        assert!(error.to_string().contains("unsafe legacy environment authority"));
    }

    #[test]
    fn parse_database_value_supports_sqlite_aiosqlite() {
        let tmp = tempfile::tempdir().unwrap();
        let parsed = parse_database_value(
            "sqlite+aiosqlite:///./legacy.db",
            tmp.path(),
            ResolvedSource::Default,
        )
        .unwrap();
        assert_eq!(parsed.path, tmp.path().join("legacy.db"));
    }

    #[test]
    fn legacy_database_authorities_reject_empty_or_whitespace_values() {
        let root = tempfile::tempdir().unwrap();
        for value in ["", " ", "\t\r\n"] {
            let error = parse_database_value(value, root.path(), ResolvedSource::ProcessEnv)
                .expect_err("blank DATABASE_URL authority must fail closed");
            assert!(
                error.to_string().contains("DATABASE_URL authority must not be empty"),
                "{error}"
            );
        }

        let error = resolve_database_path_from_snapshot(
            root.path(),
            Some(Path::new("   ")),
            None,
            &LegacyEnvSnapshot::default(),
        )
        .expect_err("blank explicit database authority must fail closed");
        assert!(error.to_string().contains("DATABASE_URL authority must not be empty"));
    }

    #[test]
    fn legacy_storage_authorities_reject_empty_or_whitespace_values() {
        let root = tempfile::tempdir().unwrap();
        for source in [
            ResolvedSource::ProcessEnv,
            ResolvedSource::ProjectEnv,
            ResolvedSource::UserEnv,
        ] {
            for value in ["", " ", "\t\r\n"] {
                let error = parse_storage_value(value, root.path(), source)
                    .expect_err("blank STORAGE_ROOT authority must fail closed");
                assert!(
                    error.to_string().contains("STORAGE_ROOT authority must not be empty"),
                    "{error}"
                );
            }
        }

        let error = resolve_storage_root_from_snapshot(
            root.path(),
            Some(Path::new("   ")),
            None,
            &LegacyEnvSnapshot::default(),
        )
        .expect_err("blank explicit storage authority must fail closed");
        assert!(error.to_string().contains("STORAGE_ROOT authority must not be empty"));
    }

    #[test]
    fn parse_database_value_prefers_absolute_candidate_for_missing_bare_relative_sqlite_url() {
        let search_root = tempfile::tempdir().unwrap();
        let db_home = tempfile::tempdir().unwrap();
        let absolute_db = db_home.path().join("legacy-url.sqlite3");
        fs::write(&absolute_db, b"sqlite").unwrap();

        let relative_path = absolute_db
            .to_string_lossy()
            .trim_start_matches('/')
            .to_string();
        assert!(
            !search_root.path().join(&relative_path).exists(),
            "search-root relative target should be absent so absolute candidate fallback is exercised"
        );

        let parsed = parse_database_value(
            &format!("sqlite://{}", relative_path),
            search_root.path(),
            ResolvedSource::Default,
        )
        .unwrap();
        assert_eq!(parsed.path, absolute_db);
    }

    #[test]
    fn parse_database_value_keeps_explicit_relative_sqlite_url_under_search_root() {
        let search_root = tempfile::tempdir().unwrap();
        let expected = search_root.path().join("legacy.db");

        let parsed = parse_database_value(
            "sqlite+aiosqlite:///./legacy.db",
            search_root.path(),
            ResolvedSource::Default,
        )
        .unwrap();

        assert_eq!(parsed.path, expected);
    }

    #[test]
    fn default_copy_targets_are_distinct() {
        let db = PathBuf::from("/tmp/storage.sqlite3");
        let storage = PathBuf::from("/tmp/.mcp_agent_mail_git_mailbox_repo");
        assert_ne!(default_copy_target_db(&db), db);
        assert_ne!(default_copy_target_storage(&storage), storage);
    }

    #[test]
    fn resolve_database_path_explicit_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let explicit = tmp.path().join("explicit.sqlite3");
        fs::write(&explicit, b"sqlite").unwrap();
        let resolved = resolve_database_path(tmp.path(), Some(explicit.as_path())).unwrap();
        assert_eq!(resolved.source, ResolvedSource::Explicit);
        assert_eq!(resolved.path, explicit);
    }

    #[test]
    fn resolve_storage_root_explicit_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let explicit = tmp.path().join("legacy-storage");
        fs::create_dir_all(&explicit).unwrap();
        let resolved = resolve_storage_root(tmp.path(), Some(explicit.as_path())).unwrap();
        assert_eq!(resolved.source, ResolvedSource::Explicit);
        assert_eq!(resolved.path, explicit);
    }

    #[test]
    fn legacy_env_snapshot_prefers_first_user_authority_candidate() {
        let tmp = tempfile::tempdir().unwrap();
        let portable = tmp.path().join(".config/mcp-agent-mail");
        let native = tmp
            .path()
            .join("Library/Application Support")
            .join("mcp-agent-mail");
        fs::create_dir_all(&portable).unwrap();
        fs::create_dir_all(&native).unwrap();
        fs::write(
            portable.join("config.env"),
            "DATABASE_URL=sqlite:////portable.sqlite3\n",
        )
        .unwrap();
        fs::write(
            native.join("config.env"),
            "DATABASE_URL=sqlite:////native.sqlite3\n",
        )
        .unwrap();

        let portable_env = portable.join("config.env");
        let native_env = native.join("config.env");
        let snapshot = capture_legacy_env_snapshot_with_reader(
            tmp.path(),
            &[portable_env.clone(), native_env],
            true,
            false,
            read_env_authority_text,
        )
        .expect("capture env snapshot");
        assert_eq!(
            snapshot.user.as_ref().map(|authority| &authority.path),
            Some(&portable_env)
        );
        assert_eq!(
            snapshot
                .user
                .as_ref()
                .and_then(|authority| authority.values.get("DATABASE_URL"))
                .map(String::as_str),
            Some("sqlite:////portable.sqlite3")
        );
    }

    #[test]
    fn build_import_plan_generates_distinct_default_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("legacy.sqlite3");
        let storage = tmp.path().join("legacy-storage");
        fs::write(&db, b"sqlite").unwrap();
        fs::create_dir_all(&storage).unwrap();
        let plan = build_import_plan(&ImportOptions {
            auto: false,
            search_root: Some(tmp.path().to_path_buf()),
            db: Some(db.clone()),
            storage_root: Some(storage.clone()),
            target_db: None,
            target_storage_root: None,
            dry_run: true,
            yes: true,
        })
        .unwrap();
        assert_eq!(plan.mode, ImportMode::Copy);
        assert_ne!(plan.source_db, plan.target_db);
        assert_ne!(plan.source_storage_root, plan.target_storage_root);
        assert!(
            plan.target_db
                .to_string_lossy()
                .contains(".rust-copy.sqlite3")
        );
        assert!(
            plan.target_storage_root
                .to_string_lossy()
                .contains("-rust-copy")
        );
    }

    #[test]
    fn build_import_plan_copy_rejects_same_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("legacy.sqlite3");
        let storage = tmp.path().join("legacy-storage");
        fs::write(&db, b"sqlite").unwrap();
        fs::create_dir_all(&storage).unwrap();
        let err = build_import_plan(&ImportOptions {
            auto: false,
            search_root: Some(tmp.path().to_path_buf()),
            db: Some(db.clone()),
            storage_root: Some(storage.clone()),
            target_db: Some(db),
            target_storage_root: Some(storage),
            dry_run: true,
            yes: true,
        })
        .unwrap_err();
        match err {
            CliError::InvalidArgument(msg) => {
                assert!(msg.contains("target DB path different"));
            }
            other => panic!("expected invalid argument, got {other:?}"),
        }
    }

    #[test]
    fn build_import_plan_rejects_source_db_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let source_db_dir = tmp.path().join("legacy.sqlite3");
        let source_storage = tmp.path().join("legacy-storage");
        fs::create_dir_all(&source_db_dir).unwrap();
        fs::create_dir_all(&source_storage).unwrap();

        let err = build_import_plan(&ImportOptions {
            auto: false,
            search_root: Some(tmp.path().to_path_buf()),
            db: Some(source_db_dir.clone()),
            storage_root: Some(source_storage),
            target_db: None,
            target_storage_root: None,
            dry_run: true,
            yes: true,
        })
        .unwrap_err();

        match err {
            CliError::InvalidArgument(msg) => {
                assert!(msg.contains("source DB must be a file path"));
                assert!(msg.contains(&source_db_dir.display().to_string()));
            }
            other => panic!("expected invalid argument, got {other:?}"),
        }
    }

    #[test]
    fn build_import_plan_rejects_source_storage_file() {
        let tmp = tempfile::tempdir().unwrap();
        let source_db = tmp.path().join("legacy.sqlite3");
        let source_storage_file = tmp.path().join("legacy-storage");
        fs::write(&source_db, b"sqlite").unwrap();
        fs::write(&source_storage_file, b"not-a-directory").unwrap();

        let err = build_import_plan(&ImportOptions {
            auto: false,
            search_root: Some(tmp.path().to_path_buf()),
            db: Some(source_db),
            storage_root: Some(source_storage_file.clone()),
            target_db: None,
            target_storage_root: None,
            dry_run: true,
            yes: true,
        })
        .unwrap_err();

        match err {
            CliError::InvalidArgument(msg) => {
                assert!(msg.contains("source storage root must be a directory"));
                assert!(msg.contains(&source_storage_file.display().to_string()));
            }
            other => panic!("expected invalid argument, got {other:?}"),
        }
    }

    #[test]
    fn build_import_plan_copy_rejects_existing_target_db() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("legacy.sqlite3");
        let storage = tmp.path().join("legacy-storage");
        let target_db = tmp.path().join("existing-target.sqlite3");
        fs::write(&db, b"sqlite").unwrap();
        fs::create_dir_all(&storage).unwrap();
        fs::write(&target_db, b"existing").unwrap();

        let err = build_import_plan(&ImportOptions {
            auto: false,
            search_root: Some(tmp.path().to_path_buf()),
            db: Some(db),
            storage_root: Some(storage),
            target_db: Some(target_db.clone()),
            target_storage_root: Some(tmp.path().join("target-storage")),
            dry_run: true,
            yes: true,
        })
        .unwrap_err();

        match err {
            CliError::InvalidArgument(msg) => {
                assert!(msg.contains("target DB path that does not already exist"));
                assert!(msg.contains(&target_db.display().to_string()));
            }
            other => panic!("expected invalid argument, got {other:?}"),
        }
    }

    #[test]
    fn build_import_plan_copy_rejects_target_storage_file() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("legacy.sqlite3");
        let storage = tmp.path().join("legacy-storage");
        let target_storage_file = tmp.path().join("target-storage");
        fs::write(&db, b"sqlite").unwrap();
        fs::create_dir_all(&storage).unwrap();
        fs::write(&target_storage_file, b"not-a-directory").unwrap();

        let err = build_import_plan(&ImportOptions {
            auto: false,
            search_root: Some(tmp.path().to_path_buf()),
            db: Some(db),
            storage_root: Some(storage),
            target_db: Some(tmp.path().join("target.sqlite3")),
            target_storage_root: Some(target_storage_file.clone()),
            dry_run: true,
            yes: true,
        })
        .unwrap_err();

        match err {
            CliError::InvalidArgument(msg) => {
                assert!(msg.contains("target storage root to be a directory path"));
                assert!(msg.contains(&target_storage_file.display().to_string()));
            }
            other => panic!("expected invalid argument, got {other:?}"),
        }
    }

    #[test]
    fn build_import_plan_copy_rejects_nested_target_storage() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("legacy.sqlite3");
        let storage = tmp.path().join("legacy-storage");
        let nested_target_storage = storage.join("nested-target");
        fs::write(&db, b"sqlite").unwrap();
        fs::create_dir_all(&storage).unwrap();

        let err = build_import_plan(&ImportOptions {
            auto: false,
            search_root: Some(tmp.path().to_path_buf()),
            db: Some(db),
            storage_root: Some(storage),
            target_db: Some(tmp.path().join("target.sqlite3")),
            target_storage_root: Some(nested_target_storage),
            dry_run: true,
            yes: true,
        })
        .unwrap_err();

        match err {
            CliError::InvalidArgument(msg) => {
                assert!(msg.contains("target storage root to be outside source storage root"));
            }
            other => panic!("expected invalid argument, got {other:?}"),
        }
    }

    #[test]
    fn build_detect_report_marks_pyproject_signal() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("pyproject.toml"),
            "[project]\nname = \"mcp-agent-mail\"\n",
        )
        .unwrap();
        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("MOCK_AM_BINARY", "")],
            || {
                let report = build_detect_report(tmp.path(), None, None).unwrap();
                assert!(report.detected);
                assert!(
                    report
                        .markers
                        .iter()
                        .any(|marker| marker.id == "pyproject_package")
                );
            },
        );
    }

    #[test]
    fn build_detect_report_marks_legacy_storage_only_env_signal() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join(".env"),
            "STORAGE_ROOT=~/.mcp_agent_mail_git_mailbox_repo\n",
        )
        .unwrap();
        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("MOCK_AM_BINARY", "")],
            || {
                let report = build_detect_report(tmp.path(), None, None).unwrap();
                assert!(
                    report
                        .markers
                        .iter()
                        .any(|marker| marker.id == "legacy_env_defaults")
                );
            },
        );
    }

    #[test]
    fn write_receipt_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let receipt = sample_receipt("2026-02-17T00:00:00Z", "/tmp/storage.sqlite3");
        write_receipt(tmp.path(), &receipt, "20260217T000000Z").unwrap();
        let receipt_path = tmp
            .path()
            .join("legacy_import_receipts")
            .join("legacy_import_20260217T000000Z.json");
        assert!(receipt_path.exists());
        let parsed: LegacyImportReceipt =
            serde_json::from_str(&fs::read_to_string(receipt_path).unwrap()).unwrap();
        assert_eq!(parsed.receipt_version, LEGACY_IMPORT_RECEIPT_VERSION);
        assert_eq!(parsed.outcome, LEGACY_IMPORT_OUTCOME_SUCCEEDED);
        assert!(parsed.failure_reason.is_none());
        assert_eq!(parsed.mode, ImportMode::Copy);
        assert_eq!(parsed.source_db, "/tmp/storage.sqlite3");
    }

    #[test]
    fn status_reader_tolerates_v1_receipts_without_outcome_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let receipts_dir = tmp.path().join("legacy_import_receipts");
        fs::create_dir_all(&receipts_dir).unwrap();
        // A verbatim v1 receipt: no `outcome`, no `failure_reason`.
        let v1_json = r#"{
            "receipt_version": 1,
            "created_at": "2026-02-17T00:00:00Z",
            "mode": "copy",
            "search_root": "/tmp/project",
            "source_db": "/tmp/storage.sqlite3",
            "source_storage_root": "/tmp/storage-root",
            "target_db": "/tmp/target.sqlite3",
            "target_storage_root": "/tmp/target-root",
            "migrated_migration_ids": [],
            "integrity_check_ok": true,
            "core_table_counts": {},
            "setup_refresh_ok": true,
            "warnings": []
        }"#;
        fs::write(
            receipts_dir.join("legacy_import_20260217T000000Z.json"),
            v1_json,
        )
        .unwrap();

        let report = collect_status_report(tmp.path()).unwrap();
        assert_eq!(report.receipt_count, 1);
        let latest = report.latest_receipt.expect("v1 receipt should parse");
        assert_eq!(latest.receipt_version, 1);
        assert_eq!(latest.outcome, LEGACY_IMPORT_OUTCOME_SUCCEEDED);
        assert!(latest.failure_reason.is_none());
    }

    #[test]
    fn write_receipt_avoids_timestamp_collision_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let first = sample_receipt("2026-02-17T00:00:00Z", "/tmp/first.sqlite3");
        let second = sample_receipt("2026-02-17T00:00:01Z", "/tmp/second.sqlite3");
        write_receipt(tmp.path(), &first, "20260217T000000Z").unwrap();
        write_receipt(tmp.path(), &second, "20260217T000000Z").unwrap();

        let receipts_dir = tmp.path().join("legacy_import_receipts");
        let path_primary = receipts_dir.join("legacy_import_20260217T000000Z.json");
        let path_collision = receipts_dir.join("legacy_import_20260217T000000Z_1.json");
        assert!(path_primary.exists(), "primary receipt path should exist");
        assert!(
            path_collision.exists(),
            "collision receipt path should exist"
        );

        let parsed_primary: LegacyImportReceipt =
            serde_json::from_str(&fs::read_to_string(path_primary).unwrap()).unwrap();
        let parsed_collision: LegacyImportReceipt =
            serde_json::from_str(&fs::read_to_string(path_collision).unwrap()).unwrap();
        assert_eq!(parsed_primary.target_db, "/tmp/first.sqlite3");
        assert_eq!(parsed_collision.target_db, "/tmp/second.sqlite3");
    }

    #[test]
    fn collect_status_report_returns_zero_for_missing_receipts_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let report = collect_status_report(tmp.path()).unwrap();
        assert_eq!(report.receipt_count, 0);
        assert!(report.latest_receipt.is_none());
    }

    #[test]
    fn collect_status_report_returns_latest_receipt() {
        let tmp = tempfile::tempdir().unwrap();
        let older = sample_receipt("2026-02-16T01:00:00Z", "/tmp/older.sqlite3");
        let newer = sample_receipt("2026-02-17T01:00:00Z", "/tmp/newer.sqlite3");
        write_receipt(tmp.path(), &older, "20260216T010000Z").unwrap();
        write_receipt(tmp.path(), &newer, "20260217T010000Z").unwrap();

        let report = collect_status_report(tmp.path()).unwrap();
        assert_eq!(report.receipt_count, 2);
        let latest = report.latest_receipt.expect("latest receipt missing");
        assert_eq!(latest.target_db, "/tmp/newer.sqlite3");
        assert_eq!(latest.created_at, "2026-02-17T01:00:00Z");
    }

    #[test]
    fn paths_overlap_detects_nested_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let nested = source.join("nested");
        let sibling = tmp.path().join("sibling");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(&sibling).unwrap();

        assert!(paths_overlap(&source, &nested));
        assert!(paths_overlap(&nested, &source));
        assert!(!paths_overlap(&source, &sibling));
    }

    #[test]
    fn paths_overlap_handles_parent_segments_for_sibling_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let sibling_via_parent = source.join("..").join("sibling");
        fs::create_dir_all(&source).unwrap();

        assert!(!paths_overlap(&source, &sibling_via_parent));
    }

    #[cfg(unix)]
    #[test]
    fn paths_overlap_resolves_symlink_before_parent_segments() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().expect("tempdir");
        let source = tmp.path().join("source");
        let nested = source.join("nested");
        let link = tmp.path().join("nested-link");
        fs::create_dir_all(&nested).expect("create nested source");
        symlink(&nested, &link).expect("create nested symlink");

        let disguised_child = link.join("..").join("not-created-yet");
        assert!(
            paths_overlap(&source, &disguised_child),
            "symlink resolution must happen before lexical '..' normalization"
        );
    }

    fn seed_v20_agents_fixture(path: &Path) {
        use mcp_agent_mail_db::sqlmodel_core::Value;

        let conn = CanonicalDbConn::open_file(path.display().to_string())
            .expect("open canonical v20 fixture DB");
        conn.execute_raw("PRAGMA foreign_keys = OFF")
            .expect("disable fixture foreign keys");
        conn.execute_raw(&schema::init_schema_sql_base())
            .expect("create current base tables for v20 fixture");
        conn.execute_raw("DROP TABLE agents")
            .expect("replace agents with Python v20 shape");
        conn.execute_raw(
            "CREATE TABLE agents (\
                id INTEGER NOT NULL,\
                project_id INTEGER NOT NULL,\
                name VARCHAR(128) NOT NULL,\
                program VARCHAR(128) NOT NULL,\
                model VARCHAR(128) NOT NULL,\
                task_description TEXT NOT NULL DEFAULT '',\
                inception_ts INTEGER NOT NULL,\
                last_active_ts INTEGER NOT NULL,\
                attachments_policy VARCHAR(32) NOT NULL DEFAULT 'auto',\
                contact_policy VARCHAR(32) NOT NULL DEFAULT 'auto',\
                reaper_exempt INTEGER NOT NULL DEFAULT 0,\
                registration_token VARCHAR(64),\
                retired_at DATETIME,\
                PRIMARY KEY (id),\
                CONSTRAINT uq_agent_project_name UNIQUE (project_id, name),\
                FOREIGN KEY (project_id) REFERENCES projects (id)\
            )",
        )
        .expect("create Python v20 agents table");
        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?, ?, ?, ?)",
            &[
                Value::BigInt(1),
                Value::Text("v20-project".to_string()),
                Value::Text("/tmp/v20-project".to_string()),
                Value::BigInt(1),
            ],
        )
        .expect("insert v20 project");
        conn.execute_sync(
            "INSERT INTO agents (\
                 id, project_id, name, program, model, task_description, inception_ts, last_active_ts\
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                Value::BigInt(1),
                Value::BigInt(1),
                Value::Text("V20Agent".to_string()),
                Value::Text("python".to_string()),
                Value::Text("legacy".to_string()),
                Value::Text("v20 source fixture".to_string()),
                Value::BigInt(1),
                Value::BigInt(1),
            ],
        )
        .expect("insert v20 agent");
        conn.execute_raw(
            "CREATE TABLE mcp_agent_mail_migrations (\
                id TEXT PRIMARY KEY,\
                description TEXT NOT NULL,\
                applied_at INTEGER NOT NULL\
            )",
        )
        .expect("create migration ledger");
        for migration in schema::schema_migrations_base() {
            if matches!(
                migration.id.as_str(),
                "v20_agents_registration_token" | "v20_idx_agents_registration_token"
            ) {
                continue;
            }
            conn.execute_sync(
                "INSERT INTO mcp_agent_mail_migrations (id, description, applied_at) VALUES (?, ?, ?)",
                &[
                    Value::Text(migration.id),
                    Value::Text(migration.description),
                    Value::BigInt(0),
                ],
            )
            .expect("seed already-applied migration");
        }
    }

    #[test]
    fn legacy_import_v20_autoindex_fixture_preserves_source_and_migrates_copy() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let source_db = tmp.path().join("legacy-v20.sqlite3");
        let source_storage = tmp.path().join("legacy-storage");
        let target_db = tmp.path().join("rust-copy.sqlite3");
        let target_storage = tmp.path().join("rust-storage");
        fs::create_dir_all(&source_storage).expect("create source storage");
        fs::write(source_storage.join("message.json"), "legacy archive")
            .expect("seed source storage");
        seed_v20_agents_fixture(&source_db);

        let source_conn =
            open_canonical_read_only(&source_db).expect("open source fixture read-only");
        let indexes = source_conn
            .query_sync("PRAGMA index_list(agents)", &[])
            .expect("inspect implicit agents indexes");
        assert!(
            indexes.iter().any(|row| {
                row.get_named::<String>("name")
                    .is_ok_and(|name| name == "sqlite_autoindex_agents_1")
            }),
            "fixture must carry the implicit UNIQUE autoindex that v20 preflight reconstructs"
        );
        drop(source_conn);
        let source_bytes_before = fs::read(&source_db).expect("read source fixture bytes");

        let plan = build_import_plan(&ImportOptions {
            auto: false,
            search_root: Some(tmp.path().to_path_buf()),
            db: Some(source_db.clone()),
            storage_root: Some(source_storage.clone()),
            target_db: Some(target_db.clone()),
            target_storage_root: Some(target_storage.clone()),
            dry_run: false,
            yes: true,
        })
        .expect("build copy-only import plan");
        let receipt = execute_import(plan, false).expect("import v20 fixture into a copy");

        assert!(receipt.integrity_check_ok);
        assert!(
            receipt
                .migrated_migration_ids
                .iter()
                .any(|id| id == "v20_agents_registration_token"),
            "v20 already-satisfied preflight should be recorded on the target"
        );
        assert_eq!(
            fs::read(&source_db).expect("reread source fixture bytes"),
            source_bytes_before,
            "legacy source DB bytes must remain unchanged by import"
        );
        verify_canonical_sqlite_readable(&source_db, "source DB")
            .expect("source remains readable after import");
        verify_canonical_sqlite_readable(&target_db, "target DB")
            .expect("target remains readable after import");
        verify_runtime_sqlite_readable(&target_db, "target DB")
            .expect("target remains runtime-readable after import");
        assert!(
            target_storage.join("legacy_import_receipts").exists(),
            "successful copy import must write its receipt under target storage"
        );
    }

    #[cfg(unix)]
    #[test]
    fn copy_dir_recursive_rejects_symlinked_directories() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let nested = src.join("nested");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("file.txt"), "payload").unwrap();
        symlink(&nested, src.join("nested-link")).unwrap();

        let err = copy_dir_recursive(&src, &dst).unwrap_err();
        match err {
            CliError::InvalidArgument(msg) => {
                assert!(msg.contains("symlinked directories are not supported"));
            }
            other => panic!("expected invalid argument, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn copy_dir_recursive_rejects_broken_symlinks() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(&src).unwrap();
        symlink("/does/not/exist", src.join("broken-link")).unwrap();

        let err = copy_dir_recursive(&src, &dst).unwrap_err();
        match err {
            CliError::InvalidArgument(msg) => {
                assert!(msg.contains("broken symlink encountered"));
            }
            other => panic!("expected invalid argument, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn copy_dir_recursive_rejects_symlinked_source_root() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let real_source = tmp.path().join("real-source");
        let linked_source = tmp.path().join("linked-source");
        let destination = tmp.path().join("destination");
        fs::create_dir(&real_source).unwrap();
        fs::write(real_source.join("message.txt"), "payload").unwrap();
        symlink(&real_source, &linked_source).unwrap();

        let error = copy_dir_recursive(&linked_source, &destination)
            .expect_err("source storage root symlink must not be followed");
        assert!(error.to_string().contains("must not be a symlink"), "{error}");
        assert!(
            fs::symlink_metadata(&destination).is_err(),
            "rejected source authority must not create a destination"
        );
    }

    #[cfg(unix)]
    #[test]
    fn copy_dir_recursive_rejects_symlinked_target_root() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let outside = tmp.path().join("outside");
        let linked_destination = tmp.path().join("linked-destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(source.join("message.txt"), "payload").unwrap();
        symlink(&outside, &linked_destination).unwrap();

        let error = copy_dir_recursive(&source, &linked_destination)
            .expect_err("target storage root symlink must not be followed");
        assert!(error.to_string().contains("must not be a symlink"), "{error}");
        assert!(
            !outside.join("message.txt").exists(),
            "rejected target authority must not receive source bytes"
        );
    }

    #[cfg(unix)]
    #[test]
    fn copy_dir_recursive_rejects_unix_socket_entries() {
        use std::os::unix::net::UnixListener;

        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let destination = tmp.path().join("destination");
        fs::create_dir(&source).unwrap();
        let socket_path = source.join("agent.sock");
        let _listener = UnixListener::bind(&socket_path).unwrap();

        let error = copy_dir_recursive(&source, &destination)
            .expect_err("socket entries must not be silently skipped");
        assert!(
            error.to_string().contains("unsupported special file"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn failed_import_writes_failure_receipt_and_stages_partial_target() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().expect("tempdir");
        let source_db = tmp.path().join("legacy.sqlite3");
        let source_storage = tmp.path().join("legacy-storage");
        let target_db = tmp.path().join("rust-copy.sqlite3");
        let target_storage = tmp.path().join("rust-storage");

        // Valid SQLite source so the preflight quick_check passes and the
        // target DB copy is created; the storage copy then fails on a broken
        // symlink (existing validation), which is the cleanest failure
        // injection AFTER the partial target DB exists.
        let conn = CanonicalDbConn::open_file(source_db.display().to_string())
            .expect("create source fixture DB");
        conn.execute_raw("CREATE TABLE t (x INTEGER)")
            .expect("create fixture table");
        drop(conn);
        fs::create_dir_all(&source_storage).expect("create source storage");
        symlink("/does/not/exist", source_storage.join("broken-link"))
            .expect("seed broken symlink");

        let opts = ImportOptions {
            auto: false,
            search_root: Some(tmp.path().to_path_buf()),
            db: Some(source_db.clone()),
            storage_root: Some(source_storage.clone()),
            target_db: Some(target_db.clone()),
            target_storage_root: Some(target_storage.clone()),
            dry_run: false,
            yes: true,
        };
        let plan = build_import_plan(&opts).expect("build import plan");
        let err = execute_import(plan, false).expect_err("import must fail on broken symlink");
        let message = err.to_string();
        assert!(
            message.contains("legacy import failed"),
            "error should be annotated: {message}"
        );
        assert!(
            message.contains("broken symlink"),
            "original failure must be preserved: {message}"
        );
        assert!(
            message.contains(".failed-"),
            "error should name the staged partial target: {message}"
        );
        assert!(
            message.contains("failure receipt written to"),
            "error should name the failure receipt: {message}"
        );

        // The partial target DB was renamed aside (never deleted), freeing the
        // original target path for retry.
        assert!(
            !target_db.exists(),
            "original target DB path must be free again"
        );
        let staged: Vec<PathBuf> = fs::read_dir(tmp.path())
            .expect("list tempdir")
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("rust-copy.sqlite3.failed-"))
            })
            .collect();
        assert_eq!(
            staged.len(),
            1,
            "exactly one staged partial target DB expected, got {staged:?}"
        );

        // A failure receipt is discoverable via the status reader.
        let report = collect_status_report(&target_storage).expect("status report");
        assert_eq!(report.receipt_count, 1);
        let latest = report.latest_receipt.expect("failure receipt present");
        assert_eq!(latest.receipt_version, LEGACY_IMPORT_RECEIPT_VERSION);
        assert_eq!(latest.outcome, LEGACY_IMPORT_OUTCOME_FAILED);
        assert!(
            latest
                .failure_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("broken symlink")),
            "failure_reason should carry the original error: {:?}",
            latest.failure_reason
        );
        assert!(!latest.integrity_check_ok);
        assert!(latest.migrated_migration_ids.is_empty());

        // Retryability: the same options build a plan again (target DB path is
        // free; target storage root holds only the receipts directory).
        build_import_plan(&opts).expect("retry plan must build after failed import");
    }

    #[test]
    fn wal_mode_source_sidecars_untouched_by_detect_and_import() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let source_db = tmp.path().join("legacy-wal.sqlite3");
        let source_storage = tmp.path().join("legacy-storage");
        let target_db = tmp.path().join("rust-copy.sqlite3");
        let target_storage = tmp.path().join("rust-storage");
        fs::create_dir_all(&source_storage).expect("create source storage");
        fs::write(source_storage.join("message.json"), "legacy archive")
            .expect("seed source storage");
        seed_v20_agents_fixture(&source_db);

        // Simulate a live legacy server: an open writer connection holding the
        // source in WAL mode with committed rows that exist only in the -wal.
        let writer = CanonicalDbConn::open_file(source_db.display().to_string())
            .expect("open live writer on source");
        writer
            .query_sync("PRAGMA journal_mode=WAL", &[])
            .expect("switch source to WAL mode");
        writer
            .execute_raw(
                "INSERT INTO projects (id, slug, human_key, created_at) \
                 VALUES (2, 'wal-live', '/tmp/wal-live', 1); \
                 CREATE TRIGGER fts_messages_ai AFTER INSERT ON messages \
                 BEGIN SELECT 1; END;",
            )
            .expect("insert WAL-resident project row and legacy signature");

        let wal_path = PathBuf::from(format!("{}-wal", source_db.display()));
        let shm_path = PathBuf::from(format!("{}-shm", source_db.display()));
        assert!(wal_path.exists(), "fixture must have a live -wal sidecar");
        assert!(shm_path.exists(), "fixture must have a live -shm sidecar");
        let wal_bytes_before = fs::read(&wal_path).expect("read wal bytes");
        let shm_bytes_before = fs::read(&shm_path).expect("read shm bytes");
        let db_bytes_before = fs::read(&source_db).expect("read source db bytes");
        let wal_mtime_before = fs::metadata(&wal_path)
            .and_then(|m| m.modified())
            .expect("wal mtime");
        let shm_mtime_before = fs::metadata(&shm_path)
            .and_then(|m| m.modified())
            .expect("shm mtime");
        let db_mtime_before = fs::metadata(&source_db)
            .and_then(|m| m.modified())
            .expect("db mtime");

        // Detect (source DB signature inspection) must not touch the sidecars.
        let report = build_detect_report(
            tmp.path(),
            Some(source_db.as_path()),
            Some(source_storage.as_path()),
        )
        .expect("detect report");
        assert!(
            report.db_signature.as_ref().is_some_and(|sig| sig.open_ok),
            "coherent private source capture must succeed on a WAL-mode source"
        );
        assert!(
            report
                .db_signature
                .as_ref()
                .is_some_and(|sig| sig.legacy_trigger_count == 1),
            "signature detection must observe legacy DDL committed only in WAL"
        );

        // Import (verification + backup + migration of the copy) likewise.
        let plan = build_import_plan(&ImportOptions {
            auto: false,
            search_root: Some(tmp.path().to_path_buf()),
            db: Some(source_db.clone()),
            storage_root: Some(source_storage.clone()),
            target_db: Some(target_db.clone()),
            target_storage_root: Some(target_storage.clone()),
            dry_run: false,
            yes: true,
        })
        .expect("build import plan");
        let receipt = execute_import(plan, false).expect("import WAL-mode source");
        assert!(receipt.integrity_check_ok);

        assert_eq!(
            fs::read(&wal_path).expect("reread wal bytes"),
            wal_bytes_before,
            "source -wal bytes must be unchanged by detect+import"
        );
        assert_eq!(
            fs::read(&shm_path).expect("reread shm bytes"),
            shm_bytes_before,
            "source -shm bytes must be unchanged by detect+import"
        );
        assert_eq!(
            fs::read(&source_db).expect("reread source db bytes"),
            db_bytes_before,
            "source db bytes must be unchanged by detect+import"
        );
        assert_eq!(
            fs::metadata(&wal_path)
                .and_then(|m| m.modified())
                .expect("wal mtime after"),
            wal_mtime_before,
            "source -wal mtime must be unchanged by detect+import"
        );
        assert_eq!(
            fs::metadata(&shm_path)
                .and_then(|m| m.modified())
                .expect("shm mtime after"),
            shm_mtime_before,
            "source -shm mtime must be unchanged by detect+import"
        );
        assert_eq!(
            fs::metadata(&source_db)
                .and_then(|m| m.modified())
                .expect("db mtime after"),
            db_mtime_before,
            "source db mtime must be unchanged by detect+import"
        );

        // The staged-copy backup path must carry WAL-resident rows into the
        // target: both the checkpointed project and the wal-only insert.
        let target = open_canonical_read_only(&target_db).expect("open migrated target");
        let rows = target
            .query_sync("SELECT COUNT(*) AS c FROM projects", &[])
            .expect("count target projects");
        let count = rows
            .first()
            .and_then(|row| row.get_named::<i64>("c").ok())
            .unwrap_or(0);
        assert_eq!(
            count, 2,
            "WAL-resident row must survive the staged-copy backup"
        );

        drop(writer);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn legacy_source_snapshot_is_coherent_during_cross_process_wal_family_races() {
        const CHILD_ROOT_ENV: &str = "MCP_AGENT_MAIL_LEGACY_WAL_RACE_ROOT";
        const CHILD_TEST_NAME: &str = "legacy::tests::legacy_source_snapshot_is_coherent_during_cross_process_wal_family_races";
        const CHILD_WITNESS: &str = "legacy-wal-race-child-completed";

        if let Some(root) = std::env::var_os(CHILD_ROOT_ENV) {
            let root = PathBuf::from(root);
            let source_db = root.join("source.sqlite3");
            let writer = CanonicalDbConn::open_file(source_db.display().to_string())
                .expect("open cross-process WAL writer");
            writer
                .query_sync("PRAGMA journal_mode=WAL", &[])
                .expect("enable WAL mode in child");
            writer
                .execute_raw(
                    "PRAGMA wal_autocheckpoint=0; \
                     CREATE TABLE snapshot_generation(generation INTEGER NOT NULL); \
                     CREATE TABLE snapshot_rows( \
                         generation INTEGER PRIMARY KEY NOT NULL, \
                         payload BLOB NOT NULL \
                     ); \
                     INSERT INTO snapshot_generation(generation) VALUES (128); \
                     WITH RECURSIVE seq(value) AS ( \
                         SELECT 1 UNION ALL SELECT value + 1 FROM seq WHERE value < 128 \
                     ) \
                     INSERT INTO snapshot_rows(generation, payload) \
                     SELECT value, zeroblob(65536) FROM seq;",
                )
                .expect("seed WAL-only schema and rows");
            fs::write(root.join("progress"), b"128").expect("publish initial generation");
            fs::write(root.join("ready"), b"ready").expect("publish child readiness");

            while !root.join("start").exists() {
                if root.join("stop").exists() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }

            let mut generation = 128_u64;
            while !root.join("stop").exists() {
                generation = generation
                    .checked_add(1)
                    .expect("generation remains bounded");
                writer
                    .execute_raw(&format!(
                        "BEGIN IMMEDIATE; \
                         UPDATE snapshot_generation SET generation = {generation}; \
                         INSERT INTO snapshot_rows(generation, payload) \
                         VALUES ({generation}, zeroblob(65536)); \
                         COMMIT;"
                    ))
                    .expect("commit one complete WAL generation");
                fs::write(root.join("progress"), generation.to_string())
                    .expect("publish child generation");
                fs::write(root.join("grew"), b"grew").expect("publish WAL growth witness");

                writer
                    .query_sync("PRAGMA wal_checkpoint(PASSIVE)", &[])
                    .expect("run concurrent passive checkpoint");
                fs::write(root.join("checkpointed"), b"checkpointed")
                    .expect("publish checkpoint witness");

                let rows = writer
                    .query_sync("PRAGMA wal_checkpoint(TRUNCATE)", &[])
                    .expect("attempt concurrent WAL reset");
                let reset_completed = rows.first().is_some_and(|row| {
                    row.get_named::<i64>("busy").ok() == Some(0)
                        && row.get_named::<i64>("log").ok() == Some(0)
                });
                if reset_completed {
                    fs::write(root.join("reset"), b"reset").expect("publish WAL reset witness");
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }

            println!("{CHILD_WITNESS}");
            return;
        }

        fn published_generation(path: &Path) -> Option<u64> {
            fs::read_to_string(path).ok()?.parse().ok()
        }

        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        let source_db = root.join("source.sqlite3");
        let mut child = LegacyTestChild::spawn(CHILD_TEST_NAME, CHILD_ROOT_ENV, root);
        child.wait_for_marker(&root.join("ready"), "WAL fixture readiness");

        let wal_path = mcp_agent_mail_core::disk::sqlite_sidecar_path(&source_db, "-wal");
        let shm_path = mcp_agent_mail_core::disk::sqlite_sidecar_path(&source_db, "-shm");
        assert!(wal_path.exists(), "child must create a WAL sidecar");
        assert!(shm_path.exists(), "child must create an SHM sidecar");
        assert!(
            fs::metadata(&wal_path).expect("inspect WAL").len() > 32,
            "child must place committed content beyond the WAL header"
        );

        // An immutable main-only view deliberately ignores WAL. Its inability
        // to see the fixture proves that the schema and rows exercised below
        // are WAL-resident rather than accidentally checkpointed setup data.
        let main_only_uri = format!(
            "file:{}?immutable=1",
            sqlite_uri_encode_path(source_db.as_path())
        );
        let main_only_flags = {
            let mut flags = mcp_agent_mail_db::sqlmodel_sqlite::OpenFlags::read_only();
            flags.uri = true;
            flags
        };
        let main_only = CanonicalDbConn::open(
            &mcp_agent_mail_db::sqlmodel_sqlite::SqliteConfig::file(main_only_uri)
                .flags(main_only_flags),
        )
        .expect("open main-only WAL control");
        let rows = main_only
            .query_sync(
                "SELECT COUNT(*) AS c FROM sqlite_master \
                 WHERE type='table' AND name='snapshot_rows'",
                &[],
            )
            .expect("query main-only schema control");
        assert_eq!(
            rows.first().and_then(|row| row.get_named::<i64>("c").ok()),
            Some(0),
            "main-only control must not see the WAL-resident schema"
        );
        drop(main_only);

        fs::write(root.join("start"), b"start").expect("start WAL race");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let progress_path = root.join("progress");
        let mut coherent_snapshots = 0_u32;
        let mut freshness_checked_snapshots = 0_u32;
        let mut observed_commit_during_capture = false;
        let mut fail_closed_errors = Vec::new();

        while std::time::Instant::now() < deadline {
            let before = published_generation(&progress_path);
            match LegacySourceSnapshot::capture(&source_db) {
                Ok(snapshot) => {
                    let conn = open_private_immutable_source_snapshot(&snapshot)
                        .expect("open coherent retained snapshot");
                    verify_canonical_quick_check(
                        &conn,
                        snapshot.snapshot_path(),
                        "racing retained snapshot",
                    )
                    .expect("racing snapshot quick_check");
                    let rows = conn
                        .query_sync(
                            "SELECT \
                                 (SELECT generation FROM snapshot_generation) AS generation, \
                                 COUNT(*) AS row_count, \
                                 COALESCE(MAX(generation), 0) AS max_generation \
                             FROM snapshot_rows",
                            &[],
                        )
                        .expect("query cross-table generation invariant");
                    let row = rows.first().expect("generation invariant row");
                    let generation = row
                        .get_named::<i64>("generation")
                        .expect("snapshot generation");
                    let row_count = row
                        .get_named::<i64>("row_count")
                        .expect("snapshot row count");
                    let max_generation = row
                        .get_named::<i64>("max_generation")
                        .expect("snapshot max generation");
                    assert_eq!(
                        row_count, generation,
                        "snapshot must not mix the generation row with a different WAL generation"
                    );
                    assert_eq!(
                        max_generation, generation,
                        "snapshot must contain every row committed through its generation"
                    );
                    if let Some(committed_before_capture) = before {
                        assert!(
                            generation
                                >= i64::try_from(committed_before_capture)
                                    .expect("published generation fits SQLite INTEGER"),
                            "snapshot must not fall back behind the last generation committed before capture"
                        );
                        freshness_checked_snapshots = freshness_checked_snapshots.saturating_add(1);
                    }
                    coherent_snapshots = coherent_snapshots.saturating_add(1);
                }
                Err(error) => fail_closed_errors.push(error.to_string()),
            }
            let after = published_generation(&progress_path);
            observed_commit_during_capture |= before
                .zip(after)
                .is_some_and(|(before, after)| after > before);

            let exercised_family_races = root.join("grew").exists()
                && root.join("checkpointed").exists()
                && root.join("reset").exists();
            if coherent_snapshots >= 2
                && freshness_checked_snapshots >= 2
                && observed_commit_during_capture
                && exercised_family_races
            {
                break;
            }
        }

        assert!(
            coherent_snapshots >= 2,
            "expected at least two coherent snapshots while racing; fail-closed errors={fail_closed_errors:?}"
        );
        assert!(
            freshness_checked_snapshots >= 2,
            "expected at least two successful snapshots with a published pre-capture freshness bound; observed {freshness_checked_snapshots}"
        );
        assert!(
            observed_commit_during_capture,
            "child must commit a new generation during at least one capture"
        );
        assert!(
            root.join("grew").exists(),
            "WAL growth branch not exercised"
        );
        assert!(
            root.join("checkpointed").exists(),
            "WAL checkpoint branch not exercised"
        );
        assert!(
            root.join("reset").exists(),
            "WAL reset branch not exercised"
        );
        child.finish(CHILD_WITNESS);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn legacy_import_fails_closed_on_wal_only_corruption_without_target_publication() {
        const CHILD_ROOT_ENV: &str = "MCP_AGENT_MAIL_LEGACY_WAL_CORRUPTION_ROOT";
        const CHILD_TEST_NAME: &str = "legacy::tests::legacy_import_fails_closed_on_wal_only_corruption_without_target_publication";
        const CHILD_WITNESS: &str = "legacy-wal-corruption-child-completed";

        if let Some(root) = std::env::var_os(CHILD_ROOT_ENV) {
            let root = PathBuf::from(root);
            let source_db = root.join("source.sqlite3");
            let writer = CanonicalDbConn::open_file(source_db.display().to_string())
                .expect("open WAL corruption fixture writer");
            writer
                .query_sync("PRAGMA journal_mode=WAL", &[])
                .expect("enable WAL mode in corruption child");
            writer
                .execute_raw(
                    "PRAGMA wal_autocheckpoint=0; \
                     CREATE TABLE wal_only_corruption_witness(value INTEGER NOT NULL); \
                     INSERT INTO wal_only_corruption_witness(value) VALUES (73);",
                )
                .expect("commit WAL-only corruption fixture");
            fs::write(root.join("ready"), b"ready").expect("publish corruption readiness");
            while !root.join("stop").exists() {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            drop(writer);
            println!("{CHILD_WITNESS}");
            return;
        }

        use std::io::{Seek, SeekFrom};

        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        let source_db = root.join("source.sqlite3");
        let source_storage = root.join("source-storage");
        let target_db = root.join("target.sqlite3");
        let target_storage = root.join("target-storage");
        fs::create_dir_all(&source_storage).expect("create source storage");

        let mut child = LegacyTestChild::spawn(CHILD_TEST_NAME, CHILD_ROOT_ENV, root);
        child.wait_for_marker(&root.join("ready"), "WAL corruption fixture readiness");
        let wal_path = mcp_agent_mail_core::disk::sqlite_sidecar_path(&source_db, "-wal");
        let main_before = fs::read(&source_db).expect("read pristine main database");
        let wal_before = fs::read(&wal_path).expect("read pristine WAL");
        assert!(
            wal_before.len() > 32,
            "corruption fixture must contain committed WAL frames"
        );

        let mut wal = fs::OpenOptions::new()
            .write(true)
            .open(&wal_path)
            .expect("open WAL fixture for corruption plant");
        wal.seek(SeekFrom::Start(0)).expect("seek WAL magic");
        wal.write_all(&[wal_before[0] ^ 0xff])
            .expect("corrupt WAL magic only");
        wal.sync_all().expect("durably plant WAL corruption");
        drop(wal);

        assert_eq!(
            fs::read(&source_db).expect("reread main database"),
            main_before,
            "WAL corruption plant must not modify the main database"
        );
        assert_ne!(
            fs::read(&wal_path).expect("reread corrupted WAL"),
            wal_before,
            "WAL corruption plant must be mutation-sensitive"
        );

        let error = build_import_plan(&ImportOptions {
            auto: false,
            search_root: Some(root.to_path_buf()),
            db: Some(source_db.clone()),
            storage_root: Some(source_storage),
            target_db: Some(target_db.clone()),
            target_storage_root: Some(target_storage.clone()),
            dry_run: false,
            yes: true,
        })
        .expect_err("WAL-only corruption must fail before import execution");
        let error = error.to_string().to_ascii_lowercase();
        assert!(
            error.contains("wal") || error.contains("coherent") || error.contains("healthy"),
            "failure must identify the refused source-family proof: {error}"
        );
        assert!(
            !target_db.exists(),
            "failed source capture must not publish a target database"
        );
        assert!(
            !target_storage.exists(),
            "failed source capture must not publish target storage"
        );
        child.finish(CHILD_WITNESS);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn legacy_source_snapshot_preserves_same_process_franken_writer_lock() {
        const CHILD_PATH_ENV: &str = "MCP_AGENT_MAIL_LEGACY_LOCK_PATH";
        const CHILD_TEST_NAME: &str =
            "legacy::tests::legacy_source_snapshot_preserves_same_process_franken_writer_lock";
        const CHILD_WITNESS: &str = "legacy-source-snapshot-child-observed-busy";

        if let Some(path) = std::env::var_os(CHILD_PATH_ENV) {
            let config = mcp_agent_mail_db::sqlmodel_sqlite::SqliteConfig::file(
                PathBuf::from(path).to_string_lossy().into_owned(),
            )
            .flags(mcp_agent_mail_db::sqlmodel_sqlite::OpenFlags::read_write())
            .busy_timeout(10);
            let contender =
                CanonicalDbConn::open(&config).expect("child opens competing canonical connection");
            let error = contender
                .execute_raw("BEGIN IMMEDIATE")
                .expect_err("child must not acquire the parent writer lock");
            let error = error.to_string().to_ascii_lowercase();
            assert!(
                error.contains("busy") || error.contains("locked"),
                "child observed an unrelated failure instead of lock contention: {error}"
            );
            println!("{CHILD_WITNESS}");
            return;
        }

        fn assert_child_observes_busy(path: &Path) {
            let output = std::process::Command::new(
                std::env::current_exe().expect("resolve current test executable"),
            )
            .arg(CHILD_TEST_NAME)
            .arg("--exact")
            .arg("--nocapture")
            .env(CHILD_PATH_ENV, path)
            .output()
            .expect("run competing child lock probe");
            assert!(
                output.status.success(),
                "child lock probe failed: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).contains(CHILD_WITNESS),
                "child filter ran no causal probe: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let temp = tempfile::tempdir().expect("tempdir");
        let source_db = temp.path().join("legacy-live-franken.sqlite3");
        let writer =
            DbConn::open_file(source_db.display().to_string()).expect("open live Franken source");
        writer
            .execute_raw(
                "PRAGMA journal_mode = DELETE; \
                 PRAGMA autocommit_retain = OFF; \
                 CREATE TABLE legacy_lock_witness(value INTEGER NOT NULL); \
                 INSERT INTO legacy_lock_witness(value) VALUES (41);",
            )
            .expect("seed legacy lock fixture");
        writer
            .execute_raw("BEGIN IMMEDIATE")
            .expect("hold live Franken writer lock");
        assert_child_observes_busy(&source_db);

        let snapshot = LegacySourceSnapshot::capture(&source_db)
            .expect("capture live Franken source without a cross-engine open");
        let snapshot_conn = open_private_immutable_source_snapshot(&snapshot)
            .expect("open retained private snapshot");
        let rows = snapshot_conn
            .query_sync("SELECT value FROM legacy_lock_witness", &[])
            .expect("query retained snapshot witness");
        assert_eq!(
            rows.first()
                .and_then(|row| row.get_named::<i64>("value").ok()),
            Some(41)
        );
        assert_child_observes_busy(&source_db);

        writer
            .execute_raw("ROLLBACK")
            .expect("release live Franken writer lock");
    }

    #[cfg(unix)]
    #[test]
    fn copy_dir_recursive_rejects_symlinked_files() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        let outside = tmp.path().join("outside.txt");
        fs::create_dir_all(&src).unwrap();
        fs::write(&outside, "outside-payload").unwrap();
        symlink(&outside, src.join("file-link")).unwrap();

        let err = copy_dir_recursive(&src, &dst).unwrap_err();
        match err {
            CliError::InvalidArgument(msg) => {
                assert!(msg.contains("symlinked files are not supported"));
            }
            other => panic!("expected invalid argument, got {other:?}"),
        }
    }
}
