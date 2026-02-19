//! Native share wizard domain model and JSON output schema.
//!
//! Defines the typed models for wizard inputs, detected environment,
//! deployment plans, and structured output. Replaces the Python-based
//! wizard with a deterministic, testable Rust implementation.
//!
//! # Design Rationale
//!
//! The wizard guides users through deploying an Agent Mail bundle to a
//! static hosting provider. It operates in two modes:
//!
//! - **Interactive**: Prompts for input with validation and guidance
//! - **Non-interactive**: Accepts all options via flags, emits JSON
//!
//! Both modes produce identical output structures for consistency.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ── Hosting Provider Types ──────────────────────────────────────────────

/// Supported static hosting providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostingProvider {
    /// GitHub Pages (classic or Actions-based deployment)
    GithubPages,
    /// Cloudflare Pages
    CloudflarePages,
    /// Netlify
    Netlify,
    /// Amazon S3 + CloudFront
    S3,
    /// Custom/manual deployment
    Custom,
}

impl HostingProvider {
    /// Machine-readable identifier.
    #[must_use]
    pub const fn id(&self) -> &'static str {
        match self {
            Self::GithubPages => "github_pages",
            Self::CloudflarePages => "cloudflare_pages",
            Self::Netlify => "netlify",
            Self::S3 => "s3",
            Self::Custom => "custom",
        }
    }

    /// Human-readable display name.
    #[must_use]
    pub const fn display_name(&self) -> &'static str {
        match self {
            Self::GithubPages => "GitHub Pages",
            Self::CloudflarePages => "Cloudflare Pages",
            Self::Netlify => "Netlify",
            Self::S3 => "Amazon S3",
            Self::Custom => "Custom",
        }
    }

    /// Brief description for selection prompts.
    #[must_use]
    pub const fn description(&self) -> &'static str {
        match self {
            Self::GithubPages => "Free hosting for public repos, GitHub Actions workflow",
            Self::CloudflarePages => "Global CDN, automatic HTTPS, native COOP/COEP headers",
            Self::Netlify => "Continuous deployment, form handling, serverless functions",
            Self::S3 => "AWS S3 bucket with CloudFront CDN distribution",
            Self::Custom => "Manual deployment with generated headers file",
        }
    }

    /// Parse from string (case-insensitive, handles aliases).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "github" | "github_pages" | "github-pages" | "ghpages" | "gh" => {
                Some(Self::GithubPages)
            }
            "cloudflare" | "cloudflare_pages" | "cloudflare-pages" | "cf" | "cfpages" => {
                Some(Self::CloudflarePages)
            }
            "netlify" => Some(Self::Netlify),
            "s3" | "aws" | "amazon" => Some(Self::S3),
            "custom" | "manual" | "other" => Some(Self::Custom),
            _ => None,
        }
    }
}

impl std::fmt::Display for HostingProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.display_name())
    }
}

// ── Wizard Inputs ───────────────────────────────────────────────────────

/// Wizard input configuration from CLI flags or prompts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WizardInputs {
    /// Target hosting provider.
    pub provider: Option<HostingProvider>,
    /// Path to the bundle directory to deploy.
    pub bundle_path: Option<PathBuf>,
    /// Output directory for deploy artifacts (provider-specific files).
    pub output_dir: Option<PathBuf>,
    /// GitHub repository (owner/repo) for GitHub Pages.
    pub github_repo: Option<String>,
    /// GitHub branch for Pages (default: gh-pages).
    pub github_branch: Option<String>,
    /// Cloudflare project name.
    pub cloudflare_project: Option<String>,
    /// Netlify site ID or name.
    pub netlify_site: Option<String>,
    /// S3 bucket name.
    pub s3_bucket: Option<String>,
    /// CloudFront distribution ID.
    pub cloudfront_id: Option<String>,
    /// Custom base URL for the deployed site.
    pub base_url: Option<String>,
    /// Skip confirmation prompts (non-interactive mode).
    pub skip_confirm: bool,
    /// Dry-run mode (generate plan but don't execute).
    pub dry_run: bool,
}

impl Default for WizardInputs {
    fn default() -> Self {
        Self {
            provider: None,
            bundle_path: None,
            output_dir: None,
            github_repo: None,
            github_branch: Some("gh-pages".to_string()),
            cloudflare_project: None,
            netlify_site: None,
            s3_bucket: None,
            cloudfront_id: None,
            base_url: None,
            skip_confirm: false,
            dry_run: false,
        }
    }
}

// ── Environment Detection ───────────────────────────────────────────────

/// Detection confidence level for auto-detected values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectionConfidence {
    /// Strong signal (e.g., explicit config file found).
    High,
    /// Moderate signal (e.g., env var or remote URL).
    Medium,
    /// Weak signal (e.g., naming convention or heuristic).
    Low,
}

