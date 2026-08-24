//! Canonical per-pane agent identity file contract.
//!
//! Resolves the diverging conventions described in `mcp_agent_mail#111`:
//!
//! - Claude Code: `~/.claude/agent-mail/identity.$TMUX_PANE` (persistent, not project-scoped)
//! - NTM #68: `/tmp/agent-mail-name.<hash>.<pane_id>` (project-scoped, ephemeral)
//!
//! The canonical contract:
//!
//! - **Path**: `~/.config/agent-mail/identity/<project_hash>/<pane_key>`
//! - **Pane key**: Composite `session_name:window_index:pane_index` via
//!   `tmux display-message`, falling back to bare `$TMUX_PANE` (see #41).
//! - **Content**: JSON [`PaneIdentityRecord`] carrying the agent name plus the
//!   tmux binding facts (`session_name`, `pane_id`, `pane_pid`, `socket_path`,
//!   `written_at`) needed to verify liveness (GH#252). Legacy bare-name files
//!   (plain text, single line) parse as a record with only `name` set.
//! - **Liveness**: A recorded binding is live iff the tmux server at the
//!   recorded `socket_path` reports the recorded pane in the recorded session
//!   with the recorded root `pane_pid` and a non-shell foreground command
//!   (see [`binding_liveness`]). Resolution never hands a live binding's name
//!   to a different pane; dead bindings are adopted in place (GH#252).
//! - **Fallback**: Reads from legacy bare-pane-ID files and older paths for
//!   backwards compatibility
//! - **Cleanup**: Stale identity files (panes that no longer exist) can be pruned
//!
//! All agent runtimes (Claude Code, NTM/Codex, Gemini, etc.) should converge on
//! [`write_identity`] and [`resolve_identity`] as the single source of truth.

use sha1::{Digest, Sha1};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Top-level directory under `~/.config` for agent-mail pane identity files.
const IDENTITY_DIR_NAME: &str = "agent-mail/identity";

/// How many hex chars of the project hash to use in the directory name.
const PROJECT_HASH_LEN: usize = 12;

/// tmux format used to probe a recorded binding's liveness on its own server.
const LIVENESS_PROBE_FORMAT: &str = "#{session_name}\t#{pane_pid}\t#{pane_current_command}";

/// tmux format used to gather the binding facts of the pane a caller is
/// writing or resolving (the reuse-seed pane named by the identity key).
const TARGET_FACTS_FORMAT: &str =
    "#{session_name}\t#{pane_id}\t#{pane_pid}\t#{pane_current_command}\t#{socket_path}";

/// Plain interactive shells. A pane whose foreground command is one of these
/// (or empty) has no agent running in it — the agent exited back to its shell —
/// so it fails liveness check (c). Runtime wrappers (`node`, `bun`, `python`,
/// ...) intentionally do NOT appear here: agents commonly run under them, and
/// treating an unknown command as live is the conservative choice (it blocks
/// name theft rather than enabling it).
const SHELL_COMMANDS: &[&str] = &[
    "ash", "bash", "csh", "dash", "fish", "ksh", "login", "nu", "pwsh", "sh", "tcsh", "zsh",
];

#[cfg(test)]
static TEST_CONFIG_BASE_DIR: std::sync::LazyLock<std::sync::Mutex<Option<PathBuf>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

#[cfg(test)]
static TEST_LIVE_TMUX_PANES: std::sync::LazyLock<std::sync::Mutex<Option<Vec<String>>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Structured pane-identity record stored in identity files (GH#252).
///
/// The `name` is the agent name (the only field a legacy bare-name file
/// carries). The remaining fields are the tmux binding facts recorded at write
/// time so later resolutions can verify whether the binding is still live:
///
/// - `session_name`: `#{session_name}` of the bound pane
/// - `pane_id`: bare tmux pane id (e.g. `%25`) — stable for the pane's lifetime
/// - `pane_pid`: `#{pane_pid}`, the root process tmux spawned in the pane
/// - `socket_path`: the tmux server socket the pane lives on
/// - `written_at`: RFC 3339 timestamp of the write (informational)
///
/// Records written outside tmux carry only `name` and are unverifiable, which
/// preserves the pre-GH#252 trust-the-file behavior for non-tmux callers.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PaneIdentityRecord {
    /// Agent name bound to the pane (e.g. `BlueLake`).
    pub name: String,
    /// tmux `#{session_name}` recorded at write time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    /// Bare tmux pane id (e.g. `%25`) recorded at write time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    /// tmux `#{pane_pid}` recorded at write time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_pid: Option<u32>,
    /// tmux server socket path recorded at write time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket_path: Option<String>,
    /// RFC 3339 timestamp of the write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub written_at: Option<String>,
}

impl PaneIdentityRecord {
    /// Build a record carrying only the agent name (legacy-equivalent).
    #[must_use]
    pub fn bare(name: &str) -> Self {
        Self {
            name: name.trim().to_string(),
            session_name: None,
            pane_id: None,
            pane_pid: None,
            socket_path: None,
            written_at: None,
        }
    }

    /// Whether the record carries every fact the liveness predicate needs.
    #[must_use]
    pub const fn is_verifiable(&self) -> bool {
        self.session_name.is_some()
            && self.pane_id.is_some()
            && self.pane_pid.is_some()
            && self.socket_path.is_some()
    }
}

/// Outcome of the GH#252 liveness predicate for a recorded pane binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneBindingLiveness {
    /// All three checks passed: the recorded pane exists in the recorded
    /// session on the recorded socket, its root pid matches, and an agent
    /// (non-shell) command is running in it.
    Live,
    /// Any check failed: tmux answered and the recorded pane/session/pid/
    /// command no longer hold, the server at the recorded socket is gone, or
    /// the socket file itself no longer exists.
    Dead,
    /// The predicate could not be run: the record does not carry the facts
    /// it needs (legacy bare-name file, or a record written outside tmux),
    /// or the `tmux` binary cannot be executed by this process. The latter
    /// is deliberately NOT `Dead`: a daemon whose `PATH` lacks tmux must not
    /// see every structured record as adoptable/purgeable.
    Unverifiable,
}

/// How a resolved pane identity relates to the liveness contract (GH#252).
///
/// Extends the GH#240 source-category surface: tool/CLI output reports this
/// alongside [`identity_source_category`] (which keeps its existing variants).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneBindingStatus {
    /// The record's binding passed the liveness predicate and the resolving
    /// pane is the recorded holder.
    VerifiedLive,
    /// The record's binding was dead; the resolving pane adopted the name
    /// (the record was rewritten with the adopter's binding facts when they
    /// were available).
    AdoptedDead,
    /// The record was unverifiable (legacy bare-name or written outside
    /// tmux) and was returned under the conservative compatibility rule.
    LegacyUnverified,
}

impl PaneBindingStatus {
    /// Stable string form surfaced in tool/CLI output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::VerifiedLive => "verified-live",
            Self::AdoptedDead => "adopted-dead",
            Self::LegacyUnverified => "legacy-unverified",
        }
    }
}

/// Classify whether a tmux `#{pane_current_command}` value looks like a
/// running agent process (liveness check (c) of GH#252).
///
/// An empty command means the pane's process is gone; a plain interactive
/// shell means the agent exited back to its shell. Anything else — including
/// runtime wrappers like `node`, `bun`, or `python` that agents commonly run
/// under — counts as an agent, which is the conservative direction: unknown
/// commands block adoption rather than enabling name theft.
#[must_use]
pub fn is_agent_pane_command(command: &str) -> bool {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return false;
    }
    let first = trimmed.split_whitespace().next().unwrap_or(trimmed);
    let base = first.rsplit('/').next().unwrap_or(first);
    // Login shells report as e.g. `-bash`.
    let base = base.strip_prefix('-').unwrap_or(base);
    !SHELL_COMMANDS.contains(&base)
}

/// Run the GH#252 liveness predicate for a recorded binding against the tmux
/// server at the recorded `socket_path`.
///
/// A binding is [`PaneBindingLiveness::Live`] iff:
///
/// 1. `tmux -S <socket> display-message -t <pane_id> -p '#{session_name}'`
///    succeeds and equals the recorded `session_name`;
/// 2. that pane's `#{pane_pid}` equals the recorded `pane_pid` (compared
///    against what tmux reports — never `kill -0` on a host pid);
/// 3. `#{pane_current_command}` is non-empty and not a plain shell
///    (see [`is_agent_pane_command`]).
///
/// Any failing check — including a missing socket or an unreachable server —
/// yields [`PaneBindingLiveness::Dead`]. Records without binding facts, and
/// records that cannot be checked because `tmux` itself cannot be executed
/// by this process, yield [`PaneBindingLiveness::Unverifiable`].
#[must_use]
pub fn binding_liveness(record: &PaneIdentityRecord) -> PaneBindingLiveness {
    if !record.is_verifiable() {
        return PaneBindingLiveness::Unverifiable;
    }
    if let Some(socket) = record.socket_path.as_deref()
        && !Path::new(socket).exists()
    {
        return PaneBindingLiveness::Dead;
    }
    // Distinguish "tmux ran and said no" (Dead) from "tmux could not be
    // spawned at all" (Unverifiable). Only the former is evidence about the
    // binding; the latter says nothing and must not enable adoption or
    // cleanup purges.
    let mut tmux_unavailable = false;
    let outcome = binding_liveness_with(record, |args| match run_tmux_capture(args) {
        Ok(stdout) => stdout,
        Err(_) => {
            tmux_unavailable = true;
            None
        }
    });
    if tmux_unavailable {
        return PaneBindingLiveness::Unverifiable;
    }
    outcome
}

/// Pure form of [`binding_liveness`]: the tmux invocation is supplied by the
/// caller so tests can fake server responses without shelling out.
///
/// `run_tmux` receives the full tmux argument vector (starting with `-S
/// <socket>`) and returns the command's stdout on success, or `None` when the
/// command fails. This variant does not check that the socket exists on disk;
/// [`binding_liveness`] does that before delegating here.
pub fn binding_liveness_with<F>(record: &PaneIdentityRecord, mut run_tmux: F) -> PaneBindingLiveness
where
    F: FnMut(&[&str]) -> Option<String>,
{
    let (Some(session_name), Some(recorded_pane), Some(recorded_pid), Some(socket_path)) = (
        record.session_name.as_deref(),
        record.pane_id.as_deref(),
        record.pane_pid,
        record.socket_path.as_deref(),
    ) else {
        return PaneBindingLiveness::Unverifiable;
    };

    let Some(output) = run_tmux(&[
        "-S",
        socket_path,
        "display-message",
        "-t",
        recorded_pane,
        "-p",
        LIVENESS_PROBE_FORMAT,
    ]) else {
        return PaneBindingLiveness::Dead;
    };

    let Some(line) = output.lines().next() else {
        return PaneBindingLiveness::Dead;
    };
    let mut fields = line.split('\t');
    let (Some(reported_session), Some(reported_pid), Some(reported_command)) =
        (fields.next(), fields.next(), fields.next())
    else {
        return PaneBindingLiveness::Dead;
    };

    if reported_session.trim() != session_name {
        return PaneBindingLiveness::Dead;
    }
    if reported_pid.trim().parse::<u32>() != Ok(recorded_pid) {
        return PaneBindingLiveness::Dead;
    }
    if !is_agent_pane_command(reported_command) {
        return PaneBindingLiveness::Dead;
    }
    PaneBindingLiveness::Live
}

/// Read and parse the identity record at `path`.
///
/// Applies the same symlink hardening as the name-only reader. A legacy
/// bare-name file parses as a record with only `name` set; a JSON file parses
/// as the full [`PaneIdentityRecord`]. Returns `None` for missing, empty, or
/// malformed files.
#[must_use]
pub fn read_identity_record(path: &Path) -> Option<PaneIdentityRecord> {
    if path_has_symlinked_parent(path).ok()? {
        return None;
    }
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() {
        return None;
    }
    let content = read_identity_file_no_follow(path).ok()?;
    parse_identity_record(&content)
}

/// Compute the canonical identity file path for a given project and tmux pane.
///
/// Returns `~/.config/agent-mail/identity/<project_hash>/<sanitized_pane_id>`.
/// The `project_key` is typically the absolute path to the project directory.
/// The `pane_id` is either a composite key (e.g., `main:0:2`) produced by
/// [`get_composite_tmux_pane_id`], or a bare tmux pane identifier (e.g., `%3`).
#[must_use]
pub fn canonical_identity_path(project_key: &str, pane_id: &str) -> PathBuf {
    let base = config_base_dir();
    let hash = project_hash(project_key);
    let sanitized_pane = sanitize_pane_id(pane_id);
    base.join(IDENTITY_DIR_NAME).join(hash).join(sanitized_pane)
}

/// Write an agent name to the canonical identity file for a pane.
///
/// Creates parent directories as needed. Returns the path written to on
/// success, or an IO error on failure.
///
/// The file content is a structured [`PaneIdentityRecord`] (GH#252): when the
/// target pane can be queried via tmux, the record carries the pane's binding
/// facts (`session_name`, `pane_id`, `pane_pid`, `socket_path`, `written_at`)
/// so later resolutions can verify liveness. Outside tmux the record carries
/// only the name, preserving the previous unverifiable behavior.
///
/// When the existing record at the path is a verifiably LIVE binding held by
/// a *different* pane than the one being written, the write is refused
/// (GH#252 adoption rule: never steal a live holder's slot silently). Callers
/// treat identity-file writes as best-effort, so a refusal degrades to their
/// existing warn-and-continue paths.
///
/// # Arguments
/// - `project_key`: Absolute path to the project directory (used for hashing)
/// - `pane_id`: Tmux pane identifier (e.g., `%0`)
/// - `agent_name`: The agent name to write (e.g., `BlueLake`)
///
/// # Errors
/// Returns an IO error when directories cannot be created, when the path or a
/// parent is symlinked, when the existing record is a live binding held by a
/// different pane, or when the write itself fails.
pub fn write_identity(
    project_key: &str,
    pane_id: &str,
    agent_name: &str,
) -> std::io::Result<PathBuf> {
    let path = canonical_identity_path(project_key, pane_id);
    if let Some(parent) = path.parent() {
        ensure_real_directory(parent)?;
    }
    if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "refusing to overwrite symlinked pane identity {}",
                path.display()
            ),
        ));
    }

    let facts = query_target_pane_facts(pane_id);

    // GH#252: never overwrite a verifiably live binding held by another pane.
    if let Some(existing) = read_identity_record(&path)
        && existing.is_verifiable()
        && binding_liveness(&existing) == PaneBindingLiveness::Live
    {
        let same_holder = facts.as_ref().is_some_and(|f| {
            existing.pane_id.as_deref() == Some(f.pane_id.as_str())
                && existing.socket_path.as_deref() == Some(f.socket_path.as_str())
        });
        if !same_holder {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!(
                    "refusing to overwrite live pane identity binding '{}' at {}: \
                     the recorded pane is still running an agent",
                    existing.name,
                    path.display()
                ),
            ));
        }
    }

    let record = record_from_facts(agent_name, facts.as_ref());
    write_record_content_no_follow(&path, &record)?;
    Ok(path)
}

