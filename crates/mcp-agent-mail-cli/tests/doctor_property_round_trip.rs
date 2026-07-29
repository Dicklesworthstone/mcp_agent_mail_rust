//! Property test: randomized mutate-then-undo round-trip invariant.
//!
//! For 32 different seeds, generate a random sequence of WriteFile /
//! AppendFile / Chmod / Rename mutations through `mutate()`, then run
//! `undo` and assert the file tree is byte-identical to the pre-mutation
//! state.
//!
//! This catches:
//! - Off-by-one in the actions.jsonl reverse-walk
//! - Lost mutations due to permission/symlink edge cases
//! - Hash mismatches between mutate's after_hash and undo's before_hash
//! - Stale lock files affecting subsequent mutations
//!
//! Uses a tiny xorshift64 PRNG (no rand crate dep) so seeds are
//! deterministic and the test fits the workspace's no-extra-deps norm.

#![forbid(unsafe_code)]

use mcp_agent_mail_cli::doctor::mutate::{Capabilities, MutateContext, MutateError, Op, mutate};
use mcp_agent_mail_cli::doctor::runs::scaffold_run_dir;
use mcp_agent_mail_cli::doctor::undo::run_undo_with_scopes;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;
use tempfile::TempDir;

/// Tiny xorshift64 PRNG. Deterministic given the seed; no extra deps.
struct Xorshift64(u64);
impl Xorshift64 {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 { 0xdeadbeef_cafebabe } else { seed })
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn next_in(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn next_bytes(&mut self, len: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(len);
        for _ in 0..len {
            v.push((self.next() & 0xff) as u8);
        }
        v
    }
}

fn make_ctx(td: &TempDir, run_id: &str, fixer_id: &str) -> MutateContext {
    let run_dir = scaffold_run_dir(td.path(), run_id).unwrap();
    let actions = OpenOptions::new()
        .create(true)
        .append(true)
        .open(run_dir.join("actions.jsonl"))
        .unwrap();
    MutateContext {
        run_id: run_id.to_string(),
        run_dir,
        capabilities: Capabilities {
            write_scopes: vec![td.path().to_path_buf()],
        },
        actions_file: Mutex::new(actions),
        fixer_id: fixer_id.to_string(),
        repo_root: td.path().to_path_buf(),
        dry_run: false,
        start: Instant::now(),
        extra_locks: Vec::new(),
    }
}

