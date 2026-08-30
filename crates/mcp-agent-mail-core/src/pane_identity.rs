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
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct BindingGeneration {
    pane_id: String,
    socket_path: PathBuf,
    socket_device: u64,
    socket_inode: u64,
    server_pid: u32,
    tmux_pane_id: String,
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Top-level directory under `~/.config` for agent-mail pane identity files.
const IDENTITY_DIR_NAME: &str = "agent-mail/identity";

/// How many hex chars of the project hash to use in the directory name.
const PROJECT_HASH_LEN: usize = 12;

/// Schema marker for rows carrying tmux server-generation evidence.
/// `parse_binding_generation` requires it, so only rows that captured a
/// complete, unambiguous generation may carry it (agent-factory-3tf).
const BINDING_SCHEMA_V1: &str = "am.pane-binding.v1";

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
#[cfg(test)]
static TEST_TMUX_QUERY_CALLS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

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
    /// `am.pane-binding.v1` marker when the row carries generation evidence
    /// (agent-factory-3tf). Absent on rows written by the GH#252-only writer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<String>,
    /// Identity key the row was written under (bare `%25` or composite
    /// `session:window:pane`). Distinct from `pane_id`, whose meaning is
    /// version-dependent; never probe tmux with this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_key: Option<String>,
    /// Authoritative bare tmux pane id (`%25`) for this binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux_pane_id: Option<String>,
    /// `st_dev` of the tmux server socket, pinning the server generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket_device: Option<u64>,
    /// `st_ino` of the tmux server socket, pinning the server generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket_inode: Option<u64>,
    /// tmux server pid, pinning the server generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_pid: Option<u32>,
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
            schema_version: None,
            identity_key: None,
            tmux_pane_id: None,
            socket_device: None,
            socket_inode: None,
            server_pid: None,
        }
    }

    /// Bare tmux pane id to probe tmux with.
    ///
    /// `tmux_pane_id` is authoritative when present. Rows written by the
    /// GH#252-only writer put the bare id in `pane_id`, so that is the
    /// fallback. A composite `pane_id` is NOT a valid tmux target and is
    /// deliberately not returned here (agent-factory-3tf).
    #[must_use]
    pub fn probe_pane_id(&self) -> Option<&str> {
        if let Some(bare) = self.tmux_pane_id.as_deref() {
            return Some(bare);
        }
        let pane = self.pane_id.as_deref()?;
        (!pane.contains(':')).then_some(pane)
    }

    /// Whether the record carries every fact the liveness predicate needs.
    #[must_use]
    pub fn is_verifiable(&self) -> bool {
        self.session_name.is_some()
            && self.probe_pane_id().is_some()
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
    let outcome = binding_liveness_with(record, |args| {
        run_tmux_capture(args).unwrap_or_else(|_| {
            tmux_unavailable = true;
            None
        })
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
        record.probe_pane_id(),
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
    base.join(IDENTITY_DIR_NAME)
        .join(hash)
        .join(sanitize_pane_id(pane_id))
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

    // agent-factory-3tf: refuse a lossy-sanitization collision BEFORE the
    // liveness rule. Two different composite keys can sanitize to one file
    // name, and the GH#252 rule only refuses a *verifiably live* holder --
    // so without this an unverifiable row belonging to a different pane is
    // silently overwritten and two panes share one identity file.
    if let Some(existing_key) = read_identity_record(&path)
        .as_ref()
        .and_then(|record| record.identity_key.as_deref())
        && identity_keys_collide_lossily(existing_key, pane_id)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "refusing to overwrite pane identity at {}: it is bound to key \
                 '{existing_key}', which sanitizes to the same file name as \
                 '{pane_id}' but is a different pane",
                path.display()
            ),
        ));
    }

    // GH#252: never overwrite a verifiably live binding held by another pane.
    if let Some(existing) = read_identity_record(&path)
        && existing.is_verifiable()
        && binding_liveness(&existing) == PaneBindingLiveness::Live
    {
        let same_holder = facts.as_ref().is_some_and(|f| {
            existing.probe_pane_id() == Some(f.pane_id.as_str())
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

    let record = record_from_facts(agent_name, pane_id, facts.as_ref());
    write_record_content_no_follow(&path, &record)?;
    Ok(path)
}

/// Classify whether an identity belongs to the current tmux pane generation.
///
/// Pane existence alone is insufficient because tmux may recycle a pane id
/// after its server restarts or a composite target is recreated. Only a v1
/// binding whose socket inode, server PID, and stable bare tmux pane id match is
/// authoritative. Legacy rows remain explicitly unverified while a pane with
/// the same id exists.
pub fn identity_binding_state(pane_id: &str, path: &Path) -> std::io::Result<&'static str> {
    let content = read_identity_file_no_follow(path)?;
    if let Some(expected) = parse_binding_generation(&content) {
        if !binding_authorizes_pane(&expected, pane_id) {
            return Ok("abandoned");
        }
        return if tmux_binding_generation_matches(&expected)? {
            Ok("verified-live")
        } else {
            Ok("abandoned")
        };
    }
    if let Some(stored_pane_id) = parse_binding_pane_id(&content)
        && !pane_ids_are_authoritatively_compatible(pane_id, &stored_pane_id)
    {
        return Ok("abandoned");
    }
    if tmux_pane_is_live(pane_id)? {
        Ok("live-unverified")
    } else {
        Ok("abandoned")
    }
}

/// Return whether an explicitly named tmux pane is live.
///
/// Unlike the best-effort stale-identity cleanup, this is a safety gate for
/// destructive release. A failed tmux query is an error, never evidence that
/// the pane is absent.
pub fn tmux_pane_is_live(pane_id: &str) -> std::io::Result<bool> {
    tmux_pane_is_live_with_socket(pane_id, None)
}

fn tmux_pane_is_live_with_socket(
    pane_id: &str,
    recorded_socket: Option<&Path>,
) -> std::io::Result<bool> {
    let pane = pane_id.trim();
    if pane.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "pane id must not be empty",
        ));
    }
    #[cfg(test)]
    if let Some(panes) = test_live_tmux_panes() {
        let call = TEST_TMUX_QUERY_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if panes.iter().any(|candidate| candidate == "__QUERY_ERROR__") {
            return Err(std::io::Error::other("injected tmux liveness failure"));
        }
        if call >= 1
            && panes
                .iter()
                .any(|candidate| candidate == "__QUERY_ERROR_AFTER_1__")
        {
            return Err(std::io::Error::other("injected late tmux liveness failure"));
        }
        if call >= 1
            && panes.iter().any(|candidate| {
                candidate
                    .strip_prefix("__LIVE_AFTER_1__:")
                    .is_some_and(|candidate| tmux_pane_ids_match(pane, candidate))
            })
        {
            return Ok(true);
        }
        return Ok(panes
            .iter()
            .any(|candidate| tmux_pane_ids_match(pane, candidate.trim())));
    }

    if tmux_server_has_pane(pane, None)? {
        return Ok(true);
    }
    let mut sockets: std::collections::BTreeSet<PathBuf> =
        tmux_socket_paths()?.into_iter().collect();
    if let Some(socket) = recorded_socket {
        sockets.insert(socket.to_path_buf());
    }
    for socket in sockets {
        if tmux_server_has_pane(pane, Some(&socket))? {
            return Ok(true);
        }
    }
    Ok(false)
}

#[allow(clippy::literal_string_with_formatting_args)] // tmux interpolation syntax, not Rust format syntax
fn tmux_server_has_pane(pane_id: &str, socket: Option<&Path>) -> std::io::Result<bool> {
    let mut command = tmux_command();
    if let Some(socket) = socket {
        command.arg("-S").arg(socket);
    }
    let output = command
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{session_name}:#{window_index}:#{pane_index}\t#{pane_id}",
        ])
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if tmux_output_means_no_server(&stderr) {
            return Ok(false);
        }
        return Err(std::io::Error::other(format!(
            "tmux liveness query failed{}: {}",
            socket.map_or_else(String::new, |path| format!(" on {}", path.display())),
            stderr.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).lines().any(|line| {
        let (composite, bare) = line.split_once('\t').unwrap_or((line, line));
        tmux_pane_ids_match(pane_id, composite.trim()) || tmux_pane_ids_match(pane_id, bare.trim())
    }))
}

/// Test-only stand-in for a real tmux server generation.
///
/// agent-factory-3tf: before steer 1 the unit suite could only ever produce
/// *unevidenced* rows, because liveness was injected at `tmux_pane_is_live` while
/// generation capture went to a deliberately-absent tmux binary. Every release
/// test therefore exercised the legacy fall-through and none of them exercised
/// the path production actually uses. Synthesising one stable generation here,
/// and honouring the injected inventory in `tmux_server_binding_generation`,
/// moves the whole suite onto the evidenced path. Rows that must stay legacy are
/// written directly by `write_legacy_fixture`, not through this.
#[cfg(test)]
fn test_generation_socket() -> &'static Path {
    static SOCKET: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    SOCKET.get_or_init(|| {
        let path =
            std::env::temp_dir().join(format!("am-test-generation-socket-{}", std::process::id()));
        let _ = std::fs::write(&path, b"");
        path
    })
}

#[cfg(test)]
fn test_binding_generation(pane_id: &str) -> Option<BindingGeneration> {
    use std::os::unix::fs::MetadataExt as _;
    let socket = test_generation_socket();
    let metadata = std::fs::metadata(socket).ok()?;
    let requested = pane_id.trim();
    if requested.is_empty() {
        return None;
    }
    Some(BindingGeneration {
        pane_id: requested.to_string(),
        socket_path: socket.to_path_buf(),
        socket_device: metadata.dev(),
        socket_inode: metadata.ino(),
        server_pid: 424_242,
        // Keep the injected inventory the single source of liveness truth:
        // `tmux_pane_ids_match` already understands composite/bare aliasing, so
        // carrying the requested key here preserves the existing match
        // semantics exactly.
        tmux_pane_id: requested.to_string(),
    })
}

fn capture_binding_generation(pane_id: &str) -> std::io::Result<Option<BindingGeneration>> {
    #[cfg(test)]
    if crate::config::process_env_value("AM_TEST_TMUX_BIN")
        .is_none_or(|value| value.trim().is_empty())
    {
        // Unit tests have no tmux server. Record the same evidence a real
        // registration would, so the suite exercises the evidenced path.
        return Ok(test_binding_generation(pane_id));
    }

    // A registration request carries only a pane id, not a trusted tmux
    // socket namespace. Neither the daemon's inherited TMUX value nor the
    // default server is therefore authoritative: an unrelated server can
    // reuse the same bare pane id. Collect every discoverable match and only
    // persist a generation when the result is unique.
    let mut generations = std::collections::BTreeSet::new();
    if let Some(generation) = tmux_server_binding_generation(pane_id, None)? {
        generations.insert(generation);
    }
    for socket in tmux_socket_paths()? {
        if let Some(generation) = tmux_server_binding_generation(pane_id, Some(&socket))? {
            generations.insert(generation);
        }
    }
    // A bare pane id shared by multiple servers is ambiguous. Persisting no
    // generation keeps the row live-unverified and therefore non-authoritative.
    if generations.len() == 1 {
        Ok(generations.into_iter().next())
    } else {
        Ok(None)
    }
}

