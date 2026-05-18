//! `fm-runtime-processes-pid-hint-symlink-toctou` — P1
//! detect-only.
//!
//! **Subsystem**: runtime_processes.
//!
//! ## What's broken
//!
//! The listener PID hint file (`<storage_root>/listener.pid` or
//! whatever `pid_hint_candidates` resolves to) is the canonical
//! lock that prevents two `am serve` instances from racing on
//! the same SQLite database. Its integrity depends on the
//! filesystem path being a regular file at a regular-directory
//! parent — symlinks anywhere in the chain open a TOCTOU window:
//!
//! - **Hint path itself is a symlink**: an attacker (or
//!   misconfigured deploy script) replaced the regular file with
//!   a symlink pointing at a different PID. `kill(pid, 0)` walks
//!   the symlink and probes the wrong process; the doctor's
//!   `stale_listener_pid_hint` FM may quarantine a "stale" file
//!   that was actually pointing at a live listener.
//! - **Parent directory chain contains a symlink**: any
//!   intermediate path component being a symlink opens a window
//!   where an attacker can redirect WRITES to the hint to a
//!   different location, or vice versa.
//! - **Parent directory mode wider than 0o700**: an attacker with
//!   write access to the parent can rename the hint file to a
//!   different name and replace it with their own.
//! - **Stray symlinks in the hint dir**: any symlink in the same
//!   directory is a security signal — the canonical installer
//!   never writes symlinks here.
//!
//! Distinct from:
//!
//! - `stale_listener_pid_hint` (existing FM): handles dead-PID +
//!   old-mtime quarantine. Assumes the hint is a regular file.
//! - `stale_python_server_shadow` (existing FM): handles the
//!   case where the live PID is a Python interpreter, not the
//!   Rust binary. Also assumes regular-file shape.
//!
//! THIS FM specifically owns the path-shape issues that would
//! either short-circuit the other FMs OR let them write through
//! the wrong path.
//!
//! ## Detection (pure)
//!
//! For each path in `pid_hint_candidates`:
//!
//! 1. Walk the parent chain via `symlink_metadata` on each
//!    component. Record components that are symlinks.
//! 2. Check the hint path itself via `symlink_metadata`. If it's
//!    a symlink, record it.
//! 3. Check the parent dir's POSIX mode. If group/other bits are
//!    set (mode & 0o077 != 0), record the mode as too-open.
//! 4. Read the parent dir. Any other entry that's a symlink (NOT
//!    the hint itself) is a stray symlink — record it.
//!
//! Emit one aggregated finding per candidate that has any signal.
//!
//! ## Fix
//!
//! **Detect-only.** Auto-fix would Op::Rename the symlinked
//! hint into quarantine + ensure the parent dir is mode 0o700 +
//! quarantine each stray symlink. Multi-step with security-
//! sensitive side effects; deferred. Manual remediation: inspect
//! each entry; if any symlink target is unexpected, treat as a
//! security incident; tighten parent dir to 0o700; replace each
//! offending symlink with a regular file (or move it aside).

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{MutateContext, MutateError};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub const FM_ID: &str = "fm-runtime-processes-pid-hint-symlink-toctou";
const FM_SEVERITY: &str = "P1";
const FM_SUBSYSTEM: &str = "runtime_processes";

