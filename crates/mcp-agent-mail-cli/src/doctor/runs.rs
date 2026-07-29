//! Per-run artifact directory management for `am doctor`.
//!
//! Each invocation that writes (`am doctor --fix`, future `am doctor fix`
//! subcommand) creates `.doctor/runs/<ISO8601>__<run-id>/` inside the
//! target repo. The layout follows OUTPUT-SCHEMA.md from the
//! world-class-doctor-mode-for-cli-tools skill:
//!
//! ```text
//! .doctor/
//! ├── runs/<ISO>__<id>/
//! │   ├── report.json           ← findings + summary
//! │   ├── report.md             ← human narrative
//! │   ├── actions.jsonl         ← one line per mutate() call
//! │   ├── backups/              ← verbatim per-file backups
//! │   ├── stderr.log
//! │   ├── stdout.json
//! │   └── undo.sh
//! ├── latest -> runs/<ISO>__<id>
//! └── scorecard_history.jsonl   ← per-run trend timeseries
//! ```
//!
//! `<run-id>` is derived from `sha256(target_sha + ISO8601_seconds)[..6]`,
//! so concurrent runs in the same second collide naturally.
//!
//! AGENTS.md compliance:
//! - No file deletion. `am doctor gc --before <date> --yes` (TODO: pass-2)
//!   is the only surface that may prune old run dirs, and only with
//!   explicit operator cutoff.
//! - All writes use atomic write-tmp-then-rename via `tempfile`.
//! - `.doctor/` added to `.gitignore` on first scaffold. Idempotent.

#![forbid(unsafe_code)]

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Schema version for the per-run artifact directory layout.
pub const RUN_SCHEMA_VERSION: &str = "1.0";

/// Doctor contract version — bumps independently of `am --version`. Agents
/// only care about this; it pins detectors/fixers/exit-codes/JSON shapes.
pub const DOCTOR_CONTRACT_VERSION: &str = "1.0";

/// Doctor implementation version. Minor for new fixers, major for incompatible
/// refactors. Distinct from `am --version`.
pub const DOCTOR_VERSION: &str = "1.0.0";

/// Derive the canonical run-id: `<ISO8601-seconds>__<6-char hex>`.
///
/// `target_sha` — current commit SHA of the target repo.
/// `iso_seconds` — `YYYY-MM-DDTHH-MM-SSZ` (note: dashes in time, NOT colons,
/// to keep the dir name FS-portable).
pub fn derive_run_id(target_sha: &str, iso_seconds: &str) -> String {
    let mut h = Sha256::new();
    h.update(target_sha.as_bytes());
    h.update(b"\0");
    h.update(iso_seconds.as_bytes());
    let digest = h.finalize();
    let hash6: String = (0..3).map(|i| format!("{:02x}", digest[i])).collect();
    format!("{iso_seconds}__{hash6}")
}

/// Compute the current ISO8601-seconds string in UTC, with `:` replaced by
/// `-` so the run-id is FS-portable.
pub fn now_iso_seconds() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H-%M-%SZ").to_string()
}

/// Resolve the `.doctor/` root for a target. Defaults to `<target>/.doctor/`
/// but honors `AM_DOCTOR_BACKUPS_DIR` if set.
pub fn doctor_root(target: &Path) -> PathBuf {
    if let Ok(s) = std::env::var("AM_DOCTOR_BACKUPS_DIR")
        && !s.is_empty()
    {
        return PathBuf::from(s);
    }
    target.join(".doctor")
}

/// Scaffold a fresh run dir under `<target>/.doctor/runs/`.
///
/// Creates: `runs/<run_id>/`, `runs/<run_id>/backups/`, touches `actions.jsonl`.
/// Idempotent — calling twice with the same run_id is a no-op.
pub fn scaffold_run_dir(target: &Path, run_id: &str) -> std::io::Result<PathBuf> {
    let root = doctor_root(target);
    ensure_dir_without_symlink(&root)?;
    let runs_dir = root.join("runs");
    ensure_dir_without_symlink(&runs_dir)?;
    let run_dir = root.join("runs").join(run_id);
    ensure_dir_without_symlink(&run_dir)?;
    ensure_dir_without_symlink(&run_dir.join("backups"))?;
    open_actions_log(&run_dir)?;
    Ok(run_dir)
}

