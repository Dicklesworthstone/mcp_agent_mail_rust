//! Deployment plan executor for the native share wizard.
//!
//! Executes deployment plans step-by-step, creating files and running commands
//! as specified in the plan.
//!
//! # Design Rationale
//!
//! The executor operates in a controlled, observable manner:
//! - Each step is executed sequentially
//! - Step outcomes are recorded for reporting
//! - Errors in optional steps don't halt execution
//! - Confirmation prompts are handled by the caller (prompt module)
//!
//! File generation (headers, nojekyll) is handled internally.
//! Shell commands are executed via `std::process::Command`.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use crate::wizard::{
    DeploymentPlan, HostingProvider, PlanStep, StepOutcome, WIZARD_VERSION, WizardError,
    WizardErrorCode, WizardMetadata, WizardMode, WizardResult,
};

/// Execution configuration.
#[derive(Debug, Clone, Default)]
pub struct ExecutorConfig {
    /// Whether to prompt for confirmation on confirmable steps.
    pub interactive: bool,
    /// Skip all confirmations (auto-yes).
    pub skip_confirm: bool,
    /// Dry-run mode (don't execute, just report).
    pub dry_run: bool,
    /// Show verbose output.
    pub verbose: bool,
}

/// Execute a deployment plan.
///
/// Returns a `WizardResult` with step outcomes and timing information.
pub fn execute_plan(
    plan: &DeploymentPlan,
    config: &ExecutorConfig,
) -> Result<WizardResult, WizardError> {
    let start = Instant::now();
    let mut outcomes = Vec::new();
    let mut all_files_created = Vec::new();

    for step in &plan.steps {
        let step_start = Instant::now();

        // Check if we should skip this step
        if step.requires_confirm && !config.skip_confirm {
            if config.interactive {
                if !prompt_step_confirm(step)? {
                    outcomes.push(StepOutcome {
                        step_id: step.id.clone(),
                        success: true,
                        message: "Skipped by user".to_string(),
                        duration_ms: step_start.elapsed().as_millis() as u64,
                        files_created: vec![],
                    });
                    continue;
                }
            } else if !config.dry_run {
                // Non-interactive and not dry-run: skip confirmable steps
                outcomes.push(StepOutcome {
                    step_id: step.id.clone(),
                    success: true,
                    message: "Skipped (requires confirmation in non-interactive mode)".to_string(),
                    duration_ms: step_start.elapsed().as_millis() as u64,
                    files_created: vec![],
                });
                continue;
            }
        }

        // Execute the step
        let outcome = if config.dry_run {
            StepOutcome {
                step_id: step.id.clone(),
                success: true,
                message: format!("[dry-run] Would execute: {}", step.description),
                duration_ms: step_start.elapsed().as_millis() as u64,
                files_created: vec![],
            }
        } else {
            execute_step(step, plan, config.verbose)?
        };

        all_files_created.extend(outcome.files_created.clone());
        outcomes.push(outcome);
    }

    let total_duration_ms = start.elapsed().as_millis() as u64;

    Ok(WizardResult {
        success: outcomes
            .iter()
            .all(|o| o.success || plan.steps.iter().any(|s| s.id == o.step_id && s.optional)),
        provider: plan.provider,
        bundle_path: plan.bundle_path.clone(),
        deployed_url: plan.expected_url.clone(),
        steps: outcomes,
        total_duration_ms,
        error: None,
        error_code: None,
        metadata: WizardMetadata {
            version: WIZARD_VERSION.to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            mode: if config.interactive {
                WizardMode::Interactive
            } else {
                WizardMode::NonInteractive
            },
            dry_run: config.dry_run,
        },
    })
}

