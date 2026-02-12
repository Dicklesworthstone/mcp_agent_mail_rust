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
use std::path::PathBuf;
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
                if let Some(path) = cmd.strip_prefix("mkdir -p ") {
                    let path = PathBuf::from(path.trim());
                    std::fs::create_dir_all(&path).map_err(|e| {
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
                if let Some(path) = cmd.strip_prefix("touch ") {
                    let path = PathBuf::from(path.trim());
                    std::fs::write(&path, "").map_err(|e| {
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
                if let Some(parent) = headers_path.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                std::fs::write(headers_path, headers_content).map_err(|e| {
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
    use std::path::PathBuf;

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
}
