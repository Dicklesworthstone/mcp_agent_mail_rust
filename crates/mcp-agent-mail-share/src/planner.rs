//! Deterministic wizard plan-generation engine.
//!
//! Converts detected environment and user intent into an ordered, explicit
//! action plan. The plan can be executed in interactive or non-interactive
//! mode, and supports dry-run for preview.
//!
//! # Design Rationale
//!
//! Plans are deterministic: identical inputs always produce identical plans.
//! This enables:
//! - Reliable testing via snapshot comparison
//! - Dry-run preview before execution
//! - JSON output for CI/CD integration
//! - Human-readable explanations for interactive mode

use std::path::{Path, PathBuf};

use crate::detection::detect_environment;
use crate::wizard::{
    DeploymentPlan, DetectedEnvironment, HostingProvider, PlanStep, WizardError, WizardErrorCode,
    WizardInputs,
};

/// Result type for plan generation.
pub type PlanResult<T> = Result<T, WizardError>;

/// Generate a deployment plan from inputs and environment.
///
/// This is the main entry point for plan generation. It:
/// 1. Validates inputs
/// 2. Detects environment if not provided
/// 3. Selects the target provider
/// 4. Generates provider-specific steps
///
/// # Arguments
///
/// * `inputs` - User-provided wizard inputs
/// * `env` - Optional pre-detected environment (will detect if None)
///
/// # Returns
///
/// A `DeploymentPlan` with ordered steps, or an error if planning fails.
pub fn generate_plan(
    inputs: &WizardInputs,
    env: Option<DetectedEnvironment>,
) -> PlanResult<DeploymentPlan> {
    // Validate and resolve bundle path
    let bundle_path = resolve_bundle_path(inputs)?;

    // Detect environment if not provided
    let cwd = std::env::current_dir().map_err(|e| {
        WizardError::new(
            WizardErrorCode::InternalError,
            format!("Failed to get cwd: {e}"),
        )
    })?;
    let env = env.unwrap_or_else(|| detect_environment(Some(&bundle_path), &cwd));

    // Determine target provider
    let provider = resolve_provider(inputs, &env)?;

    // Generate provider-specific plan
    let plan = match provider {
        HostingProvider::GithubPages => generate_github_pages_plan(inputs, &env, &bundle_path)?,
        HostingProvider::CloudflarePages => {
            generate_cloudflare_pages_plan(inputs, &env, &bundle_path)?
        }
        HostingProvider::Netlify => generate_netlify_plan(inputs, &env, &bundle_path)?,
        HostingProvider::S3 => generate_s3_plan(inputs, &env, &bundle_path)?,
        HostingProvider::Custom => generate_custom_plan(inputs, &env, &bundle_path)?,
    };

    Ok(plan)
}

/// Validate inputs before plan generation.
pub fn validate_inputs(inputs: &WizardInputs) -> PlanResult<()> {
    // Check bundle path if provided
    if let Some(ref path) = inputs.bundle_path {
        if !path.exists() {
            return Err(WizardError::new(
                WizardErrorCode::BundleNotFound,
                format!("Bundle path does not exist: {}", path.display()),
            )
            .with_hint("Run 'am share export' to create a bundle first"));
        }
        if !path.is_dir() {
            return Err(WizardError::new(
                WizardErrorCode::BundleInvalid,
                format!("Bundle path is not a directory: {}", path.display()),
            ));
        }
        let manifest = path.join("manifest.json");
        if !manifest.exists() {
            return Err(WizardError::new(
                WizardErrorCode::BundleInvalid,
                format!("Bundle is missing manifest.json: {}", path.display()),
            )
            .with_hint("Ensure the bundle was created with 'am share export'"));
        }
    }

    // Validate provider-specific options
    if let Some(provider) = inputs.provider {
        validate_provider_options(provider, inputs)?;
    }

    Ok(())
}

// ── Provider Resolution ─────────────────────────────────────────────────

