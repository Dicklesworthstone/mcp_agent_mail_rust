//! Data models for MCP Agent Mail
//!
//! These models map directly to the `SQLite` tables defined in the legacy Python codebase.
//! All datetime fields use naive UTC (no timezone info) for `SQLite` compatibility.

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

// =============================================================================
// Project
// =============================================================================

/// A project represents a working directory where agents coordinate.
///
/// # Constraints
/// - `slug`: Unique, indexed. Computed from `human_key` (lowercased, safe chars).
/// - `human_key`: Indexed. MUST be an absolute directory path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: Option<i64>,
    pub slug: String,
    pub human_key: String,
    pub created_at: NaiveDateTime,
}

// =============================================================================
// Product
// =============================================================================

/// A product is a logical grouping across multiple repositories/projects.
///
/// # Constraints
/// - `product_uid`: Unique, indexed.
/// - `name`: Unique, indexed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Product {
    pub id: Option<i64>,
    pub product_uid: String,
    pub name: String,
    pub created_at: NaiveDateTime,
}

/// Links products to projects (many-to-many).
///
/// # Constraints
/// - Unique: `(product_id, project_id)`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductProjectLink {
    pub id: Option<i64>,
    pub product_id: i64,
    pub project_id: i64,
    pub created_at: NaiveDateTime,
}

// =============================================================================
// Agent
// =============================================================================

/// An agent represents a coding assistant or AI model working on a project.
///
/// # Naming Rules
/// Agent names MUST be adjective+noun combinations (e.g., "`GreenLake`", "`BlueDog`").
/// - 75 adjectives × 132 nouns = 9,900 valid combinations
/// - Case-insensitive unique per project
/// - NOT descriptive role names (e.g., "`BackendHarmonizer`" is INVALID)
///
/// # Constraints
/// - Unique: `(project_id, name)`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: Option<i64>,
    pub project_id: i64,
    pub name: String,
    pub program: String,
    pub model: String,
    pub task_description: String,
    pub inception_ts: NaiveDateTime,
    pub last_active_ts: NaiveDateTime,
    /// Attachment policy: "auto" | "inline" | "file"
    pub attachments_policy: String,
    /// Contact policy: "open" | "auto" | "`contacts_only`" | "`block_all`"
    pub contact_policy: String,
}

impl Default for Agent {
    fn default() -> Self {
        let now = chrono::Utc::now().naive_utc();
        Self {
            id: None,
            project_id: 0,
            name: String::new(),
            program: String::new(),
            model: String::new(),
            task_description: String::new(),
            inception_ts: now,
            last_active_ts: now,
            attachments_policy: "auto".to_string(),
            contact_policy: "auto".to_string(),
        }
    }
}

// =============================================================================
// Message
// =============================================================================

/// A message sent between agents.
///
/// # Thread Rules
/// - `thread_id` pattern: `^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$`
/// - Max 128 chars, must start with alphanumeric
///
/// # Importance Levels
/// - "low", "normal", "high", "urgent"
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: Option<i64>,
    pub project_id: i64,
    pub sender_id: i64,
    pub thread_id: Option<String>,
    pub subject: String,
    pub body_md: String,
    /// Importance: "low" | "normal" | "high" | "urgent"
    pub importance: String,
    pub ack_required: bool,
    pub created_ts: NaiveDateTime,
    /// JSON array of attachment metadata
    pub attachments: String,
}

impl Default for Message {
    fn default() -> Self {
        Self {
            id: None,
            project_id: 0,
            sender_id: 0,
            thread_id: None,
            subject: String::new(),
            body_md: String::new(),
            importance: "normal".to_string(),
            ack_required: false,
            created_ts: chrono::Utc::now().naive_utc(),
            attachments: "[]".to_string(),
        }
    }
}

// =============================================================================
// MessageRecipient
// =============================================================================

/// Links messages to recipient agents (many-to-many).
///
/// # Kind Values
/// - "to": Primary recipient
/// - "cc": Carbon copy
/// - "bcc": Blind carbon copy
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRecipient {
    pub message_id: i64,
    pub agent_id: i64,
    /// Recipient kind: "to" | "cc" | "bcc"
    pub kind: String,
    pub read_ts: Option<NaiveDateTime>,
    pub ack_ts: Option<NaiveDateTime>,
}

impl Default for MessageRecipient {
    fn default() -> Self {
        Self {
            message_id: 0,
            agent_id: 0,
            kind: "to".to_string(),
            read_ts: None,
            ack_ts: None,
        }
    }
}

// =============================================================================
// FileReservation
// =============================================================================

/// An advisory file lock (lease) on file paths or glob patterns.
///
/// # Pattern Matching
/// Uses gitignore-style patterns (via pathspec/globset).
/// Matching is symmetric: `fnmatch(pattern, path) OR fnmatch(path, pattern)`.
///
/// # TTL
/// Minimum TTL is 60 seconds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileReservation {
    pub id: Option<i64>,
    pub project_id: i64,
    pub agent_id: i64,
    pub path_pattern: String,
    pub exclusive: bool,
    pub reason: String,
    pub created_ts: NaiveDateTime,
    pub expires_ts: NaiveDateTime,
    pub released_ts: Option<NaiveDateTime>,
}

impl Default for FileReservation {
    fn default() -> Self {
        let now = chrono::Utc::now().naive_utc();
        Self {
            id: None,
            project_id: 0,
            agent_id: 0,
            path_pattern: String::new(),
            exclusive: true,
            reason: String::new(),
            created_ts: now,
            expires_ts: now,
            released_ts: None,
        }
    }
}

// =============================================================================
// AgentLink
// =============================================================================

