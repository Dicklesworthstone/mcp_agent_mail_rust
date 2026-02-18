//! Static file serving for the optional `web/` SPA directory.
//!
//! Legacy Python mounts `web/` at root if it exists and contains `index.html`.
//! We replicate this behavior: resolve the directory at startup, serve files with
//! correct MIME types, and fall back to `index.html` for SPA routing.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

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
        if !relative.is_empty() {
            let file_path = self.root.join(relative);
            if file_path.is_file() && is_safe_path(&self.root, &file_path) {
                return Self::read_file(&file_path);
            }
        }

        // Directory index: try appending /index.html.
        if relative.is_empty() || relative.ends_with('/') {
            let index = self.root.join(relative).join("index.html");
            if index.is_file() && is_safe_path(&self.root, &index) {
                return Self::read_file(&index);
            }
        }

        // SPA fallback: return index.html for any path that isn't a file.
        let index = self.root.join("index.html");
        if index.is_file() && is_safe_path(&self.root, &index) {
            return Self::read_file(&index);
        }

        None
    }

    fn read_file(path: &Path) -> Option<(&'static str, Vec<u8>)> {
        let content_type = mime_type_for_path(path);
        let body = std::fs::read(path).ok()?;
        Some((content_type, body))
    }
}

/// Resolve the web root directory, matching legacy Python behavior.
///
/// Checks candidates in order:
/// 1. `<executable_parent>/../../../web` (legacy: relative to Python source)
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
        candidates.push(cwd.join("web"));
    }

    for candidate in candidates {
        if candidate.is_dir() && candidate.join("index.html").is_file() {
            return Some(WebRoot { root: candidate });
        }
    }

    None
}

fn executable_web_candidates(exe_parent: &Path) -> Vec<PathBuf> {
    // Keep parity with legacy path probing: parent/web, ../../web, ../../../web.
    exe_parent
        .ancestors()
        .take(3)
        .map(|ancestor| ancestor.join("web"))
        .collect()
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
    fn executable_web_candidates_include_third_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("a").join("b").join("c").join("d");
        std::fs::create_dir_all(&parent).unwrap();

        let candidates = executable_web_candidates(&parent);
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0], parent.join("web"));
        assert_eq!(candidates[1], parent.parent().unwrap().join("web"));
        assert_eq!(
            candidates[2],
            parent.parent().and_then(Path::parent).unwrap().join("web")
        );
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
}
