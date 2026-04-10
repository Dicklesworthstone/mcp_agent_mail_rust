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
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use asupersync::Cx;
use fastmcp_core::block_on;
use mcp_agent_mail_core::config::Config;
use mcp_agent_mail_db::pool::DbPool;
use mcp_agent_mail_db::{DbPoolConfig, get_or_create_pool, queries};
use mcp_agent_mail_share::scan_for_secrets;
use serde::Serialize;
use sha2::{Digest, Sha256};

const EXPORT_INBOX_SCAN_LIMIT: usize = 10_000;

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
    ensure_export_features_supported(config)?;
    ensure_output_dir_empty(&config.output_dir)?;
    ensure_real_directory(&config.output_dir)
        .map_err(|e| format!("create output dir {}: {e}", config.output_dir.display()))?;
    let staged_output = create_export_stage(&config.output_dir)
        .map_err(|e| format!("create staged output {}: {e}", config.output_dir.display()))?;
    let staged_output_dir = staged_output.path();

    let read_pool_owner = open_static_export_read_pool()?;
    let live_pool_owner = if read_pool_owner.is_some() {
        None
    } else {
        Some(get_pool()?)
    };
    let pool = read_pool_owner.as_ref().map_or_else(
        || {
            live_pool_owner
                .as_ref()
                .expect("static export live pool fallback should exist when no read pool exists")
        },
        crate::ObservabilityDbPool::pool,
    );
    // Use infinite budget — static export is a batch CLI operation, not a
    // request-scoped handler. Individual DB queries have their own timeouts
    // via pool acquire. Using for_request_with_budget (not for_testing) to
    // get proper production Cx lineage for tracing/observability.
    let cx = Cx::for_request_with_budget(asupersync::Budget::INFINITE);
    let mut files = BTreeMap::new();

    // ── 1. Enumerate projects ───────────────────────────────────────
    let all_projects = bo(&cx, queries::list_projects(&cx, &pool))?;
    let existing_slugs: Vec<String> = all_projects.iter().map(|p| p.slug.clone()).collect();
    let project_slugs = resolve_requested_project_slugs(&existing_slugs, &config.projects)?;

    // ── 2. Top-level routes ─────────────────────────────────────────
    emit_html_route("/mail", "__static_export=1", staged_output_dir, &mut files)?;
    emit_html_route("/mail/projects", "", staged_output_dir, &mut files)?;

    // ── 3. Per-project routes ───────────────────────────────────────
    for slug in &project_slugs {
        emit_project_routes(slug, &cx, &pool, staged_output_dir, &mut files)?;
    }

    // ── 4. Archive routes ───────────────────────────────────────────
    if config.include_archive {
        emit_archive_routes(staged_output_dir, &mut files)?;
    }

    // ── 5. Search index ─────────────────────────────────────────────
    if config.include_search_index {
        emit_search_index(&project_slugs, &cx, &pool, staged_output_dir, &mut files)?;
    }

    // ── 6. Navigation manifest ──────────────────────────────────────
    emit_navigation(&project_slugs, &cx, &pool, staged_output_dir, &mut files)?;

    // ── 7. Hosting files ────────────────────────────────────────────
    emit_hosting_files(staged_output_dir, &mut files)?;

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
        &staged_output_dir.join("manifest.json"),
        manifest_json.as_bytes(),
    )?;
    if let Err(error) = publish_staged_export_output(staged_output_dir, &config.output_dir) {
        let stage_path = staged_output.persist();
        return Err(format!(
            "publish staged static export into {} failed with staged output preserved at {}: {error}",
            config.output_dir.display(),
            stage_path.display()
        ));
    }

    Ok(manifest)
}

fn ensure_export_features_supported(config: &ExportConfig) -> Result<(), String> {
    if config.include_archive {
        return Err(
            "static export archive routes are not offline-safe yet; rerun without --include-archive"
                .to_string(),
        );
    }
    if !config.include_search_index {
        return Err(
            "static export requires the search index because offline /mail/{project}/search pages are backed by search-index.json"
                .to_string(),
        );
    }
    Ok(())
}

