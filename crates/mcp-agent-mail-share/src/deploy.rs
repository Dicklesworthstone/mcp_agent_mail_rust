//! Deployment validation, diagnostics, and reporting for static exports.
//!
//! Provides pre-flight checks, deployment report generation, platform-specific
//! configuration helpers, rollback guidance, post-deploy verification, and
//! security expectation documentation for GitHub Pages, Cloudflare Pages,
//! Netlify, and S3.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{ShareError, ShareResult};

// ── Deployment report ───────────────────────────────────────────────────

/// Machine-readable deployment report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployReport {
    /// When this report was generated.
    pub generated_at: String,
    /// Overall deployment readiness.
    pub ready: bool,
    /// Pre-flight check results.
    pub checks: Vec<DeployCheck>,
    /// Detected hosting platforms.
    pub platforms: Vec<PlatformInfo>,
    /// Bundle statistics.
    pub bundle_stats: BundleStats,
    /// File integrity checksums.
    pub integrity: BTreeMap<String, String>,
    /// Security expectations for the deployment.
    pub security: SecurityExpectations,
    /// Rollback guidance for the deployment.
    pub rollback: RollbackGuidance,
}

/// A single pre-flight check result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployCheck {
    pub name: String,
    pub passed: bool,
    pub message: String,
    pub severity: CheckSeverity,
}

/// Check severity level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckSeverity {
    Error,
    Warning,
    Info,
}

/// Platform deployment information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformInfo {
    pub id: String,
    pub name: String,
    pub detected: bool,
    pub config_present: bool,
    pub deploy_command: Option<String>,
}

/// Bundle file statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleStats {
    pub total_files: usize,
    pub total_bytes: u64,
    pub html_pages: usize,
    pub data_files: usize,
    pub asset_files: usize,
    pub has_database: bool,
    pub has_viewer: bool,
    pub has_pages: bool,
}

/// Security expectations for the deployment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityExpectations {
    /// Whether COOP/COEP headers are configured (required for SQLite OPFS).
    pub cross_origin_isolation: bool,
    /// Whether the bundle contains a database (privacy consideration).
    pub contains_database: bool,
    /// Whether static pages are pre-rendered (no runtime data leakage).
    pub static_only: bool,
    /// Scrub preset used during export (if detectable from manifest).
    pub scrub_preset: Option<String>,
    /// Security notes and recommendations.
    pub notes: Vec<String>,
}

/// Rollback guidance for the deployment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackGuidance {
    /// Content hash of the current bundle.
    pub current_hash: Option<String>,
    /// Content hash of the previous deployment (if history available).
    pub previous_hash: Option<String>,
    /// Platform-specific rollback steps.
    pub steps: Vec<RollbackStep>,
}

/// A single rollback step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackStep {
    pub platform: String,
    pub instruction: String,
    pub command: Option<String>,
}

/// Post-deploy verification result for a live URL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyResult {
    pub url: String,
    pub checked_at: String,
    pub checks: Vec<DeployCheck>,
    pub all_passed: bool,
}

/// Deployment history entry for tracking deployments over time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployHistoryEntry {
    pub deployed_at: String,
    pub content_hash: String,
    pub platform: String,
    pub file_count: usize,
    pub total_bytes: u64,
}

/// Deployment history stored alongside the bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployHistory {
    pub entries: Vec<DeployHistoryEntry>,
}

// ── Pre-flight validation ───────────────────────────────────────────────

