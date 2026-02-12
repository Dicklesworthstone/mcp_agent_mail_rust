//! Hosting provider detection for native share wizard.
//!
//! Produces structured, explainable detection results using the wizard domain model.
//! Detects GitHub Pages, Cloudflare Pages, Netlify, S3, and custom deployment contexts
//! based on filesystem artifacts, git remotes, and environment variables.
//!
//! # Design Rationale
//!
//! Detection is deterministic and platform-safe:
//! - No shelling out to Python
//! - All filesystem checks are explicit and logged
//! - Environment variable access is explicit
//! - Git remote detection uses `git remote get-url origin`
//!
//! Each signal includes confidence level and explanation for user transparency.

use std::path::{Path, PathBuf};

use crate::wizard::{DetectedEnvironment, DetectedSignal, DetectionConfidence, HostingProvider};

/// Detect hosting environment for the wizard.
///
/// Probes the filesystem, git repository, and environment variables to produce
/// structured detection results. Returns a fully populated `DetectedEnvironment`
/// with signals sorted by confidence.
///
/// # Arguments
///
/// * `bundle_path` - Path to the bundle directory (or intended output location)
/// * `cwd` - Current working directory for git/environment detection
pub fn detect_environment(bundle_path: Option<&Path>, cwd: &Path) -> DetectedEnvironment {
    let mut env = DetectedEnvironment {
        cwd: cwd.to_path_buf(),
        ..Default::default()
    };

    // Detect Git context
    detect_git_context(cwd, &mut env);

    // Detect environment variables
    detect_env_vars(&mut env);

    // Detect filesystem signals
    detect_filesystem_signals(cwd, &mut env);

    // Check for existing bundle
    if let Some(path) = bundle_path {
        if path.is_dir() && path.join("manifest.json").exists() {
            env.existing_bundle = Some(path.to_path_buf());
            env.signals.push(DetectedSignal {
                source: "filesystem".to_string(),
                detail: format!("Bundle found at {}", path.display()),
                confidence: DetectionConfidence::High,
            });
        }
    }

    // Determine recommended provider based on signals
    env.recommended_provider = determine_recommended_provider(&env);

    // Sort signals by confidence (high to low)
    env.signals.sort_by_key(|a| confidence_order(a.confidence));

    env
}

/// Detect GitHub Pages readiness signals.
///
/// Checks for:
/// - Git remote pointing to GitHub
/// - `.github/workflows/` directory with pages-related workflows
/// - `GITHUB_REPOSITORY` environment variable
/// - docs/ directory location
pub fn detect_github_pages(cwd: &Path) -> Vec<DetectedSignal> {
    let mut signals = Vec::new();

    // Check git remote
    if let Some(url) = git_remote_url(cwd) {
        if url.contains("github.com") || url.contains("github:") {
            signals.push(DetectedSignal {
                source: "git_remote".to_string(),
                detail: format!("Git remote points to GitHub: {url}"),
                confidence: DetectionConfidence::High,
            });
        }
    }

    // Check for GitHub Actions workflows
    if let Some(workflows_dir) = find_ancestor_path(cwd, ".github/workflows") {
        if workflows_dir.is_dir() {
            let mut has_pages_workflow = false;
            if let Ok(entries) = std::fs::read_dir(&workflows_dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_lowercase();
                    if (name.ends_with(".yml") || name.ends_with(".yaml"))
                        && (name.contains("pages") || name.contains("deploy"))
                    {
                        has_pages_workflow = true;
                        signals.push(DetectedSignal {
                            source: "config_file".to_string(),
                            detail: format!("GitHub Actions workflow found: {name}"),
                            confidence: DetectionConfidence::High,
                        });
                    }
                }
            }
            if !has_pages_workflow {
                signals.push(DetectedSignal {
                    source: "config_file".to_string(),
                    detail: ".github/workflows/ directory exists".to_string(),
                    confidence: DetectionConfidence::Medium,
                });
            }
        }
    }

    // Check GITHUB_REPOSITORY env var
    if let Ok(repo) = std::env::var("GITHUB_REPOSITORY") {
        signals.push(DetectedSignal {
            source: "env_var".to_string(),
            detail: format!("GITHUB_REPOSITORY={repo}"),
            confidence: DetectionConfidence::High,
        });
    }

    // Check for docs/ directory (common GitHub Pages pattern)
    let docs_dir = cwd.join("docs");
    if docs_dir.is_dir() {
        signals.push(DetectedSignal {
            source: "filesystem".to_string(),
            detail: "docs/ directory exists (common Pages pattern)".to_string(),
            confidence: DetectionConfidence::Low,
        });
    }

    signals
}

