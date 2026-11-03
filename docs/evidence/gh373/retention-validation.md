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

## 2026-11-03 producer follow-through: preserve the durable repair allowance

Review of the caller above rotation found a second path around the completed
mutation barrier. `rotate_after_backup_attempt_with` preserves the producer's
result and logs the cleanup report. After it returned, `run_with_backup_lease`
independently counted the remaining paths and could clear the durable
one-repair reservation, even if cleanup had reported an unconfirmed unlink.

The producer now clears that reservation only at the beginning of a later
attempt, after ordinary retention admission succeeds under the parent exporter
lease. Post-attempt cleanup and completion never clear it. This preserves the
published/skipped/failed outcome and the existing retry schedule without
allowing an uncertain mutation to replenish the allowance in the same attempt.

The new coordinator regression uses actual mailbox images, quota inventory,
parent admission, journal slots, unlink and journal reopen. It injects the
producer outcome and completed-unlink receipt at their explicit coordination
boundary; it is not a physical fsync-fault test. All three outcomes must retain
the spent reservation after reopen. Renewed quota pressure must then refuse
another exporter, while a later attempt with ordinary capacity must reach its
producer and clear the reservation. Existing namespace tests supply separate
real-operation sync-fault coverage.

For exact `integrity_guard.rs` SHA-256
`41554272c120b9355f8306cda939e23b44ed92eef95bce2f4db20a860c31ce6f`:

- The complete production integrity-guard and rotation modules pass metadata
  checking and strict Clippy using real DB/core dependencies frozen at
  `09b362e` (unchanged for this producer fix). The three surrounding server-root
  helpers are verbatim production definitions. This is a module-level gate,
  not a full server-crate build.
- A focused compilation of the verbatim coordinator functions, actual fixture
  helper and exact new regression passes metadata checking and strict Clippy.
  It includes the complete real rotation, journal, schedule and budget modules.
  Unrelated pool-runtime tests are outside this focused result; no persistence
  implementation, helper behavior or assertion is substituted.
- The unchanged production journal and scheduler execute **53 native tests**:
  32 journal and 21 schedule tests, all passing, zero ignored. Their real
  cross-process locks, journal writes, interruption and restart cases provide
  compensating component evidence. Their strict Clippy check also passes.
  This is not execution of the changed producer regression.
- Formatting, diff checks and independent source review pass. No production
  dependency, manifest, feature default or test threshold changed.

The full integrity-guard test compilation remains blocked: metadata generation
for unchanged Asupersync with its real `test-internals` feature was killed by
`SIGKILL` before reaching those tests. An initially broader library harness also
hit the dependency resource limit; its direct `fastmcp-core` dependency was then
correctly classified as test-only for these selected production modules. The
passing production gate uses the actual production dependency graph for those
modules. Preliminary scratch-harness dependency omissions, an unavailable
offline package and a canceled lock-generation attempt were not passing source
checks; the final harnesses use repository-pinned versions and matching patch
entries. No additional native Asupersync build was attempted. The new producer
regression has compiled but has not executed here.

## 2026-11-03 reader follow-through: private worker tables and pin selection

This correction supersedes the earlier assumption that a live leader's
descriptor inventory represents all its workers. A real Python worker called
`unshare(CLONE_FILES)` and opened the candidate while the leader remained alive.
The leader had `VmSize` and two threads, but only the worker's descriptor table
contained the reader. The unchanged production inspection returned `Ok(())`.
The new inline regression failed against the old implementation, after its idle
and ordinary shared-table controls passed. This is an executed counterexample,
not an inference from source or a synthetic proc tree.

The scanner now inspects every worker's descriptors through pinned task
directories, without recursively enumerating workers again. Mapping coverage is
shared only after a successful nonempty scan; an empty, gone or inaccessible
leader leaves worker mapping inspection necessary. Permission retries reset the
coverage established by the previous failed attempt. The 131,072-entry bound
and hard-error/refusal rules remain intact.

The cleaner also cannot exclude a descriptor solely because its number matches
the retained file's number. A worker with an unshared table can reuse that number
for an independent reader. The scan now borrows the retained `File` and, only
within its own process group, requires bounded `fdinfo` evidence of the same
exclusive whole-file FLOCK, owner PID, device/inode and descriptor metadata.
Linux's [`__show_fd_locks`](https://github.com/torvalds/linux/blob/v6.12/fs/locks.c)
filters records by the open file description. The held exclusive lock therefore
distinguishes the cleaner's description from an independent open of the same
inode. The local record ordinal is not an identity. A real shared/copied/reused
descriptor experiment confirmed that distinction; missing or malformed proof
remains a refusal. Other processes receive no cleaner-descriptor exclusion.

### No-open protection before pin selection

The review also found that snapshot selection could open, hash and close a
candidate before the later deletion preflight. Even rejecting a candidate on
size or hash mismatch can release a same-current-table POSIX lock on its inode.
Retention now supplies a no-open current-thread preflight before each
selection candidate's first open. The public shared selector retains its
existing shared-authority semantics with a neutral callback.

A refused early candidate may be bypassed only when selection finds a fully
verified fallback. If no fallback is selected, acquisition propagates the first
reader-preflight error, preserving its kind, and releases both authority locks.
In particular, an occupied but reader-blocked pin must not be classified as
missing verification and turn into permission for a repair export. The
pin-only regression requires `WouldBlock`; the valid dated fallback case still
requires successful admission.

The additional public-guard integration regression covers a locked actual pin
and a locked nonmatching canonical `.bak` preceding a valid dated pin. It takes
an actual `F_SETLK` read lock, probes from a separate process before and after
guard acquisition, and avoids opening or closing the locked inode in the parent
while checking it. After the reader closes, the probe must show the lock gone
and selection must succeed. Primary, metadata and fallback bytes remain intact.
The exact Python probe independently passed held/free controls and rejected both
inverted expectations. This validates the probe, not execution of the complete
public-guard regression.

### Native namespace evidence