fn resolve_bundle_path(inputs: &WizardInputs) -> PlanResult<PathBuf> {
    if let Some(ref path) = inputs.bundle_path {
        return Ok(path.clone());
    }

    // Try default locations
    let cwd = std::env::current_dir().map_err(|e| {
        WizardError::new(
            WizardErrorCode::InternalError,
            format!("Failed to get cwd: {e}"),
        )
    })?;

    // Check cwd/bundle
    let default_bundle = cwd.join("bundle");
    if default_bundle.is_dir() && default_bundle.join("manifest.json").exists() {
        return Ok(default_bundle);
    }

    // Check cwd/agent-mail-bundle
    let alt_bundle = cwd.join("agent-mail-bundle");
    if alt_bundle.is_dir() && alt_bundle.join("manifest.json").exists() {
        return Ok(alt_bundle);
    }

    Err(WizardError::new(
        WizardErrorCode::BundleNotFound,
        "No bundle path specified and no default bundle found",
    )
    .with_hint("Specify --bundle or run 'am share export' in the current directory"))
}

fn resolve_provider(
    inputs: &WizardInputs,
    env: &DetectedEnvironment,
) -> PlanResult<HostingProvider> {
    // User explicitly specified provider
    if let Some(provider) = inputs.provider {
        return Ok(provider);
    }

    // Use detected recommendation
    if let Some(provider) = env.recommended_provider {
        return Ok(provider);
    }

    // Default to GitHub Pages if we have GitHub context
    if env.github_repo.is_some() || env.github_env {
        return Ok(HostingProvider::GithubPages);
    }

    // No clear choice - require explicit selection
    Err(WizardError::new(
        WizardErrorCode::MissingRequiredOption,
        "Could not determine hosting provider",
    )
    .with_context("No provider specified and no strong detection signals")
    .with_hint("Specify --provider (github, cloudflare, netlify, s3, custom)"))
}

fn validate_provider_options(provider: HostingProvider, inputs: &WizardInputs) -> PlanResult<()> {
    match provider {
        HostingProvider::GithubPages => {
            // GitHub repo is helpful but can be auto-detected
        }
        HostingProvider::CloudflarePages => {
            // Project name can be prompted
        }
        HostingProvider::Netlify => {
            // Site ID can be prompted
        }
        HostingProvider::S3 => {
            // S3 bucket is required
            if inputs.s3_bucket.is_none() && inputs.skip_confirm {
                return Err(WizardError::new(
                    WizardErrorCode::MissingRequiredOption,
                    "S3 bucket name required in non-interactive mode",
                )
                .with_hint("Specify --s3-bucket"));
            }
        }
        HostingProvider::Custom => {
            // No specific requirements
        }
    }
    Ok(())
}

// ── Provider-Specific Plan Generators ───────────────────────────────────

