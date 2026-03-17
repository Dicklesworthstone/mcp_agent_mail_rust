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
                    .entry(compiled.normalized().to_owned())
                    .or_default()
                    .push((compiled, rref));
            } else if let Some(prefix) = compiled.first_literal_segment() {
                // Glob with a literal prefix segment.
                globs_by_prefix
                    .entry(prefix.to_owned())
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
        conflicts.clear();
        let req_norm = request_pat.normalized();
        let req_is_glob = request_pat.is_glob();
        let req_prefix = request_pat.first_literal_segment();

        if req_is_glob {
            // Glob request: must scan relevant prefix groups + root for both
            // exact and glob reservations, because a glob request can overlap
            // exact paths via one-directional matching.
            self.scan_glob_request(request_pat, req_prefix, conflicts);
        } else {
            // Exact request path: check for exact equality + overlapping globs.
            self.scan_exact_request(request_pat, req_norm, req_prefix, conflicts);
        }

        // Always check root-level globs.
        for (res_pat, rref) in &self.root_globs {
            if res_pat.overlaps(request_pat) {
                conflicts.push(rref);
            }
        }
    }

    /// Scan for conflicts when the request is an exact path (no glob chars).
    fn scan_exact_request<'a>(
        &'a self,
        request_pat: &CompiledPattern,
        req_norm: &str,
        req_prefix: Option<&str>,
        conflicts: &mut Vec<&'a ReservationRef>,
    ) {
        if let Some(entries) = self.exact_by_path.get(req_norm) {
            for (_, rref) in entries {
                conflicts.push(rref);
            }
        }

        for slash_idx in req_norm.match_indices('/').map(|(idx, _)| idx) {
            let ancestor = &req_norm[..slash_idx];
            if let Some(entries) = self.exact_by_path.get(ancestor) {
                for (_, rref) in entries {
                    conflicts.push(rref);
                }
            }
        }

        if let Some(descendant_prefix) = descendant_prefix(req_norm) {
            for (path, entries) in self.exact_by_path.range(descendant_prefix.clone()..) {
                if !path.starts_with(&descendant_prefix) {
                    break;
                }
                for (_, rref) in entries {
                    conflicts.push(rref);
                }
            }
        }

        if let Some(prefix) = req_prefix {
            if let Some(entries) = self.globs_by_prefix.get(prefix) {
                for (res_pat, rref) in entries {
                    if res_pat.overlaps(request_pat) {
                        conflicts.push(rref);
                    }
                }
            }
        } else {
            for entries in self.globs_by_prefix.values() {
                for (res_pat, rref) in entries {
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
    fn scan_glob_request<'a>(
        &'a self,
        request_pat: &CompiledPattern,
        req_prefix: Option<&str>,
        conflicts: &mut Vec<&'a ReservationRef>,
    ) {
        if let Some(prefix) = req_prefix {
            // Scoped scan: only check the exact anchor (`src`) and its
            // descendants (`src/...`) instead of every exact entry sharing the
            // first path segment.
            if let Some(entries) = self.exact_by_path.get(prefix) {
                for (exact_pat, rref) in entries {
                    if request_pat.matches(exact_pat.normalized())
                        || exact_pat.overlaps(request_pat)
                    {
                        conflicts.push(rref);
                    }
                }
            }

            if let Some(descendant_prefix) = descendant_prefix(prefix) {
                for (path, entries) in self.exact_by_path.range(descendant_prefix.clone()..) {
                    if !path.starts_with(&descendant_prefix) {
                        break;
                    }
                    for (exact_pat, rref) in entries {
                        if request_pat.matches(exact_pat.normalized())
                            || exact_pat.overlaps(request_pat)
                        {
                            conflicts.push(rref);
                        }
                    }
                }
            }

            if let Some(entries) = self.globs_by_prefix.get(prefix) {
                for (res_pat, rref) in entries {
                    if res_pat.overlaps(request_pat) {
                        conflicts.push(rref);
                    }
                }
            }
        } else {
            // Root glob request (no prefix): must check ALL groups.
            for entries in self.exact_by_path.values() {
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
                    if res_pat.overlaps(request_pat) {
                        conflicts.push(rref);
                    }
                }
            }
        }
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

    fn make_ref(agent_id: i64) -> ReservationRef {
        ReservationRef {
            agent_id,
            path_pattern: String::new(),
            exclusive: true,
            expires_ts: 0,
        }
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
}
