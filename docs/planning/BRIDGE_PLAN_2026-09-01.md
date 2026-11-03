# Bridge Plan: MCP Agent Mail (Rust)

## Current priority: live workers with unshared reader tables

- [x] Review the subsequent shared-checkout merge 9d38fd84 in full. Upstream
  a73ae5e3/71fda742/a3cdd223 change only the rotation/guard production files:
  numeric-generation reclamation order, sticky refusal after unconfirmed
  namespace mutations, and budget deferral before snapshot admission. Five
  added tests preserve the original populations and deadlines.
- [x] After all frozen 11d binaries finish compiling, fast-forward the clean
  validation checkout to 9d38fd84. The two changed server inputs receive mtimes
  after the old artifacts finish, while the already-running remote namespace
  continues to match 11d. Freeze all 3,902 expected source inputs again.
- [ ] Re-run required compiler gates and the complete now-139-case selection.
  Normal hz3 admission refuses unavailable slots; the hz4 rerun refuses the
  combined I/O/disk-pressure gate before Cargo. Preserve both failures and
  finish the authorized verified move; never override admission or capacity.
  After the move, normal hz4 admission starts the frozen 9d native build;
  normal vmi1152480 admission passes all-targets in 3,937.2 seconds. Its
  subsequent production request refuses before Cargo; that worker then reaches
  zero disk headroom during unrelated builds. Preserve the pass and refusal
  separately. A normal vmi1149989 request starts the remaining production and
  strict Clippy pipeline. Live native/all-targets input witnesses match all
  3,896 present files with the same six historical JSON omissions. Native
  execution is active; its full five-mode visible-reader negative and both
  completed-mutation receipt regressions pass. The full result remains pending.
  11d compiler/server results do not qualify the new server edits.
- [x] Complete the authorized hz4 storage relocation before admitting its
  native retry. Preserve and verify all 78,193 entries in the 14,378,229,760-byte
  archive on trj, independently hash/fsync it, repeat the complete source
  manifest and idle/open/cwd/protection/Git checks, then cut over at 17:10:24
  UTC. Recover a measured 13,492 MiB and leave a synced owner1000:1000/mode0775
  restore stub. Preserve and verify all 17 operator receipts, including the
  user's authorization, exact command and UTC time. Keep active/protected
  caches and proof images; no NAS recovery is inferred.
- [ ] Recover emergency headroom on vmi1152480 without discarding data. Keep
  the original six-hour idle refusal. At zero available bytes the operator
  skill permits a shorter floor: the 5,924 MiB Cargo target passes the complete
  one-hour guard after 1.276 hours idle, with no Git/protection/open/cwd/mount
  or external-hardlink exceptions. Copy to trj and verify complete content and
  metadata before any fresh guard or authorized cutover. No RCH capacity,
  source, timeout or product gate changes are permitted.
- [x] Check the suspected live-worker gap against the real kernel: an
  unprivileged CLONE_FILES-unshared worker holds the exact generation inode,
  its leader has no matching descriptor and the leader still has VmSize.
- [x] Reproduce through the unchanged actual public SnapshotRetentionGuard
  test and image 90210e68. Ordinary reader: pass in 27.66 seconds. The same
  reader program b3592c84 in a live unshared worker: exit 101 in 1.117 seconds,
  with an admitted ReclaimedFileBytes at the original refusal assertion.
  Independent kernel evidence is captured before publishing readiness.
  This counterexample already exists in the pre-follow-up image; it is not
  proof that 09b362e0 introduced the external-worker gap.
- [x] Checkpoint repair 11d262da: pinned live-worker descriptor scans, shared
  mappings checked once, full dead-leader survivor inspection and no recursive
  group enumeration. Preserve current-thread strict preflight, own-worker
  no-open coverage, PID/task pinning, real retry and the shared inventory bound.
- [x] Extend the existing public visible-reader negative with real live FD
  and O_PATH workers. Require a distinct worker TID, a still-live leader,
  matching worker inode and no matching leader descriptor. Preserve every
  original mode, refusal, byte, accounting and release assertion.
- [x] Execute that complete five-mode negative on the actual repaired 11d
  integration image: pass in 21.864 seconds. Whole image SHA-256 is
  74eee95c3f030c24da9cc7e7747b3d49e3780945c074d9b8b2ca043f540c6a09;
  both new modes and reader_descriptor are present. The restart fixture,
  dead-leader and ordinary descriptor/mapping protections also pass. The
  complete 134-case run finishes 113 pass, 20 fail and one timeout in
  1,043.433 seconds, with 10,461 cases outside selection. This is not a green
  full-run claim. The 720-cycle case reaches its configured 240-second limit;
  the recorded 260.186 seconds includes process termination. Concurrent
  archival I/O is explicit, and no performance qualification is inferred.
- [x] Run all 63 original database cases on the same repaired images in one
  consistent coordinator role: UID/GID 65534, CapEff zero, readable ordinary
  controller, umask 002 and full host procfs. All 63 pass with one populated
  test per invocation and stable image hashes before/after. Preserve the first
  harness run's 28/35 result: its UID1000/0711 parent refused RDONLY directory
  opens. A direct kernel probe demonstrates that refusal and the corrected
  UID65534/0700 parent's successful open. No production policy or oracle changes.
  This separate role result does not replace the ordinary 113/20/1 run.
- [x] Formatting/diff checks pass. UBS retains four existing critical fixture
  patterns; all 15 added-line findings are reviewed, with no added critical
  or production findings and no suppression. All-targets passes in 237 seconds;
  its live witness has 3,896 matching present inputs and six historical JSON
  omissions, with no changed present inputs.
- [x] Strict workspace Clippy passes on 11d262da in 385.5 seconds.
- [x] Finish the separate production graph on this repair: normal hz2
  execution passes in 1,712.5 seconds. Its live source witness matches all
  3,896 present inputs, with the same six historical JSON omissions. Earlier
  namespace refusals and queue timeouts never reach Cargo and stay failures;
  exact owned-job recovery acknowledges remote exit 1 without replay.
- [x] Let the f0 native job finish: 112 pass, 21 fail and one original
  240-second timeout across 134 cases. Retain the three actual images,
  including clean library and integration/server collector receipts.
- [x] Reject the later canonical 0.45-second fresh build as stale: its
  failure locations match f0 rather than 11d262da despite transferred source
  hashes matching. Its 112/21/1 results do not execute the new worker cases.
  Preserve this recurrence under br-kp1in.75; do not infer cache repair from
  isolated validation or alter the original task's acceptance.
- [x] Execute all original namespace, public-guard, rotation, legacy and
  warning controls on the repaired source, including both new live-worker
  cases and original 16,641-directory/720-cycle workloads and deadlines.
  The 11d result is the explicit 113/20/1 result above; execution does not
  establish passing acceptance. The merged 9d rerun remains open separately.
- [x] Preserve the counterexample wrapper, unchanged input program identity,
  kernel witnesses and original failing output on trj, with full archive,
  member/metadata verification and fsync. This is a failing counterexample,
  not passing repair evidence; no source data is removed.
- [x] Finish the separate-checkout 11d hz4 native run, with collectors already
  running before test execution. Verify the actual integration image contains
  both new modes and capture the real positive/negative runtime results.
- [x] Preserve the 11d runtime evidence and original f0 terminal log on trj.
  Independently verify and fsync all three actual 11d image hashes and the
  1,002-member terminal/source/role archive. Retain the raced collector error,
  initial 28/35 harness result and corrected 63/0 result. These proof copies
  preserve their sources and claim zero reclaimed root bytes.
- [ ] Update the existing validation record, Beads and final commit/push after
  qualification. The broader br-5nr0m acceptance remains open; no performance
  win, whole-workspace runtime or release qualification is inferred.

## November 3 UTC: qualify the published opaque-reader retention change

- [x] Finish and commit the source-bound doctor and legacy-comparison repairs
  in `715440a9`; close only br-doctor-project-config-discovery-j0bh5 and
  br-kp1in.80 on their original acceptance evidence. The 147 native passes,
  five fresh cases and real-MCP repair/undo passes precede the change below.
- [x] Review all five files in published `159e9bb5`, including the complete
  namespace diff, and merge both histories without conflict at `775cac5c`.
  Run `br sync --import-only`; claim br-5nr0m without closing its broader work.
- [x] Record the contract change explicitly: qualified superseded snapshots
  may lose their pathname despite an opaque reader, while the reader retains
  its inode bytes. Generic and incident reclamation keep strict exclusion.
  Incomplete reader inventory never establishes physically reclaimed bytes.
- [x] Preserve the first compiler request's pre-Cargo namespace refusal,
  exit 103. Recover exact wrapper `rchw-e704bdb8-6657-498b-8b29-885afbd71376`:
  remote exit 1 acknowledged, without replay. Retry through normal RCH;
  do not alter admission, source isolation or worker capacity.
- [x] Check formatting on the merged source: pass.
- [x] Complete the pre-reporting merged-source all-targets compiler check:
  exit 0 in 3,990.2 seconds. Its direct live Cargo witness matches all
  3,902 tracked inputs. This precedes the reporting and fixture edits below.
- [x] Wire the DB's incomplete-reader flag through rotation summaries,
  completed-unlink errors and the shared default-warning renderer used by
  startup and automatic cleanup. Add actual opaque-reader byte preservation
  and tracing warning/ordinary-pass controls; no reader-admission change.
- [x] Preserve the first native result: 41 pass, 22 fail of 63 cases,
  4,645 outside selection. Three legacy cases pass; all seven public Linux
  cases execute, with six failures. Retain the actual DB library dd4dea95
  and integration cda0281f images. Thirteen failures retain strict census
  refusal; nine reject accidentally shared test fixtures.
- [x] Diagnose those fixtures with the same retained ELFs and assertions:
  three selected cases each fail with umask 002 and pass with 077.
  Actual retained roots are mode 0775 versus 0700. FIXTURE-CHANGE:
  explicitly make fixture roots 0700 and mailbox/owned-source files 0600;
  keep all file-0622, parent-0777, owner, alias and replacement refusals.
  Make the replacement itself private so the inode negative is independent
  of permission rejection. Production admission and deadlines are unchanged.
  Formatting passes; the fresh runtime request refuses before tests because
  hz3 has 13.9 GiB free and zero admitted slots. No pressure override.
- [x] Restore hz3 headroom through the authorized verified relocation
  process; preserve protected, active, Git-bearing or insufficiently idle trees.
  Move the 200-hour-idle HFDT target to trj `/bulk`, verify all 2,548 entries
  and the full archive, and fsync payload, manifests and directories before
  the authorized source cutover. Archive SHA-256 is dff2c633bb1249b8a35bf5463876e2afe5a991a62897ae8d008f50764fb9640d.
  Cutover at 10:41:24 UTC recovers a measured 13,005 MiB; preserve owner/mode
  and a synced restore pointer. Verify/fsync all 17 operator receipts too.
  Normal RCH admission restores three hz3 slots, without a policy override.
- [x] Resolve strict Clippy's three redundant visibility errors inside the
  private namespace module, keeping its effective crate-private exports.
  Replace the extra report boolean with a count rather than allowing a lint.
  Count-source Clippy, all-targets and production checks pass in 501.3,
  277.1 and 250.6 seconds. These precede the last test-directory helper.
- [x] Make historical test directories private at creation, independently
  of umask. Final-source server run executes all 68 selected cases: 61 pass,
  six fail and the original 720-cycle case times out at 240 seconds.
  Both new real-opaque-reader and default-warning controls pass. Preserve
  every failure and the actual 350,932,632-byte ELF, SHA-256 89d9fae6ddbf55f5090624cb783f01c4a0b5b85512c0cf01cf736fb71f4422bf.
  A complete same-ELF unprivileged replay with umask 002 is diagnostic only;
  it cannot erase the ordinary-worker failures or certify load-20 performance.
- [x] Review the final four-file UBS result: exit 1, four unchanged critical
  fixture patterns, 2,166 warnings and 783 informational findings. Review
  all 67 added-line findings; none is critical. Keep the three intentional
  failure panics and fixed Python argument literal; no scanner suppression.
- [x] Review all seven files in newly fetched maintainer follow-up 09b362e0.
  It adds logical-byte accounting, avoids redundant own-thread reader scans,
  and adds parked-window regressions. Preserve the earlier-source runs above.
- [x] Merge 09b362e0 with the local reporting and private-fixture repairs;
  retain one reporting vocabulary, the default warning and real reader test.
  Preserve both histories at f0a835cc and import Beads after merging.
  Resolve all three overlapping files manually. Keep upstream count and
  logical-byte accounting, including completed-but-unsynced operations;
  retain the local default warning, private fixtures and actual hidden-reader
  test, now checking both aggregate and per-kind logical bytes.
- [x] Finish the complete pre-follow-up unprivileged server replay using
  actual ELF 89d9fae6, umask 002 and unchanged 240-second per-test bounds:
  67 pass, one timeout of 68 cases. The original 720-cycle case passes in
  77.696 seconds in this role; the 16,641-directory case still times out.
  Preserve both this failed aggregate and the ordinary-worker 61/6/1 result.
- [x] Freeze the merged source and create an isolated verification checkout
  at `/data/projects/am-retention09-qualification-20261103`; all 3,902
  tracked regular inputs match. Preserve two pre-compiler source-ownership
  refusals on vmi1227854 and a queued-worker refusal on hz3, all exit 103.
  Do not override the unfinished broad source claim on vmi1227854.
  Merged-source Clippy starts on normally admitted hz3 from the isolated
  checkout. Its live witness matches all 3,896 present inputs; preserve
  the same six omitted historical performance JSON files and observer exit 2.
- [x] Review merged-source UBS: exit 1, the same four unchanged critical
  fixture patterns, 2,243 warnings and 783 informational findings. None of
  the 70 changed-line findings is critical; no suppression or weakened guard.
- [x] Check merged f0a835cc before the final corrupt-file fixture edit:
  strict workspace Clippy passes in 1,915.9 seconds, all-targets in 266.3,
  and the separate production library/binary check in 351.1 seconds.
  Live source witnesses have 3,896 matching present inputs, the same six
  omitted historical performance JSON files and zero changed present inputs.
- [x] Finish the private-fixture pre-follow-up DB run: 49 pass, 14 fail of
  63 in 80.629 seconds after a 3,408.333-second remote invocation. Six public
  cases pass; the restart case rejects its deliberately corrupt file's
  inherited mode 0664 before the intended full-verification assertion.
  The other 13 failures retain strict ambient-process reader refusals.
- [x] Diagnose that last fixture with identical retained image 90210e68:
  the complete restart case fails under umask 002 and passes under 077
  in one paired invocation, with unchanged image hashes and assertions.
  Commit c004204a makes only that corrupt fixture mode 0600. Its bytes,
  full-verification refusal, real SIGKILL/restart and deadlines stay intact.
  Formatting passes. UBS remains red on three unchanged fixture panic paths;
  the only added-line finding is the required setup unwrap.
- [x] Preserve a collector mistake: stdout redirection prematurely created
  each helper's exclusive receipt path. Both collectors copied the actual
  live images, then failed to write their final local receipts. Retain both
  tracebacks and independently recover/re-hash the retained c2c71a07 library
  and 90210e68 integration images. Do not call these intact original receipts.
  Later collectors use distinct stdout and helper-owned receipt paths.
- [x] Repeat all-targets, production lib/bins and strict workspace Clippy
  after the final one-line fixture edit in c004204a: pass in 40.2, 37.5 and
  34.9 seconds, respectively; formatting also passes. Preserve the first
  queued Clippy refusal and the observer attempts before Cargo started.
  These short reruns have no separate live-observer qualification claim.
- [ ] Execute every public Linux retention case on the merged source,
  including opaque FD/O_PATH/mapping byte preservation, visible-reader
  refusal, dead-leader threads, real permission refusal and interrupted cleanup.
- [ ] Execute strict namespace controls and the original legacy comparison;
  preserve environmental refusals rather than turning them into skips/passes.
- [ ] Exercise the existing server tests for the shared resident/historical
  keep window; the GH#373 keep-count repair already exists in this checkout.
- [ ] Retain actual source and executable witnesses, terminal results and
  any failures; update this checklist and br-5nr0m with the bounded outcome.
- [ ] Review and commit owned tracking changes; fetch again and preserve any
  new remote history before a normal push and compatibility-branch mirror.
- [ ] Continue original persistence, authority, performance and platform
  acceptance work. This selected run cannot close those tasks or qualify a release.

## November 2: restore client access and connect doctor discovery

- [x] Reproduce real client failures: six project configurations receive HTTP
  401 from the live MCP server. Preserve original files privately before repair.
- [x] Run native project-only setup for Cline, Cursor, Factory, Gemini,
  OpenCode and Windsurf with hooks/user-config updates disabled. All six
  configurations subsequently authenticate with HTTP 200 and green MCP health.
  The canonical token file remains byte-identical; files are mode 0600.
- [x] Preserve and manually repair the remaining three project entries:
  Codex TOML, legacy Codex JSON and Claude settings. Canonicalize the Codex
  source alias to remove OMP's duplicate. Requests using all three repaired
  entries return HTTP 200. `am doctor check` now reports healthy=true and
  token parity OK; lesser missing/unclassified/stdio warnings remain visible.
  This validates the configuration entries, not an application restart for
  each coding-agent product. The separate reservation-provenance drift stays
  open. Native setup's secret-ignore additions to `.gitignore` are reviewed.
- [x] Reproduce the repair blind spot on an authenticated private MCP server:
  the installed CLI finds one stale home token but misses the stale project
  token and wrong project URL. Its read-only OMP fallback is untouched.
  Baseline-only evidence is not a successful repair claim.
- [x] Claim br-doctor-project-config-discovery-j0bh5 and remove the accidental
  OMP-only filter from the existing shared mutation inventory. Preserve legacy
  home paths, deduplication and read-only OMP exclusions. Add two discriminating
  inventory tests; the original OMP test and all fixer assertions stay intact.
  Formatting and whitespace checks pass. No dependency or feature change.
- [x] Preserve the next real baseline failure: installed CLI detects
  `~/.claude.json` but applying its token repair refuses with exit 4 because
  that supported file is outside the default scopes. Reproduce the refusal
  again with the same installed executable; all six files remain byte-identical.
  Add only that explicit file to default scopes, plus a native control that
  HOME, `.claude.json.bak` and an unrelated sibling remain out of scope.
  This admits one documented config path, no broader home-directory authority.
  Real candidate repair/undo still must pass; the original red run is retained.
- [x] Finish the strict remote workspace all-targets check on vmi1156319.
  Its actual source snapshot matches all 3,902 tracked regular-file inputs,
  including the dirty patch, with zero omissions or changed present inputs.
  The separate release-box request refused because that id is not in the
  active RCH configuration; no worker configuration or admission policy changed.
  That first snapshot predates the file-specific scope repair; repeat the
  all-targets gate on the final source rather than crediting it to later edits.
  Final doctor-source check passes in 73 seconds. After the two equivalent
  assertion edits below, all-targets passes again in 38 seconds.
- [x] Complete the production lib/bins check: 645.7 seconds initially,
  then 24.3 seconds after the test-only assertion edits. Preserve the actual
  source witnesses and distinguish compiler checks from executed tests.
- [x] Preserve strict Clippy's exit 101 on two pre-existing retention-test
  `assert_is_empty` lints. Acquire conflict-free lease 422 and replace only
  those two conditions with equivalent empty-vector equality/inequality
  assertions. Formatting passes; strict workspace Clippy then passes in
  206.9 seconds. No assertion, timeout or scanner exemption is removed.
  UBS reports three unchanged intentional fixture panic paths in this test
  file; keep its red result and review, with no UBS-green claim for that file.
- [x] Finish native inventory/token/URL/alias fixer tests through normal RCH
  on vmi1156319, with the verified Nextest runner and private root-owned
  HOME/TMP/XDG paths: 64 pass, zero fail, 3,144 outside the selected scope.
  Both actual runner processes and a late witness match all 3,902 frozen
  tracked inputs. Cold build takes 7,545 seconds; test execution takes 6.736
  seconds. The observer did not preserve its executable copy; retain that
  failure. Recover the original 243,772,592-byte image by stable open-file
  identity and full SHA-256 9ec7794d3f0227748033ddb665fc53678150312691f297b01de1aee9cdcbcff3.
  All three new cases pass again individually in a fresh private root.
  Preserve the image, actual source, original observer failure and results in
  a 12-member archive on trj; every member and the archive are verified/fsynced.
  This pre-merge native scope does not qualify later namespace changes.
- [x] Build and retain the new CLI, then execute the private real-MCP test:
  two token findings and one URL finding, dry-run byte neutrality, intended
  field-only repairs, real authenticated requests, idempotence, exact undo,
  renewed 401 rejection and untouched read-only fallback/unrelated entries.
  Portable production CLI bf35a95b (290,327,568 bytes) builds successfully
  in 7,134 seconds, with all 3,902 live source inputs matching. Primary and
  fresh-fixture solo runs each pass all ten actual CLI calls, including the
  original secret-artifact refusal before protection, intended field-only
  changes, real HTTP 401 to 200, zero-action idempotence, reverse exact undo
  of all six files and renewed 401. Read-only fallback/unrelated entries stay
  untouched. Each executing CLI image matches bf35a95b; both private servers
  exit 0 and both stderr logs are empty. Fresh solo is not independent review.
  Preserve the executable, unchanged harness, both runs, per-call image
  digests, original red runs and source/gate witnesses in trj
  `doctor-undo-real-mcp-0630/`: 293,488,640 bytes, 69 verified members,
  SHA-256 99b23e81ccb1f7009188cf0de830819b77962e379630cbbff8bba17c79476d8b.
  Archive, manifest, receipt and directories are verified and fsynced.
- [x] Retain the workspace-built standalone CLI 0c5e26bd (309,760,848 bytes)
  by stable source-file hash and byte-verified/fsynced coordinator copy. This
  is the merged workspace test feature graph, not the separate production
  artifact. Its source witness has the six historical JSON omissions below.
- [x] Preserve its first real-MCP failure: the fixture's `.gitignore` does not
  protect adjacent credential artifacts, so the unchanged safety gate refuses
  exit 3. Preserve original harness 84e9f581 and failed fixtures. Correct only
  the scratch fixture to native setup's exact five patterns per client file;
  ten real-Git positive and three negative controls discriminate protection.
  The revised harness first requires exit 3 with all six config bytes and
  `.gitignore` unchanged. Original positive repair/undo assertions stay intact.
- [x] Observe actual token/URL repair, HTTP authentication and idempotence on
  that CLI. Preserve its next real failure: URL undo refuses the repaired
  project config as outside default write scopes. This is not an end-to-end
  pass, and the doctor Bead stays open.
- [x] Qualify the resulting undo repair: add only mutable project-config
  files to shared fix/undo defaults, reject canonical paths escaping the
  project, and add positive/file-bounded/static-parent-symlink controls.
  Formatting passes; UBS exit 0 has zero critical findings. All 22 added-line
  findings are reviewed test expectations, filesystem fixtures and path strings;
  no scanner suppression. Current-source all-targets passes on hz4 at
  05:03 UTC (3,022.6 seconds); its live snapshot matches all 3,902 inputs.
  Production lib/bins first refuses admission for a missing capability;
  after normal capability refresh it starts on hz4 at 05:38 UTC, again with
  3,902 matching inputs and no omissions, and passes in 583.2 seconds.
  Final all-targets including the legacy comparison test also passes;
  the live observer misses this short run and exits 1. Its later 3,902-match
  read is explicitly post-terminal, not live proof. Final strict Clippy
  passes in 538.8 seconds with all 3,902 live inputs matching. Formatting
  passes locally and read-only on hz4 over SSH; its RCH non-compilation job
  refuses an oversized default request, with no policy change or SSH compile.
  The focused native request refuses
  before execution because a healthy peer job occupies hz3; no override.
  Final selected native run passes 147/147 in 32.304 seconds, with 8,080
  tests outside the selection. Both new scope cases, original home and undo
  controls, both help snapshots and the real crypto/Beads cases execute.
  All five fresh cases pass individually on retained actual CLI/DB test images,
  within unchanged 240-second bounds. The real-MCP primary and fresh-solo
  flows pass on production CLI bf35a95b. Broader authority tasks stay open.
- [x] Close this bug on those source-bound native/CLI results: closed
  br-doctor-project-config-discovery-j0bh5 at 07:38:54 UTC. Keep the existing
  authority tasks br-3cqr4/br-x2jf5 and wider release work open. Fresh solo
  verification is not independent review or native Windows qualification.

## November 3 UTC: finish headroom recovery and integrate retained work

- [x] Preserve the full-workspace native retry's admission refusal at
  23:52:37 UTC: exit 103, zero tests. Normal hz3 telemetry reports critical
  I/O pressure and low headroom; do not override admission or pause peer builds.
- [x] Reject the 28 GiB cold fln-wf-gate tree because it is protected by
  `.sbh-protect`. Select the unprotected MossyTrout tree only after full
  ancestor/tree marker, idle, mount, root-privileged open-file and cwd guards.
  It has been idle over 54 hours and has no external hard links.
- [x] Under the user's explicit relocation authorization, preserve all 124,227
  entries in a 24,118,026,240-byte archive on trj `/bulk`. Stream digest,
  entry count and source/archive per-file metadata/content manifests match.
  Archive SHA-256 c5192654fd4e12ba63103bf103b1a7d4de33b1ff1364deaffc57aa462c06a8c9.
  Fsync archive, manifests and directory before the fresh full source guard.
- [x] Execute the provided relocation cutover at 00:43:25 UTC. It removes
  only the verified source payload and leaves a verified, synced restore
  pointer. Net root space rises 23,222 MiB during cutover, from 15,046 to
  38,268 MiB. Do not credit later external space jumps. Exact authorization,
  command, time and ten byte-verified/fsynced receipts are retained on trj in
  `hz3-mossy-root-2357/` under the existing relocation root.
- [x] Refresh normal RCH telemetry: initial stale critical state remains a
  refusal boundary; the next normal probe clears it to warning, with over
  53 GiB free. No pressure, cache, source, slot or admission policy changes.
- [x] Recheck headroom as the full-workspace build progresses: root free
  space falls to about 21 GiB. Select CreamHill, vt-891549b4b and vt-29c6eb0cf
  (about 12,908 MiB combined) only after complete protection-marker, 64–70-hour
  idle, root-device, no-Git-metadata, no-external-hardlink, nested-mount,
  root-privileged open-file and process-cwd checks pass for all three.
- [x] Complete the provided lossless copy/stream-hash/entry-count/per-file
  manifest checks into trj `hz3-cold-targets-root-0327/`. Existing explicit
  move authorization is recorded. All three are VERIFIED by 04:01 UTC;
  remaining vt originals stay until durability and fresh cutover guards pass.
- [x] Complete CreamHill independently while the other copies continue:
  5,069,045,760 bytes, 48,595 entries, SHA-256 cd2c906aa44c78f18266e265fe9130943290bfb32e863a1f190b4891f14b06fd.
  Verify all manifests and fsync before a fresh marker/metadata/hardlink/mount/
  open-file/cwd guard. Execute the provided cutover at 03:43:57 UTC, recording
  exact authorization/command/time. Root free rises 19,307 to 24,184 MiB:
  credit only the measured net 4,877 MiB. The sole remaining child is a verified,
  synced restore pointer; its root owner and mode match the archived original.
- [x] Fsync all verified archives/manifests, repeat full guards and prove no
  change since copy start before any cutover. Preserve synced restore pointers,
  exact authorization/command/time and measured net root headroom afterward.
  Remaining vt cutovers start 04:11:33 UTC; net root increases 7,982 MiB
  (18,891 to 26,873 MiB). Both pointers, owners and modes match and are synced.
  Three-directory total is 12,859 MiB measured net. All 36 operator-receipt
  archive members verify on trj; archive/manifest/receipt/directory are fsynced.
- [x] Finish the doctor native/CLI qualification. Shared commits abfc61fa
  (legacy comparison), 3ccc84b1 (two help fields), cce5fa19 (tracker) and
  d73e4bb5 (plan) record already-reviewed work; this agent did not invoke
  those commits. All 647 code/lock/toolchain/help inputs still match the
  actual native snapshot. Preserve them; commit only the owned final update.
- [x] Commit the doctor repair as 58025b24 after its compiler/lint/format gates,
  explicitly retaining pending native/CLI acceptance and the open Bead.
- [x] Preserve both histories with normal merge 6fef128c, integrating remote
  f4b9da4f/534a8dd6 (procfs retry/pinned-inventory fixes). Review the entire
  incoming code/test/doc diff and import Beads after the merge. Later peer
  commit 13afff25 changes only tracker metadata; compiled inputs are identical.
  Never force-push or flatten. The old native pass is not incoming-code proof.
- [ ] Run required checks and full-workspace native validation on the merged
  source. Normal RCH admits the hz3 workspace native run at 00:58 UTC; it is
  compiled in 170 minutes and starts 20,762 tests across 27 binaries, with 54
  existing skips. Run aee8ebe1-7a51-4397-9f4a-b93837aaf913 reaches its unchanged
  four-hour limit: 10,200 PASS and 42 FAIL terminal records survive; remaining
  tests are unexecuted or unreported. No full-suite pass. Exact wrapper recovery
  acknowledges exit 137 and releases ownership without replay. Its source
  witness matches 3,896 present inputs but misses
  six historical perf JSONs: the complete-input observer exits 2, retained red.
  The first merged compiler request refuses 103 before admission. Retry only
  after the CLI worker advances to current source; no admission overrides.
- [x] Preserve and resolve the full run's six real-`br` fixture failures:
  this invocation's private PATH omitted `/home/ubuntu/.local/bin`, where the
  worker's actual installed `br` resides. Keep the current run's environment
  fixed and its failures visible; include that real directory in the next
  source-bound invocation. No mocks, assertion changes or test exclusions.
  All six original cases pass individually in fresh private fixtures with
  actual br 0.7.6 on PATH and retained CLI test image 9ec7794d. The other 136
  CLI Rust source files match its producer; the newer doctor module is
  explicitly outside this claim. Preserve the original full-run failures.
- [x] GOLDEN-CHANGE: review the stale vacuum description in both doctor-help
  goldens against actual CLI 0c5e26bd and immutable source change 3194449a.
  `acquire_doctor_vacuum_locks` refuses live-owner overrides; the handler takes
  locks, retains a content-verified backup before compaction, and verifies the
  result. Six existing native vacuum controls pass in the running suite.
  Semantic diff is exactly one line per fixture: "Reclaim orphaned pages
  INSIDE the live database" becomes "Compact the mailbox database offline
  (verified backup, VACUUM + ANALYZE)". Manually update only those two expected
  descriptions under leases 441/442. No bulk regeneration, normalization
  change, removed assertion, bypassed owner guard or new product behavior.
  Preserve the original snapshot failure; full native recheck remains pending.
- [x] Complete the original real-age matrix with required dependencies:
  18/18 pass, zero skips, on retained production CLI 50658da2. Thirteen cases
  use ordinary real age; five bounded publication probes delay return only
  after real age completes, including two actual CLI contenders with one
  complete winner. Age/keygen 1.2.1 binaries are byte-verified copies from trj.
  This is existing crypto-path requalification on a pre-undo artifact, not new
  implementation, latest undo qualification, Windows proof or release clearance.
- [x] Retain actual Core test image 6b96e829 (19,935,760 bytes), verify/fsync
  worker and coordinator copies, and verify current setup/embedded installer/
  lock/toolchain match its producer source. The old run passed 235 setup tests.
  Execute the original ignored group-readable case separately: one test passes
  in 1.379 seconds within the unchanged 240-second bound, UID 1000, zero
  effective capabilities; sudo only prepares the foreign-owned fixture.
  Keep br-3cqr4 open pending its remaining compiler/lint/platform acceptance.
  Fresh solo execution of the same image again passes all 235 default setup
  tests in 16.94 seconds, with the original opt-in still ignored in that
  default invocation. All 80 Core/installer/manifest/lock/toolchain inputs
  match its producer. Required current compiler/lint/format gates now pass;
  native Windows remains unverified, with no independent-review claim.
- [ ] Resolve and retain the ordinary UID-1000 host's reclamation boundary:
  same-UID systemd PID 2212 has unreadable `/proc/2212/fd/0`; strict reader
  census correctly refuses. Do not whitelist names, loosen inspection, or
  turn these failures into a general unprivileged Linux pass. br-cdsge stays open.
- [x] Investigate the legacy-upgrade preservation test's Int/BigInt comparison
  and independently verify complete payloads before deciding on a gate fix.
  Read-only analysis of the complete first failed assertion finds exactly
  25 canonical Int wrappers versus native BigInt wrappers; widening those
  wrappers alone makes both complete debug payloads identical, preserving all
  quoted text. That read-only analysis alone did not execute later phases;
  the subsequent original native test and fresh repeat now do, and pass.
  GATE-CHANGE for br-kp1in.80: primary adapter source independently confirms
  canonical SQLite narrows i32-range INTEGERs while FrankenSQLite returns
  BigInt. Widen only Int to BigInt in the existing test comparison helper;
  preserve every original migration, payload, timestamp, ID-floor, recipient
  and integrity assertion. Add a real two-engine file probe for raw wrapper
  differences, signed integer boundaries and five unequal/type negatives
  (8, text '7', float 7.0, blob 0x37, NULL versus integer 7). Native acceptance
  publishes how the original veto and all negatives resolve; no runtime
  repair or performance win is claimed. Formatting passes. UBS remains red
  on two unchanged intentional test panic guards; no suppression or removal.
  Preserve the original failure and the capacity-replay p95/p99 loss; no
  threshold changes or data-preservation claim from a type-width hypothesis.
  Actual verdict: the one previously vetoed legacy test passes every original
  migration/write/timestamp/recipient/integrity assertion in 6.330 seconds,
  and passes again fresh on DB test image b4f9f486. The new real-engine
  integer-bound/type test also passes twice. Close br-kp1in.80 at 07:39:11 UTC.
  This is test maintenance, with no runtime corruption-repair or performance win.
- [x] Finish the undo-scope standalone CLI build and its real-MCP/fresh-solo
  acceptance. The pre-undo production CLI 50658da2 reproduces undo exit 3;
  the new build's live witness matches all 3,902 frozen inputs at 9513cc33-dirty.
  New bf35a95b passes both original real-MCP runs. Source-bound selected native
  checks are admitted on hz3 after the peer job ends: doctor/undo/new scope
  controls, Beads, help snapshots, crypto and canonical legacy upgrade. The
  two-job request uses two configured slots without an override. Both live
  source witnesses match 3,896 present files; six historical perf JSONs remain
  missing and the complete-input observer stays red. Retain actual CLI and
  integration test images from Nextest PID 3004111: all 147 selected tests pass.
  Nineteen of the original 42 failures are rechecked and now pass; the other
  23 are not selected. The original full run remains red/partial. The first
  comparison script mistakenly included old passes; preserve that output and
  the corrected FAIL/TIMEOUT-filtered count before citing the result.
- [x] Preserve all four actual native ELF images, early/runtime source
  witnesses, 147 results, five fresh cases and original failures on trj
  `native-final-147-0730/`: 897,751,040 bytes, all 45 members verified,
  SHA-256 7d0975c36ad5327aeb94f5da1ff1e73e6b397cdbd1de5e1f491f5955b471381b.
  Archive, manifest, receipt and directories are fsynced. Two observer PIDs
  disappear before a later proc lookup; both still bind the same complete
  verified af132457 integration image. Preserve those raw observer errors.
- [x] Execute the original opt-in setup permission case within its unchanged
  240-second bound after full-workspace native processes finish: one actual
  test passes, as detailed above. Recheck malformed Codex TOML with actual
  retained production CLI 50658da2: exit 1, refusal, unchanged bytes, neither
  credential in either output stream. Preserve the initial project-path
  fixture failure; correct the fixture to setup's supported HOME path.
  Comments 3912/3913 record the evidence and unmet acceptance. Keep br-3cqr4
  open. No Windows-native, release or soak qualification is implied here.
- [x] Preserve the exact pre-undo production/Core/age executables, actual
  source witnesses, crypto/setup results and original full-run failures in
  trj `real-crypto-core-0542/`: archive SHA-256
  2a4b09aea8f8270082eca9f42e58a8904db4ac507f0bc9bfc211447f3a567b0a,
  322,007,040 bytes, all 24 members verified. Archive, manifest, durable
  receipt and containing directories are fsynced. Real operator configs and
  their private backups are excluded. This older archive remains scoped to
  its pre-undo artifact; the two newer archives preserve the qualified repair.
- [ ] Continue the remaining P0 authority/platform and P1 persistence work
  against its original acceptance before lower-priority read-routing expansion.
  Windows is online in Tailscale but its configured SSH port times out.
  Existing reconciler candidate patches remain unapplied: refresh and verify
  their current-base inputs before adoption, retaining the original orphan
  counts, 300-second probe, BusySnapshot and release holds.

## November 2: populated CLI reads and coordinator headroom

- [x] Claim br-kp1in.18. Bind the retained real CLI to its SHA-256
  c2d7351c0e9581d3 and the already-qualified source manifest 399faa76.
  Current robot commands, CLI library, DB queries and reservation tools are
  byte-identical to those captured build inputs. No new implementation credit.
- [x] Create a private authenticated daemon and send 10,000 messages through
  the actual MCP transport and runtime engine, using two senders. Confirm
  10,002 stored messages, recipients and archive files (two automatic messages).
- [x] Run search, robot health and doctor health three times with the daemon
  running; all finish below the predeclared 10-second ceiling, without LockBusy.
  Retain the real post-load red health detail rather than declaring it green.
- [x] Stop only this private daemon and run all three direct fallbacks; all
  succeed. Search returns actual scored fixture hits without a daemon.
- [x] Restart the same populated mailbox in a fresh solo verification pass.
  Check three live searches for 20 distinct positive-score matching hits and
  a missing-term negative control; verify daemon route and no CLI database
  open using strace. Both health commands complete; robot health is green.
- [x] Finish the recipient-kind negative control and fresh stopped fallbacks.
  The kind-filtered branch explicitly bypasses the daemon in current source;
  `--kind bcc` returns the correct empty result in 28.9699 seconds. The fresh
  stopped search/robot-health/doctor-health calls pass in 1.83/1.75/1.45 seconds.
- [x] Prepare the bounded empty-kind pagination repair in the detached,
  isolated checkout /data/projects/am-kind-read-20261102. Preserve the first
  backend call and diagnostics; stop later pages only if the real recipient
  table proves the kind absent. Add absent-kind, later-page and backend-error
  controls; leave the original cursor/paging tests intact. Formatting passes.
- [x] UBS scans the changed CLI file: exit 0, no critical findings, 2,725
  warnings and 911 informational findings. All 20 findings on added lines
  concern test setup/assertions. No suppression or scanner-policy edit.
- [x] Re-execute the five original archive-inventory cache controls with the
  retained real tools ELF in a fresh private root. All five pass. The health
  test asserts one scan for five calls, then a second scan and an error verdict
  after an epoch/message advance ahead of SQLite. This qualifies existing
  cache behavior, with no new implementation or independent-review credit.
- [x] Promote the exact reviewed patch into the canonical checkout; source
  SHA-256 af582885189e1d87 is unchanged. Another actor committed it as
  1af19a7c while validation was running; this is not a completed qualification.
- [x] Execute the five collector tests: 5/5 pass, 3,200 skipped; the same
  five pass in a fresh private-root solo repeat. Retain the actual executing
  CLI test ELF c8df80458aa3a6df. These backend-runner controls use real recipient
  tables but are R3 search evidence under the canonical rubric: their backend
  pages are fabricated to force branches. Correct the earlier R2 label;
  the real CLI comparison remains required.
  Original live/post source inputs match (3,764 present, six historical JSONs
  omitted). The final typed source receipt fails on 139 peer-added file/directory
  deltas; preserve that red barrier and separate recovered remote exit 0.
- [x] Complete both mandatory workspace compiler checks: production lib/bins
  and all-targets pass remotely. All-targets finishes in 11m12s on hz3 after
  relocation and fresh normal telemetry. Its live/post source audit includes
  the 107 peer-added preservation files: 3,871 present inputs match, with six
  omitted historical perf JSONs explicit. The dependency proc-macro-error2
  emits a future-Rust compatibility warning; no warning is hidden.
  Preserve admission refusals and optional source-receipt containment failures;
  use normal strict RCH with actual live Cargo source witnesses. All-targets
  fails with ENOSPC, exit 101; exact-wrapper recovery acknowledges that failure.
  Retry with debug/incremental artifacts disabled, without changing feature
  coverage, assertions or lint policy. Production lib/bins graph passes;
  its post-source audit matches 3,764 present inputs, with six omitted
  historical perf JSONs explicit. The low-debug retry is canceled through RCH
  when worker root falls below 2 GiB: local wrapper exits 143, exact recovery
  reports remote exit 101. Its terminal compiler diagnostic was not captured;
  do not invent one. First recovery fails releasing the source lock; after
  externally restored headroom a second recovery acknowledges the terminal
  result. Resume gates only with usable worker headroom. Real CLI build remains
  pending; no admission, source, pressure or lint gate is weakened.
- [x] Finish strict workspace all-targets Clippy with -D warnings: exit 0
  remotely, 10m18s. Live/post source audit matches all 3,871 present frozen
  inputs, with the same six historical JSON omissions. No source edit or
  suppression is needed. The dependency's future-Rust warning remains explicit.
- [x] Build the real repaired CLI through normal RCH: 199m28s, executable
  SHA-256 4b7b100560f93a15. All 3,770 frozen source inputs match live/post.
- [x] Complete the declared 24-call old/new comparison against the same
  populated mailbox in one invocation, including nonempty-kind and malformed
  cursor controls. Report any self-speedup as maintenance, not an incumbent win.
  Two attempts remain failed: guidance equality, then equal-count facet order.
  Preserve both original checkers and raw failures. Existing source already
  specifies kind-specific guidance and sorts facets only by count. Six unchanged
  baseline calls produce both tied facet orders; 13 checker controls accept only
  those documented differences and reject semantic drift. The complete pair
  subsequently passes with the retained rebuild. The first returned executable became unavailable
  locally and at its recorded remote pool path; its cause is unestablished.
  A normal hz3 rebuild is now admitted after a queue-timeout refusal, with 3,871
  present inputs matching and six historical JSON omissions explicit. Retain the
  returned executable outside the build target and sync it to trj immediately.
- [x] Start copy-only executable retention and bounded continuation under
  /tmp/am-reservation-read-20261102/. It waits for a successful returned build,
  runs the 13 checker controls, declared 24-call pair and fresh six-call solo,
  then preserves the executable and all raw results on trj, including failures.
  It makes no tracker closure, publication or release decision automatically.
- [x] Observe the continuation's terminal pass: all 13 checker controls,
  24 old/new calls and six fresh solo calls pass. Empty-kind old calls take
  31.07/31.68/31.71 seconds versus 1.98/1.93/2.07 seconds after repair.
  Matching-kind, daemon-route and invalid-cursor controls pass. This is
  maintenance against our own prior binary, not an incumbent campaign win.
  Rebuilt ELF f7637a6ecc100b93 is retained and synced to trj; evidence archive
  7746ffbb95a0aaa1 verifies all 72 files and is fsynced. Post-build source
  matches 3,871 present frozen inputs; six historical JSON omissions remain
  explicit. This older source excludes subsequent retention and doctor edits.
- [x] Fresh solo recheck: six real new-CLI calls pass, including matching and
  absent recipient kinds, daemon routing, rejected cursor and stopped fallbacks.
  Absent-kind calls take 1.877/1.879 seconds; no independent-review credit.
  Source was committed by another actor as 1af19a7c before qualification.
  Keep br-kp1in.18 in progress: its broader routing acceptance remains incomplete.
- [ ] Finish recipient-kind daemon routing within the same original task.
  Source review confirms `robot_commands.rs` explicitly bypasses the server
  for `--kind`, while the MCP search tool and typed `SearchQuery` have no
  recipient-kind field. Add real server-side filtering; removing the CLI
  condition alone would silently lose the filter. Preserve project authority,
  relevance, pagination and cursor validation, including matches on later pages.
- [ ] Route the remaining empty-query branch without losing its existing
  CLI guidance: the server's compatibility response is intentionally empty
  and has no guidance. Keep stopped-daemon direct reads correct.
- [ ] Qualify the complete routing change on the original 10k-message real
  daemon fixture with mixed recipient kinds, matching later-page results,
  absent-kind and invalid-cursor negatives, and stopped-daemon fallbacks.
  Preserve the original archive-walk counter controls and timing bounds.
  No routing closure from the earlier absent-kind shortcut or these source reads.
- [ ] Execute full workspace native tests. The earlier Nextest invocation was
  canceled before admission and ran no tests; compiler passes are not test passes.
  A fresh normal hz3 Nextest invocation was submitted at 22:25 UTC with private
  HOME/TMP/XDG roots and application credentials unset. It refused before
  admission at 22:29:51 UTC (exit 103, queue_timeout): zero tests ran. Retry only
  after worker capacity changes. Other probed workers lacked sufficient root
  headroom or the runner; trj is healthy storage but not a configured RCH worker.
- [x] Diagnose the live installed mailbox write breaker independently: stock
  SQLite confirms pages 135/136 never used; the responsive owner is an unmanaged
  am 0.3.38 TUI in FrankenTerm pane 42. Preserve failed reservation renewals.
- [x] Gracefully stop that exact foreground owner through the native mux input
  API and documented two-press Ctrl-C confirmation. No process kill, takeover,
  owner override, mux restart or deployment. Doctor drain reports safe_to_mutate.
- [x] Preserve the complete quiesced mailbox on trj before mutation: archive
  SHA-256 a924bd4b77d2cd19, 78,254,080 bytes, all 3,657 file hashes verified and
  synced at live-quiesced-2206/ beneath the existing relocation root.
- [x] Rehearse actual installed doctor repair on live and quiesced copies;
  all 22 table row counts/content hashes survive. Preserve the generic live
  repair failure at its backup-integrity guard; it did not complete repair.
- [x] Preview and rehearse the dedicated am doctor vacuum operation on an
  untouched quiesced copy, then apply with no live owner. Stock integrity and
  foreign-key checks pass; all 22 tables exactly match the quiesced snapshot.
  Two physical pages are reclaimed. Restart the unchanged installed executable
  in the original shell/pane. The previously blocked reservation write succeeds
  (leases 402/403, no conflicts); this is operator recovery, not an upstream fix.
- [x] Finish post-restart owner/config/health checks: new live owner 2886325;
  authenticated MCP health reports green; actual authenticated MCP renewal
  succeeds for both leases. Subsequent stock integrity and foreign keys pass.
  Preserve the separate CLI health finding: FrankenSearch reservation 346 has
  thread-provenance drift; the native per-FM detector plans zero automatic fixes.
  Attach incident evidence as br-htobc comment 3902 and CLI limits as br-kp1in.18
  comment 3903. Keep that
  upstream corruption bead open: neither SIGKILL causality nor an engine fix
  has been established by this recovery.
- [x] Measure current coordinator writers: a peer FrankenSearch build writes
  197 MiB in 45 seconds. Preserve that useful work; do not call it the unique
  cause of root growth. Root has approximately 15 GiB free before relocation.
- [x] Recheck the idle 13,867 MiB target under mistycondor-53bnp/work with the
  provided dry-run guard. Scan all marker depths, ancestors and mounts; no
  protection, nested mount, open file or cwd is present. Newest entry is over
  six hours old. This is preservation, not permission to discard cache work.
- [x] Record the operator's exact move authorization. The provided archive
  helpers reject hidden basenames; atomically rename only this idle target to
  am-read-cold-target-20261102-1645 with no-clobber and leave an original-path
  restore pointer. The complete source bytes remain until verified cutover.
- [x] Finish copying to trj:/bulk/worker-relocated-20261102-am-reservation-read/
  ts1-cold-target-1645/. Require stream hash, entry count and per-file metadata/
  content agreement; sync archive, manifests and authorization before cutover.
- [x] Repeat full marker, activity, modification and mount checks. Run the
  provided cutover helper under the existing authorization; preserve exact
  command/time, restore pointers and before/after disk headroom on both hosts.
  Coordinator cutover completes 17:27:55 UTC: 13,867 MiB allocated, net
  13,839 MiB headroom. The separate external space jump receives no credit.
- [x] Preserve two idle hz3 scratch trees with all three archive checks and
  authorized cutover. Correct the volume-selection mistake: /tmp is tmpfs;
  14,282 MiB freed there provides no root-disk gain. Keep that result explicit.
- [x] Finish the verified archive of hz3's actual root-volume vt-4bde2b1b4
  tree to hz3-vt-root-1816/ on trj. Its 36,150 MiB allocation has no outside
  hardlinks. Require all three checks, sync, and fresh full activity/protection
  guards before cutover; measure actual root headroom and retain safe restore
  pointers. Do not move protected or recently active peers' trees.
  All three checks pass: 20,295 entries, 38,419,292,160 bytes, SHA-256
  9b0f3b0a7b6c1fb912d5f1332a8d29e4643f7413025969497e0853c73200a77d.
  Sync archive/manifests/authorization, then two full guards pass. DCG rejects
  an assembled SCP argument list before execution; literal commands succeed
  without an override. Authorized cutover at 19:25:49 UTC raises actual root
  free space from 2,721 to 38,871 MiB. Preserve exact authorization, command,
  execution interval, logs and safe restore pointers beside the archive.
- [x] Resume required checks through normal RCH admission. The first hz3
  attempt after cutover is still refused for critical disk-pressure telemetry;
  preserve exit 103 and refresh normal worker health, without a gate override.
  Fresh telemetry reports warning-level pressure and admits the check; the
  all-targets check then passes. Attribute no external headroom changes to us.
- [x] Preserve the quiescent 10,002-message mailbox for real-path replay,
  without removing its original. All three copy checks pass at 20:00:30 UTC:
  134,370 entries, 2,120,519,680 bytes, SHA-256
  517d7472e09e0f9443bcf02a7262c54c0b2bf390b52c5dbe7d1ae70dc2d4fd82.
  Archive/manifests, replay harnesses and the old CLI c2d7351c0e9581d3 are
  verified and synced in kind-read-fixture-1955/ on trj. Native and compiler
  receipts are also hash-verified and synced in kind-read-20261102/.
- [ ] Preserve the real CLI comparison, new executable and fresh solo receipts
  durably; audit the work, update Beads honestly and publish the reviewed
  changes with original failures intact.

## November 2: qualify reservation reads during writer contention (br-list-write-lock-kx4vn)

- [x] Recheck governing document identities, current ownership and ready Beads.
  Contact-stat CPU and release publication issues retain their outstanding
  production/platform probes. Do not close them from source inspection.
- [x] Claim the reservation-listing Bead and reserve its exact source files.
  The implementation and native regressions already landed in 0a8b0633;
  preserve that authorship and give this block qualification credit only.
- [x] Review the original read/write separation, writable/query-only pool
  regression, independent writer, committed-state freshness, durable-family
  byte controls and queued-release stage assertion without weakening them.
- [x] Run both original native regressions through strict, normal RCH on
  the current source/dependency graph; retain the source and executable
  identities, terminal outcomes and any failures.
  Three named real-engine probes pass, with 4,715 tests skipped. Actual DB/tool
  executable hashes bc67135d7ccd5524 / 12e05b16a1cff914 are retained. Preserve
  the original cache-only source-content barrier failure; exact RCH recovery
  acknowledges exit 0 without issuing a successful typed source receipt.
- [x] No implementation failure is demonstrated and no reservation code is
  changed. Give already-landed 0a8b0633 qualification credit only.
- [x] Re-execute original acceptance in a fresh solo verification pass.
  Explicitly distinguish that pass from independent review.
  The same three cases pass again in private roots, not six independent cases.
- [x] Complete the required real-work audit and honesty inventory. Record
  test counts, remaining limits and the original negative controls.
  Durable evidence: trj:/bulk/worker-relocated-20261102-am-reservation-read/
  reservation-qualified-1750/; 19 files hash-verified and synced. Present source
  inputs match (3,764), with six omitted historical perf JSONs explicit.
- [x] Cite terminal evidence and close only br-list-write-lock-kx4vn against
  its original held-writer/freshness/release-stage acceptance. No independent,
  platform, release or whole-project qualification is claimed.
- [ ] Flush Beads, review the diff and publish the qualification record.
- [ ] Verify remote branch identities and release these file reservations.

## November 2: qualify landed retention policy (br-refused-pass-window-policy-q7mxt)

- [x] Read the original refusal/window bug and already-landed 7e9d2466 policy.
  No retention source or acceptance condition changes in this work block.
- [x] Reuse the exact real server test executable, SHA-256 047478ddeedbb230,
  retained during the source-bound native run; all three retention files
  match HEAD and the full live/post 3,770-input manifest 399faa76.
- [x] Run the real older-first/window-capacity controls, publication after
  refusals and 24 real producer iterations. Four tests pass. Invoke the
  publication child directly and reject any unasserted-publication early
  return; none appears. Preserve stdout/stderr and exact filtered counts.
- [x] Fresh solo re-execution passes all four again in new private roots.
  The 24 scheduled iterations take 32.62/32.72 seconds, not a 24-hour soak.
  Complete the bounded honesty inventory. No independent review is claimed.
- [x] Hash-verify and sync the exact executable, source witnesses and both
  logs to trj:/bulk/worker-relocated-20261102-am-sri/retention-qualified-1232/.
  Cite the evidence and close only this claimed Bead. No new implementation,
  performance win, broader platform, release or whole-project claim follows.
- [x] Publish this closure in 9cc92c90 after preserving the existing graph.
  Numeric repair's separate acceptance is recorded below.

## November 2: finish Mail UI asset integrity (br-kp1in.43)

- [x] Verify governing AGENTS.md and README.md are byte-identical to the full
  review; recheck suite rules, current Beads readiness and file ownership.
- [x] Keep existing engine, platform and release blockers open. Choose the
  remaining concrete Tailwind integrity gap; claim br-kp1in.43 as BlueSpring.
- [x] Reuse the already shipped Tailwind 3.4.17 bytes from the share viewer.
  Add a filesystem-free embedded-asset accessor and an exact, public versioned
  bootstrap URL; preserve authentication on every private/unknown Mail route.
- [x] Point the template at those bytes with their SHA-256 SRI. Preserve the
  existing Tailwind configuration and asset bytes; remove the CDN exception.
- [x] Add the original missing-SRI Tailwind tag as a failing validator control,
  immutable-byte/lookup controls and native handler/authentication controls.
- [x] Add an actual private daemon HTTP test for the page/script hash binding,
  missing-token rejection, unknown-asset rejection and HEAD framing.
- [x] Formatting and whitespace checks pass. UBS scans all four changed Rust
  files and exits 1: 1 critical / 5,985 warnings / 2,804 informational findings.
  Review all 83 located changed-line findings: the critical flags the predictable
  token of an empty private test mailbox, not production token generation;
  the others are test assertions/parsing, slash-bearing strings and test-only
  validator allocations. Keep this red scanner receipt and its 14 unlocated
  occurrences explicit. No suppression or scanner policy change is added.
- [x] Fetch all 40 external script/stylesheet URLs with the honest project
  User-Agent. All 40 response bodies match their declared SHA-384 SRI; all
  40 tags carry integrity/crossorigin. The shared local Tailwind bytes also
  match a fresh fetch of the previously pinned 3.4.17 URL.
- [x] Execute populated native template, embedded-asset, HTTP/authentication
  and private-daemon tests through normal strict RCH; retain every failure.
- [x] Preserve Clippy's first two exit-101 receipts. Correct redundant
  pub(crate) inside the private template module, then use an equality
  diagnostic for the empty HEAD body. No assertion is weakened. Add the
  route's explicit Allow header and its native assertion. Freeze final
  source manifest 399faa76; formatting and whitespace pass.
- [x] Cancel and reconcile only this task's superseded builds through RCH's
  exact wrapper identities, preserving their cache work and exit 143.
  Preserve the daemon-130/delivery-143 reconciliation warnings. Two malformed
  queued Nextest retries were caught and canceled before admission.
- [x] Restore compiler-worker headroom through the operator-authorized
  lossless relocation process. hz3's 20,263 MiB HFDT target contains evidence:
  preserve the complete tree, never treat it as disposable pure build output.
  Initial full marker/open-file/cwd checks pass; newest entry is 162 hours old.
  Copy/verify on trj before any cutover; recheck all activity and protection.
  Keep pressure-based admission intact. Final Clippy/all-target retries exit
  103 without compiling while worker disk pressure is critical.
  All three archive checks pass: 8,030 entries, 21,230,448,640 bytes,
  SHA-256 c961d29d82a2a0be42f2ecd473b3dce37110388a3082e78399360d7d0c78353c.
  Sync the archive and receipts, then repeat full protection, modification,
  strict lsof and cwd guards before the authorized cutover. Free space rises
  from 14,589 to 34,853 MiB. Keep the complete evidence at
  trj:/bulk/worker-relocated-20261102-am-sri/hz3-hfdt-target-1022/ and a
  rename-before-extract restore pointer. No pressure threshold is changed.
- [x] Coordinator headroom falls to 12,467 MiB again. Reject relocating the
  complete mb-sound tree because SunnySnow still has live cwd/open-file users.
  Its separate parked-20261102 archive directory is idle for 7.7 hours with
  no protection markers, mounts or live users. Copy all 11 entries (8,380,692,480
  bytes) to trj, verify all three checks, sync and repeat strict guards before
  the operator-authorized cutover. SHA-256 5af60871d08be9b9bd70e892d127b7bd218eaa0e272798d8f7de59334e548d77.
  Free space rises to 20,464 MiB; active processes remain untouched. Exact
  authorization, command, time, manifests and archive are retained at
  trj:/bulk/worker-relocated-20261102-am-sri/ts1-mb-parked-1230/.
- [x] Re-admit final-source native tests on vmi1152480/vmi1153651 and allow
  normal scheduling to place all-target checking on healthy hz4. Actual live
  Cargo/Nextest directories match all 3,770 final manifest inputs. The native
  retries recompile dependencies despite identical pool paths; do not claim
  that restarting reused earlier compilation time. Leave final runs intact.
- [x] Strict workspace/all-target Clippy passes on final source (remote
  239.3 seconds). All 3,764 present inputs match live and post-run; retain
  the six missing historical performance JSON files as explicit exclusions.
- [x] Workspace/all-target check passes on hz4: Cargo 28m24s, remote 1729.7s.
  All 3,770 inputs match both live and post-run. The dependency's existing
  future-compatibility notice remains. Separate production checking starts
  through normal scheduling on vmi1149989; do not misattribute it to hz3.
- [x] Separate workspace production lib/bins check passes on vmi1149989,
  remote 3557.2 seconds. All 3,770 inputs match live and post-run.
- [x] Final-source formatting and whitespace checks pass.
- [x] In Chromium against the repaired native daemon, verify generated styles
  load without the Tailwind CDN and a tampered response is blocked by SRI.
- [x] Run the original tests/e2e/test_web_dashboard.sh acceptance against a
  rebuilt native server, preserving every assertion and reporting actual counts.
  Original suite: 8 cases, 23 pass, 0 fail, 0 skip. Fresh Chromium Projects
  probe loads local Tailwind with CDN blocked (60px heading, custom primary
  color); an altered server response is rejected by SRI (32px heading,
  transparent background). Preserve the first probe's wrong-root failure:
  /mail is Unified Inbox; Projects is /mail/projects. No product fix or
  weakened browser criterion was used to correct that probe.
- [x] Preserve the populated native result: 41/44 pass; three unchanged JSON
  conversation-template assertions fail. New asset, authentication and real
  conversation-handler tests all pass. The direct pinned-SDK reproduction
  passes the typed context and fails the JSON context with private Serde
  number maps. Claim br-mail-json-template-numbers-4zje9; leave both items
  open while repairing and validating the rendering boundary.
- [x] Convert JSON public values into native MiniJinja scalars at the private
  Mail rendering boundary. Preserve integer precision, ordinary typed DTOs,
  arrays/maps, booleans/null and finite decimals; reject out-of-range numbers.
  Existing assertions and dependency graph remain unchanged. The first
  copied-module probe fixes all three original failures but fails one new
  expected boolean spelling (True, not true); retain that 30/31 result.
- [x] Execute the corrected production module under arbitrary_precision,
  then the actual native server tests and all required compiler checks on
  the final numeric repair. Copied-module proof is explicitly scoped.
- [x] The exact copied production module passes 31/31 under arbitrary_precision
  and 31/31 with ordinary JSON serialization. Two additional real-boundary
  probes preserve u128::MAX exactly and reject u128::MAX + 1 rather than
  rounding it. These temporary native SDK/module probes do not replace the
  actual server build. Both original SDK fault controls remain unchanged.
  The pre-comment module SHA-256 is b7f1d7825b6a4427f4357d23a22147685bd4062405b979fb9bdb08c359d556ec;
  manifest 97f0f86 binds its 3,770 inputs; the sole later doc edit is below.
- [x] Workspace all-target check passes on numeric source 97f0f86, Cargo
  27m44s / remote 1687.6s, with 3,770 live/post matches. Clippy first fails
  solely because MiniJinja needs backticks in the new doc comment. Preserve
  that exit 101; correct only the comment and freeze f4341af6. Strict
  workspace/all-target Clippy then passes (3m26s / remote 215.3s), with all
  3,764 present live/post inputs matching and six historical JSON omissions
  explicit. Formatting passes. The production/native jobs continue on the
  behavior-identical pre-comment source; no assertion or runtime token changes.
- [x] Separate production lib/bins check passes on vmi1149989, remote
  3360.9 seconds. Keep its pre-comment source binding explicit rather than
  claiming the later documentation bytes executed. No dev-feature waiver.
- [x] Expand actual HTTP regression coverage against the verified SRI server
  and CLI binaries using the supported direct suite. Parent: 107 assertions,
  39 cases, zero failure/skip. Children: JWT 33 pass/1 skip; rate-limit 11
  pass/1 skip; peer-address 8 pass; Mail UI 37 pass; streamable HTTP 17 pass.
  These nested counts are not an additive independent-proof denominator.
  Preserve both native-runner dispatch failures: the isolated HOME has no
  worker SSH key, so RCH refuses before tests (103). Recover only those two
  exact wrapper leases to acknowledged terminal exit 1; do not present the
  direct suite as a canonical native-runner pass or hide either child skip.
- [x] Re-execute the original SRI acceptance in a fresh solo pass; fill the
  honesty inventory. CLI daemon passes 1/1 again, both browser controls pass
  in new profiles and original dashboard E2E again passes 23 assertions,
  8 cases, zero fail/skip. Keep both Beads open pending numeric qualification.
- [x] Commit the separately qualified integrity source as 07ebe869. Every
  staged source file matches the tested SRI manifest 399faa76 byte-for-byte.
  Leave numeric source unstaged while its full native/compiler runs finish.
  No original test failure or final-source pending gate is called a pass.
- [x] Full numeric native server/share run passes 46/46, 6,284 filtered,
  execution 4.609 seconds after 116m15s compilation (remote 7013.4 seconds).
  All 3,770 pre-comment inputs match the actual live and post-run source.
  The actual executing server ELF is 331,668,496 bytes, stable-open SHA-256
  f31f25356d3eaed63e57f8f6e90696278910c808e625f3dc1bc8ee5fb9c0c745.
  Automatic observer copying was unavailable; retrieve a byte-identical ELF
  afterward, verify its full hash and preserve it on trj before further work.
- [x] Fresh solo acceptance uses that exact executable in a new private
  environment: 44/44 server tests pass, zero failure/ignore, 5,432 filtered,
  4.41 seconds. Original three failures and all existing assertions remain
  unchanged. This is real native acceptance, not the copied-module probe.
  Native/production evidence predates only the doc-backticks correction;
  strict Clippy checks the final f4341af6 source. No independent review claimed.
- [x] Commit numeric source as e6a637ff. Complete the written work/honesty
  audit, explicitly recording excess repeated inspection and stopping extra
  process work. Close only the two claimed integrity/numeric Beads on cited
  populated evidence. Keep wider engine, release and platform holds open.
- [x] Export and preserve Beads, check cycles and publish the qualified source.
  e6a637ff is verified on both remote refs. All 2,847 original issue IDs and
  every prior comment remain; the sole added issue is the observed JSON bug.
  Release the implementation leases; preserve the failed probes and complete
  written audit with terminal evidence on trj. Final tracker publication is
  documentation only. No whole-project, release or native-platform claim.

## November 1 mailbox-routing repair (br-eos8u)

Latest continuation, November 2, 09:08 UTC:

- [x] Final storage follow-up after source/acceptance publication: concurrent
  work reduces root headroom to 14 GiB and the ballast reserve is empty.
  Recheck current writers and preserve a bounded allocation trace; do not
  claim a unique cause of every prior burst. Select inactive rp496, last
  modified October 26, with no ancestor/nested protection or open files.
  Its verified private archive is 19,757,834,240 bytes, 14,213 entries,
  SHA-256 87fc72120ff215dd42f09f2caca2d4807550ba77432966c5b90337398adb8770.
  All three checks pass; sync archive/manifests/logs before fresh activity
  checks and the already authorized cutover. Relocate 18,846 MiB and leave a
  restore pointer, raising root free 13,631 to 32,474 MiB. Five moves now
  total 91,725 MiB (89.6 GiB). Additional receipts are on
  trj:/bulk/ts1-relocated-20261102-am-eos8u/cass-rp496-0900/.
  Preserve the optional compression-export warning: ordinary SSH completes
  the valid copy and every verification. No project source is changed by
  this follow-up; ongoing work can still consume the recovered space.

- [x] Publish the routing acceptance record in a19dad71. Fresh remote refs
  at 06:19 UTC agree with the pushed main commit and its legacy mirror.
  Tracker export and cycle checks pass; no existing issue or comment is lost.
  Release the completed routing repair's leases. Retain only this plan and
  queries.rs under BlueSpring while working on the next observed bug.
- [x] Claim br-kp1in.79 and inspect both real conversation-browser callers.
  The existing server test project_conversations_follow_stable_rendered_pagination_links
  exercises project preview, rendered older/newer links and concurrent inserts
  through the real native pool. Retain it as the caller acceptance probe.
- [x] Try explicit SQL parameter slots across the scoped CTE and outer page
  query. Preserve the original binding vector, covering index, high-water
  query, page bounds and all existing assertions. The native probes below
  accept this repair; compiler passes alone did not establish correctness.
- [x] Preserve the first native attempt's remote exit 101: the normally
  selected hz3 worker lacks nextest, so no test executes there. Retry through
  normal strict RCH on known-capable vmi1152480. Formatted source manifest
  048b157f differs from the qualified baseline only in queries.rs; the actual
  Cargo and Nextest working directories match all 3,770 frozen inputs.
- [x] Finish native DB probe 30090344884338748: 4/4 pass, 3494 skipped,
  remote exit 0, 55m57s compilation and 5.756s execution. All three original
  failing project_threads_ tests and the existing cross-engine covering-index
  control pass unchanged. Live and post-run source match all 3,770 inputs.
  The actual executing DB test ELF is 126,724,008 bytes, stable-open SHA-256
  494310fc453ade5485528ba80920b57bd4ed373269c301b8d762afb4bd6dc561.
  Preserve the baseline 0/3 failures; the fourth control was not executed in
  that baseline. Bead progress comment 3881 records this bounded acceptance.
- [x] Admit the existing three conversation UI tests on vmi1153651 and the
  all-target workspace compiler check on vmi1149989 while the DB tests compile.
  Both actual worker source trees match all 3,770 frozen inputs. The new UBS
  scan exits 0 with no changed-region findings; its whole-file totals remain
  0 critical / 3,835 warning / 634 informational. This does not clear earlier
  scans or substitute for executing the native probes.
- [x] All-target workspace check 30090344884338751 on vmi1149989 exits 0:
  Cargo 59m32s, remote 3600.6s. All 3,770 inputs match both live and post-run.
  The existing proc-macro-error2 future-compatibility notice remains.
- [x] Admit separate production check 30090344884338753 on vmi1152480 and
  strict Clippy on normally recovered hz3 capacity. Production's actual Cargo
  source matches all 3,770 inputs. Clippy's 3,764 present inputs match; its
  source audit explicitly retains the same six omitted historical performance
  JSON files as the earlier hz3 receipt. No transfer policy is changed.
- [x] Strict workspace/all-target Clippy on hz3 exits 0 with -D warnings:
  Cargo 31m03s, remote 1879.3s. All 3,764 present inputs match both live and
  post-run; the same six historical performance JSON files remain absent.
  Full-manifest source audits correctly exit 2 for those omissions; retain
  that explicit exclusion rather than claiming a 3,770-file match. The
  existing dependency future-compatibility notice remains.
- [x] Separate production check 30090344884338753 exits 0: Cargo 38m29s,
  remote 2335.2s. All 3,770 inputs match live and post-run. This checks the
  default production library/binary graph independently of dev dependencies.
- [x] Final cargo fmt --check and git diff --check pass.
- [x] Native caller probe 30090344884338750 passes 3/3, 5468 skipped:
  remote exit 0, 114m compilation and 4.953s execution. Existing native
  file-backed WAL fixtures exercise rendered preview, forward/back links,
  concurrent inserts and exact conversation/search links; the third test
  checks bounded project-specific cursors. These are direct handler tests,
  not browser/network E2E. All 3,770 inputs match live and post-run.
  Actual executing server test ELF: 329,443,496 bytes, stable-open SHA-256
  fd26b42d5322183091bf47ab91f779f9333ebd29a012c799c069fd89ff409007.
- [x] Complete a fresh solo review of the original br-kp1in.79 acceptance.
  All four DB probes, the real native caller and every required compiler/style
  gate pass without relaxing assertions, budgets or index controls. Close
  br-kp1in.79 at 08:48 UTC; comment 3882 cites exact receipts and exclusions.
  This is selected Linux portable acceptance, not independent review,
  whole-workspace runtime, release, macOS/Windows or soak qualification.
- [x] Preserve terminal receipts on healthy trj storage, record the final
  disposition in Beads, export/check the graph and publish the repaired source.
  Commit 0d876c0a is pushed to main and its legacy mirror at 08:47 UTC.
  Full receipts are retained in trj:/bulk/ts1-relocated-20261102-am-eos8u/
  thread-slots-candidate-0656/. Preserve the baseline failures and missing-runner
  attempt. Engine memory/import (br-kp1in.46), cache/normal-route evidence
  (br-kp1in.75) and release holds remain open. No original criterion is moved
  elsewhere to close this repair.
- [x] Final tracker preservation check: both exported JSONL files retain all
  2,847 issue IDs and every pre-existing comment. Only br-kp1in.79 changes;
  no issue is added or removed. The dependency-cycle check remains empty.

- [x] Native routing acceptance 30090344884338730 finishes with 57/57
  selected tests passing (3462 skipped), including both real Linux daemon
  round trips, unresolved-owner refusal, invalid-path isolation and the actual
  FastMCP resource-ID decoder controls. Live and post-run routing inputs match
  all 3,770 files. The executing CLI unit, integration and `am` ELF hashes are
  retained from stable open `/proc/PID/exe` descriptors. This is selected Linux
  acceptance, not release or whole-workspace runtime qualification.
- [x] Separate production gate 30090344884338735 passes (42m46s); its live
  and post-run inputs match all 3,770 files of manifest a61defcb. Preserve the
  earlier hz4 transport-255 failure separately.
- [x] Strict Clippy on a61defcb finishes 101 on one additional existing
  queries.rs test assertion. Replace only its empty-vector assertion with an
  equality diagnostic; no production behavior or lint policy changes. Current
  manifest 435f811f differs only at that cfg(test) assertion. Formatting and
  whitespace pass. Its actual UBS scan exits 0 (0 critical / 3835 warning /
  634 informational); the sole changed-line warning is the test assertion.
  Correct the earlier grouping of this scan with the two nonzero scans;
  neither the CLI nor the DB-fixture scanner result is thereby cleared.
- [x] Execute all three DB regression controls on the frozen a61defcb source:
  index density passes both original page sizes and all thresholds; legacy
  payload equality fails on canonical Int versus native BigInt representations;
  connection memory fails at 57,160 KiB above baseline against the original
  32 MiB limit. Preserve exit 100 and the actual integration ELF identity.
  Neither failure is waived or represented as passing.
  Read-only comparison of the complete printed payloads confirms equality
  after only the 25 integer tags are normalized. Later migration/write phases
  remain unexecuted because the original assertion aborts the test.
- [ ] Resolve the canonical integer-representation oracle within the existing
  br-kp1in.46 engine-adoption acceptance work;
  retain exact payload, type, null, row-order and timestamp controls.
- [x] Record the memory failure against existing br-kp1in.46 (comment 3878);
  live and post-run DB source both match all 3,770 inputs. The new registry
  engine does not yet meet this probe's bounded-memory budget.
- [x] Execute the three existing project_threads_ query tests on 435f811f:
  native 30090344884338738 exits 100, 0/3 pass (3495 skipped), after 58m31s
  compilation and 3.839s execution. Thread counts return 1 instead of 14/200;
  previous-page navigation includes a concurrent reply. These failures precede
  the edited empty-result assertion or occur in untouched tests. Live and
  post-run source match all 3,770 inputs. No assertion is relaxed.
- [x] Diagnose and repair native thread-browser completeness and stable
  pagination in br-kp1in.79. Explicit SQL parameter slots preserve bindings
  when the engine separates CTE materialization from the outer query. The
  original named DB probes and actual caller pass as recorded above, with
  every assertion retained. Failed runtime/source receipts remain on trj.
  Preserve these actual failures independently of routing proof. The one-bead
  structural batch held exclusive reservation 361 and ended with no cycles;
  that graph reservation is released.
- [x] Current strict Clippy 30090344884338739 on hz3 passes exit 0
  (33m28s Cargo / 2018.3s frontdoor). The hz3 transfer omits six historical tests/artifacts/perf
  JSON receipts; all 3,764 present files match. Keep that explicit exclusion,
  both live and post-run, rather than claiming an exact full-tree match or
  changing transfer policy. The existing proc-macro-error2 future-compatibility
  notice remains; this is not a claim of warning-free dependencies.
- [x] Finish all-target 30090344884338740 on vmi1153651: exit 0,
  Cargo 58m38s, frontdoor 3543.1s. Live and post-run source match all 3,770
  inputs. Its following production request refuses admission 103 before Cargo;
  preserve the two outcomes separately. The existing future-compatibility
  notice remains.
- [x] Finish normal fleet-selected production check 30090344884338743, admitted on
  vmi1152480 at 05:13 UTC. Actual Cargo PID 2081079 matches all 3,770 inputs.
  Its source tag is a0023222; source bytes still match 435f811f after the shared
  a143e70d/a0023222 commits. Exit 0, Cargo 43m41s, frontdoor 2648.1s; post-run
  all 3,770 inputs also match. No worker-policy override or local compilation.
- [x] Apply the explicitly requested system-performance-remediation move to
  healthy trj:/bulk. The first inactive scratch tree is retained in a private
  verified 11,937,157,120-byte archive (166,099 entries; SHA-256
  95b0f86cde72b0e2ac43a46c43b8863c7903b97d2acf8625a475f55e85bee6f7).
  All three copy checks pass and archive/manifests are durable before cutover.
  The source now has a restore pointer; root headroom rises from 1,068 to
  12,734 MiB. Exact authorization, command and invocation time are logged.
- [x] Finish COPY/VERIFY of the two selected inactive CASS scratchpad trees
  gh537 and gh483, then recheck protection markers, modification times, open
  files and process working directories before each authorized cutover.
  Keep active/recent and protected scratch trees in place; leave the failed
  NAS untouched. Preserve source-operation logs durably on trj.
  All three selected moves are now complete, relocating about 53 GiB of
  original disk use. The latest observed root headroom is 36 GiB; concurrent
  work still consumes space, so this is not a promise of permanent relief.
- [x] Complete an additional verified move of inactive codexnoop10k (about
  19 GiB, 136,500 entries, last modified October 12). Continued active work
  creates new preservation archives on root, consuming the recovered space;
  leave those recent artifacts in place. All ancestor/nested protections are
  clear and strict lsof returns 1 with empty stdout/stderr. COPY/VERIFY starts
  at 05:15 UTC with the same reviewed helpers. A bounded 64 MiB source-to-trj
  uncompressed transfer measures 12.3 MiB/s, so retain per-invocation SSH
  compression. All three checks pass at 05:39 UTC: SHA-256
  3bd58b53e11388c4d107ef0ca05f451265a077491a80837b49e4368d3e8c738c,
  136,500 entries, 19,527,843,840 archive bytes. Sync archive/log/manifests
  before cutover; relocate 18,728 MiB, leave a restore pointer, and observe
  root 19,207 to 37,935 MiB free. The four moves total 72,879 MiB (71.2 GiB)
  of original disk use. Complete authorization/command/time/result log is
  synced as evidence-0350/relocation-source-complete-0552.log on trj.
- [x] Preserve completed compiler/scanner/source receipts separately on healthy
  trj storage. A bounded allocation trace captures ordinary database checkpoint
  and linker allocations, not a proven single cause for every root-space
  change. Journals are on RAM-backed /run. No shared service or kernel setting
  is changed; concurrent work continues consuming recovered headroom.
- [x] Verify gh537's 22,858,557,440-byte archive (595,319 entries;
  SHA-256 8fe610a3437978bbd8be0688ab8ae6dbee247c9e0f76ee35c609b3d2e10dad7c)
  by all three checks; sync archive/log/manifests. Source cutover then refuses
  on its open-file check and removes nothing. Two subsequent read-only lsof
  checks are clear, including the helper's exact invocation; retain the refusal
  and defer retry while gh483 copies. A verified archive is not a completed move.
- [x] After the gh483 tar stream finishes, retry gh537 through the unchanged
  helper. Cutover succeeds: 22,290 MiB relocated, source pointer left, root
  8,526 to 30,814 MiB free. Preserve the initial refusal and successful retry
  separately. Source authorization/command/result log and completed README
  are synced on trj; restore instructions preserve the pointer by renaming it.
- [x] Verify gh483's 21,140,008,960-byte archive (28,583 entries; SHA-256
  554dfffa4ad691fe3a1fbe533b9285f280678230f54f650c8cc67e2fd29e8d9c),
  sync archive/log/manifests, then complete its protected/idleness-checked
  cutover: 20,195 MiB relocated, source pointer retained, root 16,759 to
  36,943 MiB free. Complete authorization, exact commands, timestamps and
  results are retained in trj:/bulk/ts1-relocated-20261102-am-eos8u/
  evidence-0350/relocation-source-complete-0443.log. No data is discarded.
- [x] Identify the recurring scratch writer: six-day experimental daemon
  3839886, private port 47299, absent launcher/session leader 3567556, no
  external connected clients. A strict empty-socket check first aborts without
  signaling on a self-health connection. Recheck executable/arguments and
  allow only sockets owned by that same process; send SIGTERM through its
  pidfd at 03:57:35 UTC. It exits cleanly. Preserve all its data and leave
  shared mailbox PID 3047470 running. Root free space then holds near 9.2 GiB
  in subsequent samples; do not infer that every possible writer is fixed.
- [x] Finish a fresh solo pass against the original routing criteria and close
  owned br-eos8u on native 57/57 plus the required compiler gates. Closure
  comment 3880 records exact source/ELF identities, transfer exclusions and
  retained failures. There is no independent runtime, full-workspace runtime,
  release, macOS/Windows or soak clearance. No original routing requirement is
  transferred to the new conversation bug.
- [x] Complete final tracker export/graph checks and publish the routing
  acceptance record; hand off leases as described above. Mailbox health reports
  one unrelated reservation-thread provenance finding (346), left untouched.

Earlier continuation, November 2, 02:10 UTC:

- [x] Complete routing-source all-target check 30090344884338731: exit 0,
  57m20s. Its live and post-run source both match all 3,770 routing inputs.
  The separate production request refuses admission 103 before Cargo.
- [x] Preserve strict Clippy's real failure: five incoming DB-test lints,
  remote 101 plus caller 1 after local ENOSPC obscures completion. Exact-wrapper
  recovery initially fails on the full disk, then acknowledges 101 when 117 MiB
  returns. No compiler pass is inferred from the edited CLI crate checking.
- [x] Reserve the two DB test files (348/349, through 05:06 UTC), preserve their
  originals in RAM, and manually implement all five lint remediations. Improve
  the empty-result diagnostic; add doc backticks; move only canonical fixture
  setup into two helpers. Every schema, timestamp, row, index-density, memory,
  child-process and acknowledgement assertion remains. No lint suppression.
- [x] Formatting and whitespace pass. New manifest a61defcb differs from
  routing manifest 37fce3fd only in canonical_schema_readable.rs and
  startup_page_leak.rs. CLI/runtime production bytes are unchanged. The running
  native routing job remains on its original frozen remote source; do not replay
  it or claim that it tests these subsequent DB fixture edits.
- [x] Scan both edited DB test files: 1s, exit 1, 4 critical / 215 warning /
  49 informational. The four critical sites are existing test-failure panics.
  All 16 changed-line findings (14 warning / 2 info) are retained fixture
  unwraps/assertions, the owned body clone and a slash in SQL misread as division.
  Review is complete; the raw scan remains red, with no new suppressions.
- [ ] Execute the three existing populated DB regressions after normal admission:
  canonical_legacy_mailbox_survives_native_upgrade_and_new_writes,
  descending_metrics_insertions_keep_index_pages_dense, and
  existing_mailbox_reads_have_bounded_connection_memory. Retain every original
  count, page size, byte/RSS budget and child acknowledgement.
- [ ] Obtain new-source all-target, separate production and strict Clippy gates.
  First new Clippy request refuses admission 103: requested worker is draining
  or unavailable. Do not force admission, compile locally or change fleet policy.
- [ ] Finish the existing native CLI acceptance job, then fresh original
  acceptance, Beads export/graph checks and publication. Routing proof and DB
  fixture proof must retain their distinct source identities.

- [x] Review peer commits d575364e and 7f9c3bfd: they preserve the exact
  resource-ID repair and its pending-acceptance record. All 3,770 source inputs
  still match manifest 2c86bd93; committed-head manifest 37fce3fd records the
  same inputs at 7f9c3bfd. No source edit or test result is inferred from a commit.
- [x] Complete the three-file UBS scan with reliable RAM output: 739 seconds,
  exit 1, 4 critical / 12,069 warning / 5,471 informational findings. All four
  critical flags are existing reservation_read.rs comparisons at lines
  182/183/205/218: protocol version, public tool request ID, content type and
  equality of two returned payload representations. These are not comparisons
  of authentication secrets. All 14 findings on new lines are test-only
  (12 warnings / 2 informational). Preserve the nonzero scan; no annotation,
  skipped category, baseline regeneration or unrelated gate-clearing rewrite.
- [x] Admit current-source native, all-target and strict Clippy jobs through
  normal RCH policy. Each actual Cargo namespace matches all 3,770 frozen
  inputs with no exclusion or mismatch. Their output is RAM-backed. The native
  selection includes all original routing controls and reservation_read::tests.
- [ ] Observe populated native results, required compiler exits and a separate
  current production lib/bins check. No current runtime pass exists yet.
- [ ] Keep the red scanner result explicit in acceptance; do not label it clean
  or manufacture a green result by rewriting unrelated response checks.
- [x] At 01:21 UTC the coordinator's /tmp tmpfs also reaches 64/64 GiB.
  Inspect memory first (49 GiB available), then raise only its capacity limit
  to 72 GiB with `sudo -n mount -o remount,size=72G /tmp`. Preserve nosuid,
  nodev, inode settings and every file; the failed NAS is untouched. This
  reversible operational change restores 8 GiB of capture headroom. All three
  owned wrappers remain active; their compilations are not replayed.
- [x] At 01:39 UTC /tmp usage independently falls to 48 GiB. Restore the
  original limit with `sudo -n mount -o remount,size=64G /tmp`, retaining
  16 GiB of headroom and the original mount protections. No files are moved or
  removed by this task. Root free space has independently fallen to 4.2 GiB;
  preserve both actual capacity observations rather than claiming global relief.

- [x] Preserve the exact resource-ID correction and Unicode long-URI fixture.
  Current manifest 2c86bd93 has 3,770 tracked inputs except Beads/planning;
  CLI hash 382dfa13, reservation reader 05605eb8, integration e5fa4d4c.
  The encoded URI grows without enlarging the fixture's ASCII archive slug.
- [x] Verify fresh formatting/whitespace and the current read-only mailbox
  health: same owner PID, parity 343/343, healthy. Renew only the five owned
  source/plan leases through 04:01–04:12 UTC.
- [x] Observe build 30090344884338728's actual Cargo and Nextest namespaces:
  all 3,770 inputs match, no exclusions or mismatch. It is ordinary strict RCH
  after the receipt-enabled compile request fails before Cargo on excluded
  fuzz manifests' unavailable sibling paths. No sibling dependency is smuggled
  in and no manifest or source-admission policy is weakened.
- [x] Preserve the second full-disk failure: the first three-file scanner
  exits 2 on its home-cache write. New cache and temporary output paths use
  supported per-process RAM settings; protected existing home caches stay put.
- [x] Preserve the NAS failure separately. Kernel buffer I/O errors on sda1
  make the existing NAS evidence directory inaccessible. Source, patches,
  manifests and tracker are copied to local recovery storage; earlier NAS
  receipts are currently unreadable, not missing-test successes. No filesystem
  repair, remount, service restart or shared-mailbox reconstruction is performed.
- [x] Recover old all-target build 30090344884338723 after its caller aborts
  134: actual remote exit 143, terminal acknowledged. No all-target pass follows.
  Cancel only owned native build 30090344884338728 while it is compiling into
  the failed capture destination; recover exit 143/acknowledged, not a test pass.
- [ ] Complete the replacement native selection with the same cases, features
  and watchdogs, using RAM logs/artifacts. No test result for the resource-ID
  correction exists yet; normal requests have also refused admission 103.
- [ ] Complete current-source strict Clippy, all-target and separate production
  checks through ordinary admission. The earlier production pass (28m30s,
  all 3,770 source files matched afterward) qualifies c573e11e only.
- [ ] Finish the replacement three-file scanner with reliable local/RAM output,
  review changed-line findings, then fresh original acceptance and publication.

Current reliable recovery root:
`/data/tmp/am-eos8u-io-recovery-20261102T0044/`; new run output is under
`/tmp/am-eos8u-runs-20261102T0050/`. Preserve the NAS receipts for operator
recovery. Do not run its old cache-relocation helper while the volume is failing.

- [x] Resume after disk recovery: 22 GiB available; read-only mailbox health
  passes again with the same owner PID, reservation parity 327/327. No repair,
  reconstruction, service change or protected-home cache move was performed.
- [x] Recover the original 0.4.13 all-target wrapper without replay: exit 0,
  terminal acknowledged. This qualifies the older 96b25a56 source only.
- [x] Inspect and fast-forward the four incoming commits through 7c9137da;
  preserve existing plan/tracker changes and import Beads. Incoming offline
  VACUUM and conversation browsing changes remain distinct from this repair.
- [x] Apply the additional invalid-database-path guard as small manual hunks.
  Unresolvable paths no longer mean unowned, error text does not echo URL
  credentials, and explicit endpoint authority is retained. Add an isolated
  child regression for default/private roots, both invalid URL spellings and
  explicit-port behavior; require one actual child test to prevent zero-run green.
- [x] Current formatting and whitespace checks pass. Freeze resumed manifest
  c573e11e (3,770 tracked files except Beads/planning); CLI hash 162afec7,
  lock d6918089. This is a new source epoch, not qualified by the older run.
- [x] Finish the already-running 96b25a56 native routing suite without cancelling
  or replaying it: 29 executed, 28 passed, one real round-trip failure at inbox
  reading (HTTP 400), after registration and send succeeded. The real unresolved
  owner refusal and every selected ownership/recording control pass. Preserve
  native exit 100 and caller exit 1: RCH's comparison also rejected 42 deltas
  after the local source advanced. Exact-wrapper recovery acknowledges exit 100;
  no runtime acceptance or source-barrier waiver follows.
- [x] Diagnose the HTTP 400 with the exact compiled binary (SHA c1d13b1e).
  The first private probe stops at ownership before transport and is retained.
  An explicit private endpoint isolates the transport: registration/send pass,
  inbox fails, a 295-byte resource request ID gets HTTP 400, and the identical
  resource URI with a short ID gets 200. Actual 256/257-byte boundary controls
  return 200/400. All private children are stopped and reaped; no live mailbox
  is used. Receipts: http400-private-diagnosis*.json under the existing NAS root.
- [x] Replace all three URI-sized resource correlation IDs with one bounded
  SHA-256-based ID, retaining exact resource URIs and response identity checks.
  Add real FastMCP-decoder positive/old-ID-negative controls and make the existing
  real round-trip fixture use a long project path. Keep every assertion and
  original deadline. No protocol limit or transport gate is weakened.
- [ ] Run the complete declared routing suite including the added invalid-path
  regression on resumed source. Require the real daemon registration/send/inbox
  and real unresolved-owner refusal, retaining original assertions/deadlines.
  Include all reservation_read::tests because resource ID generation changed.
  Keep this checkout's source payload frozen until the receipt job finishes.
  First new-source request refused admission with exit 103/no free slots; no
  tests ran. Retry only through normal admission after capacity changes.
- [ ] Complete resumed-source all-target, separate production lib/bins and
  strict Clippy checks through ordinary strict RCH admission. The all-target
  job is running on vmi1149989; no local compilation or worker-policy override.
  The first resumed Clippy run exits 101 on an incoming conversation-query
  documentation paragraph. Split that paragraph without changing its words,
  code or lint rules; the corrected source needs a fresh strict Clippy run.
- [ ] Finish the changed CLI scanner, review new changed-line findings, retain
  original failed attempts and preserve the unchanged integration-file receipt.
- [ ] Fresh original-acceptance check; update Beads with terminal evidence,
  flush JSONL, check cycles and graph preservation, commit/publish without force,
  then release only this task's three leases (renewed through 02:12 UTC).

The fresh scanner on CLI hash 162afec7 completed in 740s, exit 0, one file,
0 critical / 10,227 warnings / 4,986 informational findings. All 15 new changed-line
findings were reviewed: 12 intentional fixture/assertion warnings and three
test-only clone/Command/expect_err notes. Production guard lines have no finding.
This predates the resource-ID fix, which requires a new changed-file scan.

Peer commits 4bea1fe5 and f80ddff9 preserve the plan and exact invalid-path guard
bytes with acceptance explicitly pending. They do not change the c573e11e code
manifest. The resource-ID correction is a subsequent source epoch; preceding
compiler/scanner results are not its acceptance.

The full-disk failure and prepared recovery candidate are preserved in the NAS
recovery copy. Capacity is now restored independently of the blocked cache move;
the helper was not run by this agent. Native build 30090344884338721 still runs
its original frozen source. Its result cannot qualify the additional guard or
the incoming conversation/VACUUM work. New logs and scanner outputs remain under
`/mnt/je_nas/am-eos8u-preserved-20261101-zpzkz13u/`.

- [x] Refresh authoritative ready work and preserve the clean tracked checkout.
  Governing project docs match the fully reviewed snapshot. The TUI contact
  query already landed; its remaining Mac production check is not a new fix.
- [x] Claim `br-eos8u` and reserve the CLI implementation, existing integration
  tests and this plan. The partial repair still guesses for unresolved default
  mailbox owners and skips their bounded discovery retry. Its argv filter can
  also discard a live exclusive owner the database probe already identified.
- [x] Remove the default-root exception while retaining genuine unowned/default,
  unowned/private and explicit-endpoint behavior. Keep the existing retry budget.
- [x] Treat every reported competing owner as requiring listener verification;
  retain both-lock/exclusive/single-PID authority and the ownership recheck.
  Show owner PIDs and remediation without echoing owner command-line secrets.
- [x] Add recording-endpoint regressions on ephemeral ports only, with a real
  authenticated HTTP positive control and zero requests for unresolved owners.
  Cover missing/ambiguous owner, disappeared/changed ownership, actual dedicated
  server-style command lines, known CLI holders and unreadable command lines.
- [ ] Execute the real nondefault-daemon CLI registration/send/inbox round trip
  and the real owned/unresolved refusal test. Distinguish injected probe tests
  from these real lock/listener/transport paths; never touch the live default port.
- [ ] Run exact-source focused native tests plus all-target, separate production,
  strict Clippy, formatting and scanner checks through strict RCH.
- [ ] Fresh solo acceptance against the original Bead; retain failures and
  original broader holds, then close only on populated evidence. Flush Beads,
  check graph preservation/cycles, commit/push without force and release leases.

Implementation and regression code are written; runtime acceptance is pending.
Evidence is retained in `/data/tmp/am-eos8u-20261101T2014/`. Initial worker
admission refusals are exit 103, not test failures or passes. The receipt-enabled
workspace attempt also failed before Cargo: RCH's contained-source policy rejects
sibling paths in excluded fuzz/browser manifests. The unchanged Cargo workspace
check is being retried through strict remote execution with a separate live-source
observation. The native test job was admitted with a source-content receipt.
Its source matches the frozen manifest except six historical perf JSON files
already excluded by `.rchignore`; those exclusions are not new and are not test
inputs for this routing slice. They remain explicit exclusions, not exact-tree
equality. No compiler or runtime success is claimed yet.

Scanner result: the initial UBS run timed out at 300 seconds (exit 2, no files
scanned). Doctor found the scanner healthy. The identical two-file/rule scan
completed in 763 seconds with a 1,200-second scanner budget: exit 0, zero
critical findings, 11,724 warnings and 5,364 informational findings across the
full monolithic files. All 62 findings on changed lines were reviewed: test
assertions/panics, known-nonempty fixture indexing, bounded read-buffer slicing,
strict UTF-8 on the test client's own request, loopback HTTP, cloning and slash
strings misidentified as division. Production changes have one informational
clone finding and no new warning or critical finding. No suppression, category
exclusion, product-test timeout change or whole-project scanner claim follows.

Source transition: another session committed the same pending routing patch in
`03814432`, explicitly without acceptance or closure. The frozen CLI/test bytes
are unchanged. Then upstream `96b25a56` upgraded the runtime engine to 0.4.13;
its only changes are AGENTS, Cargo.toml and Cargo.lock (20 registry engine
packages; the separate Beads engine is unchanged). The non-overlapping
fast-forward and required Beads import succeeded. Current manifest
`78b950f3` has lock hash `d6918089`; the old manifest `18881c42`/lock `4e6bb8d6`
remains separately preserved. The old native build was cooperatively cancelled
through its exact RCH wrapper and exited 143 before tests ran. It provides no
runtime pass. The replacement uses the identical tests and watchdogs on the new
graph, with one runner thread to fit normal fleet admission. The old all-target
job may finish, but any result from it qualifies only the 0.4.10 graph. Current
0.4.13 native, all-target, production and Clippy acceptance remain open.

The 0.4.10 all-target job completed (build `30090344884338715`, exit 0,
47m55s). All 3,769 frozen files still matched on the worker afterward.
Cargo reported the existing `proc-macro-error2` future-compatibility warning;
this is compiler evidence only, not a runtime or current-graph pass.

The coordinator subsequently ran out of disk space. The first 0.4.13
all-target caller aborted (134); recovery of its exact wrapper reports 143,
not success. A queued native retry also aborted before admission. Only those
owned wrappers were reconciled. Completed diagnostic trees were archived and
byte/metadata-compared on the NAS, then moved to Linux RAM storage with links
at their old paths. The archive preserves the non-UTF-8 negative-test filename
that a loose NAS copy could not represent. This recovered limited working
headroom without discarding the diagnostics; the coordinator remains short
of disk space. Authoritative new logs/artifacts are under
`/mnt/je_nas/am-eos8u-preserved-20261101-zpzkz13u/`.

Current native build `30090344884338721` runs on `vmi1152480` with the same
tests, features and watchdogs; its caller artifact directory is on the NAS
while RCH retains normal worker-side target placement and admission. The
current all-target run is on `vmi1149989`. Both compile the 0.4.13 graph.
Live source checks match the current manifest (3,769 files for the workspace;
3,763 plus the six pre-existing perf exclusions for native). Neither is a
runtime pass yet. The live mailbox remains healthy with the same owner PID;
only this task's three leases were renewed, through 00:12 UTC on November 2.

## November 1 execution queue (publication verified)

Beads remains the complete dependency-aware work list. This queue records the
current execution details in the existing plan because the earlier loose
temporary logs and TODO are no longer present. Their absence is not a pass.

- [x] Preserve the local corrections and incoming history. A concurrent worker
  advanced `main` through the 89 upstream commits to `1c21a192`, then preserved
  the preceding bridge entry in `333cb8c0`. Equivalent barrier/archive test lint
  fixes are already upstream; do not duplicate them or replace peer changes.
- [x] Recover original strict-remote completion records: DB build
  `30085930664591513`, all-target build `30085930664591512`, and GNU production
  build `30085930664591514` each report completed/exit 0. DB test counts and
  named cases cannot be recovered from the missing stdout. This is completion
  evidence on the older frozen source, not a current 0.3.40 runtime verdict.
- [x] Recheck the live mailbox read-only: healthy, reservation parity300/300
  on the latest probe; the same live owner remains in place.
  Leave its installed binary, service, database and archive untouched.
- [x] Import Beads after the incoming work. An overlapping read/import hit
  the sole-opener checkpoint guard; the sequential retry succeeds. No database
  repair, force flag, row deletion or lost-issue reconstruction was used.
- [x] Claim `br-97lrm` and reserve the installer and existing exit-status test.
  Honor `TMPDIR` and `AM_INSTALL_LOCK_FILE`, documenting the environment in
  `--help`; retain the atomic directory/PID/owner/stale-lock machinery.
- [x] Run all44 existing exit controls and16 complete-installer lock controls:
  no failures. Cover file/stdin delivery, free/busy locks, private and
  trailing-slash temporary paths, explicit and empty overrides, and spaces.
  Free-lock cases reach the intended destination refusal and clean up their
  lock; busy-lock cases retain the original live owner's PID evidence.
- [x] Complete the old/corrected installer comparison in the same private
  mount/network environment: old source passes44 existing cases and fails all16
  new cases; corrected source passes all60. The initial sandbox had unusable
  `/dev/null`, failed52 cases and gets no behavior credit. Keep that failure.
- [x] Run four actual authenticated0.3.40 installations, including default
  file/stdin delivery: all pass signature/checksum, exact installed versions,
  setup witnesses and exit0. All use private homes/destinations. `br-97lrm`
  initially closes at this scope after fresh solo acceptance; no independent
  review. The final concurrency review below finds a regression and reopens it.
- [x] Fix both installer-test subshells to abort if entering their private
  working directory fails. Final44+16 controls pass again. The Bash scan reports
  two pre-existing critical findings and zero warnings; it remains a red scan.
- [x] Installer/test Bash syntax, final Rust formatting, whitespace and the
  existing three lastpipe controls pass. These are bounded checks, not an
  authenticated installation or a release qualification.
- [x] Finish the admitted older-source Clippy run on `vmi1152480`: exit101 on
  three initializer visibility lints. Incoming0.3.40 already fixes all three;
  no extra patch or suppression is added. Its live
  compiler source matched all3779 files of manifest `92bae2ee…`, based on
  `8820c3fb`; subsequent incoming 0.3.40 code still needs its own checks.
- [x] Finish the three current 0.3.40 gates: all-target checking on `vmi1152480`,
  production lib/bins on `vmi1293453`, strict all-target Clippy on `vmi1153651`.
  All three live compiler namespaces match all 3,782 dispatch-snapshot files,
  manifest `bed7ccf6`, with no exclusions or mismatches. Subsequent plan/tracker
  updates do not change the compiled Rust/Cargo inputs.
  All three finish with exit0 (38m08s / 70m12s / 58m13s respectively). The
  upstream `proc-macro-error2` future-compatibility notice remains. Formatting
  and whitespace also pass. This is compiler qualification, not a runtime or
  release certificate. These passes precede the later `.78` test-fixture edit;
  that source has its own required gates pending below.
- [x] Run the original populated DB admission/initializer, storage
  checkpoint/path/owner-lock/decoded-index and bundle publication controls on
  the final source when admitted. GNU compilation and native Windows execution
  remain distinct; `.76`, `.77`, `.49` and broader authority holds stay open.
  Two ordinary native requests refuse exit 103 because the requested workers cannot
  provide the original two test threads; zero tests run. Nextest archive/list
  requests refuse RCH-E301 as unsupported. A normal one-job Cargo compile-only
  request refuses exit 103 (pressure/project exclusion). No direct SSH compilation,
  local compilation, job-mode compiler smuggling, fleet override or weakened
  runner settings are used. Retry only after running gates release capacity.
  After the all-target job finishes, the normal two-thread native request is
  admitted on `vmi1152480`. It is compiling on source `0c350359`, manifest
  `422c61c8`; all 3,782 actual input files match with no exclusions. Both outer
  timeout variables are14400; per-test deadlines remain unchanged. Debug info
  and incremental compilation are disabled to bound build resource use. No
  The run finishes exit100:180 selected tests execute,179 pass and one fails;
  5,187 tests outside the declared filter are skipped. DB49/49, share3/3 and
  storage127/128 pass. Both new checkpoint cases execute and pass on the actual
  current storage ELF `95bde54d`, rather than the older stale77 executable.
  The failure is `test_file_lock_stale_cleanup_permission_denied_returns_false`:
  its uid65534 child cannot execute through root-private `/data/tmp` ancestors,
  so the original denial assertions have not run in that failing invocation.
  A late runtime-source observer reaches the process after exit and sees no
  process; it receives no runtime-source proof credit. The live compiler source
  witness and the three observed executable hashes remain valid bounded evidence.
- [x] Claim `.78`, reserve the existing storage source and reproduce the fourth
  permission-probe launch defect. On the same exact original ELF, root-path
  execution fails101/EACCES while a retained-descriptor invocation with real
  uid/gid65534 and no supplementary groups passes1/1, executing the unchanged
  denial, lock/metadata preservation and restored-writable positive assertions.
  Reuse `.65`'s existing Linux descriptor technique in the fixture; no shared
  path modes, child privileges, production behavior, assertion or deadline change.
- [x] Complete `.78`'s unchanged180-control rerun:180/180 pass (DB49, storage128,
  share3), with all11 declared filter families populated and5,187 outside-filter
  cases skipped. The repaired storage ELF is `7a45bc8c`; DB `8938bf3e` and share
  `7f1be08b` are unchanged. All-target checking passes39m10s and strict Clippy
  passes31m23s; separate production checking is still running. These compiler
  runs precede the later installer concurrency correction. The native,
  all-target and separate-production processes each match all
  3,782 files of repaired-source manifest `d405cd55`, with no exclusions or
  mismatches. Do not promote original179 passes into a green suite or close the
  platform/cache/engine/release requirements from this fixture maintenance.
- [x] Scan the edited storage file: UBS exits1 with4 critical,4,399 warning and
  1,154 informational findings. Three critical sites are pre-existing worker-
  spawn panic paths; the fourth flags the cfg(test) command whose executable is
  formed only from the retained current ELF descriptor. No user-controlled
  executable selection, annotation or scanner-rule change is introduced. This
  review does not make the overall scan green.
- [x] Remove the ineffective checksum setting in `ad628c9c` after its real
  controlled reproducer fails. `.75` stays open: this removal is not a
  normal-route cache fix. Preserve the stale77 correction below.
- [ ] Keep `.63` and `br-5nr0m` open under their original workloads and reader
  protection. The incoming detailed reader-scan task covers the observed
  per-candidate full process scan; do not create a duplicate performance lane.
  A private Linux notification probe rejects one shortcut: ordinary directory
  opens emit `IN_OPEN`, while duplicated and `O_PATH` handles do not. All are
  visible readers of the same inode to the existing inventory. A notification-
  only replacement would weaken protection; no production change is made.
- [x] Complete the fresh written20-question solo acceptance review, flush
  Beads, and verify zero cycles. Issues and base retain all old IDs/comments,
  with no duplicate IDs; their2845 rows include the six earlier owned tasks.
  Preserve all original engine/platform/cache/authority/release holds.
- [x] Execute all22 existing real-Git commit-admission regressions: pass. This
  is standalone production-template coverage, not a native Rust CLI-renderer
  test, and closes no additional feature task.
- [x] Build the mandated ACFS checker remotely. Its local baseline checksum
  validation, installer listing and `check --dry-run --local` all pass using a
  private data directory. New installer pin validation follows publication.
- [x] Commit/push the owned changes in logical batches: `ad628c9c` removes the
  falsified cache setting; `0c350359` carries the installer repair and its
  original acceptance; `003c7d79` records the current compiler gates and the
  retention counterexample. Non-force atomic publication succeeds; both public
  refs equal `003c7d790843c8d29fa4ea29e54ee48a9aa622ae` at17:01:57 UTC.
- [x] Generate the canonical ACFS checksum candidate after publication. Its
  Agent Mail pin is correct; four unrelated upstream installer changes are
  held unchanged. ACFS's checksum monitor independently publishes `47eec968`
  with only the Agent Mail pin, timestamp and two generated checksum consumers
  changed. Review all three paths, fast-forward the clean ACFS checkout to that
  immutable commit, and import its Beads sequentially without repair.
- [x] Verify the public installer blob `be535232` and ACFS public file agree
  on SHA256 `f8ca5d8173bb86ea599c7a84210869981c972380854b00982165685e2d4c1c07`.
  Both ACFS public refs equal `47eec96814ce7aa4b9da7e6c638da75e4dc593b4`;
  both generated consumers match the actual checksums.yaml hash `415ef8da`.
  The mandated local checker again passes validation, installer listing and
  `check --dry-run --local` after synchronization. No unrelated pin is updated.
- [x] Re-read the four open release P0s against the current source. Public pin
  parity settles this installer follow-through, not `br-quq4r`'s original
  release-blocking acceptance. Canonical DSR policy, tag identity, advertised
  artifact coverage and container provenance remain distinct open requirements;
  no dormant workflow or post-publication poller is promoted into release proof.
- [x] Reopen `br-97lrm` after the final concurrency review catches a regression
  in the initial TMPDIR repair. In one private PID/network environment, original
  `4a08f950` refuses a second same-destination installer, while published
  `f8ca5d81` lets it reach download with a different TMPDIR. The first owner is
  still running and its real PID lock remains owned. Distinct destinations do
  advance as intended. The download substitute establishes admission behavior,
  not authenticated installation or a claimed observed corruption.
- [x] Add destination-bound exclusion before transaction recovery. Both locks
  use the same atomic mkdir/owner/PID/stale-lock rules and reverse-order cleanup;
  an initial override naming the destination lock itself does not self-contend.
  No transaction, signing, owner-preservation or primary-lock condition is removed.
- [x] Execute all44 original exit controls,16 initial-lock controls and6 new
  two-installer controls:66/66 pass. Cover file/stdin delivery, same/distinct
  destinations, alternate spellings of the same initial/destination lock, exact
  live owner PID, unchanged active-journal inode/bytes and owned-lock cleanup.
  The published TMPDIR-only source passes the original60 and the two distinct-
  destination controls, but fails all four shared-destination controls. Retain
  the initial pre-lock curl observer mistake and the missing-source mount error
  as setup failures with no behavior credit.
- [x] Repeat all four actual authenticated0.3.40 installations on corrected
  installer SHA256 `2036b05888b613a05c74162364f8aceefd00a0fafa18123f8f3aae4c2e47a180`:
  exact versions, minisign, manifest checksums, setup witnesses and exit0 all pass.
- [x] Finish the earlier separate production check:0/51m59s. Qualify the
  combined installer and fixture source, manifest `4c6a12fd`: all-target
  checking0/4m44s on `hz4`, separate production0/3.34s on `vmi1153651`, and strict
  Clippy0/7.90s on `hz4`. Formatting, Bash syntax and whitespace pass. Two core
  tests embed install.sh, so this later qualification matters. Actual all-target
  and production process namespaces match all3,782 files, with no exclusions.
  The Clippy rerun has its frozen transfer/command receipt, not a separate live
  source observer: the extra observer request refuses admission and receives no
  additional proof credit. A first observer misses `cargo-rch-real` by matching
  only the literal cargo executable name; the corrected all-target observer
  matches the real process. All failed/refused observations remain retained.
- [x] Cancel only the redundant owned all-target job after the equivalent final
  gate passes. The legacy build-ID cancellation refuses an ownership mismatch;
  its recommended exact-wrapper command cancels `rchw-8ef9fcc9` successfully.
  The original wrapper exits143 and receives no pass credit. No process signal,
  daemon restart, slot override or cleanup is applied to another agent's work.
- [x] Close `.78` on its original Linux fixture criteria and cited terminal
  evidence. Keep `.75/.76/.77/.49`, retention, engine, authority, large-archive
  and release requirements open; the original failed whole-workspace run stays
  red. UBS still reports2 Bash critical findings and4 in the edited Rust file.
- [x] Publish both owned source fixes: `a61642cc` retains permission-test ELF
  authority; `1bd555e9` fences shared installer destinations. Non-force atomic
  publication succeeds; both public MCP refs equal
  `1bd555e92e840e1e0d34f3c07ff6261f7ffcd9cb` at18:20:14 UTC. Public installer
  blob `f53c37dbbdc0868c334fd7cb0b080169a37af3ec` matches local source2036b058.
  The earlier public `f8ca5d81` pin and its regression remain historical evidence.
- [x] Read and preserve ACFS's incoming DSR-only policy commits through
  `fd59ab47`, with clean fast-forward and sequential Beads import. Do not rely
  on a GitHub Actions workflow for this update. The shared DSR registry has no
  ACFS entry; a private, explicitly scoped job configuration runs the existing
  canonical utilities and local checker without changing that shared registry.
- [x] Independently observe all48 installer URLs:43 matches,5 mismatches,
  zero errors and zero skipped entries. Canonical bound candidate construction
  and validation pass2/2 through direct DSR, deferring exactly atuin/grok/omp/uv.
  Only the header and Agent Mail pin change in checksums.yaml. Regenerate the
  consumers; the internal ledger also updates the actual newly changed
  `acfs/AGENTS.md` policy checksum to `eb5caa1e`.
- [x] Retain the first DSR consumer run as an overall failure: its4 commands
  exit0, but generation changes tracked source during that run. The subsequent
  stable source passes all4 generator/local-checker commands, and both static
  checks pass. The two generated consumer files scan with zero UBS findings.
  This is scoped checksum maintenance, not a full ACFS installation or release.
- [x] Publish ACFS `5c584b728384c4bb37b993ca54903c0dc3ff1087` with the exact
  installer2036b058 pin. Independently read the public checksums.yaml back;
  both public ACFS refs match at18:33:10 UTC. Its YAML hash `5de4a32a` matches
  both generated consumers. Close `.97` only after this public verification.
  Keep `br-quq4r` open: this post-publication installer repair does not prove
  its original version/tag pre-publication or mutation-sensitive release gate.
- [x] Preserve all2,846 tracker IDs and old comments, flush through br and
  verify zero dependency cycles. Release completed source and ACFS reservations.
  The documentation lease is retained only for the final evidence commit and
  then released. The final read-only mailbox probe is healthy with the same
  PID3047470 and reservation parity306/306. No version bump, binary release or
  live-server replacement occurs.

Current raw receipts are under `/data/tmp/am-agency-20261101/`. Neither missing
old logs, the earlier 18-file UBS result (five critical findings), peer release
reports nor this solo review establishes a clean whole-workspace runtime gate.

## October 30 integrated-source verification (02:43 UTC)

The checkpoint rerun described as pending below exposed a real verification
defect. Its second **77/77** result executed the earlier executable even though
the running Nextest process saw all current source bytes. Preserved source
mtimes were older than that cached executable, and the two new checkpoint tests
were absent. That result receives **no current-source validation credit**. The
original executable hash and both receipts remain recorded in `.75` and the
session TODO.

The proposed nightly Cargo checksum setting failed the controlled real
Cargo/RCH reproducer, including a newly primed checksum-mode control. Its
ineffective configuration addition is being removed. RCH's existing source
receipt mode rebuilds the small reproducer correctly, but refuses this project's
parked manifests because sibling inputs are absent. No manifest, source ledger,
assertion or dependency was removed to evade that refusal. `.75` remains open;
an isolated checkout is bounded validation, not a normal-route cache repair.

The latest integrated verification snapshot includes the incoming cancellable
SQLite initializer and bounded ATC outcome modules. All 3,779 tracked non-Beads
files are frozen under manifest `d0743e27…`. Actual native/all-target compiler
namespaces match 3,773 inputs; exactly six historical performance receipts are
absent, with no other missing or mismatched inputs. The GNU Windows production
compiler namespace matches all 3,779. Separate production checking passes on
this snapshot; its source witness was collected after completion, not during
the compiler process. Integrated all-target checking and native database
execution are still pending in this entry.

Other reviewed repairs preserve the original controls: Windows recovery errors
use the existing contextual error variant, file identity checks use comparable
metadata, three locked-owner fixtures read through the still-owning descriptor,
and both bundle rollback tests select the native platform's file-symlink API.
No tests or permission failures are skipped. The Windows OS lock probe is
contract evidence, and GNU checking is compiler evidence; neither executes the
supported native Windows Rust tests. `.76` and `.77` remain unresolved.

Two native attempts expired before running tests because the outer RCH timeout
was configured incorrectly. The active retry uses `RCH_TEST_TIMEOUT_SEC=14400`;
original per-test deadlines and populations are unchanged. An unnecessary
all-target cancellation is also retained as an execution mistake. The older
failed whole-workspace run and all engine, authority, header, retention,
large-archive, native-platform and release holds below remain open. Neither
peer-reported receipts nor this solo review establish independent certification.

## October 29 credential, transport and recovery follow-through (22:37 UTC)

The useful changes through `addc201f` are preserved alongside the incoming
checkpoint work through `b12861ad`. These are bounded repairs; the failed full
workspace result and the engine, platform, durability and performance holds below
remain authoritative.

- Rejected legacy `DATABASE_URL` values no longer appear in diagnostics.
  The retained installed executable reproduced the disclosure using synthetic
  credentials. A fresh development executable refuses the same input with an
  actionable static hint, prints neither credential marker nor the full URL, and
  creates neither target database nor archive. All **66/66** native legacy tests
  pass, including valid SQLite controls and three rejected URL schemes checked
  through both Display and Debug. The full changed-file static scan still reports
  four critical findings on reviewed metadata comparisons/test markers; it is not
  an overall clean scan. `.73` is closed at that original scope.
- GNU tar on the native Windows worker now respects the existing directory
  exclusions in `.rchignore`. Actual extraction succeeds and all 3,774 frozen
  project files match. Six tracked historical benchmark receipts remain included;
  they are not compile-time inputs. `.72` closes the transfer defect only.
- The stdio suite now supplies a private archive as well as a private database.
  The old fixture correctly refused the live mailbox's owner lock. The same
  executable passes the unchanged **18/18** assertions with private storage; a
  fresh solo recheck also passes, with no skips. Tool listing, negotiation,
  malformed requests, project creation and reservation release execute. Listing
  45 tools is not functional testing of all 45. `.74` is closed; the shared
  installed binary and live mailbox were not replaced or repaired.
- Recovery preflight now uses the existing Windows no-follow handle/link-count
  helpers instead of accepting aliased primary files unconditionally. The expanded
  existing test covers both aliases and both promotion attempts, preserving
  bytes, inode identity and candidate permissions. **10/10** native Linux recovery
  tests pass, including real missing-primary reconstruction and parent-swap
  refusal. Linux does not execute the new Windows branch. `br-8asz3` remains in
  progress for native Windows and the original broader parent/engine authority
  criteria; `br-xzgcj` and `br-hb3mk` also remain open.

Workspace all-target, separate production, strict Clippy and formatting checks
pass on merged `addc201f`; both fresh check processes match all 3,777 frozen
inputs. Clippy's older source observer failed on incoming storage changes and is
retained as a failure. The newer `b12861ad` checkpoint changes have been read and
preserved; their required integrated compiler checks and native checkpoint
re-execution are still pending in this entry. The running earlier checkpoint
suite remains bound to `addc201f`, rather than being relabeled as newer-source
evidence. Its actual runner/compiler inputs match all 3,777 frozen files.

The native Windows default-feature core/share build reached its unchanged
14,400-second timeout with **zero observed tests**. RCH could not confirm remote
cleanup on Windows. The remaining original compiler was stopped through a
guard-approved, identity-pinned single-process action; a subsequent native
process query found no Cargo, Nextest or compiler tree. Three ordinary RCH
cancellation attempts timed out. A fresh strict portable request then refused
admission because the worker was unavailable. No drain override, shared-daemon
restart, file removal or substitute platform result was used. `.49` and the
remaining platform acceptance stay open; official native tool installation and
source transfer are not crypto execution evidence.

These closures use fresh solo re-execution against the original acceptance
criteria, not independent review. The honesty review records excessive process
volume; tracker updates and this bridge earn no capability credit. Detailed
current tasks and retained command logs remain in Beads and
`/tmp/am-agency-20261027/remaining-verification-todo-0642.md`.

## October 29 native verification follow-through (06:04 UTC)

This entry supersedes the pending results in the October 28 entries below.
The completed native workspace run executed **20,435 tests: 20,431 passed,
three failed and one timed out**, with 54 existing skips. It is a failed workspace
run, not release qualification. Of the 95 distinct failures retained from the
earlier matrix, 93 now pass; archive-header preservation and the empty-directory
retention deadline still fail. The two additional failures exposed test-fixture
problems, addressed separately without changing production safeguards.

- Cross-project reply archives now retain their existing database parent.
  Seventeen real integration/idempotency cases pass. A fresh development binary
  passes the unchanged 46-check HTTP/CLI/stdio flow: seven database/canonical
  messages, 21 committed canonical/inbox/outbox copies and seven immutable Git
  objects converge in 1.039 seconds within the original 30-second bound. The first
  run of that same binary failed initial database readiness and remains recorded;
  the successful repeat does not establish startup reliability under load.
  `.69` is closed; `.35` retains its original large-archive/300-second requirement.
- Malformed setup TOML exposed an existing bearer through the parser's error
  excerpt. Diagnostics now retain only a fixed category and numeric span. The
  actual CLI refuses the original malformed bytes without printing either bearer
  or changing the file. All 227 native setup cases execute successfully, including
  the original opt-in foreign-owner fixture. `br-3cqr4` remains open for native
  Windows and directory-durability requirements.
- Original `.57`, `.62`, then `.53` close against their named assertions in the
  completed run and current compiler/lint checks. These are scoped fixture/build
  repairs, not additional features or a green workspace. That run recorded the
  mapped-reader fixture's final reclaim failure, the original retention
  timeout, ATC fixture project admission and archive-header preservation.
- Commit `48440b09` repairs the ATC fixture's private project precondition and
  isolates the sole intentional opaque same-user reader with Nextest's existing
  full-capacity reservation. Two-core same-binary probes demonstrate that the
  opaque fixture makes an unrelated mapping test refuse reclamation; before and
  after that fixture, the original mapping assertions pass. All original reader
  protections, refusal assertions and deadlines remain. The first focused native
  run passes all five reader protections and seven project
  guards, while ATC reaches a second fixture defect: its setup retains one
  request's writer lease across independent tools, blocking a cold bootstrap.
  The final fixture retains the real pool and releases that bootstrap request
  lease. All **13/13** cases now pass with two test threads, including original
  grants, denials, deadlock detection, release and independently reopened durable
  same-project/other-project resolution assertions. Final all-target, separate
  production, strict Clippy, formatting and changed-server static checks pass;
  all 3,770 frozen project inputs match the actual compiler namespaces. `.70`
  and `.71` are closed. The earlier 12-pass/one-fail run remains recorded.
  The whole native workspace has not been rerun after these test-only repairs.
  No production cache/barrier fix or performance credit is claimed.
- The three bounded procfs buffer allocations do not establish a speedup. Both
  retained baseline and candidate time out on the original 16,641-directory case;
  both separately fail a permission refusal on the original 720-generation case.
  That periodic case passes in the full run, while the empty-directory case still
  times out at 240 seconds. Preserve both histories and keep `.63` open.
- `.52` still publishes runtime snapshot header values 0/0 instead of 231/4242.
  `.46` still needs the qualified published engine; `.49` needs actual native
  Windows execution, which ordinary RCH admission refuses; a later read-only
  status still reports its worker unreachable. No platform override, substitute
  backend or weakened assertion was used. `br-xzgcj` remains open: four real authority controls pass,
  but retained directory authority does not yet span the decisive engine opens.
  `br-hb3mk` likewise still needs ownership held across format detection and open.

Four additional writer-barrier controls pass on retained native producer `70f…`;
their component source matches the current source byte for byte. Held-writer
refusal, fresh idle handoff and original file-generation preservation still work.
This is retained-component evidence: the fresh four-case RCH attempt timed out
in admission without executing tests. It is separate from the current 13-case
pass and supplies no whole-binary, engine-I/O or performance qualification.

Incoming `8c80b6f3` adds a GH353 CI qualifier and unapplied patch artifacts, with
no Rust/Cargo changes or overlap. Its classifier self-test, Python syntax and
pinned patch hashes pass static checks. No candidate patch or native CI workload
was executed here. Its source guard pins `48440b09`; qualification against later
main source needs refreshed inputs and another actual run. Original `.35` and
`.46` requirements remain open; the source guard must stay intact.

The native and compiler receipts identify the actual frozen inputs and distinguish
development/test artifacts from production checks. Changed server-file UBS has
zero critical findings (3,239 warnings, 1,697 informational); the separate setup
scan retains two critical findings on synthetic test-token literals, so there is
no overall clean-scan claim. This is
solo verification, not independent review. Detailed actions and exact local logs
remain in `/tmp/am-agency-20261027/remaining-verification-todo-0642.md` and Beads.

## October 28 integration follow-through (19:18 UTC)

The fixes through `d37756f8` were pushed, then eight incoming commits were
preserved through `972d6f08`. Integration repair `549f9aba` restores the inbox
test build and actual server startup. Fifteen assessed Beads have closed against
their individual criteria; the remaining release and persistence gates stay open.

- `.67`: three old full-row hydration calls now pass the required argument;
  equivalent strict lint repairs preserve the existing fixtures and assertions.
  The original compiler/lint failures remain recorded. Fifteen real inbox and
  priority tests pass. This is build/fixture maintenance, with no performance credit.
- `.68`: registering ordinary and query-specific project templates terminated
  actual HTTP startup at FastMCP final admission. One admitted handler now reads
  the original URI and preserves exact/ancestor resolution and encoded delimiters.
  Prepared current development binary `34c6e21c…` starts HTTP and stdio, advertises
  45 tools/25 templates and exits cleanly. Actual encoded-path, missing-project,
  no-write and invalid-option controls pass; an authenticated retirement probe
  passes nine checks including missing-token refusal and no reactivation.
- The unchanged 46-check live flow produces **45 pass, one fail**. Completed
  passive snapshots show seven DB/seven archive messages, but the cross-project
  reply's archive lacks `reply_to` although SQLite records its parent. The terminal
  deadline observation's `TimeoutError` does not erase that earlier mismatch or
  establish a database outage. `.69` tracks this distinct observed defect; its
  routed-envelope fix and real archive/DB/replay assertions are implemented, with
  native acceptance blocked by repeated remote failures. Original `.35` remains open with its original
  large-archive orphan and unchanged 300-second gate.
- All-target, separate production, strict Clippy and formatting checks pass for
  the `75194424…` integration source. Its completed five-file UBS scan retains
  zero critical, 7,591 warning and 3,071 informational findings. After the reply
  fix and incoming recovery change, all-target and production checks also pass;
  final formatting and the three-file UBS scan pass with zero critical findings
  and 1,432 warnings. Final-source strict Clippy and native results remain
  unconfirmed after repeated SSH/source-authority failures. Do not carry the
  earlier Clippy or full-workspace claim forward to the changed source.
- The current prepared development executable came from a parent build that
  exited 137. It supplies scoped real runtime evidence, not a completed-parent,
  normal-production or release qualification. The earlier native matrix was
  interrupted with its 63 observed failures retained. Subsequent obsolete
  compilation stopped 143 before tests; its remote completion was read directly,
  while RCH acknowledgment still failed. Complete current native acceptance for
  `.53`, `.57` and `.62` remains open; no partial sum is a workspace pass.
- `.52` remains a real snapshot failure: the canonical-engine branch preserves
  header values 231/4242; the runtime branch writes them into WAL but publishes a
  copy with 0/0. No fixture normalization, mixed-snapshot header read or unqualified
  engine adoption was used. `.46`'s qualified engine, `.49`'s native Windows,
  `.63`'s original 240-second retention cases and authority/platform work remain.

The incoming committed-ancestor recovery patch now has compiler evidence and
standard formatting, but its new native regressions remain unverified. Preserve
its fail-closed Git authority checks. RCH failures are retained without local
fallback, lock clearing, drain overrides or peer-process interruption. Detailed
current actions and exact manifests/logs remain in
`/tmp/am-agency-20261027/remaining-verification-todo-0642.md` and Beads. This is
self-verification; no independent runtime or application-performance claim is made.

## October 28 implementation follow-through (12:35 UTC)

The October 27 assessment below is historical. Its actionable crypto, compiler,
schema-rebuild and recovery-owner defects now have implementation and scoped native
evidence. The project still fails the original large-archive recovery and snapshot
metadata gates; these repairs do not establish release readiness.

- Real official-age 1.2.1 interoperability passes through a prepared normal production
  `am`: 18 detailed cases, 32 commands and five publication races, plus separately
  executed missing-input and conflicting-option controls. Exact candidate SHA-256 is
  `3f278c873b714562f395f445f06907b314daf234e5607589bf56b57e855f339b`.
  Passphrase whitespace/TAB, identity/default-output positives and tamper/wrong-secret/
  overwrite refusals retain byte witnesses. `.48` is closed; native Windows publication
  evidence remains required by `.49`.
- The original archive-save failures now pass real save/restore/scope/scrub paths in a
  536/536 CLI library batch. Private snapshot namespace evidence stays outside published
  payloads, and handoff failures retain their stages. `.58` remains open while dedicated
  lower-level handoff/rollback coverage runs.
- Legacy schema migration preserves the exact owned external cursor trigger and later
  columns/indexes/triggers, rejects unsupported definitions before data loss, and retains
  rollback/retry controls. Original decisive DB cases passed; `.60` is closed. This does
  not repair `.52`'s retained `user_version` 231-to-0 engine failure.
- Initializing-owner recovery and failed namespace promotions have real positive and
  rollback evidence (`.61`). The original 400-message startup keeper now uses separate
  cold child processes (`.64`), without presenting mixed live-engine checkpoints as an
  engine-corruption proof. Real unprivileged reader/readonly controls execute through
  retained ELF authority on Linux RCH targets (`.65`).
- A real one-slot pool timeout exposed a separate MCP conversion defect. The mapper now
  preserves the existing pool-exhaustion classification without widening classifiers,
  retries or identity policy. The entire tools library passes 1,160/1,160 with zero
  skipped; `.66` is closed. The merged storage library passes 928/928 with two unchanged
  skips. These are named component results, not their sum presented as a workspace pass.
- Strict all-target, production lib/bins, Clippy and formatting checks pass on final code
  manifest `4f7c83559aabe4d8dd07d4fafecdcf4e51bfad14a5d7dbbfedd85586a9427945`.
  Native tools/workspace snapshot `581ea000…` precedes only three explanatory UBS comments
  and an equivalent ATC branch inversion. Static scans have zero critical findings with
  substantial warnings retained. The full current native workspace lane is still running;
  `.53`, `.57`, `.59` and `.62` remain open for their complete acceptance.
- The release oracle now compares durable contents passively after workers stop, rather
  than polling MCP and trusting equal counts. Forty oracle tests pass; an actual equal-
  count/changed-body negative exposes the former false positive. Candidate and incumbent
  still fail the original release workload; retained orphan 20195 misses the unchanged
  300-second bound. `.35`, `.63`, qualified upstream-engine adoption, native-platform and
  remaining authority/performance work stay open. No application performance win is claimed.

Implementation and scoped receipts are linked from the corresponding Beads. Detailed
logs and the requested granular TODO are retained under `/tmp/am-agency-20261027/`;
they are host-local evidence, not portable release certification. Verification here is
self-reverification with bounded peer source review, not independent runtime certification.

## October 27, 2026 assessment (source aed1b90b; published v0.3.39)

**Agent Mail is a real, useful coordination product, but it is not finished or fully
qualified at the current source.** The ordinary coordination loop works. The remaining
critical work is sustained archive convergence, current-candidate correctness and release
enforcement, plus concrete crypto and ATC persistence-boundary defects and a retained
full-snapshot metadata regression requiring current-candidate confirmation. Fresh validation
also found a CLI test-build failure: the production graph compiles, but the all-target
graph does not.
Finishing a large number of beads is not a substitute for those outcomes.

This section supersedes the October 22 judgment. Older observations below remain evidence
for their named source, executable and workload; they are not current failure counts.

### Scope and evidence identity

- Read all 1,394 lines of repository AGENTS.md, all 2,267 lines of README.md and all
  663 lines of the suite instructions before substantive assessment. Read both vision
  copies and the existing bridge. Three read-only investigations covered every current
  SPEC, the architecture/port/transition plans, the full historical Python structure,
  the release/rollout contracts, verification ledger, and relevant security/doctor audits.
  Identical root/planning copies were compared; divergent sections were reviewed. In
  particular root UPGRADE_LOG.md has 799 lines, versus the planning copy's 114, and contains
  newer native Windows evidence. Historical Search V3 proposals and TUI parity inventories
  are explicitly superseded; they do not create missing-feature claims against new code.
- Source inspected: `aed1b90bc3b2c525ad52e63cefeeb735f2b76fa6`, main. Initially there were
  no tracked edits, only pre-existing Beads WAL-index and RCH directories. Those were
  preserved. This assessment changes only this plan and the Beads graph.
- GitHub's live release API reports **v0.3.39**, published **2026-10-13 22:10:49 UTC**,
  with six platform archives, their checksum files, a release-smoke evidence archive,
  SHA256SUMS and its minisign signature. Local installed `am` is **0.3.38**, SHA-256
  `171713fa43e891f8a3430fc4f0218ff966c78465f53a79eb65253370762dfe3c`.
  Source, installed and published binaries are separate subjects.
- Fresh published-artifact verification downloaded v0.3.39's Linux musl archive and
  verified SHA256SUMS with the installer's pinned minisign key, then the archive digest.
  Its `am` SHA-256 is `aeb222d8efbc9199cc00bb93f1875bca672ded4daabaaf0e6d990f04b34d5805`,
  matching the retained swarm control. Its server binary reports 0.3.39 and has SHA-256
  `a9fd508c0d8ab303651d694e7b3c2d032ab3596d7808d8a6d472d089e90cc8d2`.
- Fresh isolated installed-binary HTTP/CLI probe: **34 checks passed**, including the
  actual send/read/ack/reply/receipt/search/reservation loop, broadcast refusal, exact-key
  replay/conflict, qualified cross-project send and reply-back, four HTTP routes and
  robot/doctor JSON. Twenty-five resource templates were advertised. All seven accepted
  fixture messages reached the archive; SIGTERM completed with exit 0. This is a small
  fixture, not swarm, repair, crypto or current-source qualification.
- The first scratch stdio probe omitted the required `notifications/initialized` and
  correctly received -32600 on operating requests. A causal handshake check reproduced
  that refusal and then obtained tools after the notification. Its initial failed
  receipt is retained as a harness error, not a product regression. A subsequent scratch
  assertion also incorrectly treated absent optional `isError` as true. The final saved
  harness fixes those two protocol mistakes and explicitly retains the pre-initialized
  refusal as a negative control: **46/46 checks pass on each of installed 0.3.38 and
  signature-verified published 0.3.39**, comprising 37 HTTP/CLI/archive/shutdown and
  nine stdio checks per binary. JSON, stderr and re-executable `bounded_probe.py` are
  retained in the scratch root. These are fresh functional results, not a performance
  A/B, full installer transaction, repair, all-resource behavior or swarm qualification.
- Fresh official **age v1.2.1** execution rejects the exact `-d -p` combination used by
  the share implementation, exit 1. This establishes the external-command defect;
  it is not an executed test of a rebuilt current Rust binary.
- Current source compilation/testing results are recorded in the terminal validation
  note below. Strict source-content/clean-overlay attempts initially refused before
  compilation because RCH treated excluded standalone WASM/fuzz Cargo manifests as
  selected sibling dependencies. An ordinary strict-remote invocation was then used;
  no local fallback or deletion of those manifests was permitted.
- Evidence scratch root: `/tmp/am-reality-20261027T1933Z/`. It retains original failures,
  runtime JSON/logs, upstream-tool diagnostics, GitHub API snapshots and remote logs.
  No shared mailbox repair, service replacement, publication, feature-default change,
  paid provider request, source fix or performance claim is part of this assessment.

### What is working, and where that proof stops

The 12-member architecture is coherent. Native entrypoints feed FastMCP and shared tools;
FrankenSQLite holds live mailbox rows; storage writes canonical Git artifacts through
the write-behind and reconciliation machinery. CLI, robot, TUI and authenticated HTML
consume that state. C SQLite remains in canonical verification/recovery and the isolated
ATC sidecar; it is not the normal mailbox driver. Asupersync is the async runtime.

All sixteen TUI screens construct real implementations. All nineteen robot commands,
the mail UI, search, doctor mutations, import, share, installer and update paths have
substantive code. Placeholder-screen test fixtures, intentional detect-only doctor FMs
and explicit 501s for deferred live browser parity are not fake shipped functionality.
Portable lexical search uses the local Tantivy bridge; optional hybrid integrates
FrankenSearch. Current manifests select registry FastMCP **0.13.0**, contrary to the
active documentation's older 0.10 Git-pin description.

The strongest tracked operational evidence remains the two October 26 release-smoke
receipts under `docs/evidence/release_smoke/`:

| Subject | Ordinary flow/storm/crash | Mixed soak | 60-agent swarm | Archive gap after 300 s idle |
|---|---|---|---|---|
| main `62740a64`, dynamic ELF `0180807e…`, both orders | PASS | PASS | FAIL | 813 / 853 messages |
| published v0.3.39, static ELF `aeb222d8…`, both orders | PASS | FAIL: drain windows/verdict | FAIL | 1,900 / 1,600 messages |

The receipts identify host, load, governor and exact binaries. Both arms retain every
acknowledged message across their deliberate SIGKILL and pass independent stopped-server
integrity checks. That does **not** establish eventual Git recovery-ledger completeness:
the same runs miss archive convergence and encounter tool errors. These compare dynamic
main with static musl release artifacts, so they are operational artifact comparisons,
not isolated algorithm measurements or a competitive benchmark win.

The newer October 27 `br-bx73n` rs27/sw27 comments report substantial maintenance gains:
reservation/release p99 around 1.2–1.7 s, 34–39% more acknowledged sends, and advancing
WBQ windows. `br-9ott5` is legitimately closed on its own named reservation probe.
However, the full trigger was **not met**: send p99 was 6.18/4.41 s against 3.72/3.56 s
for v0.3.39, tool errors persisted, and archive convergence passed only one order.
Those full newer receipts were not yet among the tracked evidence files inspected;
the comments are recorded experimental evidence, not a new independently rerun verdict.
Do not repeat October 22's reservation-refusal rate as today's result or reopen its
closed repair merely because a broader release remains unqualified.

The sw27 attribution records increased closed-loop send waiting after lease waits shrank,
an engine BusySnapshot after restart, throughput decay and two never-visited crash orphans.
It is useful direction, not permission to waive the p99 gate or adopt an unqualified engine
tip. `br-kp1in.46` owns adoption of the engine release containing the relevant upstream
repairs; `.35` still owns archive completeness. The current reconciler already has real
tail/history/retry lanes and generation-bound checkpoints; asking to implement those
again would miss the remaining real-path failure.

### Numbered vision checklist and complete gap coverage

`WORKING` is bounded to the named executed path. `PARTIAL` means implemented with an
observed remaining defect or incomplete acceptance. `UNPROVEN` means this assessment has
no decisive current execution, not that code or prior evidence is absent.

| # | Testable goal | Current assessment | Evidence / remaining owner |
|---|---|---|---|
| 1 | Discoverable 45 tools / 25 resources | WORKING for installed discovery and sampled handlers; full current conformance UNPROVEN | fresh HTTP probe; conformance modules and current gate |
| 2 | Send/reply/read/ack and durable receipts | WORKING on fresh isolated small fixtures | final 46-check per-binary probes; broader `.1/.2` |
| 3 | Broadcast refused | WORKING | fresh negative plus real handler refusal; never enable it |
| 4 | DB-to-Git convergence without a helpful client | PARTIAL | tracked swarm gaps and newer orphan evidence; `br-8j6cb`, `.35` |
| 5 | No loss/duplicates/corruption across concurrent failure | PARTIAL | retained acknowledged-ID survival; long-horizon SIGBUS/corruption incidents remain `br-0flbu`, `br-htobc`, `.33`, `.1/.2/.9/.10` |
| 6 | Stable-authority, recoverable mailbox state | PARTIAL | real owner/promotion/generation machinery; `br-xzgcj`, `br-8asz3`, `br-cdsge`, `br-oyget`, `br-3p187`, `br-cxsgx` |
| 7 | Bounded engine/pool resources | PARTIAL | short retained descriptor evidence; `.44/.46`, `br-eru3j`, `br-oiok2`; no extrapolated ten-day heap verdict on this host |
| 8 | Identity/contact/topic/session isolation | WORKING for exercised cross-project path; full opt-in session matrix UNPROVEN | current token-required HTTP lifecycle; `br-sgaee`, current conformance |
| 9 | Leases, symmetric conflicts, TTL and real Git guard | WORKING for sampled tools; whole scale/guard contract PARTIAL | fresh lease lifecycle; closed `br-9ott5` scoped probe; `.19`, `br-gh368` |
| 10 | Product bus and advisory build slots | Implemented; not freshly exercised here | gated by WORKTREES_ENABLED/GIT_IDENTITY_ENABLED; `.1/.2` |
| 11 | Safe bare am launch and explicit setup | Implemented; docs PARTIAL | startup preserves configs; interactive coexistence defaults to ask; `.51` |
| 12 | Safe client credentials/config authority | PARTIAL | real setup implementation and retained Windows positives; `br-3cqr4`, `br-x2jf5`, `br-yfzgx`, `br-17tyc` |
| 13 | Responsive sixteen-screen TUI | Implemented; current interaction/soak UNPROVEN | concrete screen constructors; `br-drvoq`, `br-bdo5g`, `br-l4fk6`; source fixes are not new native receipts |
| 14 | Web inspection and usable private share/export | web sampled WORKING; crypto/full-copy PARTIAL | four fresh routes; **new `.48/.49/.52`**; `.52` starts from retained native evidence |
| 15 | Fast truthful robot/doctor state | WORKING on fresh small fixture; scale PARTIAL | fresh JSON; `.18/.44`, `br-am-health-endpoint-slow-false-fail-45e0e`, `br-eru3j` |
| 16 | Reversible, owner-safe doctor repair/undo | Implemented; current mutation qualification UNPROVEN | seven-op chokepoint and real tests; recovery owner probes required |
| 17 | Scoped lexical and optional hybrid search | lexical sampled WORKING; optional quality UNPROVEN | local Tantivy plus model-bound semantic paths; `.7/.8` |
| 18 | Useful real optional summaries/TOON | PARTIAL proof | heuristic/provider/encoder paths exist; stub lanes do not establish quality; `.7/.8` |
| 19 | Quiet, bounded and private ATC learning | PARTIAL | shadow/write-off defaults; real append/resolve; **new `.50`**; `br-au76r`, `br-hwney`, `.2` |
| 20 | Signed install/update and advertised native artifacts | Implemented; complete current delivery acceptance PARTIAL | public v0.3.39; `br-bx73n`, `br-nq2kb`, `br-54v1u`, `br-quq4r`, `br-p9zor`, `br-s1ejh`, `br-r1say`, `.45` |
| 21 | Legacy import and deliberate Python/crates.io transition | PARTIAL / decisions tracked | real import; `br-lkhxw`, `br-dbt24`, `br-hb3mk`, `br-ajiq8`, `br-95spu` |
| 22 | Complete current correctness and installed parity | **FAIL: current CLI test graph does not compile; production graph PASS** | four fresh E0599 errors now `.53`; retained full-copy red `.52`; `br-bx73n`, `.3/.4/.28`, `br-ah0au` |
| 23 | Current latency/capacity/resource budgets | PARTIAL | retained release-bound failures, dated budgets; `.5/.6`, `br-qbhs0`, `br-v0ucm` |
| 24 | Accurate active guidance | PARTIAL | **new `.51`**; newer dependency/startup/format/auth contradictions |
| 25 | Privacy-safe public WASM replay | Implemented; current bundle qualification UNPROVEN | `br-f9avw/.10`, `br-mq9q1`; live browser mirror stays deferred |

The native Windows setup evidence is real: root UPGRADE_LOG records 1,873 native core
tests on SURFACEBOOKJE. It does not prove Windows installer interruption recovery or
today's release. Likewise the macOS production st_dev fix is present; `br-ah0au` remains
open for native test/Clippy proof, not proof that the old source compile bug still exists.
The web CDN gap has narrowed to the Tailwind exemption under `.43`; the old description
of fifteen mutable, unhashed scripts is no longer current.

### New uncovered gaps and granular bridge

Before this pass, JSONL had **58 open, 22 in progress, 2 blocked, 2,514 closed and
208 tombstones**: 82 unfinished. An ordinary import restored the missing local
materialization row; no recovery files or database were rebuilt/deleted. The graph
already covered every major feature family, but lacked active ownership of three
concrete boundary defects, a retained full-snapshot metadata regression, the freshly
observed CLI test-build failure and current user-facing documentation drift.

The local materialization also retained nine equal-timestamp CLOSED records despite
committed JSONL's OPEN status. Export exposed that disagreement. Supported `br reopen`
restored the already-authoritative committed status, with the old closure reasons
preserved in reconciliation comments. This is no new decision against peer work or
fresh closure validation. Existing comments were preserved; importer-renumbered comment
IDs are metadata changes, not added or lost comment text.

| New bead | Priority | Specific implementation and decisive acceptance |
|---|---|---|
| `br-kp1in.48` | P1 | Repair real age passphrase exchange. Official age rejects current `-d -p`; stdin-only secret piping is also not its terminal protocol. Prove actual CLI successful/wrong-secret/tampered-file paths, preserve original fixtures and output refusals, and stop arbitrary encrypt errors silently passing the real-dependency lane. |
| `br-kp1in.49` | P1 | Make crypto/signature publication atomically no-replace under retained parent authority. Absence preflight followed by ordinary rename/write can overwrite a concurrent result or follow a raced signature symlink. Causal raced-destination/parent tests, complete concurrent winner and real crypto positives must pass. This race is source-supported, not a claimed executed exploit. |
| `br-kp1in.50` | P1 | Enforce bounded ATC content validation at actual append/resolve/context mutations. Runtime bypasses insert_experience validation; even row.validate omits nested outcome.evidence. Real sidecar negatives must leave state/rollups unchanged after reopen. This is a missing fail-closed invariant, not evidence of a live secret leak. |
| `br-kp1in.51` | P2 | Correct existing launch/format/dependency/search/auth guidance against source/help and locked graph. Preserve historical ambitions, current safety and functionality. This is documentation work, never feature closure. |
| `br-kp1in.52` | P1 | Reproduce and repair full archive copy losing user_version/application_id. `br-uwvwz` records deterministic native user_version=0 versus expected 231 on earlier main cuts; the current guarded VACUUM INTO copy still lacks metadata preservation. The existing populated schema/unknown-payload test must pass without weakening its 231/4242 assertions; current execution remains separate from that retained failure. |
| `br-kp1in.53` | P1 | Restore the CLI test build without removing its real pagination test. At robot_commands.rs:29295/29300/29301, SQL `BigInt`/`Text` expressions resolve to the test module's serde_json::Value. The current all-target check emits four E0599 errors. Preserve the real database traversal and invalid-window assertions; run them after correcting the import/qualification. |

Each bead embeds source context, reasoning, implementation scope, realistic positives,
a negative a naive fix fails, E2E logging, and closure evidence. Implementation and tests
stay in one logical bead, as the suite's scope-splitting prohibition requires.

#### Search rendered-text retention (adjacent to `br-kp1in.44`, October 27)

Source review at `789efef153c3aaa458c96980f260e0d1eef85073` found that the search
screen independently retained rendered Markdown and composed detail text until each
map exceeded 512 entries. Full message bodies were preserved, with no byte admission
limit. This is a concrete cache-retention gap, not attribution of the historical
8.8 GB daemon observation and not a measured memory or latency improvement.

The commit containing this note replaces those maps with one fixed-capacity FIFO:
8 MiB of accounted retained storage, at most 2 MiB per admitted render, and 512 entries
across both layers. Accounting includes actual line/span vector capacities, owned text
and hyperlink string capacities, the Text allocation and Arc counters, and queue
metadata. The ownership-preserving vector roundtrip is checked against pinned
`ftui-text 0.7.0`; the implementation documents that dependency. Admission inspection
stops after the bounded prefix needed to reject an entry, moving the remaining text
back intact. Oversized entries remain available for complete display without evicting
ordinary cached entries in the current theme. Hits clone an Arc; eviction is bounded.

Theme changes release both old render layers. Detail-context changes release the old
detail generation while keeping reusable Markdown, including when the generation
counter is saturated. The actual screen rendering path now requests composed detail
once, avoiding a second Markdown parse per frame when an oversized body bypasses
retention. JSON mode and complete message bodies remain unchanged. The budget covers
cache ownership; full result bodies, active caller Arcs, transient widget/rendering
allocations, allocator bookkeeping and process RSS remain outside it.

Seven new regressions exercise both cache layers below the old count threshold,
combined entry pressure, eviction and full-body reconstruction, oversized bypass,
theme/detail churn, saturated generation invalidation, and spare vector/string/link
capacity accounting. Weak references assert that obsolete allocations are released.
Two actual `MailScreen::view` frames assert one Markdown render per oversized request
and a visible final-body canary after scrolling. An existing small-hit test now checks
Arc reuse in both layers. All fixtures are synthetic and in memory; these are authored
regressions, not executed runtime proof.

**Validation remains OPEN.** Pinned nightly formatting and the scoped formatter check
passed, as did `git diff --check`; independent production and test source review found
no further concrete blocker. Six strict `RCH_REQUIRE_REMOTE=1 rch exec -- ...` attempts
for both compiler graphs, Clippy, workspace formatting, the search-screen nextest
selection and full workspace nextest all stopped with exit 127 because `rch` is absent.
UBS and Beads import were also unavailable. No compilation or runtime test ran. The
six-hour paired TUI-on/off soak, incumbent comparison and heap attribution required by
`.44` remain necessary before closing that bead.

#### Sidecarless migration ownership review (`br-hb3mk`, October 27)

The same source review confirms that format detection probes activity/ownership locks
and returns `()` before the canonical/raw open and format read. Retaining the existing
CLI mutation locks would exclude cooperating Agent Mail callers, but bare engine opens
do not consult them. Pinned `fsqlite-vfs 0.4.11` explicitly makes an absent-namespace
`ReadOnlyUnadmitted` lease hold no locks; `ExistingCompanion` refuses absent authority,
while `Shared`/`ReservedExclusive` create namespace files. Its generation-transition
guard requires existing authority and writes a prepare record. The packaged source is
[namespace.rs at 3bb28e73168991913974c1a7b4bb0414b48765fa](https://github.com/Dicklesworthstone/frankensqlite/blob/3bb28e73168991913974c1a7b4bb0414b48765fa/crates/fsqlite-vfs/src/namespace.rs).

Migration source remains unchanged and the bead remains open. A complete repair needs
an offline reservation honored by engine admission, or an explicit change to the
contract permitting coordination artifacts for sidecarless `--check`. A second probe
or retained CLI-only lock does not prove the required mixed-engine exclusion. No live
mailbox or migration family was opened during this review.

#### Restored forward-tail catch-up (`br-kp1in.35`, October 27)

Source review at `629d9acfe7458fd281b349a2cbe40a75f0a61cdc` found a remaining
scheduling gap alongside the existing retry/checkpoint/startup-history machinery.
A saved descending history cursor can finish its round while the saved forward cursor
is still far behind newer eligible mail. A full healthy forward page then reported no
backlog, returning the worker to minute-long polling before it reached unseen orphans.
This is a source-supported interleaving, not attribution of the historical swarm IDs.

Forward selection now reads at most 17 eligible IDs and admits the original 16. The
extra ID is only a lookahead; it authorizes no payload read or archive publication.
When the tail watermark actually advances but that successor remains ahead, the
existing catch-up hint stays set independently of history completion. Exactly-full
terminal pages, initial newest-page selection, cancellation and blocked admission
do not invent forward backlog. Cooling duplicate IDs can advance the cursor without
another repair attempt. The 32-attempt, four-repair, 16 MiB batch bounds, retry backoff,
normal grace, server turn limits, publication checks and checkpoint schema remain.

Four new tests cover this change. The server test uses real mailbox and Git operations
to publish a checkpoint at tail 9009/history 9000, builds a healthy higher-ID backlog
without altering that checkpoint, adds DB-only orphan 9042, and reconstructs the cursor.
The first two healthy forward pages must continue before the orphan is repaired, and
the finite backlog must return to idle. It compares every message/recipient column,
including read/ack state, and all 74 canonical/outbox/inbox copies against raw Git HEAD
blobs. Three storage controls cover initial/exact-end pages, cooling cursor progress,
and cancelled, saturated-retry and grace-only passes. This is an authored cursor-restart
regression; it does not claim an executed process-crash or release-smoke result.

**Validation remains OPEN.** Pinned nightly formatting and scoped `--check` passed for
both changed Rust files, along with `git diff --check` and independent source review.
Strict RCH attempts for both compiler graphs, Clippy, workspace formatting, storage
reconciler tests, server catch-up tests and full workspace nextest all stopped with
exit 127 (`rch` absent). UBS and Beads import were also unavailable. No Rust compilation
or runtime test ran. The named release-build swarm/SIGKILL/300-second convergence
acceptance remains unrun, and `.35` and its broader convergence owners remain open.

#### Recovery promotion follow-up (`br-8asz3`, October 27)

At `629d9acfe7458fd281b349a2cbe40a75f0a61cdc`, the retained-parent recovery helper
still handled an unsupported atomic no-replace rename by checking absence again and
using an ordinary replacing rename. A competing destination could appear after that
second check. This remaining source defect is separate from the already implemented
directory anchoring and hard-link-alias rejection recorded on the bead.

The commit containing this note removes that fallback for primary quarantine and
candidate activation. The anchored atomic operation now either succeeds or propagates
its original error and context. Existing rollback, receipt and admission rules retain
control after refusal; no replacing rename is substituted when the primitive fails.

Three public `promote_recovery_candidate` tests cover seven disposable fixture cases:
`NOSYS` and `INVAL` selectively injected at quarantine and activation, real-kernel
regular-file and dangling-symlink collisions after the last destination inspection,
and successful real-kernel promotion. They retain source/candidate inode witnesses,
the exact candidate bytes after receipt preparation, and the pending intent bytes.
Safe refusal/rollback must preserve that intent as aborted; successful promotion must
retain the original generation in quarantine and commit a verifiable receipt. Fixtures
are retained. Selective syscall injection is not native unsupported-platform proof.

**Validation remains OPEN.** The pinned nightly formatter parsed/formatted `pool.rs`
and its scoped `--check` passed, as did `git diff --check`. Source review found no further
concrete blocker. Strict RCH attempts for both compiler graphs, Clippy, workspace
formatting, focused promotion tests and full workspace nextest all stopped with exit
127 because `rch` is absent. UBS and `br sync --import-only` likewise could not run.
No compilation, runtime test or native-platform qualification is claimed; the bead
remains open. No live mailbox or recovery family was modified during implementation.

#### Crypto publication implementation follow-up (`br-kp1in.49`, October 27)

The implementation in the commit containing this note replaces crypto output
preflight-plus-rename/write with one retained publication boundary. Encryption,
identity decryption, in-process passphrase decryption and signature creation retain
the output parent and private staging handles. External age receives held input and
output files through stdin/stdout. New outputs publish through an atomic no-replace
hard link; a concurrently created regular file or symlink remains untouched. Private
stages use mode 0700 with 0600 files on Unix and are retained on success and refusal.
No temporary destructor follows an old pathname to remove artifacts after a parent
substitution. A late namespace change or failed post-publication confirmation returns
an explicit unconfirmed result and retains the original-directory artifacts.

Authorized signature overwrite remains supported. Linux/macOS exchange and native
Windows replacement retain the displaced object and compare its identity, streamed
content hash, size, mode, modification time and link topology with the captured target.
A separate candidate link survives replacement. Recovery detaches and inspects the
current final object before restoring a retained object with no-replace semantics;
it does not blindly overwrite a third writer. Unsupported replacement platforms fail
closed. Windows replacement releases staged-file handles before the exclusive native
open and reopens the retained candidate afterward; directory ancestors remain pinned.
These platform implementations still require native execution evidence.

Six new test functions exercise the public APIs across raced regular destinations,
dangling/outside symlinks, bounded concurrent winner/loser ordering, early/late parent
substitution with a recreated staging-path trap, changed authorized overwrite targets
and Windows directory pinning. Existing child-failure and authenticated-stream negatives
now assert retained private partial evidence. The ordinary overwrite positive independently
verifies the new Ed25519 signature and retention of both candidate and displaced object.
Unix child shims establish causal interleavings only; real-age positives remain required.
The existing `e2e_share.sh --crypto-only` lane adds five real CLI/age publication cases,
including two different completed ciphertexts, structured process completion receipts,
no-follow inode/hash/link witnesses and bounded retained-artifact inventories.

**Validation status: OPEN.** The pinned `nightly-2026-08-31` formatter parsed/formatted
`crypto.rs`, and its subsequent scoped `--check` passed. `bash -n`, the crypto matrix
and generated-wrapper Python AST checks, Cargo manifest/lock TOML dependency checks,
and `git diff --check` passed. Seven strict `RCH_REQUIRE_REMOTE=1 rch exec -- ...`
attempts stopped with exit 127 (`rch` absent): production and all-target compiler graphs,
Clippy, workspace formatting, required real-age crypto tests, full workspace nextest,
and the separate `.52` snapshot reproduction. UBS and `br sync --import-only` also
stopped with exit 127. No Rust compilation, runtime tests, real CLI matrix or native
Windows validation ran; no passing safety verdict or bead closure is claimed. The
snapshot implementation is unchanged because its required current-candidate reproduction
could not execute. Suite guidance at `/data/projects/AGENTS.md` is absent here.

#### ATC persistence implementation follow-up (`br-kp1in.50`, October 27)

Commit `2a31869504f045cd03e04997bf4782d7188eba30` adds the shared bounded content
validator at direct append, outcome resolution/replacement and context/reason/state
amendment boundaries in both storage implementations. Nested outcome evidence and
decoded JSON keys/values are checked before serialization. Validation bounds dynamic
content to 64 KiB, 4,096 visited fields/values, 16 JSON levels and 4,096 bytes per string
(the existing 256-byte summary limit remains). Merged context and replacement outcomes
are checked against the complete row inside the write transaction. Atomic execution
capture and observed-outcome resolution prevent rejected content leaving an intermediate
Dispatched or Open state. Valid first-insert-wins behavior and rollup APIs are retained.

The server integration in the commit containing this note uses those atomic APIs and
removes rejected input from diagnostics. Across the implementation there are 25 new
test functions: 11 core validator tests, 12 file/memory public-entrypoint tests, a real
server sidecar seam test and an opt-in HTTP test on disposable mailbox/storage fixtures.
The persistence tests independently reopen the sidecar and compare all experience and
rollup columns after rejection; positive metrics, outcomes and idempotent replays remain
covered. The HTTP fixture also inspects the operator inventory. Default shadow/write-off
configuration is unchanged. These are authored regressions, not executed runtime proof.

**Validation status: OPEN, not closure evidence.** The pinned `nightly-2026-08-31`
formatter (`rustfmt 1.10.0-nightly`, runtime `9085017724`) parsed/formatted all five
changed Rust files; a subsequent direct `rustfmt --check` and `git diff --check` passed.
Strict `RCH_REQUIRE_REMOTE=1 rch exec -- ...` attempts for the production compiler graph,
all-target compiler graph, Clippy, workspace formatting, focused core tests, DB lifecycle
and rollup tests, server seam test and HTTP test all stopped with exit 127 because `rch`
is unavailable. UBS likewise stopped with exit 127 (`ubs` unavailable). No Rust compilation
or runtime tests ran, and local formatting is not a substitute for the strict remote
gates. Suite guidance at `/data/projects/AGENTS.md` was absent in this environment.
Keep `.50` open until the named gates and real fixture tests pass on the exact candidate.
No live mailbox or existing sidecar was modified during this implementation session.

##### Atomic message-feedback follow-up (`br-kp1in.50`)

Source review of `a03883ffff666cb4ff43c5c681c8f671c9e0139a` found a remaining
lifecycle race: an acknowledgement could fetch an Executed/Open row, lose to a read or
overdue resolution, then receive a successful no-op from the generic resolver. The
server's separate resolved-row branch never ran, so its documented acknowledgement
precedence could be lost. This is a source-confirmed interleaving, not an executed
current-binary reproduction.

The follow-up in the commit containing this note adds `resolve_atc_message_outcome`.
Both storage implementations select, validate and decide precedence in the same write
transaction. Acknowledgements upgrade read/overdue outcomes; duplicate acknowledgements
preserve the first accepted timestamp/evidence; late reads and overdue observations
cannot downgrade them. Unrelated resolved outcomes retain their existing semantics.
The generic resolution APIs remain first-resolution-wins. The shared bounded validator
also checks the complete replacement candidate before replay/no-op decisions.

The server feedback and overdue sweep consume the committed effect receipt. Only an
initial resolution updates the resolved/open counters; upgrades and no-ops do not count
another lifecycle completion. Incoming feedback content is checked before shadow,
lookup-retry, missing-row or terminal diagnostics, and failure logs retain static
categories instead of supplied metadata or opaque errors. Tests force competing writes
between the server fetch and mutation, exercise both database implementations directly,
and reopen the private sidecar to compare stored outcomes, timestamps and rollups.

There are six additional authored regressions: four paired file/memory database tests
and two server tests. The server interleaving matrix uses separate mailbox directories
for all eight cases because ATC uses a fixed sidecar basename within each directory.
The diagnostic regression captures tracing fields for synthetic agent/project canaries
against missing/resolved rows in live/shadow mode. These tests have not executed here.

**Validation remains OPEN.** `git diff --check` passed, and the pinned nightly
formatter parsed/formatted the three changed Rust files. Ten fresh strict
`RCH_REQUIRE_REMOTE=1 rch exec -- ...`
attempts stopped with exit 127 (`rch: command not found`): production and all-target
compiler graphs, Clippy, workspace formatting, core validator tests, DB lifecycle/rollup
tests, server feedback tests, the prior server content-boundary test, opt-in HTTP test,
and full workspace nextest. UBS also stopped with exit 127. No compilation or runtime
test result is claimed. Suite guidance at `/data/projects/AGENTS.md` and the `br`
executable are absent here. This follow-up does not close `.50` or change ATC defaults. Only
disposable fixtures are used by the regressions; no live mailbox was accessed.

**Would finishing all pre-existing open/in-progress beads close the gap? No.** It would
leave `.48/.49/.50/.52/.53` and the current guidance errors uncovered; literal execution of stale
release tasks would also keep targeting the wrong publication path. After these additions
and refinements, the graph covers the known bounded product goals. It cannot promise no
undiscovered defect or universal zero latency/corruption across all future workloads.

Execute the bridge in this order, with independent repairs proceeding concurrently:

1. **Restore the current test build** (`.53`, S). Correct the SQL/JSON value qualification,
   retaining the original populated pagination test. Run the all-target, production,
   Clippy and existing focused/full test gates; a compiler fix alone is not a runtime result.
2. **Finish accepted-message archive convergence** (`.35`, `br-8j6cb`, M). Prove every
   accepted message's canonical/content/recipient witnesses after SIGKILL and bounded
   idle drain, without client reads/resends. Retain useful existing retry/checkpoint
   code; diagnose the never-visited IDs rather than reimplementing it. Pass the named
   release-smoke swarm probe; queue progress or equal aggregate counts alone is insufficient.
3. **Qualify the actual dependency candidate** (`.46`, `br-bx73n`, M–L). Confirm which
   published engine contains the observed BusySnapshot/hydration repairs, then compare
   real candidate/control on the same host/invocation. Keep p99, errors, offered/admitted
   load, completion throughput and resource slopes together. Current ordinary checks,
   full nextest and mandatory production graph must have terminal receipts at that cut.
4. **Repair the concrete write/copy boundaries** (`.48/.49/.50/.52`, M each). Use existing crypto,
   snapshot and DB seams; preserve safe positives, no-clobber, header/schema authority and idempotent state. Direct entrypoint
   and post-insert-update negatives prevent a preferred wrapper from becoming an illusory
   invariant. No new framework, runtime, unsafe exception or default ATC promotion.
5. **Enforce DSR acceptance before publication** (`br-kp1in.22`, `.3/.4/.45`,
   `br-nq2kb`, `br-bx73n`, existing artifact owners, M). DSR with strict RCH is already
   the documented policy. Wire the actual publisher to complete candidate-bound gates,
   installed parity, exact tag/version, platform packages, signatures, container inputs
   and ACFS installer checksum. An Actions-only static check cannot certify DSR.
6. **Complete selected surface/quality lanes** (existing owners, S–L). Run real TUI
   input/resize/traversal/flash/soak, owner-safe doctor undo, legacy import/reopen,
   optional model/provider quality, and public-replay privacy/WASM gates on their actual
   selected artifacts. Keep optional unselected features separate from lexical acceptance.
7. **Correct the active docs** (`.51`, S). Explain explicit setup and interactive attach
   choice; TOON-at-TTY/JSON-when-piped; FastMCP 0.13; local lexical/optional hybrid; and
   configured versus absent HTTP authentication. Do not change behavior to fit prose.

### Ambition rounds and Bead regeneration

Applied the frozen Phase 3a generation instruction retained verbatim in the September 4
workflow below. Regeneration uses `br` exclusively and revises existing owners instead of
creating another assessment epic.

1. **Make capacity and waiting distinguishable.** Faster leases can move a fixed client
   population into the send queue. Extend existing `.5/.6` with fixed offered/admitted-load
   controls and measured queue/service time alongside closed-loop runs. Preserve the
   original release p99 and errors; a capacity gain cannot erase a failed latency gate.
   This was incorporated into the existing measurement/test beads, without another bench.
2. **Put safety at the actual durable boundary.** The crypto and ATC defects share a
   pattern: a preflight or preferred wrapper cannot constrain a later mutation that bypasses
   it. New tasks require atomic publication or shared validation at every actual mutation,
   followed by independently reopened byte/state witnesses. This improves the existing
   implementation plan without inventing a general privacy or filesystem framework.
3. **Bind delivery to what will execute.** Keep exact source/lock/features/executable and
   publisher consumer as one candidate. Update `.22` from an obsolete venue choice to
   enforcement of accepted DSR policy. Keep historical Windows/retained release evidence
   useful within scope. A clean short smoke cannot retire the long-running incidents.

The regenerated graph adds `.49 -> .48` for the complete passphrase positive;
`.2 -> .50` for effect-enabled mixed-workflow certification; and
`br-bx73n -> .35/.45/.48/.49/.52/.53/br-nq2kb` for supported archive/crypto, strict-build
and actual publisher acceptance. Arrows mean
left depends on right. ATC default-off lexical acceptance is not silently forced to
claim live learning. Historical privacy tasks are related provenance, not reopened.
`.22` remains policy/retarget work upstream of `br-nq2kb`; requiring it to execute its
own downstream publication gate would be a semantic cycle. Release acceptance depends
on the actual enforcement task, so policy-only closure cannot qualify publication.

### Refinement passes and terminal validation

The frozen Phase 5 instruction retained verbatim below was applied to each pass:

1. **Current truth and ownership:** checked all 25 goal rows against source and active
   JSONL. Retained the closed reservation improvement, corrected stale macOS/Windows/CDN
   descriptions, and preserved every peer status/assignee. No duplicate implementation
   owner or replacement feature family was introduced.
2. **Behavior and decisive dependencies:** new tasks carry real crypto/sidecar positives,
   causal no-clobber and update-boundary negatives, nonempty test counts and sanitized
   structured logs. Tests and code are one unit; optional absence cannot masquerade as
   a required-path pass. `.5/.6` now distinguish load migration from service-time effects.
3. **Graph and execution order:** added the causal edges above and historical privacy
   provenance; retained independent authoring of tests and existing owner tasks. `br dep
   cycles --json` reported zero active cycles after each structural batch.
4. **Coverage and scope:** the additional security review found conflicting auth-default
   prose, incorporated in `.51`. Current trusted-local behavior remains unchanged.
   Full adaptive-state restart replay remains an existing `.2` acceptance question,
   not a new feature mandated by a checked historical TODO.
5. **Independent challenge:** found and corrected an advisory-only strict-build link,
   added the actual publisher prerequisite, and kept `.22` from waiting on its downstream
   gate. Made the auth discrepancy explicit in `.51`'s standalone description. The
   retained full-copy header failure had no active owner, so `.52` now owns its existing
   populated probe, preserving the distinction between old execution and current source.
   The fresh compiler result supplied `.53`, with the original real tests preserved.
   No existing test expectation, product behavior or release threshold was relaxed.
6. **Final convergence check:** readback, dependency/duplicate review and `bv` validation
   found no further substantive change. Independent read-only review confirmed the
   coherent `.22 -> publisher -> final acceptance` ordering and no active blocking
   cycles. No unexecuted runtime claim was promoted by this review.

### Terminal validation and handoff

| Check | Actual terminal result | Meaning |
|---|---|---|
| `cargo check --locked --workspace --lib --bins` | **PASS**, strict remote hz4, exit 0, 18m31s | Current default-feature production graph compiles. This does not compile its test targets or qualify a packaged release. |
| `cargo check --locked --workspace --all-targets` | **FAIL**, strict remote hz4, Cargo exit **101**, four CLI lib-test E0599 errors | SQL bind expressions `Value::BigInt`/`Value::Text` resolve to JSON Value in the existing real pagination fixture; `.53` owns the repair. |
| `cargo fmt --all --check` | **PASS**, exit 0 | No formatting drift at the assessed source. |
| Full workspace nextest attempt | **CANCELED**, strict remote vmi1153651, exit **143**, **zero tests executed** | Stopped the owned cold compilation after the independent all-target failure proved the required test graph cannot pass. No selected/full runtime verdict is inferred. |
| All-target Clippy attempt | **CANCELED BEFORE ADMISSION**, exit **130** | No Clippy execution or green result. The known test-compile blocker must be fixed before the required complete gate. |
| Installed 0.3.38 bounded real-binary workflow | **46/46 PASS** | 37 HTTP/CLI/archive/shutdown and nine stdio checks on private fixtures. |
| Signature-verified published 0.3.39 bounded real-binary workflow | **46/46 PASS** | The actual public musl executable, not merely a version-matching local build. |
| Rust AST and text placeholder scans | no `todo!()` / `unimplemented!()` matches | Inventory evidence only; substantive data-flow review and actual runs establish behavior. |
| Scoped UBS for changed Markdown/JSONL | **exit 3: nothing scanned** | Unsupported file types; not a scanner pass. No no-scan override or suppression used. |
| Graph and change review | **zero active cycles**, no duplicate suggestion on new beads, whitespace clean | `br` mutations, `bv --robot-triage`/`--robot-suggest`, semantic JSONL comparison. |

The all-target wrapper initially reported unconfirmed completion while releasing its
source-authority lock. Exact-job recovery subsequently acknowledged **the underlying
Cargo exit 101**. Recovery's own exit 0 is successful result retrieval, not a passing
compiler check. The canceled nextest job was separately recovered with exit 143 and
terminal acknowledgment. No automatic replay or local fallback was used; no owned
runtime probe server remains running. Source/lock and critical inspected files matched
the worker; the local product source stayed at aed1b90b throughout.

The final durable graph has **2,810 records: 64 open, 22 in progress, 2 blocked,
2,514 closed and 208 tombstones**. That is **88 unfinished**, versus the initial 82,
because six previously uncovered defects/drift items now have explicit owners. There
is no implementation-completion claim in that increase. Existing status, assignee and
priority values match the initial committed JSONL; no original ID or comment text was
lost. The nine materialization reconciliations preserve earlier closure evidence.

All requested assessment phases are complete: full governing-document reading,
vision-to-code/runtime map, coverage audit, in-place bridge, initial Beads, three ambition
rounds, regenerated Beads, five refinement rounds and a final no-change convergence
check, then `br`/`bv` validation. The next implementation is the small `.53` test-build
repair, followed by the actual named gates and archive/crypto/ATC/release work above.
The current project remains unfinished; this assessment does not implement those fixes,
close their beads, change a gate/default, publish a release or repair the shared mailbox.

## October 22, 2026 assessment (shipped v0.3.39; origin/main 127defb6)

**For a small or medium team, Agent Mail still does what the README says, and v0.3.39 is
measurably better than v0.3.38 at swarm scale.** At 60 agents it acknowledges about twice as many
sends, hits zero 30 s dispatch timeouts, and loses no acknowledged message across SIGKILL.

It still fails the README's central swarm promise, on the same checks that failed on October 8:

- reservation calls take 14–20 s at p99;
- the archive is still 172–850 messages behind SQLite 300 s after load stops;
- the write-behind queue (WBQ) has windows where it stops draining.

Main is 300 commits past v0.3.39. Those commits fix many user-reported issues, but they also change
the reservation failure from *slow* into *refused*, unless the L2 admission wait ships with them.
The long-lived reference daemon has a problem with no bead: an 8.8 GB heap that keeps health red at
idle.

This section supersedes the October 8 judgment.

### What was actually run

- **Reading.**
  - AGENTS.md and README.md, in full.
  - The shared checkout `/data/projects/mcp_agent_mail_rust` is 656 commits behind origin/main and
    carries 6 local "preserve" commits. It was not touched.
  - Every claim below was re-verified at origin/main `127defb6` in a detached worktree. Two of the
    three sub-agent audits first read the stale checkout, and their findings were re-checked.
- **Three read-only code audits** (sub-agents, spot-checked):
  - surface counts and stubs;
  - web, share, guard, product bus, cross-project mail and the proof gate;
  - search, ATC, the release path and CI.
- **Black-box receipts.**
  - The committed release-smoke receipt
    `docs/evidence/release_smoke/2026-10-13_a4ec56bc-release_vs_v0.3.38_swarm-only_idle-host.json`.
    It covers 6 runs in the order A B B A A B, with 60 agents, 480 s of load and a SIGKILL at about
    242 s. Its v0.3.39 binary (`aeb222d8…`) is byte-identical to the published, minisign-verified
    `x86_64-unknown-linux-musl` `am`.
  - The paced-swarm A/B receipts from 10-21 and 10-22 (`fg22`, `fl23`), measured on main builds.
- **Live host, read-only.** MCP `health_check`; `/proc/<pid>/status` and `smaps_rollup` of the 8765
  owner; a storage-root listing.
- **Tracker and GitHub.** Beads were imported from origin/main's JSONL into an isolated database.
  All 23 open issues were mapped to beads.
- **Not run.**
  - A full test gate on `127defb6` (see "Gate").
  - A paced swarm of the shipped v0.3.39. It was queued, but the run requires a host load below 8 and
    the load stayed between 40 and 50.
  - The interactive TUI, semantic search, mutating doctor verbs and the WASM replay.

### Findings, ranked by user impact

1. **Swarm-scale coordination still fails on the shipped release.**
   - P1; owners `br-9ott5`, `br-kp1in.13`, `br-kp1in.35`, `br-qbhs0`.
   - Receipt above, idle host (load 2.5–5.4 for v0.3.39):

     | Arm (3 runs each) | Acked sends | Sends/s | Reserve p99 | 30 s timeouts | Archive gap 300 s after load |
     |---|---|---|---|---|---|
     | v0.3.39 | 6,413–7,322 | 13.0–14.8 | 14.2–19.9 s | 0 | 172 / 358 / 850 |
     | v0.3.38 | 686–3,252 | 1.2–6.5 | 30.0 s | 10–180 | 60 / 80 / 80 |

   - Every v0.3.39 run fails the same 5 swarm checks: both reservation p99 bounds, zero tool errors
     (one `RESOURCE_BUSY`), WBQ progress in every 10 s window, and archive convergence.
   - Every v0.3.39 run passes the rest: send p99 within its 5 s bound, no dispatch zombies, at most 2
     SQLite descriptors, independent `integrity_check` ok, and 20,224 of 20,224 acknowledged sends
     surviving SIGKILL.
   - The archive gap is larger than v0.3.38's because v0.3.39 accepts about twice as much mail
     through the same single drain.
2. **Main turns reservation latency into refusals.**
   - P1; owner `br-9ott5`.
   - Cause: since `3f550ad1` (10-14), a direct reservation publication tries the global fence without
     waiting, and since `fdece065` (10-20) a failed try returns `RESERVATION_ARCHIVE_UNAVAILABLE`.
     Both are unreleased.
   - Paced swarm, refusals per 1,000 `file_reservation_paths` calls, valid runs only:

     | Build | P6 (below saturation) | P2 (saturation) |
     |---|---|---|
     | main + FG (`baea07e2`) | 513 | 942 |
     | main + FG + L2 | 13.2 | 694 |

   - L2 waits up to 500 ms for the fence. It lost its own pre-registered rule (P6 ≤ 10, P2 ≤ 0.5×).
     It lands by the owner's decision, after its gates pass on `127defb6`.
   - A v0.3.40 cut from main without L2 or better would regress reservations from slow to refused.
   - Every lever measured inside the global-fence design has lost or been marginal:
     - lost: bundle staging, skip-publication (×2), reconciler grace, L2 alone, L2 on FG;
     - landed by decision: FG.
   - The fence exists so that snapshot publication sees no in-flight archive mutation, yet every
     mutation in every project takes the same `std::sync::Mutex`
     (`storage/src/lib.rs:818-847`, `:867`).
   - The next lever (`fy24`, a short give-way to waiting direct writers) has a test that passes
     3 of 3 and a planted negative that fails 3 of 3. It runs under a rule written before any run.
     If it loses, the structural options are recorded on `br-9ott5`.
3. **The long-lived daemon's heap sits above its own fatal threshold, and health blames the wrong
   subsystem.**
   - P1; new bead `br-kp1in.44`.
   - The reference host runs the shipped v0.3.38 glibc `am` as an interactive TUI, up 10 days, for a
     72 MB database with 42,618 messages. It holds:
     - VmRSS 8.8 GB, VmHWM 11.8 GB;
     - 6.7 GB in `[anon:mimalloc]` and about 2.0 GB of other anonymous memory;
     - only 38 MB file-backed.
   - The default `memory_fatal_mb` is 8192, so `health_check` is red at idle, with an idle pool and
     queues and every verdict green.
   - The only red contributor reads "live pool/queue pressure level"
     (`tools/src/identity.rs:2340`). Nothing in health names memory.
   - Shedding is off by default. With `BACKPRESSURE_SHEDDING_ENABLED=true`, this idle daemon would
     shed sheddable tools.
   - GH#353's reconciler bursts (1–4 GB each) are one candidate cause, and their fixes are on main,
     unreleased. The bead requires attribution before any fix.
4. **Convergence and retention defects are still visible live.**
   - Owners `br-kp1in.35`, `br-kp1in.36`; the live host is on v0.3.38, so main's fixes cannot show
     there.
   - SQLite is ahead of the archive by 29 messages, the same gap as on 10-08, and parity still reads
     green.
   - 63.7 GB of recovery artifacts are resident against a 72 MB database (58.0 GB of direct
     backups), with `reclaimable_attention: false`.
   - There are 818 rotated `.bak` files (817 on 10-08). GH#359 documents the hourly producer on a
     running v0.3.39.
5. **Release cadence is now the main obstacle to user value.**
   - Owners `br-bx73n` and new `br-kp1in.45`.
   - origin/main is 300 commits past v0.3.39 (230 by Jeff), yet the CHANGELOG "Unreleased" section
     says "No changes since v0.3.39".
   - The maintainer's own status comments say these fixes are on main and "ship in the next release
     after v0.3.39": #362 (2 of 3 costs) and #353, still open, and #355–#358, #360 and #361, already
     closed. `install.ps1` (#352) is fixed on main and already served from it.
   - GH#350: strict dsr cannot build a release while the Beads-only FrankenSQLite git patch (which
     carries a submodule) exists. v0.3.39 needed an older dsr and a primed host Cargo cache.
6. **No current full-gate verdict.**
   - Known reds on main:
     - `br-zhhq7` (8 db lib failures);
     - `br-3ukrb` and `br-u5biy` (storage);
     - `br-wvn4i` (startup regression);
     - `br-ah0au` (macOS does not compile the db crate);
     - GH#349.
   - All GitHub workflows that gate main are disabled. The release gate is the manual DSR checklist
     (`docs/RELEASE_CHECKLIST.md:9-43`).
7. **Documentation drift, one item user-impacting.** Owner `br-4meup`; the full list is in its
   comment. The user-impacting one: README.md:1313 documents the rejected v1 proof-signing format, so
   an issuer written from the README fails verification. The others:
   - the comparison table claims hybrid search in release builds;
   - ATC "appends experience rows" by default;
   - overseer mail is "Git + SQLite" at send time;
   - the stress table shows p99 6.8 s;
   - `WORKTREES_ENABLED` is described as a build-slot flag only;
   - a stale CHANGELOG, UPGRADE_LOG and release checklist;
   - `robot-docs` says "10 verbs".
8. **The tracker still lags the work.**
   - 69 beads are in progress; 60 of them, including 16 of the 17 in-progress P0s, have not been
     updated since 10-08. 41 of the 74 open beads have not either.
   - 50 beads closed since 10-08. 132 of 417 commits (32%; 46% on 10-08) cite no bead or issue.
   - Some open GitHub issues are tracked only on GitHub, with active fixes by the maintainer: #362,
     #353, #341 and #291. Others are open although their bead closed: #345 (`br-9bwnb` closed 10-18,
     and the unreleased bounded admission it cites is what produces finding 2).
   - `br-kp1in.22` (DSR vs Actions) is settled in practice by `docs/RELEASE_CHECKLIST.md:11-12` and
     README.md:300, but it still ranks in `bv --robot-triage`'s top picks.

### What is verifiably working

- **Surface counts at `127defb6`:**
  - 45 registered tools in 9 clusters;
  - 33 resource handlers over 25 distinct URIs;
  - 16 real screens (the placeholder screen is test-only);
  - 19 robot subcommands and 28 doctor verbs.
  - Broadcast is refused both at normalization and in the handler.
  - There are no `todo!`/`unimplemented!`. The web dashboard's 501 routes and the 38 detect-only
    doctor FMs are documented refusals, not fake data.
- **Shipped v0.3.39, from the receipt:**
  - the flow phase passes in every run;
  - acknowledged mail survives SIGKILL at 60 agents (20,224 / 20,224), integrity is ok, and descriptors
    are bounded;
  - no HTTP restarts or EMFILE;
  - clean SIGTERM exit;
  - send p99 is within bound.
- **Fixed since 10-08, verified in source at `127defb6`** (shipped status as noted):
  - the share viewer's controller ships (`viewer.js`, with DOMPurify loaded under SRI; `br-kp1in.42`;
    in v0.3.39);
  - web search field scopes (in v0.3.39);
  - the guard hook bakes in `STORAGE_ROOT` from `config.env`, and the Python and Rust guard readers
    agree;
  - a refused reservation archive write is now reported to the holder (`db_committed: true`)
    instead of returning silent success (unreleased);
  - root installs (`br-kp1in.38`, GH#339);
  - `INTEGRITY_CHECK_ON_STARTUP=false` no longer disables the whole guard (`br-kp1in.37`, in
    v0.3.39);
  - the project-mailbox foreign-key violation (`br-kp1in.39`);
  - main's inbox read path (`br-kp1in.40`).

### Vision checklist delta (the September 21 25-goal matrix remains the inventory)

| # | Goal | Oct 8 | Oct 22 | Evidence |
|---|---|---|---|---|
| 1 | 45 tools / 25 resources | WORKING (shipped) | WORKING | counts at `127defb6`; flow phase PASS on v0.3.39 |
| 2 | Send/reply/read/ack, receipts | WORKING (shipped) | WORKING (v0.3.39) | flow PASS ×3; 20,224 acked swarm sends |
| 3 | Broadcast refused | WORKING | WORKING | two refusal sites in `messaging.rs` |
| 4 | DB→Git convergence | PARTIAL | **PARTIAL** | v0.3.39 172–850 behind after 300 s; live 29-message gap, green |
| 5 | No lost/duplicate accepted mail | WORKING at 60 incl. SIGKILL | WORKING at 60 incl. SIGKILL | 20,224/20,224; integrity ok |
| 6 | Safe recovery / namespace authority | PARTIAL (not re-verified) | PARTIAL (not re-verified) | authority P0s still untouched |
| 7 | Bounded engine/pool lifetime | WORKING on Linux | **PARTIAL** | descriptors ≤ 2 in the swarm, but live heap 8.8 GB (`br-kp1in.44`) |
| 8 | Identity / contact / topic | WORKING (cross-project) | WORKING (code) | contacts fail closed; proof gate v2; README format wrong |
| 9 | Leases, conflicts, guard | PARTIAL | **PARTIAL, changing shape** | v0.3.39 slow (p99 14–20 s); main refuses (L2: 1.3% at P6, 69% at P2) |
| 10 | Product bus, build slots | WORKING (shipped) | WORKING (code) | gated by `WORKTREES_ENABLED`, under-documented |
| 11 | Bare `am`, agent setup | SUPERSEDED | SUPERSEDED | GH#318 decision |
| 12 | Credentials / client config | PARTIAL (not re-verified) | PARTIAL (not re-verified) | `br-3cqr4`, `br-x2jf5`, `br-6u4hx` open |
| 13 | 16-screen TUI | PARTIAL (not exercised) | PARTIAL (not exercised) | 16 real screens; #338 repaint (`br-drvoq`) open |
| 14 | Web mail, share/export | web WORKING | WORKING (code), not exercised | viewer controller and search scopes fixed |
| 15 | Fast, truthful robot/doctor health | PARTIAL | **PARTIAL** | red at idle, misattributed; parity green on a persistent gap; retention blind |
| 16 | Reversible owner-safe doctor | UNPROVEN | UNPROVEN | not exercised |
| 17 | Scoped Search V3 | WORKING (lexical, shipped) | WORKING (lexical) | release binaries are lexical only |
| 18 | Optional model / TOON quality | UNPROVEN | UNPROVEN | semantic tier not in release binaries |
| 19 | ATC learns quietly | defaults verified | defaults verified; long-run UNPROVEN | `atc_record_outcome` has no production caller; write mode off |
| 20 | Signed install/update | WORKING (unprivileged); root FAILS | WORKING (Linux); release build PARTIAL | root fix closed; `install.ps1` fixed on main; GH#350 |
| 21 | Python import and cutover | PARTIAL | PARTIAL | `br-ajiq8` decision open |
| 22 | Complete gate | did not compile | **NO VERDICT; known reds** | finding 6 |
| 23 | Latency / resource budgets | PARTIAL | PARTIAL | swarm p99 out of bounds; v0.3.39 ~13–15 sends/s at 60 agents |
| 24 | Honest active docs | PARTIAL | PARTIAL | finding 7 |
| 25 | Privacy-safe WASM replay | not re-verified | not re-verified | `br-f9avw` untouched since 08-26 |

### Bead coverage (skill questions 4 and 5)

**If every open and in-progress bead were finished as written, would the gap close?** No, for
three reasons:

- Two observed defects had no bead:
  - the daemon heap and the health misattribution (now `br-kp1in.44`);
  - the strict-dsr release build (now `br-kp1in.45`, GH#350).
- The swarm reservation failure has an owner (`br-9ott5`) but no design known to pass: every
  measured lever inside the global fence has lost. October 8's ambition round 2 (one writer per
  archive object) was never turned into work. It is now recorded on `br-9ott5` as the structural
  option, gated on the swarm-phase crash-under-load receipt, rather than as a parallel bead.
- Most user-visible progress is blocked on a release, not on code. No bead change fixes that; it is
  the maintainer's call.

**New beads** (parent `br-kp1in`):

| Bead | P | Gap |
|---|---|---|
| `br-kp1in.44` | P1 | Daemon heap 8.8 GB, above `MEMORY_FATAL_MB`; health red at idle and blaming pool/queue |
| `br-kp1in.45` | P2 | GH#350: strict dsr cannot build while the Beads-only FrankenSQLite git patch exists |

**Graph changes:**

- related links: `.44`↔`br-kp1in.29`, `.44`↔`br-on1zo`, `br-bx73n`↔`.44`, `br-bx73n`↔`.45`;
- `br dep cycles` is empty.

**Evidence comments added** (no status, owner or priority changed): `br-9ott5`, `br-bx73n`,
`br-4meup`, `br-kp1in.35`, `br-kp1in.36`, `br-ge9o4`.

### Bridge: shortest route to a release users can trust under swarm load

1. **Land L2 (S, in progress).** Its change lines are byte-identical on `127defb6`; the gates run
   there first. Without it, finding 2 ships.
2. **Run `fy24` against main + L2 (S).** The rule is frozen before the runs. If it loses, take the
   structural option on `br-9ott5` (M–L): direct reservation artifacts as a priority lane of the
   single archive writer, so the tool waits on that writer's acknowledgement with a deadline instead
   of racing it for a mutex.
3. **Cut v0.3.40 (maintainer, M).** The candidate must:
   - fix or classify the known reds;
   - pass the release smoke, including the swarm phase, in both orders against v0.3.39 on glibc and
     musl;
   - be no worse than v0.3.39 on every swarm check;
   - ship a CHANGELOG for its 300 commits;
   - include L2.
   GH#350 is either fixed or the dsr workaround is recorded in the receipt.
4. **Attribute the daemon heap (S–M, `br-kp1in.44`):** a 6 h soak of main vs v0.3.39 in the same
   invocation, before any fix.
5. **Upgrade the reference host to v0.3.40 (maintainer).** Then re-read the live evidence for
   `.35`, `.36` and `.44`. The interactive TUI has been the daemon since the systemd unit went
   inactive on 10-12.
6. **Docs (S, `br-4meup`):** fix the proof-format text first.
7. **Owner-led tracker triage (maintainer, not done here):**
   - the 60 in-progress beads untouched since 10-08;
   - `br-kp1in.22`;
   - GitHub issues that are open with a closed bead (#345);
   - issues tracked only on GitHub.

### Ambition round 1: one archive writer, not one archive mutex

The fence is a mutual-exclusion lock standing in for a scheduling decision. The WBQ drain, the
commit coalescer and the reconcilers release and retake it between units of work. Direct
reservation writers poll it from other threads, so who wins depends on timing, not on priority.

- **The alternative:** make the drain the only archive mutator, and give direct writes a priority
  lane with a per-request deadline. Snapshot publication keeps its exclusion, because it runs on, or
  pauses, that one writer.
- **What it buys:** the race becomes an ordered queue with measurable waiting time. That is the
  service level October 8's round 1 asked health to report.
- **What it must not do:** reorder publication relative to recovery promotion. The crash-under-load
  receipt is the bar, not a microbench.

### Ambition round 2: release whenever main beats the release

The swarm-phase smoke already compares two binaries in one invocation. Run it on a fixed cadence
(main vs the latest release, both orders), and treat "main is no worse on every check and better on
at least one" as the trigger for a release candidate. That turns finding 5 from a judgment into a
measured, recurring decision. It adds no new artifact: the receipts already exist under
`docs/evidence/release_smoke/`.

### Ambition round 3 (considered, deliberately bounded)

Considered and rejected for now:

- **Heap-profiling instrumentation in the shipped binary.** `br-kp1in.44` needs attribution once,
  on a scratch soak, not a permanent surface.
- **A formal model of fence scheduling.** The paced swarm reproduces the defect in 5 minutes.

### Gate

At the time of writing: L2's gates are running on `127defb6`, together with a storage-lib baseline
run of `127defb6` itself (results go on `br-9ott5` and in L2's landing commit). There is no full
workspace verdict for `127defb6`. The last classified state is in `br-uwvwz` and `br-zhhq7`.

### Refinement record (Phase 3a and Phase 5 passes)

Both new beads follow the frozen Phase 3a intent. Each carries observed evidence with file:line,
why it matters, the required change, and acceptance that includes a planted negative and an E2E
script with structured logs. Neither needs this document to be understood.

1. **Users and optimality.**
   - `.44` requires attribution before any fix, and forbids "fixing" it by raising thresholds.
   - `.45` offers three paths, so the build-path fix stays the owner's choice.
2. **Duplicates and owners.**
   - `bv --robot-suggest` flags nothing for the new beads.
   - Findings with an existing owner got comments, not beads: the fence design on `br-9ott5`, the
     live evidence on `.35`/`.36`/`br-ge9o4`, release inputs on `br-bx73n`, docs on `br-4meup`.
3. **Dependency semantics.** The new beads are linked as related, not blocking, so `br ready`
   still offers them. The release-acceptance bead names them in its comment.
4. **Tests.** Each acceptance includes a negative that a naive fix fails:
   - for `.44`, a rename-only health fix;
   - for `.45`, re-adding a submodule source must fail the strict build.
5. **Convergence.**
   - A re-read found nothing further to change. `br dep cycles` is empty.
   - `bv --robot-triage` top picks (`br-3cqr4`, `br-kp1in.13`, `br-kp1in.3`) follow graph
     position. The bridge order above follows measured user impact.

### Real-work and honesty disposition

This assessment is user-requested process. Its weight is in:

- the committed shipped-artifact receipt (re-read, with binary identity verified);
- live-host reads;
- source verification at the real origin/main.

It changed no product code, gate, golden, default or configuration. It revised this plan in place,
created two beads, and added six evidence comments. No bead was closed, reassigned or
re-prioritized.

Limits:

- The v0.3.39 swarm numbers are a committed receipt from 10-13, not a fresh run, because the fresh
  run could not start under host load 40–50.
- The memory finding is one process at one point in time, plus a short sample series. The bead
  requires the soak.
- The sub-agent audits read source code; they did not run it.

Left behind:

- detached worktrees `/data/projects/mcp_agent_mail_rust-rc1022r` (audit) and `-rc1022l` (L2
  landing);
- scratch receipts under the session scratchpad.

No shared mailbox was modified.

## October 8, 2026 assessment (black-box acceptance of the shipped v0.3.38; superseded by October 22)

**Agent Mail now does what the README says for a small or medium team of agents,
and the build users actually download is the one that does it.** The signed
v0.3.38 artifacts install cleanly. They run the full README coordination loop
over HTTP and stdio, including cross-project `Name@project` mail with reply-back,
the git guard, product bus and build slots. They pass every phase of the in-repo
release smoke on both the glibc and the static-musl archive, and lose no
acknowledged message across SIGKILL at 16 or at 60 concurrent agents. The live
host daemon, which was leaking toward EMFILE on v0.3.36, holds about 50
descriptors.

What still falls short of the README's central claim, reliability under swarm
load, shows up only at swarm scale and only through the black box:

- With 60 agents, reservation tools take a median of about 17 s and time out at
  30 s, because all archive writes serialize on one global fence.
- Messages accepted just before a crash stay out of the Git archive for hours,
  while health calls the archive "green".
- On a long-lived host the recovery machinery has accumulated 1,280× the live
  database in backups that nothing prunes or reports.

None of these three had a bead.

This section supersedes the September 23 judgment. Its weight is fresh execution
against the signed release artifacts, not a desk review.

### What was actually run

- **Governing documents:** AGENTS.md (1,393 lines at origin/main) and README.md
  (2,138) were read in full. Sub-agents audited VISION.md and the planning
  documents read-only, and I spot-checked their load-bearing claims in source.
  - Source cut: origin/main `d752db3a` (2026-10-08 17:07 ET), 88 commits past
    the v0.3.38 tag.
  - The shared checkout was 218 commits behind with 156 modified files from
    other agents. It was not touched. All reading used detached worktrees.
- **Artifacts:**
  - The v0.3.37 and v0.3.38 `x86_64-unknown-linux-gnu` archives were downloaded
    and their `SHA256SUMS` verified with minisign (key `1BBD79B28BF718D0`).
  - The v0.3.38 `am` (SHA-256 `171713fa…`) is byte-identical to
    `~/.local/bin/am`, which the live supervised daemon runs.
- **Installer:** `install.sh --version v0.3.38 --dest <scratch> --no-service
  --yes --verify` in an isolated `HOME`. It verified the signature and checksum,
  installed in 8 s and passed its self-test. On x86_64 Linux it chose the
  **static musl** archive, which the README platform table does not list.
- **Release smoke** (`tests/e2e/lib/release_smoke.py`, unmodified, release
  bounds):
  - v0.3.38 glibc vs v0.3.37 control: every phase PASS for both.
  - v0.3.38 musl vs v0.3.38 glibc: every phase PASS for both.
- **README probe** (scratch harness, 76 checks + follow-ups) on v0.3.38:
  - surface: `tools/list` = 45 over HTTP and stdio; 25/25 resource templates
    readable;
  - macros, identity lifecycle with tokens, contacts, cross-project send /
    refuse / reply-back, product bus, build slots;
  - 13 `/mail` routes, git pre-commit guard (blocks a peer and allows the
    holder), 12 robot/doctor verbs.
- **Swarm** (scratch harness): 60 agents over 3 projects, mixed tools, SIGKILL
  at 240 s, 487 s of load, 600 s idle, then an independent C-SQLite
  `integrity_check`.
- **Idle latency:** 100 sequential MCP calls and 20 CLI invocations per
  operation, single client.
- **Live host (read-only):**
  - MCP `health_check`, `/proc` of the 8765 owner;
  - timed `am robot`/`am doctor` verbs from the same v0.3.38 binary, so there
    was no version skew;
  - storage-root inventory with `ls`/`stat`/`du`; the live database file was
    never opened.
- **Full test gate** on `d752db3a` through RCH: see "Full gate" below.
- **Four read-only audits** (sub-agents, spot-checked): surface and stubs,
  durability/liveness code paths, GitHub issues and tracker health, VISION vs
  code.

### Findings, ranked by user impact

1. **Reservation tools stall at swarm scale (P1; owner `br-9bwnb`, new probe
   below).**
   - v0.3.38 with 60 agents:

     | Tool | p50 | p99 | Timeouts at 30 s |
     |---|---|---|---|
     | `file_reservation_paths` | 17.3 s | 30.0 s | 59 |
     | `release_file_reservations` | 17.8 s | 30.0 s | 56 |
     | `send_message` | 0.15 s | 5.8 s | — |
     | `fetch_inbox` | 0.05 s | 0.30 s | — |

   - About 3,700 `archive publication fence contended` WARNs were logged, held
     by dispatch-blocking reservation writes at `storage/src/lib.rs:3604`.
   - `health_level` stayed red for the whole run, and archive lag grew linearly
     to 231 s.
   - Throughput was 5.4 sends/s, because agents spend their loop blocked on
     reservations.
   - This is the fence named in `br-kp1in.13`: September's permanent wedge never
     reproduced on production builds, but the fence is still the scaling limit.
   - The 16-client release smoke cannot see this. The README's in-process
     gauntlet ("80+80 mixed reservations + messages, 0 errors") is not the same
     workload.

2. **Crash orphans stay unarchived for hours while parity reads green (P1, new
   bead).**
   - In the swarm, the 22 messages accepted in the 5 s before the SIGKILL (ids
     1596–1617) were still missing from the archive 14 minutes after restart,
     10 of those minutes idle.
   - Each id was deferred exactly once, 12 s after restart, with "publication
     fence busy", and never revisited.
   - Cause: `recovery/message_reconcile/database.rs:797-806` advances the
     in-memory cursor past transient deferrals exactly as past successes. A wrap
     takes about max_id/16 passes × 60 s (about 45 h on the live 43k mailbox),
     and every restart resets the cursor.
   - `archive_db_parity` cannot leave green on a DB-ahead gap with an empty
     queue (`tools/src/identity.rs:883-910`; a test pins a 3,235-message gap as
     green).
   - The live daemon shows the same signature: 29 DB-only messages, queue empty,
     verdict green.
   - No acknowledged message was lost from SQLite, but the durable ledger that
     `reconstruct` rebuilds from does not hold them.
   - The 16-client smoke's crash phase passes (it converged in 80 s) only
     because it writes little after the restart.

3. **Recovery artifacts grow without bound and block automatic backups (P1, new
   bead).** The live storage root occupies 87 GB on disk for a 68 MB database:
   - 817 rotated `storage.sqlite3.bak.<ts>` files (54 GB), produced 1–2 per hour
     by earlier daemons from 2026-09-06 to 19:56Z today; none in the 1.7 h since
     the v0.3.38 restart, too short to conclude;
   - 169 retained proactive-backup staging directories (24 GB, the br-2hpuk
     window);
   - about 70 `corruptN` sidecars, 143 lock quarantines, and 2.6 GB under
     `doctor/`.

   Why it is not reclaimed:
   - Rotation prunes only at cold start, and refuses when the snapshot metadata
     is stale; it has been stale since 09-03.
   - The v0.3.37 staging cap of 3 means every automatic backup is now refused.
   - `am doctor reclaim` and `reclaimable_attention` see none of these families.
     Health reports `reclaimable_attention: false` at 63.7 GB resident.

4. **`INTEGRITY_CHECK_ON_STARTUP=false` disables far more than the startup check
   (P1, new bead).**
   - The documented "fast unblock" stops the whole integrity-guard worker:
     periodic checks, proactive and verified backups, and K4
     checkpoint/ANALYZE/VACUUM maintenance (`server/src/integrity_guard.rs:217-224`).
     It also pins `health_level` at yellow for the process lifetime.
   - The reference host has run with it since a 2026-09-01 drop-in marked
     "remove once br-l1q6z cleanup lands". `br-l1q6z` is now closed, so the
     drop-in's own removal condition has been met.

5. **The live reservation ledger drifts further (P2, owner `br-kp1in.19`).**
   - `am doctor health` exits 1: `reservation_parity drift total=41` (16 on
     09-23), 1 archive-id collision, 20 release-pending rows.

6. **Operator reads are still slow on a big mailbox (P2, owners `br-kp1in.18`,
   `br-es9fm`).** Live, 42k messages:

   | Verb | Time |
   |---|---|
   | `robot search` | 0.63 s (was 36.5 s on 09-23) |
   | `robot overview` | 9.6 s (GH#274 is "fixed") |
   | `robot health` | 4.8 s |
   | `doctor health` | 8.3 s |
   | `robot status` | 2.1 s |

7. **88 unreleased commits, and the README already documents two of their
   features.** A release build of `d752db3a` (`am` SHA-256 `bd377968…`) passes
   72 of the 76 README probe checks.
   - `project:<slug>` delivery works (`via: project`), and the `am inbox-events`
     port bug is fixed.
   - **But every project-addressed send writes a foreign-key violation** (new
     bead `br-kp1in.39`, which now blocks `br-vsj5s`):
     - the cursor event uses sentinel `agent_id = 0` against
       `inbox_delivery_events.agent_id REFERENCES agents(id)`
       (`db/src/project_mailbox.rs:54-61`);
     - independent `PRAGMA foreign_key_check` flags it;
     - `am doctor health` exits 1 ("repairable foreign-key violation; next: am
       doctor repair") after a single send.
   - Project-addressed mailboxes (GH#282) and opt-in session identity (GH#279)
     are real end to end in code, but thinly tested:
     - `resource://inbox` lacks `via`;
     - `am robot inbox`'s local fallback drops project mail;
     - there are no mismatch tests for reply, inbox events, read/ack or
       reservations.
   - `acknowledge_message` queues an ack intent before the session-identity
     check when the database is busy, and the replay path never checks
     (`tools/src/messaging.rs:6107-6158`).
   - Also unreleased: the `am inbox-events` default-port fix (bc75fe5c). v0.3.38
     calls `127.0.0.1:8765/api/` even when `HTTP_PORT` names another server. I
     observed this against the live daemon, which refused with 401. Guard and
     share-redaction hardening (about 25 commits) and search field hints are
     unreleased too.
   - Several commits since 10-04 state that they were written without compiling.

8. **Documentation drift.** In README.md:
   - The two unreleased features above are documented with no marker.
   - The musl artifact that the installer prefers is not in the platform table.
   - The Gemini/Claude stdio snippets conflict with a running `am`: stdio
     refuses with "another Agent Mail server is already serving this storage
     root", by design and undocumented.
   - The `INTEGRITY_CHECK_ON_STARTUP` row is wrong (finding 4).

   Elsewhere:
   - AGENTS.md cites FrankenSQLite 0.4.4 (actual 0.4.10) and agent detection
     0.2.2 (0.3.7). It says only `file_reservation_paths` takes
     `idempotency_key`; send, reply and ack do too. It says bare `am`
     auto-configures clients; it no longer does since GH#318.
   - VISION.md has 5 contradicted promises: bare `am` setup, no runtime C SQLite
     (the ATC sidecar is C SQLite at runtime), robot output defaulting to JSON,
     zero ignored tests (43), and no web UI.
   - `docs/RELEASE_TRAIN_PLAN.md` plans v0.2.x→v0.5.0.
   - Owner: `br-4meup`.

9. **The tracker no longer describes the work.**
   - 167 beads are unfinished: 59 open, 96 in progress, 12 blocked.
   - 38 of the 39 in-progress P0s have no commit since 09-24, and 27 of them have
     not been updated since August.
   - 132 of 288 commits since 09-24 (46%) cite no bead. Only 31 beads were closed
     in that period.
   - Shipped but still open: `br-es9fm`, `br-7znpo`, `br-vsj5s`, `br-30nk1`,
     `br-1m1tv`.
   - Obsolete: `br-gozln` (v0.3.31 metadata).
   - `br dep cycles` is empty.

10. **Open GitHub issues.** Of the 15 open issues, none is fully closed out:
    - 2 are fixed but unreleased (#282, #279);
    - 1 is fixed and released but awaits reporter confirmation (#274);
    - 8 are partial;
    - 4 have no fix (#341 planning, #339 root install, #257 corruption under
      swarm load, and #340's remaining failures).

    Reporters of #264, #258 and #274 were never told that fixes shipped in
    v0.3.37. #339 (a root `curl | bash` aborts after verification because GNU
    tar keeps the archive owner, `install.sh:9133`) had no bead.

### What is verifiably working (shipped v0.3.38)

- **Install and update:**
  - signed, fail-closed installer for unprivileged users;
  - GHCR images `0.3.38`/`latest` (the September note "frozen at v0.3.13" no
    longer holds);
  - six signed platform archives;
  - the installed binary equals the release asset.
- **The README coordination loop, end to end** (probe 71/76; the 5 misses are
  explained above, plus one probe-check bug of mine):
  - register, invalid-name refusal, symmetric-glob conflicts, macros;
  - send, fetch, ack, receipt, reply, mark-read, search with `subject:`,
    summarize;
  - identity lifecycle with tokens (wrong token refused), contacts, product bus;
  - build slots with an exclusive conflict;
  - 25 resources;
  - stdio (`initialize` in 0.4 s, 45 tools, send/fetch);
  - web routes and web search;
  - a real git commit blocked by a peer's reservation and allowed for the
    holder.
- **Cross-project mail as the README now documents it** (closes September's
  finding 4):
  - the handshake welcome is delivered into the target project;
  - `Name@/abs/path` is delivered and acknowledged there;
  - a default reply routes back;
  - an unlinked target gets `CONTACT_REQUIRED`;
  - no placeholder agent is created.
- **Bounded resources and safety under crash:**
  - SQLite descriptors ≤ 64 in every smoke phase and 10–19 in the swarm; the
    live daemon is at 50–52 over 2 h;
  - acknowledged ids survive SIGKILL: 271/271, 248/248 and 227/227 in the
    smokes, 2,613/2,613 in the swarm;
  - integrity ok after every crash;
  - no dispatch zombies, no HTTP restarts.
- **Idle single-client latency** (p50; README February budgets in brackets):

  | Operation | p50 | README budget |
  |---|---|---|
  | MCP send | 48 ms | — |
  | `am mail send` | 76 ms | < 50 ms (over) |
  | MCP `fetch_inbox` | 5 ms | — |
  | `am mail inbox` | 14 ms | < 25 ms |
  | `am --help` | 6 ms | < 10 ms |
  | MCP `search_messages` | 97 ms | — |

  The August era note in `benches/BUDGETS.md` (336 ms / 2.8 s) does not
  reproduce on this host today.
- **Surface counts in README/AGENTS are all true:** 45 tools, 25 resources, 16
  screens, 19 robot verbs, 28 doctor verbs, 42 themes, 65 FMs / 27 auto-fixable,
  12 members. There are no `todo!`/`unimplemented!`. The only real stub is
  `am doctor repair [PROJECT]`, which parses and then refuses with "not
  implemented". The Tokio family is absent from `Cargo.lock`.

### Vision checklist delta (the September 21 25-goal matrix remains the inventory)

| # | Goal | Sept 23 | Oct 8 | Evidence |
|---|---|---|---|---|
| 1 | 45 tools / 25 resources | WORKING | **WORKING (shipped)** | HTTP + stdio `tools/list`; 25/25 resources read |
| 2 | Explicit send/reply/read/ack, receipts | WORKING (small) | **WORKING (shipped, glibc + musl)** | smoke flow; probe |
| 3 | Broadcast refused | WORKING | WORKING | probe, smoke |
| 4 | DB→Git convergence | REGRESSED on HEAD | **PARTIAL** | converges after a 16-client crash; not under sustained load (22 orphans after 14 min); live 29; verdict green |
| 5 | No lost/duplicate accepted mail | WORKING at 16 | **WORKING at 60 incl. SIGKILL** | 2,613/2,613; 100-agent sustained and #257/#278 still UNPROVEN |
| 6 | Safe recovery / namespace authority | PARTIAL | PARTIAL (not re-verified) | 27 August authority P0s untouched |
| 7 | Bounded engine/pool lifetime | REGRESSED (shipped) | **WORKING on Linux (shipped)** | live 50 fds at 2 h; smoke ≤ 64; swarm 10–19; macOS residual unverified here |
| 8 | Identity / contact / topic | PARTIAL + misdelivery | **WORKING (cross-project)**; session identity unreleased with an ack gap | probe; `messaging.rs:6107-6158` |
| 9 | Leases, conflicts, guard | WORKING (tool path) | **PARTIAL** | correct at small scale and through the real git hook; 17 s / 30 s at 60 agents; live drift 41 |
| 10 | Product bus, build slots | not re-verified | **WORKING (shipped)** | probe |
| 11 | Bare `am`, agent setup | PARTIAL | SUPERSEDED (GH#318 decision) | VISION/AGENTS still promise auto-config |
| 12 | Credentials / client config | PARTIAL | PARTIAL (not re-verified) | 3 token-leak P0s untouched since 08-25 |
| 13 | 16-screen TUI | PARTIAL | PARTIAL (not exercised) | #338 open; action-menu fix unreleased |
| 14 | Web mail, share/export | UNPROVEN | web **WORKING**; share hardening unreleased | 13 routes 200 + search |
| 15 | Fast, truthful robot/doctor health | PARTIAL | **PARTIAL** | search fixed; overview 9.6 s; parity, integrity and retention verdicts untruthful (findings 2–4) |
| 16 | Reversible owner-safe doctor | UNPROVEN | UNPROVEN | mutating verbs not exercised; scoped repair stub |
| 17 | Scoped Search V3 | PARTIAL | **WORKING (lexical, shipped)** | body/subject/web search; hint filters unreleased |
| 18 | Optional model / TOON quality | UNPROVEN | UNPROVEN | not exercised |
| 19 | ATC learns quietly | UNPROVEN (long) | defaults verified; long-run UNPROVEN | shadow/off defaults in code and live |
| 20 | Signed install/update | PARTIAL | **WORKING (unprivileged)**; root FAILS | isolated install; GH#339 |
| 21 | Python import and cutover | PARTIAL | PARTIAL | Python repo: banner only (10-08); import fix unreleased |
| 22 | Complete gate | NO_VERDICT | see "Full gate" | release notes: 4 failures (#340) |
| 23 | Latency / resource budgets | UNPROVEN | **PARTIAL** | idle within about 2× of February; swarm p99 out of bounds |
| 24 | Honest active docs | PARTIAL | PARTIAL | finding 8 |
| 25 | Privacy-safe WASM replay | not re-verified | not re-verified | `br-f9avw` untouched since 08-26 |

### Bead coverage (skill questions 4 and 5)

**If every open and in-progress bead were finished as written, would the gap
close?** No.

- Three of the four top findings had no owner: reconciler deferral and verdict,
  recovery-artifact growth, and the integrity-flag scope.
- Neither did the probe that would catch findings 1–2 before users do.
- Many P0s describe August states of code that has since moved. Finishing them
  literally would not close their goals; they need an owner-led triage with
  evidence.

**New beads (parent `br-kp1in`), each tied to an observed defect:**

| Bead | P | Gap |
|---|---|---|
| `br-kp1in.34` | P1 | Release-smoke swarm phase (60 agents, SIGKILL under load); the closing probe for `.35` and `br-9bwnb`; must run on glibc and musl |
| `br-kp1in.35` | P1 | Reconciler skips fence-deferred ids until wraparound; parity verdict green on a persistent gap |
| `br-kp1in.36` | P1 | Recovery artifacts grow without bound; automatic backups refused; reclaim/health blind |
| `br-kp1in.37` | P1 | `INTEGRITY_CHECK_ON_STARTUP=false` disables the whole integrity guard |
| `br-kp1in.38` | P2 | GH#339: root install aborts on archive ownership |
| `br-kp1in.39` | P1 | Project-mailbox cursor events violate the `inbox_delivery_events` FK (blocks `br-vsj5s`) |
| `br-kp1in.40` | P1 | Main's soak inbox reads are ~45% below v0.3.38 in both orders; confirm on a quiet host and attribute before v0.3.39 |

Graph changes:

- Related links: `.35`↔`.34`, `br-9bwnb`↔`.34`, `.35`↔`br-8j6cb`, `.34`↔`.13`,
  `.36`↔`br-3p187`, `.37`↔`.36`, `br-bx73n`↔`.34`.
- Also related: `.40`↔`br-vsj5s` and `br-bx73n`↔`.40`.
- One blocking edge: `br-vsj5s` → `.39`.
- `br dep cycles` is empty.

**Evidence comments added** (no status, owner or priority changed): `br-9bwnb`,
`br-kp1in.13`, `br-8j6cb`, `br-kp1in.19`, `br-kp1in.18`, `br-es9fm`, `br-30nk1`,
`br-vsj5s`, `br-4meup`, `br-7ilwx`, `br-ajiq8`, `br-bx73n`, `br-kp1in`.

### Bridge: shortest route to "reliable under swarm load"

1. **Make the gap visible first (S).** Commit the swarm phase to the release
   smoke. Its first receipt must FAIL on v0.3.38 for findings 1–2: that failing
   receipt is the proof the probe works.
2. **Convergence that converges (S–M).** Make transient reconciler deferrals
   retryable, persist the sweep watermark, and let parity escalate on a
   persistent gap. Probe: the swarm phase's "archived ≤ 300 s after load stops".
3. **Take reservation artifacts off the global fence (M–L, `br-9bwnb`,
   `br-kp1in.13`).**
   - Per-project or per-artifact publication, or reservation artifacts through
     the write-behind queue as messages already are.
   - Never hold an MCP dispatch slot across a fence wait.
   - Probe: swarm reservation p99 ≤ 5 s with zero timeouts.
   - The refuted shared/exclusive fence (`br-9bwnb` history) is not retried.
4. **Bound what recovery keeps (M)** and say it in health and `doctor reclaim`.
   Fix the integrity flag's scope (S).
5. **Release v0.3.39** from a candidate that:
   - compiles its test targets (main does not; see "Full gate");
   - passes the full gate;
   - passes the smoke with the swarm phase, on glibc and musl;
   - resolves `br-kp1in.40`. Ship the unreleased fixes. Ship
   project mailboxes only after `br-kp1in.39` and their listed test gaps close.
   Ship session identity only after its test gaps and the ack-identity gap
   close.
6. **Owner-led tracker triage (S, not done by this assessment).** For the 38
   untouched P0s and the shipped-but-open beads: close with evidence, or reopen
   unassigned. Tell the reporters of #264, #258 and #274 what shipped.
7. **Docs (S, `br-4meup`):** the list in finding 8.
8. **Reference host (the maintainer's call; this assessment did not do it):**
   decide whether the 09-01 integrity drop-in is still needed, and reclaim the
   87 GB of recovery artifacts once the retention bead lands or by hand
   (RULE 1).

### Ambition round 1: convergence and contention as measured service levels

Every September and October finding is a liveness or boundedness failure, and
each one was invisible to the correctness suites. Treat three numbers as
service levels, measured by the swarm phase and reported in `health_check`:

- **Time-to-archive p99 after acceptance.** Little's law on WBQ arrivals and
  drain gives an expected lag. A gap that does not shrink while the queue is
  empty is a defect, not "tolerance".
- **Reservation dispatch p99.** An agent waiting 17 s for a lease is an agent
  that times out and retries, which amplifies load.
- **Recovery bytes / live bytes,** with a projected time to disk-full.

The ATC core already carries anytime-valid e-process/CUSUM machinery. Point it
at these three series and the next regression trips an alert in the operator's
health view instead of in a reality check.

### Ambition round 2: one writer per archive object, not one fence per mailbox

The global `ARCHIVE_PUBLICATION_FENCE` exists so that archive publication and
recovery promotion never interleave. Recovery promotion needs mailbox-wide
exclusion; ordinary publication only needs per-object ordering.

- Ordinary writes take a per-project read-side guard plus per-object ordering.
  Promotion takes the write side.
- That restores parallelism across projects and objects without reopening the
  promotion races that motivated the fence.
- A shared/exclusive variant was tried and refuted in `br-9bwnb`. The difference
  here is the per-object ordering, and it must be proven on the swarm phase
  before it lands (expected-loss reasoning: a wrong fence silently corrupts the
  ledger, so the bar is a passing crash-under-load receipt, not a microbench).

### Ambition round 3 (considered, deliberately bounded)

A formal model of the reconciler and fence (TLA+/model checking) was
considered. It was rejected for now: the defects found here are scheduling and
cursor bugs that the swarm phase reproduces in minutes, and AGENTS.md forbids
adding verification surfaces without an observed defect class.
`br-kp1in.9/.10` keep the bounded history checker.

### Full gate

- `rch exec -- cargo test --workspace --no-fail-fast` on `d752db3a` (worker hz4,
  113 min) **did not compile**.
  - `crates/mcp-agent-mail-tools/tests/session_identity.rs:533` and `:670` match
    `DbError::NotFound` against an `OutcomeError<DbError>` (E0308). The errors
    came in with b56b6573 (10-08 14:24), whose message says its native tests were
    "compiling … in progress".
  - **No test on main has a current verdict.** Cargo stops at the first target
    that fails to build, so later compile errors may be hidden.
- The compiler's suggested pattern (`asupersync::OutcomeError::Err(...)`, the
  idiom already used in `db/src/queries.rs`) was applied in a worktree, and the
  full gate rerun is in progress; its result goes on `br-kp1in`.
- `cargo nextest` was not available on the worker the job was routed to (hz4),
  so this is the README-documented `cargo test` gate, not nextest.
- The last complete counts are v0.3.38's own (GH#340): 18,742 passed, 4 failed,
  each classified.

### Release-smoke A/B detail (same invocation, isolated mailboxes)

Soak = 5 minutes of 2 senders + 2 inbox readers + search + health.

| Receipt (arm order) | Arm | Storm p50/p99 | Soak reads | fetch_inbox p99 | Verdict | Host load |
|---|---|---|---|---|---|---|
| v0.3.38 vs v0.3.37 | v0.3.38 | 0.34/0.77 s | 42,943 | 23.5 ms | PASS | ~10 |
| | v0.3.37 | 0.32/0.81 s | 5,075 | 221.8 ms | PASS | ~10 |
| musl vs glibc | v0.3.38 musl | 0.34/0.99 s | 40,255 | 24.7 ms | PASS | ~10 |
| | v0.3.38 glibc | 0.37/0.98 s | 40,537 | 38.3 ms | PASS | ~10 |
| HEAD vs v0.3.38 | main `d752db3a` | 0.39/1.39 s | **25,008** | **115.4 ms** | PASS | 31–38 (my probe ran concurrently) |
| | v0.3.38 | 0.36/0.94 s | 43,364 | 23.8 ms | PASS | ~10–30 |
| v0.3.38 vs HEAD (reverse) | v0.3.38 | 0.32/1.48 s | 41,095 | 29.5 ms | **FAIL** (30 s drain stall windows) | 30–80 |
| | main `d752db3a` | 0.39/0.93 s | **22,495** | **124.9 ms** | PASS | 80–388 (another session's numpy jobs) |

What the table supports:

- **v0.3.37 → v0.3.38 maintenance gain:** 8× the inbox reads at a tenth of the
  p99. This is the shared live read pool. It is a release-over-release
  self-speedup, not a competitive win.
- **Main read path, possibly regressed:** about 45% fewer reads and about 4× the
  p99, in both orders. The direction is consistent, but both main arms ran under
  elevated load (`br-kp1in.40`: confirm on a quiet host and attribute before
  v0.3.39).
- **The soak verdict is load-sensitive:** shipped v0.3.38 failed drain progress
  once at load 30–80, the transient fence-stall class of `br-kp1in.13`, and
  passed three times at load around 10. Gates should record host load and treat
  a high-load FAIL as a signal to re-run, never as a pass.

### Dogfooding note

The live mailbox lists two agents active in the last 14 days in this project
(NavySummit on 10-04, CopperGoose today), against 288 commits. The swarm
building Agent Mail is not coordinating through Agent Mail at present.
Bead-owner names on in-progress work (PinkLynx, RainyForest, GentleBeaver,
GreenValley) do not appear as active agents.

### Refinement record (Phase 3a and five Phase 5 passes)

Beads were written with the frozen Phase 3a intent: background, observed
evidence with file:line, root cause, required change, and acceptance carrying
unit tests (including a negative case that a naive fix fails) plus an E2E
script with structured logging. They are self-contained without this document.
The Phase 5 passes used the frozen refinement checklist.

1. **Users and optimality.**
   - `.34` must run on every artifact a user can receive. The installer ships
     musl on x86_64, so the receipt covers musl and glibc.
   - `.35` records live-host convergence as post-upgrade supporting evidence,
     never as its closing probe.
   - Release acceptance (`br-bx73n`) is linked to the new probe.
2. **Duplicates and owners.**
   - `bv --robot-suggest` flagged nothing for the new beads.
   - Findings with an existing owner received evidence comments instead of new
     beads: `br-9bwnb`, `br-kp1in.13`, `br-8j6cb`, `br-kp1in.19`,
     `br-kp1in.18`, `br-es9fm`, `br-30nk1`, `br-vsj5s`, `br-4meup`, `br-7ilwx`,
     `br-ajiq8`.
3. **Dependency semantics.**
   - Probes are linked as `related`, not `blocks`, so implementation is not
     starved by `br ready`. Closure conditions name the probe in prose and
     acceptance.
   - The one hard edge is `br-vsj5s` → `.39`: a feature must not ship while its
     own first use corrupts the integrity report.
4. **New evidence folded in.** The HEAD probe produced `.39`; the both-orders
   A/B produced `.40`, with its confound stated in the bead rather than
   smoothed over.
5. **Convergence.**
   - Re-read all seven new beads and the graph; no further change warranted.
   - `br dep cycles` is empty.
   - `bv --robot-triage` top picks (`br-kp1in.5`, `br-xzgcj`, `br-kp1in.22`)
     follow graph position. The bridge order above follows measured user impact.

### Real-work and honesty disposition

This was a user-requested assessment (PROCESS). Most of its weight is fresh
execution against the signed artifacts:

- two release smokes on three binaries, 76 README probe checks, the installer,
  a 60-agent crash-under-load swarm, idle latency;
- live-host read-only measurements and a full test gate.

It changed no product code, gate, golden, default or configuration.

- The only source edit is a two-line test-pattern fix in
  `crates/mcp-agent-mail-tools/tests/session_identity.rs`. Main's test build
  does not compile without it. It is committed separately, and only once its
  gate runs.
- It revised this plan in place, created the seven beads listed above
  (`br-kp1in.34`–`.40`), and added evidence comments.
- No bead was closed, reassigned or re-prioritized.

Limits stated plainly:

- The swarm harness and the probe are my shapes and live in the session
  scratchpad until the swarm-phase bead ports them.
- The 60-agent run is one run on a shared 64-core host (load about 10).
- Interactive TUI, mutating doctor verbs, semantic search, LLM paths and the
  WASM replay were not exercised.
- The producer of the hourly `.bak` rotation is unverified.

Left behind on the host:

- detached worktrees `/data/projects/mcp_agent_mail_rust-rc1008{,b}` (the
  second holds a 134 MB release target);
- isolated fixture mailboxes, the HEAD release binaries and logs under the
  session scratchpad;
- one doctor dry-run copy in `/tmp/am-doctor-repair-dryrun-owG6iv`;
- an Agent Mail identity (CopperGoose) with a released reservation.

No shared mailbox was modified.

## September 23, 2026 assessment (black-box A/B; superseded by October 8)

**The coordination product is real and its core loop works end to end, but
neither artifact a user can obtain today is fit for sustained multi-agent use.**
The shipped v0.3.36 leaks SQLite descriptors until it fails (EMFILE within one
minute of ordinary mixed load at the default 1,024 limit; the live host daemon
is at 13.9k and will exhaust its manually raised 32k limit around October 1–2)
and never repairs archive copies orphaned by a crash. Source HEAD bounds the
descriptors, but under the same black-box load its archive write-behind drain
blocked indefinitely while health reported archive lag green. The missing
piece is not another feature or audit: it is a bounded, black-box, release-mode
acceptance run between "the fix landed" and "users receive it." Neither defect
is caught by the default in-process stress suites (HEAD: `stress_pipeline` 9/9);
only an ignored sustained 100-agent test times out, and it may be the same stall.
310 commits since v0.3.36 are unreleased.

This section supersedes the September 22 judgment. Unlike the September 21/22
desk reviews, its central evidence is fresh execution by this assessment:
the shipped and HEAD binaries driven side by side on the same host through the
MCP HTTP protocol, as an agent would drive them.

### What was actually run

- **Governing documents:** all of AGENTS.md (1,372 lines), README.md (2,063),
  VISION.md, the suite-wide rules, and this plan's September 21/22 sections were
  read in full. Source cut `e7744d7a` (main = master = origin), tree clean apart
  from untracked `.rch-tmp/`.
- **Tracker:** local `beads.db` was schema v17 while br 0.6.0 requires v19; the
  reviewed, reversible `br doctor migrate-schema` (17→19) was applied (undo id
  `20260923T161910.038668Z-3818208-0`), `br doctor` then reported JSONL/DB in sync
  at 2,720 records. 43 open + 93 in progress + 12 blocked = **148 unfinished**,
  2,364 closed, `br dep cycles` empty.
- **Black-box harness** (session scratchpad, not committed): Python MCP-over-HTTP
  client doing `initialize` → tools; isolated `HOME`/`STORAGE_ROOT`/`DATABASE_URL`
  per server; ports 18765 (installed **v0.3.36**, SHA-256 `e6cf98a3…`, identical
  to the live daemon's executable) and 18766/18767 (**HEAD `e7744d7a` debug build**,
  SHA-256 prefix `c5922763`). Phases: README coordination flow (27 checks), 16-client
  send storms (240 sends), SIGKILL mid-storm + restart + read-back of every
  server-acknowledged id, 10-minute mixed soak (2 senders, 2 inbox readers,
  search, health) sampling `/proc/<pid>/fd`, and archive convergence observed with
  no client reads.
- **Live host (read-only):** MCP `health_check`, `/proc` of the port-8765 owner,
  timed `am robot`/`am doctor` read verbs, `doctor fix --only … --list`.
- **Tests:** a full default-feature workspace nextest was attempted twice and is
  **NO_VERDICT**: the RCH fleet had 3/14 then 0/12 slots (hz3 queue timeout), and
  the local attempt was stopped by me at 59 GB free (97% disk, shared host) before
  linking completed. Focused HEAD suites, run per package through nextest in the
  same private target under a 50 GB free-disk watchdog, are reported below with
  their exact counts. No release, publication or live repair was performed.
- **Source moved during the assessment:** origin/main advanced to `e538b0dd`
  (5 commits, 16:27–16:46 local: agent-discovery routing, late-model search
  recovery, ACK idempotency across outages, committed-archive proof before
  retention pruning with `message_reconcile/database.rs` +640 lines, reservation
  renewal ownership). **No finding here was re-run against them.** The drain wedge
  (`br-kp1in.13`) must be re-reproduced on the newest main before attribution.
- **Parallel read-only audits** (four subagents, results spot-checked): documented
  surface counts, stub/ignored-test scan, 12 open + 6 recently closed GitHub
  issues against HEAD, and all 39 P0 in-progress beads against code.

### Findings, ranked by user impact

1. **Shipped v0.3.36 exhausts file descriptors (REGRESSED in the field; fixed in
   source, unreleased).** Isolated 0.3.36 under the mixed soak: `storage.sqlite3`
   descriptors 277 → 609 → 763 in 60 s; at the 1,024 soft limit (systemd user-unit
   default, and the installer-generated unit sets no `LimitNOFILE`) the process
   logged `No file descriptors available` 597×, restarted its HTTP server 257×,
   failed 58 post-commit durability probes, fell back to archive snapshots for
   inbox reads, and hit `archive HEAD points at a missing/corrupt object … re-root`
   66× (`git fsck` afterwards: only dangling trees); the 10-minute soak ended at
   995 descriptors with 34,960 client-visible errors. Live daemon PID 1685075 (bare `am`,
   TUI; the supervised `agent-mail.service` has been inactive since September 17):
   13,835 → 13,886 descriptors on `storage.sqlite3` over 6.08 days of uptime,
   ≈2,000–2,300/day, soft limit 32,768 (raised by hand after the September 18
   2,048-limit outage recorded on `br-8r6dl`). **HEAD under the identical soak held
   2 descriptors throughout.** Owner: `br-8r6dl`; defense in depth: `br-kp1in.17`.
2. **HEAD's archive write-behind drain wedges under mixed load (NEW, P0, blocks
   release; `br-kp1in.13`, health verdict `br-kp1in.23`).** HEAD instance with a SIGKILL history: WBQ `drained_total` frozen at
   414 from about 16:37Z to the end of the session (≥19 minutes, including after
   load stopped) while `enqueued_total` rose to 3,870 (depth 3,453); no archive
   commit after 16:36:54 while the DB grew to 3,927 messages against 692 archived;
   the `wbq-drain` thread sat in `futex_do_wait` with zero CPU ticks over 10 s
   (blocked, not slow — a debug build cannot explain zero progress). 41
   `fetch_inbox` dispatches hit the 30 s deadline and became zombies holding
   admission capacity (zero successful reads in the 10-minute soak);
   `read-only WAL admission refused pending recovery` ×28.
   Health stayed `archive_lag: green, oldest_unmaterialized_ms: 0` — the lag
   contributor does not observe WBQ backlog. Clean-instance reproduction (no crash
   history): see the reproduction record below.
3. **Crash orphans are never repaired by the shipped binary (fixed in source,
   unreleased).** After SIGKILL, 0.3.36's two kill-orphaned messages were never
   archived (DB 311 / archive 309 at restart; the gap persisted for the ~30 minutes
   observed, with 23 uncommitted archive files); DB→archive
   reconciliation (`br-8j6cb`, `0c906599`/`8b631146`) is not in v0.3.36. **Zero
   acknowledged messages were lost on either binary** (305/305 on 0.3.36, 297/297
   on HEAD, all ids distinct) and full integrity passed after restart on both.
4. **Cross-project messaging silently misdelivers (NEW; had no bead, now
   `br-kp1in.15`).** After a
   successful cross-project `macro_contact_handshake`, `send_message` from project
   A to the linked agent's name auto-registered a placeholder `BronzeHare
   (unknown/unknown)` **in project A**, delivered there, and returned a receipt with
   `persisted: true`; the real recipient in project B received nothing. The
   handshake itself drops `welcome_subject/body` for cross-project pairs with only
   a debug log (`tools/src/macros.rs:765`, "messaging across projects not yet
   supported"). README's Quick Start shows exactly that handshake and says
   separate repos can "link agents, then message directly"; the FAQ repeats it.
   Python never supported this either; the promise is simply unimplemented.
5. **Write concurrency mode is implicit (NEW; had no bead, now `br-kp1in.16`).** VISION/README say
   `BEGIN CONCURRENT` is opt-in and off. That is true only for explicit
   transactions: the pinned engine's autocommit path uses
   `TransactionMode::Concurrent` whenever `concurrent_mode_default` is true, which
   is its default (`fsqlite-core/src/connection.rs:14649, 56669` at `db458bfb`), and
   the runtime never sets `PRAGMA fsqlite.concurrent_mode = OFF` (only two tests
   do). HEAD logged `MVCC write conflict … snapshot conflict on pages` under a
   16-client storm. `pool.rs:2618` still cites "≥10 concurrent autocommit writers
   UNSUPPORTED (bd-9inpb)" while the engine's own test says the bd-9inpb fix
   landed. The effective mode for the durability claim must be decided and pinned.
6. **HEAD admission/latency under writers is unqualified.** 2 of 720 HEAD sends
   returned `RESOURCE_BUSY: database is busy (recovery in progress)` (0 of 480 on
   0.3.36 — not statistically distinguishable at this n, but a release gate must
   measure it); HEAD's engine logged `correlated_exists_fallback` ("in-memory
   fallback path while parity-cert mode is enabled"). Debug-build latencies are
   not release evidence; they are why a release-mode run is mandatory.
7. **CLI reads contend with the live daemon.** On the 42k-message live mailbox:
   `am robot search` 36.5 s (Tantivy `LockBusy` → private-snapshot fallback with an
   `error` alert), `am robot health` 6.6 s, `am doctor health` 7.7 s and exit 1.
   Other read verbs 0.1–2.6 s. GH#298's residual (health_check walks the archive
   per call; CLI read verbs are not proxied) had no owner; now `br-kp1in.18`.
8. **Live reservation archive drift is persistent and semantic.** `am doctor
   health` exits 1 on 16 reservations whose `released_ts`/`active_status` differ
   between DB (4,102 rows) and archive (2,960), the same count as September 22.
   The auto-fixer exists but nothing converges it automatically, and the
   pre-commit guard reads archive state. No mutation was applied (`br-kp1in.19`).
9. **Release plumbing points at a path that never runs.** Repository Actions are
   disabled; releases go through DSR with minisign. Several release-integrity beads
   were satisfied only in `dist.yml`/`docker.yml` (static tests in
   `tests/docs_drift_ci.rs`); nothing on the DSR path consumes a zero-failure gate
   (`br-nq2kb`; v0.3.34 shipped with 12 failing tests). One-shot "landing"
   workflows (`land-atc-reliability-20260918.yml`, `gh326-private-export.yml`,
   `gh274-overview.yml`, `installer-exit.yml`) and `patches/` were committed for a
   CI that cannot run; the GH#326 patch series was never applied (`br-kp1in.22`,
   `br-kp1in.27`).
10. **Smaller verified defects:** GH#329 (`file_reservations release/renew
    --paths/--ids` accept one value) and GH#330 (robot reservation times only
    humanized) are unfixed and unowned; the engine emits
    `WAL-FEC requires a caller-owned native runtime` at WARN once per connection
    open (≈8,000 lines in 20 minutes on each server); 8 config variables are parsed
    and never read (`LLM_DEFAULT_MODEL` — LLM calls use a hard-coded `gpt-5.4` —,
    `LOG_LEVEL`, `LOG_INCLUDE_TRACE`, `DATABASE_ECHO`, `HTTP_OTEL_*`,
    `AM_EPHEMERAL_TTL_HOURS`, `AM_TUI_TREE_STYLE`) and three are parsed twice with
    divergent defaults (`FSQLITE_CONCURRENT_RETRIES` 5 vs 16, `AM_COALESCER_*`);
    `MCP_AGENT_MAIL_LLM_STUB=1` makes release builds return canned LLM output
    (`br-kp1in.24`, `.25`, `.26`, `.20`, `.21` respectively).

### What is verifiably working

- **Surface:** 45 tools (`tools/list` = 45 on both binaries), 25 resource URIs
  (33 registrations incl. query variants), 16 screens, 19 robot verbs, 28 doctor
  verbs, 65 fixers / 27 auto-fixable, 42 themes, 12 members, no Tokio family in
  either lockfile. Zero `todo!`/`unimplemented!`; every tool handler has a real body.
- **Coordination loop (27/27 on both binaries):** register, symmetric-glob
  reservation conflict, release and re-grant, send/fetch/ack/reply,
  delivery receipt, search, thread summary, `resource://inbox`, inbox-event cursors,
  broadcast refusal, invalid-name refusal, `file_reservation_paths` idempotent
  replay and `IDEMPOTENCY_KEY_CONFLICT`.
- **Durability of acknowledged writes across SIGKILL** on both binaries (above).
- **All robot/doctor/mail verbs** answer in 0.02–1.2 s on a small mailbox.
- **HEAD focused suites:** storage `stress_pipeline` 9/9 passed (6 ignored
  skipped), including the 30-agent pipeline, WBQ saturation and 120-agent
  multi-project cases — the in-process sources of README's stress table. Further
  focused-suite results are recorded below.
- **Six GitHub defects have source fixes** (#326, #328, #323, #264, #258, #274
  partial) — none is released. #319/#318/#321 fixes are released.
- **Stale P0 wave:** of 39 P0 in-progress beads, 15 have code plus tests for every
  acceptance item, 21 are partial (often only a missing run or negative test),
  3 are obsolete (`br-ivcmf`, `br-rch-frankensearch-closure-jdgvg`, `br-l1z6f`),
  none is untouched. `br-c2is6` (master mirror) is already satisfied at `e7744d7a`.

### Reproduction record for the HEAD drain wedge

| Instance | History | Drain froze at | Queued at last sample | After load stopped | Health `archive_lag` |
|---|---|---|---|---|---|
| A (port 18766) | SIGKILL + restart, 3 storms, 10-min soak | 414 (~16:37Z) | 3,453 | still 414 at 16:56Z; zombies 41 | green, `oldest_unmaterialized_ms: 0` |
| B (port 18767) | fresh, no crash; flow, 2 storms, 5-min soak | 679 (~16:48Z, ~90 s into soak) | 1,624 | still 679 at 16:56Z; zombies 20 | warning, 104–228 s |
| v0.3.36 control | same workload | never | 2 | drains normally | — |

The wedge is permanent once reached (no recovery after load removal) and does not
require a crash. `am` still accepts and acknowledges sends into SQLite, so no
acknowledged message is lost, but reads time out, zombies accumulate, and the
archive, search backfill and every Git-backed guarantee stop advancing.

### Vision checklist delta (the September 21 25-goal matrix remains the inventory)

Only goals re-exercised in this pass change status; all others keep their
September 21/22 status (`UNPROVEN`/`PARTIAL`) and were **not** re-verified.

| # | Goal | Sept 22 | This pass | Evidence |
|---|---|---|---|---|
| 1 | 45 tools / 25 resources | PARTIAL | **WORKING** (both binaries) | `tools/list`=45 black-box; source counts |
| 2 | Explicit send/reply/read/ack, receipts | PARTIAL | **WORKING** at small scale | 27/27 flow on both binaries |
| 3 | Broadcast refused | implemented | **WORKING** | `BROADCAST_DISABLED` black-box |
| 4 | DB→Git convergence | PARTIAL | **REGRESSED on HEAD (wedge)**, NOT_SHIPPED fix on 0.3.36 | findings 2–3 |
| 5 | No lost/duplicate accepted mail under concurrency | UNPROVEN | **WORKING at 16 clients incl. SIGKILL**; swarm scale UNPROVEN | 602/602 acked ids survive; distinct ids |
| 7 | Bounded engine/pool lifetime | PARTIAL | **REGRESSED in shipped** (EMFILE), **WORKING on HEAD** (2 fds) | finding 1 |
| 8 | Identity/contact/topic | PARTIAL | PARTIAL + **new misdelivery defect** | finding 4 |
| 9 | Leases, conflicts, guard | PARTIAL | **WORKING** (tool path) / live archive drift persists | flow + finding 8, GH#329 |
| 15 | Fast, truthful robot/doctor health | PARTIAL | PARTIAL: fast on small mailbox; 36 s search, 7.7 s doctor health, lag verdict wrong under wedge | findings 2, 7 |
| 17 | Scoped Search V3 | PARTIAL | PARTIAL: MCP search 0.15 s small; CLI 36 s live | finding 7 |
| 20 | Signed install/update | PARTIAL | PARTIAL: installed binary = release asset; 310 commits unreleased | GitHub API, SHA |
| 22 | Complete gate | UNPROVEN | **NO_VERDICT, now explained**: ~100 GB of test executables | `br-kp1in.28` |
| 23 | Latency/resource budgets | UNPROVEN | UNPROVEN; README's 27 ms send baseline is not what users see (2.6–3.5 s observed sequential on a loaded shared host, confounded) | finding 10, `br-kp1in.5` |
| 24 | Honest active docs | PARTIAL | PARTIAL: ten verified drifts appended to `br-4meup` | `br-4meup` comment |

### Bead coverage (skill questions 4 and 5)

**If every open and in-progress bead were completed as written, would the gap
close?** Before this pass: **no**. Nothing owned the HEAD drain wedge, the
cross-project misdelivery, the implicit concurrency mode, the lag verdict, the
runnable full gate, or a bounded black-box release gate — and several release
beads target a pipeline (Actions) that is disabled. The long-horizon soak beads
(`br-kp1in.1/.2`) would eventually have exercised the wedge, but they are scoped
as 24-hour two-host programs blocked on four other beads.

**Vision goals with no bead before this pass (now created, parent `br-kp1in`):**

| New bead | P | Gap |
|---|---|---|
| `br-kp1in.13` | P0 | HEAD archive drain wedge (release blocker) |
| `br-kp1in.14` | P0 | Bounded black-box release smoke with previous-release control (release blocker) |
| `br-kp1in.15` | P1 | Cross-project send misdelivery / handshake welcome drop / README promise |
| `br-kp1in.16` | P1 | Explicit effective write-concurrency mode for autocommit writes |
| `br-kp1in.17` | P1 | Descriptor-limit defense in depth (soft-limit raise, unit `LimitNOFILE`, doctor) |
| `br-kp1in.22` | P1 | Decision: canonical release path; re-scope dist.yml-only release beads |
| `br-kp1in.23` | P1 | `archive_lag` verdict ignores WBQ backlog |
| `br-kp1in.28` | P1 | Consolidate 137 integration-test binaries so the full gate can run |
| `br-kp1in.29` | P1 | Liveness instruments: lockdep-lite, holder dumps, progress/slope watchdogs |
| `br-kp1in.30` | P1 | Un-ignore the passing 100-agent lifecycle test; classify the 600 s sustained timeout |
| `br-kp1in.18` | P2 | CLI read verbs contend with the daemon (36 s search), GH#298 residual |
| `br-kp1in.19` | P2 | Reservation DB/archive semantic drift without automatic convergence |
| `br-kp1in.20` | P2 | Config surface honesty (8 dead + 3 double-parsed variables) |
| `br-kp1in.21` | P2 | `MCP_AGENT_MAIL_LLM_STUB` honored by release builds |
| `br-kp1in.24` | P2 | GH#329 multi-value `--paths/--ids` |
| `br-kp1in.26` | P2 | WAL-FEC WARN once per connection open |
| `br-kp1in.25` | P3 | GH#330 absolute reservation timestamps |
| `br-kp1in.27` | P3 | Repo detritus (removal needs maintainer approval under RULE 1) |

New blocking edges (left depends on right): `br-bx73n` → `.13`, `.14`, `.22`, `.23`,
`.28`; `br-nq2kb` → `.22`. Related links tie the new beads to `br-8r6dl`,
`br-8j6cb`, `br-0flbu`, `br-es9fm`, `br-vsj5s`, `br-kp1in.1/.2/.7`. Evidence comments
were added to `br-8r6dl`, `br-8j6cb`, `br-5lgwn`, `br-4meup`, `br-nq2kb`, `br-0flbu`,
`br-c2is6` (already satisfied) and the three obsolete beads (`br-ivcmf`,
`br-rch-frankensearch-closure-jdgvg`, `br-l1z6f`). No existing bead was closed,
reassigned or re-prioritized; closure is left to owners with the cited evidence.

### Bridge: the shortest route from here to a release users can trust

1. **Unwedge HEAD (`br-kp1in.13`, P0, M).** Reproduce in a spawned-binary test
   (the clean instance B recipe needs no crash), take stacks with the server as a
   harness child (ptrace allowed), fix the blocking wait at its root. Add the
   drain-progress verdict (`br-kp1in.23`) in the same change set so the next stall
   cannot report green.
2. **Commit the bounded black-box release smoke (`br-kp1in.14`, P0, M) in
   parallel.** It must fail today on both artifacts for the stated reasons; that
   failing receipt is the proof the gate works.
3. **Make the full gate runnable (`br-kp1in.28`, P1, M–L).** Consolidating test
   binaries is what turns every future "full gate NO_VERDICT" into a verdict.
4. **Pin the concurrency mode (`br-kp1in.16`)** before the release-mode run, so the
   durability numbers mean something.
5. **Release.** Decide the path (`br-kp1in.22`), run consolidated full gate +
   smoke on a release-mode build of the exact candidate, installed-binary parity
   (`br-bx73n`), publish. This ships the descriptor fix, DB→archive
   reconciliation, and the six unreleased GitHub fixes.
6. **Operational, now (maintainer's call — not done by this assessment):** the
   live daemon PID 1685075 will hit its 32,768 descriptor limit around October
   1–2. A restart resets the count (and the supervised unit is available but
   inactive); the durable fix is step 5. Consider `LimitNOFILE` on the unit
   meanwhile (`br-kp1in.17`).
7. **Then, in parallel:** cross-project correctness (`.15`), CLI read contention
   (`.18`), reservation drift convergence (`.19`), liveness instruments (`.29`),
   config honesty (`.20`, `.21`), GH#329/#330, log flood, docs (`br-4meup`).
8. **Keep the long-horizon program** (`br-kp1in.1/.2/.9/.10`, swarm-scale
   GH#257/#278) — but it is not a prerequisite for shipping steps 1–5.

### Ambition round 1: liveness is a first-class property

Both release blockers are liveness failures — resource exhaustion and a blocked
drain — in a codebase whose ~17k tests are overwhelmingly safety checks. The plan
therefore treats "keeps making progress with bounded resources" as a product
invariant with instruments, not an emergent property: named-lock order recording
and holder dumps so the next wedge diagnoses itself without ptrace; per-queue
progress watchdogs; resource-slope detection against completed work using the
anytime-valid e-process/CUSUM machinery the ATC core already contains; and a
time-to-exhaustion projection ("EMFILE in ~9 days") in `am robot health` and
`am doctor`. The September 18 outage was predictable from data the process
already had. Recorded in `br-kp1in.29`; the release smoke asserts the same
predicates, and gains randomized crash-point iterations (≥20 kills at random
offsets, offsets recorded for replay) instead of one hand-timed SIGKILL.

### Ambition round 2: make verdicts cheap enough to get every time

The recurring NO_VERDICT on the full gate is not bad luck: ~137 integration-test
executables at ~700 MB each is ~100 GB per build, more than this host or most
workers have free. Consolidation per crate (`br-kp1in.28`) and a minutes-long
black-box smoke (`br-kp1in.14`) make a trustworthy verdict affordable on one
machine, which in turn makes small, frequent releases possible (`br-kp1in.22`
records cadence with the path decision). Health reports time-to-drain from
measured arrival and drain rates (Little's law), so "behind but converging" and
"wedged" are different colors (`br-kp1in.23`).

### Ambition round 3 (considered, deliberately bounded)

Lock-order graphs (lockdep), anytime-valid sequential tests for leak slopes, and
Little's-law drain estimates are the mathematically grounded pieces that pay for
themselves here, each tied to an observed defect. A general model checker or a
new chaos framework was considered and rejected for this bridge: the existing
bounded history checker (`br-kp1in.9/.10`) already owns trace legality, and adding
more checks without an observed defect class would be the "conformance
metastasis" pattern AGENTS.md forbids.

### Focused HEAD test results (cargo nextest, default profile, debug line-tables-only)

| Suite | Result | Notes |
|---|---|---|
| storage `stress_pipeline` (default) | **9/9 passed**, 6 ignored skipped, 201.6 s | thundering herd, coalescer batching, inbox during storm, 30-agent pipeline, mixed reservations+messages, 120-agent multi-project, multi-project concurrency, stale git lock, WBQ saturation |
| `stress_100_agent_full_lifecycle` (ignored) | **passed**, 105.9 s | its "un-ignore on fsqlite bump" condition is met → `br-kp1in.30` |
| `stress_sustained_100_agents_60s` (ignored) | **TIMEOUT at 600.1 s** (nominal 60 s) | unclassified: same stall class or debug slowness → `br-kp1in.30` |
| db/tools suites (stress, pool exhaustion, fault injection, idempotency, nocase backup) | queued behind per-package rebuilds at handoff | results, if any, recorded on `br-kp1in` |

The full default-feature workspace gate remains **NO_VERDICT** for the reasons above;
no count is projected for it.

### Refinement record (Phase 3a and five Phase 5 passes)

Phase 3a used the frozen bead-generation instruction retained verbatim in the
September 21 section; Phase 5 used the frozen refinement instruction. Beads carry
background, reasoning, acceptance, unit/E2E tests and logging requirements so they
are self-contained without this document.

1. **Ownership:** `.13`, `.23` and `.29` each proposed a drain watchdog. Assigned one
   owner each (root-cause fix / health verdict / cross-cutting instruments); added
   structured-logging and E2E requirements to `.13`, `.16`, `.19`, `.23`, `.24`, `.29`.
2. **Duplicates and links:** `bv --robot-suggest` flagged nothing for the new beads;
   a manual sweep found no existing owner for test consolidation; linked `.20` to
   `br-yzk37` (inert pragma knobs) and `.14` to `.15`/`.29`.
3. **Dependency semantics:** corrected `.13`'s prose — it blocks release acceptance
   (`br-bx73n`) but deliberately not `.14`, whose failing run against the wedge is
   its proof of usefulness.
4. **Evidence accuracy:** corrected `.14`'s description of the live outage (observed:
   2,048 limit exhausted within about a day of the 09-17 start; the October 1–2 date
   is a projection). Scanned the plan and JSONL for the local bearer token that
   appeared in this session's terminal output: absent.
5. **Convergence:** added `.30` from the final suite result; re-read all 18 new beads
   and the graph; no further change warranted. `br dep cycles` empty; `bv
   --robot-triage`: 166 unfinished, acyclic, top picks `br-kp1in.13` then
   `br-kp1in.14` (previously `br-c2is6`, which is already satisfied).

### Real-work and honesty disposition

This was a user-requested assessment (PROCESS), but unlike the three preceding
desk assessments most of its weight is fresh execution: two binaries driven
black-box through the real protocol, a same-host A/B, a crash test, a soak, live
`/proc` measurements, and focused suites. It changed no product code, test, gate,
golden, default or configuration; it made one reversible local tracker schema
migration, created 18 beads, added 11 evidence comments and 11 dependency edges,
and revised this plan in place. No bead was closed or reassigned; peers' obsolete
or satisfied beads received evidence for their owners to act on.

Limits stated plainly: HEAD was a **debug** build — its latencies are not release
evidence, though the zero-CPU blocked drain thread is a wedge, not slowness; the
full gate is NO_VERDICT; sample sizes for the RESOURCE_BUSY comparison are small;
the harness lives in the session scratchpad (to be ported by `br-kp1in.14`), and
its storm/soak shapes are mine, not a standard workload. Surface counts, the stub
scan, the GitHub audit and the P0 audit came from read-only subagents; I
spot-checked the concurrency-mode, cross-project and reservation-drift claims
myself and reproduced two of them black-box.

What this assessment left behind on the host: a 58 GB private build target at
`~/.cache/rc0923-target` (RULE 1: not deleted without the maintainer's approval),
isolated fixture mailboxes under the session scratchpad, and stopped test servers.
No shared mailbox was modified.

## September 22, 2026 assessment (superseded by September 23)

**Agent Mail is a substantial working product. Its remaining critical work is
reliable operation across failures and delivery of one fully qualified current
candidate. It is not finished, and the latest source is not the released binary.**
This assessment supersedes the dated judgments below without changing their
receipts or treating old failures as present failures.

### Scope and current evidence

- Reread all 1,372 lines of repository `AGENTS.md`, all 2,063 lines of
  `README.md`, and all 663 lines of the governing suite instructions. Reviewed
  the current vision, durability, security, browser, verification and release
  contracts. The full historical plan/spec survey recorded below is reused,
  not represented as a fresh reread of every historical document. Since its
  September 21 source cut, the only changed Markdown files are AGENTS, README
  and this bridge plan.
- Source cut: `41bf1305c5d1554a3e4edc0ad53ff302d8ba7e03`. Both remote branch
  tips matched it during this assessment. The working source was clean;
  existing Beads changes and `.rch-tmp/` were preserved.
- Live GitHub API still reports **v0.3.36**, published September 16 at
  10:14:19 UTC, with 15 uploaded assets. Local `am --version` also reports
  0.3.36; a version string does not establish source or executable identity.
  Main is 305 commits after the local release tag. The release's reported
  17,547 passing tests and 36 skips belong to its pre-version-bump candidate,
  not this source cut. No release was installed or published in this audit.
- Current JSONL contains **44 open, 93 in progress, 12 blocked and 2,363
  closed** issues, plus 208 tombstones. `br doctor` identified a local
  materialization mismatch; ordinary `br sync --import-only` imported the
  missing `br-q8z1r` without changing the existing issues. Unfinished total is
  **149**. Stale titles, ownership flags and closure counts are not correctness
  evidence. No existing implementation owner is displaced by this assessment.
- Fresh live MCP coordination persisted message 42912 for both addressed
  peers; its delivery receipt reports persisted, not signaled or acknowledged.
  The live health call returned `status=ok` and **health_level=red**: critical
  verdicts green, pressure red. WBQ p95 was 1,049 ms; commit-queue p95 was
  4,195 ms. Message inventories were 42,306 archive / 42,335 DB, within the
  configured tolerance, not row-level equality. These are live observations,
  not a controlled performance experiment or current-source test.
- Local `am doctor health` exited 1 for `local_config_unattested` with 32
  reservation-field differences. It did not attest the live server target;
  do not combine that result with the MCP health result. No shared repair ran.
  CASS reports a stale, incomplete August 14 checkpoint, so current history
  comes from Git, Beads and retained receipts, not claimed fresh CASS coverage.
  The bounded current-task CASS search timed out after 20 seconds without a
  result; its index was not rebuilt during this assessment.

### What has materially improved

The following are implemented repairs with retained, bounded evidence, not
additional work to reimplement:

| Change | Evidence and boundary |
|---|---|
| Idle stdio archive reconciliation | `ce784836` starts the existing maintenance worker; real idle and accepted-send/outbox-failure/restart subprocess cases passed at their recorded cuts. |
| Reply metadata survives repair and ordinary Git commits | `7352a3d3`; 80/80 selected tests, including six staged-only replies repaired in fresh processes. This does not prove the transport batch cursor. |
| Live model discovery recovers after late installation | `br-drpda`, published through `169143db`; real registered-model discovery/context/embedding passed. It does not certify full semantic indexing or retrieval. |
| Queue configuration reaches the implementation | `640831a3`, `br-dqbpk`; the enqueue-deadline counterexample and 32/32 focused tests passed. Idle-poll configuration reached its consumer in source review; no separate nondefault idle-poll timing measurement ran. |
| Idempotency retries retire obsolete pooled readers | `167b7815`, `br-9m5il`; 7/7 focused native tests, workspace check and strict Clippy passed on the recorded matching source. |
| Lost lifecycle profile writes have background repair | `41bf1305`; bounded 32-identity/four-repair passes, project locking and reconstruction support exist. Seven isolated helper tests and ten SQL checks are narrower than native DB/Git or transport acceptance; those remain unverified. |

Receipts for the September 22 work are retained under
`/data/projects/am-release-20260912/`, particularly
`20260922-rainyforest-staged-final-receipt.json`,
`20260922-rainyforest-wbq-timing-receipt.json`,
`20260922-rainyforest-idempotency-stale-reader-receipt.json`, and
`20260922-rainyforest-pressure-stdio-receipt.json`.

The pressure fixture is preserved outside the primary source at
`/data/projects/am-release-20260912/wbq-publication-ff0346/` in
`crates/mcp-agent-mail-cli/tests/integration_runs.rs`. Final frozen candidate
`08755a914763a35e50e5c840c03d8f6688d4c2d2` is unpublished. Formatting and scoped
static review passed; **zero native pressure tests executed**. A smaller worker
ran out of memory, the larger worker timed out during compilation, and final
admission was refused. The timeout's cleanup was subsequently verified and
worker health restored. These are infrastructure limitations, not a test pass
or a demonstrated failure of the fixture. No own build remains active.

### Vision checklist and gap coverage

The 12-crate architecture is coherent: CLI/server entrypoints feed FastMCP and
shared tools; FrankenSQLite holds live operational rows; storage queues and Git
hold recoverable artifacts; core owns contracts/configuration; search, guard,
share, robot, TUI and authenticated HTML are consumers. Asupersync is the runtime.
The embedded Beads engine is separate from the mailbox engine. Optional hybrid
source builds and lexical portable releases are distinct products to qualify.

`PARTIAL` below means implementation exists but the complete promise still has
known work or missing decisive evidence. It does not mean the whole surface is
broken. The existing 25-goal matrix below remains the detailed inventory; this
table updates its acceptance groups against current code.

| Goals | Current reality | Remaining work and existing owner beads |
|---|---|---|
| 1–3: tools/resources and explicit messaging | Real handlers and fresh persisted coordination; broadcast refusal remains intentional. Current complete conformance UNPROVEN. | `br-s3xbp`, `br-w9v59.1`, `br-kp1in.1/.2` |
| 4–5: durability and safe retry | DB-first acknowledgement, metadata persistence, bounded repair and stale-reader fix implemented; fault/restart composition PARTIAL. | `br-8j6cb`, `br-q8z1r`, `br-kp1in.2/.9/.10` |
| 6–7: recovery authority and engine lifetime | Real generation, descriptor, snapshot and recovery machinery; latest whole-candidate qualification UNPROVEN. | `br-5lgwn`, `br-xzgcj`, `br-jgieq`, `br-qfvd6`, existing recovery owners |
| 8: identity/contact/topic | Real lifecycle tools and newly wired profile repair; whole imported-mailbox lifecycle round trip PARTIAL. | `br-sgaee`, `br-g6c0z`, `br-qayvs` |
| 9–10: leases/guard/product/build slots | Real coordination; keyed empty-grant conflict race remains a source-supported defect awaiting a runtime reproducer. | `br-q8z1r`, `br-9bwnb`, `br-ssog9`, `br-kp1in.2` |
| 11–12: setup and credentials | Discovery/setup and authority hardening exist; race and exact-install acceptance PARTIAL. | `br-db75q`, `br-fphbm`, `br-6u4hx`, `br-x2jf5`, existing setup/installer owners |
| 13–16: operator surfaces, share and doctor | Real FTUI, HTML, robot, crypto and reversible repair; selected older evidence is substantial, combined candidate UNPROVEN. | `br-ivgot`, `br-l4fk6`, `br-ji2f0`, `br-qdgio`, `br-kp1in.2/.5/.6` |
| 17–18: search and optional quality | Lexical path real; model discovery now tested. Late-model vector-index identity and real transport quality remain PARTIAL. | `br-kp1in.7/.8`, `br-7x5fm`; preserve lexical fallback |
| 19: quiet ATC and learning | Runtime hooks, persistence and snapshots exist; defaults remain shadow/write-off. Sustained mixed-workflow acceptance UNPROVEN. | `br-hwney`, `br-au76r`, `br-kp1in.1/.2` |
| 20–22: release, import and complete gates | Published v0.3.36 exists. New dependency/recovery work is source-only; manual scorecard and installed parity PARTIAL. | `br-5lgwn`, `br-bx73n`, `br-kp1in.3/.4`, `br-nq2kb`, existing legacy/platform owners |
| 23: performance | Historical budgets and real benchmark paths exist. Today's latency/resource targets UNPROVEN; MVCC is still opt-in with an explicit drift warning. | `br-kp1in.5/.6`, `br-eru3j`, health/stress owners |
| 24–25: truthful scope and browser replay | Public replay uses the real screen with synthetic identifying data. Live browser parity is deliberately deferred. Active vision prose still conflicts with newer safe startup/release policy. | `br-4meup`, `br-f9avw.10`, `br-mq9q1`; cutover/crates.io remain explicit decisions |

**Would completing the existing Beads finish the project?** They cover every
current feature family found in this review. Literal execution of stale task
descriptions would not: some ask for already-landed wiring or obsolete dependency
migrations. Completing their updated behavioral acceptance, including actual
installed artifacts and sustained runs, would close the known scoped gaps.
That is not proof against undiscovered bugs. No new feature family or new audit
epic is justified. Missing acceptance connections belong in the existing graph.

### Ordered bridge and granular execution TODO

1. **Finish the current recovery candidate (M, critical).**
   - [ ] `br-8j6cb`: execute the preserved real pressure/restart fixture through
     strict RCH on a memory-adequate admitted worker, using the exact final
     source/features and a functioning nextest runner. Do not restart a cold
     multi-package build merely to obtain another queue refusal.
   - [ ] Verify actual journal-enqueue failure, backlog exhaustion, seven
     accepted messages spanning the four-repair limit, idle restart and exact
     canonical/outbox/inbox Git bytes; no read/resend trigger or read/ack change.
   - [ ] Retain attachment, topic, exact reply, wrong-generation, interruption,
     foreign-path and missing-byte cases from the original acceptance. The
     existing pressure fixture alone does not cover all of them.
   - [ ] `br-sgaee`: execute the new production DB/Git lifecycle tests and a
     mounted retire/restart/unretire/deregister/reconstruct history. Verify
     routing/roster agreement and no resurrection, not only helper serialization.
2. **Fix the remaining exact-retry hole (S–M).**
   - [ ] `br-q8z1r`: deterministically interleave the tool's precheck with a real
     competing DB grant; show the successful empty result currently loses its
     key, then record/replay that result atomically. Releasing the competitor
     must not let an old key acquire a new lease. Preserve changed-payload
     conflicts, partial grants, retention expiry and concurrent same-key winner.
   - [ ] Carry that negative case into `br-kp1in.2/.9/.10`; authoring may proceed
     now, but full workflow certification must wait for the product fix.
3. **Qualify one combined revision (M–L).**
   - [ ] `br-5lgwn`: freeze main, lockfile, feature selection and worker target;
     reuse one admitted remote build lane for focused tests and mandatory
     workspace/all-target check, Clippy, formatting and full nextest.
   - [ ] Reclassify the earlier 36 skips against current dependencies. Preserve
     every failed attempt; selected old green runs do not qualify the latest
     lifecycle/reconstruction additions. No local fallback or inferred ELF.
   - [ ] Existing recovery/setup/import owners supply their named positive and
     adversarial probes at that candidate; do not duplicate their implementations.
4. **Prove the composed user workflow (L).**
   - [ ] `br-kp1in.1/.2/.9/.10`: bounded real stdio/HTTP smoke, then owned faults
     and restart, then the existing two-host 24-hour, 90-client, 13-project,
     300-message/hour profiles. Keep receipts, scopes, cursor semantics, quiet
     ATC, resource slopes and legal operation histories separate from counts.
5. **Qualify optional quality and performance in parallel (M each).**
   - [ ] `.7/.8`: real transport indexing/query after a model appears late;
     preserve model/dimension identity, privacy canaries and truthful fallback.
     Existing real discovery/embedding results are prerequisites, not retrieval
     proof. Unselected optional models do not block a lexical-only candidate.
   - [ ] `.5/.6`: measure offered/admitted/completed work, tail latency, retries,
     archive convergence and FD/RSS growth on the actual selected route. Keep
     cold startup separate from steady state; do not relax historical budgets.
6. **Deliver the verified candidate (M).**
   - [ ] `br-kp1in.3/.4` and `br-bx73n`: finish executed-child identity and
     selected-capability evidence, candidate-bound reliability scorecard and
     installed-binary parity. The mtime-selection bug is already repaired.
   - [ ] `br-nq2kb` and release owners: existing manual DSR/RCH gates, six target
     packages, signatures, actual install/update, applicable containers and
     ACFS checksum refresh. No Actions enablement or publication in this audit.
7. **Correct active guidance while preserving scope (S).**
   - [ ] `br-4meup`: distinguish new DB `archive_metadata_json` from legacy
     unknown reply metadata; current idempotency schemas from old prose;
     opt-in setup from the old auto-rewrite vision; lexical releases from
     optional hybrid; historical skip counts from present acceptance.
   - [ ] Keep deliberate non-goals: broadcast, hostile same-account isolation,
     federation and deferred live browser parity. No new scope is needed to
     finish the current product.

### Ambition round 1: prove the recovery workers together

Separate message and profile passes can each succeed while their shared Git,
locking or reconstruction interactions remain wrong. Extend the existing
`br-kp1in.2` private workflow with both missing message artifacts and lost
retire/deregister profile writes in one mailbox. Use enough accepted messages
and changed identities to cross each four-repair bound; keep adding ordinary
activity while older records wait. Observe actual progress for both classes,
then restart and reconstruct privately. Verify exact reply/topic/recipient
metadata and lifecycle authorization together, preserving BCC and registration
token confidentiality. Heartbeat-only changes must not create commit churn.
This extends the existing runner and product tests, not a second chaos system.
`br-8j6cb` and `br-sgaee` retain their original acceptance and cannot close merely
because a helper or one type of repair passes.

### Ambition round 2: check the contract the user actually observes

Use the existing bounded history checker to distinguish durable results from
fresh observations. A replay must preserve message or grant identity, original
expiry and the recorded empty/partial result; its live conflict snapshot may
legitimately change. Never infer reacquisition from an old expired grant or a
durability guarantee from an acknowledgement receipt alone. Challenge that
predicate with `br-q8z1r`'s competing grant/release schedule and independent
hand-labeled histories, alongside actual retained DB/Git evidence.

For source-to-release completion, bind acceptance to the executable that will
ship, including its feature and engine closure. Reuse one admitted build and
the existing scorecard/parity machinery; do not create a new certification
framework. The current helper-only lifecycle validation and unexecuted pressure
fixture make combined candidate qualification a real prerequisite of release
acceptance. Keep the larger two-host swarm goal distinct from the selected
release policy: this audit neither silently adds a new release gate nor waives
an existing one. Runtime capability, static checks, qualification and publication
remain four different outcomes.

### Refinement passes

1. **Scope and superseded assumptions:** rewrote `br-8j6cb`'s current
   description to stop requesting completed stdio wiring. It now distinguishes
   new transactional reply metadata, legacy unknown metadata, preserved staged
   evidence and the unexecuted transport-pressure fixture. All original positive
   and negative acceptance remains; no task was closed or reassigned.
2. **Behavior and test realism:** checked production message metadata, the
   reservation conflict return, real lifecycle test setup and semantic bridge
   construction. Added an actual late-model indexing/query case to `.8`, with
   lexical fallback and vector-space negatives. Isolated embeddings, plausible
   backend names and empty results cannot qualify retrieval. The model-dimension
   concern is source-supported and still awaits a runtime reproducer. Existing
   pressure coverage cannot absorb attachment/reply acceptance it never executes.
3. **Causal dependency order:** added four blocking edges with `br` only:
   `.2 -> br-q8z1r`, `.2 -> br-sgaee`, `br-bx73n -> br-5lgwn`, and
   `br-bx73n -> br-kp1in.4`. The left task depends on the right. Mixed-workflow
   certification now explicitly waits for the remaining retry/lifecycle
   contracts; installed-candidate acceptance waits for combined qualification
   and its evidence tests. Harness authoring need not wait. No blanket epic
   barrier or new sustained-run release prerequisite was introduced. The
   post-batch `br dep cycles --json` check reports no active cycles.
4. **Evidence and release scope:** reread the retained pressure and timing
   receipts and the current release requirements. Corrected the queue summary
   to separate measured enqueue timing from reviewed idle-poll wiring. The
   pressure receipt's original candidate limitation predates its final-candidate
   integration record; the final `08755a91` includes `41bf1305`, but neither
   candidate ran its pressure tests. Live health, unattested local doctor output,
   stale CASS, historical release tests and current source remain distinct.
   Selected lexical and optional hybrid acceptance stay separate; existing
   release gates and larger project goals are neither combined nor waived.
   Updated `.3/.4`'s stale descriptions: exact producer-receipt/run/digest
   selection is already implemented. Their unchanged acceptance now clearly
   targets executed-child identity, complete joins and real adversarial tests;
   manual release venue selection is no longer presented as unresolved.
5. **Convergence:** reviewed the affected task descriptions, acceptance,
   comments and graph after those corrections. No further scope, test or
   dependency changes were justified. All 14 updated issues retain their
   status, assignee, priority, original acceptance fields and prior comments;
   no issue was created, removed or closed. All referenced IDs resolve and
   JSONL has no duplicate IDs. Final `br dep cycles --json` reports zero cycles;
   `bv --robot-triage` also reports an acyclic graph with 149 unfinished issues.
   Its 124 structurally actionable items include in-progress work and stale
   titles, not 124 unowned implementation opportunities. Its top mirror-sync
   suggestion is already satisfied at this source cut and is not new work.

### Requested assessment workflow

- [x] Full governing-document reread, current source/release/tracker comparison,
  live bounded probes, vision-to-code map and initial bridge.
- [x] Phase 3a: reconcile existing Beads using the frozen prompt retained below.
- [x] Two ambition rounds, revising this section in place; regenerate Beads.
- [x] Five refinement passes, ending without further changes; validate with
  `br dep cycles` and `bv --robot-triage`.
- [x] Review the exact assessment diff and prepare only the intended plan/tracker
  changes. Record actual publication and reservation release in the final handoff.

Creation-gate worksheet: this is PROCESS, requested explicitly by the user for
implementation steering. The consumer is that user; the decision is which
remaining product work to do next. Observed defects are stale tracker/source
assumptions and unexecuted combined recovery acceptance. This section retires
as active guidance at the next assessment; historical evidence is retained.
The integrity-control exception is unnecessary. Ready `br-q8z1r` and remaining
`br-8j6cb` acceptance offer more runtime value than extending this report after
the requested phases finish. This assessment earns zero capability credit.

Real-work audit and honesty disposition: this requested turn changed one
existing plan and existing tracker records, with four new causal dependencies.
It contains no product implementation, performance improvement, test execution,
new runtime validation or release qualification. Recent code fixes and their
bounded receipts are credited separately above. No test, gate, assertion,
golden, default, source file or tool configuration was changed; no subagent
was delegated this audit, and persisted peer mail is not independent review.
The risk is extending planning while real recovery work waits; the correction
is to stop after this requested assessment and use the existing code-bearing
tasks. Future status must not count these planning edits as product progress.

Validation of this documentation/tracker change: whitespace checks and semantic
JSONL review passed; existing issue ownership and acceptance were preserved.
The required scoped UBS invocation returned exit 3 because Markdown/JSONL are
unsupported: **nothing was scanned, and this is not a scanner pass**. No
allow-no-scan override was used. No compiled checks were run for this audit;
the source and release verdicts above remain unchanged.

## September 21, 2026 assessment

**The product is substantial and useful, and its ordinary workspace gate has
passed since the previous assessment. The current dependency migration, sustained
reliability, archive convergence under failure, and strict release acceptance are
still unfinished.** Prioritize those outcomes over more features or another
planning campaign. This section supersedes the dated conclusions below; retained
historical failures are not a current failure count.

### Evidence boundary

**Implementation investigation correction:** bounded DB-to-archive reconciliation
already exists in `storage/src/recovery/message_reconcile/database.rs` and is
called by the server retention worker. HTTP and TUI start that worker; stdio
omitted it. Earlier conclusions inferred missing machinery from stale comments.
The stdio lifecycle wiring and real idle-session regression landed in
`ce784836`; failure-path qualification remains in progress. Threaded messages
without surviving authoritative reply metadata remain deliberately deferred.

**Execution update:** the strict RCH baseline workspace/all-targets Clippy run
completed successfully on `vmi1264463` at 15:10:24 UTC, September 21 (536 seconds;
`/tmp/rainyforest-archive-baseline-clippy.log`, exit 0). This supersedes the
baseline Clippy NO_VERDICT below, not the wider release gate. Subsequent stdio
wiring and its integration test require their own validation.

The stdio wiring now has real-process evidence: strict RCH nextest on
`vmi1264463` passed `serve_stdio_reconciles_db_only_mail_without_client_reads`
in 12.826 seconds (one test passed; 81 outside the selection), terminal
September 21 at 16:18:17 UTC. The private fixture verifies initialized stdio,
canonical/outbox/inbox content in Git HEAD without client reads/retries, and
unchanged read/ack state. Retained log:
`/data/projects/am-release-20260912/20260921-rainyforest-stdio-reconcile-pass.log`.
The preceding capacity refusal and 1,800-second compilation timeout are excluded
attempts, not test failures or passes. This closes the observed wiring defect's
positive regression, not the bead's full fault/restart acceptance. The patch
received a two-file UBS scan with 98 critical and 3,924 warning heuristic
findings, not a clean full scan. September 22 triage established that all 98
critical findings also occur at unchanged HEAD: test panics and test-executable
setup, route/status comparisons misidentified as secrets, snapshot directory
names, and guarded CORS/JWT test paths. The supported baseline comparison
completed with zero new critical findings and 30 warning/10 informational
records, reviewed as test assertions/setup, fixed paths, bounded polling and
ordinal-shifted existing statements. No suppressions or scanner rules changed.
The installed scanner omitted per-finding output due to a missing helper path;
an isolated copy of the same binary linked to its existing helpers produced the
complete comparison. This is patch-level review, not full-scan/release clearance.
Edited-source workspace/all-targets check passed at 16:27:07 UTC (480.6 seconds)
and Clippy with `-D warnings` passed at 16:29:30 UTC (102.1 seconds), both through
strict RCH on the same worker. Fresh formatting and whitespace checks passed.
`br-g2ip0` is closed for its compiler/format scope; this is self-verification,
not independent review or broader release acceptance.

**September 22 failure/restart qualification:** 25 existing storage reconciliation
tests passed through strict RCH on `vmi1264463` (405 outside selection; 5.225
seconds). Both CLI subprocess tests then passed (81 outside selection; 58.189
seconds total), including a real accepted send, an observed outbox write failure,
an abrupt owned-process stop, and an idle restart that commits all three message
copies without reads/resends or read/ack changes. The journal directory was also
unavailable, but the journal-enqueue failure branch and backlog exhaustion were
not exercised. The first fixture incorrectly blocked the canonical ID scan and
was refused before acceptance; its failed run is retained separately. Correcting
the obstruction to the outbox preserved every acceptance assertion. Logs are
`20260922-rainyforest-reconcile-controls.log` and
`20260922-rainyforest-restart-pass.log` under the artifact directory above.
The broader bead remains in progress for the original journal/backlog,
attachment, threaded-metadata and interrupted-repair acceptance. Existing unit
negatives support their own scope; they do not replace those transport cases.
Final workspace/all-target check and Clippy with warnings denied passed through
strict RCH at 01:47:55 and 01:48:55 UTC, respectively; formatting passed. The final
complete UBS baseline comparison exited 0 with zero new critical findings;
83 warnings and 32 informational records were reviewed as test setup/assertions,
fixed paths, bounded polling and shifted pre-existing occurrences. The full
historical scan remains nonzero. All verification here was self-executed.

**Integration with newer main:** before pushing, origin/main had advanced by 18
commits to `5e5a73d4`. The clean merge includes new durable-recipient authority,
legacy/cross-project routing, exact attachment recovery, journal authority,
bounded Git/retry admission and ATC population scoping. These are existing
upstream implementations to qualify, not missing features to duplicate. The
pre-merge results above do not qualify this combined candidate. Selected
recovery/stdio tests passed; workspace check passed at 02:24:20 UTC. Incoming
formatting drift was corrected manually; two
delegates handled eight files and the parent reviewed their full diffs and
reran formatting checks. One intentional, caught test panic needed a documented
same-line UBS false-positive exception after formatting changed its fingerprint.
The 14-file comparison against incoming main exited 0: zero new critical,
230 warning and 66 informational findings, reviewed as test setup/assertions,
fixed-path heuristics and unchanged production statements moved by formatting.
The untriaged report is retained; no broad scanner rules or test gates changed.

The first merged stdio run exposed a real integration defect: the new compound
source query deferred every repair with `bind parameter index out of range: 1`.
The pinned engine evaluates compound `LIMIT` without the supplied bindings.
The source query now renders only the fixed internal row budget as an integer
literal, with explicit bound indices for IDs and size limits across both arms.
Single-statement observation, payload limits and the excess-recipient witness
remain intact. The failing terminal run is retained as
`20260922-rainyforest-merged-source-bind-failure.log`: one failed, 58 not run,
3,404 outside selection. After the fix, all 59 selected tests passed in 83.884
seconds (57 storage controls and two stdio subprocess cases; 3,404 outside
selection), terminal September 22 at 02:17:16 UTC on `vmi1264463`. Fail-fast was
disabled to execute every selected control; assertions and pass conditions were
unchanged. Receipt: `20260922-rainyforest-merged-recovery-pass.log`. Fifteen
source/lock hashes matched the worker; the executed `am` SHA-256 was
`64c301bd33bfe5160f688fb0496c98c1ee7cb132b2bf053f6c3a5856e0dbc0a9`.

Merge `48af590d` subsequently reached main and its required mirror. Final-source
workspace/all-target Clippy with warnings denied passed through strict RCH at
02:51:13 UTC after direct fixes to incoming diagnostics: documentation markup,
borrowed journal errors, a boxed private repair error, and equivalent ATC/retry
test syntax. No assertions, lint rules or runtime refusal conditions changed.
The 14-file UBS comparison still exits 0 with zero new critical findings,
230 warnings and 66 informational records; the last changed fingerprint is the
same breaker-identity test assertion with explicit pointer syntax.

Final-source runtime qualification passed through strict RCH on `vmi1264463`:
100/100 tests at 03:19:43 UTC (57 archive reconciliation, 39 boot-repair, two
retry and two real stdio cases; 11,597 outside selection; 76.241 seconds after a
25m55s build). The actual `atc::population::scope` module then passed 17/17 at
03:20:24 UTC (11,680 outside selection; 0.609 seconds), correcting the initial
filename-style filter that matched none of those tests. Logs are
`20260922-rainyforest-final-selected-tests-pass.log` and
`20260922-rainyforest-final-atc-scope-tests-pass.log` in the artifact directory
above. Fifteen source files and Cargo.lock match the worker; the executed `am`
SHA-256 is `2823775d964be14cea7360120c7f6645b16fe2c742fa15dd2487c0bc96d348e7`.
Final workspace/all-target check passed at 03:23:32 UTC; formatting and whitespace
checks pass. These selected checks do not close
the original journal/backlog transport acceptance, independent review, or the
full release gate; the broader recovery bead remains open.

**Further upstream integration:** checkpoint `3979cf4e` retains the verified
117-test result above. Incoming ACK pagination and canonical-identity recovery
through `e5b1e7bf` initially failed compilation because pool acquisition returned
the driver error instead of `DbError`. After correcting that conversion, the
real pinned-engine ACK tests ran on `vmi1264463` at 04:01:22 UTC: eight failed
with `bind parameter index out of range: 1`; only the page-size guard passed.
The terminal log is `20260922-rainyforest-ack-native-initial.log` in the artifact
directory above. The engine evaluates compound-query LIMIT without bindings.
Local integration checkpoint `84f01e5c` renders only the validated integer page
bound and uses numbered bindings for mailbox values, retaining one statement,
the metadata row, overflow witness, and every existing assertion. A fresh rebuilt
binary passed all nine ACK tests at 04:08:06 UTC (0.674 seconds;
`20260922-rainyforest-ack-native-fresh.log`). An intervening rerun executed stale
code and is excluded: source bytes matched, but their preserved edit timestamp
preceded the completed baseline artifact. Refreshing that source timestamp
forced a real rebuild without changing bytes. Newer `8b6e0276` adds surviving staged Git
copies as a last-resort source of reply/extension metadata; this is merged for
the same combined gate. Formatting passes; the five-file UBS comparison against
`8b6e0276` exits 0 with zero new critical, 80 warning and seven informational
findings, all reviewed as test fixtures/assertions and formatting shifts. The
all-target check also exposed two incoming ACK test compile errors: pooled
connections lack `Debug`. Using `Outcome::expect` preserves the failure checks
without formatting the success value. Strict workspace/all-target Clippy with
warnings denied then passed at 04:19:07 UTC; the terminal receipt is
`20260922-rainyforest-combined-clippy-final.log`. The combined runtime run ended
at 05:08:15 UTC: 115/116 passed (11,611 outside selection; 110.807 seconds),
including both real stdio cases, nine ACK query tests, all 35 ACK worker tests
and 69 storage controls. The one failure occurred during a new staged fixture's
setup: libgit2 rejected a missing object when adding to a repository-owned index,
before reconciliation ran. The fixture now opens its index without an attached
object database and asserts the object is absent, so the original refusal and
preservation assertions can execute. The corrected test subsequently passed in
the 68-test run recorded below.
The failed terminal log remains `20260922-rainyforest-combined-runtime.log`;
the executed `am` SHA-256 was
`804383fc0242465041af0fdaa4054177dd6ec971011d4937a2b21af11d5d70a3`.
Publication remains pending. Upstream `2f7ee9ed` adds exhaustive ACK page-bound
regressions and retryable model discovery; those are the next fixed integration
cut. Search helper tests alone will not establish real-model loading coverage.

**Live search follow-through (`br-drpda`, closed):** tracing the imports
revealed that the newer discovery fix modified only the optional `search-core`
module. The mailbox actually uses DB `search_model2vec` and `search_auto_init`;
neither the loader's permanent negative cache nor the context's permanent
unavailable snapshot was repaired there. The DB path now has ordered fallback,
retryable failed discovery, stable successful model references, and lazy metadata
that lets the same context observe a later installation. Model I/O occurs outside
the context initialization lock. Initial metrics retain their historical meaning.
Seven deterministic controls were transferred manually and reviewed. A separate
opt-in subprocess regression requires actual registered potion artifacts, tests
the production cooldown and late installation behind a broken first cache, and
checks a normalized embedding plus stable context/model references. It is ignored
in ordinary model-free runs and must be explicitly executed for this bead's
closure. Tiny synthetic weights cannot qualify this path: the pinned dependency
correctly requires the registered artifact identity. The real model is present on
`vmi1264463`. Default workspace Clippy at 05:16:13 UTC did
not enable the optional `search-core` semantic module and predates these DB edits.
The current strict RCH gate explicitly enables that feature. The earlier cold
feature test job was cancelled before tests (exit 143, no verdict); its log is
retained. No model admission policy, runtime assertion or test threshold was
relaxed. This remains model-discovery/context qualification, not full semantic
transport or performance certification.

The frozen candidate passed all 68 selected tests through strict RCH on
`vmi1264463` at 08:00:51 UTC: 12 ACK scan, 19 live context, 15 live model-loader,
14 optional model-loader and eight staged-recovery tests (4,999 outside selection;
86.432 seconds). The registered-model subprocess passed in 33.464 seconds and
emitted its required completion marker. The missing-object fixture passed in
0.089 seconds. The receipt is `20260922-rainyforest-live-search-focused-tests-warm.log`
under the artifact directory above (SHA-256
`3299b38995d527cfcf85b3a29137fab46d5d3a1b2b4cc57a636d0bbe59d48f6d`).
The preceding 90-minute cold compilation timed out before tests and is excluded.
Metadata-only remote inspection confirmed all three test binaries were unchanged;
their hashes are retained in `20260922-rainyforest-live-search-test-elf.sha256`.
All 24 selected source/lock hashes matched the worker. Optional-feature workspace
Clippy passed at 06:11:16 UTC; final library timestamp invalidation and workspace
gates remain to be completed on the publication candidate.

Upstream subsequently advanced eight commits to `ae296364`, adding transactional
ACK admission, explorer pagination/privacy repairs, ATC reset/locking repairs and
optional-context promotion. These are the next fixed integration base; the
68-test result does not qualify their new behavior. A separate source audit found
that the legacy `SemanticBridge` can retain a 384-dimension index while the live
fast model emits 256 dimensions. Exact call sites were sent to GentleBeaver's
existing `br-kp1in.7` work (Mail 42841). This is a source finding awaiting a real
indexing/search reproducer, not a claim that full semantic transport now recovers.

Checkpoint `0c72bac5` preserves the live DB fix and the 68-test result. The
`ae296364` integration required manual formatting plus direct compiler/lint
repairs: ACK candidate coalescing now inserts a key before attempting admission
and removes it only for a stale delivery, retaining eligible siblings and bounded
error retries; fixture checkout uses `Outcome::expect`; optional context
publication uses equivalent lazy `map_or_else` branches and explicit unit
patterns. Strict RCH workspace/all-target Clippy with the optional search feature
and warnings denied passed at 08:28:14 UTC on `vmi1264463` (receipt
`20260922-rainyforest-ae296-clippy-final4.log`). The workspace/all-target compiler
check with the same feature passed at 08:37:31 UTC (475.871 seconds; receipt
`20260922-rainyforest-ae296-check.log`). The combined DB/storage/search-core
candidate passed 158/158 selected native tests at 08:59:07 UTC (4,935 outside
selection, 162.597 seconds after a 14m51s build). This includes 12 transactional
ACK admission tests, 12 ACK scans, 54 explorer helper/regression tests, 19 live
context tests, 15 live model tests, 24 optional-context tests, 14 optional-model
tests and eight staged-recovery controls. The registered-model subprocess passed
in 34.056 seconds with its completion marker. Receipt
`20260922-rainyforest-ae296-db-core-tests.log` has SHA-256
`34c97aa62cc5f37c29e397896e61d59c46c4147f7982aec14ed1a1887a25e4cd`;
metadata-only remote inspection identified three unchanged test executables,
with hashes in `20260922-rainyforest-ae296-test-elf.sha256`.

The server ACK-worker and ATC admission selection subsequently passed **59/59**
through strict RCH on the same worker at 11:46:40 UTC: 42 ACK-worker tests and
17 ATC admission tests, with 4,866 outside the selection. Run
`d0c9cc41-3b21-4101-b281-22b80ad24e78` took 47.582 seconds after a 117m31s cold
build. Receipt `20260922-rainyforest-ae296-server-only-tests.log` has SHA-256
`61983c9d1486b07bf02c34b69ad5c9176ddf7f9427337cf4183ebccd665b8763`.
The command was `cargo nextest run --locked --build-jobs 1 -j 1 --no-fail-fast
--success-output final -p mcp-agent-mail-server --lib -E
'test(ack_ttl::tests) | test(atc::admission_boundary_tests)'`, executed with
`RCH_WORKER=vmi1264463 RCH_REQUIRE_REMOTE=1 rch exec --` and 7,200-second build
and test limits. The executed server binary is recorded in
`20260922-rainyforest-server-test-elf.sha256`; all 29 recorded source/lock hashes
still match the worker. This brings the two current-source selections to 217
passing native tests, without claiming a full workspace or release gate.

The old default-feature target cache no longer contained the necessary compiled
artifacts. The initial command kept the prior four-package set for cache reuse,
but manifest/lock review then showed
that it needlessly built the CLI's embedded Beads engine and older async runtime.
That build was stopped before tests and the same two test-module selections were
narrowed to the server crate. An earlier combined server/optional-search attempt
was also cancelled before tests when feature unification forced a cold rebuild.
Both cancelled attempts have no runtime verdict; their logs are retained.
The final 13-file UBS comparison against the
incoming base exits 0 with zero new critical, 265 warning and 25 informational
findings. Warnings were reviewed as fixture assertions/setup, bounded cloning
and location diagnostics; exact test-only panic annotations preserve the
assertions. The full historical scan is still not a release clearance.

The live discovery/context bead is closed on this evidence, published in
`169143db` to main and its required mirror. The broader `br-8j6cb` recovery
acceptance remains open.

The next continuation reproduced the staged-metadata loss on `5a298c07` plus a
new regression, before changing production code. The first threaded message
repaired and repeated idempotently in fresh processes; the next message failed
because resetting the shared Git index had discarded its only reply metadata.
Strict RCH run `a1004463-1617-465f-9214-3486dfd081cb` failed that one test at
12:46:28 UTC on September 22. Its complete log and source receipt are retained
as `20260922-rainyforest-staged-initial-{reproducer.log,source.sha256}` under
`/data/projects/am-release-20260912`. The test's later ordinary-commit variant
was not reached in that failing run.

Archive commits now construct trees independently of the shared index. After a
successful commit, index maintenance holds Git's `index.lock` from observation
through atomic replacement, and updates only an index clean relative to that
commit's actual parent. Dirty, locked and invalid indexes stay byte-identical;
unrelated staged-only objects are excluded from the committed tree. Maintenance
failure warns without retrying an already-durable commit. A busy lock can leave
the index stale, so this does not promise clean status after every concurrent
commit.

The corrected selection passed **80/80** tests on strict RCH `vmi1264463`, run
`24ddec7d-0776-442d-a05d-8565c9ab3f11`, terminal 12:56:03 UTC, 28.730 seconds
runtime, 5,015 tests outside selection. The real-Git regression repairs six
staged-only replies in separate processes, both with and without an earlier
ordinary archive commit. It verifies exact reply/extension metadata, BCC
redaction, body and committed bytes, retained evidence and idempotence. Another
regression crosses three commit strategies with clean, staged, locked and
corrupt indexes, including rejected-path preservation. Existing recovery and
commit concurrency controls also pass. The six processes establish evidence
survival beyond a four-repair batch's lifetime; they do not exercise the actual
batch cursor or transport backlog. Journal-enqueue failure, backlog exhaustion,
interrupted transport recovery and independent review remain open acceptance.
The fixed log is `20260922-rainyforest-staged-fixed-tests.log`, SHA-256
`be5d7943065d25ad72bcbc8f6b21d21035417052de21ca2e108aed1c01542b90`.

The fix was published in `7352a3d3`; its source bytes match that passing run.
Workspace/all-target `cargo check` and `cargo clippy -- -D warnings`, both with
`--features mcp-agent-mail-search-core/hybrid`, passed through strict RCH at
13:00:04 and 13:03:31 UTC respectively. Formatting and whitespace checks pass.
A metadata-only RCH invocation confirmed the tested storage executable remains
fresh; its hash is retained in `20260922-rainyforest-staged-test-elf.sha256`,
alongside matching local and worker source receipts. The two-file UBS comparison
against `5a298c07` exits 0 with zero new critical, 107 warning and 17 information
findings. Reviewed warnings cover fixture assertions/setup, bounded fixture
cloning, line-shifted existing tests, private scratch writes and header indexing
protected by a checked 12-byte slice. No new suppressions or assertion changes
were needed. An initial check invocation was refused before execution because
its nextest-specific jobs flag did not constrain Cargo; the corrected `-j 1`
invocation passed. There was no local build fallback or full-release claim.

RainyForest read all 1,350 lines of current AGENTS.md and 2,053 lines of README.md.
The current vision, durability, threat, browser and ATC contracts were compared
with source, the verification ledger, dependency upgrade log, previous full
plan/spec audit and tracker. Historical plan/spec coverage is reused explicitly;
this is not a claim that every historical document was freshly reread. The prior
assessment's conclusions were challenged against newer source and terminal
artifacts, rather than inherited as present truth.

- Source inspected: `17be6b647049d8a775661a2737c59466932665fc`, main. The only
  initially dirty tracked files were the two Beads JSONL files. No production
  source was edited or shared mailbox repaired in this assessment.
- Live GitHub API: latest public release **v0.3.36**, published September 16 at
  10:14:19 UTC, 15 assets. Publication is not a verdict on current main. Local
  `am --version` is also 0.3.36, SHA-256
  `e6cf98a365fae0d569d865e3f43e1068153191bb18641d663fbf4803705e77a7`.
  Version equality alone does not establish identical source or release assets.
- Important correction: `br-l1q6z` is **closed with a later full green run**.
  Its September 16 terminal receipt reports 17,547 tests passed, zero failed,
  36 excluded, zero passing-leaky. Inspected retained
  `all-eleven-fixes-full-workspace-vmi1227854.{log,result.json,source.json}` in
  `/data/projects/am-release-20260912`; exit 0 and `source_stable=true`.
  This predates the current 0.4.4 runtime migration. Do not reopen that historical
  restoration task merely because a newer candidate needs qualification.
- More recent selected evidence is also real: inspected terminal logs show
  3,335 DB/schema/search tests passed with seven skips on September 19, and
  3,392 FTUI/console tests passed with 1,578 tests outside that selection.
  Logs are `044-runtime-pin-db-broad-quiet-extract.log` and
  `044-ftui-070-runtime-resumed.log` in the same artifact directory. These are
  retained executions, not fresh runs performed by this assessment.
- At assessment start, whole-workspace Clippy had **NO_VERDICT**:
  the preceding continuation hit a Cargo `color_quant` source-cache panic on
  hz2, then a 1,800-second remote build timeout on vmi1264463, then a capacity
  refusal. The later terminal workspace passes above supersede that state and
  close `br-g2ip0`. Focused passes cannot certify later merged source. No local
  build fallback was used.
- Fresh live coordination succeeded: message 42750 persisted with both named
  recipients; its delivery receipt explicitly says no signal or acknowledgment
  witnessed. Plan reservation 11785 was granted without conflict. This proves
  a bounded live path, not every tool or disaster recovery.
- Fresh live MCP health returned `status=ok`, **health_level=red**. Critical
  integrity/connectivity verdicts were green; pressure was red. WBQ p95 was
  1,047 ms and commit-queue p95 4,195 ms. Current pressure classification has
  a 250 ms red queue threshold, so empty queues and the separate 15-second
  coalescer degradation threshold do not make this contradictory or healthy.
  Archive/DB message counts were 42,144/42,173 within configured tolerance;
  that is not row-level parity proof. This observation is not a benchmark.
- Local `am doctor health` exited 1 with `local_config_unattested`, one orphan
  page in its private probe and 32 reservation-field differences. That is a
  different, unattested target; do not merge it with the live integrity verdict.
- Initial tracker: **44 open + 92 in progress + 13 blocked = 149 unfinished**,
  plus 2,359 closed. Assignees and in-progress flags are not proof of current
  activity. CASS health reports a stale index; no fresh session-search coverage
  is claimed. Recent git, actual artifacts and Beads comments supply history.

### Architecture and numbered vision checklist

The native CLI/server drive FastMCP stdio/HTTP handlers and shared tools/resources.
Tools use the SQLModel pool and FrankenSQLite runtime for canonical operational
rows, then feed Git archival and search indexing. Core owns configuration,
models, metrics and safety contracts; storage owns archive/coalescing; guard and
share are separate consumers. Server supplies the TUI and authenticated HTML
mail UI; CLI adds setup, robot, doctor and native E2E/bench runners. This is a
12-member workspace, with the public dashboard replay built separately.

Runtime is Asupersync 0.5 / SQLModel 0.5 / patched FrankenSQLite 0.4.4; embedded
Beads retains a separate patched 0.3.18 engine. Canonical C SQLite remains a
verification/recovery exception, not the runtime mailbox. FrankenSearch and
FastMCP are now immutable Git pins; the old gated sibling-clone story is stale.

`PARTIAL` means real implementation with incomplete acceptance or known gaps.
`UNPROVEN` does not mean absent. A live sampled path is deliberately narrower
than whole-family certification. Paths below are relative to `crates/` unless
otherwise stated. Existing implementation/test pairs are retained.

| # | Testable promise | Source and actual status | Remaining acceptance / Beads |
|---|---|---|---|
| 1 | 45 tools and 25 resource contracts | tools modules and server registration are real; protocol upgrade PARTIAL | `br-s3xbp`, `br-w9v59.1`, conformance at final candidate |
| 2 | Explicit send/reply/read/ack and durable receipts | `tools/src/messaging.rs`; fresh persisted send WORKING within that sample | `br-kp1in.1/.2`, identity/recipient reconciliation across restart |
| 3 | Broadcast always refused | messaging normalization and handler both reject it; implemented | Preserve negative transport coverage; no broadcast feature |
| 4 | Reliable DB-to-Git convergence | Bounded background reconciliation now wired for HTTP/TUI/stdio; real idle stdio recovery passes; PARTIAL | `br-8j6cb`, `br-kp1in.2/.9/.10`; broader failure/restart qualification |
| 5 | Concurrent operations without lost/duplicate accepted mail | Transactional/idempotency paths real; sustained current proof UNPROVEN | `br-htobc`, `br-sa58k`, `br-22gm3`, `br-kp1in.1/.2` |
| 6 | Safe recovery and retained namespace authority | `db/src/pool.rs`, write barriers and CLI recovery real; PARTIAL | `br-xzgcj`, `br-8asz3`, `br-cdsge`, `br-oyget`, `br-3p187`, `br-qfvd6` |
| 7 | Bounded engine/pool lifetime and real dependency behavior | Current pin includes retained-FD repair; selected tests pass; PARTIAL | `br-5lgwn`, `br-8r6dl`, `br-eru3j`, `br-oiok2`, `br-mmnyj` |
| 8 | Identity, contact and topic semantics | Real typed handlers, proof gate and persisted topics; PARTIAL | `br-sgaee`, `br-g6c0z`, `br-qayvs`; transport/restart matrix |
| 9 | Leases, conflicts, renew/release and guard | Real reservations plus fresh successful plan lease; wider PARTIAL | `br-9bwnb`, `br-ssog9`, mixed workflow and real commit guard |
| 10 | Product membership and build-slot coordination | `tools/src/products.rs`, `build_slots.rs` are implemented | `br-kp1in.1/.2`; scope/TTL tests, advisory slots are not mutexes |
| 11 | Bare am and automatic agent setup | CLI startup and core setup real; authority races remain PARTIAL | `br-49eak`, `br-siq0z`, `br-db75q`, `br-fphbm`, `br-7znpo` |
| 12 | Safe credentials and client configuration | Real token/config logic and hardening, still PARTIAL | `br-ww5js`, `br-x5a8y`, `br-q8k82`, `br-6u4hx`, `br-x2jf5` |
| 13 | 16-screen TUI with responsive input | Real FTUI screens; retained 3,392 selected passes; PARTIAL qualification | `br-ivgot`, `br-l4fk6`; quiet input/resize/attachment/read-only scenarios |
| 14 | Authenticated web mail and usable share/export | Real `mail_ui.rs` and share crypto/snapshot modules; current full UNPROVEN | `br-ji2f0`, real HTTP/auth/export tamper lanes |
| 15 | Fast truthful robot/doctor health | Implemented; fresh health explains red pressure; performance PARTIAL | `br-es9fm`, `br-eru3j`, health latency task, `br-kp1in.5/.6` |
| 16 | Reversible owner-safe doctor | Real mutation/undo machinery; latest backup/writer fixes unqualified together | `br-qfvd6`, `br-qdgio`, `br-r6psd`, `br-sd3md`, recovery owner tests |
| 17 | Search-as-you-type and scoped Search V3 | Real lexical/private-snapshot/optional hybrid routes; PARTIAL | `br-7x5fm`, `br-eh8bj`, `br-kp1in.7/.8`; retain safe SQL fallback |
| 18 | Real optional model/TOON quality | Implemented selected paths; TOON retained real tests improve prior picture | `br-kp1in.7/.8`; actual model/encoder and privacy/relevance controls |
| 19 | ATC learns while defaults stay quiet | Hooks, persistence and bounded hydration real; long-duration UNPROVEN | `br-hwney`, `br-au76r`, `br-kp1in.1/.2`; shadow/write-off retained |
| 20 | Signed install/update on supported targets | v0.3.36 published; installer/source pin logic real; acceptance PARTIAL | `br-nq2kb`, `br-bx73n`, installer and platform owners |
| 21 | Lossless Python import and explicit cutover | Legacy code implemented; namespace/publication cases PARTIAL | `br-lkhxw`, `br-dbt24`, `br-1m1tv`, `br-ajiq8` |
| 22 | Complete current correctness and reproducible release gate | Older full green exists; current candidate UNPROVEN | `br-5lgwn`, `br-g2ip0`, `br-kp1in.3/.4`, `br-bx73n` |
| 23 | Measured operational latency/resource budgets | Bench infrastructure real; current general claims UNPROVEN | `br-kp1in.5/.6`, stress tasks; no historical-number extrapolation |
| 24 | Honest active docs and deliberate distribution scope | Current README boundaries much improved; VISION/old tasks conflict | `br-4meup`, `br-95spu`, `br-ajiq8`; reconcile authority without deleting history |
| 25 | Privacy-safe public WASM replay | Standalone real screen/replay exists; deployment not reverified here | `br-f9avw.10`, `br-mq9q1`; live browser mirror stays deferred |

No entirely uncovered feature family was found. The backlog already covers the
goals, but literal completion of stale descriptions would **not** finish them.
Examples: `br-kp1in.3` still describes mtime selection although current
`read_incident_scorecard` consumes exact producer identity/digest; `br-ku0kl`
describes an obsolete sibling clone; `br-yzk37` calls knobs inert despite engine
implementations now present. These need current acceptance, not duplicate code.
Project-addressed mailboxes and session-bound identity remain explicit future
requests, not prerequisites silently added to today's trusted-local contract.

### Bridge: smallest coherent route to the promised product

1. **Finish the current candidate, not another dependency campaign (M).**
   `br-5lgwn` owns final combined-runtime qualification; `br-g2ip0` owns outstanding
   compiler/lint verification. Use one admitted RCH build lane with a frozen
   source/lock/features manifest, then reuse its artifacts for focused and full
   gates. Classify each of the 36 older exclusions under the new engine; do not
   silently carry obsolete trigger/concurrency excuses or count exclusions green.
   Preserve original failures and watchdogs. Source fixes are not deployed fixes.
2. **Close accepted-message recovery and publication gaps (M–L, critical).**
   Promote `br-8j6cb` from P3: an accepted DB row must eventually regain its
   canonical, sender and recipient archive records after a failed queue/journal
   and restart, even if no client reads/retries it. Wire the existing bounded
   reconciliation worker into stdio; retain its authority and no-clobber checks.
   Recovery/path/snapshot owners retain their scopes. Prove real positive repair
   and no-clobber/scope negatives, not merely a safe refusal.
3. **Complete one coherent real user history (L).** `br-kp1in.1/.2/.9/.10`
   join messaging, contacts/products, leases, quiet ATC, cancellation and restart.
   Begin with bounded smoke; then controlled faults on private fixtures; only
   then the already-specified 24-hour/two-host profiles. Track committed versus
   returned versus archived state separately; a timeout is initially unknown.
   Require row/recipient witnesses and bounded resource growth, not equal counts.
4. **Make performance explain the observed route (M).** `br-kp1in.5/.6`
   report queue latency, archive lag, pressure, tool deadlines, RSS/FDs and
   retry amplification together. Empty queues must not erase observed latency.
   Keep absolute budget breaches when comparative controls are inadmissible.
   Profile current health/global-search routes before changing algorithms or
   weakening thresholds; preserve private-snapshot SQL fallback correctness.
5. **Qualify the exact executable and gate publication (M).** Scorecard producer
   binding already exists; finish its adversarial tests and actual manual
   publisher consumer (`br-kp1in.3/.4`, `br-nq2kb`, `br-bx73n`). Require explicit
   installed parity, selected real-path suites and native target evidence.
   Shipping, signatures and full default tests are three different conclusions.
   Do not enable Actions or publish a test release for this assessment.
6. **Independent user-surface lanes (S–M each).** Finish setup/config authority,
   legacy import/reopen, interactive TUI, optional dependency quality and share
   export with existing owners. Optional model absence cannot certify that model
   or block an explicitly lexical-only candidate. Preserve all existing features.
7. **Reconcile active promises (S).** `br-4meup` must resolve VISION's automatic
   stale-process killing, no-C-SQLite, byte-for-byte prose, zero ignored tests,
   and deferred-web statements against newer governing contracts. Preserve the
   goal and date the supersession; do not weaken safety or invent acceptance to
   make old checkboxes true. Product decisions remain `br-ajiq8`/`br-95spu`.

Implementation does not wait for an audit epic. The genuine dependency chain is
candidate fixes → combined gates → real sustained/installed acceptance → release.
Harness, measurement and adversarial-test development can proceed independently;
certification waits on product correctness. No new architecture or tracker is
needed. Exact Bead refinements and final graph validation follow below.

### September 21 plan refinement record

Phase 3a and Phase 5 use the frozen prompts retained verbatim below. The existing
implementation/test graph is regenerated by updating its current evidence and
acceptance rather than duplicating its issues. No implementation or release
closure is credited to this assessment.

**Ambition round 1: recovery without a helpful client.** A read-triggered healer
alone does not close the archive promise: a message may never be read again.
Require bounded startup/background reconciliation after transient archive failure
and restart, with no client read or retry. An idempotent retry skips one-time
archive dispatch by design, so it cannot be assumed to repair the missing copy.
Keep source DB durability and eventual Git convergence as separate assertions.
Do not attempt recovery of bytes no authoritative source retains. Companion
tests must prove actual repaired artifacts, not only a logged degradation.

**Ambition round 2: bounded progress under sustained contention.** Successful
eventual retries can hide overload. Extend existing measurements to report
offered, admitted, committed, replied and reconciled operations; attempt count
and oldest outstanding age accompany percentiles. Bound the reconciliation
backlog by work scheduling, not by dropping accepted DB truth. Sweep load until
saturation on an owned fixture, then show recovery after load removal. Measure
FD/RSS slopes and cold initialization separately from steady state. An admission
refusal is a distinct outcome, never a fast successful operation. This improves
the existing workload and performance pairs without a new telemetry subsystem.

**Ambition round 3: separate transaction order from eventual convergence.** The
existing bounded history checker should use partial-order constraints for
transactional message/recipient/idempotency operations, with explicit barriers
and durable witnesses for ambiguous responses. Archive convergence uses a
different predicate after an admitted bounded drain; it must not be forced into
an invented synchronous Git contract. Partition independent histories only while
retaining cross-project global ID and recovery-generation constraints. Show a
minimal counterexample for duplicate/lost rows or wrong-generation repair;
exhaustion is NO_VERDICT. This is a useful application of history checking, not
a new general model-checking framework or a universal correctness proof.

Bead regeneration updated `br-kp1in.2/.3/.5/.9` and the existing product,
migration and documentation tasks. The initial frozen instruction remains
unchanged; improvements are embedded in the Beads, not only this document.

**Refinement 1 — scope and source:** corrected obsolete dependency/scorecard
assumptions, retained the legitimately closed older full gate, and left all
ownership/status claims intact. Updated `br-ku0kl` with the current immutable
dependency graph; its old sibling-clone title must not trigger duplicate work.

**Refinement 2 — preserved behavior and test realism:** extended existing
companions `.6` and `.10` with independently calculated accounting controls,
no-client-trigger reconciliation, wrong-generation and missing-byte cases.
Archive-only recovery cannot invent metadata it never stored. All existing
scope/contact/identity, advisory-slot and broadcast-refusal assertions remain.

**Refinement 3 — evidence:** inspected the retained full-gate terminal result and
source manifest plus selected DB/FTUI terminal summaries, and updated `br-bx73n`.
The released, locally installed, live-daemon and source candidates remain
distinct. No recreated wrapper or inferred current pass substitutes for a
terminal result; the old red count is explicitly superseded.

**Refinement 4 — dependency order:** the existing workflow certification task
`.2` had no dependency on the remaining archive convergence acceptance. Added
exactly `br-kp1in.2 -> br-8j6cb` (the former depends on the latter). Test authoring
need not wait, but certification cannot close while accepted messages still
require luck or a client request to regain their recovery archive. The repair
itself does not wait on the history checker, avoiding a cycle. Retained all
other implementation/test pairings and assignees.

**Refinement 5 — convergence:** reread the current 25-goal matrix and revised
Beads against the original promises. No further scope, acceptance, priority or
dependency change was needed. All literal issue IDs resolve; no issues were
added or removed. This assessment changed 11 existing issues, with no status or
assignee changes; the pre-existing `br-g2ip0` blocked-status update is also present
in the shared JSONL diff. `br dep cycles --json` reports zero active cycles.
Final `bv --robot-triage` reports 149 unfinished, 124 actionable and 25 not
actionable; actionable includes owned work and is not permission to claim it.
`br sync --flush-only` completed. `git diff --check` passed.

The assessment-only UBS invocation returned **exit 3: no supported languages,
nothing scanned** for Markdown/JSONL. It was not a pass, and those changes were
initially left uncommitted. The subsequent implementation received the complete
Rust baseline comparison and triage recorded above; `UBS_ALLOW_NO_SCAN` was never
enabled. Neither an unsupported-language result nor a patch comparison proves
that the complete release gate is green.

### Requested-work and honesty disposition

Window: the September 21 reality-check request only. The creation-gate consumer
is the user explicitly requesting this assessment; the decision is where to
direct implementation effort. Observed defects justifying it are the stale red
gate narrative, obsolete dependency tasks and under-prioritized missing archive
convergence. This section retires as active guidance at the next assessment;
history is retained, never deleted. Running product code does not branch on it.
The integrity-control exception is unnecessary because the request supplies the
gate. Highest-value ready capability work is stable path authority (`br-xzgcj`)
and archive convergence (`br-8j6cb`); another hour implementing those would offer
more runtime value than extending this already complete requested assessment.

Real-work inventory: **zero USER implementations, zero ENABLER implementations,
one PROCESS assessment with its associated tracker updates**. There is no new
feature to demonstrate; fresh live sends/receipts exercise existing capability.
Without this work the binaries would be identical, but the requested steering
would still rely on obsolete status. The long-standing corruption/recovery work
remains important; this window served the explicit assessment request. No swarm
was dispatched, no close count used as progress, no original acceptance split
off to permit closure. Disposition: bounded requested process, zero runtime
credit; stop planning and hand the existing implementation graph back to owners.

Honesty inventory (solo review, not independent certification):

1. No tests weakened/deleted/ignored (checked: only plan/JSONL changes).
2. No doubles introduced (checked: no code/test changes).
3. No goldens regenerated (checked: no fixture changes).
4. No gate/validator relaxation (checked: no workflow/config changes; UBS's
   suggested no-scan override was not used).
5. No gate gaming (checked: all acceptance remains open; no publication).
6. No zero-run green (checked: retained terminal counts, UBS explicitly no scan).
7. No unrun-test claim (checked: retained versus fresh probes distinguished;
   historical plan/spec audit reuse and unavailable current full gate disclosed).
8. No lower-grade evidence promoted (checked: source presence, earlier tests,
   live persisted receipt and released assets each retain their own scope).
9. Corrected the previous assessment's obsolete red-test conclusion in place;
   retained actual later green and current NO_VERDICT without projecting a count.
10. No stderr suppression used for cited evidence (checked: tool output and
    retained logs; truncated display output was not treated as a complete read).
11. No unmet task closed (checked: IDs/statuses and assignees preserved).
12. No spec weakened to fit code (checked: historical contradictions are assigned
    to the existing docs task; safety/quality requirements remain intact).
13–16. No delegated agents or delegated closures in this window; historical
    peer receipts inspected as retained evidence, not independently rerun.
17. No peer agreement counted as proof (checked: evidence comes from code,
    terminal artifacts, API observations and tracker, not endorsement).
18. No postselected performance denominator (checked: no performance win claim;
    retained test totals and exclusions are shown together).
19. Explainable limits: broad tool output can truncate, so neither `cat` alone
    nor earlier audit reuse means every historical document was freshly read.
    CASS's stale index precluded a claimed fresh history survey. The requested
    AGENTS and README were read completely before assessment. The initial UBS
    landing limitation and subsequent scoped triage are recorded above; no
    permission or repair request is hidden behind a success claim.
20. Strongest evidence: the inspected September 16 terminal full-gate receipt,
    the exact current archive failure/replay paths, fresh persisted message
    receipt and reproducible plan/Beads graph diff. These support the assessment,
    not a claim that the remaining product gaps are fixed.

Countermeasure for stale/overbroad success or failure reporting: keep exact
candidate, selected scope and terminal evidence attached to every conclusion;
no planning revision may stand in for product
delivery (RH-10). No fresh CASS-wide retrospective honesty sweep is claimed.

**Handoff:** assessment, bridge, initial Bead refinement, three ambition rounds,
regeneration and five refinement passes are complete within the stated evidence
limits. No new issues, runtime edits, default changes, release, or live repair.
Resume the existing candidate/recovery/archive lanes; do not launch another
assessment to avoid their remaining implementation work.

## Historical September 8 assessment (superseded)

**Current assessment: September 8, 2026 — requested complete reality check.**
This assessment supersedes the September 4 and September 2 assessments retained
below. The audit began at `ba2ad9ce`; peer changes subsequently advanced the code
through `4717f869`. Released v0.3.34 is still `0125f050`. These are different
subjects of verification.

## September 8 judgment

**The core product is real and useful; reliable operation under contention and
strict release acceptance are unfinished.** This is no longer a missing-server
or empty-command problem. Actual released binaries can register agents, reserve
paths, send/read/ack/reply, serve resources, and enforce the archive-backed guard.
However, a fresh execution of the canonical workflow failed its concurrent HTTP
phase, the last complete workspace run had 12 failures, and manual publication
did not satisfy the documented zero-failure gate. Those facts outweigh the large
number of closed issues when deciding whether the project is finished.

The highest return comes from delivering already-tested isolated repairs,
resolving the remaining database/recovery failures, and making the existing
release and workflow machinery decisive. Another architecture, dashboard,
replacement engine, or broad feature campaign would dilute that work.

### Evidence and its limits

- Read all 1,339 lines of `AGENTS.md` and all initial 1,993 lines of `README.md`.
  Read the subsequent 15-line README addition for the newly landed guard bounds.
  Reviewed the existing full September 4 vision/plan audit, current documentation
  index, durability contract, verification realism/closure policy and release
  gates. Compared plan/spec documentation since the earlier assessment: only
  this bridge changed before the concurrent README addition. Historical document
  reads and measurements are reused explicitly; they are not new executions.
- Initial tracker inventory: **2,457 issues: 2,289 closed, 90 open, 77 in progress,
  one blocked**. Thus 168 unfinished issues, not 90. `bv` reported 155 actionable
  and 13 not actionable; actionable does not mean unowned. Its top suggestion
  was an old mirror-sync task, demonstrating why graph ranking needs source and
  release evidence before taking action.
- Live GitHub API confirmed v0.3.34 public, published September 8 at 02:58:05 UTC,
  with 25 uploaded assets. Prior retained receipts establish six target families,
  signed public downloads, Bash install/update, Homebrew formula publication and
  dual-architecture GHCR publication. Linux ARM execution used QEMU; Intel Mac
  used Rosetta. A fresh full PowerShell installer and `brew install` were not
  executed. Every active crate remains `publish = false`; crates.io is an explicit
  product decision (`br-95spu`), not a venue silently completed by this release.
- Full nextest receipt at source `43459c5a`, run
  `f70dd3fe-fc26-4d85-a6aa-a1dcc3aaf19c`: **17,384 executed, 17,372 passed,
  12 failed, 37 skipped, two passing tests reported leaky**, exit 100. Raw log
  SHA-256 `265e326d0a11f1c8ab66f31ffa0d41f62f840c85bd023aa888323ec0ea890d48`.
  Final release-source check, Clippy with warnings denied, formatting and 123
  focused tests passed. Neither result validates the later peer commits or
  changes the red full-suite verdict.
- **Fresh published-binary workflow, 19:19:23–19:20:09 UTC:** 41 assertions passed,
  one failed, zero skipped. Phases 1–8 passed. Phase 9 completed HTTP initialize,
  send, inbox and ack, then RedFox's concurrent `register_agent` returned
  `RESOURCE_BUSY` / `busy_retryable`; the client exited before the ready barrier.
  The other two clients were terminated by the owned harness and the server
  exited 0. The reopen portion was not reached. This is a real failed workflow,
  not evidence of lost mail or a proven new engine regression. The server calls
  the error recoverable; the fixture currently requires first-attempt success.
  That acceptance mismatch must be resolved explicitly, with bounded retry and
  durable identity checks if retry is the intended contract.
- That fresh run used release `am` SHA-256
  `5931c453048d78154046865986a2c274e6189cc53714e418967b77b38f0785c7` and server
  SHA-256 `687d5569130a7366cfe2abad2b6510ceec2acf8d618b49e9dab34c282aa94f20`.
  Both workflow script and common harness hashes match the frozen source manifest.
  The earlier 42/42 run remains valid for its own invocation; it cannot erase
  this failure. No retry-until-green loop was run.
- Local `timeout 15s am doctor health` returned 124 after reporting
  `local_config_unattested` and migration work on its private probe copy. This is
  no health verdict about the shared mailbox. The separate authoritative
  reservation snapshot was conflict-free and lease 9572 was granted for this
  document. No shared-mailbox repair or process takeover was attempted.
- Keyword and Rust AST scans found no `todo!()` or `unimplemented!()` calls in
  `crates`. Real handlers and known failures are stronger evidence than this
  absence. Explicit model/encoder substitutes remain proof limitations, not
  missing implementations inferred from a keyword.

Evidence roots: local/CSS `/data/projects/am-release-0334-artifacts-20260908`;
fresh execution on CSS under `reality-check-0908-7_egxbm3/` with the terminal
`final-release-workflow-receipt.json`, full stdout and each client's stderr and
history. Published release: https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.3.34.

### Vision checklist: present capability, remaining gap, owner

`WORKING` below is bounded to the named executed path. `PARTIAL` includes known
integration defects. `UNPROVEN` means the required current proof is absent, not
that implementation is absent. No whole-product completion percentage is used.

| # | Promise from README / governing contracts | Current assessment | Remaining work and existing coverage |
|---|---|---|---|
| 1 | Discoverable tools and real MCP dispatch | WORKING for exercised handlers; 45-tool source/retained discovery | Full per-tool closure: `br-l1q6z`, conformance. Registry counts alone prove no behavior. |
| 2 | Inbox/thread and other resources | WORKING for fresh inbox/thread; wider 25-resource contract PARTIAL | `resources.rs`, conformance and `br-4meup`; no empty-resource rewrite. |
| 3 | Send, reply, read/ack and explicit routing | WORKING sequentially in fresh release run; concurrency PARTIAL | `messaging.rs`, `queries.rs`; `br-kp1in.1/.2`, `br-sa58k`, `br-oeam8`, `br-e0blb`. |
| 4 | Broadcast intentionally forbidden | Implemented refusal and tests at both normalization/tool boundaries | Preserve rejection at `messaging.rs:1136,1891`; no broadcast feature task. |
| 5 | DB commit plus durable Git recovery ledger | PARTIAL: real persistence/archive pipeline and offline parity pass | `br-0flbu`, `br-htobc`, `br-kp1in.1/.2/.9/.10`, `br-8j6cb`; eventual archive is not synchronous commit. |
| 6 | Concurrent agents without wedges or corruption | PARTIAL: fresh three-client identity contention failed workflow | `br-kp1in.1/.2`, `br-22gm3`, `br-qz7rz`, `br-fkam4`, `br-9bwnb`; retain retry/cancellation histories. |
| 7 | Recover corrupt state without false health or source loss | PARTIAL; multiple full-suite failures; new source recovery fix `ad87cdb8` | `br-qdgio`, `br-r6psd`, `br-sd3md`, `br-kp1in.12`, `br-l1q6z`; validate new promotion behavior, not just a refusal. |
| 8 | FrankenSQLite runtime and structured async | Real fsqlite 0.3.18/asupersync 0.4.9; lifecycle guarantee PARTIAL | Canonical SQLite remains verification/recovery only. `br-tl2sg`, `br-22gm3`, engine probes; no policy/default reversal. |
| 9 | Scoped identities, contacts and durable topics | PARTIAL; identity sequential pass, concurrent Busy; new source contact repair `708ae6ba` | `contacts.rs`, `macros.rs`; `br-g6c0z`, `br-sgaee`, `br-qayvs`, current conformance gate. |
| 10 | Reservations, TTL/release and commit/push guard | WORKING for fresh offline lifecycle/guard; broader bounds PARTIAL | New source guard `4717f869` is not in release; `br-ssog9`, `br-l1q6z`; preserve advisory semantics. |
| 11 | Product bus and advisory build slots | Real handlers, current complete behavior UNPROVEN | Existing mixed workflow and conformance; `br-kp1in.1/.2`; membership/privacy and expiry remain acceptance. |
| 12 | Useful 16-screen TUI | Real implementation; current visual/interaction proof PARTIAL | `br-boq46`, `br-y8k4z`, `br-mljnz`, full TUI traversal; snapshots are not live input proof. |
| 13 | Web mailbox/archive review | PARTIAL; two archive HTTP failures and static-export failure | `mail_ui.rs:71` opens DB before archive dispatch; validated isolated routing repair remains unlanded under `br-l1q6z`. |
| 14 | Robot snapshots and noninteractive CLI | WORKING for sampled agents/list; broader truth PARTIAL | `robot.rs`; `br-4myjj`, `br-eru3j`, `br-49eak`; recheck stale titles before reimplementing fixes. |
| 15 | Reversible, scoped, owner-safe doctor | PARTIAL; real seven-op mutation/undo machinery, failed integrity cases | `doctor/mutate.rs`, `br-x2jf5`, `br-6u4hx`, recovery tasks; local unattested timeout is not live health. |
| 16 | Agent setup, config authority and secret safety | PARTIAL; substantial real integration and unfinished authority tasks | `br-siq0z`, `br-ww5js`, `br-q8k82`, `br-3cqr4`, `br-x5a8y`, `br-fphbm`. |
| 17 | Search V3 lexical and optional hybrid | Real integration; route/quality coverage PARTIAL | `search_service.rs`; `br-7x5fm`, `br-ku0kl`, `br-eh8bj`, `br-kp1in.7/.8`; portable release is lexical. |
| 18 | Useful optional summaries and compact output | PARTIAL; offline substitutes cannot establish model/encoder quality | `br-kp1in.7/.8`; actual selected model/encoder, scope canaries and semantic round trip required. |
| 19 | ATC learning with quiet, safe defaults | PARTIAL; fresh default-disabled workflow; learning test still red | `br-hwney`, `br-au76r`, `br-kp1in.1/.2`; isolated enabled fixture fix is not file-backed restart proof. |
| 20 | Share/export encryption and hosted verification | Real crypto/bundle code; complete current hosted path UNPROVEN | `share/src`, `br-ji2f0`, security/share real-path gates; no synthetic-only closure. |
| 21 | Signed installation/update and all supported venues | WORKING within retained platform/install/update scopes; release acceptance PARTIAL | GitHub/Homebrew/GHCR delivered; `br-bx73n`, `br-nq2kb`, `br-kp1in.3/.4`; native installer gaps disclosed. |
| 22 | Lossless legacy import and deliberate old-repo cutover | PARTIAL; source import/reopen fixes and unresolved authority tasks | `br-dbt24`, `br-lkhxw`, `br-mrfhc`, `br-p4s42`, `br-ajiq8`; no inferred cutover/deletion. |
| 23 | Current complete correctness and installed parity | PARTIAL: retained 12-red full run; later source has no new full verdict here | `br-l1q6z`, `br-bx73n`; ignored installed parity must run explicitly with exact paths. |
| 24 | Measured latency/resource budgets | UNPROVEN at current candidate/load; dated baselines only | `br-kp1in.5/.6`, `br-eru3j`, `br-y8k4z`; report workload, route, errors and resource growth with latency. |
| 25 | Honest docs and explicit public replay/browser boundary | PARTIAL; replay deliberately sanitized, live browser mirror deferred | `br-4meup`, `br-f9avw/.10`, `br-mq9q1`; stale gate/topology prose must not redefine current scope. |

**Would completing the existing backlog close the gaps?** It covers every
identified goal at the capability-family level. No new untracked feature family
was found in this refresh. But completing stale task descriptions literally
would not finish the project: some still call for Actions, claim absent containers,
or ask for already-landed scorecard code, while strict manual publication and
concurrent retry acceptance need explicit current criteria. Revise those existing
issues rather than create a second backlog. Implementation plus current real-path
acceptance closes bounded promises; it cannot prove universal zero deadlocks,
corruption or future performance.

## September 8 bridge and execution order

1. **Deliver the small existing repairs (S–M, highest immediate return).** Under
   `br-l1q6z`, review the isolated archive-routing, polling, environment and ATC
   fixtures against current main, reserve their paths, resolve drift, and land
   only reviewed changes. Archive dispatch must work from actual committed Git
   content with unusable DB configuration, retaining auth/method/path checks.
   Polling must return bounded errors without weakening cancellation semantics.
   `br-ue7e5` similarly retains the tested CORS `Vary` repair. Prior candidate
   passes are evidence to reuse, not proof of current delivery. Do not project a
   reduced full-suite failure count from focused tests.
2. **Finish database/recovery correctness (M–L, critical path).** Classify each of
   the 12 retained failures, distinguish fixture authority from physical integrity,
   and validate new recovery commit `ad87cdb8`. Preserve corruption negatives,
   same-process writer exclusion, hot-WAL truth, stable generation and no-clobber
   promotion. `br-kp1in.12` stays blocked until its snapshot semantics work with
   the actual runtime engine. Engine-level minimal reproductions remain separate
   from application workaround proof; do not silently swap the runtime engine.
3. **Make multi-process workflows meet their actual retry contract (M).** Extend
   `br-kp1in.1/.2` from the retained failed three-client registration. Choose and
   document whether first-attempt registration must succeed or bounded Busy retry
   is supported. If retry is allowed, retain every error and attempt, reuse the
   requested identity, prove one durable identity/profile, cap elapsed time and
   attempts, and complete the send/reopen/archive assertions. A fixed-delay
   rerun-until-green or suppressing `isError` does not count.
4. **Finish child/resource lifetime bounds (S–M).** `br-tl2sg` owns the detached
   stdout reader in `run_tmux_bounded`: caller timeout does not stop `read_to_end`
   when a descendant holds the pipe. Include real descendant and sustained-output
   fixtures, bounded memory, cleanup/join evidence and repeated-probe thread/FD
   counts. Keep large finite output and unavailable-pane behavior; no unsafe code.
5. **Connect current release gates to manual publication (M).** Keep Actions
   disabled. `br-nq2kb` must gate the DSR/manual path, using `br-kp1in.3/.4`'s
   existing candidate-bound code and `br-bx73n`'s installed parity/reliability
   receipts. Deliberately red lint/test, absent required suite, wrong binary and
   incomplete producer must prevent the local publish transition. A signed asset
   proves provenance, not correctness. No retroactive green release claim and no
   deletion/replacement of published assets.
6. **Earn sustained reliability and performance claims (L).** Complete the
   existing mixed histories `.1/.2/.9/.10` before the proposed 24-hour/two-host
   workload; the smoke must first complete. Use `.5/.6` to measure actual routes,
   offered/completed rates, p95/p99, queue occupancy, errors, memory/FD/disk and
   archive lag. A/A and paired windows matter for comparative claims; absolute
   budget misses remain visible even if comparisons have no verdict. Do not turn
   a historical gauntlet into a universal safety guarantee.
7. **Finish selected optional and user surfaces (M–L, independent lanes).**
   `.7/.8` require real selected dependency evidence. TUI input, scoped web/share,
   setup secrets and legacy import keep their existing owners and strong tests.
   Optional hybrid/LLM is not a hidden dependency of a lexical-only release.
8. **Reconcile the documentation last (S).** `br-4meup` must distinguish working,
   shipped, tested and deferred. Replace stale topology and absolutist claims
   with dated boundaries while preserving the actual goals. The root historical
   TODO's checked boxes are not present-day correctness certificates.

No implementation task waits on a new audit epic. Existing implementation/test
pairs remain `.1/.2`, `.3/.4`, `.5/.6`, `.7/.8`, `.9/.10`. Causal product repairs
can proceed while tests are authored; only certification waits on their result.

## September 8 skill execution and granular TODO

The frozen Phase 3a and Phase 5 instructions retained verbatim in the September 4
record below are applied again to the current gaps. Existing self-contained
Beads are the generated work graph: regeneration updates their current evidence,
implementation/test requirements and dependencies instead of duplicating them.

- [x] Read complete AGENTS and README; inspect later README/source drift.
- [x] Read skill and references; compare current contracts with prior full audit.
- [x] Inventory every tracker status, existing ownership, and initial `bv` triage.
- [x] Inspect source wiring, AST placeholder scan, release identity and receipts.
- [x] Execute actual released software in a fresh private remote workflow.
- [x] Investigate the failed phase and preserve its original terminal artifacts.
- [x] Produce 25-goal coverage matrix and concrete prioritized bridge.
- [x] Phase 3a: revise existing Beads with current evidence and acceptance.
- [x] Ambition round 1: improve repair delivery and contract-level integration.
- [x] Ambition round 2: improve fault/retry histories and resource bounds.
- [x] Ambition round 3: improve causal release gates and measurable claims.
- [x] Regenerate Beads from those improvements without duplicate tasks.
- [x] Refinement 1: stale scope, ownership and coverage.
- [x] Refinement 2: preserved behavior and realistic positive/negative tests.
- [x] Refinement 3: exact source, executed artifacts and evidence retention.
- [x] Refinement 4: dependencies and practical execution order.
- [x] Refinement 5: fresh convergence check, `br` cycles and `bv` triage.
- [x] Validate the document/tracker delta and report honest completion limits.

Process creation gate: this report is requested explicitly by the project owner
to decide the next implementation priorities. It catches the observed stale
assessment, red release gate and failed concurrent workflow. It earns zero
capability credit and is retired as active guidance when the next assessment
supersedes it; historical material is retained, not deleted. The highest-value
implementation remains the already-tested archive/runtime repairs, and this
assessment must hand back to that work after its requested phases.

### Ambition round 1 — complete the operator journey

The initial list fixes individual failures, but the user needs one coherent
mailbox through normal operation, trouble and recovery. Extend the existing
mixed workflow to cover **the same message identities and recipient state**
through live operation, verified archive fallback, supervised recovery and resumed
live operation. Check that a healthy primary still supplies fresh reads and that
archive-only browsing remains available when database opening fails. A successful
recovery must make new messages usable, not merely return a safe refusal.

This also improves delivery order: use separate, reviewable patches for archive
dispatch, polling, environment ownership and CORS, then run their combined real
workflow. Passing each isolated candidate is insufficient if the combined source
changes routing or cancellation. The later peer recovery/contact/guard changes
must be included in that integration candidate. Keep the safety negatives and
original feature behavior; do not turn the sequence into destructive production
recovery or add a second recovery runner.

### Ambition round 2 — separate safe retry from hidden overload

For the observed Busy response, an operation history must distinguish requested,
admitted, committed, replied and retried operations. Record first-attempt success,
eventual success, deadline failures and attempts per completed operation. A
healthy eventual result with rapidly growing attempts or queue/resource usage
still fails the declared operational budget. Retry only explicit retryable
responses within one original deadline; do not restart the clock per attempt.
Timeouts with unknown commit outcome require reconciliation before repeating a
mutation. This extends the existing bounded history checker rather than adding
another event schema or production telemetry service.

The cancellation boundary also includes retained pipes and output memory. The
tmux fixture needs a finite output control, an output-flood overflow case, an
inherited-pipe case and repeated probes. Cap accepted bytes before allocating
without limit, and prove the mechanism releases owned readers and children.
An error response at two seconds is not proof that resources were reclaimed.
Use independent bounds for caller latency, output bytes and cleanup grace.

### Ambition round 3 — make release acceptance follow the actual executable

The candidate-bound scorecard is valuable only if the manual publisher consumes
it. Define one immutable candidate input to the existing release entrypoint:
source and dirty-overlay identity, dependency closure, features, targets and
artifact digests. Required suites and installed parity must resolve to that same
candidate. Validate again immediately before the publish transition so a changed
artifact or substituted successful report cannot inherit an earlier approval.
Exercise the local transition with a valid positive and one altered term at a
time; no external test release is necessary.

Keep two conclusions explicit: distribution successfully published, and strict
acceptance satisfied. v0.3.34 meets the first within its documented scope; its
retained 12-failure workspace result prevents the second. An optional, unselected
hybrid/model lane cannot block a lexical-only candidate, but failed required
correctness cannot be relabeled optional after observing its result. The existing
release gate and its tests should enforce this; no second ledger or dashboard.

Bead regeneration adds these improvements to the existing implementation/test
pairs and publication owners. There are zero new issues: the contribution is
current, executable acceptance and a shorter route to delivery, not a larger queue.

### Five fresh refinement passes

1. **Scope and ownership:** checked all 25 rows against the complete issue
   inventory and current source. Updated obsolete manual-release/container task
   titles and attached the delivered GHCR evidence without claiming an unrun
   public pull. Kept every status/assignee intact. Candidate scorecard code is
   already present; its consumer/evidence is the remaining work. Gate development
   must not wait for every product repair; publication does. Shared runner file
   edits require one reservation owner at a time.
2. **Behavior and tests:** extended the existing history and performance
   companion tasks with legal Busy/retry/stable-identity and illegal duplicate
   histories, nonretryable auth/argument errors, deadline exhaustion, preserved
   read/ack state and completion-versus-amplification controls. Real process/DB
   observations remain decisive; synthetic counterexamples test the checker,
   not the product. No existing assertions, fixtures or functionality removed.
3. **Evidence identity:** verified the retained full-nextest log against its
   recorded SHA-256 and the fresh workflow/common-harness hashes against the
   release manifest. Separated released `0125f050`, full-test `43459c5a` and
   peer source `4717f869` (metadata HEAD `bd21aeb6`). The required installed
   parity/reliability packet remains missing from the retained release evidence;
   signatures and smoke do not substitute. Both successful and failed workflow
   invocations remain visible, without a new-engine regression claim.
4. **Dependencies and completion:** checked the real `.1/.3 -> .9 -> .10 -> .2`
   prerequisite chain and the independent budget/optional-quality pairs. Kept
   those edges; added no parent/child cycles or blanket product-bug dependency
   that would prevent authoring the publication gate. Found a genuine acceptance
   hole in `br-l1q6z`: its checklist required a recorded run but not restoration
   to zero unresolved required failures. Added that criterion explicitly so a
   well-documented red run cannot close the restoration task. Existing skips and
   leaky results must remain disclosed. `br ready` exposes the unowned budget
   lane, manual acceptance and tmux task; the tested repair lanes retain owners.
5. **Convergence:** rereviewed all 25 vision rows, the revised acceptance,
   implementation/test pairings, preserved defaults and source/evidence
   boundaries. No further scope or dependency changes were needed. Fresh `br`
   inventory has the same 2,457 IDs, zero additions/removals, and zero status or
   assignee changes. All literal Bead IDs in this assessment resolve. Final
   `bv --robot-triage` completed; `br dep cycles --json` reports zero active
   cycles. `git diff --check` passes. Scoped UBS exits 3 because Markdown is
   unsupported: it ran no scanner and is not reported as a pass.

### Honesty and work audit

Window: this September 8 reality-check request, with the prior release packet
reviewed specifically for claims being relied on. The product's purpose is
persistent, auditable coordination between coding agents. Output classification:
one updated assessment and the existing Bead revisions are **PROCESS**; no USER
or ENABLER implementation is claimed. The fresh actual-binary run provides
evidence, not a new feature. This is bounded work requested by the operator;
its next consumer is the implementation owner choosing the repair sequence.

Creation worksheet: running code does not branch on this report. The explicit
operator request supplies its consumer and decision gate; observed defects are
the stale assessment, failed concurrent workflow and unmet release acceptance.
It retires as active guidance at the next assessment. The integrity-control
exception is unnecessary: the explicit request already qualifies it. Its minimal
form is this existing document and existing tracker, not another report system.
An hour on the existing archive/polling repairs would yield more runtime benefit;
the assessment therefore stops here and hands off to them.

Real-work worksheet: no feature shipped in this audit to demonstrate; the earlier
release and new peer fixes are not credited to it. Without the process work, the
same binaries would exist, but the user would lack this requested correction and
prioritized plan. No speculative runtime enabler was created. Long-standing
durability/corruption work (`br-htobc` among the oldest actionable rows) remains
valuable; this window addressed the explicitly requested steering instead.
No new swarm was dispatched, no closure count is used as progress, and no task
was split to close an unmet parent. Disposition: legitimate requested assessment,
zero capability credit; another planning round would be drift.

Honesty inventory, numbered against the skill worksheet:

1. No test weakened/deleted/ignored (checked: this turn's diff contains only this
   document and Beads; the restoration criterion was strengthened).
2. No test double introduced (checked: no test/source edits).
3. No golden regenerated (checked: no test/fixture edits by this audit; peer
   contact fixture changes are separately identified, not certified here).
4. No validator/CI relaxation (checked: no runtime gate edits; any `[skip ci]`
   commit annotation follows the user's explicit no-Actions instruction).
5. No fabricated gate satisfaction (checked: red full suite and failed workflow
   are retained and prevent a strict-ready claim).
6. No zero-run green (checked: 41/1 fresh assertions; AST zero matches is only a
   scan result; UBS exit 3 is explicitly no scan).
7. No unrun-command claim (checked: full suite is retained evidence at its named
   source, not a fresh run of the later main; missing parity is disclosed).
8. No fixture upgraded to live proof (checked: actual release binaries and real
   transport/storage for the fresh run; model/encoder substitutes stay limited).
9. **Correction required for prior release wording:** publication completion must
   not mean every release gate passed. The retained 12-failure suite and absent
   mandatory parity/scorecard packet leave strict acceptance unfinished. This
   correction is explicit here and on `br-nq2kb`/`br-bx73n`; failed required
   tests cannot be made optional after the result is known.
10. No cited command hides stderr (checked: failed client stderr and suite output
    retained; unsupported UBS and initial inventory-parser errors disclosed).
11. No unmet item closed (checked: all initial statuses and assignees preserved).
12. No requirement rewritten to match success (checked: zero-failure gate retained;
    concurrent Busy retry remains an explicit acceptance decision with negatives).
13–17. No new delegated agents or accepted subagent assertions in this window;
    peer commits are source observations, not independently verified tests or
    agreement-based proof. No refusal farming or delegated closures are credited.
18. Denominators are the complete retained run and the complete fresh workflow;
    no passing subset substitutes for either.
19. The moments requiring explanation are the initial inventory parser expecting
    an array instead of the returned object, one optional-field parser correction,
    the failed workflow and the release-completion distinction. Parser errors
    were corrected before deriving inventory claims; product failures stay red.
20. Strongest evidence: re-executable release workflow with exact binary/harness
    hashes, 41 passing assertions, original failed client stderr and joined process
    outcomes. Source/Bead validation is a separate, reproducible document check.

No additional CASS-wide older-session honesty sweep is claimed; this window uses
the retained release receipts and current source directly. The prior-release
wording correction is recorded in place, disclosed to the operator, and guarded
by explicit zero-failure acceptance on `br-l1q6z` and the manual publisher task
(`RH-9` follow-up laundering / `RH-10` plan edits cannot stand in for delivery).
Review is solo and bounded, not independent verification of new peer code.

**Handoff:** all requested skill phases are complete. The project remains
unfinished for the reasons above; implementation Beads remain open. Resume the
existing repair-delivery lane, then complete actual concurrent/recovery workflow
acceptance and the manual release gate. Do not create another planning campaign.

## Historical September 4 assessment (superseded)

<details>
<summary>September 4 assessment and earlier retained proposal; use September 8 guidance above</summary>

**Current assessment:** 2026-09-04, GentleBeaver, requested full `reality-check-for-project` workflow.
**Source reviewed:** `a6ebaf5057ad40f5de16b3c44b35e8472bf5c699` on `main`; earlier observations began at `ff47e953`. The checkout changed during the audit, so observations identify their actual revision or binary.
**Installed executable:** `am 0.3.32`, SHA-256 `8eea1560ece6d30912af107d2de5f4f66e9a8b4ece2d447ea239c0f6b23975d3`. This is not evidence that the installed executable contains current `main`.

This September 4 assessment supersedes every recommendation in the collapsed September 2 proposal below. That proposal is retained as historical evidence, not as instructions to perform repairs, delete artifacts, change defaults, or reopen completed work. No production code, shared mailbox repair, release publication, or engine migration is part of this assessment.

## Assessment and evidence boundaries

**Agent Mail has a substantial real implementation, but current evidence does not establish a finished, reliably releasable product.** Messaging, persistence, recovery, search, CLI, TUI, web, guard, share, and ATC contain real code and extensive tests. The strongest concerns are the failed basic installed-binary send, a live unhealthy integrity verdict, unresolved engine/visibility cases, unavailable current full-suite verification, and release evidence that is not sufficiently tied to a particular candidate. A count of closed Beads is not a completion percentage.

The audit read all of `AGENTS.md` and `README.md`, the root and `docs/planning` vision/transition/architecture plans (including full Python-behavior extraction), accepted and superseded ADRs, current SPEC/design documents, release/rollout plans, ATC/search/TUI contracts, and verification/performance ledgers. Identical duplicates were verified by content hashes; divergent copies were read through their differences. Historical results remain historical. Keyword, Rust AST, and behavioral inspection were combined: the `todo!`/`unimplemented!` AST scans found no hits in `crates`, while inspected stub encoders and LLM fixtures are explicit test substitutes. This does not prove every implementation correct.

| Evidence | Actual result | What it establishes |
|---|---|---|
| Isolated installed-binary stdio workflow | Initialize, list 45 tools, list resource templates, create project, and register BlueLake/GreenStone passed. First `send_message` returned JSON-RPC `-32004`, `Request timeout exceeded`, after 30,003 ms. Server exited 0 after stdin closed. | Basic workflow failed in this environment. Inbox/ack/search/reservation/build-slot/broadcast cases after send were **not reached**. No causal diagnosis of the timeout or claim of current-main regression. |
| Live MCP `health_check` | Red integrity verdict: a prior failed full check remains authoritative despite a later passing quick check. Other reported sections green; archive/DB message totals both 40,479 at observation. | A quick check must not erase known full-check failure. Equal aggregate counts do not prove row-level parity or healthy storage. No repair was attempted. |
| Live session coordination | `macro_start_session` timed out at 30 s in unattributed blocking dispatch, but GentleBeaver registration persisted. Granular reservation of this plan succeeded; inbox was empty. | A timed-out composite request can have partial effects. Repeating the whole macro without checking state is unsafe as a correctness assumption. Stage was not attributed to SQLite or archive. |
| Standalone `am doctor health` | Exit 1 with `local_config_unattested`; private probe reported six reservation-field differences. | Not an attested probe of the same live target. Do not conflate this result with the live MCP integrity verdict. Private probe artifacts were retained. |
| Current-source remote nextest, source receipt | Rejected non-regular `.doctor/latest` symlink during transfer. | `NO_VERDICT`: zero tests ran. Do not remove the symlink to satisfy transfer. |
| Current-source remote nextest, clean overlay at `a6ebaf50` | Cargo manifest loading failed: gated sibling `../frankensearch-rel-0332/frankensearch/Cargo.toml` missing on worker. | `NO_VERDICT`: no compilation or tests. Existing `br-rch-frankensearch-closure-jdgvg` owns dependency closure. No local build fallback used. |
| Latest full-suite result recorded in Beads | `br-l1q6z`, comment 2677: gate 8 at `a04a5807`, September 3, 17,270 run, 16 failed, 37 skipped. Later comments report focused fixes. | Historical recorded result, not independently replayed here. Neither the old ~212 nor projected 14 is a current measured failure count. |
| Public release and automation, live GitHub API | Latest release v0.3.32, September 1, six target families. Core CI/dist/docker/publish workflows disabled manually. Issue-138 one-shot succeeded September 4; latest coordinated release-risk runs failed. | Release assets exist; this audit did not install/verify each asset. A specialized green job is not a full release gate. |

Local evidence retained: `/tmp/am-reality-runtime-v2-20260904.log`, `/data/tmp/am-reality-20260904-ldf22flm/{summary.json,transcript.jsonl,server.stderr}`, `/tmp/am-reality-nextest-20260904.log`, `/tmp/am-reality-nextest-clean-20260904.log`, `/tmp/am-reality-health-20260904.log`, and `/tmp/am-reality-beads-initial-20260904.json`. The first probe's missing `notifications/initialized` was corrected in the harness; that protocol error is excluded from product findings. These host-local paths are audit breadcrumbs, not durable release receipts.

Public references: [v0.3.32 release](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/tag/v0.3.32), [successful narrow one-shot](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/actions/runs/33890295033), [failed release-risk run](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/actions/runs/33882137179). Workflow state was obtained through the live API rather than cached Actions HTML.

## Vision checklist and coverage

`WORKING (bounded)` below applies only to the behavior actually exercised. `PARTIAL` means implementation exists with known missing or failing integration; `UNPROVEN` means this audit could not establish the promised behavior. Source presence and historical tests are explicitly distinguished. No whole-product percentage is assigned.

| # | Testable promise and authority | Current status and implementation evidence | Remaining coverage / disposition |
|---|---|---|---|
| 1 | 45 discoverable tools; README tools/AGENTS registry | WORKING (bounded): installed `tools/list` returned 45; real handlers in `tools/src` | Per-tool behavior remains part of `br-l1q6z` and conformance, not proven by listing. |
| 2 | 25 logical resource contracts; README/AGENTS | PARTIAL: `tools/src/resources.rs`; recent-ring/filter implementation now real (`br-ciwph` closed) | Preserve 25 logical templates versus raw handler count; `br-4meup`, current conformance gate. Do not recreate the fixed empty-resource defect. |
| 3 | Explicit recipients, replies, topics, read/ack, retry; README | PARTIAL: `messaging.rs`, `queries.rs` transactional inserts and idempotency; installed send timed out | `br-sa58k`, `br-oeam8`, `br-e0blb`, `br-l1q6z`; new mixed-workflow acceptance. |
| 4 | Never broadcast; AGENTS Rule 2 | UNPROVEN in fresh execution, implemented refusal at tool boundary | Keep rejection and its tests; include negative control in mixed workflow. No feature request to enable it. |
| 5 | Git archive plus authoritative SQLite index; README durability contract | PARTIAL: transactional DB first, deferred archive/backlog in `messaging.rs` and `storage/src/lib.rs` | Require bounded eventual mirror convergence, not archive-before-ack; `br-0flbu`, `br-htobc`, new workflow/history coverage. |
| 6 | Correct swarm concurrency and deadlines; README gauntlet | PARTIAL: real stress suites, but failed installed send and live macro | `br-9bwnb`, `br-eru3j`, `br-22gm3`, `br-qz7rz`, `br-fkam4`, new sustained process workload. Old gauntlet is a dated run, not zero-failure warranty. |
| 7 | Safe recovery, no source loss or false health; mailbox SPEC/AGENTS | PARTIAL: guarded family/recovery receipt and quarantine machinery; live integrity red | `br-qdgio`, `br-r6psd`, `br-sd3md`, `br-zchj0`, `br-oyget`, `br-jcgxg`, `br-y1elw`; existing chaos suite retained. |
| 8 | Runtime engine policy and cancel-aware async; AGENTS | Implemented policy, end-to-end UNPROVEN: FrankenSQLite 0.3.16 runtime; canonical C SQLite verifier/recovery exception; asupersync | No new engine ADR required. Keep `BEGIN IMMEDIATE` default; upstream/visibility cases and `br-of0ra` remain. Never infer permission to enable concurrent mode. |
| 9 | Reservation leases, contact policy, identity lifecycle; README | PARTIAL: real typed DB paths and lease artifacts; isolated registration and live plan lease passed | `br-ssog9`, `br-g6c0z`, `br-qayvs`, lifecycle/import tasks; prove renew/release/retirement across restart. |
| 10 | Build slots and cross-project product bus; README | UNPROVEN current run: real `build_slots.rs`, products handlers | Advisory slot conflicts are not mutex exclusion. Mixed tests must preserve product membership and contact constraints. |
| 11 | 16-screen operational TUI; README/TUI V2 contract | UNPROVEN current rendering: real screens and snapshots, historical coverage | `br-boq46`, `br-y8k4z`, `br-mljnz`; require actual render/interaction receipts, not catalog counts. |
| 12 | Web/mail routes and scoped archive browsing; README/web SPEC | UNPROVEN current HTTP run: real `server/src/mail_ui.rs` routes reuse bounded pool/Cx | Existing HTTP/security suites in current gate; browser-wide parity proposal is deferred, not an unfinished shipped promise. |
| 13 | 19 robot commands and usable noninteractive CLI; README | PARTIAL: real `cli/src/robot.rs` and command dispatch; historical speedups | `br-eru3j`, `br-49eak`, `br-22gm3`, `br-4myjj`, `br-4meup`; native benchmark coverage. |
| 14 | Reversible, scoped doctor operations; AGENTS/doctor SPEC | PARTIAL: `doctor/mutate.rs` seven Op variants, backups/witnesses/undo and owner guard | `br-qdgio`, `br-r6psd`, `br-x2jf5`, `br-l4fk6`; same-target health attestation required. Never repair the shared mailbox as an audit side effect. |
| 15 | Bare `am`, setup, auth and configuration authority; README | PARTIAL: real dispatch/setup; substantial unfinished OAuth/config/credential tasks | `br-siq0z`, `br-ww5js`, `br-q8k82`, `br-vzh8o`, `br-x5a8y`, `br-fphbm`, `br-17tyc`. |
| 16 | Lexical release search; optional source hybrid; README Search V3 | PARTIAL: `search.rs` and `search_service.rs` real routes/fallback diagnostics | `br-7x5fm`, `br-ku0kl`, `br-eh8bj`; real quality suite exists, fresh feature-specific result absent. Do not promise hybrid in portable release artifacts. |
| 17 | Useful optional LLM summaries and TOON; plans/README realism policy | UNPROVEN decisive dependencies: `test_llm.sh` sets LLM stub; `test_toon.sh` simulates encoder output | NO_BEAD for fresh real-provider/real-encoder compensation found among unfinished tasks; new optional-quality lane. Offline substitutes remain useful R2/R3 evidence. |
| 18 | ATC learning, quiet defaults and safe effects; ATC SPEC | PARTIAL: real policy algorithms, experience seam and historical real canary; not fleet-duration proof | `br-hwney`, `br-au76r`; add restart/quiet/scoped-effect scenarios to sustained workflow. Keep default shadow/write-off. |
| 19 | Signed/private share exports, guard enforcement; README/threat SPEC | UNPROVEN current run: real scrub/crypto/snapshot/guard code and existing real tests | `br-ji2f0`, `br-vzh8o`, `br-x5a8y`, current security/share/guard gate; hash and authorization negative controls. |
| 20 | Verified installer/update, native targets, container/crate delivery; README/release plans | PARTIAL: six release families and real verifier; current venue/artifact tests incomplete | `br-nq2kb`, `br-8tszv`, `br-54v1u`, `br-1csqy`, `br-quq4r`, `br-bx73n`, `br-ocys6`, `br-95spu`, `br-7ilwx`, `br-s1ejh`, `br-p9zor`. |
| 21 | Lossless legacy import and explicit old-repo transition; transition plan | PARTIAL: real import/recovery; external cutover decision not inferred | `br-dbt24`, `br-lkhxw`, `br-8echk`, `br-mrfhc`, `br-p4s42`, `br-ajiq8`. No deletion or forced cutover. |
| 22 | Current complete checks and reliable test orchestration; AGENTS | UNPROVEN at audited HEAD; historical full gate red and remote attempts blocked | `br-l1q6z`, `br-rch-frankensearch-closure-jdgvg`, `br-sox5q`, `br-e38t0`; fresh source/ELF/terminal evidence required. |
| 23 | Honored latency/resource budgets; README/benches | UNPROVEN current performance: native benches and historical baselines exist | NO_BEAD found for current route-specific baseline admission/calibration; new operational-budget lane. No latency extrapolation from registry calls. |
| 24 | Trustworthy docs and Beads; README/AGENTS | PARTIAL: widespread useful docs, but historical topology and former plan advice stale | `br-4meup`, `br-l1z6f`, `br-cdwya`, `br-of0ra`; this report corrects the plan now. |
| 25 | Public dashboard replay and explicit browser limits; current AGENTS | UNPROVEN deployed bundle here: standalone dashboard exists; live browser mirror deferred | Preserve `br-f9avw`/`br-f9avw.10`, `br-mq9q1`; do not exclude this current surface as the September 2 proposal did. |

Initial Beads snapshot: 2,430 issues, 2,276 closed, 86 open, 68 in progress. `bv` found no cycles; 148 actionable and six dependency-blocked unfinished items. Its response nests `quick_ref` under `triage`; a null from the wrong jq path does not show graph corruption. Forecast 53.11 days at confidence 0.4029 is not an engineering ETA. Existing claims/assignees stay intact.

## Bridge: concrete gaps and implementation sequence

All new work extends existing modules/runners. No second issue tracker, evidence dashboard, schema registry, async runtime, or replacement test framework is proposed. Each implementation task has a companion test task with real-path coverage and terminal artifacts. Existing issue scopes are retained and receive current observations rather than duplicate ownership.

| Gap | Severity / kind | Change, acceptance and starting surface | Size / dependencies |
|---|---|---|---|
| G1: basic send and partial composite timeout | P0 integration investigation | Attach exact installed-binary transcript to `br-l1q6z`; compare the same scenario on a worker-built current candidate. Attribute elapsed stages and reconcile durable outcome before retry. `br-9bwnb` receives live macro context without claiming common cause. | S reproduction; fixes through existing owners; current RCH closure first for source validation. |
| G2: current durability/recovery truth | P0 correctness | Complete existing message ID, family-generation, WAL/oracle and recovery tasks above. Add bounded mixed-process stdio/HTTP workflow to `e2e_runner.rs`, `load_bench.rs` and existing soak suites; reuse incident corpus. Preserve DB-first acknowledgement and eventual archive semantics. | L runner + M companion tests; start independently, certify after causal fixes. |
| G3: candidate-bound release evidence | P0 proof/integration | `newest_incident_scorecard` currently selects global newest mtime with five-second slack. Bind incident, suite, executable, source, lock/dependency closure, features, target and run ID; reject mismatches or incomplete required suites. Extend `write_release_scorecard` and existing producers. | M implementation + M negative/e2e tests; feed `br-nq2kb` and `br-bx73n`, do not replace their publication/venue work. |
| G4: honest current workspace verification | P0 proof | Refresh `br-l1q6z` from last terminal run, keep ignored/flaky cases classified, repair RCH sibling transfer with owner. Run required check/clippy/fmt and process-isolated nextest, then real required E2E suites. A worker/setup refusal is not a pass. | Existing tasks; no new blanket "fix all tests" epic. |
| G5: operational budgets with route truth | P1 performance/proof | Extend `cli/src/bench.rs` and `benches/BUDGETS.md` with separate cold/warm, actual SQL/Tantivy/hybrid route, queue/acquire/write/archive spans, memory/FD/disk, per-host baseline identities and bounded load admission. Run controls before accepting relative effects. | M implementation + M companion control tests; current candidate and quiet measured host. |
| G6: real optional dependency quality | P1 proof | Extend existing search-quality, LLM and TOON suites with selected real dependencies, explicit effective mode, bounded corpus/model/tokenizer IDs, scoped results and compensated offline lanes. Missing dependency is `NO_VERDICT` for that feature, never synthetic quality success. | M implementation + M companion tests; no paid provider calls or feature-default changes in this audit. |
| G7: ATC restart, quiet and feedback behavior | P1 integration/proof | Extend mixed workflow with shadow/write-off zero-effect invariants, controlled restart/hydration and explicit isolated effect-enabled scenarios. Compare policy decisions with recorded outcomes, noise and recovery, not merely experience-row count. | M scenario extension; depends on existing `br-hwney`/`br-au76r` for certification, not for writing failing tests. |
| G8: existing delivery/security/import/UI/docs gaps | P0–P2 existing implementation | Execute mapped owner tasks; publish only tested target/feature claims. Add current acceptance to `br-4meup` for verifier exception, lexical artifacts, deferred browser scope, dated ledgers and safe commands. Preserve existing substantive tests and manual decision tasks. | Existing backlog; independent lanes, no automatic bulk reassignment/closure. |

Proposed fleet acceptance is **a new validation target**, not a retroactive README guarantee: 24 hours on each of two explicitly identified hosts, at least 13 projects, 90 actual client processes and 300 messages/hour, with mixed leases/reads/acks/replies/products and controlled restarts. A bounded smoke is required first; run duration/rate/process census and all interruptions are recorded. Success requires no lost acknowledged message, no duplicate committed idempotent send, no cross-scope exposure, no corruption, no stranded owned child, and drained archive/DB reconciliation. Numerical performance thresholds must be measured and justified per route; no blanket p99 < 2 s assertion or automatic budget inflation.

Release acceptance order: establish exact candidate/dependency closure → obtain current correctness results → bounded real workflow and fault tests → sustained/performance/optional-quality evidence → candidate-bound scorecard → existing publication gate. Harness development, UI/docs fixes and optional test design can proceed in parallel; final certification alone waits on upstream fixes. Do not block every task on an epic or on an unresolved engine-policy decision already settled by AGENTS.

## Workflow execution log

Phase 1 and Phase 2 completed in this revision. Phase 3a begins from the gaps above; the remaining entries are updated as the rounds actually run. The frozen instruction used for bead generation is retained verbatim:

```text
OK so please take ALL of that and elaborate on it and use it to create a comprehensive and granular
set of beads for all this with tasks, subtasks, and dependency structure overlaid, with detailed
comments so that the whole thing is totally self-contained and self-documenting (including relevant
background, reasoning/justification, considerations, etc.-- anything we'd want our "future self" to
know about the goals and intentions and thought process and how it serves the over-arching goals of
the project.) The beads should be so detailed that we never need to consult back to the original
markdown plan document. Remember to ONLY use the `br` tool to create and modify the beads and add
the dependencies.
```

Phase 3a baseline created with `br` only: epic `br-kp1in`; workflow implementation/tests `.1`/`.2`; candidate evidence implementation/tests `.3`/`.4`; operational budgets implementation/tests `.5`/`.6`; optional quality implementation/tests `.7`/`.8`. Existing `br-l1q6z`, `br-9bwnb`, `br-rch-frankensearch-closure-jdgvg`, `br-bx73n`, `br-nq2kb` and `br-4meup` received current evidence without reassignment or closure. The original ~212 failure count remains historical evidence; the title and body now ask for a fresh complete result. Baseline graph cycle check passed.

### Ambition round 1: make completion decidable for each shipped capability

The baseline is useful, but a single green aggregate could still hide an untested feature or demand optional dependencies from every release. The revised plan makes the unit of acceptance `(candidate, target, feature selection, capability, decisive path)` rather than a repository-wide boolean. Required capabilities must each have a nonempty required suite/case set and real-path receipt; optional unselected capabilities are explicitly outside that candidate's claim. A selected but unavailable capability is `NO_VERDICT`, not success. Historical certificates remain readable but do not satisfy a changed candidate.

The scorecard producer must capture the executable it actually launches, rather than resolve `am` again from PATH at report time. It must carry the gated sibling revision and effective route, not only a top-level commit. Artifacts are passed through the existing runner's owned invocation directory and exact child receipt; no `latest` directory or mtime heuristic supplies causal identity. Mutation-sensitive negative controls must show that changing one identity or removing one required case changes acceptance.

Risk order is explicit: source-closure and evidence binding can proceed while product owners fix deadlines and recovery. Current source correctness precedes speed certification. Optional quality work is independently claimable, and release readiness for a lexical-only candidate never silently expands to hybrid or provider-backed summaries. This round improves `.3`/`.4` and `.7`/`.8`; regeneration occurs after all ambition rounds.

### Ambition round 2: exercise failure boundaries without inventing guarantees

The process workload must describe its topology: many real HTTP clients against the supervised mailbox owner, separately bounded stdio sessions, and explicitly admitted standalone process races. Ninety clients do not mean ninety forbidden simultaneous owner daemons. Existing `db/tests/load_bench.rs` already runs full integrity and exact message/recipient counts before and after reopen; retain that coverage and add user-visible histories and archive convergence rather than rebuilding it.

Failure cases are divided by operation boundary: before admission, before commit, after commit but before response, during archive drain, during owner restart, and during recovery-generation promotion. Requests with lost replies remain indeterminate until durable reconciliation. Idempotency guarantees apply only within the configured retention window and key scope. ID gaps are legal; uniqueness and durable election are the properties to check. Broadcast refusal, contact checks, bcc privacy, advisory reservations/build slots, and product membership are preserved rather than replaced with a generic queue model.

Admission and backpressure need their own observable cases: slow reader, stalled archive worker, saturated pool, canceled waiter, and recovery refusal. Every selected fault targets an owned fixture, has a bounded duration and cancellation path, and records remaining work at the deadline. Default read paths are tested for no unintended writes. ATC noise is reported against actual decision opportunities and labels; zero observed alerts is a sample, not proof of zero false-positive probability. Hosts and measurement windows remain separate.

This round also requires an immutable input manifest for corpus/budgets and exact artifact locations; the existing soak trend code's global newest-file searches cannot supply acceptance evidence. The current source has useful before/after integrity checks already, so no new bead asks to add those from scratch.

### Ambition round 3: bounded history checking and defensible measurement

The most useful mathematical addition is a small specification of observable state transitions and a bounded partial-order history checker. For the transactional mailbox core, search for a legal sequential history consistent with each client's request/response order, durable commit witnesses and explicitly synchronized barriers. Do not invent a total order from wall clocks on different hosts. A timeout can be completed or omitted according to later durable evidence; it cannot automatically count as a failed write. The checker must report `PASS`, a minimal counterexample, or `NO_VERDICT` when evidence or exploration bounds are insufficient.

Use different properties for different consistency contracts: linearizable idempotent insert/recipient transactions; causal read/ack constraints; generation-aware recovery; eventual archive convergence only after bounded drain; reservation/build-slot behavior as actually promised; and per-scope authorization for search/products. Partition independent project/key histories only when shared global ID/generation constraints remain checked. This is a focused extension of the existing test helpers, not a model checker for all 45 tools or an alternate implementation of the server.

Add a companion test task with hand-auditable legal and illegal histories: duplicate commit after retry, acknowledged row missing after restart, ID reuse across processes, stale-generation lease resurrection, unauthorized result, allowed transient archive lag, truncated trace, and exploration exhaustion. At least one trace comes from real transport and recovery rather than fabricated events. Negative fixtures prove the checker rejects known bad behavior; property tests alone cannot establish a real runtime path.

For performance, separate the operational SLO predicate from the comparative estimator. Use paired samples and a predeclared A/A tolerance; retain sample counts, uncertainty and independent windows. A failed null or contaminated window invalidates a relative claim without erasing an absolute latency failure. For ATC, report false effects/decision opportunities and label availability; confidence bounds require their independence assumptions to be stated. These techniques make failures interpretable rather than decorate the plan with unused mathematics.

Phase 3a regeneration completed with `br`: added `.9` history checker and `.10` companion tests under `br-kp1in`, and appended each applicable ambition improvement to the original eight children. No implementation was started as a shortcut around refinement.

### Phase 5: five plan-space refinement passes

`AGENTS.md` was reread in full before this phase. The frozen instruction applied to each pass is:

```text
Reread AGENTS.md so it's still fresh in your mind.
Check over each bead super carefully-- are you sure it makes sense? Is it optimal? Could we change
anything to make the system work better for users? If so, revise the beads. It's a lot easier and
faster to operate in "plan space" before we start implementing these things! DO NOT OVERSIMPLIFY
THINGS! DO NOT LOSE ANY FEATURES OR FUNCTIONALITY! Also make sure that as part of the beads we
include comprehensive unit tests and e2e test scripts with great, detailed logging so we can be
sure that everything is working perfectly after implementation. Make sure to ONLY use the `br` cli
tool for all changes, and you can and should also use the `bv` tool to help diagnose potential
problems with the beads.
```

Pass outcomes are recorded below as completed, with actual changes and checks.

1. **Ownership, scope and dependencies:** checked every new bead against existing tasks and AGENTS. Added `.9 → .3` for the candidate record contract and `.2 → .10` for validated history checking before final workflow certification. No cycle; `.1`, `.3`, `.5` and `.7` remain implementation entry points. Optional quality does not block lexical-only certification.
2. **Feature preservation and test realism:** added cursor expiry/ahead versus message-ID cases, configurable read receipts, macro partial effects, candidate-specific retry support, per-case substitute compensation, offered/completed-rate accounting to prevent coordinated omission, and distinct native/external TOON routes. Every implementation has a companion test bead; real transport/persistence remains decisive.
3. **Artifact integrity and observer failure:** added unique invocation directories, atomic report publication, digest/path/symlink checks, bounded recorder queues/disk, incomplete-trace refusal, privacy canaries, provider budget/cancellation controls, and clock-ordering limits. Tests challenge artifact failures as well as product failures; the observer cannot discard bad samples and report success.
4. **Execution and authority:** checked all 25 vision rows and all referenced Bead IDs; refreshed the misleading historical headlines of `br-l1q6z` and `br-bx73n` without discarding their evidence. Added an explicit field-contract handoff for the two runner-editing tasks, frozen premeasurement budgets and capability-specific closure. `br ready` exposes `.1`, `.3`, `.5`, `.7`; the epic shown by `bv` is coordination, not a fifth code lane. `git diff --check` passed. Scoped UBS returned exit 3 because Markdown/JSONL are unsupported; no scanner ran, so this is not claimed as a code scan pass.
5. **Convergence:** rereviewed all 11 new issues, five implementation/test pairs, eight blocking edges, 25 vision rows and existing-task mappings. No further scope, acceptance, ownership or dependency changes were needed. `br dep cycles --json` returned zero cycles. `bv --robot-insights` skipped cycle analysis above 2,000 nodes, so its absent cycle output is not a proof; `br` supplied the explicit check. The JSONL delta contains exactly 11 added and six updated issues, zero removed, with all existing assignees/statuses preserved.

### Final handoff

All applicable skill phases are complete: assessment, bridge plan, initial Beads, three ambition rounds, Bead regeneration and five refinement passes ending without another plan change. Implementation remains open under `br-kp1in`; this audit does not close reliability, release or quality claims.

| Lane | Implementation | Companion tests | Ready implementation? |
|---|---|---|---|
| Mixed process workflows and ATC | `br-kp1in.1` | `br-kp1in.2` | Yes; agree on runner field contract with `.3` |
| Candidate/run-bound scorecards | `br-kp1in.3` | `br-kp1in.4` | Yes; first P0 recommendation |
| Operational routes and budgets | `br-kp1in.5` | `br-kp1in.6` | Yes |
| Real optional dependency quality | `br-kp1in.7` | `br-kp1in.8` | Yes for implementation; real acceptance requires selected dependencies |
| Bounded history consistency | `br-kp1in.9` | `br-kp1in.10` | Waits on `.1` and `.3` |

```mermaid
flowchart LR
  W[.1 Mixed workflow] --> H[.9 History checker]
  E[.3 Candidate evidence] --> H
  H --> HT[.10 Checker tests]
  W --> WT[.2 Workflow certification]
  HT --> WT
  E --> ET[.4 Evidence tests]
  P[.5 Operational budgets] --> PT[.6 Control tests]
  Q[.7 Optional quality] --> QT[.8 Real dependency tests]
```

Final observed inventory: 2,441 issues = 2,276 closed + 97 open + 68 in progress. `br ready` confirms four new code-bearing entry points; no existing task was claimed, reopened or closed. Recheck source, readiness and reservations when starting implementation because this is a shared active checkout.

Validation of this deliverable covers Markdown structure, 25-row coverage, live Bead IDs, JSONL preservation, exact dependency edges and `git diff --check`. No Rust source changed. Current-source execution remains `NO_VERDICT` for the two documented RCH prerequisites; the installed-binary failed workflow and historical full-suite result retain their narrower provenance. The shared mailbox and peer `lock-sync/` directory were preserved.

## Historical September 2 proposal (superseded)

<details>
<summary>Retained historical proposal at f85b22c2; all actions and status claims below are superseded by the September 4 assessment</summary>

Original scope: Phase 2 only, 23 goals, 19 gaps, 13 previously created landing beads. The earlier proposal's engine decision, default changes, cleanup advice and unfinished-phase statements are historical and must not be executed as current instructions.

---

## 0. What moved since the reality check (2026-09-01 evening wave)

Commits `ba1b8a42`, `888aea20` (partly a parallel session's sweep of this working tree) and `f85b22c2` changed the status of several goals. The plan below is written against the state after those commits.

| Goal | Reality check | Now | What changed |
|---|---|---|---|
| 6 self-healing recovery | PARTIAL | PARTIAL (better) | breaker self-poisoning inside a process's own recovery admission fixed; false P0 on a healthy live mailbox fixed; read-only CLI processes no longer counted as owners; engine-dispatching read-only opener lets restored/reconstructed families be inventoried, backed up, salvaged and reconciled; promotion retires stale namespace records |
| 10 robot mode | PARTIAL | PARTIAL (better) | `am robot handoff` 600 s timeout → 0.30 s on the 40k-message mailbox, `--max-seconds` budget; `am robot metrics` reads live daemon counters |
| 11 doctor | PARTIAL | PARTIAL (better) | four disagreeing verdicts reduced: doctor health no longer P0 on a healthy file; doctor source selection by namespace authority; `doctor reconstruct` can salvage the current database |
| 15 description parity | NOT ENFORCED | ENFORCED (WORKING) | `tool_description_parity.rs` compares types through nullable arrays instead of passing on mismatch; 55-entry error catalog |
| 16 all tests green | REGRESSED | REGRESSED (smaller) | class A (27), C (23), D (20), E2 and most of E1 (28) retired; per-process `cargo nextest` is now the only trusted runner (the storage-root guard is nextest-safe again after a one-commit regression) |
| 21 docs are the truth | PARTIAL | PARTIAL (better) | README/AGENTS counts and flags corrected to the binary; `doc_consistency` pins doctor verbs, robot subcommands, themes; VISION carries dated reality notes |
| new finding | — | — | `file_reservation_paths` timed out twice at 30 s on the live daemon in the unattributed blocking-dispatch stage (`br-9bwnb`): direct evidence for the durability gap |

Still unmoved: goals 5 (swarm durability), 7 (engine strategy), 13 (search in shipped binaries), 14 (ATC default), 17 (delivery venues), 19, 20, 22, 23.

---

## 1. Critical gaps (the vision is undeliverable without these)

### Gap 1: Zero errors or corruption under swarm load (goal 5) — REGRESSED → WORKING

**Current state.** The README's "Rust vs Python gauntlet" section states the Python failure modes are gone. Field reports GH#257 (re-corruption 61 min after a clean integrity check at 316 msg/h), GH#278 (macOS snapshot conflicts then malformed pages) and `br-htobc` (index corruption under SIGKILL) say the failure modes moved from git index locks into the storage engine. The only test that could prove the claim, the 100-agent lifecycle test in `crates/mcp-agent-mail-storage/tests/stress_pipeline.rs`, is `#[ignore]`d for an engine correctness bug ("no such table: messages" across concurrent opens); `stress_150_agent_message_storm` (line 1935) is ignored for concurrent-open serialization (p99 ~50 s vs a 45 s guard, upstream `bd-xva84`). The pool p99 on the reference host reached 18.2 s acquire and 20 s write inside a 10-minute window, and on 2026-09-02 two `file_reservation_paths` calls timed out at 30 s in `blocking_dispatch_unattributed` while `release_file_reservations` returned instantly (`br-9bwnb`). `BEGIN CONCURRENT` exists but `Config::fsqlite_concurrent_mode` (config.rs:373) defaults to `false` because of an upstream snapshot-drift bug; 85 call sites use `BEGIN IMMEDIATE`. The pragma conformance harness (`docs/FRANKENSQLITE_PRAGMA_GAPS.md`) records 35 divergences.

**Target state.** A cross-process swarm harness that runs at or above the GH#257 rate (≥ 300 msg/h across ≥ 13 projects and ≥ 90 agents, with reservation churn and restarts) for 24 h on two hosts, ends with `PRAGMA integrity_check = ok`, `quick_check = ok`, zero foreign-key violations, archive/DB parity, and no `pending_sends` residue, and does so on the pinned engine line. The README claim is either re-earned by that run or rewritten to what the run proves.

**Success criteria.**
- [ ] `am e2e run --project . --tag swarm` (new tag) drives ≥ 90 agent processes (not threads) through send/reply/ack/reserve/release/handoff for a configurable duration and emits a JSON scorecard with message rate, p50/p99 tool latency, integrity verdicts before/after a SIGKILL restart, and reservation grant latency.
- [ ] The scorecard shows 0 corruption events, 0 unattributed timeouts, reservation grant p99 < 2 s, at 300 msg/h for 24 h on ts1 and on one other host.
- [ ] `stress_150_agent_message_storm` and the 100-agent lifecycle test are un-ignored or replaced by the harness, with their upstream blockers cited on the replacement.
- [ ] `br-9bwnb` closed with the attributed stage and a regression test that drives `file_reservation_paths` under a held archive commit.

**Implementation plan.**
1. Attribute the reservation timeout first: instrument the blocking-dispatch lane (`crates/mcp-agent-mail-server` dispatch metrics, the `contended_path` / stage fields already exposed in timeout replies) so every timeout names its stage; reproduce with the live mailbox using the `am` binary against a copy; fix the grant path (conflict scan of `file_reservations/` artifacts, JSON artifact write, DB insert) so it is bounded.
2. Build the harness as a native e2e suite (`am e2e` already discovers 153 suites at runtime; add `swarm_soak` with `--agents`, `--projects`, `--rate`, `--duration`, `--restart-every`): spawn real `am` processes over stdio, drive the MCP tools, sample `health_check` and `resource://tooling/metrics`, run the doctor's double probe at the end.
3. Run it on both engine lines the repo can pin (`fsqlite =0.3.14` now; the next upstream release when available) and record the results under `tests/artifacts/swarm_soak/<ts>/`.
4. Feed every corruption reproduction into the frankensqlite repo with a minimal SQL script; track the upstream issue ids on the bead (the maintainer already routes GH#257/GH#278 upstream).
5. Only after the run is green: either flip `fsqlite_concurrent_mode` on by default (with the 85 `BEGIN IMMEDIATE` sites reviewed) or document why it stays opt-in.
6. Rewrite README "Rust vs Python gauntlet" from the scorecard, with the date and hardware.

**Dependencies.** Gap 2 (engine strategy decision) determines which engine line the harness certifies; Gap 4 (verification) provides the runner and scorecard conventions. Upstream engine fixes gate the final green.
**Complexity.** XL.
**Vision goals served.** 5, 16, 22.
**Would existing beads close it?** No. `br-sa58k` (message-id election across processes), `br-pyalb` (GH#245 pool-timeout diagnostics), `br-22gm3` (Cx budgets into the DB layer), `br-htobc` and `br-9bwnb` cover pieces; no bead builds the harness or re-earns the README claim. New epic needed.

### Gap 2: Engine strategy (goal 7) — PARTIAL → DECIDED and enforced

**Current state.** VISION.md promises "FrankenSQLite only, no C SQLite". The runtime path is FrankenSQLite (`DbConn`, test-enforced by `normal_mailbox_connection_aliases_use_frankensqlite_runtime`), but `sqlmodel-sqlite` statically bundles C SQLite into every binary as a non-optional dependency of `mcp-agent-mail-db` and `-cli`, and the canonical engine is the verification oracle and recovery reader (`CanonicalDbConn`: doctor double probe, `neutralize_private_salvage_artifact`, backups, legacy import). The dual-engine seam is itself a defect source: mixed-engine fd-close lock destruction (`br-r6psd`), read-only canonical opens that cannot read a resting Franken WAL (`br-s9d8a`, class H), stale namespace records after promotion (fixed in `f85b22c2`), and the class-E1 refusals (mostly fixed by `pool::open_guarded_read_only_sqlite_file`). `fsqlite_raptorq_enabled` (config.rs:242) has no readers (`br-of0ra`).

**Target state.** ADR-004 records one of two strategies and the code enforces it: (a) "Franken runtime, canonical verifier": C SQLite is a declared, documented verification dependency; every cross-engine boundary goes through the dispatching opener or a private neutralized copy; VISION.md drops "no C SQLite"; or (b) "Franken only": canonical uses are replaced by Franken read-only probes plus an in-process page-level integrity check, `sqlmodel-sqlite` leaves the dependency graph, and a build gate forbids it.

**Success criteria.**
- [ ] `docs/ADR-004-engine-strategy.md` exists with the decision, the evidence from Gap 1's harness, and the list of every canonical use site with its disposition.
- [ ] Under (a): `rg 'open_guarded_read_only_(franken|canonical)_' crates` shows only the dispatcher, the two openers and deliberately engine-specific sites, each with a one-line justification comment; `docs/VISION.md` reality note replaced by the ADR link.
- [ ] Under (b): `cargo tree -p mcp-agent-mail-db | rg sqlmodel-sqlite` is empty and a `tests/docs_drift_ci.rs` case asserts it.
- [ ] Either way: the class-H tests (`br-s9d8a`, 6 doctor fixer tests) and class-B probes (`br-0dw2c`, 10 writer-lock tests) are green or rewritten to the invariant the ADR states, with the rewrite justified in the test.
- [x] Removed the unused erasure-coded recovery setting and its active vision promise (`br-of0ra`, 2026-09-20); the historical finding above describes the original audit snapshot.

**Implementation plan.**
1. Inventory every `CanonicalDbConn` use (`rg -n 'CanonicalDbConn' crates --type rust | wc -l` ≈ dozens) into a table: purpose, whether the input can be Franken-admitted, whether a private neutralized copy is used.
2. Decide (maintainer). The evidence to weigh: the class-B finding that Franken and canonical share no fcntl exclusion, the readonly_shm limitation, and how often canonical caught real corruption that Franken's own `integrity_check` missed (search the doctor artifacts under `~/.mcp_agent_mail_git_mailbox_repo/doctor/`).
3. Under (a): finish routing the remaining E1 sites (`cli::open_live_sqlite_read_only` / `open_sqlite_with_fallback` and their 13 callers; `tools::identity::open_health_check_sync_db_connection`; the cli index-repair classifier and physical probe); make `CanonicalDbConn` open sites accept only paths proven non-Franken-admitted (`pool::is_franken_admitted_family`) or private copies.
4. Under (b): implement a Franken-native full check (`PRAGMA integrity_check` through `DbConn` exists; add page-level header validation for the cases canonical caught), then delete canonical sites crate by crate, `sqlmodel-sqlite` last.
5. Completed the removal option for the unused recovery setting (`br-of0ra`, 2026-09-20).

**Dependencies.** Gap 1's harness supplies the evidence; Gap 4 supplies the class B/H test beads.
**Complexity.** L for (a), XL for (b).
**Vision goals served.** 7, 5, 16.
**Would existing beads close it?** Partially. `br-0dw2c`, `br-s9d8a`, `br-00gl8`, `br-of0ra`, `br-vhxdc` follow-ups, `br-yrjwh` (registry adoption), `br-yzk37` (inert pragmas) cover pieces; no bead holds the decision. New epic + ADR bead needed.

### Gap 3: Self-healing recovery that never wedges (goal 6) — PARTIAL → WORKING

**Current state.** Reconstruct promoted a real 2.7 GB mailbox in v0.3.32 and the 2026-09-01 wave fixed the breaker self-refusal inside a process's own recovery admission (`RecoveryAdmissionDepthGuard::active_for`), the false P0, the owner classifier, and the restored/reconstructed-family refusals. Still open: the breaker chain described by `br-plksu` (startup fail-open does not recognise the live-salvage refusal shape); a 1-message archive-ahead delta triggers a full rebuild (GH#284, `br-lwx55`); startup exits 1 into a systemd restart loop when an archive-ahead reconstruct is refused despite a healthy live DB (`br-bgwj1`); reservation parity reports permanent `missing_archive` drift after a reconstruct lineage (GH#244, `br-mc0hz`); the reference host still runs with `INTEGRITY_CHECK_ON_STARTUP=false`, 5.7 GB of recovery debris and 57 `pending_sends` artifacts; the pre-init reconcile can rebuild through a symlinked storage root because the server canonicalizes `config.storage_root` before pool init (the db-side `archive_has_real_projects` guard is not reached; strace-verified 2026-09-02); three artifact-hygiene tests stay red (`restore_from_backup_leaves_primary_untouched_when_staged_backup_is_invalid`, `sqlite_family_cleanup_refuses_before_mutation_while_writer_is_active`, `archive_recovery_noop_on_healthy_db`).

**Target state.** One classifier produces the mailbox verdict that doctor, robot, MCP health and startup all report; startup never crash-loops on a healthy file; small archive-ahead deltas apply incrementally; recovery leaves no artifact behind that a later probe misreads; the reference host runs with the startup integrity check on.

**Success criteria.**
- [ ] `am doctor health --json`, `am robot health --format json`, MCP `health_check` and the startup probe return the same `mailbox_verdict` on the same file in a table-driven test that covers: healthy Franken family, healthy sidecar-less family, resting WAL with live owner, restored `.bak`, reconstructed primary, corrupt header, half namespace pair.
- [ ] `br-lwx55`: an archive ahead by ≤ N messages is applied by inserting the missing rows (with the message-id floor) instead of rebuilding; a test asserts the primary inode is unchanged.
- [ ] `br-bgwj1` and `br-plksu`: startup with a healthy live DB and a refused archive-ahead reconstruct serves and logs, exit code 0; the breaker is armed only after an attempt fails, never provisionally.
- [ ] The symlinked-root test (`probe_integrity_does_not_recover_from_archive_through_symlinked_storage_root`) passes because the server refuses a symlinked configured root before canonicalizing.
- [ ] The three artifact-hygiene tests pass; `am doctor health` on the reference host is green with `INTEGRITY_CHECK_ON_STARTUP` unset.

**Implementation plan.**
1. Extract the verdict logic into `mcp_agent_mail_db::mailbox_verdict::compute_mailbox_verdict` as the single entry point (it exists; make the cli doctor, `robot health`, the server health route and `startup_checks::probe_integrity` call it instead of their own probes) and add the table-driven test.
2. `br-plksu` / `br-bgwj1`: in `pool.rs` recovery admission, record the breaker failure after the attempt (the provisional `record_failure` before the attempt is the root cause found on 2026-09-01), and teach `startup_checks::probe_integrity` the live-salvage refusal shape (`SqlError` messages beginning "reconstruct live salvage") as "serve, do not reinitialize".
3. `br-lwx55`: in `reconcile_archive_state_before_init`, when `archive_max_id - db_max_id ≤ N` and the project identities match, call a new `reconstruct::apply_archive_delta` that parses only the missing message files and inserts them under the promotion barrier.
4. Symlinked root: in `startup_checks.rs` (before `capture_pre_recovery_snapshot`), refuse `config.storage_root` that is a symlink or has a symlinked parent (`mcp_agent_mail_core::pane_identity::path_has_symlinked_parent` already exists) with a `ProbeFailure`, and stop canonicalizing it for pool init.
5. Artifact hygiene: `restore_from_backup` must remove its `.restoring-*` staging on rejection; `sqlite_family_cleanup` must not leave `.am-recovery-breaker.lock`; `archive_recovery_noop_on_healthy_db` needs the canonical immutable reader to see Franken WAL frames (checkpoint the private copy first) — each is a small fix in `pool.rs` with the existing red test as its acceptance.
6. Reference host: run `am doctor repair --yes` for the 5.7 GB debris and the 57 `pending_sends`, then re-enable the startup check and watch for 48 h.

**Dependencies.** None hard; Gap 2 decides how the canonical corroboration in the verdict is retained.
**Complexity.** L.
**Vision goals served.** 6, 11, 16.
**Would existing beads close it?** Partially: `br-plksu`, `br-lwx55`, `br-bgwj1`, `br-mc0hz`, `br-jcgxg`, `br-zchj0`, `br-oyget`, `br-3p187`, `br-sz6k9`, `br-cxsgx`. No bead for the unified verdict, the symlinked-root refusal, or the artifact-hygiene trio.

### Gap 4: Every test passes, zero ignored, zero flaky, one trusted gate (goal 16) — REGRESSED → WORKING

**Current state.** `br-l1q6z` recorded ~212 deterministic reds on 2026-09-01; the evening wave retired classes A, C, D, E2 and most of E1, but the figure has not been re-measured with a full-workspace `cargo nextest run`. Remaining classes with baselines measured in a detached worktree: B (10 cross-engine writer-lock probes, `br-0dw2c`), F (about 12 relative-`sqlite:///` authority tests across db/server/cli, `br-z73au`), H (6 readonly_shm tests, `br-s9d8a`), source-bytes (`br-00gl8` + `strict_query_only_pool_rejects_writes_without_changing_file_family`), symlink-rejection probes in the server (3), dashboard/TUI placeholder tests (3), cli doctor salvage fixture tests (6), setup self-heal OMP tests (3), and singletons (`insert_system_agent_reselects_existing_name_case_insensitively`, `commit_tx_does_not_wait_for_external_reader_checkpoint`). 39 `#[ignore]` remain (3 heavy stress, 3 engine-blocked, the rest unclassified). `run_http_startup_preflight_probes_omits_port_check` is flaky under load. Plain `cargo test` never trips the harness guard on ts1; only nextest does. GitHub `ci.yml` has never passed (0 of 1,785). The release scorecard the checklist requires (`tests/artifacts/release_scorecard/<ts>/release_scorecard.json`) has never been produced; installed-binary parity looks for `/usr/local/bin/am` and has never passed here.

**Target state.** A single documented gate, `cargo nextest run --workspace` (per-process) plus `am e2e run --project . --tag reliability --release-scorecard`, is green on the reference host before every tag, its artifacts are committed, and CI either runs that gate or is retired.

**Success criteria.**
- [ ] A full-workspace nextest run on ts1 reports 0 failures; the run log is attached to `br-l1q6z` before it closes.
- [ ] Every `#[ignore]` carries a reason string naming an upstream issue or a `--ignored` manual-run rationale; the count is pinned by a test so new ignores need a reason.
- [ ] `release_scorecard.json` exists for the next tag and `docs/RELEASE_CHECKLIST.md` row 31 points at it; installed-binary parity resolves the `am` on `PATH` and passes.
- [ ] `docs/DEVELOPER_GUIDE.md` states the gate in one paragraph; `cargo test` is documented as not sufficient for guard-sensitive code.

**Implementation plan.**
1. Re-measure: `cargo nextest run --workspace --no-fail-fast` on ts1 (private target dir), attach the summary to `br-l1q6z`, and update the class counts on the bead.
2. Class F (`br-z73au`): decide the `sqlite:///rel` contract in `mcp_agent_mail_core::disk::sqlite_file_path_from_database_url` (preserve a missing relative authority end to end); fix `resolve_mailbox_sqlite_path`, readiness, tool_metrics and search_v3 together; the ~12 tests are the acceptance.
3. Class B (`br-0dw2c`): per ADR-004, either take classic fcntl locks in the Franken write path or rewrite the 10 probes to the stated invariant.
4. Class H (`br-s9d8a`): stage the family and checkpoint the private copy before any canonical read (the pattern `neutralize_private_salvage_artifact` uses), applied in the doctor fixers' offline candidate path.
5. Server symlink probes (3) and the dashboard/TUI placeholder tests (3): investigate individually; each is a small contract fix or fixture fix with the red test as acceptance.
6. cli salvage fixtures (6): make `sqlite_backup_candidates` accept a Franken-written `.bak` by neutralizing a private copy, or fix the fixtures to write standalone backups; decide the `doctor_reconstruct_prefers_readable_current_db_as_salvage_source` wording contract.
7. Flakiness: run the suite 3× and quarantine-by-reason (not `#[ignore]`) anything that flips; fix `run_http_startup_preflight_probes_omits_port_check`.
8. Scorecard: run `am e2e run --project . --tag reliability --release-scorecard` on ts1, fix the installed-binary parity path resolution (`which am`), commit the artifacts, and add the two commands to the release checklist as blocking.

**Dependencies.** Gap 2 for classes B and H. Gap 8 (CI venue) for where the gate runs.
**Complexity.** L.
**Vision goals served.** 16, 17, 22.
**Would existing beads close it?** Partially: `br-l1q6z` (tracker), `br-z73au`, `br-0dw2c`, `br-s9d8a`, `br-00gl8`, `br-qk7wu` (closed by evidence), `br-99aih` (pollution guard landed, closure pending a full run), `br-jpowg`, `br-y1elw`, `br-nq2kb`. No bead for the ignore-reason policy, the flake quarantine, or the scorecard run.

---

## 2. Major gaps (the vision is significantly degraded)

### Gap 5: Delivery venues (goal 17) — PARTIAL → WORKING

**Current state.** Signed v0.3.32 shipped manually via dsr and verified; the installer verifies minisign manifests end to end (goal 18 WORKING). `ci.yml` 0 of 1,785 successes, silent since 2026-08-19 (queue latency then clippy failure); `dist.yml` 0 successes; `docker.yml` never green and ghcr's newest tag is v0.3.13 while the v0.3.31 changelog claimed the image was unstuck (CHANGELOG now carries a Known-issues note); `deploy-pages.yml` is now dispatch-only with a `bundle_dir` input but the Pages site still 404s; `publish.yml` is manual-only and `mcp-agent-mail` is not on crates.io; release binaries are built `--features portable` (lexical-only search).

**Target state.** Every workflow under `.github/workflows` either passes on its trigger or is deleted; the newest ghcr tag equals the newest release; the docs site deploys; crates.io publication is decided; the release checklist is executable from a clean checkout.

**Success criteria.**
- [ ] `gh run list --workflow <each>.yml --limit 5` shows green for every remaining workflow.
- [ ] `docker pull ghcr.io/dicklesworthstone/mcp_agent_mail_rust:v<latest>` runs `am --version` matching the tag (`br-ocys6`).
- [ ] The Pages URL serves the bundle from `deploy-pages.yml`'s `bundle_dir`.
- [ ] `br-95spu` closed with a decision; if publishing, `cargo publish --dry-run` passes for every member crate in dependency order.

**Implementation plan.**
1. `br-bx73n`: decide the CI venue. Recommended: shrink `ci.yml` to fmt + clippy + `cargo nextest run -p mcp-agent-mail-core -p mcp-agent-mail-conformance` (fast subset) on the hosted runner, and move the full gate to a self-hosted rch-backed runner or document the local gate as the pre-tag requirement; delete `dist.yml` if dsr remains the release tool.
2. `br-ocys6`: add an image lane to the dsr flow (build `Dockerfile.release` from the verified release archives, push `v<tag>` and `latest`), or retract the claim permanently.
3. Pages: build `docs/site` with the existing tooling, dispatch `deploy-pages.yml` with `bundle_dir=docs/site`, verify the URL, then trigger it from the release flow.
4. `br-95spu`: decide; the blocker (frankensearch on crates.io) is gone, but the path deps on `../frankensearch-rel-0332` (`br-ku0kl`) must become registry deps first.
5. Search in binaries: ship one artifact with `--features hybrid` or state lexical-only in every doc that mentions search (README FAQ already does; AGENTS.md search caveat added); see Gap 9.

**Dependencies.** Gap 4 (the gate CI would run), Gap 9 (search feature decision), `br-ku0kl` (registry deps) for crates.io.
**Complexity.** M.
**Vision goals served.** 17, 13.
**Would existing beads close it?** Partially: `br-bx73n`, `br-ocys6`, `br-95spu`, `br-ku0kl`, `br-c2is6`, `br-gozln`, `br-nq2kb`, the P0 release-binding beads from Aug 25. No bead for Pages.

### Gap 6: Robot and operator surfaces correct at scale (goals 10, 11) — PARTIAL → WORKING

**Current state.** `am robot handoff` is fixed (0.30 s, `--max-seconds`); `am robot metrics` reads the live server. Still: `am robot overview` is slow at scale (GH#274); `/health` takes ~7 s on large mailboxes so doctor reports a healthy server as failing (`br-am-health-endpoint-slow-false-fail-45e0e`); `md` output exists only for `thread` and `message`; `doctor drain` and the ownership classifier now treat read-only readers correctly but `doctor locks` still reports the parallel-session `instest/am` daemon as the live owner without saying it is a test build; the doctor's diagnostic-source choice was only fixed on 2026-09-02 and has no table-driven test.

**Target state.** Every robot subcommand returns within 5 s on the 40k-message reference mailbox; no robot command returns constant zeros; `/health` is constant-time; the doctor's verdict on any family shape is table-tested.

**Success criteria.**
- [ ] A benchmark test (`crates/mcp-agent-mail-cli/tests/robot_scale.rs`, new) seeds 16 projects / 700 agents / 40k messages and asserts every `am robot <sub> --format json` finishes under 5 s.
- [ ] `/health` p99 < 250 ms on the same fixture (`br-am-health-endpoint-slow-false-fail-45e0e`).
- [ ] `md` renderers exist for every robot subcommand or the README states which support it.
- [ ] The doctor source-selection table test from Gap 3 covers `LiveLogicalSnapshot`, `StagedFamilyCopy`, `OfflineCanonical` and `PrivateCanonical` outcomes.

**Implementation plan.**
1. GH#274: apply the handoff pattern (batched join-free `IN (...)` lookups, wall-clock budget, `truncated_by_budget`) to `robot overview`; profile with the reference mailbox copy.
2. `/health`: cache the expensive counts behind a 30 s snapshot (the TUI poller throttle pattern, see `br-y8k4z`) and answer from the cached verdict; expose staleness in the payload.
3. Add the scale benchmark test with a generated fixture (reuse `open_robot_test_db_with_real_schema` and a bulk inserter).
4. `md` renderers: extend `robot.rs` output dispatch for the remaining subcommands.

**Dependencies.** Gap 3 for the shared verdict.
**Complexity.** M.
**Vision goals served.** 10, 11.
**Would existing beads close it?** Partially: `br-am-health-endpoint-slow-false-fail-45e0e`, `br-4myjj` (landed, verification pending), GH#274 has no bead. New beads for overview, md renderers, scale test.

### Gap 7: Search V3 hybrid in shipped artifacts (goal 13) — PARTIAL → WORKING

**Current state.** Hybrid (lexical + semantic) search exists in source builds behind the `hybrid` feature; every shipped artifact (dist.yml, both Dockerfiles) builds `--features portable`, so releases are lexical-only. Global search falls back to `LIKE` full scans after the FTS5 decommission (`br-7x5fm`). The workspace builds frankensearch from a gated clone `../frankensearch-rel-0332` because the live sibling moved to asupersync 0.4.10 while fastmcp pins =0.4.9 (`br-ku0kl`). Tantivy is not optional in db no-feature builds (`br-eh8bj`).

**Target state.** Either one shipped artifact carries hybrid search with a documented model download path, or every doc that mentions search says lexical-only for binaries; global search goes through Search V3 in both cases.

**Success criteria.**
- [ ] `br-7x5fm`: `am search` and the `search_messages` tool never issue `LIKE '%…%'` full scans on the reference mailbox (assert via `EXPLAIN QUERY PLAN` in a test).
- [ ] Decision recorded: hybrid artifact (with `am search --semantic` proven in an e2e suite on a release binary) or lexical-only statement in README, AGENTS.md and `am --help`.
- [ ] `br-ku0kl` closed: path deps replaced by registry versions once fastmcp follows asupersync, with `dist.yml`/Dockerfile/install.sh updated in the same change.

**Implementation plan.**
1. Route global search through the Search V3 planner (`search_planner`, `search_service`) and remove the LIKE fallback path.
2. Decide the artifact feature set with the maintainer; if hybrid, add a `hybrid` build to the dsr release flow and an e2e suite that runs semantic search on the produced binary with the bundled model.
3. Watch fastmcp for an asupersync ≥ 0.4.10 release; then bump asupersync in lockstep, switch frankensearch to registry deps, and retire the clone.

**Dependencies.** Gap 5 (release flow) for the artifact; upstream fastmcp for the registry move.
**Complexity.** M.
**Vision goals served.** 13, 17.
**Would existing beads close it?** Partially: `br-7x5fm`, `br-ku0kl`, `br-eh8bj`. No bead for the artifact decision.

### Gap 8: ATC learning loop live and safe by default (goal 14) — PARTIAL → WORKING

**Current state.** `atc_note_*` hooks are wired from dispatch; the executor default flipped to Shadow on 2026-08-28. `atc_record_outcome()` has zero production callers although README named it as the outcome path (README now says the real path is `record_atc_message_outcome_from_tool_payload_with_pool`). GH#264 stays open: one field mailbox is 99.8% ATC self-traffic (`br-au76r`); GH#258 hydration stalls MCP (`br-z6m08`); the default pairing decision is open (`br-rl1s4`). The parallel session landed the complete `AM_ATC_*` flag registry and runbooks on 2026-09-01 (`a3a52caf`).

**Target state.** A fresh default install produces zero ATC mail in a 24 h soak; hydration is bounded; probe traffic has its own retention or state; the outcome path has one name.

**Success criteria.**
- [ ] `br-rl1s4` closed with the decision and the shipped default documented in `docs/FLAGS_REGISTRY.md`.
- [ ] A soak test (reuse the Gap 1 harness at low rate) shows 0 ATC-authored messages in ordinary inboxes over 24 h with defaults.
- [ ] `br-z6m08`: cold-start hydration is bounded by effect-queue capacity with a test at 940 recent agents.
- [ ] `atc_record_outcome` is deleted or called; README, AGENTS and the code agree.

**Implementation plan.**
1. Decide the default pairing (maintainer) and encode it in `Config` defaults + the flag registry.
2. `br-au76r`: route liveness probes to a dedicated state table or a retention class that never lands in agent inboxes.
3. `br-z6m08`: cap hydration by queue capacity, spill the rest to a background pass.
4. Remove or wire `atc_record_outcome`.

**Dependencies.** Gap 1 harness for the soak.
**Complexity.** M.
**Vision goals served.** 14.
**Would existing beads close it?** Mostly: `br-rl1s4`, `br-au76r`, `br-z6m08`; missing: the soak criterion and the dead-function cleanup.

### Gap 9: Read-only MCP resources honour their contract (goal 2) — PARTIAL → WORKING

**Current state.** 25 templates served and test-pinned. `resource://tooling/recent/{window_seconds}` always returns an empty list (`resources.rs:2129`, "not yet implemented; return real data only"); 7 of 8 `?{query}` variants parse and discard their parameters; conformance fixtures cover 23 of 25 while README says all 25 (`br-ciwph`).

**Target state.** Every resource either honours its documented parameters or is delisted from the registry, the fixtures and the docs, consistently.

**Success criteria.**
- [ ] `tooling/recent` returns the last N tool calls from a bounded in-memory ring the server already feeds for the TUI timeline, filtered by agent/project, with a unit test; or it is removed from `TOOL_CLUSTER_MAP`-equivalent resource registry, the Python fixture and README in one change.
- [ ] Each `?query` variant either applies its parameters (test per variant) or returns a typed `unsupported parameter` error.
- [ ] Conformance fixtures cover 25 of 25 or README says 23.

**Implementation plan.** As in `br-ciwph`.
**Dependencies.** None.
**Complexity.** S–M.
**Vision goals served.** 2.
**Would existing beads close it?** Yes: `br-ciwph`.

### Gap 10: Legacy Python import and upgrade (goal 19) — PARTIAL → WORKING

**Current state.** `am legacy detect/import/status` and `am upgrade` exist with tests. GH#268: `serve-http` fails after a successful import on its own fsqlite namespace gate (Kubernetes PVC repro, `br-lkhxw`); several P1 beads cover clobber-free publication of import artifacts (`br-1m1tv`, `br-8echk`, `br-hb3mk`, `br-cxsgx`).

**Target state.** Import → serve works on a PVC-style filesystem in an e2e suite; failed imports are retryable.

**Success criteria.**
- [ ] An e2e suite imports a Python-era mailbox fixture and then serves over HTTP from the same directory, on ext4 and on an overlay/bind mount that mimics the PVC report.
- [ ] `br-lkhxw` closed with that suite as evidence.

**Implementation plan.** Reproduce GH#268 with the dispatcher in place (the namespace-gate refusal is the same family as class E1: after import the family may be sidecar-less or carry stale records — apply `admit_private_database_with_franken` or the promotion-style record retirement at import publication), then the P1 clobber beads.
**Dependencies.** Gap 2 (namespace semantics).
**Complexity.** M.
**Vision goals served.** 19.
**Would existing beads close it?** Mostly: `br-lkhxw` + the four P1 beads; missing: the PVC e2e suite.

### Gap 11: Performance baselines current and honoured (goal 22) — UNPROVEN → WORKING

**Current state.** README performance tables are the 2026-02 numbers; the latest artifacts (2026-09-01) show archive batch-100 p95 of 6.5 s against a 250 ms budget; `benches/BUDGETS.md` deliberately leaves budgets unrecalibrated; the p99 budget for the swarm test was recalibrated from the README's 6.8 s to 120 s; `br-pyalb` (GH#245) asks for windowed timeout diagnostics and `git_commit_p99` exposure (the timeout payload now carries `git_commit_p99_ms`, seen on 2026-09-02).

**Target state.** Budgets reflect the current engine on named hardware, the bench suite fails when a budget regresses, and README tables carry current numbers with the date and host.

**Success criteria.**
- [ ] `benches/BUDGETS.md` has a per-case budget with a recorded baseline run id; `cargo bench`/the bench e2e suite exits non-zero on a > 20 % regression.
- [ ] README performance tables regenerated from `tests/artifacts/bench/<ts>/` with an era note.
- [ ] Gap 1's scorecard numbers (reservation grant p99, tool p99) become budgets.

**Implementation plan.** Run the bench catalog on ts1 after Gap 1's fixes; write budgets from the measured p95 × 1.5; wire the check into the release checklist; regenerate the README tables by script (a doc generator, not a hand edit).
**Dependencies.** Gap 1.
**Complexity.** S–M.
**Vision goals served.** 22.
**Would existing beads close it?** No bead; `br-pyalb` is adjacent.

### Gap 12: Deterministic fixtures and the parallel-session workflow (goals 16, 23) — REGRESSED → WORKING

**Current state.** On 2026-09-01 a second Claude session working in the same checkout committed this session's in-progress working tree under its own message (`888aea20`), left the cli lib tests uncompilable (E0027) and two clippy doc lints; 66 in-progress beads have been untouched since Aug 25–28; `bv --robot-triage` returns null on this host (`br-cdwya` adjacent).

**Target state.** Concurrent sessions cannot publish each other's edits, the tracker reflects live work, and `bv` triage works.

**Success criteria.**
- [ ] AGENTS.md "Typical Agent Flow" gains: reserve files via Agent Mail before editing, commit only your own paths (`git add <paths>`, never `-A` in a shared checkout), run `git status -sb` and `git log -3` before committing.
- [ ] Every in-progress bead older than 7 days without a comment is released to open or tombstoned; `bv --robot-triage | jq .quick_ref` returns data.
- [ ] Each epic from this plan exists as a bead with children; `br ready` surfaces plan work first.

**Implementation plan.** Docs edit; a one-time tracker sweep by the maintainer (claims belong to other agents); fix `bv` on this host (`br-cdwya`).
**Dependencies.** None.
**Complexity.** S.
**Vision goals served.** 23.
**Would existing beads close it?** No.

### Gap 13: The live daemon's reservation path (goal 5, operator-visible) — NEW → WORKING

Listed separately from Gap 1 because it is reproducible today without a harness: `file_reservation_paths` (5 paths, ttl 10800) timed out twice at 30 s on 2026-09-02 with `stage=blocking_dispatch_unattributed`, `blocking_dispatch_inflight=1`, pool/db/archive p99s in single-digit ms, `blocking_dispatch_timeouts_total` 12 → 13, while `release_file_reservations` returned in milliseconds (`br-9bwnb`).

**Target state.** Reservation grants are bounded and attributed.
**Success criteria.** As on `br-9bwnb`.
**Implementation plan.** Instrument the blocking-dispatch lane with the stage name and elapsed per step (conflict scan over `file_reservations/*.json`, artifact write, DB insert, coalescer enqueue); reproduce on a copy of the reference mailbox (1k+ reservation artifacts); bound the conflict scan with an index (reservation patterns per project are already in the DB) instead of a directory walk.
**Dependencies.** None.
**Complexity.** M.
**Vision goals served.** 5, 10.
**Would existing beads close it?** Yes: `br-9bwnb` (new).

---

## 3. Minor gaps (polish, completeness, hygiene)

### Gap 14: Docs are the truth for the shipped surface (goal 21) — PARTIAL → WORKING

**Current state.** README/AGENTS counts, flags, dependency table and the `DATABASE_URL` default were corrected on 2026-09-01 and `doc_consistency` now pins doctor verbs, robot subcommands and themes. Still outside the guard: the bench catalog names, the resource-coverage sentence, the ATC outcome-path name, the README perf era note, and three root-level planning docs that diverge from their `docs/planning` copies (`AGENT_MAIL_RUST_VERSION_REPO_TRANSITION_PLAN.md`, `PROPOSED_ARCHITECTURE.md`, `SYNC_STRATEGY.md`, plus `FEATURE_PARITY.md`, `PLAN_TO_PORT…`, `TODO*.md`, `UPGRADE_LOG.md`, `beads_*.md` duplicated at the root).

**Target state.** Every count or name the docs state is either generated from a registry or pinned by `doc_consistency`; planning docs live once, under `docs/planning`.

**Success criteria.**
- [ ] `doc_consistency` pins: bench case names (from the bench catalog), resource fixture coverage (from the conformance fixture set), the ATC outcome function name (grep), and the perf-table era note.
- [ ] Root-level duplicates are moved (git mv, never deleted) under `docs/planning/` with the diverged copies reconciled; `br-4meup` closed.

**Implementation plan.** Extend `crates/mcp-agent-mail-conformance/tests/doc_consistency.rs` (the `LiveCounts` pattern) with the four probes; `git mv` the duplicates after a diff review.
**Dependencies.** None.
**Complexity.** S.
**Vision goals served.** 21.
**Would existing beads close it?** Partially: `br-4meup`.

### Gap 15: Workspace lints everywhere — NEW → WORKING

**Current state.** Five crates (cli, share, storage, guard, conformance; about 250k lines) do not inherit `[workspace.lints]` (`br-sox5q`); clippy with `-D warnings` is only clean for them because their own lint sets are weaker.
**Target state.** All member crates inherit `[workspace.lints]`; the pedantic/nursery baseline is met or allowed per-lint with a comment.
**Success criteria.** `cargo clippy --workspace --all-targets -- -D warnings` clean with the five manifests carrying `[lints] workspace = true`.
**Implementation plan.** Per crate: add the inheritance, fix or allow lint by lint, one commit per crate.
**Complexity.** M (mechanical, large).
**Vision goals served.** 16.
**Would existing beads close it?** Yes: `br-sox5q`.

### Gap 16: Canonical Python repo cut over to Rust-first (goal 20) — NOT STARTED → DECIDED

**Current state.** `docs/planning/AGENT_MAIL_RUST_VERSION_REPO_TRANSITION_PLAN.md` says Active; the Rust-side pieces exist; the Python repo README still describes the FastMCP server; no PR (`br-ajiq8`).
**Target state.** Phase 1 of the transition plan executed (canary cutover PR from `docs/OLD_REPO_RUST_CUTOVER_PR_BODY.md`) or the plan marked superseded in the README FAQ.
**Success criteria.** `br-ajiq8` closed with a PR link or a dated "superseded" status in the plan and the FAQ.
**Complexity.** S (decision) + M (execution).
**Vision goals served.** 20.
**Would existing beads close it?** Yes: `br-ajiq8`.

### Gap 17: Dead flags and documented-but-dead functions — NEW → WORKING

The unused recovery setting was removed with its active documentation (`br-of0ra`, 2026-09-20; see Gap 2). `atc_record_outcome` (Gap 8) remains a separate task: wire it with a test or remove it with its docs.
**Complexity.** S. **Beads:** `br-of0ra`; ATC cleanup needs a child bead under Gap 8.

### Gap 18: TUI and web polish (goals 8, 9) — WORKING → WORKING (guard)

Both goals work. Keep them working: the TUI snapshot suite is red against the frankentui main tip (`br-boq46`), the poller's full-snapshot sweep bypasses its throttle (`br-y8k4z`), and the read-only attach banner should print the reclaim command (`br-mljnz`). README should list the 42 themes by name from `NAMED_THEMES` (generated) rather than a hand-written five.
**Complexity.** S. **Beads:** yes (`br-boq46`, `br-y8k4z`, `br-mljnz`).

### Gap 19: Reference-host operational debt — NEW → CLEAN

The maintainer's own machine is not a clean oracle: `INTEGRITY_CHECK_ON_STARTUP=false`, 5.7 GB recovery debris (4.9 GB reclaimable staging), 57 `pending_sends` artifacts, 45 zero-byte `-fsqlite-ns-gate.corrupt*` sidecars, and a daemon served by another session's `instest/am` test build (observed 2026-09-02 via `am doctor locks`).
**Success criteria.** `am doctor health` green on the reference host with defaults; the daemon is the installed release binary; the debris is quarantined or reclaimed through `am doctor repair`/`vacuum`, never by hand.
**Complexity.** S (after Gap 3).

---

## 4. Dependency graph

```mermaid
flowchart TD
  G13[Gap 13 reservation timeout br-9bwnb] --> G1
  G2[Gap 2 ADR-004 engine strategy] --> G1[Gap 1 swarm durability harness]
  G2 --> G4[Gap 4 one trusted gate, 0 reds]
  G3[Gap 3 recovery never wedges] --> G1
  G3 --> G6[Gap 6 robot/doctor at scale]
  G4 --> G5[Gap 5 delivery venues]
  G1 --> G11[Gap 11 perf budgets]
  G1 --> G8[Gap 8 ATC default soak]
  G2 --> G10[Gap 10 legacy import on PVC]
  G7[Gap 7 search in binaries] --> G5
  G12[Gap 12 tracker + shared-checkout workflow] --> G4
  G14[Gap 14 docs pinned] -.-> G5
  G15[Gap 15 workspace lints] -.-> G4
  G16[Gap 16 Python cutover decision]
  G9[Gap 9 resources contract]
  G17[Gap 17 dead flags]
  G18[Gap 18 TUI/web guard]
  G19[Gap 19 reference host clean] --> G1
```

Recommended order of epics: 12 (one day, unblocks honest tracking) → 13 → 3 → 2 (decision) → 4 → 1 → 5 → 6 → 7 → 8 → 10 → 11 → 9, 14, 15, 17, 18, 19 in parallel as fillers → 16 (maintainer).

---

## 5. Verification plan (one line per vision goal)

| # | Goal | How it is proven when the plan is done |
|---|---|---|
| 1 | 45 real tools | `am doctor mcp-selftest --format json` = 45, plus `TOOL_CLUSTER_MAP` test (already green) |
| 2 | 25 resources honour contract | Gap 9 unit tests per variant; fixture coverage 25/25 |
| 3 | No broadcast | existing `BROADCAST_DISABLED` tests (green) |
| 4 | Archive + index with coalescer | `health_check` WBQ/coalescer counters after the Gap 1 soak |
| 5 | Swarm durability | Gap 1 scorecard: 24 h, 2 hosts, 0 corruption, reservation p99 < 2 s |
| 6 | Self-healing recovery | Gap 3 verdict table test + reference host green with defaults |
| 7 | Engine strategy | ADR-004 + its build gate or dispatcher inventory test |
| 8 | 16-screen TUI | snapshot suite green against the pinned ftui (`br-boq46`) |
| 9 | Web UI | route tests (green) |
| 10 | Robot mode | Gap 6 scale test: every subcommand < 5 s on the 40k fixture |
| 11 | Doctor | Gap 3/6 table tests; `doctor health` = `robot health` = MCP health |
| 12 | Bare `am` works | `am setup status` drift 0 (green) |
| 13 | Search V3 | Gap 7 EXPLAIN test + artifact decision |
| 14 | ATC safe by default | Gap 8 soak: 0 ATC mail in 24 h |
| 15 | Description parity | `tool_description_parity` green (already real) |
| 16 | All tests green | full-workspace nextest 0 failures, ignore reasons pinned |
| 17 | Delivery | every workflow green or deleted; ghcr tag = release; scorecard committed |
| 18 | Installer verification | `install.sh` e2e (green) |
| 19 | Legacy import | Gap 10 PVC e2e |
| 20 | Python repo cutover | `br-ajiq8` decision |
| 21 | Docs are truth | `doc_consistency` extended (Gap 14) |
| 22 | Perf baselines | Gap 11 budgets with baseline ids |
| 23 | Beads are the truth | Gap 12: every epic in `br`, no stale claims |

---

## 6. Decisions only the maintainer can make

1. ADR-004 engine strategy: dual-engine with a declared verifier, or Franken-only (Gap 2).
2. CI venue: repair `ci.yml` to a runnable subset, or retire GitHub Actions in favour of the local gate (Gap 5, `br-bx73n`).
3. Container image: publish through dsr or retract the claim (`br-ocys6`).
4. crates.io publication (`br-95spu`) and the registry move for frankensearch (`br-ku0kl`).
5. Shipped search features: hybrid artifact or lexical-only statement (Gap 7).
6. ATC default pairing (`br-rl1s4`).
7. Python repo cutover (`br-ajiq8`).
8. Release of the 66 stale in-progress bead claims (Gap 12).

## 7. Explicitly out of scope for this plan

- The WASM dashboard epic (the only open epic today) — not a vision goal in README/AGENTS/VISION.
- Windows-specific recovery parity (`br-y1elw`) beyond keeping its tests honest.
- Rewriting the storage engine; upstream FrankenSQLite defects are reported, not fixed here.

## 8. Provenance

Phase 1 sources: full reads of AGENTS.md, README.md, VISION.md, every root and `docs/planning` plan, `docs/VERIFICATION_COVERAGE_LEDGER.md`, `RELIABILITY_COVERAGE_MATRIX.md`, `DOCTOR_FM_DISPOSITION.md`, `FRANKENSQLITE_PRAGMA_GAPS.md`, `CONFORMANCE_AUDIT_2026-04-18.md`, `RELEASE_TRAIN_PLAN.md`, the April reality check; five parallel code audits; the 2026-09-01 nextest logs; live probes on ts1. Phase 2 sources: the 2026-09-01/02 landing lanes (baselines in a detached worktree at `ba1b8a42`, nextest summaries on `f85b22c2`), `br list` on 2026-09-02 (108 open, 66 in-progress, 2248 closed, 208 tombstoned), and the strace trace of the symlinked-root recovery.

</details>

</details>