For namespace source SHA-256
`ea70a998c56c05adedcdb1032f8ddbb2a7930c349f9e97dc05c1768ef796d2de`,
one final native invocation passed **14 tests**, with zero failures, zero
ignored and 45 filtered, in 18.84 seconds. It includes six new regressions,
the strengthened same-process POSIX-lock case, and all seven prior
superseded-snapshot cases. The cases cover:

- The live unshared-worker counterexample and ordinary idle/shared controls.
- Actual shared and copied retained descriptions versus an independent open
  reusing the same descriptor number, plus sixteen live shared Rust workers.
- Missing, malformed, truncated, oversized and wrong-identity flock records.
- Mapping reuse, empty-leader fallback and mapping reinspection on retry.
- The new current-thread preflight and original global preflight, with an
  independent process checking that a refused reader's POSIX lock survives.
- Existing inaccessible-task, process/task replacement, ownership, observable
  reader, hidden-handle unlink and interrupted-directory-sync protections.

The causal baseline source, old implementation plus only the new live-worker
test, has SHA-256
`b247f2b4cfaa8b5e4ea4b66c75e114e1a5ca4d92dfa049cc1a7d8db81b67c500`.
The passing focused test executable has SHA-256
`b71b399e030eb71c0cc82590e5d2d02d4aea249b37e1b1b97ffc5adcdc76aaf7`.
Strict exact-namespace Clippy passes with the existing workspace/DB allowances.
Two independent source reviews found no blocker in the namespace and selector
changes. No unsafe Rust, new dependency, scan-bound increase or weakened
assertion was introduced.

### Full namespace comparison and remaining qualification limits

Both complete native namespace suites were executed by the same controller,
with the same environment, working directory and real dependencies, one test
thread, no filters or ignored cases, and a 240-second outer bound. Neither run
timed out:

| Source | Passed | Failed | Ignored | Filtered |
| --- | ---: | ---: | ---: | ---: |
| Starting namespace (`3f6ee67988610a6644d9a0f27a5d0d95400337a76e023d9beeb4cee1d4aa7ae5`) | 40 | 13 | 0 | 0 |
| Changed namespace (`ea70a998c56c05adedcdb1032f8ddbb2a7930c349f9e97dc05c1768ef796d2de`) | 46 | 13 | 0 | 0 |

The failing test names and diagnoses are identical. Twelve strict generic-file
or empty-directory cases encounter the ambient `/proc/1/fd/0` permission denial
before their intended later assertions. The unchanged UID-drop fixture fails
with `EINVAL`. The six additional cases pass in the complete candidate run.
This comparison does not make the full suite green, and the different test
sets are not a performance benchmark. No visibility refusal, assertion or
scan bound was weakened to change the result.

The complete baseline log has SHA-256
`deb4c8cbfc6c9a27e943e1c0759ab14f7c7ace0046ccf00e909daba0114afde1`;
the complete candidate log has SHA-256
`7b6c2cb7571c978e420f06c94c03c918dbda158abeaecd074371b73111790054`.
The unchanged-baseline executable has SHA-256
`f03906323d1d83133f7d5c785a7aa4fb8130fe88a26c3e2cb024a217a0479ea0`.
The candidate executable is the same one used for the 14-case focused run.

The final reader and selector source identities are:

| File in `crates/mcp-agent-mail-db/` | SHA-256 |
| --- | --- |
| `src/recovery_retention.rs` | `4e38200ed52a96862b9b78c0fdb632ee3b7500764774dda191acc7110ef9d230` |
| `src/recovery_retention_namespace.rs` | `ea70a998c56c05adedcdb1032f8ddbb2a7930c349f9e97dc05c1768ef796d2de` |
| `src/snapshot.rs` | `77fde638e2a8118a67d5177e8cdc3a273ab3fab81fffad672b8b80f7f517d646` |
| `src/snapshot_authority.rs` | `05d0370b782592aeb0eaaf0c5e7553d00c8642ad285df4c8992326853979e7cd` |
| `tests/retention_linux.rs` | `2a0596aec4f03754465205049f8fc2918d4da1e50d0ddb6caa0fab2d890e3703` |

Final checks against those source identities:

| Gate | Result and proof class |
| --- | --- |
| Complete production DB source, no default features | Metadata/type checking passed in 26.142 seconds, zero diagnostics |
| All eight public Linux retention integration tests | Metadata/type checking passed in 0.563 seconds against the fresh production DB artifact |
| Those same eight integration tests | Strict Clippy passed in 0.446 seconds, zero diagnostics |
| Exact namespace source | Strict Clippy passed; the native results are recorded above |
| Formatting, diff checks and independent source review | Passed |

The final DB check uses the recorded production compiler invocation and actual
repository-pinned dependencies, including core metadata from the successful
Cargo production check. To fit the exhausted overlay, it invokes `rustc`
directly, disables incremental-cache writes and writes metadata to tmpfs;
source, dependency versions and production feature selection are unchanged.
The integration wrapper imports the exact production test file and uses that
fresh DB metadata, the actual core and test-helper crates, and real `nix`
metadata. No functions or assertions were replaced. These checks compile and
lint the eight tests; they do not execute them.

The preceding Cargo production DB check also passed before the final
blocked-pin error-propagation refinement. Its full DB strict-Clippy run was
killed by `SIGKILL` at the shared 8 GiB memory cap, without a source diagnostic;
it is not a passing gate and was not repeatedly retried. The final direct DB
metadata and integration checks completed without another OOM event. A scratch
record write hit `ENOSPC` before the last integration lint started; the lint was
then run successfully with its records on tmpfs. Final source hashes were
checked again after all successful gates.

Full native public-guard integration, the changed producer regression, the
unchanged 720-generation and 16,641-directory qualification gates, and full
workspace qualification remain unexecuted here. Earlier complete native builds
were blocked by Asupersync code generation under the same resource limit.
The native component results and exact-source compilation above do not replace
those outstanding runtime gates. No dependency, feature default, test size,
scan bound or assertion was changed to obtain a passing result.

## 2026-11-03 producer witness and pin-lock integration correction

