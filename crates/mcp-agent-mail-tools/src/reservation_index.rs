//! Prefix-partitioned index for file reservation conflict detection.
//!
//! Replaces the O(M*N) brute-force nested loop in `file_reservation_paths`
//! with an indexed structure that narrows exact-path checks to exact,
//! ancestor, and descendant range lookups while still grouping glob
//! reservations by their first literal path segment.
//!
//! # Partitioning Strategy
//!
//! Active exclusive reservations are classified into three buckets:
//!
//! 1. **Exact paths** (no glob metacharacters): stored in a `BTreeMap<String, …>`
//!    keyed by normalized path so exact/ancestor lookups are O(log N) and
//!    descendant scans use a tight prefix range instead of scanning every
//!    reservation under the same first segment.
//! 2. **Prefixed globs** (e.g. `src/**/*.rs`): grouped by first literal segment.
//! 3. **Root globs** (e.g. `*.rs`, `**`): must be checked against every request.
//!
//! For each requested path, exact-path checks stay local to the exact key,
//! its segment ancestors, and its descendant subtree, while glob checks stay
//! scoped to the relevant literal-prefix group plus root globs.

use std::collections::{BTreeMap, HashMap};

use mcp_agent_mail_core::pattern_overlap::CompiledPattern;
use std::sync::Arc;

fn routing_key(s: &str) -> String {
    if cfg!(any(target_os = "macos", target_os = "windows")) {
        s.to_ascii_lowercase()
    } else {
        s.to_string()
    }
}

/// Metadata from a reservation row needed for conflict reporting.
#[derive(Debug, Clone)]
pub(crate) struct ReservationRef {
    pub agent_id: i64,
    pub path_pattern: String,
    pub exclusive: bool,
    pub expires_ts: i64,
}

/// Indexed collection of active exclusive reservations for fast conflict detection.
pub(crate) struct ReservationIndex {
    /// Exact-path reservations keyed by normalized path.
    /// Each value contains one or more reservations on the same path.
    exact_by_path: BTreeMap<String, Vec<(Arc<CompiledPattern>, ReservationRef)>>,

    /// Glob reservations grouped by first literal path segment.
    globs_by_prefix: HashMap<String, Vec<(Arc<CompiledPattern>, ReservationRef)>>,

    /// Root-level globs with no literal prefix (e.g. `*.rs`, `**`).
    /// Must be checked against every requested path.
    root_globs: Vec<(Arc<CompiledPattern>, ReservationRef)>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ScanStats {
    exact_paths: usize,
    prefixed_globs: usize,
    root_globs: usize,
}

impl ReservationIndex {
    /// Build an index from an iterator of `(raw_pattern, reservation_ref)` pairs.
    pub fn build(reservations: impl Iterator<Item = (String, ReservationRef)>) -> Self {
        let mut exact_by_path: BTreeMap<String, Vec<(Arc<CompiledPattern>, ReservationRef)>> =
            BTreeMap::new();
        let mut globs_by_prefix: HashMap<String, Vec<(Arc<CompiledPattern>, ReservationRef)>> =
            HashMap::new();
        let mut root_globs: Vec<(Arc<CompiledPattern>, ReservationRef)> = Vec::new();

        for (raw_pattern, rref) in reservations {
            let compiled = CompiledPattern::cached(&raw_pattern);

            if !compiled.is_glob() {
                exact_by_path
                    .entry(routing_key(compiled.normalized()))
                    .or_default()
                    .push((compiled, rref));
            } else if let Some(prefix) = compiled.first_literal_segment() {
                // Glob with a literal prefix segment.
                globs_by_prefix
                    .entry(routing_key(prefix))
                    .or_default()
                    .push((compiled, rref));
            } else {
                // Root-level glob: no prefix to partition on.
                root_globs.push((compiled, rref));
            }
        }

        Self {
            exact_by_path,
            globs_by_prefix,
            root_globs,
        }
    }

    /// Find all reservations that conflict with the given request path.
    ///
    /// `request_pat` must be a `CompiledPattern` for the same `request_path`.
    pub fn find_conflicts<'a>(
        &'a self,
        request_pat: &CompiledPattern,
        conflicts: &mut Vec<&'a ReservationRef>,
    ) {
        self.find_conflicts_inner::<false>(request_pat, conflicts, &mut None);
    }

