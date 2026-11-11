//! Bounded, redacted diagnostic previews shared by the native and browser dashboards.

use serde_json::Value;
use std::path::Path;

pub(crate) const MASK_REDACTED: &str = "<redacted>";
pub(crate) const DIAGNOSTIC_TRUNCATED: &str = "<truncated>";
/// Diagnostics retain a preview, never another copy of an entire request or
/// response. The byte budget is complemented by node, depth and key limits so
/// wide JSON containing tiny values cannot hide unbounded allocation overhead.
pub(crate) const DIAGNOSTIC_JSON_BYTES: usize = 16 * 1024;
pub(crate) const DIAGNOSTIC_JSON_NODES: usize = 256;
pub(crate) const DIAGNOSTIC_STRING_BYTES: usize = 2048;
pub(crate) const DIAGNOSTIC_KEY_BYTES: usize = 256;

/// Allocate only the retained UTF-8 preview. Truncating an owned `String`
/// would keep its original, potentially multi-megabyte backing allocation.
pub(crate) fn bounded_diagnostic_text(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    const SUFFIX: &str = "... [truncated]";
    let mut end = max_bytes.saturating_sub(SUFFIX.len());
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    let mut preview = String::with_capacity(max_bytes);
    preview.push_str(&value[..end]);
    preview.push_str(&SUFFIX[..SUFFIX.len().min(max_bytes)]);
    preview
}

struct DiagnosticJsonBudget {
    bytes: usize,
    nodes: usize,
}

/// Measure an arbitrary-precision JSON number without allocating its text.
pub(crate) fn diagnostic_number_bytes(number: &serde_json::Number, limit: usize) -> Option<usize> {
    struct Counter {
        bytes: usize,
        limit: usize,
    }
    impl std::fmt::Write for Counter {
        fn write_str(&mut self, value: &str) -> std::fmt::Result {
            self.bytes = self.bytes.saturating_add(value.len());
            if self.bytes > self.limit {
                Err(std::fmt::Error)
            } else {
                Ok(())
            }
        }
    }
    let mut counter = Counter { bytes: 0, limit };
    std::fmt::write(&mut counter, format_args!("{number}")).ok()?;
    Some(counter.bytes)
}

/// Sensitive key patterns (case-insensitive substring match).
const SENSITIVE_PATTERNS: &[&str] = &[
    "token",
    "secret",
    "password",
    "credential",
    "bearer",
    "jwt",
    "api_key",
    "private_key",
];

/// Returns `true` if a JSON object key should have its value masked.
#[must_use]
pub fn is_sensitive_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    // Explicit allowlist: these are identity/safety-critical values and must remain visible.
    if lower == "project_key" || lower == "storage_root" {
        return false;
    }
    // Specific header-like keys that commonly carry secrets.
    if lower == "authorization" || lower == "auth_header" {
        return true;
    }
    SENSITIVE_PATTERNS.iter().any(|p| lower.contains(p))
}

/// Always returns the redaction placeholder (ASCII-only).
#[must_use]
pub fn mask_sensitive_value(_original: &str) -> String {
    MASK_REDACTED.to_string()
}

/// Sanitize a value for known keys where secrets can appear in *values* even if the key is not
/// obviously sensitive (e.g., `postgres://user:pass@host/db`).
#[must_use]
pub fn sanitize_known_value(key: &str, value: &str) -> Option<String> {
    let lower = key.to_ascii_lowercase();
    if lower == "am_git_binary" || lower == "git_binary" || lower.ends_with("_git_binary") {
        return basename_for_absolute_path(value);
    }
    let is_database_url = lower == "database_url" || lower.ends_with("database_url");
    let is_redis_url =
        lower == "redis_url" || lower.ends_with("redis_url") || lower.contains("redis_url");
    if !is_database_url && !is_redis_url {
        return None;
    }
    sanitize_url_userinfo(value)
}