/// A contact link between two agents (possibly cross-project).
///
/// # Status Values
/// - "pending": Contact request sent, awaiting response
/// - "approved": Contact approved
/// - "blocked": Contact explicitly blocked
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentLink {
    pub id: Option<i64>,
    pub a_project_id: i64,
    pub a_agent_id: i64,
    pub b_project_id: i64,
    pub b_agent_id: i64,
    /// Status: "pending" | "approved" | "blocked"
    pub status: String,
    pub reason: String,
    pub created_ts: NaiveDateTime,
    pub updated_ts: NaiveDateTime,
    pub expires_ts: Option<NaiveDateTime>,
}

impl Default for AgentLink {
    fn default() -> Self {
        let now = chrono::Utc::now().naive_utc();
        Self {
            id: None,
            a_project_id: 0,
            a_agent_id: 0,
            b_project_id: 0,
            b_agent_id: 0,
            status: "pending".to_string(),
            reason: String::new(),
            created_ts: now,
            updated_ts: now,
            expires_ts: None,
        }
    }
}

// =============================================================================
// ProjectSiblingSuggestion
// =============================================================================

/// LLM-ranked suggestion for related projects.
///
/// # Status Values
/// - "suggested": Initial suggestion
/// - "confirmed": User confirmed relationship
/// - "dismissed": User dismissed suggestion
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSiblingSuggestion {
    pub id: Option<i64>,
    pub project_a_id: i64,
    pub project_b_id: i64,
    pub score: f64,
    /// Status: "suggested" | "confirmed" | "dismissed"
    pub status: String,
    pub rationale: String,
    pub created_ts: NaiveDateTime,
    pub evaluated_ts: NaiveDateTime,
    pub confirmed_ts: Option<NaiveDateTime>,
    pub dismissed_ts: Option<NaiveDateTime>,
}

// =============================================================================
// Consistency
// =============================================================================

/// Lightweight descriptor of a message for archive-DB consistency checking.
///
/// Populated from a DB query (in `DbPool::sample_recent_message_refs`) and
/// consumed by `mcp_agent_mail_storage::check_archive_consistency`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsistencyMessageRef {
    pub project_slug: String,
    pub message_id: i64,
    pub sender_name: String,
    pub subject: String,
    pub created_ts_iso: String,
}

/// Result of a startup consistency probe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsistencyReport {
    /// Total messages sampled from DB.
    pub sampled: usize,
    /// Messages whose canonical archive file was found.
    pub found: usize,
    /// Messages whose canonical archive file is missing.
    pub missing: usize,
    /// IDs of missing messages (capped at 20 for log brevity).
    pub missing_ids: Vec<i64>,
}

// =============================================================================
// Agent Name Validation
// =============================================================================

/// Valid adjectives for agent names (75 total).
///
/// IMPORTANT: Keep this list in lockstep with legacy Python `mcp_agent_mail.utils.ADJECTIVES`.
pub const VALID_ADJECTIVES: &[&str] = &[
    "red",
    "orange",
    "yellow",
    "pink",
    "black",
    "purple",
    "blue",
    "brown",
    "white",
    "green",
    "chartreuse",
    "lilac",
    "fuchsia",
    "azure",
    "amber",
    "coral",
    "crimson",
    "cyan",
    "gold",
    "golden",
    "gray",
    "indigo",
    "ivory",
    "jade",
    "lavender",
    "magenta",
    "maroon",
    "navy",
    "olive",
    "pearl",
    "rose",
    "ruby",
    "sage",
    "scarlet",
    "silver",
    "teal",
    "topaz",
    "violet",
    "cobalt",
    "copper",
    "bronze",
    "emerald",
    "sapphire",
    "turquoise",
    "beige",
    "tan",
    "cream",
    "peach",
    "plum",
    "sunny",
    "misty",
    "foggy",
    "stormy",
    "windy",
    "frosty",
    "dusty",
    "hazy",
    "cloudy",
    "rainy",
    "snowy",
    "icy",
    "mossy",
    "sandy",
    "swift",
    "quiet",
    "bold",
    "calm",
    "bright",
    "dark",
    "wild",
    "silent",
    "gentle",
    "rustic",
    "noble",
    "proud",
];

/// Valid nouns for agent names (132 total).
///
/// IMPORTANT: Keep this list in lockstep with legacy Python `mcp_agent_mail.utils.NOUNS`.
pub const VALID_NOUNS: &[&str] = &[
    // Geography / Nature
    "stone",
    "lake",
    "creek",
    "pond",
    "mountain",
    "hill",
    "snow",
    "castle",
    "river",
    "forest",
    "valley",
    "canyon",
    "meadow",
    "prairie",
    "desert",
    "island",
    "cliff",
    "cave",
    "glacier",
    "waterfall",
    "spring",
    "stream",
    "reef",
    "dune",
    "ridge",
    "peak",
    "gorge",
    "marsh",
    "brook",
    "glen",
    "grove",
    "fern",
    "hollow",
    "basin",
    "cove",
    "bay",
    "harbor",
    "coast",
    "shore",
    "bluff",
    "knoll",
    "summit",
    "plateau",
    // Animals - mammals
    "dog",
    "cat",
    "bear",
    "fox",
    "wolf",
    "deer",
    "elk",
    "moose",
    "otter",
    "beaver",
    "badger",
    "lynx",
    "puma",
    "squirrel",
    "rabbit",
    "hare",
    "mouse",
    "mink",
    "seal",
    "horse",
    "lion",
    "tiger",
    "panther",
    "leopard",
    "jaguar",
    "coyote",
    "bison",
    "ox",
    // Animals - birds
    "hawk",
    "eagle",
    "owl",
    "falcon",
    "raven",
    "heron",
    "crane",
    "finch",
    "robin",
    "sparrow",
    "duck",
    "goose",
    "swan",
    "dove",
    "wren",
    "jay",
    "lark",
    "kite",
    "condor",
    "osprey",
    "pelican",
    "gull",
    "tern",
    "stork",
    "ibis",
    "cardinal",
    "oriole",
    "thrush",
    // Animals - fish/reptiles
    "trout",
    "salmon",
    "bass",
    "pike",
    "carp",
    "turtle",
    "frog",
    // Trees/Plants
    "pine",
    "oak",
    "maple",
    "birch",
    "cedar",
    "willow",
    "aspen",
    "elm",
    "orchid",
    "lotus",
    "ivy",
    // Structures
    "tower",
    "bridge",
    "forge",
    "mill",
    "barn",
    "gate",
    "anchor",
    "lantern",
    "beacon",
    "compass",
    "horizon",
    "spire",
    "chapel",
    "citadel",
    "fortress",
];

