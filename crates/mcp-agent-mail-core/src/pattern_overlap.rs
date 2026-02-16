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

        match (&self.matcher, &other.matcher) {
            (Some(a), Some(b)) => a.is_match(&other.norm) || b.is_match(&self.norm),
            _ => false,
        }
    }
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