/// Detect Cloudflare Pages readiness signals.
///
/// Checks for:
/// - `wrangler.toml` configuration file
/// - `CF_PAGES` environment variable
/// - Cloudflare-related git remote (rare)
pub fn detect_cloudflare_pages(cwd: &Path) -> Vec<DetectedSignal> {
    let mut signals = Vec::new();

    // Check for wrangler.toml
    if find_ancestor_path(cwd, "wrangler.toml").is_some() {
        signals.push(DetectedSignal {
            source: "config_file".to_string(),
            detail: "wrangler.toml found".to_string(),
            confidence: DetectionConfidence::High,
        });
    }

    // Check CF_PAGES env var (set during Cloudflare Pages builds)
    if std::env::var("CF_PAGES").is_ok() {
        signals.push(DetectedSignal {
            source: "env_var".to_string(),
            detail: "CF_PAGES environment variable set".to_string(),
            confidence: DetectionConfidence::High,
        });
    }

    // Check CF_PAGES_BRANCH (also set during builds)
    if let Ok(branch) = std::env::var("CF_PAGES_BRANCH") {
        signals.push(DetectedSignal {
            source: "env_var".to_string(),
            detail: format!("CF_PAGES_BRANCH={branch}"),
            confidence: DetectionConfidence::High,
        });
    }

    signals
}

/// Detect Netlify readiness signals.
///
/// Checks for:
/// - `netlify.toml` configuration file
/// - `NETLIFY` environment variable
/// - Netlify-specific build environment variables
pub fn detect_netlify(cwd: &Path) -> Vec<DetectedSignal> {
    let mut signals = Vec::new();

    // Check for netlify.toml
    if find_ancestor_path(cwd, "netlify.toml").is_some() {
        signals.push(DetectedSignal {
            source: "config_file".to_string(),
            detail: "netlify.toml found".to_string(),
            confidence: DetectionConfidence::High,
        });
    }

    // Check NETLIFY env var
    if std::env::var("NETLIFY").is_ok() {
        signals.push(DetectedSignal {
            source: "env_var".to_string(),
            detail: "NETLIFY environment variable set".to_string(),
            confidence: DetectionConfidence::High,
        });
    }

    // Check NETLIFY_SITE_ID
    if let Ok(site_id) = std::env::var("NETLIFY_SITE_ID") {
        signals.push(DetectedSignal {
            source: "env_var".to_string(),
            detail: format!("NETLIFY_SITE_ID={}", obscure_id(&site_id)),
            confidence: DetectionConfidence::High,
        });
    }

    signals
}

