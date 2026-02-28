//! XDG Base Directory Spec compliant path resolution.
//!
//! Provides standardized paths for configuration, data, and state directories
//! across macOS, Linux, and Windows.
//!
//! Path locations:
//! - Config: `~/.config/mcp-agent-mail/env` (or `$XDG_CONFIG_HOME/mcp-agent-mail/env`)
//! - Data: `~/.local/share/mcp-agent-mail/` (or `$XDG_DATA_HOME/mcp-agent-mail/`)
//! - State/Logs: `~/.local/state/mcp-agent-mail/logs/` (or `$XDG_STATE_HOME/mcp-agent-mail/logs/`)
//!
//! On Windows, uses `%APPDATA%` and `%LOCALAPPDATA%` as these are the Windows conventions.
//! On macOS and Linux, XDG paths are used even for CLI tools (not `~/Library/Application Support/`).

use std::fs;
use std::io;
use std::path::PathBuf;

/// Returns the configuration directory for mcp-agent-mail.
///
/// Priority order:
/// 1. `$XDG_CONFIG_HOME/mcp-agent-mail/` (if XDG_CONFIG_HOME is set)
/// 2. `~/.config/mcp-agent-mail/` (default)
/// 3. `%APPDATA%/mcp-agent-mail/` (Windows)
pub fn config_dir() -> PathBuf {
    if let Ok(xdg_config) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg_config.is_empty() {
            return PathBuf::from(xdg_config).join("mcp-agent-mail");
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Some(app_data) = dirs::config_dir() {
            return app_data.join("mcp-agent-mail");
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        if let Some(home) = dirs::home_dir() {
            return home.join(".config").join("mcp-agent-mail");
        }
    }

    PathBuf::from(".config/mcp-agent-mail")
}

/// Returns the data directory for mcp-agent-mail (DB, git mailbox repo).
///
/// Priority order:
/// 1. `$XDG_DATA_HOME/mcp-agent-mail/` (if XDG_DATA_HOME is set)
/// 2. `~/.local/share/mcp-agent-mail/` (default)
/// 3. `%LOCALAPPDATA%/mcp-agent-mail/` (Windows)
pub fn data_dir() -> PathBuf {
    if let Ok(xdg_data) = std::env::var("XDG_DATA_HOME") {
        if !xdg_data.is_empty() {
            return PathBuf::from(xdg_data).join("mcp-agent-mail");
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Some(local_app_data) = dirs::data_dir() {
            return local_app_data.join("mcp-agent-mail");
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        if let Some(home) = dirs::home_dir() {
            return home.join(".local").join("share").join("mcp-agent-mail");
        }
    }

    PathBuf::from(".local/share/mcp-agent-mail")
}

/// Returns the state directory for mcp-agent-mail (logs, runtime state).
///
/// Priority order:
/// 1. `$XDG_STATE_HOME/mcp-agent-mail/` (if XDG_STATE_HOME is set)
/// 2. `~/.local/state/mcp-agent-mail/` (default)
/// 3. `%LOCALAPPDATA%/mcp-agent-mail/state/` (Windows)
pub fn state_dir() -> PathBuf {
    if let Ok(xdg_state) = std::env::var("XDG_STATE_HOME") {
        if !xdg_state.is_empty() {
            return PathBuf::from(xdg_state).join("mcp-agent-mail");
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Some(local_app_data) = dirs::data_dir() {
            return local_app_data.join("mcp-agent-mail").join("state");
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        if let Some(home) = dirs::home_dir() {
            return home.join(".local").join("state").join("mcp-agent-mail");
        }
    }

    PathBuf::from(".local/state/mcp-agent-mail")
}

/// Returns the binary directory (typically ~/.local/bin).
pub fn bin_dir() -> PathBuf {
    if let Some(home) = dirs::home_dir() {
        return home.join(".local").join("bin");
    }
    PathBuf::from(".local/bin")
}

/// Returns the path to the env file (config_dir/env).
pub fn env_file_path() -> PathBuf {
    config_dir().join("env")
}

/// Returns the path to the SQLite database (data_dir/storage.sqlite3).
pub fn database_path() -> PathBuf {
    data_dir().join("storage.sqlite3")
}

/// Returns the log directory (state_dir/logs).
pub fn log_dir() -> PathBuf {
    state_dir().join("logs")
}

/// Returns the state file path (state_dir/service.json).
pub fn service_state_path() -> PathBuf {
    state_dir().join("service.json")
}

/// Atomically write file: create temp, write, rename, chmod.
///
/// Ensures data is durably written and file permissions are correct.
/// On Unix, sets file mode to the specified value after rename.
/// On Windows, the mode parameter is ignored.
pub fn write_file_atomic(path: &std::path::Path, content: &[u8], mode: u32) -> io::Result<()> {
    // Create parent directory if it doesn't exist
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Write to temporary file with a unique name to avoid collisions
    let temp_path = path.with_extension(format!(
        "tmp.{}",
        std::process::id()
    ));
    fs::write(&temp_path, content)?;

    // Atomic rename
    fs::rename(&temp_path, path)?;

    // Set permissions on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(mode);
        fs::set_permissions(path, perms)?;
    }

    // Silence unused warning on Windows
    let _ = mode;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_dir_has_mcp_agent_mail() {
        let config = config_dir();
        assert!(config.to_string_lossy().contains("mcp-agent-mail"));
    }

    #[test]
    fn test_env_file_path_ends_with_env() {
        let env = env_file_path();
        assert!(env.to_string_lossy().ends_with("env"));
    }

    #[test]
    fn test_database_path_ends_with_sqlite3() {
        let db = database_path();
        assert!(db.to_string_lossy().ends_with("storage.sqlite3"));
    }

    #[test]
    fn test_log_dir_contains_logs() {
        let logs = log_dir();
        assert!(logs.to_string_lossy().contains("logs"));
    }

    #[test]
    fn test_paths_are_absolute_or_relative() {
        // Paths should be either absolute or start with .
        let config = config_dir();
        assert!(config.is_absolute() || config.to_string_lossy().starts_with('.'));
    }
}
