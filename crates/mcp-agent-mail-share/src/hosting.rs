//! Hosting platform detection for bundle deployment.
//!
//! Auto-detects GitHub Pages, Cloudflare Pages, Netlify, and S3 based on
//! filesystem artifacts, git remotes, and environment variables.

use std::{cmp::Reverse, path::Path};

use serde::{Deserialize, Serialize};

/// A hosting hint with deployment instructions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostingHint {
    pub id: String,
    pub title: String,
    pub summary: String,
    pub instructions: Vec<String>,
    pub signals: Vec<String>,
}

/// Detect hosting platform hints for the given output directory.
///
/// Returns a list of hints sorted by confidence (most signals first).
#[must_use]
pub fn detect_hosting_hints(output_dir: &Path) -> Vec<HostingHint> {
    let mut hints: Vec<HostingHint> = Vec::new();
    let git_remote = git_remote_url(output_dir);

    detect_github_pages(output_dir, git_remote.as_deref(), &mut hints);
    detect_cloudflare_pages(output_dir, git_remote.as_deref(), &mut hints);
    detect_netlify(output_dir, git_remote.as_deref(), &mut hints);
    detect_s3(output_dir, &mut hints);

    // Sort by number of signals (most confident first)
    hints.sort_by_key(|hint| Reverse(hint.signals.len()));
    hints
}

fn detect_github_pages(output_dir: &Path, git_remote: Option<&str>, hints: &mut Vec<HostingHint>) {
    let mut signals = Vec::new();

    // Check for GitHub Actions workflows
    let workflows_dir = find_ancestor_path(output_dir, ".github/workflows");
    if let Some(dir) = workflows_dir
        && crate::is_real_dir(&dir)
        && let Ok(entries) = std::fs::read_dir(&dir)
    {
        let mut workflow_files: Vec<_> = entries
            .flatten()
            .map(|entry| {
                (
                    entry.file_name().to_string_lossy().to_string(),
                    entry.path(),
                )
            })
            .filter(|(_, path)| crate::is_real_file(path))
            .collect();
        workflow_files.sort_by(|a, b| a.0.cmp(&b.0));

        for (name, path) in workflow_files {
            if (name.ends_with(".yml") || name.ends_with(".yaml"))
                && let Ok(content) = std::fs::read_to_string(path)
                && (content.contains("pages") || content.contains("deploy"))
            {
                signals.push(format!("Workflow {name} references Pages"));
            }
        }
    }

    // Check git remote
    if let Some(remote) = git_remote
        && remote.contains("github")
    {
        signals.push(format!("Git remote: {remote}"));
    }

    // Check environment
    if std::env::var("GITHUB_REPOSITORY").is_ok() {
        signals.push("GITHUB_REPOSITORY env var set".to_string());
    }

    // Check if inside docs/ directory
    if is_inside_docs_dir(output_dir) {
        signals.push("Output inside docs/ directory".to_string());
    }

    if !signals.is_empty() {
        hints.push(HostingHint {
            id: "github_pages".to_string(),
            title: "GitHub Pages".to_string(),
            summary: "Deploy via GitHub Pages with .nojekyll and COI service worker".to_string(),
            instructions: vec![
                "Ensure .nojekyll file is in the root".to_string(),
                "Enable GitHub Pages in repo Settings > Pages".to_string(),
                "Include coi-serviceworker.js for OPFS/SharedArrayBuffer support".to_string(),
            ],
            signals,
        });
    }
}

fn detect_cloudflare_pages(
    output_dir: &Path,
    git_remote: Option<&str>,
    hints: &mut Vec<HostingHint>,
) {
    let mut signals = Vec::new();

    if find_ancestor_path(output_dir, "wrangler.toml").is_some() {
        signals.push("wrangler.toml found".to_string());
    }

    if let Some(remote) = git_remote
        && remote.contains("cloudflare")
    {
        signals.push(format!("Git remote: {remote}"));
    }

    if std::env::var("CF_PAGES").is_ok() {
        signals.push("CF_PAGES env var set".to_string());
    }

    if !signals.is_empty() {
        hints.push(HostingHint {
            id: "cloudflare_pages".to_string(),
            title: "Cloudflare Pages".to_string(),
            summary: "Deploy via Cloudflare Pages with _headers for COOP/COEP".to_string(),
            instructions: vec![
                "Push to your Cloudflare Pages project".to_string(),
                "The _headers file configures COOP/COEP automatically".to_string(),
            ],
            signals,
        });
    }
}

