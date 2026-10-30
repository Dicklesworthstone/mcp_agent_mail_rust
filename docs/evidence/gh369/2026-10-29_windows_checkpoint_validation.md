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
| Separate production-only workspace check | Blocked, exit 101; not a pass | The Asupersync 0.5.0 compiler was killed by SIGKILL before the workspace check completed |
| Affected portable production-library check | Blocked, exit 101; not a pass | The exact storage-library production command below also received SIGKILL while checking Asupersync, without a Rust diagnostic |

The full storage runs used the pinned compiler and the portable feature graph.
The container has an 8 GiB memory limit. The successful checkpoint build used
one Cargo job, no incremental compilation, no debug info, optimization level
zero, and 512 Asupersync test-profile codegen units. Those settings change
compiler resource use, not production source, selected features, or test bodies.
The workspace checks and Clippy used one job with debug info and incremental
compilation disabled and optimization level zero. Cargo reported a separate
future-compatibility notice for the upstream `proc-macro-error2` dependency.
The shared container recorded out-of-memory kills, which supports resource
pressure as the explanation for the production compiler failures; those
failures do not establish a source-level defect or qualify either production
build. An additional retry without a completion receipt is not counted.

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

## Publication and outstanding native execution

The two substantive corrections were committed directly to `main`, preserving
concurrent unrelated changes:

- [`e460081d`](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/e460081d6ce23e637359e4b1747b1b21dbcf8719): rooted Windows prefix validation, adversarial path tests, and native CI entry points.
- [`e795a951`](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/commit/e795a9511f6ad01eaf81c0b8f2f99646dfe83e66): checkpoint ownership, operation-level diagnostics, lifecycle regressions, and execution evidence.

The Actions runs API returned zero runs for both published commit SHAs during
this session. The workflow has matching `push` filters and `workflow_dispatch`,
but no native Windows job result was obtained. The available GitHub connection
does not expose workflow dispatch, and the workflow browser page was signed
out. This does not establish whether Actions or the workflow is disabled.
An authenticated manual dispatch of Archive Fsync Matrix on the corrected
`main` is the remaining native qualification step.

## Follow-up: component diagnostics and opened-handle validation

This follow-up builds on `1abf33ecf05d3080a70a6042cfc8dd7a2e2694e3` and preserves
the preceding corrections and all 44 existing message checkpoint tests. The
earlier execution table describes its original source revision; it must not be
read as a fresh full-crate qualification of this follow-up.

Path-validation errors now retain the exact failing `symlink_metadata`
component, the original I/O kind, and the native error source. Message checkpoint
warnings add `phase=initialization|restore|publication`, `error_kind`, and
`raw_os_error`. The latter traverses the error source chain, including the
component validator, rather than discarding the native code when context is
added. Publication and binding capture retain typed errors through the warning
boundary. Filesystem errors identify their operation and affected path.

Both lock admission paths share a no-follow open and inspect the opened handle
before attempting the lock. Windows checks reject reparse points and multiple
hard links on opened locks and on checkpoint/lock sidecars. The existing Unix
no-follow flags, permissions, device/inode checks, and directory synchronization
remain. The checkpoint schema, binding serialization, bounded snapshot reads,
paired cursor/retry-debt publication, and live-source authorization are unchanged.

The follow-up retains the existing cross-process restart, contention,
post-staging link-substitution, and Windows replacement-sharing tests. It adds
long canonical Windows checkpoint creation/replacement/recovery, a real failed
rename that preserves the preceding durable checkpoint, and dangling-link or
linked-parent refusal. Existing unsafe-sidecar coverage now checks both lock
and checkpoint symlinks/hardlinks on Unix and Windows, including direct lock
opens. The process test uses bounded child polling and a reap guard and verifies
the actual abandoned staged snapshot before a fresh process restores the old
watermark and retry debt.

### Follow-up execution evidence