    #[cfg(test)]
    fn find_conflicts_with_stats<'a>(
        &'a self,
        request_pat: &CompiledPattern,
        conflicts: &mut Vec<&'a ReservationRef>,
    ) -> ScanStats {
        let mut stats = ScanStats::default();
        self.find_conflicts_inner::<true>(request_pat, conflicts, &mut Some(&mut stats));
        stats
    }

    fn find_conflicts_inner<'a, const COUNT_STATS: bool>(
        &'a self,
        request_pat: &CompiledPattern,
        conflicts: &mut Vec<&'a ReservationRef>,
        stats: &mut Option<&mut ScanStats>,
    ) {
        conflicts.clear();
        let req_norm = request_pat.normalized();
        let req_is_glob = request_pat.is_glob();
        let req_prefix = request_pat.first_literal_segment();

        if req_is_glob {
            // Glob request: must scan relevant prefix groups + root for both
            // exact and glob reservations, because a glob request can overlap
            // exact paths via one-directional matching.
            self.scan_glob_request::<COUNT_STATS>(request_pat, req_prefix, conflicts, stats);
        } else {
            // Exact request path: check for exact equality + overlapping globs.
            self.scan_exact_request::<COUNT_STATS>(
                request_pat,
                req_norm,
                req_prefix,
                conflicts,
                stats,
            );
        }

        // Always check root-level globs.
        for (res_pat, rref) in &self.root_globs {
            note_root_glob_entry::<COUNT_STATS>(stats);
            if res_pat.overlaps(request_pat) {
                conflicts.push(rref);
            }
        }
    }

    /// Scan for conflicts when the request is an exact path (no glob chars).
    fn scan_exact_request<'a, const COUNT_STATS: bool>(
        &'a self,
        request_pat: &CompiledPattern,
        req_norm: &str,
        req_prefix: Option<&str>,
        conflicts: &mut Vec<&'a ReservationRef>,
        stats: &mut Option<&mut ScanStats>,
    ) {
        let route_norm = routing_key(req_norm);
        if let Some(entries) = self.exact_by_path.get(&route_norm) {
            note_exact_entries::<COUNT_STATS>(stats, entries.len());
            for (_, rref) in entries {
                conflicts.push(rref);
            }
        }

        // Always check root (empty string) ancestor
        if !req_norm.is_empty() {
            let route_root = routing_key("");
            if let Some(entries) = self.exact_by_path.get(&route_root) {
                note_exact_entries::<COUNT_STATS>(stats, entries.len());
                for (_, rref) in entries {
                    conflicts.push(rref);
                }
            }
        }

        for slash_idx in req_norm.match_indices('/').map(|(idx, _)| idx) {
            let ancestor = &req_norm[..slash_idx];
            let route_ancestor = routing_key(ancestor);
            if let Some(entries) = self.exact_by_path.get(&route_ancestor) {
                note_exact_entries::<COUNT_STATS>(stats, entries.len());
                for (_, rref) in entries {
                    conflicts.push(rref);
                }
            }
        }

        if let Some(descendant_prefix) = descendant_prefix(&route_norm) {
            for (path, entries) in self.exact_by_path.range(descendant_prefix.clone()..) {
                if !path.starts_with(&descendant_prefix) {
                    break;
                }
                note_exact_entries::<COUNT_STATS>(stats, entries.len());
                for (_, rref) in entries {
                    conflicts.push(rref);
                }
            }
        }

        if let Some(prefix) = req_prefix {
            let route_prefix = routing_key(prefix);
            if let Some(entries) = self.globs_by_prefix.get(&route_prefix) {
                for (res_pat, rref) in entries {
                    note_glob_entry::<COUNT_STATS>(stats);
                    if res_pat.overlaps(request_pat) {
                        conflicts.push(rref);
                    }
                }
            }
        } else {
            for entries in self.globs_by_prefix.values() {
                for (res_pat, rref) in entries {
                    note_glob_entry::<COUNT_STATS>(stats);
                    if res_pat.overlaps(request_pat) {
                        conflicts.push(rref);
                    }
                }
            }
        }
    }

    /// Scan for conflicts when the request is a glob pattern.
    ///
    /// A glob request like `src/**` can match exact reservations like `src/main.rs`,
    /// so we must scan the exact anchor (`src`) and exact descendants (`src/...`).
    fn scan_glob_request<'a, const COUNT_STATS: bool>(
        &'a self,
        request_pat: &CompiledPattern,
        req_prefix: Option<&str>,
        conflicts: &mut Vec<&'a ReservationRef>,
        stats: &mut Option<&mut ScanStats>,
    ) {
        if let Some(prefix) = req_prefix {
            let route_prefix = routing_key(prefix);

            // Check ancestors of the prefix (including the prefix itself and root)
            // against exact reservations.
            if let Some(entries) = self.exact_by_path.get(&route_prefix) {
                note_exact_entries::<COUNT_STATS>(stats, entries.len());
                for (exact_pat, rref) in entries {
                    if request_pat.matches(exact_pat.normalized())
                        || exact_pat.overlaps(request_pat)
                    {
                        conflicts.push(rref);
                    }
                }
            }

            // Always check root (empty string) ancestor
            let route_root = routing_key("");
            if let Some(entries) = self.exact_by_path.get(&route_root) {
                note_exact_entries::<COUNT_STATS>(stats, entries.len());
                for (exact_pat, rref) in entries {
                    if request_pat.matches(exact_pat.normalized())
                        || exact_pat.overlaps(request_pat)
                    {
                        conflicts.push(rref);
                    }
                }
            }

            for slash_idx in prefix.match_indices('/').map(|(idx, _)| idx) {
                let ancestor = &prefix[..slash_idx];
                let route_ancestor = routing_key(ancestor);
                if let Some(entries) = self.exact_by_path.get(&route_ancestor) {
                    note_exact_entries::<COUNT_STATS>(stats, entries.len());
                    for (exact_pat, rref) in entries {
                        if request_pat.matches(exact_pat.normalized())
                            || exact_pat.overlaps(request_pat)
                        {
                            conflicts.push(rref);
                        }
                    }
                }
            }

            if let Some(descendant_prefix) = descendant_prefix(&route_prefix) {
                for (path, entries) in self.exact_by_path.range(descendant_prefix.clone()..) {
                    if !path.starts_with(&descendant_prefix) {
                        break;
                    }
                    note_exact_entries::<COUNT_STATS>(stats, entries.len());
                    for (exact_pat, rref) in entries {
                        if request_pat.matches(exact_pat.normalized())
                            || exact_pat.overlaps(request_pat)
                        {
                            conflicts.push(rref);
                        }
                    }
                }
            }

            if let Some(entries) = self.globs_by_prefix.get(&route_prefix) {
                for (res_pat, rref) in entries {
                    note_glob_entry::<COUNT_STATS>(stats);
                    if res_pat.overlaps(request_pat) {
                        conflicts.push(rref);
                    }
                }
            }
        } else {
            // Root glob request (no prefix): must check ALL groups.
            for entries in self.exact_by_path.values() {
                note_exact_entries::<COUNT_STATS>(stats, entries.len());
                for (exact_pat, rref) in entries {
                    if request_pat.matches(exact_pat.normalized())
                        || exact_pat.overlaps(request_pat)
                    {
                        conflicts.push(rref);
                    }
                }
            }
            for entries in self.globs_by_prefix.values() {
                for (res_pat, rref) in entries {
                    note_glob_entry::<COUNT_STATS>(stats);
                    if res_pat.overlaps(request_pat) {
                        conflicts.push(rref);
                    }
                }
            }
        }
    }
}

