//! Project identity resolution helpers.
//!
//! Mirrors legacy Python `_compute_project_slug` and `_resolve_project_identity`.

use crate::config::Config;
use crate::config::ProjectIdentityMode;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_uid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub product_uid: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectIdentity {
    pub slug: String,
    pub identity_mode_used: String,
    pub canonical_path: String,
    pub human_key: String,
    pub repo_root: Option<String>,
    pub git_common_dir: Option<String>,
    pub branch: Option<String>,
    pub worktree_name: Option<String>,
    pub core_ignorecase: Option<bool>,
    pub normalized_remote: Option<String>,
    pub project_uid: String,
    pub discovery: Option<DiscoveryInfo>,
}

fn sha1_hex(text: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(text.as_bytes());
    let digest = hasher.finalize();
    format!("{digest:x}")
}

fn short_sha1(text: &str, n: usize) -> String {
    let hex = sha1_hex(text);
    hex.chars().take(n).collect()
}

/// Normalize a human-readable value into a slug.
#[must_use]
pub fn slugify(value: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in value.trim().to_ascii_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "project".to_string()
    } else {
        trimmed.to_string()
    }
}

fn resolve_path(human_key: &str) -> PathBuf {
    let expanded = shellexpand::tilde(human_key).into_owned();
    let path = PathBuf::from(expanded);
    std::fs::canonicalize(&path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(path)
        }
    })
}

fn git_cmd(repo: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

fn parse_remote_url(url: &str) -> Option<(String, String)> {
    let u = url.trim();
    if u.is_empty() {
        return None;
    }

    if let Some(pos) = u.find("://") {
        let after = &u[pos + 3..];
        let mut parts = after.splitn(2, '/');
        let host_part = parts.next().unwrap_or("");
        let path_part = parts.next().unwrap_or("");
        let host_part = host_part.rsplit('@').next().unwrap_or(host_part);
        let host = host_part
            .split(':')
            .next()
            .unwrap_or(host_part)
            .to_lowercase();
        if host.is_empty() {
            return None;
        }
        return Some((host, path_part.to_string()));
    }

    if u.contains('@') && u.contains(':') {
        let after_at = u.split('@').nth(1)?;
        let mut parts = after_at.splitn(2, ':');
        let host = parts.next()?.to_lowercase();
        let path = parts.next().unwrap_or("").to_string();
        return Some((host, path));
    }

    if u.contains(':') {
        let mut parts = u.splitn(2, ':');
        let host = parts.next()?.to_lowercase();
        let path = parts.next().unwrap_or("").to_string();
        return Some((host, path));
    }

    None
}

fn normalize_remote_first_two(url: &str) -> Option<String> {
    let (host, mut path) = parse_remote_url(url)?;
    if path.starts_with('/') {
        path = path.trim_start_matches('/').to_string();
    }
    if Path::new(&path)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("git"))
    {
        path.truncate(path.len().saturating_sub(4));
    }
    while path.contains("//") {
        path = path.replace("//", "/");
    }
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() < 2 {
        return None;
    }
    let owner = parts[0];
    let repo = parts[1];
    Some(format!("{host}/{owner}/{repo}"))
}

fn normalize_remote_last_two(url: &str) -> Option<String> {
    let (host, mut path) = parse_remote_url(url)?;
    if path.starts_with('/') {
        path = path.trim_start_matches('/').to_string();
    }
    if Path::new(&path)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("git"))
    {
        path.truncate(path.len().saturating_sub(4));
    }
    while path.contains("//") {
        path = path.replace("//", "/");
    }
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() < 2 {
        return None;
    }
    let owner = parts[parts.len() - 2];
    let repo = parts[parts.len() - 1];
    Some(format!("{host}/{owner}/{repo}"))
}

