fn relativize_path(project_root: &str, path: &str) -> Option<String> {
    let path_path = std::path::Path::new(path);
    if path_path.is_absolute() {
        if let Ok(rel) = path_path.strip_prefix(project_root) {
            return Some(rel.to_string_lossy().to_string());
        }
        // Absolute path outside project root
        return None;
    }

    // Check for traversal in relative path
    let mut depth = 0;
    for component in path_path.components() {
        match component {
            std::path::Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return None; // Traversal above root
                }
            }
            std::path::Component::Normal(_) => {
                depth += 1;
            }
            _ => {}
        }
    }

    // Already relative and safe
    Some(path.to_string())
}

fn main() {
    println!("{:?}", relativize_path("/project", "/project/../outside"));
}
