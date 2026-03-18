//! Core types, configuration, and models for MCP Agent Mail
//!
//! This crate provides:
//! - Configuration management (`Config`, environment parsing)
//! - Data models (`Agent`, `Message`, `Project`, etc.)
//! - Agent name validation and generation
//! - Common error types

#![forbid(unsafe_code)]

pub mod agent_detect;
pub mod atc_adaptation;
pub mod atc_admissibility;
pub mod atc_assumptions;
pub mod atc_attribution;
pub mod atc_baseline;
pub mod atc_contamination;
pub mod atc_effect_semantics;
pub mod atc_fairness;
pub mod atc_invariants;
pub mod atc_labeling;
pub mod atc_open_index;
pub mod atc_participation;
pub mod atc_policy_certificates;
pub mod atc_regime;
pub mod atc_retention;
pub mod atc_risk_budgets;
pub mod atc_shrinkage;
pub mod atc_transparency;
pub mod atc_user_surfaces;
pub mod atc_voi_control;
pub mod backpressure;
pub mod bocpd;
pub mod config;
pub mod conformal;
pub mod diagnostics;
pub mod disk;
pub mod error;
pub mod evidence_ledger;
pub mod experience;
pub mod flake_triage;
pub mod identity;
pub mod intern;
pub mod kpi;
pub mod lock_order;
pub mod mcp_config;
pub mod memory;
pub mod metrics;
pub mod models;
pub mod pane_identity;
pub mod pattern_overlap;
pub mod search_types;
pub mod setup;
pub mod slo;
pub mod test_harness;
pub mod timestamps;
pub mod toon;

#[cfg(test)]
pub mod proptest_generators;