/// Detect S3/AWS readiness signals.
///
/// Checks for:
/// - AWS credentials environment variables
/// - AWS profile configuration
/// - S3-related deploy scripts
pub fn detect_s3(cwd: &Path) -> Vec<DetectedSignal> {
    let mut signals = Vec::new();

    // Check AWS credentials
    if std::env::var("AWS_ACCESS_KEY_ID").is_ok() {
        signals.push(DetectedSignal {
            source: "env_var".to_string(),
            detail: "AWS_ACCESS_KEY_ID set".to_string(),
            confidence: DetectionConfidence::Medium,
        });
    }

    if std::env::var("AWS_PROFILE").is_ok() {
        signals.push(DetectedSignal {
            source: "env_var".to_string(),
            detail: "AWS_PROFILE set".to_string(),
            confidence: DetectionConfidence::Medium,
        });
    }

    // Check for S3-related scripts
    let scripts_dir = cwd.join("scripts");
    if scripts_dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&scripts_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_lowercase();
                if name.contains("s3") || name.contains("aws") || name.contains("deploy") {
                    if let Ok(content) = std::fs::read_to_string(entry.path()) {
                        if content.contains("s3 ") || content.contains("aws s3") {
                            signals.push(DetectedSignal {
                                source: "config_file".to_string(),
                                detail: format!("S3/AWS deploy script: {name}"),
                                confidence: DetectionConfidence::Medium,
                            });
                        }
                    }
                }
            }
        }
    }

    signals
}

/// Extract GitHub owner/repo from a remote URL.
///
/// Handles HTTPS, SSH, and git:// URLs.
pub fn extract_github_repo(url: &str) -> Option<String> {
    // HTTPS: https://github.com/owner/repo.git
    if let Some(rest) = url.strip_prefix("https://github.com/") {
        let repo = rest.strip_suffix(".git").unwrap_or(rest);
        return Some(repo.to_string());
    }

    // SSH: git@github.com:owner/repo.git
    if let Some(rest) = url.strip_prefix("git@github.com:") {
        let repo = rest.strip_suffix(".git").unwrap_or(rest);
        return Some(repo.to_string());
    }

    // git:// protocol
    if let Some(rest) = url.strip_prefix("git://github.com/") {
        let repo = rest.strip_suffix(".git").unwrap_or(rest);
        return Some(repo.to_string());
    }

    None
}

// ── Internal Helpers ────────────────────────────────────────────────────

fn detect_git_context(cwd: &Path, env: &mut DetectedEnvironment) {
    // Check if in git repo
    let git_dir = find_ancestor_path(cwd, ".git");
    env.is_git_repo = git_dir.is_some();

    if !env.is_git_repo {
        env.signals.push(DetectedSignal {
            source: "git".to_string(),
            detail: "Not inside a Git repository".to_string(),
            confidence: DetectionConfidence::High,
        });
        return;
    }

    // Get remote URL
    if let Some(url) = git_remote_url(cwd) {
        env.git_remote_url = Some(url.clone());

        // Extract GitHub repo if applicable
        if let Some(repo) = extract_github_repo(&url) {
            env.github_repo = Some(repo);
        }

        env.signals.push(DetectedSignal {
            source: "git_remote".to_string(),
            detail: format!("Remote origin: {url}"),
            confidence: DetectionConfidence::High,
        });
    } else {
        env.signals.push(DetectedSignal {
            source: "git_remote".to_string(),
            detail: "No remote 'origin' configured".to_string(),
            confidence: DetectionConfidence::Medium,
        });
    }
}

fn detect_env_vars(env: &mut DetectedEnvironment) {
    // GitHub
    if std::env::var("GITHUB_REPOSITORY").is_ok() || std::env::var("GITHUB_ACTIONS").is_ok() {
        env.github_env = true;
    }

    // Cloudflare
    if std::env::var("CF_PAGES").is_ok() || std::env::var("CF_PAGES_BRANCH").is_ok() {
        env.cloudflare_env = true;
    }

    // Netlify
    if std::env::var("NETLIFY").is_ok() || std::env::var("NETLIFY_SITE_ID").is_ok() {
        env.netlify_env = true;
    }

    // AWS
    if std::env::var("AWS_ACCESS_KEY_ID").is_ok()
        || std::env::var("AWS_PROFILE").is_ok()
        || std::env::var("AWS_DEFAULT_REGION").is_ok()
    {
        env.aws_env = true;
    }
}