fn generate_github_pages_plan(
    inputs: &WizardInputs,
    env: &DetectedEnvironment,
    bundle_path: &Path,
) -> PlanResult<DeploymentPlan> {
    let mut steps = Vec::new();
    let mut generated_files = Vec::new();
    let mut warnings = Vec::new();

    // Determine output directory
    let output_dir = inputs
        .output_dir
        .clone()
        .unwrap_or_else(|| bundle_path.parent().unwrap_or(bundle_path).join("docs"));

    // Step 1: Create output directory
    steps.push(PlanStep {
        index: 1,
        id: "create_output_dir".to_string(),
        description: format!("Create output directory: {}", output_dir.display()),
        command: Some(format!("mkdir -p {}", output_dir.display())),
        optional: false,
        requires_confirm: false,
    });

    // Step 2: Copy bundle to output
    steps.push(PlanStep {
        index: 2,
        id: "copy_bundle".to_string(),
        description: format!(
            "Copy bundle from {} to {}",
            bundle_path.display(),
            output_dir.display()
        ),
        command: Some(format!(
            "cp -r {}/* {}",
            bundle_path.display(),
            output_dir.display()
        )),
        optional: false,
        requires_confirm: false,
    });

    // Step 3: Create .nojekyll
    let nojekyll = output_dir.join(".nojekyll");
    steps.push(PlanStep {
        index: 3,
        id: "create_nojekyll".to_string(),
        description: "Create .nojekyll file (required for GitHub Pages)".to_string(),
        command: Some(format!("touch {}", nojekyll.display())),
        optional: false,
        requires_confirm: false,
    });
    generated_files.push(nojekyll);

    // Step 4: Generate _headers file
    let headers_file = output_dir.join("_headers");
    steps.push(PlanStep {
        index: 4,
        id: "create_headers".to_string(),
        description: "Create _headers file for COOP/COEP headers".to_string(),
        command: None,
        optional: false,
        requires_confirm: false,
    });
    generated_files.push(headers_file);

    // Step 5: Generate GitHub Actions workflow (optional)
    let workflow_path = PathBuf::from(".github/workflows/deploy-pages.yml");
    steps.push(PlanStep {
        index: 5,
        id: "create_workflow".to_string(),
        description: "Generate GitHub Actions workflow for Pages deployment".to_string(),
        command: None,
        optional: true,
        requires_confirm: true,
    });
    generated_files.push(workflow_path);

    // Step 6: Git add and commit
    steps.push(PlanStep {
        index: 6,
        id: "git_commit".to_string(),
        description: "Stage and commit changes".to_string(),
        command: Some(
            "git add . && git commit -m 'Deploy Agent Mail bundle to GitHub Pages'".to_string(),
        ),
        optional: false,
        requires_confirm: true,
    });

    // Step 7: Git push
    let branch = inputs.github_branch.as_deref().unwrap_or("gh-pages");
    steps.push(PlanStep {
        index: 7,
        id: "git_push".to_string(),
        description: format!("Push to {} branch", branch),
        command: Some(format!("git push origin {branch}")),
        optional: false,
        requires_confirm: true,
    });

    // Calculate expected URL
    let expected_url = if let Some(ref repo) = env.github_repo {
        let parts: Vec<&str> = repo.split('/').collect();
        if parts.len() == 2 {
            Some(format!("https://{}.github.io/{}", parts[0], parts[1]))
        } else {
            None
        }
    } else {
        inputs.base_url.clone()
    };

    // Add warnings
    if !env.is_git_repo {
        warnings.push("Not inside a Git repository - git commands will fail".to_string());
    }
    if env.github_repo.is_none() && inputs.github_repo.is_none() {
        warnings
            .push("GitHub repository not detected - URL prediction may be inaccurate".to_string());
    }

    Ok(DeploymentPlan {
        provider: HostingProvider::GithubPages,
        bundle_path: bundle_path.to_path_buf(),
        steps,
        expected_url,
        generated_files,
        warnings,
    })
}

fn generate_cloudflare_pages_plan(
    inputs: &WizardInputs,
    _env: &DetectedEnvironment,
    bundle_path: &Path,
) -> PlanResult<DeploymentPlan> {
    let mut steps = Vec::new();
    let mut generated_files = Vec::new();
    let warnings = Vec::new();

    let output_dir = inputs
        .output_dir
        .clone()
        .unwrap_or_else(|| bundle_path.to_path_buf());

    // Step 1: Create _headers file
    let headers_file = output_dir.join("_headers");
    steps.push(PlanStep {
        index: 1,
        id: "create_headers".to_string(),
        description: "Create _headers file for COOP/COEP headers".to_string(),
        command: None,
        optional: false,
        requires_confirm: false,
    });
    generated_files.push(headers_file);

    // Step 2: Create _redirects file (optional)
    let redirects_file = output_dir.join("_redirects");
    steps.push(PlanStep {
        index: 2,
        id: "create_redirects".to_string(),
        description: "Create _redirects file for SPA routing".to_string(),
        command: None,
        optional: true,
        requires_confirm: false,
    });
    generated_files.push(redirects_file);

    // Step 3: Deploy with Wrangler
    let project = inputs.cloudflare_project.as_deref().unwrap_or("agent-mail");
    steps.push(PlanStep {
        index: 3,
        id: "wrangler_deploy".to_string(),
        description: format!("Deploy to Cloudflare Pages project: {project}"),
        command: Some(format!(
            "wrangler pages deploy {} --project-name {}",
            bundle_path.display(),
            project
        )),
        optional: false,
        requires_confirm: true,
    });

    let expected_url = Some(format!("https://{project}.pages.dev"));

    Ok(DeploymentPlan {
        provider: HostingProvider::CloudflarePages,
        bundle_path: bundle_path.to_path_buf(),
        steps,
        expected_url,
        generated_files,
        warnings,
    })
}