fn note_exact_entries<const COUNT_STATS: bool>(stats: &mut Option<&mut ScanStats>, entries: usize) {
    if COUNT_STATS && let Some(stats) = stats.as_deref_mut() {
        stats.exact_paths += entries;
    }
}

fn note_glob_entry<const COUNT_STATS: bool>(stats: &mut Option<&mut ScanStats>) {
    if COUNT_STATS && let Some(stats) = stats.as_deref_mut() {
        stats.prefixed_globs += 1;
    }
}

fn note_root_glob_entry<const COUNT_STATS: bool>(stats: &mut Option<&mut ScanStats>) {
    if COUNT_STATS && let Some(stats) = stats.as_deref_mut() {
        stats.root_globs += 1;
    }
}

fn descendant_prefix(norm: &str) -> Option<String> {
    if norm.is_empty() {
        None
    } else {
        Some(format!("{norm}/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn make_ref(agent_id: i64) -> ReservationRef {
        ReservationRef {
            agent_id,
            path_pattern: String::new(),
            exclusive: true,
            expires_ts: 0,
        }
    }

    fn make_ref_for_pattern(agent_id: i64, pattern: &str) -> ReservationRef {
        ReservationRef {
            agent_id,
            path_pattern: pattern.to_string(),
            exclusive: true,
            expires_ts: agent_id,
        }
    }

    fn assert_index_matches_exhaustive(
        reservation_patterns: &[String],
        request_patterns: &[String],
    ) {
        let reservations: Vec<_> = reservation_patterns
            .iter()
            .enumerate()
            .map(|(idx, pattern)| {
                (
                    pattern.clone(),
                    make_ref_for_pattern(
                        i64::try_from(idx).expect("fixture index fits i64"),
                        pattern,
                    ),
                )
            })
            .collect();
        let index = ReservationIndex::build(reservations.clone().into_iter());

        for request_pattern in request_patterns {
            let request = CompiledPattern::new(request_pattern);
            let mut conflicts = Vec::new();
            index.find_conflicts(&request, &mut conflicts);
            let mut actual: Vec<i64> = conflicts.iter().map(|rref| rref.agent_id).collect();
            actual.sort_unstable();

            let mut expected: Vec<i64> = reservations
                .iter()
                .filter_map(|(reservation_pattern, rref)| {
                    CompiledPattern::new(reservation_pattern)
                        .overlaps(&request)
                        .then_some(rref.agent_id)
                })
                .collect();
            expected.sort_unstable();

            assert_eq!(
                actual, expected,
                "indexed conflicts differed from exhaustive scan for request {request_pattern:?}"
            );
        }
    }

    fn reservation_pattern_strategy() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("src".to_string()),
            Just("src/main.rs".to_string()),
            Just("src/lib.rs".to_string()),
            Just("src/**".to_string()),
            Just("src/**/*.rs".to_string()),
            Just("src/*.rs".to_string()),
            Just("src/auth/*".to_string()),
            Just("src/auth/**/*.rs".to_string()),
            Just("docs".to_string()),
            Just("docs/readme.md".to_string()),
            Just("docs/**".to_string()),
            Just("*.rs".to_string()),
            Just("**/*.rs".to_string()),
            Just("**".to_string()),
            Just("Cargo.toml".to_string()),
            Just("./src/main.rs".to_string()),
            Just("src\\windows.rs".to_string()),
            Just("[abc".to_string()),
            (0_usize..48).prop_map(|idx| format!("crate_{idx}/src/lib.rs")),
            (0_usize..48).prop_map(|idx| format!("crate_{idx}/**/*.rs")),
            (0_usize..48).prop_map(|idx| format!("crate_{idx}/tests/*.rs")),
        ]
    }

    fn request_pattern_strategy() -> impl Strategy<Value = String> {
        reservation_pattern_strategy()
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn generated_index_matches_exhaustive_overlap(
            reservations in proptest::collection::vec(reservation_pattern_strategy(), 0..64),
            requests in proptest::collection::vec(request_pattern_strategy(), 1..16),
        ) {
            assert_index_matches_exhaustive(&reservations, &requests);
        }
    }

    #[test]
    fn adversarial_index_matches_exhaustive_overlap() {
        let reservations = vec![
            String::new(),
            "src".to_string(),
            "src/main.rs".to_string(),
            "src/**".to_string(),
            "src/**/*.rs".to_string(),
            "src/auth/*".to_string(),
            "src/auth/**/*.rs".to_string(),
            "docs/**".to_string(),
            "docs/*.md".to_string(),
            "*.rs".to_string(),
            "**/*.rs".to_string(),
            "crates/db/src/schema.rs".to_string(),
            "crates/**/schema.rs".to_string(),
            "crates/*/tests/*.rs".to_string(),
            "./src/main.rs".to_string(),
            "src\\windows.rs".to_string(),
            "[abc".to_string(),
        ];
        let requests = vec![
            "src".to_string(),
            "src/main.rs".to_string(),
            "src/auth/mod.rs".to_string(),
            "src/auth/sub/mod.rs".to_string(),
            "src/*.rs".to_string(),
            "docs/readme.md".to_string(),
            "crates/db".to_string(),
            "crates/db/src/schema.rs".to_string(),
            "crates/db/tests/schema.rs".to_string(),
            "**".to_string(),
            "[abc".to_string(),
        ];

        assert_index_matches_exhaustive(&reservations, &requests);
    }

    #[test]
    fn exact_path_index_prunes_ten_thousand_unrelated_reservations() {
        let reservations = (0_i64..10_000).map(|idx| {
            let pattern = format!("project_{idx}/src/lib.rs");
            (pattern.clone(), make_ref_for_pattern(idx, &pattern))
        });
        let index = ReservationIndex::build(reservations);
        let request = CompiledPattern::new("project_7777/src/lib.rs");
        let mut conflicts = Vec::new();

        let stats = index.find_conflicts_with_stats(&request, &mut conflicts);

        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].agent_id, 7777);
        assert_eq!(stats.exact_paths, 1);
        assert_eq!(stats.prefixed_globs, 0);
        assert_eq!(stats.root_globs, 0);
        assert!(
            stats.exact_paths < 100,
            "indexed exact-path scan should not approach a 10k exhaustive walk: {stats:?}"
        );
    }

    #[test]
    fn prefixed_glob_index_prunes_ten_thousand_unrelated_reservations() {
        let reservations = (0_i64..10_000).map(|idx| {
            let pattern = format!("project_{idx}/**/*.rs");
            (pattern.clone(), make_ref_for_pattern(idx, &pattern))
        });
        let index = ReservationIndex::build(reservations);
        let request = CompiledPattern::new("project_7777/src/lib.rs");
        let mut conflicts = Vec::new();

        let stats = index.find_conflicts_with_stats(&request, &mut conflicts);

        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].agent_id, 7777);
        assert_eq!(stats.exact_paths, 0);
        assert_eq!(stats.prefixed_globs, 1);
        assert_eq!(stats.root_globs, 0);
        assert!(
            stats.prefixed_globs < 100,
            "prefixed glob bucket should not approach a 10k exhaustive walk: {stats:?}"
        );
    }

    #[test]
    fn exact_path_conflict_detected() {
        let idx =
            ReservationIndex::build(vec![("src/main.rs".to_string(), make_ref(1))].into_iter());
        let req = CompiledPattern::new("src/main.rs");
        let mut conflicts = Vec::new();
        idx.find_conflicts(&req, &mut conflicts);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].agent_id, 1);
    }

    #[test]
    fn glob_reservation_matches_exact_request_in_subdir() {
        let idx =
            ReservationIndex::build(vec![("src/**/*.rs".to_string(), make_ref(1))].into_iter());
        let req = CompiledPattern::new("src/main/foo.rs");
        let mut conflicts = Vec::new();
        idx.find_conflicts(&req, &mut conflicts);
        assert_eq!(conflicts.len(), 1);
    }

    #[test]
    fn exact_path_no_conflict_different_file() {
        let idx =
            ReservationIndex::build(vec![("src/main.rs".to_string(), make_ref(1))].into_iter());
        let req = CompiledPattern::new("src/lib.rs");
        let mut conflicts = Vec::new();
        idx.find_conflicts(&req, &mut conflicts);
        assert!(conflicts.is_empty());
    }

    #[test]
    fn glob_reservation_matches_exact_request() {
        let idx = ReservationIndex::build(vec![("src/**".to_string(), make_ref(1))].into_iter());
        let req = CompiledPattern::new("src/main.rs");
        let mut conflicts = Vec::new();
        idx.find_conflicts(&req, &mut conflicts);
        assert_eq!(conflicts.len(), 1);
    }

    #[test]
    fn glob_request_matches_exact_reservation() {
        let idx =
            ReservationIndex::build(vec![("src/main.rs".to_string(), make_ref(1))].into_iter());
        let req = CompiledPattern::new("src/**");
        let mut conflicts = Vec::new();
        idx.find_conflicts(&req, &mut conflicts);
        assert_eq!(conflicts.len(), 1);
    }

    #[test]
    fn root_glob_reservation_checked_against_all() {
        let idx = ReservationIndex::build(vec![("**/*.rs".to_string(), make_ref(1))].into_iter());
        let req = CompiledPattern::new("src/main.rs");
        let mut conflicts = Vec::new();
        idx.find_conflicts(&req, &mut conflicts);
        assert_eq!(conflicts.len(), 1);
    }

    #[test]
    fn different_prefix_no_conflict() {
        let idx =
            ReservationIndex::build(vec![("docs/readme.md".to_string(), make_ref(1))].into_iter());
        let req = CompiledPattern::new("src/main.rs");
        let mut conflicts = Vec::new();
        idx.find_conflicts(&req, &mut conflicts);
        assert!(conflicts.is_empty());
    }

    #[test]
    fn multiple_reservations_same_prefix() {
        let idx = ReservationIndex::build(
            vec![
                ("src/main.rs".to_string(), make_ref(1)),
                ("src/**/*.rs".to_string(), make_ref(2)),
                ("docs/**".to_string(), make_ref(3)),
            ]
            .into_iter(),
        );
        let req = CompiledPattern::new("src/main.rs");
        let mut conflicts = Vec::new();
        idx.find_conflicts(&req, &mut conflicts);
        // Should match exact src/main.rs and glob src/**/*.rs, but not docs/**
        assert_eq!(conflicts.len(), 2);
        let ids: Vec<i64> = conflicts.iter().map(|r| r.agent_id).collect();
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
    }

    #[test]
    fn root_glob_request_scans_all_groups() {
        let idx = ReservationIndex::build(
            vec![
                ("src/main.rs".to_string(), make_ref(1)),
                ("docs/readme.md".to_string(), make_ref(2)),
            ]
            .into_iter(),
        );
        let req = CompiledPattern::new("**");
        let mut conflicts = Vec::new();
        idx.find_conflicts(&req, &mut conflicts);
        // ** should match both files
        assert_eq!(conflicts.len(), 2);
    }

    #[test]
    fn empty_index_no_conflicts() {
        let idx = ReservationIndex::build(std::iter::empty());
        let req = CompiledPattern::new("src/main.rs");
        let mut conflicts = Vec::new();
        idx.find_conflicts(&req, &mut conflicts);
        assert!(conflicts.is_empty());
    }

    #[test]
    fn glob_vs_glob_overlap_detected() {
        let idx =
            ReservationIndex::build(vec![("src/**/*.rs".to_string(), make_ref(1))].into_iter());
        let req = CompiledPattern::new("src/**");
        let mut conflicts = Vec::new();
        idx.find_conflicts(&req, &mut conflicts);
        assert_eq!(conflicts.len(), 1);
    }

    #[test]
    fn normalized_paths_match() {
        // ./src/main.rs normalizes to src/main.rs
        let idx =
            ReservationIndex::build(vec![("./src/main.rs".to_string(), make_ref(1))].into_iter());
        let req = CompiledPattern::new("src/main.rs");
        let mut conflicts = Vec::new();
        idx.find_conflicts(&req, &mut conflicts);
        assert_eq!(conflicts.len(), 1);
    }

    // ── Additional edge-case tests ─────────────────────────────

    #[test]
    fn root_exact_path_conflict() {
        // A file at root level with no directory prefix
        let idx =
            ReservationIndex::build(vec![("Cargo.toml".to_string(), make_ref(1))].into_iter());
        let req = CompiledPattern::new("Cargo.toml");
        let mut conflicts = Vec::new();
        idx.find_conflicts(&req, &mut conflicts);
        assert_eq!(conflicts.len(), 1);
    }

    #[test]
    fn root_exact_path_no_conflict_with_different() {
        let idx =
            ReservationIndex::build(vec![("Cargo.toml".to_string(), make_ref(1))].into_iter());
        let req = CompiledPattern::new("Cargo.lock");
        let mut conflicts = Vec::new();
        idx.find_conflicts(&req, &mut conflicts);
        assert!(conflicts.is_empty());
    }

    #[test]
    fn ancestor_exact_path_conflicts_with_descendant_request() {
        let idx = ReservationIndex::build(vec![("src".to_string(), make_ref(1))].into_iter());
        let req = CompiledPattern::new("src/main.rs");
        let mut conflicts = Vec::new();
        idx.find_conflicts(&req, &mut conflicts);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].agent_id, 1);
    }

    #[test]
    fn exact_directory_request_conflicts_with_descendant_exact_path() {
        let idx = ReservationIndex::build(
            vec![("src/main/generated.rs".to_string(), make_ref(1))].into_iter(),
        );
        let req = CompiledPattern::new("src/main");
        let mut conflicts = Vec::new();
        idx.find_conflicts(&req, &mut conflicts);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].agent_id, 1);
    }

    #[test]
    fn glob_request_conflicts_with_exact_directory_anchor() {
        let idx = ReservationIndex::build(vec![("src".to_string(), make_ref(1))].into_iter());
        let req = CompiledPattern::new("src/**/*.rs");
        let mut conflicts = Vec::new();
        idx.find_conflicts(&req, &mut conflicts);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].agent_id, 1);
    }

    #[test]
    fn star_glob_matches_root_files() {
        let idx = ReservationIndex::build(vec![("*.toml".to_string(), make_ref(1))].into_iter());
        let req = CompiledPattern::new("Cargo.toml");
        let mut conflicts = Vec::new();
        idx.find_conflicts(&req, &mut conflicts);
        assert_eq!(conflicts.len(), 1);
    }

    #[test]
    fn deeply_nested_exact_path() {
        let idx = ReservationIndex::build(
            vec![("crates/db/src/schema.rs".to_string(), make_ref(1))].into_iter(),
        );
        let req = CompiledPattern::new("crates/db/src/schema.rs");
        let mut conflicts = Vec::new();
        idx.find_conflicts(&req, &mut conflicts);
        assert_eq!(conflicts.len(), 1);
    }

    #[test]
    fn deeply_nested_no_conflict_different_subtree() {
        let idx = ReservationIndex::build(
            vec![("crates/db/src/schema.rs".to_string(), make_ref(1))].into_iter(),
        );
        let req = CompiledPattern::new("crates/cli/src/schema.rs");
        let mut conflicts = Vec::new();
        idx.find_conflicts(&req, &mut conflicts);
        assert!(conflicts.is_empty());
    }

    #[test]
    fn multiple_agents_on_same_path() {
        let idx = ReservationIndex::build(
            vec![
                ("src/main.rs".to_string(), make_ref(1)),
                ("src/main.rs".to_string(), make_ref(2)),
            ]
            .into_iter(),
        );
        let req = CompiledPattern::new("src/main.rs");
        let mut conflicts = Vec::new();
        idx.find_conflicts(&req, &mut conflicts);
        assert_eq!(conflicts.len(), 2);
        let ids: Vec<i64> = conflicts.iter().map(|r| r.agent_id).collect();
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
    }

    #[test]
    fn glob_reservation_does_not_match_different_prefix() {
        let idx =
            ReservationIndex::build(vec![("src/**/*.rs".to_string(), make_ref(1))].into_iter());
        let req = CompiledPattern::new("docs/readme.md");
        let mut conflicts = Vec::new();
        idx.find_conflicts(&req, &mut conflicts);
        assert!(conflicts.is_empty());
    }

    #[test]
    fn descendant_prefix_helper() {
        assert_eq!(
            descendant_prefix("src/main.rs").as_deref(),
            Some("src/main.rs/")
        );
        assert_eq!(
            descendant_prefix("Cargo.toml").as_deref(),
            Some("Cargo.toml/")
        );
        assert_eq!(descendant_prefix("").as_deref(), None);
    }

    #[test]
    fn reservation_ref_fields_preserved() {
        let rref = ReservationRef {
            agent_id: 42,
            path_pattern: "src/**".to_string(),
            exclusive: true,
            expires_ts: 999_000,
        };
        let idx = ReservationIndex::build(vec![("src/main.rs".to_string(), rref)].into_iter());
        let req = CompiledPattern::new("src/main.rs");
        let mut conflicts = Vec::new();
        idx.find_conflicts(&req, &mut conflicts);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].agent_id, 42);
        assert!(conflicts[0].exclusive);
        assert_eq!(conflicts[0].expires_ts, 999_000);
    }

    #[test]
    fn root_reservation_conflicts_with_everything() {
        let idx = ReservationIndex::build(vec![(String::new(), make_ref(1))].into_iter());

        // Exact request
        let req_exact = CompiledPattern::new("src/main.rs");
        let mut conflicts = Vec::new();
        idx.find_conflicts(&req_exact, &mut conflicts);
        assert_eq!(
            conflicts.len(),
            1,
            "Root reservation should conflict with exact request"
        );

        // Glob request
        let req_glob = CompiledPattern::new("src/**/*.rs");
        let mut conflicts = Vec::new();
        idx.find_conflicts(&req_glob, &mut conflicts);
        assert_eq!(
            conflicts.len(),
            1,
            "Root reservation should conflict with glob request"
        );
    }
}
