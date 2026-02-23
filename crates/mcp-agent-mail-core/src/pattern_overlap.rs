use globset::{GlobBuilder, GlobMatcher};

fn normalize_pattern(pattern: &str) -> String {
    let mut normalized = pattern.trim().replace('\\', "/");

    // Collapse consecutive slashes
    while normalized.contains("//") {
        normalized = normalized.replace("//", "/");
    }

    let mut slice = normalized.as_str();
    while let Some(rest) = slice.strip_prefix("./") {
        slice = rest;
    }
    slice.trim_start_matches('/').to_string()
}

#[derive(Debug, Clone)]
pub struct CompiledPattern {
    norm: String,
    matcher: Option<GlobMatcher>,
}

/// Returns `true` if the string contains glob metacharacters (`*`, `?`, `[`, `{`).
#[must_use]
pub fn has_glob_meta(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[') || s.contains('{')
}

impl CompiledPattern {
    #[must_use]
    pub fn new(raw: &str) -> Self {
        let norm = normalize_pattern(raw);
        let matcher = GlobBuilder::new(&norm)
            .literal_separator(true)
            .build()
            .ok()
            .map(|g| g.compile_matcher());
        Self { norm, matcher }
    }

    /// Returns the normalized pattern string.
    #[must_use]
    pub fn normalized(&self) -> &str {
        &self.norm
    }

    /// Returns `true` if the normalized pattern contains glob metacharacters.
    #[must_use]
    pub fn is_glob(&self) -> bool {
        has_glob_meta(&self.norm)
    }

    /// Returns the first literal segment if it doesn't contain glob chars.
    ///
    /// E.g. `"src/api/*.rs"` → `Some("src")`, `"*.rs"` → `None`.
    #[must_use]
    pub fn first_literal_segment(&self) -> Option<&str> {
        let seg = self.norm.split('/').next().unwrap_or("");
        if seg.is_empty() || has_glob_meta(seg) {
            None
        } else {
            Some(seg)
        }
    }

    /// Returns `true` if the glob matcher matches the given path string.
    ///
    /// Returns `false` if the pattern couldn't be compiled.
    #[must_use]
    pub fn matches(&self, path: &str) -> bool {
        self.matcher.as_ref().is_some_and(|m| m.is_match(path))
    }

    #[must_use]
    pub fn overlaps(&self, other: &Self) -> bool {
        if self.norm == other.norm {
            return true;
        }

        // 1. Check subset/containment (existing logic)
        // If one pattern matches the other's *string representation*, they definitely overlap.
        // This handles cases like `src/*.rs` matching `src/main.rs`.
        if let (Some(a), Some(b)) = (&self.matcher, &other.matcher)
            && (a.is_match(&other.norm) || b.is_match(&self.norm))
        {
            return true;
        }

        // 2. Heuristic check for intersecting paths/globs
        // If they don't strictly match as strings, they might still intersect
        // (e.g., intersecting globs, or directory prefix containing a file).
        segments_overlap(&self.norm, &other.norm)
    }
}

/// Heuristic check for overlap between two glob patterns.
///
/// This is conservative: it returns `true` (overlap) if it cannot prove disjointness.
///
/// Rules:
/// 1. If either pattern contains `**` (recursive), assume overlap.
/// 2. If segment counts differ (and no `**`), assume disjoint.
/// 3. Compare segments pairwise:
///    - If both are globs, assume overlap (conservative).
///    - If one is glob and one literal, check match.
///    - If both literal, check equality.
fn segments_overlap(p1: &str, p2: &str) -> bool {
    let mut i1 = p1.split('/');
    let mut i2 = p2.split('/');

    loop {
        match (i1.next(), i2.next()) {
            (Some(seg1), Some(seg2)) => {
                if seg1 == "**" || seg2 == "**" {
                    return true; // We reached a recursive glob, assume overlap from here on
                }
                if !segment_pair_overlaps(seg1, seg2) {
                    return false;
                }
            }
            _ => return true, // Ended or length mismatch: conservatively assume overlap
        }
    }
}

fn segment_pair_overlaps(s1: &str, s2: &str) -> bool {
    if s1 == s2 {
        return true;
    }

    let g1 = has_glob_meta(s1);
    let g2 = has_glob_meta(s2);

    if g1 && g2 {
        // Both are globs (e.g. `*.rs` vs `*.txt`, or `a*` vs `*b`).
        // Without a regex intersection engine, we must be conservative and assume overlap.
        // This yields false positives (blocking `*.rs` vs `*.txt`) but ensures safety.
        return true;
    }

    if g1 {
        // s1 glob, s2 literal
        return GlobBuilder::new(s1)
            .literal_separator(true)
            .build()
            .is_ok_and(|g| g.compile_matcher().is_match(s2));
    }

    if g2 {
        // s2 glob, s1 literal
        return GlobBuilder::new(s2)
            .literal_separator(true)
            .build()
            .is_ok_and(|g| g.compile_matcher().is_match(s1));
    }

    // Both literal, unequal
    false
}

/// Returns true when two glob/literal patterns overlap under Agent Mail semantics.
#[must_use]
pub fn patterns_overlap(left: &str, right: &str) -> bool {
    let left = CompiledPattern::new(left);
    let right = CompiledPattern::new(right);
    left.overlaps(&right)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlaps_is_symmetric_for_equal_norms() {
        let a = CompiledPattern::new("./src/**");
        let b = CompiledPattern::new("src/**");
        assert!(a.overlaps(&b));
        assert!(b.overlaps(&a));
    }

    #[test]
    fn overlaps_falls_back_to_equality_if_any_glob_invalid() {
        // Glob with an unclosed character class should fail to compile.
        // In that case we must not attempt matching: only equality counts.
        let invalid = CompiledPattern::new("[abc");
        let other = CompiledPattern::new("abc");
        assert!(!invalid.overlaps(&other));
        assert!(!other.overlaps(&invalid));

        let invalid_same = CompiledPattern::new(" [abc ");
        assert!(invalid.overlaps(&invalid_same));
    }

    // ── normalize_pattern tests ──────────────────────────────────────

    #[test]
    fn normalize_strips_dot_slash_prefix() {
        assert_eq!(normalize_pattern("./src/main.rs"), "src/main.rs");
        assert_eq!(normalize_pattern("././src/main.rs"), "src/main.rs");
        assert_eq!(normalize_pattern("./"), "");
    }

    #[test]
    fn normalize_converts_backslashes() {
        assert_eq!(normalize_pattern("src\\lib.rs"), "src/lib.rs");
        assert_eq!(normalize_pattern("a\\b\\c"), "a/b/c");
    }

    #[test]
    fn normalize_strips_leading_slash() {
        assert_eq!(normalize_pattern("/src/main.rs"), "src/main.rs");
    }

    #[test]
    fn normalize_trims_whitespace() {
        assert_eq!(normalize_pattern("  src/main.rs  "), "src/main.rs");
    }

    #[test]
    fn normalize_identity_for_clean_paths() {
        assert_eq!(normalize_pattern("src/main.rs"), "src/main.rs");
        assert_eq!(normalize_pattern("Cargo.toml"), "Cargo.toml");
    }

    // ── has_glob_meta tests ──────────────────────────────────────────

    #[test]
    fn has_glob_meta_detects_metacharacters() {
        assert!(has_glob_meta("*.rs"));
        assert!(has_glob_meta("src/**"));
        assert!(has_glob_meta("file?.txt"));
        assert!(has_glob_meta("[abc].rs"));
        assert!(has_glob_meta("{a,b}.rs"));
    }

    #[test]
    fn has_glob_meta_false_for_literals() {
        assert!(!has_glob_meta("src/main.rs"));
        assert!(!has_glob_meta("Cargo.toml"));
        assert!(!has_glob_meta(""));
    }

    // ── CompiledPattern basic tests ──────────────────────────────────

    #[test]
    fn compiled_pattern_normalized_accessor() {
        let p = CompiledPattern::new("./src/main.rs");
        assert_eq!(p.normalized(), "src/main.rs");
    }

    #[test]
    fn compiled_pattern_is_glob() {
        assert!(CompiledPattern::new("src/**").is_glob());
        assert!(CompiledPattern::new("*.rs").is_glob());
        assert!(!CompiledPattern::new("src/main.rs").is_glob());
        assert!(!CompiledPattern::new("Cargo.toml").is_glob());
    }

    #[test]
    fn first_literal_segment_with_prefix() {
        assert_eq!(
            CompiledPattern::new("src/api/*.rs").first_literal_segment(),
            Some("src")
        );
        assert_eq!(
            CompiledPattern::new("docs/readme.md").first_literal_segment(),
            Some("docs")
        );
    }

    #[test]
    fn first_literal_segment_none_for_root_globs() {
        assert_eq!(CompiledPattern::new("*.rs").first_literal_segment(), None);
        assert_eq!(CompiledPattern::new("**").first_literal_segment(), None);
        assert_eq!(
            CompiledPattern::new("**/*.rs").first_literal_segment(),
            None
        );
    }

    #[test]
    fn first_literal_segment_single_file() {
        assert_eq!(
            CompiledPattern::new("Cargo.toml").first_literal_segment(),
            Some("Cargo.toml")
        );
    }

    // ── CompiledPattern::matches tests ───────────────────────────────

    #[test]
    fn matches_glob_against_path() {
        let p = CompiledPattern::new("src/**/*.rs");
        assert!(p.matches("src/main.rs"));
        assert!(p.matches("src/db/schema.rs"));
        assert!(!p.matches("docs/readme.md"));
    }

    #[test]
    fn matches_exact_path() {
        let p = CompiledPattern::new("src/main.rs");
        assert!(p.matches("src/main.rs"));
        assert!(!p.matches("src/lib.rs"));
    }

    #[test]
    fn matches_returns_false_for_invalid_glob() {
        let p = CompiledPattern::new("[abc");
        assert!(!p.matches("abc"));
    }

    // ── CompiledPattern::overlaps tests ──────────────────────────────

    #[test]
    fn overlaps_exact_same_path() {
        let a = CompiledPattern::new("src/main.rs");
        let b = CompiledPattern::new("src/main.rs");
        assert!(a.overlaps(&b));
    }

    #[test]
    fn overlaps_exact_different_paths() {
        let a = CompiledPattern::new("src/main.rs");
        let b = CompiledPattern::new("src/lib.rs");
        assert!(!a.overlaps(&b));
    }

    #[test]
    fn overlaps_glob_contains_exact() {
        let glob = CompiledPattern::new("src/**");
        let exact = CompiledPattern::new("src/main.rs");
        assert!(glob.overlaps(&exact));
        assert!(exact.overlaps(&glob));
    }

    #[test]
    fn overlaps_disjoint_globs_different_prefix() {
        let a = CompiledPattern::new("src/*.rs");
        let b = CompiledPattern::new("docs/*.md");
        assert!(!a.overlaps(&b));
    }

    #[test]
    fn overlaps_conservative_for_intersecting_globs() {
        // Both globs in same directory, conservative heuristic returns true
        let a = CompiledPattern::new("src/a*");
        let b = CompiledPattern::new("src/*b");
        assert!(a.overlaps(&b));
    }

    #[test]
    fn overlaps_recursive_glob_always_overlaps() {
        let a = CompiledPattern::new("src/**/*.rs");
        let b = CompiledPattern::new("src/**/*.txt");
        // ** triggers conservative overlap assumption
        assert!(a.overlaps(&b));
    }

    // ── segments_overlap tests ───────────────────────────────────────

    #[test]
    fn segments_overlap_recursive_fast_path() {
        assert!(segments_overlap("src/**", "src/main.rs"));
        assert!(segments_overlap("**/*.rs", "src/*.rs"));
    }

    #[test]
    fn segments_overlap_different_depth() {
        // Different segment counts without ** → disjoint
        assert!(!segments_overlap("src/*.rs", "src/deep/nested/*.rs"));
    }

    #[test]
    fn segments_overlap_same_depth_disjoint_literal() {
        // Same depth, but different literal segments
        assert!(!segments_overlap("src/alpha/*.rs", "docs/beta/*.rs"));
    }

    #[test]
    fn segments_overlap_same_depth_matching() {
        // Same depth, all segments compatible
        assert!(segments_overlap("src/*.rs", "src/*.txt"));
    }

    // ── segment_pair_overlaps tests ──────────────────────────────────

    #[test]
    fn segment_pair_both_equal() {
        assert!(segment_pair_overlaps("src", "src"));
    }

    #[test]
    fn segment_pair_both_globs_conservative() {
        assert!(segment_pair_overlaps("*.rs", "*.txt"));
    }

    #[test]
    fn segment_pair_glob_matches_literal() {
        assert!(segment_pair_overlaps("*.rs", "main.rs"));
        assert!(segment_pair_overlaps("main.rs", "*.rs"));
    }

    #[test]
    fn segment_pair_glob_no_match_literal() {
        assert!(!segment_pair_overlaps("*.rs", "readme.md"));
        assert!(!segment_pair_overlaps("readme.md", "*.rs"));
    }

    #[test]
    fn segment_pair_both_literal_unequal() {
        assert!(!segment_pair_overlaps("src", "docs"));
    }

    // ── patterns_overlap convenience function ────────────────────────

    #[test]
    fn patterns_overlap_convenience_fn() {
        assert!(patterns_overlap("src/**", "src/main.rs"));
        assert!(!patterns_overlap("src/*.rs", "docs/*.md"));
        assert!(patterns_overlap("./src/main.rs", "src/main.rs"));
    }

    // ── edge cases ───────────────────────────────────────────────────

    #[test]
    fn empty_pattern() {
        let p = CompiledPattern::new("");
        assert_eq!(p.normalized(), "");
        assert!(!p.is_glob());
        assert_eq!(p.first_literal_segment(), None);
    }

    #[test]
    fn overlaps_self() {
        let p = CompiledPattern::new("src/**/*.rs");
        assert!(p.overlaps(&p));
    }

    #[test]
    fn star_glob_single_level() {
        // *.rs should not match nested paths (literal_separator = true)
        let p = CompiledPattern::new("*.rs");
        assert!(p.matches("main.rs"));
        assert!(!p.matches("src/main.rs"));
    }

    #[test]
    fn question_mark_glob() {
        let p = CompiledPattern::new("file?.txt");
        assert!(p.matches("file1.txt"));
        assert!(p.matches("fileA.txt"));
        assert!(!p.matches("file12.txt"));
    }

    #[test]
    fn brace_expansion_glob() {
        let p = CompiledPattern::new("src/*.{rs,toml}");
        assert!(p.matches("src/main.rs"));
        assert!(p.matches("src/Cargo.toml"));
        assert!(!p.matches("src/readme.md"));
    }

    #[test]
    fn compiled_pattern_debug_impl() {
        let p = CompiledPattern::new("src/**");
        let debug = format!("{p:?}");
        assert!(debug.contains("src/**"));
    }

    #[test]
    fn compiled_pattern_clone() {
        let p = CompiledPattern::new("src/**/*.rs");
        let cloned = p.clone();
        assert_eq!(cloned.normalized(), p.normalized());
        assert_eq!(cloned.is_glob(), p.is_glob());
    }
}