fn detect_filesystem_signals(cwd: &Path, env: &mut DetectedEnvironment) {
    // Collect provider-specific signals
    let github_signals = detect_github_pages(cwd);
    let cloudflare_signals = detect_cloudflare_pages(cwd);
    let netlify_signals = detect_netlify(cwd);
    let s3_signals = detect_s3(cwd);

    // Add all signals to environment (avoiding duplicates from env var detection)
    for signal in github_signals {
        if !env.signals.iter().any(|s| s.detail == signal.detail) {
            env.signals.push(signal);
        }
    }
    for signal in cloudflare_signals {
        if !env.signals.iter().any(|s| s.detail == signal.detail) {
            env.signals.push(signal);
        }
    }
    for signal in netlify_signals {
        if !env.signals.iter().any(|s| s.detail == signal.detail) {
            env.signals.push(signal);
        }
    }
    for signal in s3_signals {
        if !env.signals.iter().any(|s| s.detail == signal.detail) {
            env.signals.push(signal);
        }
    }
}

fn determine_recommended_provider(env: &DetectedEnvironment) -> Option<HostingProvider> {
    // Priority: explicit env signals > config files > git remote

    // Check Cloudflare (highest priority if explicitly in CF environment)
    if env.cloudflare_env {
        return Some(HostingProvider::CloudflarePages);
    }

    // Check Netlify
    if env.netlify_env {
        return Some(HostingProvider::Netlify);
    }

    // Check GitHub (common case)
    if env.github_env || env.github_repo.is_some() {
        return Some(HostingProvider::GithubPages);
    }

    // Check for config file signals
    let has_wrangler = env
        .signals
        .iter()
        .any(|s| s.detail.contains("wrangler.toml"));
    if has_wrangler {
        return Some(HostingProvider::CloudflarePages);
    }

    let has_netlify_toml = env
        .signals
        .iter()
        .any(|s| s.detail.contains("netlify.toml"));
    if has_netlify_toml {
        return Some(HostingProvider::Netlify);
    }

    let has_github_workflow = env.signals.iter().any(|s| {
        s.detail.contains("GitHub Actions workflow")
            || s.detail.contains(".github/workflows/ directory exists")
    });
    if has_github_workflow {
        return Some(HostingProvider::GithubPages);
    }

    // S3 only if explicit signals
    if env.aws_env {
        let has_s3_script = env.signals.iter().any(|s| s.detail.contains("S3"));
        if has_s3_script {
            return Some(HostingProvider::S3);
        }
    }

    // Default to GitHub Pages if git remote is GitHub
    if let Some(ref url) = env.git_remote_url {
        if url.contains("github") {
            return Some(HostingProvider::GithubPages);
        }
    }

    // No clear recommendation
    None
}

fn confidence_order(c: DetectionConfidence) -> u8 {
    match c {
        DetectionConfidence::High => 0,
        DetectionConfidence::Medium => 1,
        DetectionConfidence::Low => 2,
    }
}

/// Get git remote URL for the given directory.
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

