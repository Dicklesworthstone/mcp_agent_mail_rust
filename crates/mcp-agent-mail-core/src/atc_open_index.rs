//! In-memory open-experience index for ATC learning (br-0qt6e.2.5).
//!
//! Maintains a bounded set of unresolved experience entries so the
//! resolution engine can check "what is still waiting for evidence?"
//! in O(1) without scanning the database.
//!
//! # Design
//!
//! The index tracks open experiences by:
//! - `experience_id` → entry lookup (O(1) via HashMap)
//! - `subject` (agent name) → entries for that agent (fast per-agent sweep)
//! - `created_ts_micros` for expiration ordering
//!
//! # Bounded Memory
//!
//! The index enforces a maximum capacity (default 1000). When full, the
//! oldest entries are expired automatically. This ensures memory stays
//! bounded regardless of how many experiences are created.
//!
//! # Restart Recovery
//!
//! On startup, the index is populated from the database by querying
//! `fetch_open_atc_experiences()`. This ensures crash-restart doesn't
//! lose track of open experiences.
//!
//! # Resolution Signals
//!
//! The index supports three resolution paths:
//! - **Resolved**: Agent showed activity (positive outcome)
//! - **Expired**: Resolution window elapsed without signal
//! - **Censored**: Agent departed or project closed (unobservable)

#![allow(clippy::doc_markdown)]

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Maximum number of open experiences tracked in memory.
pub const DEFAULT_OPEN_INDEX_CAPACITY: usize = 1000;

/// Default resolution window: 10 minutes in microseconds.
pub const DEFAULT_RESOLUTION_WINDOW_MICROS: i64 = 600_000_000;

/// An entry in the open-experience index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenExperienceEntry {
    /// Experience ID (from the durable store).
    pub experience_id: u64,
    /// Decision ID that generated this experience.
    pub decision_id: u64,
    /// Subject agent name (lowercase).
    pub subject: String,
    /// When the experience was created.
    pub created_ts_micros: i64,
    /// Effect kind label (e.g., "advisory", "probe").
    pub effect_kind: String,
    /// Subsystem that originated this experience.
    pub subsystem: String,
    /// Whether this entry is a candidate for causal confounding
    /// (multiple open experiences for the same subject overlap).
    pub potentially_confounded: bool,
}

/// In-memory index of open (unresolved) ATC experiences.
///
/// Provides O(1) lookup by experience_id and fast per-agent iteration.
/// Bounded to [`DEFAULT_OPEN_INDEX_CAPACITY`] entries.
#[derive(Debug)]
pub struct OpenExperienceIndex {
    /// Primary map: experience_id → entry.
    entries: HashMap<u64, OpenExperienceEntry>,
    /// Secondary index: subject (lowercase) → set of experience_ids.
    by_subject: HashMap<String, HashSet<u64>>,
    /// Expiration ordering: created_ts_micros → experience_id.
    /// BTreeMap keeps entries sorted by creation time for efficient
    /// expiration sweeps.
    by_creation: BTreeMap<i64, Vec<u64>>,
    /// Maximum capacity.
    capacity: usize,
    /// Resolution window in microseconds.
    resolution_window_micros: i64,
    /// Total entries ever added (monotonic counter).
    total_added: u64,
    /// Total entries resolved.
    total_resolved: u64,
    /// Total entries expired.
    total_expired: u64,
    /// Total entries censored.
    total_censored: u64,
    /// Total entries evicted due to capacity.
    total_evicted: u64,
}

/// Summary statistics for the open index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenIndexStats {
    pub open_count: usize,
    pub unique_subjects: usize,
    pub oldest_ts_micros: Option<i64>,
    pub newest_ts_micros: Option<i64>,
    pub total_added: u64,
    pub total_resolved: u64,
    pub total_expired: u64,
    pub total_censored: u64,
    pub total_evicted: u64,
    pub potentially_confounded: usize,
    pub capacity: usize,
}

/// Result of checking an agent for resolution eligibility.
#[derive(Debug, Clone)]
pub struct ResolutionCandidate {
    pub experience_id: u64,
    pub decision_id: u64,
    pub created_ts_micros: i64,
    pub effect_kind: String,
    pub age_micros: i64,
}

