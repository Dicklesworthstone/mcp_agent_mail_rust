# GH369: Windows reconciliation checkpoint validation

## Scope and status

This correction addresses [GH369](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/369),
separately from GH353 request latency. The investigation started at
`65852eb86673d612ddf1d222ce9209f1a7d3f18b`. No checkpoint schema, scan policy,
retry-debt merge policy, recovery admission, or mailbox authority was replaced.

Native Windows/MSVC/NTFS qualification is required before treating this report
as release qualification. Windows GNU executables executed under Wine are
additional filesystem evidence, not a native Windows substitute.

## Reproduced operation

The original production `path_existing_prefix_has_symlink` was extracted
byte-for-byte from the starting commit and compiled with the repository's
`nightly-2026-08-31` toolchain, rustc `1.100.0-nightly (908501772 2026-08-30)`.
Its SHA-256 was
`6757bb8fa618085c1215c2e3d2fe70caf30736c831079b6ed302c41c4d63fef3`.

On the Windows GNU target under stock Wine 9, the actual Rust calls produced:

```text
symlink_metadata("\\\\?\\C:")    -> Uncategorized, raw OS error 1
symlink_metadata("\\\\?\\C:\\")  -> directory
```

Every following directory component and the complete canonical directory
succeeded. The unchanged production validator rejected both an existing
canonical directory and a missing checkpoint beneath it with error 1, before
checkpoint or lock creation. The ordinary drive spelling succeeded.

The same executable exercised the locked `fs2 = 0.4.3` implementation. A second
exclusive file handle returned Windows error 33 (`ERROR_LOCK_VIOLATION`) as
`ErrorKind::Uncategorized`, not `WouldBlock`. Its raw error matched
`fs2::lock_contended_error()`. Releasing the first owner let the second handle
acquire the lock.

## Correction and retained safeguards

- Assemble `Prefix` plus its following `RootDir` before the first metadata
  query. Check the complete root and every descendant. Bare and drive-relative
  prefixes still receive their original metadata checks; no OS error is ignored.
- Normalize fs2's precise contention error at message, agent, and active
  reservation checkpoint lock acquisition. Contention defers repair instead
  of selecting the unavailable-checkpoint fallback. Terminal reservation
  checkpoints already recognized that error and required no separate change.
- Identify message checkpoint canonicalization, metadata validation, lock open
  and acquisition, snapshot read, staging, sync, and publication failures in
  their error text. Retain the original I/O error in the source chain.
- Open Windows message checkpoint locks with `FILE_FLAG_OPEN_REPARSE_POINT`.
  Retain type checks and ownership checks after opening.
- Retain same-directory private staging, complete writes, staged-file sync,
  ownership/path revalidation, and atomic publication. Directory sync remains
  Unix-only. There is no delete-then-rename fallback.

The shared validator also serves agent, active-reservation, and terminal
reservation reconciliation checkpoints. This change does not redesign their
schemas or the existing non-Unix source/lock identity scheme.

## Regression coverage

The repository tests now exercise canonical existing and missing Windows
paths, linked ancestors and leaves, dangling links, symlink-before-`..`
traversal, and invalid metadata errors. Link fixtures must actually exist;
an API that reports success without creating a symlink fails the test.

The message checkpoint subprocess regression publishes and replaces a real
snapshot, checks contention from another process, exits after the staged
candidate has been synced but before publication, then restores the previous
watermark and retry debt in a fresh process. Marker files and exact exit codes
prevent a zero-test subprocess from appearing successful. This proves that
specific process-exit boundary, not arbitrary mid-write crashes or power loss.

A Windows test holds the existing checkpoint without delete sharing, forcing
the real atomic replacement to fail. It verifies that the previous checkpoint
survives and that a later save succeeds after releasing the obstructing handle.
Agent and active-reservation regressions retain a real competing owner,
verify that repair is deferred, then check committed Git repair and checkpoint
publication after ownership becomes available.

## Available execution evidence