/// A single detected environment signal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectedSignal {
    /// Source of the signal (e.g., "git_remote", "env_var", "config_file").
    pub source: String,
    /// Description of what was found.
    pub detail: String,
    /// Confidence in this signal.
    pub confidence: DetectionConfidence,
}

/// Detected environment state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DetectedEnvironment {
    /// Git remote URL if detected.
    pub git_remote_url: Option<String>,
    /// GitHub owner/repo extracted from remote.
    pub github_repo: Option<String>,
    /// Current working directory.
    pub cwd: PathBuf,
    /// Whether inside a Git repository.
    pub is_git_repo: bool,
    /// Detected provider signals.
    pub signals: Vec<DetectedSignal>,
    /// Recommended provider based on signals.
    pub recommended_provider: Option<HostingProvider>,
    /// Existing bundle found at default location.
    pub existing_bundle: Option<PathBuf>,
    /// GitHub Pages environment variables present.
    pub github_env: bool,
    /// Cloudflare Pages environment variables present.
    pub cloudflare_env: bool,
    /// Netlify environment variables present.
    pub netlify_env: bool,
    /// AWS environment variables present.
    pub aws_env: bool,
}

// ── Execution Plan ──────────────────────────────────────────────────────

/// A single step in the deployment plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStep {
    /// Step number (1-indexed).
    pub index: u32,
    /// Short identifier for the step.
    pub id: String,
    /// Human-readable description.
    pub description: String,
    /// Command to execute (if applicable).
    pub command: Option<String>,
    /// Whether this step is optional.
    pub optional: bool,
    /// Whether this step requires user confirmation.
    pub requires_confirm: bool,
}

/// Deployment execution plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentPlan {
    /// Target provider.
    pub provider: HostingProvider,
    /// Source bundle path.
    pub bundle_path: PathBuf,
    /// Steps to execute.
    pub steps: Vec<PlanStep>,
    /// Estimated final URL.
    pub expected_url: Option<String>,
    /// Generated files that will be created.
    pub generated_files: Vec<PathBuf>,
    /// Warnings or notes for the user.
    pub warnings: Vec<String>,
}

// ── Wizard Result ───────────────────────────────────────────────────────

/// Outcome of a single plan step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepOutcome {
    /// Step identifier.
    pub step_id: String,
    /// Whether the step succeeded.
    pub success: bool,
    /// Output or error message.
    pub message: String,
    /// Duration in milliseconds.
    pub duration_ms: u64,
    /// Files created by this step.
    pub files_created: Vec<PathBuf>,
}

/// Final wizard execution result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WizardResult {
    /// Overall success.
    pub success: bool,
    /// Target provider.
    pub provider: HostingProvider,
    /// Bundle path that was deployed.
    pub bundle_path: PathBuf,
    /// Final deployed URL (if known).
    pub deployed_url: Option<String>,
    /// Outcome of each step.
    pub steps: Vec<StepOutcome>,
    /// Total duration in milliseconds.
    pub total_duration_ms: u64,
    /// Error message if failed.
    pub error: Option<String>,
    /// Error code for programmatic handling.
    pub error_code: Option<WizardErrorCode>,
    /// Additional metadata.
    pub metadata: WizardMetadata,
}

/// Metadata included in wizard output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WizardMetadata {
    /// Wizard version.
    pub version: String,
    /// Timestamp of execution.
    pub timestamp: String,
    /// Mode (interactive/non-interactive).
    pub mode: WizardMode,
    /// Dry-run flag.
    pub dry_run: bool,
}

/// Wizard execution mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WizardMode {
    /// Interactive prompts.
    Interactive,
    /// Non-interactive (all flags provided).
    NonInteractive,
}

// ── Error Taxonomy ──────────────────────────────────────────────────────

/// Wizard error categories for programmatic handling.
///
/// Error codes are stable across versions for scripting compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WizardErrorCode {
    // ── Validation Errors (1xx range) ──
    /// Bundle path does not exist or is not a directory.
    BundleNotFound,
    /// Bundle is missing required files (manifest.json, etc.).
    BundleInvalid,
    /// Provider is not supported or unrecognized.
    ProviderUnknown,
    /// Required provider-specific option is missing.
    MissingRequiredOption,
    /// Invalid option value.
    InvalidOption,

    // ── Environment Errors (2xx range) ──
    /// Git is not installed or not in PATH.
    GitNotFound,
    /// Not inside a Git repository.
    NotGitRepo,
    /// Git remote not configured.
    NoGitRemote,
    /// Required CLI tool not found (gh, wrangler, netlify, aws).
    ToolNotFound,
    /// Required environment variable not set.
    EnvVarMissing,
    /// Network connectivity issue.
    NetworkError,

    // ── Execution Errors (3xx range) ──
    /// Command execution failed.
    CommandFailed,
    /// File write/copy failed.
    FileOperationFailed,
    /// Deployment verification failed.
    VerificationFailed,
    /// User cancelled the operation.
    UserCancelled,
    /// Timeout waiting for deployment.
    Timeout,

    // ── Internal Errors (9xx range) ──
    /// Unexpected internal error.
    InternalError,
}

