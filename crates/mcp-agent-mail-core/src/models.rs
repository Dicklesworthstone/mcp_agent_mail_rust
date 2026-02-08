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
/// - 62 adjectives × 69 nouns = 4,278 valid combinations
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

/// Valid adjectives for agent names (62 total).
///
/// IMPORTANT: Keep this list in lockstep with legacy Python `mcp_agent_mail.utils.ADJECTIVES`.
pub const VALID_ADJECTIVES: &[&str] = &[
    "red",
    "orange",
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
];

/// Valid nouns for agent names (69 total).
///
/// IMPORTANT: Keep this list in lockstep with legacy Python `mcp_agent_mail.utils.NOUNS`.
pub const VALID_NOUNS: &[&str] = &[
    "stone",
    "lake",
    "dog",
    "creek",
    "pond",
    "cat",
    "bear",
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
    "hollow",
    "basin",
    "cove",
    "bay",
    "harbor",
    "fox",
    "wolf",
    "hawk",
    "eagle",
    "owl",
    "deer",
    "elk",
    "moose",
    "falcon",
    "raven",
    "heron",
    "crane",
    "otter",
    "beaver",
    "badger",
    "finch",
    "robin",
    "sparrow",
    "lynx",
    "puma",
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

/// Validates that an agent name follows the adjective+noun pattern.
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
    let lower = name.to_lowercase();

    for adj in VALID_ADJECTIVES {
        if let Some(rest) = lower.strip_prefix(adj) {
            if VALID_NOUNS.contains(&rest) {
                return true;
            }
        }
    }
    false
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
}