fn tmux_binding_generation_matches(expected: &BindingGeneration) -> std::io::Result<bool> {
    let Some(observed) =
        tmux_server_binding_generation(&expected.tmux_pane_id, Some(&expected.socket_path))?
    else {
        return Ok(false);
    };
    Ok(binding_generations_are_same(expected, &observed))
}

fn binding_generations_are_same(left: &BindingGeneration, right: &BindingGeneration) -> bool {
    left.socket_path == right.socket_path
        && left.socket_device == right.socket_device
        && left.socket_inode == right.socket_inode
        && left.server_pid == right.server_pid
        && left.tmux_pane_id == right.tmux_pane_id
}

fn binding_generation_is_live(
    pane_id: &str,
    generation: Option<&BindingGeneration>,
) -> std::io::Result<bool> {
    match generation {
        Some(expected) => {
            // agent-factory-3tf: a tmux server's socket exists for as long as
            // the server does, so its absence is positive evidence that this
            // generation is gone -- the same rule GH#252's binding_liveness
            // already applies to a recorded socket_path. Concluding DEAD here
            // is what lets a genuinely dead binding be released on a host
            // where tmux cannot be executed at all; without it the row stays
            // poisoned forever, which is the bug this line exists to fix.
            //
            // This stays a DEAD-only shortcut. A tmux query that FAILS is
            // still an error and never evidence of absence, so the release
            // gate remains fail-closed everywhere else.
            if !expected.socket_path.exists() {
                return Ok(false);
            }
            tmux_binding_generation_matches(expected)
        }
        // agent-factory-3tf, binding steer 1: an unevidenced row is REFUSED,
        // never grandfathered.
        //
        // This arm used to fall through to `tmux_pane_is_live(pane_id)`, which
        // asks "is SOME pane with this bare id alive on the default socket?".
        // That is not the question the release gate needs answered. Pane ids
        // are global per tmux server and are reissued across sockets, so the
        // fall-through happily reported a DIFFERENT server's live pane as this
        // binding's liveness -- and, worse, reported dead whenever the id had
        // been recycled away, which is how a decorrelated review (StormyOsprey,
        // FAIL, 191253a) reproduced release of LIVE panes 6 times.
        //
        // The legacy population this arm served is the 70-of-81 rows written
        // before `am.pane-binding.v1`. They are not grandfathered: a row that
        // cannot prove which tmux server generation it belongs to cannot be
        // proven dead, and a destructive gate that cannot prove death must not
        // fire. Unknown is an error here, exactly as a failed tmux query is.
        // Re-registration rewrites such a row with evidence; that is the
        // recovery path, not a silent release.
        None => Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "refusing to release pane {pane_id}: binding carries no \
                 {BINDING_SCHEMA_V1} generation evidence, so it cannot be \
                 proven dead"
            ),
        )),
    }
}

/// Release-time liveness, decided on whatever evidence the row actually carries.
///
/// agent-factory-3tf, binding steer 1. A destructive release must never fire on
/// an absence of information, so this returns three outcomes, not two:
///
/// * `Ok(true)`  -- proven LIVE. Refuse.
/// * `Ok(false)` -- proven DEAD. Release may proceed.
/// * `Err(..)`   -- UNPROVABLE. Refuse, fail closed.
///
/// The tiers, strongest evidence first:
///
/// 1. An `am.pane-binding.v1` generation. This is the only evidence that
///    identifies the tmux *server* the binding belongs to, so it is the only
///    one immune to a pane id being reissued on a different socket.
/// 2. A GH#252 structured record. Its own `binding_liveness` predicate is real
///    evidence -- recorded socket gone, or session/pid/command checked against
///    the recorded socket -- and is reused verbatim rather than reimplemented.
///    Its `Unverifiable` verdict is deliberately NOT read as dead.
/// 3. Anything else -- the bare `Name\n` legacy file, which is the 70-of-81
///    population. It carries no evidence at all and is REFUSED, never
///    grandfathered. Its heal path is re-registration, which overwrites an
///    unverifiable row (see `write_identity`) and now records evidence.
fn binding_release_liveness(pane_id: &str, content: Option<&str>) -> std::io::Result<bool> {
    if let Some(generation) = content.and_then(parse_binding_generation) {
        return binding_generation_is_live(pane_id, Some(&generation));
    }
    match content
        .and_then(parse_identity_record)
        .as_ref()
        .map(binding_liveness)
    {
        Some(PaneBindingLiveness::Live) => Ok(true),
        Some(PaneBindingLiveness::Dead) => Ok(false),
        _ => binding_generation_is_live(pane_id, None),
    }
}

#[allow(clippy::literal_string_with_formatting_args)] // tmux interpolation syntax, not Rust format syntax
fn tmux_server_binding_generation(
    pane_id: &str,
    socket: Option<&Path>,
) -> std::io::Result<Option<BindingGeneration>> {
    #[cfg(test)]
    if let Some(panes) = test_live_tmux_panes() {
        let call = TEST_TMUX_QUERY_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if panes.iter().any(|candidate| candidate == "__QUERY_ERROR__") {
            return Err(std::io::Error::other("injected tmux liveness failure"));
        }
        if call >= 1
            && panes
                .iter()
                .any(|candidate| candidate == "__QUERY_ERROR_AFTER_1__")
        {
            return Err(std::io::Error::other("injected late tmux liveness failure"));
        }
        let live = panes.iter().any(|candidate| {
            candidate.strip_prefix("__LIVE_AFTER_1__:").map_or_else(
                || tmux_pane_ids_match(pane_id, candidate.trim()),
                |candidate| call >= 1 && tmux_pane_ids_match(pane_id, candidate),
            )
        });
        return Ok(live.then(|| test_binding_generation(pane_id)).flatten());
    }

    let mut command = tmux_command();
    if let Some(socket) = socket {
        command.arg("-S").arg(socket);
    }
    let output = command
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{socket_path}\t#{pid}\t#{session_name}:#{window_index}:#{pane_index}\t#{pane_id}",
        ])
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if tmux_output_means_no_server(&stderr) {
            return Ok(None);
        }
        return Err(std::io::Error::other(format!(
            "tmux binding-generation query failed{}: {}",
            socket.map_or_else(String::new, |path| format!(" on {}", path.display())),
            stderr.trim()
        )));
    }
    let requested = pane_id.trim();
    let mut exact = std::collections::BTreeSet::new();
    let mut aliases = std::collections::BTreeSet::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.split('\t');
        let Some(socket_path) = fields.next().filter(|value| !value.trim().is_empty()) else {
            continue;
        };
        let Some(server_pid) = fields.next().and_then(|value| value.parse::<u32>().ok()) else {
            continue;
        };
        let composite = fields.next().unwrap_or_default();
        let bare = fields.next().unwrap_or_default();
        if !tmux_pane_ids_match(requested, composite) && !tmux_pane_ids_match(requested, bare) {
            continue;
        }
        let socket_path = PathBuf::from(socket_path);
        let (socket_device, socket_inode) = socket_identity(&socket_path)?;
        let generation = BindingGeneration {
            pane_id: requested.to_string(),
            socket_path,
            socket_device,
            socket_inode,
            server_pid,
            tmux_pane_id: bare.trim().to_string(),
        };
        if tmux_pane_id_is_exact_match(requested, composite, bare) {
            exact.insert(generation);
        } else {
            aliases.insert(generation);
        }
    }
    if exact.len() == 1 {
        return Ok(exact.into_iter().next());
    }
    if exact.is_empty() && aliases.len() == 1 {
        return Ok(aliases.into_iter().next());
    }
    Ok(None)
}

#[cfg(unix)]
fn socket_identity(path: &Path) -> std::io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::metadata(path)?;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn socket_identity(_path: &Path) -> std::io::Result<(u64, u64)> {
    Err(std::io::Error::other(
        "tmux binding generations require Unix socket metadata",
    ))
}

fn tmux_socket_paths() -> std::io::Result<Vec<PathBuf>> {
    let mut sockets = std::collections::BTreeSet::new();
    if let Some(tmux) = crate::config::process_env_value("TMUX")
        && let Some(socket) = tmux.split(',').next().map(str::trim)
        && !socket.is_empty()
    {
        sockets.insert(PathBuf::from(socket));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};

        // `/proc` is not available on macOS. The configured home directory is
        // already required for identity storage and gives us a same-user UID
        // without relying on a Linux-only pseudo-filesystem.
        let own_uid = home_dir()
            .ok_or_else(|| std::io::Error::other("cannot determine home directory"))
            .and_then(std::fs::metadata)?
            .uid();
        let mut roots =
            std::collections::BTreeSet::from([std::env::temp_dir(), PathBuf::from("/tmp")]);
        if let Some(root) =
            crate::config::process_env_value("TMUX_TMPDIR").filter(|value| !value.trim().is_empty())
        {
            roots.insert(PathBuf::from(root));
        }
        for root in roots {
            let entries = match std::fs::read_dir(&root) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            for directory in entries {
                let directory = directory?;
                if !directory.file_name().to_string_lossy().starts_with("tmux-") {
                    continue;
                }
                let metadata = directory.metadata()?;
                if !metadata.is_dir() || metadata.uid() != own_uid {
                    continue;
                }
                for entry in std::fs::read_dir(directory.path())? {
                    let entry = entry?;
                    let metadata = entry.metadata()?;
                    if metadata.uid() == own_uid && metadata.file_type().is_socket() {
                        sockets.insert(entry.path());
                    }
                }
            }
        }

        // `TMUX_TMPDIR` may point anywhere, and the releasing process does not
        // necessarily inherit the value used by the tmux server. Linux exposes
        // bound Unix socket paths in `/proc/net/unix`; tmux still creates its
        // `tmux-<uid>` namespace below a custom root, so include every
        // same-user socket in that namespace. This prevents a live pane on a
        // custom tmux server from becoming invisible to release.
        #[cfg(target_os = "linux")]
        {
            let namespace = format!("tmux-{own_uid}");
            let unix_sockets = std::fs::read_to_string("/proc/net/unix")?;
            for line in unix_sockets.lines().skip(1) {
                let fields: Vec<&str> = line.split_ascii_whitespace().collect();
                if fields.len() < 8 {
                    continue;
                }
                let socket = PathBuf::from(fields[7..].join(" "));
                if !socket
                    .components()
                    .any(|component| component.as_os_str() == OsStr::new(&namespace))
                {
                    continue;
                }
                let metadata = std::fs::metadata(&socket)?;
                if metadata.uid() == own_uid && metadata.file_type().is_socket() {
                    sockets.insert(socket);
                }
            }
        }
    }

    Ok(sockets.into_iter().collect())
}