fn generate_netlify_plan(
    inputs: &WizardInputs,
    _env: &DetectedEnvironment,
    bundle_path: &Path,
) -> PlanResult<DeploymentPlan> {
    let mut steps = Vec::new();
    let mut generated_files = Vec::new();
    let warnings = Vec::new();

    let output_dir = inputs
        .output_dir
        .clone()
        .unwrap_or_else(|| bundle_path.to_path_buf());

    // Step 1: Create _headers file
    let headers_file = output_dir.join("_headers");
    steps.push(PlanStep {
        index: 1,
        id: "create_headers".to_string(),
        description: "Create _headers file for COOP/COEP headers".to_string(),
        command: None,
        optional: false,
        requires_confirm: false,
    });
    generated_files.push(headers_file);

    // Step 2: Create netlify.toml (optional)
    let netlify_toml = output_dir.join("netlify.toml");
    steps.push(PlanStep {
        index: 2,
        id: "create_netlify_toml".to_string(),
        description: "Generate netlify.toml configuration".to_string(),
        command: None,
        optional: true,
        requires_confirm: false,
    });
    generated_files.push(netlify_toml);

    // Step 3: Deploy with Netlify CLI
    let site = inputs.netlify_site.as_deref().unwrap_or("agent-mail");
    steps.push(PlanStep {
        index: 3,
        id: "netlify_deploy".to_string(),
        description: format!("Deploy to Netlify site: {site}"),
        command: Some(format!(
            "netlify deploy --dir {} --prod",
            bundle_path.display()
        )),
        optional: false,
        requires_confirm: true,
    });

    let expected_url = Some(format!("https://{site}.netlify.app"));

    Ok(DeploymentPlan {
        provider: HostingProvider::Netlify,
        bundle_path: bundle_path.to_path_buf(),
        steps,
        expected_url,
        generated_files,
        warnings,
    })
}

fn generate_s3_plan(
    inputs: &WizardInputs,
    _env: &DetectedEnvironment,
    bundle_path: &Path,
) -> PlanResult<DeploymentPlan> {
    let mut steps = Vec::new();
    let generated_files = Vec::new();
    let mut warnings = Vec::new();

    // S3 bucket is required
    let bucket = match &inputs.s3_bucket {
        Some(b) => b.clone(),
        None => {
            return Err(WizardError::new(
                WizardErrorCode::MissingRequiredOption,
                "S3 bucket name is required",
            )
            .with_hint("Specify --s3-bucket"));
        }
    };

    // Step 1: Sync to S3
    steps.push(PlanStep {
        index: 1,
        id: "s3_sync".to_string(),
        description: format!("Sync bundle to S3 bucket: {bucket}"),
        command: Some(format!(
            "aws s3 sync {} s3://{} --delete",
            bundle_path.display(),
            bucket
        )),
        optional: false,
        requires_confirm: true,
    });

    // Step 2: Set content types
    steps.push(PlanStep {
        index: 2,
        id: "s3_content_types".to_string(),
        description: "Set Content-Type for SQLite files".to_string(),
        command: Some(format!(
            "aws s3 cp s3://{bucket}/ s3://{bucket}/ --recursive \
             --exclude '*' --include '*.sqlite3' \
             --content-type 'application/x-sqlite3' \
             --metadata-directive REPLACE"
        )),
        optional: false,
        requires_confirm: false,
    });

    // Step 3: Invalidate CloudFront (if configured)
    if let Some(ref dist_id) = inputs.cloudfront_id {
        steps.push(PlanStep {
            index: 3,
            id: "cloudfront_invalidate".to_string(),
            description: format!("Invalidate CloudFront distribution: {dist_id}"),
            command: Some(format!(
                "aws cloudfront create-invalidation --distribution-id {} --paths '/*'",
                dist_id
            )),
            optional: true,
            requires_confirm: true,
        });
    } else {
        warnings.push(
            "No CloudFront distribution configured - COOP/COEP headers must be set manually"
                .to_string(),
        );
    }

    let expected_url = inputs.base_url.clone().or_else(|| {
        inputs
            .cloudfront_id
            .as_ref()
            .map(|_| format!("https://{bucket}.s3.amazonaws.com"))
    });

    Ok(DeploymentPlan {
        provider: HostingProvider::S3,
        bundle_path: bundle_path.to_path_buf(),
        steps,
        expected_url,
        generated_files,
        warnings,
    })
}

