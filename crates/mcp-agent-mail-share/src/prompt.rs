//! Native interactive wizard prompt flow.
//!
//! Provides TTY-based prompts for the share deployment wizard, replacing
//! the legacy Python implementation with deterministic, testable Rust code.
//!
//! # Design Rationale
//!
//! The prompt flow operates in two modes:
//!
//! - **Interactive**: Prompts user for input with validation and guidance
//! - **Non-interactive**: Validates all required inputs are provided via flags
//!
//! Both modes produce identical `WizardInputs` for downstream plan generation.
//!
//! # Non-Interactive Safeguards
//!
//! When stdin is not a TTY or `--non-interactive` is set:
//! - Required parameters must be provided via flags
//! - Missing parameters cause immediate failure with clear error messages
//! - No prompts are issued; the wizard fails fast

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::detection::detect_environment;
use crate::planner::{format_plan_human, generate_plan, validate_inputs};
use crate::wizard::{
    DeploymentPlan, DetectedEnvironment, HostingProvider, WIZARD_VERSION, WizardError,
    WizardErrorCode, WizardInputs, WizardJsonOutput, WizardMetadata, WizardMode, WizardResult,
};

/// Interactive wizard configuration.
#[derive(Debug, Clone, Default)]
pub struct WizardConfig {
    /// Pre-filled inputs from CLI flags.
    pub inputs: WizardInputs,
    /// Force non-interactive mode (fail if prompts would be needed).
    pub non_interactive: bool,
    /// Emit JSON output instead of human-readable.
    pub json_output: bool,
    /// Skip environment detection (use only explicit inputs).
    pub skip_detection: bool,
}

/// Result of running the interactive wizard.
#[derive(Debug)]
pub struct WizardOutcome {
    /// The final wizard inputs after prompts.
    pub inputs: WizardInputs,
    /// The detected environment.
    pub environment: DetectedEnvironment,
    /// The generated deployment plan.
    pub plan: DeploymentPlan,
    /// Whether the user confirmed execution.
    pub confirmed: bool,
}

/// Run the interactive wizard flow.
///
/// This is the main entry point for the native wizard. It:
/// 1. Detects environment and validates pre-filled inputs
/// 2. Prompts for missing required inputs (if interactive)
/// 3. Generates a deployment plan
/// 4. Displays the plan and asks for confirmation
///
/// # Errors
///
/// Returns `WizardError` if:
/// - Non-interactive mode and required inputs are missing
/// - Input validation fails
/// - Plan generation fails
/// - User cancels the operation
pub fn run_interactive_wizard(config: WizardConfig) -> Result<WizardOutcome, WizardError> {
    let is_tty = io::stdin().is_terminal();
    let interactive = is_tty && !config.non_interactive;

    // Detect environment
    let cwd = std::env::current_dir().map_err(|e| {
        WizardError::new(
            WizardErrorCode::InternalError,
            format!("Failed to get current directory: {e}"),
        )
    })?;

    let env = if config.skip_detection {
        DetectedEnvironment {
            cwd: cwd.clone(),
            ..Default::default()
        }
    } else {
        detect_environment(config.inputs.bundle_path.as_deref(), &cwd)
    };

    // Build inputs by merging CLI flags with prompted values
    let inputs = if interactive {
        prompt_missing_inputs(config.inputs, &env)?
    } else {
        validate_non_interactive(config.inputs, &env)?
    };

    // Validate inputs
    validate_inputs(&inputs)?;

    // Generate plan
    let plan = generate_plan(&inputs, Some(env.clone()))?;

    // Show plan and confirm
    let confirmed = if interactive && !inputs.skip_confirm {
        show_plan_and_confirm(&plan)?
    } else {
        // Non-interactive or skip_confirm: auto-confirm unless dry-run
        !inputs.dry_run
    };

    Ok(WizardOutcome {
        inputs,
        environment: env,
        plan,
        confirmed,
    })
}