fn tmux_output_means_no_server(stderr: &str) -> bool {
    let message = stderr.to_ascii_lowercase();
    message.contains("no server running on")
        || (message.contains("failed to connect to server")
            && (message.contains("no such file or directory")
                || message.contains("connection refused")))
        || (message.contains("error connecting to")
            && (message.contains("no such file or directory")
                || message.contains("connection refused")))
}

fn tmux_pane_ids_match(requested: &str, observed: &str) -> bool {
    let requested = requested.trim();
    let observed = observed.trim();
    if requested.contains(':') || observed.contains(':') {
        return requested == observed || sanitize_pane_id(requested) == sanitize_pane_id(observed);
    }
    match (numeric_bare_pane(requested), numeric_bare_pane(observed)) {
        (Some(left), Some(right)) => left == right,
        _ => requested == observed,
    }
}

fn tmux_pane_id_is_exact_match(requested: &str, composite: &str, bare: &str) -> bool {
    let requested = requested.trim();
    let composite = composite.trim();
    let bare = bare.trim();
    if requested == composite || requested == bare {
        return true;
    }
    if requested.contains(':') {
        return false;
    }
    matches!(
        (numeric_bare_pane(requested), numeric_bare_pane(bare)),
        (Some(left), Some(right)) if left == right
    )
}

/// Whether a stored identity key and a requested key are a lossy-sanitization
/// collision: both composite, and different.
///
/// `foo bar:1:1` and `foo@bar:1:1` both sanitize to `foo_bar-1-1`, so the file
/// name cannot tell them apart. The recorded `identity_key` can
/// (agent-factory-3tf). Deliberately scoped to composite-vs-composite: a bare
/// id resolving to its own composite key is normal recovery (GH#177 Defect 2),
/// not a collision.
fn identity_keys_collide_lossily(stored: &str, requested: &str) -> bool {
    let stored = stored.trim();
    let requested = requested.trim();
    stored.contains(':') && requested.contains(':') && stored != requested
}

fn pane_ids_are_authoritatively_compatible(requested: &str, stored: &str) -> bool {
    let requested = requested.trim();
    let stored = stored.trim();
    if requested.contains(':') || stored.contains(':') {
        return requested == stored;
    }
    matches!(
        (numeric_bare_pane(requested), numeric_bare_pane(stored)),
        (Some(left), Some(right)) if left == right
    ) || requested == stored
}

fn binding_authorizes_pane(binding: &BindingGeneration, requested: &str) -> bool {
    pane_ids_are_authoritatively_compatible(requested, &binding.pane_id)
        || pane_ids_are_authoritatively_compatible(requested, &binding.tmux_pane_id)
}

fn numeric_bare_pane(pane: &str) -> Option<&str> {
    let numeric = pane.strip_prefix('%').unwrap_or(pane);
    numeric
        .chars()
        .all(|ch| ch.is_ascii_digit())
        .then_some(numeric)
}

/// Release one pane identity after proving the pane is absent from tmux.
///
/// `expected_agent` is a compare-and-release guard: a caller may only remove
/// the exact binding it observed before teardown. Generation-aware liveness is
/// checked before and after quarantine so a recycled pane ID does not keep an
/// abandoned binding alive or let a live binding disappear.
pub fn release_identity(
    project_key: &str,
    pane_id: &str,
    expected_agent: &str,
) -> std::io::Result<(String, PathBuf)> {
    let Some((agent_name, path)) = resolve_identity_with_path(project_key, pane_id) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no identity registered for pane {pane_id}"),
        ));
    };
    if agent_name != expected_agent.trim() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "pane {pane_id} is bound to {agent_name}, not expected agent {}",
                expected_agent.trim()
            ),
        ));
    }
    release_resolved_identity(pane_id, &agent_name, &path)
        .map(|released_path| (agent_name, released_path))
}

/// Release a binding that was already resolved from an identity inventory.
///
/// Sweep callers use this path so every row gets the same repeated direct-tmux,
/// compare-and-release, quarantine, and race checks as a one-row release.
fn release_resolved_identity(
    pane_id: &str,
    agent_name: &str,
    path: &Path,
) -> std::io::Result<PathBuf> {
    let canonical_release_path = path
        .file_name()
        .and_then(pending_original_name)
        .map_or_else(
            || path.to_path_buf(),
            |original| path.with_file_name(original),
        );
    let pending_siblings: Vec<std::ffi::OsString> = pending_release_paths(&canonical_release_path)
        .into_iter()
        .filter_map(|pending| pending.file_name().map(OsStr::to_os_string))
        .collect();
    let recorded_content = read_identity_file_no_follow(path).ok();
    if binding_release_liveness(pane_id, recorded_content.as_deref())? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("refusing to release recycled live tmux pane {pane_id}"),
        ));
    }
    // Quarantine with one atomic, directory-fd-anchored rename before the
    // final liveness decision.
    // If tmux recycles the id between the check above and this rename, the
    // subsequent live check restores the exact inode we moved (or leaves a
    // concurrently written replacement in place). This closes the
    // check-then-unlink race that could otherwise delete a new live binding.
    let anchored = AnchoredIdentity::open(path)?;
    let source_name = anchored.file_name.clone();
    let already_pending = is_release_pending_name(&source_name);
    let quarantine_name = if already_pending {
        source_name.clone()
    } else {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random)
            .map_err(|error| std::io::Error::other(format!("randomness failed: {error}")))?;
        let nonce = hex::encode(random);
        let file_name = source_name.to_string_lossy();
        std::ffi::OsString::from(format!(
            ".{file_name}.release-{}-{nonce}",
            std::process::id()
        ))
    };
    if !already_pending {
        anchored.rename_no_replace(&source_name, &quarantine_name)?;
        anchored.sync()?;
    }

    let quarantined_content = anchored.read(&quarantine_name).ok();
    let quarantined_agent = quarantined_content.as_deref().and_then(parse_identity);
    let live_after_quarantine =
        match binding_release_liveness(pane_id, quarantined_content.as_deref()) {
            Ok(live) => live,
            Err(error) => {
                if !already_pending {
                    match anchored.rename_no_replace(&quarantine_name, &source_name) {
                        Ok(()) => {}
                        // A concurrent registration won the canonical name. Keep
                        // both it and the pending row while liveness is unknown;
                        // a later release can reconcile them safely.
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(restore_error) => return Err(restore_error),
                    }
                }
                anchored.sync()?;
                return Err(error);
            }
        };
    if live_after_quarantine || quarantined_agent.as_deref() != Some(agent_name) {
        if !already_pending {
            match anchored.rename_no_replace(&quarantine_name, &source_name) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    // A concurrent registration already installed a new
                    // binding. Discard only the quarantined old row without
                    // touching that canonical replacement.
                    anchored.unlink(&quarantine_name)?;
                }
                Err(error) => return Err(error),
            }
        }
        anchored.sync()?;
        let reason = if live_after_quarantine {
            format!("refusing to release recycled live tmux pane {pane_id}")
        } else {
            format!("pane {pane_id} binding changed during release")
        };
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            reason,
        ));
    }
    let released_name = released_name_for_pending(&quarantine_name).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "release quarantine has no durable receipt name",
        )
    })?;
    anchored.rename_no_replace(&quarantine_name, &released_name)?;
    for sibling in pending_siblings {
        if sibling == quarantine_name {
            continue;
        }
        match anchored.unlink(&sibling) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    anchored.sync()?;
    Ok(canonical_release_path.with_file_name(released_name))
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

    // 1a. A release that was quarantined but not completed (the process died
    //     inside the rename window) leaves the binding in a `.NAME.release-*`
    //     sibling. That row still holds the binding, so it must stay
    //     resolvable until a retry completes or restores it — otherwise a
    //     crash mid-release silently orphans a live agent's identity.
    //     Restored in agent-factory-3tf: the GH#252 merge dropped this step,
    //     so every pending row resolved as NotFound.
    for pending in pending_release_paths(&canonical_identity_path(project_key, pane_id)) {
        if let Some(hit) = resolver.consider(pending) {
            return Some(hit);
        }
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

    // After a pane dies tmux can no longer translate its stable bare id to
    // the composite key used as the canonical filename. Recover a unique v1
    // row by the bare id stored in its generation receipt. Ambiguity fails
    // closed because a recycled id may have multiple abandoned generations.
    if !pane_id.contains(':')
        && let Some(identity) = resolve_unique_v1_by_bare_pane(project_key, pane_id)
    {
        return Some(identity);
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
    let legacy_ntm = legacy_ntm_root().join(format!("agent-mail-name.{hash}.{sanitized}"));
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
                legacy_ntm_root().join(format!("agent-mail-name.{hash}.{bare_sanitized}"));
            if let Some(hit) = resolver.consider(legacy_ntm_bare) {
                return Some(hit);
            }
        }
    }

    None
}

