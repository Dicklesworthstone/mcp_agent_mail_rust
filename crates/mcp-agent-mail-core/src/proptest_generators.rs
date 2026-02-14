//! Property-based test generators for core models.
//!
//! Provides `proptest` strategies for `ProjectRow`, `AgentRow`, `InboxStatsRow`,
//! subjects, body markdown, and thread IDs. All generated values satisfy the
//! domain constraints documented on each type.

use proptest::prelude::*;

use crate::models::{VALID_ADJECTIVES, VALID_NOUNS};

// ─── Configuration ───────────────────────────────────────────────────────────

/// Shared proptest configuration: 1 000 cases, generous shrink budget.
#[must_use]
pub fn proptest_config() -> ProptestConfig {
    ProptestConfig {
        cases: 1000,
        max_shrink_iters: 5000,
        ..ProptestConfig::default()
    }
}

// ─── Leaf strategies ─────────────────────────────────────────────────────────

/// Strategy for a valid project slug: `[a-z0-9_]{1,20}`.
pub fn arb_slug() -> impl Strategy<Value = String> {
    proptest::string::string_regex("[a-z0-9_]{1,20}").expect("valid regex")
}

/// Strategy for a valid agent name (adjective + noun, title-cased).
///
/// Picks one adjective and one noun from the canonical lists and title-cases
/// both halves, matching the `AdjectiveNoun` convention (e.g. `"GreenLake"`).
pub fn arb_agent_name() -> impl Strategy<Value = String> {
    let adj_idx = 0..VALID_ADJECTIVES.len();
    let noun_idx = 0..VALID_NOUNS.len();
    (adj_idx, noun_idx).prop_map(|(ai, ni)| {
        let adj = VALID_ADJECTIVES[ai];
        let noun = VALID_NOUNS[ni];
        format!("{}{}", title_case(adj), title_case(noun))
    })
}

/// Strategy for a valid `thread_id`: `^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$`.
pub fn arb_thread_id() -> impl Strategy<Value = String> {
    proptest::string::string_regex("[A-Za-z0-9][A-Za-z0-9._-]{0,127}").expect("valid regex")
}

/// Strategy for a message subject: 0–200 arbitrary unicode characters.
pub fn arb_subject() -> impl Strategy<Value = String> {
    proptest::string::string_regex(".{0,200}").expect("valid regex")
}

/// Strategy for a message body (markdown): 0–10 000 arbitrary characters.
///
/// Uses `any::<String>()` filtered to the length bound rather than a regex
/// to avoid pathological generation of 10k-char regexes.
pub fn arb_body_md() -> impl Strategy<Value = String> {
    proptest::collection::vec(any::<char>(), 0..=10_000)
        .prop_map(|chars| chars.into_iter().collect::<String>())
}

/// Strategy for `created_at` / timestamp fields (microseconds since epoch).
/// Range: `0..=i64::MAX` — always non-negative.
pub fn arb_timestamp_micros() -> impl Strategy<Value = i64> {
    0..=i64::MAX
}

// ─── Composite strategies ────────────────────────────────────────────────────

/// Strategy for `mcp_agent_mail_db::models::ProjectRow`-shaped tuples.
///
/// Returns `(Option<i64>, String, String, i64)` — `(id, slug, human_key, created_at)`.
/// Callers construct the actual `ProjectRow` from these fields (avoids coupling
/// this crate to the db crate's derive macros).
pub fn arb_project_row_fields() -> impl Strategy<Value = (Option<i64>, String, String, i64)> {
    let id = proptest::option::of(1..=10_000i64);
    let slug = arb_slug();
    let created_at = arb_timestamp_micros();
    (id, slug, created_at).prop_map(|(id, slug, ts)| {
        let human_key = format!("/data/{slug}");
        (id, slug, human_key, ts)
    })
}

/// Strategy for `AgentRow`-shaped tuples.
///
/// Returns `(Option<i64>, i64, String, String, String)` —
/// `(id, project_id, name, program, model)`.
pub fn arb_agent_row_fields() -> impl Strategy<Value = (Option<i64>, i64, String, String, String)> {
    let id = proptest::option::of(1..=10_000i64);
    let project_id = 1..=100i64;
    let name = arb_agent_name();
    let program = proptest::sample::select(vec!["claude-code", "cursor", "codex", "gemini-cli"]);
    let model = proptest::sample::select(vec![
        "claude-opus-4-6",
        "claude-sonnet-4-5",
        "gpt-4o",
        "o3",
        "gemini-2.5-pro",
    ]);
    (id, project_id, name, program, model)
        .prop_map(|(id, pid, n, p, m)| (id, pid, n, p.to_string(), m.to_string()))
}