Fresh review of `e16fc7f` found that its pin-selection preflight was too broad.
`DbPool::create_proactive_backup_admitted` retains a `SqliteFileWitness` after
publication, whose `same_file::Handle` keeps the backup inode open. Refusing
every same-process descriptor during read-only pin selection therefore also
refused the producer's own ordinary identity witness. Once retention needed
the pin to reclaim a saturated window, that refusal could strand cleanup and
subsequent automatic exports. This is a concrete integration regression in
the preceding change, not an unrelated feature request.

The existing real-pool regression
`verified_snapshot_uses_one_publication_admission_and_excludes_retention`
already expresses the decisive sequence: publish through the real pool, then
acquire retention while the pool remains alive. Its original assertions are
preserved. The earlier compilation-only gate could not expose its runtime
failure; the component checks below do not claim that this full producer test
has now executed.

Pin selection now checks actual traditional POSIX lock records in the current
thread's descriptor table before opening a candidate. An ordinary unlocked
handle can coexist with read-only pin verification. All matching descriptors
must be inspected: the locking descriptor and its duplicates expose the POSIX
record, while an independent unlocked descriptor of the same inode does not.
The printed lock PID is not an ownership filter; a descriptor table can be
shared across process IDs. Unknown, incomplete or unreadable relevant lock
evidence remains a refusal. The full descriptor/mapping checks for deleting a
candidate are unchanged.

Linux's [`__show_fd_locks`](https://github.com/torvalds/linux/blob/v6.12/fs/locks.c)
filters records by both the open file description and its owning descriptor
table (or open description for OFD/flock locks). Actual Linux memfd probes
confirmed READ and WRITE POSIX ranges, including to-EOF locks, on the locking
descriptor and its duplicate, their absence on independent ordinary/O_PATH
descriptors, and a real independent-process conflict. Opening and closing a
new same-table descriptor released the POSIX lock. In contrast, a worker's
lock after real `unshare(CLONE_FILES)` survived the main thread's independent
open/close and remained visible to an independent process until worker exit.
Real FLOCK and OFD read/write locks likewise survived independent open/close,
with conflict and post-unlock controls. These probes used disposable anonymous
files and no substituted syscall results.

The public Linux regression now retains the producer's concrete
`same_file::Handle` identity type throughout all eight superseded-backup
unlinks beside a genuinely nondumpable same-user process, then checks the
pinned inode and bytes. Its POSIX-lock case also holds that independent
unlocked identity handle before acquiring the lock on a second descriptor;
the refusal must preserve the lock, and later selection must succeed while
the identity handle remains open. This additional file/guard coverage does not
substitute for native execution of the existing complete pool regression.

### Executed component controls and full comparison

Four new native namespace tests passed with real Linux descriptor tables and
repository-pinned dependencies. Together with the prior 14 reader cases, the
focused run passed **18 tests, zero failures, zero ignored**. The added cases
cover retained producer witnesses, recognized and complete lock records,
independently opened POSIX lockers, and real shared versus private worker
descriptor tables. The witness case separately proves that allowing read-only
pin selection does not admit deletion of the same open inode. FLOCK and OFD
controls retain their conflicts after an independent open/read/close.

A small causal control includes the entire unchanged preceding namespace
source (`ea70a998c56c05adedcdb1032f8ddbb2a7930c349f9e97dc05c1768ef796d2de`)
and calls its existing helper directly, without aliasing, editing visibility,
or copying the production implementation. With the same payload and real
`same_file::Handle` plus O_PATH setup, that helper returns `WouldBlock` and the
control fails. The candidate witness test passes. This is a component
counterexample, not a full pool execution or a verbatim transplant of all
the candidate's later lock controls. The baseline fixture uses an absolute
temporary directory without the candidate fixture's extra canonicalization;
both helpers canonicalize their source, and failure precedes the differing
later assertions.

Both unfiltered native suites then ran in one controller invocation, with one
test thread and the same environment and 240-second bound. Both used
`TMPDIR=/dev/shm` because the shared overlay was full. No procfs restriction,
test assertion, scan bound or ignored-test setting changed.

| Namespace source | Passed | Failed | Ignored | Filtered |
| --- | ---: | ---: | ---: | ---: |
| Preceding `ea70a998` | 46 | 13 | 0 | 0 |
| Candidate `bdf70eda` | 50 | 13 | 0 | 0 |

The same 13 test names fail with the same diagnoses: twelve strict generic
file or empty-directory cases encounter the ambient `/proc/1/fd/0` permission
denial, and the unchanged UID-transition fixture encounters `EINVAL`. There
are no new or resolved failing names. These are not green full-suite results,
and the differing test sets are not a performance benchmark.

The candidate native executable has SHA-256
`f4a4ebd7d2aa8d2dbeea018c630c3b78700f392675da9eceac40b525952190cd`.
The focused log has SHA-256
`61d6f760cc02358fc55acafc3bbda95a038f433e58fe6b13d5a55e7391bd9ce6`.
The complete baseline and candidate logs have SHA-256
`7b0eae5076302cc34bf42ab2b630f6036c4ca8cc541d3f7b5e675f93bea0084e`
and `4bd9e4a0c902844ab7944c0b6fe2cdda2c6ded70f94e3dab94239e106fe37783`.
The comparison receipt has SHA-256
`85debcdd4877dcd6554dbe2653582e20c00c750a74671f60b8a57a39cd1ed9ce`.

The exact changed source identities are:

| File in `crates/mcp-agent-mail-db/` | SHA-256 |
| --- | --- |
| `src/recovery_retention.rs` | `11476dd16b3f758a4675e5451946f351829c46e866ef9bbe5c4533f034b9d33f` |
| `src/recovery_retention_namespace.rs` | `bdf70eda07f6996e929a5b818e46ad0cab223772bd6b3d1f7978058cf0e8131b` |
| `src/snapshot_authority.rs` | `31433c8b3b45d66efba856a0483182271432b569a3161f1c3c63f8093170bce0` |
| `tests/retention_linux.rs` | `445947b7468f2c0246c1a072adfbb89cd129eaab7e870e0c4ebb2b5f53554078` |