fn legacy_ntm_root() -> PathBuf {
    std::fs::canonicalize("/tmp").unwrap_or_else(|_| PathBuf::from("/tmp"))
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
    if is_ntm_name && (path.starts_with("/tmp") || path.starts_with(legacy_ntm_root())) {
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
    let base = config_base_dir();
    let hash = project_hash(project_key);
    let project_dir = base.join(IDENTITY_DIR_NAME).join(&hash);
    cleanup_identity_directory(&project_dir)
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

    let Ok(entries) = std::fs::read_dir(&identity_root) else {
        return removed;
    };

    for dir_entry in entries.flatten() {
        let project_dir = dir_entry.path();
        if !dir_entry_is_real_directory(&dir_entry) {
            continue;
        }
        removed.extend(cleanup_identity_directory(&project_dir));
    }

    removed
}

fn cleanup_identity_directory(project_dir: &Path) -> Vec<PathBuf> {
    if !path_is_real_directory(project_dir) {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(project_dir) else {
        return Vec::new();
    };
    let live_panes = list_live_tmux_panes();
    let mut bindings = std::collections::BTreeMap::new();
    for entry in entries.flatten() {
        if !dir_entry_is_real_file(&entry) {
            continue;
        }
        let raw_name = entry.file_name();
        if is_released_name(&raw_name) {
            continue;
        }
        // agent-factory-3tf: upstream decides WHICH rows are stale (liveness
        // predicate, socket-gone rule, and the "retain everything when this
        // host has no live panes" guard); 7x5 decides WHAT HAPPENS to a stale
        // row (durable release receipt, never an unlink). Before this, the
        // 7x5 merge released every row and delegated the decision entirely to
        // the release gate, which tombstoned rows that were merely
        // unverifiable and left `identity_entry_is_stale` with no callers.
        if !identity_entry_is_stale(&entry, &live_panes) {
            continue;
        }
        let pending = pending_original_name(&raw_name);
        let filename_pane_id =
            pending.map_or_else(|| raw_name.to_string_lossy().into_owned(), str::to_owned);
        let path = entry.path();
        // agent-factory-3tf: report/act on the pane id a caller can hand back
        // to resolve-pane and release (`%99`), not the sanitized on-disk file
        // name (`99`). The recorded identity key is authoritative; a v1 row's
        // stored pane is next; the file name is the last resort.
        let pane_id = recorded_identity_key(&path)
            .or_else(|| binding_pane_id(&path))
            .unwrap_or(filename_pane_id);
        if let Some(agent_name) = read_identity_file(&path) {
            let replace = bindings
                .get(&pane_id)
                .is_none_or(|(_, _, was_pending)| *was_pending && pending.is_none());
            if replace {
                bindings.insert(pane_id, (agent_name, path, pending.is_some()));
            }
        }
    }
    bindings
        .into_iter()
        .filter_map(|(pane_id, (agent_name, path, _pending))| {
            release_resolved_identity(&pane_id, &agent_name, &path).ok()
        })
        .collect()
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

    let mut result = std::collections::BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(&project_dir) else {
        return Vec::new();
    };

    for entry in entries.flatten() {
        if !dir_entry_is_real_file(&entry) {
            continue;
        }
        let raw_name = entry.file_name();
        if is_released_name(&raw_name) {
            continue;
        }
        // agent-factory-3tf: upstream skips every dotfile as an internal
        // atomic-write artifact, but 7x5's pending-release rows are dotfiles
        // that still hold a real binding. Skip artifacts, keep pending rows.
        if pending_original_name(&raw_name).is_none() && identity_entry_is_internal(&entry) {
            continue;
        }
        let pending = pending_original_name(&raw_name);
        let filename_pane_id =
            pending.map_or_else(|| raw_name.to_string_lossy().into_owned(), str::to_owned);
        let path = entry.path();
        // agent-factory-3tf: report/act on the pane id a caller can hand back
        // to resolve-pane and release (`%99`), not the sanitized on-disk file
        // name (`99`). The recorded identity key is authoritative; a v1 row's
        // stored pane is next; the file name is the last resort.
        let pane_id = recorded_identity_key(&path)
            .or_else(|| binding_pane_id(&path))
            .unwrap_or(filename_pane_id);
        if let Some(name) = read_identity_file(&path) {
            let replace = result
                .get(&pane_id)
                .is_none_or(|(_, _, was_pending)| *was_pending && pending.is_none());
            if replace {
                result.insert(pane_id, (name, path, pending.is_some()));
            }
        }
    }
    result
        .into_iter()
        .map(|(pane, (name, path, _pending))| (pane, name, path))
        .collect()
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

fn identity_entry_is_internal(entry: &std::fs::DirEntry) -> bool {
    entry.file_name().to_string_lossy().starts_with('.')
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
                    Ok(metadata)
                        if metadata.file_type().is_symlink()
                            && crate::disk::is_trusted_system_directory_alias(&current) => {}
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

fn parse_identity(content: &str) -> Option<String> {
    let trimmed = content.trim().to_string();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&trimmed)
        && let Some(name) = value.get("name").and_then(serde_json::Value::as_str)
        && !name.trim().is_empty()
    {
        return Some(name.trim().to_string());
    }
    Some(trimmed)
}

fn parse_binding_generation(content: &str) -> Option<BindingGeneration> {
    let value = serde_json::from_str::<serde_json::Value>(content).ok()?;
    if value.get("schema_version")?.as_str()? != BINDING_SCHEMA_V1 {
        return None;
    }
    let tmux_pane_id = value.get("tmux_pane_id")?.as_str()?.trim();
    let pane_id = value.get("pane_id")?.as_str()?.trim();
    if tmux_pane_id.is_empty() || pane_id.is_empty() {
        return None;
    }
    Some(BindingGeneration {
        pane_id: pane_id.to_string(),
        socket_path: PathBuf::from(value.get("socket_path")?.as_str()?),
        socket_device: value.get("socket_device")?.as_u64()?,
        socket_inode: value.get("socket_inode")?.as_u64()?,
        server_pid: u32::try_from(value.get("server_pid")?.as_u64()?).ok()?,
        tmux_pane_id: tmux_pane_id.to_string(),
    })
}

fn parse_binding_pane_id(content: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(content).ok()?;
    if value.get("schema_version")?.as_str()? != BINDING_SCHEMA_V1 {
        return None;
    }
    let pane_id = value.get("pane_id")?.as_str()?.trim();
    (!pane_id.is_empty()).then(|| pane_id.to_string())
}

/// Query tmux for all live pane IDs (sanitized).
///
/// Returns composite keys (`session_name:window_index:pane_index`) for each
/// live pane, plus the legacy bare pane ID (e.g., `%3` -> `3`) for backwards
/// compatibility during cleanup. Returns an empty vec if tmux is not running
/// or the command fails.
///
/// Restored in agent-factory-3tf: the 7x5 merge dropped this along with the
/// staleness rule that consumes it.
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

fn binding_pane_id(path: &Path) -> Option<String> {
    let content = read_identity_file_no_follow(path).ok()?;
    parse_binding_pane_id(&content)
}

/// Identity key a record was written under, when it recorded one.
///
/// Lets listings and cleanup report/act on the caller-facing pane id even
/// when the file name had to be sanitized (agent-factory-3tf).
fn recorded_identity_key(path: &Path) -> Option<String> {
    let content = read_identity_file_no_follow(path).ok()?;
    let value = serde_json::from_str::<serde_json::Value>(&content).ok()?;
    let key = value.get("identity_key")?.as_str()?.trim();
    (!key.is_empty()).then(|| key.to_string())
}

fn resolve_unique_v1_by_bare_pane(
    project_key: &str,
    pane_id: &str,
) -> Option<(String, PathBuf, PaneBindingStatus)> {
    let project_dir = config_base_dir()
        .join(IDENTITY_DIR_NAME)
        .join(project_hash(project_key));
    if !path_is_real_directory(&project_dir) {
        return None;
    }
    let entries = std::fs::read_dir(project_dir).ok()?;
    let mut match_found: Option<(String, PathBuf, PaneBindingStatus)> = None;
    for entry in entries.flatten() {
        if !dir_entry_is_real_file(&entry) || is_released_name(&entry.file_name()) {
            continue;
        }
        let path = entry.path();
        let content = read_identity_file_no_follow(&path).ok()?;
        let Some(binding) = parse_binding_generation(&content) else {
            continue;
        };
        if !pane_ids_are_authoritatively_compatible(pane_id, &binding.tmux_pane_id) {
            continue;
        }
        let name = parse_identity(&content)?;
        if match_found.is_some() {
            return None;
        }
        match_found = Some((name, path, PaneBindingStatus::AdoptedDead));
    }
    match_found
}

fn is_release_pending_name(name: &OsStr) -> bool {
    name.to_string_lossy().contains(".release-")
}

/// Directory-fd anchor for destructive identity operations. Once opened, a
/// concurrent parent-directory rename or symlink swap cannot redirect any
/// rename/unlink below it.
#[cfg(unix)]
struct AnchoredIdentity {
    parent: File,
    file_name: std::ffi::OsString,
}

#[cfg(unix)]
impl AnchoredIdentity {
    fn open(path: &Path) -> std::io::Result<Self> {
        let parent_path = path.parent().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "identity has no parent")
        })?;
        if path_has_symlinked_parent(path)? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "refusing symlinked identity parent {}",
                    parent_path.display()
                ),
            ));
        }

        #[cfg(target_os = "linux")]
        let parent = {
            use nix::fcntl::{OFlag, OpenHow, ResolveFlag, openat2};
            let root = File::open("/")?;
            let relative = parent_path.strip_prefix("/").map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "identity parent must be absolute",
                )
            })?;
            let fd = openat2(
                &root,
                relative,
                OpenHow::new()
                    .flags(OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC)
                    .resolve(ResolveFlag::RESOLVE_NO_SYMLINKS),
            )
            .map_err(nix_to_io)?;
            File::from(fd)
        };

        #[cfg(not(target_os = "linux"))]
        let parent = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW)
                .open(parent_path)?
        };

        let file_name = path.file_name().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "identity has no filename")
        })?;
        Ok(Self {
            parent,
            file_name: file_name.to_os_string(),
        })
    }

    fn read(&self, name: &OsStr) -> std::io::Result<String> {
        let fd = nix::fcntl::openat(
            &self.parent,
            name,
            nix::fcntl::OFlag::O_RDONLY
                | nix::fcntl::OFlag::O_CLOEXEC
                | nix::fcntl::OFlag::O_NOFOLLOW,
            nix::sys::stat::Mode::empty(),
        )
        .map_err(nix_to_io)?;
        let mut file = File::from(fd);
        let mut content = String::new();
        file.read_to_string(&mut content)?;
        Ok(content)
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    fn rename_no_replace(&self, from: &OsStr, to: &OsStr) -> std::io::Result<()> {
        nix::fcntl::renameat2(
            &self.parent,
            from,
            &self.parent,
            to,
            nix::fcntl::RenameFlags::RENAME_NOREPLACE,
        )
        .map_err(nix_to_io)
    }

    #[cfg(all(
        not(all(target_os = "linux", target_env = "gnu")),
        not(target_os = "redox")
    ))]
    fn rename_no_replace(&self, from: &OsStr, to: &OsStr) -> std::io::Result<()> {
        // POSIX linkat is an atomic no-replace claim on `to`. Removing the old
        // link afterward gives rename semantics without the check-then-rename
        // overwrite race present in plain renameat on macOS.
        nix::unistd::linkat(
            &self.parent,
            from,
            &self.parent,
            to,
            nix::fcntl::AtFlags::empty(),
        )
        .map_err(nix_to_io)?;
        self.unlink(from)
    }

    #[cfg(target_os = "redox")]
    fn rename_no_replace(&self, _from: &OsStr, _to: &OsStr) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "atomic no-replace identity release is unavailable on this platform",
        ))
    }

    fn unlink(&self, name: &OsStr) -> std::io::Result<()> {
        nix::unistd::unlinkat(&self.parent, name, nix::unistd::UnlinkatFlags::NoRemoveDir)
            .map_err(nix_to_io)
    }

    fn sync(&self) -> std::io::Result<()> {
        self.parent.sync_all()
    }
}

#[cfg(unix)]
fn nix_to_io(error: nix::errno::Errno) -> std::io::Error {
    std::io::Error::from_raw_os_error(error as i32)
}

#[cfg(not(unix))]
struct AnchoredIdentity {
    parent: PathBuf,
    file_name: std::ffi::OsString,
}