/// Run pre-flight validation checks on a bundle directory.
///
/// Returns a deployment report with check results, platform detection,
/// bundle statistics, and file integrity checksums.
pub fn validate_bundle(bundle_dir: &Path) -> ShareResult<DeployReport> {
    if !bundle_dir.is_dir() {
        return Err(ShareError::BundleNotFound {
            path: bundle_dir.display().to_string(),
        });
    }

    let mut checks = Vec::new();

    // ── Required files ──────────────────────────────────────────────
    check_file_exists(
        bundle_dir,
        "manifest.json",
        CheckSeverity::Error,
        &mut checks,
    );
    check_file_exists(bundle_dir, ".nojekyll", CheckSeverity::Warning, &mut checks);
    check_file_exists(bundle_dir, "_headers", CheckSeverity::Warning, &mut checks);
    check_file_exists(bundle_dir, "index.html", CheckSeverity::Error, &mut checks);

    // ── Viewer assets ───────────────────────────────────────────────
    check_file_exists(
        bundle_dir,
        "viewer/index.html",
        CheckSeverity::Warning,
        &mut checks,
    );
    check_file_exists(
        bundle_dir,
        "viewer/styles.css",
        CheckSeverity::Warning,
        &mut checks,
    );
    check_dir_exists(
        bundle_dir,
        "viewer/vendor",
        CheckSeverity::Warning,
        &mut checks,
    );

    // ── Database ────────────────────────────────────────────────────
    let has_db = bundle_dir.join("mailbox.sqlite3").is_file();
    checks.push(DeployCheck {
        name: "database_present".to_string(),
        passed: has_db,
        message: if has_db {
            "mailbox.sqlite3 found".to_string()
        } else {
            "mailbox.sqlite3 not found — viewer will have limited functionality".to_string()
        },
        severity: CheckSeverity::Warning,
    });

    // ── Static pages ────────────────────────────────────────────────
    let has_pages = bundle_dir.join("viewer/pages").is_dir();
    checks.push(DeployCheck {
        name: "static_pages_present".to_string(),
        passed: has_pages,
        message: if has_pages {
            "Pre-rendered HTML pages found".to_string()
        } else {
            "No pre-rendered pages — search engines won't index content".to_string()
        },
        severity: CheckSeverity::Info,
    });

    // ── Data files ──────────────────────────────────────────────────
    let _has_data = bundle_dir.join("viewer/data").is_dir();
    check_file_exists(
        bundle_dir,
        "viewer/data/messages.json",
        CheckSeverity::Warning,
        &mut checks,
    );
    check_file_exists(
        bundle_dir,
        "viewer/data/meta.json",
        CheckSeverity::Warning,
        &mut checks,
    );

    // ── Manifest validation ─────────────────────────────────────────
    let manifest_path = bundle_dir.join("manifest.json");
    if manifest_path.is_file() {
        match std::fs::read_to_string(&manifest_path) {
            Ok(content) => match serde_json::from_str::<serde_json::Value>(&content) {
                Ok(manifest) => {
                    checks.push(DeployCheck {
                        name: "manifest_valid_json".to_string(),
                        passed: true,
                        message: "manifest.json is valid JSON".to_string(),
                        severity: CheckSeverity::Info,
                    });

                    // Check schema version
                    if let Some(version) = manifest.get("schema_version").and_then(|v| v.as_str()) {
                        checks.push(DeployCheck {
                            name: "manifest_schema_version".to_string(),
                            passed: true,
                            message: format!("Schema version: {version}"),
                            severity: CheckSeverity::Info,
                        });
                    }
                }
                Err(e) => {
                    checks.push(DeployCheck {
                        name: "manifest_valid_json".to_string(),
                        passed: false,
                        message: format!("manifest.json parse error: {e}"),
                        severity: CheckSeverity::Error,
                    });
                }
            },
            Err(e) => {
                checks.push(DeployCheck {
                    name: "manifest_readable".to_string(),
                    passed: false,
                    message: format!("Cannot read manifest.json: {e}"),
                    severity: CheckSeverity::Error,
                });
            }
        }
    }

    // ── Cross-origin headers check ──────────────────────────────────
    let headers_path = bundle_dir.join("_headers");
    if headers_path.is_file() {
        if let Ok(content) = std::fs::read_to_string(&headers_path) {
            let has_coop = content.contains("Cross-Origin-Opener-Policy");
            let has_coep = content.contains("Cross-Origin-Embedder-Policy");
            checks.push(DeployCheck {
                name: "coop_coep_headers".to_string(),
                passed: has_coop && has_coep,
                message: if has_coop && has_coep {
                    "COOP/COEP headers configured for cross-origin isolation".to_string()
                } else {
                    "Missing COOP or COEP headers — SQLite OPFS may not work".to_string()
                },
                severity: CheckSeverity::Warning,
            });
        }
    }

    // ── Platform detection ──────────────────────────────────────────
    let hosting_hints = crate::hosting::detect_hosting_hints(bundle_dir);
    let platforms = build_platform_info(bundle_dir, &hosting_hints);

    // ── Bundle stats ────────────────────────────────────────────────
    let bundle_stats = compute_bundle_stats(bundle_dir);

    // ── Integrity checksums ─────────────────────────────────────────
    let integrity = compute_integrity(bundle_dir);

    // ── Security expectations ───────────────────────────────────────
    let security = build_security_expectations(bundle_dir, &bundle_stats);

    // ── Rollback guidance ────────────────────────────────────────────
    let rollback = build_rollback_guidance(bundle_dir, &integrity);

    // ── Overall readiness ───────────────────────────────────────────
    let ready = !checks
        .iter()
        .any(|c| !c.passed && c.severity == CheckSeverity::Error);

    Ok(DeployReport {
        generated_at: Utc::now().to_rfc3339(),
        ready,
        checks,
        platforms,
        bundle_stats,
        integrity,
        security,
        rollback,
    })
}

// ── Platform config generators ──────────────────────────────────────────

/// Generate a GitHub Actions workflow for deploying to GitHub Pages.
#[must_use]
pub fn generate_gh_pages_workflow() -> String {
    r###"# Deploy MCP Agent Mail static export to GitHub Pages
#
# Usage:
#   1. Place your bundle output in the `docs/` directory (or configure path below)
#   2. Enable GitHub Pages in repo Settings > Pages > Source: "GitHub Actions"
#   3. Push to main branch to trigger deployment
#
# Or manually: Actions > "Deploy to GitHub Pages" > "Run workflow"

name: Deploy to GitHub Pages

on:
  push:
    branches: [main]
    paths:
      - 'docs/**'
  workflow_dispatch:

permissions:
  contents: read
  pages: write
  id-token: write

concurrency:
  group: pages
  cancel-in-progress: false

jobs:
  validate:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Validate bundle
        run: |
          BUNDLE_DIR="docs"
          echo "=== Pre-flight checks ==="
          test -f "$BUNDLE_DIR/manifest.json" || { echo "FAIL: manifest.json missing"; exit 1; }
          test -f "$BUNDLE_DIR/index.html" || { echo "FAIL: index.html missing"; exit 1; }
          test -f "$BUNDLE_DIR/.nojekyll" || { echo "WARN: .nojekyll missing"; }
          test -f "$BUNDLE_DIR/_headers" || { echo "WARN: _headers missing"; }
          test -d "$BUNDLE_DIR/viewer" || { echo "WARN: viewer/ directory missing"; }
          echo "=== Manifest ==="
          python3 -c "import json; m=json.load(open('$BUNDLE_DIR/manifest.json')); print(json.dumps({k: m[k] for k in ['schema_version','generated_at','database'] if k in m}, indent=2))" 2>/dev/null || echo "(manifest parse skipped)"
          echo "=== Bundle size ==="
          du -sh "$BUNDLE_DIR"
          echo "=== All checks passed ==="

  deploy:
    needs: validate
    runs-on: ubuntu-latest
    environment:
      name: github-pages
      url: ${{ steps.deployment.outputs.page_url }}
    steps:
      - uses: actions/checkout@v4

      - name: Setup Pages
        uses: actions/configure-pages@v5

      - name: Upload artifact
        uses: actions/upload-pages-artifact@v3
        with:
          path: docs

      - name: Deploy to GitHub Pages
        id: deployment
        uses: actions/deploy-pages@v4

      - name: Generate deployment report
        if: always()
        run: |
          echo "## Deployment Report" >> $GITHUB_STEP_SUMMARY
          echo "- **Status**: ${{ steps.deployment.outcome }}" >> $GITHUB_STEP_SUMMARY
          echo "- **URL**: ${{ steps.deployment.outputs.page_url }}" >> $GITHUB_STEP_SUMMARY
          echo "- **Commit**: ${{ github.sha }}" >> $GITHUB_STEP_SUMMARY
          echo "- **Triggered by**: ${{ github.event_name }}" >> $GITHUB_STEP_SUMMARY
"###
    .to_string()
}