fn ensure_output_dir_empty(output_dir: &Path) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(output_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!("stat output dir {}: {error}", output_dir.display()));
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "static export output path must not be a symlink: {}",
            output_dir.display()
        ));
    }
    if !metadata.file_type().is_dir() {
        return Err(format!(
            "static export output path is not a directory: {}",
            output_dir.display()
        ));
    }

    let mut entries = fs::read_dir(output_dir)
        .map_err(|e| format!("read output dir {}: {e}", output_dir.display()))?;
    if let Some(entry) = entries.next() {
        let entry = entry.map_err(|e| format!("read output dir entry: {e}"))?;
        let rel = entry.path().strip_prefix(output_dir).map_or_else(
            |_| entry.path().display().to_string(),
            |path| path.display().to_string(),
        );
        return Err(format!(
            "static export output directory must be empty because the exporter does not prune stale files: {} contains {rel}",
            output_dir.display()
        ));
    }

    Ok(())
}

#[derive(Debug)]
struct StaticExportStage {
    path: PathBuf,
    cleanup_on_drop: bool,
}

impl StaticExportStage {
    fn path(&self) -> &Path {
        &self.path
    }

    fn persist(mut self) -> PathBuf {
        self.cleanup_on_drop = false;
        self.path.clone()
    }
}