#[cfg(not(unix))]
impl AnchoredIdentity {
    fn open(path: &Path) -> std::io::Result<Self> {
        Ok(Self {
            parent: path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf(),
            file_name: path
                .file_name()
                .unwrap_or_else(|| OsStr::new("identity"))
                .to_os_string(),
        })
    }
    fn full_path(&self, name: &OsStr) -> PathBuf {
        self.parent.join(name)
    }
    fn read(&self, name: &OsStr) -> std::io::Result<String> {
        std::fs::read_to_string(self.full_path(name))
    }
    fn rename_no_replace(&self, from: &OsStr, to: &OsStr) -> std::io::Result<()> {
        std::fs::hard_link(self.full_path(from), self.full_path(to))?;
        self.unlink(from)
    }
    fn unlink(&self, name: &OsStr) -> std::io::Result<()> {
        std::fs::remove_file(self.full_path(name))
    }
    fn sync(&self) -> std::io::Result<()> {
        Ok(())
    }
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
                    Ok(metadata)
                        if metadata.file_type().is_symlink()
                            && !crate::disk::is_trusted_system_directory_alias(&current) =>
                    {
                        return Ok(true);
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                    Err(error) => return Err(error),
                }
            }
        }
    }
    Ok(false)
}

fn pending_original_name(file_name: &OsStr) -> Option<&str> {
    let name = file_name.to_str()?.strip_prefix('.')?;
    let (original, suffix) = name.split_once(".release-")?;
    (!original.is_empty() && !suffix.is_empty()).then_some(original)
}

fn pending_release_paths(path: &Path) -> Vec<PathBuf> {
    identity_sibling_paths(path, ".release-")
}

fn released_identity_paths(path: &Path) -> Vec<PathBuf> {
    identity_sibling_paths(path, ".released-")
}

fn identity_sibling_paths(path: &Path, marker: &str) -> Vec<PathBuf> {
    let Some(parent) = path.parent() else {
        return Vec::new();
    };
    if path_has_symlinked_parent(path).unwrap_or(true) {
        return Vec::new();
    }
    let Some(file_name) = path.file_name() else {
        return Vec::new();
    };
    let prefix = format!(".{}{marker}", file_name.to_string_lossy());
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut candidates: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(prefix.as_str())
                && entry.file_type().is_ok_and(|file_type| file_type.is_file())
        })
        .map(|entry| entry.path())
        .collect();
    candidates.sort_by(|left, right| {
        let left_modified = std::fs::metadata(left)
            .and_then(|metadata| metadata.modified())
            .ok();
        let right_modified = std::fs::metadata(right)
            .and_then(|metadata| metadata.modified())
            .ok();
        left_modified
            .cmp(&right_modified)
            .then_with(|| left.cmp(right))
    });
    candidates
}

/// Resolve the newest durable release receipt for a pane.
#[must_use]
pub fn resolve_released_identity(project_key: &str, pane_id: &str) -> Option<(String, PathBuf)> {
    if let Some(hit) = released_identity_paths(&canonical_identity_path(project_key, pane_id))
        .into_iter()
        .rev()
        .find_map(|path| read_identity_record(&path).map(|record| (record.name, path)))
    {
        return Some(hit);
    }

    // agent-factory-3tf: a tombstone for a COMPOSITE-keyed row is a sibling of
    // that composite file name, so a lookup by the bare id misses it entirely
    // -- the same gap resolve_unique_v1_by_bare_pane already closes for live
    // rows. Recover it by the bare id recorded in the receipt's own
    // generation. Newest wins, matching the canonical path above: a tombstone
    // records who WAS released, it never grants authority to bind.
    if pane_id.contains(':') {
        return None;
    }
    resolve_released_by_bare_pane(project_key, pane_id)
}

/// Newest release receipt whose recorded generation names `pane_id`.
fn resolve_released_by_bare_pane(project_key: &str, pane_id: &str) -> Option<(String, PathBuf)> {
    let project_dir = config_base_dir()
        .join(IDENTITY_DIR_NAME)
        .join(project_hash(project_key));
    if !path_is_real_directory(&project_dir) {
        return None;
    }
    let mut matches: Vec<(std::time::SystemTime, String, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(project_dir).ok()?.flatten() {
        if !dir_entry_is_real_file(&entry) || !is_released_name(&entry.file_name()) {
            continue;
        }
        let path = entry.path();
        let Ok(content) = read_identity_file_no_follow(&path) else {
            continue;
        };
        let Some(generation) = parse_binding_generation(&content) else {
            continue;
        };
        if !pane_ids_are_authoritatively_compatible(pane_id, &generation.tmux_pane_id) {
            continue;
        }
        let Some(name) = parse_identity(&content) else {
            continue;
        };
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        matches.push((modified, name, path));
    }
    matches.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.2.cmp(&right.2)));
    matches.pop().map(|(_, name, path)| (name, path))
}

fn is_released_name(file_name: &OsStr) -> bool {
    file_name.to_string_lossy().contains(".released-")
}

fn released_name_for_pending(file_name: &OsStr) -> Option<std::ffi::OsString> {
    let name = file_name.to_str()?;
    name.contains(".release-")
        .then(|| std::ffi::OsString::from(name.replacen(".release-", ".released-", 1)))
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
fn record_from_facts(
    name: &str,
    identity_key: &str,
    facts: Option<&TargetPaneFacts>,
) -> PaneIdentityRecord {
    // agent-factory-3tf: the GH#252 writer recorded only the "is an agent
    // running here" facts, which left `parse_binding_generation` returning
    // None for every row production wrote. That silently downgraded the
    // destructive release gate to the `tmux_pane_is_live` fallback -- the
    // exact path that was proven to grant release of LIVE panes. The write
    // path therefore captures the server generation too, and a row that
    // cannot capture one unambiguously stays deliberately non-authoritative
    // (schema_version absent -> live-unverified, never verified-live).
    //
    // The two evidence sets are INDEPENDENT and neither gates the other.
    // GH#252's facts come from `display-message -t <pane>` ("is an agent
    // running in this pane"); the generation comes from `list-panes -a`
    // ("which tmux server does this pane belong to"). Either query can fail
    // while the other succeeds, so capturing the generation must not sit
    // behind the facts. It did, and that is why a row with no facts was
    // written unevidenced and then became unreleasable under steer 1.
    let generation = capture_binding_generation(identity_key)
        .ok()
        .flatten()
        .filter(|generation| binding_authorizes_pane(generation, identity_key));

    let Some(f) = facts else {
        let mut record = PaneIdentityRecord::bare(name);
        record.identity_key = Some(identity_key.trim().to_string());
        if let Some(generation) = generation {
            record.schema_version = Some(BINDING_SCHEMA_V1.to_string());
            record.pane_id = Some(generation.pane_id.clone());
            record.socket_path = Some(generation.socket_path.to_string_lossy().into_owned());
            record.socket_device = Some(generation.socket_device);
            record.socket_inode = Some(generation.socket_inode);
            record.server_pid = Some(generation.server_pid);
            record.tmux_pane_id = Some(generation.tmux_pane_id);
        }
        return record;
    };

    let mut record = PaneIdentityRecord {
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
        schema_version: None,
        identity_key: Some(identity_key.trim().to_string()),
        tmux_pane_id: Some(f.pane_id.clone()),
        socket_device: None,
        socket_inode: None,
        server_pid: None,
    };

    if let Some(generation) = generation {
        // v1 contract: `pane_id` is the REQUESTED identity key (composite
        // when registration used a composite key, bare when it used a bare
        // one) -- see tmux_server_binding_generation, which sets it from
        // `requested`. `binding_authorizes_pane` compares an incoming key
        // against it OR against `tmux_pane_id`, the authoritative bare id.
        // `probe_pane_id()` keeps GH#252's liveness probe on the bare id, so
        // both readers stay correct on one row.
        record.schema_version = Some(BINDING_SCHEMA_V1.to_string());
        record.pane_id = Some(generation.pane_id.clone());
        record.socket_path = Some(generation.socket_path.to_string_lossy().into_owned());
        record.socket_device = Some(generation.socket_device);
        record.socket_inode = Some(generation.socket_inode);
        record.server_pid = Some(generation.server_pid);
        record.tmux_pane_id = Some(generation.tmux_pane_id);
    }
    record
}

/// Best-effort adoption rewrite: bind `name` to the adopter's pane facts at
/// the identity file that produced the candidate (upgrading legacy files to
/// structured records in place). IO failures are ignored — adoption must not
/// break resolution.
fn adopt_record_at(path: &Path, name: &str, identity_key: &str, facts: &TargetPaneFacts) {
    if path_has_symlinked_parent(path).unwrap_or(true) {
        return;
    }
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return;
    }
    let record = record_from_facts(name, identity_key, Some(facts));
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
        // agent-factory-3tf: the file name cannot distinguish two composite
        // keys that sanitize to it, so never return a record whose recorded
        // key names a different pane.
        if let Some(stored_key) = record.identity_key.as_deref()
            && identity_keys_collide_lossily(stored_key, &self.pane_arg)
        {
            return None;
        }
        match binding_liveness(&record) {
            PaneBindingLiveness::Live => {
                let holder_matches = self.target_facts().is_some_and(|f| {
                    record.probe_pane_id() == Some(f.pane_id.as_str())
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
                    adopt_record_at(&path, &record.name, &self.pane_arg.clone(), &facts);
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
                        adopt_record_at(&path, &record.name, &self.pane_arg.clone(), &facts);
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

    // agent-factory-3tf: a row carrying tmux server-generation evidence is
    // judgeable on its own terms, so it does not need the legacy live-pane
    // inventory. Upstream's "retain everything when this host reports no
    // live panes" guard exists because an unverifiable row cannot be judged;
    // a v1 row can. A failed tmux query stays fail-closed (retained), never
    // read as evidence of absence.
    if let Some(generation) = read_identity_file_no_follow(&path)
        .ok()
        .as_deref()
        .and_then(parse_binding_generation)
    {
        return binding_generation_is_live(&generation.tmux_pane_id, Some(&generation))
            .is_ok_and(|live| !live);
    }

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

fn file_name_matches_live_pane(file_name: &OsStr, live_panes: &[String]) -> bool {
    let key = pending_original_name(file_name)
        .map_or_else(|| file_name.to_string_lossy().into_owned(), str::to_owned);
    live_panes
        .iter()
        .any(|pane| sanitize_pane_id(pane) == key || pane.trim() == key)
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

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    ensure_real_directory(parent)?;
    validate_identity_file_target(path)?;

    let file_name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("pane identity path must name a file: {}", path.display()),
        )
    })?;
    let pid = std::process::id();
    let now = crate::timestamps::now_micros();
    let mut temp_file = None;
    for attempt in 0..1024 {
        let temp_path = parent.join(format!(
            ".{}.{}.{}.{}.tmp",
            file_name.to_string_lossy(),
            pid,
            now,
            attempt
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW);
        }
        match options.open(&temp_path) {
            Ok(file) => {
                temp_file = Some((temp_path, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    let Some((temp_path, file)) = temp_file else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "could not create a unique pane identity temporary file next to {}",
                path.display()
            ),
        ));
    };

    // From here on the temporary file exists on disk: remove it on any
    // failure so aborted writes do not strand `.tmp` artifacts that listing
    // and cleanup deliberately ignore (see `identity_entry_is_internal`).
    let commit = |mut file: std::fs::File| -> std::io::Result<()> {
        file.write_all(content)?;
        file.sync_all()?;
        drop(file);

        // Revalidate immediately before the atomic replace. On Unix, rename
        // replaces a leaf symlink rather than following it; the parent check
        // also catches a directory swap that completed before this
        // validation.
        if path_has_symlinked_parent(path)? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!(
                    "refusing symlinked pane identity directory for {}",
                    path.display()
                ),
            ));
        }
        validate_identity_file_target(path)?;
        std::fs::rename(&temp_path, path)?;
        // The file contents were synced above; syncing the containing
        // directory makes the rename durable across a sudden power loss as
        // well.
        std::fs::File::open(parent)?.sync_all()
    };
    let result = commit(file);
    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    result
}

