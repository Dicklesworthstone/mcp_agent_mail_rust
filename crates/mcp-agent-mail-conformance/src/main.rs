#![forbid(unsafe_code)]

use clap::{Args, Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Parser)]
#[command(
    name = "mcp-agent-mail-conformance",
    about = "Conformance utilities for the MCP Agent Mail Rust port."
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Regenerate conformance fixtures using the legacy Python reference generator.
    Regen(RegenArgs),
}

#[derive(Debug, Args)]
struct RegenArgs {
    /// Python interpreter to run the legacy fixture generator.
    ///
    /// Defaults to the bundled legacy venv interpreter if present, otherwise `python3`.
    #[arg(long)]
    python: Option<String>,

    /// Path to the Python fixture generator script.
    #[arg(long)]
    script: Option<PathBuf>,

    /// Output path for the generated fixtures JSON.
    ///
    /// Defaults to a timestamped path under the OS temp dir (so running `regen` does not
    /// implicitly dirty the repo). To update the tracked fixtures, pass `--output` explicitly.
    #[arg(long)]
    output: Option<PathBuf>,

    /// Optional path used by the fixture generator to initialize temporary git repos.
    /// Sets `AM_FIXTURE_REPO_ROOT` for the child Python process.
    #[arg(long)]
    fixture_repo_root: Option<PathBuf>,

    /// Optional scratch root for the Python fixture generator run dir.
    ///
    /// This sets `MCP_AGENT_MAIL_CONFORMANCE_SCRATCH_ROOT` for the child Python process.
    /// By default, the generator uses the OS temp dir (never inside the repo).
    #[arg(long)]
    scratch_root: Option<PathBuf>,

    /// Print the command/environment without executing.
    #[arg(long)]
    dry_run: bool,
}

fn main() -> ExitCode {
    if let Err(e) = run() {
        eprintln!("[conformance] ERROR: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Regen(args) => run_regen(args),
    }
}

fn run_regen(args: RegenArgs) -> Result<(), Box<dyn std::error::Error>> {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = crate_dir
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| other_error("failed to resolve repo root from CARGO_MANIFEST_DIR"))?;

    let default_python_venv =
        repo_root.join("legacy_python_mcp_agent_mail_code/mcp_agent_mail/.venv/bin/python");
    let python = match args.python {
        Some(v) => v,
        None => {
            if default_python_venv.exists() {
                default_python_venv.to_string_lossy().to_string()
            } else {
                "python3".to_string()
            }
        }
    };

    let script = args.script.unwrap_or_else(|| {
        crate_dir.join("tests/conformance/python_reference/generate_fixtures.py")
    });
    if !script.exists() {
        return Err(other_error(format!(
            "fixture generator script not found at {}",
            script.display()
        ))
        .into());
    }

    let output = match &args.output {
        Some(v) => v.clone(),
        None => default_regen_output_path(),
    };
    let output = absolute_path(&output)?;

    let fixture_repo_root = args
        .fixture_repo_root
        .map(|p| absolute_path(&p))
        .transpose()?;

    let scratch_root = args.scratch_root.map(|p| absolute_path(&p)).transpose()?;

    eprintln!("[conformance] python:  {python}");
    eprintln!("[conformance] script:  {}", script.display());
    eprintln!("[conformance] output:  {}", output.display());
    if args.output.is_none() {
        eprintln!(
            "[conformance] note: default output is a temp file; to update the tracked fixtures, pass: --output {}",
            crate_dir
                .join(mcp_agent_mail_conformance::FIXTURE_PATH)
                .display()
        );
    }
    if let Some(root) = &fixture_repo_root {
        eprintln!("[conformance] AM_FIXTURE_REPO_ROOT: {}", root.display());
    }
    if let Some(root) = &scratch_root {
        eprintln!(
            "[conformance] MCP_AGENT_MAIL_CONFORMANCE_SCRATCH_ROOT: {}",
            root.display()
        );
    }

    if args.dry_run {
        eprintln!("[conformance] dry-run: not executing");
        return Ok(());
    }

    // Ensure output directory exists for non-default use cases.
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut cmd = Command::new(&python);
    cmd.arg(&script)
        .env(
            "MCP_AGENT_MAIL_CONFORMANCE_FIXTURE_PATH",
            output.to_string_lossy().to_string(),
        )
        .env("PYTHONUNBUFFERED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    if let Some(root) = &fixture_repo_root {
        cmd.env("AM_FIXTURE_REPO_ROOT", root.to_string_lossy().to_string());
    }
    if let Some(root) = &scratch_root {
        cmd.env(
            "MCP_AGENT_MAIL_CONFORMANCE_SCRATCH_ROOT",
            root.to_string_lossy().to_string(),
        );
    }

    let status = cmd.status()?;
    if !status.success() {
        return Err(other_error(format!(
            "fixture generator failed: command={python} {} exit_status={status}",
            script.display()
        ))
        .into());
    }

    let fixtures = mcp_agent_mail_conformance::Fixtures::load(&output)?;
    eprintln!(
        "[conformance] fixtures ok: tools={} resources={}",
        fixtures.tools.len(),
        fixtures.resources.len()
    );

    Ok(())
}