/// Normalize a user-provided agent name; return `None` if nothing remains.
///
/// Mirrors legacy Python `sanitize_agent_name()`:
/// - `value.strip()`
/// - Remove all non `[A-Za-z0-9]` characters
/// - Truncate to max length 128
#[must_use]
pub fn sanitize_agent_name(value: &str) -> Option<String> {
    let mut cleaned: String = value
        .trim()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect();

    if cleaned.is_empty() {
        return None;
    }

    if cleaned.len() > 128 {
        cleaned.truncate(128);
    }

    Some(cleaned)
}

/// Precomputed set of all 9,900 valid lowercased agent names for O(1) lookup.
///
/// Initialized on first access. 75 adjectives × 132 nouns ≈ 300 KB.
fn valid_names_set() -> &'static std::collections::HashSet<String> {
    static SET: std::sync::OnceLock<std::collections::HashSet<String>> = std::sync::OnceLock::new();
    SET.get_or_init(|| {
        let mut set =
            std::collections::HashSet::with_capacity(VALID_ADJECTIVES.len() * VALID_NOUNS.len());
        for adj in VALID_ADJECTIVES {
            for noun in VALID_NOUNS {
                set.insert(format!("{adj}{noun}"));
            }
        }
        set
    })
}

/// Validates that an agent name follows the adjective+noun pattern.
///
/// Uses a precomputed `HashSet` of all 9,900 valid names for O(1) lookup,
/// replacing the previous O(75×132) linear scan.
///
/// # Examples
/// ```
/// use mcp_agent_mail_core::is_valid_agent_name;
///
/// assert!(is_valid_agent_name("GreenLake"));
/// assert!(is_valid_agent_name("blueDog"));
/// assert!(!is_valid_agent_name("BackendHarmonizer"));
/// ```
#[must_use]
pub fn is_valid_agent_name(name: &str) -> bool {
    valid_names_set().contains(&name.to_lowercase())
}

// ──────────────────────────────────────────────────────────────────────
// Agent name mistake detection (Python parity)
// ──────────────────────────────────────────────────────────────────────

/// Known program names that should go in the `program` parameter, not `name`.
///
/// Case-insensitive exact match after trim.
pub const KNOWN_PROGRAM_NAMES: &[&str] = &[
    "claude-code",
    "claude",
    "codex-cli",
    "codex",
    "cursor",
    "windsurf",
    "cline",
    "aider",
    "copilot",
    "github-copilot",
    "gemini-cli",
    "gemini",
    "opencode",
    "vscode",
    "neovim",
    "vim",
    "emacs",
    "zed",
    "continue",
];

/// Model name patterns that should go in the `model` parameter, not `name`.
///
/// Case-insensitive substring match after trim.
pub const MODEL_NAME_PATTERNS: &[&str] = &[
    "gpt-",
    "gpt4",
    "gpt3",
    "claude-",
    "opus",
    "sonnet",
    "haiku",
    "gemini-",
    "llama",
    "mistral",
    "codestral",
    "o1-",
    "o3-",
];

/// Broadcast-like tokens that should not be used as agent names.
pub const BROADCAST_TOKENS: &[&str] = &["all", "everyone", "broadcast", "*"];

/// Keywords that indicate descriptive/role-style names rather than random adjective+noun names.
pub const DESCRIPTIVE_NAME_KEYWORDS: &[&str] = &[
    "dev",
    "developer",
    "agent",
    "worker",
    "assistant",
    "helper",
    "bot",
    "user",
    "manager",
    "lead",
    "engineer",
];

/// Check if a value looks like a known program name.
#[must_use]
pub fn looks_like_program_name(value: &str) -> bool {
    let v = value.to_lowercase();
    let v = v.trim();
    KNOWN_PROGRAM_NAMES.contains(&v)
}

/// Check if a value looks like a model name.
#[must_use]
pub fn looks_like_model_name(value: &str) -> bool {
    let v = value.to_lowercase();
    let v = v.trim();
    MODEL_NAME_PATTERNS.iter().any(|p| v.contains(p))
}

/// Check if a value looks like an email address.
#[must_use]
pub fn looks_like_email(value: &str) -> bool {
    value.contains('@')
        && value
            .split('@')
            .next_back()
            .is_some_and(|s| s.contains('.'))
}

/// Check if a value looks like an attempt to broadcast to all agents.
#[must_use]
pub fn looks_like_broadcast(value: &str) -> bool {
    let v = value.to_lowercase();
    let v = v.trim();
    BROADCAST_TOKENS.contains(&v)
}

/// Check if a value looks like a descriptive role name.
#[must_use]
pub fn looks_like_descriptive_name(value: &str) -> bool {
    let v = value.to_lowercase();
    DESCRIPTIVE_NAME_KEYWORDS.iter().any(|kw| v.contains(kw))
}

