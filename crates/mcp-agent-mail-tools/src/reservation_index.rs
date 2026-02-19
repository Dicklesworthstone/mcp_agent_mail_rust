//! Prefix-partitioned index for file reservation conflict detection.
//!
//! Replaces the O(M*N) brute-force nested loop in `file_reservation_paths`
//! with an indexed structure that groups reservations by their first path
//! segment, enabling O(M * (`K_seg` + `K_root`)) lookups where `K_seg` is
//! the count of reservations sharing the request path's prefix and `K_root`
//! is the (typically small) set of root-level glob patterns.
//!
//! # Partitioning Strategy
//!
//! Active exclusive reservations are classified into three buckets:
//!
//! 1. **Exact paths** (no glob metacharacters): stored in a `HashMap<String, …>`
//!    keyed by normalized path, grouped by first segment for prefix-scoped scans.
//! 2. **Prefixed globs** (e.g. `src/**/*.rs`): grouped by first literal segment.
//! 3. **Root globs** (e.g. `*.rs`, `**`): must be checked against every request.
//!
//! For each requested path, only the relevant prefix group + root globs are
//! examined, skipping all reservations under unrelated directory subtrees.

use std::collections::HashMap;

use mcp_agent_mail_core::pattern_overlap::CompiledPattern;

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
    /// Exact-path reservations grouped by first path segment.
    /// Each entry is `(normalized_path, ref)`.
    exact_by_prefix: HashMap<String, Vec<(String, ReservationRef)>>,

    /// Glob reservations grouped by first literal path segment.
    globs_by_prefix: HashMap<String, Vec<(CompiledPattern, ReservationRef)>>,

    /// Root-level globs with no literal prefix (e.g. `*.rs`, `**`).
    /// Must be checked against every requested path.
    root_globs: Vec<(CompiledPattern, ReservationRef)>,
}

impl ReservationIndex {
    /// Build an index from an iterator of `(raw_pattern, reservation_ref)` pairs.
    pub fn build(reservations: impl Iterator<Item = (String, ReservationRef)>) -> Self {
        let mut exact_by_prefix: HashMap<String, Vec<(String, ReservationRef)>> = HashMap::new();
        let mut globs_by_prefix: HashMap<String, Vec<(CompiledPattern, ReservationRef)>> =
            HashMap::new();
        let mut root_globs: Vec<(CompiledPattern, ReservationRef)> = Vec::new();

        for (raw_pattern, rref) in reservations {
            let compiled = CompiledPattern::new(&raw_pattern);
            let norm = compiled.normalized().to_owned();

            if !compiled.is_glob() {
                // Exact path: group by first segment for prefix-scoped scans.
                let prefix = first_segment(&norm).unwrap_or("").to_owned();
                exact_by_prefix
                    .entry(prefix)
                    .or_default()
                    .push((norm, rref));
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
            exact_by_prefix,
            globs_by_prefix,
            root_globs,
        }
    }

    /// Find all reservations that conflict with the given request path.
    ///
    /// `request_pat` must be a `CompiledPattern` for the same `request_path`.
    pub fn find_conflicts<'a>(&'a self, request_pat: &CompiledPattern) -> Vec<&'a ReservationRef> {
        let mut conflicts: Vec<&ReservationRef> = Vec::new();
        let req_norm = request_pat.normalized();
        let req_is_glob = request_pat.is_glob();
        let req_prefix = request_pat.first_literal_segment();

        if req_is_glob {
            // Glob request: must scan relevant prefix groups + root for both
            // exact and glob reservations, because a glob request can overlap
            // exact paths via one-directional matching.
            self.scan_glob_request(request_pat, req_prefix, &mut conflicts);
        } else {
            // Exact request path: check for exact equality + overlapping globs.
            self.scan_exact_request(request_pat, req_norm, req_prefix, &mut conflicts);
        }

        // Always check root-level globs.
        for (res_pat, rref) in &self.root_globs {
            if res_pat.overlaps(request_pat) {
                conflicts.push(rref);
            }
        }