/// Prompt for missing required inputs in interactive mode.
fn prompt_missing_inputs(
    mut inputs: WizardInputs,
    env: &DetectedEnvironment,
) -> Result<WizardInputs, WizardError> {
    // Show welcome banner
    eprintln!();
    eprintln!("╭─────────────────────────────────────────────────────╮");
    eprintln!(
        "│       Agent Mail Deployment Wizard v{}        │",
        WIZARD_VERSION
    );
    eprintln!("╰─────────────────────────────────────────────────────╯");
    eprintln!();

    // Show detected environment hints
    if !env.signals.is_empty() {
        eprintln!("Detected environment:");
        for signal in env.signals.iter().take(3) {
            eprintln!("  • {}", signal.detail);
        }
        eprintln!();
    }

    // Prompt for provider if not specified
    if inputs.provider.is_none() {
        inputs.provider = Some(prompt_provider_selection(env)?);
    }

    let provider = inputs.provider.unwrap();

    // Prompt for bundle path if not specified
    if inputs.bundle_path.is_none() {
        inputs.bundle_path = Some(prompt_bundle_path(env)?);
    }

    // Prompt for provider-specific options
    prompt_provider_options(provider, &mut inputs, env)?;

    Ok(inputs)
}

/// Validate inputs for non-interactive mode.
///
/// Ensures all required parameters are present when prompts cannot be issued.
fn validate_non_interactive(
    inputs: WizardInputs,
    env: &DetectedEnvironment,
) -> Result<WizardInputs, WizardError> {
    // Provider is required
    if inputs.provider.is_none() && env.recommended_provider.is_none() {
        return Err(WizardError::new(
            WizardErrorCode::MissingRequiredOption,
            "Provider not specified and could not be auto-detected",
        )
        .with_hint("Use --provider to specify the hosting provider"));
    }

    // Bundle path is required (or must be auto-detectable)
    if inputs.bundle_path.is_none() && env.existing_bundle.is_none() {
        return Err(WizardError::new(
            WizardErrorCode::MissingRequiredOption,
            "Bundle path not specified and no bundle found in current directory",
        )
        .with_hint("Use --bundle to specify the bundle path"));
    }

    // Fill in defaults from environment
    let mut inputs = inputs;
    if inputs.provider.is_none() {
        inputs.provider = env.recommended_provider;
    }
    if inputs.bundle_path.is_none() {
        inputs.bundle_path = env.existing_bundle.clone();
    }

    // Validate provider-specific requirements
    let provider = inputs.provider.unwrap();
    validate_provider_requirements(provider, &inputs)?;

    Ok(inputs)
}

/// Validate provider-specific required options.
fn validate_provider_requirements(
    provider: HostingProvider,
    inputs: &WizardInputs,
) -> Result<(), WizardError> {
    match provider {
        HostingProvider::GithubPages => {
            if inputs.github_repo.is_none() {
                return Err(WizardError::new(
                    WizardErrorCode::MissingRequiredOption,
                    "GitHub repository not specified for GitHub Pages deployment",
                )
                .with_hint("Use --github-repo owner/repo to specify the repository"));
            }
        }
        HostingProvider::CloudflarePages => {
            if inputs.cloudflare_project.is_none() {
                return Err(WizardError::new(
                    WizardErrorCode::MissingRequiredOption,
                    "Cloudflare project name not specified",
                )
                .with_hint("Use --cloudflare-project to specify the project name"));
            }
        }
        HostingProvider::Netlify => {
            if inputs.netlify_site.is_none() {
                return Err(WizardError::new(
                    WizardErrorCode::MissingRequiredOption,
                    "Netlify site not specified",
                )
                .with_hint("Use --netlify-site to specify the site name or ID"));
            }
        }
        HostingProvider::S3 => {
            if inputs.s3_bucket.is_none() {
                return Err(WizardError::new(
                    WizardErrorCode::MissingRequiredOption,
                    "S3 bucket not specified",
                )
                .with_hint("Use --s3-bucket to specify the bucket name"));
            }
        }
        HostingProvider::Custom => {
            // Custom provider has no required options
        }
    }
    Ok(())
}

// ── Provider Selection ──────────────────────────────────────────────────