fn generate_custom_plan(
    _inputs: &WizardInputs,
    _env: &DetectedEnvironment,
    bundle_path: &Path,
) -> PlanResult<DeploymentPlan> {
    let mut steps = Vec::new();
    let mut generated_files = Vec::new();
    let warnings = Vec::new();

    // Step 1: Generate _headers file
    let headers_file = bundle_path.join("_headers");
    steps.push(PlanStep {
        index: 1,
        id: "create_headers".to_string(),
        description: "Create _headers file for COOP/COEP headers".to_string(),
        command: None,
        optional: false,
        requires_confirm: false,
    });
    generated_files.push(headers_file);

    // Step 2: Manual deployment instructions
    steps.push(PlanStep {
        index: 2,
        id: "manual_deploy".to_string(),
        description: format!(
            "Upload bundle contents from {} to your hosting provider",
            bundle_path.display()
        ),
        command: None,
        optional: false,
        requires_confirm: false,
    });

    // Step 3: Configure headers
    steps.push(PlanStep {
        index: 3,
        id: "configure_headers".to_string(),
        description: "Configure Cross-Origin-Opener-Policy and Cross-Origin-Embedder-Policy headers on your server".to_string(),
        command: None,
        optional: false,
        requires_confirm: false,
    });

    Ok(DeploymentPlan {
        provider: HostingProvider::Custom,
        bundle_path: bundle_path.to_path_buf(),
        steps,
        expected_url: None,
        generated_files,
        warnings,
    })
}

