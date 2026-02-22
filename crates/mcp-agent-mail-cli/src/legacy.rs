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
use mcp_agent_mail_core::disk::{
    is_sqlite_memory_database_url, sqlite_file_path_from_database_url,
};
use mcp_agent_mail_db::DbConn;
use mcp_agent_mail_db::schema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::{IsTerminal, Write};
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
        /// Force in-place migration (default mode).
        #[arg(long, default_value_t = false, conflicts_with = "copy")]
        in_place: bool,
        /// Copy source DB/storage to target paths, then migrate the copy.
        #[arg(long, default_value_t = false, conflicts_with = "in_place")]
        copy: bool,
        /// Optional target DB path when `--copy` is used.
        #[arg(long)]
        target_db: Option<PathBuf>,
        /// Optional target storage root when `--copy` is used.
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
    InPlace,
    Copy,
}

#[derive(Debug, Clone)]
struct ImportPlan {
    mode: ImportMode,
    search_root: PathBuf,
    source_db: PathBuf,
    source_storage_root: PathBuf,
    target_db: PathBuf,
    target_storage_root: PathBuf,
    operations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyImportReceipt {
    receipt_version: u32,
    created_at: String,
    mode: ImportMode,
    search_root: String,
    source_db: String,
    source_storage_root: String,
    target_db: String,
    target_storage_root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    backup_root: Option<String>,
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
            in_place,
            copy,
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
                in_place,
                copy,
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
        in_place: false,
        copy: false,
        target_db: None,
        target_storage_root: None,
        dry_run: args.dry_run,
        yes: args.yes,
    };
    let plan = build_import_plan(&import_opts)?;

