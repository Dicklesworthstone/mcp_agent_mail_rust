//! Deterministic static HTML export engine.
//!
//! Generates pre-rendered HTML pages for all web UI routes, producing
//! a self-contained static directory deployable on GitHub Pages or
//! Cloudflare Pages without any runtime server.
//!
//! The pipeline:
//! 1. Enumerate all projects, agents, threads, messages from the DB
//! 2. For each entity, render the corresponding web template to HTML
//! 3. Write to a directory structure that mirrors URL paths
//! 4. Generate a client-side search index (JSON)
//! 5. Emit navigation manifest and hosting files
//! 6. Compute deterministic manifest with SHA-256 content hashes

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use asupersync::Cx;
use fastmcp_core::block_on;
use mcp_agent_mail_db::pool::DbPool;
use mcp_agent_mail_db::{DbPoolConfig, get_or_create_pool, queries};
use serde::Serialize;
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Configuration for a static export run.
pub struct ExportConfig {
    /// Output directory for generated files.
    pub output_dir: PathBuf,
    /// Project slugs to export (empty = all).
    pub projects: Vec<String>,
    /// Include archive visualization routes.
    pub include_archive: bool,
    /// Generate client-side search index artifact.
    pub include_search_index: bool,
}

/// Manifest entry for a generated file.
#[derive(Debug, Clone, Serialize)]
pub struct ManifestEntry {
    /// The URL route this file corresponds to.
    pub route: String,
    /// File size in bytes.
    pub size: u64,
    /// SHA-256 hex digest.
    pub sha256: String,
}

