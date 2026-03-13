//! Static file serving for the optional `web/` SPA directory.
//!
//! Legacy Python mounts `web/` at root if it exists and contains `index.html`.
//! We replicate this behavior: resolve the directory at startup, serve files with
//! correct MIME types, and fall back to `index.html` for SPA routing.

#![forbid(unsafe_code)]

use std::path::{Component, Path, PathBuf};

/// Resolved web root directory (if found).
///
/// Call [`resolve_web_root`] at startup to find the directory.
#[derive(Debug, Clone)]
pub struct WebRoot {
    root: PathBuf,
}

impl WebRoot {
    /// Try to serve a file from the web root.
    ///
    /// Returns `Some((content_type, body))` on success, `None` if not found.
    /// For unknown paths, falls back to `index.html` (SPA mode).
    pub fn serve(&self, path: &str) -> Option<(&'static str, Vec<u8>)> {
        // Strip leading slash and normalize.
        let relative = path.trim_start_matches('/');

        // Try the exact file first.
        if !relative.is_empty()
            && let Some(relative_path) = normalized_relative_path(relative)
            && let Some(response) = self.read_relative_file(&relative_path)
        {
            return Some(response);
        }

        // Directory index: try appending /index.html.
        if relative.is_empty() || relative.ends_with('/') {
            let base = normalized_relative_path(relative).unwrap_or_default();
            let index = base.join("index.html");
            if let Some(response) = self.read_relative_file(&index) {
                return Some(response);
            }
        }

        // SPA fallback: return index.html for any path that isn't a file.
        self.read_relative_file(Path::new("index.html"))
    }

    fn read_relative_file(&self, relative_path: &Path) -> Option<(&'static str, Vec<u8>)> {
        let file = open_relative_file(&self.root, relative_path)?;
        Self::read_file(relative_path, file)
    }

    fn read_file(path: &Path, mut file: std::fs::File) -> Option<(&'static str, Vec<u8>)> {
        use std::io::Read;
        let content_type = mime_type_for_path(path);

        let max_bytes: u64 = 100 * 1024 * 1024; // 100 MB limit
        let mut body = Vec::new();
        file.by_ref()
            .take(max_bytes + 1)
            .read_to_end(&mut body)
            .ok()?;
        if body.len() > usize::try_from(max_bytes).unwrap_or(usize::MAX) {
            return None;
        }

        Some((content_type, body))
    }
}

/// Resolve the web root directory, matching legacy Python behavior.
///
/// Checks candidates in order:
/// 1. `<executable_ancestor>/web` for ancestors that look like the source tree root
/// 2. `<cwd>/web`
///
/// Returns `Some(WebRoot)` if a candidate exists with an `index.html`.
pub fn resolve_web_root() -> Option<WebRoot> {
    let mut candidates = Vec::new();

    // Candidate 1: relative to executable (mirrors Python's `Path(__file__).parents[3] / "web"`).
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        candidates.extend(executable_web_candidates(parent));
    }

    // Candidate 2: relative to CWD.
    if let Ok(cwd) = std::env::current_dir() {
        candidates.extend(current_dir_web_candidates(&cwd));
    }

    for candidate in candidates {
        if is_valid_web_root_candidate(&candidate) {
            return Some(WebRoot { root: candidate });
        }
    }

    None
}

fn executable_web_candidates(exe_parent: &Path) -> Vec<PathBuf> {
    // Keep legacy-style source-tree probing, but only when an ancestor looks like
    // the project root. This avoids serving unrelated `~/web` trees when the
    // binary is installed under locations like `~/.local/bin`.
    exe_parent
        .ancestors()
        .take(3)
        .filter(|ancestor| looks_like_source_tree_root(ancestor))
        .map(|ancestor| ancestor.join("web"))
        .collect()
}

fn current_dir_web_candidates(cwd: &Path) -> Vec<PathBuf> {
    cwd.ancestors()
        .take(10)
        .filter(|ancestor| looks_like_source_tree_root(ancestor))
        .map(|ancestor| ancestor.join("web"))
        .collect()
}

fn looks_like_source_tree_root(path: &Path) -> bool {
    is_real_file(&path.join("Cargo.toml"))
        || is_real_file(&path.join(".git"))
        || is_real_directory(&path.join(".git"))
        || is_real_directory(&path.join("crates"))
}

fn is_valid_web_root_candidate(candidate: &Path) -> bool {
    is_real_directory(candidate)
        && is_real_file(&candidate.join("index.html"))
        && is_safe_path(candidate, &candidate.join("index.html"))
}

fn is_real_directory(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
}

fn is_real_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
}

fn normalized_relative_path(relative: &str) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in Path::new(relative).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(segment) => normalized.push(segment),
            Component::Prefix(_) | Component::RootDir | Component::ParentDir => return None,
        }
    }
    Some(normalized)
}

