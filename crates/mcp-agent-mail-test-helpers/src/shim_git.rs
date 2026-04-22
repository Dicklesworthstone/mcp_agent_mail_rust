//! `shim_git`: build fake `git` binaries for tests.
//!
//! # Recursion safety
//!
//! If a test sets `AM_GIT_BINARY` to a shim, and the shim itself calls
//! back into `git`, we get infinite recursion. This builder captures
//! the current path to REAL git at build time and sets `AM_TEST_REAL_GIT`
//! in the shim's environment so downstream calls bypass PATH resolution.
//!
//! # Exit sequence
//!
//! `ShimBehavior::exits` is a *sequence* of exit specs. First call gets
//! the first element, second call gets the second, etc. Once exhausted
//! it loops (so `[Ok]` means "always succeed", `[Segfault, Ok]` means
//! "fail first call, succeed second").
//!
//! Per-call tracking is done by incrementing a counter file
//! (`<tempdir>/shim.counter`), which is robust against being invoked
//! by multiple processes in parallel (flock-gated).

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// Controlled behaviors for a shim git binary.
#[derive(Debug, Clone)]
pub struct ShimBehavior {
    /// What `--version` should print (stdout). Typical:
    /// `"git version 2.50.2"`.
    pub version_output: String,

    /// Sequence of exit specs, one per invocation. Loops once exhausted.
    pub exits: Vec<ShimExit>,

    /// Artificial per-call delay in milliseconds (0 = none).
    pub delay_ms: u64,

    /// If Some, the shim writes its pid + argv + counter to this file
    /// per invocation (append mode). Useful for "was this shim called?"
    /// assertions and for "which invocation was the Nth?".
    pub marker_file: Option<PathBuf>,
}

impl Default for ShimBehavior {
    fn default() -> Self {
        Self {
            version_output: "git version 2.50.2".to_string(),
            exits: vec![ShimExit::Ok],
            delay_ms: 0,
            marker_file: None,
        }
    }
}

/// How the shim should exit on a given invocation.
#[derive(Debug, Clone)]
pub enum ShimExit {
    /// Exit 0, print version output and forward any non-`--version` args
    /// to the real git (if `AM_TEST_REAL_GIT` is set).
    Ok,
    /// Exit with the given code.
    Exit(i32),
    /// Kill self with SIGSEGV (signal 11 → exit 139 on bash `$?`).
    Segfault,
    /// Kill self with SIGBUS (signal 7 → exit 135).
    Bus,
    /// Sleep forever (useful for timeout tests). Caller is responsible
    /// for killing.
    Hang,
}