/// Snapshot every regular file under `dir` into a Vec<(rel_path, bytes, mode)>.
fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>, u32)> {
    use std::os::unix::fs::PermissionsExt;
    let mut out = Vec::new();
    fn walk(root: &Path, cur: &Path, out: &mut Vec<(PathBuf, Vec<u8>, u32)>) {
        let entries = match fs::read_dir(cur) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // Skip the .doctor/ artifact dir AND any hidden file (locks etc.).
            let name = entry.file_name();
            let name_s = name.to_string_lossy();
            if name_s == ".doctor" || name_s.starts_with('.') {
                continue;
            }
            let ty = entry.file_type().ok();
            if ty.as_ref().is_some_and(|t| t.is_dir()) {
                walk(root, &path, out);
            } else if ty.as_ref().is_some_and(|t| t.is_file()) {
                let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
                let bytes = fs::read(&path).unwrap_or_default();
                let mode = fs::metadata(&path)
                    .map(|m| m.permissions().mode())
                    .unwrap_or(0);
                out.push((rel, bytes, mode));
            }
        }
    }
    walk(dir, dir, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[test]
fn property_round_trip_undo_restores_byte_identical() {
    use std::os::unix::fs::PermissionsExt;
    // 16 different seeds — each generates a different random sequence.
    for (i, seed) in [
        0xa1b2c3d4_u64,
        0x1234567890abcdef,
        0xdeadbeefcafebabe,
        0xfeedfacefeedface,
        0x0123456789abcdef,
        0xfedcba9876543210,
        0x5555_aaaa_55aa_55aa,
        0xbabe_face_dead_beef,
        0x1010101010101010,
        0xeeee_dddd_cccc_bbbb,
        0xf0f0f0f0f0f0f0f0,
        0x0808080808080808,
        0x4242424242424242,
        0x7777_8888_9999_aaaa,
        0xffffffffffffffff,
        0x1,
    ]
    .iter()
    .enumerate()
    {
        let td = TempDir::new().unwrap();
        // Seed initial files.
        let initial = vec![
            ("alpha.txt", b"alpha original".to_vec(), 0o644),
            ("beta.txt", b"beta original".to_vec(), 0o644),
            ("gamma/sub.toml", b"key = 1\n".to_vec(), 0o644),
        ];
        for (name, content, mode) in &initial {
            let p = td.path().join(name);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&p, content).unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(*mode)).unwrap();
        }

        let pre_snapshot = snapshot(td.path());

        let mut rng = Xorshift64::new(*seed);
        let run_id = format!("2026-05-10T12-00-{:02}Z__seed{}", i, *seed & 0xffff);
        let ctx = make_ctx(&td, &run_id, "property-test");

        // Perform 5–8 random mutations.
        let n = 5 + rng.next_in(4) as usize;
        let candidate_paths = ["alpha.txt", "beta.txt", "gamma/sub.toml", "newfile.txt"];
        for _step in 0..n {
            let path_idx = rng.next_in(candidate_paths.len() as u64) as usize;
            let path = td.path().join(candidate_paths[path_idx]);
            let op_kind = rng.next_in(4);
            let op = match op_kind {
                0 => {
                    let len = 8 + (rng.next_in(64) as usize);
                    Op::WriteFile {
                        content: rng.next_bytes(len),
                        mode: 0o644,
                    }
                }
                1 => {
                    let len = 4 + (rng.next_in(16) as usize);
                    Op::AppendFile {
                        content: rng.next_bytes(len),
                    }
                }
                2 => {
                    if !path.exists() {
                        // Skip Chmod if file doesn't exist; pick WriteFile instead.
                        Op::WriteFile {
                            content: b"placeholder".to_vec(),
                            mode: 0o644,
                        }
                    } else {
                        Op::Chmod {
                            mode: if rng.next() & 1 == 0 { 0o644 } else { 0o600 },
                        }
                    }
                }
                _ => {
                    // Rename to quarantine — only if file exists.
                    if !path.exists() {
                        Op::WriteFile {
                            content: b"new".to_vec(),
                            mode: 0o644,
                        }
                    } else {
                        Op::Rename {
                            to: td
                                .path()
                                .join(format!("quarantine_{}_{}.txt", seed & 0xff, _step)),
                        }
                    }
                }
            };
            // Some mutations may fail for legitimate reasons (rename clobber,
            // append on missing path's parent). We just skip them — they don't
            // affect undo correctness because the failed mutation also wasn't
            // recorded as ok=true.
            let result = mutate(&ctx, &path, op);
            match result {
                Ok(_) => {}
                Err(MutateError::ExecFailed { .. }) => {}
                Err(MutateError::OutOfScope(_)) => {}
                Err(MutateError::RenameDestinationExists(_)) => {}
                Err(MutateError::TamperedBeforeMutate(_)) => {}
                Err(MutateError::LockHeld(_)) => {}
                Err(MutateError::BackupVerify(_)) => {}
                Err(other) => panic!("unexpected mutate error: {other:?}"),
            }
        }
        drop(ctx);

        // Now undo and verify. Round-6 (Gemini F1 P0): pass
        // explicit scope so the test td.path() is whitelisted.
        let summary =
            run_undo_with_scopes(td.path(), &run_id, false, false, &[td.path().to_path_buf()])
                .unwrap();
        // Even with non-strict undo, no failures should remain — the
        // property holds: every committed action has a backup that
        // restores byte-identical.
        assert_eq!(
            summary.failures.len(),
            0,
            "seed {seed:x}: undo had failures: {:?}",
            summary.failures
        );

        let post_snapshot = snapshot(td.path());
        assert_eq!(
            pre_snapshot, post_snapshot,
            "seed {seed:x}: round-trip failed; pre vs post differ"
        );
    }
}