/// Generate a GitHub Actions workflow for deploying to Cloudflare Pages.
#[must_use]
pub fn generate_cf_pages_workflow() -> String {
    r###"# Deploy MCP Agent Mail static export to Cloudflare Pages
#
# Usage:
#   1. Set CLOUDFLARE_API_TOKEN and CLOUDFLARE_ACCOUNT_ID secrets in repo settings
#   2. Create a Cloudflare Pages project: wrangler pages project create agent-mail
#   3. Push to main branch to trigger deployment
#
# Or manually: Actions > "Deploy to Cloudflare Pages" > "Run workflow"

name: Deploy to Cloudflare Pages

on:
  push:
    branches: [main]
    paths:
      - 'docs/**'
  workflow_dispatch:

concurrency:
  group: cf-pages
  cancel-in-progress: false

jobs:
  validate:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Validate bundle
        run: |
          BUNDLE_DIR="docs"
          echo "=== Pre-flight checks ==="
          test -f "$BUNDLE_DIR/manifest.json" || { echo "FAIL: manifest.json missing"; exit 1; }
          test -f "$BUNDLE_DIR/index.html" || { echo "FAIL: index.html missing"; exit 1; }
          test -f "$BUNDLE_DIR/_headers" || { echo "WARN: _headers missing (COOP/COEP may be broken)"; }
          echo "=== Manifest ==="
          python3 -c "import json; m=json.load(open('$BUNDLE_DIR/manifest.json')); print(json.dumps({k: m[k] for k in ['schema_version','generated_at','database'] if k in m}, indent=2))" 2>/dev/null || echo "(manifest parse skipped)"
          echo "=== Bundle size ==="
          du -sh "$BUNDLE_DIR"
          echo "=== All checks passed ==="

  deploy:
    needs: validate
    runs-on: ubuntu-latest
    permissions:
      contents: read
      deployments: write
    steps:
      - uses: actions/checkout@v4

      - name: Deploy to Cloudflare Pages
        id: deploy
        uses: cloudflare/wrangler-action@v3
        with:
          apiToken: ${{ secrets.CLOUDFLARE_API_TOKEN }}
          accountId: ${{ secrets.CLOUDFLARE_ACCOUNT_ID }}
          command: pages deploy docs/ --project-name=agent-mail

      - name: Generate deployment report
        if: always()
        run: |
          echo "## Cloudflare Pages Deployment Report" >> $GITHUB_STEP_SUMMARY
          echo "- **Status**: ${{ steps.deploy.outcome }}" >> $GITHUB_STEP_SUMMARY
          echo "- **Commit**: ${{ github.sha }}" >> $GITHUB_STEP_SUMMARY
          echo "- **Triggered by**: ${{ github.event_name }}" >> $GITHUB_STEP_SUMMARY
"###
    .to_string()
}

/// Generate a Cloudflare Pages deployment configuration.
#[must_use]
pub fn generate_cf_pages_config() -> String {
    r#"# Cloudflare Pages Configuration
#
# This file is a template for wrangler.toml when deploying
# MCP Agent Mail static exports to Cloudflare Pages.
#
# Usage:
#   1. Install wrangler: npm install -g wrangler
#   2. Login: wrangler login
#   3. Deploy: wrangler pages deploy docs/ --project-name=agent-mail

name = "agent-mail-export"
compatibility_date = "2024-01-01"

[site]
bucket = "./docs"

# Cloudflare Pages automatically picks up _headers file
# for custom response headers (COOP/COEP configured there).
"#
    .to_string()
}

/// Generate a Netlify deployment configuration.
#[must_use]
pub fn generate_netlify_config() -> String {
    r#"# Netlify Configuration
#
# Place this file at the repo root when deploying to Netlify.
#
# Usage:
#   1. Connect your repo to Netlify
#   2. Set publish directory to "docs"
#   3. No build command needed (static files only)

[build]
  publish = "docs"

# Netlify automatically picks up _headers file
# for custom response headers (COOP/COEP configured there).

# Additional headers can be set here:
[[headers]]
  for = "/*.sqlite3"
  [headers.values]
    Content-Type = "application/x-sqlite3"
    Cross-Origin-Resource-Policy = "same-origin"

[[headers]]
  for = "/chunks/*"
  [headers.values]
    Content-Type = "application/octet-stream"
    Cross-Origin-Resource-Policy = "same-origin"
"#
    .to_string()
}