/// Resolve the agent name for a given project and tmux pane.
///
/// Checks the following locations in order:
/// 1. Canonical path: `~/.config/agent-mail/identity/<project_hash>/<pane_id>`
/// 2. Legacy Claude Code path: `~/.claude/agent-mail/identity.<pane_id>`
/// 3. Legacy NTM path: `/tmp/agent-mail-name.<project_hash>.<pane_id>`
///
/// Returns `None` if no identity file is found or all are empty.
#[must_use]
pub fn resolve_identity(project_key: &str, pane_id: &str) -> Option<String> {
    resolve_identity_with_path(project_key, pane_id).map(|(name, _)| name)
}

/// Resolve the agent name and the identity file path actually used.
///
/// This follows the same lookup order as [`resolve_identity`], but returns the
/// concrete file path that produced the winning match. Callers that surface the
/// resolved path to operators should prefer this helper so diagnostics reflect
/// reality when a legacy fallback file is read.
///
/// When `pane_id` is a composite key (contains `:`), also tries a legacy
/// lookup using the bare `$TMUX_PANE` value to ensure backwards compatibility
/// with identity files written before the composite key migration.
///
/// Every candidate found by the lookup passes through the GH#252 liveness
/// predicate before being returned; see [`resolve_identity_with_binding`] for
/// the adoption semantics (this wrapper simply discards the binding status).
#[must_use]
pub fn resolve_identity_with_path(project_key: &str, pane_id: &str) -> Option<(String, PathBuf)> {
    resolve_identity_with_binding(project_key, pane_id).map(|(name, path, _)| (name, path))
}

/// Resolve the agent name, identity file path, and GH#252 binding status.
///
/// Follows the exact lookup order of [`resolve_identity_with_path`] (key
/// formats and lookup order are unchanged by GH#252 — the positional key is
/// the reuse *seed*, not the trust anchor). Each candidate record found is
/// classified with the liveness predicate before being returned:
///
/// - **live** binding held by the resolving pane → returned as
///   [`PaneBindingStatus::VerifiedLive`];
/// - **live** binding held by a *different* pane → the candidate is skipped
///   (never hand a live agent's name to a second process) and the lookup
///   continues; when no candidate survives, `None` is returned so the caller
///   mints a fresh identity;
/// - **dead** binding → adopted: the name is returned as
///   [`PaneBindingStatus::AdoptedDead`] and the record is atomically
///   rewritten (best-effort) with the resolving pane's own binding facts;
/// - **unverifiable** record (legacy bare-name, written outside tmux, or a
///   structured record this process cannot check because `tmux` is not
///   executable here):
///   conservative compatibility — if the pane named by the file's key exists
///   and is running an agent, the record is returned untouched as
///   [`PaneBindingStatus::LegacyUnverified`]; if the pane is gone or idles in
///   a plain shell, the name is adopted (upgrading the file to a structured
///   record) as [`PaneBindingStatus::AdoptedDead`]; without any tmux context
///   the name is returned untouched, preserving pre-GH#252 behavior.
#[must_use]
pub fn resolve_identity_with_binding(
    project_key: &str,
    pane_id: &str,
) -> Option<(String, PathBuf, PaneBindingStatus)> {
    let mut resolver = PaneBindingResolver::new(pane_id);

    // 1. Canonical path (composite or bare)
    let canonical = canonical_identity_path(project_key, pane_id);
    if let Some(hit) = resolver.consider(canonical) {
        return Some(hit);
    }

    // 1b. If pane_id is a composite key, try legacy bare $TMUX_PANE canonical path.
    //     A composite key contains `:`, e.g., `main:0:2`. The bare pane env var
    //     is something like `%3`. We check the env so we can find files written
    //     before the composite key migration.
    if pane_id.contains(':')
        && let Some(bare) = tmux_pane_env()
    {
        let bare = bare.trim().to_string();
        if !bare.is_empty() {
            let legacy_canonical = canonical_identity_path(project_key, &bare);
            if let Some(hit) = resolver.consider(legacy_canonical) {
                return Some(hit);
            }
        }
    }

    // 1c. If pane_id is a BARE tmux pane id (e.g. `%97`, no `:`), normalize it to
    //     its composite `session:window:pane` key via tmux and try the canonical
    //     composite path. Identity files are keyed by the composite, so a caller
    //     that supplies a bare pane id — an explicit `resolve_pane_identity`
    //     call, or a trusted `X-Tmux-Pane` header — would otherwise miss its own
    //     composite-keyed identity (GH#177 Defect 2).
    if !pane_id.contains(':')
        && let Some(composite) = composite_for_bare_pane(pane_id)
        && composite != pane_id
    {
        let composite_canonical = canonical_identity_path(project_key, &composite);
        if let Some(hit) = resolver.consider(composite_canonical) {
            return Some(hit);
        }
    }

    // 2. Legacy Claude Code path: ~/.claude/agent-mail/identity.$TMUX_PANE
    if let Some(home) = home_dir() {
        let sanitized = sanitize_pane_id(pane_id);
        let legacy_claude = home
            .join(".claude")
            .join("agent-mail")
            .join(format!("identity.{sanitized}"));
        if let Some(hit) = resolver.consider(legacy_claude) {
            return Some(hit);
        }

        // 2b. If composite key, also try bare pane ID for legacy Claude Code path
        if pane_id.contains(':')
            && let Some(bare) = tmux_pane_env()
        {
            let bare_sanitized = sanitize_pane_id(bare.trim());
            if bare_sanitized != sanitized {
                let legacy_claude_bare = home
                    .join(".claude")
                    .join("agent-mail")
                    .join(format!("identity.{bare_sanitized}"));
                if let Some(hit) = resolver.consider(legacy_claude_bare) {
                    return Some(hit);
                }
            }
        }
    }

    // 3. Legacy NTM path: /tmp/agent-mail-name.<project_hash>.<pane_id>
    let hash = project_hash(project_key);
    let sanitized = sanitize_pane_id(pane_id);
    let legacy_ntm = PathBuf::from(format!("/tmp/agent-mail-name.{hash}.{sanitized}"));
    if let Some(hit) = resolver.consider(legacy_ntm) {
        return Some(hit);
    }

    // 3b. If composite key, also try bare pane ID for legacy NTM path
    if pane_id.contains(':')
        && let Some(bare) = tmux_pane_env()
    {
        let bare_sanitized = sanitize_pane_id(bare.trim());
        if bare_sanitized != sanitized {
            let legacy_ntm_bare =
                PathBuf::from(format!("/tmp/agent-mail-name.{hash}.{bare_sanitized}"));
            if let Some(hit) = resolver.consider(legacy_ntm_bare) {
                return Some(hit);
            }
        }
    }

    None
}

/// Classify which identity-file convention produced a resolved path.
///
/// Callers that surface resolution results to automation (GH#240) should
/// report this category instead of the concrete filesystem path, so the
/// contract does not disclose identity-file locations or contents.
///
/// Categories:
/// - `canonical`: `~/.config/agent-mail/identity/<project_hash>/<pane_key>`
/// - `legacy-claude`: `~/.claude/agent-mail/identity.<pane_id>`
/// - `legacy-ntm`: `/tmp/agent-mail-name.<project_hash>.<pane_id>`
/// - `compatible`: any other path a fallback rule matched
///
/// GH#252 extends this surface with the binding status of the resolution
/// (`verified-live` / `adopted-dead` / `legacy-unverified`); callers obtain it
/// from [`resolve_identity_with_binding`] via [`PaneBindingStatus::as_str`]
/// and report it alongside this category (existing variants are unchanged).
#[must_use]
pub fn identity_source_category(path: &Path) -> &'static str {
    let canonical_root = config_base_dir().join(IDENTITY_DIR_NAME);
    if path.starts_with(&canonical_root) {
        return "canonical";
    }
    if let Some(home) = home_dir()
        && path.starts_with(home.join(".claude").join("agent-mail"))
    {
        return "legacy-claude";
    }
    let is_ntm_name = path
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.starts_with("agent-mail-name."));
    if is_ntm_name && path.starts_with("/tmp") {
        return "legacy-ntm";
    }
    "compatible"
}

/// Resolve the agent name for the current tmux pane.
///
/// Uses [`get_composite_tmux_pane_id`] to obtain a session-unique composite
/// key (e.g., `main:0:2`), falling back to bare `$TMUX_PANE` if unavailable.
/// Returns `None` if no pane identifier can be determined.
#[must_use]
pub fn resolve_identity_current_pane(project_key: &str) -> Option<String> {
    let pane_id = get_composite_tmux_pane_id();
    resolve_identity_for_pane(project_key, pane_id.as_deref())
}

/// Resolve the agent name for an explicit pane when supplied, otherwise for
/// the current tmux pane.
#[must_use]
pub fn resolve_identity_with_optional_pane(
    project_key: &str,
    pane_id: Option<&str>,
) -> Option<String> {
    let trimmed = pane_id.map(str::trim).filter(|pane| !pane.is_empty());
    if let Some(pane) = trimmed {
        return resolve_identity_for_pane(project_key, Some(pane));
    }
    resolve_identity_current_pane(project_key)
}

/// Write identity for the current tmux pane.
///
/// Uses [`get_composite_tmux_pane_id`] to obtain a session-unique composite
/// key (e.g., `main:0:2`), falling back to bare `$TMUX_PANE` if unavailable.
/// Returns `None` if no pane identifier can be determined.
#[must_use]
pub fn write_identity_current_pane(
    project_key: &str,
    agent_name: &str,
) -> Option<std::io::Result<PathBuf>> {
    let pane_id = get_composite_tmux_pane_id();
    write_identity_for_pane(project_key, pane_id.as_deref(), agent_name)
}

/// Write identity for an explicit pane when supplied, otherwise for the
/// current tmux pane.
#[must_use]
pub fn write_identity_with_optional_pane(
    project_key: &str,
    pane_id: Option<&str>,
    agent_name: &str,
) -> Option<std::io::Result<PathBuf>> {
    let trimmed = pane_id.map(str::trim).filter(|pane| !pane.is_empty());
    if let Some(pane) = trimmed {
        return write_identity_for_pane(project_key, Some(pane), agent_name);
    }
    write_identity_current_pane(project_key, agent_name)
}

/// Remove stale identity files for panes that no longer exist.
///
/// Structured records (GH#252) are judged by the liveness predicate against
/// their recorded socket: a record that passes is never removed; a record
/// whose binding is dead is purged, including one whose socket no longer
/// exists — provided tmux reports at least one live pane on this host (with
/// no local panes at all, socket-gone records are retained; see
/// `identity_entry_is_stale`). Legacy/unverifiable files, and structured
/// records that cannot be checked because tmux is not executable here, keep
/// the historical behavior: they are matched against tmux's live
/// composite/bare pane keys, and left untouched when tmux is not running
/// (to avoid accidentally removing everything).
///
/// Returns the list of removed file paths.
#[must_use]
pub fn cleanup_stale_identities(project_key: &str) -> Vec<PathBuf> {
    let mut removed = Vec::new();
    let base = config_base_dir();
    let hash = project_hash(project_key);
    let project_dir = base.join(IDENTITY_DIR_NAME).join(&hash);

    if !path_is_real_directory(&project_dir) {
        return removed;
    }

    let live_panes = list_live_tmux_panes();

    let Ok(entries) = std::fs::read_dir(&project_dir) else {
        return removed;
    };

    for entry in entries.flatten() {
        if identity_entry_is_stale(&entry, &live_panes) {
            let path = entry.path();
            if dir_entry_is_real_file(&entry) && std::fs::remove_file(&path).is_ok() {
                removed.push(path);
            }
        }
    }

    removed
}

/// Clean up stale identities across all project hash directories.
///
/// Iterates over every `<project_hash>/` directory under the identity root
/// and prunes files for dead panes using the same per-record rules as
/// [`cleanup_stale_identities`]. Returns all removed file paths.
#[must_use]
pub fn cleanup_all_stale_identities() -> Vec<PathBuf> {
    let mut removed = Vec::new();
    let base = config_base_dir();
    let identity_root = base.join(IDENTITY_DIR_NAME);

    if !path_is_real_directory(&identity_root) {
        return removed;
    }

    let live_panes = list_live_tmux_panes();

    let Ok(entries) = std::fs::read_dir(&identity_root) else {
        return removed;
    };

    for dir_entry in entries.flatten() {
        let project_dir = dir_entry.path();
        if !dir_entry_is_real_directory(&dir_entry) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&project_dir) else {
            continue;
        };
        for file_entry in files.flatten() {
            if identity_entry_is_stale(&file_entry, &live_panes) {
                let path = file_entry.path();
                if dir_entry_is_real_file(&file_entry) && std::fs::remove_file(&path).is_ok() {
                    removed.push(path);
                }
            }
        }
    }

    removed
}