/// Prompt user to select a hosting provider.
fn prompt_provider_selection(env: &DetectedEnvironment) -> Result<HostingProvider, WizardError> {
    eprintln!("Select hosting provider:");
    eprintln!();

    let providers = [
        HostingProvider::GithubPages,
        HostingProvider::CloudflarePages,
        HostingProvider::Netlify,
        HostingProvider::S3,
        HostingProvider::Custom,
    ];

    // Determine recommended provider
    let recommended = env.recommended_provider;

    for (i, provider) in providers.iter().enumerate() {
        let marker = if Some(*provider) == recommended {
            " (recommended)"
        } else {
            ""
        };
        eprintln!(
            "  [{}] {}{} - {}",
            i + 1,
            provider.display_name(),
            marker,
            provider.description()
        );
    }
    eprintln!();

    let default = recommended
        .and_then(|r| providers.iter().position(|p| *p == r))
        .map(|i| i + 1);

    loop {
        let prompt = if let Some(d) = default {
            format!("Enter choice [{}]: ", d)
        } else {
            "Enter choice: ".to_string()
        };

        let input = prompt_line(&prompt)?;
        let trimmed = input.trim();

        // Handle default
        if trimmed.is_empty() {
            if let Some(d) = default {
                return Ok(providers[d - 1]);
            }
            eprintln!("Please enter a number 1-5.");
            continue;
        }

        // Try parsing as number
        if let Ok(n) = trimmed.parse::<usize>() {
            if n >= 1 && n <= providers.len() {
                return Ok(providers[n - 1]);
            }
            eprintln!("Please enter a number 1-5.");
            continue;
        }

        // Try parsing as provider name
        if let Some(provider) = HostingProvider::parse(trimmed) {
            return Ok(provider);
        }

        eprintln!("Invalid choice. Enter a number 1-5 or provider name.");
    }
}

// ── Bundle Path ─────────────────────────────────────────────────────────

/// Prompt user for bundle path.
fn prompt_bundle_path(env: &DetectedEnvironment) -> Result<PathBuf, WizardError> {
    // Check for existing bundle
    let default = env
        .existing_bundle
        .as_ref()
        .map(|p| p.display().to_string());

    if let Some(ref path) = default {
        eprintln!("Found bundle at: {path}");
    }

    loop {
        let prompt = if let Some(ref d) = default {
            format!("Bundle path [{}]: ", d)
        } else {
            "Bundle path: ".to_string()
        };

        let input = prompt_line(&prompt)?;
        let trimmed = input.trim();

        // Handle default
        if trimmed.is_empty() {
            if let Some(ref d) = default {
                let path = PathBuf::from(d);
                if validate_bundle_path(&path)? {
                    return Ok(path);
                }
                continue;
            }
            eprintln!("Please enter a bundle path.");
            continue;
        }

        let path = PathBuf::from(trimmed);
        if validate_bundle_path(&path)? {
            return Ok(path);
        }
    }
}

/// Validate that a path is a valid bundle directory.
fn validate_bundle_path(path: &Path) -> Result<bool, WizardError> {
    if !path.exists() {
        eprintln!("Path does not exist: {}", path.display());
        return Ok(false);
    }
    if !path.is_dir() {
        eprintln!("Path is not a directory: {}", path.display());
        return Ok(false);
    }
    let manifest = path.join("manifest.json");
    if !manifest.exists() {
        eprintln!(
            "Not a valid bundle (missing manifest.json): {}",
            path.display()
        );
        return Ok(false);
    }
    Ok(true)
}

// ── Provider-Specific Options ───────────────────────────────────────────

/// Prompt for provider-specific options.
fn prompt_provider_options(
    provider: HostingProvider,
    inputs: &mut WizardInputs,
    env: &DetectedEnvironment,
) -> Result<(), WizardError> {
    match provider {
        HostingProvider::GithubPages => {
            prompt_github_options(inputs, env)?;
        }
        HostingProvider::CloudflarePages => {
            prompt_cloudflare_options(inputs)?;
        }
        HostingProvider::Netlify => {
            prompt_netlify_options(inputs)?;
        }
        HostingProvider::S3 => {
            prompt_s3_options(inputs)?;
        }
        HostingProvider::Custom => {
            prompt_custom_options(inputs)?;
        }
    }
    Ok(())
}

/// Prompt for GitHub Pages options.
fn prompt_github_options(
    inputs: &mut WizardInputs,
    env: &DetectedEnvironment,
) -> Result<(), WizardError> {
    eprintln!();
    eprintln!("GitHub Pages Configuration");
    eprintln!("───────────────────────────");

    // GitHub repo
    if inputs.github_repo.is_none() {
        let default = env.github_repo.clone();
        let prompt = if let Some(ref d) = default {
            format!("Repository (owner/repo) [{}]: ", d)
        } else {
            "Repository (owner/repo): ".to_string()
        };

        loop {
            let input = prompt_line(&prompt)?;
            let trimmed = input.trim();

            if trimmed.is_empty() {
                if let Some(ref d) = default {
                    inputs.github_repo = Some(d.clone());
                    break;
                }
                eprintln!("Repository is required.");
                continue;
            }

            // Validate format
            if trimmed.contains('/') && trimmed.matches('/').count() == 1 {
                inputs.github_repo = Some(trimmed.to_string());
                break;
            }
            eprintln!("Invalid format. Use owner/repo (e.g., myuser/myrepo).");
        }
    }

    // GitHub branch
    let default_branch = inputs
        .github_branch
        .clone()
        .unwrap_or_else(|| "gh-pages".to_string());
    let prompt = format!("Branch [{}]: ", default_branch);
    let input = prompt_line(&prompt)?;
    let trimmed = input.trim();
    inputs.github_branch = Some(if trimmed.is_empty() {
        default_branch
    } else {
        trimmed.to_string()
    });

    Ok(())
}