#[cfg(not(unix))]
fn write_identity_file_no_follow(path: &Path, content: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    ensure_real_directory(parent)?;
    validate_identity_file_target(path)?;

    // `std::fs::rename` cannot atomically replace an existing destination on
    // every non-Unix platform. Preserve the pre-existing portable behavior
    // instead of making identity refreshes fail after their first write.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    file.write_all(content)?;
    file.sync_all()
}

fn validate_identity_file_target(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "refusing to overwrite symlinked pane identity {}",
                path.display()
            ),
        )),
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("pane identity target is not a file: {}", path.display()),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
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
    TEST_TMUX_QUERY_CALLS.store(0, std::sync::atomic::Ordering::SeqCst);
    *TEST_LIVE_TMUX_PANES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = panes;
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

    fn identity_real_tempdir() -> tempfile::TempDir {
        let temp_root =
            std::fs::canonicalize(std::env::temp_dir()).expect("canonical temp directory");
        tempfile::Builder::new()
            .prefix("mcp-agent-mail-pane-identity-")
            .tempdir_in(temp_root)
            .expect("pane identity temp directory")
    }

    struct IsolatedConfigBaseDir {
        _guard: MutexGuard<'static, ()>,
        tempdir: tempfile::TempDir,
    }

    impl IsolatedConfigBaseDir {
        fn new() -> Self {
            let guard = TEST_CONFIG_BASE_DIR_SERIAL
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // On macOS, `std::env::temp_dir()` commonly resolves through the
            // `/var` -> `/private/var` compatibility symlink. Production code
            // deliberately rejects symlinked config-directory components, so
            // create the fixture beneath the canonical temp root.
            let tempdir = identity_real_tempdir();
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

    /// A pre-`am.pane-binding.v1` row: a bare agent name and nothing else.
    ///
    /// agent-factory-3tf: `write_identity` now records generation evidence, so a
    /// test that needs the LEGACY population must write it directly. Going
    /// through the production path would quietly produce an evidenced row and
    /// the test would stop testing what it names.
    fn write_legacy_fixture(project: &str, pane_id: &str, agent: &str) -> PathBuf {
        let path = canonical_identity_path(project, pane_id);
        std::fs::create_dir_all(path.parent().expect("identity parent"))
            .expect("create identity parent");
        std::fs::write(&path, format!("{agent}\n")).expect("write legacy fixture");
        path
    }

    fn write_v1_fixture(project: &str, composite: &str, bare: &str, agent: &str) -> PathBuf {
        let path = canonical_identity_path(project, composite);
        std::fs::create_dir_all(path.parent().expect("identity parent"))
            .expect("create identity parent");
        let content = serde_json::json!({
            "schema_version": "am.pane-binding.v1",
            "name": agent,
            "pane_id": composite,
            "socket_path": "/tmp/agent-mail-test-no-server",
            "socket_device": 7,
            "socket_inode": 11,
            "server_pid": 13,
            "tmux_pane_id": bare,
        });
        std::fs::write(&path, format!("{content}\n")).expect("write v1 fixture");
        path
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
    fn registration_refuses_lossy_session_alias_collision() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("alias-collision-project");
        let _tmux = LiveTmuxPanesGuard::new(Vec::new());
        let spaced = canonical_identity_path(&project, "foo bar:1:1");
        let punctuated = canonical_identity_path(&project, "foo@bar:1:1");
        assert_eq!(spaced, punctuated);

        write_identity(&project, "foo bar:1:1", "BlueLake").expect("first binding");
        let error = write_identity(&project, "foo@bar:1:1", "GreenHarbor")
            .expect_err("colliding target must not overwrite the first identity");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            resolve_identity(&project, "foo bar:1:1").as_deref(),
            Some("BlueLake")
        );
        assert!(resolve_identity(&project, "foo@bar:1:1").is_none());
        drop(config);
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

        let tmp = identity_real_tempdir();
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
        let tmp = identity_real_tempdir();
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
    fn read_identity_file_extracts_name_from_ntm_receipt() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file_path = tmp.path().join("identity");
        std::fs::write(
            &file_path,
            r#"{"name":"BronzeCardinal","pane_id":"%42","pane_pid":1234}"#,
        )
        .expect("write receipt");

        assert_eq!(
            read_identity_file(&file_path).as_deref(),
            Some("BronzeCardinal")
        );
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

    #[test]
    fn release_identity_removes_only_expected_dead_binding() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("release-dead-project");
        let pane = "%42";
        let path = write_identity(&project, pane, "BlueLake").expect("write identity");
        let _tmux = LiveTmuxPanesGuard::new(Vec::new());

        let (agent, released_path) =
            release_identity(&project, pane, "BlueLake").expect("release dead pane");

        assert_eq!(agent, "BlueLake");
        assert_ne!(released_path, path);
        assert!(!path.exists());
        assert!(released_path.exists());
        assert_eq!(
            resolve_released_identity(&project, pane)
                .as_ref()
                .map(|(name, _)| name.as_str()),
            Some("BlueLake")
        );
        let second = release_identity(&project, pane, "BlueLake")
            .expect_err("double release must report no binding");
        assert_eq!(second.kind(), std::io::ErrorKind::NotFound);
        write_identity(&project, pane, "NewHarbor").expect("claim after release");
        assert_eq!(
            resolve_identity(&project, pane).as_deref(),
            Some("NewHarbor")
        );
        drop(config);
    }

    #[test]
    fn legacy_live_binding_is_never_generation_verified() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("legacy-live-state");
        let tmux = LiveTmuxPanesGuard::new(vec!["%42".into()]);
        let path = write_legacy_fixture(&project, "%42", "BlueLake");

        assert_eq!(
            identity_binding_state("%42", &path).expect("classify binding"),
            "live-unverified"
        );
        drop(tmux);
        drop(config);
    }

    #[test]
    fn binding_generation_parser_requires_the_complete_v1_marker() {
        let complete = serde_json::json!({
            "schema_version": "am.pane-binding.v1",
            "name": "BlueLake",
            "pane_id": "main:0:1",
            "socket_path": "/tmp/tmux-1000/default",
            "socket_device": 7,
            "socket_inode": 11,
            "server_pid": 13,
            "tmux_pane_id": "%17",
        });
        let parsed = parse_binding_generation(&complete.to_string()).expect("parse generation");
        assert_eq!(parsed.pane_id, "main:0:1");
        assert_eq!(parsed.socket_device, 7);
        assert_eq!(parsed.socket_inode, 11);
        assert_eq!(parsed.server_pid, 13);
        assert_eq!(parsed.tmux_pane_id, "%17");

        let mut incomplete = complete;
        incomplete
            .as_object_mut()
            .expect("binding object")
            .remove("socket_inode");
        assert!(parse_binding_generation(&incomplete.to_string()).is_none());
    }

    #[test]
    fn generation_match_ignores_renamed_composite_but_not_stable_tmux_fields() {
        let original = BindingGeneration {
            pane_id: "former:1:2".into(),
            socket_path: "/tmp/tmux-1000/default".into(),
            socket_device: 7,
            socket_inode: 11,
            server_pid: 13,
            tmux_pane_id: "%42".into(),
        };
        let mut renamed = original.clone();
        renamed.pane_id = "current:1:2".into();
        assert!(binding_generations_are_same(&original, &renamed));

        renamed.server_pid = 14;
        assert!(!binding_generations_are_same(&original, &renamed));
    }

    #[test]
    fn dead_composite_v1_binding_resolves_and_releases_by_bare_pane() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("dead-composite-v1-project");
        let path = write_v1_fixture(&project, "former:1:2", "%42", "BlueLake");

        assert_eq!(
            resolve_identity(&project, "%42").as_deref(),
            Some("BlueLake")
        );
        let (_, released) =
            release_identity(&project, "%42", "BlueLake").expect("release dead v1 row");

        assert!(!path.exists());
        assert!(released.exists());
        assert_eq!(
            resolve_released_identity(&project, "%42")
                .as_ref()
                .map(|(name, path)| (name.as_str(), path)),
            Some(("BlueLake", &released))
        );
        drop(config);
    }

    #[test]
    fn ambiguous_dead_v1_generations_for_bare_pane_fail_closed() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("ambiguous-v1-project");
        write_v1_fixture(&project, "former-a:1:2", "%42", "BlueLake");
        write_v1_fixture(&project, "former-b:1:2", "%42", "RedStone");

        assert!(resolve_identity(&project, "%42").is_none());
        let error = release_identity(&project, "%42", "BlueLake")
            .expect_err("ambiguous generations must not be guessed");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        drop(config);
    }

    #[test]
    fn composite_v1_inventory_uses_stored_pane_and_cleanup_releases_it() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("composite-v1-inventory-project");
        let composite = "former:1:2";
        let path = write_v1_fixture(&project, composite, "%42", "BlueLake");

        let listed = list_identities_with_paths(&project);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].0, composite);
        assert_eq!(listed[0].1, "BlueLake");
        assert_eq!(listed[0].2, path);

        let released = cleanup_stale_identities(&project);
        assert_eq!(released.len(), 1);
        assert!(!path.exists());
        assert!(released[0].exists());
        drop(config);
    }

    #[test]
    fn exact_composite_match_beats_lossy_sanitized_alias() {
        assert!(!tmux_pane_id_is_exact_match(
            "foo@bar:1:1",
            "foo bar:1:1",
            "%0"
        ));
        assert!(tmux_pane_ids_match("foo@bar:1:1", "foo bar:1:1"));
        assert!(tmux_pane_id_is_exact_match(
            "foo@bar:1:1",
            "foo@bar:1:1",
            "%1"
        ));
    }

    #[test]
    fn successful_release_removes_older_pending_sibling() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("release-pending-sibling-project");
        let pane = "%42";
        let canonical = write_identity(&project, pane, "NewAgent").expect("write identity");
        let pending = canonical.with_file_name(".42.release-old-fixture");
        std::fs::write(&pending, "OldAgent\n").expect("write old pending identity");
        let _tmux = LiveTmuxPanesGuard::new(Vec::new());

        release_identity(&project, pane, "NewAgent").expect("release dead pane state");

        assert!(resolve_identity(&project, pane).is_none());
        assert!(!canonical.exists());
        assert!(!pending.exists());
        drop(config);
    }

    #[test]
    fn tmux_no_server_errors_mean_no_live_panes() {
        assert!(tmux_output_means_no_server(
            "no server running on /tmp/tmux-1/default"
        ));
        assert!(tmux_output_means_no_server(
            "error connecting to /tmp/tmux-1/test (No such file or directory)"
        ));
        assert!(!tmux_output_means_no_server("permission denied"));
        assert!(!tmux_output_means_no_server(
            "failed to connect to server: Permission denied"
        ));
    }

    #[test]
    fn release_identity_refuses_live_pane_and_preserves_binding() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("release-live-project");
        let pane = "%42";
        let path = write_identity(&project, pane, "BlueLake").expect("write identity");
        let _tmux = LiveTmuxPanesGuard::new(vec![pane.to_string()]);

        let error = release_identity(&project, pane, "BlueLake")
            .expect_err("live pane release must refuse");

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(path.exists());
        drop(config);
    }

    #[test]
    fn release_identity_refuses_live_unprefixed_numeric_pane() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("release-live-unprefixed-project");
        let path = write_identity(&project, "42", "BlueLake").expect("write identity");
        let _tmux = LiveTmuxPanesGuard::new(vec!["%42".to_string()]);

        let error = release_identity(&project, "42", "BlueLake")
            .expect_err("unprefixed alias of live pane must refuse");

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(path.exists());
        drop(config);
    }

    #[test]
    fn release_identity_preserves_binding_when_tmux_query_fails() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("release-query-failure-project");
        let path = write_identity(&project, "%42", "BlueLake").expect("write identity");
        let _tmux = LiveTmuxPanesGuard::new(vec!["__QUERY_ERROR__".to_string()]);

        let error = release_identity(&project, "%42", "BlueLake")
            .expect_err("tmux query failure must fail closed");

        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert!(path.exists());
        drop(config);
    }

    #[test]
    fn pending_release_remains_resolvable_after_crash_window() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("release-crash-project");
        let pane = "%42";
        let path = write_identity(&project, pane, "BlueLake").expect("write identity");
        let pending = path.with_file_name(".42.release-crash-fixture");
        std::fs::rename(&path, &pending).expect("simulate crash after quarantine rename");

        let (agent, resolved_path) =
            resolve_identity_with_path(&project, pane).expect("pending identity must resolve");

        assert_eq!(agent, "BlueLake");
        assert_eq!(resolved_path, pending);
        let _tmux = LiveTmuxPanesGuard::new(Vec::new());
        release_identity(&project, pane, "BlueLake").expect("retry pending release");
        assert!(resolve_identity(&project, pane).is_none());
        drop(config);
    }

    #[test]
    fn pending_release_survives_late_tmux_query_failure() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("release-pending-query-failure-project");
        let pane = "%42";
        let path = write_identity(&project, pane, "BlueLake").expect("write identity");
        let pending = path.with_file_name(".42.release-crash-fixture");
        std::fs::rename(&path, &pending).expect("simulate pending release");
        let _tmux = LiveTmuxPanesGuard::new(vec!["__QUERY_ERROR_AFTER_1__".to_string()]);

        let error = release_identity(&project, pane, "BlueLake")
            .expect_err("late tmux query failure must fail closed");

        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert_eq!(
            resolve_identity(&project, pane).as_deref(),
            Some("BlueLake")
        );
        assert!(pending.exists());
        drop(config);
    }

    #[test]
    fn recycled_pane_becoming_live_after_quarantine_is_restored() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("release-late-live-project");
        let pane = "%42";
        let path = write_identity(&project, pane, "BlueLake").expect("write identity");
        let _tmux = LiveTmuxPanesGuard::new(vec!["__LIVE_AFTER_1__:%42".to_string()]);

        let error = release_identity(&project, pane, "BlueLake")
            .expect_err("pane recycled after quarantine must refuse");

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(
            resolve_identity(&project, pane).as_deref(),
            Some("BlueLake")
        );
        assert!(path.exists());
        drop(config);
    }

    #[test]
    fn anchored_restore_never_overwrites_concurrent_canonical_binding() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("release-no-overwrite-project");
        let pane = "%42";
        let path = write_identity(&project, pane, "NewAgent").expect("write canonical");
        let pending = path.with_file_name(".42.release-race-fixture");
        std::fs::write(&pending, "OldAgent\n").expect("write pending identity");
        let anchored = AnchoredIdentity::open(&path).expect("open identity parent");

        let error = anchored
            .rename_no_replace(
                pending.file_name().expect("pending name"),
                path.file_name().expect("canonical name"),
            )
            .expect_err("restore must not replace a concurrent canonical binding");

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(read_identity_file(&path).as_deref(), Some("NewAgent"));
        assert_eq!(read_identity_file(&pending).as_deref(), Some("OldAgent"));
        drop(config);
    }

    #[test]
    fn release_identity_refuses_live_composite_pane_and_preserves_binding() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("release-live-composite-project");
        let pane = "session-a:1:2";
        let path = write_identity(&project, pane, "BlueLake").expect("write identity");
        let _tmux = LiveTmuxPanesGuard::new(vec![pane.to_string()]);

        let error = release_identity(&project, pane, "BlueLake")
            .expect_err("live composite pane release must refuse");

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(path.exists());
        drop(config);
    }

    #[test]
    fn release_identity_refuses_wrong_agent_and_preserves_binding() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("release-wrong-agent-project");
        let pane = "%42";
        let path = write_identity(&project, pane, "BlueLake").expect("write identity");
        let _tmux = LiveTmuxPanesGuard::new(Vec::new());

        let error = release_identity(&project, pane, "RedStone")
            .expect_err("wrong-agent release must refuse");

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(path.exists());
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
        let _tmux = LiveTmuxPanesGuard::new(Vec::new());
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
        let _tmux = LiveTmuxPanesGuard::new(Vec::new());
        let unique_key = config.project_key("list-project");
        let pane = "%99";
        write_identity(&unique_key, pane, "RedFox").expect("write identity");

        let entries = list_identities(&unique_key);
        assert!(
            entries.iter().any(|(p, n)| p == "%99" && n == "RedFox"),
            "expected RedFox entry: {entries:?}"
        );
        drop(config);
    }

    #[test]
    fn cleanup_and_listing_preserve_live_pending_release_identity() {
        let config = IsolatedConfigBaseDir::new();
        let _tmux = LiveTmuxPanesGuard::new(Vec::new());
        let scoped_project = config.project_key("pending-scoped-cleanup-project");
        let global_project = config.project_key("pending-global-cleanup-project");
        let scoped =
            write_identity(&scoped_project, "%42", "BlueLake").expect("write scoped identity");
        let global =
            write_identity(&global_project, "%42", "RedStone").expect("write global identity");
        let scoped_pending = scoped.with_file_name(".42.release-scoped-fixture");
        let global_pending = global.with_file_name(".42.release-global-fixture");
        std::fs::rename(&scoped, &scoped_pending).expect("quarantine scoped identity");
        std::fs::rename(&global, &global_pending).expect("quarantine global identity");
        set_test_live_tmux_panes(Some(vec!["42".to_string()]));

        assert_eq!(
            cleanup_stale_identities(&scoped_project),
            Vec::<PathBuf>::new()
        );
        assert!(scoped_pending.exists());
        assert_eq!(
            list_identities(&scoped_project),
            vec![("%42".to_string(), "BlueLake".to_string())]
        );

        assert_eq!(cleanup_all_stale_identities(), Vec::<PathBuf>::new());
        assert!(scoped_pending.exists());
        assert!(global_pending.exists());
        drop(config);
    }

    #[test]
    fn cleanup_releases_dead_binding_but_preserves_live_binding() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("safe-cleanup-project");
        let dead = write_identity(&project, "%41", "OldHarbor").expect("write dead binding");
        let live = write_identity(&project, "%42", "BlueLake").expect("write live binding");
        let _tmux = LiveTmuxPanesGuard::new(vec!["%42".to_string()]);

        let released = cleanup_stale_identities(&project);

        assert_eq!(released.len(), 1);
        assert!(released[0].exists());
        assert!(!dead.exists());
        assert!(live.exists());
        assert_eq!(
            resolve_identity(&project, "%42").as_deref(),
            Some("BlueLake")
        );
        write_identity(&project, "%41", "NewHarbor").expect("claim released pane");
        assert_eq!(
            resolve_identity(&project, "%41").as_deref(),
            Some("NewHarbor")
        );
        drop(config);
    }

    #[test]
    fn cleanup_fails_closed_when_tmux_query_fails() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("cleanup-query-failure-project");
        let binding = write_identity(&project, "%42", "BlueLake").expect("write identity binding");
        let _tmux = LiveTmuxPanesGuard::new(vec!["__QUERY_ERROR__".to_string()]);

        assert_eq!(cleanup_stale_identities(&project), Vec::<PathBuf>::new());
        assert!(binding.exists());
        drop(config);
    }

    #[test]
    fn cleanup_preserves_live_composite_binding_from_sanitized_filename() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("cleanup-live-composite-project");
        let pane = "session-a:1:2";
        let binding = write_identity(&project, pane, "BlueLake").expect("write binding");
        assert!(
            binding
                .file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| name == "session-a-1-2")
        );
        let _tmux = LiveTmuxPanesGuard::new(vec![pane.to_string()]);

        assert_eq!(cleanup_stale_identities(&project), Vec::<PathBuf>::new());
        assert!(binding.exists());
        drop(config);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_identity_replacement_leaves_one_complete_visible_record() {
        let config = IsolatedConfigBaseDir::new();
        let unique_key = config.project_key("atomic-replace-project");
        let pane = "%99";
        let path = write_identity(&unique_key, pane, "RedFox").expect("initial identity");
        write_identity(&unique_key, pane, "BlueLake").expect("replace identity");

        let record = read_identity_record(&path).expect("complete replacement record");
        assert_eq!(record.name, "BlueLake");
        let parent = path.parent().expect("identity parent");
        let names = std::fs::read_dir(parent)
            .expect("read identity parent")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["99"]);
        // agent-factory-3tf: the on-disk FILE name stays sanitized ("99"),
        // but the listing reports the pane id a caller can hand back to
        // resolve-pane/release ("%99") under 7x5's semantics. The test's
        // intent — exactly one complete visible record — is unchanged.
        assert_eq!(
            list_identities(&unique_key),
            vec![("%99".into(), "BlueLake".into())]
        );
        drop(config);
    }

    #[test]
    fn list_identities_ignores_internal_atomic_write_artifacts() {
        let config = IsolatedConfigBaseDir::new();
        let unique_key = config.project_key("internal-artifact-project");
        let real_path = write_identity(&unique_key, "%4", "RedFox").expect("write identity");
        std::fs::write(
            real_path
                .parent()
                .expect("identity parent")
                .join(".4.123.456.0.tmp"),
            r#"{"name":"PhantomAgent"}
"#,
        )
        .expect("write simulated interrupted temporary file");

        assert_eq!(
            list_identities(&unique_key),
            vec![("%4".into(), "RedFox".into())]
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
        let _tmux = LiveTmuxPanesGuard::new(Vec::new());
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
        let tmp = identity_real_tempdir();
        let unique_key = tmp
            .path()
            .join("legacy-project")
            .to_string_lossy()
            .into_owned();
        let pane = "%42";
        let hash = project_hash(&unique_key);
        let sanitized = sanitize_pane_id(pane);
        let legacy_ntm = legacy_ntm_root().join(format!("agent-mail-name.{hash}.{sanitized}"));
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
            ..PaneIdentityRecord::bare(name)
        })
        .expect("serialize record")
    }

    /// Whether `removed` carries the durable release receipt for `original`.
    ///
    /// agent-factory-3tf: upstream's cleanup UNLINKED a stale row and returned
    /// the path it deleted; 7x5's releases it and returns the `.released-`
    /// tombstone, which is the whole point of the 7x5 line -- a released
    /// binding must stay distinguishable from an abandoned one across a
    /// restart. 7x5 wins on the action, so these assertions check that the
    /// binding is gone AND that a receipt for it was returned.
    fn contains_release_receipt_for(removed: &[PathBuf], original: &Path) -> bool {
        let Some(name) = original
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
        else {
            return false;
        };
        let prefix = format!(".{name}.released-");
        removed.iter().any(|path| {
            path.parent() == original.parent()
                && path
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with(prefix.as_str()))
                && path.exists()
        })
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
            ..PaneIdentityRecord::bare("AmberRabbit")
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
                    contains_release_receipt_for(&removed, &dead_path),
                    "dead structured record must be released with a receipt: {removed:?}"
                );
                assert!(!dead_path.exists(), "dead binding must no longer resolve");
                // agent-factory-3tf, binding steer 1: this file used to be
                // swept here. It carries no evidence at all, so the only thing
                // that ever condemned it was "the host's live-pane inventory
                // does not list this key" -- and that inventory does not cover
                // the server the row belongs to. Sweeping on it is the
                // grandfathering the steer removes. Retention is non-
                // destructive and the row heals on re-registration.
                assert!(
                    !contains_release_receipt_for(&removed, &legacy_stale_path),
                    "an unevidenced legacy row must never be swept on inventory \
                     evidence alone: {removed:?}"
                );
                assert!(
                    legacy_stale_path.exists(),
                    "an unevidenced legacy row must be retained, not destroyed"
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
                contains_release_receipt_for(&removed, &dead_path),
                "record with a gone socket must be released with a receipt: {removed:?}"
            );
            assert!(
                !dead_path.exists(),
                "socket-gone binding must no longer resolve"
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
    /// is RETAINED whether or not its file name matches a live pane key.
    ///
    /// agent-factory-3tf, binding steer 1: upstream purged the non-matching one
    /// on the strength of the host's live-pane inventory. "tmux could not be
    /// executed" is the textbook unprovable case -- the process learned nothing
    /// about this binding -- so a destructive sweep must not fire on it. Note
    /// this is the same verdict `binding_liveness` already reaches on its own
    /// (`Unverifiable`, explicitly not `Dead`); only the sweep disagreed.
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
                removed.is_empty(),
                "a sweep that cannot execute tmux must destroy nothing: {removed:?}"
            );
            assert!(
                stale_path.exists(),
                "an uncheckable record must be retained, not swept on inventory \
                 evidence alone"
            );
            assert!(
                kept_path.exists(),
                "uncheckable record matching a live pane key must be kept"
            );
        });
        drop(tmux);
        drop(config);
    }
    /// agent-factory-3tf regression: the PRODUCTION write path must record
    /// tmux server-generation evidence.
    ///
    /// The GH#252 merge left `capture_binding_generation` with zero callers,
    /// so every row production wrote carried no generation. Nothing failed:
    /// `parse_binding_generation` simply returned None, and the destructive
    /// release gate quietly fell back to `tmux_pane_is_live` -- the path a
    /// decorrelated review had already proven grants release of LIVE panes.
    /// The whole pane suite stayed green through it, because every other test
    /// injects its record as a fixture instead of writing one.
    ///
    /// This test writes through the real path and asserts the evidence is on
    /// disk, so that regression cannot reappear silently.
    #[cfg(unix)]
    #[test]
    fn write_identity_records_generation_evidence_through_production_path() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("generation-evidence-project");

        // A real file so the generation can stat a device and inode.
        let sock = config.tempdir.path().join("tmux-generation-sock");
        std::fs::write(&sock, b"").expect("create socket placeholder");
        let sock_text = sock.to_string_lossy().into_owned();

        let stub_dir = tempfile::tempdir().expect("stub dir");
        let script = format!(
            "#!/bin/sh\n\
             cmd=\"\"\n\
             for a in \"$@\"; do\n\
             case \"$a\" in\n\
             list-panes) cmd=lp ;;\n\
             display-message) cmd=dm ;;\n\
             esac\n\
             done\n\
             if [ \"$cmd\" = lp ]; then\n\
             printf '%s\\t%s\\t%s\\t%s\\n' '{sock_text}' '4242' 'alpha:0:1' '%77'\n\
             exit 0\n\
             fi\n\
             if [ \"$cmd\" = dm ]; then\n\
             printf '%s\\t%s\\t%s\\t%s\\t%s\\n' 'alpha' '%77' '999' 'claude' '{sock_text}'\n\
             exit 0\n\
             fi\n\
             exit 1\n"
        );
        let tmux_bin = write_tmux_stub(stub_dir.path(), &script);

        crate::config::with_process_env_overrides_for_test(
            &[("AM_TEST_TMUX_BIN", tmux_bin.as_str())],
            || {
                let path = write_identity(&project, "%77", "GenAgent").expect("write identity");
                let content = read_identity_file_no_follow(&path).expect("read written record");

                // The claim that actually matters: the destructive release
                // gate can parse a generation out of what production wrote.
                let generation = parse_binding_generation(&content).expect(
                    "production write path must record generation evidence the \
                     release gate can parse",
                );
                assert_eq!(generation.tmux_pane_id, "%77");
                assert_eq!(generation.server_pid, 4242);
                assert_eq!(generation.socket_path, sock);

                let record = read_identity_record(&path).expect("parse written record");
                assert_eq!(record.name, "GenAgent");
                assert_eq!(record.schema_version.as_deref(), Some(BINDING_SCHEMA_V1));
                assert_eq!(record.identity_key.as_deref(), Some("%77"));
                // The v1 contract: `pane_id` is the requested identity key
                // (bare here, composite when registration used a composite
                // key), `tmux_pane_id` is the authoritative bare id, and the
                // GH#252 liveness probe uses the bare id via probe_pane_id().
                assert_eq!(record.pane_id.as_deref(), Some("%77"));
                assert_eq!(record.tmux_pane_id.as_deref(), Some("%77"));
                assert_eq!(record.probe_pane_id(), Some("%77"));
                assert!(record.socket_device.is_some());
                assert!(record.socket_inode.is_some());
                assert!(record.is_verifiable());
            },
        );
        drop(config);
    }
    /// agent-factory-3tf, binding steer 1: an unevidenced legacy binding is
    /// REFUSED, not grandfathered.
    ///
    /// The old `None` arm answered "is SOME pane with this bare id alive on the
    /// default socket?". That is not this binding's liveness: pane ids are
    /// global per tmux server and get reissued across sockets, so the answer
    /// was about a different server as often as not. A decorrelated review
    /// (`StormyOsprey`, FAIL, 191253a) rode that arm to release LIVE panes six
    /// times. The 70-of-81 legacy population is exactly the input that reached
    /// it, so the refusal is asserted here directly rather than left implied by
    /// the tests that happen to route around it.
    #[test]
    fn legacy_binding_without_generation_evidence_refuses_release() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("legacy-refusal-project");
        let pane = "%42";
        let path = write_legacy_fixture(&project, pane, "BlueLake");
        // The inventory reports the pane as absent -- which under the old arm
        // was read as "dead, go ahead and release".
        let _tmux = LiveTmuxPanesGuard::new(Vec::new());

        let error = release_identity(&project, pane, "BlueLake")
            .expect_err("an unevidenced legacy binding must never be released");

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(
            error.to_string().contains(BINDING_SCHEMA_V1),
            "the refusal must name the missing evidence: {error}"
        );
        assert!(path.exists(), "a refused release must leave the row intact");
        assert_eq!(
            resolve_identity(&project, pane).as_deref(),
            Some("BlueLake")
        );

        // The heal path: re-registration overwrites the unverifiable row and
        // records evidence, after which the row is releasable on its own terms.
        write_identity(&project, pane, "BlueLake").expect("re-registration must heal the row");
        let (agent, receipt) =
            release_identity(&project, pane, "BlueLake").expect("healed row releases");
        assert_eq!(agent, "BlueLake");
        assert!(receipt.exists(), "release must leave a durable receipt");
        drop(config);
    }

    /// agent-factory-3tf: a release is keyed on (project, pane), and a pane id
    /// alone never reaches across projects.
    ///
    /// Pane ids are global per tmux server while Agent Mail scopes identity per
    /// project, so the same `%N` legitimately names a different agent in each
    /// project on this machine. Releasing one must leave the others untouched.
    #[test]
    fn release_is_scoped_to_its_project() {
        let config = IsolatedConfigBaseDir::new();
        let project_a = config.project_key("scope-project-a");
        let project_b = config.project_key("scope-project-b");
        let pane = "%42";
        write_identity(&project_a, pane, "AgentA").expect("bind in project A");
        let path_b = write_identity(&project_b, pane, "AgentB").expect("bind in project B");
        assert_ne!(
            canonical_identity_path(&project_a, pane),
            canonical_identity_path(&project_b, pane),
            "two projects must not share one identity file"
        );
        let _tmux = LiveTmuxPanesGuard::new(Vec::new());

        release_identity(&project_a, pane, "AgentA").expect("release project A binding");

        assert!(
            resolve_identity(&project_a, pane).is_none(),
            "project A binding must be gone"
        );
        assert!(path_b.exists(), "project B binding must survive untouched");
        assert_eq!(
            resolve_identity(&project_b, pane).as_deref(),
            Some("AgentB"),
            "a release in one project must never reach into another"
        );
        drop(config);
    }

    /// agent-factory-3tf: compare-and-release refuses when the caller names an
    /// agent that does not hold the binding, so one agent cannot release
    /// another's pane by guessing the id.
    #[test]
    fn release_refuses_when_named_agent_does_not_hold_the_binding() {
        let config = IsolatedConfigBaseDir::new();
        let project = config.project_key("wrong-agent-project");
        let pane = "%42";
        let path = write_identity(&project, pane, "BlueLake").expect("write identity");
        let _tmux = LiveTmuxPanesGuard::new(Vec::new());

        let error = release_identity(&project, pane, "GreenHarbor")
            .expect_err("a release naming the wrong agent must refuse");

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(path.exists(), "the binding must survive a refused release");
        assert_eq!(
            resolve_identity(&project, pane).as_deref(),
            Some("BlueLake")
        );
        drop(config);
    }
}
