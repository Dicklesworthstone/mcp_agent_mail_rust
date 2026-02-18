#![forbid(unsafe_code)]

pub mod bundle;
pub mod crypto;
pub mod deploy;
pub mod detection;
pub mod executor;
pub mod finalize;
pub mod hosting;
pub mod planner;
pub mod probe;
pub mod prompt;
pub mod scope;
pub mod scrub;
pub mod snapshot;
pub mod static_render;
pub mod wizard;

pub use bundle::{
    AttachmentConfig, AttachmentItem, AttachmentManifest, AttachmentStats, ChunkManifest,
    ViewerDataManifest, ViewerMetaInfo, bundle_attachments, compute_viewer_sri, copy_viewer_assets,
    export_viewer_data, maybe_chunk_database, package_directory_as_zip, write_bundle_scaffolding,
};
pub use crypto::{
    ManifestSignature, VerifyResult, decrypt_with_age, encrypt_with_age, sign_manifest,
    verify_bundle as verify_bundle_crypto,
};
pub use deploy::{
    BundleStats, CheckSeverity, DeployCheck, DeployReport, PlatformInfo, VerifyConfig,
    VerifyLiveCheck, VerifyLiveOptions, VerifyLiveReport, VerifyStage, VerifyStages, VerifySummary,
    VerifyVerdict, generate_cf_pages_config, generate_cf_pages_workflow,
    generate_gh_pages_workflow, generate_netlify_config, generate_validation_script,
    run_verify_live, validate_bundle, write_deploy_tooling,
};
pub use detection::{
    detect_cloudflare_pages, detect_environment, detect_github_pages, detect_netlify, detect_s3,
    extract_github_repo,
};
pub use executor::{ExecutorConfig, execute_plan};
pub use finalize::{
    FinalizeResult, build_materialized_views, build_search_indexes, create_performance_indexes,
    finalize_export_db, finalize_snapshot_for_export,
};
pub use hosting::{HostingHint, detect_hosting_hints, generate_headers_file};
pub use planner::{PlanResult, format_plan_human, generate_plan, validate_inputs};
pub use prompt::{WizardConfig, WizardOutcome, format_json_output, run_interactive_wizard};
pub use scope::{ProjectRecord, ProjectScopeResult, RemainingCounts, apply_project_scope};
pub use scrub::{ScrubSummary, scan_for_secrets, scrub_snapshot};
pub use snapshot::{SnapshotContext, create_snapshot_context, create_sqlite_snapshot};
pub use static_render::{
    SearchIndexEntry, SitemapEntry, StaticRenderConfig, StaticRenderResult, render_static_site,
};
pub use wizard::{
    DeploymentPlan, DetectedEnvironment, DetectedSignal, DetectionConfidence, HostingProvider,
    PlanStep, StepOutcome, WIZARD_VERSION, WizardError, WizardErrorCode, WizardInputs,
    WizardJsonOutput, WizardMetadata, WizardMode, WizardResult, exit_codes,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Inline attachments at or below this size (bytes).
pub const INLINE_ATTACHMENT_THRESHOLD: usize = 64 * 1024; // 64 KiB
/// Mark attachments at or above this size as external (not bundled).
pub const DETACH_ATTACHMENT_THRESHOLD: usize = 25 * 1024 * 1024; // 25 MiB
/// Chunk SQLite DB when size exceeds this threshold (bytes).
pub const DEFAULT_CHUNK_THRESHOLD: usize = 20 * 1024 * 1024; // 20 MiB
/// Chunk size in bytes when chunking is enabled.
pub const DEFAULT_CHUNK_SIZE: usize = 4 * 1024 * 1024; // 4 MiB

/// Supported scrub presets for sharing.
pub const SCRUB_PRESETS: [&str; 3] = ["standard", "strict", "archive"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrubPreset {
    Standard,
    Strict,
    Archive,
}

impl ScrubPreset {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Strict => "strict",
            Self::Archive => "archive",
        }
    }
}

impl std::fmt::Display for ScrubPreset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── Export redaction policy ──────────────────────────────────────────────

/// Export-time redaction policy derived from the scrub preset.
///
/// Controls what the static renderer enforces beyond the DB-level scrub pass.
/// Defense-in-depth: even after scrubbing, the renderer re-scans output for
/// any secret patterns and applies visibility rules.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportRedactionPolicy {
    /// Whether to scan and replace any remaining secret patterns in rendered output.
    pub scan_secrets: bool,
    /// Whether message bodies are treated as redacted (strict preset).
    pub redact_bodies: bool,
    /// Whether recipient lists are hidden in rendered output.
    pub redact_recipients: bool,
    /// Placeholder text for redacted message bodies.
    pub body_placeholder: String,
    /// Placeholder text for redacted search snippets.
    pub snippet_placeholder: String,
}