fn absolute_path(path: &Path) -> Result<PathBuf, std::io::Error> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir()?.join(path))
}

fn default_regen_output_path() -> PathBuf {
    let now_ms: u128 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let pid = std::process::id();

    std::env::temp_dir()
        .join("mcp-agent-mail-conformance")
        .join(format!("python_reference_{now_ms}_{pid}.json"))
}

fn other_error(msg: impl Into<String>) -> std::io::Error {
    std::io::Error::other(msg.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_regen_dry_run() {
        let cli = Cli::try_parse_from(["amc", "regen", "--dry-run"]).expect("parse cli");
        match cli.command {
            Commands::Regen(args) => assert!(args.dry_run),
        }
    }

    #[test]
    fn python_generator_defaults_to_os_temp_scratch() {
        // Guard against regressions where the generator writes into the committed
        // fixture tree under `tests/conformance/python_reference`.
        let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let script = crate_dir.join("tests/conformance/python_reference/generate_fixtures.py");
        let content = std::fs::read_to_string(&script).expect("read python generator");

        assert!(
            content.contains("MCP_AGENT_MAIL_CONFORMANCE_SCRATCH_ROOT"),
            "generator should support MCP_AGENT_MAIL_CONFORMANCE_SCRATCH_ROOT override"
        );
        assert!(
            content.contains("tempfile.mkdtemp"),
            "generator should default scratch outputs to the OS temp dir"
        );
        assert!(
            !content.contains("/ \"_scratch\""),
            "generator must not default to writing into repo python_reference/_scratch"
        );
    }

    #[test]
    fn regen_runs_stub_python_script_to_temp_output() {
        // Avoid relying on the legacy venv or packages: run a tiny stub script.
        if Command::new("python3")
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_err()
        {
            eprintln!("SKIP: python3 not available");
            return;
        }

        let dir = tempfile::tempdir().expect("temp dir");
        let script = dir.path().join("gen.py");
        let out = dir.path().join("fixtures.json");

        std::fs::write(
            &script,
            r#"
import json
import os
from pathlib import Path

out = Path(os.environ["MCP_AGENT_MAIL_CONFORMANCE_FIXTURE_PATH"])
out.parent.mkdir(parents=True, exist_ok=True)
data = {"version":"test","generated_at":"1970-01-01T00:00:00Z","tools":{},"resources":{}}
out.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
print(f"Wrote fixtures to {out}")
"#,
        )
        .expect("write stub script");

        run_regen(RegenArgs {
            python: Some("python3".to_string()),
            script: Some(script),
            output: Some(out.clone()),
            fixture_repo_root: None,
            scratch_root: None,
            dry_run: false,
        })
        .expect("run regen");

        assert!(out.exists());
        let fixtures = mcp_agent_mail_conformance::Fixtures::load(&out).expect("load fixtures");
        assert_eq!(fixtures.version, "test");
    }
}