/// Check if a value looks like a Unix username (e.g. from `$USER`).
///
/// A unix username is all-lowercase, alphanumeric, 2-16 chars, and NOT a
/// known adjective or noun from the agent name vocabulary.
#[must_use]
pub fn looks_like_unix_username(value: &str) -> bool {
    let v = value.trim();
    if v.is_empty() {
        return false;
    }
    // Agent names are PascalCase; unix usernames are all lowercase
    if v.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && (2..=16).contains(&v.len())
    {
        // If it matches a known adjective or noun, it's not a username
        let lower = v.to_lowercase();
        if VALID_ADJECTIVES.iter().any(|a| a.to_lowercase() == lower) {
            return false;
        }
        if VALID_NOUNS.iter().any(|n| n.to_lowercase() == lower) {
            return false;
        }
        return true;
    }
    false
}

/// Detect common mistakes when agents provide invalid agent names.
///
/// Returns `Some((mistake_type, helpful_message))` or `None` if no obvious
/// mistake detected.  Only the first matching category is returned.
///
/// Categories checked (in order):
/// 1. `PROGRAM_NAME_AS_AGENT` — value is a known program name
/// 2. `MODEL_NAME_AS_AGENT`  — value contains a model name pattern
/// 3. `EMAIL_AS_AGENT` — value looks like an email address
/// 4. `BROADCAST_ATTEMPT` — value is a broadcast token
/// 5. `DESCRIPTIVE_NAME` — value looks like a descriptive role name
/// 6. `UNIX_USERNAME_AS_AGENT` — value looks like a Unix username
#[must_use]
pub fn detect_agent_name_mistake(value: &str) -> Option<(&'static str, String)> {
    if looks_like_program_name(value) {
        return Some((
            "PROGRAM_NAME_AS_AGENT",
            format!(
                "'{value}' looks like a program name, not an agent name. \
                 Agent names must be adjective+noun combinations like 'BlueLake' or 'GreenCastle'. \
                 Use the 'program' parameter for program names, and omit 'name' to auto-generate a valid agent name."
            ),
        ));
    }
    if looks_like_model_name(value) {
        return Some((
            "MODEL_NAME_AS_AGENT",
            format!(
                "'{value}' looks like a model name, not an agent name. \
                 Agent names must be adjective+noun combinations like 'RedStone' or 'PurpleBear'. \
                 Use the 'model' parameter for model names, and omit 'name' to auto-generate a valid agent name."
            ),
        ));
    }
    if looks_like_email(value) {
        return Some((
            "EMAIL_AS_AGENT",
            format!(
                "'{value}' looks like an email address. Agent names are simple identifiers like 'BlueDog', \
                 not email addresses. Check the 'to' parameter format."
            ),
        ));
    }
    if looks_like_broadcast(value) {
        return Some((
            "BROADCAST_ATTEMPT",
            format!(
                "'{value}' looks like a broadcast attempt. Agent Mail doesn't support broadcasting to all agents. \
                 List specific recipient agent names in the 'to' parameter."
            ),
        ));
    }
    if looks_like_descriptive_name(value) {
        return Some((
            "DESCRIPTIVE_NAME",
            format!(
                "'{value}' looks like a descriptive role name. Agent names must be randomly generated \
                 adjective+noun combinations like 'WhiteMountain' or 'BrownCreek', NOT descriptive of the agent's task. \
                 Omit the 'name' parameter to auto-generate a valid name."
            ),
        ));
    }
    if looks_like_unix_username(value) {
        return Some((
            "UNIX_USERNAME_AS_AGENT",
            format!(
                "'{value}' looks like a Unix username (possibly from $USER environment variable). \
                 Agent names must be adjective+noun combinations like 'BlueLake' or 'GreenCastle'. \
                 When you called register_agent, the system likely auto-generated a valid name for you. \
                 To find your actual agent name, check the response from register_agent or use \
                 resource://agents/{{project_key}} to list all registered agents in this project."
            ),
        ));
    }
    None
}

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let mut out: String = first.to_uppercase().collect();
    out.push_str(chars.as_str());
    out
}