| Check | Result | Scope |
|---|---|---|
| Final path helper and inline tests, native Linux | 3 passed, 0 failed, 0 ignored | Exact final production helper and inline test module |
| Final checkpoint filesystem component, native Linux | 29 passed, 0 failed, 0 ignored | Exact Store, binding filesystem capture, snapshot/cursor/debt logic, warning function, real filesystem/clock/locking, and selected production tests |
| Final Windows GNU target typecheck | Blocked during disk exhaustion; no valid completion | The path command returned 101, but the disk also prevented its log/exit receipt from being written; the checkpoint cross-check was not attempted |
| Final native Windows execution | Not run | No Windows/MSVC/NTFS lifecycle or symlink qualification obtained |
| Final workspace formatting | Passed, exit 0 | `cargo fmt --check` with the repository-pinned toolchain |
| Final whitespace validation | Passed, exit 0 | `git diff --check` |

The focused harnesses used the repository-pinned compiler and dependency
versions, sources, and checksums. The checkpoint harness contained 54
byte-identical production excerpts verified by an independent reconstruction
audit; all 105 external dependency entries matched the repository lockfile.
It included 33 production checkpoint test definitions, of which 29 applied on
Linux. Fourteen database/runtime test definitions and their live integration
paths were explicitly excluded. These component results do not qualify SQLite
authorization, the maintenance worker, or MCP messaging end to end.

The attempted final Windows command was:

```sh
cargo check --tests --locked --offline --target x86_64-pc-windows-gnu
```

It was intended only to typecheck the Windows branches; even successful
cross-compilation would not establish execution.
Earlier draft-source Windows cross-checks are not counted as a final integrated
source pass. No source or test was weakened to work around the resource failure.
The failed archive copy and receipt write reported `No space left on device`;
the incomplete Windows log is not evidence of a source-level compiler error.

The validated source SHA-256 values are:

```text
crates/mcp-agent-mail-storage/src/lib.rs
d97b5795751a9aa0d6dd79ab5f3a51c2af847bcf672d3a5660ac2bdb207c6a88
crates/mcp-agent-mail-storage/src/recovery/message_reconcile/database/checkpoint.rs
eaf816df41a26cc48bdcc146022ac19ff9f997f94cf97d090ee174bbac77cc2f
```

### Follow-up build limitations

The separate portable production-library command was attempted with the pinned
compiler, one build job, no incremental compilation, no debug information, and
optimization level zero:

```sh
cargo check --locked --offline -p mcp-agent-mail-storage --no-default-features --features portable --lib
```

It exited 101 after the Asupersync 0.5.0 compiler received SIGKILL, before the
changed storage library was checked. The container's out-of-memory kill counter
increased during that attempt. This is a failed qualification gate, not a
source-level compiler diagnostic or a passing production build. An earlier
attempt without a completion receipt is not counted.

The required workspace all-targets, production-only (`--lib --bins`), and
Clippy (`--all-targets -- -D warnings`) commands were also attempted with
`--locked --offline`. Each exited 101 because `addr2line 0.25.1` was absent from
the cache. The shared filesystem had no generally available space, so these
attempts did not download the rest of the workspace dependencies. No workspace
compiler or Clippy pass is claimed for the follow-up. UBS was not installed in
this environment.

No native Windows/MSVC/NTFS execution was obtained for this follow-up. The
Actions runs API returned zero runs for `1abf33ecf05d3080a70a6042cfc8dd7a2e2694e3`.
The isolated Wine reproduction above identifies the bare-prefix metadata
operation in that environment; it does not retroactively instrument the
reporter's native v0.3.39 process. The new warning fields distinguish that
operation from lock, read, sync, and replacement failures in a future native
report.

## 2026-10-30 follow-up: native availability, file identity, and publication ownership

### Native qualification is still outstanding

The investigation resumed at `6bbadb7a95b22aa2af6dca3ea14b6751e52a27ea` and
incorporated concurrent changes through
`a40050d666086a4b005f4068581944ccd90f097d` without replacing unrelated work.

The repository Actions runs API returned zero runs created on or after
2026-10-29 and zero runs for each of `e460081d`, `e795a951`, `b12861ad`, and
the inspected main revisions. The most recent Archive Fsync Matrix run found
was [32154617836](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/actions/runs/32154617836),
from August 18, before these fixes; its jobs did not include Windows.

The execution blocker is now directly identified: the authenticated
[repository Actions settings](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/settings/actions)
showed **Disable actions** selected, with the explanation that the Actions tab
is hidden and no workflows can run. No repository execution policy was changed.
Rerunning an August job would execute the old source and cannot qualify this
correction. No available native Windows worker was obtained.

### Additional correctness defects and corrections

