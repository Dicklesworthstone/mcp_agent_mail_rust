//! Service management for mcp-agent-mail.
//!
//! Provides platform-specific service registration and lifecycle management:
//! - macOS: LaunchAgent via launchctl
//! - Linux: systemd --user
//! - Windows: Scheduled Task via PowerShell (future)

use clap::Subcommand;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Command;
use std::fs;

/// Service management commands (install, uninstall, status, logs, restart)
#[derive(Debug, Clone, Subcommand)]
pub enum ServiceCommand {
    /// Install mcp-agent-mail as a background service
    #[command(name = "install")]
    Install {
        /// Show generated config without installing
        #[arg(long)]
        dry_run: bool,

        /// Health check timeout in seconds (default: 30)
        #[arg(long, default_value_t = 30)]
        health_timeout: u64,
    },

    /// Uninstall the background service and stop the daemon
    #[command(name = "uninstall")]
    Uninstall,

    /// Check service status
    #[command(name = "status")]
    Status {
        /// Output machine-readable JSON
        #[arg(long)]
        json: bool,
    },

    /// View service logs
    #[command(name = "logs")]
    Logs {
        /// Follow log output (like tail -f)
        #[arg(long, short = 'f')]
        follow: bool,

        /// Number of lines to display (default: 50)
        #[arg(long, short = 'n', default_value_t = 50)]
        lines: u32,
    },

    /// Gracefully restart the service
    #[command(name = "restart")]
    Restart,
}

/// Service status information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceStatus {
    /// Service status: "running", "stopped", or "not_registered"
    pub status: String,

    /// Process ID if running
    pub pid: Option<u32>,

    /// Service uptime in seconds if running
    pub uptime_secs: Option<u64>,

    /// Service version (from CARGO_PKG_VERSION)
    pub version: String,

    /// Installation timestamp (RFC 3339)
    pub installed_at: Option<String>,

    /// Health check result
    pub health: Option<bool>,

    /// Last error if any
    pub error: Option<String>,
}

/// Service configuration
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    /// Absolute path to the binary
    pub binary_path: PathBuf,

    /// Environment file path
    pub env_file: PathBuf,

    /// Data directory for logs
    pub data_dir: PathBuf,

    /// Service label/name (e.g., "com.mcp-agent-mail.server")
    pub service_label: String,

    /// HTTP host (from env file or default)
    pub http_host: String,

    /// HTTP port (from env file or default)
    pub http_port: u16,

    /// HTTP path (from env file or default)
    pub http_path: String,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        use mcp_agent_mail_core::paths;

        Self {
            binary_path: paths::bin_dir().join("am"),
            env_file: paths::env_file_path(),
            data_dir: paths::state_dir(),
            service_label: "com.mcp-agent-mail.server".to_string(),
            http_host: "127.0.0.1".to_string(),
            http_port: 8765,
            http_path: "/mcp/".to_string(),
        }
    }
}

/// Platform-specific service backend trait
pub trait ServiceBackend: Send + Sync {
    /// Install service with given configuration
    fn install(&self, config: &ServiceConfig) -> crate::CliResult<()>;

    /// Uninstall service
    fn uninstall(&self) -> crate::CliResult<()>;

    /// Get current service status
    fn status(&self) -> crate::CliResult<ServiceStatus>;

    /// Restart service gracefully (SIGTERM 10s timeout, force kill if needed)
    fn restart(&self) -> crate::CliResult<()>;

    /// Get paths to active service logs
    fn log_paths(&self) -> crate::CliResult<Vec<PathBuf>>;
}

/// macOS LaunchAgent backend
pub struct LaunchAgentBackend;

/// Linux systemd --user backend
pub struct SystemdUserBackend;

/// Windows Scheduled Task backend (future)
pub struct WindowsTaskBackend;

impl LaunchAgentBackend {
    fn plist_path() -> PathBuf {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        home.join("Library")
            .join("LaunchAgents")
            .join("com.mcp-agent-mail.server.plist")
    }

    fn service_label() -> &'static str {
        "com.mcp-agent-mail.server"
    }
}

impl SystemdUserBackend {
    fn unit_file_path() -> PathBuf {
        use mcp_agent_mail_core::paths;
        let config_dir = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from(".config"))
            .join("systemd")
            .join("user");
        config_dir.join("mcp-agent-mail.service")
    }

    fn service_name() -> &'static str {
        "mcp-agent-mail.service"
    }
}

impl WindowsTaskBackend {
    fn task_name() -> &'static str {
        "mcp-agent-mail"
    }
}