impl ExportRedactionPolicy {
    /// Create a redaction policy from a scrub preset.
    #[must_use]
    pub fn from_preset(preset: ScrubPreset) -> Self {
        match preset {
            ScrubPreset::Standard => Self {
                scan_secrets: true,
                redact_bodies: false,
                redact_recipients: false,
                body_placeholder: "[Message body redacted]".to_string(),
                snippet_placeholder: "[Content hidden per export policy]".to_string(),
            },
            ScrubPreset::Strict => Self {
                scan_secrets: true,
                redact_bodies: true,
                redact_recipients: true,
                body_placeholder: "[Message body redacted]".to_string(),
                snippet_placeholder: "[Content hidden per export policy]".to_string(),
            },
            ScrubPreset::Archive => Self::none(),
        }
    }

    /// No redaction applied (archive/operator mode).
    #[must_use]
    pub fn none() -> Self {
        Self {
            scan_secrets: false,
            redact_bodies: false,
            redact_recipients: false,
            body_placeholder: String::new(),
            snippet_placeholder: String::new(),
        }
    }

    /// Returns `true` if any redaction rules are active.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.scan_secrets || self.redact_bodies || self.redact_recipients
    }
}

impl Default for ExportRedactionPolicy {
    fn default() -> Self {
        Self::from_preset(ScrubPreset::Standard)
    }
}

/// Reason code explaining why content was hidden or redacted in export output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RedactionReason {
    /// Content was redacted by the scrub preset during DB-level pass.
    ScrubPreset,
    /// A secret pattern was detected and replaced during render.
    SecretDetected,
    /// Message body replaced per strict export policy.
    BodyRedacted,
    /// Recipients list hidden per strict export policy.
    RecipientsHidden,
    /// Search snippet excluded to prevent content leakage.
    SnippetExcluded,
}

impl RedactionReason {
    /// Operator-facing description of why content was hidden.
    #[must_use]
    pub fn description(self) -> &'static str {
        match self {
            Self::ScrubPreset => "Content removed during scrub pass (preset policy)",
            Self::SecretDetected => "Secret pattern detected and replaced",
            Self::BodyRedacted => "Message body hidden per strict export policy",
            Self::RecipientsHidden => "Recipient list hidden per strict export policy",
            Self::SnippetExcluded => "Search snippet excluded to prevent content leakage",
        }
    }
}

/// A single redaction event recorded during the export pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedactionEvent {
    pub reason: RedactionReason,
    /// Human-readable context (e.g. "message 42", "search index entry 7").
    pub context: String,
    /// Optional entity ID (message ID, etc.).
    pub entity_id: Option<i64>,
}

/// Audit log summarizing all redaction actions during a static export.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RedactionAuditLog {
    pub events: Vec<RedactionEvent>,
    pub secrets_caught: usize,
    pub bodies_redacted: usize,
    pub snippets_filtered: usize,
    pub recipients_hidden: usize,
}

impl RedactionAuditLog {
    /// Record a redaction event.
    pub fn record(&mut self, reason: RedactionReason, context: String, entity_id: Option<i64>) {
        match reason {
            RedactionReason::SecretDetected => self.secrets_caught += 1,
            RedactionReason::BodyRedacted | RedactionReason::ScrubPreset => {
                self.bodies_redacted += 1;
            }
            RedactionReason::SnippetExcluded => self.snippets_filtered += 1,
            RedactionReason::RecipientsHidden => self.recipients_hidden += 1,
        }
        self.events.push(RedactionEvent {
            reason,
            context,
            entity_id,
        });
    }