fn detect_netlify(output_dir: &Path, git_remote: Option<&str>, hints: &mut Vec<HostingHint>) {
    let mut signals = Vec::new();

    if find_ancestor_path(output_dir, "netlify.toml").is_some() {
        signals.push("netlify.toml found".to_string());
    }

    if let Some(remote) = git_remote
        && remote.contains("netlify")
    {
        signals.push(format!("Git remote: {remote}"));
    }

    if std::env::var("NETLIFY").is_ok() {
        signals.push("NETLIFY env var set".to_string());
    }

    if !signals.is_empty() {
        hints.push(HostingHint {
            id: "netlify".to_string(),
            title: "Netlify".to_string(),
            summary: "Deploy via Netlify with _headers for COOP/COEP".to_string(),
            instructions: vec![
                "Push to your Netlify site or drag-and-drop the bundle".to_string(),
                "The _headers file configures COOP/COEP automatically".to_string(),
            ],
            signals,
        });
    }
}

fn detect_s3(output_dir: &Path, hints: &mut Vec<HostingHint>) {
    let mut signals = Vec::new();

    // Check for deploy scripts referencing S3
    let scripts_dir = find_ancestor_path(output_dir, "scripts");
    if let Some(dir) = scripts_dir
        && crate::is_real_dir(&dir)
        && let Ok(entries) = std::fs::read_dir(&dir)
    {
        let mut script_files: Vec<_> = entries
            .flatten()
            .map(|entry| {
                (
                    entry.file_name().to_string_lossy().to_string(),
                    entry.path(),
                )
            })
            .filter(|(_, path)| crate::is_real_file(path))
            .collect();
        script_files.sort_by(|a, b| a.0.cmp(&b.0));

        for (name, path) in script_files {
            if (name.contains("deploy") || name.contains("s3"))
                && let Ok(content) = std::fs::read_to_string(path)
                && content.contains("aws s3")
            {
                signals.push(format!("Found S3 deployment script: {name}"));
            }
        }
    }

    if !signals.is_empty() {
        hints.push(HostingHint {
            id: "s3".to_string(),
            title: "Amazon S3".to_string(),
            summary: "Deploy to S3 with CloudFront for COOP/COEP headers".to_string(),
            instructions: vec![
                "Upload bundle to S3 bucket".to_string(),
                "Configure CloudFront distribution with COOP/COEP response headers".to_string(),
                "Set Content-Type for .sqlite3 files to application/x-sqlite3".to_string(),
            ],
            signals,
        });
    }
}

/// Walk ancestor directories looking for a specific file/dir.
fn find_ancestor_path(start: &Path, name: &str) -> Option<std::path::PathBuf> {
    let search_root = if crate::is_real_file(start) {
        start.parent()?
    } else if crate::is_real_dir(start) {
        start
    } else {
        return None;
    };

    for current in search_root.ancestors() {
        let candidate = current.join(name);
        if std::fs::symlink_metadata(&candidate)
            .is_ok_and(|metadata| metadata.file_type().is_file() || metadata.file_type().is_dir())
        {
            return Some(candidate);
        }
    }
    None
}

/// Try to extract the git remote URL for the directory.
fn git_remote_url(dir: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(dir)
        .output()
        .ok()?;
    if output.status.success() {
        let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !url.is_empty() {
            return Some(url);
        }
    }
    None
}

/// Check if path is inside a `docs/` directory.
fn is_inside_docs_dir(path: &Path) -> bool {
    let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    abs.components().any(|c| {
        c.as_os_str()
            .to_str()
            .is_some_and(|s| s.eq_ignore_ascii_case("docs"))
    })
}