The exact namespace also passes strict Clippy with the existing workspace/DB
allowlist and real metadata/native dependencies, with zero diagnostics.
Independent review confirmed that the deletion inventory remains unchanged.

### Integrated compilation and remaining runtime limits

The initial production DB metadata check and all eight Linux integration tests'
metadata/strict-Clippy checks passed. While this work was being validated,
`main` advanced to `835083b277cf750a027a9af817f21b78d6f691c3`, preserving the
preceding retention change and merging unrelated inbox/archive work. None of
the seven files in this correction overlapped that merge.

To check the integrated DB source despite the full overlay, a separate source
tree on tmpfs contains exact copies of module parents, the changed retention
files, and all three DB files changed by that merge; unchanged modules point
to the original source files. The merged `queries.rs`, `sync.rs`, and
`sync/inbox.rs` bytes were independently checked against GitHub's exact blob
identities. This changes the compiler's source-root/output paths, not the
production code, dependencies, feature selection or Cargo package environment.

| Final integrated gate | Result and proof class |
| --- | --- |
| Complete production DB source, no default features | Metadata/type check passed in 34.697 seconds, zero diagnostics |
| Exact eight public Linux retention tests | Metadata/type check passed in 0.737 seconds against the fresh DB artifact |
| Those same eight tests | Strict Clippy passed in 0.706 seconds, zero diagnostics |
| Exact namespace | Strict Clippy passed; 18 focused native tests passed as recorded above |
| Formatting, diff checks and independent source review | Passed |

The integration checks import the exact production test file and actual
dependencies, including `same_file`; no test or production function is replaced.
These are compilation/lint results, not execution of the public guard tests.
The existing complete pool regression, native public integration tests, the
unchanged 720-generation and 16,641-directory qualification gates, and full
workspace qualification remain unexecuted here. Earlier native DB builds were
blocked by Asupersync code generation under the shared memory limit. The known
resource-blocked broad builds were not repeatedly rerun, and no dependency,
feature default, assertion, test size or scan bound was weakened.

## 2026-11-03 retained exporter parent binding

The next review found a separate stale-authority path. The exporter retained a
directory lease, but its cleanup entry point accepted only a boolean assertion
that a parent lease was held. If that directory's pathname was replaced, the
cleanup path could acquire snapshot authority from the replacement while its
caller still held the displaced directory's lease. Publication's later pool
authority check and journal completion checks did not protect the earlier
retention attempt. This finding comes from the actual call chain; a baseline
full-runtime deletion has not been executed on this runner.

The server now passes a borrowed opaque `Admission` containing the retained
lease and frozen parent spelling through quota preparation, producer dispatch,
export admission, completion cleanup and admitted-refusal cleanup. A failed
outer admission still uses public rotation, which must acquire its own lease.
In-memory operation supplies no filesystem token and performs no cleanup.

Both early empty-directory reclamation and later payload reclamation bind
snapshot selection through `SnapshotRetentionGuard::acquire_for_parent`. Its
initial check refuses an already replaced parent before control-file
acquisition. The decisive check compares the directory actually captured by
snapshot authority with the expected retained directory, rather than merely
checking the pathname again. The guard duplicates that expected handle and
repeats the binding alongside its existing authority, recovery and pin checks.
Retained descriptors prevent inode reuse from making an unrelated directory
appear to be the admitted one. Reader, recovery, ownership and sync policies
are unchanged.

Added regressions cover a real directory lease across replacement and
restoration; refusal before creating controls in an already replaced parent;
captured-directory mismatch even after restoring the pathname; and real
rotation/coordinator call paths with replacement before and during a producer.
The replacement fixtures contain valid snapshot metadata created at their
eventual configured pathname. Invalid foreign-path metadata cannot mask the
stale-lease defect. They require the replacement's complete file/control tree
to remain intact and require reclamation to resume after restoring the
original directory. Existing workload sizes and interrupted-operation
assertions remain intact, and no procfs-observability skip was added.

### Executed admission and staging foundations

The exact new borrowed-admission replacement test passed. The complete actual
admission and staging modules then passed **25 tests, zero failures, zero
ignored, zero filtered**. This includes real cross-process flock contention,
release after panic, shared and independent parent directories, count/byte
limits, bounded inventory and preservation of symlink or unclassified evidence.
The harness uses only a neutral crate alias and source-module paths to compile
the complete production modules with real dependencies; it substitutes no DB,
pool, syscall or test implementation.

The executable has SHA-256
`d272a107e2fe9381a181c7312f6f1eb5884cfe93c805819fe5ccdf7173afc0e5`.
The full native log has SHA-256
`ac1bb75c90b6dd53125070e3ae001138cabca7be3ad40dc03ebbce309d908e71`,
and the test receipt has SHA-256
`0e2e04e772edbd02ff189ce7aa3054ba6645f03e8a84f457323b46e1a1ef678c`.
The unchanged staging source has SHA-256
`e7ad4a6824783ee2b2de398708bcf5ac99720ca8408552389f208dd039df8098`.

The changed source identities are:

| File | SHA-256 |
| --- | --- |
| DB `src/snapshot_authority.rs` | `e9227c1fd43146470ab3dbac780b6abbeceab9b8ee260202a482d4f6cdcd2afd` |
| Server `src/integrity_guard_backup_budget.rs` | `e7716488150a1d93aa85995c3d8a212513ec8a72b3713f7c0c08f4f9d86381a8` |
| Server `src/integrity_guard.rs` | `dc9cd498dd73d4caa33ee3314a9f19486d66325cba586552ff18babc4607d303` |
| Server `src/backup_rotation.rs` | `75555cd0d517986444c0f81fb295895da25ee8ed903302f13c287187d0ea1037` |

The two independent source reviews found no blocking issue in the token chain
or retained-descriptor comparison. The native foundation result does not
claim native execution of the full snapshot guard, rotation, pool or
coordinator regressions. The unchanged recovery/journal control-file paths
can still create control state if an uncooperative replacement occurs between
checks; this change does not claim every control-file operation is
atomically relative to the retained directory. The guarantee added here is
that a stale exporter token cannot adopt a different captured snapshot parent
as its reclamation authority.