**Creation timestamps are not Windows file identity.** The Windows source
bindings and message/agent/active checkpoint ownership checks used creation
time alone; terminal checkpoints had no corresponding Windows file-identity
comparison. NTFS can retain creation time across rename/recreation through its
tunnel cache. This is documented by Microsoft in
[GetFileTime](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getfiletime)
and [FltGetTunneledName](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/fltkernel/nf-fltkernel-fltgettunneledname).
A different file can therefore pass a timestamp-only comparison.

The new shared `windows_file_identity` helper obtains the volume serial number
and 64-bit file index from an already-open handle using the existing pinned
`cap-std`/`cap-fs-ext` dependencies. Their pinned `winx` implementation calls
`GetFileInformationByHandle` and propagates native failure; there is no
timestamp or placeholder-ID fallback. Microsoft documents
[volume/file-index comparison](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getfileinformationbyhandle)
as a way to identify the same underlying target. No dependency or unsafe
repository code was added.

All four checkpoint bindings now read Windows source metadata from a no-follow
regular-file handle and include its volume/file index alongside the existing
creation timestamp. Ownership checks compare the retained lock with a freshly
opened no-follow handle for the named lock. The three adjacent stores also
open Windows lock leaves with `FILE_FLAG_OPEN_REPARSE_POINT`; the message
store retains its existing opened-handle reparse/hard-link validation.

The serialized schema and logical mailbox-generation checks are unchanged.
Previously written Windows bindings containing only timestamps no longer
match and take the existing conservative rescan path. This changes scheduling
hints, not mailbox authority. Ordinary writes must still preserve the binding.
The helper exposes the API's 64-bit identity; no full ReFS 128-bit identity or
UNC-share qualification is claimed.

**Active-reservation publication needed a final ownership check.** That store
previously validated ownership only before staging. It now revalidates the
lock, checkpoint target, and source binding after the actual staged write and
file sync, immediately before atomic publication. A private callback allows
tests to replace a lock or database at that precise boundary; production uses
a no-op. Previous checkpoint bytes remain intact when revalidation fails.

The existing bounded reads, private same-directory staging, complete staged
writes and sync, atomic replacement, retry-debt/cursor validation, live-source
authorization, and Unix safeguards remain. No delete-then-rename publication
or Wine-specific assertion relaxation was introduced.

### Regressions and completed execution

Windows regression fixtures explicitly copy the original creation timestamp
onto a distinct replacement using the stable `FileTimesExt::set_created` API.
They do not depend on tunnel-cache timing. Coverage includes stable identity
across ordinary writes/reopening/hard links/rename, source replacement with an
unchanged logical generation, old-owner read/write refusal, legitimate new-owner
continuation, and replacement during staged publication.

The message lifecycle component also retains the real subprocess restart
test: a child exits after syncing the staged candidate, and a fresh process
restores the prior paired watermark/retry debt. That is a specific process-exit
boundary, not arbitrary crash timing or power loss.

| Check on this follow-up | Result | Exact scope |
|---|---|---|
| Native Linux filesystem components, overlay-backed fixtures | 54 passed, 0 failed, 0 ignored; exit 0 | 29 message, 9 active-reservation, 7 agent, 6 terminal-reservation, and 3 path tests |
| Final restored Linux component rerun, tmpfs fixtures | 54 passed, 0 failed, 0 ignored; exit 0 | Same final production excerpts and repository-matching dependency lock, after the negative control |
| Intentional scratch negative control | 1 passed, 2 expected failures; exit 101 | Removing only active post-staging revalidation let both stale publications incorrectly succeed; the unchanged terminal control passed |
| Final Windows MSVC component typecheck from Linux | Passed; exit 0 | `cargo check --tests --locked --offline --target x86_64-pc-windows-msvc`; all four filesystem components, core identity helper, and selected Windows test bodies; **no Windows execution** |
| Source/dependency reconstruction audit | Passed | 95 byte-identical production excerpts; every byte of 8 generated files accounted for by those excerpts and declared import/module glue; 14 source-file hashes matched; all 106 external versions, sources, and checksums matched the repository lockfile |
| Workspace formatting and whitespace | Passed; exit 0 | Pinned `cargo fmt --all -- --check` and `git diff --check` |
| Workflow static validation | Passed | YAML parsing, Bash syntax, and preservation of all five existing test gates |
| Native Windows/MSVC/NTFS runtime | Not run | Blocked by repository Actions policy; no alternate native host was available |
| Wine | Not run in this follow-up | Earlier Wine failures and their limits remain unchanged |