/// Execute a single plan step.
fn execute_step(
    step: &PlanStep,
    plan: &DeploymentPlan,
    verbose: bool,
) -> Result<StepOutcome, WizardError> {
    let start = Instant::now();
    let mut files_created = Vec::new();

    // Handle special step types
    match step.id.as_str() {
        "create_output_dir" => {
            if let Some(ref cmd) = step.command {
                // Extract path from "mkdir -p <path>"
                if let Some(path) = path_from_simple_shell_command(cmd, "mkdir -p ") {
                    ensure_real_directory(&path).map_err(|e| {
                        WizardError::new(
                            WizardErrorCode::FileOperationFailed,
                            format!("Failed to create directory: {e}"),
                        )
                        .with_context(path.display().to_string())
                    })?;
                    if verbose {
                        eprintln!("  Created directory: {}", path.display());
                    }
                }
            }
        }
        "copy_bundle" => {
            if let Some(ref cmd) = step.command {
                // Execute copy command
                let output = execute_shell_command(cmd)?;
                if verbose && !output.is_empty() {
                    eprintln!("  {output}");
                }
            }
        }
        "create_nojekyll" => {
            if let Some(ref cmd) = step.command {
                // Extract path from "touch <path>"
                if let Some(path) = path_from_simple_shell_command(cmd, "touch ") {
                    write_generated_file(&path, "").map_err(|e| {
                        WizardError::new(
                            WizardErrorCode::FileOperationFailed,
                            format!("Failed to create .nojekyll: {e}"),
                        )
                        .with_context(path.display().to_string())
                    })?;
                    files_created.push(path.clone());
                    if verbose {
                        eprintln!("  Created: {}", path.display());
                    }
                }
            }
        }
        "create_headers" => {
            // Generate headers file content based on provider
            let headers_content = generate_headers_content(plan.provider);
            // Find the headers file path from generated_files
            if let Some(headers_path) = plan.generated_files.iter().find(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n == "_headers")
                    .unwrap_or(false)
            }) {
                write_generated_file(headers_path, &headers_content).map_err(|e| {
                    WizardError::new(
                        WizardErrorCode::FileOperationFailed,
                        format!("Failed to write _headers: {e}"),
                    )
                    .with_context(headers_path.display().to_string())
                })?;
                files_created.push(headers_path.clone());
                if verbose {
                    eprintln!("  Created: {}", headers_path.display());
                }
            }
        }
        "create_workflow" | "create_netlify_toml" | "create_redirects" => {
            // These are optional file generation steps
            // Skip for now - they require more complex templates
            if verbose {
                eprintln!("  [skipped] {}: requires template generation", step.id);
            }
        }
        "git_commit"
        | "git_push"
        | "wrangler_deploy"
        | "netlify_deploy"
        | "s3_sync"
        | "s3_content_types"
        | "cloudfront_invalidate" => {
            // Execute shell command
            if let Some(ref cmd) = step.command {
                let output = execute_shell_command(cmd)?;
                if verbose && !output.is_empty() {
                    eprintln!("  {output}");
                }
            }
        }
        "manual_deploy" | "configure_headers" => {
            // Informational steps - no action needed
            if verbose {
                eprintln!("  [info] {}", step.description);
            }
        }
        _ => {
            // Unknown step type - try to execute command if present
            if let Some(ref cmd) = step.command {
                let output = execute_shell_command(cmd)?;
                if verbose && !output.is_empty() {
                    eprintln!("  {output}");
                }
            }
        }
    }

    Ok(StepOutcome {
        step_id: step.id.clone(),
        success: true,
        message: format!("Completed: {}", step.description),
        duration_ms: start.elapsed().as_millis() as u64,
        files_created,
    })
}

/// Execute a shell command and return the output.
fn execute_shell_command(command: &str) -> Result<String, WizardError> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(command)
        .output()
        .map_err(|e| {
            WizardError::new(
                WizardErrorCode::CommandFailed,
                format!("Failed to execute command: {e}"),
            )
            .with_context(command.to_string())
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(WizardError::new(
            WizardErrorCode::CommandFailed,
            format!("Command failed: {}", stderr.trim()),
        )
        .with_context(command.to_string()));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn ensure_real_directory(path: &Path) -> std::io::Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        use std::path::Component;

        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(std::io::Error::other(format!(
                    "refusing to create directory with parent traversal: {}",
                    path.display()
                )));
            }
            Component::Normal(segment) => {
                current.push(segment);
                match std::fs::symlink_metadata(&current) {
                    Ok(metadata) => {
                        if metadata.file_type().is_symlink() {
                            return Err(std::io::Error::other(format!(
                                "refusing to traverse symlinked directory {}",
                                current.display()
                            )));
                        }
                        if !metadata.file_type().is_dir() {
                            return Err(std::io::Error::other(format!(
                                "expected directory but found non-directory {}",
                                current.display()
                            )));
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        std::fs::create_dir(&current)?;
                    }
                    Err(error) => return Err(error),
                }
            }
        }
    }
    Ok(())
}

fn write_generated_file(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        ensure_real_directory(parent)?;
    }
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() {
            return Err(std::io::Error::other(format!(
                "refusing to write through symlinked path {}",
                path.display()
            )));
        }
        if !metadata.file_type().is_file() {
            return Err(std::io::Error::other(format!(
                "expected file but found non-file {}",
                path.display()
            )));
        }
    }
    std::fs::write(path, content)
}

fn path_from_simple_shell_command(command: &str, prefix: &str) -> Option<PathBuf> {
    let raw = command.strip_prefix(prefix)?.trim();
    decode_shell_single_argument(raw).map(PathBuf::from)
}