impl Drop for StaticExportStage {
    fn drop(&mut self) {
        if self.cleanup_on_drop {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn create_export_stage(output_dir: &Path) -> std::io::Result<StaticExportStage> {
    let parent = output_dir.parent().unwrap_or_else(|| Path::new("."));
    ensure_real_directory(parent)?;
    let stem = output_dir
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("static-export");
    for attempt in 0..1024 {
        let candidate = parent.join(format!(".{stem}.stage.{}.{}", std::process::id(), attempt));
        match fs::create_dir(&candidate) {
            Ok(()) => {
                return Ok(StaticExportStage {
                    path: candidate,
                    cleanup_on_drop: true,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!(
            "could not create unique staged static export directory next to {}",
            output_dir.display()
        ),
    ))
}

fn resolve_requested_project_slugs(
    existing: &[String],
    requested: &[String],
) -> Result<Vec<String>, String> {
    if requested.is_empty() {
        return Ok(existing.to_vec());
    }

    let existing_set: std::collections::BTreeSet<&str> =
        existing.iter().map(String::as_str).collect();
    let mut missing = Vec::new();
    let mut resolved = Vec::new();
    let mut seen = std::collections::BTreeSet::new();

    for slug in requested {
        if !existing_set.contains(slug.as_str()) {
            missing.push(slug.clone());
            continue;
        }
        if seen.insert(slug.clone()) {
            resolved.push(slug.clone());
        }
    }

    if !missing.is_empty() {
        return Err(format!(
            "unknown project slug(s) requested for static export: {}",
            missing.join(", ")
        ));
    }

    Ok(resolved)
}

fn publish_staged_export_output(
    staged_output_dir: &Path,
    output_dir: &Path,
) -> std::io::Result<()> {
    ensure_real_directory(output_dir)?;
    let mut entry_names = Vec::new();
    for entry in fs::read_dir(staged_output_dir)? {
        let entry = entry?;
        entry_names.push(entry.file_name());
    }
    entry_names.sort();

    let mut published = Vec::new();
    for entry_name in &entry_names {
        match move_staged_export_entry(staged_output_dir, output_dir, entry_name) {
            Ok(()) => published.push(entry_name.clone()),
            Err(error) => {
                rollback_published_export_entries(output_dir, staged_output_dir, &published)
                    .map_err(|rollback_error| {
                        std::io::Error::other(format!(
                            "failed to publish staged export output {}: {error}; rollback failed: {rollback_error}",
                            Path::new(entry_name).display()
                        ))
                    })?;
                return Err(std::io::Error::other(format!(
                    "failed to publish staged export output {}: {error}",
                    Path::new(entry_name).display()
                )));
            }
        }
    }
    Ok(())
}

fn move_staged_export_entry(
    staged_output_dir: &Path,
    output_dir: &Path,
    entry_name: &OsString,
) -> std::io::Result<()> {
    let source = staged_output_dir.join(entry_name);
    let destination = output_dir.join(entry_name);
    let source_metadata = fs::symlink_metadata(&source)?;
    if source_metadata.file_type().is_symlink() {
        return Err(std::io::Error::other(format!(
            "refusing to publish symlinked staged export path {}",
            source.display()
        )));
    }
    if fs::symlink_metadata(&destination).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(std::io::Error::other(format!(
            "refusing to publish through symlinked export path {}",
            destination.display()
        )));
    }
    if fs::symlink_metadata(&destination).is_ok() {
        return Err(std::io::Error::other(format!(
            "expected empty export destination but found existing path {}",
            destination.display()
        )));
    }
    fs::rename(source, destination)
}

fn rollback_published_export_entries(
    output_dir: &Path,
    staged_output_dir: &Path,
    published: &[OsString],
) -> std::io::Result<()> {
    for entry_name in published.iter().rev() {
        let source = output_dir.join(entry_name);
        let destination = staged_output_dir.join(entry_name);
        if fs::symlink_metadata(&source).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err(std::io::Error::other(format!(
                "refusing to rollback symlinked published export path {}",
                source.display()
            )));
        }
        if fs::symlink_metadata(&destination).is_ok() {
            return Err(std::io::Error::other(format!(
                "cannot rollback published export output because staged path already exists {}",
                destination.display()
            )));
        }
        if fs::symlink_metadata(&source)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        {
            continue;
        }
        fs::rename(source, destination)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// DB helpers
// ---------------------------------------------------------------------------

fn get_pool() -> Result<DbPool, String> {
    let cfg = DbPoolConfig::from_env();
    get_or_create_pool(&cfg).map_err(|e| format!("Database error: {e}"))
}

fn open_static_export_read_pool() -> Result<Option<crate::ObservabilityDbPool>, String> {
    let config = Config::from_env();
    if mcp_agent_mail_core::disk::is_sqlite_memory_database_url(&config.database_url) {
        return Ok(None);
    }
    crate::open_observability_db_pool(
        &config.database_url,
        &config.storage_root,
        "static export read route",
    )
    .map(Some)
    .map_err(|err| format!("static export read pool unavailable: {err}"))
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

/// Convert a route like `/mail/demo/search` into a pretty static path.
fn route_html_output_path(route: &str) -> String {
    let trimmed = route.trim_matches('/');
    if trimmed.is_empty() {
        "index.html".to_string()
    } else {
        format!("{trimmed}/index.html")
    }
}

fn export_thread_ref(message_id: i64, thread_id: Option<&str>) -> Option<String> {
    match thread_id.map(str::trim).filter(|thread| !thread.is_empty()) {
        Some(thread_id) => Some(thread_id.to_string()),
        None if message_id > 0 => Some(message_id.to_string()),
        None => None,
    }
}

fn export_thread_route(project_slug: &str, thread_id: &str) -> String {
    crate::mail_ui::mail_thread_href(project_slug, thread_id)
}

const fn export_inbox_scan_is_truncated(row_count: usize) -> bool {
    row_count > EXPORT_INBOX_SCAN_LIMIT
}

fn fetch_export_inbox_rows(
    slug: &str,
    agent_name: &str,
    cx: &Cx,
    pool: &DbPool,
    project_id: i64,
    agent_id: i64,
) -> Result<Vec<queries::InboxRow>, String> {
    let rows = bo(
        cx,
        queries::fetch_inbox(
            cx,
            pool,
            project_id,
            agent_id,
            false,
            None,
            EXPORT_INBOX_SCAN_LIMIT.saturating_add(1),
        ),
    )?;

    if export_inbox_scan_is_truncated(rows.len()) {
        return Err(format!(
            "static export aborted because project {slug} agent {agent_name} has at least {EXPORT_INBOX_SCAN_LIMIT} inbox rows and export would be truncated"
        ));
    }

    Ok(rows)
}

/// Render a single HTML route via the web UI dispatcher and write to its
/// static path mirror.
fn emit_html_route(
    path: &str,
    query: &str,
    output_dir: &Path,
    files: &mut BTreeMap<String, ManifestEntry>,
) -> Result<(), String> {
    let file_path = route_html_output_path(path);
    emit_route(path, query, &file_path, output_dir, files)
}

/// Render a single route via the web UI dispatcher and write to a file.
fn emit_route(
    path: &str,
    query: &str,
    file_path: &str,
    output_dir: &Path,
    files: &mut BTreeMap<String, ManifestEntry>,
) -> Result<(), String> {
    match crate::mail_ui::dispatch(path, query, "GET", "") {
        Ok(Some(html)) => {
            let dest = output_dir.join(file_path);
            write_html(&dest, &html).map_err(|err| {
                format!("failed to write exported route {path} -> {file_path}: {err}")
            })?;
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
            Ok(())
        }
        Ok(None) => Err(format!(
            "exported route was not handled by mail UI dispatcher: {path} -> {file_path}"
        )),
        Err((status, msg)) => Err(format!(
            "route failed during static export: {path}{} (status {status}): {msg}",
            if query.is_empty() {
                String::new()
            } else {
                format!("?{query}")
            }
        )),
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

    // Project overview
    emit_html_route(&prefix, "__static_export=1", output_dir, files)?;

    // Search (empty query → shows interface)
    emit_html_route(
        &format!("{prefix}/search"),
        "__static_export=1",
        output_dir,
        files,
    )?;

    // File reservations
    emit_html_route(
        &format!("{prefix}/file_reservations"),
        "",
        output_dir,
        files,
    )?;

    // Attachments
    emit_html_route(&format!("{prefix}/attachments"), "", output_dir, files)?;

    // Get project ID for agent/message queries.
    let project = bo(cx, queries::get_project_by_slug(cx, pool, slug))?;
    let pid = project.id.unwrap_or(0);

    // ── Agents → inbox pages ────────────────────────────────────────
    let agents = bo(cx, queries::list_agents(cx, pool, pid))?;
    for agent in &agents {
        let name = &agent.name;
        emit_html_route(
            &format!("{prefix}/inbox/{name}"),
            "__static_export=1",
            output_dir,
            files,
        )?;
    }

    // ── Threads and messages ────────────────────────────────────────
    // Collect all messages for this project by iterating agent inboxes.
    let mut seen_threads = std::collections::BTreeSet::new();
    let mut seen_messages = std::collections::BTreeSet::new();

    for agent in &agents {
        let aid = agent.id.unwrap_or(0);
        let inbox = fetch_export_inbox_rows(slug, &agent.name, cx, pool, pid, aid)?;
        for row in &inbox {
            let msg = &row.message;
            let mid = msg.id.unwrap_or(0);
            if mid > 0 {
                seen_messages.insert(mid);
            }
            if let Some(thread_ref) = export_thread_ref(mid, msg.thread_id.as_deref()) {
                seen_threads.insert(thread_ref);
            }
        }
    }

    // Render thread pages.
    for tid in &seen_threads {
        emit_html_route(&export_thread_route(slug, tid), "", output_dir, files)?;
    }

    // Render message detail pages.
    for mid in &seen_messages {
        emit_html_route(&format!("{prefix}/message/{mid}"), "", output_dir, files)?;
    }

    Ok(())
}

/// Emit archive visualization routes.
fn emit_archive_routes(
    output_dir: &Path,
    files: &mut BTreeMap<String, ManifestEntry>,
) -> Result<(), String> {
    let routes = [
        "guide",
        "timeline",
        "activity",
        "browser",
        "network",
        "time-travel",
    ];
    for route in &routes {
        emit_html_route(&format!("/mail/archive/{route}"), "", output_dir, files)?;
    }
    Ok(())
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
    body_text: String,
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
    let mut seen_message_ids = std::collections::BTreeSet::new();

    for slug in project_slugs {
        let project = bo(cx, queries::get_project_by_slug(cx, pool, slug))?;
        let pid = project.id.unwrap_or(0);
        let agents = bo(cx, queries::list_agents(cx, pool, pid))?;

        for agent in &agents {
            let aid = agent.id.unwrap_or(0);
            let inbox = fetch_export_inbox_rows(slug, &agent.name, cx, pool, pid, aid)?;
            for row in inbox {
                let msg = row.message;
                let mid = msg.id.unwrap_or(0);
                // Deduplicate by message ID.
                if !seen_message_ids.insert(mid) {
                    continue;
                }
                // Defense-in-depth: scan subject and body text for leaked secrets
                let (safe_subject, _) = scan_for_secrets(&msg.subject);
                let (safe_body_text, _) = scan_for_secrets(&msg.body_md);
                let excerpt = truncate(&msg.body_md, 300);
                let (safe_excerpt, _) = scan_for_secrets(&excerpt);
                entries.push(SearchIndexEntry {
                    id: mid,
                    project: slug.clone(),
                    subject: safe_subject,
                    body_excerpt: safe_excerpt,
                    body_text: safe_body_text,
                    from_agent: row.sender_name.clone(),
                    thread_id: export_thread_ref(mid, msg.thread_id.as_deref()).unwrap_or_default(),
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
    write_and_record(output_dir, "index.html", redirect.as_bytes(), "/", files)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// File I/O helpers
// ---------------------------------------------------------------------------

fn write_html(dest: &Path, html: &str) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        ensure_real_directory(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    if fs::symlink_metadata(dest).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(format!(
            "refusing to write through symlinked export path {}",
            dest.display()
        ));
    }
    fs::write(dest, html.as_bytes()).map_err(|e| format!("write {}: {e}", dest.display()))
}

fn write_to_file(dest: &Path, data: &[u8]) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        ensure_real_directory(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    if fs::symlink_metadata(dest).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(format!(
            "refusing to write through symlinked export path {}",
            dest.display()
        ));
    }
    fs::write(dest, data).map_err(|e| format!("write {}: {e}", dest.display()))
}

fn ensure_real_directory(path: &Path) -> std::io::Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        use std::path::Component;

        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(std::io::Error::other(format!(
                    "refusing to create directory with parent traversal: {}",
                    path.display()
                )));
            }
            Component::Normal(segment) => {
                current.push(segment);
                match fs::symlink_metadata(&current) {
                    Ok(metadata) => {
                        if metadata.file_type().is_symlink() {
                            return Err(std::io::Error::other(format!(
                                "refusing to traverse symlinked export directory {}",
                                current.display()
                            )));
                        }
                        if !metadata.file_type().is_dir() {
                            return Err(std::io::Error::other(format!(
                                "expected export directory but found non-directory {}",
                                current.display()
                            )));
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        fs::create_dir(&current)?;
                    }
                    Err(error) => return Err(error),
                }
            }
        }
    }
    Ok(())
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
    const HEX_CHARS: &[u8] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX_CHARS[(b >> 4) as usize] as char);
        s.push(HEX_CHARS[(b & 0x0f) as usize] as char);
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
#[cfg(test)]
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
    use std::path::{Path, PathBuf};

    fn write_archive_ahead_fixture(base: &Path) -> (PathBuf, PathBuf) {
        let storage_root = base.join("storage");
        let db_path = base.join("static-export-stale.sqlite3");
        let project_dir = storage_root.join("projects").join("ahead-project");
        let agent_dir = project_dir.join("agents").join("Alice");
        let messages_dir = project_dir.join("messages").join("2026").join("03");
        std::fs::create_dir_all(&agent_dir).expect("create agent dir");
        std::fs::create_dir_all(&messages_dir).expect("create messages dir");
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"ahead-project","human_key":"/ahead-project","created_at":0}"#,
        )
        .expect("write project metadata");
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"agent_name":"Alice","program":"coder","model":"test","registered_ts":"2026-03-22T00:00:00Z"}"#,
        )
        .expect("write agent profile");
        std::fs::write(
            messages_dir.join("2026-03-22T12-00-00Z__first__1.md"),
            r#"---json
{
  "id": 1,
  "from": "Alice",
  "to": ["Bob"],
  "subject": "First copy",
  "importance": "normal",
  "created_ts": "2026-03-22T12:00:00Z"
}
---

first body
"#,
        )
        .expect("write canonical message");

        let conn = mcp_agent_mail_db::DbConn::open_file(db_path.to_string_lossy().as_ref())
            .expect("open db");
        conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
            .expect("init schema");
        drop(conn);
        (storage_root, db_path)
    }

    fn with_static_export_env<T>(
        storage_root: &Path,
        database_path: &Path,
        f: impl FnOnce() -> T,
    ) -> T {
        let database_url = format!("sqlite:///{}", database_path.display());
        let storage_root_str = storage_root
            .to_str()
            .expect("static export storage root utf-8")
            .to_string();
        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[
                ("DATABASE_URL", database_url.as_str()),
                ("STORAGE_ROOT", storage_root_str.as_str()),
            ],
            f,
        )
    }

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
    fn emit_route_reports_missing_dispatch() {
        let dir = PathBuf::from("/tmp/static_export_test_missing");
        let _ = fs::remove_dir_all(&dir);
        let mut files = BTreeMap::new();
        let err = emit_route("/nonexistent/route", "", "test.html", &dir, &mut files)
            .expect_err("missing dispatch route should fail export");
        assert!(err.contains("not handled by mail UI dispatcher"));
        assert!(files.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_thread_ref_uses_explicit_thread_id_when_present() {
        assert_eq!(
            export_thread_ref(42, Some("thread-123")),
            Some("thread-123".to_string())
        );
    }

    #[test]
    fn export_thread_ref_uses_message_id_for_root_messages() {
        assert_eq!(export_thread_ref(42, None), Some("42".to_string()));
        assert_eq!(export_thread_ref(42, Some("")), Some("42".to_string()));
    }

    #[test]
    fn export_thread_route_encodes_path_special_thread_ids() {
        let route = export_thread_route("demo", "topic/with space+plus");
        assert_eq!(route, "/mail/demo/thread/topic%2Fwith%20space%2Bplus");
        assert_eq!(
            route_html_output_path(&route),
            "mail/demo/thread/topic%2Fwith%20space%2Bplus/index.html"
        );
    }

    #[test]
    fn export_thread_ref_returns_none_for_invalid_zero_message_without_thread() {
        assert_eq!(export_thread_ref(0, None), None);
    }

    #[test]
    fn export_inbox_scan_is_truncated_only_above_limit() {
        assert!(!export_inbox_scan_is_truncated(EXPORT_INBOX_SCAN_LIMIT - 1));
        assert!(!export_inbox_scan_is_truncated(EXPORT_INBOX_SCAN_LIMIT));
        assert!(export_inbox_scan_is_truncated(
            EXPORT_INBOX_SCAN_LIMIT.saturating_add(1)
        ));
    }

    #[test]
    fn resolve_requested_project_slugs_rejects_unknown_slugs() {
        let err = resolve_requested_project_slugs(
            &["alpha".to_string(), "beta".to_string()],
            &["beta".to_string(), "missing".to_string()],
        )
        .expect_err("unknown requested slug should fail");
        assert!(err.contains("missing"));
    }

    #[test]
    fn resolve_requested_project_slugs_preserves_requested_order_without_duplicates() {
        let resolved = resolve_requested_project_slugs(
            &["alpha".to_string(), "beta".to_string(), "gamma".to_string()],
            &[
                "gamma".to_string(),
                "alpha".to_string(),
                "gamma".to_string(),
            ],
        )
        .expect("requested slugs should resolve");
        assert_eq!(resolved, vec!["gamma".to_string(), "alpha".to_string()]);
    }

    #[test]
    fn ensure_export_features_supported_rejects_archive_routes() {
        let config = ExportConfig {
            output_dir: PathBuf::from("/tmp/static-export-features"),
            projects: Vec::new(),
            include_archive: true,
            include_search_index: true,
        };
        let err = ensure_export_features_supported(&config)
            .expect_err("archive routes should not be exported yet");
        assert!(err.contains("--include-archive"));
    }

    #[test]
    fn ensure_export_features_supported_requires_search_index() {
        let config = ExportConfig {
            output_dir: PathBuf::from("/tmp/static-export-features"),
            projects: Vec::new(),
            include_archive: false,
            include_search_index: false,
        };
        let err =
            ensure_export_features_supported(&config).expect_err("search index should be required");
        assert!(err.contains("search index"));
    }

    #[test]
    fn ensure_output_dir_empty_rejects_non_empty_directory() {
        let dir = PathBuf::from("/tmp/static_export_test_nonempty");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create test dir");
        fs::write(dir.join("stale.txt"), "stale").expect("seed stale file");

        let err = ensure_output_dir_empty(&dir).expect_err("non-empty dir should fail");
        assert!(err.contains("must be empty"));
        assert!(err.contains("stale.txt"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_output_dir_empty_rejects_symlink_directory() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let real_output = dir.path().join("real-output");
        std::fs::create_dir(&real_output).unwrap();
        let symlink_output = dir.path().join("output-link");
        symlink(&real_output, &symlink_output).unwrap();

        let err = ensure_output_dir_empty(&symlink_output)
            .expect_err("symlinked output dir should be rejected");
        assert!(err.contains("must not be a symlink"));
    }

    #[cfg(unix)]
    #[test]
    fn export_static_site_rejects_symlinked_parent_for_missing_output_dir() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let linked_parent = dir.path().join("linked-parent");
        symlink(&outside, &linked_parent).unwrap();

        let config = ExportConfig {
            output_dir: linked_parent.join("export"),
            projects: Vec::new(),
            include_archive: false,
            include_search_index: true,
        };

        let err = export_static_site(&config)
            .expect_err("symlinked parent path should be rejected before any DB work");
        assert!(err.contains("symlinked export directory"));
    }

    #[cfg(unix)]
    #[test]
    fn publish_staged_export_output_rolls_back_partial_publish_on_failure() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("output");
        fs::create_dir(&output).unwrap();
        let stage = create_export_stage(&output).unwrap();
        fs::write(stage.path().join("manifest.json"), b"{}").unwrap();
        let outside = dir.path().join("outside.txt");
        fs::write(&outside, "bad").unwrap();
        symlink(&outside, stage.path().join("zz-bad")).unwrap();

        let err = publish_staged_export_output(stage.path(), &output)
            .expect_err("symlinked staged entry should fail publish");
        assert!(err.to_string().contains("symlinked staged export path"));
        assert!(output.read_dir().unwrap().next().is_none());
        assert!(stage.path().join("manifest.json").exists());
        assert!(stage.path().join("zz-bad").exists());
    }

    #[test]
    fn export_static_site_uses_archive_snapshot_when_live_db_is_stale() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (storage_root, db_path) = write_archive_ahead_fixture(dir.path());
        let output_dir = dir.path().join("export");
        let config = ExportConfig {
            output_dir: output_dir.clone(),
            projects: Vec::new(),
            include_archive: false,
            include_search_index: true,
        };

        let manifest = with_static_export_env(&storage_root, &db_path, || {
            export_static_site(&config).expect("static export should succeed")
        });

        assert!(
            manifest.files.contains_key("mail/ahead-project/index.html"),
            "{manifest:?}"
        );
        let project_html =
            std::fs::read_to_string(output_dir.join("mail/ahead-project/index.html"))
                .expect("read exported project page");
        assert!(project_html.contains("ahead-project"), "{project_html}");
        assert!(project_html.contains("Alice"), "{project_html}");

        let search_index = std::fs::read_to_string(output_dir.join("search-index.json"))
            .expect("read search index");
        assert!(
            search_index.contains("\"project\": \"ahead-project\""),
            "{search_index}"
        );
        assert!(
            search_index.contains("\"from_agent\": \"Alice\""),
            "{search_index}"
        );
    }

    #[test]
    fn export_static_site_fails_closed_when_archive_snapshot_setup_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (storage_root, db_path) = write_archive_ahead_fixture(dir.path());
        let output_dir = dir.path().join("export");
        let config = ExportConfig {
            output_dir,
            projects: Vec::new(),
            include_archive: false,
            include_search_index: true,
        };
        let tmpdir_file = dir.path().join("tmpdir-file");
        std::fs::write(&tmpdir_file, "not a directory").expect("write tmpdir file");
        let tmpdir = tmpdir_file
            .to_str()
            .expect("tmpdir override utf-8")
            .to_string();

        let err = with_static_export_env(&storage_root, &db_path, || {
            mcp_agent_mail_core::config::with_process_env_overrides_for_test(
                &[("TMPDIR", tmpdir.as_str())],
                || export_static_site(&config),
            )
        })
        .expect_err("static export should fail closed when snapshot setup fails");

        assert!(err.contains("static export read pool unavailable"), "{err}");
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

    #[cfg(unix)]
    #[test]
    fn write_and_record_rejects_symlinked_descendant_directory() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("output");
        fs::create_dir_all(&output).unwrap();
        let outside = dir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, output.join("nested")).unwrap();

        let mut files = BTreeMap::new();
        let err = write_and_record(&output, "nested/test.txt", b"hello", "/test", &mut files)
            .expect_err("symlinked descendant directory should be rejected");
        assert!(err.contains("symlinked export directory"));
    }

    #[test]
    fn hex_encode_empty() {
        assert_eq!(hex_encode(&[]), "");
    }

    #[test]
    fn hex_encode_single_byte() {
        assert_eq!(hex_encode(&[0x00]), "00");
        assert_eq!(hex_encode(&[0x0a]), "0a");
        assert_eq!(hex_encode(&[0xff]), "ff");
    }

    #[test]
    fn hex_encode_multi_byte() {
        assert_eq!(hex_encode(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
        assert_eq!(hex_encode(&[0x01, 0x23, 0x45, 0x67]), "01234567");
    }

    #[test]
    fn hex_encode_always_lowercase_padded() {
        // Ensure leading zeros are preserved
        assert_eq!(hex_encode(&[0x00, 0x01, 0x02]), "000102");
        // Ensure lowercase
        assert_eq!(hex_encode(&[0xAB, 0xCD]), "abcd");
    }

    // ── br-3h13: Additional static_export.rs test coverage ─────────

    #[test]
    fn sanitize_filename_control_chars() {
        let input = "file\x00name\x1f.html";
        let result = sanitize_filename(input);
        assert!(!result.contains('\x00'));
        assert!(!result.contains('\x1f'));
        assert!(result.contains("file_name_.html"));
    }

    #[test]
    fn sanitize_filename_all_special_chars() {
        let input = r#"a/b\c:d*e?f"g<h>i|j"#;
        let result = sanitize_filename(input);
        for ch in ['/', '\\', ':', '*', '?', '"', '<', '>', '|'] {
            assert!(!result.contains(ch), "should replace '{ch}'");
        }
        assert_eq!(result, "a_b_c_d_e_f_g_h_i_j");
    }

    #[test]
    fn sanitize_filename_normal_chars_unchanged() {
        let input = "my-file_v2.0.html";
        assert_eq!(sanitize_filename(input), input);
    }

    #[test]
    fn truncate_empty_string() {
        assert_eq!(truncate("", 100), "");
        // Empty string with max=0 still returns "" because len(0) <= max(0)
        assert_eq!(truncate("", 0), "");
    }

    #[test]
    fn truncate_exact_length() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn truncate_multibyte_chars() {
        // Each emoji is 4 bytes
        let s = "🎉🎊🎈";
        let result = truncate(s, 5);
        // Should truncate on char boundary (4 bytes for first emoji)
        assert!(result.ends_with('…'));
        assert!(result.len() <= 8); // 4 bytes + 3 bytes for …
    }

    #[test]
    fn sha256_hex_known_vector() {
        // SHA-256 of empty string
        let hash = sha256_hex(b"");
        assert_eq!(
            hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_hex_hello() {
        let hash = sha256_hex(b"hello");
        assert_eq!(
            hash,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn content_hash_empty_files() {
        let files = BTreeMap::new();
        let hash = compute_content_hash(&files);
        assert_eq!(hash.len(), 64);
        // Empty files should produce the SHA-256 of empty string
        assert_eq!(
            hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn content_hash_different_files_differ() {
        let mut files1 = BTreeMap::new();
        files1.insert(
            "a.html".to_string(),
            ManifestEntry {
                route: "/a".to_string(),
                size: 10,
                sha256: "abc".to_string(),
            },
        );

        let mut files2 = BTreeMap::new();
        files2.insert(
            "b.html".to_string(),
            ManifestEntry {
                route: "/b".to_string(),
                size: 10,
                sha256: "def".to_string(),
            },
        );

        assert_ne!(compute_content_hash(&files1), compute_content_hash(&files2));
    }

    #[test]
    fn manifest_entry_debug_and_clone() {
        let entry = ManifestEntry {
            route: "/test".to_string(),
            size: 42,
            sha256: "abc123".to_string(),
        };
        let cloned = entry.clone();
        assert_eq!(cloned.route, "/test");
        assert_eq!(cloned.size, 42);
        let debug = format!("{entry:?}");
        assert!(debug.contains("/test"));
    }

    #[test]
    fn export_manifest_serializes() {
        let manifest = ExportManifest {
            schema_version: "1.0.0".to_string(),
            generated_at: "2026-02-19T00:00:00Z".to_string(),
            file_count: 2,
            total_bytes: 100,
            content_hash: "abc".repeat(22),
            files: BTreeMap::new(),
        };
        let json = serde_json::to_string_pretty(&manifest).unwrap();
        assert!(json.contains("schema_version"));
        assert!(json.contains("1.0.0"));
        assert!(json.contains("total_bytes"));
        assert!(json.contains("content_hash"));
    }

    #[test]
    fn write_html_creates_nested_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("a").join("b").join("c").join("page.html");
        write_html(&dest, "<html></html>").unwrap();
        assert!(dest.exists());
        assert_eq!(fs::read_to_string(&dest).unwrap(), "<html></html>");
    }

    #[test]
    fn write_to_file_creates_parent() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("nested").join("data.json");
        write_to_file(&dest, b"{}").unwrap();
        assert!(dest.exists());
    }

    #[test]
    fn route_html_output_path_mirrors_pretty_urls() {
        assert_eq!(route_html_output_path("/"), "index.html");
        assert_eq!(route_html_output_path("/mail"), "mail/index.html");
        assert_eq!(
            route_html_output_path("/mail/demo/search"),
            "mail/demo/search/index.html"
        );
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
        assert!(files.contains_key("index.html"));
        let _ = fs::remove_dir_all(&dir);
    }
}