/// Generates a random valid agent name.
#[must_use]
pub fn generate_agent_name() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    // Simple pseudo-random using system time
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());

    let mut hasher = DefaultHasher::new();
    seed.hash(&mut hasher);
    let hash = hasher.finish();

    let adj_idx = usize::try_from(hash % (VALID_ADJECTIVES.len() as u64)).unwrap_or(0);
    let noun_idx = usize::try_from((hash >> 32) % (VALID_NOUNS.len() as u64)).unwrap_or(0);

    let adj = VALID_ADJECTIVES[adj_idx];
    let noun = VALID_NOUNS[noun_idx];

    // Capitalize first letter of each (UTF-8 safe).
    let adj_cap = capitalize_first(adj);
    let noun_cap = capitalize_first(noun);

    format!("{adj_cap}{noun_cap}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_agent_names() {
        assert!(is_valid_agent_name("GreenLake"));
        assert!(is_valid_agent_name("greenlake"));
        assert!(is_valid_agent_name("GREENLAKE"));
        assert!(is_valid_agent_name("BlueDog"));
        assert!(is_valid_agent_name("CrimsonGorge"));
        assert!(is_valid_agent_name("FuchsiaForge"));
        // Newly added nouns (fern, horizon, orchid, duck)
        assert!(is_valid_agent_name("ScarletFern"));
        assert!(is_valid_agent_name("CrimsonFern"));
        assert!(is_valid_agent_name("VioletHorizon"));
        assert!(is_valid_agent_name("CrimsonOrchid"));
        assert!(is_valid_agent_name("GreenDuck"));
        // Extended vocabulary (yellow, golden, squirrel, etc.)
        assert!(is_valid_agent_name("YellowSquirrel"));
        assert!(is_valid_agent_name("GoldenFalcon"));
        assert!(is_valid_agent_name("GoldenEagle"));
        assert!(is_valid_agent_name("YellowFinch"));
        assert!(is_valid_agent_name("SnowyOwl"));
        assert!(is_valid_agent_name("IcyPeak"));
        assert!(is_valid_agent_name("NobleLion"));
        assert!(is_valid_agent_name("ProudPanther"));
        assert!(is_valid_agent_name("SwiftRabbit"));
        assert!(is_valid_agent_name("SilentSwan"));
    }

    #[test]
    fn test_invalid_agent_names() {
        assert!(!is_valid_agent_name("BackendHarmonizer"));
        assert!(!is_valid_agent_name("DatabaseMigrator"));
        assert!(!is_valid_agent_name("Alice"));
        assert!(!is_valid_agent_name(""));
    }

    #[test]
    fn test_generate_agent_name() {
        let name = generate_agent_name();
        assert!(
            is_valid_agent_name(&name),
            "Generated name should be valid: {name}"
        );
    }

    #[test]
    fn test_sanitize_agent_name() {
        assert_eq!(
            sanitize_agent_name("  BlueLake "),
            Some("BlueLake".to_string())
        );
        assert_eq!(
            sanitize_agent_name("Blue Lake!"),
            Some("BlueLake".to_string())
        );
        assert_eq!(sanitize_agent_name("$$$"), None);
        assert_eq!(sanitize_agent_name(""), None);
    }

    // =========================================================================
    // Agent name mistake detection tests (br-2dp8f: T3.1)
    // =========================================================================

    #[test]
    fn program_name_detection_all_19() {
        // Exact match against Python's _KNOWN_PROGRAM_NAMES frozenset (19 entries)
        let expected = [
            "claude-code",
            "claude",
            "codex-cli",
            "codex",
            "cursor",
            "windsurf",
            "cline",
            "aider",
            "copilot",
            "github-copilot",
            "gemini-cli",
            "gemini",
            "opencode",
            "vscode",
            "neovim",
            "vim",
            "emacs",
            "zed",
            "continue",
        ];
        assert_eq!(
            KNOWN_PROGRAM_NAMES.len(),
            19,
            "must have exactly 19 program names"
        );
        for name in &expected {
            assert!(
                looks_like_program_name(name),
                "'{name}' should be detected as a program name"
            );
        }
    }

    #[test]
    fn program_name_detection_case_insensitive() {
        assert!(looks_like_program_name("Claude-Code"));
        assert!(looks_like_program_name("CLAUDE-CODE"));
        assert!(looks_like_program_name("Cursor"));
        assert!(looks_like_program_name("CURSOR"));
        assert!(looks_like_program_name("  claude-code  ")); // with whitespace
    }

    #[test]
    fn program_name_detection_non_matches() {
        assert!(!looks_like_program_name("BlueLake")); // valid agent name
        assert!(!looks_like_program_name("my-tool")); // unknown tool
        assert!(!looks_like_program_name(""));
        assert!(!looks_like_program_name("claude-code-2")); // not exact match
    }

    #[test]
    fn model_name_detection_all_13() {
        // Exact match against Python's _MODEL_NAME_PATTERNS tuple (13 entries)
        let expected = [
            "gpt-",
            "gpt4",
            "gpt3",
            "claude-",
            "opus",
            "sonnet",
            "haiku",
            "gemini-",
            "llama",
            "mistral",
            "codestral",
            "o1-",
            "o3-",
        ];
        assert_eq!(
            MODEL_NAME_PATTERNS.len(),
            13,
            "must have exactly 13 model patterns"
        );
        for pattern in &expected {
            assert!(
                MODEL_NAME_PATTERNS.contains(pattern),
                "'{pattern}' must be in MODEL_NAME_PATTERNS"
            );
        }
    }

    #[test]
    fn model_name_detection_substring_match() {
        // These should all be detected as model names (substring match)
        assert!(looks_like_model_name("gpt-4-turbo"));
        assert!(looks_like_model_name("gpt4o"));
        assert!(looks_like_model_name("gpt3.5"));
        assert!(looks_like_model_name("claude-opus-4-6"));
        assert!(looks_like_model_name("claude-sonnet-4-5"));
        assert!(looks_like_model_name("claude-haiku-4-5"));
        assert!(looks_like_model_name("opus"));
        assert!(looks_like_model_name("sonnet"));
        assert!(looks_like_model_name("haiku"));
        assert!(looks_like_model_name("gemini-2.5-pro"));
        assert!(looks_like_model_name("llama-3.1-70b"));
        assert!(looks_like_model_name("mistral-large"));
        assert!(looks_like_model_name("codestral"));
        assert!(looks_like_model_name("o1-mini"));
        assert!(looks_like_model_name("o3-mini"));
    }

    #[test]
    fn model_name_detection_case_insensitive() {
        assert!(looks_like_model_name("GPT-4"));
        assert!(looks_like_model_name("Claude-Opus-4.6"));
        assert!(looks_like_model_name("SONNET"));
        assert!(looks_like_model_name("  opus  ")); // with whitespace
    }

    #[test]
    fn model_name_detection_non_matches() {
        assert!(!looks_like_model_name("BlueLake")); // valid agent name
        assert!(!looks_like_model_name("my-model")); // unknown model
        assert!(!looks_like_model_name(""));
        assert!(!looks_like_model_name("cursor")); // program name, not model
    }

    #[test]
    fn detect_mistake_program_name() {
        let result = detect_agent_name_mistake("claude-code");
        assert!(result.is_some());
        let (kind, msg) = result.unwrap();
        assert_eq!(kind, "PROGRAM_NAME_AS_AGENT");
        assert!(msg.contains("looks like a program name"), "message: {msg}");
        assert!(
            msg.contains("'claude-code'"),
            "message should contain the value: {msg}"
        );
        assert!(
            msg.contains("BlueLake"),
            "message should suggest valid name: {msg}"
        );
        assert!(
            msg.contains("Use the 'program' parameter"),
            "message should suggest fix: {msg}"
        );
    }

    #[test]
    fn detect_mistake_model_name() {
        let result = detect_agent_name_mistake("gpt-4-turbo");
        assert!(result.is_some());
        let (kind, msg) = result.unwrap();
        assert_eq!(kind, "MODEL_NAME_AS_AGENT");
        assert!(msg.contains("looks like a model name"), "message: {msg}");
        assert!(
            msg.contains("'gpt-4-turbo'"),
            "message should contain the value: {msg}"
        );
        assert!(
            msg.contains("RedStone"),
            "message should suggest valid name: {msg}"
        );
        assert!(
            msg.contains("Use the 'model' parameter"),
            "message should suggest fix: {msg}"
        );
    }

    #[test]
    fn email_detection_matches_python_rule() {
        assert!(looks_like_email("alice@example.com"));
        assert!(looks_like_email("ops@alerts.dev"));
        assert!(!looks_like_email("alice@localhost"));
        assert!(!looks_like_email("BlueLake"));
        assert!(!looks_like_email("not-an-email@"));
    }

    #[test]
    fn broadcast_detection_exact_tokens() {
        for token in BROADCAST_TOKENS {
            assert!(looks_like_broadcast(token), "token {token:?} should match");
            assert!(looks_like_broadcast(&token.to_uppercase()));
            assert!(looks_like_broadcast(&format!("  {token}  ")));
        }
        assert!(!looks_like_broadcast("@all"));
        assert!(!looks_like_broadcast("@everyone"));
        assert!(!looks_like_broadcast("BlueLake"));
    }

    #[test]
    fn descriptive_name_detection_keyword_contains() {
        assert!(looks_like_descriptive_name("BackendDeveloper"));
        assert!(looks_like_descriptive_name("agent_worker"));
        assert!(looks_like_descriptive_name("LeadCoordinator"));
        assert!(!looks_like_descriptive_name("BlueLake"));
    }

    #[test]
    fn detect_mistake_email() {
        let (kind, msg) = detect_agent_name_mistake("alice@example.com")
            .expect("email-shaped value should be detected");
        assert_eq!(kind, "EMAIL_AS_AGENT");
        assert_eq!(
            msg,
            "'alice@example.com' looks like an email address. Agent names are simple identifiers like 'BlueDog', \
             not email addresses. Check the 'to' parameter format."
        );
    }

    #[test]
    fn detect_mistake_broadcast() {
        let (kind, msg) =
            detect_agent_name_mistake("everyone").expect("broadcast token should be detected");
        assert_eq!(kind, "BROADCAST_ATTEMPT");
        assert_eq!(
            msg,
            "'everyone' looks like a broadcast attempt. Agent Mail doesn't support broadcasting to all agents. \
             List specific recipient agent names in the 'to' parameter."
        );
    }

    #[test]
    fn detect_mistake_descriptive_name() {
        let (kind, msg) = detect_agent_name_mistake("BackendEngineer")
            .expect("descriptive role names should be detected");
        assert_eq!(kind, "DESCRIPTIVE_NAME");
        assert_eq!(
            msg,
            "'BackendEngineer' looks like a descriptive role name. Agent names must be randomly generated \
             adjective+noun combinations like 'WhiteMountain' or 'BrownCreek', NOT descriptive of the agent's task. \
             Omit the 'name' parameter to auto-generate a valid name."
        );
    }

    #[test]
    fn detect_mistake_valid_name_returns_none() {
        assert!(detect_agent_name_mistake("BlueLake").is_none());
        assert!(detect_agent_name_mistake("RedFox").is_none());
    }

    #[test]
    fn detect_mistake_unknown_invalid_returns_none() {
        // Not a known program, model, email, broadcast, descriptive, or unix username
        assert!(detect_agent_name_mistake("my-random-thing").is_none()); // has hyphen
        assert!(detect_agent_name_mistake("FooBar123").is_none()); // mixed case
    }

    // ── Unix username detection (br-3oyfw: T3.3) ──

    #[test]
    fn unix_username_detection_typical_names() {
        assert!(looks_like_unix_username("john"));
        assert!(looks_like_unix_username("alice"));
        assert!(looks_like_unix_username("ubuntu"));
        assert!(looks_like_unix_username("jeff"));
        assert!(looks_like_unix_username("root"));
        assert!(looks_like_unix_username("admin2"));
        assert!(looks_like_unix_username("ab")); // minimum 2 chars
    }

    #[test]
    fn unix_username_not_adjectives_or_nouns() {
        // Known adjectives should NOT be flagged as unix usernames
        assert!(!looks_like_unix_username("red"));
        assert!(!looks_like_unix_username("blue"));
        assert!(!looks_like_unix_username("green"));
        assert!(!looks_like_unix_username("swift"));
        assert!(!looks_like_unix_username("calm"));
        // Known nouns should NOT be flagged
        assert!(!looks_like_unix_username("lake"));
        assert!(!looks_like_unix_username("dog"));
        assert!(!looks_like_unix_username("fox"));
        assert!(!looks_like_unix_username("hawk"));
        assert!(!looks_like_unix_username("castle"));
    }

    #[test]
    fn unix_username_not_mixed_case() {
        // PascalCase / mixed case is not a unix username
        assert!(!looks_like_unix_username("GreenLake"));
        assert!(!looks_like_unix_username("BlueDog"));
        assert!(!looks_like_unix_username("John"));
    }

    #[test]
    fn unix_username_length_bounds() {
        assert!(!looks_like_unix_username("a")); // too short (< 2)
        assert!(looks_like_unix_username("ab")); // minimum
        assert!(looks_like_unix_username("abcdefghijklmnop")); // 16 chars, maximum
        assert!(!looks_like_unix_username("abcdefghijklmnopq")); // 17 chars, too long
    }

    #[test]
    fn unix_username_rejects_non_alnum() {
        assert!(!looks_like_unix_username("john_doe")); // underscore
        assert!(!looks_like_unix_username("john-doe")); // hyphen
        assert!(!looks_like_unix_username("john.doe")); // dot
        assert!(!looks_like_unix_username("john@host")); // at sign
    }

    #[test]
    fn detect_mistake_unix_username() {
        let result = detect_agent_name_mistake("john");
        assert!(result.is_some());
        let (kind, msg) = result.unwrap();
        assert_eq!(kind, "UNIX_USERNAME_AS_AGENT");
        assert!(msg.contains("looks like a Unix username"), "message: {msg}");
        assert!(msg.contains("$USER"), "should mention $USER: {msg}");
        assert!(
            msg.contains("resource://agents/"),
            "should mention agents resource: {msg}"
        );
    }

    #[test]
    fn detect_message_exact_format_unix_username() {
        let (_, msg) = detect_agent_name_mistake("jeff").unwrap();
        assert_eq!(
            msg,
            "'jeff' looks like a Unix username (possibly from $USER environment variable). \
             Agent names must be adjective+noun combinations like 'BlueLake' or 'GreenCastle'. \
             When you called register_agent, the system likely auto-generated a valid name for you. \
             To find your actual agent name, check the response from register_agent or use \
             resource://agents/{project_key} to list all registered agents in this project."
        );
    }

    #[test]
    fn program_takes_priority_over_model() {
        // "claude" is both a program name and contains "claude" which looks model-like.
        // Program detection should fire first (exact match).
        let result = detect_agent_name_mistake("claude");
        assert!(result.is_some());
        assert_eq!(result.unwrap().0, "PROGRAM_NAME_AS_AGENT");
    }

    #[test]
    fn detect_message_exact_format_program() {
        // Verify exact message format matches Python
        let (_, msg) = detect_agent_name_mistake("cursor").unwrap();
        assert_eq!(
            msg,
            "'cursor' looks like a program name, not an agent name. \
             Agent names must be adjective+noun combinations like 'BlueLake' or 'GreenCastle'. \
             Use the 'program' parameter for program names, and omit 'name' to auto-generate a valid agent name."
        );
    }

    #[test]
    fn detect_message_exact_format_model() {
        // Verify exact message format matches Python
        let (_, msg) = detect_agent_name_mistake("opus").unwrap();
        assert_eq!(
            msg,
            "'opus' looks like a model name, not an agent name. \
             Agent names must be adjective+noun combinations like 'RedStone' or 'PurpleBear'. \
             Use the 'model' parameter for model names, and omit 'name' to auto-generate a valid agent name."
        );
    }

    // =========================================================================
    // br-3h13.1.1: Serialize/deserialize roundtrip tests for all model structs
    // =========================================================================

    #[test]
    fn test_project_serde_roundtrip() {
        let p = Project {
            id: Some(42),
            slug: "my-project".into(),
            human_key: "/data/projects/my-project".into(),
            created_at: chrono::Utc::now().naive_utc(),
        };
        let json = serde_json::to_string(&p).unwrap();
        let p2: Project = serde_json::from_str(&json).unwrap();
        assert_eq!(p.id, p2.id);
        assert_eq!(p.slug, p2.slug);
        assert_eq!(p.human_key, p2.human_key);
        assert_eq!(p.created_at, p2.created_at);
    }

    #[test]
    fn test_product_serde_roundtrip() {
        let p = Product {
            id: Some(1),
            product_uid: "prod-abc".into(),
            name: "My Product".into(),
            created_at: chrono::Utc::now().naive_utc(),
        };
        let json = serde_json::to_string(&p).unwrap();
        let p2: Product = serde_json::from_str(&json).unwrap();
        assert_eq!(p.id, p2.id);
        assert_eq!(p.product_uid, p2.product_uid);
        assert_eq!(p.name, p2.name);
    }

    #[test]
    fn test_product_project_link_serde_roundtrip() {
        let link = ProductProjectLink {
            id: Some(5),
            product_id: 1,
            project_id: 2,
            created_at: chrono::Utc::now().naive_utc(),
        };
        let json = serde_json::to_string(&link).unwrap();
        let link2: ProductProjectLink = serde_json::from_str(&json).unwrap();
        assert_eq!(link.product_id, link2.product_id);
        assert_eq!(link.project_id, link2.project_id);
    }

    #[test]
    fn test_agent_serde_roundtrip() {
        let a = Agent {
            id: Some(10),
            project_id: 1,
            name: "GreenLake".into(),
            program: "claude-code".into(),
            model: "opus-4.6".into(),
            task_description: "Testing serde".into(),
            attachments_policy: "inline".into(),
            contact_policy: "contacts_only".into(),
            ..Agent::default()
        };
        let json = serde_json::to_string(&a).unwrap();
        let a2: Agent = serde_json::from_str(&json).unwrap();
        assert_eq!(a.name, a2.name);
        assert_eq!(a.program, a2.program);
        assert_eq!(a.model, a2.model);
        assert_eq!(a.task_description, a2.task_description);
        assert_eq!(a.attachments_policy, a2.attachments_policy);
        assert_eq!(a.contact_policy, a2.contact_policy);
    }

    #[test]
    fn test_agent_default_values() {
        let a = Agent::default();
        assert_eq!(a.attachments_policy, "auto");
        assert_eq!(a.contact_policy, "auto");
        assert!(a.id.is_none());
        assert_eq!(a.project_id, 0);
    }

    #[test]
    fn test_message_serde_roundtrip() {
        let m = Message {
            id: Some(100),
            project_id: 1,
            sender_id: 2,
            thread_id: Some("FEAT-42".into()),
            subject: "Hello world".into(),
            body_md: "## Title\n\nBody text.".into(),
            importance: "high".into(),
            ack_required: true,
            attachments: "[{\"name\":\"file.txt\"}]".into(),
            ..Message::default()
        };
        let json = serde_json::to_string(&m).unwrap();
        let m2: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(m.id, m2.id);
        assert_eq!(m.thread_id, m2.thread_id);
        assert_eq!(m.subject, m2.subject);
        assert_eq!(m.body_md, m2.body_md);
        assert_eq!(m.importance, m2.importance);
        assert_eq!(m.ack_required, m2.ack_required);
        assert_eq!(m.attachments, m2.attachments);
    }

    #[test]
    fn test_message_default_values() {
        let m = Message::default();
        assert!(m.id.is_none());
        assert_eq!(m.importance, "normal");
        assert!(!m.ack_required);
        assert!(m.thread_id.is_none());
        assert_eq!(m.attachments, "[]");
    }

    #[test]
    fn test_message_recipient_serde_roundtrip() {
        let r = MessageRecipient {
            message_id: 1,
            agent_id: 2,
            kind: "cc".into(),
            read_ts: Some(chrono::Utc::now().naive_utc()),
            ack_ts: None,
        };
        let json = serde_json::to_string(&r).unwrap();
        let r2: MessageRecipient = serde_json::from_str(&json).unwrap();
        assert_eq!(r.kind, r2.kind);
        assert!(r2.read_ts.is_some());
        assert!(r2.ack_ts.is_none());
    }

    #[test]
    fn test_message_recipient_default_values() {
        let r = MessageRecipient::default();
        assert_eq!(r.kind, "to");
        assert!(r.read_ts.is_none());
        assert!(r.ack_ts.is_none());
    }

    #[test]
    fn test_file_reservation_serde_roundtrip() {
        let f = FileReservation {
            id: Some(7),
            project_id: 1,
            agent_id: 3,
            path_pattern: "src/**/*.rs".into(),
            exclusive: true,
            reason: "editing source".into(),
            released_ts: None,
            ..FileReservation::default()
        };
        let json = serde_json::to_string(&f).unwrap();
        let f2: FileReservation = serde_json::from_str(&json).unwrap();
        assert_eq!(f.path_pattern, f2.path_pattern);
        assert_eq!(f.exclusive, f2.exclusive);
        assert_eq!(f.reason, f2.reason);
        assert!(f2.released_ts.is_none());
    }

    #[test]
    fn test_file_reservation_default_values() {
        let f = FileReservation::default();
        assert!(f.exclusive);
        assert!(f.released_ts.is_none());
    }

    #[test]
    fn test_agent_link_serde_roundtrip() {
        let link = AgentLink {
            id: Some(3),
            a_project_id: 1,
            a_agent_id: 10,
            b_project_id: 2,
            b_agent_id: 20,
            status: "approved".into(),
            reason: "collaboration".into(),
            expires_ts: Some(chrono::Utc::now().naive_utc()),
            ..AgentLink::default()
        };
        let json = serde_json::to_string(&link).unwrap();
        let link2: AgentLink = serde_json::from_str(&json).unwrap();
        assert_eq!(link.status, link2.status);
        assert_eq!(link.reason, link2.reason);
        assert!(link2.expires_ts.is_some());
    }

    #[test]
    fn test_agent_link_default_values() {
        let link = AgentLink::default();
        assert_eq!(link.status, "pending");
        assert!(link.expires_ts.is_none());
    }

    #[test]
    fn test_consistency_report_serde_roundtrip() {
        let r = ConsistencyReport {
            sampled: 50,
            found: 48,
            missing: 2,
            missing_ids: vec![101, 203],
        };
        let json = serde_json::to_string(&r).unwrap();
        let r2: ConsistencyReport = serde_json::from_str(&json).unwrap();
        assert_eq!(r.sampled, r2.sampled);
        assert_eq!(r.found, r2.found);
        assert_eq!(r.missing, r2.missing);
        assert_eq!(r.missing_ids, r2.missing_ids);
    }

    #[test]
    fn test_consistency_message_ref_serde_roundtrip() {
        let mr = ConsistencyMessageRef {
            project_slug: "my-proj".into(),
            message_id: 42,
            sender_name: "RedFox".into(),
            subject: "Test subject".into(),
            created_ts_iso: "2026-01-15T10:30:00Z".into(),
        };
        let json = serde_json::to_string(&mr).unwrap();
        let mr2: ConsistencyMessageRef = serde_json::from_str(&json).unwrap();
        assert_eq!(mr.project_slug, mr2.project_slug);
        assert_eq!(mr.message_id, mr2.message_id);
        assert_eq!(mr.sender_name, mr2.sender_name);
    }

    #[test]
    fn test_message_with_none_thread_id_serde() {
        let m = Message {
            thread_id: None,
            ..Message::default()
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"thread_id\":null"));
        let m2: Message = serde_json::from_str(&json).unwrap();
        assert!(m2.thread_id.is_none());
    }

    #[test]
    fn test_sanitize_agent_name_long_input() {
        let long = "A".repeat(200);
        let result = sanitize_agent_name(&long);
        assert!(result.is_some());
        assert_eq!(result.unwrap().len(), 128);
    }

    #[test]
    fn test_sanitize_agent_name_special_chars_only() {
        assert_eq!(sanitize_agent_name("!@#$%^&*()"), None);
        assert_eq!(sanitize_agent_name("   "), None);
    }

    #[test]
    fn test_valid_name_count() {
        // 75 adjectives x 132 nouns = 9,900 valid names
        assert_eq!(VALID_ADJECTIVES.len(), 75);
        assert_eq!(VALID_NOUNS.len(), 132);
        assert_eq!(valid_names_set().len(), 75 * 132);
    }
}