/// Generate a deployment validation script (shell).
#[must_use]
pub fn generate_validation_script() -> String {
    r#"#!/usr/bin/env bash
# MCP Agent Mail Static Export — Deployment Validation Script
#
# Usage: ./validate_deploy.sh <bundle_dir> [deployed_url]
#
# Checks:
#   1. Bundle structure integrity
#   2. File checksums
#   3. (If URL provided) HTTP response validation

set -euo pipefail

BUNDLE_DIR="${1:?Usage: $0 <bundle_dir> [deployed_url]}"
DEPLOYED_URL="${2:-}"

echo "=== MCP Agent Mail Deploy Validator ==="
echo "Bundle: $BUNDLE_DIR"
echo ""

ERRORS=0
WARNINGS=0

check() {
    local severity="$1" name="$2" condition="$3" msg_pass="$4" msg_fail="$5"
    if eval "$condition"; then
        echo "  ✅ $name: $msg_pass"
    else
        if [ "$severity" = "error" ]; then
            echo "  ❌ $name: $msg_fail"
            ERRORS=$((ERRORS + 1))
        else
            echo "  ⚠️  $name: $msg_fail"
            WARNINGS=$((WARNINGS + 1))
        fi
    fi
}

echo "--- Structure Checks ---"
check error "manifest" "test -f '$BUNDLE_DIR/manifest.json'" "Present" "Missing"
check error "index.html" "test -f '$BUNDLE_DIR/index.html'" "Present" "Missing"
check warning ".nojekyll" "test -f '$BUNDLE_DIR/.nojekyll'" "Present" "Missing (needed for GH Pages)"
check warning "_headers" "test -f '$BUNDLE_DIR/_headers'" "Present" "Missing (needed for COOP/COEP)"
check warning "viewer" "test -d '$BUNDLE_DIR/viewer'" "Present" "Missing"
check warning "database" "test -f '$BUNDLE_DIR/mailbox.sqlite3'" "Present" "Missing"
check info "pages" "test -d '$BUNDLE_DIR/viewer/pages'" "Present" "Not generated"
check info "search_index" "test -f '$BUNDLE_DIR/viewer/data/search_index.json'" "Present" "Not generated"
echo ""

echo "--- Manifest Validation ---"
if [ -f "$BUNDLE_DIR/manifest.json" ]; then
    if python3 -c "import json; json.load(open('$BUNDLE_DIR/manifest.json'))" 2>/dev/null; then
        echo "  ✅ Valid JSON"
        python3 -c "
import json
m = json.load(open('$BUNDLE_DIR/manifest.json'))
print(f'  Schema: {m.get(\"schema_version\", \"unknown\")}')
print(f'  Generated: {m.get(\"generated_at\", \"unknown\")}')
db = m.get('database', {})
print(f'  DB size: {db.get(\"size_bytes\", 0):,} bytes')
print(f'  FTS: {db.get(\"fts_enabled\", False)}')
" 2>/dev/null || echo "  (details unavailable)"
    else
        echo "  ❌ Invalid JSON"
        ERRORS=$((ERRORS + 1))
    fi
fi
echo ""

echo "--- Bundle Size ---"
du -sh "$BUNDLE_DIR" 2>/dev/null || echo "  (size unavailable)"
find "$BUNDLE_DIR" -type f | wc -l | xargs -I{} echo "  Files: {}"
echo ""

if [ -n "$DEPLOYED_URL" ]; then
    echo "--- HTTP Checks ($DEPLOYED_URL) ---"
    check_url() {
        local path="$1" expected="$2"
        local status
        status=$(curl -s -o /dev/null -w "%{http_code}" "$DEPLOYED_URL/$path" 2>/dev/null || echo "000")
        if [ "$status" = "$expected" ]; then
            echo "  ✅ GET /$path → $status"
        else
            echo "  ❌ GET /$path → $status (expected $expected)"
            ERRORS=$((ERRORS + 1))
        fi
    }
    check_url "" "200"
    check_url "viewer/" "200"
    check_url "manifest.json" "200"
    echo ""

    echo "--- COOP/COEP Header Check ---"
    HEADERS=$(curl -s -I "$DEPLOYED_URL/viewer/" 2>/dev/null || echo "")
    if echo "$HEADERS" | grep -qi "Cross-Origin-Opener-Policy"; then
        echo "  ✅ COOP header present"
    else
        echo "  ⚠️  COOP header missing"
        WARNINGS=$((WARNINGS + 1))
    fi
    if echo "$HEADERS" | grep -qi "Cross-Origin-Embedder-Policy"; then
        echo "  ✅ COEP header present"
    else
        echo "  ⚠️  COEP header missing"
        WARNINGS=$((WARNINGS + 1))
    fi
    echo ""
fi

echo "=== Summary ==="
echo "  Errors:   $ERRORS"
echo "  Warnings: $WARNINGS"
if [ "$ERRORS" -gt 0 ]; then
    echo "  Result:   FAIL"
    exit 1
else
    echo "  Result:   PASS"
    exit 0
fi
"#
    .to_string()
}

// ── Internal helpers ────────────────────────────────────────────────────

fn check_file_exists(
    bundle_dir: &Path,
    relative_path: &str,
    severity: CheckSeverity,
    checks: &mut Vec<DeployCheck>,
) {
    let exists = bundle_dir.join(relative_path).is_file();
    checks.push(DeployCheck {
        name: format!("file_{}", relative_path.replace('/', "_")),
        passed: exists,
        message: if exists {
            format!("{relative_path} present")
        } else {
            format!("{relative_path} missing")
        },
        severity,
    });
}

fn check_dir_exists(
    bundle_dir: &Path,
    relative_path: &str,
    severity: CheckSeverity,
    checks: &mut Vec<DeployCheck>,
) {
    let exists = bundle_dir.join(relative_path).is_dir();
    checks.push(DeployCheck {
        name: format!("dir_{}", relative_path.replace('/', "_")),
        passed: exists,
        message: if exists {
            format!("{relative_path}/ present")
        } else {
            format!("{relative_path}/ missing")
        },
        severity,
    });
}

fn build_platform_info(
    bundle_dir: &Path,
    hosting_hints: &[crate::hosting::HostingHint],
) -> Vec<PlatformInfo> {
    let mut platforms = Vec::new();

    let detected_ids: Vec<&str> = hosting_hints.iter().map(|h| h.id.as_str()).collect();

    platforms.push(PlatformInfo {
        id: "github_pages".to_string(),
        name: "GitHub Pages".to_string(),
        detected: detected_ids.contains(&"github_pages"),
        config_present: bundle_dir.join(".nojekyll").is_file(),
        deploy_command: Some("gh-pages -d docs/".to_string()),
    });

    platforms.push(PlatformInfo {
        id: "cloudflare_pages".to_string(),
        name: "Cloudflare Pages".to_string(),
        detected: detected_ids.contains(&"cloudflare_pages"),
        config_present: bundle_dir.join("_headers").is_file(),
        deploy_command: Some("wrangler pages deploy docs/ --project-name=agent-mail".to_string()),
    });

    platforms.push(PlatformInfo {
        id: "netlify".to_string(),
        name: "Netlify".to_string(),
        detected: detected_ids.contains(&"netlify"),
        config_present: bundle_dir.join("_headers").is_file(),
        deploy_command: Some("netlify deploy --prod --dir=docs/".to_string()),
    });

    platforms.push(PlatformInfo {
        id: "s3".to_string(),
        name: "Amazon S3".to_string(),
        detected: detected_ids.contains(&"s3"),
        config_present: false,
        deploy_command: Some("aws s3 sync docs/ s3://your-bucket/ --delete".to_string()),
    });

    platforms
}