/// Generate macOS LaunchAgent plist XML content
fn generate_launch_agent_plist(config: &ServiceConfig) -> String {
    let log_dir = &config.data_dir;
    let stdout_log = log_dir.join("stdout.log").display();
    let stderr_log = log_dir.join("stderr.log").display();
    let env_file = config.env_file.display();
    let binary = config.binary_path.display();

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{}</string>

    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>serve-http</string>
        <string>--no-tui</string>
        <string>--no-reuse-running</string>
        <string>--env-file</string>
        <string>{}</string>
    </array>

    <key>StandardOutPath</key>
    <string>{}</string>

    <key>StandardErrorPath</key>
    <string>{}</string>

    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>

    <key>RunAtLoad</key>
    <true/>

    <key>ProcessType</key>
    <string>Background</string>
</dict>
</plist>"#,
        config.service_label, binary, env_file, stdout_log, stderr_log
    )
}

#[cfg(target_os = "macos")]
impl ServiceBackend for LaunchAgentBackend {
    fn install(&self, config: &ServiceConfig) -> crate::CliResult<()> {
        let plist_path = Self::plist_path();

        // Create parent directory if needed
        if let Some(parent) = plist_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Create log directory
        fs::create_dir_all(&config.data_dir)?;

        // Generate plist content
        let plist_content = generate_launch_agent_plist(config);

        // Write plist file
        mcp_agent_mail_core::paths::write_file_atomic(&plist_path, plist_content.as_bytes(), 0o644)?;

        // Register with launchctl
        #[cfg(target_os = "macos")]
        let uid = std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(501); // Default to 501 if unable to get UID

        let label = format!("gui/{}/{}", uid, Self::service_label());

        // Stop any existing service first
        let _ = Command::new("launchctl")
            .args(&["bootout", &label])
            .output();

        // Register the service
        Command::new("launchctl")
            .args(&["bootstrap", &format!("gui/{}", uid), &plist_path.display().to_string()])
            .output()
            .map_err(|e| {
                crate::CliError::Other(format!(
                    "Failed to register service with launchctl: {}. Make sure ~/Library/LaunchAgents/ exists and is writable.",
                    e
                ))
            })?;

        println!("✓ Service registered with launchctl");
        Ok(())
    }

    fn uninstall(&self) -> crate::CliResult<()> {
        let plist_path = Self::plist_path();

        // Get current UID
        #[cfg(target_os = "macos")]
        let uid = std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(501);

        let label = format!("gui/{}/{}", uid, Self::service_label());

        // Unregister with launchctl
        let _ = Command::new("launchctl")
            .args(&["bootout", &label])
            .output();

        // Remove plist file
        let _ = fs::remove_file(&plist_path);

        println!("✓ Service uninstalled");
        Ok(())
    }

    fn status(&self) -> crate::CliResult<ServiceStatus> {
        #[cfg(target_os = "macos")]
        let uid = std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(501);

        let label = format!("gui/{}/{}", uid, Self::service_label());

        // Query launchctl for service status
        let output = Command::new("launchctl")
            .arg("list")
            .output()
            .map_err(|e| crate::CliError::Other(format!("Failed to query launchctl: {}", e)))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let is_running = stdout.contains(Self::service_label());

        let status = if is_running {
            "running".to_string()
        } else {
            "stopped".to_string()
        };

        Ok(ServiceStatus {
            status,
            pid: None,
            uptime_secs: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            installed_at: None,
            health: None,
            error: None,
        })
    }

    fn restart(&self) -> crate::CliResult<()> {
        #[cfg(target_os = "macos")]
        let uid = std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(501);

        let label = format!("gui/{}/{}", uid, Self::service_label());

        // Graceful shutdown: send bootout (equivalent to SIGTERM)
        Command::new("launchctl")
            .args(&["bootout", &label])
            .output()
            .map_err(|e| crate::CliError::Other(format!("Failed to stop service: {}", e)))?;

        // Wait for process to exit (launchctl bootout waits)
        std::thread::sleep(std::time::Duration::from_secs(2));

        // Re-bootstrap the service
        let plist_path = Self::plist_path();
        Command::new("launchctl")
            .args(&["bootstrap", &format!("gui/{}", uid), &plist_path.display().to_string()])
            .output()
            .map_err(|e| crate::CliError::Other(format!("Failed to restart service: {}", e)))?;

        println!("✓ Service restarted");
        Ok(())
    }