/// Find an ancestor path containing the given name.
fn find_ancestor_path(start: &Path, name: &str) -> Option<PathBuf> {
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

/// Obscure sensitive ID for logging.
fn obscure_id(id: &str) -> String {
    if id.len() <= 8 {
        return "***".to_string();
    }
    let visible = &id[..4];
    format!("{visible}***")
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_github_repo_https() {
        assert_eq!(
            extract_github_repo("https://github.com/owner/repo.git"),
            Some("owner/repo".to_string())
        );
        assert_eq!(
            extract_github_repo("https://github.com/owner/repo"),
            Some("owner/repo".to_string())
        );
    }

    #[test]
    fn extract_github_repo_ssh() {
        assert_eq!(
            extract_github_repo("git@github.com:owner/repo.git"),
            Some("owner/repo".to_string())
        );
        assert_eq!(
            extract_github_repo("git@github.com:owner/repo"),
            Some("owner/repo".to_string())
        );
    }

    #[test]
    fn extract_github_repo_non_github() {
        assert_eq!(
            extract_github_repo("https://gitlab.com/owner/repo.git"),
            None
        );
        assert_eq!(
            extract_github_repo("git@bitbucket.org:owner/repo.git"),
            None
        );
    }

    #[test]
    fn obscure_id_short() {
        assert_eq!(obscure_id("abc"), "***");
        assert_eq!(obscure_id("12345678"), "***");
    }

    #[test]
    fn obscure_id_long() {
        assert_eq!(obscure_id("abcdefghij"), "abcd***");
        assert_eq!(obscure_id("site-id-12345"), "site***");
    }

    #[test]
    fn detect_environment_minimal() {
        let dir = tempfile::tempdir().unwrap();
        let env = detect_environment(None, dir.path());
        assert_eq!(env.cwd, dir.path());
        assert!(!env.is_git_repo);
        // Should have "Not inside a Git repository" signal
        assert!(env.signals.iter().any(|s| s.detail.contains("Not inside")));
    }

    #[test]
    fn confidence_order_is_correct() {
        assert!(
            confidence_order(DetectionConfidence::High)
                < confidence_order(DetectionConfidence::Medium)
        );
        assert!(
            confidence_order(DetectionConfidence::Medium)
                < confidence_order(DetectionConfidence::Low)
        );
    }

    #[test]
    fn determine_provider_defaults_to_none() {
        let env = DetectedEnvironment::default();
        let provider = determine_recommended_provider(&env);
        assert!(provider.is_none());
    }

    #[test]
    fn determine_provider_github_from_repo() {
        let env = DetectedEnvironment {
            github_repo: Some("owner/repo".to_string()),
            ..Default::default()
        };
        let provider = determine_recommended_provider(&env);
        assert_eq!(provider, Some(HostingProvider::GithubPages));
    }

    #[test]
    fn determine_provider_cloudflare_from_env() {
        let env = DetectedEnvironment {
            cloudflare_env: true,
            ..Default::default()
        };
        let provider = determine_recommended_provider(&env);
        assert_eq!(provider, Some(HostingProvider::CloudflarePages));
    }

    #[test]
    fn determine_provider_netlify_from_env() {
        let env = DetectedEnvironment {
            netlify_env: true,
            ..Default::default()
        };
        let provider = determine_recommended_provider(&env);
        assert_eq!(provider, Some(HostingProvider::Netlify));
    }

    #[test]
    fn detect_environment_recommends_cloudflare_from_wrangler_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("wrangler.toml"), "name = \"demo\"")
            .expect("write wrangler.toml");

        let env = detect_environment(None, dir.path());
        assert_eq!(
            env.recommended_provider,
            Some(HostingProvider::CloudflarePages)
        );
        assert!(
            env.signals
                .iter()
                .any(|s| s.detail.contains("wrangler.toml")),
            "expected wrangler signal in detection output"
        );
    }

    #[test]
    fn detect_environment_recommends_netlify_from_netlify_toml() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("netlify.toml"),
            "[build]\npublish = \"dist\"",
        )
        .expect("write netlify.toml");

        let env = detect_environment(None, dir.path());
        assert_eq!(env.recommended_provider, Some(HostingProvider::Netlify));
        assert!(
            env.signals
                .iter()
                .any(|s| s.detail.contains("netlify.toml")),
            "expected netlify signal in detection output"
        );
    }

    #[test]
    fn detect_environment_recommends_github_from_workflow_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workflows = dir.path().join(".github").join("workflows");
        std::fs::create_dir_all(&workflows).expect("create workflows dir");
        std::fs::write(
            workflows.join("deploy-pages.yml"),
            "name: Deploy\non: [push]\njobs: {}\n",
        )
        .expect("write workflow");

        let env = detect_environment(None, dir.path());
        assert_eq!(env.recommended_provider, Some(HostingProvider::GithubPages));
        assert!(
            env.signals
                .iter()
                .any(|s| s.detail.contains("GitHub Actions workflow found")),
            "expected github workflow signal in detection output"
        );
    }
}
