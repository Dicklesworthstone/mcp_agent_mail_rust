//! Core types, configuration, and models for MCP Agent Mail
//!
//! This crate provides:
//! - Configuration management (`Config`, environment parsing)
//! - Data models (`Agent`, `Message`, `Project`, etc.)
//! - Agent name validation and generation
//! - Common error types

#![forbid(unsafe_code)]

pub mod agent_detect;
pub mod config;
pub mod error;
pub mod identity;
pub mod models;
pub mod toon;

// Re-export key types for convenience
pub use agent_detect::{
    AgentDetectError, AgentDetectOptions, AgentDetectRootOverride, InstalledAgentDetectionEntry,
    InstalledAgentDetectionReport, InstalledAgentDetectionSummary, detect_installed_agents,
};
pub use config::{
    AgentNameEnforcementMode, AppEnvironment, Config, ProjectIdentityMode, RateLimitBackend,
};
pub use error::{Error as MailError, Result as MailResult};
pub use identity::{ProjectIdentity, compute_project_slug, resolve_project_identity, slugify};
pub use models::{
    Agent, AgentLink, FileReservation, Message, MessageRecipient, Product, ProductProjectLink,
    Project, ProjectSiblingSuggestion, VALID_ADJECTIVES, VALID_NOUNS, generate_agent_name,
    is_valid_agent_name,
};
