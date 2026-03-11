use globset::{GlobBuilder, GlobMatcher};
use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    sync::Arc,
};

const PATTERN_CACHE_CAPACITY: usize = 256;

thread_local! {
    static PATTERN_CACHE: RefCell<PatternCache> = RefCell::new(PatternCache::new(PATTERN_CACHE_CAPACITY));
}

#[derive(Debug)]
struct PatternCache {
    capacity: usize,
    entries: HashMap<String, Arc<CompiledPattern>>,
    order: VecDeque<String>,
}

impl PatternCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get_or_insert(&mut self, raw: &str) -> Arc<CompiledPattern> {
        if let Some(compiled) = self.entries.get(raw) {
            if let Some(pos) = self.order.iter().position(|x| x == raw)
                && let Some(key) = self.order.remove(pos)
            {
                self.order.push_back(key);
            }
            return Arc::clone(compiled);
        }

        let compiled = Arc::new(CompiledPattern::new(raw));
        if self.entries.len() >= self.capacity
            && let Some(oldest) = self.order.pop_front()
        {
            self.entries.remove(&oldest);
        }

        let key = raw.to_owned();
        self.entries.insert(key.clone(), Arc::clone(&compiled));
        self.order.push_back(key);
        compiled
    }
}

fn normalize_pattern(pattern: &str) -> String {
    let trimmed = pattern.trim();
    let mut parts: Vec<&str> = Vec::with_capacity(trimmed.len() / 4 + 1);
    for component in trimmed.split(['/', '\\']) {
        match component {
            "" | "." => {}
            ".." => {
                if !parts.is_empty() {
                    parts.pop();
                }
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

#[derive(Debug, Clone)]
pub struct CompiledPattern {
    norm: String,
    matcher: Option<GlobMatcher>,
    is_glob: bool,
    first_literal_segment_end: Option<usize>,
    segments: Vec<PatternSegment>,
}

#[derive(Debug, Clone)]
pub enum PatternSegment {
    Literal(String),
    Glob(GlobMatcher),
    Recursive, // "**"
}

impl PatternSegment {
    fn _matches_literal(&self, literal: &str) -> bool {
        match self {
            Self::Literal(l) => l == literal,
            Self::Glob(m) => m.is_match(literal),
            Self::Recursive => true,
        }
    }

    fn overlaps(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Recursive, _) | (_, Self::Recursive) => true,
            (Self::Literal(l1), Self::Literal(l2)) => l1 == l2,
            (Self::Glob(m), Self::Literal(l)) | (Self::Literal(l), Self::Glob(m)) => m.is_match(l),
            (Self::Glob(_), Self::Glob(_)) => true, // conservative
        }
    }
}

/// Returns `true` if the string contains glob metacharacters (`*`, `?`, `[`, `{`).
#[must_use]
pub fn has_glob_meta(s: &str) -> bool {
    s.bytes().any(|b| matches!(b, b'*' | b'?' | b'[' | b'{'))
}

fn first_literal_segment_end(norm: &str) -> Option<usize> {
    let seg_end = norm.find('/').unwrap_or(norm.len());
    let seg = &norm[..seg_end];
    if seg.is_empty() || has_glob_meta(seg) {
        None
    } else {
        Some(seg_end)
    }
}

fn is_directory_prefix(prefix: &str, full: &str) -> bool {
    prefix.is_empty()
        || (full.starts_with(prefix)
            && full
                .as_bytes()
                .get(prefix.len())
                .is_some_and(|b| *b == b'/'))
}

impl CompiledPattern {
    #[must_use]
    pub fn new(raw: &str) -> Self {
        let norm = normalize_pattern(raw);
        let is_glob = has_glob_meta(&norm);
        let first_literal_segment_end = first_literal_segment_end(&norm);
        let matcher = if is_glob {
            GlobBuilder::new(&norm)
                .literal_separator(true)
                .build()
                .ok()
                .map(|g| g.compile_matcher())
        } else {
            None
        };

        let segments = norm
            .split('/')
            .map(|s| {
                if s == "**" {
                    PatternSegment::Recursive
                } else if has_glob_meta(s) {
                    let m = GlobBuilder::new(s)
                        .literal_separator(true)
                        .build()
                        .ok()
                        .map(|g| g.compile_matcher());
                    if let Some(m) = m {
                        PatternSegment::Glob(m)
                    } else {
                        PatternSegment::Literal(s.to_string())
                    }
                } else {
                    PatternSegment::Literal(s.to_string())
                }
            })
            .collect();

        Self {
            norm,
            matcher,
            is_glob,
            first_literal_segment_end,
            segments,
        }
    }

    /// Returns the normalized pattern string.
    #[must_use]
    pub fn normalized(&self) -> &str {
        &self.norm
    }

    /// Returns `true` if the normalized pattern contains glob metacharacters.
    #[must_use]
    pub const fn is_glob(&self) -> bool {
        self.is_glob
    }

    /// Returns `true` when this pattern can participate in literal/glob matching.
    ///
    /// Exact paths are always matchable. Glob patterns are only matchable when
    /// the glob compiled successfully.
    #[must_use]
    pub const fn is_matchable(&self) -> bool {
        !self.is_glob || self.matcher.is_some()
    }

    /// Returns the first literal segment if it doesn't contain glob chars.
    ///
    /// E.g. `"src/api/*.rs"` → `Some("src")`, `"*.rs"` → `None`.
    #[must_use]
    pub fn first_literal_segment(&self) -> Option<&str> {
        self.first_literal_segment_end.map(|end| &self.norm[..end])
    }

    /// Returns `true` if the glob matcher matches the given path string.
    ///
    /// Returns `false` if the pattern couldn't be compiled.
    #[must_use]
    pub fn matches(&self, path: &str) -> bool {
        self.matcher.as_ref().is_some_and(|m| m.is_match(path))
            || (!self.is_glob && self.norm == path)
    }

    /// Returns the pre-compiled segments of this pattern.
    #[must_use]
    pub fn segments(&self) -> &[PatternSegment] {
        &self.segments
    }

    #[must_use]
    pub fn overlaps(&self, other: &Self) -> bool {
        if self.norm == other.norm {
            return true;
        }

        // An empty pattern (normalized from "/" or ".") represents the root directory.
        // Reserving the root directory overlaps with everything in the workspace.
        if self.norm.is_empty() || other.norm.is_empty() {
            return true;
        }

        // Distinct exact literals overlap only when one is a slash-boundary
        // directory prefix of the other.
        if !self.is_glob && !other.is_glob {
            return is_directory_prefix(&self.norm, &other.norm)
                || is_directory_prefix(&other.norm, &self.norm);
        }

        // 1. Check subset/containment (existing logic)
        // If one pattern matches the other's *string representation*, they definitely overlap.
        // This handles cases like `src/*.rs` matching `src/main.rs`.
        if let Some(a) = &self.matcher
            && (!other.is_glob || other.matcher.is_some())
            && a.is_match(&other.norm)
        {
            return true;
        }
        if let Some(b) = &other.matcher
            && (!self.is_glob || self.matcher.is_some())
            && b.is_match(&self.norm)
        {
            return true;
        }

        // Invalid glob patterns (failed compile) do not match anything.
        if (self.is_glob && self.matcher.is_none()) || (other.is_glob && other.matcher.is_none()) {
            return false;
        }

        // If both patterns start with different literal first segments, they are disjoint.
        if let (Some(left_end), Some(right_end)) = (
            self.first_literal_segment_end,
            other.first_literal_segment_end,
        ) && self.norm[..left_end] != other.norm[..right_end]
        {
            return false;
        }

        // 2. Heuristic check for intersecting paths/globs
        // If they don't strictly match as strings, they might still intersect
        // (e.g., intersecting globs, or directory prefix containing a file).
        segments_overlap(&self.segments, &other.segments)
    }
}

/// Heuristic check for overlap between two glob patterns.
///
/// This is conservative: it returns `true` (overlap) if it cannot prove disjointness.
fn segments_overlap(s1: &[PatternSegment], s2: &[PatternSegment]) -> bool {
    let mut i1 = s1.iter();
    let mut i2 = s2.iter();

    loop {
        match (i1.next(), i2.next()) {
            (Some(seg1), Some(seg2)) => {
                if matches!(seg1, PatternSegment::Recursive)
                    || matches!(seg2, PatternSegment::Recursive)
                {
                    return true; // We reached a recursive glob, assume overlap from here on
                }
                if !seg1.overlaps(seg2) {
                    return false;
                }
            }
            _ => return true, // Ended or length mismatch: conservatively assume overlap
        }
    }
}

/// Returns true when two glob/literal patterns overlap under Agent Mail semantics.
#[must_use]
pub fn patterns_overlap(left: &str, right: &str) -> bool {
    PATTERN_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let left = cache.get_or_insert(left);
        let right = cache.get_or_insert(right);
        left.overlaps(&right)
    })
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

        let invalid_other = CompiledPattern::new("[def");
        assert!(!invalid.overlaps(&invalid_other));
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

    #[test]
    fn normalize_strips_trailing_slash() {
        assert_eq!(normalize_pattern("src/"), "src");
        assert_eq!(normalize_pattern("src/api/"), "src/api");
        assert_eq!(normalize_pattern("/"), "");
    }

    #[test]
    fn normalize_collapses_dot_dot() {
        assert_eq!(normalize_pattern("src/../docs/readme.md"), "docs/readme.md");
        assert_eq!(
            normalize_pattern("app/models/../../api/users.py"),
            "api/users.py"
        );
        assert_eq!(normalize_pattern("../evil"), "evil");
        assert_eq!(normalize_pattern("../../evil"), "evil");
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
    fn overlaps_exact_prefix_paths_do_not_overlap() {
        let a = CompiledPattern::new("src/main");
        let b = CompiledPattern::new("src/main.rs");
        assert!(!a.overlaps(&b));
        assert!(!b.overlaps(&a));
    }

    #[test]
    fn overlaps_exact_directory_prefix_paths_overlap() {
        let a = CompiledPattern::new("src");
        let b = CompiledPattern::new("src/main.rs");
        assert!(a.overlaps(&b));
        assert!(b.overlaps(&a));

        let c = CompiledPattern::new("src/");
        assert!(c.overlaps(&b));
        assert!(b.overlaps(&c));
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
        let a = CompiledPattern::new("src/**");
        let b = CompiledPattern::new("src/main.rs");
        assert!(segments_overlap(a.segments(), b.segments()));

        let c = CompiledPattern::new("**/*.rs");
        let d = CompiledPattern::new("src/*.rs");
        assert!(segments_overlap(c.segments(), d.segments()));
    }

    #[test]
    fn segments_overlap_different_depth() {
        // Different segment counts without ** → disjoint
        let a = CompiledPattern::new("src/*.rs");
        let b = CompiledPattern::new("src/deep/nested/*.rs");
        assert!(!segments_overlap(a.segments(), b.segments()));
    }

    #[test]
    fn segments_overlap_same_depth_disjoint_literal() {
        // Same depth, but different literal segments
        let a = CompiledPattern::new("src/alpha/*.rs");
        let b = CompiledPattern::new("docs/beta/*.rs");
        assert!(!segments_overlap(a.segments(), b.segments()));
    }

    #[test]
    fn segments_overlap_same_depth_matching() {
        // Same depth, all segments compatible
        let a = CompiledPattern::new("src/*.rs");
        let b = CompiledPattern::new("src/*.txt");
        assert!(segments_overlap(a.segments(), b.segments()));
    }

    // ── segment_pair_overlaps tests ──────────────────────────────────

    #[test]
    fn segment_pair_both_equal() {
        let s1 = PatternSegment::Literal("src".to_string());
        let s2 = PatternSegment::Literal("src".to_string());
        assert!(s1.overlaps(&s2));
    }

    #[test]
    fn segment_pair_both_globs_conservative() {
        let s1 = PatternSegment::Glob(
            GlobBuilder::new("*.rs")
                .literal_separator(true)
                .build()
                .unwrap()
                .compile_matcher(),
        );
        let s2 = PatternSegment::Glob(
            GlobBuilder::new("*.txt")
                .literal_separator(true)
                .build()
                .unwrap()
                .compile_matcher(),
        );
        assert!(s1.overlaps(&s2));
    }

    #[test]
    fn segment_pair_glob_matches_literal() {
        let s1 = PatternSegment::Glob(
            GlobBuilder::new("*.rs")
                .literal_separator(true)
                .build()
                .unwrap()
                .compile_matcher(),
        );
        let s2 = PatternSegment::Literal("main.rs".to_string());
        assert!(s1.overlaps(&s2));
        assert!(s2.overlaps(&s1));
    }

    #[test]
    fn segment_pair_glob_no_match_literal() {
        let s1 = PatternSegment::Glob(
            GlobBuilder::new("*.rs")
                .literal_separator(true)
                .build()
                .unwrap()
                .compile_matcher(),
        );
        let s2 = PatternSegment::Literal("readme.md".to_string());
        assert!(!s1.overlaps(&s2));
        assert!(!s2.overlaps(&s1));
    }

    #[test]
    fn segment_pair_both_literal_unequal() {
        let s1 = PatternSegment::Literal("src".to_string());
        let s2 = PatternSegment::Literal("docs".to_string());
        assert!(!s1.overlaps(&s2));
    }

    // ── patterns_overlap convenience function ────────────────────────

    #[test]
    fn patterns_overlap_convenience_fn() {
        assert!(patterns_overlap("src/**", "src/main.rs"));
        assert!(!patterns_overlap("src/*.rs", "docs/*.md"));
        assert!(patterns_overlap("./src/main.rs", "src/main.rs"));
    }

    #[test]
    fn patterns_overlap_repeated_calls_are_stable() {
        for _ in 0..32 {
            assert!(patterns_overlap("src/**/*.rs", "src/main.rs"));
            assert!(!patterns_overlap("docs/*.md", "src/main.rs"));
            assert!(patterns_overlap("src", "src/main.rs"));
        }
    }

    #[test]
    fn patterns_overlap_cache_eviction_preserves_correctness() {
        for i in 0..(PATTERN_CACHE_CAPACITY + 64) {
            let left = format!("dir{i}/**/*.rs");
            let right = format!("dir{i}/main.rs");
            assert!(patterns_overlap(&left, &right));
        }
        assert!(patterns_overlap("src/**/*.rs", "src/main.rs"));
        assert!(!patterns_overlap("src/*.rs", "docs/readme.md"));
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