        conflicts
    }

    /// Scan for conflicts when the request is an exact path (no glob chars).
    fn scan_exact_request<'a>(
        &'a self,
        request_pat: &CompiledPattern,
        req_norm: &str,
        req_prefix: Option<&str>,
        conflicts: &mut Vec<&'a ReservationRef>,
    ) {
        // Check exact reservations: only same-prefix group, string equality.
        if let Some(prefix) = req_prefix
            && let Some(entries) = self.exact_by_prefix.get(prefix)
        {
            for (norm, rref) in entries {
                if norm == req_norm {
                    conflicts.push(rref);
                }
            }
        }
        // Also check exact entries with empty prefix (root-level exact paths).
        if let Some(entries) = self.exact_by_prefix.get("") {
            for (norm, rref) in entries {
                if norm == req_norm {
                    conflicts.push(rref);
                }
            }
        }

        // Check glob reservations in the same prefix group.
        if let Some(prefix) = req_prefix
            && let Some(entries) = self.globs_by_prefix.get(prefix)
        {
            for (res_pat, rref) in entries {
                if res_pat.overlaps(request_pat) {
                    conflicts.push(rref);
                }
            }
        }
    }

    /// Scan for conflicts when the request is a glob pattern.
    ///
    /// A glob request like `src/**` can match exact reservations like `src/main.rs`,
    /// so we must scan exact entries in the same prefix group.
    fn scan_glob_request<'a>(
        &'a self,
        request_pat: &CompiledPattern,
        req_prefix: Option<&str>,
        conflicts: &mut Vec<&'a ReservationRef>,
    ) {
        if let Some(prefix) = req_prefix {
            // Scoped scan: only check entries sharing this prefix.

            // Check exact entries: request glob might match exact paths.
            if let Some(entries) = self.exact_by_prefix.get(prefix) {
                for (exact_norm, rref) in entries {
                    // One-directional: does our glob match the exact path?
                    if request_pat.matches(exact_norm) {
                        conflicts.push(rref);
                    }
                }
            }

            // Check glob entries in same prefix group.
            if let Some(entries) = self.globs_by_prefix.get(prefix) {
                for (res_pat, rref) in entries {
                    if res_pat.overlaps(request_pat) {
                        conflicts.push(rref);
                    }
                }
            }
        } else {
            // Root glob request (no prefix): must check ALL groups.
            for entries in self.exact_by_prefix.values() {
                for (exact_norm, rref) in entries {
                    if request_pat.matches(exact_norm) {
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

/// Extract the first path segment from a normalized path.
fn first_segment(norm: &str) -> Option<&str> {
    let seg = norm.split('/').next().unwrap_or("");
    if seg.is_empty() { None } else { Some(seg) }
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
        let conflicts = idx.find_conflicts(&req);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].agent_id, 1);
    }

    #[test]
    fn exact_path_no_conflict_different_file() {
        let idx =
            ReservationIndex::build(vec![("src/main.rs".to_string(), make_ref(1))].into_iter());
        let req = CompiledPattern::new("src/lib.rs");
        let conflicts = idx.find_conflicts(&req);
        assert!(conflicts.is_empty());
    }

    #[test]
    fn glob_reservation_matches_exact_request() {
        let idx = ReservationIndex::build(vec![("src/**".to_string(), make_ref(1))].into_iter());
        let req = CompiledPattern::new("src/main.rs");
        let conflicts = idx.find_conflicts(&req);
        assert_eq!(conflicts.len(), 1);
    }

    #[test]
    fn glob_request_matches_exact_reservation() {
        let idx =
            ReservationIndex::build(vec![("src/main.rs".to_string(), make_ref(1))].into_iter());
        let req = CompiledPattern::new("src/**");
        let conflicts = idx.find_conflicts(&req);
        assert_eq!(conflicts.len(), 1);
    }

    #[test]
    fn root_glob_reservation_checked_against_all() {
        let idx = ReservationIndex::build(vec![("**/*.rs".to_string(), make_ref(1))].into_iter());
        let req = CompiledPattern::new("src/main.rs");
        let conflicts = idx.find_conflicts(&req);
        assert_eq!(conflicts.len(), 1);
    }

    #[test]
    fn different_prefix_no_conflict() {
        let idx =
            ReservationIndex::build(vec![("docs/readme.md".to_string(), make_ref(1))].into_iter());
        let req = CompiledPattern::new("src/main.rs");
        let conflicts = idx.find_conflicts(&req);
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
        let conflicts = idx.find_conflicts(&req);
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
        let conflicts = idx.find_conflicts(&req);
        // ** should match both files
        assert_eq!(conflicts.len(), 2);
    }

    #[test]
    fn empty_index_no_conflicts() {
        let idx = ReservationIndex::build(std::iter::empty());
        let req = CompiledPattern::new("src/main.rs");
        let conflicts = idx.find_conflicts(&req);
        assert!(conflicts.is_empty());
    }

    #[test]
    fn glob_vs_glob_overlap_detected() {
        let idx =
            ReservationIndex::build(vec![("src/**/*.rs".to_string(), make_ref(1))].into_iter());
        let req = CompiledPattern::new("src/**");
        let conflicts = idx.find_conflicts(&req);
        assert_eq!(conflicts.len(), 1);
    }

    #[test]
    fn normalized_paths_match() {
        // ./src/main.rs normalizes to src/main.rs
        let idx =
            ReservationIndex::build(vec![("./src/main.rs".to_string(), make_ref(1))].into_iter());
        let req = CompiledPattern::new("src/main.rs");
        let conflicts = idx.find_conflicts(&req);
        assert_eq!(conflicts.len(), 1);
    }

    // ── Additional edge-case tests ─────────────────────────────

    #[test]
    fn root_exact_path_conflict() {
        // A file at root level with no directory prefix
        let idx =
            ReservationIndex::build(vec![("Cargo.toml".to_string(), make_ref(1))].into_iter());
        let req = CompiledPattern::new("Cargo.toml");
        let conflicts = idx.find_conflicts(&req);
        assert_eq!(conflicts.len(), 1);
    }

    #[test]
    fn root_exact_path_no_conflict_with_different() {
        let idx =
            ReservationIndex::build(vec![("Cargo.toml".to_string(), make_ref(1))].into_iter());
        let req = CompiledPattern::new("Cargo.lock");
        let conflicts = idx.find_conflicts(&req);
        assert!(conflicts.is_empty());
    }

    #[test]
    fn star_glob_matches_root_files() {
        let idx = ReservationIndex::build(vec![("*.toml".to_string(), make_ref(1))].into_iter());
        let req = CompiledPattern::new("Cargo.toml");
        let conflicts = idx.find_conflicts(&req);
        assert_eq!(conflicts.len(), 1);
    }

    #[test]
    fn deeply_nested_exact_path() {
        let idx = ReservationIndex::build(
            vec![("crates/db/src/schema.rs".to_string(), make_ref(1))].into_iter(),
        );
        let req = CompiledPattern::new("crates/db/src/schema.rs");
        let conflicts = idx.find_conflicts(&req);
        assert_eq!(conflicts.len(), 1);
    }

    #[test]
    fn deeply_nested_no_conflict_different_subtree() {
        let idx = ReservationIndex::build(
            vec![("crates/db/src/schema.rs".to_string(), make_ref(1))].into_iter(),
        );
        let req = CompiledPattern::new("crates/cli/src/schema.rs");
        let conflicts = idx.find_conflicts(&req);
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
        let conflicts = idx.find_conflicts(&req);
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
        let conflicts = idx.find_conflicts(&req);
        assert!(conflicts.is_empty());
    }

    #[test]
    fn first_segment_helper() {
        assert_eq!(first_segment("src/main.rs"), Some("src"));
        assert_eq!(first_segment("Cargo.toml"), Some("Cargo.toml"));
        assert_eq!(first_segment(""), None);
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
        let conflicts = idx.find_conflicts(&req);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].agent_id, 42);
        assert!(conflicts[0].exclusive);
        assert_eq!(conflicts[0].expires_ts, 999_000);
    }
}