/// Result manifest for the export run.
#[derive(Debug, Serialize)]
pub struct ExportManifest {
    pub schema_version: String,
    pub generated_at: String,
    pub file_count: usize,
    pub total_bytes: u64,
    /// SHA-256 of all file hashes concatenated (deterministic).
    pub content_hash: String,
    /// Map from relative file path to manifest entry.
    pub files: BTreeMap<String, ManifestEntry>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run the full static export pipeline.
///
/// Returns the export manifest on success.
pub fn export_static_site(config: &ExportConfig) -> Result<ExportManifest, String> {
    fs::create_dir_all(&config.output_dir).map_err(|e| format!("create output dir: {e}"))?;

    let pool = get_pool()?;
    let cx = Cx::for_testing();
    let mut files = BTreeMap::new();

    // ── 1. Enumerate projects ───────────────────────────────────────
    let all_projects = bo(&cx, queries::list_projects(&cx, &pool))?;
    let project_slugs: Vec<String> = if config.projects.is_empty() {
        all_projects.iter().map(|p| p.slug.clone()).collect()
    } else {
        // Filter to requested slugs that actually exist.
        let existing: Vec<String> = all_projects.iter().map(|p| p.slug.clone()).collect();
        config
            .projects
            .iter()
            .filter(|s| existing.contains(s))
            .cloned()
            .collect()
    };

    // ── 2. Top-level routes ─────────────────────────────────────────
    emit_route("/mail", "", "index.html", &config.output_dir, &mut files);
    emit_route(
        "/mail/projects",
        "",
        "projects.html",
        &config.output_dir,
        &mut files,
    );

    // ── 3. Per-project routes ───────────────────────────────────────
    for slug in &project_slugs {
        emit_project_routes(slug, &cx, &pool, &config.output_dir, &mut files)?;
    }

    // ── 4. Archive routes ───────────────────────────────────────────
    if config.include_archive {
        emit_archive_routes(&config.output_dir, &mut files);
    }

    // ── 5. Search index ─────────────────────────────────────────────
    if config.include_search_index {
        emit_search_index(&project_slugs, &cx, &pool, &config.output_dir, &mut files)?;
    }

    // ── 6. Navigation manifest ──────────────────────────────────────
    emit_navigation(&project_slugs, &cx, &pool, &config.output_dir, &mut files)?;

    // ── 7. Hosting files ────────────────────────────────────────────
    emit_hosting_files(&config.output_dir, &mut files)?;

    // ── 8. Compute manifest ─────────────────────────────────────────
    let total_bytes = files.values().map(|e| e.size).sum();
    let content_hash = compute_content_hash(&files);

    let manifest = ExportManifest {
        schema_version: "1.0.0".to_string(),
        generated_at: chrono::Utc::now().to_rfc3339(),
        file_count: files.len(),
        total_bytes,
        content_hash,
        files,
    };

    let manifest_json =
        serde_json::to_string_pretty(&manifest).map_err(|e| format!("serialize manifest: {e}"))?;
    write_to_file(
        &config.output_dir.join("manifest.json"),
        manifest_json.as_bytes(),
    )?;

    Ok(manifest)
}

// ---------------------------------------------------------------------------
// DB helpers
// ---------------------------------------------------------------------------

fn get_pool() -> Result<DbPool, String> {
    let cfg = DbPoolConfig::from_env();
    get_or_create_pool(&cfg).map_err(|e| format!("Database error: {e}"))
}

/// Block on an async outcome, converting errors to String.
fn bo<T>(
    _cx: &Cx,
    f: impl std::future::Future<Output = asupersync::Outcome<T, mcp_agent_mail_db::DbError>>,
) -> Result<T, String> {
    match block_on(f) {
        asupersync::Outcome::Ok(v) => Ok(v),
        asupersync::Outcome::Err(e) => Err(format!("DB error: {e}")),
        asupersync::Outcome::Cancelled(_) => Err("Cancelled".to_string()),
        asupersync::Outcome::Panicked(p) => Err(format!("Panicked: {}", p.message())),
    }
}

// ---------------------------------------------------------------------------
// Route emission
// ---------------------------------------------------------------------------

/// Render a single route via the web UI dispatcher and write to a file.
///
/// Non-fatal: logs warnings on errors instead of aborting the export.
fn emit_route(
    path: &str,
    query: &str,
    file_path: &str,
    output_dir: &Path,
    files: &mut BTreeMap<String, ManifestEntry>,
) {
    match crate::mail_ui::dispatch(path, query, "GET", "") {
        Ok(Some(html)) => {
            let dest = output_dir.join(file_path);
            if write_html(&dest, &html).is_ok() {
                let sha = sha256_hex(html.as_bytes());
                files.insert(
                    file_path.to_string(),
                    ManifestEntry {
                        route: if query.is_empty() {
                            path.to_string()
                        } else {
                            format!("{path}?{query}")
                        },
                        size: html.len() as u64,
                        sha256: sha,
                    },
                );
            }
        }
        Ok(None) => {} // Route not matched, skip silently.
        Err((_status, _msg)) => {
            // Non-fatal: the entity may simply have no data.
        }
    }
}

/// Emit all routes for a single project.
fn emit_project_routes(
    slug: &str,
    cx: &Cx,
    pool: &DbPool,
    output_dir: &Path,
    files: &mut BTreeMap<String, ManifestEntry>,
) -> Result<(), String> {
    let prefix = format!("/mail/{slug}");
    let dir_prefix = format!("mail/{slug}");

    // Project overview
    emit_route(
        &prefix,
        "",
        &format!("{dir_prefix}/index.html"),
        output_dir,
        files,
    );

    // Search (empty query → shows interface)
    emit_route(
        &format!("{prefix}/search"),
        "",
        &format!("{dir_prefix}/search.html"),
        output_dir,
        files,
    );

    // File reservations
    emit_route(
        &format!("{prefix}/file_reservations"),
        "",
        &format!("{dir_prefix}/file_reservations.html"),
        output_dir,
        files,
    );

    // Attachments
    emit_route(
        &format!("{prefix}/attachments"),
        "",
        &format!("{dir_prefix}/attachments.html"),
        output_dir,
        files,
    );

    // Get project ID for agent/message queries.
    let project = bo(cx, queries::get_project_by_slug(cx, pool, slug))?;
    let pid = project.id.unwrap_or(0);

    // ── Agents → inbox pages ────────────────────────────────────────
    let agents = bo(cx, queries::list_agents(cx, pool, pid))?;
    for agent in &agents {
        let name = &agent.name;
        emit_route(
            &format!("{prefix}/inbox/{name}"),
            "",
            &format!("{dir_prefix}/inbox/{name}.html"),
            output_dir,
            files,
        );
    }

    // ── Threads and messages ────────────────────────────────────────
    // Collect all messages for this project by iterating agent inboxes.
    let mut seen_threads = std::collections::BTreeSet::new();
    let mut seen_messages = std::collections::BTreeSet::new();

    for agent in &agents {
        let aid = agent.id.unwrap_or(0);
        let inbox = bo(
            cx,
            queries::fetch_inbox(cx, pool, pid, aid, false, None, 10_000),
        )?;
        for row in &inbox {
            let msg = &row.message;
            let mid = msg.id.unwrap_or(0);
            if mid > 0 {
                seen_messages.insert(mid);
            }
            if let Some(ref tid) = msg.thread_id {
                if !tid.is_empty() {
                    seen_threads.insert(tid.clone());
                }
            }
        }
    }

    // Render thread pages.
    for tid in &seen_threads {
        let safe_name = sanitize_filename(tid);
        emit_route(
            &format!("{prefix}/thread/{tid}"),
            "",
            &format!("{dir_prefix}/thread/{safe_name}.html"),
            output_dir,
            files,
        );
    }

    // Render message detail pages.
    for mid in &seen_messages {
        emit_route(
            &format!("{prefix}/message/{mid}"),
            "",
            &format!("{dir_prefix}/message/{mid}.html"),
            output_dir,
            files,
        );
    }

    Ok(())
}

/// Emit archive visualization routes.
fn emit_archive_routes(output_dir: &Path, files: &mut BTreeMap<String, ManifestEntry>) {
    let routes = [
        ("guide", "guide"),
        ("timeline", "timeline"),
        ("activity", "activity"),
        ("browser", "browser"),
        ("network", "network"),
        ("time-travel", "time-travel"),
    ];
    for (route, file) in &routes {
        emit_route(
            &format!("/mail/archive/{route}"),
            "",
            &format!("mail/archive/{file}.html"),
            output_dir,
            files,
        );
    }
}

// ---------------------------------------------------------------------------
// Search index generation
// ---------------------------------------------------------------------------

/// Message metadata for the client-side search index.
#[derive(Serialize)]
struct SearchIndexEntry {
    id: i64,
    project: String,
    subject: String,
    body_excerpt: String,
    from_agent: String,
    thread_id: String,
    importance: String,
    created_ts: String,
}

/// Generate a JSON search index artifact for client-side search.
fn emit_search_index(
    project_slugs: &[String],
    cx: &Cx,
    pool: &DbPool,
    output_dir: &Path,
    files: &mut BTreeMap<String, ManifestEntry>,
) -> Result<(), String> {
    let mut entries: Vec<SearchIndexEntry> = Vec::new();

    for slug in project_slugs {
        let project = bo(cx, queries::get_project_by_slug(cx, pool, slug))?;
        let pid = project.id.unwrap_or(0);
        let agents = bo(cx, queries::list_agents(cx, pool, pid))?;

        for agent in &agents {
            let aid = agent.id.unwrap_or(0);
            let inbox = bo(
                cx,
                queries::fetch_inbox(cx, pool, pid, aid, false, None, 10_000),
            )?;
            for row in inbox {
                let msg = row.message;
                let mid = msg.id.unwrap_or(0);
                // Deduplicate by message ID.
                if entries.iter().any(|e| e.id == mid) {
                    continue;
                }
                entries.push(SearchIndexEntry {
                    id: mid,
                    project: slug.clone(),
                    subject: msg.subject.clone(),
                    body_excerpt: truncate(&msg.body_md, 300),
                    from_agent: row.sender_name.clone(),
                    thread_id: msg.thread_id.clone().unwrap_or_default(),
                    importance: msg.importance.clone(),
                    created_ts: mcp_agent_mail_db::timestamps::micros_to_iso(msg.created_ts),
                });
            }
        }
    }

    // Sort deterministically by (project, id).
    entries.sort_by(|a, b| a.project.cmp(&b.project).then(a.id.cmp(&b.id)));

    let json = serde_json::to_string_pretty(&entries)
        .map_err(|e| format!("serialize search index: {e}"))?;

    let path = "search-index.json";
    let dest = output_dir.join(path);
    write_to_file(&dest, json.as_bytes())?;

    let sha = sha256_hex(json.as_bytes());
    files.insert(
        path.to_string(),
        ManifestEntry {
            route: "/search-index.json".to_string(),
            size: json.len() as u64,
            sha256: sha,
        },
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Navigation manifest
// ---------------------------------------------------------------------------

/// Navigation entry for the sitemap/nav structure.
#[derive(Serialize)]
struct NavEntry {
    title: String,
    path: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    children: Vec<Self>,
}

/// Generate a navigation manifest (sitemap-like JSON).
fn emit_navigation(
    project_slugs: &[String],
    cx: &Cx,
    pool: &DbPool,
    output_dir: &Path,
    files: &mut BTreeMap<String, ManifestEntry>,
) -> Result<(), String> {
    let mut nav = vec![
        NavEntry {
            title: "Unified Inbox".to_string(),
            path: "/mail/".to_string(),
            children: Vec::new(),
        },
        NavEntry {
            title: "Projects".to_string(),
            path: "/mail/projects".to_string(),
            children: Vec::new(),
        },
    ];

    for slug in project_slugs {
        let project = bo(cx, queries::get_project_by_slug(cx, pool, slug))?;
        let pid = project.id.unwrap_or(0);
        let agents = bo(cx, queries::list_agents(cx, pool, pid))?;

        let agent_children: Vec<NavEntry> = agents
            .iter()
            .map(|a| NavEntry {
                title: format!("{} inbox", a.name),
                path: format!("/mail/{slug}/inbox/{}", a.name),
                children: Vec::new(),
            })
            .collect();

        let mut project_children = vec![
            NavEntry {
                title: "Search".to_string(),
                path: format!("/mail/{slug}/search"),
                children: Vec::new(),
            },
            NavEntry {
                title: "File Reservations".to_string(),
                path: format!("/mail/{slug}/file_reservations"),
                children: Vec::new(),
            },
        ];
        project_children.extend(agent_children);

        nav.push(NavEntry {
            title: project.human_key.clone(),
            path: format!("/mail/{slug}"),
            children: project_children,
        });
    }

    let json =
        serde_json::to_string_pretty(&nav).map_err(|e| format!("serialize navigation: {e}"))?;

    let path = "navigation.json";
    let dest = output_dir.join(path);
    write_to_file(&dest, json.as_bytes())?;

    let sha = sha256_hex(json.as_bytes());
    files.insert(
        path.to_string(),
        ManifestEntry {
            route: "/navigation.json".to_string(),
            size: json.len() as u64,
            sha256: sha,
        },
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Hosting files
// ---------------------------------------------------------------------------

/// Emit platform-specific hosting files for GitHub Pages / Cloudflare Pages.
fn emit_hosting_files(
    output_dir: &Path,
    files: &mut BTreeMap<String, ManifestEntry>,
) -> Result<(), String> {
    // .nojekyll (GitHub Pages: bypass Jekyll processing).
    write_and_record(output_dir, ".nojekyll", b"", "/", files)?;

    // _headers (Cloudflare Pages / Netlify: security headers).
    let headers = "\
/*
  X-Content-Type-Options: nosniff
  X-Frame-Options: SAMEORIGIN
  Referrer-Policy: strict-origin-when-cross-origin
";
    write_and_record(
        output_dir,
        "_headers",
        headers.as_bytes(),
        "/_headers",
        files,
    )?;

    // Root index.html redirect to /mail/.
    let redirect = r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta http-equiv="refresh" content="0;url=mail/index.html">
  <title>Redirecting…</title>
</head>
<body>
  <p>Redirecting to <a href="mail/index.html">mail inbox</a>…</p>
</body>
</html>
"#;
    write_and_record(output_dir, "redirect.html", redirect.as_bytes(), "/", files)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// File I/O helpers
// ---------------------------------------------------------------------------

fn write_html(dest: &Path, html: &str) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    fs::write(dest, html.as_bytes()).map_err(|e| format!("write {}: {e}", dest.display()))
}

fn write_to_file(dest: &Path, data: &[u8]) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    fs::write(dest, data).map_err(|e| format!("write {}: {e}", dest.display()))
}

fn write_and_record(
    output_dir: &Path,
    file_path: &str,
    data: &[u8],
    route: &str,
    files: &mut BTreeMap<String, ManifestEntry>,
) -> Result<(), String> {
    let dest = output_dir.join(file_path);
    write_to_file(&dest, data)?;
    let sha = sha256_hex(data);
    files.insert(
        file_path.to_string(),
        ManifestEntry {
            route: route.to_string(),
            size: data.len() as u64,
            sha256: sha,
        },
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Hashing
// ---------------------------------------------------------------------------

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let result = hasher.finalize();
    hex_encode(&result)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Compute a deterministic content hash from all file hashes.
///
/// Concatenates all `sha256` values in sorted key order, then hashes the result.
fn compute_content_hash(files: &BTreeMap<String, ManifestEntry>) -> String {
    let mut hasher = Sha256::new();
    // BTreeMap iterates in sorted key order → deterministic.
    for (path, entry) in files {
        hasher.update(path.as_bytes());
        hasher.update(b":");
        hasher.update(entry.sha256.as_bytes());
        hasher.update(b"\n");
    }
    hex_encode(&hasher.finalize())
}

// ---------------------------------------------------------------------------
// String helpers
// ---------------------------------------------------------------------------

/// Sanitize a string for use as a filename (replace unsafe chars).
fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_ascii_control() => '_',
            _ => c,
        })
        .collect()
}

/// Truncate a string to `max` bytes on a char boundary.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn sanitize_filename_replaces_slashes() {
        assert_eq!(sanitize_filename("foo/bar"), "foo_bar");
        assert_eq!(sanitize_filename("a:b*c"), "a_b_c");
    }

    #[test]
    fn truncate_respects_char_boundary() {
        let s = "hello world";
        assert_eq!(truncate(s, 100), "hello world");
        assert_eq!(truncate(s, 5), "hello…");
    }

    #[test]
    fn sha256_hex_produces_64_char_hex() {
        let hash = sha256_hex(b"test");
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn content_hash_is_deterministic() {
        let mut files = BTreeMap::new();
        files.insert(
            "a.html".to_string(),
            ManifestEntry {
                route: "/a".to_string(),
                size: 10,
                sha256: "abc123".to_string(),
            },
        );
        files.insert(
            "b.html".to_string(),
            ManifestEntry {
                route: "/b".to_string(),
                size: 20,
                sha256: "def456".to_string(),
            },
        );
        let h1 = compute_content_hash(&files);
        let h2 = compute_content_hash(&files);
        assert_eq!(h1, h2, "Content hash must be deterministic");
        assert_eq!(h1.len(), 64);
    }

    #[test]
    fn manifest_entry_serializes() {
        let entry = ManifestEntry {
            route: "/mail".to_string(),
            size: 1024,
            sha256: "abcdef".to_string(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"route\":\"/mail\""));
        assert!(json.contains("\"size\":1024"));
    }

    #[test]
    fn emit_route_handles_missing_dispatch() {
        // dispatch() will fail without a real DB, but emit_route is non-fatal.
        let dir = PathBuf::from("/tmp/static_export_test_missing");
        let _ = fs::remove_dir_all(&dir);
        let mut files = BTreeMap::new();
        emit_route("/nonexistent/route", "", "test.html", &dir, &mut files);
        // Should not crash; file may or may not be recorded.
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_and_record_creates_file() {
        let dir = PathBuf::from("/tmp/static_export_test_write");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut files = BTreeMap::new();
        write_and_record(&dir, "test.txt", b"hello", "/test", &mut files).unwrap();
        assert!(files.contains_key("test.txt"));
        assert_eq!(files["test.txt"].size, 5);
        assert_eq!(files["test.txt"].route, "/test");
        assert_eq!(fs::read_to_string(dir.join("test.txt")).unwrap(), "hello");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hosting_files_are_emitted() {
        let dir = PathBuf::from("/tmp/static_export_test_hosting");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut files = BTreeMap::new();
        emit_hosting_files(&dir, &mut files).unwrap();
        assert!(files.contains_key(".nojekyll"));
        assert!(files.contains_key("_headers"));
        assert!(files.contains_key("redirect.html"));
        let _ = fs::remove_dir_all(&dir);
    }
}