    /// Total number of redaction actions taken.
    #[must_use]
    pub fn total(&self) -> usize {
        self.events.len()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ShareError {
    #[error("not implemented")]
    NotImplemented,
    #[error("invalid scrub preset: {preset}")]
    InvalidScrubPreset { preset: String },
    #[error("invalid threshold for {field}: {value}")]
    InvalidThreshold { field: &'static str, value: i64 },
    #[error("bundle not found: {path}")]
    BundleNotFound { path: String },
    #[error("manifest.json not found in {path}")]
    ManifestNotFound { path: String },
    #[error("failed to parse manifest.json: {message}")]
    ManifestParse { message: String },
    #[error("snapshot source not found: {path}")]
    SnapshotSourceNotFound { path: String },
    #[error("snapshot destination already exists: {path}")]
    SnapshotDestinationExists { path: String },
    #[error("database has no projects")]
    ScopeNoProjects,
    #[error("project identifier not found: {identifier}")]
    ScopeIdentifierNotFound { identifier: String },
    #[error("sqlite error: {message}")]
    Sqlite { message: String },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type ShareResult<T> = Result<T, ShareError>;

/// Normalize and validate a scrub preset string (case-insensitive).
pub fn normalize_scrub_preset(input: &str) -> ShareResult<ScrubPreset> {
    let preset = input.trim().to_ascii_lowercase();
    match preset.as_str() {
        "standard" => Ok(ScrubPreset::Standard),
        "strict" => Ok(ScrubPreset::Strict),
        "archive" => Ok(ScrubPreset::Archive),
        _ => Err(ShareError::InvalidScrubPreset { preset }),
    }
}

/// Adjust detach threshold to exceed inline threshold (legacy behavior).
#[must_use]
pub fn adjust_detach_threshold(inline_threshold: usize, detach_threshold: usize) -> usize {
    if detach_threshold > inline_threshold {
        return detach_threshold;
    }
    let bump = inline_threshold / 2;
    inline_threshold + std::cmp::max(1024, bump.max(1))
}

/// Validate non-negative integer thresholds and minimum chunk size.
pub fn validate_thresholds(
    inline_threshold: i64,
    detach_threshold: i64,
    chunk_threshold: i64,
    chunk_size: i64,
) -> ShareResult<()> {
    if inline_threshold < 0 {
        return Err(ShareError::InvalidThreshold {
            field: "inline_threshold",
            value: inline_threshold,
        });
    }
    if detach_threshold < 0 {
        return Err(ShareError::InvalidThreshold {
            field: "detach_threshold",
            value: detach_threshold,
        });
    }
    if chunk_threshold < 0 {
        return Err(ShareError::InvalidThreshold {
            field: "chunk_threshold",
            value: chunk_threshold,
        });
    }
    if chunk_size < 1024 {
        return Err(ShareError::InvalidThreshold {
            field: "chunk_size",
            value: chunk_size,
        });
    }
    Ok(())
}

/// Default output path for decrypt when `--output` is omitted.
#[must_use]
pub fn default_decrypt_output(encrypted_path: &Path) -> PathBuf {
    if encrypted_path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("age"))
        .unwrap_or(false)
    {
        return encrypted_path.with_extension("");
    }
    let stem = encrypted_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("bundle");
    let suffix = encrypted_path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let mut file_name = format!("{stem}_decrypted");
    if !suffix.is_empty() {
        file_name.push('.');
        file_name.push_str(suffix);
    }
    encrypted_path.with_file_name(file_name)
}

/// Resolve the SQLite database path from a config database URL.
pub fn resolve_sqlite_database_path(database_url: &str) -> ShareResult<PathBuf> {
    // Strip SQLAlchemy-style prefixes
    let path_str = database_url
        .strip_prefix("sqlite+aiosqlite:///")
        .or_else(|| database_url.strip_prefix("sqlite:///"))
        .or_else(|| database_url.strip_prefix("sqlite:"))
        .unwrap_or(database_url);

    if path_str.is_empty() {
        return Err(ShareError::SnapshotSourceNotFound {
            path: "empty database URL".to_string(),
        });
    }

    let path = PathBuf::from(path_str);
    if path.is_absolute() {
        Ok(path)
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(ShareError::Io)
    }
}

#[derive(Debug, Clone)]
pub struct StoredExportConfig {
    pub projects: Vec<String>,
    pub inline_threshold: i64,
    pub detach_threshold: i64,
    pub chunk_threshold: i64,
    pub chunk_size: i64,
    pub scrub_preset: String,
}

fn coerce_int(value: Option<&Value>, default: i64) -> i64 {
    let Some(value) = value else { return default };
    if let Some(n) = value.as_i64() {
        return n;
    }
    if let Some(s) = value.as_str() {
        return s.parse::<i64>().unwrap_or(default);
    }
    default
}

fn get_object<'a>(root: &'a Value, key: &str) -> Option<&'a serde_json::Map<String, Value>> {
    root.get(key)?.as_object()
}

fn get_str_list(value: Option<&Value>) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    let Some(arr) = value.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect()
}

