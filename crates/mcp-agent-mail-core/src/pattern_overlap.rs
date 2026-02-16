use globset::{Glob, GlobMatcher};

fn normalize_pattern(pattern: &str) -> String {
    let mut normalized = pattern.trim().replace('\\', "/");
    while normalized.starts_with("./") {
        normalized = normalized[2..].to_string();
    }
    normalized.trim_start_matches('/').to_string()
}

#[derive(Debug, Clone)]
pub struct CompiledPattern {
    norm: String,
    matcher: Option<GlobMatcher>,
}

/// Returns `true` if the string contains glob metacharacters (`*`, `?`, `[`, `{`).
pub fn has_glob_meta(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[') || s.contains('{')
}

impl CompiledPattern {
    pub fn new(raw: &str) -> Self {
        let norm = normalize_pattern(raw);
        let matcher = Glob::new(&norm).ok().map(|g| g.compile_matcher());
        Self { norm, matcher }
    }

    /// Returns the normalized pattern string.
    pub fn normalized(&self) -> &str {
        &self.norm
    }

    /// Returns `true` if the normalized pattern contains glob metacharacters.
    pub fn is_glob(&self) -> bool {
        has_glob_meta(&self.norm)
    }

    /// Returns the first literal segment if it doesn't contain glob chars.
    ///
    /// E.g. `"src/api/*.rs"` → `Some("src")`, `"*.rs"` → `None`.
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
    pub fn matches(&self, path: &str) -> bool {
        self.matcher.as_ref().is_some_and(|m| m.is_match(path))
    }

    pub fn overlaps(&self, other: &Self) -> bool {
        if self.norm == other.norm {
            return true;
        }

        // 1. Check subset/containment (existing logic)
        // If one pattern matches the other's *string representation*, they definitely overlap.
        // This handles cases like `src/*.rs` matching `src/main.rs`.
        match (&self.matcher, &other.matcher) {
            (Some(a), Some(b)) => {
                if a.is_match(&other.norm) || b.is_match(&self.norm) {
                    return true;
                }
            }
            _ => return false,
        }

        // 2. Heuristic check for intersecting globs (e.g. `src/a*` vs `src/*b`)
        // If both are globs and neither strictly matches the other as a string,
        // they might still intersect.
        if self.is_glob() && other.is_glob() {
            return segments_overlap(&self.norm, &other.norm);
        }

        false
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
    // fast path for recursive globs
    if p1.contains("**") || p2.contains("**") {
        return true;
    }

    let s1: Vec<&str> = p1.split('/').collect();
    let s2: Vec<&str> = p2.split('/').collect();

    if s1.len() != s2.len() {
        return false;
    }

    for (seg1, seg2) in s1.iter().zip(s2.iter()) {
        if !segment_pair_overlaps(seg1, seg2) {
            return false;
        }
    }

    true
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
        return match Glob::new(s1) {
            Ok(g) => g.compile_matcher().is_match(s2),
            Err(_) => false, // Invalid glob treated as non-matching
        };
    }

    if g2 {
        // s2 glob, s1 literal
        return match Glob::new(s2) {
            Ok(g) => g.compile_matcher().is_match(s1),
            Err(_) => false,
        };
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
    use super::CompiledPattern;

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
}