/// Strategy for `InboxStatsRow`-shaped tuples.
///
/// Returns `(i64, i64, i64, i64, Option<i64>)` —
/// `(agent_id, total_count, unread_count, ack_pending_count, last_message_ts)`.
///
/// Invariant: `unread_count <= total_count` and `ack_pending_count <= total_count`.
pub fn arb_inbox_stats_fields() -> impl Strategy<Value = (i64, i64, i64, i64, Option<i64>)> {
    let agent_id = 1..=10_000i64;
    let total_count = 0..=100_000i64;
    (agent_id, total_count).prop_flat_map(|(aid, total)| {
        let unread = 0..=total;
        let ack_pending = 0..=total;
        let last_ts = proptest::option::of(0..=i64::MAX);
        (Just(aid), Just(total), unread, ack_pending, last_ts)
    })
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Title-case a word: first char uppercase, rest lowercase.
fn title_case(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map_or_else(String::new, |c| {
        let upper: String = c.to_uppercase().collect();
        upper + &chars.as_str().to_lowercase()
    })
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::is_valid_agent_name;

    proptest! {
        #![proptest_config(proptest_config())]

        /// All generated `ProjectRow` tuples have non-empty slug and human_key.
        #[test]
        fn proptest_project_row_valid(
            (id, slug, human_key, created_at) in arb_project_row_fields()
        ) {
            prop_assert!(!slug.is_empty(), "slug must be non-empty");
            prop_assert!(!human_key.is_empty(), "human_key must be non-empty");
            prop_assert!(human_key.starts_with("/data/"), "human_key must start with /data/");
            // slug chars: [a-z0-9_]{1,20}
            prop_assert!(slug.len() <= 20, "slug too long: {}", slug.len());
            prop_assert!(
                slug.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "slug has invalid chars: {slug}"
            );
            // id in valid range
            if let Some(id_val) = id {
                prop_assert!((1..=10_000).contains(&id_val));
            }
            // created_at is non-negative
            prop_assert!(created_at >= 0);
        }

        /// All generated agent names pass `is_valid_agent_name()`.
        #[test]
        fn proptest_agent_row_valid_name(
            (_id, _project_id, name, _program, _model) in arb_agent_row_fields()
        ) {
            prop_assert!(
                is_valid_agent_name(&name),
                "generated name failed validation: {name}"
            );
        }

        /// All inbox stats fields are non-negative, with unread/ack ≤ total.
        #[test]
        fn proptest_inbox_stats_non_negative(
            (agent_id, total, unread, ack_pending, last_ts) in arb_inbox_stats_fields()
        ) {
            prop_assert!(agent_id > 0, "agent_id must be positive");
            prop_assert!(total >= 0, "total_count must be non-negative");
            prop_assert!(unread >= 0, "unread_count must be non-negative");
            prop_assert!(ack_pending >= 0, "ack_pending_count must be non-negative");
            prop_assert!(unread <= total, "unread {unread} > total {total}");
            prop_assert!(ack_pending <= total, "ack_pending {ack_pending} > total {total}");
            if let Some(ts) = last_ts {
                prop_assert!(ts >= 0, "last_message_ts must be non-negative");
            }
        }

        /// All generated subjects are ≤ 200 characters.
        #[test]
        fn proptest_subject_length_bounded(subj in arb_subject()) {
            prop_assert!(
                subj.chars().count() <= 200,
                "subject too long: {} chars",
                subj.chars().count()
            );
        }

        /// All generated thread IDs match the documented format.
        #[test]
        fn proptest_thread_id_format(tid in arb_thread_id()) {
            prop_assert!(!tid.is_empty(), "thread_id must be non-empty");
            prop_assert!(
                tid.len() <= 128,
                "thread_id too long: {} bytes",
                tid.len()
            );
            let first = tid.chars().next().unwrap();
            prop_assert!(
                first.is_ascii_alphanumeric(),
                "thread_id must start with alphanumeric, got: {first}"
            );
            for ch in tid.chars().skip(1) {
                prop_assert!(
                    ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-',
                    "invalid char in thread_id: {ch}"
                );
            }
        }
    }
}