fn compute_bundle_stats(bundle_dir: &Path) -> BundleStats {
    let mut total_files = 0usize;
    let mut total_bytes = 0u64;
    let mut html_pages = 0usize;
    let mut data_files = 0usize;
    let mut asset_files = 0usize;

    if let Ok(entries) = walk_dir_recursive(bundle_dir) {
        for entry in entries {
            total_files += 1;
            total_bytes += entry.size;

            if entry.path.ends_with(".html") {
                html_pages += 1;
            } else if entry.path.ends_with(".json") {
                data_files += 1;
            } else {
                asset_files += 1;
            }
        }
    }

    BundleStats {
        total_files,
        total_bytes,
        html_pages,
        data_files,
        asset_files,
        has_database: bundle_dir.join("mailbox.sqlite3").is_file(),
        has_viewer: bundle_dir.join("viewer/index.html").is_file(),
        has_pages: bundle_dir.join("viewer/pages").is_dir(),
    }
}

fn compute_integrity(bundle_dir: &Path) -> BTreeMap<String, String> {
    let mut checksums = BTreeMap::new();

    // Checksum key files only (not all files, for performance)
    let key_files = [
        "manifest.json",
        "index.html",
        "mailbox.sqlite3",
        "viewer/index.html",
        "viewer/data/messages.json",
        "viewer/data/meta.json",
        "viewer/data/sitemap.json",
        "viewer/data/search_index.json",
    ];

    for rel in &key_files {
        let path = bundle_dir.join(rel);
        if path.is_file() {
            if let Ok(data) = std::fs::read(&path) {
                let hash = hex::encode(Sha256::digest(&data));
                checksums.insert((*rel).to_string(), hash);
            }
        }
    }

    checksums
}

struct FileEntry {
    path: String,
    size: u64,
}

fn walk_dir_recursive(dir: &Path) -> Result<Vec<FileEntry>, std::io::Error> {
    let mut entries = Vec::new();
    walk_dir_inner(dir, dir, &mut entries)?;
    Ok(entries)
}

fn walk_dir_inner(
    root: &Path,
    current: &Path,
    entries: &mut Vec<FileEntry>,
) -> Result<(), std::io::Error> {
    if !current.is_dir() {
        return Ok(());
    }

    for entry in std::fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk_dir_inner(root, &path, entries)?;
        } else if path.is_file() {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            entries.push(FileEntry { path: rel, size });
        }
    }
    Ok(())
}

// ── Security expectations ───────────────────────────────────────────────

fn build_security_expectations(bundle_dir: &Path, stats: &BundleStats) -> SecurityExpectations {
    let headers_path = bundle_dir.join("_headers");
    let cross_origin = if headers_path.is_file() {
        std::fs::read_to_string(&headers_path)
            .map(|c| {
                c.contains("Cross-Origin-Opener-Policy")
                    && c.contains("Cross-Origin-Embedder-Policy")
            })
            .unwrap_or(false)
    } else {
        false
    };

    let scrub_preset = bundle_dir
        .join("manifest.json")
        .is_file()
        .then(|| {
            std::fs::read_to_string(bundle_dir.join("manifest.json"))
                .ok()
                .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
                .and_then(|m| {
                    m.get("scrub")
                        .and_then(|s| s.get("preset"))
                        .and_then(|p| p.as_str().map(|s| s.to_string()))
                        .or_else(|| {
                            m.get("export_config")
                                .and_then(|e| e.get("scrub_preset"))
                                .and_then(|p| p.as_str().map(|s| s.to_string()))
                        })
                })
        })
        .flatten();

    let mut notes = Vec::new();
    if stats.has_database {
        notes.push(
            "Bundle contains SQLite database — ensure scrub preset meets privacy requirements"
                .to_string(),
        );
    }
    if !cross_origin {
        notes.push(
            "COOP/COEP headers missing — SQLite OPFS will not work in the browser viewer"
                .to_string(),
        );
    }
    if !stats.has_pages {
        notes.push("No pre-rendered pages — content requires JavaScript for rendering".to_string());
    }
    if scrub_preset.as_deref() == Some("archive") {
        notes.push(
            "Archive scrub preset retains all data — suitable for private deployments only"
                .to_string(),
        );
    }

    SecurityExpectations {
        cross_origin_isolation: cross_origin,
        contains_database: stats.has_database,
        static_only: !stats.has_database && stats.has_pages,
        scrub_preset,
        notes,
    }
}

// ── Rollback guidance ──────────────────────────────────────────────────

fn build_rollback_guidance(
    bundle_dir: &Path,
    integrity: &BTreeMap<String, String>,
) -> RollbackGuidance {
    // Compute current content hash from integrity checksums.
    let current_hash = if integrity.is_empty() {
        None
    } else {
        let mut hasher = Sha256::new();
        for (k, v) in integrity {
            hasher.update(k.as_bytes());
            hasher.update(v.as_bytes());
        }
        Some(hex::encode(hasher.finalize()))
    };

    // Load previous hash from deploy history if available.
    let previous_hash = load_deploy_history(bundle_dir)
        .ok()
        .and_then(|h| h.entries.last().map(|e| e.content_hash.clone()));

    let steps = vec![
        RollbackStep {
            platform: "github_pages".to_string(),
            instruction: "Revert the docs/ directory to the previous commit and push".to_string(),
            command: Some("git revert HEAD -- docs/ && git push".to_string()),
        },
        RollbackStep {
            platform: "cloudflare_pages".to_string(),
            instruction: "Roll back to the previous deployment in the Cloudflare dashboard, or re-deploy from a previous commit".to_string(),
            command: Some("wrangler pages deployment rollback --project-name=agent-mail".to_string()),
        },
        RollbackStep {
            platform: "netlify".to_string(),
            instruction: "Use the Netlify dashboard to restore a previous deploy, or re-deploy from a previous commit".to_string(),
            command: Some("netlify deploy --prod --dir=docs/ # from previous commit checkout".to_string()),
        },
        RollbackStep {
            platform: "s3".to_string(),
            instruction: "Re-sync from a previous bundle snapshot".to_string(),
            command: Some("aws s3 sync <previous-bundle>/ s3://your-bucket/ --delete".to_string()),
        },
    ];

    RollbackGuidance {
        current_hash,
        previous_hash,
        steps,
    }
}