/// List all identity entries for a project (for diagnostic/debug use).
///
/// Returns `(pane_id, agent_name)` pairs from the canonical directory.
#[must_use]
pub fn list_identities(project_key: &str) -> Vec<(String, String)> {
    list_identities_with_paths(project_key)
        .into_iter()
        .map(|(pane_id, name, _path)| (pane_id, name))
        .collect()
}

/// List all identity entries for a project, including the concrete file path
/// that backs each entry.
///
/// Returns `(pane_id, agent_name, path)` tuples enumerated from the LIVE
/// canonical pane-identity directory
/// (`~/.config/agent-mail/identity/<project_hash>/`). Diagnostics that surface
/// these to operators should include `path` so a phantom/orphaned warning can be
/// traced to a real file on disk (see #243 Bug 1).
#[must_use]
pub fn list_identities_with_paths(project_key: &str) -> Vec<(String, String, PathBuf)> {
    let base = config_base_dir();
    let hash = project_hash(project_key);
    let project_dir = base.join(IDENTITY_DIR_NAME).join(hash);

    if !path_is_real_directory(&project_dir) {
        return Vec::new();
    }

    let mut result = Vec::new();
    let Ok(entries) = std::fs::read_dir(&project_dir) else {
        return result;
    };

    for entry in entries.flatten() {
        let pane_id = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        if let Some(name) = read_identity_file(&path) {
            result.push((pane_id, name, path));
        }
    }

    result.sort_by(|a, b| a.0.cmp(&b.0));
    result
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn path_is_real_directory(path: &Path) -> bool {
    if path_has_symlinked_parent(path).unwrap_or(true) {
        return false;
    }
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
}

fn dir_entry_is_real_directory(entry: &std::fs::DirEntry) -> bool {
    entry.file_type().is_ok_and(|file_type| file_type.is_dir())
}

fn dir_entry_is_real_file(entry: &std::fs::DirEntry) -> bool {
    entry.file_type().is_ok_and(|file_type| file_type.is_file())
}

fn ensure_real_directory(path: &Path) -> std::io::Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            std::path::Component::RootDir => current.push(component.as_os_str()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("refusing parent traversal in {}", path.display()),
                ));
            }
            std::path::Component::Normal(segment) => {
                current.push(segment);
                match std::fs::symlink_metadata(&current) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::AlreadyExists,
                            format!(
                                "refusing symlinked pane identity directory {}",
                                current.display()
                            ),
                        ));
                    }
                    Ok(metadata) if metadata.file_type().is_dir() => {}
                    Ok(_) => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::AlreadyExists,
                            format!("{} is not a directory", current.display()),
                        ));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        std::fs::create_dir(&current)?;
                    }
                    Err(error) => return Err(error),
                }
            }
        }
    }
    Ok(())
}

fn path_has_symlinked_parent(path: &Path) -> std::io::Result<bool> {
    let Some(parent) = path.parent() else {
        return Ok(false);
    };
    let mut current = PathBuf::new();
    for component in parent.components() {
        match component {
            std::path::Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            std::path::Component::RootDir | std::path::Component::ParentDir => {
                current.push(component.as_os_str());
            }
            std::path::Component::CurDir => {}
            std::path::Component::Normal(segment) => {
                current.push(segment);
                match std::fs::symlink_metadata(&current) {
                    Ok(metadata) if metadata.file_type().is_symlink() => return Ok(true),
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                    Err(error) => return Err(error),
                }
            }
        }
    }
    Ok(false)
}

fn file_name_matches_live_pane(file_name: &OsStr, live_panes: &[String]) -> bool {
    let name = file_name.to_string_lossy();
    live_panes.iter().any(|pane| pane.as_str() == name.as_ref())
}

/// Compute a truncated SHA-1 hex hash of the project key.
fn project_hash(project_key: &str) -> String {
    let normalized_key = if Path::new(project_key).is_absolute() {
        crate::identity::resolve_project_path(project_key)
    } else {
        PathBuf::from(project_key)
    };
    let mut hasher = Sha1::new();
    hasher.update(normalized_key.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let hex = crate::identity::bytes_to_lower_hex(digest);
    hex.chars().take(PROJECT_HASH_LEN).collect()
}

/// Sanitize a tmux pane ID for use as a filename.
///
/// Strips the leading `%` character and replaces any filesystem-unsafe
/// characters with hyphens (for `:` in composite keys like
/// `session:window:pane`) or underscores (for other unsafe chars).
///
/// The `%` prefix is conventional in tmux (e.g., `%0`, `%3`) but not
/// great for filenames. Composite keys use `:` as separator which becomes
/// `-` so that `mysession:0:2` becomes `mysession-0-2`.
fn sanitize_pane_id(pane_id: &str) -> String {
    let stripped = pane_id.strip_prefix('%').unwrap_or(pane_id);
    let mut out = String::with_capacity(stripped.len());
    for ch in stripped.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else if ch == ':' {
            out.push('-');
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "unknown".to_string()
    } else {
        out
    }
}

/// Read the agent name from an identity file (structured record or legacy
/// bare-name). Returns `None` if the file doesn't exist, is empty, or holds
/// a malformed record.
fn read_identity_file(path: &Path) -> Option<String> {
    read_identity_record(path).map(|record| record.name)
}

/// Parse identity-file content: a JSON [`PaneIdentityRecord`], or a legacy
/// bare-name line which becomes a record with only `name` set.
fn parse_identity_record(content: &str) -> Option<PaneIdentityRecord> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('{') {
        let record = serde_json::from_str::<PaneIdentityRecord>(trimmed).ok()?;
        let name = record.name.trim();
        if name.is_empty() {
            return None;
        }
        if name == record.name {
            return Some(record);
        }
        return Some(PaneIdentityRecord {
            name: name.to_string(),
            ..record
        });
    }
    Some(PaneIdentityRecord::bare(trimmed))
}

/// Serialize a record and write it through the symlink-hardened writer.
fn write_record_content_no_follow(path: &Path, record: &PaneIdentityRecord) -> std::io::Result<()> {
    let json = serde_json::to_string(record)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    write_identity_file_no_follow(path, format!("{json}\n").as_bytes())
}

/// Binding facts of the pane a caller is writing or resolving, gathered from
/// the tmux server reachable in the caller's environment.
#[derive(Debug, Clone)]
struct TargetPaneFacts {
    session_name: String,
    pane_id: String,
    pane_pid: u32,
    current_command: String,
    socket_path: String,
}

/// Convert an identity pane key into a tmux target specifier.
///
/// Bare pane ids (`%3`) are already valid targets. Composite keys
/// (`session:window:pane`) become tmux's `session:window.pane` target form.
fn pane_target_for(pane_id: &str) -> Option<String> {
    let pane = pane_id.trim();
    if pane.is_empty() {
        return None;
    }
    if !pane.contains(':') {
        return Some(pane.to_string());
    }
    let mut parts = pane.rsplitn(3, ':');
    let pane_index = parts.next()?;
    let window_index = parts.next()?;
    parts.next().map_or_else(
        || Some(pane.to_string()),
        |session| Some(format!("{session}:{window_index}.{pane_index}")),
    )
}

/// Query tmux (in the caller's environment) for the binding facts of the pane
/// named by `pane_id`. Returns `None` when tmux is unavailable or the pane
/// does not exist — the caller then behaves as it did before GH#252.
fn query_target_pane_facts(pane_id: &str) -> Option<TargetPaneFacts> {
    let target = pane_target_for(pane_id)?;
    let output = tmux_command()
        .args(["display-message", "-t", &target, "-p", TARGET_FACTS_FORMAT])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_target_facts_line(stdout.lines().next()?)
}

/// Parse one `TARGET_FACTS_FORMAT` line into [`TargetPaneFacts`].
fn parse_target_facts_line(line: &str) -> Option<TargetPaneFacts> {
    let mut fields = line.split('\t');
    let session_name = fields.next()?.trim().to_string();
    let pane = fields.next()?.trim().to_string();
    let root_pid = fields.next()?.trim().parse::<u32>().ok()?;
    let current_command = fields.next()?.trim().to_string();
    let socket_path = fields.next()?.trim().to_string();
    if session_name.is_empty() || pane.is_empty() {
        return None;
    }
    let socket_path = if socket_path.is_empty() {
        tmux_env_socket_path().unwrap_or_default()
    } else {
        socket_path
    };
    Some(TargetPaneFacts {
        session_name,
        pane_id: pane,
        pane_pid: root_pid,
        current_command,
        socket_path,
    })
}

/// Socket path from `$TMUX` (`socket_path,server_pid,session_index`).
fn tmux_env_socket_path() -> Option<String> {
    crate::config::process_env_value("TMUX").and_then(|value| {
        let first = value.split(',').next()?.trim().to_string();
        if first.is_empty() { None } else { Some(first) }
    })
}

/// Build a structured record for `name` from optional target-pane facts.
fn record_from_facts(name: &str, facts: Option<&TargetPaneFacts>) -> PaneIdentityRecord {
    facts.map_or_else(
        || PaneIdentityRecord::bare(name),
        |f| PaneIdentityRecord {
            name: name.trim().to_string(),
            session_name: Some(f.session_name.clone()),
            pane_id: Some(f.pane_id.clone()),
            pane_pid: Some(f.pane_pid),
            socket_path: if f.socket_path.is_empty() {
                None
            } else {
                Some(f.socket_path.clone())
            },
            written_at: Some(chrono::Utc::now().to_rfc3339()),
        },
    )
}

/// Best-effort adoption rewrite: bind `name` to the adopter's pane facts at
/// the identity file that produced the candidate (upgrading legacy files to
/// structured records in place). IO failures are ignored — adoption must not
/// break resolution.
fn adopt_record_at(path: &Path, name: &str, facts: &TargetPaneFacts) {
    if path_has_symlinked_parent(path).unwrap_or(true) {
        return;
    }
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return;
    }
    let record = record_from_facts(name, Some(facts));
    let _ = write_record_content_no_follow(path, &record);
}

/// Per-resolution classifier implementing the GH#252 adoption rule.
///
/// Lazily gathers the target pane's facts once (the pane named by the
/// caller's `pane_id` argument — the reuse-seed slot every candidate key in
/// the lookup order describes) and classifies each candidate record found.
struct PaneBindingResolver {
    pane_arg: String,
    facts_queried: bool,
    facts: Option<TargetPaneFacts>,
}

impl PaneBindingResolver {
    fn new(pane_id: &str) -> Self {
        Self {
            pane_arg: pane_id.to_string(),
            facts_queried: false,
            facts: None,
        }
    }

    fn target_facts(&mut self) -> Option<&TargetPaneFacts> {
        if !self.facts_queried {
            self.facts = query_target_pane_facts(&self.pane_arg);
            self.facts_queried = true;
        }
        self.facts.as_ref()
    }

    /// Classify the candidate at `path`. Returns `Some` when the candidate is
    /// returnable (verified-live for this pane, adopted-dead, or legacy
    /// compatibility), `None` when there is no usable record — or when the
    /// record is a LIVE binding held by a different pane, in which case the
    /// lookup continues and ultimately mints a fresh identity.
    fn consider(&mut self, path: PathBuf) -> Option<(String, PathBuf, PaneBindingStatus)> {
        let record = read_identity_record(&path)?;
        match binding_liveness(&record) {
            PaneBindingLiveness::Live => {
                let holder_matches = self.target_facts().is_some_and(|f| {
                    record.pane_id.as_deref() == Some(f.pane_id.as_str())
                        && record.socket_path.as_deref() == Some(f.socket_path.as_str())
                });
                if holder_matches {
                    return Some((record.name, path, PaneBindingStatus::VerifiedLive));
                }
                // Live holder elsewhere: never adopt, never return.
                None
            }
            PaneBindingLiveness::Dead => {
                if let Some(facts) = self.target_facts().cloned() {
                    adopt_record_at(&path, &record.name, &facts);
                }
                Some((record.name, path, PaneBindingStatus::AdoptedDead))
            }
            PaneBindingLiveness::Unverifiable => {
                // Legacy bare-name record, a record written outside tmux, or
                // a structured record this process cannot check because tmux
                // is not executable here: verify what is checkable. If the
                // pane named by the file's key exists and runs an agent, treat
                // as live (conservative — blocks theft, and the resolver at
                // this key is that pane); if it idles in a shell, adopt,
                // upgrading the file to a structured record; with no tmux
                // context at all, return the name untouched (pre-GH#252
                // behavior).
                match self.target_facts().cloned() {
                    Some(facts) if is_agent_pane_command(&facts.current_command) => {
                        Some((record.name, path, PaneBindingStatus::LegacyUnverified))
                    }
                    Some(facts) => {
                        adopt_record_at(&path, &record.name, &facts);
                        Some((record.name, path, PaneBindingStatus::AdoptedDead))
                    }
                    None => Some((record.name, path, PaneBindingStatus::LegacyUnverified)),
                }
            }
        }
    }
}

/// Decide whether a cleanup candidate file is stale (GH#252).
///
/// Structured records use the liveness predicate: a live binding is never
/// removed; a dead one — including a record whose socket is gone — is stale.
/// Legacy files, and structured records this process cannot check (tmux not
/// executable), keep the historical rule: stale when their file name matches
/// no live tmux pane key, and never stale while tmux reports no panes at all
/// (so a stopped or unreachable tmux cannot wipe identities).
///
/// Conservative retention: a record whose socket is gone is purged only when
/// tmux reports at least one live pane on this host. With no local panes at
/// all we cannot tell "that server was killed" from "these records were
/// written on another host sharing this config directory" (cross-host panes
/// are out of scope for GH#252 but must not be destroyed); the records are
/// harmless to keep and become adoptable/purgeable once a local server runs.
fn identity_entry_is_stale(entry: &std::fs::DirEntry, live_panes: &[String]) -> bool {
    let path = entry.path();
    let record = read_identity_record(&path);
    match record.as_ref().map(binding_liveness) {
        Some(PaneBindingLiveness::Live) => false,
        Some(PaneBindingLiveness::Dead) => {
            let socket_gone = record
                .as_ref()
                .and_then(|r| r.socket_path.as_deref())
                .is_some_and(|socket| !Path::new(socket).exists());
            // Stale unless this is a socket-gone record on a host with no
            // local panes (retained; see the doc comment above).
            !socket_gone || !live_panes.is_empty()
        }
        Some(PaneBindingLiveness::Unverifiable) | None => {
            if live_panes.is_empty() {
                return false;
            }
            !file_name_matches_live_pane(&entry.file_name(), live_panes)
        }
    }
}