#[derive(Debug, Clone, Serialize)]
pub struct SymlinkComponent {
    pub path: PathBuf,
    /// Best-effort `read_link` target. Empty if the link is
    /// unreadable (rare).
    pub target: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct HintEntry {
    pub hint_path: PathBuf,
    pub hint_is_symlink: bool,
    pub hint_symlink_target: Option<PathBuf>,
    /// Each parent-chain component that is a symlink. Indexed
    /// from `hint_path.parent()` outward toward the filesystem
    /// root.
    pub parent_chain_symlinks: Vec<SymlinkComponent>,
    /// `parent_dir`'s POSIX mode (masked to 0o777). `None` when
    /// the dir doesn't exist / isn't statable.
    pub parent_dir_mode: Option<u32>,
    /// Whether the parent dir has any group/other bits set
    /// (mode & 0o077 != 0). The canonical installer writes 0o700.
    pub parent_dir_mode_too_open: bool,
    /// Other entries in the hint dir that are symlinks (NOT the
    /// hint itself). Each is a security signal.
    pub stray_symlinks: Vec<SymlinkComponent>,
}

impl HintEntry {
    fn has_any_signal(&self) -> bool {
        self.hint_is_symlink
            || !self.parent_chain_symlinks.is_empty()
            || self.parent_dir_mode_too_open
            || !self.stray_symlinks.is_empty()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RuntimePidHintSymlinkToctouFinding {
    pub entries: Vec<HintEntry>,
}

impl RuntimePidHintSymlinkToctouFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "{} listener PID hint(s) have symlink TOCTOU signals (hint is symlink / parent chain has symlinks / parent dir too open / stray symlinks)",
            self.entries.len(),
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: 1.0,
            evidence: serde_json::json!({
                "entries": self.entries,
                "expected_parent_dir_mode_octal": "0o700",
                "manual_remediation": {
                    "steps": [
                        "Inspect each `hint_is_symlink` / `parent_chain_symlinks` / `stray_symlinks` target via `ls -la` + `readlink`. If any target is unexpected (especially outside `<storage_root>`), treat as a security incident — preserve forensics before mutating.",
                        "Tighten the hint dir mode: `chmod 700 <parent_dir>`.",
                        "If the hint path itself is a symlink: stop the running `am serve` (the symlink may be redirecting the lock check), `mkdir -p .doctor/quarantine/pid-hint-symlinks && mv <hint_path> .doctor/quarantine/pid-hint-symlinks/`, then restart `am serve` to write a fresh regular-file hint.",
                        "If parent-chain components are symlinks: this is harder — the canonical install path was followed through symlinks. Replace each symlinked component with a regular dir (preserving the target's contents via `cp -a`).",
                        "Stray symlinks in the hint dir: `mv <stray_path> .doctor/quarantine/pid-hint-symlinks/`.",
                        "Re-run `am doctor fix --only fm-runtime-processes-pid-hint-symlink-toctou --list` to confirm zero residual signals.",
                    ],
                    "warning": "SECURITY signal: the installer NEVER writes symlinks at hint paths. Symlinks here open a TOCTOU window between the doctor's `kill(pid, 0)` probe and any subsequent quarantine — an attacker could swap the target between the two syscalls. Investigate before mutating.",
                    "safe_fix_deferred": "Auto-fix via Op::Rename of the symlinked hint, Op::Chmod of the parent dir, and Op::Rename of each stray symlink is intentionally deferred in this first cut. Each action has security-sensitive side effects (especially the symlinked-hint quarantine, which strips the only lock preventing concurrent server startup); the chokepoint already supports Op::Rename + Op::Chmod, but the integration test needed to prove the right ordering (chmod parent BEFORE rename hint) is harness work.",
                    "common_causes": [
                        "Deploy script `cp -s` (copy-as-symlink) of the install dir.",
                        "Worktree migration that aliased the storage root via symlinks.",
                        "Manual `ln -s` to point the hint at a different listener (e.g., during a debug session).",
                        "Parent dir was created with a wider umask than 0o077 (typical default 0o022).",
                        "ATTACKER pre-positioned a symlink to redirect the lock check.",
                    ],
                },
            }),
            remediation: FindingRemediation {
                command: format!("am doctor explain {FM_ID}"),
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable: false,
                estimated_actions: 0,
            },
        }
    }
}

/// Detector. PURE w.r.t. the supplied `pid_hint_candidates`.
pub fn detect(pid_hint_candidates: &[PathBuf]) -> Vec<RuntimePidHintSymlinkToctouFinding> {
    let mut entries: Vec<HintEntry> = Vec::new();
    for hint_path in pid_hint_candidates {
        let entry = inspect_one(hint_path);
        if entry.has_any_signal() {
            entries.push(entry);
        }
    }
    if entries.is_empty() {
        return Vec::new();
    }
    vec![RuntimePidHintSymlinkToctouFinding { entries }]
}

