use std::path::{Path, PathBuf};

/// Try to extract the git remote URL for the directory.
///
/// br-8ujfs.4.1 (D1): routes through GitCmd for per-repo locking.
pub fn git_remote_url(dir: &Path) -> Option<String> {
    let output = mcp_agent_mail_core::git_cmd::GitCmd::new(dir)
        .args(["remote", "get-url", "origin"])
        .run()
        .ok()?;
    if output.status.success() {
        let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !url.is_empty() {
            return Some(url);
        }
    }
    None
}

/// Walk ancestor directories looking for a specific file/dir.
pub fn find_ancestor_path(start: &Path, name: &str) -> Option<PathBuf> {
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