### Final production call-chain checks

The complete production DB source, with the new authority and the exact merged
inbox source from `117a56b`, passed metadata/type checking in 22.503 seconds.
The complete production `integrity_guard` and `backup_rotation` modules, their
actual sibling modules and three verbatim production root helpers then passed
metadata/type checking in 1.294 seconds and strict Clippy in 1.043 seconds,
using that fresh DB artifact and actual dependencies. Both final server gates
had zero diagnostics. Four initial idiom/visibility lint findings were fixed
without adding suppressions, and the native 25-test foundation gate was rerun
against the final budget bytes recorded above.

The DB check uses the same exact-source tmpfs approach as the preceding pin
fix. The server check compiles the full production modules and real token
callers; it is not a whole-server-crate qualification or a runtime execution.
It excludes `cfg(test)`. The two new authority unit-test definitions and new
full rotation/coordinator regressions were initially source-reviewed only.
The genuine whole-DB test graph requires Asupersync `test-internals`; an earlier
attempt to build that graph was killed by the shared memory limit, and no
alternate helper implementation or weakened feature graph was substituted to
claim a pass.

A subsequent scoped check compiled the entire final rotation module and 16
verbatim source-item spans from the final integrity guard, including its real
producer, admission, cleanup and coordinator bodies, five fixture helpers,
and the new coordinator regression. Complete actual budget, journal and
schedule test modules were also included. Test metadata passed in 2.392
seconds and strict Clippy passed in 2.334 seconds, with zero diagnostics.
Every selected source span was rechecked byte-for-byte against the final
production file. No producer method, helper, assertion, fixture or type was
substituted; unrelated integrity pool/runtime tests requiring unavailable
test-internals metadata were excluded rather than faked.

This typechecks and lints
`replaced_parent_blocks_coordinator_and_all_admitted_cleanup`,
`exporter_cleanup_cannot_adopt_a_replaced_database_parent`, and
`borrowed_admission_keeps_its_parent_identity_after_path_replacement`.
The first two were not executed natively; the third also passed in the
separate 25-test native foundation suite. The two new DB authority unit-test
definitions remain source-reviewed only. The final receipt has SHA-256
`8b07435465aa2593e554d2d7b02c9342dfa0549a6521f9efac54bb7ba66c7dc9`
and records all commands, source spans, hashes, earlier lint findings and
remaining runtime limits.

## Dedicated native qualification workflow

The focused [Issue 373 workflow](../../../.github/workflows/issue373-native.yml)
gives the remaining real-runtime gates independent DB and server jobs. The
general CI job already declares workspace tests, but earlier formatting/lint
or build failure can skip that test step. This workflow builds genuine test
executables first, then runs the namespace/reclamation, snapshot authority,
retained-pool-witness, public Linux retention, rotation, and integrity-guard
suites without requiring an unrelated workspace lint step to succeed.

Both jobs use the repository's exact pinned toolchain, locked dependencies and
unchanged default features. They retain normal Cargo test execution, serial
tests and the original workload sizes. The pool regression's actual module
is `pool_impl::tests`; public Linux integration is selected through target
`it`, not a nonexistent standalone `retention_linux` target.

A failed test build prevents every later suite in that job. After a successful
build, a failed suite remains a job failure but does not suppress the other
independent suites. Each test gate checks its command and logging exit statuses
and the final parent executable's summary, requiring a nonzero pass count,
zero failures and zero ignored tests. Exact source identity, commands, logs,
statuses and source-change checks are retained as artifacts, including after
failure. A pending or failed run cannot be reported as runtime qualification.

The workflow source at publication has SHA-256
`97fad4ba2dafffff4588d4a1c2076ea412d0f9054fe0176b6c889b48183249d5`.
YAML structure, all nine Bash blocks, the embedded gate helper and exact test
names were checked. Nine synthetic helper controls checked acceptance and
rejection of result/exit-status shapes; they are workflow-logic checks, not
Agent Mail test results. Independent review repeated YAML and shell syntax
checks. No native GitHub Actions result is claimed by this static validation;
the runtime gaps recorded above remain pending until an actual run on the
changed commit completes.

## Journal authority follows retained exporter admission

Further review of `501f455` found that the coordinator validated its retained
parent admission and then called a pathname-only journal constructor. That
constructor independently opened the database parent. Replacing directory A
with B between those steps allowed B to receive the durable export intent.
Restoring A before the next admission check made the pathname valid again,
while the journal and its one-time repair reservation still belonged to B.
The original directory could consequently accept another repair export.
This is a durable admission defect, even though the journal's existing
descriptor-relative completion code faithfully updated its captured parent.

Production `AutomaticBackupLease::try_begin` now requires the actual retained
`BackupParentAdmission`. It preserves the existing no-follow ancestor walk,
absolute-path and regular-mailbox checks, compares the captured directory
against the retained admission's device and inode before any journal open,
then duplicates the admitted directory for every journal operation and sync.
Both directory handles remain alive during the comparison, preventing inode
reuse. Restoring the pathname cannot make a captured foreign directory pass.
The pathname-only clock-controlled constructor is restricted to tests.

The coordinator supplies its existing token. The journal format, slot
checksums, retry/backoff state transitions, repair reservation, completion
semantics, no-truncation rule, file lock, and file/parent sync ordering are
unchanged. The pool's existing authority check after the before-export
callback remains in place. Two independent source reviews found no blocking
issue in this change.

### Native regression and causal evidence

Three new tests run the real admission and journal implementations. They
cover replacement after token validation with both absent and valid existing
foreign journals; restoration after the foreign parent has already been
captured; successful admission after restoring the original directory;
retained repair debt on reopening; completion after a later replacement; and
preservation of the original no-follow ancestor policy. The foreign mailbox
and complete journal namespace must remain unchanged. The replacement seam
uses ordinary directory renames, not a mocked filesystem.

All three new tests passed. The complete actual journal, schedule, parent
admission and staging modules then passed **81 tests, zero failures, zero
ignored, zero filtered**, serially. The native harness uses complete source
modules, a neutral crate alias, and the production parent constant/type alias
with real dependencies. It substitutes no syscall, admission, journal,
schedule or staging implementation. Tests use real files, locks, and child
processes; the native fixture filesystem was tmpfs. This is component-level
runtime evidence, not execution of the full pool exporter or server.