| Execution | Result | Scope |
|---|---|---|
| Full native Linux storage checkpoint suite | 74 passed, 0 failed, 0 ignored | All four production checkpoint modules, real database authorization and reconciliation, competing-owner regressions, staged process exit, and restored repair progress |
| Full native Linux storage path tests | 3 passed, 0 failed, 0 ignored | Actual storage crate, canonical paths, real symlinks, and fail-closed metadata errors |
| Pinned native Linux path/lock harness | 5 passed, 0 failed, 0 ignored | Exact production validator, full inline path tests, real fs2 locking and error context |
| Pinned native Linux Store harness | 24 passed, 0 failed, 0 ignored | Exact production filesystem Store, cursor, snapshot, bounded reads, clock, and selected production tests |
| Windows GNU path/lock harness under Wine 9 | 6 passed, 1 failed, 0 ignored | Canonical/root/bare-prefix checks and real locking passed; symlink fixture failed |
| Windows GNU Store harness under Wine 9 | 22 passed, 1 failed, 0 ignored | New process-restart, contention, failed replacement, and error-context cases passed; stable-creation-time test failed |
| Windows GNU symlink-test compilation | Passed; no execution | Both Windows-only sidecar and post-staging link-substitution tests typechecked against the real Store |
| Workspace formatting | Passed | `cargo fmt --all -- --check` |
| Workspace check across all targets | Passed | `cargo check --locked --offline --workspace --all-targets` |
| Workspace Clippy across all targets | Passed | `cargo clippy --locked --offline --workspace --all-targets -- -D warnings` |
| Separate production-only workspace check | Resource-limited; not a pass | The Asupersync 0.5.0 compiler was killed by SIGKILL before the workspace check completed; a constrained retry was started |

The full storage runs used the pinned compiler and the portable feature graph.
The container has an 8 GiB memory limit. The successful checkpoint build used
one Cargo job, no incremental compilation, no debug info, optimization level
zero, and 512 Asupersync test-profile codegen units. Those settings change
compiler resource use, not production source, selected features, or test bodies.
The workspace checks and Clippy used one job with debug info and incremental
compilation disabled and optimization level zero. Cargo reported a separate
future-compatibility notice for the upstream `proc-macro-error2` dependency.

The scratch Store harness retained 43 byte-identical excerpts from five
production files, with recorded ranges and SHA-256 digests. It used actual
filesystem, serialization, clock, and locking implementations. It excluded
live-database capture, source authorization, and worker execution; these
results do not qualify those paths. The native workflow runs the full crate.

The two Wine failures remain failures. No production test was skipped or
weakened to accommodate them:

1. Wine 9's [CreateSymbolicLinkW](https://github.com/wine-mirror/wine/blob/wine-9.0/dlls/kernelbase/file.c#L944)
   is a success-returning stub. Direct diagnostics confirmed that neither
   directory nor file symlink objects were created.
2. Wine 9's [get_file_times](https://github.com/wine-mirror/wine/blob/wine-9.0/dlls/ntdll/unix/file.c#L1804)
   falls back to modification time for creation time when host `struct stat`
   lacks a birth-time field. Ordinary writes therefore changed the binding's
   observed creation time in this environment. This is not native NTFS evidence.

Wine was the distribution's `9.0~repack-4build3` package, without custom DLLs,
overrides, or API shims. In particular, the prefix's `bcryptprimitives.dll`
matched the installed `libwine:amd64` file byte-for-byte, SHA-256
`48d5c452bf8f2a1d6acb607580d735b753052b70d91f88e50ab2991d7f60c721`.

## Native qualification entry point

`.github/workflows/archive-fsync-matrix.yml` contains independent Windows and
Linux checkpoint jobs, triggered by relevant pushes to `main`. They use the
repository-pinned compiler and the shipped portable feature graph. The
metadata diagnostic does not depend on the full Cargo build. The lifecycle
job attempts all five test groups and rejects empty or ignored-test successes.
Unrelated broad-CI failures cannot suppress these jobs.

The essential local commands are:

```sh
cargo check --locked -p mcp-agent-mail-storage --no-default-features --features portable --lib
cargo test --locked -p mcp-agent-mail-storage --no-default-features --features portable --lib path_prefix_tests:: -- --nocapture
cargo test --locked -p mcp-agent-mail-storage --no-default-features --features portable --lib checkpoint::tests -- --nocapture
```

Native Windows symlink behavior, source/lock identity on NTFS, and the complete
live-database recovery path must pass that native lane. No native UNC share or
arbitrary power-loss coverage is claimed.
