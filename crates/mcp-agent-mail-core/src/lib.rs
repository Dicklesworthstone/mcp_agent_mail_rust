//! Core types, configuration, and models for MCP Agent Mail
//!
//! This crate provides:
//! - Configuration management (`Config`, environment parsing)
//! - Data models (`Agent`, `Message`, `Project`, etc.)
//! - Agent name validation and generation
//! - Common error types

#![forbid(unsafe_code)]

pub mod agent_detect;
pub mod backpressure;
pub mod bocpd;
pub mod config;
pub mod conformal;
pub mod diagnostics;
pub mod disk;
pub mod error;
pub mod evidence_ledger;
pub mod flake_triage;
pub mod identity;
pub mod intern;
pub mod kpi;
pub mod lock_order;
pub mod memory;
pub mod metrics;
pub mod models;
pub mod setup;
pub mod slo;
pub mod test_harness;
pub mod toon;

#[cfg(test)]
pub mod proptest_generators;

// Re-export key types for convenience
pub use agent_detect::{
    AgentDetectError, AgentDetectOptions, AgentDetectRootOverride, InstalledAgentDetectionEntry,
    InstalledAgentDetectionReport, InstalledAgentDetectionSummary, detect_installed_agents,
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
pub use slo::{OpClass, PoolHealth};