fn inspect_one(hint_path: &Path) -> HintEntry {
    let mut entry = HintEntry {
        hint_path: hint_path.to_path_buf(),
        hint_is_symlink: false,
        hint_symlink_target: None,
        parent_chain_symlinks: Vec::new(),
        parent_dir_mode: None,
        parent_dir_mode_too_open: false,
        stray_symlinks: Vec::new(),
    };

    // (1) Is the hint path itself a symlink?
    if let Ok(meta) = std::fs::symlink_metadata(hint_path)
        && meta.file_type().is_symlink()
    {
        entry.hint_is_symlink = true;
        entry.hint_symlink_target = std::fs::read_link(hint_path).ok();
    }

    // (2) Parent chain symlink walk. Build each prefix path and
    // lstat the prefix itself; lstat'ing only `parent()` paths
    // misses symlinked intermediate components once they are
    // extended with child names.
    entry.parent_chain_symlinks = parent_chain_symlinks(hint_path);

    // (3) Parent dir mode check.
    let Some(parent_dir) = hint_path.parent() else {
        return entry;
    };
    if let Ok(meta) = std::fs::metadata(parent_dir) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = meta.permissions().mode() & 0o777;
            entry.parent_dir_mode = Some(mode);
            entry.parent_dir_mode_too_open = (mode & 0o077) != 0;
        }
    }

    // (4) Stray symlinks in the parent dir (NOT the hint itself).
    let Ok(rd) = std::fs::read_dir(parent_dir) else {
        return entry;
    };
    for de in rd.flatten() {
        let p = de.path();
        if p == hint_path {
            continue;
        }
        if let Ok(meta) = std::fs::symlink_metadata(&p)
            && meta.file_type().is_symlink()
        {
            entry.stray_symlinks.push(SymlinkComponent {
                path: p.clone(),
                target: std::fs::read_link(&p).unwrap_or_default(),
            });
        }
    }

    entry
}

fn parent_chain_symlinks(hint_path: &Path) -> Vec<SymlinkComponent> {
    let Some(parent) = hint_path.parent() else {
        return Vec::new();
    };
    let mut current = PathBuf::new();
    let mut found = Vec::new();
    for component in parent.components() {
        current.push(component.as_os_str());
        if let Ok(meta) = std::fs::symlink_metadata(&current)
            && meta.file_type().is_symlink()
        {
            found.push(SymlinkComponent {
                path: current.clone(),
                target: std::fs::read_link(&current).unwrap_or_default(),
            });
        }
    }
    found.reverse();
    found
}