impl OpenExperienceIndex {
    /// Create a new empty index with default capacity.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_OPEN_INDEX_CAPACITY, DEFAULT_RESOLUTION_WINDOW_MICROS)
    }

    /// Create a new index with custom capacity and resolution window.
    #[must_use]
    pub fn with_capacity(capacity: usize, resolution_window_micros: i64) -> Self {
        Self {
            entries: HashMap::with_capacity(capacity.min(256)),
            by_subject: HashMap::new(),
            by_creation: BTreeMap::new(),
            capacity,
            resolution_window_micros,
            total_added: 0,
            total_resolved: 0,
            total_expired: 0,
            total_censored: 0,
            total_evicted: 0,
        }
    }

    /// Add an open experience to the index.
    ///
    /// If the index is at capacity, the oldest entry is evicted.
    /// Duplicate experience_ids are silently ignored (idempotent).
    pub fn add(&mut self, entry: OpenExperienceEntry) {
        if self.entries.contains_key(&entry.experience_id) {
            return; // idempotent
        }

        // Evict oldest if at capacity.
        while self.entries.len() >= self.capacity {
            self.evict_oldest();
        }

        let subject_key = entry.subject.to_ascii_lowercase();

        // Check for causal confounding: if there's already an open
        // experience for the same subject, both are potentially confounded.
        if let Some(existing_ids) = self.by_subject.get(&subject_key).filter(|ids| !ids.is_empty()) {
            // Mark the new entry as confounded.
            let mut entry = entry;
            entry.potentially_confounded = true;
            // Mark existing entries for this subject as confounded too.
            for &eid in existing_ids {
                if let Some(existing) = self.entries.get_mut(&eid) {
                    existing.potentially_confounded = true;
                }
            }
            self.insert_entry(entry, subject_key);
            return;
        }

        self.insert_entry(entry, subject_key);
    }

    fn insert_entry(&mut self, entry: OpenExperienceEntry, subject_key: String) {
        let exp_id = entry.experience_id;
        let created_ts = entry.created_ts_micros;

        self.by_subject
            .entry(subject_key)
            .or_default()
            .insert(exp_id);
        self.by_creation
            .entry(created_ts)
            .or_default()
            .push(exp_id);
        self.entries.insert(exp_id, entry);
        self.total_added += 1;
    }

    fn evict_oldest(&mut self) {
        // Find the oldest creation timestamp.
        let Some((&oldest_ts, ids)) = self.by_creation.iter_mut().next() else {
            return;
        };

        // Remove ONE entry at the oldest timestamp (not all of them).
        // Under high load, many experiences can share a timestamp (same ATC tick).
        // Evicting all of them at once would be overly aggressive.
        let Some(id) = ids.pop() else {
            self.by_creation.remove(&oldest_ts);
            return;
        };

        // Clean up the BTreeMap entry if now empty.
        if ids.is_empty() {
            self.by_creation.remove(&oldest_ts);
        }

        // Remove from primary and secondary indexes.
        if let Some(entry) = self.entries.remove(&id) {
            let subject_key = entry.subject.to_ascii_lowercase();
            if let Some(set) = self.by_subject.get_mut(&subject_key) {
                set.remove(&id);
                if set.is_empty() {
                    self.by_subject.remove(&subject_key);
                }
            }
            self.total_evicted += 1;
        }
    }

    /// Remove an experience from the index (resolved/expired/censored).
    fn remove(&mut self, experience_id: u64) -> Option<OpenExperienceEntry> {
        let entry = self.entries.remove(&experience_id)?;
        let subject_key = entry.subject.to_ascii_lowercase();

        if let Some(set) = self.by_subject.get_mut(&subject_key) {
            set.remove(&experience_id);
            if set.is_empty() {
                self.by_subject.remove(&subject_key);
            }
        }

        if let Some(ids) = self.by_creation.get_mut(&entry.created_ts_micros) {
            ids.retain(|&id| id != experience_id);
            if ids.is_empty() {
                self.by_creation.remove(&entry.created_ts_micros);
            }
        }

        Some(entry)
    }

    /// Mark an experience as resolved and remove it from the index.
    pub fn mark_resolved(&mut self, experience_id: u64) -> Option<OpenExperienceEntry> {
        let entry = self.remove(experience_id)?;
        self.total_resolved += 1;
        Some(entry)
    }

    /// Mark an experience as expired and remove it from the index.
    pub fn mark_expired(&mut self, experience_id: u64) -> Option<OpenExperienceEntry> {
        let entry = self.remove(experience_id)?;
        self.total_expired += 1;
        Some(entry)
    }

    /// Mark an experience as censored and remove it from the index.
    pub fn mark_censored(&mut self, experience_id: u64) -> Option<OpenExperienceEntry> {
        let entry = self.remove(experience_id)?;
        self.total_censored += 1;
        Some(entry)
    }

    /// Get open experiences for a specific agent.
    #[must_use]
    pub fn for_subject(&self, subject: &str) -> Vec<&OpenExperienceEntry> {
        let subject_key = subject.to_ascii_lowercase();
        self.by_subject
            .get(&subject_key)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| self.entries.get(id))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Get resolution candidates that have exceeded the resolution window.
    #[must_use]
    pub fn expired_candidates(&self, now_micros: i64) -> Vec<ResolutionCandidate> {
        let cutoff = now_micros.saturating_sub(self.resolution_window_micros);
        let mut candidates = Vec::new();

        for (&ts, ids) in &self.by_creation {
            if ts > cutoff {
                break; // BTreeMap is sorted, so all remaining are newer
            }
            for &id in ids {
                if let Some(entry) = self.entries.get(&id) {
                    candidates.push(ResolutionCandidate {
                        experience_id: entry.experience_id,
                        decision_id: entry.decision_id,
                        created_ts_micros: entry.created_ts_micros,
                        effect_kind: entry.effect_kind.clone(),
                        age_micros: now_micros.saturating_sub(entry.created_ts_micros),
                    });
                }
            }
        }

        candidates
    }

    /// Check if an experience is in the open index.
    #[must_use]
    pub fn contains(&self, experience_id: u64) -> bool {
        self.entries.contains_key(&experience_id)
    }

    /// Number of open experiences in the index.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the index is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Get summary statistics.
    #[must_use]
    pub fn stats(&self) -> OpenIndexStats {
        let oldest = self.by_creation.keys().next().copied();
        let newest = self.by_creation.keys().next_back().copied();
        let confounded = self.entries.values().filter(|e| e.potentially_confounded).count();

        OpenIndexStats {
            open_count: self.entries.len(),
            unique_subjects: self.by_subject.len(),
            oldest_ts_micros: oldest,
            newest_ts_micros: newest,
            total_added: self.total_added,
            total_resolved: self.total_resolved,
            total_expired: self.total_expired,
            total_censored: self.total_censored,
            total_evicted: self.total_evicted,
            potentially_confounded: confounded,
            capacity: self.capacity,
        }
    }

    /// Iterate over all open entries (for restart recovery serialization).
    pub fn iter(&self) -> impl Iterator<Item = &OpenExperienceEntry> {
        self.entries.values()
    }
}