#[cfg(unix)]
fn open_relative_file(root: &Path, relative_path: &Path) -> Option<std::fs::File> {
    use nix::fcntl::{OFlag, open, openat};
    use nix::sys::stat::Mode;

    let mut current = open(
        root,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .ok()?;

    let mut components = relative_path.components().peekable();
    while let Some(component) = components.next() {
        let segment = match component {
            Component::CurDir => continue,
            Component::Normal(segment) => segment,
            Component::Prefix(_) | Component::RootDir | Component::ParentDir => return None,
        };
        let flags = if components.peek().is_some() {
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC
        } else {
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC
        };
        let next = openat(&current, segment, flags, Mode::empty()).ok()?;
        if components.peek().is_none() {
            return Some(std::fs::File::from(next));
        }
        current = next;
    }

    None
}

#[cfg(not(unix))]
fn open_relative_file(root: &Path, relative_path: &Path) -> Option<std::fs::File> {
    let candidate = root.join(relative_path);
    if candidate.is_file() && is_safe_path(root, &candidate) {
        std::fs::File::open(candidate).ok()
    } else {
        None
    }
}

/// Determine the MIME type for a file path based on its extension.
fn mime_type_for_path(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "wasm" => "application/wasm",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "txt" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// Ensure the resolved path doesn't escape the web root (path traversal protection).
fn is_safe_path(root: &Path, resolved: &Path) -> bool {
    match (root.canonicalize(), resolved.canonicalize()) {
        (Ok(root_canon), Ok(resolved_canon)) => resolved_canon.starts_with(root_canon),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_types_cover_common_web_assets() {
        assert_eq!(
            mime_type_for_path(Path::new("app.js")),
            "application/javascript; charset=utf-8"
        );
        assert_eq!(
            mime_type_for_path(Path::new("style.css")),
            "text/css; charset=utf-8"
        );
        assert_eq!(
            mime_type_for_path(Path::new("index.html")),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            mime_type_for_path(Path::new("data.json")),
            "application/json"
        );
        assert_eq!(
            mime_type_for_path(Path::new("sql-wasm.wasm")),
            "application/wasm"
        );
        assert_eq!(mime_type_for_path(Path::new("logo.png")), "image/png");
        assert_eq!(mime_type_for_path(Path::new("font.woff2")), "font/woff2");
        assert_eq!(
            mime_type_for_path(Path::new("unknown.xyz")),
            "application/octet-stream"
        );
    }

    #[test]
    fn resolve_web_root_does_not_panic() {
        let _ = resolve_web_root();
    }

    #[test]
    fn executable_web_candidates_include_detected_workspace_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("workspace");
        std::fs::create_dir_all(root.join("crates")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]").unwrap();
        let parent = root.join("target").join("debug");
        std::fs::create_dir_all(&parent).unwrap();

        let candidates = executable_web_candidates(&parent);
        assert_eq!(candidates, vec![root.join("web")]);
    }

    #[test]
    fn executable_web_candidates_ignore_unmarked_install_ancestors() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join(".local").join("bin");
        std::fs::create_dir_all(&parent).unwrap();

        let candidates = executable_web_candidates(&parent);
        assert!(candidates.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn executable_web_candidates_ignore_symlinked_git_marker() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("workspace");
        let outside = dir.path().join("outside-git");
        let parent = root.join("target").join("debug");
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, root.join(".git")).unwrap();

        let candidates = executable_web_candidates(&parent);
        assert!(candidates.is_empty());
    }

    #[test]
    fn current_dir_web_candidates_include_detected_workspace_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("workspace");
        std::fs::create_dir_all(root.join("crates/server")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]").unwrap();

        let nested = root.join("crates/server");
        let candidates = current_dir_web_candidates(&nested);
        assert_eq!(candidates, vec![root.join("web")]);
    }

    #[test]
    fn current_dir_web_candidates_ignore_unmarked_directories() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("scratch/notes");
        std::fs::create_dir_all(&nested).unwrap();

        let candidates = current_dir_web_candidates(&nested);
        assert!(candidates.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn current_dir_web_candidates_ignore_symlinked_crates_marker() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("workspace");
        let nested = root.join("subdir");
        let outside = dir.path().join("outside-crates");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, root.join("crates")).unwrap();

        let candidates = current_dir_web_candidates(&nested);
        assert!(candidates.is_empty());
    }

    #[test]
    fn web_root_serves_files_from_temp_dir() {
        let dir = tempfile::tempdir().unwrap();
        let web = dir.path().join("web");
        std::fs::create_dir(&web).unwrap();
        std::fs::write(web.join("index.html"), "<html>hi</html>").unwrap();
        std::fs::write(web.join("app.js"), "console.log('ok')").unwrap();

        let root = WebRoot { root: web };

        // Serve index.
        let (ct, body) = root.serve("/").unwrap();
        assert_eq!(ct, "text/html; charset=utf-8");
        assert_eq!(body, b"<html>hi</html>");

        // Serve JS file.
        let (ct, body) = root.serve("/app.js").unwrap();
        assert_eq!(ct, "application/javascript; charset=utf-8");
        assert_eq!(body, b"console.log('ok')");

        // SPA fallback for unknown path.
        let (ct, _body) = root.serve("/some/spa/route").unwrap();
        assert_eq!(ct, "text/html; charset=utf-8");
    }

    #[test]
    fn web_root_blocks_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let web = dir.path().join("web");
        std::fs::create_dir(&web).unwrap();
        std::fs::write(web.join("index.html"), "ok").unwrap();

        // Write a file outside web root.
        std::fs::write(dir.path().join("secret.txt"), "classified").unwrap();

        let root = WebRoot { root: web };
        // Path traversal attempt should not serve files outside root.
        let result = root.serve("/../secret.txt");
        // Should return SPA fallback (index.html), not the secret file.
        if let Some((_ct, body)) = result {
            assert_ne!(body, b"classified");
        }
    }

    #[test]
    fn web_root_blocks_directory_index_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let web = dir.path().join("web");
        std::fs::create_dir(&web).unwrap();
        std::fs::write(web.join("index.html"), "inside").unwrap();

        // Write an index.html outside the web root to confirm we never serve it.
        std::fs::write(dir.path().join("index.html"), "outside").unwrap();

        let root = WebRoot { root: web };
        let (ct, body) = root.serve("/../").unwrap();
        assert_eq!(ct, "text/html; charset=utf-8");
        assert_eq!(body, b"inside");
    }

    #[cfg(unix)]
    #[test]
    fn web_root_blocks_spa_fallback_symlink_escape() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let web = dir.path().join("web");
        std::fs::create_dir(&web).unwrap();

        let outside = dir.path().join("outside-secret.txt");
        std::fs::write(&outside, "outside-secret").unwrap();
        symlink(&outside, web.join("index.html")).unwrap();

        let root = WebRoot { root: web };
        assert!(root.serve("/unknown/spa/route").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn web_root_blocks_exact_file_symlink_escape() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let web = dir.path().join("web");
        let vendor = web.join("vendor");
        std::fs::create_dir_all(&vendor).unwrap();
        std::fs::write(web.join("index.html"), "inside").unwrap();

        let outside = dir.path().join("outside.js");
        std::fs::write(&outside, "outside-secret").unwrap();
        symlink(&outside, vendor.join("lib.js")).unwrap();

        let root = WebRoot { root: web };
        let (ct, body) = root.serve("/vendor/lib.js").unwrap();
        assert_eq!(ct, "text/html; charset=utf-8");
        assert_eq!(body, b"inside");
    }

    #[cfg(unix)]
    #[test]
    fn web_root_blocks_symlinked_directory_component_escape() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let web = dir.path().join("web");
        std::fs::create_dir(&web).unwrap();
        std::fs::write(web.join("index.html"), "inside").unwrap();

        let outside_dir = dir.path().join("outside-assets");
        std::fs::create_dir_all(&outside_dir).unwrap();
        std::fs::write(outside_dir.join("app.js"), "outside-secret").unwrap();
        symlink(&outside_dir, web.join("vendor")).unwrap();

        let root = WebRoot { root: web };
        let (ct, body) = root.serve("/vendor/app.js").unwrap();
        assert_eq!(ct, "text/html; charset=utf-8");
        assert_eq!(body, b"inside");
    }

    #[test]
    fn web_root_serves_vendor_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        let web = dir.path().join("web");
        let vendor = web.join("vendor");
        std::fs::create_dir_all(&vendor).unwrap();
        std::fs::write(web.join("index.html"), "root").unwrap();
        std::fs::write(vendor.join("lib.js"), "vendor code").unwrap();

        let root = WebRoot { root: web };
        let (ct, body) = root.serve("/vendor/lib.js").unwrap();
        assert_eq!(ct, "application/javascript; charset=utf-8");
        assert_eq!(body, b"vendor code");
    }

    // ── Additional MIME type edge cases ──────────────────────────────────

    #[test]
    fn mime_type_remaining_extensions() {
        assert_eq!(
            mime_type_for_path(Path::new("page.htm")),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            mime_type_for_path(Path::new("module.mjs")),
            "application/javascript; charset=utf-8"
        );
        assert_eq!(
            mime_type_for_path(Path::new("chunk.map")),
            "application/json"
        );
        assert_eq!(mime_type_for_path(Path::new("photo.jpg")), "image/jpeg");
        assert_eq!(mime_type_for_path(Path::new("photo.jpeg")), "image/jpeg");
        assert_eq!(mime_type_for_path(Path::new("anim.gif")), "image/gif");
        assert_eq!(mime_type_for_path(Path::new("icon.svg")), "image/svg+xml");
        assert_eq!(mime_type_for_path(Path::new("favicon.ico")), "image/x-icon");
        assert_eq!(mime_type_for_path(Path::new("img.webp")), "image/webp");
        assert_eq!(mime_type_for_path(Path::new("img.avif")), "image/avif");
        assert_eq!(mime_type_for_path(Path::new("font.woff")), "font/woff");
        assert_eq!(mime_type_for_path(Path::new("font.ttf")), "font/ttf");
        assert_eq!(mime_type_for_path(Path::new("font.otf")), "font/otf");
        assert_eq!(
            mime_type_for_path(Path::new("readme.txt")),
            "text/plain; charset=utf-8"
        );
        assert_eq!(mime_type_for_path(Path::new("feed.xml")), "application/xml");
        assert_eq!(mime_type_for_path(Path::new("doc.pdf")), "application/pdf");
    }

    #[test]
    fn mime_type_case_insensitive() {
        assert_eq!(
            mime_type_for_path(Path::new("app.JS")),
            "application/javascript; charset=utf-8"
        );
        assert_eq!(
            mime_type_for_path(Path::new("style.CSS")),
            "text/css; charset=utf-8"
        );
        assert_eq!(
            mime_type_for_path(Path::new("page.HTML")),
            "text/html; charset=utf-8"
        );
    }

    #[test]
    fn mime_type_no_extension() {
        assert_eq!(
            mime_type_for_path(Path::new("Makefile")),
            "application/octet-stream"
        );
        assert_eq!(
            mime_type_for_path(Path::new("")),
            "application/octet-stream"
        );
    }

    #[test]
    fn is_safe_path_rejects_nonexistent() {
        let root = Path::new("/nonexistent/root");
        let resolved = Path::new("/nonexistent/root/file.txt");
        assert!(!is_safe_path(root, resolved));
    }

    #[test]
    fn is_safe_path_accepts_child() {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("file.txt");
        std::fs::write(&child, "ok").unwrap();
        assert!(is_safe_path(dir.path(), &child));
    }

    #[test]
    fn is_safe_path_rejects_outside() {
        let dir = tempfile::tempdir().unwrap();
        let inside = dir.path().join("web");
        std::fs::create_dir(&inside).unwrap();
        let outside = dir.path().join("secret.txt");
        std::fs::write(&outside, "nope").unwrap();
        assert!(!is_safe_path(&inside, &outside));
    }

    #[test]
    fn valid_web_root_candidate_accepts_real_directory() {
        let dir = tempfile::tempdir().unwrap();
        let web = dir.path().join("web");
        std::fs::create_dir(&web).unwrap();
        std::fs::write(web.join("index.html"), "ok").unwrap();

        assert!(is_valid_web_root_candidate(&web));
    }

    #[cfg(unix)]
    #[test]
    fn valid_web_root_candidate_rejects_symlinked_root() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let real_web = dir.path().join("real-web");
        std::fs::create_dir(&real_web).unwrap();
        std::fs::write(real_web.join("index.html"), "ok").unwrap();

        let symlinked_web = dir.path().join("web");
        symlink(&real_web, &symlinked_web).unwrap();

        assert!(!is_valid_web_root_candidate(&symlinked_web));
    }

    #[cfg(unix)]
    #[test]
    fn valid_web_root_candidate_rejects_index_symlink_escape() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let web = dir.path().join("web");
        std::fs::create_dir(&web).unwrap();

        let outside = dir.path().join("outside-index.html");
        std::fs::write(&outside, "outside").unwrap();
        symlink(&outside, web.join("index.html")).unwrap();

        assert!(!is_valid_web_root_candidate(&web));
    }

    #[test]
    fn web_root_empty_path_serves_index() {
        let dir = tempfile::tempdir().unwrap();
        let web = dir.path().join("web");
        std::fs::create_dir(&web).unwrap();
        std::fs::write(web.join("index.html"), "<h1>home</h1>").unwrap();

        let root = WebRoot { root: web };
        let (ct, body) = root.serve("").unwrap();
        assert_eq!(ct, "text/html; charset=utf-8");
        assert_eq!(body, b"<h1>home</h1>");
    }

    #[test]
    fn web_root_debug_impl() {
        let root = WebRoot {
            root: PathBuf::from("/tmp/web"),
        };
        let debug = format!("{root:?}");
        assert!(debug.contains("/tmp/web"));
    }
}