    if args.dry_run {
        report.action =
            "dry-run: legacy detected; would run in-place import + setup refresh".into();
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
        if !std::io::stdin().is_terminal() {
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
    in_place: bool,
    copy: bool,
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
        if !std::io::stdin().is_terminal() {
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
        if let Some(path) = &receipt.backup_root {
            ftui_runtime::ftui_println!("- Backup root: {path}");
        }
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
    let detect = build_detect_report(&root, opts.db.as_deref(), opts.storage_root.as_deref())?;
    if opts.auto && !detect.detected {
        return Err(CliError::InvalidArgument(
            "no legacy installation detected; run `am legacy detect` to inspect details"
                .to_string(),
        ));
    }

    let source_db = PathBuf::from(&detect.database.path);
    let source_storage = PathBuf::from(&detect.storage_root.path);
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
    if !source_storage.exists() {
        return Err(CliError::InvalidArgument(format!(
            "source storage root missing: {}",
            source_storage.display()
        )));
    }
    if !source_storage.is_dir() {
        return Err(CliError::InvalidArgument(format!(
            "source storage root must be a directory: {}",
            source_storage.display()
        )));
    }

    let mode = match (opts.in_place, opts.copy) {
        (false, true) => ImportMode::Copy,
        (true, false) | (false, false) => ImportMode::InPlace,
        (true, true) => {
            return Err(CliError::InvalidArgument(
                "--in-place and --copy are mutually exclusive".to_string(),
            ));
        }
    };

    let (target_db, target_storage) = match mode {
        ImportMode::InPlace => {
            if opts.target_db.is_some() || opts.target_storage_root.is_some() {
                return Err(CliError::InvalidArgument(
                    "--target-db/--target-storage-root require --copy".to_string(),
                ));
            }
            (source_db.clone(), source_storage.clone())
        }
        ImportMode::Copy => {
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
            (target_db, target_storage)
        }
    };

    if mode == ImportMode::Copy && source_db == target_db {
        return Err(CliError::InvalidArgument(
            "copy mode requires target DB path different from source DB".to_string(),
        ));
    }
    if mode == ImportMode::Copy && target_db.exists() {
        return Err(CliError::InvalidArgument(format!(
            "copy mode requires target DB path that does not already exist: {}",
            target_db.display()
        )));
    }
    if mode == ImportMode::Copy && source_storage == target_storage {
        return Err(CliError::InvalidArgument(
            "copy mode requires target storage root different from source storage root".to_string(),
        ));
    }
    if mode == ImportMode::Copy && target_storage.exists() && !target_storage.is_dir() {
        return Err(CliError::InvalidArgument(format!(
            "copy mode requires target storage root to be a directory path: {}",
            target_storage.display()
        )));
    }
    if mode == ImportMode::Copy && paths_overlap(&source_storage, &target_storage) {
        return Err(CliError::InvalidArgument(
            "copy mode requires target storage root to be outside source storage root".to_string(),
        ));
    }

    let mut operations = Vec::new();
    operations.push(format!("resolve source DB: {}", source_db.display()));
    operations.push(format!(
        "resolve source storage root: {}",
        source_storage.display()
    ));
    match mode {
        ImportMode::InPlace => {
            operations.push("create safety backup of source DB and storage root".to_string());
            operations.push("run schema::migrate_to_latest against source DB".to_string());
        }
        ImportMode::Copy => {
            operations.push(format!(
                "copy source DB to target DB: {}",
                target_db.display()
            ));
            operations.push(format!(
                "copy source storage root to target storage root: {}",
                target_storage.display()
            ));
            operations.push("run schema::migrate_to_latest against target DB".to_string());
        }
    }
    operations.push("run integrity_check and core-table sanity queries".to_string());
    operations.push("write JSON receipt under target storage root".to_string());
    operations.push("refresh agent MCP config via setup run".to_string());

    Ok(ImportPlan {
        mode,
        search_root: root,
        source_db,
        source_storage_root: source_storage,
        target_db,
        target_storage_root: target_storage,
        operations,
    })
}

fn execute_import(plan: ImportPlan, should_refresh_setup: bool) -> CliResult<LegacyImportReceipt> {
    let now = Utc::now();
    let timestamp = now.format("%Y%m%dT%H%M%SZ").to_string();
    let mut warnings = Vec::new();
    let mut backup_root: Option<PathBuf> = None;

    match plan.mode {
        ImportMode::InPlace => {
            let backup_dir = default_backup_dir(&plan.source_storage_root, &timestamp);
            backup_db_with_sidecars(&plan.source_db, &backup_dir.join("db"))?;
            copy_dir_recursive(
                &plan.source_storage_root,
                &backup_dir.join("storage_root_backup"),
            )?;
            backup_root = Some(backup_dir);
        }
        ImportMode::Copy => {
            if plan.target_storage_root.exists() {
                let mut iter = fs::read_dir(&plan.target_storage_root)?;
                if iter.next().is_some() {
                    return Err(CliError::InvalidArgument(format!(
                        "target storage root {} already exists and is not empty; choose a different path",
                        plan.target_storage_root.display()
                    )));
                }
            }
            copy_db_with_sidecars(&plan.source_db, &plan.target_db)?;
            copy_dir_recursive(&plan.source_storage_root, &plan.target_storage_root)?;
        }
    }

    let migrated_ids = migrate_sqlite_db(&plan.target_db)?;
    let integrity_ok = integrity_check_ok(&plan.target_db)?;
    if !integrity_ok {
        return Err(CliError::Other(format!(
            "integrity_check failed after migration for {}",
            plan.target_db.display()
        )));
    }
    let core_counts = query_core_table_counts(&plan.target_db)?;

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

    let receipt = LegacyImportReceipt {
        receipt_version: 1,
        created_at: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        mode: plan.mode,
        search_root: plan.search_root.display().to_string(),
        source_db: plan.source_db.display().to_string(),
        source_storage_root: plan.source_storage_root.display().to_string(),
        target_db: plan.target_db.display().to_string(),
        target_storage_root: plan.target_storage_root.display().to_string(),
        backup_root: backup_root.as_ref().map(|p| p.display().to_string()),
        migrated_migration_ids: migrated_ids,
        integrity_check_ok: integrity_ok,
        core_table_counts: core_counts,
        setup_refresh_ok: setup_ok,
        warnings,
    };
    write_receipt(&plan.target_storage_root, &receipt, &timestamp)?;
    Ok(receipt)
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
    conn.execute_raw(schema::PRAGMA_SETTINGS_SQL)
        .map_err(|e| CliError::Other(format!("failed to apply PRAGMAs: {e}")))?;

    let cx = asupersync::Cx::for_request();
    let rt = RuntimeBuilder::current_thread()
        .build()
        .map_err(|e| CliError::Other(format!("failed to build runtime: {e}")))?;
    match rt.block_on(async { schema::migrate_to_latest_base(&cx, &conn).await }) {
        asupersync::Outcome::Ok(ids) => Ok(ids),
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
    Ok(value == "ok")
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
) -> CliResult<()> {
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
    fs::write(path, format!("{content}\n"))?;
    Ok(())
}

fn build_detect_report(
    search_root: &Path,
    explicit_db: Option<&Path>,
    explicit_storage_root: Option<&Path>,
) -> CliResult<LegacyDetectReport> {
    let db_resolved = resolve_database_path(search_root, explicit_db)?;
    let storage_resolved = resolve_storage_root(search_root, explicit_storage_root)?;

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
    if let Some(marker) = detect_env_marker(search_root) {
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

    let db_signature = inspect_db_signature(&db_resolved.path);
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
            raw_value: db_resolved.raw_value,
            error: None,
        },
        storage_root: ResolvedPathInfo {
            path: storage_resolved.path.display().to_string(),
            source: storage_resolved.source,
            exists: storage_resolved.exists,
            raw_value: storage_resolved.raw_value,
            error: None,
        },
        markers,
        db_signature,
        recommended_action,
    })
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

fn detect_env_marker(search_root: &Path) -> Option<LegacyMarker> {
    let env_file = search_root.join(".env");
    if !env_file.exists() {
        return None;
    }
    let map = read_env_file_map(&env_file);
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
            path: Some(env_file.display().to_string()),
        });
    }
    None
}