/// Run tmux with `args`.
///
/// `Err` means tmux could not be executed at all (binary missing, not
/// executable, ...); `Ok(None)` means tmux ran but exited non-zero;
/// `Ok(Some(stdout))` is a successful invocation.
fn run_tmux_capture(args: &[&str]) -> std::io::Result<Option<String>> {
    let output = tmux_command().args(args).output()?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()))
}

#[cfg(unix)]
fn read_identity_file_no_follow(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)?;
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    Ok(content)
}

#[cfg(not(unix))]
fn read_identity_file_no_follow(path: &Path) -> std::io::Result<String> {
    std::fs::read_to_string(path)
}

#[cfg(unix)]
fn write_identity_file_no_follow(path: &Path, content: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)?;
    file.write_all(content)
}

#[cfg(not(unix))]
fn write_identity_file_no_follow(path: &Path, content: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, content)
}

#[must_use]
fn resolve_identity_for_pane(project_key: &str, pane_id: Option<&str>) -> Option<String> {
    let pane_id = pane_id?.trim();
    if pane_id.is_empty() {
        return None;
    }
    resolve_identity(project_key, pane_id)
}

fn write_identity_for_pane(
    project_key: &str,
    pane_id: Option<&str>,
    agent_name: &str,
) -> Option<std::io::Result<PathBuf>> {
    let pane_id = pane_id?.trim();
    if pane_id.is_empty() {
        return None;
    }
    Some(write_identity(project_key, pane_id, agent_name))
}

/// Get the XDG-compatible config base directory (`~/.config`).
fn config_base_dir() -> PathBuf {
    #[cfg(test)]
    if let Some(path) = test_config_base_dir() {
        return path;
    }

    if let Some(path) = env_path("XDG_CONFIG_HOME") {
        return path;
    }

    home_dir()
        .map(|home| home.join(".config"))
        .or_else(dirs::config_dir)
        .unwrap_or_else(|| PathBuf::from("/tmp").join(".config"))
}

fn home_dir() -> Option<PathBuf> {
    env_path("HOME").or_else(dirs::home_dir)
}

fn env_path(key: &str) -> Option<PathBuf> {
    crate::config::process_env_value(key).and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(PathBuf::from(shellexpand::tilde(trimmed).into_owned()))
        }
    })
}

fn tmux_pane_env() -> Option<String> {
    crate::config::process_env_value("TMUX_PANE").and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

#[cfg(test)]
fn tmux_command() -> std::process::Command {
    // Unit tests are hermetic: they must never consult a real tmux server.
    // Tests that want tmux behavior install a stub via AM_TEST_TMUX_BIN;
    // everything else gets a command that cannot execute, so shell-outs fail
    // deterministically regardless of the developer's tmux environment.
    crate::config::process_env_value("AM_TEST_TMUX_BIN")
        .filter(|value| !value.trim().is_empty())
        .map_or_else(
            || std::process::Command::new("/nonexistent/am-test-tmux-disabled"),
            std::process::Command::new,
        )
}

#[cfg(not(test))]
fn tmux_command() -> std::process::Command {
    std::process::Command::new("tmux")
}

#[cfg(test)]
fn test_config_base_dir() -> Option<PathBuf> {
    TEST_CONFIG_BASE_DIR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

#[cfg(test)]
fn set_test_config_base_dir(path: Option<PathBuf>) {
    *TEST_CONFIG_BASE_DIR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = path;
}

#[cfg(test)]
fn test_live_tmux_panes() -> Option<Vec<String>> {
    TEST_LIVE_TMUX_PANES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

#[cfg(test)]
fn set_test_live_tmux_panes(panes: Option<Vec<String>>) {
    *TEST_LIVE_TMUX_PANES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = panes;
}

/// Query tmux for all live pane IDs (sanitized).
///
/// Returns composite keys (`session_name:window_index:pane_index`) for each
/// live pane, plus the legacy bare pane ID (e.g., `%3` -> `3`) for backwards
/// compatibility during cleanup. Returns an empty vec if tmux is not running
/// or the command fails.
fn list_live_tmux_panes() -> Vec<String> {
    #[cfg(test)]
    if let Some(panes) = test_live_tmux_panes() {
        return panes;
    }

    let output = tmux_command()
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{session_name}:#{window_index}:#{pane_index}:#{pane_id}",
        ])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            let mut ids = Vec::new();
            for line in text.lines().filter(|l| !l.is_empty()) {
                let line = line.trim();
                // Parse "session:window:pane_idx:pane_id" format.
                // The composite key is the first three fields joined by `:`.
                // We also include the bare pane_id for backwards compat.
                if let Some((composite, bare_id)) = line.rsplit_once(':') {
                    ids.push(sanitize_pane_id(composite));
                    ids.push(sanitize_pane_id(bare_id));
                } else {
                    // Fallback: treat the entire line as a bare pane ID
                    ids.push(sanitize_pane_id(line));
                }
            }
            ids.sort();
            ids.dedup();
            ids
        }
        _ => Vec::new(),
    }
}

/// Get a composite tmux pane identifier for the **caller's own** pane.
///
/// Runs `tmux display-message -t $TMUX_PANE -p
/// '#{session_name}:#{window_index}:#{pane_index}'` to produce a key like
/// `main:0:2` that is unique across tmux sessions, falling back to the bare
/// `$TMUX_PANE` value if `display-message` is unavailable.
///
/// **Fails closed when `$TMUX_PANE` is unset/empty** (GH#177). A process with no
/// caller pane env — most importantly the `serve-http` daemon, which does not
/// run in the caller's pane — must NOT run a `-t`-less `display-message`: tmux
/// resolves that to the *currently-active* pane, so `macro_start_session` /
/// `resolve_pane_identity` would bind the caller to whatever identity happens to
/// occupy the active pane, sending mail under another live agent's name with
/// `verified_sender=false`. Returning `None` instead lets the caller mint a
/// fresh identity rather than hijack the active pane's.
///
/// Returns `None` when `$TMUX_PANE` cannot be determined.
#[must_use]
pub fn get_composite_tmux_pane_id() -> Option<String> {
    // Fail closed: only resolve the caller's *own* pane. With no caller pane env
    // we return None rather than letting a `-t`-less display-message stand in
    // with the active pane (GH#177 Defect 1).
    let pane_target = tmux_pane_env()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;

    let output = tmux_command()
        .args([
            "display-message",
            "-t",
            &pane_target,
            "-p",
            "#{session_name}:#{window_index}:#{pane_index}",
        ])
        .output();

    if let Ok(out) = output
        && out.status.success()
    {
        let composite = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !composite.is_empty() && composite.contains(':') {
            return Some(composite);
        }
    }

    // Fallback to the bare caller pane id when display-message didn't yield a
    // composite (e.g. tmux unavailable) — still the *caller's* pane, never the
    // active one.
    Some(pane_target)
}