The component harness extracts production code and tests without changing
their algorithms or assertions. It explicitly excludes live database
capture/authorization, reconciliation-worker integration, Git operations, and
MCP execution. Real filesystem, subprocess, serialization, clock, and locking
implementations are retained. These passes do not qualify the complete storage
crate or a shipped Windows binary. A harness-only unused-import warning remains
because the corresponding live-database tests are excluded.

The final checks used `nightly-2026-08-31`,
`rustc 1.100.0-nightly (908501772 2026-08-30)`. The Windows target standard library
was verified against its official distribution checksum. The initial matched
54-test Linux run completed before the shared overlay filled. An attempted
post-control rerun then failed with `No space left on device`; preserving all
files, the final checks were completed with an approved memory-backed target
and temporary directory. This is why the final Linux rerun is explicitly
identified as tmpfs evidence.

Completed log SHA-256 receipts:

```text
Linux overlay component run (54 passed)
b4a2ae76eaa8caf14e9bbd9978c0b0e74ba991341a92945fde3aa43436a71ebb
Linux final tmpfs component run (54 passed)
89e8b54a8c47ebf92de12707f0746b390ad2906b4919a79cb5f3a2babc63c93c
Final Windows MSVC cross-target typecheck (exit 0)
66aea41a9dd374cfd588f353cbaa2433c4fb7501af1d0512ad59b178ec7a4dff
Final negative-control run (two expected active-publication failures)
1c9f5711f16dcb629620dfbcc18c9f163425059f6b71a00fb685ad1d8276e4fc
Component dependency lockfile
e3096b2eaddeefb5e9044d8bfed42048a8df8fc6eeceaa4131200141c3398d24
```

### Full-build limitations

The full portable storage checkpoint suite was attempted with the pinned
compiler, the repository lockfile, one Cargo job, no incremental compilation,
no debug information, optimization level zero, and 512 test-profile codegen
units. It exited 101 when the Asupersync 0.5.0 compiler received SIGKILL before
the changed storage crate was compiled. The shared 8 GiB cgroup's OOM-kill
counter increased, and the shared overlay also exhausted its available space.
This is a failed full-crate gate, not a storage compiler diagnostic or a pass.

The separate portable production-library gate,
`cargo check --locked --offline -p mcp-agent-mail-storage --no-default-features
--features portable --lib`, was also attempted in the memory-backed target.
It exited 101 when Asupersync's compiler received signal 9 (SIGKILL), before
compiling the changed storage crate. Its log SHA-256 is
`db822c908ba83946ce58bb72fe6204da528a4da51f831e1669dadf48efaecfa1`.
This production gate remains unverified.

The required workspace all-targets check, production-only `--lib --bins`
check, and Clippy `--all-targets -- -D warnings` were each attempted with
`--locked --offline`. All exited 101 because `addr2line 0.25.1` was not cached;
the full disk prevented further ordinary dependency downloads. No workspace
build or Clippy pass is claimed. UBS was not installed.

### Prepared native workflow and remaining qualification

Archive Fsync Matrix retains its independent production-library check and all
five nonempty, nonignored path/checkpoint test gates. It now also:

- Triggers on changes to the shared core disk helpers.
- Requires an actual `x86_64-pc-windows-msvc` compiler host.
- Compiles and runs a native Rust probe for the test fixture temporary directory.
- Resolves a real witness file's volume with `Get-Volume -FilePath`, requires
  NTFS, and records a JSON environment receipt even if the check fails.
- Keeps subsequent Cargo tests on that measured temporary directory and MSVC target.
- Executes the core Windows file-identity regression and retains its log.

The PowerShell environment gate has not executed in this session. Native
canonical/long-path behavior, symlink/reparse and hard-link rejection, actual
NTFS identity, Windows lock contention, interrupted publication, atomic
replacement, and persistence across fresh Windows processes still require a
successful run of the prepared workflow on the corrected source. The complete
live-database recovery path and production feature graph must also pass.
Issue #369 remains open pending that native qualification.