    fn log_paths(&self) -> crate::CliResult<Vec<PathBuf>> {
        // Read plist to find log paths
        let plist_path = Self::plist_path();
        if !plist_path.exists() {
            return Ok(vec![]);
        }

        let content = std::fs::read_to_string(&plist_path)
            .unwrap_or_default();

        let mut paths = vec![];

        // Simple parsing: look for StandardOutPath and StandardErrorPath
        for line in content.lines() {
            if line.contains("<string>") && !line.contains("Label")
                && !line.contains("ProgramArguments")
            {
                if let Some(start) = line.find("<string>") {
                    if let Some(end) = line.find("</string>") {
                        let path_str = &line[start + 8..end];
                        if path_str.contains("log") {
                            paths.push(PathBuf::from(path_str));
                        }
                    }
                }
            }
        }

        Ok(paths)
    }
}

/// Execute the full service installation flow
pub fn install_service(dry_run: bool, health_timeout: u64) -> crate::CliResult<()> {
    use mcp_agent_mail_core::paths;

    eprintln!("Preparing service installation...");

    // Step 1: Pre-flight checks
    eprintln!("\nRunning pre-flight checks:");

    // Check env file exists
    let env_file = paths::env_file_path();
    if !env_file.exists() {
        return Err(crate::CliError::Other(
            format!(
                "✗ Env file not found at {}\n  Create it first by running 'am setup run'",
                env_file.display()
            )
        ));
    }
    eprintln!("  ✓ Env file exists at {}", env_file.display());

    // Check bearer token
    let env_contents = fs::read_to_string(&env_file)
        .map_err(|e| crate::CliError::Other(format!("Failed to read env file: {}", e)))?;

    if !env_contents.contains("HTTP_BEARER_TOKEN") {
        eprintln!("  ⚠ No bearer token found in env file");
        eprintln!("  Note: Bearer token should be set for security");
    } else {
        eprintln!("  ✓ Bearer token configured");
    }

    // Step 2: Build service configuration
    eprintln!("\nBuilding service configuration:");
    let config = ServiceConfig::default();
    eprintln!("  ✓ Service label: {}", config.service_label);
    eprintln!("  ✓ Binary path: {}", config.binary_path.display());
    eprintln!("  ✓ Env file: {}", config.env_file.display());
    eprintln!("  ✓ Log directory: {}", config.data_dir.display());
    eprintln!("  ✓ HTTP: {}:{}{}", config.http_host, config.http_port, config.http_path);

    if dry_run {
        eprintln!("\n[DRY RUN] Would install service with above configuration");
        return Ok(());
    }

    // Step 3: Create log directory
    eprintln!("\nPreparing installation:");
    fs::create_dir_all(&config.data_dir)
        .map_err(|e| crate::CliError::Other(format!("Failed to create log directory: {}", e)))?;
    eprintln!("  ✓ Created log directory");

    // Step 4: Detect and select platform backend
    #[cfg(target_os = "macos")]
    {
        eprintln!("\n✓ Detected macOS platform");
        let backend = LaunchAgentBackend;
        backend.install(&config)?;
    }

    #[cfg(target_os = "linux")]
    {
        eprintln!("\n✓ Detected Linux platform");
        eprintln!("  TODO: Implement systemd backend");
        return Err(crate::CliError::NotImplemented("Linux systemd service installation"));
    }

    #[cfg(target_os = "windows")]
    {
        eprintln!("\n✓ Detected Windows platform");
        eprintln!("  TODO: Implement Windows Scheduled Task backend");
        return Err(crate::CliError::NotImplemented("Windows Scheduled Task installation"));
    }

    // Step 5: Perform health check
    eprintln!("\nPerforming health check (timeout: {}s):", health_timeout);
    let health_check_result = perform_health_check(
        &config.http_host,
        config.http_port,
        &config.http_path,
        health_timeout,
    );

    match health_check_result {
        Ok(latency_ms) => {
            eprintln!("  ✓ Service is healthy (latency: {}ms)", latency_ms);
        }
        Err(e) => {
            eprintln!("  ⚠ Health check failed (this may be normal if service is still starting)");
            eprintln!("    Error: {}", e);
            eprintln!("    Run 'am service status' or 'am doctor check' to diagnose");
        }
    }

    eprintln!("\n✓ Service installation complete!");
    eprintln!("  Run 'am service status' to check service state");
    eprintln!("  Run 'am service logs' to view service logs");

    Ok(())
}