/// Build a shim `git` binary at `dir/<name>` with the given behavior.
///
/// Returns the absolute path to the shim binary.
///
/// # Panics
///
/// Panics if file creation, permission setting, or the environment
/// lookup fails — this is test code and should surface failure loudly.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn build_shim_git(dir: &Path, name: &str, behavior: &ShimBehavior) -> PathBuf {
    let path = dir.join(name);
    let counter_path = dir.join(format!("{name}.counter"));
    // Initialize counter.
    fs::write(&counter_path, b"0").expect("init shim counter");

    // Capture path to the real git NOW so the shim can forward without
    // recursing into itself.
    let real_git = which_real_git().unwrap_or_else(|| PathBuf::from("/usr/bin/git"));

    // Serialize the exit sequence as bash strings.
    let exits_bash = behavior
        .exits
        .iter()
        .map(|e| match e {
            ShimExit::Ok => "ok".to_string(),
            ShimExit::Exit(c) => format!("exit:{c}"),
            ShimExit::Segfault => "segv".to_string(),
            ShimExit::Bus => "bus".to_string(),
            ShimExit::Hang => "hang".to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ");

    let marker_line = behavior.marker_file.as_ref().map_or_else(
        || "# no marker".to_string(),
        |p| {
            format!(
                r#"echo "$$ $(date -u +%s%N) $count $@" >> "{}""#,
                p.display()
            )
        },
    );

    #[cfg(unix)]
    {
        let delay_s = format!(
            "{}.{:03}",
            behavior.delay_ms / 1000,
            behavior.delay_ms % 1000
        );
        let script = format!(
            r#"#!/usr/bin/env bash
# mcp-agent-mail-test-helpers: shim git binary
# Built at test time; behavior is governed by the exit sequence below.

set +e

# AM_TEST_REAL_GIT points at the real git so forwarding doesn't recurse.
REAL_GIT="${{AM_TEST_REAL_GIT:-{real_git}}}"

# Per-call counter (atomic via flock on the counter file).
COUNTER_PATH="{counter_path}"
count=$(
  (
    flock -x 9
    cur=$(cat "$COUNTER_PATH" 2>/dev/null || echo 0)
    nxt=$((cur + 1))
    echo "$nxt" > "$COUNTER_PATH"
    echo "$nxt"
  ) 9<>"$COUNTER_PATH.lock"
)

EXITS=({exits_bash})
# Loop the sequence once exhausted.
idx=$(( (count - 1) % ${{#EXITS[@]}} ))
action="${{EXITS[$idx]}}"

{marker_line}

# Optional delay.
if [ "{delay_ms}" -gt 0 ]; then
    sleep "{delay_s}"
fi

# --version always prints the controlled output (regardless of exit).
if [ "$1" = "--version" ]; then
    echo "{version_output}"
fi

case "$action" in
    ok)
        if [ "$1" != "--version" ] && [ -n "${{AM_TEST_REAL_GIT:-}}" ]; then
            exec "$REAL_GIT" "$@"
        fi
        exit 0
        ;;
    exit:*)
        code="${{action#exit:}}"
        exit "$code"
        ;;
    segv)
        # Trigger SIGSEGV on ourselves.
        kill -SEGV $$
        ;;
    bus)
        kill -BUS $$
        ;;
    hang)
        # Sleep forever; caller must kill.
        sleep 3600
        ;;
    *)
        echo "shim: unknown action '$action'" >&2
        exit 2
        ;;
esac
"#,
            real_git = real_git.display(),
            counter_path = counter_path.display(),
            exits_bash = exits_bash,
            marker_line = marker_line,
            delay_ms = behavior.delay_ms,
            delay_s = delay_s,
            version_output = behavior.version_output,
        );
        let mut f = File::create(&path).expect("create shim");
        f.write_all(script.as_bytes()).expect("write shim");
        drop(f);
        let mut perm = fs::metadata(&path).unwrap().permissions();
        perm.set_mode(0o755);
        fs::set_permissions(&path, perm).expect("chmod shim");
    }

    #[cfg(windows)]
    {
        // On Windows, write a .cmd that dispatches similarly.
        let path = path.with_extension("cmd");
        let script = format!(
            r#"@echo off
rem mcp-agent-mail-test-helpers: shim git binary (Windows)
rem For simplicity, only the version print and pass-through-to-real-git
rem are supported on Windows; the full segv/bus/hang matrix is Unix-only.

if "%1"=="--version" (
    echo {version_output}
    exit /b 0
)

if defined AM_TEST_REAL_GIT (
    "%AM_TEST_REAL_GIT%" %*
    exit /b %ERRORLEVEL%
)

exit /b 0
"#,
            version_output = behavior.version_output,
        );
        File::create(&path)
            .expect("create shim.cmd")
            .write_all(script.as_bytes())
            .expect("write shim.cmd");
    }

    path
}