/// Detect-only FM. `fix()` is a no-op.
pub fn fix(
    _ctx: &MutateContext,
    _finding: &RuntimePidHintSymlinkToctouFinding,
) -> Result<FixOutcome, MutateError> {
    Ok(FixOutcome {
        actions_taken: 0,
        actions_skipped: 1,
        quarantined_paths: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// **NEGATIVE TEST FIRST**: empty input → no finding.
    #[test]
    fn detector_returns_empty_for_no_candidates() {
        assert!(detect(&[]).is_empty());
    }

    /// **NEGATIVE**: hint dir doesn't exist → no finding.
    #[test]
    fn detector_returns_empty_for_missing_hint_path() {
        let td = TempDir::new().unwrap();
        let hint = td.path().join("nope").join("listener.pid");
        assert!(detect(&[hint]).is_empty());
    }

    /// **NEGATIVE**: healthy regular-file hint at 0o700 parent →
    /// no finding.
    #[cfg(unix)]
    #[test]
    fn detector_returns_empty_for_healthy_hint() {
        use std::os::unix::fs::PermissionsExt;
        let td = TempDir::new().unwrap();
        let parent = td.path().join("am-pid");
        fs::create_dir_all(&parent).unwrap();
        // Make parent 0o700 (the canonical installer mode).
        let mut perms = fs::metadata(&parent).unwrap().permissions();
        perms.set_mode(0o700);
        fs::set_permissions(&parent, perms).unwrap();
        let hint = parent.join("listener.pid");
        fs::write(&hint, b"12345\n").unwrap();
        // Re-set in case write reset the parent mode.
        let mut perms = fs::metadata(&parent).unwrap().permissions();
        perms.set_mode(0o700);
        fs::set_permissions(&parent, perms).unwrap();
        let findings = detect(&[hint]);
        assert!(
            findings.is_empty(),
            "healthy hint at 0o700 parent must not flag: {findings:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn detector_flags_hint_path_itself_as_symlink() {
        use std::os::unix::fs::symlink;
        let td = TempDir::new().unwrap();
        let real = td.path().join("real.pid");
        fs::write(&real, b"99999\n").unwrap();
        let hint = td.path().join("listener.pid");
        symlink(&real, &hint).unwrap();
        let findings = detect(std::slice::from_ref(&hint));
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.entries.len(), 1);
        assert!(f.entries[0].hint_is_symlink);
        assert_eq!(f.entries[0].hint_symlink_target, Some(real));
    }

    #[cfg(unix)]
    #[test]
    fn detector_flags_parent_dir_too_open() {
        use std::os::unix::fs::PermissionsExt;
        let td = TempDir::new().unwrap();
        let parent = td.path().join("am-pid");
        fs::create_dir_all(&parent).unwrap();
        let hint = parent.join("listener.pid");
        fs::write(&hint, b"12345\n").unwrap();
        // Deliberately set 0o755 (group + other read+execute).
        let mut perms = fs::metadata(&parent).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&parent, perms).unwrap();
        let findings = detect(&[hint]);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].entries[0].parent_dir_mode_too_open);
        assert_eq!(findings[0].entries[0].parent_dir_mode, Some(0o755));
    }

    #[cfg(unix)]
    #[test]
    fn detector_flags_stray_symlink_in_parent_dir() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let td = TempDir::new().unwrap();
        let parent = td.path().join("am-pid");
        fs::create_dir_all(&parent).unwrap();
        let mut perms = fs::metadata(&parent).unwrap().permissions();
        perms.set_mode(0o700);
        fs::set_permissions(&parent, perms).unwrap();
        let hint = parent.join("listener.pid");
        fs::write(&hint, b"12345\n").unwrap();
        // Plant a stray symlink in the same dir.
        let decoy = td.path().join("decoy");
        fs::write(&decoy, b"x").unwrap();
        symlink(&decoy, parent.join("aliased.txt")).unwrap();
        let findings = detect(&[hint]);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].entries[0].stray_symlinks.len(), 1);
        assert!(
            findings[0].entries[0].stray_symlinks[0]
                .path
                .ends_with("aliased.txt")
        );
    }

    #[cfg(unix)]
    #[test]
    fn detector_flags_intermediate_parent_chain_symlink() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let td = TempDir::new().unwrap();
        let real_root = td.path().join("real-root");
        let real_parent = real_root.join("am-pid");
        fs::create_dir_all(&real_parent).unwrap();
        let mut perms = fs::metadata(&real_parent).unwrap().permissions();
        perms.set_mode(0o700);
        fs::set_permissions(&real_parent, perms).unwrap();
        fs::write(real_parent.join("listener.pid"), b"12345\n").unwrap();

        let link_root = td.path().join("linked-root");
        symlink(&real_root, &link_root).unwrap();
        let hint = link_root.join("am-pid").join("listener.pid");

        let findings = detect(&[hint]);
        assert_eq!(findings.len(), 1);
        let parent_symlinks = &findings[0].entries[0].parent_chain_symlinks;
        assert_eq!(parent_symlinks.len(), 1);
        assert_eq!(parent_symlinks[0].path, link_root);
        assert_eq!(parent_symlinks[0].target, real_root);
    }

    /// Pin that the hint path ITSELF is NOT reported as a stray
    /// symlink even when hint_is_symlink is true. The two
    /// signals are independent — we don't double-emit.
    #[cfg(unix)]
    #[test]
    fn detector_does_not_double_emit_hint_as_stray() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let td = TempDir::new().unwrap();
        let parent = td.path().join("am-pid");
        fs::create_dir_all(&parent).unwrap();
        let mut perms = fs::metadata(&parent).unwrap().permissions();
        perms.set_mode(0o700);
        fs::set_permissions(&parent, perms).unwrap();
        let real = td.path().join("real.pid");
        fs::write(&real, b"99999\n").unwrap();
        let hint = parent.join("listener.pid");
        symlink(&real, &hint).unwrap();
        let findings = detect(&[hint]);
        assert_eq!(findings.len(), 1);
        let e = &findings[0].entries[0];
        assert!(e.hint_is_symlink);
        assert!(
            e.stray_symlinks.is_empty(),
            "hint path must not double-emit as stray: {:?}",
            e.stray_symlinks
        );
    }

    #[cfg(unix)]
    #[test]
    fn detector_aggregates_multiple_signals_into_one_entry() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let td = TempDir::new().unwrap();
        let parent = td.path().join("am-pid");
        fs::create_dir_all(&parent).unwrap();
        let real = td.path().join("real.pid");
        fs::write(&real, b"x").unwrap();
        let hint = parent.join("listener.pid");
        symlink(&real, &hint).unwrap();
        // Too-open parent + stray symlink in same dir.
        let mut perms = fs::metadata(&parent).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&parent, perms).unwrap();
        let decoy = td.path().join("decoy");
        fs::write(&decoy, b"x").unwrap();
        symlink(&decoy, parent.join("aliased.txt")).unwrap();
        let findings = detect(&[hint]);
        assert_eq!(findings.len(), 1);
        let e = &findings[0].entries[0];
        assert!(e.hint_is_symlink);
        assert!(e.parent_dir_mode_too_open);
        assert_eq!(e.stray_symlinks.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn detector_aggregates_multiple_hints_into_one_finding() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let td = TempDir::new().unwrap();
        let mut hints: Vec<PathBuf> = Vec::new();
        for i in 0..3 {
            let parent = td.path().join(format!("am-pid-{i}"));
            fs::create_dir_all(&parent).unwrap();
            let real = td.path().join(format!("real-{i}.pid"));
            fs::write(&real, b"x").unwrap();
            let hint = parent.join("listener.pid");
            symlink(&real, &hint).unwrap();
            // Force the too-open signal too so the entry counts.
            let mut perms = fs::metadata(&parent).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&parent, perms).unwrap();
            hints.push(hint);
        }
        let findings = detect(&hints);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].entries.len(), 3);
    }

    #[test]
    fn finding_serializes_with_security_warning_and_remediation() {
        let f = RuntimePidHintSymlinkToctouFinding {
            entries: vec![HintEntry {
                hint_path: "/tmp/am-pid/listener.pid".into(),
                hint_is_symlink: true,
                hint_symlink_target: Some("/some/decoy".into()),
                parent_chain_symlinks: Vec::new(),
                parent_dir_mode: Some(0o755),
                parent_dir_mode_too_open: true,
                stray_symlinks: vec![SymlinkComponent {
                    path: "/tmp/am-pid/stray".into(),
                    target: "/etc/passwd".into(),
                }],
            }],
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains(FM_ID));
        assert!(s.contains("SECURITY signal"));
        assert!(s.contains("\"expected_parent_dir_mode_octal\":\"0o700\""));
        assert!(s.contains("\"hint_is_symlink\":true"));
        assert!(s.contains("\"parent_dir_mode_too_open\":true"));
        assert!(s.contains("safe_fix_deferred"));
        assert!(s.contains("common_causes"));
        assert!(s.contains("\"auto_fixable\":false"));
    }

    #[test]
    fn fixer_is_no_op_returning_skipped() {
        let td = TempDir::new().unwrap();
        let run_dir = crate::doctor::runs::scaffold_run_dir(td.path(), "test_run").unwrap();
        let actions = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join("actions.jsonl"))
            .unwrap();
        let ctx = MutateContext {
            run_id: "test_run".into(),
            run_dir,
            capabilities: crate::doctor::mutate::Capabilities {
                write_scopes: vec![td.path().to_path_buf()],
            },
            actions_file: std::sync::Mutex::new(actions),
            fixer_id: FM_ID.into(),
            repo_root: td.path().to_path_buf(),
            dry_run: false,
            start: std::time::Instant::now(),
            extra_locks: Vec::new(),
        };
        let finding = RuntimePidHintSymlinkToctouFinding {
            entries: Vec::new(),
        };
        let outcome = fix(&ctx, &finding).expect("fix");
        assert_eq!(outcome.actions_taken, 0);
        assert_eq!(outcome.actions_skipped, 1);
    }
}