fn read_discovery_yaml(base_dir: &Path) -> DiscoveryInfo {
    let path = base_dir.join(".agent-mail.yaml");
    let mut info = DiscoveryInfo {
        project_uid: None,
        product_uid: None,
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return info;
    };

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || !trimmed.contains(':') {
            continue;
        }
        let mut parts = trimmed.splitn(2, ':');
        let key = parts.next().unwrap_or("").trim();
        let mut value = parts.next().unwrap_or("").trim().to_string();
        if let Some(comment_idx) = value.find('#') {
            value.truncate(comment_idx);
        }
        value = value
            .trim()
            .trim_matches('\'')
            .trim_matches('"')
            .to_string();
        if value.is_empty() {
            continue;
        }
        match key {
            "project_uid" => info.project_uid = Some(value),
            "product_uid" => info.product_uid = Some(value),
            _ => {}
        }
    }

    info
}

const fn mode_to_str(mode: ProjectIdentityMode) -> &'static str {
    match mode {
        ProjectIdentityMode::Dir => "dir",
        ProjectIdentityMode::GitRemote => "git-remote",
        ProjectIdentityMode::GitCommonDir => "git-common-dir",
        ProjectIdentityMode::GitToplevel => "git-toplevel",
    }
}

/// Compute the project slug based on config and path.
#[must_use]
pub fn compute_project_slug(human_key: &str) -> String {
    let config = &Config::get();
    if !config.worktrees_enabled {
        return slugify(human_key);
    }

    let mode = config.project_identity_mode;
    if mode == ProjectIdentityMode::Dir {
        return slugify(human_key);
    }
    let target_path = resolve_path(human_key);

    let repo_root = git_cmd(&target_path, &["rev-parse", "--show-toplevel"]);
    let remote_name = config.project_identity_remote.as_str();
    let remote_url = repo_root
        .as_ref()
        .and_then(|root| git_cmd(Path::new(root), &["remote", "get-url", remote_name]))
        .or_else(|| {
            repo_root.as_ref().and_then(|root| {
                git_cmd(
                    Path::new(root),
                    &["config", "--get", &format!("remote.{remote_name}.url")],
                )
            })
        });

    match mode {
        ProjectIdentityMode::GitRemote => {
            if let Some(url) = remote_url
                && let Some(normalized) = normalize_remote_first_two(&url)
            {
                let base = normalized.rsplit('/').next().unwrap_or("repo").to_string();
                let canonical = normalized;
                return format!("{base}-{}", short_sha1(&canonical, 10));
            }
            slugify(human_key)
        }
        ProjectIdentityMode::GitToplevel => {
            if let Some(root) = repo_root {
                let base = Path::new(&root)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("repo")
                    .to_string();
                return format!("{base}-{}", short_sha1(&root, 10));
            }
            slugify(human_key)
        }
        ProjectIdentityMode::GitCommonDir => {
            let common_dir = repo_root
                .as_ref()
                .and_then(|root| git_cmd(Path::new(root), &["rev-parse", "--git-common-dir"]));
            if let Some(common_dir) = common_dir {
                let resolved = if Path::new(&common_dir).is_absolute() {
                    common_dir
                } else if let Some(root) = repo_root.as_ref() {
                    Path::new(root)
                        .join(common_dir)
                        .to_string_lossy()
                        .to_string()
                } else {
                    common_dir
                };
                return format!("repo-{}", short_sha1(&resolved, 10));
            }
            slugify(human_key)
        }
        ProjectIdentityMode::Dir => slugify(human_key),
    }
}