/// Find the real git binary for the current process.
///
/// Honors `AM_TEST_REAL_GIT` for nested test contexts. Falls back to PATH
/// scan, then common absolute paths.
fn which_real_git() -> Option<PathBuf> {
    if let Ok(v) = std::env::var("AM_TEST_REAL_GIT") {
        let p = PathBuf::from(v);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(path_env) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_env) {
            let candidate = dir.join("git");
            if candidate.is_file() {
                return Some(candidate);
            }
            #[cfg(windows)]
            {
                let with_ext = candidate.with_extension("exe");
                if with_ext.is_file() {
                    return Some(with_ext);
                }
            }
        }
    }
    for hardcoded in ["/usr/bin/git", "/usr/local/bin/git"] {
        let p = PathBuf::from(hardcoded);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn run(shim: &Path, args: &[&str]) -> (Option<i32>, String, Option<i32>) {
        let out = Command::new(shim).args(args).output().expect("run shim");
        let code = out.status.code();
        #[cfg(unix)]
        let signal = {
            use std::os::unix::process::ExitStatusExt;
            out.status.signal()
        };
        #[cfg(not(unix))]
        let signal: Option<i32> = None;
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        (code, stdout, signal)
    }

    #[test]
    #[cfg(unix)]
    fn builds_and_prints_version() {
        let tmp = TempDir::new().unwrap();
        let shim = build_shim_git(
            tmp.path(),
            "git",
            &ShimBehavior {
                version_output: "git version 2.50.2".to_string(),
                exits: vec![ShimExit::Ok],
                ..Default::default()
            },
        );
        let (code, stdout, _) = run(&shim, &["--version"]);
        assert_eq!(code, Some(0));
        assert!(stdout.contains("git version 2.50.2"), "got: {stdout}");
    }

    #[test]
    #[cfg(unix)]
    fn sequence_of_exits_advances_per_call() {
        let tmp = TempDir::new().unwrap();
        let shim = build_shim_git(
            tmp.path(),
            "git",
            &ShimBehavior {
                exits: vec![ShimExit::Exit(7), ShimExit::Exit(0)],
                ..Default::default()
            },
        );
        // --version branch exits 0 regardless; use a non-version arg.
        let (c1, _, _) = run(&shim, &["status"]);
        assert_eq!(c1, Some(7), "first call should use exits[0] = Exit(7)");
        let (c2, _, _) = run(&shim, &["status"]);
        assert_eq!(c2, Some(0), "second call should use exits[1] = Exit(0)");
        // Looping: third call wraps to exits[0] again.
        let (c3, _, _) = run(&shim, &["status"]);
        assert_eq!(c3, Some(7), "third call should loop back to exits[0]");
    }

    #[test]
    #[cfg(unix)]
    fn segfault_action_raises_sigsegv() {
        let tmp = TempDir::new().unwrap();
        let shim = build_shim_git(
            tmp.path(),
            "git",
            &ShimBehavior {
                exits: vec![ShimExit::Segfault],
                ..Default::default()
            },
        );
        let (_code, _stdout, signal) = run(&shim, &["status"]);
        // bash kill -SEGV $$ causes the shim process to die with signal 11.
        // Different shells may map this to exit 139 instead of reporting
        // signal; accept either.
        let saw_segv = signal == Some(11) || {
            let out = Command::new(&shim).args(["status"]).output();
            out.as_ref().is_ok_and(|o| o.status.code() == Some(139))
        };
        assert!(saw_segv, "expected SIGSEGV or exit 139");
    }

    #[test]
    #[cfg(unix)]
    fn marker_file_records_each_call() {
        let tmp = TempDir::new().unwrap();
        let marker = tmp.path().join("marker.log");
        let shim = build_shim_git(
            tmp.path(),
            "git",
            &ShimBehavior {
                exits: vec![ShimExit::Ok],
                marker_file: Some(marker.clone()),
                ..Default::default()
            },
        );
        run(&shim, &["--version"]);
        run(&shim, &["status"]);
        run(&shim, &["log"]);
        let contents = fs::read_to_string(&marker).expect("marker exists");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 3, "expected 3 markers, got {lines:?}");
    }
}