impl WizardErrorCode {
    /// Numeric code for exit status calculation.
    #[must_use]
    pub const fn code(&self) -> u8 {
        match self {
            // Validation errors: exit 1
            Self::BundleNotFound
            | Self::BundleInvalid
            | Self::ProviderUnknown
            | Self::MissingRequiredOption
            | Self::InvalidOption => 1,
            // Environment errors: exit 2
            Self::GitNotFound
            | Self::NotGitRepo
            | Self::NoGitRemote
            | Self::ToolNotFound
            | Self::EnvVarMissing
            | Self::NetworkError => 2,
            // Execution errors: exit 3
            Self::CommandFailed
            | Self::FileOperationFailed
            | Self::VerificationFailed
            | Self::Timeout => 3,
            // User cancelled: exit 130 (standard SIGINT convention)
            Self::UserCancelled => 130,
            // Internal errors: exit 99
            Self::InternalError => 99,
        }
    }

    /// Human-readable error category.
    #[must_use]
    pub const fn category(&self) -> &'static str {
        match self {
            Self::BundleNotFound
            | Self::BundleInvalid
            | Self::ProviderUnknown
            | Self::MissingRequiredOption
            | Self::InvalidOption => "validation",
            Self::GitNotFound
            | Self::NotGitRepo
            | Self::NoGitRemote
            | Self::ToolNotFound
            | Self::EnvVarMissing
            | Self::NetworkError => "environment",
            Self::CommandFailed
            | Self::FileOperationFailed
            | Self::VerificationFailed
            | Self::Timeout => "execution",
            Self::UserCancelled => "cancelled",
            Self::InternalError => "internal",
        }
    }
}

impl std::fmt::Display for WizardErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Use SCREAMING_SNAKE_CASE format for programmatic parsing
        let name = match self {
            Self::BundleNotFound => "BUNDLE_NOT_FOUND",
            Self::BundleInvalid => "BUNDLE_INVALID",
            Self::ProviderUnknown => "PROVIDER_UNKNOWN",
            Self::MissingRequiredOption => "MISSING_REQUIRED_OPTION",
            Self::InvalidOption => "INVALID_OPTION",
            Self::GitNotFound => "GIT_NOT_FOUND",
            Self::NotGitRepo => "NOT_GIT_REPO",
            Self::NoGitRemote => "NO_GIT_REMOTE",
            Self::ToolNotFound => "TOOL_NOT_FOUND",
            Self::EnvVarMissing => "ENV_VAR_MISSING",
            Self::NetworkError => "NETWORK_ERROR",
            Self::CommandFailed => "COMMAND_FAILED",
            Self::FileOperationFailed => "FILE_OPERATION_FAILED",
            Self::VerificationFailed => "VERIFICATION_FAILED",
            Self::UserCancelled => "USER_CANCELLED",
            Self::Timeout => "TIMEOUT",
            Self::InternalError => "INTERNAL_ERROR",
        };
        f.write_str(name)
    }
}

/// Wizard-specific error type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WizardError {
    /// Error code for programmatic handling.
    pub code: WizardErrorCode,
    /// Human-readable error message.
    pub message: String,
    /// Additional context (e.g., file path, command output).
    pub context: Option<String>,
    /// Suggested remediation steps.
    pub hint: Option<String>,
}

impl WizardError {
    /// Create a new wizard error.
    #[must_use]
    pub fn new(code: WizardErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            context: None,
            hint: None,
        }
    }

    /// Add context to the error.
    #[must_use]
    pub fn with_context(mut self, context: impl Into<String>) -> Self {
        self.context = Some(context.into());
        self
    }

    /// Add a hint for remediation.
    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Exit code for this error.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        i32::from(self.code.code())
    }
}

impl std::fmt::Display for WizardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)?;
        if let Some(ref ctx) = self.context {
            write!(f, " ({})", ctx)?;
        }
        Ok(())
    }
}

impl std::error::Error for WizardError {}

// ── Exit Code Contract ──────────────────────────────────────────────────

/// Exit codes for the wizard command.
///
/// These codes are stable and documented for scripting.
pub mod exit_codes {
    /// Success: wizard completed and deployment succeeded.
    pub const SUCCESS: i32 = 0;
    /// Validation error: invalid inputs, missing bundle, unknown provider.
    pub const VALIDATION_ERROR: i32 = 1;
    /// Environment error: missing tools, git issues, network problems.
    pub const ENVIRONMENT_ERROR: i32 = 2;
    /// Execution error: command failed, deployment verification failed.
    pub const EXECUTION_ERROR: i32 = 3;
    /// Internal error: unexpected bug.
    pub const INTERNAL_ERROR: i32 = 99;
    /// User cancelled (SIGINT convention).
    pub const USER_CANCELLED: i32 = 130;
}