/// Generate COOP/COEP headers content for `_headers` file.
///
/// Format matches the legacy Python output exactly (Cloudflare Pages / Netlify compatible).
#[must_use]
pub fn generate_headers_file() -> String {
    "\
# Cross-Origin Isolation headers for OPFS and SharedArrayBuffer support
# Compatible with Cloudflare Pages and Netlify
# See: https://web.dev/coop-coep/

/*
  Cross-Origin-Opener-Policy: same-origin
  Cross-Origin-Embedder-Policy: require-corp

# Allow viewer assets to be loaded
/viewer/*
  Cross-Origin-Resource-Policy: same-origin

# SQLite database and chunks
/*.sqlite3
  Cross-Origin-Resource-Policy: same-origin
  Content-Type: application/x-sqlite3

/chunks/*
  Cross-Origin-Resource-Policy: same-origin
  Content-Type: application/octet-stream

# Attachments
/attachments/*
  Cross-Origin-Resource-Policy: same-origin
"
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_file_contains_coop_coep() {
        let headers = generate_headers_file();
        assert!(headers.contains("Cross-Origin-Opener-Policy: same-origin"));
        assert!(headers.contains("Cross-Origin-Embedder-Policy: require-corp"));
    }

    #[test]
    fn empty_dir_detects_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let hints = detect_hosting_hints(dir.path());
        // May find nothing or env-based hints
        for hint in &hints {
            assert!(!hint.signals.is_empty());
        }
    }

    #[test]
    fn headers_file_contains_sqlite3_content_type() {
        let headers = generate_headers_file();
        assert!(headers.contains("Content-Type: application/x-sqlite3"));
    }

    #[test]
    fn headers_file_contains_cors_resource_policy() {
        let headers = generate_headers_file();
        assert!(headers.contains("Cross-Origin-Resource-Policy: same-origin"));
    }

    #[test]
    fn headers_file_contains_viewer_section() {
        let headers = generate_headers_file();
        assert!(headers.contains("/viewer/*"));
    }

    #[test]
    fn headers_file_contains_chunks_section() {
        let headers = generate_headers_file();
        assert!(headers.contains("/chunks/*"));
        assert!(headers.contains("Content-Type: application/octet-stream"));
    }

    #[test]
    fn headers_file_contains_attachments_section() {
        let headers = generate_headers_file();
        assert!(headers.contains("/attachments/*"));
    }

    #[test]
    fn wrangler_toml_triggers_cloudflare_hint() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("wrangler.toml"), "[vars]").unwrap();
        let hints = detect_hosting_hints(dir.path());
        let cf = hints.iter().find(|h| h.id == "cloudflare_pages");
        assert!(
            cf.is_some(),
            "wrangler.toml should trigger cloudflare_pages hint"
        );
        let cf = cf.unwrap();
        assert!(cf.signals.iter().any(|s| s.contains("wrangler.toml")));
    }

    #[test]
    fn netlify_toml_triggers_netlify_hint() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("netlify.toml"), "[build]").unwrap();
        let hints = detect_hosting_hints(dir.path());
        let nl = hints.iter().find(|h| h.id == "netlify");
        assert!(nl.is_some(), "netlify.toml should trigger netlify hint");
        let nl = nl.unwrap();
        assert!(nl.signals.iter().any(|s| s.contains("netlify.toml")));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_wrangler_toml_is_ignored() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let real = outside.path().join("wrangler.toml");
        std::fs::write(&real, "[vars]").unwrap();
        symlink(&real, dir.path().join("wrangler.toml")).unwrap();

        let hints = detect_hosting_hints(dir.path());
        assert!(
            hints.iter().all(|hint| hint.id != "cloudflare_pages"),
            "symlinked wrangler.toml should not trigger cloudflare hint"
        );
    }

    #[test]
    fn hints_sorted_by_signal_count_descending() {
        let dir = tempfile::tempdir().unwrap();
        // Create both wrangler.toml and netlify.toml
        std::fs::write(dir.path().join("wrangler.toml"), "").unwrap();
        std::fs::write(dir.path().join("netlify.toml"), "").unwrap();
        let hints = detect_hosting_hints(dir.path());
        // Verify descending sort by signals count
        for window in hints.windows(2) {
            assert!(
                window[0].signals.len() >= window[1].signals.len(),
                "hints should be sorted by signal count descending"
            );
        }
    }

    #[test]
    fn hosting_hint_serialization() {
        let hint = HostingHint {
            id: "test".to_string(),
            title: "Test Platform".to_string(),
            summary: "A test hint".to_string(),
            instructions: vec!["Step 1".to_string()],
            signals: vec!["signal-1".to_string()],
        };
        let json = serde_json::to_value(&hint).unwrap();
        assert_eq!(json["id"], "test");
        assert_eq!(json["title"], "Test Platform");
        assert_eq!(json["signals"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn hosting_hint_deserialization() {
        let json = serde_json::json!({
            "id": "s3",
            "title": "Amazon S3",
            "summary": "Deploy to S3",
            "instructions": ["Upload"],
            "signals": ["AWS env"]
        });
        let hint: HostingHint = serde_json::from_value(json).unwrap();
        assert_eq!(hint.id, "s3");
        assert_eq!(hint.instructions.len(), 1);
    }

    #[test]
    fn docs_subdir_detected() {
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        assert!(is_inside_docs_dir(&docs));
    }

    #[test]
    fn non_docs_dir_not_detected() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        assert!(!is_inside_docs_dir(&src));
    }

    #[test]
    fn find_ancestor_path_finds_in_current() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker.txt"), "").unwrap();
        let result = find_ancestor_path(dir.path(), "marker.txt");
        assert!(result.is_some());
    }

    #[test]
    fn find_ancestor_path_finds_in_parent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker.txt"), "").unwrap();
        let child = dir.path().join("child");
        std::fs::create_dir_all(&child).unwrap();
        let result = find_ancestor_path(&child, "marker.txt");
        assert!(result.is_some());
    }

    #[test]
    fn find_ancestor_path_returns_none_for_missing() {
        let dir = tempfile::tempdir().unwrap();
        let result = find_ancestor_path(dir.path(), "nonexistent.xyz");
        assert!(result.is_none());
    }

    #[test]
    fn find_ancestor_path_walks_past_ten_levels() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker.txt"), "").unwrap();
        let mut nested = dir.path().to_path_buf();
        for depth in 0..12 {
            nested.push(format!("level-{depth}"));
        }
        std::fs::create_dir_all(&nested).unwrap();

        let result = find_ancestor_path(&nested, "marker.txt");
        assert_eq!(result, Some(dir.path().join("marker.txt")));
    }

    #[test]
    fn s3_deploy_script_triggers_hint() {
        let dir = tempfile::tempdir().unwrap();
        let scripts = dir.path().join("scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        std::fs::write(scripts.join("deploy-s3.sh"), "aws s3 sync . s3://bucket").unwrap();
        let hints = detect_hosting_hints(dir.path());
        let s3 = hints.iter().find(|h| h.id == "s3");
        assert!(s3.is_some(), "S3 deploy script should trigger s3 hint");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_scripts_directory_is_ignored_for_s3_detection() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let scripts = outside.path().join("scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        std::fs::write(scripts.join("deploy-s3.sh"), "aws s3 sync . s3://bucket").unwrap();
        symlink(&scripts, dir.path().join("scripts")).unwrap();

        let hints = detect_hosting_hints(dir.path());
        assert!(
            hints.iter().all(|hint| hint.id != "s3"),
            "symlinked scripts directory should not trigger s3 hint"
        );
    }

    #[test]
    fn github_workflow_triggers_hint() {
        let dir = tempfile::tempdir().unwrap();
        let workflows = dir.path().join(".github").join("workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        std::fs::write(
            workflows.join("deploy.yml"),
            "name: Deploy\njobs:\n  pages:\n    runs-on: ubuntu-latest",
        )
        .unwrap();
        // Also need a git repo for the workflow detection
        let hints = detect_hosting_hints(dir.path());
        let gh = hints.iter().find(|h| h.id == "github_pages");
        assert!(
            gh.is_some(),
            "GitHub Actions workflow with 'pages' should trigger github_pages hint"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_workflow_file_is_ignored() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let workflows = dir.path().join(".github").join("workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let real = outside.path().join("deploy.yml");
        std::fs::write(
            &real,
            "name: Deploy\njobs:\n  pages:\n    runs-on: ubuntu-latest",
        )
        .unwrap();
        symlink(&real, workflows.join("deploy.yml")).unwrap();

        let hints = detect_hosting_hints(dir.path());
        assert!(
            hints.iter().all(|hint| hint.id != "github_pages"),
            "symlinked workflow files should not trigger github_pages hint"
        );
    }
}