A separate native counterexample compiled the entire unchanged journal from
`501f455` alongside the actual admission module. Its honest-parent control
passed. The replacement probe observed A's admission passing before and
after replacement, B retaining the journal and spent repair allowance, and
restored A accepting another repair. The required original-parent debt
invariant then failed as expected. This was an explicitly separate
counterexample, not a claim that the new regression source compiled unchanged
against the old API or that a full coordinator export was executed.

The candidate journal has SHA-256
`efa1d7a080b822a5c2eb56a264d2967ff1c5eec9deb700606e63748c37935442`;
the coordinator has SHA-256
`1e1407a332592ace494814644522763003c9b2752f1b24fe6f14dbf7960edfc1`.
The native executable has SHA-256
`a173ba9a151bfed88c3ea98323b1b69f7a007ef1dfbb552727027ae1b57d43b5`,
the complete native log has SHA-256
`470886595ffb9c1e8aa842074c7bb59aa9d68d47150d5bc2452733c761f3055b`,
and the native receipt has SHA-256
`e512f44c28b0ac0c19ca2f3d79a3b2c83c18c2073f4efba5333d94b9c41eacaf`.
Exact source witnesses, commands, logs, both executables, and the baseline
scope are recorded under `am-retention-journal-parent-validation`.

### Production integration and lint checks

The complete production `integrity_guard` and `backup_rotation` module graph,
its actual sibling modules and the same three verbatim production root helpers
passed metadata/type checking in 1.524 seconds and strict Clippy in 2.683
seconds. The complete native component modules, including all 81 test
definitions, also passed strict Clippy in 0.768 seconds. All three final logs
contained zero diagnostics; no lint requirement was reduced.

The existing actual production DB metadata was reused because this change
modifies only the server journal and its call site. All 87 DB source-file
hashes were checked against that artifact's source manifest before and after
validation. Its SHA-256 remains
`18a00719cde9b3f9058583bb724e82d54a88ad7bb69dceeccd176178d35bf425`.
This is not a new DB build, a whole-server-crate check, or a substitute for the
remaining full-runtime producer gate. The production/lint receipt has SHA-256
`82e6eebc6c57049150d765bef55bb1a7954b4af47f6cb1d5dc326bf14f24b4f9`.
The repository-wide `cargo fmt --check` and final `git diff --check` passed.

### Remaining authority and reader limits

This closes the journal part of the control-file limitation recorded above.
Recovery-breaker control-file creation and permission changes still have
separate pathname-based acquisition paths; this commit does not claim those
paths are descriptor-relative. It also does not close the broader
`br-xzgcj` pool/archive authority work.

Source review also identified an unresolved POSIX-lock preservation gap in
the shared reclamation primitives. Staging, or final deletion after a reader
arrives beyond the earlier preflight, can open an ordinary descriptor and
then close it on completion or refusal. Linux can release another descriptor's
traditional POSIX lock in the same descriptor table when that close occurs.
The existing preflight test does not cover the whole subsequent open/close
interval. No new full-production runtime reproducer is claimed for this
separate source finding.

