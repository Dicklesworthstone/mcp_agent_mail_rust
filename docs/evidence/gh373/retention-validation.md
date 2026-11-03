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
full rotation/coordinator regressions are source-reviewed only on this runner,
not represented as compiled or executed tests. The genuine DB test graph
requires Asupersync `test-internals`; an earlier attempt to build that graph
was killed by the shared memory limit, and no alternate helper implementation
or weakened feature graph was substituted to claim a pass.