/// Open a run's `actions.jsonl` for append without following symlinks.
pub fn open_actions_log(run_dir: &Path) -> std::io::Result<File> {
    ensure_existing_dir_without_symlink(run_dir)?;
    open_append_regular_file_without_symlink(&run_dir.join("actions.jsonl"))
}

/// Persist the user-facing run artifacts for a completed mutating doctor run.
pub fn write_run_artifacts(
    run_dir: &Path,
    run_id: &str,
    report: &serde_json::Value,
) -> std::io::Result<()> {
    ensure_existing_dir_without_symlink(run_dir)?;

    let pretty = serde_json::to_vec_pretty(report).map_err(std::io::Error::other)?;
    let mut json = pretty;
    json.push(b'\n');

    atomic_write_bytes(&run_dir.join("report.json"), &json, 0o644)?;
    atomic_write_bytes(&run_dir.join("stdout.json"), &json, 0o644)?;
    atomic_write_bytes(
        &run_dir.join("report.md"),
        run_report_markdown(run_id, report).as_bytes(),
        0o644,
    )?;
    atomic_write_bytes(
        &run_dir.join("undo.sh"),
        run_undo_script(run_id).as_bytes(),
        0o755,
    )?;
    Ok(())
}

fn ensure_dir_without_symlink(path: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => ensure_existing_dir_without_symlink(path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent()
                && parent != path
                && !parent.as_os_str().is_empty()
            {
                ensure_dir_without_symlink(parent)?;
            }
            fs::create_dir(path)?;
            ensure_existing_dir_tree_without_symlink(path)
        }
        Err(err) => Err(err),
    }
}

fn ensure_existing_dir_without_symlink(path: &Path) -> std::io::Result<()> {
    ensure_existing_dir_tree_without_symlink(path)
}

fn ensure_existing_dir_tree_without_symlink(path: &Path) -> std::io::Result<()> {
    let mut ancestors: Vec<&Path> = path
        .ancestors()
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
        .collect();
    ancestors.reverse();
    for ancestor in ancestors {
        ensure_existing_dir_leaf_without_symlink(ancestor)?;
    }
    Ok(())
}

fn ensure_existing_dir_leaf_without_symlink(path: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(symlink_dir_error(path)),
        Ok(meta) if meta.file_type().is_dir() => Ok(()),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("{} exists but is not a directory", path.display()),
        )),
        Err(err) => Err(err),
    }
}

fn symlink_dir_error(path: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!(
            "refusing to use symlinked doctor artifact directory {}",
            path.display()
        ),
    )
}

fn open_append_regular_file_without_symlink(path: &Path) -> std::io::Result<File> {
    if let Some(parent) = path.parent() {
        ensure_dir_without_symlink(parent)?;
    }

    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(symlink_file_error(path)),
        Ok(meta) if meta.file_type().is_file() => open_append_no_follow(path),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("{} exists but is not a regular file", path.display()),
        )),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => open_append_no_follow(path),
        Err(err) => Err(err),
    }
}

fn open_append_no_follow(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc_consts::O_NOFOLLOW);
    }
    options.open(path)
}

#[cfg(unix)]
mod libc_consts {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub const O_NOFOLLOW: i32 = 0o400000;
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    pub const O_NOFOLLOW: i32 = 0x0100;
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd"
    )))]
    pub const O_NOFOLLOW: i32 = 0;
}

fn symlink_file_error(path: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!(
            "refusing to use symlinked doctor artifact file {}",
            path.display()
        ),
    )
}

fn atomic_write_bytes(path: &Path, content: &[u8], mode: u32) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("path has no parent: {}", path.display()),
        )
    })?;
    ensure_dir_without_symlink(parent)?;

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("artifact");
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = parent.join(format!(
        ".{file_name}.tmp.{}.{}",
        std::process::id(),
        now_ns
    ));

    let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
    }
    file.write_all(content)?;
    file.sync_data()?;
    drop(file);
    fs::rename(&tmp, path)?;
    Ok(())
}