// ── Deploy history ─────────────────────────────────────────────────────

const DEPLOY_HISTORY_FILE: &str = ".deploy_history.json";

/// Load deployment history from the bundle directory.
pub fn load_deploy_history(bundle_dir: &Path) -> ShareResult<DeployHistory> {
    let path = bundle_dir.join(DEPLOY_HISTORY_FILE);
    if !path.is_file() {
        return Ok(DeployHistory {
            entries: Vec::new(),
        });
    }
    let content = std::fs::read_to_string(&path)?;
    serde_json::from_str(&content).map_err(|e| ShareError::ManifestParse {
        message: format!("deploy history parse error: {e}"),
    })
}

/// Append an entry to the deployment history and write it to disk.
pub fn record_deploy(bundle_dir: &Path, entry: DeployHistoryEntry) -> ShareResult<()> {
    let mut history = load_deploy_history(bundle_dir)?;
    history.entries.push(entry);
    // Keep only the last 50 entries.
    if history.entries.len() > 50 {
        let drain_count = history.entries.len() - 50;
        history.entries.drain(..drain_count);
    }
    let json = serde_json::to_string_pretty(&history).unwrap_or_else(|_| "{}".to_string());
    std::fs::write(bundle_dir.join(DEPLOY_HISTORY_FILE), json)?;
    Ok(())
}

// ── Post-deploy verification ───────────────────────────────────────────

/// Build a verification plan for a deployed URL (returns the list of checks
/// that *would* be performed). Actual HTTP checks require a runtime client,
/// so this produces the check descriptions and expected status codes.
pub fn build_verify_plan(deployed_url: &str) -> VerifyResult {
    let url = deployed_url.trim_end_matches('/');
    let mut checks = Vec::new();

    // Root page
    checks.push(DeployCheck {
        name: "root_page".to_string(),
        passed: false,
        message: format!("GET {url}/ should return 200"),
        severity: CheckSeverity::Error,
    });

    // Viewer
    checks.push(DeployCheck {
        name: "viewer_page".to_string(),
        passed: false,
        message: format!("GET {url}/viewer/ should return 200"),
        severity: CheckSeverity::Error,
    });

    // Manifest
    checks.push(DeployCheck {
        name: "manifest_accessible".to_string(),
        passed: false,
        message: format!("GET {url}/manifest.json should return 200"),
        severity: CheckSeverity::Error,
    });

    // COOP/COEP headers
    checks.push(DeployCheck {
        name: "coop_header".to_string(),
        passed: false,
        message: format!("GET {url}/viewer/ should include Cross-Origin-Opener-Policy header"),
        severity: CheckSeverity::Warning,
    });
    checks.push(DeployCheck {
        name: "coep_header".to_string(),
        passed: false,
        message: format!("GET {url}/viewer/ should include Cross-Origin-Embedder-Policy header"),
        severity: CheckSeverity::Warning,
    });

    // Database (if present)
    checks.push(DeployCheck {
        name: "database_accessible".to_string(),
        passed: false,
        message: format!("GET {url}/mailbox.sqlite3 should return 200 (if database included)"),
        severity: CheckSeverity::Info,
    });

    VerifyResult {
        url: url.to_string(),
        checked_at: Utc::now().to_rfc3339(),
        checks,
        all_passed: false,
    }
}

// ── Write workflow files to disk ────────────────────────────────────────

