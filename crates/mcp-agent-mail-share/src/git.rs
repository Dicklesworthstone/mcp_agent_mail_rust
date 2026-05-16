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
        if is_shared_ancestor_boundary(current) {
            break;
        }
        let candidate = current.join(name);
        if std::fs::symlink_metadata(&candidate)
            .is_ok_and(|metadata| metadata.file_type().is_file() || metadata.file_type().is_dir())
        {
            return Some(candidate);
        }
    }
    None
}

/// Shared sticky directories such as `/tmp` are not project ancestors in the
/// semantic sense: a marker there can belong to any process on the machine.
#[cfg(unix)]
pub(crate) fn is_shared_ancestor_boundary(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::symlink_metadata(path).is_ok_and(|metadata| {
        if !metadata.file_type().is_dir() {
            return false;
        }
        let mode = metadata.permissions().mode();
        mode & 0o1000 != 0 && mode & 0o002 != 0
    })
}

#[cfg(not(unix))]
pub(crate) fn is_shared_ancestor_boundary(_path: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn find_ancestor_path_does_not_trust_sticky_shared_start() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).unwrap();
        std::fs::write(shared.join("Cargo.toml"), "[workspace]").unwrap();

        assert_eq!(find_ancestor_path(&shared, "Cargo.toml"), None);
    }

    #[cfg(unix)]
    #[test]
    fn find_ancestor_path_detects_project_inside_sticky_parent() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).unwrap();
        let project = shared.join("project");
        let nested = project.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let marker = project.join("Cargo.toml");
        std::fs::write(&marker, "[workspace]").unwrap();

        assert_eq!(find_ancestor_path(&nested, "Cargo.toml"), Some(marker));
    }
}
