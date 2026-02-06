//! Template rendering for the Mail SSR UI.
//!
//! We embed the legacy Jinja templates at compile time to keep the binary self-contained.

#![forbid(unsafe_code)]

use std::sync::LazyLock;

use include_dir::{Dir, include_dir};
use minijinja::value::Value;
use minijinja::{AutoEscape, Environment, Error, ErrorKind};

static TEMPLATE_DIR: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/templates");

/// Strip HTML tags from a string (Jinja2 `striptags` filter).
#[allow(clippy::needless_pass_by_value)]
fn striptags(value: Value) -> Value {
    let s = value.to_string();
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    Value::from(out)
}

/// Truncate a string (Jinja2 `truncate` filter).
///
/// Usage: `truncate(150)` — truncates at word boundary, appends `...`.
#[allow(clippy::needless_pass_by_value)]
fn truncate(value: Value, length: Option<usize>) -> Value {
    let s = value.to_string();
    let length = length.unwrap_or(255);

    if s.len() <= length {
        return Value::from(s);
    }

    let end = "...";
    let end_len = end.len();
    let trunc_len = length.saturating_sub(end_len);

    // Find a char boundary.
    let mut boundary = trunc_len;
    while boundary > 0 && !s.is_char_boundary(boundary) {
        boundary -= 1;
    }

    // Default: truncate at last word boundary (killwords=false).
    if let Some(pos) = s[..boundary].rfind(' ') {
        boundary = pos;
    }

    Value::from(format!("{}{end}", &s[..boundary]))
}

#[allow(clippy::needless_pass_by_value)] // MiniJinja filter signature uses owned `Value`.
fn tojson(value: Value) -> Result<Value, Error> {
    let s = serde_json::to_string(&value).map_err(|err| {
        Error::new(ErrorKind::InvalidOperation, "cannot serialize to JSON").with_source(err)
    })?;

    // Ensure the output is safe for embedding in HTML and inline JS contexts.
    // Mirrors MiniJinja's built-in `tojson` filter behavior when the `json`
    // feature is enabled.
    let mut rv = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => rv.push_str("\\u003c"),
            '>' => rv.push_str("\\u003e"),
            '&' => rv.push_str("\\u0026"),
            '\'' => rv.push_str("\\u0027"),
            _ => rv.push(c),
        }
    }

    Ok(Value::from_safe_string(rv))
}

static ENV: LazyLock<Environment<'static>> = LazyLock::new(|| {
    let mut env = Environment::new();

    env.set_auto_escape_callback(|name| {
        let is_html = std::path::Path::new(name)
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("html"));
        if is_html {
            AutoEscape::Html
        } else {
            AutoEscape::None
        }
    });

    // Jinja2 compatibility: templates rely on `|tojson` extensively.
    env.add_filter("tojson", tojson);
    // Jinja2 built-ins used in mail_thread.html and elsewhere.
    env.add_filter("striptags", striptags);
    env.add_filter("truncate", truncate);

    // Load all embedded templates.
    for file in TEMPLATE_DIR.files() {
        // include_dir stores paths as static, so this can be a `&'static str`.
        let Some(name) = file.path().to_str() else {
            continue;
        };
        let contents =
            std::str::from_utf8(file.contents()).unwrap_or("<!-- invalid utf-8 template -->");
        // Ignore duplicates; the directory is flat.
        let _ = env.add_template(name, contents);
    }

    env
});

pub fn render_template<T: serde::Serialize>(name: &str, ctx: T) -> Result<String, Error> {
    let tpl = ENV.get_template(name)?;
    tpl.render(ctx)
}