/// Resolve a bare tmux pane id (e.g. `%97`) to its composite
/// `session:window:pane` key via `tmux display-message -t <pane>`.
///
/// Unlike [`get_composite_tmux_pane_id`], this targets an *explicitly supplied*
/// pane rather than the caller's own `$TMUX_PANE`, so it is safe to call from
/// the daemon for a caller-provided pane (GH#177 Defect 2). Returns `None` when
/// tmux is unavailable, the pane is unknown, or the answer isn't a composite key.
#[must_use]
fn composite_for_bare_pane(pane_id: &str) -> Option<String> {
    let pane = pane_id.trim();
    if pane.is_empty() {
        return None;
    }
    let output = tmux_command()
        .args([
            "display-message",
            "-t",
            pane,
            "-p",
            "#{session_name}:#{window_index}:#{pane_index}",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let composite = String::from_utf8_lossy(&output.stdout).trim().to_string();
    composite.contains(':').then_some(composite)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{LazyLock, Mutex, MutexGuard};

    static TEST_CONFIG_BASE_DIR_SERIAL: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    struct IsolatedConfigBaseDir {
        _guard: MutexGuard<'static, ()>,
        tempdir: tempfile::TempDir,
    }

    impl IsolatedConfigBaseDir {
        fn new() -> Self {
            let guard = TEST_CONFIG_BASE_DIR_SERIAL
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let tempdir = tempfile::tempdir().expect("temp config dir");
            set_test_config_base_dir(Some(tempdir.path().to_path_buf()));
            Self {
                _guard: guard,
                tempdir,
            }
        }

        fn project_key(&self, suffix: &str) -> String {
            self.tempdir
                .path()
                .join(suffix)
                .to_string_lossy()
                .into_owned()
        }
    }

    impl Drop for IsolatedConfigBaseDir {
        fn drop(&mut self) {
            set_test_config_base_dir(None);
        }
    }

    struct LiveTmuxPanesGuard;

    impl LiveTmuxPanesGuard {
        fn new(panes: Vec<String>) -> Self {
            set_test_live_tmux_panes(Some(panes));
            Self
        }
    }

    impl Drop for LiveTmuxPanesGuard {
        fn drop(&mut self) {
            set_test_live_tmux_panes(None);
        }
    }

    // -- identity_source_category -------------------------------------------

    #[test]
    fn identity_source_category_classifies_canonical_path() {
        let isolated = IsolatedConfigBaseDir::new();
        let path = canonical_identity_path(&isolated.project_key("proj"), "main:0:2");
        // The guard must stay alive through the classification: both calls
        // above and below read the isolated config base dir it installs.
        assert_eq!(identity_source_category(&path), "canonical");
        drop(isolated);
    }

    #[test]
    fn identity_source_category_classifies_legacy_paths() {
        let _isolated = IsolatedConfigBaseDir::new();
        if let Some(home) = home_dir() {
            let claude = home.join(".claude").join("agent-mail").join("identity.%3");
            assert_eq!(identity_source_category(&claude), "legacy-claude");
        }
        let ntm = PathBuf::from("/tmp/agent-mail-name.abc123def456.%3");
        assert_eq!(identity_source_category(&ntm), "legacy-ntm");
        let other = PathBuf::from("/var/lib/agent-mail/identity/xyz");
        assert_eq!(identity_source_category(&other), "compatible");
    }

    // -- project_hash --------------------------------------------------------

    #[test]
    fn project_hash_produces_expected_length() {
        let h = project_hash("/data/projects/backend");
        assert_eq!(h.len(), PROJECT_HASH_LEN);
    }

    #[test]
    fn project_hash_deterministic() {
        let a = project_hash("/data/projects/backend");
        let b = project_hash("/data/projects/backend");
        assert_eq!(a, b);
    }

    #[test]
    fn project_hash_differs_for_different_projects() {
        let a = project_hash("/data/projects/alpha");
        let b = project_hash("/data/projects/beta");
        assert_ne!(a, b);
    }

    #[test]
    fn project_hash_converges_case_variants_on_case_insensitive_filesystem() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let stored = tmp.path().join("ProjectRepo");
        std::fs::create_dir_all(&stored).expect("create mixed-case project path");
        let variant = tmp.path().join("projectrepo");
        if !variant.exists() {
            return;
        }

        assert_eq!(
            project_hash(&stored.to_string_lossy()),
            project_hash(&variant.to_string_lossy())
        );
    }

    // -- sanitize_pane_id ----------------------------------------------------

    #[test]
    fn sanitize_strips_percent() {
        assert_eq!(sanitize_pane_id("%0"), "0");
        assert_eq!(sanitize_pane_id("%123"), "123");
    }

    #[test]
    fn sanitize_preserves_plain_id() {
        assert_eq!(sanitize_pane_id("42"), "42");
    }

    #[test]
    fn sanitize_replaces_unsafe_chars() {
        assert_eq!(sanitize_pane_id("%foo/bar"), "foo_bar");
    }

    #[test]
    fn sanitize_empty_returns_unknown() {
        assert_eq!(sanitize_pane_id(""), "unknown");
        assert_eq!(sanitize_pane_id("%"), "unknown");
    }

    #[test]
    fn sanitize_composite_key_uses_hyphens() {
        assert_eq!(sanitize_pane_id("main:0:2"), "main-0-2");
        assert_eq!(sanitize_pane_id("my_session:1:0"), "my_session-1-0");
    }

    // -- canonical_identity_path ---------------------------------------------

    #[test]
    fn canonical_path_has_expected_structure() {
        let path = canonical_identity_path("/data/projects/backend", "%3");
        let path_str = path.to_string_lossy();
        assert!(
            path_str.contains("agent-mail/identity/"),
            "missing identity dir: {path_str}"
        );
        assert!(
            path_str.ends_with("/3"),
            "expected pane id suffix: {path_str}"
        );
    }

    #[test]
    fn canonical_path_project_scoped() {
        let a = canonical_identity_path("/data/projects/alpha", "%0");
        let b = canonical_identity_path("/data/projects/beta", "%0");
        assert_ne!(a, b, "different projects should produce different paths");
    }

    #[test]
    fn canonical_path_composite_key_differs_from_bare() {
        let bare = canonical_identity_path("/data/projects/backend", "%3");
        let composite = canonical_identity_path("/data/projects/backend", "main:0:2");
        assert_ne!(
            bare, composite,
            "composite key should produce a different path than bare pane ID"
        );
        let composite_str = composite.to_string_lossy();
        assert!(
            composite_str.ends_with("/main-0-2"),
            "expected composite key filename: {composite_str}"
        );
    }

    #[test]
    fn canonical_path_different_sessions_differ() {
        let a = canonical_identity_path("/data/projects/backend", "session_a:0:2");
        let b = canonical_identity_path("/data/projects/backend", "session_b:0:2");
        assert_ne!(
            a, b,
            "different sessions with the same window/pane index should produce different paths"
        );
    }

    #[test]
    fn canonical_path_honors_virtual_xdg_config_home() {
        let _guard = TEST_CONFIG_BASE_DIR_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set_test_config_base_dir(None);

        let tmp = tempfile::tempdir().expect("temp config home");
        let xdg_config_home = tmp.path().join("xdg-config");
        let xdg_config_home_text = xdg_config_home.to_string_lossy().into_owned();

        crate::config::with_process_env_overrides_for_test(
            &[("XDG_CONFIG_HOME", xdg_config_home_text.as_str())],
            || {
                let path = canonical_identity_path("/data/projects/backend", "%3");
                assert!(
                    path.starts_with(&xdg_config_home),
                    "canonical pane identity path ignored virtual XDG_CONFIG_HOME: {path:?}"
                );
            },
        );
    }

    #[test]
    fn canonical_path_honors_virtual_home_fallback() {
        let _guard = TEST_CONFIG_BASE_DIR_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set_test_config_base_dir(None);

        let tmp = tempfile::tempdir().expect("temp home");
        let home = tmp.path().join("home");
        let home_text = home.to_string_lossy().into_owned();
        let expected_config_home = home.join(".config");

        crate::config::with_process_env_overrides_for_test(
            &[("XDG_CONFIG_HOME", ""), ("HOME", home_text.as_str())],
            || {
                let path = canonical_identity_path("/data/projects/backend", "%3");
                assert!(
                    path.starts_with(&expected_config_home),
                    "canonical pane identity path ignored virtual HOME fallback: {path:?}"
                );
            },
        );
    }

    #[test]
    fn legacy_claude_identity_honors_virtual_home() {
        let _guard = TEST_CONFIG_BASE_DIR_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set_test_config_base_dir(None);

        let tmp = tempfile::tempdir().expect("temp home");
        let home = tmp.path().join("home");
        let home_text = home.to_string_lossy().into_owned();
        let identity_dir = home.join(".claude").join("agent-mail");
        std::fs::create_dir_all(&identity_dir).expect("create legacy identity dir");
        let identity_path = identity_dir.join("identity.18");
        std::fs::write(&identity_path, "BlueLake\n").expect("write legacy identity");

        crate::config::with_process_env_overrides_for_test(
            &[("XDG_CONFIG_HOME", ""), ("HOME", home_text.as_str())],
            || {
                let resolved =
                    resolve_identity_with_path("/data/projects/backend", "%18").expect("resolve");
                assert_eq!(resolved.0, "BlueLake");
                assert_eq!(resolved.1, identity_path);
            },
        );
    }

    // -- write / resolve roundtrip -------------------------------------------

    #[test]
    fn write_then_resolve_roundtrip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Override config dir by writing directly to a temp path
        let identity_dir = tmp.path().join("agent-mail/identity");
        let hash = project_hash("/data/test-project");
        let pane_dir = identity_dir.join(&hash);
        std::fs::create_dir_all(&pane_dir).expect("create dirs");
        let file_path = pane_dir.join("5");
        std::fs::write(&file_path, "BlueLake\n").expect("write");

        let name = read_identity_file(&file_path);
        assert_eq!(name.as_deref(), Some("BlueLake"));
    }

    #[test]
    fn read_identity_file_missing_returns_none() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("nonexistent");
        assert!(read_identity_file(&path).is_none());
    }

    #[test]
    fn read_identity_file_empty_returns_none() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("empty");
        std::fs::write(&path, "  \n  ").expect("write");
        assert!(read_identity_file(&path).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn read_identity_file_ignores_symlink_leaf() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().expect("tempdir");
        let target = tmp.path().join("target");
        let link = tmp.path().join("identity-link");
        std::fs::write(&target, "BlueLake\n").expect("write target");
        symlink(&target, &link).expect("symlink identity");

        assert!(
            read_identity_file(&link).is_none(),
            "pane identity reads must not follow symlink leaves"
        );
    }

    // -- list_identities (with isolated config dir) --------------------------

    #[test]
    fn write_then_resolve_roundtrip_composite_key() {
        let config = IsolatedConfigBaseDir::new();
        let unique_key = config.project_key("composite-project");
        let composite_pane = "test_session:0:1";
        write_identity(&unique_key, composite_pane, "GreenOwl").expect("write identity");

        let resolved = resolve_identity(&unique_key, composite_pane);
        assert_eq!(resolved.as_deref(), Some("GreenOwl"));
        drop(config);
    }

    #[cfg(unix)]
    #[test]
    fn write_identity_refuses_symlink_leaf() {
        use std::os::unix::fs::symlink;

        let config = IsolatedConfigBaseDir::new();
        let unique_key = config.project_key("symlink-write-project");
        let pane = "%17";
        let identity_path = canonical_identity_path(&unique_key, pane);
        let parent = identity_path.parent().expect("identity parent");
        std::fs::create_dir_all(parent).expect("create identity dir");
        let target = config.tempdir.path().join("outside-identity-target");
        std::fs::write(&target, "OriginalAgent\n").expect("write target");
        symlink(&target, &identity_path).expect("symlink identity leaf");

        let err = write_identity(&unique_key, pane, "BlueLake").expect_err("symlink refused");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read_to_string(&target).expect("read target"),
            "OriginalAgent\n"
        );
        drop(config);
    }

    #[cfg(unix)]
    #[test]
    fn write_identity_refuses_symlinked_project_directory() {
        use std::os::unix::fs::symlink;

        let config = IsolatedConfigBaseDir::new();
        let unique_key = config.project_key("symlink-parent-write-project");
        let identity_root = config.tempdir.path().join(IDENTITY_DIR_NAME);
        let project_dir = identity_root.join(project_hash(&unique_key));
        let outside_dir = config.tempdir.path().join("outside-identity-dir");

        std::fs::create_dir_all(&identity_root).expect("create identity root");
        std::fs::create_dir_all(&outside_dir).expect("create outside dir");
        symlink(&outside_dir, &project_dir).expect("symlink project identity dir");

        let err = write_identity(&unique_key, "%17", "BlueLake")
            .expect_err("symlinked project directory refused");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(
            !outside_dir.join("17").exists(),
            "write_identity must not write through a symlinked project directory"
        );
        drop(config);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_identity_ignores_symlinked_project_directory() {
        use std::os::unix::fs::symlink;

        let config = IsolatedConfigBaseDir::new();
        let unique_key = config.project_key("symlink-parent-read-project");
        let identity_root = config.tempdir.path().join(IDENTITY_DIR_NAME);
        let project_dir = identity_root.join(project_hash(&unique_key));
        let outside_dir = config.tempdir.path().join("outside-identity-dir");

        std::fs::create_dir_all(&identity_root).expect("create identity root");
        std::fs::create_dir_all(&outside_dir).expect("create outside dir");
        std::fs::write(outside_dir.join("17"), "BlueLake\n").expect("write outside identity");
        symlink(&outside_dir, &project_dir).expect("symlink project identity dir");

        assert!(
            resolve_identity(&unique_key, "%17").is_none(),
            "resolve_identity must not read through a symlinked project directory"
        );
        drop(config);
    }

    #[test]
    fn composite_resolution_honors_virtual_bare_tmux_pane_fallback() {
        let config = IsolatedConfigBaseDir::new();
        let unique_key = config.project_key("bare-fallback-project");
        let bare_pane = "%23";
        let written_path =
            write_identity(&unique_key, bare_pane, "BlueLake").expect("write bare pane identity");

        crate::config::with_process_env_overrides_for_test(&[("TMUX_PANE", bare_pane)], || {
            let resolved =
                resolve_identity_with_path(&unique_key, "session:0:1").expect("resolve identity");
            assert_eq!(resolved.0, "BlueLake");
            assert_eq!(resolved.1, written_path);
        });

        drop(config);
    }

    #[test]
    fn list_identities_returns_entries() {
        let config = IsolatedConfigBaseDir::new();
        let unique_key = config.project_key("list-project");
        let pane = "%99";
        write_identity(&unique_key, pane, "RedFox").expect("write identity");

        let entries = list_identities(&unique_key);
        assert!(
            entries.iter().any(|(p, n)| p == "99" && n == "RedFox"),
            "expected RedFox entry: {entries:?}"
        );
        drop(config);
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_identities_skips_symlinked_project_directories() {
        use std::os::unix::fs::symlink;

        let config = IsolatedConfigBaseDir::new();
        let tmux = LiveTmuxPanesGuard::new(vec!["live-pane".to_string()]);
        let unique_key = config.project_key("symlink-cleanup-project");
        let identity_root = config.tempdir.path().join(IDENTITY_DIR_NAME);
        let project_dir = identity_root.join(project_hash(&unique_key));
        let outside_dir = config.tempdir.path().join("outside-identities");
        let outside_stale = outside_dir.join("stale-pane");

        std::fs::create_dir_all(&identity_root).expect("create identity root");
        std::fs::create_dir_all(&outside_dir).expect("create outside dir");
        std::fs::write(&outside_stale, "OtherAgent\n").expect("write outside identity");
        symlink(&outside_dir, &project_dir).expect("symlink project identity dir");

        let scoped_removed = cleanup_stale_identities(&unique_key);
        assert!(
            scoped_removed.is_empty(),
            "scoped cleanup must not walk a symlinked project dir: {scoped_removed:?}"
        );
        assert!(
            outside_stale.exists(),
            "scoped cleanup must not remove files behind symlinked project dirs"
        );

        let global_removed = cleanup_all_stale_identities();
        assert!(
            global_removed.is_empty(),
            "global cleanup must not walk symlinked project dirs: {global_removed:?}"
        );
        assert!(
            outside_stale.exists(),
            "global cleanup must not remove files behind symlinked project dirs"
        );
        drop(tmux);
        drop(config);
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_identities_skips_symlinked_identity_root_parent() {
        use std::os::unix::fs::symlink;

        let config = IsolatedConfigBaseDir::new();
        let tmux = LiveTmuxPanesGuard::new(vec!["live-pane".to_string()]);
        let unique_key = config.project_key("symlink-root-parent-project");
        let agent_mail_parent = config.tempdir.path().join("agent-mail");
        let outside_agent_mail = config.tempdir.path().join("outside-agent-mail");
        let outside_project_dir = outside_agent_mail
            .join("identity")
            .join(project_hash(&unique_key));
        let outside_stale = outside_project_dir.join("stale-pane");

        std::fs::create_dir_all(&outside_project_dir).expect("create outside project dir");
        std::fs::write(&outside_stale, "OtherAgent\n").expect("write outside identity");
        symlink(&outside_agent_mail, &agent_mail_parent).expect("symlink identity root parent");

        let scoped_removed = cleanup_stale_identities(&unique_key);
        assert!(
            scoped_removed.is_empty(),
            "scoped cleanup must not walk through a symlinked identity root parent: \
             {scoped_removed:?}"
        );
        assert!(
            outside_stale.exists(),
            "scoped cleanup must not remove files behind symlinked identity root parents"
        );

        let global_removed = cleanup_all_stale_identities();
        assert!(
            global_removed.is_empty(),
            "global cleanup must not walk through a symlinked identity root parent: \
             {global_removed:?}"
        );
        assert!(
            outside_stale.exists(),
            "global cleanup must not remove files behind symlinked identity root parents"
        );
        assert!(
            list_identities(&unique_key).is_empty(),
            "list_identities must not read through symlinked identity root parents"
        );
        drop(tmux);
        drop(config);
    }

    // -- write_identity_current_pane -----------------------------------------

    #[test]
    fn current_pane_returns_none_when_no_tmux_pane_env() {
        assert!(resolve_identity_for_pane("/data/test", None).is_none());
        assert!(resolve_identity_for_pane("/data/test", Some("")).is_none());
        assert!(resolve_identity_for_pane("/data/test", Some("   ")).is_none());
    }

    #[test]
    fn tmux_pane_env_is_trimmed_before_fallback() {
        crate::config::with_process_env_overrides_for_test(
            &[
                ("AM_TEST_TMUX_BIN", "/definitely/not/tmux"),
                ("TMUX_PANE", "  %7  "),
            ],
            || {
                assert_eq!(get_composite_tmux_pane_id().as_deref(), Some("%7"));
            },
        );
    }

    #[cfg(unix)]
    #[test]
    fn composite_tmux_pane_id_targets_tmux_pane_env() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = TEST_CONFIG_BASE_DIR_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = tempfile::tempdir().expect("tmux stub tempdir");
        let bin_dir = temp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).expect("create bin dir");
        let tmux_path = bin_dir.join("tmux");
        let arg_log = temp.path().join("tmux-args.log");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nif [ \"$1\" = \"display-message\" ] && [ \"$2\" = \"-t\" ] && [ \"$3\" = \"%7\" ] && [ \"$4\" = \"-p\" ]; then\n  printf 'agentmail:2:7\\n'\n  exit 0\nfi\nexit 1\n",
            arg_log.display()
        );
        std::fs::write(&tmux_path, script).expect("write tmux stub");
        let mut perms = std::fs::metadata(&tmux_path)
            .expect("tmux stub metadata")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&tmux_path, perms).expect("chmod tmux stub");

        let tmux_bin = tmux_path.to_string_lossy().into_owned();
        let arg_log = arg_log.to_string_lossy().into_owned();
        crate::config::with_process_env_overrides_for_test(
            &[("AM_TEST_TMUX_BIN", tmux_bin.as_str()), ("TMUX_PANE", "%7")],
            || {
                assert_eq!(
                    get_composite_tmux_pane_id().as_deref(),
                    Some("agentmail:2:7")
                );
            },
        );

        let args = std::fs::read_to_string(arg_log).expect("read tmux arg log");
        assert!(
            args.contains("-t\n%7\n-p"),
            "tmux display-message must target TMUX_PANE, got args: {args:?}"
        );
    }

    /// GH#177 Defect 1: under `serve-http` the daemon has no caller `TMUX_PANE`,
    /// so it must FAIL CLOSED rather than run a `-t`-less `display-message` (which
    /// tmux resolves to the *active* pane) and bind the caller to whatever
    /// identity occupies it.
    #[cfg(unix)]
    #[test]
    fn daemon_without_caller_pane_fails_closed_not_active_pane_identity() {
        use std::os::unix::fs::PermissionsExt;

        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("backend");

        // The active / orchestrator pane already owns a composite-keyed identity.
        write_identity(&project, "main:19:1", "OliveSparrow").expect("write active-pane identity");

        // Fake tmux: display-message WITHOUT -t -> ACTIVE pane (main:19:1);
        //            display-message -t %97      -> caller pane (main:14:1, no identity file).
        let temp = tempfile::tempdir().expect("tmux stub tempdir");
        let bin_dir = temp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).expect("create bin dir");
        let tmux_path = bin_dir.join("tmux");
        let script = "#!/bin/sh\n\
             tgt=\"\"; prev=\"\"\n\
             for a in \"$@\"; do if [ \"$prev\" = \"-t\" ]; then tgt=\"$a\"; fi; prev=\"$a\"; done\n\
             if [ \"$tgt\" = \"%97\" ]; then printf 'main:14:1\\n'; else printf 'main:19:1\\n'; fi\n";
        std::fs::write(&tmux_path, script).expect("write tmux stub");
        let mut perms = std::fs::metadata(&tmux_path)
            .expect("tmux stub metadata")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&tmux_path, perms).expect("chmod tmux stub");
        let tmux_bin = tmux_path.to_string_lossy().into_owned();

        crate::config::with_process_env_overrides_for_test(
            &[("AM_TEST_TMUX_BIN", tmux_bin.as_str()), ("TMUX_PANE", "")],
            || {
                // Fix: no caller TMUX_PANE -> None, NOT the active pane (main:19:1).
                assert_eq!(
                    get_composite_tmux_pane_id(),
                    None,
                    "daemon with no caller TMUX_PANE must fail closed, not adopt the active pane"
                );
                // ...so the caller is NOT handed the active pane's OliveSparrow identity.
                assert_eq!(
                    resolve_identity_current_pane(&project),
                    None,
                    "caller must not inherit the active pane's identity under the daemon"
                );
            },
        );
        drop(config);
    }

    /// GH#177 Defect 2: a bare pane id (e.g. `%97`) must be normalized to its
    /// composite `session:window:pane` key before lookup, otherwise an explicit
    /// `resolve_pane_identity(pane_id="%97")` (or a trusted `X-Tmux-Pane` header)
    /// misses its own composite-keyed identity file and returns not-found.
    #[cfg(unix)]
    #[test]
    fn bare_pane_id_normalizes_to_composite_identity() {
        use std::os::unix::fs::PermissionsExt;

        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("backend");

        // The caller's pane %97 has composite key main:14:1, which owns the
        // identity (files are keyed by the composite, not the bare id).
        write_identity(&project, "main:14:1", "BlueLake").expect("write composite identity");

        let temp = tempfile::tempdir().expect("tmux stub tempdir");
        let bin_dir = temp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).expect("create bin dir");
        let tmux_path = bin_dir.join("tmux");
        // Fake tmux: display-message -t %97 -> main:14:1; anything else fails.
        let script = "#!/bin/sh\n\
             tgt=\"\"; prev=\"\"\n\
             for a in \"$@\"; do if [ \"$prev\" = \"-t\" ]; then tgt=\"$a\"; fi; prev=\"$a\"; done\n\
             if [ \"$tgt\" = \"%97\" ]; then printf 'main:14:1\\n'; exit 0; fi\n\
             exit 1\n";
        std::fs::write(&tmux_path, script).expect("write tmux stub");
        let mut perms = std::fs::metadata(&tmux_path)
            .expect("tmux stub metadata")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&tmux_path, perms).expect("chmod tmux stub");
        let tmux_bin = tmux_path.to_string_lossy().into_owned();

        crate::config::with_process_env_overrides_for_test(
            &[("AM_TEST_TMUX_BIN", tmux_bin.as_str()), ("TMUX_PANE", "")],
            || {
                // Bare %97 normalizes to main:14:1 and resolves the identity.
                assert_eq!(
                    resolve_identity(&project, "%97").as_deref(),
                    Some("BlueLake"),
                    "bare %97 must normalize to its composite key and resolve the identity"
                );
                // A bare pane tmux doesn't know still returns None (no false match).
                assert_eq!(resolve_identity(&project, "%99"), None);
            },
        );
        drop(config);
    }

    #[test]
    fn explicit_pane_identity_helpers_do_not_consult_current_pane() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("explicit-pane-project");

        write_identity_with_optional_pane(&project, Some("%42"), "BlueLake")
            .expect("explicit pane should be used")
            .expect("write explicit identity");

        crate::config::with_process_env_overrides_for_test(&[("TMUX_PANE", "%7")], || {
            assert_eq!(
                resolve_identity_with_optional_pane(&project, Some("%42")).as_deref(),
                Some("BlueLake")
            );
            assert!(
                resolve_identity_with_optional_pane(&project, Some("%7")).is_none(),
                "explicit pane must not fall back to TMUX_PANE when a different pane is supplied"
            );
        });
        drop(config);
    }

    #[test]
    fn resolve_identity_with_path_reports_legacy_ntm_path() {
        let tmp = tempfile::tempdir().expect("temp project key");
        let unique_key = tmp
            .path()
            .join("legacy-project")
            .to_string_lossy()
            .into_owned();
        let pane = "%42";
        let hash = project_hash(&unique_key);
        let sanitized = sanitize_pane_id(pane);
        let legacy_ntm = PathBuf::from(format!("/tmp/agent-mail-name.{hash}.{sanitized}"));
        std::fs::write(&legacy_ntm, "BlueLake\n").expect("write legacy identity");

        let resolved =
            resolve_identity_with_path(&unique_key, pane).expect("resolve legacy identity");
        assert_eq!(resolved.0, "BlueLake");
        assert_eq!(resolved.1, legacy_ntm);

        let _ = std::fs::remove_file(&resolved.1);
    }

    // -- GH#252: structured records, liveness predicate, adoption rule -------

    /// Write an executable tmux stub and return its path as a string.
    #[cfg(unix)]
    fn write_tmux_stub(dir: &Path, script: &str) -> String {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.join("tmux");
        std::fs::write(&path, script).expect("write tmux stub");
        let mut perms = std::fs::metadata(&path)
            .expect("tmux stub metadata")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod tmux stub");
        path.to_string_lossy().into_owned()
    }

    /// Build a tmux stub that answers the record-side liveness probe (invoked
    /// with `-S <socket>`) with `liveness_body` and the target-facts query
    /// (no `-S`) with `target_body`. Bodies are raw `sh` snippets; use
    /// `stub_print(line)` for a success response and `"exit 1"` for failure.
    #[cfg(unix)]
    fn liveness_stub_script(liveness_body: &str, target_body: &str) -> String {
        format!(
            "#!/bin/sh\n\
             sock=\"\"; tgt=\"\"; prev=\"\"\n\
             for a in \"$@\"; do\n\
             if [ \"$prev\" = \"-S\" ]; then sock=\"$a\"; fi\n\
             if [ \"$prev\" = \"-t\" ]; then tgt=\"$a\"; fi\n\
             prev=\"$a\"\n\
             done\n\
             if [ -n \"$sock\" ]; then\n{liveness_body}\nelse\n{target_body}\nfi\n"
        )
    }

    /// `sh` snippet printing `line` and succeeding.
    #[cfg(unix)]
    fn stub_print(line: &str) -> String {
        format!("printf '%s\\n' '{line}'; exit 0")
    }

    /// A structured record bound to `pane`/`root_pid` on `socket`.
    fn verifiable_record(name: &str, pane: &str, root_pid: u32, socket: &str) -> String {
        serde_json::to_string(&PaneIdentityRecord {
            name: name.to_string(),
            session_name: Some("alpha".to_string()),
            pane_id: Some(pane.to_string()),
            pane_pid: Some(root_pid),
            socket_path: Some(socket.to_string()),
            written_at: Some("2026-08-20T09:00:00Z".to_string()),
        })
        .expect("serialize record")
    }

    fn write_record_fixture(path: &Path, json: &str) {
        std::fs::create_dir_all(path.parent().expect("record parent")).expect("create parent");
        std::fs::write(path, format!("{json}\n")).expect("write record fixture");
    }

    #[test]
    fn is_agent_pane_command_classifies_shells_and_wrappers() {
        // Empty / whitespace: process gone.
        assert!(!is_agent_pane_command(""));
        assert!(!is_agent_pane_command("   "));
        // Plain shells (including login-shell form): agent exited.
        assert!(!is_agent_pane_command("bash"));
        assert!(!is_agent_pane_command("zsh"));
        assert!(!is_agent_pane_command("-bash"));
        assert!(!is_agent_pane_command("fish"));
        assert!(!is_agent_pane_command("/bin/sh"));
        // Agents and runtime wrappers count as live (issue caveat: wrappers
        // like bun/node report the wrapper, not the agent).
        assert!(is_agent_pane_command("claude"));
        assert!(is_agent_pane_command("codex"));
        assert!(is_agent_pane_command("node"));
        assert!(is_agent_pane_command("bun"));
        assert!(is_agent_pane_command("python3"));
    }

    #[test]
    fn pane_binding_status_strings_are_stable() {
        assert_eq!(PaneBindingStatus::VerifiedLive.as_str(), "verified-live");
        assert_eq!(PaneBindingStatus::AdoptedDead.as_str(), "adopted-dead");
        assert_eq!(
            PaneBindingStatus::LegacyUnverified.as_str(),
            "legacy-unverified"
        );
    }

    #[test]
    fn parse_identity_record_handles_legacy_and_structured_content() {
        let legacy = parse_identity_record("BlueLake\n").expect("legacy parses");
        assert_eq!(legacy.name, "BlueLake");
        assert!(!legacy.is_verifiable());

        let structured =
            parse_identity_record(&verifiable_record("AmberRabbit", "%25", 3_452_123, "/sock"))
                .expect("structured parses");
        assert_eq!(structured.name, "AmberRabbit");
        assert_eq!(structured.session_name.as_deref(), Some("alpha"));
        assert_eq!(structured.pane_id.as_deref(), Some("%25"));
        assert_eq!(structured.pane_pid, Some(3_452_123));
        assert_eq!(structured.socket_path.as_deref(), Some("/sock"));
        assert!(structured.is_verifiable());

        assert!(parse_identity_record("").is_none());
        assert!(parse_identity_record("{\"name\":\"\"}").is_none());
        assert!(parse_identity_record("{not json").is_none());
    }

    #[test]
    fn pane_target_for_converts_composite_keys() {
        assert_eq!(pane_target_for("%3").as_deref(), Some("%3"));
        assert_eq!(pane_target_for("alpha:0:2").as_deref(), Some("alpha:0.2"));
        assert_eq!(pane_target_for("  ").as_deref(), None);
    }

    // -- the pure liveness predicate ----------------------------------------

    fn predicate_record() -> PaneIdentityRecord {
        PaneIdentityRecord {
            name: "AmberRabbit".to_string(),
            session_name: Some("alpha".to_string()),
            pane_id: Some("%25".to_string()),
            pane_pid: Some(3_452_123),
            socket_path: Some("/tmp/tmux-1000/default".to_string()),
            written_at: None,
        }
    }

    #[test]
    fn binding_liveness_with_reports_live_when_all_checks_pass() {
        let record = predicate_record();
        let outcome = binding_liveness_with(&record, |args| {
            // The probe must run against the recorded socket and pane.
            assert_eq!(args[0], "-S");
            assert_eq!(args[1], "/tmp/tmux-1000/default");
            assert!(args.contains(&"%25"));
            Some("alpha\t3452123\tclaude\n".to_string())
        });
        assert_eq!(outcome, PaneBindingLiveness::Live);
    }

    #[test]
    fn binding_liveness_with_treats_runtime_wrapper_as_live() {
        let record = predicate_record();
        let outcome =
            binding_liveness_with(&record, |_| Some("alpha\t3452123\tnode\n".to_string()));
        assert_eq!(outcome, PaneBindingLiveness::Live);
    }

    #[test]
    fn binding_liveness_with_dead_on_any_failed_check() {
        let record = predicate_record();

        // (a) recycled %N living in a different session.
        assert_eq!(
            binding_liveness_with(&record, |_| Some("beta\t3452123\tclaude\n".to_string())),
            PaneBindingLiveness::Dead
        );
        // (b) pane root pid changed (server restart / respawn).
        assert_eq!(
            binding_liveness_with(&record, |_| Some("alpha\t999\tclaude\n".to_string())),
            PaneBindingLiveness::Dead
        );
        // (c) agent exited back to its shell, or the process is gone.
        assert_eq!(
            binding_liveness_with(&record, |_| Some("alpha\t3452123\tzsh\n".to_string())),
            PaneBindingLiveness::Dead
        );
        assert_eq!(
            binding_liveness_with(&record, |_| Some("alpha\t3452123\t\n".to_string())),
            PaneBindingLiveness::Dead
        );
        // tmux query failed entirely (server gone).
        assert_eq!(
            binding_liveness_with(&record, |_| None),
            PaneBindingLiveness::Dead
        );
    }

    #[test]
    fn binding_liveness_unverifiable_without_facts_and_dead_without_socket() {
        assert_eq!(
            binding_liveness(&PaneIdentityRecord::bare("BlueLake")),
            PaneBindingLiveness::Unverifiable
        );

        let tmp = tempfile::tempdir().expect("tempdir");
        let mut record = predicate_record();
        record.socket_path = Some(
            tmp.path()
                .join("gone-socket")
                .to_string_lossy()
                .into_owned(),
        );
        assert_eq!(binding_liveness(&record), PaneBindingLiveness::Dead);
    }

    /// A socket that exists but a `tmux` binary that cannot be executed is
    /// no evidence about the binding: the predicate must report Unverifiable,
    /// never Dead (which would make every structured record adoptable and
    /// purgeable from a daemon whose PATH lacks tmux).
    #[test]
    fn binding_liveness_is_unverifiable_when_tmux_cannot_run() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sock = tmp.path().join("present-socket");
        std::fs::write(&sock, b"").expect("create socket placeholder");
        let mut record = predicate_record();
        record.socket_path = Some(sock.to_string_lossy().into_owned());
        // No AM_TEST_TMUX_BIN: the hermetic tmux command cannot be spawned.
        crate::config::with_process_env_overrides_for_test(&[("AM_TEST_TMUX_BIN", "")], || {
            assert_eq!(binding_liveness(&record), PaneBindingLiveness::Unverifiable);
        });
    }

    // -- writers record binding facts ---------------------------------------

    #[cfg(unix)]
    #[test]
    fn write_identity_records_binding_facts_inside_tmux() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("record-write-project");
        let sock = config.tempdir.path().join("tmux-sock");
        std::fs::write(&sock, b"").expect("create socket placeholder");
        let sock_text = sock.to_string_lossy().into_owned();

        let stub_dir = tempfile::tempdir().expect("stub dir");
        let script = liveness_stub_script(
            "exit 1",
            &stub_print(&format!("alpha\t%7\t4242\tclaude\t{sock_text}")),
        );
        let tmux_bin = write_tmux_stub(stub_dir.path(), &script);

        crate::config::with_process_env_overrides_for_test(
            &[("AM_TEST_TMUX_BIN", tmux_bin.as_str())],
            || {
                let path = write_identity(&project, "alpha:0:1", "BlueLake")
                    .expect("write structured identity");
                let record = read_identity_record(&path).expect("read record");
                assert_eq!(record.name, "BlueLake");
                assert_eq!(record.session_name.as_deref(), Some("alpha"));
                assert_eq!(record.pane_id.as_deref(), Some("%7"));
                assert_eq!(record.pane_pid, Some(4242));
                assert_eq!(record.socket_path.as_deref(), Some(sock_text.as_str()));
                assert!(record.written_at.is_some(), "written_at must be stamped");
            },
        );
        drop(config);
    }

    #[test]
    fn write_identity_outside_tmux_writes_name_only_record() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("no-tmux-write-project");
        // No AM_TEST_TMUX_BIN: the hermetic test tmux always fails, exactly
        // like a host without tmux. The record must carry only the name.
        let path = write_identity(&project, "%3", "BlueLake").expect("write identity");
        let record = read_identity_record(&path).expect("read record");
        assert_eq!(record.name, "BlueLake");
        assert!(!record.is_verifiable());
        assert_eq!(
            resolve_identity(&project, "%3").as_deref(),
            Some("BlueLake")
        );
        drop(config);
    }

    #[cfg(unix)]
    #[test]
    fn write_identity_refuses_live_binding_held_by_other_pane() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("live-write-refusal-project");
        let sock = config.tempdir.path().join("tmux-sock");
        std::fs::write(&sock, b"").expect("create socket placeholder");
        let sock_text = sock.to_string_lossy().into_owned();

        let path = canonical_identity_path(&project, "alpha:0:2");
        write_record_fixture(
            &path,
            &verifiable_record("AmberRabbit", "%2", 42, &sock_text),
        );

        // Record's pane %2 is alive; the writer's pane is %3 (renumber shift).
        let stub_dir = tempfile::tempdir().expect("stub dir");
        let script = liveness_stub_script(
            &stub_print("alpha\t42\tclaude"),
            &stub_print(&format!("alpha\t%3\t99\tclaude\t{sock_text}")),
        );
        let tmux_bin = write_tmux_stub(stub_dir.path(), &script);

        crate::config::with_process_env_overrides_for_test(
            &[("AM_TEST_TMUX_BIN", tmux_bin.as_str())],
            || {
                let err = write_identity(&project, "alpha:0:2", "GreenOwl")
                    .expect_err("live binding held elsewhere must refuse the overwrite");
                assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
                let record = read_identity_record(&path).expect("record survives");
                assert_eq!(record.name, "AmberRabbit");
                assert_eq!(record.pane_pid, Some(42));
            },
        );
        drop(config);
    }

    #[cfg(unix)]
    #[test]
    fn write_identity_allows_overwrite_by_live_holder_pane() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("live-write-same-holder-project");
        let sock = config.tempdir.path().join("tmux-sock");
        std::fs::write(&sock, b"").expect("create socket placeholder");
        let sock_text = sock.to_string_lossy().into_owned();

        let path = canonical_identity_path(&project, "alpha:0:2");
        write_record_fixture(
            &path,
            &verifiable_record("AmberRabbit", "%2", 42, &sock_text),
        );

        // The writer IS the recorded pane: same pane id, same socket.
        let stub_dir = tempfile::tempdir().expect("stub dir");
        let script = liveness_stub_script(
            &stub_print("alpha\t42\tclaude"),
            &stub_print(&format!("alpha\t%2\t42\tclaude\t{sock_text}")),
        );
        let tmux_bin = write_tmux_stub(stub_dir.path(), &script);

        crate::config::with_process_env_overrides_for_test(
            &[("AM_TEST_TMUX_BIN", tmux_bin.as_str())],
            || {
                write_identity(&project, "alpha:0:2", "AmberRabbit")
                    .expect("live holder may rewrite its own binding");
                let record = read_identity_record(&path).expect("read record");
                assert_eq!(record.name, "AmberRabbit");
                assert_eq!(record.pane_id.as_deref(), Some("%2"));
            },
        );
        drop(config);
    }

    // -- the adoption rule at resolution ------------------------------------

    /// Acceptance (GH#252): a respawned session reuses its prior agent names.
    /// The old holder's `pane_pid` no longer matches (the respawn got a new
    /// root process), so the binding is dead and the new occupant of the same
    /// positional key adopts the name — the roster grows by zero.
    #[cfg(unix)]
    #[test]
    fn respawned_pane_adopts_dead_binding_and_rewrites_record() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("respawn-adopt-project");
        let sock = config.tempdir.path().join("tmux-sock");
        std::fs::write(&sock, b"").expect("create socket placeholder");
        let sock_text = sock.to_string_lossy().into_owned();

        let path = canonical_identity_path(&project, "alpha:0:1");
        write_record_fixture(
            &path,
            &verifiable_record("AmberRabbit", "%5", 111, &sock_text),
        );

        // tmux says pane %5 now has root pid 222: the recorded binding is dead.
        // The pane occupying the key (also %5 — recycled) carries pid 222.
        let stub_dir = tempfile::tempdir().expect("stub dir");
        let script = liveness_stub_script(
            &stub_print("alpha\t222\tclaude"),
            &stub_print(&format!("alpha\t%5\t222\tclaude\t{sock_text}")),
        );
        let tmux_bin = write_tmux_stub(stub_dir.path(), &script);

        crate::config::with_process_env_overrides_for_test(
            &[("AM_TEST_TMUX_BIN", tmux_bin.as_str()), ("TMUX_PANE", "")],
            || {
                let (name, resolved_path, status) =
                    resolve_identity_with_binding(&project, "alpha:0:1")
                        .expect("dead binding must be adoptable");
                assert_eq!(name, "AmberRabbit", "respawn must reuse the prior name");
                assert_eq!(resolved_path, path);
                assert_eq!(status, PaneBindingStatus::AdoptedDead);

                // Adoption rewrote the record with the adopter's facts.
                let record = read_identity_record(&path).expect("read adopted record");
                assert_eq!(record.name, "AmberRabbit");
                assert_eq!(record.pane_pid, Some(222));
                assert_eq!(record.pane_id.as_deref(), Some("%5"));
                assert_eq!(record.socket_path.as_deref(), Some(sock_text.as_str()));
            },
        );
        drop(config);
    }

    /// Acceptance (GH#252): pane insertion renumbering cannot reassign a live
    /// holder's name. The record is live (session + pid match, agent running)
    /// but the pane now sitting at the composite key is a different pane, so
    /// resolution refuses and the caller mints a fresh identity.
    #[cfg(unix)]
    #[test]
    fn live_holder_in_other_pane_is_never_reassigned() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("live-holder-project");
        let isolated_home = config.tempdir.path().join("home");
        let home_text = isolated_home.to_string_lossy().into_owned();
        let sock = config.tempdir.path().join("tmux-sock");
        std::fs::write(&sock, b"").expect("create socket placeholder");
        let sock_text = sock.to_string_lossy().into_owned();

        let path = canonical_identity_path(&project, "alpha:0:2");
        write_record_fixture(
            &path,
            &verifiable_record("AmberRabbit", "%2", 42, &sock_text),
        );

        // Liveness: pane %2 is alive with the recorded pid, running an agent.
        // Target: after `split-window`, the pane at alpha:0:2 is %3.
        let stub_dir = tempfile::tempdir().expect("stub dir");
        let script = liveness_stub_script(
            &stub_print("alpha\t42\tclaude"),
            &stub_print(&format!("alpha\t%3\t99\tclaude\t{sock_text}")),
        );
        let tmux_bin = write_tmux_stub(stub_dir.path(), &script);

        crate::config::with_process_env_overrides_for_test(
            &[
                ("AM_TEST_TMUX_BIN", tmux_bin.as_str()),
                ("TMUX_PANE", ""),
                ("HOME", home_text.as_str()),
            ],
            || {
                assert_eq!(
                    resolve_identity_with_binding(&project, "alpha:0:2"),
                    None,
                    "a live holder's name must never transfer to a different pane"
                );
                // The live holder's record is untouched.
                let record = read_identity_record(&path).expect("record survives");
                assert_eq!(record.name, "AmberRabbit");
                assert_eq!(record.pane_pid, Some(42));
            },
        );
        drop(config);
    }

    /// Acceptance (GH#252): a tmux server restart recycles `%N` with a new
    /// `pane_pid`; the old socket is gone, so the binding is dead and the new
    /// server's pane adopts cleanly with the new socket recorded.
    #[cfg(unix)]
    #[test]
    fn server_restart_recycled_pane_adopts_with_new_socket() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("server-restart-project");
        let old_sock = config.tempdir.path().join("old-sock");
        let new_sock = config.tempdir.path().join("new-sock");
        // Old socket intentionally missing (server killed); new one exists.
        std::fs::write(&new_sock, b"").expect("create new socket placeholder");
        let old_sock_text = old_sock.to_string_lossy().into_owned();
        let new_sock_text = new_sock.to_string_lossy().into_owned();

        let path = canonical_identity_path(&project, "alpha:0:1");
        write_record_fixture(
            &path,
            &verifiable_record("AmberRabbit", "%1", 100, &old_sock_text),
        );

        let stub_dir = tempfile::tempdir().expect("stub dir");
        let script = liveness_stub_script(
            "exit 1",
            &stub_print(&format!("alpha\t%1\t555\tclaude\t{new_sock_text}")),
        );
        let tmux_bin = write_tmux_stub(stub_dir.path(), &script);

        crate::config::with_process_env_overrides_for_test(
            &[("AM_TEST_TMUX_BIN", tmux_bin.as_str()), ("TMUX_PANE", "")],
            || {
                let (name, _, status) = resolve_identity_with_binding(&project, "alpha:0:1")
                    .expect("dead binding (socket gone) must be adoptable");
                assert_eq!(name, "AmberRabbit");
                assert_eq!(status, PaneBindingStatus::AdoptedDead);

                let record = read_identity_record(&path).expect("read adopted record");
                assert_eq!(record.pane_pid, Some(555));
                assert_eq!(record.socket_path.as_deref(), Some(new_sock_text.as_str()));
            },
        );
        drop(config);
    }

    /// Acceptance (GH#252): two parallel tmux servers with identical session
    /// layouts must never cross-adopt. The recorded socket routes the check:
    /// the record is live on server A, so a caller whose pane lives on server
    /// B is refused even though pane id and layout coincide.
    #[cfg(unix)]
    #[test]
    fn parallel_servers_route_liveness_by_socket() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("parallel-servers-project");
        let isolated_home = config.tempdir.path().join("home");
        let home_text = isolated_home.to_string_lossy().into_owned();
        let live_sock = config.tempdir.path().join("sock-a");
        let caller_sock = config.tempdir.path().join("sock-b");
        std::fs::write(&live_sock, b"").expect("create socket a");
        std::fs::write(&caller_sock, b"").expect("create socket b");
        let live_sock_text = live_sock.to_string_lossy().into_owned();
        let caller_sock_text = caller_sock.to_string_lossy().into_owned();

        let path = canonical_identity_path(&project, "alpha:0:2");
        write_record_fixture(
            &path,
            &verifiable_record("AmberRabbit", "%2", 42, &live_sock_text),
        );

        // Liveness against socket A: alive. Caller's identically-numbered
        // pane lives on socket B.
        let stub_dir = tempfile::tempdir().expect("stub dir");
        let liveness_body = format!(
            "if [ \"$sock\" = '{live_sock_text}' ]; then {}\nfi\nexit 1",
            stub_print("alpha\t42\tclaude")
        );
        let script = liveness_stub_script(
            &liveness_body,
            &stub_print(&format!("alpha\t%2\t42\tclaude\t{caller_sock_text}")),
        );
        let tmux_bin = write_tmux_stub(stub_dir.path(), &script);

        crate::config::with_process_env_overrides_for_test(
            &[
                ("AM_TEST_TMUX_BIN", tmux_bin.as_str()),
                ("TMUX_PANE", ""),
                ("HOME", home_text.as_str()),
            ],
            || {
                assert_eq!(
                    resolve_identity_with_binding(&project, "alpha:0:2"),
                    None,
                    "identical layouts on parallel servers must not cross-adopt a live name"
                );
                let record = read_identity_record(&path).expect("record survives");
                assert_eq!(record.socket_path.as_deref(), Some(live_sock_text.as_str()));
            },
        );
        drop(config);
    }

    /// GH#252 legacy compat: a bare-name file whose key pane exists and runs
    /// an agent is conservatively treated as live — resolution returns the
    /// name without rewriting the file (no theft, no upgrade).
    #[cfg(unix)]
    #[test]
    fn legacy_file_with_agent_at_key_is_conservatively_live() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("legacy-live-project");
        let sock = config.tempdir.path().join("tmux-sock");
        std::fs::write(&sock, b"").expect("create socket placeholder");
        let sock_text = sock.to_string_lossy().into_owned();

        let path = canonical_identity_path(&project, "alpha:0:2");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        std::fs::write(&path, "AmberRabbit\n").expect("write legacy bare-name file");

        let stub_dir = tempfile::tempdir().expect("stub dir");
        let script = liveness_stub_script(
            "exit 1",
            &stub_print(&format!("alpha\t%2\t42\tclaude\t{sock_text}")),
        );
        let tmux_bin = write_tmux_stub(stub_dir.path(), &script);

        crate::config::with_process_env_overrides_for_test(
            &[("AM_TEST_TMUX_BIN", tmux_bin.as_str()), ("TMUX_PANE", "")],
            || {
                let (name, _, status) = resolve_identity_with_binding(&project, "alpha:0:2")
                    .expect("legacy file must resolve under the compat rule");
                assert_eq!(name, "AmberRabbit");
                assert_eq!(status, PaneBindingStatus::LegacyUnverified);
                assert_eq!(
                    std::fs::read_to_string(&path).expect("read file"),
                    "AmberRabbit\n",
                    "a conservatively-live legacy file must not be rewritten"
                );
            },
        );
        drop(config);
    }

    /// GH#252 legacy compat: when the key pane idles in a plain shell (the
    /// agent exited), the bare-name file is adoptable and the first adoption
    /// upgrades it to a structured record carrying the adopter's facts.
    #[cfg(unix)]
    #[test]
    fn legacy_file_upgrades_to_structured_record_on_adoption() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("legacy-upgrade-project");
        let sock = config.tempdir.path().join("tmux-sock");
        std::fs::write(&sock, b"").expect("create socket placeholder");
        let sock_text = sock.to_string_lossy().into_owned();

        let path = canonical_identity_path(&project, "alpha:0:2");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        std::fs::write(&path, "AmberRabbit\n").expect("write legacy bare-name file");

        let stub_dir = tempfile::tempdir().expect("stub dir");
        let script = liveness_stub_script(
            "exit 1",
            &stub_print(&format!("alpha\t%2\t42\tzsh\t{sock_text}")),
        );
        let tmux_bin = write_tmux_stub(stub_dir.path(), &script);

        crate::config::with_process_env_overrides_for_test(
            &[("AM_TEST_TMUX_BIN", tmux_bin.as_str()), ("TMUX_PANE", "")],
            || {
                let (name, _, status) = resolve_identity_with_binding(&project, "alpha:0:2")
                    .expect("legacy file with idle shell must be adoptable");
                assert_eq!(name, "AmberRabbit");
                assert_eq!(status, PaneBindingStatus::AdoptedDead);

                let record = read_identity_record(&path).expect("read upgraded record");
                assert_eq!(record.name, "AmberRabbit");
                assert_eq!(record.session_name.as_deref(), Some("alpha"));
                assert_eq!(record.pane_id.as_deref(), Some("%2"));
                assert_eq!(record.pane_pid, Some(42));
                assert_eq!(record.socket_path.as_deref(), Some(sock_text.as_str()));
            },
        );
        drop(config);
    }

    /// GH#252 out-of-scope preservation: with no tmux context at all, a
    /// legacy file resolves exactly as before — name returned, file bytes
    /// untouched, status reported as legacy-unverified.
    #[test]
    fn no_tmux_context_preserves_legacy_resolution_untouched() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("no-tmux-legacy-project");

        let path = canonical_identity_path(&project, "alpha:0:2");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        std::fs::write(&path, "AmberRabbit\n").expect("write legacy bare-name file");

        // No AM_TEST_TMUX_BIN: every tmux shell-out fails, as on a tmux-less
        // host. Resolution must behave exactly as before GH#252.
        let (name, resolved_path, status) = resolve_identity_with_binding(&project, "alpha:0:2")
            .expect("legacy resolution must be preserved without tmux");
        assert_eq!(name, "AmberRabbit");
        assert_eq!(resolved_path, path);
        assert_eq!(status, PaneBindingStatus::LegacyUnverified);
        assert_eq!(
            std::fs::read_to_string(&path).expect("read file"),
            "AmberRabbit\n"
        );
        drop(config);
    }

    /// A structured record that this process cannot check (tmux not
    /// executable) resolves under the compatibility rule exactly like a
    /// legacy file: name returned, record untouched, never labelled adopted.
    #[test]
    fn structured_record_without_executable_tmux_resolves_as_legacy_unverified() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("no-tmux-structured-project");
        let sock = config.tempdir.path().join("present-socket");
        std::fs::write(&sock, b"").expect("create socket placeholder");
        let sock_text = sock.to_string_lossy().into_owned();

        let path = canonical_identity_path(&project, "alpha:0:2");
        let json = verifiable_record("AmberRabbit", "%2", 42, &sock_text);
        write_record_fixture(&path, &json);

        crate::config::with_process_env_overrides_for_test(
            &[("AM_TEST_TMUX_BIN", ""), ("TMUX_PANE", "")],
            || {
                let (name, resolved_path, status) =
                    resolve_identity_with_binding(&project, "alpha:0:2")
                        .expect("unverifiable record must still resolve");
                assert_eq!(name, "AmberRabbit");
                assert_eq!(resolved_path, path);
                assert_eq!(status, PaneBindingStatus::LegacyUnverified);
                assert_eq!(
                    std::fs::read_to_string(&path).expect("read file"),
                    format!("{json}\n"),
                    "an uncheckable record must not be rewritten"
                );
            },
        );
        drop(config);
    }

    // -- cleanup uses the predicate -----------------------------------------

    /// Acceptance (GH#252): cleanup never removes a record whose binding
    /// passes the predicate; dead structured records are purged; legacy files
    /// keep the live-pane-list rule.
    #[cfg(unix)]
    #[test]
    fn cleanup_keeps_live_structured_records_and_purges_dead_ones() {
        let config = IsolatedConfigBaseDir::new();
        let tmux = LiveTmuxPanesGuard::new(vec!["legacy-live-0-0".to_string()]);
        let project = config.project_key("cleanup-predicate-project");
        let sock = config.tempdir.path().join("tmux-sock");
        std::fs::write(&sock, b"").expect("create socket placeholder");
        let sock_text = sock.to_string_lossy().into_owned();

        let live_path = canonical_identity_path(&project, "alpha:0:1");
        write_record_fixture(
            &live_path,
            &verifiable_record("LiveAgent", "%1", 42, &sock_text),
        );
        let dead_path = canonical_identity_path(&project, "alpha:0:9");
        write_record_fixture(
            &dead_path,
            &verifiable_record("DeadAgent", "%9", 43, &sock_text),
        );
        let legacy_live_path = canonical_identity_path(&project, "legacy-live:0:0");
        std::fs::write(&legacy_live_path, "LegacyLive\n").expect("write legacy live");
        let legacy_stale_path = canonical_identity_path(&project, "legacy-stale:0:0");
        std::fs::write(&legacy_stale_path, "LegacyStale\n").expect("write legacy stale");

        // Pane %1 passes the predicate; pane %9 is gone.
        let stub_dir = tempfile::tempdir().expect("stub dir");
        let liveness_body = format!(
            "if [ \"$tgt\" = '%1' ]; then {}\nfi\nexit 1",
            stub_print("alpha\t42\tclaude")
        );
        let script = liveness_stub_script(&liveness_body, "exit 1");
        let tmux_bin = write_tmux_stub(stub_dir.path(), &script);

        crate::config::with_process_env_overrides_for_test(
            &[("AM_TEST_TMUX_BIN", tmux_bin.as_str())],
            || {
                let removed = cleanup_stale_identities(&project);
                assert!(
                    removed.contains(&dead_path),
                    "dead structured record must be purged: {removed:?}"
                );
                assert!(
                    removed.contains(&legacy_stale_path),
                    "stale legacy file must be purged: {removed:?}"
                );
                assert!(
                    live_path.exists(),
                    "cleanup must never remove a record whose binding passes the predicate"
                );
                assert!(
                    legacy_live_path.exists(),
                    "legacy file matching a live pane key must be kept"
                );
            },
        );
        drop(tmux);
        drop(config);
    }

    /// GH#252: a structured record whose socket no longer exists is purged
    /// once tmux reports live panes on this host (the server was restarted
    /// or killed while others run), without needing a tmux shell-out; legacy
    /// files keep the live-pane-list rule.
    #[test]
    fn cleanup_purges_record_with_missing_socket_when_local_panes_exist() {
        let config = IsolatedConfigBaseDir::new();
        let tmux = LiveTmuxPanesGuard::new(vec!["legacy-0-0".to_string()]);
        let project = config.project_key("cleanup-gone-socket-project");
        let gone_sock = config
            .tempdir
            .path()
            .join("gone-sock")
            .to_string_lossy()
            .into_owned();

        let dead_path = canonical_identity_path(&project, "alpha:0:9");
        write_record_fixture(
            &dead_path,
            &verifiable_record("DeadAgent", "%9", 43, &gone_sock),
        );
        let legacy_path = canonical_identity_path(&project, "legacy:0:0");
        std::fs::write(&legacy_path, "LegacyAgent\n").expect("write legacy");

        crate::config::with_process_env_overrides_for_test(&[("AM_TEST_TMUX_BIN", "")], || {
            let removed = cleanup_stale_identities(&project);
            assert!(
                removed.contains(&dead_path),
                "record with a gone socket must be purged: {removed:?}"
            );
            assert!(
                legacy_path.exists(),
                "legacy file matching a live pane key must be kept"
            );
        });
        drop(tmux);
        drop(config);
    }

    /// Conservative retention (GH#252 review): while tmux reports no panes on
    /// this host, cleanup must not purge anything — neither socket-gone
    /// records (indistinguishable from records written on another host that
    /// shares this config dir) nor records with a present socket that cannot
    /// be checked because tmux is not executable here. A stopped or
    /// unreachable tmux must never mass-purge structured identities.
    #[test]
    fn cleanup_without_local_panes_retains_all_structured_records() {
        let config = IsolatedConfigBaseDir::new();
        let tmux = LiveTmuxPanesGuard::new(Vec::new());
        let project = config.project_key("cleanup-no-panes-project");
        let gone_sock = config
            .tempdir
            .path()
            .join("gone-sock")
            .to_string_lossy()
            .into_owned();
        let present_sock = config.tempdir.path().join("present-sock");
        std::fs::write(&present_sock, b"").expect("create socket placeholder");
        let present_sock_text = present_sock.to_string_lossy().into_owned();

        let gone_socket_path = canonical_identity_path(&project, "alpha:0:9");
        write_record_fixture(
            &gone_socket_path,
            &verifiable_record("GoneSocketAgent", "%9", 43, &gone_sock),
        );
        let uncheckable_path = canonical_identity_path(&project, "alpha:0:1");
        write_record_fixture(
            &uncheckable_path,
            &verifiable_record("UncheckableAgent", "%1", 42, &present_sock_text),
        );
        let legacy_path = canonical_identity_path(&project, "legacy:0:0");
        std::fs::write(&legacy_path, "LegacyAgent\n").expect("write legacy");

        // No AM_TEST_TMUX_BIN: tmux cannot be executed at all.
        crate::config::with_process_env_overrides_for_test(&[("AM_TEST_TMUX_BIN", "")], || {
            let removed = cleanup_stale_identities(&project);
            assert!(
                removed.is_empty(),
                "cleanup with no local panes must retain everything: {removed:?}"
            );
            assert!(gone_socket_path.exists());
            assert!(uncheckable_path.exists());
            assert!(legacy_path.exists());

            let removed_all = cleanup_all_stale_identities();
            assert!(
                removed_all.is_empty(),
                "global cleanup with no local panes must retain everything: {removed_all:?}"
            );
        });
        drop(tmux);
        drop(config);
    }

    /// A record with a present socket that tmux cannot check (not executable)
    /// is treated like a legacy file even when local panes exist: kept when
    /// its file name matches a live pane key, purged otherwise.
    #[test]
    fn cleanup_applies_legacy_rule_to_uncheckable_structured_records() {
        let config = IsolatedConfigBaseDir::new();
        let tmux = LiveTmuxPanesGuard::new(vec!["alpha-0-1".to_string()]);
        let project = config.project_key("cleanup-uncheckable-project");
        let present_sock = config.tempdir.path().join("present-sock");
        std::fs::write(&present_sock, b"").expect("create socket placeholder");
        let present_sock_text = present_sock.to_string_lossy().into_owned();

        let kept_path = canonical_identity_path(&project, "alpha:0:1");
        write_record_fixture(
            &kept_path,
            &verifiable_record("KeptAgent", "%1", 42, &present_sock_text),
        );
        let stale_path = canonical_identity_path(&project, "alpha:0:7");
        write_record_fixture(
            &stale_path,
            &verifiable_record("StaleAgent", "%7", 44, &present_sock_text),
        );

        crate::config::with_process_env_overrides_for_test(&[("AM_TEST_TMUX_BIN", "")], || {
            let removed = cleanup_stale_identities(&project);
            assert!(
                removed.contains(&stale_path),
                "uncheckable record not matching a live pane key must be purged: {removed:?}"
            );
            assert!(
                kept_path.exists(),
                "uncheckable record matching a live pane key must be kept"
            );
        });
        drop(tmux);
        drop(config);
    }
}