fn run_report_markdown(run_id: &str, report: &serde_json::Value) -> String {
    let mode = report
        .get("mode")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown");
    let fm_id = report
        .get("fm_id")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown");
    let ok = report
        .get("ok")
        .and_then(|value| value.as_bool())
        .map_or("unknown".to_string(), |value| value.to_string());
    let actions_taken = report
        .get("actions_taken")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let remaining_findings = report
        .get("summary")
        .and_then(|summary| summary.get("total_findings"))
        .and_then(|value| value.as_u64())
        .unwrap_or(0);

    format!(
        "# am doctor run {run_id}\n\n- mode: {mode}\n- finding: {fm_id}\n- ok: {ok}\n- actions taken: {actions_taken}\n- remaining findings: {remaining_findings}\n"
    )
}

fn run_undo_script(run_id: &str) -> String {
    format!(
        "#!/bin/sh\nset -eu\nam doctor undo {} \"$@\"\n",
        shell_single_quote(run_id)
    )
}

fn shell_single_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

/// Atomically replace `<doctor_root>/latest` -> `runs/<run_id>` symlink.
///
/// Uses a unique pid+ns-suffixed tmp symlink that we created ourselves,
/// then `rename(tmp, latest)`. Never touches user data.
pub fn update_latest_symlink(target: &Path, run_id: &str) -> std::io::Result<()> {
    let root = doctor_root(target);
    ensure_dir_without_symlink(&root)?;
    let latest = root.join("latest");
    if let Ok(meta) = fs::symlink_metadata(&latest)
        && !meta.file_type().is_symlink()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "refusing to replace non-symlink .doctor/latest at {}",
                latest.display()
            ),
        ));
    }
    let target_relative = PathBuf::from("runs").join(run_id);
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = root.join(format!(".latest.tmp.{}.{}", std::process::id(), now_ns));
    if fs::symlink_metadata(&tmp).is_ok() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "refusing to replace pre-existing temporary symlink path {}",
                tmp.display()
            ),
        ));
    }
    crate::doctor::platform::create_symlink(&target_relative, &tmp)?;
    fs::rename(&tmp, &latest)?;
    Ok(())
}

