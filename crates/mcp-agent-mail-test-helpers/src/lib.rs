//! Shared test helpers for the git 2.51.0 hardening epic (br-8ujfs).
//!
//! # What this crate provides
//!
//! - [`shim_git`]: builders for fake `git` binaries with controlled
//!   version output, exit behavior, and delays.
//! - [`repo`]: factory for tempdir repos in every shape we care about
//!   (empty, with commits, with orphan stash, bare, worktree, etc.).
//!
//! # Why a dedicated crate
//!
//! Tracks A-F across the epic each need 3-5 test files that all build
//! the same fixture scaffolding. Centralizing here prevents drift and
//! deduplicates ~500 lines of inline scaffolding.
//!
//! # Scope
//!
//! - dev-dependency ONLY. Production code must never pull this in.
//! - Safe on Unix and Windows; shim-script builder produces `.bat` on
//!   Windows.
//! - Uses `tempfile::TempDir` for guaranteed cleanup.

#![forbid(unsafe_code)]

pub mod parity;
pub mod repo;
pub mod shim_git;

// Re-export so callers can `use mcp_agent_mail_test_helpers::*` if they
// prefer.
pub use repo::{RepoBuilder, RepoFixture};
pub use shim_git::{ShimBehavior, ShimExit, build_shim_git};
