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
pub mod config;
pub mod diagnostics;
pub mod disk;
pub mod error;
pub mod identity;
pub mod intern;
pub mod lock_order;
pub mod metrics;
pub mod models;
pub mod slo;
pub mod toon;

// Re-export key types for convenience
pub use agent_detect::{
    AgentDetectError, AgentDetectOptions, AgentDetectRootOverride, InstalledAgentDetectionEntry,
    InstalledAgentDetectionReport, InstalledAgentDetectionSummary, detect_installed_agents,
};
pub use backpressure::{
    HealthLevel, HealthSignals, cached_health_level, compute_health_level,
    compute_health_level_with_signals, is_shedable_tool, level_transitions, refresh_health_level,
};
pub use config::{
    AgentNameEnforcementMode, AppEnvironment, Config, ProjectIdentityMode, RateLimitBackend,
};
pub use diagnostics::{
    DiagnosticReport, HealthInfo, Recommendation, SystemInfo, init_process_start,
};
pub use error::{Error as MailError, Result as MailResult};
pub use identity::{ProjectIdentity, compute_project_slug, resolve_project_identity, slugify};
pub use intern::{InternedStr, intern, intern_count, pre_intern, pre_intern_policies};
pub use lock_order::{
    LockContentionEntry, LockLevel, OrderedMutex, OrderedRwLock, lock_contention_reset,
    lock_contention_snapshot,
};
pub use metrics::{
    Counter, DbMetricsSnapshot, GaugeI64, GaugeU64, GlobalMetricsSnapshot, HistogramSnapshot,
    HttpMetricsSnapshot, Log2Histogram, StorageMetricsSnapshot, ToolsMetricsSnapshot,
    global_metrics,
};
pub use models::{
    Agent, AgentLink, ConsistencyMessageRef, ConsistencyReport, FileReservation, Message,
    MessageRecipient, Product, ProductProjectLink, Project, ProjectSiblingSuggestion,
    VALID_ADJECTIVES, VALID_NOUNS, generate_agent_name, is_valid_agent_name,
};
pub use slo::{OpClass, PoolHealth};