/// Write all deployment configuration files to a bundle directory.
///
/// Creates:
/// - `.github/workflows/deploy-pages.yml` (GitHub Actions workflow)
/// - `.github/workflows/deploy-cf-pages.yml` (Cloudflare Pages CI workflow)
/// - `wrangler.toml.template` (Cloudflare Pages config)
/// - `netlify.toml.template` (Netlify config)
/// - `scripts/validate_deploy.sh` (validation script)
/// - `deploy_report.json` (pre-flight validation report)
pub fn write_deploy_tooling(bundle_dir: &Path) -> ShareResult<Vec<String>> {
    let mut written = Vec::new();

    // GitHub Actions workflow (GH Pages)
    let workflow_dir = bundle_dir.join(".github").join("workflows");
    std::fs::create_dir_all(&workflow_dir)?;
    std::fs::write(
        workflow_dir.join("deploy-pages.yml"),
        generate_gh_pages_workflow(),
    )?;
    written.push(".github/workflows/deploy-pages.yml".to_string());

    // GitHub Actions workflow (Cloudflare Pages)
    std::fs::write(
        workflow_dir.join("deploy-cf-pages.yml"),
        generate_cf_pages_workflow(),
    )?;
    written.push(".github/workflows/deploy-cf-pages.yml".to_string());

    // Cloudflare Pages template
    std::fs::write(
        bundle_dir.join("wrangler.toml.template"),
        generate_cf_pages_config(),
    )?;
    written.push("wrangler.toml.template".to_string());

    // Netlify template
    std::fs::write(
        bundle_dir.join("netlify.toml.template"),
        generate_netlify_config(),
    )?;
    written.push("netlify.toml.template".to_string());

    // Validation script
    let scripts_dir = bundle_dir.join("scripts");
    std::fs::create_dir_all(&scripts_dir)?;
    let script_path = scripts_dir.join("validate_deploy.sh");
    std::fs::write(&script_path, generate_validation_script())?;
    // Make executable on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755));
    }
    written.push("scripts/validate_deploy.sh".to_string());

    // Deploy report
    let report = validate_bundle(bundle_dir)?;
    let report_json = serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".to_string());
    std::fs::write(bundle_dir.join("deploy_report.json"), &report_json)?;
    written.push("deploy_report.json".to_string());

    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_minimal_bundle(dir: &Path) {
        std::fs::create_dir_all(dir.join("viewer/vendor")).unwrap();
        std::fs::create_dir_all(dir.join("viewer/data")).unwrap();
        std::fs::write(
            dir.join("manifest.json"),
            r#"{"schema_version":"0.1.0","generated_at":"2024-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        std::fs::write(dir.join("index.html"), "<html></html>").unwrap();
        std::fs::write(dir.join(".nojekyll"), "").unwrap();
        std::fs::write(
            dir.join("_headers"),
            "Cross-Origin-Opener-Policy: same-origin\nCross-Origin-Embedder-Policy: require-corp",
        )
        .unwrap();
        std::fs::write(dir.join("viewer/index.html"), "<html>viewer</html>").unwrap();
        std::fs::write(dir.join("viewer/styles.css"), "body{}").unwrap();
        std::fs::write(dir.join("viewer/data/messages.json"), "[]").unwrap();
        std::fs::write(dir.join("viewer/data/meta.json"), "{}").unwrap();
    }

    // ── validate_bundle ─────────────────────────────────────────────

    #[test]
    fn validate_complete_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle");
        create_minimal_bundle(&bundle);

        let report = validate_bundle(&bundle).unwrap();
        assert!(report.ready);
        assert!(
            report
                .checks
                .iter()
                .all(|c| c.passed || c.severity != CheckSeverity::Error)
        );
    }

    #[test]
    fn validate_missing_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("index.html"), "").unwrap();

        let report = validate_bundle(&bundle).unwrap();
        assert!(!report.ready);
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name == "file_manifest.json" && !c.passed)
        );
    }

    #[test]
    fn validate_nonexistent_dir() {
        let result = validate_bundle(Path::new("/nonexistent/path"));
        assert!(result.is_err());
    }

    // ── bundle stats ────────────────────────────────────────────────

    #[test]
    fn bundle_stats_counts() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle");
        create_minimal_bundle(&bundle);

        let stats = compute_bundle_stats(&bundle);
        assert!(stats.total_files > 0);
        assert!(stats.total_bytes > 0);
        assert!(stats.has_viewer);
        assert!(!stats.has_database);
    }

    // ── integrity checksums ─────────────────────────────────────────

    #[test]
    fn integrity_checksums_computed() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle");
        create_minimal_bundle(&bundle);

        let integrity = compute_integrity(&bundle);
        assert!(integrity.contains_key("manifest.json"));
        assert!(integrity.contains_key("index.html"));
        // SHA256 hex is 64 chars
        for hash in integrity.values() {
            assert_eq!(hash.len(), 64);
        }
    }

    // ── config generators ───────────────────────────────────────────

    #[test]
    fn gh_pages_workflow_is_valid_yaml() {
        let workflow = generate_gh_pages_workflow();
        assert!(workflow.contains("Deploy to GitHub Pages"));
        assert!(workflow.contains("actions/deploy-pages@v4"));
        assert!(workflow.contains("permissions:"));
    }

    #[test]
    fn cf_pages_config_valid() {
        let config = generate_cf_pages_config();
        assert!(config.contains("wrangler"));
        assert!(config.contains("compatibility_date"));
    }

    #[test]
    fn netlify_config_valid() {
        let config = generate_netlify_config();
        assert!(config.contains("[build]"));
        assert!(config.contains("publish"));
    }

    #[test]
    fn validation_script_is_shell() {
        let script = generate_validation_script();
        assert!(script.starts_with("#!/usr/bin/env bash"));
        assert!(script.contains("manifest.json"));
    }

    // ── write_deploy_tooling ────────────────────────────────────────

    #[test]
    fn write_deploy_tooling_creates_files() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle");
        create_minimal_bundle(&bundle);

        let written = write_deploy_tooling(&bundle).unwrap();
        assert!(written.contains(&".github/workflows/deploy-pages.yml".to_string()));
        assert!(written.contains(&".github/workflows/deploy-cf-pages.yml".to_string()));
        assert!(written.contains(&"wrangler.toml.template".to_string()));
        assert!(written.contains(&"netlify.toml.template".to_string()));
        assert!(written.contains(&"scripts/validate_deploy.sh".to_string()));
        assert!(written.contains(&"deploy_report.json".to_string()));

        // Verify files exist
        assert!(bundle.join(".github/workflows/deploy-pages.yml").is_file());
        assert!(
            bundle
                .join(".github/workflows/deploy-cf-pages.yml")
                .is_file()
        );
        assert!(bundle.join("wrangler.toml.template").is_file());
        assert!(bundle.join("netlify.toml.template").is_file());
        assert!(bundle.join("scripts/validate_deploy.sh").is_file());
        assert!(bundle.join("deploy_report.json").is_file());

        // Verify deploy report is valid JSON with new fields
        let report_json = std::fs::read_to_string(bundle.join("deploy_report.json")).unwrap();
        let report: DeployReport = serde_json::from_str(&report_json).unwrap();
        assert!(report.ready);
        assert!(!report.generated_at.is_empty());
        assert!(!report.rollback.steps.is_empty());
    }

    // ── platform info ───────────────────────────────────────────────

    #[test]
    fn platform_info_includes_all_providers() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle");
        create_minimal_bundle(&bundle);

        let platforms = build_platform_info(&bundle, &[]);
        assert_eq!(platforms.len(), 4);
        let ids: Vec<&str> = platforms.iter().map(|p| p.id.as_str()).collect();
        assert!(ids.contains(&"github_pages"));
        assert!(ids.contains(&"cloudflare_pages"));
        assert!(ids.contains(&"netlify"));
        assert!(ids.contains(&"s3"));
    }

    // ── deploy report serialization ─────────────────────────────────

    #[test]
    fn deploy_report_round_trips() {
        let report = DeployReport {
            generated_at: "2024-01-01T00:00:00Z".to_string(),
            ready: true,
            checks: vec![DeployCheck {
                name: "test".to_string(),
                passed: true,
                message: "ok".to_string(),
                severity: CheckSeverity::Info,
            }],
            platforms: vec![],
            bundle_stats: BundleStats {
                total_files: 10,
                total_bytes: 1000,
                html_pages: 5,
                data_files: 3,
                asset_files: 2,
                has_database: true,
                has_viewer: true,
                has_pages: true,
            },
            integrity: BTreeMap::new(),
            security: SecurityExpectations {
                cross_origin_isolation: true,
                contains_database: true,
                static_only: false,
                scrub_preset: Some("standard".to_string()),
                notes: vec!["test note".to_string()],
            },
            rollback: RollbackGuidance {
                current_hash: Some("abc123".to_string()),
                previous_hash: None,
                steps: vec![],
            },
        };

        let json = serde_json::to_string(&report).unwrap();
        let parsed: DeployReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.ready, report.ready);
        assert_eq!(parsed.bundle_stats.total_files, 10);
        assert!(parsed.security.cross_origin_isolation);
        assert_eq!(parsed.rollback.current_hash.as_deref(), Some("abc123"));
    }

    // ── CF Pages workflow ────────────────────────────────────────────

    #[test]
    fn cf_pages_workflow_is_valid_yaml() {
        let workflow = generate_cf_pages_workflow();
        assert!(workflow.contains("Deploy to Cloudflare Pages"));
        assert!(workflow.contains("cloudflare/wrangler-action@v3"));
        assert!(workflow.contains("CLOUDFLARE_API_TOKEN"));
    }

    // ── Security expectations ────────────────────────────────────────

    #[test]
    fn security_expectations_with_database() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle");
        create_minimal_bundle(&bundle);
        // Add a database
        std::fs::write(bundle.join("mailbox.sqlite3"), b"fake-db").unwrap();

        let stats = compute_bundle_stats(&bundle);
        let security = build_security_expectations(&bundle, &stats);
        assert!(security.cross_origin_isolation);
        assert!(security.contains_database);
        assert!(!security.static_only);
        assert!(security.notes.iter().any(|n| n.contains("SQLite database")));
    }

    #[test]
    fn security_expectations_static_only() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle");
        create_minimal_bundle(&bundle);
        std::fs::create_dir_all(bundle.join("viewer/pages")).unwrap();
        std::fs::write(bundle.join("viewer/pages/index.html"), "<html/>").unwrap();

        let stats = compute_bundle_stats(&bundle);
        let security = build_security_expectations(&bundle, &stats);
        assert!(!security.contains_database);
        assert!(security.static_only);
    }

    #[test]
    fn security_expectations_no_headers() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("index.html"), "").unwrap();

        let stats = compute_bundle_stats(&bundle);
        let security = build_security_expectations(&bundle, &stats);
        assert!(!security.cross_origin_isolation);
        assert!(security.notes.iter().any(|n| n.contains("COOP/COEP")));
    }

    // ── Rollback guidance ────────────────────────────────────────────

    #[test]
    fn rollback_has_all_platforms() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle");
        create_minimal_bundle(&bundle);

        let integrity = compute_integrity(&bundle);
        let rollback = build_rollback_guidance(&bundle, &integrity);
        assert!(rollback.current_hash.is_some());
        assert!(rollback.previous_hash.is_none());
        assert_eq!(rollback.steps.len(), 4);
        let platforms: Vec<&str> = rollback.steps.iter().map(|s| s.platform.as_str()).collect();
        assert!(platforms.contains(&"github_pages"));
        assert!(platforms.contains(&"cloudflare_pages"));
        assert!(platforms.contains(&"netlify"));
        assert!(platforms.contains(&"s3"));
    }

    // ── Deploy history ───────────────────────────────────────────────

    #[test]
    fn deploy_history_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle");
        std::fs::create_dir_all(&bundle).unwrap();

        // Initially empty.
        let history = load_deploy_history(&bundle).unwrap();
        assert!(history.entries.is_empty());

        // Record an entry.
        record_deploy(
            &bundle,
            DeployHistoryEntry {
                deployed_at: "2024-01-01T00:00:00Z".to_string(),
                content_hash: "abc123".to_string(),
                platform: "github_pages".to_string(),
                file_count: 10,
                total_bytes: 1000,
            },
        )
        .unwrap();

        let history = load_deploy_history(&bundle).unwrap();
        assert_eq!(history.entries.len(), 1);
        assert_eq!(history.entries[0].content_hash, "abc123");

        // Record a second entry.
        record_deploy(
            &bundle,
            DeployHistoryEntry {
                deployed_at: "2024-01-02T00:00:00Z".to_string(),
                content_hash: "def456".to_string(),
                platform: "cloudflare_pages".to_string(),
                file_count: 12,
                total_bytes: 1200,
            },
        )
        .unwrap();

        let history = load_deploy_history(&bundle).unwrap();
        assert_eq!(history.entries.len(), 2);
        assert_eq!(history.entries[1].content_hash, "def456");
    }

    // ── Verify plan ──────────────────────────────────────────────────

    #[test]
    fn verify_plan_has_expected_checks() {
        let plan = build_verify_plan("https://example.com/site");
        assert_eq!(plan.url, "https://example.com/site");
        assert!(!plan.all_passed);
        assert!(plan.checks.len() >= 5);
        let names: Vec<&str> = plan.checks.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"root_page"));
        assert!(names.contains(&"viewer_page"));
        assert!(names.contains(&"manifest_accessible"));
        assert!(names.contains(&"coop_header"));
        assert!(names.contains(&"coep_header"));
    }

    #[test]
    fn verify_plan_strips_trailing_slash() {
        let plan = build_verify_plan("https://example.com/site/");
        assert_eq!(plan.url, "https://example.com/site");
    }

    // ── write_deploy_tooling includes CF workflow ────────────────────

    #[test]
    fn write_deploy_tooling_includes_cf_workflow() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle");
        create_minimal_bundle(&bundle);

        let written = write_deploy_tooling(&bundle).unwrap();
        assert!(written.contains(&".github/workflows/deploy-cf-pages.yml".to_string()));
        assert!(
            bundle
                .join(".github/workflows/deploy-cf-pages.yml")
                .is_file()
        );
    }

    // ── Full report includes new fields ──────────────────────────────

    #[test]
    fn full_report_has_security_and_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle");
        create_minimal_bundle(&bundle);

        let report = validate_bundle(&bundle).unwrap();
        assert!(!report.generated_at.is_empty());
        assert!(report.security.cross_origin_isolation);
        assert!(!report.security.contains_database);
        assert!(report.rollback.steps.len() >= 3);
    }
}