/// Prompt for Cloudflare Pages options.
fn prompt_cloudflare_options(inputs: &mut WizardInputs) -> Result<(), WizardError> {
    eprintln!();
    eprintln!("Cloudflare Pages Configuration");
    eprintln!("───────────────────────────────");

    if inputs.cloudflare_project.is_none() {
        loop {
            let input = prompt_line("Project name: ")?;
            let trimmed = input.trim();

            if trimmed.is_empty() {
                eprintln!("Project name is required.");
                continue;
            }

            inputs.cloudflare_project = Some(trimmed.to_string());
            break;
        }
    }

    Ok(())
}

/// Prompt for Netlify options.
fn prompt_netlify_options(inputs: &mut WizardInputs) -> Result<(), WizardError> {
    eprintln!();
    eprintln!("Netlify Configuration");
    eprintln!("─────────────────────");

    if inputs.netlify_site.is_none() {
        loop {
            let input = prompt_line("Site name or ID: ")?;
            let trimmed = input.trim();

            if trimmed.is_empty() {
                eprintln!("Site name or ID is required.");
                continue;
            }

            inputs.netlify_site = Some(trimmed.to_string());
            break;
        }
    }

    Ok(())
}

/// Prompt for S3 options.
fn prompt_s3_options(inputs: &mut WizardInputs) -> Result<(), WizardError> {
    eprintln!();
    eprintln!("Amazon S3 Configuration");
    eprintln!("───────────────────────");

    if inputs.s3_bucket.is_none() {
        loop {
            let input = prompt_line("S3 bucket name: ")?;
            let trimmed = input.trim();

            if trimmed.is_empty() {
                eprintln!("Bucket name is required.");
                continue;
            }

            inputs.s3_bucket = Some(trimmed.to_string());
            break;
        }
    }

    // CloudFront distribution (optional)
    if inputs.cloudfront_id.is_none() {
        let input = prompt_line("CloudFront distribution ID (optional): ")?;
        let trimmed = input.trim();
        if !trimmed.is_empty() {
            inputs.cloudfront_id = Some(trimmed.to_string());
        }
    }

    Ok(())
}

/// Prompt for custom deployment options.
fn prompt_custom_options(inputs: &mut WizardInputs) -> Result<(), WizardError> {
    eprintln!();
    eprintln!("Custom Deployment Configuration");
    eprintln!("────────────────────────────────");

    // Base URL (optional)
    if inputs.base_url.is_none() {
        let input = prompt_line("Base URL (optional, e.g., https://example.com/mail): ")?;
        let trimmed = input.trim();
        if !trimmed.is_empty() {
            inputs.base_url = Some(trimmed.to_string());
        }
    }

    Ok(())
}

// ── Plan Confirmation ───────────────────────────────────────────────────

/// Display plan and ask for confirmation.
fn show_plan_and_confirm(plan: &DeploymentPlan) -> Result<bool, WizardError> {
    eprintln!();
    eprintln!("{}", format_plan_human(plan));

    // Show warnings
    if !plan.warnings.is_empty() {
        eprintln!();
        eprintln!("Warnings:");
        for warning in &plan.warnings {
            eprintln!("  ⚠ {warning}");
        }
    }

    eprintln!();

    loop {
        let input = prompt_line("Proceed with deployment? [Y/n]: ")?;
        let trimmed = input.trim().to_ascii_lowercase();

        if trimmed.is_empty() || trimmed == "y" || trimmed == "yes" {
            return Ok(true);
        }
        if trimmed == "n" || trimmed == "no" {
            eprintln!("Deployment cancelled.");
            return Ok(false);
        }
        eprintln!("Please answer y or n.");
    }
}

// ── JSON Output ─────────────────────────────────────────────────────────