// ── JSON Output Schema ──────────────────────────────────────────────────

/// Complete wizard JSON output for `--json` mode.
///
/// This is the schema emitted to stdout when `--json` flag is provided.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WizardJsonOutput {
    /// Success or failure.
    pub success: bool,
    /// Target provider identifier.
    pub provider: String,
    /// Final deployed URL (if successful and known).
    pub url: Option<String>,
    /// Bundle path that was deployed.
    pub bundle_path: String,
    /// Error message (if failed).
    pub error: Option<String>,
    /// Error code (if failed).
    pub error_code: Option<String>,
    /// Detailed execution result.
    pub result: Option<WizardResult>,
    /// Detected environment (for debugging).
    pub environment: Option<DetectedEnvironment>,
    /// Generated deployment plan.
    pub plan: Option<DeploymentPlan>,
}

impl WizardJsonOutput {
    /// Create success output.
    #[must_use]
    pub fn success(result: WizardResult) -> Self {
        Self {
            success: true,
            provider: result.provider.id().to_string(),
            url: result.deployed_url.clone(),
            bundle_path: result.bundle_path.display().to_string(),
            error: None,
            error_code: None,
            result: Some(result),
            environment: None,
            plan: None,
        }
    }

    /// Create failure output.
    #[must_use]
    pub fn failure(error: WizardError, bundle_path: Option<PathBuf>) -> Self {
        Self {
            success: false,
            provider: String::new(),
            url: None,
            bundle_path: bundle_path
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            error: Some(error.message.clone()),
            error_code: Some(error.code.to_string()),
            result: None,
            environment: None,
            plan: None,
        }
    }

    /// Attach environment detection info.
    #[must_use]
    pub fn with_environment(mut self, env: DetectedEnvironment) -> Self {
        self.environment = Some(env);
        self
    }

    /// Attach deployment plan.
    #[must_use]
    pub fn with_plan(mut self, plan: DeploymentPlan) -> Self {
        self.plan = Some(plan);
        self
    }
}

// ── Wizard Version ──────────────────────────────────────────────────────