fn inspect_db_signature(path: &Path) -> Option<LegacyDbSignature> {
    if !path.exists() {
        return None;
    }
    let conn = match DbConn::open_file(path.display().to_string()) {
        Ok(v) => v,
        Err(_) => {
            return Some(LegacyDbSignature {
                open_ok: false,
                core_tables_present: false,
                legacy_trigger_count: 0,
                datetime_like_column_count: 0,
                migrations_table_present: false,
                notes: vec!["failed to open sqlite database".to_string()],
            });
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

    Some(LegacyDbSignature {
        open_ok: true,
        core_tables_present,
        legacy_trigger_count,
        datetime_like_column_count,
        migrations_table_present,
        notes,
    })
}

fn resolve_database_path(search_root: &Path, explicit: Option<&Path>) -> CliResult<ResolvedPath> {
    if let Some(path) = explicit {
        let normalized = normalize_input_path(&path.to_string_lossy(), search_root);
        return Ok(ResolvedPath {
            exists: normalized.exists(),
            path: normalized,
            source: ResolvedSource::Explicit,
            raw_value: Some(path.display().to_string()),
        });
    }

    if let Ok(v) = std::env::var("DATABASE_URL") {
        return parse_database_value(&v, search_root, ResolvedSource::ProcessEnv);
    }

    let project_env = search_root.join(".env");
    let map = read_env_file_map(&project_env);
    if let Some(v) = map.get("DATABASE_URL") {
        return parse_database_value(v, search_root, ResolvedSource::ProjectEnv);
    }

    if let Some(user_env) = discover_user_env_file() {
        let map = read_env_file_map(&user_env);
        if let Some(v) = map.get("DATABASE_URL") {
            return parse_database_value(v, search_root, ResolvedSource::UserEnv);
        }
    }

    parse_database_value(
        "sqlite+aiosqlite:///./storage.sqlite3",
        search_root,
        ResolvedSource::Default,
    )
}

fn resolve_storage_root(search_root: &Path, explicit: Option<&Path>) -> CliResult<ResolvedPath> {
    if let Some(path) = explicit {
        let normalized = normalize_input_path(&path.to_string_lossy(), search_root);
        return Ok(ResolvedPath {
            exists: normalized.exists(),
            path: normalized,
            source: ResolvedSource::Explicit,
            raw_value: Some(path.display().to_string()),
        });
    }

    if let Ok(v) = std::env::var("STORAGE_ROOT") {
        let path = normalize_input_path(&v, search_root);
        return Ok(ResolvedPath {
            exists: path.exists(),
            path,
            source: ResolvedSource::ProcessEnv,
            raw_value: Some(v),
        });
    }

    let project_env = search_root.join(".env");
    let map = read_env_file_map(&project_env);
    if let Some(v) = map.get("STORAGE_ROOT") {
        let path = normalize_input_path(v, search_root);
        return Ok(ResolvedPath {
            exists: path.exists(),
            path,
            source: ResolvedSource::ProjectEnv,
            raw_value: Some(v.clone()),
        });
    }

    if let Some(user_env) = discover_user_env_file() {
        let map = read_env_file_map(&user_env);
        if let Some(v) = map.get("STORAGE_ROOT") {
            let path = normalize_input_path(v, search_root);
            return Ok(ResolvedPath {
                exists: path.exists(),
                path,
                source: ResolvedSource::UserEnv,
                raw_value: Some(v.clone()),
            });
        }
    }

    let value = "~/.mcp_agent_mail_git_mailbox_repo";
    let path = normalize_input_path(value, search_root);
    Ok(ResolvedPath {
        exists: path.exists(),
        path,
        source: ResolvedSource::Default,
        raw_value: Some(value.to_string()),
    })
}

fn parse_database_value(
    value: &str,
    search_root: &Path,
    source: ResolvedSource,
) -> CliResult<ResolvedPath> {
    if is_sqlite_memory_database_url(value) {
        return Err(CliError::InvalidArgument(
            "in-memory DATABASE_URL is not supported for legacy import".to_string(),
        ));
    }

    let db_path = if value.contains("://") {
        sqlite_file_path_from_database_url(value).ok_or_else(|| {
            CliError::InvalidArgument(format!(
                "unsupported DATABASE_URL scheme for import: {value}"
            ))
        })?
    } else {
        PathBuf::from(value)
    };
    let path = if db_path.is_absolute() {
        db_path
    } else {
        search_root.join(db_path)
    };
    Ok(ResolvedPath {
        exists: path.exists(),
        path,
        source,
        raw_value: Some(value.to_string()),
    })
}

fn read_env_file_map(path: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let text = match fs::read_to_string(path) {
        Ok(v) => v,
        Err(_) => return out,
    };
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

fn discover_user_env_file() -> Option<PathBuf> {
    let home = home_dir()?;
    let preferred = home.join(".mcp_agent_mail").join(".env");
    if preferred.is_file() {
        return Some(preferred);
    }
    let legacy = home.join("mcp_agent_mail").join(".env");
    if legacy.is_file() {
        return Some(legacy);
    }
    None
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
    path.canonicalize()
        .or_else(|_| normalize_lexical_path(path).canonicalize())
        .unwrap_or_else(|_| normalize_lexical_path(path))
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
    std::env::var_os("HOME").map(PathBuf::from)
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

fn default_backup_dir(source_storage_root: &Path, timestamp: &str) -> PathBuf {
    let parent = source_storage_root
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    parent.join("mcp-agent-mail-legacy-backups").join(timestamp)
}

fn backup_db_with_sidecars(db_path: &Path, destination_root: &Path) -> CliResult<()> {
    fs::create_dir_all(destination_root)?;
    let db_name = db_path
        .file_name()
        .map(|v| v.to_owned())
        .unwrap_or_else(|| "storage.sqlite3".into());
    fs::copy(db_path, destination_root.join(db_name))?;
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{}", db_path.display(), suffix));
        if sidecar.exists() {
            let file_name = sidecar
                .file_name()
                .ok_or_else(|| CliError::Other("invalid sidecar filename".to_string()))?;
            fs::copy(&sidecar, destination_root.join(file_name))?;
        }
    }
    Ok(())
}

fn copy_db_with_sidecars(source_db: &Path, target_db: &Path) -> CliResult<()> {
    if let Some(parent) = target_db.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(source_db, target_db)?;
    for suffix in ["-wal", "-shm"] {
        let source_sidecar = PathBuf::from(format!("{}{}", source_db.display(), suffix));
        if source_sidecar.exists() {
            let target_sidecar = PathBuf::from(format!("{}{}", target_db.display(), suffix));
            fs::copy(source_sidecar, target_sidecar)?;
        }
    }
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> CliResult<()> {
    if !src.exists() {
        return Err(CliError::InvalidArgument(format!(
            "source directory does not exist: {}",
            src.display()
        )));
    }
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let target = dst.join(entry.file_name());
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            if path.is_dir() {
                return Err(CliError::InvalidArgument(format!(
                    "symlinked directories are not supported during recursive copy: {}",
                    path.display()
                )));
            }
            if !path.is_file() {
                return Err(CliError::InvalidArgument(format!(
                    "broken symlink encountered during recursive copy: {}",
                    path.display()
                )));
            }
        }
        if path.is_dir() {
            copy_dir_recursive(&path, &target)?;
        } else if path.is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&path, &target)?;
        }
    }
    Ok(())
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

    fn sample_receipt(created_at: &str, target_db: &str) -> LegacyImportReceipt {
        let mut counts = BTreeMap::new();
        counts.insert("messages".to_string(), 1);
        LegacyImportReceipt {
            receipt_version: 1,
            created_at: created_at.to_string(),
            mode: ImportMode::InPlace,
            search_root: "/tmp/project".to_string(),
            source_db: "/tmp/storage.sqlite3".to_string(),
            source_storage_root: "/tmp/storage-root".to_string(),
            target_db: target_db.to_string(),
            target_storage_root: "/tmp/storage-root".to_string(),
            backup_root: Some("/tmp/backup".to_string()),
            migrated_migration_ids: vec!["20260216_add_indexes".to_string()],
            integrity_check_ok: true,
            core_table_counts: counts,
            setup_refresh_ok: true,
            warnings: vec![],
        }
    }

    #[test]
    fn read_env_file_map_parses_key_values() {
        let tmp = tempfile::tempdir().unwrap();
        let env = tmp.path().join(".env");
        fs::write(
            &env,
            "DATABASE_URL=sqlite+aiosqlite:///./storage.sqlite3\nSTORAGE_ROOT=~/.mcp_agent_mail_git_mailbox_repo\n",
        )
        .unwrap();
        let map = read_env_file_map(&env);
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
    fn read_env_file_map_parses_export_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let env = tmp.path().join(".env");
        fs::write(
            &env,
            "export DATABASE_URL=sqlite+aiosqlite:///./storage.sqlite3\nexport STORAGE_ROOT=~/mailbox\n",
        )
        .unwrap();

        let map = read_env_file_map(&env);
        assert_eq!(
            map.get("DATABASE_URL").unwrap(),
            "sqlite+aiosqlite:///./storage.sqlite3"
        );
        assert_eq!(map.get("STORAGE_ROOT").unwrap(), "~/mailbox");
    }

    #[test]
    fn read_env_file_map_parses_export_with_tabs() {
        let tmp = tempfile::tempdir().unwrap();
        let env = tmp.path().join(".env");
        fs::write(
            &env,
            "export\tDATABASE_URL=sqlite+aiosqlite:///./tabbed.sqlite3\n",
        )
        .unwrap();

        let map = read_env_file_map(&env);
        assert_eq!(
            map.get("DATABASE_URL").unwrap(),
            "sqlite+aiosqlite:///./tabbed.sqlite3"
        );
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
    fn build_import_plan_in_place_uses_source_paths() {
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
            in_place: true,
            copy: false,
            target_db: None,
            target_storage_root: None,
            dry_run: true,
            yes: true,
        })
        .unwrap();
        assert_eq!(plan.mode, ImportMode::InPlace);
        assert_eq!(plan.source_db, db);
        assert_eq!(plan.target_db, plan.source_db);
        assert_eq!(plan.source_storage_root, storage);
        assert_eq!(plan.target_storage_root, plan.source_storage_root);
        assert!(
            plan.operations
                .iter()
                .any(|op| op.contains("create safety backup"))
        );
    }

    #[test]
    fn build_import_plan_copy_generates_default_targets() {
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
            in_place: false,
            copy: true,
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
            in_place: false,
            copy: true,
            target_db: Some(db),
            target_storage_root: Some(storage),
            dry_run: true,
            yes: true,
        })
        .unwrap_err();
        match err {
            CliError::InvalidArgument(msg) => {
                assert!(msg.contains("copy mode requires target DB path different"));
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
            in_place: true,
            copy: false,
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
            in_place: true,
            copy: false,
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
            in_place: false,
            copy: true,
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
            in_place: false,
            copy: true,
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
            in_place: false,
            copy: true,
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
        let report = build_detect_report(tmp.path(), None, None).unwrap();
        assert!(report.detected);
        assert!(
            report
                .markers
                .iter()
                .any(|marker| marker.id == "pyproject_package")
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
        let report = build_detect_report(tmp.path(), None, None).unwrap();
        assert!(
            report
                .markers
                .iter()
                .any(|marker| marker.id == "legacy_env_defaults")
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
        assert_eq!(parsed.receipt_version, 1);
        assert_eq!(parsed.mode, ImportMode::InPlace);
        assert_eq!(parsed.source_db, "/tmp/storage.sqlite3");
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
    fn default_backup_dir_includes_timestamp() {
        let root = PathBuf::from("/tmp/.mcp_agent_mail_git_mailbox_repo");
        let backup = default_backup_dir(&root, "20260217T000000Z");
        assert!(
            backup
                .to_string_lossy()
                .contains("mcp-agent-mail-legacy-backups")
        );
        assert!(backup.to_string_lossy().ends_with("20260217T000000Z"));
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

    #[test]
    fn import_fixture_copy_mode_migrates_and_writes_receipt() {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../legacy_python_mcp_agent_mail_code/mcp_agent_mail/storage.sqlite3");
        assert!(
            fixture.exists(),
            "missing legacy fixture DB at {}",
            fixture.display()
        );

        let tmp = tempfile::tempdir().unwrap();
        let source_db = tmp.path().join("legacy_fixture.sqlite3");
        fs::copy(&fixture, &source_db).unwrap();

        let source_storage = tmp.path().join("legacy-storage");
        fs::create_dir_all(&source_storage).unwrap();
        fs::write(source_storage.join(".placeholder"), "legacy-storage-root").unwrap();

        let source_conn = DbConn::open_file(source_db.display().to_string()).unwrap();
        let source_rows = source_conn
            .query_sync("SELECT COUNT(*) AS c FROM messages", &[])
            .unwrap();
        let source_message_count = source_rows
            .first()
            .and_then(|r| r.get_named::<i64>("c").ok())
            .unwrap_or(0);

        let target_db = tmp.path().join("rust_import.sqlite3");
        let target_storage = tmp.path().join("rust-storage");
        let plan = build_import_plan(&ImportOptions {
            auto: false,
            search_root: Some(tmp.path().to_path_buf()),
            db: Some(source_db.clone()),
            storage_root: Some(source_storage),
            in_place: false,
            copy: true,
            target_db: Some(target_db.clone()),
            target_storage_root: Some(target_storage.clone()),
            dry_run: false,
            yes: true,
        })
        .unwrap();

        let receipt = execute_import(plan, false).unwrap();
        assert!(receipt.integrity_check_ok);
        assert!(target_db.exists(), "target DB missing after import");
        let receipts_dir = target_storage.join("legacy_import_receipts");
        assert!(receipts_dir.exists(), "receipt directory should exist");
        let receipt_files: Vec<PathBuf> = fs::read_dir(&receipts_dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .collect();
        assert!(
            receipt_files.iter().any(|p| {
                p.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("legacy_import_"))
            }),
            "expected at least one legacy import receipt file"
        );

        let conn = DbConn::open_file(target_db.display().to_string()).unwrap();
        let migration_rows = conn
            .query_sync("SELECT COUNT(*) AS c FROM mcp_agent_mail_migrations", &[])
            .unwrap();
        let migration_count = migration_rows
            .first()
            .and_then(|r| r.get_named::<i64>("c").ok())
            .unwrap_or(0);
        assert!(migration_count > 0, "expected applied migration rows");

        let message_rows = conn
            .query_sync("SELECT COUNT(*) AS c FROM messages", &[])
            .unwrap();
        let target_message_count = message_rows
            .first()
            .and_then(|r| r.get_named::<i64>("c").ok())
            .unwrap_or(0);
        assert_eq!(
            target_message_count, source_message_count,
            "message row count should be preserved"
        );

        let trigger_rows = conn
            .query_sync(
                "SELECT COUNT(*) AS c FROM sqlite_master WHERE type='trigger' \
                 AND name IN ('fts_messages_ai','fts_messages_ad','fts_messages_au')",
                &[],
            )
            .unwrap();
        let trigger_count = trigger_rows
            .first()
            .and_then(|r| r.get_named::<i64>("c").ok())
            .unwrap_or(0);
        assert_eq!(trigger_count, 0, "legacy FTS triggers should be removed");
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
}