/// Append `.doctor/` to target's `.gitignore` if missing. Idempotent.
pub fn ensure_gitignore_entry(target: &Path) -> std::io::Result<()> {
    let gi = target.join(".gitignore");
    let needs_entry = match fs::read_to_string(&gi) {
        Ok(s) => !s
            .lines()
            .any(|l| l.trim() == ".doctor/" || l.trim() == ".doctor"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => return Err(e),
    };
    if needs_entry {
        let mut f = OpenOptions::new().create(true).append(true).open(&gi)?;
        f.write_all(b"\n# am doctor per-run artifacts (world-class doctor surface)\n.doctor/\n")?;
        f.sync_data()?;
    }
    Ok(())
}

/// Append one line to `<doctor_root>/scorecard_history.jsonl`.
pub fn append_scorecard_history(target: &Path, line_json: &str) -> std::io::Result<()> {
    let root = doctor_root(target);
    ensure_dir_without_symlink(&root)?;
    let path = root.join("scorecard_history.jsonl");
    let mut f = open_append_regular_file_without_symlink(&path)?;
    f.write_all(line_json.as_bytes())?;
    if !line_json.ends_with('\n') {
        f.write_all(b"\n")?;
    }
    f.sync_data()?;
    Ok(())
}

/// One row of `am doctor ls`.
#[derive(Debug, serde::Serialize)]
pub struct RunSummary {
    pub run_id: String,
    pub started_at: String,
    pub exit_code: Option<i32>,
    pub action_count: usize,
    pub finding_count: Option<usize>,
    pub bytes_backed_up: Option<u64>,
}

/// Enumerate all runs in `<doctor_root>/runs/` in chronological order.
pub fn list_runs(target: &Path) -> std::io::Result<Vec<RunSummary>> {
    let runs_dir = doctor_root(target).join("runs");
    let mut out = Vec::new();
    let entries = match fs::read_dir(&runs_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let run_id = entry.file_name().to_string_lossy().into_owned();
        out.push(summarize_run(&path, &run_id));
    }
    out.sort_by(|a, b| a.run_id.cmp(&b.run_id));
    Ok(out)
}

/// Build a RunSummary by reading actions.jsonl + report.json from a run dir.
fn summarize_run(run_dir: &Path, run_id: &str) -> RunSummary {
    let started_at = run_id
        .split_once("__")
        .map(|(ts, _)| ts.replace('-', ":"))
        .unwrap_or_else(|| run_id.to_string());

    let action_count = fs::read_to_string(run_dir.join("actions.jsonl"))
        .map(|s| s.lines().count())
        .unwrap_or(0);

    let mut exit_code = None;
    let mut finding_count = None;
    let mut bytes_backed_up = None;
    if let Ok(mut f) = fs::File::open(run_dir.join("report.json")) {
        let mut s = String::new();
        if f.read_to_string(&mut s).is_ok()
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&s)
        {
            exit_code = v
                .get("exit_code")
                .and_then(|e| e.as_i64())
                .map(|i| i as i32);
            finding_count = v
                .get("summary")
                .and_then(|sm| sm.get("total_findings"))
                .and_then(|n| n.as_u64())
                .map(|n| n as usize);
            bytes_backed_up = v
                .get("summary")
                .and_then(|sm| sm.get("bytes_backed_up"))
                .and_then(|n| n.as_u64());
        }
    }

    RunSummary {
        run_id: run_id.to_string(),
        started_at,
        exit_code,
        action_count,
        finding_count,
        bytes_backed_up,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn run_id_is_deterministic_for_same_inputs() {
        let a = derive_run_id("deadbeef", "2026-05-09T16-30-15Z");
        let b = derive_run_id("deadbeef", "2026-05-09T16-30-15Z");
        assert_eq!(a, b);
        assert!(a.contains("__"));
        assert_eq!(a.len(), "2026-05-09T16-30-15Z".len() + 2 + 6);
    }

    #[test]
    fn run_id_changes_with_target_sha() {
        let a = derive_run_id("aaa", "2026-05-09T16-30-15Z");
        let b = derive_run_id("bbb", "2026-05-09T16-30-15Z");
        assert_ne!(a, b);
    }

    #[test]
    fn run_id_format_is_fs_portable() {
        let id = derive_run_id("a", "2026-05-09T16-30-15Z");
        assert!(!id.contains(':'));
        assert!(!id.contains('/'));
        assert!(!id.contains(' '));
    }

    #[test]
    fn now_iso_seconds_has_no_colons() {
        let s = now_iso_seconds();
        assert!(!s.contains(':'));
        assert!(s.ends_with('Z'));
    }

    #[test]
    fn scaffold_creates_run_dir_with_backups_and_actions() {
        let td = TempDir::new().expect("tempdir");
        let run_id = "2026-05-09T16-30-15Z__abc123";
        let run_dir = scaffold_run_dir(td.path(), run_id).expect("scaffold");
        assert!(run_dir.join("backups").is_dir());
        assert!(run_dir.join("actions.jsonl").exists());
    }

    #[cfg(unix)]
    #[test]
    fn scaffold_refuses_symlinked_run_dir_without_writing_target() {
        let td = TempDir::new().expect("tempdir");
        let target = td.path().join("outside");
        fs::create_dir(&target).expect("target dir");
        let runs = doctor_root(td.path()).join("runs");
        fs::create_dir_all(&runs).expect("runs dir");
        let run_id = "2026-05-09T16-30-15Z__abc123";
        std::os::unix::fs::symlink(&target, runs.join(run_id)).expect("symlink run dir");

        let err = scaffold_run_dir(td.path(), run_id).expect_err("symlink run dir must refuse");

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(
            !target.join("actions.jsonl").exists(),
            "scaffold must not follow run-dir symlinks"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scaffold_refuses_symlinked_actions_log_without_writing_target() {
        let td = TempDir::new().expect("tempdir");
        let target = td.path().join("outside");
        fs::create_dir(&target).expect("target dir");
        let run_id = "2026-05-09T16-30-15Z__abc123";
        let run_dir = doctor_root(td.path()).join("runs").join(run_id);
        fs::create_dir_all(run_dir.join("backups")).expect("run dir");
        std::os::unix::fs::symlink(target.join("actions.jsonl"), run_dir.join("actions.jsonl"))
            .expect("symlink actions log");

        let err = scaffold_run_dir(td.path(), run_id).expect_err("symlink actions log must refuse");

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(
            !target.join("actions.jsonl").exists(),
            "scaffold must not follow actions.jsonl symlinks"
        );
    }

    #[test]
    fn write_run_artifacts_persists_report_stdout_markdown_and_undo() {
        let td = TempDir::new().expect("tempdir");
        let run_id = "2026-05-09T16-30-15Z__abc123";
        let run_dir = scaffold_run_dir(td.path(), run_id).expect("scaffold");
        let report = serde_json::json!({
            "run_id": run_id,
            "mode": "fix",
            "fm_id": "fm-test",
            "ok": true,
            "actions_taken": 1,
            "summary": {
                "total_findings": 0
            }
        });

        write_run_artifacts(&run_dir, run_id, &report).expect("write artifacts");

        let report_json = fs::read_to_string(run_dir.join("report.json")).unwrap();
        let stdout_json = fs::read_to_string(run_dir.join("stdout.json")).unwrap();
        assert_eq!(report_json, stdout_json);
        let parsed: serde_json::Value = serde_json::from_str(&report_json).unwrap();
        assert_eq!(parsed["run_id"], run_id);
        assert!(run_dir.join("report.md").is_file());
        let undo = fs::read_to_string(run_dir.join("undo.sh")).unwrap();
        assert!(undo.contains("am doctor undo '2026-05-09T16-30-15Z__abc123'"));
    }

    #[cfg(unix)]
    #[test]
    fn write_run_artifacts_refuses_symlinked_run_dir_without_writing_target() {
        let td = TempDir::new().expect("tempdir");
        let target = td.path().join("outside");
        fs::create_dir(&target).expect("target dir");
        let link = td.path().join("run-link");
        std::os::unix::fs::symlink(&target, &link).expect("symlink run dir");
        let report = serde_json::json!({
            "run_id": "2026-05-09T16-30-15Z__abc123",
            "mode": "fix",
            "fm_id": "fm-test",
            "ok": true,
            "actions_taken": 1,
            "summary": {
                "total_findings": 0
            }
        });

        let err = write_run_artifacts(&link, "2026-05-09T16-30-15Z__abc123", &report)
            .expect_err("symlink run dir must refuse");

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(
            !target.join("report.json").exists(),
            "artifact writer must not follow run-dir symlinks"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_run_artifacts_refuses_symlinked_parent_without_writing_target() {
        let td = TempDir::new().expect("tempdir");
        let target = td.path().join("outside");
        let target_run = target.join("run");
        fs::create_dir(&target).expect("target dir");
        fs::create_dir(&target_run).expect("target run dir");
        let link = td.path().join("linked-parent");
        std::os::unix::fs::symlink(&target, &link).expect("symlink parent");
        let run_dir = link.join("run");
        let report = serde_json::json!({
            "run_id": "2026-05-09T16-30-15Z__abc123",
            "mode": "fix",
            "fm_id": "fm-test",
            "ok": true,
            "actions_taken": 1,
            "summary": {
                "total_findings": 0
            }
        });

        let err = write_run_artifacts(&run_dir, "2026-05-09T16-30-15Z__abc123", &report)
            .expect_err("symlinked parent must refuse");

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(
            !target_run.join("report.json").exists(),
            "artifact writer must not follow symlinked parents"
        );
    }

    #[test]
    fn run_undo_script_shell_quotes_run_id() {
        let script = run_undo_script("bad'id; touch nope");

        assert!(script.contains("am doctor undo 'bad'\\''id; touch nope' \"$@\""));
    }

    #[test]
    fn scaffold_is_idempotent() {
        let td = TempDir::new().expect("tempdir");
        let run_id = "2026-05-09T16-30-15Z__abc123";
        let _a = scaffold_run_dir(td.path(), run_id).expect("first");
        let _b = scaffold_run_dir(td.path(), run_id).expect("second — should not error");
    }

    #[test]
    fn update_latest_symlink_refuses_regular_file_without_clobbering() {
        let td = TempDir::new().expect("tempdir");
        let run_id = "2026-05-09T16-30-15Z__abc123";
        let root = doctor_root(td.path());
        scaffold_run_dir(td.path(), run_id).expect("scaffold");
        fs::write(root.join("latest"), "operator data\n").expect("plant regular latest");

        let err = update_latest_symlink(td.path(), run_id)
            .expect_err("regular .doctor/latest must be preserved");

        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read_to_string(root.join("latest")).unwrap(),
            "operator data\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn update_latest_symlink_refuses_symlinked_doctor_root_without_writing_target() {
        let td = TempDir::new().expect("tempdir");
        let target = td.path().join("outside");
        fs::create_dir(&target).expect("target dir");
        std::os::unix::fs::symlink(&target, doctor_root(td.path())).expect("symlink doctor root");

        let err = update_latest_symlink(td.path(), "2026-05-09T16-30-15Z__abc123")
            .expect_err("symlinked doctor root must refuse");

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(
            !target.join("latest").exists(),
            "latest update must not follow a symlinked doctor root"
        );
    }

    #[test]
    fn ensure_gitignore_entry_creates_and_is_idempotent() {
        let td = TempDir::new().expect("tempdir");
        ensure_gitignore_entry(td.path()).expect("first");
        let s1 = fs::read_to_string(td.path().join(".gitignore")).unwrap();
        assert!(s1.contains(".doctor/"));
        ensure_gitignore_entry(td.path()).expect("second");
        let s2 = fs::read_to_string(td.path().join(".gitignore")).unwrap();
        // No duplicate entry.
        assert_eq!(s1, s2);
    }

    #[test]
    fn ensure_gitignore_entry_does_not_duplicate_existing() {
        let td = TempDir::new().expect("tempdir");
        std::fs::write(td.path().join(".gitignore"), "target/\n.doctor/\n").unwrap();
        ensure_gitignore_entry(td.path()).expect("ok");
        let s = fs::read_to_string(td.path().join(".gitignore")).unwrap();
        assert_eq!(s.matches(".doctor/").count(), 1);
    }

    #[test]
    fn list_runs_returns_empty_when_no_runs() {
        let td = TempDir::new().expect("tempdir");
        let runs = list_runs(td.path()).expect("ok");
        assert!(runs.is_empty());
    }

    #[test]
    fn list_runs_returns_chronological_order() {
        let td = TempDir::new().expect("tempdir");
        let _ = scaffold_run_dir(td.path(), "2026-05-09T16-30-15Z__aaa").unwrap();
        let _ = scaffold_run_dir(td.path(), "2026-05-09T16-31-15Z__bbb").unwrap();
        let _ = scaffold_run_dir(td.path(), "2026-05-09T16-29-15Z__ccc").unwrap();
        let runs = list_runs(td.path()).expect("ok");
        assert_eq!(runs.len(), 3);
        // Lexicographic order matches chronological because of the ISO prefix.
        assert!(runs[0].run_id.starts_with("2026-05-09T16-29-15Z"));
        assert!(runs[1].run_id.starts_with("2026-05-09T16-30-15Z"));
        assert!(runs[2].run_id.starts_with("2026-05-09T16-31-15Z"));
    }

    #[test]
    fn append_scorecard_history_creates_and_appends() {
        let td = TempDir::new().expect("tempdir");
        append_scorecard_history(td.path(), r#"{"run_id":"a","aggregate":700}"#).unwrap();
        append_scorecard_history(td.path(), r#"{"run_id":"b","aggregate":750}"#).unwrap();
        let s =
            fs::read_to_string(td.path().join(".doctor").join("scorecard_history.jsonl")).unwrap();
        assert_eq!(s.lines().count(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn append_scorecard_history_refuses_symlinked_file_without_writing_target() {
        let td = TempDir::new().expect("tempdir");
        let target = td.path().join("outside");
        fs::create_dir(&target).expect("target dir");
        let root = doctor_root(td.path());
        fs::create_dir(&root).expect("doctor root");
        std::os::unix::fs::symlink(
            target.join("scorecard_history.jsonl"),
            root.join("scorecard_history.jsonl"),
        )
        .expect("symlink scorecard");

        let err = append_scorecard_history(td.path(), r#"{"run_id":"a","aggregate":700}"#)
            .expect_err("symlinked scorecard must refuse");

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(
            !target.join("scorecard_history.jsonl").exists(),
            "scorecard append must not follow symlinked files"
        );
    }
}
