# GH373: backup retention and Linux reader evidence

The first sections preserve the earlier conservative-policy investigation.
The **2026-11-03 follow-up** at the end describes the subsequently implemented
snapshot-specific unlink policy and its separate validation receipts.

## Scope and source identity

The investigation started from `de449d46f905785dfe561514e9f8bc1d1804d17c`
and [issue #373](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/373).
The port plan, current bridge-plan retention sections, and `br-5nr0m`,
`br-cdsge`, `br-kp1in.36`, `br-kp1in.63`, `br-kp1in.71`, and
`br-refused-pass-window-policy-q7mxt` were reviewed before implementation.
None of those broader open requirements is closed by this work.

The decisive reader experiments compiled the exact production
`recovery_retention_namespace.rs`, including its existing tests, in a small
standalone Cargo harness. Its syscall dependencies were the repository's
`nix 0.31.3`, `rustix 1.1.5`, `fs2 0.4.3`, and `tempfile 3.27.0`.
There was no replacement procfs, unlink, permission, or namespace implementation.
This is native component evidence, not a complete Agent Mail executable run.

| Namespace source | SHA-256 |
| --- | --- |
| Unchanged starting main | `39002c251e57a03148e51bb0b7128b60d3627ee19eca5d9fc6c12df68d5e2b92` |
| Corrected source used for the 22-case process matrix | `09fa295c7fe137f240c57a2520e27c36b6f4cfb9c2e8121e7c54b510b688a74e` |
| Final source, after making the terminal predicate `const fn` | `3bdb9444f51e2d7c2951558c676103257f6bfabdf69c8c7329b52fce136f85f1` |

All candidate deletions used newly created private test directories. No real
mailbox or pre-existing recovery artifact was modified.

Preserved receipts: [baseline processes](exact_source_baseline_dumpable_driver.jsonl),
[corrected processes](exact_source_candidate_terminal_witness.jsonl),
[PID-namespace control](exact_source_pid_namespace_isolated.json),
[39 module tests](namespace_unit_final.log), and
[strict module Clippy](namespace_clippy_final.log).

## What reproduced, and what did not

Real observer and reader processes ran as UID/GID 65534. Reader children used
`prctl(PR_SET_DUMPABLE, 0)`, and the probes verified actual procfs permission
denials, matching UIDs, and the kernel's status records. Mapping fixtures closed
every descriptor to the candidate and retained only the mapping.

| Controlled case | Starting main | Corrected source |
| --- | --- | --- |
| Generic opaque same-user process unrelated to the candidate | Permission refusal | Permission refusal |
| Generic opaque process retaining FD, `O_PATH`, or descriptorless mapping | File retained | File retained |
| Opaque process named `ssh-agent`, `(sd-pam)`, or `sshd-session` retaining those references | **Actual unlink succeeded while the reader retained the bytes** | File retained |
| Terminated leader with a surviving worker retaining FD or mapping | **Actual unlink succeeded while the worker retained the bytes** | Visible reader refused; opaque worker refused |
| Terminated leader with an inspectable, unrelated idle worker | Not executed; source allows cleanup | Cleanup succeeds |
| Terminated leader with an opaque unrelated worker | Not executed; source skips the leader without inspecting workers | Permission refusal |
| Reader exits after a refusal | Subsequent cleanup succeeds | Subsequent cleanup succeeds |

The reporter's named-daemon refusal on 0.3.40 was therefore **not reproduced
unchanged against starting main**: main already exempted those names. A generic
same-user nondumpable process did reproduce the conservative refusal. More
seriously, the existing exemptions also admitted genuine readers. Process names
are mutable, and credential changes do not close inherited file references.

The correction removes name, GID, capability, and partial-UID exemptions. An
inaccessible process sharing any caller UID or the candidate owner's UID stays
uncertain. Inaccessible processes with disjoint UIDs are still skipped under
the existing policy; they can retain inherited or passed descriptors and
mappings. Readable foreign readers are still inspected. The procfs scan is
observational and cannot atomically exclude arbitrary external opens. Snapshot
authority supplies the separate exclusion for cooperating Agent Mail readers
and publishers.

A terminal task is known to have no own readers only when all three kernel
facts hold: terminal state, no address space, and `FDSize: 0`. This does not
certify the whole process. A post-scan status check detects a dead leader and
inspects surviving tasks within the unchanged inventory bound. The
surviving-worker fallback does not exclude descriptor numbers from another
task's table. It does not broaden inspection to every private descriptor table
of live-leader threads; that remains part of the existing external-reader
visibility limitation.

## Actual PID-namespace control

A separate unprivileged control used `unshare --user --map-root-user --pid
--fork` while preserving the ancestor `/proc` mount. The controller's local PID
was 1 while `/proc/self` named its PID in the mount's namespace.

Starting main passed preflight but refused deletion with `WouldBlock`, naming
the cleaner's own retained descriptor. Corrected preflight and deletion both
succeeded. The fix derives self identity from the mounted `/proc/self`, not
`std::process::id()`.

## Retention-count correction

Starting main unconditionally enqueued eligible native parked periodic images
with `within_window: false`. Consequently the keep window covered resident
images while all eligible parked images could be reclaimed. That contradicts
the documented policy and explains the report's keep-300 case by source
inspection; full native count execution was blocked as described below.

Eligible resident and native parked periodic images now enter the existing
generation/collision sorter together. The publication footprint is calculated
before merging, and historical entries are moved out of their old queue, so
neither budget nor report accounting double-counts them. Historical candidates
still revalidate their whole transaction directory immediately before deletion.

The `.bak` slot continues to participate in the count; it and the verified pin
remain independently protected. The prior capacity-shortening policy remains.
Manifest-bearing and interrupted transaction directories stay protected and
do not enter generation ranking. Other strictly named periodic images can
occupy the keep window before their contents are verified, but deletion still
requires the unchanged lineage, integrity, and recovery checks. Protected
historical evidence still consumes publication admission capacity.

Three real-filesystem regressions cover thirteen mixed images under keep 300,
global chronological selection with a later keep reduction, and numeric
collision ordering despite copied mtimes. Two older historical safety fixtures
request keep one so their original verification/refusal checks still exercise
an expired image. The existing 720-generation test keeps all 720 iterations
and its original bounds; its accounting now includes retained parked images.

## Regression coverage and validation boundary

The final namespace module's **39 tests passed**, zero ignored, as an
unprivileged Linux user. Strict pedantic/nursery Clippy with `-D warnings`
passed for that exact module, using the existing workspace and DB-crate lint
allowances. The real-process matrix passed 22 cases before the behavior-neutral
`const fn` adjustment. Repository-wide `cargo fmt --check` passed.

Five new public `SnapshotRetentionGuard` integration tests are registered in the
DB crate's consolidated `tests/it.rs`:

- `external_descriptors_and_mappings_preserve_backups_until_reader_exit`
- `nondumpable_process_names_never_authorize_deletion`
- `exited_thread_group_leader_does_not_hide_a_live_backup_reader`
- `real_unlink_permission_refusal_preserves_bytes_and_accounting`
- `concurrent_cleanup_and_process_restart_preserve_recovery_evidence`

They cover six process names across four opaque-process modes, eight
dead-leader controls, actual unlink permission denial, concurrent cleanup
admission, SIGKILL after a completed unlink, fresh-process recovery, and
preservation of incomplete evidence and the verified snapshot. The opaque
fixtures reserve nextest's existing thread capacity in default and CI profiles;
no test deadlines or original large-backlog acceptance thresholds are raised.
Their Python process helper was also independently executed, including a
deliberately failing worker assertion that must exit nonzero.

**The count regressions and public-guard tests have not yet executed.** A focused
harness included the exact original/corrected rotation modules and these tests
against the real current DB/core crates, with no persistence substitutes.
Apart from the harness package itself, its 362 locked dependency records were
an exact subset of the repository's 868 records, and all 20 patch entries
matched. Three focused build attempts were killed
while compiling unchanged registry `asupersync 0.5.0`, before test binaries
existed. The shared environment's 8 GiB cgroup exhausted memory. The second
attempt used a disclosed harness-only opt-level-zero override for that
dependency. The third additionally used the pinned Cargo's documented
`-Zprofile-hint-mostly-unused` option with
`profile.dev.package.asupersync.hint-mostly-unused=true`; the actual compiler
command contained `-Zhint-mostly-unused`, and it too received SIGKILL. These
attempts changed compiler settings only, not dependency versions or production
implementations. They are resource failures, not passing tests or observed test
assertion failures.

The repository-wide `cargo check --locked --workspace --all-targets` was also
attempted, then interrupted while still checking dependencies under the same
memory pressure to prioritize the focused retry. The separate production-graph
check and workspace-wide Clippy were not run after those dependency build
failures; they remain follow-up gates. Only the exact-module Clippy result above
is passing evidence. Neither `rch` nor `ubs` was available on the execution
environment's PATH. No full workspace, release, or complete issue closure is
claimed. The build attempts are recorded in
[native-build-validation.json](native-build-validation.json).

Supported native commands for completing the unexecuted coverage are:

```sh
cargo test --locked -p mcp-agent-mail-db --no-default-features --test it retention_linux:: -- --test-threads=1
cargo test --locked -p mcp-agent-mail-server --no-default-features --features portable --lib periodic_retention_ -- --test-threads=1
cargo test --locked -p mcp-agent-mail-db --no-default-features --lib recovery_retention::namespace::tests:: -- --test-threads=1
```

The larger reader-scan performance, other-user visibility, and platform work in
`br-5nr0m`, and the original large-backlog requirements in `br-kp1in.63`, remain
open. Arbitrary opaque same-user processes cannot be declared unrelated merely
because inspecting them is inconvenient; the corrected policy keeps their
backups until reader evidence is available.

## 2026-11-03 follow-up: reclamation with unobservable external readers

This follow-up starts at `534a8dd635db5f5b5db3b995b0e98a819ceba7d1`.
The one-retry policy still reproduced the operational failure: an unrelated,
same-user nondumpable process could refuse every eligible backup indefinitely.
The fix does not classify such a process as harmless or reader-free.

### Deliberate safety boundary

Only `SnapshotRetentionGuard`'s fully verified **superseded periodic snapshot**
lane can accept a typed unobservable-reader result. Its existing exclusive
recovery/snapshot authority, verified successor, lineage or identical-byte
proof, standalone SQLite checks, pin revalidation, no-follow parent handles,
single-link inode identity, mtime/ctime validation and directory fsyncs remain.
The uncertainty policy additionally requires candidate ownership by the caller's
filesystem UID and an owned immediate directory, neither writable by group or
other users. Explicit incident deletion and generic file/directory cleanup keep
their stricter same-owner visibility policy.

The operation is **unlink only**. Existing opaque descriptors, `O_PATH` handles
and memory mappings keep the original inode's bytes alive. Their original
pathname can disappear, and reopening that name is not guaranteed. This is an
explicit change from preserving the pathname of every potentially hidden
external reader, not proof of an absence of readers. Cooperating Agent Mail
restore readers remain excluded by snapshot authority; observed external
readers still refuse reclamation. No process-name, capability or group allowlist
is introduced. POSIX reference lifetime, not a claim about daemon behavior,
supplies the protection for unobservable external readers.

`reader_inventory_incomplete` survives both preflight and the final scan and
is retained in `CompletedReclaimDelete` after a completed unlink with failed
directory sync. Such operations return `allocated_bytes: None`. Rotation reports
count their logical bytes separately as `unlinks_with_unobserved_readers` and
`bytes_unlinked_with_unobserved_readers`, excluding them from allocation-reclaimed
totals. No physical free-space increase is asserted. Strict empty-directory
cleanup can still report permission refusals after periodic payloads have been
successfully unlinked; those ordinary directory refusals do not undo payload
reclamation or mark publication admission unavailable.

### Reader and concurrency review

Persistent denial is tagged only after reading valid kernel identity for the
particular denied task through its retained directory. A denied worker's status
cannot be excused using its leader's readable status. Process and task pathname
reuse cannot redirect retry or identity evidence. Missing tasks remain ordinary
exit races; malformed/unreadable status, nonpermission IO errors and inventory
exhaustion remain hard refusals.

The scan continues through later descriptors, mappings, surviving workers and
other processes after observing uncertainty. A known reader takes precedence.
The current thread's own inventory is strictly inspected **before** verification
can open the candidate; this preserves its POSIX locks even when an unrelated
process is opaque. A review removed redundant rescans of every worker's shared
table/address space, which could otherwise exhaust the unchanged 131,072-entry
budget. The leader/surviving-worker checks remain. Linux ties POSIX lock ownership
to `current->files` in its
[file-lock implementation](https://kernel.googlesource.com/pub/scm/linux/kernel/git/torvalds/linux.git/+/c17fdf62aeecbbaf2c2fd5c494e2089c02b0e75b/fs/locks.c);
the real lock regression verifies the relevant close/open
boundary rather than assuming that process names or thread count confer safety.

An exclusive `F_SETLEASE` probe was investigated and rejected as an absence
oracle. Native experiments admitted separate `O_PATH` handles, and the local
overlay filesystem admitted a descriptorless mapping as well. The implementation
therefore adds no lease syscall, signal handler, unsafe code, or dependency.

### Executed evidence and limitations

The repository's pinned `nightly-2026-08-31` compiler and unchanged dependency
versions were used. The standalone namespace harness imports the exact
production module, with the repository's `nix 0.31.3`, `rustix 1.1.5`, `fs2 0.4.3`
and `tempfile 3.27.0`. Final checked source identities are:

| Source | SHA-256 |
| --- | --- |
| `recovery_retention_namespace.rs` | `3f6ee67988610a6644d9a0f27a5d0d95400337a76e023d9beeb4cee1d4aa7ae5` |
| `backup_rotation.rs` | `6a6e0943101e849a14456863c1d9ea5be68d0d638eec0c2e1b6186eb7f68c8c7` |
| `retention_linux.rs` | `eb804eb9ef5e13e0df64894eb8d197c77c3b468bbffc7c435500f1cf3482d2d4` |

- **Seven new native namespace regressions pass**, zero ignored. They cover
  actual denied fd-directory/fdinfo operations, malformed and unreadable task
  identity, replaced PID/task paths, later observable readers and mappings,
  namespace/ownership restrictions, strict own-reader preflight, and actual
  unlink with successful or interrupted directory durability.
- The real-process test covers nondumpable idle, FD, `O_PATH` and descriptorless
  mapping modes. Each hidden reader verifies its original bytes after unlink.
  A separate process proves a same-process POSIX lock survives refused preflight
  while an unrelated nondumpable process is alive.
- **Strict exact-module Clippy passes**, using workspace pedantic/nursery and
  existing DB-crate allowances. The harness-only `dead_code` allowance covers
  functions whose surrounding production callers are absent from the harness.
- **The production DB library check passes** with the real core/DB crates and
  dependencies, `--locked --no-default-features --lib`, one compiler job and
  lower-memory development-profile settings supplied only on the command line.
  The final-source rerun also passes.
- **The exact production rotation module and all its tests type-check** in a
  focused harness with the real DB/core crates. This includes the parked-count
  and completed-unlink accounting regressions. **Strict rotation-module Clippy
  passes**, using the workspace pedantic/nursery settings and the existing
  server-crate allowances. These are compile/lint results, not runtime results.
  The production `integrity_guard` logging changes received source review and
  formatting validation; the full server library was not compiled here.
- **All seven public-guard integration tests type-check**, with zero diagnostics,
  using the pinned `rustc --test --emit=metadata` and real Cargo-produced DB/core,
  test-helper, chrono, serde_json and tempfile metadata. **Strict direct Clippy
  passes** with pedantic/nursery, the workspace's three existing allowances and
  `-D warnings`. The exact test source is included through `#[path]`; no macro,
  helper or persistence implementation is substituted. This is also compilation
  evidence, not execution of `SnapshotRetentionGuard` tests.
- A full namespace comparison before the behavior-preserving removal of
  redundant worker scans produced **40 passes / 13 failures**, versus unchanged
  main's **32 passes / 14 failures**. There were no new failing test names; the
  existing own-POSIX-lock preflight case improved. Remaining failures are the
  strict generic/empty-directory paths encountering ambient inaccessible
  same-root processes, plus the existing unconditional UID-drop fixture.

This executor runs as UID 0 without capabilities. Real `PR_SET_DUMPABLE=0`
children produce ptrace permission denials for descriptor targets and maps;
the root-owned fd directory itself can remain enumerable. Separately, real
mode-000 directory/file fixtures reproduce traversal/fdinfo permission denial
in a kernel-shaped proc tree. They supplement, not replace, the live procfs
tests. UID changes and private procfs mounts were unavailable (`EPERM`), so the
strict ambient-process suite is not claimed green by filtering away its errors.

The public-guard integration file additionally tests eight eligible backups
with an unrelated opaque process, observable readers amid opaque processes,
all opaque name/handle modes, terminated leaders, actual unlink permission
refusal, concurrent cleaner admission, and SIGKILL/restart after completed
unlink while a hidden reader still retains the old bytes. The Python helper's
full-byte verification was exercised independently in 16 visible/opaque/worker
cases. Four deliberately corrupted nonheader-byte controls failed as intended,
including worker-thread error propagation.

The existing parked-backup keep-window correction was already present at the
starting commit. Two additional native regressions cover move-only staging
followed by re-enabled deletion and a raised/lowered keep count, and interrupted
historical transactions that consume the publication budget but do not displace
the retained resident window. An accounting regression distinguishes incomplete
reader visibility from directory-durability failure and reported allocated-byte
reclamation. These larger tests require the real DB/core dependency graph.

Two executable-build attempts for that exact-source native harness were killed
with `SIGKILL` while compiling unchanged `asupersync 0.5.0`, before test binaries
existed, under shared pressure in the 8 GiB cgroup. Unrelated workspace builds
were also using several GiB; this does not establish a standalone minimum
memory requirement for Agent Mail. One used LLVM; the second used the
pinned compiler's supported Cranelift backend for **only** that dependency.
No application source, feature default or dependency version was replaced to
work around either executable-build failure. The
harness lockfile's 363 packages were an exact subset of the repository lockfile
apart from the harness package itself, with all 20 patch entries identical.
These are build-resource failures, not passing integration/count tests.

The combined public-guard/rotation harness's metadata-only `cargo check --tests`
also received `SIGKILL` in `asupersync`, before reaching the test source. Its
test-helper dependency graph unified native-build features differently from the
passed DB check. Making that helper and the integration module optional in the
scratch harness allowed a rotation-only check to reuse the actual DB/core
dependency artifacts. No production manifest, lockfile, feature default,
persistence code or test logic was replaced to obtain the passing rotation
compile/lint results.

For the integration compile/lint check, the actual test-helper crate was then
checked separately, avoiding that feature-unification rebuild. Its real metadata
and the cached real production DB/core dependency artifacts supplied the direct
compiler and Clippy invocations. Two assertion-equivalent Clippy corrections
were applied to the dead-leader mapping tests, and the final source passed both
checks and rustfmt without diagnostics.

The exact public-guard and parked-count execution, full workspace/release
validation and a green strict ambient-process suite therefore remain unproven
in this environment. No issue closure or release qualification is inferred from
the passing focused component tests.

### Reproduction commands

The final production DB check passed with the pinned toolchain, from the
repository root:

```sh
cargo check --locked -p mcp-agent-mail-db --no-default-features --lib -j1 \
  --config 'build.target-dir="/tmp/am-retention-target"' \
  --config 'profile.dev.debug=0' \
  --config 'profile.dev.package."*".opt-level=0' \
  --config 'net.git-fetch-with-cli=true'
```

On a normally provisioned Linux host, the corresponding repository test
targets below exercise the new namespace cases, the public guard and the full
rotation module. These commands are provided for the remaining runtime
validation; they are not additional passing receipts from this executor.

```sh
cargo test --locked -p mcp-agent-mail-db --no-default-features --lib \
  superseded_ -- --test-threads=1
cargo test --locked -p mcp-agent-mail-db --no-default-features --test it \
  retention_linux:: -- --test-threads=1
cargo test --locked -p mcp-agent-mail-server --no-default-features \
  --features portable --lib backup_rotation::tests:: -- --test-threads=1
```

## 2026-11-03 fleet qualification follow-up

The following are separate source-bound runs, not a combined green suite.
They use actual workspace test binaries and real SQLite files, processes,
descriptor tables, mappings and snapshot authority. Compiler work uses normal
remote-only RCH admission, the pinned toolchain and unchanged lockfile. Worker
pressure, source-ownership and queue refusals are retained as pre-test failures;
no worker capacity, source-ownership policy or test deadline is overridden.

| Source and actual image | Executed result | Qualification limit |
| --- | --- | --- |
| Published `159e9bb5`, DB library `dd4dea95` and integration `cda0281f` | Initial 63-case run: 41 pass, 22 fail. A later complete same-image run as UID 65534 with a readable controller and child umask 077: 63 pass, zero fail. | The ordinary UID 1000 run with secure fixtures still has 13 strict census failures. The role-specific pass precedes later source edits. |
| Private-fixture/reporting source, DB library `c2c71a07` and integration `90210e68` | 49 pass, 14 fail of 63 in 80.629 seconds; six of seven public Linux cases pass. | Thirteen strict census refusals remain. The restart case fails on a corrupt fixture's inherited shared mode before its intended verification refusal. |
| Private-fixture/reporting source, server `89d9fae6` | Ordinary worker: 61 pass, six fail, one original 240-second timeout of 68. Complete same-image UID 65534 replay with umask 002: 67 pass, one timeout. | Both added real-opaque-reader and default-warning controls pass. The 16,641-directory workload still exceeds 240 seconds. The 720-cycle case passes in 77.696 seconds in the replay role; its original worker timeout remains red. |

The private-fixture changes follow discriminating same-image controls, not
weakened production admission. Three original cases each fail under umask 002
and pass under 077. Test roots and historical directories are made mode 0700,
and owned source images mode 0600. Unsafe-mode, owner, alias, replacement,
known-reader and interrupted-operation assertions remain. The replacement
fixture itself is private, so its inode-refusal control is independent of an
accidental permission refusal.

The final restart-fixture counterexample receives its own paired control:
unchanged image `90210e68` fails under umask 002 and passes under 077 in one
invocation, with identical hashes before and after both runs. Commit `c004204a`
sets only the deliberately incomplete snapshot to mode 0600. Its original
bytes, full-verification refusal, completed-unlink checks and real
SIGKILL/restart assertions stay unchanged. This paired control diagnoses the
fixture; it does not execute the subsequent source edit.

Maintainer follow-up `09b362e0` is merged with the local changes at `f0a835cc`,
preserving both histories. Reports use the maintainer's single count-and-byte
vocabulary. The local actual-reader test now checks aggregate and per-kind
logical bytes as well as zero allocation credit; the shared startup and
automatic-cleanup renderer warns at the default level for opaque-reader
unlinks. No new reader-admission exception is added by this reporting repair.

Strict workspace Clippy, all-targets and the separate production library/binary
check pass on `f0a835cc` in 1,915.9, 266.3 and 351.1 seconds. After the one-line
fixture edit, they pass again on `c004204a` in 34.9, 40.2 and 37.5 seconds;
formatting passes too. The earlier live witnesses match all 3,896 present
tracked inputs and preserve six omitted historical performance JSON files,
with zero changed present inputs. The fast final reruns have no separate live
observer claim. These are compiler results, not runtime or release clearance.
The broader merged-source native run and final public-guard follow-up remain
pending at this checkpoint.

UBS remains red on four unchanged fixture patterns: three intentional panic
guards and a fixed Python program passed as an argument without a shell or
untrusted interpolation. The merged four-file scan has 2,243 warnings and 783
informational findings; none of the 70 changed-line findings is critical.
The last one-line edit adds only a test-setup unwrap finding. No scanner
suppression, failure guard or deadline is removed.

Evidence is preserved on trj under
`/bulk/worker-relocated-20261102-am-reservation-read/`:

| Bundle | SHA-256 | Preserved scope |
| --- | --- | --- |
| `retention159-native-boundaries-1110/retention159-native-boundaries-20261103.tar` | `b320068b609df8e9dc49748b811e6ed9b212061a4f5e4bf5a2579b92576ec299` | 680,468,480 bytes, 479 verified members: earlier actual images, source witnesses, raw failures and role-specific replays. |
| `retention159-private-native-recovery-1200/executables-1706466-retention159-private-db-library.tar` | `37557a2058026bf3ea275496956d7891c6d3a006f36a8515230d6a5807086adf` | Actual retained `c2c71a07` image, independently recovered after the collector failure below. |
| `retention159-private-native-recovery-1200/executables-1706466-retention159-private-db-integration.tar` | `3b8d98699492c1eb49617dfb6312cd9facb267ed495981b8066af58761192a48` | Actual retained `90210e68` image and the complete FAIL002/PASS077 control outputs and receipt. |

Archive contents, full hashes, manifests, receipts and containing directories
are verified and fsynced. Copy-only proof preservation removes no source data
and receives no disk-reclaim credit. Two original collectors copied live
executable FDs but lost their final local receipts because stdout redirection
prematurely created their exclusive receipt paths. Both tracebacks and the
independent whole-image recovery hashes are preserved. Those recoveries are
not intact original live-FD receipts. Later collectors use separate stdout
and helper-owned receipt paths.

None of these receipts closes br-5nr0m's original five acceptance items. The
load-20, 16,641-directory bound, namespace-hidden-reader negative, doctor/metric
surface, inventory-bound behavior and macOS work remain open. Role-specific
timings are not incumbent comparisons or performance wins. Windows and full
workspace/release qualification are not established by this selected evidence.

### Newly confirmed live-worker counterexample

A further actual-kernel check finds an observable reader missed while its
leader remains alive: a worker uses `unshare(CLONE_FILES)` and opens the
generation in its private descriptor table. The leader has no matching FD
and still has VmSize, so the old survivor scan returns before visiting the
worker. This is a counterexample in the pre-follow-up `90210e68` image too;
it is not attributed to the later maintainer commit.

One paired invocation executes the unchanged actual public test
`visible_reader_still_refuses_deletion_amid_inaccessible_processes` and binary.
The ordinary leader case passes in 27.66 seconds. The identical Python reader
program `b3592c84` running in a live unshared worker fails in 1.117 seconds:
the original `unwrap_err` receives successful `ReclaimedFileBytes`, proving
admitted unlink. The experimental wrapper changes only fixture execution and
delays its ready-file publication until an independent kernel witness proves
the worker-only inode reference and live leader; it does not substitute the
production scanner or its original oracle. Image hashes remain identical.

Checkpoint `11d262da` adds pinned FD-only inspection of live workers, retaining
full inspection for dead-leader survivors and strict own-process preflight.
It does not assume shared descriptor tables. Live thread groups share their
address space, so the leader's mapping inventory is not repeated per worker;
Linux enforces those thread-group/VM constraints in
[`copy_process` and `check_unshare_flags`](https://raw.githubusercontent.com/torvalds/linux/v6.18/kernel/fork.c).
The [kernel unshare documentation](https://docs.kernel.org/userspace-api/unshare.html)
describes descriptor-table separation. The scan budget and original timeouts
are unchanged; descriptor work remains bounded and no performance win is claimed.

The existing public negative gains actual live FD and O_PATH worker cases,
with explicit inode, leader-liveness and descriptor-table witnesses. All its
original cases and refusal/byte-preservation/accounting assertions remain.
Formatting, all-targets checking (237 seconds) and strict workspace Clippy
(385.5 seconds) pass on this repair. The separate production library/binary
check also passes through normal hz2 execution in 1,712.5 seconds; its live
source witness matches all 3,896 present inputs, retaining the six historical
JSON omissions and zero changed present inputs. Earlier namespace refusals
and queue timeouts do not execute Cargo; exact owned-job recovery acknowledges
remote exit 1 without replay. A separate-checkout hz4 native build is compiling,
with executable collectors already running before test execution.

The original f0 native run finishes with 112 passes, 21 failures and one
240-second timeout across 134 cases. A later canonical run with the repaired
source transferred reports a fresh build in 0.45 seconds and again 112/21/1.
Its panic points at namespace line 2028, the f0 preflight line, while the
corresponding 11d262da line is 2094. This repeats the known br-kp1in.75 stale
cache defect. The later run receives no repaired-source runtime credit, even
though its live namespace matches all 3,896 present inputs; six historical
performance JSON omissions remain explicit. No assertion or deadline is
weakened to make these runs green.

The counterexample wrapper, kernel witnesses, original failing output and
paired receipt are independently verified and fsynced on trj in
`retention-live-worker-counterexample-1311/` under the preservation root above.
Its nine-member, 20,480-byte archive has SHA-256
`2f18abf4c51b3d1423ccd5812bc22ed5037ee437b3023204fe5b0126ce8b63f9`.
The actual counterexample image is retained separately in the earlier
private-native recovery bundle. This copy-only preservation receives no
reclaim credit. Neither older compiler passes nor the stale native run
qualifies the new production change.

The original f0 executable images and terminal/source receipts are also
independently verified and fsynced in `retention09-canonical-native-1350/`:

| Archive | SHA-256 | Scope |
| --- | --- | --- |
| `executables-3432534-retention09-canonical-db-library.tar` | `c18b5048bb3c1b87e0415e6d2451bd7594a36456c706c09f26381a3ae5af88f6` | 146,831,360 bytes; actual DB image `3c9a5087`. |
| `executables-3432534-retention09-canonical-server.tar` | `4711222a35a905222ddb70f968ff2b49921fa4d8d2f846c65afb48850d12c7f3` | 477,347,840 bytes; actual integration `3449a7ba` and server `9d309750` images. |
| `retention09-terminal-source-receipts-1355.tar` | `6cb8b21af7de4c8127172d80955a351a57c853985c13060b082cd5daf809f8c7` | Eight verified members: original terminal logs, source witnesses and all collector receipts, including the generic collector's raw copy errors. |

The clean server collector establishes the integration image identity without
hiding those independent generic-collector errors. All originals remain on
the source worker; this preservation has zero root-disk reclaim credit.

### Repaired live-worker case executes on the new image

The separate-checkout 11d262da hz4 build finishes compilation in 111 minutes
and starts all 134 selected cases. Its original five-mode public negative,
including real live FD and O_PATH workers, passes in 21.864 seconds. The
restart/corrupt-evidence fixture, dead-leader, ordinary FD and mapping
protections pass too. The captured integration image is 125,984,704 bytes,
SHA-256 `74eee95c3f030c24da9cc7e7747b3d49e3780945c074d9b8b2ca043f540c6a09`;
independent inspection confirms both new mode strings and reader_descriptor.
The live execution namespace again matches all 3,896 present tracked inputs,
with six historical JSON omissions and no changed present inputs.

The DB unit image is `ab40be3025d34502a49f7dea85e41f9065f0f0447214c3c17b264c79efb42735`
(146,831,704 bytes), and the server image is
`78b45d5a78766dda66858a9176faf9d629d07f6614a5993f5b6c5484c13fb0ea`
(351,390,544 bytes). Successful image records retain stable whole-file hashes
and fsynced copies. The server collector also records one raced process-exit
capture error, preserved verbatim; it is not reported as error-free.

The frozen run finishes with 113 passes, 20 failures and one timeout across
134 cases in 1,043.433 seconds; 10,461 cases are outside selection. The
720-cycle case reaches its configured 240-second deadline, with 260.186
seconds recorded including process termination. The large directory case
fails on protected-process inventory refusal in 157.030 seconds. A separate
lossless archival transfer runs concurrently from 15:25 UTC; these ambient
timings are not performance qualification or an incumbent comparison.

The later shared-checkout merge 9d38fd84 changes rotation/guard code and adds five
tests; none of this frozen-source server evidence qualifies those new edits.
Formatting and a two-file scanner review pass for that merged source, while
its compiler/native checks remain pending. br-5nr0m stays in progress.

The same actual 11d database images also execute all 63 original database cases
in one consistent coordinator role: UID/GID 65534, CapEff zero, dumpable and
independently readable controller, umask 002 and full host procfs. All 63 pass;
each invocation must report exactly one populated passing test, and image
hashes match before and after the run. This is additional real-path coverage
for the strict namespace/mutation cases, not a replacement for the ordinary
worker's failed run or qualification of the new server code.

The first role harness records 28 passes and 35 failures because its outer
parent is UID1000/mode0711: the unprivileged directory walk cannot open that
parent RDONLY. Both actual failed outputs and the original directory remain.
A direct kernel open probe returns EACCES there and succeeds on the new
UID65534/mode0700 parent. The second run retains identical controller bytes,
case names, executable hashes and assertions; only the harness root/ownership
precondition changes. No directory enforcement, procfs inventory, deadline,
test population or production code is weakened. Do not sum these two role
results or combine them with the ordinary 134-case run into a green total.

## 2026-11-03 continuation: ordering, durability and startup admission

Starting from `09b362e0c4f7b041fc12f76ce77c849f01851660`, a second review
identified three gaps in the coordinator around the reader policy. These changes
leave the namespace module, snapshot authority, public-guard integration tests,
dependency versions and feature defaults unchanged. They do not relax the
reader, recovery-election, ownership, verified-survivor or durability checks.

### Capacity-driven keep-window ordering

Commit `a73ae5e377cbab481643dfb8b29e8b146f71b94c` shares the existing
keep-selection comparator with the oldest-first pending reclamation queue.
Previously that queue sorted equal timestamps by the complete pathname, which
could remove collision `-100` before `-99`, or let a parked parent directory
override the generation order. This matters when publication capacity requires
shortening the normal keep window. The progress cursor remains a fairness hint
for expired candidates; it does not override oldest-first window shortening.

`periodic_retention_shortens_the_window_by_numeric_generation_order` covers
resident and mixed resident/parked generations, numeric suffixes and misleading
mtimes. It sets up a real verified mailbox at its six-image publication limit,
three refused older corrupt images, and two eligible window images. Exactly one
necessary unlink must remove the older eligible image and admit publication,
leaving the newer generation and refused evidence intact. An unchanged
`09b362e` production module with the identical new test was also prepared and
type-checked as a baseline. Type checking alone does not establish the expected
baseline failure or a passing candidate runtime result.

### Stop after an unconfirmed namespace mutation

Commit `71fda742e96f8cc9162f0a68cde993fe10f9dca2` makes completed rename/unlink
failure receipts a sticky publication-admission refusal. The coordinator now
preserves all later candidates and skips the final empty-directory sweep after
such a failure. A subsequent inventory cannot treat a missing pathname as new
capacity or authorize a repair export during that pass. An ordinary
candidate-local refusal before mutation still allows bounded progress on other
candidates; loss of shared snapshot or namespace authority remains a refusal.

The review traced the actual `CompletedReclaimMove` and `CompletedReclaimDelete`
IO errors through snapshot authority and every mutation catch in the rotation
coordinator. The existing namespace tests exercise actual rename/unlink followed
by injected sync failure. New coordinator tests separately cover receipt
aggregation and stopping a real periodic queue; they explicitly do not pretend
that seeding a receipt reproduces a filesystem sync fault. The existing real
post-stage authority-replacement regression now requires immediate refusal,
one completed-move receipt and no subsequent attempt. A new continuation control
injects a pre-mutation refusal and then permits a real later rename. Byte
accounting still comes from each kind's summary exactly once.

### Defer snapshot admission after the startup budget expires

After the historical and resident inventories complete, rotation now checks its
deadline before publication-budget planning and admission of new backup-file
reclamation or survivor verification work. The existing early empty-directory
sweep checks its own deadline before acquiring a `SnapshotRetentionGuard`.
A spent budget uses the existing preserve-and-account path, sets
`deadline_reached` and retains any genuine inventory/admission refusal already
recorded. It does not turn ordinary deferral into an admission refusal.

`spent_budget_does_not_acquire_snapshot_authority` uses a verified mailbox,
13 resident/parked images, protected recovery payload and manifest, and a separate
incident artifact. It holds the real existing authority-file flock without
first acquiring the recovery election. The zero-budget pass must account for
all 16 files and exact bytes, preserve every pathname and file content, and
create no control files. The same inventory with ample time must actually
encounter the held authority and refuse admission. The deadline remains a soft
bound: inventory completes, and an operation already underway is not preempted.
No SQLite cancellation hook or unsafe code was introduced.

### Exact-source validation for this continuation

| Rotation source | SHA-256 | Compile/lint result |
| --- | --- | --- |
| Ordering commit | `5bc934685e4cffe07fc51fe51454a6dbba7d345b65b307f08ce5dcef433503d5` | All-test metadata check and strict Clippy pass |
| Durability commit | `c44b94adbfd1fd6b563dc991cf905d8bba1fdbf7cd46e9b02e9ddf94163f18f6` | Final strict Clippy, including all-test type checking, passes |
| Combined ordering, durability and deadline | `6eb1f6c7602f6b38b45db02f40e36dea2dda3a1975a28d30f452ec37b96d4d7c` | All-test metadata check and strict Clippy pass |

These checks used frozen copies of the exact rotation module, the pinned
`nightly-2026-08-31` toolchain, and the real production DB/core dependencies.
Separately named scratch harness packages prevented baseline/candidate cache
identity collisions. Their lockfiles remained subsets of the repository's
lockfile and their 20 patch entries matched the repository. No fake persistence,
replacement helper, test macro, reduced inventory bound or skipped regression
was used to obtain these compile/lint results. The existing 720-generation and
16,641-directory tests remain unchanged.

The unique ordering baseline check passed in 1.12 seconds; ordering candidate
check and strict Clippy passed in 1.01 and 1.35 seconds. Durability's final strict
Clippy passed in 1.70 seconds after an assertion-equivalent `from_ref` correction;
the earlier standalone metadata check preceded that correction. The combined
final source passed its metadata check in 1.38 seconds and strict Clippy in
1.50 seconds. Formatting and `git diff --check` also passed. The only
`integrity_guard.rs` change in this continuation is the refusal diagnostic's
addition of durability failure; it received formatting and source review, not
a full server-library build.

Two additional native executable-build attempts in this continuation both
received `SIGKILL` while compiling unchanged `asupersync 0.5.0`, before any
application or test binary existed. Both used one Cargo job and the pinned
compiler's Cranelift backend for that dependency only. The first ran for
249.70 seconds and sampled a peak child RSS of 7,476,640 KiB. The final attempt
also enabled the compiler's supported `hint-mostly-unused` option for
Asupersync only, through command-line profile configuration. Its actual compiler
arguments confirmed that option was active; it ran for 270.29 seconds and
sampled a peak child RSS of 6,871,824 KiB. It still exited with Cargo status 101
after the dependency was killed. Shared cgroup OOM-kill counters increased
during both builds, and unrelated workspace compilers were also active. These
observations do not establish a standalone memory requirement for Agent Mail
or demonstrate that the hint would make this build viable on another host.

No production source, feature default, dependency version or tracked profile
was changed for those build attempts. There were no additional codegen retries
after the second failure. The new ordering, durability and deadline runtime
regressions, the seven public-guard integration tests, and full server/workspace
execution therefore remain unproven here. In particular, the prepared
old-production/new-test ordering counterexample has not executed. The passing
native namespace evidence above still applies to its unchanged source; the
passing compile/lint results in this continuation do not replace the outstanding
runtime validation. No issue closure or release qualification is inferred.

## November 3: durable 11d proof and normal merged-source admission

The completed 11d run remains **113 pass, 20 fail and one timeout**. Its three
actual images are independently rehashed inside their preserved archives on
`trj:/bulk/worker-relocated-20261102-am-reservation-read/retention-live-worker-11d-native-1705/`:

| Preserved payload | Archive SHA-256 |
| --- | --- |
| DB library image | `5480e87ea91bae7f3b9841c20b86fe6732ca4e563b41ff248555e00d3ac4786d` |
| Integration and server images | `85733b8dedd7f924899dcb57b8138a01286286d528ba1b3e1cddce1add07e65e` |
| Terminal/source records and both role contexts, 1,002 members | `f889551737ef03e2cd29755fc3fff3e1a091ec0667ff2cea2cd1250a720ffece` |

The terminal bundle retains the ordinary-worker failures, raced live collector
error, initial 28/35 parent-permission harness result, corrected 63/0 role
result, original case listings, controller, configuration and per-case output.
All member contents and archived ownership/modes are verified; payloads,
manifests and directories are synced. Proof sources remain intact and protected;
these copies claim zero reclaimed bytes. The corrected role result remains
separate from the ordinary-worker run and does not qualify the merged server.

The user authorized operator remediation with: "you do it!!! use
$system-performance-remediation !!! and move stuff to /bulk on trj". Following
full preservation and fresh source checks, the provided relocation helper cut
over hz4's idle, non-Git `/data/projects/fastmcp_rust_batchverify` at
**2026-11-03 17:10:24 UTC**. All 78,193 entries are preserved in the independently
verified and synced 14,378,229,760-byte archive under
`trj:/bulk/worker-relocated-20261102-am-reservation-read/hz4-fastmcp-batch-1524/`.
Its SHA-256 is `3aafa0e4bebb594b12908858c7570330de3945eedf2f06dd3bda3c0e1af6e8d2`;
source/archive/fresh-source manifest SHA-256 is
`ab1b5240860292603572c3470b0ebe2401cbc382e89cf38fb40719f7f82a2d7f`.
The measured root gain is **13,492 MiB**, from 19,557 to 33,049 MiB available
at cutover. Owner1000:1000/mode0775 and a synced restore pointer remain.
The 17-member operator archive preserves the exact authorization, executed
command, UTC time, full guards, source manifests and restore-stub verification;
its SHA-256 is `88364a233196e35f45fbac10dd7ea6f19ad29dbd84d159d64c7ea6230fe98b98`.
Active/protected trees were excluded. Current free space can fall as other jobs
write; this is neither a persistent-headroom guarantee nor a NAS repair claim.

Normal RCH admission then started the frozen 9d38fd84 139-case native rerun on
hz4 and the required compiler gates on vmi1152480. Live source witnesses match
3,896 present inputs, with the same six historical performance JSON omissions
and no changed present input. All-targets passes in 3,937.2 seconds; Cargo also
reports an existing third-party future-compatibility notice. The following
production request refuses before Cargo starts. That worker reaches zero root
headroom during unrelated batch builds, whose sessions subsequently exit; none
is signalled. The remaining production request is admitted
normally on vmi1149989, with the same live input witness. It finishes with
Cargo exit0 in 103m07s, remote exit0 in 6,200.579 seconds and terminal RCH
client exit0. The old Clippy follow-up does not run. No source identity,
worker capacity, deadline or product policy
override is used.

The merged native run compiles in 92 minutes 42 seconds, then executes every
selected case: **117 pass, 22 fail, no timeout**, in 535.602 seconds, with
10,461 cases outside selection. All five added ordering/durability/deadline
regressions pass. The five-mode public visible-reader negative passes in
8.043 seconds. Every failure remains retained. The original 720-cycle case
fails after 110.882 seconds because a worker's proc status has no valid thread
count; transient teardown is a hypothesis, not an independently reproduced
kernel explanation. The opaque-unlink accounting case and ample-time empty
directory cleanup also fail and need diagnosis. No faster-runtime or
whole-suite conclusion follows from this changed failure split.

Both collectors retain the actual complete images with no copy errors. DB
library ab40be30 and integration 74eee95c match the unchanged 11d source;
their bit identity is not a cache-freshness repair claim. The changed server's
SHA-256 is `6e0b047411b67b75b38c49f3c5968e9a1029f7a50d5cb3d68e9d976aaaeee5c9`,
351,912,336 bytes. All three images are independently checked inside their
verified, synced archives under `trj:/bulk/worker-relocated-20261102-am-reservation-read/retention-merged-9d38-native-1854/`.
The library archive SHA-256 is
`84bc98091c0e2d26d1d6caa9a966d81693d91dc6bb094bf52e0db36b980cf54f`;
integration/server archive SHA-256 is
`53ccc5aea4ae68d3f53f9975052fe1eeaae188c3beebd4a258707ff9dbf6a3e7`.
Sources remain protected and intact; proof copies claim zero reclaimed bytes.

A separate replay of all 76 server cases finishes in the established
coordinator UID/GID65534, CapEff-zero, readable-controller, umask002 role,
with full host procfs, unchanged executable/assertions and original 240-second
deadlines: **75 pass, one timeout**. The 16,641-directory backlog times out at
240.005 seconds; the 720-cycle case passes in 150.274 seconds and ample-time
empty-directory cleanup passes in 2.027 seconds. This shows success in that
role and host; it does not isolate which environment difference caused the
ordinary failure or rule out an intermittent defect. It does not qualify the
load-20 performance acceptance. The first staging attempt preceded completion of the image transfer;
its 345,984,000-byte partial copy is preserved. Staging was corrected with the
complete hash-verified image before execution. Automatic review rejected a
combined driver with an opaque constructed subprocess argument list; no driver
code ran. Explicit, statically reviewable move/install/hash/runner commands
completed staging without a review bypass. No role result replaces the
ordinary-worker 117/22 result, and no independent or release clearance is claimed.

The complete ordinary-run terminal record, source witnesses, clean live capture
receipts, all 76 role-case outputs and the kernel probe below are independently
verified and synced in a 178-member, 2,846,720-byte archive alongside the actual
images on trj. Its SHA-256 is
`d067585d462906d155d5c1c2024b5a8ef132b8acb69b0c4328670e164d776ed7`.
The partial diagnostic install is an independently hashed exact prefix of the
preserved full server image; it remains locally protected and never executed.
No unrelated live mailbox configuration or database is included.

### Valid zero-thread status: a separate parser repair

The affected worker's real `7.0.0-31-generic` procfs emitted a pinned worker
status with `Threads: 0`, `State: R`, `FDSize: 0`, no `VmSize`, and its leader
still present. The first bounded attempt observes this after 25 status reads;
the entire raw record, probe and six ESRCH observations are retained. This is
not the exact status of the PID from the earlier failing test. The separate
probe establishes a valid kernel input that the current positive-only count
parser rejects. Upstream [Linux 6.8 procfs source](https://raw.githubusercontent.com/torvalds/linux/v6.8/fs/proc/array.c)
also explicitly initializes this output to zero when the signal-handler lock
cannot be obtained; that older implementation is a mechanism reference, not
proof of the exact timing in the earlier test.

The new draft removes only the positive-value filter after unsigned parsing.
Zero does not satisfy the existing terminal-state or foreign-UID predicates,
and it does not bypass group enumeration. Two added unit cases retain refusal
of missing/malformed/negative/overflow counts and plant a surviving reader
behind both live and dead zero-count leaders, alongside an empty-group control.
These parser/proc-shaped fixtures are R3; the real kernel observation and
existing real-public-reader path are separate evidence. The change admits
syntactically valid unsigned zero counts previously refused by the parser;
all other reader exclusions remain. No test is disabled, deadline widened or
count bound changed.

The namespace draft SHA-256 is
`27e569dd96b5da2bd080e3662ff0182773d98b67a796a2f87a1f5854216cc58d`.
It is reproduced manually in a separate validation worktree at clean base
05d2bc1b plus an explicit source overlay, SHA-256
`ea33ba6a97b802aaf58db009b3f3c6009e089c5d37f9041e980c4fc139b417be`.
The remote 9d checkout remained frozen through its completed production gate.
The original local producer was deliberately advanced at 20:24 UTC to 05d2bc1b plus
the identical manual parser overlay. All 3,902 producer inputs match the
separate compiler checkout. Before/after live input witnesses confirm the old
remote production compiler still sees the 9d inputs, including after that
local advance. Its local sequential Python controller alone is paused to
prevent an obsolete Clippy follow-up; the RCH client and remote compiler are
uninterrupted. At 20:36:01 UTC, after verifying the child's original start
identity, zombie state and exit0, send SIGTERM then SIGCONT only to the stopped
Python controller. Both processes are confirmed gone; the outer driver exits
143 from this deliberate cleanup. An independent receipt binds the old
production log hash, Cargo completion, remote exit0 and RCH child exit0.
The unfinished original driver state cannot substitute for that receipt.
The shared checkout records the parser change in b2a29afa. Formatting passes.
The one-file UBS scan remains red: one unchanged critical fixed Python lock
fixture, 581 warnings and 458 informational findings. All 47 added-line findings
are reviewed; none is critical or in production code. Nothing is suppressed.
The new all-targets check passes under normal vmi1152480 admission:
Cargo99m24s, remote exit0 in 5,968.364 seconds, outer RCH exit0 in 5,993.7
seconds. Its live witness matches 3,896 present inputs plus the same six
historical JSON omissions. The production check passes with Cargo20m59s,
remote exit0 in 1,268.014 seconds and outer RCH exit0 in 1,303.0 seconds. Its
live source witness again matches all 3,896 present inputs with the same six
omissions. Strict workspace Clippy with -D warnings also passes:
Cargo15m33s, remote exit0 in 938.201 seconds and outer RCH exit0 in 970.8
seconds. Its live witness has the same matches and omissions. The complete
three-gate controller exits0 at 21:38:46 UTC. A fresh formatting check also
passes. The initial 141-case native
request refuses before Cargo because hz4
has no admitted slots. At 20:24:52 UTC a retry from the original producer path
is admitted normally on hz4, using its protected 357d41b1 target. Both live
native processes match all 3,896 present inputs and the same six historical
JSON omissions. Whole-executable collectors are active before tests begin;
the execution-time source witness repeats those matches and omissions.

The native run is terminal red: **120 pass, 20 fail, one timeout**, all 141
selected cases executed, 10,461 outside selection, 479.674 seconds after a
122m58s build. RCH returns100. Both new zero-thread controls pass, as does the
real five-mode public reader guard in 6.385 seconds. Opaque-unlink accounting
passes in 5.450 seconds; the exact cause of its earlier failure remains
unproved. Ordinary strict reader-census refusals remain. The 720-cycle case
times out at its unchanged 240-second limit (240.100 seconds observed), and
the large-directory case fails in 36.274 seconds. These outcomes do not
establish a speed improvement or complete the original retention acceptance.

The actual new images are DB unit
`76c51a9c2b13db54be4fc2f7e923dba17f11cc96edf492f3b988bb38e5e842e8`
(146,843,168 bytes), integration
`48e35f92938960b63f72aba1b3a77ff51ed578a3e7bedb9f75404beb7b57732f`
(125,985,208 bytes), and server
`84588f861c3fdfa3cc6fb19c434f72be96940f55eb0aa958f9ab292ecfcf921c`
(351,914,432 bytes). Complete held-descriptor hashes match their retained
copies. One raced `/proc/1035138/exe` capture error remains in the original
collector receipt; the successful complete captures are not reconstructed
from that failed attempt. All three files are hash-verified and synced on trj
beside the compiler archive below. No cache-policy fix is claimed.

A second execution of the exact same 141 names, using those same three images,
finishes in one consistent coordinator role: UID/GID65534, CapEff0,
dumpable/readable controller, umask002 and full host procfs. **140 pass, one
timeout, zero other failures**; the original 16,641-directory case reaches
240.005 seconds. The 720-cycle case passes in 152.652 seconds. Image hashes
remain stable before/after and the case population matches the ordinary run
exactly. Both host and role differ, so this does not isolate a causal
environment difference. It neither replaces ordinary120/20/1 nor establishes
load-20 performance, platform, release or whole-project clearance. The
original five-item Bead remains in progress.

The entire stopped role tree, including its three images, controller, case
outputs and timed-out fixture, is verified and synced on trj beside the
compiler evidence: 15,356 entries, 202,067,520 compressed bytes, SHA-256
`726dc631dd15baae76211f14d91d4030c1ff7fdb90e18dacda0323f420217dc3`.
Its source and archive manifests match
`61261a45f0c06f9af196b0ba303a1d994ece83e54887b5c5c6a7309ba8054731`.
All content, modes, owners and symlinks are checked; files and directory
chain are synced. Sources remain intact and reclaimed-byte credit is zero.

The separate terminal/source/collector bundle is also verified and synced:
25 members, 1,474,560 bytes, SHA-256
`423f10864a83ef5f3e36f785095415b14c4ed0a1249dce33f56eb24062400958`.
It retains the ordinary failures, execution-time source witness, original
collector race, real kernel probe and bounded process audit. The compiler
and complete role archives remain separate; no passing claim is reconstructed
from an unsuccessful capture.

A separate native arm64 Mac probe exercises the exact production
`lsof -n -P -Fpf -- <candidate>` invocation on Darwin25.2.0/macOS26.2 with
lsof4.91. Three samples each cover clear-before, one real FD, two real FDs,
clear-after-close, a real mmap with its FD closed (`trackfd=False`), and
clear-after-unmap. All 18 raw results independently match the expected
process/descriptor fields: clean empty inventories return1, reader inventories
return0, and the mapping is reported as `ftxt`. Stderr is empty throughout and
candidate bytes remain unchanged. Per-mode medians range 0.191–0.313 seconds.
The raw receipt and separate field-by-field review are retained; this is
native dependency-boundary evidence only. No Rust retention execution,
scan-cost improvement, paired incumbent performance or macOS acceptance
closure follows. The configured Windows endpoint still times out on SSH.

The completed compiler logs, all source witnesses, explicit overlay, both UBS
and formatting records, old production terminal/cleanup receipts, and Mac
probe/review are preserved in a 26-member, 5,406,720-byte archive under
`trj:/bulk/worker-relocated-20261102-am-reservation-read/retention-zero-thread-native-2144/`.
SHA-256 is `b39aff5111a61558576c17b3d90c01fb08867a217c317472a18b250143e81c47`.
Every member hash and metadata field, plus the complete archive hash, is
verified on trj; the archive, control records and directory chain are synced.
This is a copy-only preservation of completed bounded evidence. No native
execution, release clearance, source removal or reclaimed bytes are inferred.

### Separate retained-CLI reliability diagnostic

The existing, unmodified release-smoke harness04210cb8 executes the retained
production-feature CLIbf35a95b (290,327,568 bytes), bound to the older
9513cc33 plus doctor811 source manifest. This is not the current parser
candidate or a release-mode qualification. Original thresholds remain:
300-second soak/convergence, 60 agents for 480 seconds and unchanged latency
and progress gates. The run lasts 21:58:13–22:17:54 UTC and exits1 with clean
shutdown. README flow, cross-project checks, 240-send storm, crash recovery
and all nine mixed-soak checks pass. The swarm fails on four inbox
RESOURCE_BUSY responses among 10,265 calls and failure to complete live Git
archive convergence within 300 seconds. Error detail is `database is busy
(recovery in progress)`. All 10,319 acknowledged swarm sends survive SIGKILL;
full stock SQLite integrity passes after both crash phases.

At 296.174 seconds the last completed live observation has 22,989 DB and
archive messages, no missing/extra/duplicate/mismatched IDs, but 12 reported
uncommitted copies and an advancing Git head. The terminal observation hits
its deadline at 300.001 seconds; its zero counters are unobserved values,
not proof of an empty database. After shutdown, the unchanged observer passes
in 15.168 seconds: all 22,989 messages and 79,287 required copies are
committed and consistent. That later result never replaces the live failure.
The harness and gate remain unchanged; the original frozen-drain phenotype
is not reproduced in the observed progress windows, and no root cause or
closure is inferred. The inherited Git observer discards child stderr;
those missing diagnostics cannot be recovered from this run. That limitation
does not turn the failed live gate into a pass.

The complete fixture, exact CLI, harness, source manifest and full retained
logs are verified and synced under
`trj:/bulk/worker-relocated-20261102-am-reservation-read/retained-bf35-reliability-2240/`.
All 353,969 entries match content, modes, owners and symlinks. The
2,586,600,289-byte gzip has SHA-256
`3ffc3aa56188d66b481f4b8393de45c7a98aa3f76d2e91f09b098961d9303adf`;
source/archive manifest SHA-256 is
`6ee96138d5a0003bbf2f0220c291b87f06d0ade79123f1d9efef1af0990943be`.
Files and the directory chain are synced. Sources remain intact, with zero
reclaimed bytes. Fresh solo verification is not independent review. The
bounded process audit remains DRIFTING because repeated bookkeeping and
speculative analysis exceeded their value; the next work should repair an
observed runtime blocker while preserving these original failed gates.

### Live mailbox incident discovered during handoff

At 23:24–23:25 UTC, both attempts to release the eleven owned reservations
queue real deferred intents: the live corruption breaker refuses writes
after reporting page149 never used. Independent stock SQLite full integrity
finds seven orphaned pages149–155, not a healthy database or an observed
lost-row/index corruption. The supported vacuum plan classifies the database
as leaked-pages-only; its count of one is one multiline result, not seven
independently counted pages.

The original installed TUI is gracefully stopped with its documented two
Ctrl-C confirmations through only native mux pane42. Its executable hash,
PID/foreground identity and original shell/cwd are checked; no force signal,
takeover or mux restart occurs. Drain confirms safe-to-mutate. The complete
quiesced database family is copied, whole-file hashes verified and synced.
The full-family rehearsal correctly refuses the copied namespace generation
without changing rows. A separate primary/WAL/SHM copy establishes its own
generation and passes the same retained CLIbf35a95b supported vacuum.

The live operation then passes at 23:30:29 UTC, with its verified pre-vacuum
backup retained. All 23 original non-statistics tables preserve identical
schema and canonical typed rows, full stock integrity returns `ok` and
foreign-key checks return no violations. Restart the same installed binary
171713fa in the original pane/shell/environment: PID189418. Live MCP health
reports green with a successful full integrity check; the first deferred
release intent replays eleven IDs and its duplicate is a zero-row no-op.
Read-only SQL independently confirms all eleven released timestamps and
full integrity remains `ok`. CLI doctor health still exits1 for the existing,
unrelated reservation346 thread-provenance drift. This recovery neither
proves an engine root cause nor changes the open br-htobc acceptance.

All bounded incident evidence is verified and synced in the same trj evidence
directory: `live-breaker-2328.tar`, 64 entries, 4,229,120 bytes, SHA-256
`723efcc8ce5e32581c01a36229ce868e184cdd3b280ea69c06047b6c73be75fe`.
Content, modes, owners and symlinks match source/archive manifest
`f33a261f89d4dcc4569485ad470731deeb1d2171ceeed9e5ef3d78c8e3dd13f4`.
The archive, controls and directory chain are synced. All sources remain
preserved; this proof copy credits zero reclaimed bytes.