An `O_PATH` pin has lock-neutral close semantics in the
[Linux close implementation](https://github.com/torvalds/linux/blob/v6.12/fs/open.c),
but cannot directly supply the current per-file fsync and exclusive flock.
The change therefore does not remove either protection, replace per-file
sync with filesystem-wide sync, or claim another preflight makes that interval
atomic. Resolving this remaining issue requires a reviewed I/O ownership
design. The original full-runtime, public Linux integration, and large
retention gates remain distinct from the component result above.

## One retained directory for recovery and snapshot control acquisition

The follow-up to `1fad15c` closes the separate control-file acquisition gap
identified above. Retention previously checked an exporter's directory, then
independently resolved the recovery election and snapshot authority paths.
A replacement between those operations could create a snapshot authority in
another directory or create/change permissions on its recovery election file
before the eventual admission refusal. Restoring the original pathname did
not undo those foreign-directory mutations.

The actual `SnapshotRetentionGuard` now delegates control acquisition to one
filesystem-only helper that captures a no-follow parent, compares any supplied
exporter parent before mutation, and lends that same retained directory to
both control constructors. Standalone retention captures the same shared
parent even without an exporter token. Recovery election still precedes
snapshot admission, and both controls are revalidated before pin selection.

Both Unix constructors compare their newly captured parent with the retained
directory before any control-file open. Creation and recovery-lock permission
normalization then use `openat` against a retained clone. Snapshot authority
keeps its existing strict empty/private/single-link rules, shared versus
exclusive locking, and file/parent sync sequence. The generic Unix recovery
election uses the same retained-directory implementation. Its revalidation
compares the held control with `statat` in the retained directory and checks
the current parent before and after, without reopening the control file.
Private-mode drift also refuses continued authority. Non-Unix recovery-lock
behavior is unchanged, and the existing recognized platform-directory alias
rule is retained.

Late replacement can still leave an empty control file in the admitted
directory before admission refuses. No foreign directory inherits that
creation or permission change. Control files are not removed on refusal or
release; their persistent inode remains the cooperating processes' election
authority. This change does not read or update recovery outcome/backoff state.

### Actual native control-pair evidence

The candidate and baseline native harnesses used 50 complete
production/test/helper items verbatim, with source-byte ranges and hashes.
The candidate includes the actual two-control helper called by retention,
both real control constructors and guard revalidation bodies, and the real
core permission/platform-alias helpers. No pool, database, syscall, or
filesystem implementation was substituted. The selected native boundary
passed **12 tests, zero failures,
zero ignored, zero filtered**: six new regressions and six unchanged controls.
Strict component Clippy also passed without diagnostics or policy changes.

The new tests cover a captured foreign parent followed by restoration,
replacement after a successful parent comparison, absent and existing foreign
controls, and replacements before either constructor in the actual paired
acquisition. The paired test exercises both supplied and absent expected
parent identities. Foreign namespace checks preserve entries, bytes, device,
inode, mode, link count and modification/change timestamps. Breaker controls
include initially permissive and read-only modes. Existing and new controls
also exercise real cross-process contention, release, reacquisition,
shared/exclusive snapshot admission, symlinks, hard links, unsafe shapes,
and retained-control replacement. The fixture filesystem was tmpfs.

Two ordinary-parent controls using unchanged old code passed first. A
separate old breaker-leaf probe then measured foreign control creation and
`0666`/`0400` normalization to `0600`; its preservation invariant failed.
A separate old complete snapshot-authority probe measured creation in the
foreign directory before a later retained-parent refusal; its preservation
invariant also failed. These are actual old-source component counterexamples,
not full old recovery/retention runtime runs or transplanted new regression
definitions.

The reviewed source identities are:

| File | SHA-256 |
| --- | --- |
| DB `src/recovery_breaker.rs` | `8b185f6ae4089d19279e3758df0894419ef0c1e3f2785b24b0c02e697778b4a0` |
| DB `src/snapshot_authority.rs` | `af1e8324fb10b417c932d8082c4d0f2a54c2268110360bd64e51270f138316b4` |

The native executable has SHA-256
`ab973cef7af35086b9c9ad5721893d29c9c953c8dd701718ec243d390ddde4b9`,
the complete selected-test log has SHA-256
`e4c0de99282c820e4810f9941f1a965d007382d26d6c595727ce1fbc492ffc84`,
and the native receipt has SHA-256
`0b1759f7f257990630d6776fb3f2673785589cf9c711e689f2801f4bb2057057`.
Exact source items, both native executables, commands, results and baseline
scope are retained under `am-retention-control-parent-validation`.

### Production integration attempt

One fresh metadata/typecheck attempt used the complete production DB source
with both final changed files and actual dependencies. It ended with SIGKILL
(exit `-9`) after 40.198 seconds and emitted no compiler diagnostic. During
that interval the shared cgroup's `oom_kill` counter increased from 21 to 23.
The recorded initial usage was 8,589,926,400 of 8,589,934,592 bytes. This is a
resource failure, not a passed typecheck or a reported source error.

No fresh DB artifact was produced, so the planned downstream server checks
were not started against an older artifact. A subsequent headroom check found
the shared cgroup full again; the same large graph was not blindly retried.
The failed command, empty log, resource samples, and result are preserved in
`production-first-failure.json`, SHA-256
`8593bd2587b7ea4a59465977a41d0556d98076822335732dbe03e62a5178b5f3`.
The native component and strict component Clippy results above remain the
executed evidence for this patch; whole-production integration qualification
remains pending.

### Hosted coverage and remaining scope

The existing issue-373 native workflow now also runs the complete
`recovery_breaker::` suite from its already-built real DB test executable.
The new step uses the same build-success prerequisite, locked dependencies,
default features, serial execution, and nonzero/zero-failure/zero-ignored
result check as the other suites. It can run after another test suite fails,
and its logs have a separate name. No existing gate was weakened. YAML and
all ten Bash blocks passed static syntax checks; independent review confirmed
the actual test namespace and conditions. This does not claim a completed
hosted native run.

The native result proves the real filesystem control pair and selected
regressions. It does not execute `SnapshotRetentionGuard`'s SQLite pin
selection, the full pool producer, or the large retention workloads. Recovery
breaker outcome-sidecar storage still has its separate pathname-based flow;
this commit does not claim all breaker state I/O or the broader
`br-8asz3`/`br-xzgcj` work is complete. The previously identified ordinary
descriptor-close/POSIX-reader-lock issue also remains unresolved. Its file
lock and per-file synchronization protections have not been weakened.

## 2026-11-03 follow-up: recovery outcomes use the held election

This change starts from `7dafa0af881e7d02f60216cd1921a38e4363ce48` and
closes the outcome-sidecar boundary identified above. The actual automatic
recovery wrapper retained an election lock but reloaded and stored the JSON
history through independent pathname operations. A directory replacement
could redirect the history read, legacy permission normalization, temporary
creation, publication, or directory sync. Replacing only the election leaf
also allowed the old wrapper to clear history under an obsolete lock while
another owner held the replacement inode in the same directory.

### Implementation and policy

The Unix recovery wrapper now acquires its election through the actual
`AnchoredRecoveryParent.parent_dir`. The guard owns immutable state names;
its authoritative `load()` and `store()` take no caller-supplied database
path. The reload and all five writes use this guard: entry arming, panic
retargeting, successful completion, restoration after host storage failure,
and terminal failure recording.

Guarded loading checks the directory entry without following links, refuses
nonregular or multiply linked controls before opening, compares the captured
entry with the opened inode, and reads at most the existing 16 KiB limit plus
one overflow-detection byte. It rechecks file metadata and name identity.
Both present state and clean absence require final election validation.

Guarded storage normalizes an existing readable legacy control through its
verified descriptor, including a `0400` control. Temporary creation uses
exclusive `openat` in the retained directory. The tempfile builder supplies
random leaf names with its own pathname cleanup disabled before creation.
After writing and syncing the complete private file, the implementation
rechecks the destination, stage, and election, publishes with `renameat`,
then syncs the published file and the retained parent. Cleanup checks the
stage's identity and uses `unlinkat` in that same parent. Successful rename
disarms cleanup immediately, so a later error cannot unlink authoritative
state. A stage that was replaced by another entry is preserved.

The wrapper revalidates election authority immediately before execution and
after every nonpanicking result. Operator bypass still permits supervised
recovery from malformed or tripped history and ordinary best-effort outcome
persistence. It cannot suppress lost election ownership, including when a
guarded store fails. Original operation errors remain in contextual refusal
messages; the panic path preserves its original payload and leaves the
already-durable unfinished marker when retargeting cannot be completed.

The public pathname `load` remains nonmutating for pre-election refusal and
diagnostic callers; it does not create an election artifact. Public standalone
`store` remains compatible, while the production recovery wrapper no longer
uses it. Both forms share the same JSON decoder, encoder, schema, fingerprint,
size, and semantic rules. The pure failure-count, cooldown, unfinished-lineage,
host-storage, and fingerprint transitions are unchanged. Non-Unix guard I/O
revalidates around the existing pathname implementation; this change does not
claim a new descriptor-relative implementation on those platforms.

This is checked cooperative election ownership and retained-directory I/O.
Unix rename and unlink are not inode compare-and-swap operations against an
uncooperative actor changing leaf names after the final check. No such stronger
guarantee is claimed. The broader ownership beads remain open, as does the
separate ordinary-descriptor/POSIX-lock-close defect.

### Regression coverage and qualification boundary

Eleven new filesystem tests exercise guarded state loading, missing state,
foreign controls absent or initially `0400`/`0666`, all six publication phases,
productive directory replacement/restoration during file writes and directory
syncs, replaced elections, unsafe destination shapes, substituted stages,
malformed or oversized state, and invalid outgoing state. An open reader keeps
the complete old JSON while new readers see the complete replacement. Errors
injected before and after publication leave complete old or new authority.
Separate child processes exit without destructors immediately before or after
publication; restart reacquires the election and loads the appropriate complete
state. These are process-interruption tests, not simulated power-loss tests.

Three new tests in the actual pool module exercise automatic success, failure,
quota failure, and panic after election replacement; operator bypass losing
ownership before execution; and successful or failed operator operations that
lose ownership before terminal handling. They retain exact unfinished-state
bytes and genuine replacement locks. The panic case advances its completion
clock so an incorrect old retarget write would change the bytes. The workflow
adds these three tests and nine existing coordinator transition tests as twelve
separately checked exact cases. Existing suites and workload bounds are unchanged.

Independent source reviews covered the complete breaker implementation, caller
wiring, temporary cleanup, publication ordering, bypass behavior, and the new
regressions. YAML parsing and all eleven workflow Bash blocks passed syntax
checks; all twelve selected coordinator names resolve to actual tests.

### Executed evidence on the final source

The final native component run passed **49 tests, zero failures, zero ignored,
and zero filtered**. This consists of 37 unchanged Linux state, fingerprint,
and election tests; all eleven new guarded-state tests; and the unchanged
`retention_control_pair_refuses_replacement_between_each_acquisition` test
against the new guard. The latter uses the actual shared recovery/snapshot
control acquisition helper. Strict component Clippy passed without diagnostics
or new lint allowances. The native build took 1.518 seconds, the tests 0.131
seconds, and strict component Clippy 0.629 seconds.

The selected native boundary uses 110 byte-identical breaker source spans,
twelve unchanged snapshot spans, five actual core disk helpers, and the actual
SQLite suffix helper. Module/import routing and allowances for otherwise-unused
items in this selected harness do not substitute state, serializer, syscall,
filesystem, or pool implementations. The complete production breaker module
was separately metadata/typechecked against the genuine cached core/config and
dependency artifacts in 0.235 seconds. That check includes its real environment
configuration function. It is not a typecheck of the full DB crate or pool.

The unchanged old production primitives passed an ordinary-parent round-trip
control. Three separate preservation assertions then failed as expected:
the raw load accepted the replacement directory's cleared history even after
the original directory was restored; the raw store created or overwrote state
in the replacement directory; and the raw terminal store cleared unfinished
debt after another owner acquired a replacement election inode. These are
actual old state/filesystem operations under real locks, not execution of the
complete old coordinator.

The first candidate run passed 48/49 tests. Its sole failure was `EPERM` from
creating a listening Unix socket at fixture setup. Linux now creates a real
`S_IFSOCK` filesystem inode with safe `nix::mknod`, explicitly verifies its
type, and runs every original refusal and preservation assertion. Socket
coverage was not removed or skipped. Other Unix platforms retain the listener
fixture. The next run passed 49/49 and exposed eight strict-Clippy empty-vector
assertion diagnostics; equivalent typed-vector equality assertions resolved
them without changing the predicates. The exact final source was then rebuilt
and all four scoped gates passed. Production bytes were identical throughout
these test-only refinements.

| Reviewed final file | SHA-256 |
| --- | --- |
| DB `src/recovery_breaker.rs` | `dfbaf573db5c1be38dd08c65257869f82460bc1c743e3cb9b5d2f136e8e00519` |
| DB `src/pool.rs` | `9c910e5de3f0c090246090b85f147061611eeaae9fffdf85b1f6cdb449749e08` |
| `.github/workflows/issue373-native.yml` | `5b0913e780def6f3ee96f87483bfad9427c06fe86d397e8ff34465088552efcd` |

The complete command/log/provenance receipt is
`am-retention-outcome-parent-validation-final/validation-receipt.json`,
307,805 bytes, SHA-256
`bbda4469f491c8ccea42c37dd0cd55bc2528fdb74df720b6cc64744e71572f62`.
The exact source-span receipt has SHA-256
`f3fff41660827f9a95fac6f3a2e9344b792f4ceb4f4cac4592530bb89dfadcc9`.
The final native executable was 3,007,480 bytes with SHA-256
`56fde711cec0989d61b1ee09d0d11061e40ed1b880461556a4d25d7ea48ab315`.
The final module metadata was 82,238 bytes with SHA-256
`b2044eb8374e38dd6f0ab21c16895c369825a842e10d3c7c165f26be254d3ca7`.
Both were hashed within the successful invocation but remain ephemeral;
shared temporary storage filled during an earlier executable-copy attempt.
That incomplete copy is explicitly invalid and was never executed. The full
receipt, final source witnesses, commands, and historical failures were
preserved. The shared OOM counter did not increase during the final gates.

Whole-workspace `cargo fmt --all -- --check` and `git diff --check` passed with
the final code in the working tree. No complete DB build was retried under the
shared resource pressure; the previous 40.198-second SIGKILL remains the
documented full-graph blocker. The three new pool regressions are reviewed
source and hosted-CI selections, not locally compiled or executed pool tests.
No downstream server proof, full workspace test/Clippy pass, or completed
hosted run is inferred from the component results.
