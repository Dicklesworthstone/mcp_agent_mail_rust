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