fn decode_shell_single_argument(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    let mut out = String::new();
    let chars: Vec<char> = raw.chars().collect();
    let mut idx = 0usize;

    while idx < chars.len() {
        match chars[idx] {
            '\'' => {
                idx += 1;
                while idx < chars.len() && chars[idx] != '\'' {
                    out.push(chars[idx]);
                    idx += 1;
                }
                if idx >= chars.len() {
                    return None;
                }
                idx += 1;
            }
            '\\' => {
                idx += 1;
                if idx >= chars.len() {
                    return None;
                }
                out.push(chars[idx]);
                idx += 1;
            }
            c if c.is_whitespace() => {
                if chars[idx..].iter().all(|ch| ch.is_whitespace()) {
                    return Some(out);
                }
                return None;
            }
            c => {
                out.push(c);
                idx += 1;
            }
        }
    }

    Some(out)
}

/// Generate headers file content for a provider.
fn generate_headers_content(provider: HostingProvider) -> String {
    // COOP/COEP headers required for SharedArrayBuffer (used by SQLite WASM)
    match provider {
        HostingProvider::GithubPages
        | HostingProvider::CloudflarePages
        | HostingProvider::Netlify => r#"/*
  Cross-Origin-Opener-Policy: same-origin
  Cross-Origin-Embedder-Policy: require-corp
  Cross-Origin-Resource-Policy: cross-origin

/*.wasm
  Content-Type: application/wasm

/*.sqlite3
  Content-Type: application/x-sqlite3
"#
        .to_string(),
        HostingProvider::S3 | HostingProvider::Custom => {
            r#"# Required headers for Agent Mail viewer
# Configure these in your server/CDN:
#
# Cross-Origin-Opener-Policy: same-origin
# Cross-Origin-Embedder-Policy: require-corp
# Cross-Origin-Resource-Policy: cross-origin
#
# For .wasm files:
#   Content-Type: application/wasm
#
# For .sqlite3 files:
#   Content-Type: application/x-sqlite3
"#
            .to_string()
        }
    }
}

/// Prompt user to confirm a step.
fn prompt_step_confirm(step: &PlanStep) -> Result<bool, WizardError> {
    eprint!(
        "  Execute step {}. {}? [Y/n]: ",
        step.index, step.description
    );
    io::stderr().flush().ok();

    let stdin = io::stdin();
    let mut line = String::new();
    stdin.lock().read_line(&mut line).map_err(|e| {
        WizardError::new(
            WizardErrorCode::InternalError,
            format!("Failed to read input: {e}"),
        )
    })?;

    let trimmed = line.trim().to_ascii_lowercase();
    Ok(trimmed.is_empty() || trimmed == "y" || trimmed == "yes")
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn shell_quote(path: &std::path::Path) -> String {
        let raw = path.to_string_lossy();
        if !raw.contains([' ', '\'', '\t', '\n']) {
            return raw.into_owned();
        }

        let mut out = String::from("'");
        for ch in raw.chars() {
            if ch == '\'' {
                out.push_str("'\\''");
            } else {
                out.push(ch);
            }
        }
        out.push('\'');
        out
    }

    #[test]
    fn headers_content_github_includes_coop_coep() {
        let content = generate_headers_content(HostingProvider::GithubPages);
        assert!(content.contains("Cross-Origin-Opener-Policy"));
        assert!(content.contains("Cross-Origin-Embedder-Policy"));
        assert!(content.contains("application/wasm"));
    }

    #[test]
    fn headers_content_s3_is_comment_format() {
        let content = generate_headers_content(HostingProvider::S3);
        assert!(content.starts_with('#'));
        assert!(content.contains("Cross-Origin-Opener-Policy"));
    }

    #[test]
    fn execute_plan_dry_run_does_not_create_files() {
        let temp = tempfile::tempdir().unwrap();
        let plan = DeploymentPlan {
            provider: HostingProvider::Custom,
            bundle_path: temp.path().to_path_buf(),
            steps: vec![PlanStep {
                index: 1,
                id: "create_nojekyll".to_string(),
                description: "Create .nojekyll".to_string(),
                command: Some(format!("touch {}", temp.path().join(".nojekyll").display())),
                optional: false,
                requires_confirm: false,
            }],
            expected_url: None,
            generated_files: vec![],
            warnings: vec![],
        };

        let config = ExecutorConfig {
            dry_run: true,
            ..Default::default()
        };

        let result = execute_plan(&plan, &config).unwrap();
        assert!(result.success);
        assert!(!temp.path().join(".nojekyll").exists());
    }

    #[test]
    fn execute_step_creates_directory() {
        let temp = tempfile::tempdir().unwrap();
        let new_dir = temp.path().join("output").join("docs");
        let plan = DeploymentPlan {
            provider: HostingProvider::GithubPages,
            bundle_path: temp.path().to_path_buf(),
            steps: vec![],
            expected_url: None,
            generated_files: vec![],
            warnings: vec![],
        };

        let step = PlanStep {
            index: 1,
            id: "create_output_dir".to_string(),
            description: "Create output directory".to_string(),
            command: Some(format!("mkdir -p {}", new_dir.display())),
            optional: false,
            requires_confirm: false,
        };

        execute_step(&step, &plan, false).unwrap();
        assert!(new_dir.exists());
    }

    #[test]
    fn execute_step_creates_directory_from_quoted_path() {
        let temp = tempfile::tempdir().unwrap();
        let new_dir = temp.path().join("docs with space").join("o'hare");
        let plan = DeploymentPlan {
            provider: HostingProvider::GithubPages,
            bundle_path: temp.path().to_path_buf(),
            steps: vec![],
            expected_url: None,
            generated_files: vec![],
            warnings: vec![],
        };

        let step = PlanStep {
            index: 1,
            id: "create_output_dir".to_string(),
            description: "Create output directory".to_string(),
            command: Some(format!("mkdir -p {}", shell_quote(&new_dir))),
            optional: false,
            requires_confirm: false,
        };

        execute_step(&step, &plan, false).unwrap();
        assert!(new_dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn execute_step_rejects_symlinked_output_dir_ancestor() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let linked = temp.path().join("linked");
        symlink(outside.path(), &linked).unwrap();
        let new_dir = linked.join("docs");

        let plan = DeploymentPlan {
            provider: HostingProvider::GithubPages,
            bundle_path: temp.path().to_path_buf(),
            steps: vec![],
            expected_url: None,
            generated_files: vec![],
            warnings: vec![],
        };

        let step = PlanStep {
            index: 1,
            id: "create_output_dir".to_string(),
            description: "Create output directory".to_string(),
            command: Some(format!("mkdir -p {}", new_dir.display())),
            optional: false,
            requires_confirm: false,
        };

        let result = execute_step(&step, &plan, false);
        assert!(matches!(
            result,
            Err(error)
                if error.message.contains("symlinked directory")
                    && error.context.as_deref() == Some(new_dir.to_string_lossy().as_ref())
        ));
    }

    #[test]
    fn execute_step_creates_nojekyll() {
        let temp = tempfile::tempdir().unwrap();
        let nojekyll = temp.path().join(".nojekyll");
        let plan = DeploymentPlan {
            provider: HostingProvider::GithubPages,
            bundle_path: temp.path().to_path_buf(),
            steps: vec![],
            expected_url: None,
            generated_files: vec![],
            warnings: vec![],
        };

        let step = PlanStep {
            index: 1,
            id: "create_nojekyll".to_string(),
            description: "Create .nojekyll".to_string(),
            command: Some(format!("touch {}", nojekyll.display())),
            optional: false,
            requires_confirm: false,
        };

        execute_step(&step, &plan, false).unwrap();
        assert!(nojekyll.exists());
    }

    #[test]
    fn execute_step_creates_nojekyll_from_quoted_path() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("site with space");
        std::fs::create_dir_all(&dir).unwrap();
        let nojekyll = dir.join(".nojekyll");
        let plan = DeploymentPlan {
            provider: HostingProvider::GithubPages,
            bundle_path: temp.path().to_path_buf(),
            steps: vec![],
            expected_url: None,
            generated_files: vec![],
            warnings: vec![],
        };

        let step = PlanStep {
            index: 1,
            id: "create_nojekyll".to_string(),
            description: "Create .nojekyll".to_string(),
            command: Some(format!("touch {}", shell_quote(&nojekyll))),
            optional: false,
            requires_confirm: false,
        };

        execute_step(&step, &plan, false).unwrap();
        assert!(nojekyll.exists());
    }

    #[cfg(unix)]
    #[test]
    fn execute_step_rejects_symlinked_nojekyll_path() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let nojekyll = temp.path().join(".nojekyll");
        let outside = temp.path().join("outside.txt");
        std::fs::write(&outside, "keep").unwrap();
        symlink(&outside, &nojekyll).unwrap();

        let plan = DeploymentPlan {
            provider: HostingProvider::GithubPages,
            bundle_path: temp.path().to_path_buf(),
            steps: vec![],
            expected_url: None,
            generated_files: vec![],
            warnings: vec![],
        };

        let step = PlanStep {
            index: 1,
            id: "create_nojekyll".to_string(),
            description: "Create .nojekyll".to_string(),
            command: Some(format!("touch {}", nojekyll.display())),
            optional: false,
            requires_confirm: false,
        };

        let result = execute_step(&step, &plan, false);
        assert!(matches!(
            result,
            Err(error)
                if error.message.contains("symlinked path")
                    && error.context.as_deref() == Some(nojekyll.to_string_lossy().as_ref())
        ));
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "keep");
    }

    #[test]
    fn execute_step_creates_headers_file() {
        let temp = tempfile::tempdir().unwrap();
        let headers_path = temp.path().join("_headers");
        let plan = DeploymentPlan {
            provider: HostingProvider::GithubPages,
            bundle_path: temp.path().to_path_buf(),
            steps: vec![],
            expected_url: None,
            generated_files: vec![headers_path.clone()],
            warnings: vec![],
        };

        let step = PlanStep {
            index: 1,
            id: "create_headers".to_string(),
            description: "Create _headers file".to_string(),
            command: None,
            optional: false,
            requires_confirm: false,
        };

        execute_step(&step, &plan, false).unwrap();
        assert!(headers_path.exists());
        let content = std::fs::read_to_string(&headers_path).unwrap();
        assert!(content.contains("Cross-Origin-Opener-Policy"));
    }

    #[cfg(unix)]
    #[test]
    fn execute_step_rejects_symlinked_headers_path() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let headers_path = temp.path().join("_headers");
        let outside = temp.path().join("outside-headers");
        std::fs::write(&outside, "preserve").unwrap();
        symlink(&outside, &headers_path).unwrap();

        let plan = DeploymentPlan {
            provider: HostingProvider::GithubPages,
            bundle_path: temp.path().to_path_buf(),
            steps: vec![],
            expected_url: None,
            generated_files: vec![headers_path.clone()],
            warnings: vec![],
        };

        let step = PlanStep {
            index: 1,
            id: "create_headers".to_string(),
            description: "Create _headers file".to_string(),
            command: None,
            optional: false,
            requires_confirm: false,
        };

        let result = execute_step(&step, &plan, false);
        assert!(matches!(
            result,
            Err(error)
                if error.message.contains("symlinked path")
                    && error.context.as_deref() == Some(headers_path.to_string_lossy().as_ref())
        ));
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "preserve");
    }

    // ── generate_headers_content: all providers ──────────────────────

    #[test]
    fn headers_content_cloudflare_pages_includes_coop_coep() {
        let content = generate_headers_content(HostingProvider::CloudflarePages);
        assert!(content.contains("Cross-Origin-Opener-Policy: same-origin"));
        assert!(content.contains("Cross-Origin-Embedder-Policy: require-corp"));
        assert!(content.contains("Cross-Origin-Resource-Policy: cross-origin"));
        assert!(content.contains("application/wasm"));
        assert!(content.contains("application/x-sqlite3"));
    }

    #[test]
    fn headers_content_netlify_includes_coop_coep() {
        let content = generate_headers_content(HostingProvider::Netlify);
        assert!(content.contains("Cross-Origin-Opener-Policy: same-origin"));
        assert!(content.contains("application/wasm"));
    }

    #[test]
    fn headers_content_custom_is_comment_format() {
        let content = generate_headers_content(HostingProvider::Custom);
        assert!(content.starts_with('#'));
        assert!(content.contains("Cross-Origin-Opener-Policy"));
    }

    #[test]
    fn headers_content_github_includes_sqlite3_type() {
        let content = generate_headers_content(HostingProvider::GithubPages);
        assert!(content.contains("application/x-sqlite3"));
    }

    #[test]
    fn headers_content_s3_mentions_wasm() {
        let content = generate_headers_content(HostingProvider::S3);
        assert!(content.contains("application/wasm"));
        assert!(content.contains("application/x-sqlite3"));
    }

    // ── ExecutorConfig defaults ──────────────────────────────────────

    #[test]
    fn executor_config_default() {
        let config = ExecutorConfig::default();
        assert!(!config.interactive);
        assert!(!config.skip_confirm);
        assert!(!config.dry_run);
        assert!(!config.verbose);
    }

    // ── execute_plan: confirmable steps in non-interactive mode ──────

    #[test]
    fn execute_plan_non_interactive_skips_confirmable_steps() {
        let temp = tempfile::tempdir().unwrap();
        let plan = DeploymentPlan {
            provider: HostingProvider::Custom,
            bundle_path: temp.path().to_path_buf(),
            steps: vec![PlanStep {
                index: 1,
                id: "manual_deploy".to_string(),
                description: "Deploy manually".to_string(),
                command: None,
                optional: false,
                requires_confirm: true,
            }],
            expected_url: None,
            generated_files: vec![],
            warnings: vec![],
        };

        let config = ExecutorConfig {
            interactive: false,
            skip_confirm: false,
            dry_run: false,
            verbose: false,
        };

        let result = execute_plan(&plan, &config).unwrap();
        assert!(result.success);
        assert_eq!(result.steps.len(), 1);
        assert!(result.steps[0].message.contains("Skipped"));
    }

    #[test]
    fn execute_plan_skip_confirm_runs_confirmable_steps() {
        let temp = tempfile::tempdir().unwrap();
        let plan = DeploymentPlan {
            provider: HostingProvider::Custom,
            bundle_path: temp.path().to_path_buf(),
            steps: vec![PlanStep {
                index: 1,
                id: "manual_deploy".to_string(),
                description: "Info step".to_string(),
                command: None,
                optional: false,
                requires_confirm: true,
            }],
            expected_url: None,
            generated_files: vec![],
            warnings: vec![],
        };

        let config = ExecutorConfig {
            interactive: false,
            skip_confirm: true,
            dry_run: false,
            verbose: false,
        };

        let result = execute_plan(&plan, &config).unwrap();
        assert!(result.success);
        assert_eq!(result.steps.len(), 1);
        assert!(result.steps[0].message.contains("Completed"));
    }

    // ── execute_plan: multiple steps ─────────────────────────────────

    #[test]
    fn execute_plan_multiple_steps_all_succeed() {
        let temp = tempfile::tempdir().unwrap();
        let new_dir = temp.path().join("output");
        let nojekyll = temp.path().join(".nojekyll");

        let plan = DeploymentPlan {
            provider: HostingProvider::GithubPages,
            bundle_path: temp.path().to_path_buf(),
            steps: vec![
                PlanStep {
                    index: 1,
                    id: "create_output_dir".to_string(),
                    description: "Create output directory".to_string(),
                    command: Some(format!("mkdir -p {}", new_dir.display())),
                    optional: false,
                    requires_confirm: false,
                },
                PlanStep {
                    index: 2,
                    id: "create_nojekyll".to_string(),
                    description: "Create .nojekyll".to_string(),
                    command: Some(format!("touch {}", nojekyll.display())),
                    optional: false,
                    requires_confirm: false,
                },
            ],
            expected_url: Some("https://example.com".to_string()),
            generated_files: vec![],
            warnings: vec![],
        };

        let config = ExecutorConfig::default();
        let result = execute_plan(&plan, &config).unwrap();
        assert!(result.success);
        assert_eq!(result.steps.len(), 2);
        assert!(result.steps.iter().all(|s| s.success));
        assert!(new_dir.exists());
        assert!(nojekyll.exists());
    }

    // ── execute_plan: dry run metadata ───────────────────────────────

    #[test]
    fn execute_plan_dry_run_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let plan = DeploymentPlan {
            provider: HostingProvider::CloudflarePages,
            bundle_path: temp.path().to_path_buf(),
            steps: vec![PlanStep {
                index: 1,
                id: "wrangler_deploy".to_string(),
                description: "Deploy with Wrangler".to_string(),
                command: Some("wrangler pages deploy .".to_string()),
                optional: false,
                requires_confirm: false,
            }],
            expected_url: Some("https://cf.example.com".to_string()),
            generated_files: vec![],
            warnings: vec![],
        };

        let config = ExecutorConfig {
            dry_run: true,
            ..Default::default()
        };

        let result = execute_plan(&plan, &config).unwrap();
        assert!(result.success);
        assert_eq!(result.metadata.mode, WizardMode::NonInteractive);
        assert!(result.metadata.dry_run);
        assert_eq!(result.metadata.version, WIZARD_VERSION);
        assert!(!result.metadata.timestamp.is_empty());
        assert_eq!(result.provider, HostingProvider::CloudflarePages);
        assert_eq!(
            result.deployed_url,
            Some("https://cf.example.com".to_string())
        );
        // Dry-run messages contain "[dry-run]"
        assert!(result.steps[0].message.contains("[dry-run]"));
    }

    #[test]
    fn execute_plan_interactive_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let plan = DeploymentPlan {
            provider: HostingProvider::S3,
            bundle_path: temp.path().to_path_buf(),
            steps: vec![],
            expected_url: None,
            generated_files: vec![],
            warnings: vec![],
        };

        let config = ExecutorConfig {
            interactive: true,
            dry_run: false,
            skip_confirm: false,
            verbose: false,
        };

        let result = execute_plan(&plan, &config).unwrap();
        assert_eq!(result.metadata.mode, WizardMode::Interactive);
        assert!(!result.metadata.dry_run);
    }

    // ── execute_step: informational steps ────────────────────────────

    #[test]
    fn execute_step_manual_deploy_is_noop() {
        let temp = tempfile::tempdir().unwrap();
        let plan = DeploymentPlan {
            provider: HostingProvider::Custom,
            bundle_path: temp.path().to_path_buf(),
            steps: vec![],
            expected_url: None,
            generated_files: vec![],
            warnings: vec![],
        };

        let step = PlanStep {
            index: 1,
            id: "manual_deploy".to_string(),
            description: "Deploy manually to your server".to_string(),
            command: None,
            optional: false,
            requires_confirm: false,
        };

        let outcome = execute_step(&step, &plan, false).unwrap();
        assert!(outcome.success);
        assert!(outcome.files_created.is_empty());
    }

    #[test]
    fn execute_step_configure_headers_is_noop() {
        let temp = tempfile::tempdir().unwrap();
        let plan = DeploymentPlan {
            provider: HostingProvider::Custom,
            bundle_path: temp.path().to_path_buf(),
            steps: vec![],
            expected_url: None,
            generated_files: vec![],
            warnings: vec![],
        };

        let step = PlanStep {
            index: 1,
            id: "configure_headers".to_string(),
            description: "Configure server headers".to_string(),
            command: None,
            optional: false,
            requires_confirm: false,
        };

        let outcome = execute_step(&step, &plan, false).unwrap();
        assert!(outcome.success);
    }

    // ── execute_step: unknown step with command ──────────────────────

    #[test]
    fn execute_step_unknown_id_runs_command() {
        let temp = tempfile::tempdir().unwrap();
        let plan = DeploymentPlan {
            provider: HostingProvider::Custom,
            bundle_path: temp.path().to_path_buf(),
            steps: vec![],
            expected_url: None,
            generated_files: vec![],
            warnings: vec![],
        };

        let step = PlanStep {
            index: 1,
            id: "custom_step_42".to_string(),
            description: "A custom step".to_string(),
            command: Some("echo hello".to_string()),
            optional: false,
            requires_confirm: false,
        };

        let outcome = execute_step(&step, &plan, false).unwrap();
        assert!(outcome.success);
        assert!(outcome.message.contains("Completed"));
    }

    // ── execute_shell_command ─────────────────────────────────────────

    #[test]
    fn execute_shell_command_echo() {
        let output = execute_shell_command("echo test_output").unwrap();
        assert_eq!(output, "test_output");
    }

    #[test]
    fn execute_shell_command_failing_command() {
        let err = execute_shell_command("false").unwrap_err();
        assert_eq!(err.code, WizardErrorCode::CommandFailed);
    }

    #[test]
    fn execute_shell_command_nonexistent_command() {
        let err = execute_shell_command("this_command_does_not_exist_xyz 2>/dev/null");
        assert!(err.is_err());
    }

    // ── execute_step: create_output_dir without mkdir prefix ─────────

    #[test]
    fn execute_step_create_output_dir_no_command() {
        let temp = tempfile::tempdir().unwrap();
        let plan = DeploymentPlan {
            provider: HostingProvider::Custom,
            bundle_path: temp.path().to_path_buf(),
            steps: vec![],
            expected_url: None,
            generated_files: vec![],
            warnings: vec![],
        };

        let step = PlanStep {
            index: 1,
            id: "create_output_dir".to_string(),
            description: "Create output directory".to_string(),
            command: None,
            optional: false,
            requires_confirm: false,
        };

        let outcome = execute_step(&step, &plan, false).unwrap();
        assert!(outcome.success);
    }

    // ── execute_step: create_headers without matching generated_file ─

    #[test]
    fn execute_step_create_headers_no_generated_file() {
        let temp = tempfile::tempdir().unwrap();
        let plan = DeploymentPlan {
            provider: HostingProvider::GithubPages,
            bundle_path: temp.path().to_path_buf(),
            steps: vec![],
            expected_url: None,
            generated_files: vec![], // no _headers in list
            warnings: vec![],
        };

        let step = PlanStep {
            index: 1,
            id: "create_headers".to_string(),
            description: "Create _headers".to_string(),
            command: None,
            optional: false,
            requires_confirm: false,
        };

        // Should succeed without creating any files (no match in generated_files)
        let outcome = execute_step(&step, &plan, false).unwrap();
        assert!(outcome.success);
        assert!(outcome.files_created.is_empty());
    }

    // ── execute_step: copy_bundle with echo ──────────────────────────

    #[test]
    fn execute_step_copy_bundle_runs_command() {
        let temp = tempfile::tempdir().unwrap();
        let plan = DeploymentPlan {
            provider: HostingProvider::Custom,
            bundle_path: temp.path().to_path_buf(),
            steps: vec![],
            expected_url: None,
            generated_files: vec![],
            warnings: vec![],
        };

        let step = PlanStep {
            index: 1,
            id: "copy_bundle".to_string(),
            description: "Copy bundle".to_string(),
            command: Some("echo copied".to_string()),
            optional: false,
            requires_confirm: false,
        };

        let outcome = execute_step(&step, &plan, false).unwrap();
        assert!(outcome.success);
    }

    // ── execute_step: optional template steps are skipped ─────────────

    #[test]
    fn execute_step_create_workflow_skipped() {
        let temp = tempfile::tempdir().unwrap();
        let plan = DeploymentPlan {
            provider: HostingProvider::GithubPages,
            bundle_path: temp.path().to_path_buf(),
            steps: vec![],
            expected_url: None,
            generated_files: vec![],
            warnings: vec![],
        };

        let step = PlanStep {
            index: 1,
            id: "create_workflow".to_string(),
            description: "Create GitHub workflow".to_string(),
            command: None,
            optional: true,
            requires_confirm: false,
        };

        let outcome = execute_step(&step, &plan, false).unwrap();
        assert!(outcome.success);
        assert!(outcome.files_created.is_empty());
    }

    #[test]
    fn execute_step_create_netlify_toml_skipped() {
        let temp = tempfile::tempdir().unwrap();
        let plan = DeploymentPlan {
            provider: HostingProvider::Netlify,
            bundle_path: temp.path().to_path_buf(),
            steps: vec![],
            expected_url: None,
            generated_files: vec![],
            warnings: vec![],
        };

        let step = PlanStep {
            index: 1,
            id: "create_netlify_toml".to_string(),
            description: "Create netlify.toml".to_string(),
            command: None,
            optional: true,
            requires_confirm: false,
        };

        let outcome = execute_step(&step, &plan, false).unwrap();
        assert!(outcome.success);
    }

    // ── execute_plan: empty plan ──────────────────────────────────────

    #[test]
    fn execute_plan_empty_steps() {
        let temp = tempfile::tempdir().unwrap();
        let plan = DeploymentPlan {
            provider: HostingProvider::Custom,
            bundle_path: temp.path().to_path_buf(),
            steps: vec![],
            expected_url: None,
            generated_files: vec![],
            warnings: vec![],
        };

        let config = ExecutorConfig::default();
        let result = execute_plan(&plan, &config).unwrap();
        assert!(result.success);
        assert!(result.steps.is_empty());
        assert!(result.total_duration_ms < 1000); // nearly instant
    }

    // ── execute_plan: timing is reasonable ────────────────────────────

    #[test]
    fn execute_plan_records_timing() {
        let temp = tempfile::tempdir().unwrap();
        let plan = DeploymentPlan {
            provider: HostingProvider::Custom,
            bundle_path: temp.path().to_path_buf(),
            steps: vec![PlanStep {
                index: 1,
                id: "manual_deploy".to_string(),
                description: "Info".to_string(),
                command: None,
                optional: false,
                requires_confirm: false,
            }],
            expected_url: None,
            generated_files: vec![],
            warnings: vec![],
        };

        let result = execute_plan(&plan, &ExecutorConfig::default()).unwrap();
        assert!(result.total_duration_ms < 5000); // should be nearly instant
        assert_eq!(result.steps.len(), 1);
        // Step duration should be recorded
        assert!(result.steps[0].duration_ms < 5000);
    }

    // ── execute_step: verbose output ──────────────────────────────────

    #[test]
    fn execute_step_verbose_creates_directory() {
        let temp = tempfile::tempdir().unwrap();
        let new_dir = temp.path().join("verbose_dir");
        let plan = DeploymentPlan {
            provider: HostingProvider::Custom,
            bundle_path: temp.path().to_path_buf(),
            steps: vec![],
            expected_url: None,
            generated_files: vec![],
            warnings: vec![],
        };

        let step = PlanStep {
            index: 1,
            id: "create_output_dir".to_string(),
            description: "Create directory".to_string(),
            command: Some(format!("mkdir -p {}", new_dir.display())),
            optional: false,
            requires_confirm: false,
        };

        // Verbose flag should not change behavior, just add output
        let outcome = execute_step(&step, &plan, true).unwrap();
        assert!(outcome.success);
        assert!(new_dir.exists());
    }
}