/// Wizard implementation version.
pub const WIZARD_VERSION: &str = "2.0.0";

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_parse_handles_aliases() {
        assert_eq!(
            HostingProvider::parse("github"),
            Some(HostingProvider::GithubPages)
        );
        assert_eq!(
            HostingProvider::parse("GITHUB_PAGES"),
            Some(HostingProvider::GithubPages)
        );
        assert_eq!(
            HostingProvider::parse("gh"),
            Some(HostingProvider::GithubPages)
        );
        assert_eq!(
            HostingProvider::parse("cloudflare"),
            Some(HostingProvider::CloudflarePages)
        );
        assert_eq!(
            HostingProvider::parse("cf"),
            Some(HostingProvider::CloudflarePages)
        );
        assert_eq!(
            HostingProvider::parse("netlify"),
            Some(HostingProvider::Netlify)
        );
        assert_eq!(HostingProvider::parse("s3"), Some(HostingProvider::S3));
        assert_eq!(
            HostingProvider::parse("custom"),
            Some(HostingProvider::Custom)
        );
        assert_eq!(HostingProvider::parse("unknown"), None);
    }

    #[test]
    fn provider_identifiers_and_labels_are_stable() {
        assert_eq!(HostingProvider::GithubPages.id(), "github_pages");
        assert_eq!(HostingProvider::CloudflarePages.id(), "cloudflare_pages");
        assert_eq!(HostingProvider::GithubPages.display_name(), "GitHub Pages");
        assert_eq!(HostingProvider::S3.display_name(), "Amazon S3");
        assert!(
            HostingProvider::Netlify
                .description()
                .contains("Continuous deployment")
        );
    }

    #[test]
    fn error_code_exit_codes_are_stable() {
        // These exit codes must not change
        assert_eq!(WizardErrorCode::BundleNotFound.code(), 1);
        assert_eq!(WizardErrorCode::GitNotFound.code(), 2);
        assert_eq!(WizardErrorCode::CommandFailed.code(), 3);
        assert_eq!(WizardErrorCode::UserCancelled.code(), 130);
        assert_eq!(WizardErrorCode::InternalError.code(), 99);
    }

    #[test]
    fn error_code_categories_are_grouped_correctly() {
        assert_eq!(WizardErrorCode::BundleInvalid.category(), "validation");
        assert_eq!(WizardErrorCode::ToolNotFound.category(), "environment");
        assert_eq!(WizardErrorCode::VerificationFailed.category(), "execution");
        assert_eq!(WizardErrorCode::UserCancelled.category(), "cancelled");
        assert_eq!(WizardErrorCode::InternalError.category(), "internal");
    }

    #[test]
    fn json_output_serializes_correctly() {
        let result = WizardResult {
            success: true,
            provider: HostingProvider::GithubPages,
            bundle_path: PathBuf::from("/tmp/bundle"),
            deployed_url: Some("https://example.github.io/agent-mail".to_string()),
            steps: vec![],
            total_duration_ms: 5000,
            error: None,
            error_code: None,
            metadata: WizardMetadata {
                version: WIZARD_VERSION.to_string(),
                timestamp: "2026-02-12T07:00:00Z".to_string(),
                mode: WizardMode::NonInteractive,
                dry_run: false,
            },
        };
        let output = WizardJsonOutput::success(result);
        let json = serde_json::to_string_pretty(&output).unwrap();
        assert!(json.contains("\"success\": true"));
        assert!(json.contains("\"provider\": \"github_pages\""));
        assert!(json.contains("https://example.github.io/agent-mail"));
    }

    #[test]
    fn json_output_failure_contains_error_details() {
        let error = WizardError::new(WizardErrorCode::CommandFailed, "deploy failed");
        let output = WizardJsonOutput::failure(error, Some(PathBuf::from("/tmp/bundle")));
        assert!(!output.success);
        assert!(output.provider.is_empty());
        assert_eq!(output.bundle_path, "/tmp/bundle");
        assert_eq!(output.error.as_deref(), Some("deploy failed"));
        assert_eq!(output.error_code.as_deref(), Some("COMMAND_FAILED"));
    }

    #[test]
    fn json_output_with_environment_and_plan_populates_fields() {
        let base = WizardJsonOutput::failure(
            WizardError::new(WizardErrorCode::InternalError, "internal"),
            None,
        );
        let env = DetectedEnvironment {
            github_env: true,
            ..Default::default()
        };
        let plan = DeploymentPlan {
            provider: HostingProvider::GithubPages,
            bundle_path: PathBuf::from("/tmp/bundle"),
            steps: vec![PlanStep {
                index: 1,
                id: "generate".to_string(),
                description: "Generate workflow".to_string(),
                command: Some("am share wizard --provider github_pages".to_string()),
                optional: false,
                requires_confirm: true,
            }],
            expected_url: Some("https://example.github.io/repo".to_string()),
            generated_files: vec![PathBuf::from("deploy.yml")],
            warnings: vec!["Manual DNS check recommended".to_string()],
        };

        let output = base.with_environment(env).with_plan(plan.clone());
        assert!(output.environment.is_some());
        assert_eq!(
            output.plan.expect("plan should be set").provider,
            plan.provider
        );
    }

    #[test]
    fn error_with_context_formats_correctly() {
        let err = WizardError::new(WizardErrorCode::BundleNotFound, "Bundle not found")
            .with_context("/path/to/bundle")
            .with_hint("Run 'am share export' first");
        assert_eq!(err.exit_code(), 1);
        let msg = err.to_string();
        assert!(msg.contains("BUNDLE_NOT_FOUND"));
        assert!(msg.contains("/path/to/bundle"));
    }

    #[test]
    fn default_inputs_are_sensible() {
        let inputs = WizardInputs::default();
        assert!(inputs.provider.is_none());
        assert!(!inputs.skip_confirm);
        assert!(!inputs.dry_run);
        assert_eq!(inputs.github_branch, Some("gh-pages".to_string()));
    }

    // ── Additional alias coverage ────────────────────────────────────────

    #[test]
    fn provider_parse_remaining_aliases() {
        // GitHub aliases not covered by existing test
        assert_eq!(
            HostingProvider::parse("ghpages"),
            Some(HostingProvider::GithubPages)
        );
        assert_eq!(
            HostingProvider::parse("github-pages"),
            Some(HostingProvider::GithubPages)
        );
        // Cloudflare aliases
        assert_eq!(
            HostingProvider::parse("cfpages"),
            Some(HostingProvider::CloudflarePages)
        );
        assert_eq!(
            HostingProvider::parse("cloudflare-pages"),
            Some(HostingProvider::CloudflarePages)
        );
        assert_eq!(
            HostingProvider::parse("cloudflare_pages"),
            Some(HostingProvider::CloudflarePages)
        );
        // S3 aliases
        assert_eq!(HostingProvider::parse("aws"), Some(HostingProvider::S3));
        assert_eq!(HostingProvider::parse("amazon"), Some(HostingProvider::S3));
        // Custom aliases
        assert_eq!(
            HostingProvider::parse("manual"),
            Some(HostingProvider::Custom)
        );
        assert_eq!(
            HostingProvider::parse("other"),
            Some(HostingProvider::Custom)
        );
    }

    #[test]
    fn provider_parse_trims_whitespace_and_ignores_case() {
        assert_eq!(
            HostingProvider::parse("  GITHUB  "),
            Some(HostingProvider::GithubPages)
        );
        assert_eq!(
            HostingProvider::parse("Netlify"),
            Some(HostingProvider::Netlify)
        );
        assert_eq!(HostingProvider::parse("  S3 "), Some(HostingProvider::S3));
        assert_eq!(HostingProvider::parse(""), None);
        assert_eq!(HostingProvider::parse("   "), None);
    }

    #[test]
    fn provider_display_matches_display_name() {
        let providers = [
            HostingProvider::GithubPages,
            HostingProvider::CloudflarePages,
            HostingProvider::Netlify,
            HostingProvider::S3,
            HostingProvider::Custom,
        ];
        for p in providers {
            assert_eq!(p.to_string(), p.display_name());
        }
    }

    // ── Serde roundtrips ─────────────────────────────────────────────────

    #[test]
    fn hosting_provider_serde_roundtrip_all_variants() {
        let variants = [
            HostingProvider::GithubPages,
            HostingProvider::CloudflarePages,
            HostingProvider::Netlify,
            HostingProvider::S3,
            HostingProvider::Custom,
        ];
        for v in variants {
            let json = serde_json::to_string(&v).unwrap();
            let back: HostingProvider = serde_json::from_str(&json).unwrap();
            assert_eq!(back, v, "roundtrip failed for {v:?}");
        }
        // Verify snake_case serialization
        assert_eq!(
            serde_json::to_string(&HostingProvider::GithubPages).unwrap(),
            "\"github_pages\""
        );
        assert_eq!(
            serde_json::to_string(&HostingProvider::CloudflarePages).unwrap(),
            "\"cloudflare_pages\""
        );
    }

    #[test]
    fn detection_confidence_serde_roundtrip() {
        for v in [
            DetectionConfidence::High,
            DetectionConfidence::Medium,
            DetectionConfidence::Low,
        ] {
            let json = serde_json::to_string(&v).unwrap();
            let back: DetectionConfidence = serde_json::from_str(&json).unwrap();
            assert_eq!(back, v);
        }
        assert_eq!(
            serde_json::to_string(&DetectionConfidence::High).unwrap(),
            "\"high\""
        );
    }

    #[test]
    fn wizard_mode_serde_roundtrip() {
        for v in [WizardMode::Interactive, WizardMode::NonInteractive] {
            let json = serde_json::to_string(&v).unwrap();
            let back: WizardMode = serde_json::from_str(&json).unwrap();
            assert_eq!(back, v);
        }
        assert_eq!(
            serde_json::to_string(&WizardMode::NonInteractive).unwrap(),
            "\"non_interactive\""
        );
    }

    #[test]
    fn error_code_serde_roundtrip_all_variants() {
        let all_codes = [
            WizardErrorCode::BundleNotFound,
            WizardErrorCode::BundleInvalid,
            WizardErrorCode::ProviderUnknown,
            WizardErrorCode::MissingRequiredOption,
            WizardErrorCode::InvalidOption,
            WizardErrorCode::GitNotFound,
            WizardErrorCode::NotGitRepo,
            WizardErrorCode::NoGitRemote,
            WizardErrorCode::ToolNotFound,
            WizardErrorCode::EnvVarMissing,
            WizardErrorCode::NetworkError,
            WizardErrorCode::CommandFailed,
            WizardErrorCode::FileOperationFailed,
            WizardErrorCode::VerificationFailed,
            WizardErrorCode::UserCancelled,
            WizardErrorCode::Timeout,
            WizardErrorCode::InternalError,
        ];
        for v in all_codes {
            let json = serde_json::to_string(&v).unwrap();
            let back: WizardErrorCode = serde_json::from_str(&json).unwrap();
            assert_eq!(back, v, "roundtrip failed for {v:?}");
        }
        // SCREAMING_SNAKE_CASE
        assert_eq!(
            serde_json::to_string(&WizardErrorCode::BundleNotFound).unwrap(),
            "\"BUNDLE_NOT_FOUND\""
        );
    }

    // ── Exhaustive code() and category() ─────────────────────────────────

    #[test]
    fn error_code_all_exit_codes_exhaustive() {
        // Validation: exit 1
        for v in [
            WizardErrorCode::BundleNotFound,
            WizardErrorCode::BundleInvalid,
            WizardErrorCode::ProviderUnknown,
            WizardErrorCode::MissingRequiredOption,
            WizardErrorCode::InvalidOption,
        ] {
            assert_eq!(v.code(), 1, "{v:?} should have code 1");
        }
        // Environment: exit 2
        for v in [
            WizardErrorCode::GitNotFound,
            WizardErrorCode::NotGitRepo,
            WizardErrorCode::NoGitRemote,
            WizardErrorCode::ToolNotFound,
            WizardErrorCode::EnvVarMissing,
            WizardErrorCode::NetworkError,
        ] {
            assert_eq!(v.code(), 2, "{v:?} should have code 2");
        }
        // Execution: exit 3
        for v in [
            WizardErrorCode::CommandFailed,
            WizardErrorCode::FileOperationFailed,
            WizardErrorCode::VerificationFailed,
            WizardErrorCode::Timeout,
        ] {
            assert_eq!(v.code(), 3, "{v:?} should have code 3");
        }
        assert_eq!(WizardErrorCode::UserCancelled.code(), 130);
        assert_eq!(WizardErrorCode::InternalError.code(), 99);
    }

    #[test]
    fn error_code_all_categories_exhaustive() {
        for v in [
            WizardErrorCode::BundleNotFound,
            WizardErrorCode::BundleInvalid,
            WizardErrorCode::ProviderUnknown,
            WizardErrorCode::MissingRequiredOption,
            WizardErrorCode::InvalidOption,
        ] {
            assert_eq!(v.category(), "validation", "{v:?}");
        }
        for v in [
            WizardErrorCode::GitNotFound,
            WizardErrorCode::NotGitRepo,
            WizardErrorCode::NoGitRemote,
            WizardErrorCode::ToolNotFound,
            WizardErrorCode::EnvVarMissing,
            WizardErrorCode::NetworkError,
        ] {
            assert_eq!(v.category(), "environment", "{v:?}");
        }
        for v in [
            WizardErrorCode::CommandFailed,
            WizardErrorCode::FileOperationFailed,
            WizardErrorCode::VerificationFailed,
            WizardErrorCode::Timeout,
        ] {
            assert_eq!(v.category(), "execution", "{v:?}");
        }
        assert_eq!(WizardErrorCode::UserCancelled.category(), "cancelled");
        assert_eq!(WizardErrorCode::InternalError.category(), "internal");
    }

    // ── Display impls ────────────────────────────────────────────────────

    #[test]
    fn error_code_display_all_screaming_snake_case() {
        let expected = [
            (WizardErrorCode::BundleNotFound, "BUNDLE_NOT_FOUND"),
            (WizardErrorCode::BundleInvalid, "BUNDLE_INVALID"),
            (WizardErrorCode::ProviderUnknown, "PROVIDER_UNKNOWN"),
            (
                WizardErrorCode::MissingRequiredOption,
                "MISSING_REQUIRED_OPTION",
            ),
            (WizardErrorCode::InvalidOption, "INVALID_OPTION"),
            (WizardErrorCode::GitNotFound, "GIT_NOT_FOUND"),
            (WizardErrorCode::NotGitRepo, "NOT_GIT_REPO"),
            (WizardErrorCode::NoGitRemote, "NO_GIT_REMOTE"),
            (WizardErrorCode::ToolNotFound, "TOOL_NOT_FOUND"),
            (WizardErrorCode::EnvVarMissing, "ENV_VAR_MISSING"),
            (WizardErrorCode::NetworkError, "NETWORK_ERROR"),
            (WizardErrorCode::CommandFailed, "COMMAND_FAILED"),
            (
                WizardErrorCode::FileOperationFailed,
                "FILE_OPERATION_FAILED",
            ),
            (WizardErrorCode::VerificationFailed, "VERIFICATION_FAILED"),
            (WizardErrorCode::UserCancelled, "USER_CANCELLED"),
            (WizardErrorCode::Timeout, "TIMEOUT"),
            (WizardErrorCode::InternalError, "INTERNAL_ERROR"),
        ];
        for (code, text) in expected {
            assert_eq!(code.to_string(), text, "{code:?}");
        }
    }

    #[test]
    fn wizard_error_display_without_context() {
        let err = WizardError::new(WizardErrorCode::NetworkError, "connection refused");
        assert_eq!(err.to_string(), "NETWORK_ERROR: connection refused");
    }

    #[test]
    fn wizard_error_display_with_context() {
        let err = WizardError::new(WizardErrorCode::ToolNotFound, "wrangler not found")
            .with_context("PATH=/usr/bin");
        assert_eq!(
            err.to_string(),
            "TOOL_NOT_FOUND: wrangler not found (PATH=/usr/bin)"
        );
    }

    #[test]
    fn wizard_error_is_std_error() {
        let err = WizardError::new(WizardErrorCode::Timeout, "timed out");
        let dyn_err: &dyn std::error::Error = &err;
        assert!(dyn_err.to_string().contains("TIMEOUT"));
    }

    #[test]
    fn wizard_error_exit_code_matches_code() {
        let err = WizardError::new(WizardErrorCode::UserCancelled, "ctrl-c");
        assert_eq!(err.exit_code(), 130);
        let err2 = WizardError::new(WizardErrorCode::InternalError, "bug");
        assert_eq!(err2.exit_code(), 99);
    }

    // ── JSON output edge cases ───────────────────────────────────────────

    #[test]
    fn json_output_failure_none_bundle_path() {
        let error = WizardError::new(WizardErrorCode::BundleNotFound, "not found");
        let output = WizardJsonOutput::failure(error, None);
        assert_eq!(output.bundle_path, "");
        assert!(!output.success);
    }

    // ── Exit code constants ──────────────────────────────────────────────

    #[test]
    fn exit_code_constants_match_error_categories() {
        assert_eq!(exit_codes::SUCCESS, 0);
        assert_eq!(
            exit_codes::VALIDATION_ERROR,
            i32::from(WizardErrorCode::BundleNotFound.code())
        );
        assert_eq!(
            exit_codes::ENVIRONMENT_ERROR,
            i32::from(WizardErrorCode::GitNotFound.code())
        );
        assert_eq!(
            exit_codes::EXECUTION_ERROR,
            i32::from(WizardErrorCode::CommandFailed.code())
        );
        assert_eq!(
            exit_codes::INTERNAL_ERROR,
            i32::from(WizardErrorCode::InternalError.code())
        );
        assert_eq!(
            exit_codes::USER_CANCELLED,
            i32::from(WizardErrorCode::UserCancelled.code())
        );
    }

    // ── Misc struct serde ────────────────────────────────────────────────

    #[test]
    fn wizard_inputs_serde_roundtrip() {
        let inputs = WizardInputs {
            provider: Some(HostingProvider::Netlify),
            bundle_path: Some(PathBuf::from("/tmp/b")),
            output_dir: Some(PathBuf::from("/tmp/out")),
            netlify_site: Some("my-site".to_string()),
            skip_confirm: true,
            dry_run: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&inputs).unwrap();
        let back: WizardInputs = serde_json::from_str(&json).unwrap();
        assert_eq!(back.provider, Some(HostingProvider::Netlify));
        assert!(back.skip_confirm);
        assert!(back.dry_run);
        assert_eq!(back.netlify_site, Some("my-site".to_string()));
    }

    #[test]
    fn step_outcome_serde_roundtrip() {
        let outcome = StepOutcome {
            step_id: "deploy".to_string(),
            success: true,
            message: "OK".to_string(),
            duration_ms: 1234,
            files_created: vec![PathBuf::from("deploy.yml")],
        };
        let json = serde_json::to_string(&outcome).unwrap();
        let back: StepOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(back.step_id, "deploy");
        assert!(back.success);
        assert_eq!(back.duration_ms, 1234);
        assert_eq!(back.files_created.len(), 1);
    }

    #[test]
    fn detected_signal_serde_roundtrip() {
        let signal = DetectedSignal {
            source: "git_remote".to_string(),
            detail: "github.com/user/repo".to_string(),
            confidence: DetectionConfidence::High,
        };
        let json = serde_json::to_string(&signal).unwrap();
        let back: DetectedSignal = serde_json::from_str(&json).unwrap();
        assert_eq!(back.source, "git_remote");
        assert_eq!(back.confidence, DetectionConfidence::High);
    }

    #[test]
    fn detected_environment_default_is_empty() {
        let env = DetectedEnvironment::default();
        assert!(env.git_remote_url.is_none());
        assert!(env.github_repo.is_none());
        assert!(!env.is_git_repo);
        assert!(env.signals.is_empty());
        assert!(env.recommended_provider.is_none());
        assert!(env.existing_bundle.is_none());
        assert!(!env.github_env);
        assert!(!env.cloudflare_env);
        assert!(!env.netlify_env);
        assert!(!env.aws_env);
    }

    #[test]
    fn wizard_version_is_semver() {
        let parts: Vec<&str> = WIZARD_VERSION.split('.').collect();
        assert_eq!(parts.len(), 3, "version should have 3 parts");
        for part in parts {
            part.parse::<u32>()
                .unwrap_or_else(|_| panic!("'{part}' should be numeric"));
        }
    }

    // ── Provider description coverage ────────────────────────────────────

    #[test]
    fn all_providers_have_nonempty_description() {
        let all = [
            HostingProvider::GithubPages,
            HostingProvider::CloudflarePages,
            HostingProvider::Netlify,
            HostingProvider::S3,
            HostingProvider::Custom,
        ];
        for p in all {
            assert!(!p.description().is_empty(), "{p:?} has empty description");
            assert!(!p.id().is_empty(), "{p:?} has empty id");
            assert!(!p.display_name().is_empty(), "{p:?} has empty display_name");
        }
    }

    // ── WizardError serde ────────────────────────────────────────────────

    #[test]
    fn wizard_error_serde_roundtrip() {
        let err = WizardError::new(WizardErrorCode::BundleInvalid, "bad manifest")
            .with_context("manifest.json missing")
            .with_hint("Run am share export first");
        let json = serde_json::to_string(&err).unwrap();
        let back: WizardError = serde_json::from_str(&json).unwrap();
        assert_eq!(back.code, WizardErrorCode::BundleInvalid);
        assert_eq!(back.message, "bad manifest");
        assert_eq!(back.context.as_deref(), Some("manifest.json missing"));
        assert_eq!(back.hint.as_deref(), Some("Run am share export first"));
    }
}