/// Load export configuration defaults from an existing bundle.
pub fn load_bundle_export_config(bundle_dir: &Path) -> ShareResult<StoredExportConfig> {
    let manifest_path = bundle_dir.join("manifest.json");
    if !manifest_path.exists() {
        return Err(ShareError::ManifestNotFound {
            path: bundle_dir.display().to_string(),
        });
    }
    let manifest_text =
        std::fs::read_to_string(&manifest_path).map_err(|e| ShareError::ManifestParse {
            message: e.to_string(),
        })?;
    let manifest: Value =
        serde_json::from_str(&manifest_text).map_err(|e| ShareError::ManifestParse {
            message: e.to_string(),
        })?;

    let export_config = get_object(&manifest, "export_config");
    let attachments_section = get_object(&manifest, "attachments");
    let attachments_config = attachments_section
        .and_then(|v| v.get("config"))
        .and_then(|v| v.as_object());
    let project_scope = get_object(&manifest, "project_scope");
    let scrub_section = get_object(&manifest, "scrub");
    let database_section = get_object(&manifest, "database");

    let raw_projects = export_config
        .and_then(|v| v.get("projects"))
        .or_else(|| project_scope.and_then(|v| v.get("requested")));
    let projects = get_str_list(raw_projects);

    let scrub_preset = export_config
        .and_then(|v| v.get("scrub_preset"))
        .and_then(|v| v.as_str())
        .or_else(|| {
            scrub_section
                .and_then(|v| v.get("preset"))
                .and_then(|v| v.as_str())
        })
        .unwrap_or("standard")
        .to_string();

    let inline_threshold = coerce_int(
        export_config
            .and_then(|v| v.get("inline_threshold"))
            .or_else(|| attachments_config.and_then(|v| v.get("inline_threshold"))),
        INLINE_ATTACHMENT_THRESHOLD as i64,
    );
    let detach_threshold = coerce_int(
        export_config
            .and_then(|v| v.get("detach_threshold"))
            .or_else(|| attachments_config.and_then(|v| v.get("detach_threshold"))),
        DETACH_ATTACHMENT_THRESHOLD as i64,
    );
    let chunk_threshold = coerce_int(
        export_config.and_then(|v| v.get("chunk_threshold")),
        DEFAULT_CHUNK_THRESHOLD as i64,
    );

    let chunk_manifest = database_section
        .and_then(|v| v.get("chunk_manifest"))
        .and_then(|v| v.as_object());
    let mut chunk_size = coerce_int(
        export_config
            .and_then(|v| v.get("chunk_size"))
            .or_else(|| chunk_manifest.and_then(|v| v.get("chunk_size"))),
        DEFAULT_CHUNK_SIZE as i64,
    );

    let chunk_config_path = bundle_dir.join("mailbox.sqlite3.config.json");
    if chunk_config_path.exists()
        && let Ok(text) = std::fs::read_to_string(&chunk_config_path)
        && let Ok(config) = serde_json::from_str::<Value>(&text)
        && let Some(obj) = config.as_object()
    {
        chunk_size = coerce_int(obj.get("chunk_size"), chunk_size);
        let threshold = coerce_int(obj.get("threshold_bytes"), chunk_threshold);
        return Ok(StoredExportConfig {
            projects,
            inline_threshold,
            detach_threshold,
            chunk_threshold: threshold,
            chunk_size,
            scrub_preset,
        });
    }

    Ok(StoredExportConfig {
        projects,
        inline_threshold,
        detach_threshold,
        chunk_threshold,
        chunk_size,
        scrub_preset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn export_redaction_policy_presets_map_to_expected_flags() {
        let standard = ExportRedactionPolicy::from_preset(ScrubPreset::Standard);
        assert!(standard.scan_secrets);
        assert!(!standard.redact_bodies);
        assert!(!standard.redact_recipients);
        assert!(standard.is_active());

        let strict = ExportRedactionPolicy::from_preset(ScrubPreset::Strict);
        assert!(strict.scan_secrets);
        assert!(strict.redact_bodies);
        assert!(strict.redact_recipients);
        assert!(strict.is_active());

        let archive = ExportRedactionPolicy::from_preset(ScrubPreset::Archive);
        assert!(!archive.scan_secrets);
        assert!(!archive.redact_bodies);
        assert!(!archive.redact_recipients);
        assert!(!archive.is_active());
    }

    #[test]
    fn redaction_audit_log_records_reason_counters() {
        let mut log = RedactionAuditLog::default();
        log.record(
            RedactionReason::SecretDetected,
            "msg-1".to_string(),
            Some(1),
        );
        log.record(RedactionReason::BodyRedacted, "msg-2".to_string(), Some(2));
        log.record(RedactionReason::ScrubPreset, "msg-3".to_string(), Some(3));
        log.record(
            RedactionReason::SnippetExcluded,
            "search-1".to_string(),
            None,
        );
        log.record(
            RedactionReason::RecipientsHidden,
            "msg-4".to_string(),
            Some(4),
        );

        assert_eq!(log.total(), 5);
        assert_eq!(log.events.len(), 5);
        assert_eq!(log.secrets_caught, 1);
        assert_eq!(log.bodies_redacted, 2);
        assert_eq!(log.snippets_filtered, 1);
        assert_eq!(log.recipients_hidden, 1);
        assert_eq!(
            RedactionReason::SnippetExcluded.description(),
            "Search snippet excluded to prevent content leakage"
        );
    }

    #[test]
    fn normalize_scrub_preset_accepts_case_and_trims() {
        assert_eq!(
            normalize_scrub_preset("  StRiCt  ").expect("strict preset should parse"),
            ScrubPreset::Strict
        );
        assert_eq!(
            normalize_scrub_preset("standard").expect("standard preset should parse"),
            ScrubPreset::Standard
        );
        assert_eq!(
            normalize_scrub_preset("archive").expect("archive preset should parse"),
            ScrubPreset::Archive
        );
    }

    #[test]
    fn normalize_scrub_preset_rejects_unknown_values() {
        let err = normalize_scrub_preset("  unknown  ").expect_err("expected invalid preset");
        match err {
            ShareError::InvalidScrubPreset { preset } => assert_eq!(preset, "unknown"),
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn adjust_detach_threshold_preserves_or_bumps_value() {
        assert_eq!(adjust_detach_threshold(10_000, 20_000), 20_000);
        assert_eq!(adjust_detach_threshold(10_000, 10_000), 15_000);
        assert_eq!(adjust_detach_threshold(512, 1), 1_536);
    }

    #[test]
    fn validate_thresholds_rejects_invalid_inputs() {
        let cases = [
            ("inline_threshold", -1, 10, 20, 1024),
            ("detach_threshold", 1, -10, 20, 1024),
            ("chunk_threshold", 1, 10, -20, 1024),
            ("chunk_size", 1, 10, 20, 1023),
        ];
        for (field, inline, detach, chunk_threshold, chunk_size) in cases {
            let err = validate_thresholds(inline, detach, chunk_threshold, chunk_size)
                .expect_err("expected invalid threshold");
            match err {
                ShareError::InvalidThreshold {
                    field: actual_field,
                    ..
                } => assert_eq!(actual_field, field),
                other => panic!("unexpected error variant: {other:?}"),
            }
        }
    }

    #[test]
    fn validate_thresholds_accepts_non_negative_and_min_chunk_size() {
        validate_thresholds(0, 0, 0, 1024).expect("thresholds should be valid");
    }

    #[test]
    fn default_decrypt_output_strips_age_extension() {
        assert_eq!(
            default_decrypt_output(Path::new("/tmp/export.zip.age")),
            PathBuf::from("/tmp/export.zip")
        );
        assert_eq!(
            default_decrypt_output(Path::new("/tmp/export.tar.AGE")),
            PathBuf::from("/tmp/export.tar")
        );
    }

    #[test]
    fn default_decrypt_output_adds_suffix_for_non_age_files() {
        assert_eq!(
            default_decrypt_output(Path::new("/tmp/export.tar.gz")),
            PathBuf::from("/tmp/export.tar_decrypted.gz")
        );
        assert_eq!(
            default_decrypt_output(Path::new("/tmp/export")),
            PathBuf::from("/tmp/export_decrypted")
        );
    }

    #[test]
    fn resolve_sqlite_database_path_handles_prefixes_and_relative_paths() {
        assert_eq!(
            resolve_sqlite_database_path("sqlite:////tmp/agent-mail.sqlite3")
                .expect("absolute path should resolve"),
            PathBuf::from("/tmp/agent-mail.sqlite3")
        );

        let relative = resolve_sqlite_database_path("sqlite:var/db.sqlite3")
            .expect("relative path should resolve");
        assert!(relative.ends_with(Path::new("var/db.sqlite3")));

        assert_eq!(
            resolve_sqlite_database_path("sqlite+aiosqlite:////tmp/agent-mail-aio.sqlite3")
                .expect("aiosqlite prefix should resolve"),
            PathBuf::from("/tmp/agent-mail-aio.sqlite3")
        );
    }

    #[test]
    fn resolve_sqlite_database_path_rejects_empty_input() {
        let err = resolve_sqlite_database_path("").expect_err("empty url should fail");
        match err {
            ShareError::SnapshotSourceNotFound { path } => {
                assert_eq!(path, "empty database URL");
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn load_bundle_export_config_errors_when_manifest_missing() {
        let dir = tempdir().expect("tempdir");
        let err = load_bundle_export_config(dir.path()).expect_err("missing manifest should fail");
        match err {
            ShareError::ManifestNotFound { path } => {
                assert_eq!(path, dir.path().display().to_string());
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn load_bundle_export_config_errors_on_invalid_manifest_json() {
        let dir = tempdir().expect("tempdir");
        std::fs::write(dir.path().join("manifest.json"), "{not json").expect("write manifest");

        let err =
            load_bundle_export_config(dir.path()).expect_err("invalid manifest should fail parse");
        match err {
            ShareError::ManifestParse { message } => {
                assert!(!message.is_empty());
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn load_bundle_export_config_prefers_export_config_values() {
        let dir = tempdir().expect("tempdir");
        let manifest = json!({
            "export_config": {
                "projects": ["alpha", "beta"],
                "inline_threshold": "111",
                "detach_threshold": 222,
                "chunk_threshold": "333",
                "chunk_size": "4444",
                "scrub_preset": "strict"
            },
            "attachments": {
                "config": {
                    "inline_threshold": 9_999,
                    "detach_threshold": 9_998
                }
            },
            "project_scope": {"requested": ["fallback"]},
            "scrub": {"preset": "archive"}
        });
        std::fs::write(
            dir.path().join("manifest.json"),
            serde_json::to_string_pretty(&manifest).expect("serialize manifest"),
        )
        .expect("write manifest");

        let cfg = load_bundle_export_config(dir.path()).expect("manifest should parse");
        assert_eq!(cfg.projects, vec!["alpha".to_string(), "beta".to_string()]);
        assert_eq!(cfg.inline_threshold, 111);
        assert_eq!(cfg.detach_threshold, 222);
        assert_eq!(cfg.chunk_threshold, 333);
        assert_eq!(cfg.chunk_size, 4_444);
        assert_eq!(cfg.scrub_preset, "strict");
    }

    #[test]
    fn load_bundle_export_config_uses_fallback_sections_when_export_missing() {
        let dir = tempdir().expect("tempdir");
        let manifest = json!({
            "project_scope": {"requested": ["project-a"]},
            "attachments": {
                "config": {
                    "inline_threshold": 1_500,
                    "detach_threshold": 2_500
                }
            },
            "database": {
                "chunk_manifest": {
                    "chunk_size": 3_072
                }
            },
            "scrub": {"preset": "archive"}
        });
        std::fs::write(
            dir.path().join("manifest.json"),
            serde_json::to_string_pretty(&manifest).expect("serialize manifest"),
        )
        .expect("write manifest");

        let cfg = load_bundle_export_config(dir.path()).expect("manifest should parse");
        assert_eq!(cfg.projects, vec!["project-a".to_string()]);
        assert_eq!(cfg.inline_threshold, 1_500);
        assert_eq!(cfg.detach_threshold, 2_500);
        assert_eq!(cfg.chunk_threshold, DEFAULT_CHUNK_THRESHOLD as i64);
        assert_eq!(cfg.chunk_size, 3_072);
        assert_eq!(cfg.scrub_preset, "archive");
    }

    #[test]
    fn load_bundle_export_config_chunk_config_file_overrides_thresholds() {
        let dir = tempdir().expect("tempdir");
        let manifest = json!({
            "export_config": {
                "chunk_threshold": 5_000,
                "chunk_size": 6_000
            }
        });
        std::fs::write(
            dir.path().join("manifest.json"),
            serde_json::to_string_pretty(&manifest).expect("serialize manifest"),
        )
        .expect("write manifest");
        let chunk_cfg = json!({
            "chunk_size": "7000",
            "threshold_bytes": "8000"
        });
        std::fs::write(
            dir.path().join("mailbox.sqlite3.config.json"),
            serde_json::to_string_pretty(&chunk_cfg).expect("serialize chunk config"),
        )
        .expect("write chunk config");

        let cfg = load_bundle_export_config(dir.path()).expect("manifest should parse");
        assert_eq!(cfg.chunk_size, 7_000);
        assert_eq!(cfg.chunk_threshold, 8_000);
    }
}