/// Format wizard result as JSON output.
pub fn format_json_output(
    outcome: &WizardOutcome,
    executed: bool,
    error: Option<&WizardError>,
) -> String {
    let output = if let Some(err) = error {
        WizardJsonOutput::failure(err.clone(), outcome.inputs.bundle_path.clone())
            .with_environment(outcome.environment.clone())
            .with_plan(outcome.plan.clone())
    } else {
        let result = WizardResult {
            success: executed,
            provider: outcome.plan.provider,
            bundle_path: outcome.plan.bundle_path.clone(),
            deployed_url: outcome.plan.expected_url.clone(),
            steps: vec![],
            total_duration_ms: 0,
            error: None,
            error_code: None,
            metadata: WizardMetadata {
                version: WIZARD_VERSION.to_string(),
                timestamp: chrono::Utc::now().to_rfc3339(),
                mode: if outcome.confirmed {
                    WizardMode::Interactive
                } else {
                    WizardMode::NonInteractive
                },
                dry_run: outcome.inputs.dry_run,
            },
        };
        WizardJsonOutput::success(result)
            .with_environment(outcome.environment.clone())
            .with_plan(outcome.plan.clone())
    };

    serde_json::to_string_pretty(&output).unwrap_or_else(|_| "{}".to_string())
}

// ── Prompt Helpers ──────────────────────────────────────────────────────

