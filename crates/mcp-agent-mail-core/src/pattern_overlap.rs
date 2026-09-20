use globset::{GlobBuilder, GlobMatcher};
use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    sync::Arc,
};

const PATTERN_CACHE_CAPACITY: usize = 4096;

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
        if let Some(compiled) = self.entries.get(raw).cloned() {
            // Move to back of order queue (LRU)
            if let Some(pos) = self.order.iter().position(|x| x == raw) {
                let val = self.order.remove(pos).unwrap();
                self.order.push_back(val);
            }
            return compiled;
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
    Glob { raw: String, matcher: GlobMatcher },
    Recursive, // "**"
}

impl PatternSegment {
    fn _matches_literal(&self, literal: &str) -> bool {
        match self {
            Self::Literal(l) => {
                if cfg!(any(target_os = "macos", target_os = "windows")) {
                    l.eq_ignore_ascii_case(literal)
                } else {
                    l == literal
                }
            }
            Self::Glob { matcher, .. } => matcher.is_match(literal),
            Self::Recursive => true,
        }
    }

    fn overlaps(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Literal(l1), Self::Literal(l2)) => {
                if cfg!(any(target_os = "macos", target_os = "windows")) {
                    l1.eq_ignore_ascii_case(l2)
                } else {
                    l1 == l2
                }
            }
            (Self::Glob { matcher, .. }, Self::Literal(l))
            | (Self::Literal(l), Self::Glob { matcher, .. }) => matcher.is_match(l),
            (
                Self::Glob {
                    raw: left_raw,
                    matcher: _,
                },
                Self::Glob {
                    raw: right_raw,
                    matcher: _,
                },
            ) => simple_glob_patterns_overlap(left_raw, right_raw).unwrap_or(true),
            (Self::Recursive, _) | (_, Self::Recursive) => true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SimpleGlobToken {
    Literal(char),
    AnyChar,
    AnyString,
}

const fn fold_ascii_case(ch: char) -> char {
    if cfg!(any(target_os = "macos", target_os = "windows")) {
        ch.to_ascii_lowercase()
    } else {
        ch
    }
}

fn parse_simple_glob_tokens(segment: &str) -> Option<Vec<SimpleGlobToken>> {
    let mut tokens = Vec::with_capacity(segment.len());
    for ch in segment.chars() {
        match ch {
            '*' => {
                if tokens.last() != Some(&SimpleGlobToken::AnyString) {
                    tokens.push(SimpleGlobToken::AnyString);
                }
            }
            '?' => tokens.push(SimpleGlobToken::AnyChar),
            '[' | ']' | '{' | '}' => return None,
            other => tokens.push(SimpleGlobToken::Literal(fold_ascii_case(other))),
        }
    }
    Some(tokens)
}

/// Intersect sequences whose repeat token accepts zero or more arbitrary items.
/// `compatible` must be symmetric and is used only for two non-repeat items.
/// Both character globs (`*`) and path segments (`**`) have this recurrence.
///
/// Keep one suffix-DP row, not a matrix or a recursive call per input item.
/// Auxiliary space is O(min(n, m)); worst-case DP work remains O(n*m).
/// Mandatory anchored items are consumed first, keeping fixed-width inputs and
/// anchored mismatches linear even when the original inputs are very long.
fn sequence_patterns_overlap<T>(
    mut left: &[T],
    mut right: &[T],
    is_repeat: impl Fn(&T) -> bool,
    compatible: impl Fn(&T, &T) -> bool,
) -> bool {
    while let (Some((a, rest_a)), Some((b, rest_b))) =
        (left.split_first(), right.split_first())
    {
        if is_repeat(a) || is_repeat(b) {
            break;
        }
        if !compatible(a, b) {
            return false;
        }
        left = rest_a;
        right = rest_b;
    }
    while let (Some((a, rest_a)), Some((b, rest_b))) =
        (left.split_last(), right.split_last())
    {
        if is_repeat(a) || is_repeat(b) {
            break;
        }
        if !compatible(a, b) {
            return false;
        }
        left = rest_a;
        right = rest_b;
    }
    if left.is_empty() {
        return right.iter().all(&is_repeat);
    }
    if right.is_empty() {
        return left.iter().all(&is_repeat);
    }
    if left.iter().all(&is_repeat) || right.iter().all(&is_repeat) {
        return true;
    }

    // Intersection is symmetric. Put the shorter residual sequence in the
    // retained row; a deep path against a tiny recursive glob needs tiny space.
    let (rows, columns) = if left.len() >= right.len() {
        (left, right)
    } else {
        (right, left)
    };
    let end = columns.len();
    let mut row = vec![false; end + 1];
    row[end] = true;
    for (j, item) in columns.iter().enumerate().rev() {
        row[j] = is_repeat(item) && row[j + 1];
    }
    for a in rows.iter().rev() {
        let repeats = is_repeat(a);
        let mut diagonal = row[end];
        row[end] = repeats && diagonal;
        for (j, b) in columns.iter().enumerate().rev() {
            let below = row[j];
            // A repeat can be empty or absorb the other sequence's next item.
            // Otherwise both items must match and both suffixes must overlap.
            row[j] = if repeats || is_repeat(b) {
                below || row[j + 1]
            } else {
                diagonal && compatible(a, b)
            };
            diagonal = below;
        }
    }
    row[0]
}

fn simple_glob_tokens_overlap(left: &[SimpleGlobToken], right: &[SimpleGlobToken]) -> bool {
    sequence_patterns_overlap(
        left,
        right,
        |token| *token == SimpleGlobToken::AnyString,
        |a, b| match (a, b) {
            (SimpleGlobToken::Literal(a), SimpleGlobToken::Literal(b)) => a == b,
            _ => true,
        },
    )
}

fn simple_glob_patterns_overlap(left: &str, right: &str) -> Option<bool> {
    let left_tokens = parse_simple_glob_tokens(left)?;
    let right_tokens = parse_simple_glob_tokens(right)?;
    Some(simple_glob_tokens_overlap(&left_tokens, &right_tokens))
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
    if prefix.is_empty() {
        return true;
    }
    // An unrelated Unicode path can have a scalar straddling prefix.len().
    // Compare bytes instead of slicing that path at a non-character boundary.
    let Some(head) = full.as_bytes().get(..prefix.len()) else {
        return false;
    };
    let is_prefix = if cfg!(any(target_os = "macos", target_os = "windows")) {
        head.eq_ignore_ascii_case(prefix.as_bytes())
    } else {
        head == prefix.as_bytes()
    };
    is_prefix
        && full
            .as_bytes()
            .get(prefix.len())
            .is_some_and(|b| *b == b'/')
}

impl CompiledPattern {
    #[must_use]
    pub fn new(raw: &str) -> Self {
        let norm = normalize_pattern(raw);
        let is_glob = has_glob_meta(&norm);
        let first_literal_segment_end = first_literal_segment_end(&norm);
        // On case-insensitive filesystems (macOS HFS+/APFS, Windows NTFS),
        // glob matching must be case-insensitive to correctly detect conflicts
        // between e.g. src/Main.rs and src/main.rs.
        let case_insensitive = cfg!(any(target_os = "macos", target_os = "windows"));

        let matcher = if is_glob {
            GlobBuilder::new(&norm)
                .literal_separator(true)
                .case_insensitive(case_insensitive)
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
                        .case_insensitive(case_insensitive)
                        .build()
                        .ok()
                        .map(|g| g.compile_matcher());
                    m.map_or_else(
                        || PatternSegment::Literal(s.to_string()),
                        |matcher| PatternSegment::Glob {
                            raw: s.to_string(),
                            matcher,
                        },
                    )
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

    /// Get a compiled pattern from the thread-local cache, or compile it if missing.
    #[must_use]
    pub fn cached(raw: &str) -> Arc<Self> {
        PATTERN_CACHE.with(|cache| cache.borrow_mut().get_or_insert(raw))
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
        let exact_match = if cfg!(any(target_os = "macos", target_os = "windows")) {
            self.norm.eq_ignore_ascii_case(&other.norm)
        } else {
            self.norm == other.norm
        };
        if exact_match {
            return true;
        }

        if !self.is_glob && is_directory_prefix(&self.norm, &other.norm) {
            return true;
        }

        if !other.is_glob && is_directory_prefix(&other.norm, &self.norm) {
            return true;
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
        ) {
            let left_seg = &self.norm[..left_end];
            let right_seg = &other.norm[..right_end];
            let mismatch = if cfg!(any(target_os = "macos", target_os = "windows")) {
                !left_seg.eq_ignore_ascii_case(right_seg)
            } else {
                left_seg != right_seg
            };
            if mismatch {
                return false;
            }
        }

        // 2. Heuristic check for intersecting paths/globs
        // If they don't strictly match as strings, they might still intersect
        // (e.g., intersecting globs, or directory prefix containing a file).
        segments_overlap(&self.segments, &other.segments)
    }
}

/// Heuristic check for overlap between two glob patterns.
///
/// This precisely handles `*`/`?` segment globs and still respects path depth:
/// `*` only covers a single segment, while `**` can absorb arbitrary depth
/// mismatches. More complex segment syntax like classes/alternation remains
/// conservative.
fn segments_overlap(s1: &[PatternSegment], s2: &[PatternSegment]) -> bool {
    sequence_patterns_overlap(
        s1,
        s2,
        |segment| matches!(segment, PatternSegment::Recursive),
        PatternSegment::overlaps,
    )
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
    fn overlaps_detects_intersecting_simple_globs() {
        let a = CompiledPattern::new("src/a*");
        let b = CompiledPattern::new("src/*b");
        assert!(a.overlaps(&b));
    }

    #[test]
    fn overlaps_recursive_globs_with_disjoint_suffixes_do_not_overlap() {
        let a = CompiledPattern::new("src/**/*.rs");
        let b = CompiledPattern::new("src/**/*.txt");
        assert!(!a.overlaps(&b));
        assert!(!b.overlaps(&a));
    }

    #[test]
    fn overlaps_single_level_glob_does_not_hit_deeper_exact_path() {
        let a = CompiledPattern::new("src/auth/*");
        let b = CompiledPattern::new("src/auth/sub/file.rs");
        assert!(!a.overlaps(&b));
        assert!(!b.overlaps(&a));
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
        let a = CompiledPattern::new("src/alpha/*.rs");
        let b = CompiledPattern::new("docs/beta/*.rs");
        assert!(!segments_overlap(a.segments(), b.segments()));
    }

    #[test]
    fn segments_overlap_same_depth_disjoint_simple_globs() {
        let a = CompiledPattern::new("src/*.rs");
        let b = CompiledPattern::new("src/*.txt");
        assert!(!segments_overlap(a.segments(), b.segments()));
    }

    #[test]
    fn segments_overlap_same_depth_intersecting_simple_globs() {
        let a = CompiledPattern::new("src/a*");
        let b = CompiledPattern::new("src/*b");
        assert!(segments_overlap(a.segments(), b.segments()));
    }

    #[test]
    fn segments_overlap_single_level_glob_does_not_match_deeper_exact_path() {
        let a = CompiledPattern::new("src/auth/*");
        let b = CompiledPattern::new("src/auth/sub/file.rs");
        assert!(!segments_overlap(a.segments(), b.segments()));
        assert!(!segments_overlap(b.segments(), a.segments()));
    }

    // ── segment_pair_overlaps tests ──────────────────────────────────

    #[test]
    fn segment_pair_both_equal() {
        let s1 = PatternSegment::Literal("src".to_string());
        let s2 = PatternSegment::Literal("src".to_string());
        assert!(s1.overlaps(&s2));
    }

    #[test]
    fn segment_pair_both_globs_detects_disjoint_simple_patterns() {
        let s1 = PatternSegment::Glob {
            raw: "*.rs".to_string(),
            matcher: GlobBuilder::new("*.rs")
                .literal_separator(true)
                .build()
                .unwrap()
                .compile_matcher(),
        };
        let s2 = PatternSegment::Glob {
            raw: "*.txt".to_string(),
            matcher: GlobBuilder::new("*.txt")
                .literal_separator(true)
                .build()
                .unwrap()
                .compile_matcher(),
        };
        assert!(!s1.overlaps(&s2));
    }

    #[test]
    fn segment_pair_both_globs_detects_intersection_when_witness_exists() {
        let s1 = PatternSegment::Glob {
            raw: "a*".to_string(),
            matcher: GlobBuilder::new("a*")
                .literal_separator(true)
                .build()
                .unwrap()
                .compile_matcher(),
        };
        let s2 = PatternSegment::Glob {
            raw: "*b".to_string(),
            matcher: GlobBuilder::new("*b")
                .literal_separator(true)
                .build()
                .unwrap()
                .compile_matcher(),
        };
        assert!(s1.overlaps(&s2));
    }

    #[test]
    fn segment_pair_glob_matches_literal() {
        let s1 = PatternSegment::Glob {
            raw: "*.rs".to_string(),
            matcher: GlobBuilder::new("*.rs")
                .literal_separator(true)
                .build()
                .unwrap()
                .compile_matcher(),
        };
        let s2 = PatternSegment::Literal("main.rs".to_string());
        assert!(s1.overlaps(&s2));
        assert!(s2.overlaps(&s1));
    }

    #[test]
    fn segment_pair_glob_no_match_literal() {
        let s1 = PatternSegment::Glob {
            raw: "*.rs".to_string(),
            matcher: GlobBuilder::new("*.rs")
                .literal_separator(true)
                .build()
                .unwrap()
                .compile_matcher(),
        };
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
    fn patterns_overlap_respects_single_level_glob_depth() {
        assert!(!patterns_overlap("src/auth/*", "src/auth/sub/file.rs"));
        assert!(patterns_overlap("src/*/foo.rs", "src/bar/*.rs"));
    }

    #[test]
    fn patterns_overlap_rejects_disjoint_simple_sibling_globs() {
        assert!(!patterns_overlap("src/*.rs", "src/*.txt"));
        assert!(!patterns_overlap("src/**/*.rs", "src/**/*.txt"));
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

    // Independent oracle: intersect two small token automata using explicit
    // epsilon/consuming transitions, rather than the production DP recurrence.
    fn automaton_overlap(left: &[SimpleGlobToken], right: &[SimpleGlobToken]) -> bool {
        use std::collections::HashSet;

        let mut pending = vec![(0, 0)];
        let mut seen = HashSet::new();
        while let Some((i, j)) = pending.pop() {
            if !seen.insert((i, j)) {
                continue;
            }
            if i == left.len() && j == right.len() {
                return true;
            }
            let a = left.get(i);
            let b = right.get(j);
            let repeat_a = a == Some(&SimpleGlobToken::AnyString);
            let repeat_b = b == Some(&SimpleGlobToken::AnyString);
            if repeat_a {
                pending.push((i + 1, j));
            }
            if repeat_b {
                pending.push((i, j + 1));
            }
            if let (Some(a), Some(b)) = (a, b) {
                let compatible = match (a, b) {
                    (SimpleGlobToken::Literal(a), SimpleGlobToken::Literal(b)) => a == b,
                    _ => true,
                };
                if compatible {
                    pending.push((i + usize::from(!repeat_a), j + usize::from(!repeat_b)));
                }
            }
        }
        false
    }

    fn short_token_patterns() -> Vec<Vec<SimpleGlobToken>> {
        let alphabet = [
            SimpleGlobToken::Literal('a'),
            SimpleGlobToken::Literal('b'),
            SimpleGlobToken::AnyChar,
            SimpleGlobToken::AnyString,
        ];
        let mut patterns = Vec::new();
        for length in 0_u32..=3 {
            for mut code in 0..alphabet.len().pow(length) {
                let mut pattern = Vec::new();
                for _ in 0..length {
                    pattern.push(alphabet[code % alphabet.len()]);
                    code /= alphabet.len();
                }
                patterns.push(pattern);
            }
        }
        patterns
    }

    #[test]
    fn token_overlap_matches_independent_automaton_exhaustively() {
        let patterns = short_token_patterns();
        for left in &patterns {
            for right in &patterns {
                let expected = automaton_overlap(left, right);
                assert_eq!(simple_glob_tokens_overlap(left, right), expected);
                assert_eq!(simple_glob_tokens_overlap(right, left), expected);
            }
        }
    }

    #[test]
    fn path_overlap_matches_independent_automaton_exhaustively() {
        let tokens = short_token_patterns();
        let paths: Vec<Vec<PatternSegment>> = tokens
            .iter()
            .map(|pattern| {
                pattern
                    .iter()
                    .map(|token| match token {
                        SimpleGlobToken::Literal(ch) => PatternSegment::Literal(ch.to_string()),
                        SimpleGlobToken::AnyString => PatternSegment::Recursive,
                        SimpleGlobToken::AnyChar => PatternSegment::Glob {
                            raw: "?".to_string(),
                            matcher: GlobBuilder::new("?")
                                .literal_separator(true)
                                .build()
                                .unwrap()
                                .compile_matcher(),
                        },
                    })
                    .collect()
            })
            .collect();
        for (i, left) in paths.iter().enumerate() {
            for (j, right) in paths.iter().enumerate() {
                assert_eq!(segments_overlap(left, right), automaton_overlap(&tokens[i], &tokens[j]));
            }
        }
    }

    #[test]
    fn long_fixed_width_tokens_need_no_quadratic_workspace() {
        let left = vec![SimpleGlobToken::Literal('a'); 100_000];
        let any = vec![SimpleGlobToken::AnyChar; 100_000];
        assert!(simple_glob_tokens_overlap(&left, &any));
        assert!(!simple_glob_tokens_overlap(&left, &any[..99_999]));
        let mut different = left.clone();
        different[99_999] = SimpleGlobToken::Literal('b');
        assert!(!simple_glob_tokens_overlap(&left, &different));
    }

    #[test]
    fn long_repeat_pattern_runs_on_a_small_stack() {
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(|| {
                let mut left = vec![SimpleGlobToken::AnyString];
                left.extend(std::iter::repeat_n(SimpleGlobToken::Literal('a'), 50_000));
                left.extend([SimpleGlobToken::Literal('b'), SimpleGlobToken::AnyString]);
                let right = [
                    SimpleGlobToken::Literal('a'),
                    SimpleGlobToken::AnyString,
                    SimpleGlobToken::Literal('b'),
                ];
                assert!(simple_glob_tokens_overlap(&left, &right));
                assert!(simple_glob_tokens_overlap(&right, &left));
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn deep_recursive_path_runs_on_a_small_stack() {
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(|| {
                let mut left = vec![PatternSegment::Recursive];
                left.extend(std::iter::repeat_n(
                    PatternSegment::Literal("part".to_string()),
                    20_000,
                ));
                left.extend([
                    PatternSegment::Literal("end".to_string()),
                    PatternSegment::Recursive,
                ]);
                let right = [
                    PatternSegment::Literal("start".to_string()),
                    PatternSegment::Recursive,
                    PatternSegment::Literal("end".to_string()),
                ];
                assert!(segments_overlap(&left, &right));
                assert!(segments_overlap(&right, &left));
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn anchored_mismatches_are_rejected_before_grid_work() {
        use std::cell::Cell;

        for (left, right) in [
            (format!("a*{}*", "a".repeat(10_000)), "b*a*".to_string()),
            (format!("*{}*a", "a".repeat(10_000)), "*a*b".to_string()),
        ] {
            let left = parse_simple_glob_tokens(&left).unwrap();
            let right = parse_simple_glob_tokens(&right).unwrap();
            let comparisons = Cell::new(0_usize);
            assert!(!sequence_patterns_overlap(
                &left,
                &right,
                |token| *token == SimpleGlobToken::AnyString,
                |a, b| {
                    comparisons.set(comparisons.get() + 1);
                    a == b
                },
            ));
            assert_eq!(comparisons.get(), 1);
        }
    }

    #[test]
    fn nullable_and_repeated_wildcards_preserve_overlap_semantics() {
        for (left, right, expected) in [
            ("**", "", true),
            ("*a*", "", false),
            ("*", "a*", true),
            ("?", "", false),
            ("*a", "*b", false),
            ("a*", "*b", true),
            ("a**b", "a*b", true),
        ] {
            assert_eq!(simple_glob_patterns_overlap(left, right), Some(expected));
            assert_eq!(simple_glob_patterns_overlap(right, left), Some(expected));
        }
    }

    #[test]
    fn unicode_directory_prefix_checks_never_split_a_scalar() {
        for (prefix, full) in [("a", "é/file"), ("é", "猫/file"), ("abc", "🦀/file")] {
            assert!(!full.is_char_boundary(prefix.len()));
            assert!(!is_directory_prefix(prefix, full));
            assert!(!patterns_overlap(prefix, full));
            assert!(!patterns_overlap(full, prefix));
        }
    }

    #[test]
    fn directory_prefix_keeps_unicode_ancestors_and_platform_case_rules() {
        assert!(is_directory_prefix("", "猫/file"));
        assert!(is_directory_prefix("猫", "猫/file"));
        assert!(!is_directory_prefix("猫", "猫"));
        assert!(!is_directory_prefix("src", "source/file"));
        assert!(!is_directory_prefix("src/long", "src"));
        assert_eq!(
            is_directory_prefix("Src", "src/file"),
            cfg!(any(target_os = "macos", target_os = "windows"))
        );
    }

    #[test]
    fn compiled_deep_paths_keep_glob_intersection_and_suffix_separation() {
        let prefix = "part/".repeat(256);
        assert!(patterns_overlap(&format!("{prefix}a*"), &format!("{prefix}*b")));
        assert!(!patterns_overlap(
            &format!("{prefix}**/*.rs"),
            &format!("{prefix}**/*.txt")
        ));
        assert!(!patterns_overlap(
            &format!("{prefix}*"),
            &format!("{prefix}nested/file")
        ));
    }
}