/// Format a plan as human-readable text.
pub fn format_plan_human(plan: &DeploymentPlan) -> String {
    let mut output = String::new();

    output.push_str(&format!(
        "Deployment Plan: {} -> {}\n",
        plan.bundle_path.display(),
        plan.provider.display_name()
    ));
    output.push_str(&"─".repeat(60));
    output.push('\n');

    if let Some(ref url) = plan.expected_url {
        output.push_str(&format!("Expected URL: {url}\n\n"));
    }

    output.push_str("Steps:\n");
    for step in &plan.steps {
        let optional = if step.optional { " (optional)" } else { "" };
        let confirm = if step.requires_confirm {
            " [confirm]"
        } else {
            ""
        };
        output.push_str(&format!(
            "  {}. {}{}{}\n",
            step.index, step.description, optional, confirm
        ));
        if let Some(ref cmd) = step.command {
            output.push_str(&format!("     $ {cmd}\n"));
        }
    }

    if !plan.warnings.is_empty() {
        output.push_str("\nWarnings:\n");
        for warning in &plan.warnings {
            output.push_str(&format!("  ⚠ {warning}\n"));
        }
    }

    if !plan.generated_files.is_empty() {
        output.push_str("\nFiles to generate:\n");
        for file in &plan.generated_files {
            output.push_str(&format!("  - {}\n", file.display()));
        }
    }

    output
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn normalize_snapshot_text(text: &str) -> String {
        let mut out = String::new();
        for line in text.replace("\r\n", "\n").lines() {
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }

    #[test]
    fn validate_inputs_empty_ok() {
        let inputs = WizardInputs::default();
        // Should fail because no bundle path
        let result = validate_inputs(&inputs);
        assert!(result.is_ok()); // Empty inputs are ok, bundle path checked in resolve
    }

    #[test]
    fn validate_inputs_missing_bundle() {
        let inputs = WizardInputs {
            bundle_path: Some(PathBuf::from("/nonexistent/bundle")),
            ..Default::default()
        };
        let result = validate_inputs(&inputs);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code, WizardErrorCode::BundleNotFound);
    }

    #[test]
    fn resolve_provider_explicit() {
        let inputs = WizardInputs {
            provider: Some(HostingProvider::Netlify),
            ..Default::default()
        };
        let env = DetectedEnvironment::default();
        let provider = resolve_provider(&inputs, &env).unwrap();
        assert_eq!(provider, HostingProvider::Netlify);
    }

    #[test]
    fn resolve_provider_from_env() {
        let inputs = WizardInputs::default();
        let env = DetectedEnvironment {
            recommended_provider: Some(HostingProvider::CloudflarePages),
            ..Default::default()
        };
        let provider = resolve_provider(&inputs, &env).unwrap();
        assert_eq!(provider, HostingProvider::CloudflarePages);
    }

    #[test]
    fn resolve_provider_github_fallback() {
        let inputs = WizardInputs::default();
        let env = DetectedEnvironment {
            github_repo: Some("owner/repo".to_string()),
            ..Default::default()
        };
        let provider = resolve_provider(&inputs, &env).unwrap();
        assert_eq!(provider, HostingProvider::GithubPages);
    }

    #[test]
    fn resolve_provider_fails_without_signals() {
        let inputs = WizardInputs::default();
        let env = DetectedEnvironment::default();
        let result = resolve_provider(&inputs, &env);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code, WizardErrorCode::MissingRequiredOption);
    }

    #[test]
    fn github_plan_has_required_steps() {
        let inputs = WizardInputs {
            provider: Some(HostingProvider::GithubPages),
            ..Default::default()
        };
        let env = DetectedEnvironment {
            is_git_repo: true,
            github_repo: Some("owner/repo".to_string()),
            ..Default::default()
        };
        let bundle = tempfile::tempdir().unwrap();
        std::fs::write(bundle.path().join("manifest.json"), "{}").unwrap();

        let plan = generate_github_pages_plan(&inputs, &env, bundle.path()).unwrap();
        assert_eq!(plan.provider, HostingProvider::GithubPages);
        assert!(!plan.steps.is_empty());
        assert!(plan.steps.iter().any(|s| s.id == "create_nojekyll"));
        assert!(plan.steps.iter().any(|s| s.id == "create_headers"));
    }

    #[test]
    fn cloudflare_plan_has_wrangler_step() {
        let inputs = WizardInputs {
            provider: Some(HostingProvider::CloudflarePages),
            cloudflare_project: Some("my-project".to_string()),
            ..Default::default()
        };
        let env = DetectedEnvironment::default();
        let bundle = tempfile::tempdir().unwrap();

        let plan = generate_cloudflare_pages_plan(&inputs, &env, bundle.path()).unwrap();
        assert!(plan.steps.iter().any(|s| s.id == "wrangler_deploy"));
        assert!(plan.expected_url.as_ref().unwrap().contains("my-project"));
    }

    #[test]
    fn s3_plan_requires_bucket() {
        let inputs = WizardInputs {
            provider: Some(HostingProvider::S3),
            ..Default::default()
        };
        let env = DetectedEnvironment::default();
        let bundle = tempfile::tempdir().unwrap();

        let result = generate_s3_plan(&inputs, &env, bundle.path());
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code, WizardErrorCode::MissingRequiredOption);
    }

    #[test]
    fn format_plan_human_includes_steps() {
        let plan = DeploymentPlan {
            provider: HostingProvider::GithubPages,
            bundle_path: PathBuf::from("/tmp/bundle"),
            steps: vec![PlanStep {
                index: 1,
                id: "test".to_string(),
                description: "Test step".to_string(),
                command: Some("echo test".to_string()),
                optional: false,
                requires_confirm: false,
            }],
            expected_url: Some("https://example.github.io/repo".to_string()),
            generated_files: vec![],
            warnings: vec![],
        };

        let output = format_plan_human(&plan);
        assert!(output.contains("GitHub Pages"));
        assert!(output.contains("Test step"));
        assert!(output.contains("echo test"));
        assert!(output.contains("https://example.github.io/repo"));
    }

    #[test]
    fn format_plan_human_matches_snapshot() {
        let plan = DeploymentPlan {
            provider: HostingProvider::GithubPages,
            bundle_path: PathBuf::from("/tmp/bundle"),
            steps: vec![
                PlanStep {
                    index: 1,
                    id: "prepare".to_string(),
                    description: "Prepare workflow".to_string(),
                    command: Some("echo prepare".to_string()),
                    optional: true,
                    requires_confirm: true,
                },
                PlanStep {
                    index: 2,
                    id: "deploy".to_string(),
                    description: "Deploy bundle".to_string(),
                    command: Some("gh workflow run deploy.yml".to_string()),
                    optional: false,
                    requires_confirm: false,
                },
            ],
            expected_url: Some("https://example.github.io/repo".to_string()),
            generated_files: vec![
                PathBuf::from("/tmp/bundle/.nojekyll"),
                PathBuf::from("/tmp/bundle/_headers"),
            ],
            warnings: vec![
                "Ensure Pages source is set to GitHub Actions".to_string(),
                "First deployment may take a few minutes".to_string(),
            ],
        };

        let expected = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/plan_human_github_snapshot.txt"
        ));
        let actual = format_plan_human(&plan);

        assert_eq!(
            normalize_snapshot_text(expected),
            normalize_snapshot_text(&actual),
            "format_plan_human snapshot drift"
        );
    }
}