impl Default for OpenExperienceIndex {
    fn default() -> Self {
        Self::new()
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(id: u64, subject: &str, ts: i64) -> OpenExperienceEntry {
        OpenExperienceEntry {
            experience_id: id,
            decision_id: id * 10,
            subject: subject.to_string(),
            created_ts_micros: ts,
            effect_kind: "advisory".to_string(),
            subsystem: "liveness".to_string(),
            potentially_confounded: false,
        }
    }

    #[test]
    fn add_and_lookup() {
        let mut idx = OpenExperienceIndex::new();
        idx.add(make_entry(1, "AgentA", 1000));
        assert_eq!(idx.len(), 1);
        assert!(idx.contains(1));
        assert_eq!(idx.for_subject("agenta").len(), 1); // case-insensitive
    }

    #[test]
    fn idempotent_add() {
        let mut idx = OpenExperienceIndex::new();
        idx.add(make_entry(1, "AgentA", 1000));
        idx.add(make_entry(1, "AgentA", 1000));
        assert_eq!(idx.len(), 1);
        assert_eq!(idx.total_added, 1); // only counted once
    }

    #[test]
    fn resolve_removes_from_index() {
        let mut idx = OpenExperienceIndex::new();
        idx.add(make_entry(1, "AgentA", 1000));
        let resolved = idx.mark_resolved(1);
        assert!(resolved.is_some());
        assert_eq!(idx.len(), 0);
        assert_eq!(idx.total_resolved, 1);
        assert!(!idx.contains(1));
    }

    #[test]
    fn expire_removes_from_index() {
        let mut idx = OpenExperienceIndex::new();
        idx.add(make_entry(1, "AgentA", 1000));
        let expired = idx.mark_expired(1);
        assert!(expired.is_some());
        assert_eq!(idx.len(), 0);
        assert_eq!(idx.total_expired, 1);
    }

    #[test]
    fn censor_removes_from_index() {
        let mut idx = OpenExperienceIndex::new();
        idx.add(make_entry(1, "AgentA", 1000));
        let censored = idx.mark_censored(1);
        assert!(censored.is_some());
        assert_eq!(idx.len(), 0);
        assert_eq!(idx.total_censored, 1);
    }

    #[test]
    fn capacity_evicts_oldest() {
        let mut idx = OpenExperienceIndex::with_capacity(3, DEFAULT_RESOLUTION_WINDOW_MICROS);
        idx.add(make_entry(1, "A", 100));
        idx.add(make_entry(2, "B", 200));
        idx.add(make_entry(3, "C", 300));
        assert_eq!(idx.len(), 3);

        // Adding a 4th should evict the oldest (id=1, ts=100).
        idx.add(make_entry(4, "D", 400));
        assert_eq!(idx.len(), 3);
        assert!(!idx.contains(1)); // evicted
        assert!(idx.contains(4)); // added
        assert_eq!(idx.total_evicted, 1);
    }

    #[test]
    fn expired_candidates_finds_old_entries() {
        let mut idx = OpenExperienceIndex::with_capacity(10, 500); // 500μs window
        idx.add(make_entry(1, "A", 100));
        idx.add(make_entry(2, "B", 300));
        idx.add(make_entry(3, "C", 700));

        // At now=700, window=500 → cutoff=200.
        // Entry 1 (ts=100) is expired (<= 200).
        // Entry 2 (ts=300) is NOT expired (> 200).
        // Entry 3 (ts=700) is NOT expired.
        let candidates = idx.expired_candidates(700);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].experience_id, 1);
    }

    #[test]
    fn confounding_detection() {
        let mut idx = OpenExperienceIndex::new();
        idx.add(make_entry(1, "AgentA", 1000));
        assert!(!idx.entries[&1].potentially_confounded);

        // Adding another for the same agent marks both as confounded.
        idx.add(make_entry(2, "AgentA", 2000));
        assert!(idx.entries[&1].potentially_confounded);
        assert!(idx.entries[&2].potentially_confounded);
    }

    #[test]
    fn stats_are_accurate() {
        let mut idx = OpenExperienceIndex::new();
        idx.add(make_entry(1, "A", 100));
        idx.add(make_entry(2, "B", 200));
        idx.mark_resolved(1);

        let stats = idx.stats();
        assert_eq!(stats.open_count, 1);
        assert_eq!(stats.unique_subjects, 1);
        assert_eq!(stats.total_added, 2);
        assert_eq!(stats.total_resolved, 1);
    }

    #[test]
    fn for_subject_is_case_insensitive() {
        let mut idx = OpenExperienceIndex::new();
        idx.add(make_entry(1, "AgentAlpha", 1000));
        assert_eq!(idx.for_subject("agentalpha").len(), 1);
        assert_eq!(idx.for_subject("AGENTALPHA").len(), 1);
        assert_eq!(idx.for_subject("AgentAlpha").len(), 1);
    }

    #[test]
    fn empty_index_stats() {
        let idx = OpenExperienceIndex::new();
        let stats = idx.stats();
        assert_eq!(stats.open_count, 0);
        assert_eq!(stats.oldest_ts_micros, None);
        assert_eq!(stats.newest_ts_micros, None);
    }
}