/// Resolve identity details for a given `human_key` path.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn resolve_project_identity(human_key: &str) -> ProjectIdentity {
    let config = &Config::get();
    let mode_config = config.project_identity_mode;
    let mode_used = if config.worktrees_enabled {
        mode_to_str(mode_config).to_string()
    } else {
        "dir".to_string()
    };

    let target_path = resolve_path(human_key);
    let target_str = target_path.to_string_lossy().to_string();

    if !config.worktrees_enabled {
        let slug_value = slugify(human_key);
        let project_uid = short_sha1(&target_str, 20);
        let resolved_human_key = target_str.clone();
        return ProjectIdentity {
            slug: slug_value,
            identity_mode_used: "dir".to_string(),
            canonical_path: target_str,
            human_key: resolved_human_key,
            repo_root: None,
            git_common_dir: None,
            branch: None,
            worktree_name: None,
            core_ignorecase: None,
            normalized_remote: None,
            project_uid,
            discovery: None,
        };
    }

    let repo_root = git_cmd(&target_path, &["rev-parse", "--show-toplevel"]);
    let git_common_dir = repo_root
        .as_ref()
        .and_then(|root| git_cmd(Path::new(root), &["rev-parse", "--git-common-dir"]))
        .map(|g| {
            if Path::new(&g).is_absolute() {
                g
            } else if let Some(root) = repo_root.as_ref() {
                Path::new(root).join(g).to_string_lossy().to_string()
            } else {
                g
            }
        });

    let branch = repo_root
        .as_ref()
        .and_then(|root| git_cmd(Path::new(root), &["rev-parse", "--abbrev-ref", "HEAD"]))
        .and_then(|b| if b == "HEAD" { None } else { Some(b) });

    let worktree_name = repo_root.as_ref().and_then(|root| {
        Path::new(root)
            .file_name()
            .and_then(|s| s.to_str())
            .map(ToString::to_string)
    });

    let core_ignorecase = repo_root
        .as_ref()
        .and_then(|root| git_cmd(Path::new(root), &["config", "--get", "core.ignorecase"]))
        .map(|v| v.trim().eq_ignore_ascii_case("true"));

    let remote_name = config.project_identity_remote.as_str();
    let remote_url = repo_root
        .as_ref()
        .and_then(|root| git_cmd(Path::new(root), &["remote", "get-url", remote_name]))
        .or_else(|| {
            repo_root.as_ref().and_then(|root| {
                git_cmd(
                    Path::new(root),
                    &["config", "--get", &format!("remote.{remote_name}.url")],
                )
            })
        });
    let normalized_remote = remote_url
        .as_ref()
        .and_then(|url| normalize_remote_last_two(url));

    let default_branch = repo_root.as_ref().and_then(|root| {
        git_cmd(
            Path::new(root),
            &["symbolic-ref", &format!("refs/remotes/{remote_name}/HEAD")],
        )
    });
    let default_branch = default_branch
        .and_then(|s| s.rsplit('/').next().map(ToString::to_string))
        .unwrap_or_else(|| "main".to_string());

    let canonical_path = match mode_config {
        ProjectIdentityMode::GitRemote => normalized_remote
            .clone()
            .unwrap_or_else(|| target_str.clone()),
        ProjectIdentityMode::GitToplevel => repo_root.clone().unwrap_or_else(|| target_str.clone()),
        ProjectIdentityMode::GitCommonDir => {
            git_common_dir.clone().unwrap_or_else(|| target_str.clone())
        }
        ProjectIdentityMode::Dir => target_str.clone(),
    };

    let repo_root_path = repo_root.as_ref().map(PathBuf::from);
    let discovery = repo_root_path
        .as_deref()
        .map(read_discovery_yaml)
        .or_else(|| {
            if target_path.exists() {
                Some(read_discovery_yaml(&target_path))
            } else {
                None
            }
        })
        .and_then(|info| {
            if info.project_uid.is_some() || info.product_uid.is_some() {
                Some(info)
            } else {
                None
            }
        });

    let mut project_uid: Option<String> = None;
    let marker_committed = repo_root_path
        .as_ref()
        .map(|root| root.join(".agent-mail-project-id"));
    if let Some(marker) = marker_committed.as_ref()
        && let Ok(text) = std::fs::read_to_string(marker)
    {
        let trimmed = text.trim().to_string();
        if !trimmed.is_empty() {
            project_uid = Some(trimmed);
        }
    }

    if project_uid.is_none()
        && let Some(info) = discovery.as_ref()
        && let Some(uid) = info.project_uid.as_ref()
        && !uid.trim().is_empty()
    {
        project_uid = Some(uid.trim().to_string());
    }

    let marker_private = git_common_dir
        .as_ref()
        .map(|g| PathBuf::from(g).join("agent-mail").join("project-id"));
    if project_uid.is_none()
        && let Some(marker) = marker_private.as_ref()
        && let Ok(text) = std::fs::read_to_string(marker)
    {
        let trimmed = text.trim().to_string();
        if !trimmed.is_empty() {
            project_uid = Some(trimmed);
        }
    }

    if project_uid.is_none()
        && let Some(remote) = normalized_remote.as_ref()
    {
        let fingerprint = format!("{remote}@{default_branch}");
        project_uid = Some(short_sha1(&fingerprint, 20));
    }

    if project_uid.is_none()
        && let Some(common) = git_common_dir.as_ref()
    {
        project_uid = Some(short_sha1(common, 20));
    }

    if project_uid.is_none() {
        project_uid = Some(short_sha1(&target_str, 20));
    }

    if let (true, Some(marker)) = (config.worktrees_enabled, marker_private.as_ref())
        && !marker.exists()
    {
        if let Some(parent) = marker.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(
            marker,
            format!("{}\n", project_uid.as_deref().unwrap_or("")),
        );
    }

    let slug_value = compute_project_slug(&target_str);

    ProjectIdentity {
        slug: slug_value,
        identity_mode_used: mode_used,
        canonical_path,
        human_key: target_str.clone(),
        repo_root,
        git_common_dir,
        branch,
        worktree_name,
        core_ignorecase,
        normalized_remote,
        project_uid: project_uid.unwrap_or_else(|| short_sha1(&target_str, 20)),
        discovery,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // slugify
    // -----------------------------------------------------------------------

    #[test]
    fn slugify_simple_path() {
        assert_eq!(slugify("/data/projects/backend"), "data-projects-backend");
    }

    #[test]
    fn slugify_collapses_runs_of_non_alnum() {
        assert_eq!(
            slugify("/data///projects///backend"),
            "data-projects-backend"
        );
    }

    #[test]
    fn slugify_trims_dashes() {
        assert_eq!(slugify("---hello---"), "hello");
    }

    #[test]
    fn slugify_lowercases() {
        assert_eq!(slugify("MyProject"), "myproject");
    }

    #[test]
    fn slugify_empty_returns_project() {
        assert_eq!(slugify(""), "project");
    }

    #[test]
    fn slugify_all_special_chars_returns_project() {
        assert_eq!(slugify("///---///"), "project");
    }

    #[test]
    fn slugify_spaces_become_dashes() {
        assert_eq!(slugify("  hello  world  "), "hello-world");
    }

    #[test]
    fn slugify_mixed_separators() {
        assert_eq!(slugify("my_project.v2@latest"), "my-project-v2-latest");
    }

    #[test]
    fn slugify_unicode_stripped() {
        assert_eq!(slugify("/data/café/naïve"), "data-caf-na-ve");
    }

    #[test]
    fn slugify_preserves_digits() {
        assert_eq!(slugify("v1.2.3-rc4"), "v1-2-3-rc4");
    }

    #[test]
    fn slugify_windows_style_separators() {
        assert_eq!(slugify(r"C:\Users\Alice\Repo"), "c-users-alice-repo");
    }

    #[test]
    fn slugify_relative_components() {
        assert_eq!(slugify("../repo/./module"), "repo-module");
    }

    #[test]
    fn slugify_trailing_slashes() {
        assert_eq!(
            slugify("/data/projects/backend///"),
            "data-projects-backend"
        );
    }

    #[test]
    fn slugify_very_long_path_is_stable_and_non_empty() {
        let long = format!("/{}", "a".repeat(5000));
        let slug = slugify(&long);
        assert_eq!(slug, "a".repeat(5000));
        assert!(!slug.is_empty());
    }

    // -----------------------------------------------------------------------
    // sha1_hex / short_sha1
    // -----------------------------------------------------------------------

    #[test]
    fn sha1_hex_known_value() {
        // SHA-1 of "hello" is well-known
        assert_eq!(
            sha1_hex("hello"),
            "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d"
        );
    }

    #[test]
    fn sha1_hex_empty() {
        assert_eq!(sha1_hex(""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    }

    #[test]
    fn short_sha1_truncates() {
        let full = sha1_hex("hello");
        let short = short_sha1("hello", 10);
        assert_eq!(short.len(), 10);
        assert_eq!(short, &full[..10]);
    }

    #[test]
    fn short_sha1_zero_length() {
        let short = short_sha1("hello", 0);
        assert!(short.is_empty());
    }

    #[test]
    fn short_sha1_distinguishes_different_inputs() {
        let a = short_sha1("/tmp/project-a", 20);
        let b = short_sha1("/tmp/project-b", 20);
        assert_ne!(a, b);
    }

    // -----------------------------------------------------------------------
    // parse_remote_url
    // -----------------------------------------------------------------------

    #[test]
    fn parse_remote_https() {
        let (host, path) = parse_remote_url("https://github.com/user/repo.git").unwrap();
        assert_eq!(host, "github.com");
        assert_eq!(path, "user/repo.git");
    }

    #[test]
    fn parse_remote_ssh_at_colon() {
        let (host, path) = parse_remote_url("git@github.com:user/repo.git").unwrap();
        assert_eq!(host, "github.com");
        assert_eq!(path, "user/repo.git");
    }

    #[test]
    fn parse_remote_ssh_protocol() {
        let (host, path) = parse_remote_url("ssh://git@github.com/user/repo.git").unwrap();
        assert_eq!(host, "github.com");
        assert_eq!(path, "user/repo.git");
    }

    #[test]
    fn parse_remote_with_port() {
        let (host, path) = parse_remote_url("https://github.com:443/user/repo.git").unwrap();
        assert_eq!(host, "github.com");
        assert_eq!(path, "user/repo.git");
    }

    #[test]
    fn parse_remote_empty_returns_none() {
        assert!(parse_remote_url("").is_none());
    }

    #[test]
    fn parse_remote_whitespace_only_returns_none() {
        assert!(parse_remote_url("   ").is_none());
    }

    #[test]
    fn parse_remote_scp_without_user() {
        let (host, path) = parse_remote_url("github.com:user/repo.git").unwrap();
        assert_eq!(host, "github.com");
        assert_eq!(path, "user/repo.git");
    }

    #[test]
    fn parse_remote_invalid_without_separator_returns_none() {
        assert!(parse_remote_url("just-a-random-string").is_none());
    }

    // -----------------------------------------------------------------------
    // normalize_remote_first_two
    // -----------------------------------------------------------------------

    #[test]
    fn normalize_remote_first_two_https() {
        let result = normalize_remote_first_two("https://github.com/user/repo.git").unwrap();
        assert_eq!(result, "github.com/user/repo");
    }

    #[test]
    fn normalize_remote_first_two_ssh() {
        let result = normalize_remote_first_two("git@github.com:user/repo.git").unwrap();
        assert_eq!(result, "github.com/user/repo");
    }

    #[test]
    fn normalize_remote_first_two_strips_git_suffix() {
        let with_git = normalize_remote_first_two("https://github.com/user/repo.git").unwrap();
        let without_git = normalize_remote_first_two("https://github.com/user/repo").unwrap();
        assert_eq!(with_git, without_git);
    }

    #[test]
    fn normalize_remote_first_two_too_few_segments() {
        // Only one path segment -> None
        assert!(normalize_remote_first_two("https://github.com/just-one").is_none());
    }

    #[test]
    fn normalize_remote_first_two_deep_path() {
        // Takes only first two segments
        let result =
            normalize_remote_first_two("https://gitlab.com/org/sub/deep/repo.git").unwrap();
        assert_eq!(result, "gitlab.com/org/sub");
    }

    #[test]
    fn normalize_remote_first_two_collapses_duplicate_slashes() {
        let result = normalize_remote_first_two("https://github.com//org///repo.git").unwrap();
        assert_eq!(result, "github.com/org/repo");
    }

    // -----------------------------------------------------------------------
    // normalize_remote_last_two
    // -----------------------------------------------------------------------

    #[test]
    fn normalize_remote_last_two_https() {
        let result = normalize_remote_last_two("https://github.com/user/repo.git").unwrap();
        assert_eq!(result, "github.com/user/repo");
    }

    #[test]
    fn normalize_remote_last_two_deep_path() {
        // Takes last two segments
        let result = normalize_remote_last_two("https://gitlab.com/org/sub/deep/repo.git").unwrap();
        assert_eq!(result, "gitlab.com/deep/repo");
    }

    #[test]
    fn normalize_remote_last_two_collapses_duplicate_slashes() {
        let result =
            normalize_remote_last_two("https://gitlab.com///team//service///api.git").unwrap();
        assert_eq!(result, "gitlab.com/service/api");
    }

    // -----------------------------------------------------------------------
    // read_discovery_yaml
    // -----------------------------------------------------------------------

    #[test]
    fn read_discovery_yaml_parses_fields() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join(".agent-mail.yaml"),
            "project_uid: my-proj-uid\nproduct_uid: my-prod-uid\n",
        )
        .expect("write");
        let info = read_discovery_yaml(tmp.path());
        assert_eq!(info.project_uid.as_deref(), Some("my-proj-uid"));
        assert_eq!(info.product_uid.as_deref(), Some("my-prod-uid"));
    }

    #[test]
    fn read_discovery_yaml_handles_comments() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join(".agent-mail.yaml"),
            "# comment\nproject_uid: uid123 # inline\n",
        )
        .expect("write");
        let info = read_discovery_yaml(tmp.path());
        assert_eq!(info.project_uid.as_deref(), Some("uid123"));
    }

    #[test]
    fn read_discovery_yaml_strips_quotes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join(".agent-mail.yaml"),
            "project_uid: 'quoted'\nproduct_uid: \"double\"\n",
        )
        .expect("write");
        let info = read_discovery_yaml(tmp.path());
        assert_eq!(info.project_uid.as_deref(), Some("quoted"));
        assert_eq!(info.product_uid.as_deref(), Some("double"));
    }

    #[test]
    fn read_discovery_yaml_missing_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let info = read_discovery_yaml(tmp.path());
        assert!(info.project_uid.is_none());
        assert!(info.product_uid.is_none());
    }

    // -----------------------------------------------------------------------
    // mode_to_str
    // -----------------------------------------------------------------------

    #[test]
    fn mode_to_str_covers_all_variants() {
        assert_eq!(mode_to_str(ProjectIdentityMode::Dir), "dir");
        assert_eq!(mode_to_str(ProjectIdentityMode::GitRemote), "git-remote");
        assert_eq!(
            mode_to_str(ProjectIdentityMode::GitCommonDir),
            "git-common-dir"
        );
        assert_eq!(
            mode_to_str(ProjectIdentityMode::GitToplevel),
            "git-toplevel"
        );
    }

    #[test]
    fn resolve_project_identity_returns_core_fields_for_existing_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let identity = resolve_project_identity(&tmp.path().display().to_string());

        assert!(!identity.slug.is_empty());
        assert!(!identity.canonical_path.is_empty());
        assert!(!identity.human_key.is_empty());
        assert!(!identity.project_uid.is_empty());
        assert!(!identity.identity_mode_used.is_empty());
    }

    #[test]
    fn compute_project_slug_returns_non_empty_for_existing_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let slug = compute_project_slug(&tmp.path().display().to_string());
        assert!(!slug.is_empty());
    }
}