// Re-export key types for convenience
pub use agent_detect::{
    AgentDetectError, AgentDetectOptions, AgentDetectRootOverride, InstalledAgentDetectionEntry,
    InstalledAgentDetectionReport, InstalledAgentDetectionSummary, detect_installed_agents,
};
pub use atc_fairness::{
    ACCEPTABLE_ASYMMETRY_RULES, AGENT_STARVATION_WINDOW_MICROS, COHORT_STARVATION_WINDOW_MICROS,
    DEFAULT_REGIME_DISCOUNT_BP, FairnessAssessment, FairnessBudget, FairnessDisposition,
    FairnessEvent, FairnessEventKind, FairnessMetric, FairnessScope, FairnessSnapshot,
    FairnessTarget, FairnessViolationCode, ImpactedTarget, PROJECT_STARVATION_WINDOW_MICROS,
    StarvedTarget, TradeoffWinner, UtilityTradeoff,
};
pub use atc_retention::{
    ARCHIVE_DISCOVERABILITY_MIN_DAYS, ATC_RETENTION_RULES, ArchiveRetention, ArchiveTrigger,
    ArtifactRetentionRule, CompactionStrategy, ComparabilityAnchor,
    EVIDENCE_LEDGER_DROP_AFTER_DAYS, EVIDENCE_LEDGER_HOT_DAYS, FORENSIC_TRACE_DISCOVERABILITY_DAYS,
    GIT_ARCHIVE_DEFAULT_ARTIFACTS, GIT_ARCHIVE_DENYLIST, GIT_ARCHIVE_EXPLICIT_EXCLUSIONS,
    GIT_ARCHIVE_PROMOTION_ONLY_ARTIFACTS, LearningArtifactKind, OPEN_EXPERIENCE_STALE_AFTER_DAYS,
    OPEN_EXPERIENCE_TERMINALIZE_AFTER_DAYS, OPERATOR_LIFECYCLE_RULES, PERIODIC_AUDIT_CADENCE_DAYS,
    POLICY_SNAPSHOT_HOT_DAYS, REPLAY_DISCOVERABILITY_REQUIREMENTS,
    RESOLVED_EXPERIENCE_DROP_AFTER_DAYS, RESOLVED_EXPERIENCE_FULL_FIDELITY_DAYS, ROLLUP_LIVE_DAYS,
    STALE_REGIME_AFTER_DAYS, StoragePlane, retention_rule,
};
pub use backpressure::{
    HealthLevel, HealthSignals, cached_health_level, compute_health_level,
    compute_health_level_with_signals, is_shedable_tool, level_transitions, refresh_health_level,
    set_shedding_enabled, shedding_enabled, should_shed_tool,
};
pub use config::{AppEnvironment, Config, InterfaceMode, ProjectIdentityMode, RateLimitBackend};
pub use diagnostics::{
    DiagnosticReport, HealthInfo, Recommendation, SystemInfo, init_process_start,
};
pub use error::{Error as MailError, Result as MailResult};
pub use evidence_ledger::{
    EVIDENCE_LEDGER_PATH_ENV, EvidenceLedger, EvidenceLedgerEntry,
    append_evidence_entry_if_configured, append_evidence_entry_to_path, evidence_ledger,
};
pub use experience::{
    EffectKind, ExperienceBuilder, ExperienceOutcome, ExperienceRow, ExperienceState,
    ExperienceSubsystem, FEATURE_VERSION, FeatureExtension, FeatureVector, NonExecutionReason,
    loss_to_bp, prob_to_bp, saturating_u8, saturating_u16, validate_transition,
};
pub use identity::{ProjectIdentity, compute_project_slug, resolve_project_identity, slugify};
pub use intern::{InternedStr, intern, intern_count, pre_intern, pre_intern_policies};
pub use kpi::{
    AckPressureKpi, AnomalyAlert, AnomalyKind, AnomalySeverity, AnomalyThresholds, ContentionKpi,
    CorrelationPair, ForecastPoint, InsightCard, InsightFeed, KpiReport, KpiSnapshot, KpiWindow,
    LatencyKpi, Sensitivity, ThroughputKpi, TrendDirection, TrendIndicator, TrendReport,
    build_insight_feed, compute_correlations, compute_forecasts, compute_trends, detect_anomalies,
    kpi_gauges, latest_raw as kpi_latest_raw, quick_anomaly_scan, quick_insight_feed,
    quick_trend_report, record_sample as kpi_record_sample, report as kpi_report,
    reset_samples as kpi_reset_samples, sample_count as kpi_sample_count, snapshot as kpi_snapshot,
    trend_report,
};
pub use lock_order::{
    LockContentionEntry, LockLevel, OrderedMutex, OrderedRwLock, lock_contention_reset,
    lock_contention_snapshot,
};
pub use mcp_config::{
    McpConfigDetectParams, McpConfigLocation, McpConfigTool, detect_mcp_config_locations,
    detect_mcp_config_locations_default,
};
pub use memory::{MemoryPressure, MemorySample};
pub use metrics::{
    Counter, DbMetricsSnapshot, GaugeI64, GaugeU64, GlobalMetricsSnapshot, HistogramSnapshot,
    HttpMetricsSnapshot, Log2Histogram, StorageMetricsSnapshot, ToolsMetricsSnapshot,
    global_metrics,
};
pub use models::{
    Agent, AgentLink, ConsistencyMessageRef, ConsistencyReport, FileReservation,
    KNOWN_PROGRAM_NAMES, MODEL_NAME_PATTERNS, Message, MessageRecipient, Product,
    ProductProjectLink, Project, ProjectSiblingSuggestion, VALID_ADJECTIVES, VALID_NOUNS,
    detect_agent_name_mistake, generate_agent_name, is_valid_agent_name, looks_like_model_name,
    looks_like_program_name, looks_like_unix_username,
};
pub use pane_identity::{
    canonical_identity_path, cleanup_all_stale_identities, cleanup_stale_identities,
    get_composite_tmux_pane_id, list_identities, resolve_identity, resolve_identity_current_pane,
    resolve_identity_with_path, write_identity, write_identity_current_pane,
};
pub use search_types::{
    DateRange, DocChange, DocId, DocKind, Document, ExplainComposerConfig, ExplainReasonCode,
    ExplainReport, ExplainStage, ExplainVerbosity, HighlightRange, HitExplanation,
    ImportanceFilter, ScoreFactor, SearchFilter, SearchHit, SearchMode, SearchQuery, SearchResults,
    StageExplanation, StageScoreInput, compose_explain_report, compose_hit_explanation,
    factor_sort_cmp, missing_stage, redact_hit_explanation, redact_report_for_docs,
};
pub use slo::{OpClass, PoolHealth};
pub use test_harness::DeterministicClock;
pub use timestamps::{
    ClockSkewMetrics, clock_skew_metrics, clock_skew_reset, iso_to_micros, micros_to_iso,
    micros_to_naive, naive_to_micros, now_micros, now_micros_raw,
};
pub use toon::{
    EncoderError, EncoderSuccess, FormatDecision, ToonEnvelope, ToonMeta, ToonStats,
    apply_resource_format, apply_tool_format, apply_toon_format, looks_like_toon_rust_encoder,
    parse_toon_stats, resolve_encoder, resolve_output_format, run_encoder, validate_encoder,
};
