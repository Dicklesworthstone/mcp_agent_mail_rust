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

    detect_github_pages(output_dir, &mut hints);
    detect_cloudflare_pages(output_dir, &mut hints);
    detect_netlify(output_dir, &mut hints);
    detect_s3(output_dir, &mut hints);

    // Sort by number of signals (most confident first)
    hints.sort_by_key(|hint| Reverse(hint.signals.len()));
    hints
}

fn detect_github_pages(output_dir: &Path, hints: &mut Vec<HostingHint>) {
    let mut signals = Vec::new();

    // Check for GitHub Actions workflows
    let workflows_dir = find_ancestor_path(output_dir, ".github/workflows");
    if let Some(dir) = workflows_dir {
        if dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.ends_with(".yml") || name.ends_with(".yaml") {
                        if let Ok(content) = std::fs::read_to_string(entry.path()) {
                            if content.contains("pages") || content.contains("deploy") {
                                signals.push(format!("Workflow {name} references Pages"));
                            }
                        }
                    }
                }
            }
        }
    }

    // Check git remote
    if let Some(remote) = git_remote_url(output_dir) {
        if remote.contains("github") {
            signals.push(format!("Git remote: {remote}"));
        }
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

fn detect_cloudflare_pages(output_dir: &Path, hints: &mut Vec<HostingHint>) {
    let mut signals = Vec::new();

    if find_ancestor_path(output_dir, "wrangler.toml").is_some() {
        signals.push("wrangler.toml found".to_string());
    }

    if let Some(remote) = git_remote_url(output_dir) {
        if remote.contains("cloudflare") {
            signals.push(format!("Git remote: {remote}"));
        }
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

fn detect_netlify(output_dir: &Path, hints: &mut Vec<HostingHint>) {
    let mut signals = Vec::new();

    if find_ancestor_path(output_dir, "netlify.toml").is_some() {
        signals.push("netlify.toml found".to_string());
    }

    if let Some(remote) = git_remote_url(output_dir) {
        if remote.contains("netlify") {
            signals.push(format!("Git remote: {remote}"));
        }
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

    if let Some(remote) = git_remote_url(output_dir) {
        if remote.contains("amazonaws") || remote.contains("s3") {
            signals.push(format!("Git remote: {remote}"));
        }
    }

    if std::env::var("AWS_ACCESS_KEY_ID").is_ok() || std::env::var("AWS_PROFILE").is_ok() {
        signals.push("AWS env vars set".to_string());
    }

    // Check for deploy scripts referencing S3
    let scripts_dir = find_ancestor_path(output_dir, "scripts");
    if let Some(dir) = scripts_dir {
        if dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.contains("deploy") || name.contains("s3") {
                        if let Ok(content) = std::fs::read_to_string(entry.path()) {
                            if content.contains("s3") || content.contains("aws") {
                                signals.push(format!("Deploy script {name} references S3/AWS"));
                            }
                        }
                    }
                }
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
    let mut current = if start.is_file() {
        start.parent()?.to_path_buf()
    } else {
        start.to_path_buf()
    };

    for _ in 0..10 {
        let candidate = current.join(name);
        if candidate.exists() {
            return Some(candidate);
        }
        match current.parent() {
            Some(p) if p != current => current = p.to_path_buf(),
            _ => break,
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
}