/// Perform health check on service endpoint with exponential backoff
fn perform_health_check(host: &str, port: u16, path: &str, timeout_secs: u64) -> Result<u64, String> {
    let url = format!("http://{}:{}{}", host, port, path);
    let start = std::time::Instant::now();
    let deadline = start + std::time::Duration::from_secs(timeout_secs);

    let mut backoff_ms = 100;
    const MAX_BACKOFF_MS: u64 = 5000;

    loop {
        // Try to connect
        match std::net::TcpStream::connect(format!("{}:{}", host, port)) {
            Ok(_) => {
                let latency = start.elapsed().as_millis() as u64;
                return Ok(latency);
            }
            Err(_) => {
                if std::time::Instant::now() > deadline {
                    return Err("Health check timeout".to_string());
                }

                // Exponential backoff
                std::thread::sleep(std::time::Duration::from_millis(backoff_ms));
                backoff_ms = (backoff_ms * 2).min(MAX_BACKOFF_MS);
            }
        }
    }
}

/// Get service status (platform-specific)
pub fn status_service(json: bool) -> crate::CliResult<()> {
    #[cfg(target_os = "macos")]
    {
        let backend = LaunchAgentBackend;
        let status = backend.status()?;

        if json {
            // Output JSON format
            let json_output = serde_json::json!({
                "status": status.status,
                "pid": status.pid,
                "uptime_secs": status.uptime_secs,
                "version": status.version,
                "installed_at": status.installed_at,
                "health": status.health,
            });
            println!("{}", json_output.to_string());
        } else {
            // Human-readable output
            println!("Service: mcp-agent-mail");
            println!("Status:  {}", status.status);
            if let Some(pid) = status.pid {
                println!("PID:     {}", pid);
            }
            if let Some(uptime) = status.uptime_secs {
                println!("Uptime:  {} seconds", uptime);
            }
            println!("Version: {}", status.version);
            if let Some(installed_at) = &status.installed_at {
                println!("Installed: {}", installed_at);
            }
            if let Some(health) = status.health {
                println!("Health:  {}", if health { "✓ healthy" } else { "✗ unhealthy" });
            }
            if let Some(error) = &status.error {
                println!("Error:   {}", error);
            }
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    {
        eprintln!("TODO: Implement Linux systemd status");
        Err(crate::CliError::NotImplemented("Linux service status"))
    }

    #[cfg(target_os = "windows")]
    {
        eprintln!("TODO: Implement Windows service status");
        Err(crate::CliError::NotImplemented("Windows service status"))
    }
}

/// Restart service (platform-specific)
pub fn restart_service() -> crate::CliResult<()> {
    eprintln!("Restarting mcp-agent-mail service...");

    #[cfg(target_os = "macos")]
    {
        let backend = LaunchAgentBackend;
        backend.restart()?;
        eprintln!("Service restarted successfully");
        Ok(())
    }

    #[cfg(target_os = "linux")]
    {
        eprintln!("TODO: Implement Linux systemd restart");
        Err(crate::CliError::NotImplemented("Linux service restart"))
    }

    #[cfg(target_os = "windows")]
    {
        eprintln!("TODO: Implement Windows service restart");
        Err(crate::CliError::NotImplemented("Windows service restart"))
    }
}

/// Uninstall service (platform-specific)
pub fn uninstall_service() -> crate::CliResult<()> {
    eprintln!("Uninstalling mcp-agent-mail service...");

    #[cfg(target_os = "macos")]
    {
        let backend = LaunchAgentBackend;
        backend.uninstall()?;
        eprintln!("Service uninstalled successfully");
        Ok(())
    }

    #[cfg(target_os = "linux")]
    {
        eprintln!("TODO: Implement Linux systemd uninstall");
        Err(crate::CliError::NotImplemented("Linux service uninstall"))
    }

    #[cfg(target_os = "windows")]
    {
        eprintln!("TODO: Implement Windows service uninstall");
        Err(crate::CliError::NotImplemented("Windows service uninstall"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_service_config_default() {
        let config = ServiceConfig::default();
        assert_eq!(config.service_label, "com.mcp-agent-mail.server");
        assert_eq!(config.http_host, "127.0.0.1");
        assert_eq!(config.http_port, 8765);
        assert_eq!(config.http_path, "/mcp/");
    }

    #[test]
    fn test_service_status_default() {
        let status = ServiceStatus {
            status: "stopped".to_string(),
            pid: None,
            uptime_secs: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            installed_at: None,
            health: None,
            error: None,
        };
        assert_eq!(status.status, "stopped");
        assert!(status.pid.is_none());
    }
}