/// Read a line from stdin with prompt.
fn prompt_line(prompt: &str) -> Result<String, WizardError> {
    eprint!("{prompt}");
    io::stderr().flush().ok();

    let stdin = io::stdin();
    let mut line = String::new();
    stdin.lock().read_line(&mut line).map_err(|e| {
        WizardError::new(
            WizardErrorCode::InternalError,
            format!("Failed to read input: {e}"),
        )
    })?;

    Ok(line.trim_end().to_string())
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_non_interactive_requires_provider() {
        let inputs = WizardInputs::default();
        let env = DetectedEnvironment::default();

        let result = validate_non_interactive(inputs, &env);
        assert!(result.is_err());

        let err = result.unwrap_err();
        assert_eq!(err.code, WizardErrorCode::MissingRequiredOption);
        assert!(err.message.contains("Provider"));
    }

    #[test]
    fn validate_non_interactive_uses_detected_provider() {
        let inputs = WizardInputs {
            bundle_path: Some(PathBuf::from("/tmp/bundle")),
            github_repo: Some("owner/repo".to_string()),
            ..Default::default()
        };
        let env = DetectedEnvironment {
            recommended_provider: Some(HostingProvider::GithubPages),
            ..Default::default()
        };

        let result = validate_non_interactive(inputs, &env);
        // Will fail on bundle validation, but not on provider
        assert!(result.is_ok() || !result.unwrap_err().message.contains("Provider"));
    }

    #[test]
    fn validate_provider_requirements_github() {
        let inputs = WizardInputs::default();
        let result = validate_provider_requirements(HostingProvider::GithubPages, &inputs);
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("GitHub repository"));
    }

    #[test]
    fn validate_non_interactive_requires_github_repo() {
        let inputs = WizardInputs {
            provider: Some(HostingProvider::GithubPages),
            bundle_path: Some(PathBuf::from("/tmp/bundle")),
            ..Default::default()
        };
        let env = DetectedEnvironment::default();

        let err = validate_non_interactive(inputs, &env)
            .expect_err("github provider should require --github-repo");
        assert_eq!(err.code, WizardErrorCode::MissingRequiredOption);
        assert!(err.message.contains("GitHub repository"));
    }

    #[test]
    fn validate_non_interactive_requires_cloudflare_project() {
        let inputs = WizardInputs {
            provider: Some(HostingProvider::CloudflarePages),
            bundle_path: Some(PathBuf::from("/tmp/bundle")),
            ..Default::default()
        };
        let env = DetectedEnvironment::default();

        let err = validate_non_interactive(inputs, &env)
            .expect_err("cloudflare provider should require --cloudflare-project");
        assert_eq!(err.code, WizardErrorCode::MissingRequiredOption);
        assert!(err.message.contains("Cloudflare project"));
    }

    #[test]
    fn validate_non_interactive_requires_netlify_site() {
        let inputs = WizardInputs {
            provider: Some(HostingProvider::Netlify),
            bundle_path: Some(PathBuf::from("/tmp/bundle")),
            ..Default::default()
        };
        let env = DetectedEnvironment::default();

        let err = validate_non_interactive(inputs, &env)
            .expect_err("netlify provider should require --netlify-site");
        assert_eq!(err.code, WizardErrorCode::MissingRequiredOption);
        assert!(err.message.contains("Netlify site"));
    }

    #[test]
    fn validate_non_interactive_requires_s3_bucket() {
        let inputs = WizardInputs {
            provider: Some(HostingProvider::S3),
            bundle_path: Some(PathBuf::from("/tmp/bundle")),
            ..Default::default()
        };
        let env = DetectedEnvironment::default();

        let err = validate_non_interactive(inputs, &env)
            .expect_err("s3 provider should require --s3-bucket");
        assert_eq!(err.code, WizardErrorCode::MissingRequiredOption);
        assert!(err.message.contains("S3 bucket"));
    }

    #[test]
    fn validate_non_interactive_accepts_detected_bundle_path() {
        let detected_bundle = PathBuf::from("/tmp/detected-bundle");
        let inputs = WizardInputs {
            provider: Some(HostingProvider::Custom),
            bundle_path: None,
            ..Default::default()
        };
        let env = DetectedEnvironment {
            existing_bundle: Some(detected_bundle.clone()),
            ..Default::default()
        };

        let resolved = validate_non_interactive(inputs, &env)
            .expect("detected bundle should satisfy non-interactive validation");
        assert_eq!(resolved.bundle_path, Some(detected_bundle));
    }

    #[test]
    fn validate_provider_requirements_custom_has_none() {
        let inputs = WizardInputs::default();
        let result = validate_provider_requirements(HostingProvider::Custom, &inputs);
        assert!(result.is_ok());
    }

    #[test]
    fn validate_bundle_path_rejects_nonexistent() {
        let result = validate_bundle_path(Path::new("/nonexistent/path/12345"));
        assert!(result.is_ok());
        assert!(!result.unwrap());
    }

    #[test]
    fn validate_bundle_path_rejects_file_not_dir() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("not_a_dir.txt");
        std::fs::write(&file_path, "data").unwrap();

        let result = validate_bundle_path(&file_path);
        assert!(result.is_ok());
        assert!(!result.unwrap(), "should reject file path");
    }

    #[test]
    fn validate_bundle_path_rejects_dir_without_manifest() {
        let dir = tempfile::tempdir().unwrap();
        // Directory exists but no manifest.json
        let result = validate_bundle_path(dir.path());
        assert!(result.is_ok());
        assert!(!result.unwrap(), "should reject dir without manifest.json");
    }

    #[test]
    fn validate_bundle_path_accepts_valid_bundle() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("manifest.json"), "{}").unwrap();

        let result = validate_bundle_path(dir.path());
        assert!(result.is_ok());
        assert!(result.unwrap(), "should accept dir with manifest.json");
    }

    #[test]
    fn validate_provider_requirements_cloudflare() {
        let inputs = WizardInputs::default();
        let result = validate_provider_requirements(HostingProvider::CloudflarePages, &inputs);
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("Cloudflare project"));
    }

    #[test]
    fn validate_provider_requirements_netlify() {
        let inputs = WizardInputs::default();
        let result = validate_provider_requirements(HostingProvider::Netlify, &inputs);
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("Netlify site"));
    }

    #[test]
    fn validate_provider_requirements_s3() {
        let inputs = WizardInputs::default();
        let result = validate_provider_requirements(HostingProvider::S3, &inputs);
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("S3 bucket"));
    }

    #[test]
    fn validate_non_interactive_fills_defaults_from_env() {
        let detected_bundle = tempfile::tempdir().unwrap();
        std::fs::write(detected_bundle.path().join("manifest.json"), "{}").unwrap();

        let inputs = WizardInputs {
            provider: Some(HostingProvider::Custom),
            bundle_path: None,
            ..Default::default()
        };
        let env = DetectedEnvironment {
            existing_bundle: Some(detected_bundle.path().to_path_buf()),
            recommended_provider: Some(HostingProvider::Custom),
            ..Default::default()
        };

        let resolved = validate_non_interactive(inputs, &env).unwrap();
        assert_eq!(
            resolved.bundle_path,
            Some(detected_bundle.path().to_path_buf())
        );
        assert_eq!(resolved.provider, Some(HostingProvider::Custom));
    }

    #[test]
    fn wizard_config_default() {
        let config = WizardConfig::default();
        assert!(!config.non_interactive);
        assert!(!config.json_output);
        assert!(!config.skip_detection);
    }
}
