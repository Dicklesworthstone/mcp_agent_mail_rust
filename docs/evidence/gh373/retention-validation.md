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
hz4 and the sequential required compiler gates on vmi1152480. Both live source
witnesses match 3,896 present inputs, with the same six historical performance
JSON omissions and no changed present input. Both builds are active at this
checkpoint. No source identity, worker capacity, deadline or production policy
override was used, and no merged-source runtime or compiler pass is inferred.