fn mask_json_depth(value: &Value, depth: usize, budget: &mut DiagnosticJsonBudget) -> Value {
    if budget.nodes == 0 || budget.bytes < 64 {
        return Value::String(DIAGNOSTIC_TRUNCATED.to_string());
    }
    budget.nodes -= 1;
    budget.bytes = budget.bytes.saturating_sub(32);
    if depth >= 20 {
        return Value::String(DIAGNOSTIC_TRUNCATED.to_string());
    }
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                if k.len() > DIAGNOSTIC_KEY_BYTES
                    || budget.nodes == 0
                    || budget.bytes < k.len().saturating_add(128)
                {
                    out.insert("_diagnostic_truncated".to_string(), Value::Bool(true));
                    break;
                }
                budget.bytes -= k.len() + 64;
                let masked = if is_sensitive_key(k) {
                    budget.bytes = budget.bytes.saturating_sub(MASK_REDACTED.len());
                    Value::String(mask_sensitive_value(""))
                } else if let Value::String(s) = v
                    && s.len() <= DIAGNOSTIC_STRING_BYTES
                    && let Some(sanitized) = sanitize_known_value(k, s)
                {
                    mask_json_depth(&Value::String(sanitized), depth + 1, budget)
                } else if k.to_ascii_lowercase().contains("url")
                    && v.as_str()
                        .is_some_and(|s| s.len() > DIAGNOSTIC_STRING_BYTES)
                {
                    // A cut URL can lose the delimiter needed to recognize
                    // credentials. Omit it instead of publishing that prefix.
                    Value::String(DIAGNOSTIC_TRUNCATED.to_string())
                } else {
                    mask_json_depth(v, depth + 1, budget)
                };
                out.insert(k.clone(), masked);
            }
            Value::Object(out)
        }
        Value::Array(arr) => {
            let mut out = Vec::new();
            for item in arr {
                if budget.nodes == 0 || budget.bytes < 64 {
                    out.push(Value::String(DIAGNOSTIC_TRUNCATED.to_string()));
                    break;
                }
                out.push(mask_json_depth(item, depth + 1, budget));
            }
            Value::Array(out)
        }
        Value::String(value) => {
            let preview = bounded_diagnostic_text(value, DIAGNOSTIC_STRING_BYTES.min(budget.bytes));
            budget.bytes = budget.bytes.saturating_sub(preview.len());
            Value::String(preview)
        }
        Value::Number(number) => {
            // serde_json's arbitrary_precision feature permits enormous
            // number strings too; their diagnostic copy needs the same cap.
            if let Some(bytes) =
                diagnostic_number_bytes(number, DIAGNOSTIC_STRING_BYTES.min(budget.bytes))
            {
                budget.bytes -= bytes;
                Value::Number(number.clone())
            } else {
                Value::String(DIAGNOSTIC_TRUNCATED.to_string())
            }
        }
        Value::Null => Value::Null,
        Value::Bool(value) => Value::Bool(*value),
    }
}

/// Walk a `serde_json::Value` tree and replace values whose keys match
/// sensitive patterns with redaction placeholders. Also sanitizes select known
/// keys and visibly omits data beyond the bounded diagnostic preview budget.
#[must_use]
pub fn mask_json(value: &Value) -> Value {
    mask_json_depth(
        value,
        0,
        &mut DiagnosticJsonBudget {
            bytes: DIAGNOSTIC_JSON_BYTES,
            nodes: DIAGNOSTIC_JSON_NODES,
        },
    )
}

/// Back-compat name used by existing panel renderers/tests.
#[must_use]
pub fn mask_json_params(value: &Value) -> Value {
    mask_json(value)
}

#[must_use]
fn sanitize_url_userinfo(value: &str) -> Option<String> {
    // Conservative, allocation-light sanitizer:
    // - only modifies strings that look like: `scheme://user:pass@host/...`
    // - masks ONLY the password segment
    let scheme_end = value.find("://")?;
    let after_scheme = scheme_end + 3;

    // The authority section ends at the first '/', '?', or '#'
    let authority_end = value[after_scheme..]
        .find(['/', '?', '#'])
        .map(|i| i + after_scheme)
        .unwrap_or(value.len());

    let authority = &value[after_scheme..authority_end];

    // Find the '@' that separates userinfo from host within the authority.
    let at_pos_in_authority = authority.rfind('@')?;
    let userinfo = &authority[..at_pos_in_authority];

    let colon_pos = userinfo.find(':')?;
    let user = &userinfo[..colon_pos];
    let pass = &userinfo[(colon_pos + 1)..];
    if pass.is_empty() {
        return None;
    }

    let absolute_at_pos = after_scheme + at_pos_in_authority;

    let mut out = String::with_capacity(value.len() + MASK_REDACTED.len());
    out.push_str(&value[..after_scheme]);
    out.push_str(user);
    out.push(':');
    out.push_str(MASK_REDACTED);
    out.push_str(&value[absolute_at_pos..]);
    Some(out)
}

fn basename_for_absolute_path(value: &str) -> Option<String> {
    let path = Path::new(value);
    if !path.is_absolute() {
        return None;
    }
    path.file_name()
        .and_then(|name| name.to_str())
        .map(std::string::ToString::to_string)
}
