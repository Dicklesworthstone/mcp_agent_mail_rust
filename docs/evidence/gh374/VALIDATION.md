# Agent Mail #374: recovery and inbox qualification

Date: November 4, 2026. The initial investigation is based on
`4b8ddf30f34a73cc2553bc95db8f906dd95328a6`. Initial preparation made no commit,
issue comment, pull request, release, or remote branch update. In a subsequent
instruction, the user authorized the two commits below directly on main through
GitHub, without a pull request. Publication receipts establish whether that
authorized update completed; publication does not establish native qualification.

The initial changes were published directly to main as
`a79b72f5676cab8b5ea2f624b84e4f99632801da` and
`aaf8c0e47f70fab1d9e44327909b18df621a964c`. The continuation below starts from
`1d12d78c26c87afecd08617f0c52d6a44127aa71`, which incorporates subsequent
upstream changes. The user again explicitly authorized direct main commits and
prohibited pull requests.

## Status and evidence boundary

### November 6 continuation: private installer lock authority

The ready item `br-x0aie` had a real remaining availability defect at
`334400d0`: the installer flocked the public destination directory itself.
In `/data/tmp/am-installer-public-lock-jqmwiw46/result.json`, an actual UID65534
holder locks UID1000's mode0755 destination; the unchanged full installer
refuses before downloading. The intended no-foreign-interference assertion
fails. The witness inode and bytes survive. This is an observed negative,
not an inferred vulnerability or a performance comparison.

The repair retains a mode0700 authority directory inside each destination,
keyed by the destination owner's UID. The owner and root rendezvous on its
same kernel lock. Destination identity/owner and held authority
identity/owner/mode are checked separately before existing mutation paths.
Release unlocks the held descriptor; it never deletes a PID or lock path.
Explicit auxiliary aliases deduplicate against the held inode. Existing
symlink, traversal, binary ownership, signature and transaction guards remain.
Source builds now require an already installed nightly instead of executing
an unchecked Rustup download. No Rust dependency or runtime source changed.

Final `install.sh` SHA-256 is
`bd7dadaa75736bcb3eb649a82bd691176420f2f11669239a0cf89123765297fd`.
The final exit/prune test hashes are respectively
`8ab1f45f2d0e92ffbcdf2ae44d5af033c045961a4f6e60d0c844eee68eea4359`
and `25510f836284f87115fb90153cbaea7725439cccf7ca428c579547605432fc0a`.
Frozen qualifier remains `aceebacc` plus the three previously qualified Rust
overlays and these three shell overlays; its older report bytes are not
claimed to equal the moving main report. Main's reviewed code matches it.

- Fresh final-source boundary suite: 46 exit +16 lock +10 destination
  +42 real-kernel +10 native-launch cases, zero failures. The same complete
  script then passes four genuine signed file/stdin/default installations.
  Native-launch executables are explicit R2 fixtures. Held-download controls
  substitute transport only to expose the real kernel interleaving.
- Fresh artifact/rollback suite and live-executable history pruning both
  exit0. Artifact binaries/signing fixtures are R1/R2 evidence, compensated by
  the genuine public-release installations; they are not published payloads.
- Genuine final-source v0.3.40 file/stdin installs under a real foreign public
  lock both succeed at `/data/tmp/am-installer-private-lock-myws96w3/`.
  Fresh solo repeats at `/data/tmp/am-installer-private-lock-inxdvsqq/`
  also succeed (11.157s/15.315s). Both verify the pinned Minisign key, exact
  signed tag, SHA256SUMS, actual installed versions and `--verify`. UID65534
  cannot open the private authority, even after release; its inode is retained.
- Privileged fresh install at `/data/tmp/am-installer-root-fresh-vefq7224/`
  passes with the same UID1000-owned authority and auxiliary alias, destination
  still UID1000, installed pair UID0. Root contention during an owner's actual
  held lock is refused; replacing the owner's installed binary pair remains
  refused by the existing ownership guard. No guard was relaxed for root.
- Shell syntax, both trees' formatting and final three-file UBS exit0;
  UBS reports zero critical/warning/info findings and no failed module.
  All-target, separate production lib/bin and strict all-target Clippy pass
  on the same 3,917 freshly verified input records, zero mismatches. Their
  retained receipt-file hashes are respectively `94037c706555d4276d23a3f345511d677ee61f56ecf5e350b3eb15edfce3d078`,
  `7fbff4611536a966196a9669dd22945ceb872fc3fe0ccd3830bab5e90bd63604`
  and `c9637e0685613ca330e25885869f94e225b64dc6e7a04ead2db4998acca21f1d`.
  The six reviewed shell/Rust overlays match committed main `6b584bef` byte for
  byte. No worker resources, gate bounds or dependency inputs were changed.
- Final-source foreign-destination/precreated-foreign-authority controls both
  refuse before network while preserving owners and witness inode/bytes:
  `/data/tmp/am-installer-foreign-final-3qpzgivj/result.json`.

Original `br-x0aie` closes on published `6b584bef` after that fresh solo pass.
The ACFS consumer pin is published as `3358273c9f2c58e79c021aa6a1003483590f6eb3`.
Its canonical report observes all48 with45 matches/3 mismatches, no errors/skips;
SHA-256 `b28698248351f63f195142de500cc748bc66d41b516a6d2e0beb55bcc56306a7`.
The bound builder/validator updates only MCP, explicitly deferring existing
Atuin/Grok drift; validated policy SHA-256
`60eeaa840e3792e21a83cf8251e00a38957bf309c53b0131d95cc0530a7f99fc`.
Canonical generation refreshes the two provenance mirrors. Checker0.1.0
validate/list/local dry-run pass. Its stronger actual URL/hash check reaches
all48 URLs and matches46 hashes including MCP; exit4 is solely unchanged
Atuin/Grok mismatches, not an all-green consumer claim. The initial ShellCheck
attempt exits127 because it is absent locally and on the worker. An official
0.11.0 archive is then verified against GitHub's recorded SHA-256 before
extracting its binary into private scratch; all generated shell files pass.
The manifest contract passes all461 checks, including121 internal hashes and
six repository MCP configs. No global tool installation or ACFS host provisioning
installer was run. Five owned RCH entries are terminal/acknowledged with no
live wrapper;117 unrelated malformed fleet journals preclude a fleet-health claim.

Raw continuation logs are retained under
`/data/tmp/am-startup-review-20261106.mSIrexF8/installer-*` and the private
probe directories above. Preserve the unsuccessful controls: two dynamic
launch commands were rejected by DCG before execution and replaced by explicit
native arguments; inherited project CWD triggered required Codex setup; a
custom destination missing from PATH failed `--verify`; a renamed multicall
`sleep` fixture exited early, including the ineffective `exec -a` attempt;
one observer inspected stderr although the refusal printed on stdout; and
the root probe incorrectly expected permission to replace user-owned binaries.
Those fixture/observer expectations were corrected without weakening product
guards, signature checks, deadlines or assertions. Their original logs remain.
Initial UBS exited1 with two critical findings: the unchecked Rustup execution
was removed; its compound-test syntax finding was resolved by assigning the
unchanged Git probe result before comparing it, without scanner suppression.

The unchanged `.90` native build retry again refuses120 before Cargo
(`started:false`, free59,731,886,080 bytes, normal growth32,212,254,720 and
floor10,737,418,240). Zero of its predeclared 30+6 library cases executed.
It remains in progress. Prior full-load send p99 failure, missing release-build
qualification and all previously documented exclusions remain red. These
installer results qualify an installer using released payloads, not a newly
built server, macOS/Windows execution, independent review or release clearance.

#### Required anti-ceremony worksheets (solo continuation)

Process worksheet: reports/TODO are PROCESS; running code does not read them.
Consumer is the operator who explicitly requested a granular fresh review;
gate is their acceptance of this installer repair against original `br-x0aie`;
observed defect is the retained real cross-UID lock denial. Retire this checklist
as execution history at handoff. Raw immutable source/negative receipts are the
minimal integrity control for source/evidence loss: required at this scale,
smallest reliable record, no cheaper equivalent. Highest ready tractable
capability selected was this installer repair; more speculative status work
would not improve it. Verdict: minimum requested gate, no new runtime gate.

Real-work worksheet: window starts at 334400d0/09:45 UTC. Mechanical inventory
is current three-file source/test diff, TODO/tracker diff, recent Git history,
ready/open graph and latest three closed records; no closure credit is taken
from unrelated history. Work units: USER1 (installer authority/toolchain behavior),
ENABLER2 (kernel/transaction tests and mandatory compiler/checksum handoff),
PROCESS1 (requested TODO/evidence/tracker), UNKNOWN0; this is not a productivity
quota. Demo: retain the foreign public lock and perform the real signed install.
Without process files the product behavior is unchanged. Tests exercise the
repair now, rather than speculative infrastructure. The oldest listed
read-neutrality task `br-mmnyj` and corruption task `br-htobc` remain open;
this bounded ready repair avoids pretending a new
upstream storage investigation can close it. No swarm, follow-up laundering,
spec substitution or plan-only implementation. Verdict: DRIFTING: cold compiler
qualification and excessive exploratory/fixture work consumed too much time.
Correction: freeze source, finish required checks and the actual checksum consumer;
create no additional framework or speculative tasks.

Honesty inventory, all 20 questions: Q1 No (checked complete test/config/history
diff; original assertions and bounds retained). Q2 No (checked new empty-PATH
and real kernel controls; existing launch doubles remain labeled R2).
Q3 No (checked diff: no golden regeneration). Q4 Yes: lock-aware test observers
change with runtime authority; reviewed each directly against independent fcntl,
cross-UID and genuine signed controls, with no suppression/relaxation.
Q5 No (checked runtime diff: no benchmark path or result hardcoding).
Q6 No (checked actual case summaries; native zero-run refusal remains open).
Q7 No (checked observed terminal logs versus claims). Q8 No (checked explicit
fixture/live distinctions above). Q9 No (checked retained failures and prior
red gates are disclosed). Q10 No (checked commands: both output streams retained;
expected internal probes suppress only their own discovery noise).
Q11 No (checked original criteria and owned statuses; no follow-up split).
Q12 No (checked no acceptance/spec edits). Q13–17 not applicable: truly solo,
no delegated work or claimed independent verifier. Q18 No (checked case lists
declared before each run; expanded cohorts/repeats are not summed as unique).
Older-session CASS audit is unavailable (`not_initialized`); no such audit claim.
Q19 Yes: the failed controls and wasted exploration are explicitly listed above;
corrected in place with failures retained, not hidden behind later passes.
Q20 strongest evidence: old full installer fails under real foreign UID lock,
then final genuine signed file/stdin installs and fresh repeats pass under that
same attack. A skeptic can rerun the retained probe with those original criteria.
Disposition: Q4/Q19 corrections and limits are disclosed here and in the handoff;
RH-1 gate weakening, RH-17 failure erasure and proof-class inflation are prevented
by retained assertions, failed logs and separate live/fixture/source receipts.
Fresh solo re-execution is self-verification, never independent verification.

### Integration on main after the November 4 merge

The investigation below records its original 0.5.0 backport and build attempts.
Merged main instead retains the application's public SQLModel 0.5.1 dependency
family, with pool and core pinned to
`c996710674d92c00d23f30fe17ecc5d9644818c7`. The `vendor/sqlmodel-pool` tree is
preserved provenance and is not selected by the root manifest or lockfile.
The native workflow now derives the pool manifest from `cargo metadata
--locked` and runs that selected source's unit tests and formatting check.
Those dependency unit tests use the dependency's development graph; the
separate real HTTP cancellation test exercises Agent Mail's consumer graph.
The workflow refuses an ambiguous pool selection and still requires a nonzero
passing test count. This wiring correction does not itself establish a
successful hosted run, large-mailbox qualification, or issue closure.

Local integration qualification subsequently ran against the frozen
`e088f5f65d62b72369136746b54ef7605bfad566` source, preserving the earlier
attempts below. Nine native inbox/header/snapshot tests passed (84.714 seconds;
3,548 excluded). All-target, separate production lib/bin, and strict workspace
Clippy checks each exited zero. Their source receipts independently matched all
3,910 input records; receipt hashes are, respectively,
`da72417b2f766d1f398cd76b50cb76587cab127572479a11a8d6667d8057583c`,
`2da289035b96e1b254204a63e8643563938f888aeff4b1c1cd308f33430f18ce`, and
`3159847a1aff3273312d3ad9cecaabafadd5c5a988eca5b0350cce9e08b6bfef`.
The native receipt is
`9df353848a8924df5267baeab381771b72efa9f5a4ab1a5c1d614e408d3c91bc`.

Review found that the continuation's `pool::tests::cancelled_` selector executed
zero initializer tests: the compiled module is named `pool_impl`. Both active
workflow commands now use `pool_impl::tests::cancelled_`, and the lane still
requires exactly two passing cases. Executing those original cases from the
same native image produced **one pass and one failure** in 12.75 seconds. The
cancelled first checkout remained retriable; the lazy-initializer waiter case
passed its cancellation/accounting checks but the subsequent fresh acquisition
timed out at its original ten-second bound. The executable SHA-256 was
`328dbda6349e2397805baf8a1013dc2c0b6aa4bb8e55ae3188474b9588a3d7a5`.
No current trace establishes that timeout's cause. This result keeps the pool
qualification task open; successful dependency unit tests do not override it.

The original two-case HTTP Cargo command subsequently passed both cases in
76.00 seconds, with 5,498 excluded. Its receipt is
`f0e0badae55cf604519ac271821b1a8eb9d743fe70ba83966187d97618653657`;
all 3,910 source records matched. This covers the original 600-message
responsiveness workload with its strengthened read requirement, and the real
HTTP cancellation/permit/pool-reuse path. The initializer failure above remains.
The later `9f3bc1b5`
shared-obligation query is a separate source snapshot and requires its own
qualification. No 25,100-message HTTP/archive result or release clearance is
claimed here. Raw terminal logs and receipts are retained under
`/tmp/am-reality-20261103-nSlwbq/merged-gh374-*`.

### November 5: shared-obligation continuation

Upstream `9f3bc1b5fe6a0c4a8942f473ad88ab5604893543` also gives the obligation
count pager a typed `i64::MIN` lower bound and a deterministic message-ID tie
break. The original 257-delivery regression now checks three bounded native
`SeekGE` pages, exact counts, and unchanged receipts/activity. The local source
adds only a redundant-borrow cleanup and three manual test-format corrections.
The first native run passed 32 cases before those formatting corrections; its
receipt `fbf01c6d3b52a7ca96f5d2e6bc72eff6f536d4157fdc234c1e1236f87a9cdf54`
remains evidence for that earlier byte snapshot.

Final formatted-source qualification completed independently:

| Check | Result | Source receipt |
| --- | --- | --- |
| Native portable `project_mailbox::tests::` family | 32 passed, 0 failed, 1 deliberately ignored scaling benchmark; 3,524 filtered; 43.06 seconds | `ac2b9c8d904f8d2e8aeabc9f079c6282ed95572327c32262bae682e8df8f156b` |
| Workspace all-target check | Exit 0 | `2a4c6603a35cc42ff0e8b257bf9d3b3a144c480643d71df5b122dcac761ce67d` |
| Separate production lib/bin check | Exit 0 | `80013ac7d3b666bf567d5a93f18c5017d60f4a1b618d6bf61a34cbc6ef4924a4` |
| Strict workspace all-target Clippy | Exit 0 | `5a8417c325fe7baa5da38cb280a7fca122147157c701a3c6ba8bf013cf64efa8` |
| Final workspace formatting | Exit 0 | Retained local format log |

All four remote receipts contain the same 3,910 source records, freshly rehashed
with zero mismatches. The native executable on `vmi1152480` is 128,102,944 bytes,
SHA-256 `73cf03e378e2ebc11ea76574826528899b18a6c6bd06946881aec5c8137ea730`.
The scan of the changed Rust file reported zero critical, 303 warning and 44
informational findings, with no failed module; it is not a warning-free audit.

The workflow's `shared_inbox_` filter did not select the obligation regression.
It now also executes that original fully qualified case with `--exact` and
requires exactly one pass with zero ignored cases. All 24 workflow shell blocks
parse. This is CI wiring and native query qualification, not a measured latency
win or a successful hosted workflow result. Worker admission/source-packing
refusals and the initial formatting failure remain retained zero-case/red
attempts. The initializer failure above still keeps its separate task open.

The original 25,100-message archive case executed on `vmi1152480` and failed:
zero passed, one failed, zero ignored, 31 filtered, 220.25 seconds. Seeding
completed with 25,100 messages and 117,178,368 database bytes in 120,236 ms.
An unnamed concurrent participant then panicked on a pool-acquisition timeout
(`archive_test_outcome`, line 253). The stop-on-unwind guard subsequently caused
the reconciliation interruption assertion to fail; that is a secondary failure.
The last progress sample reported 36 passes, 16 repairs and 43 scanned messages
after 78,204 ms of the concurrent phase. This was not a 45-minute convergence
timeout, and the case did not reach its final archive/integrity acceptance.
The participant's role and the timeout's cause remain unproved. Receipt
`828cdcc8683ba19808d02afc5db96b03d3bd7ba213118b3bc3b36096ae148bf3`
freshly matches all 3,910 source records. A later attempt to retain the exact
worker executable found its managed source directory empty; no executable
identity or diagnostic replay is claimed for that failed case. Its original
terminal output and source receipt remain retained.

The separate original 25,100-message HTTP responsiveness case passed on
`vmi1149989`: one passed, zero failed/ignored, 5,499 filtered, 57.43 seconds
(57.33 seconds in its isolated child). It recorded 418 real inbox reads,
including 186 large-recipient reads overlapping repair batches: 93 metadata
and 93 full-body reads. Eight batches repaired 16 messages; the concurrent
phase lasted 11,180 ms, maximum batch 1,695 ms, and maximum inbox read 248 ms.
Every original deadline, payload/receipt, unrelated Git staging, cancellation
drain and graceful-stop assertion was retained. Its own witness explicitly
states `full_convergence_checked: false`: this is responsiveness evidence,
not full archive convergence, a comparative speed win, or release clearance.
Receipt `d5d9c422fa87cb307c036f50a165a9aa848bb30f5fb6413f120e128bda139d13`
freshly matches all 3,910 source records. The running worker test executable
and retained local copy are both 327,234,936 bytes, SHA-256
`4fc2770248855eb554f7520d0325083bff88a521ac5b91cd28174e0295c9c70d`.
The 112-minute compilation is separate from the case's runtime verdict.

The unchanged HTTP cancellation case also passed from that retained executable
on the local host: one passed, zero failed/ignored, 5,499 filtered, 31.80 seconds
(31.78 in its child). Its real eight-message mailbox deliberately holds the
only read connection until the original request deadline. The request returned
the intended timeout at 30,001 ms; its dispatch permit drained before the held
connection was released. Pending counts before release and after follow-up were
both zero; subsequent reads took 19 and 4 ms, with one idle connection and zero
active connections afterward. The executable hash remained unchanged. This
separate real-consumer cancellation pass does not erase the earlier initializer
failure, the untraced warmup assertion (zero of five requested connections), or
the large archive failure.

The first optimized portable CLI build reached its 7,200-second build transport
deadline. RCH verified that the remote process group was killed; supported
recovery acknowledged terminal exit 137. It produced no completed CLI or runtime
qualification. A new normal-cache attempt was refused before compilation by
the excluded dashboard's sibling-path containment check (exit 103, zero cases).
An explicit isolated-target retry uses two build jobs and a 14,400-second
build-only deadline; release optimization and runtime acceptance limits are
unchanged. That retry completed successfully on `vmi1293453`: Cargo 119 minutes
26 seconds, wrapper 7,220.3 seconds. Receipt
`478754ebf17bdbac203ea8ed6f8ac7a5515b3a77f56651f5cf6dbf22c4aaa462`
freshly matches all 3,910 frozen inputs. Worker, delivered and retained CLI
images agree: 126,099,488 bytes, SHA-256
`cfd9c443f37d79f76feaac1e6eb9ba01f15ee0070e13d72724a40b7fd03b087e`.
The retained `/data/tmp/am-ee-current-query-cli.nsof9nu7/debug/am` is a standard
optimized portable release image; the `debug` directory only stages runner
lookup. This source is `9f3bc1b5` plus the previously described cleanup, injected
as a dirty build source, with production inputs matching merged `6e70244c`.

The unchanged real mail probe passes 33 of 33 checks in 6.787 seconds, including
the executing process's image hash, identity/reservation/send/inbox/ack/reply,
exact committed archive content, invalid/duplicate identity and broadcast
refusals, and independent stopped C SQLite integrity. Doctor selftest passes
six of six private-directory primitive mutation/backup/byte-identical-undo
checks in 274 ms. Its stderr explicitly says the inline run is legacy/unsealed;
this does not qualify sealed chain-of-custody verification, all seven operations
or the complete failure-mode registry.

Current quiet stdio bundle `tests/artifacts/stdio/20261105_065650` passes
18 assertions with zero failures/skips (48,780 ms wrapper). The whole named
malformed-frame case takes 3,752 ms, within its original 5,000 ms bound; its
response has error code -32700 and omits the request ID. This quiet result does
not qualify the stricter loaded deadline or erase the earlier 5,135 ms case.

The original clean public-control-first workload started at 07:24 UTC as
`tests/artifacts/release_smoke/20261105_072447`. Its original 300-second soak,
300-second convergence, 60 agents, 480-second swarm and 3,600-second total
budget are unchanged. No extra owned local stress is run during this pair.
The wrapper is terminal exit 1; complete-bundle validation exits zero. Receipt
SHA-256 is `3b0bfe73c9d71aa2a1ca587977de3611f2efa1ae3d0a1d37ccf902862ddbfe24`.
Each arm has 53 passing and two failing checks, with different failed conditions:

| Arm | Send p99 | Other failed condition | Acknowledged swarm sends preserved |
| --- | --- | --- | --- |
| Current candidate | 5.620801 s / 10,518 samples | Archive does not converge within 300 s | 10,518 / 10,518 |
| Published control | 6.619425 s / 7,292 samples | Two `RESOURCE_BUSY` tool errors | 7,290 / 7,290 |

Both send latencies exceed the unchanged five-second gate. Candidate tool,
30-second dispatch timeout and connection-error counts outside the declared
kill window are zero. Its 241 earlier acknowledged sends also survive SIGKILL;
both arms pass independent stopped C SQLite integrity and graceful shutdown.
The candidate's last complete archive sample at 287.440 seconds has 22,485 DB
messages, 22,483 canonical archive messages, missing IDs 17819/17820, and 16
uncommitted copies across IDs 17812/17815/17816/17821. At 300.036 seconds the
last scan times out partway through; its 2,594 partial records are not the
actual total archive size, and its empty difference arrays do not prove parity.
The public control converges 18,839 messages and 63,808 committed copies in
267.211 seconds. These different closed-loop populations on a shared host
establish neither a comparative speed win nor a source-isolated regression.

Earlier exact-ID log entries record publication-fence contention and one
identity-change retry. Their causal connection to the final gap is unproved.
The checkpoint was read after the probe and its final restart; its embedded
`saved_at_us` is 08:02:45.698 UTC, before that restart at 08:02:57. Its saved
tail/backfill hints are 872/7419 with 102 retry IDs, excluding the six message
IDs above. This is a retained scheduling snapshot, not a live stack or proof of
how retry debt was lost. The existing archive task keeps
its original acceptance and owner; its evidence comment and direct owner
message preserve this failure without changing the bound.
The actual published GNU v0.3.40 control executable has SHA-256
`009ee7b746daafc26ec5f1ffe7627b38a942a031710c0076de4075323de115b4`.
Fresh release metadata, signed manifest, pinned-key Minisign verification and
streaming archive-member equality were rechecked. Earlier private read-retry
control results remain labeled as that separate subject. The bounded HTTP pass
above establishes neither complete 25,100-message archive qualification nor
release clearance. The CLI image also predates the separate test-only warmup
fixture continuation; those later test bytes cannot be attributed to this image.
The separate loaded-stdio repeat was initially deferred after host disk headroom
fell from 45 GB to 12 GB. When it recovered to 59 GB without cleanup by this
assessment, a separate candidate-only native diagnostic began as
`tests/artifacts/release_smoke/20261105_082849`. Its original 60-agent registration
was observed at 08:35:31 UTC; unchanged stdio started at 08:37:02, more than a
minute later during the 480-second swarm. Bundle
`tests/artifacts/stdio/20261105_083702` passes 18 assertions, zero failures/skips,
wrapper exit zero in 62,194 ms, with complete-bundle validation zero. Its whole
named malformed-frame case takes 4,397 ms, transport 4,120 ms, at the original
5,000 ms limit with no forced timeout. This is a strict passing observation;
the earlier 5,135 ms result remains recorded. Shutdown/maintenance/integrity/
disk/retention source files are byte-identical between `0783de81` and upstream
`a318493a`, preserving the earlier real-worker ordering regressions. The separate
diagnostic is terminal exit 1 with 54 passing checks and one failure: the original
300-second live archive-convergence bound. All 11,606 swarm and 394 earlier
acknowledgements survive; stopped independent integrity and clean shutdown pass.
Its last complete sample at 278.939 seconds has 25,520 DB messages, 25,514 canonical
messages, six missing IDs and 12 uncommitted copies. The final scan again times
out partway through. Receipt SHA-256 is
`6a9e85ee2e50332be9ba74ba534ab51650dd700e7cb891596ac3942bd16ea7fb`;
complete-bundle validation exits zero. Added stdio and one read-only procfs
observation are declared diagnostic interference, precluding a comparative
performance claim. The completed clean pair's failures stand.

Fresh solo review closes only `br-kp1in.86`: the original quiet and loaded
whole-case deadlines now pass, while the retained old-order/new-order real-worker
controls establish the shutdown wiring change and every join/flush remains.
The current mail, cancellation, acknowledged-data, independent integrity and stop
controls are cited above. The archive failure occurs during the live quiet period,
before shutdown, and remains on the pre-existing archive task with its original
acceptance. Neither that task, the pool-timeout task, capacity nor release is
closed by the bounded shutdown result.

The existing manual native workflow dispatch was accepted for published source
`6e70244c` as run `41957916399`, but the latest read still reports `queued` with
no jobs. Cancellation returned HTTP 409, "not been queued yet," despite that
status. No hosted test executed or passed, and no cancellation is claimed.
This dispatch was a mistake: `docs/RELEASE_CHECKLIST.md` and
`docs/ROLLOUT_PLAYBOOK.md` explicitly prohibit enabling or dispatching Actions.
The supported force-cancel endpoint repeatedly returns HTTP 409 with the same
reason; a fresh read at 14:49 UTC still has zero jobs. No setting was enabled,
release published, or Actions evidence used for qualification. Stop this path;
the canonical DSR/strict-RCH policy remains in force. Retain both the erroneous
dispatch and failed withdrawal attempts rather than describing them as progress.

### Separate warmup fixture isolation

`br-kp1in.92` addresses a reproduced fixture defect, not the untraced original
ten-second pool timeout. The old warmup test sets a private DB URL but leaves
its archive inherited. With the retained old executable and an unrelated private
regular-file `STORAGE_ROOT`, its original exact case fails at pool creation in
0.43 seconds (zero passed, one failed), leaving the sentinel unchanged.

The corrected fixture explicitly owns its archive and runs that foreign-root
control in a child process. It adds 26 test-only lines; the parent's environment,
production authority checks, min/max pool sizes, five requested connections,
ten-second budget and original pool-stat assertions remain unchanged. An earlier
proposed process-wide override was rejected during review and was never landed.
The corrected original native case passes one case, zero failures/ignored,
3,556 filtered, in 2.13 seconds. Receipt
`9a773c10c0bbd38fa3f936d1ffe8034959e4934cf0851fd12aec584ddff8b319`
freshly matches all 3,910 inputs. Producer descriptor, retained worker image and
local copy agree at 128,110,616 bytes, SHA-256
`bbb60aed2b4f8ccb9f1d44e26cb855a3b86630b2af48c1bfe61993930d36ad17`.
An additional exact leaf replay with the real OS environment supplied at process
entry passes in 2.870 seconds and preserves sentinel inode, mode, size and bytes.
Those image comparisons describe the original collection. At 10:48 UTC both
recorded warmup image paths are absent; a fresh local invocation exits 127
before running a test, and the bounded worker lookup also fails. The cause is
unknown. Original terminal logs, identities and the separate leaf replay remain
retained. No new warmup execution or presently available image is claimed, and
no cold rebuild is launched merely to replace this missing artifact.

Five incoming upstream files do not enter this DB-library test target: CLI
adoption implementation/tests, the standalone DB integration test, its workflow,
and changelog. DB/core/test-helper sources, root/DB manifests, lockfile, toolchain
and Cargo config are byte-identical from native base `3a184ce5` to combined base
`a318493a`; Cargo artifact records contain no CLI package and only the DB test
target. The full raw receipt arrays are consequently not claimed identical
across those two repository snapshots. Combined-source workspace all-target and
separate production checks pass with 3,911 freshly matched inputs each, receipts
`45b5b98c49eff8254b9ef41966a92159f08ab0b454dbcdfae703eae9a1143112`
and `7fd19b5515f83c3f19960947af80d70afaa0beccfe8562abcad1bf7a08623fe5`.
Strict Clippy on that combined snapshot failed with exit 101, receipt
`93d2bf96f77f88838e735a6b064e717a9436da601e186185ba0d9994cd062e80`:
four lints in the incoming GH372 integration test. Separate task
`br-kp1in.93` renames an ambiguous measurement vector, uses the required
`if let`, and extracts the existing real SQL snapshot read into one helper.
All eleven observations and their assertions remain. No lint allowance,
workload, memory bound or acceptance threshold changes. Its frozen source is
being checked independently; earlier gates are not final-source evidence.
The new production lib/bin check completed exit zero and supported RCH recovery
acknowledged that result after collection failed. Receipt
`8a5d0bf95ab342bc4c6eaab719064667f64afc9bd42c9bf29ba92d20d2838bca`
freshly matches 3,911 source records with zero mismatches. Final strict Clippy
also passes, receipt
`ef9145caddd5567f45a7d4b7bb335bbeebf1bc4bfb270b849f7d3eb03c21bf57`.
The original real snapshot integration case passes one case, zero failures or
ignored, 817 filtered, in 0.69 seconds, receipt
`23d904831d79e0c9a2889aa1704e7999d9076498030687e69202cbfeac627a53`.
Both receipts freshly match 3,911 inputs with zero mismatches. Both initial
clients failed during collection; exact recovery acknowledges terminal zero.
The native producer descriptor, worker copy and local retained image agree:
106,990,160 bytes, SHA-256
`85f5c703128e30133ce36097771af85e07558676593da051cc46afbabc50b327`.
This is transaction/snapshot/durable-write evidence, not the larger memory
gate. Final all-target also passes, receipt
`56650b97655955da9590d96f7971587320fabc92e693900031e388740ceede9b`,
3,911 freshly matched inputs, zero mismatches. Its initial collection failure
was recovered as terminal zero too. The manually integrated two-file diff is
byte-identical to the frozen qualified source and formatting passes on main.

Formatting passes; the pool-file static scan reports zero critical,
4,819 warning and 2,305 informational findings with no failed module. The changed
test's 15 findings are retained test assertions/unwrap, private scratch I/O and
literal-argument self-execution, without suppressions. Commit `751138c8`
contains the two qualified test repairs. Fresh solo acceptance (not independent
verification) closes lint task `.93` before dependent fixture task `.92`.
The snapshot replay passes one case, zero failures/ignored, in 1.88 seconds.
Warmup acceptance cites the original native and separate leaf replay on the
identical pool source; its later missing-image limitation remains above.
No runtime performance, original pool-timeout, large-memory or release repair
is claimed from this fixture work.

The separate GH372-file scan exits nonzero with two critical findings at
unchanged, deliberate test panics: rejecting a non-index B-tree page and refusing
an absent `/proc/self/status` measurement. Both remain intact. Raw totals are
two critical, 249 warning and 70 informational findings, without a failed module;
this is a reviewed result, not a clean scan.

### Unsaved retry debt survives same-mailbox pool recreation

`br-kp1in.94` investigates a concrete source path: archive workers recreate a
pool after a source error, and the new pool's cache identity resets the cursor
before dirty checkpoint debt can merge. Its causal connection to the failed
full workload is unproved. A private test-only control uses a real mailbox,
publication-fence refusal, checkpoint I/O refusal and a second checkpoint owner;
it compares retaining the pool against reopening the same physical mailbox.
The first probe changes only tests. It runs two cases:
one passes and one fails, zero ignored, 1,039 filtered, in 4.75 seconds. It fails
before the intended debt assertion: `create_pool` reuses the still-live weak-cache
entry held by the fixture, so its key does not change. This is an invalid setup,
not proof of lost debt. Receipt
`d4851d48b65960a9f15052a7bb58d1a402f476f4f92069e50f761d3d0359ffac`
freshly matches 3,911 inputs. The 98,096,200-byte failed-setup image is retained,
SHA-256 `ca1a30a304c8e81e9bfd3fe6347c735a89af4a5db73a94fd800587be66fcad41`.

The corrected probe separates kept, cached-factory and genuinely new `DbPool::new`
handles, retaining the different-key prerequisite for the new-pool case. Its
predeclared original-code set is three cases: two positive controls and the
intended failure only if unsaved retry 903 is neither retained nor committed.
The corrected original-code run executes exactly that set: two passed, one
failed, zero ignored, 1,039 filtered, in 8.61 seconds. The new-pool case verifies
equal source binding and a different cache key, then reports retry 903 neither
retained nor committed (one scanned/repaired row) and fails at the intended
assertion. Receipt
`dca9d75bdae67c21d91c76e3c9378e0b1898d5b6aa4b9942174aa05a703de79f`
freshly matches all 3,911 inputs. Its 98,096,904-byte producer/retained/delivered
image hashes agree:
`c27748e31099a0c4859a82ed424089c84ce237554f3b7684520b4552eaa98de9`.
A fresh local replay of this identified worker image reproduces the same two
passes and exact debt-loss failure in 8.94 seconds. This establishes the narrow
cursor defect, not the cause of the original full workload's archive gap.

The repair retains an independently captured, process-local
full `Binding` on the cursor. A changed pool key can preserve memory only when
that binding matches; changed physical/logical/archive identity still resets,
and an explicitly cleared source identity remains invalid. Nothing restores
this proof from checkpoint JSON. Existing live-source/no-clobber checks, dirty
debt merge, bounds, cancellation and checkpoint schema remain intact.

The first repaired-source family runs all 51 selected cases: 50 pass and one
fails, zero ignored, 992 filtered, in 60.04 seconds, receipt
`78f629b15b3835bc6d02cb4001ff367c7d451f358a6536361abf4aebef339b87`
(3,911 freshly matched inputs). All original pool controls pass. The new
changed-generation control reaches the fresh-row commit, excludes old retry
903, then fails because its oracle incorrectly unwraps a missing Git entry as
success. Its identified 98,126,376-byte image is retained as
`c95370e5b390295eeabcfd5a27ce5daf4c909e5df8d90273b90bd55fdce17d5b`.
This is a failed family, not completed qualification.

The corrected oracle independently requires both filesystem `NotFound` and Git
`ErrorCode::NotFound`; arbitrary verification errors cannot pass. Production
code and all three original probe bodies are unchanged from that first repaired
run. Formatting passes; final scans report zero critical findings, 698/96
warning/info on the parent and 844/209 on the child, with no failed module or
new suppression. The corrected final family passes all 51 selected cases,
zero failed or ignored, 992 filtered, in 53.34 seconds. Receipt
`7093617b807d2fd3699ed319ca8549eeb48afcd3393a1cbaaa9eb6a6cb8d6719`
freshly matches all 3,911 inputs. Cargo exits zero; the collection-only client
failure recovers zero with terminal acknowledgement. The producer, retained
worker and protected local 98,129,568-byte image all hash
`fb613ab342f58c1b7d511a72a2d7bdaeaf5b5a733684a493bcab94dfb2a8cdc7`.
The all-target workspace check finishes zero in 26m56s; collection recovers zero
with terminal acknowledgement. Receipt
`53b05e500bd83562b55327957bc775472f7d4d2df45da2fbe5cd4245f4299d96`
freshly matches 3,911 inputs. A fresh solo replay of the protected final image
passes all four pool-reopen cases, zero failed or ignored, 1,039 filtered, in
12.87 seconds. This is not independent verification. The separate production
lib/bin check finishes zero in 38m00s, receipt
`1e0056760e4012c699e801fce9cac2d2c5c77ace9184ee1e7666043cc9a89e2a`.
Strict workspace Clippy with `-D warnings` finishes zero in 52m18s, receipt
`aaa4cab235a987212e18ee2a955c9561433008f5a2c1c6acd34692ca64f1cc4d`.
Both collection-only failures recover zero with terminal acknowledgement;
each freshly matches 3,911 inputs. All four final receipt input tables agree.
The initial
all-target source-authority-lock failure recovers 120 with acknowledgement;
superseded production/Clippy clients stop at 143 and recover 120/137 respectively,
all acknowledged and none counted as passes. A documented isolated all-target
retry preserves the same source and compiler gate. Earlier disk refusals,
stopped attempts, the invalid Binding clone and the avoidable constructor
assumption remain retained. The two changed source files match the qualified
candidate byte for byte; formatting on main also passes. Source commit
`b600735b` implements the repair, and `.94` is closed after the fresh solo
acceptance pass. The original archive task `.35`, pool-timeout investigation
`.87`, capacity and release gates remain open. No full-workload causal fix or
comparative performance win is established by this repair.

All owned native/compiler wrappers are terminal and acknowledged. The retained
evidence copy is `/data/tmp/am-reality-preserved-20261105.AnVT1Bbf`; comparison
of 5,540 regular source/copy files finds zero mismatches. Six protected
CLI/snapshot/checkpoint executable hashes are freshly verified. The original
evidence remains, and the older honesty worksheet is preserved separately.
The final honesty audit records excessive rebuild/report overhead and the
avoidable setup/oracle errors; it gives credit only to the narrow real repair.

The initial cancellation-accounting fix and qualification tests are on main.
Formatting, provenance, configuration parsing, and independent source review
have passed; those checks are not native execution of the pool, HTTP, or recovery
paths. Native execution evidence and its limits are recorded below. The failed
large archive attempt and the bounded HTTP pass above do not establish complete
large-mailbox qualification.
The changes do not establish production-scale qualification or resolution of
issue #374.

The maintainer's [November 4 comment](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/374#issuecomment-6632024968)
reports four DB tests, 96-message archive convergence, and 600-message HTTP
responsiveness at the base commit. These are prior maintainer results, not new
results from this investigation. That comment explicitly leaves large-mailbox
qualification and the pool's pending counter unresolved.

## November 6 fresh-eyes review: ATC build repair and startup ownership

The reviewed source is `aceebacc297aacc262de246469121dede4ae0156`
with three manual source overlays in the separate
`mcp_agent_mail_rust-ee-startup90-20261106` checkout. The production repair
qualifies `std::result::Result` in the ATC population collector: SQLModel's
one-parameter `Result` alias caused E0107, three E0277 diagnostics and E0308.
The other overlays add two required semicolons to existing test assignments
and correct a stale comment about automatic database recovery. SQL, byte
limits, refusal behavior, assertions, test populations and deadlines are
unchanged. The lifecycle implementation was already present at the baseline;
this review does not claim to have newly implemented it.

All three required compiler graphs pass on the same 3,917 exact file records,
freshly checked against the frozen source with zero mismatches. Commands use
`--locked`, `-j2` and `--config profile.dev.debug=0`, strict remote execution,
and the existing nightly-2026-08-31 toolchain on `vmi1149989`:

- Full all-target strict Clippy, including `--keep-going` and `-D warnings`:
  client/remote exit0, receipt
  `e565e112d4769f83d02b9c5fe05ffb4ae5d85d25a916856a73d99c5a7ea4d5ac`.
- Workspace all-target check: client/remote exit0, receipt
  `eabec2cfc983ec88ec1bbed4bfd574e53aa002f028026f2ac94f38551d8de8d2`.
- Separate production lib/bin check: client/remote exit0, receipt
  `f1994bbe27b4b6a60812bb631fc2f6369bdaaf70151d66492ca573046b1eb705`.

The failed initial CLI build remains receipt
`c59201fcfa025f8ed27272edbeed3c08152610d64afaebb0484f3a8f33e77e97`
(101, the query type errors). The intermediate full Clippy remains receipt
`470b31f8182254c4861c1f8a477862424a7be9b9b8552bf91190ae61278970de`
(101, the two semicolon lints). Both matched 3,917 fresh inputs at their
respective source points. They are not discarded or rewritten as passes.

Frozen formatting passes. UBS on the eight baseline reviewed files returns0
with zero critical findings, 12,603 warnings, 4,026 informational findings
and no failed modules. Separate scans of the three final changed files
return0 with zero critical findings, 7,735 warnings and 2,462 informational
findings in aggregate, with no failed module. These are scoped scanner
results, not a warning-free or full-workspace qualification claim.

`br-kp1in.90` remains in progress. Before native execution, the declared
selection is 30 unique cases (17 server, seven DB, six storage), followed
by six fresh solo repeats. It covers real pool acquisition, delayed search
and exact subsequent indexing, retained writer contention, interrupted
archive healing and consistency reads, shutdown/deferred-start ordering,
and the unchanged ATC bounds controls. Compiler success does not execute
these cases. Original quiet/loaded stdio and mail/durability acceptance
is required in addition; this candidate's results are below. Earlier November 5 results are historical evidence
for their own source cohort, not a substitute for this candidate.

Several native requests were refused before Cargo: namespace collision103,
disk admission120, queue timeout103 and a draining worker103. The latest
native request was refused120 despite 69,745,319,936 free bytes because
normal active reservations and the protected floor did not fit its normal
30GiB growth reservation. No resource budget, protected floor, worker
configuration or foreign job was changed. These attempts ran zero cases.
The corrected production CLI build passes client/remote0. Receipt
`309123f55f2d4144251080d6f725643051c1f17d50cbd7f23faa4f02d8a65bc5`
freshly matches the same 3,917 inputs with zero mismatches. The worker,
retrieved and protected local 305,231,496-byte CLI images share SHA-256
`caac40a68450b715cb17b9a03450b3b2c5a172d4b7493794ad7f7634dc625479`.
Quiet native-runner stdio passes: seven cases, 18 assertions, zero failures
or skips, 45,194ms. Its malformed-frame whole-case time is 3,484ms, below
the unchanged 5,000ms bound. These subprocess/transport results are not
the unexecuted 30 selected library cases. The native-build retry after the
CLI completed is also refused120 before Cargo (65,247,039,488 free bytes,
normal reservation and floor unchanged). The full original release-smoke
workload ran against this exact protected CLI in a private mailbox:
**FAIL, 54 checks pass and one fails**, native runner exit1/974,330ms. The sole
failure is `send_message` p99 **6.010874s > 5.000s**, with8,830 measured
successful sends, p50 2.479508s and max12.346421s. The original gate is retained.

All six original phases executed: flow, cross-project isolation, storm, crash
and soak pass; swarm fails that latency condition. The300s soak reports9,907
sends,53,045 inbox reads and zero tool errors. The60-agent/three-project swarm
runs486.2s, SIGKILL at245.7s, restart8.31s; all8,830 acknowledged swarm sends
survive. Post-load archive verification completes in21.453s within the unchanged
300s bound, without MCP reads:19,788 DB messages,19,788 verified Git archive
messages,68,194 committed copies, no missing/extra/duplicate/mismatched IDs or
missing/uncommitted copies. Both stopped independent C SQLite integrity checks
and clean shutdown pass. SQLite FD maximum is2 and dispatch zombies0.

Loaded original stdio runs09:29:10–09:30:09UTC, after registration of all60 swarm
agents at09:27:10UTC. It passes seven cases/18 assertions with no failure/skip
in59,723ms. The whole malformed-frame case is3,946ms versus the original5,000ms
bound; `timed_out=false`, deliberate parse-error server exit1. Quiet and loaded
execution use the same protected CLI. Concurrent stdio is declared diagnostic
interference, not a clean performance comparison. The swarm producer and running
`/proc/<pid>/exe` hashes match that CLI. Original host load averages are retained
in the receipt, not used to waive the failure.
This is the development profile (opt-level1, with the recorded debug override),
not a release build. Even a passing full workload cannot satisfy `.35`'s
explicit release-build criterion. That peer-owned task remains open.
Raw logs, source diffs and receipts are retained under
`/data/tmp/am-startup-review-20261106.mSIrexF8`.
Final verification again matches all3,917 frozen inputs with zero mismatches;
the three main source files match the frozen files and the protected CLI hash
is unchanged after all execution. All13 identified owned RCH journal entries
are terminal, acknowledged and have no live wrapper. The global inventory
itself returns1/partial with117 journal errors; this is no whole-fleet health
claim. The initial namespace refusal's client103 and final journal1 are
separate retained statuses, with no Cargo execution or test credit.

An avoidable Cargo-help inspection was intercepted and offloaded by the
managed shim. The cancellation request was initially unconfirmed (E323);
exact supported recovery later returned1 with terminal acknowledgement.
The shell pipeline's later grep returned0 and supplies no validation credit.
No replay or successful-cancellation claim is made.

Process worksheet: the operator consumes the requested TODO and report;
their review request supplies provenance and the original `.90` acceptance
is the decision gate. The existing checklist retires as execution history
after handoff. Raw receipts and protected executable copies are the minimum
integrity controls needed after observed source/producer-artifact drift;
they create no new release gate or status framework.

Work audit window: November6 06:46UTC onward. At the pre-publication inventory,
actual Git history has no new commit in the window; the most recent `.95`/`.96`
closures predate it. The three-file patch is a build ENABLER plus a comment correction;
qualification is an ENABLER; tracker/checklist/evidence edits are PROCESS.
Changed-file tally before publication: USER0, ENABLER2, PROCESS6, UNKNOWN0;
compiler/transport execution is a separate ENABLER activity, not another feature.
There is no new user-facing feature or measured performance win. Disposition:
DRIFTING, because repeated analysis, oversized diagnostic output, a cold build
and the accidental help offload consumed too much time. The corrective action
is to finish the concrete repair and retain unmet native acceptance honestly.

There is no user-facing demo yet; the concrete consumer is the successful
CLI compiler and live stdio subprocess. Removing PROCESS edits would not
change runtime behavior. The older full-load/convergence work remains open;
this window follows the operator's fresh-review and startup-ownership request.
No subagents, follow-up carve-outs or closure claims were used. Redirect:
finish the original real workload and publish the minimal build repair.

Honesty worksheet (20 questions): 1–6, no test removal, ignore, weakened
assertion, regenerated golden, narrowed original workload, new substitute or
fake pass; 7, actual compiler failures and successes are separate; 8, no
independent-review, release or moving-main qualification claim; 9, failures
and original unmet criteria remain visible; 10, stderr is retained and the
masked help-pipeline status earns no credit; 11, no Bead closes without its
named probes; 12, requirements remain unchanged; 13–17, solo work, with no
agent-consensus claim; 18, the native denominator is declared before execution
and child cases will not be counted twice; 19, the overhead and diagnostic
mistakes above are admitted; 20, the strongest current evidence is the actual
compiler negative followed by all three corrected, source-bound compiler
positives, now followed by source-bound quiet/loaded stdio and real persistence
results with the full diagnostic's failed send-latency gate visible. Library native
execution remains a separate requirement. Checks for 1–6 and12 are the entire
current diff (only two test semicolons; no test/gate/config/golden additions),
for7–10 the retained command logs and actual exit files, for11 and13 the
current Bead state and bounded recent-closure listing, and for14–18 the solo
dispatch record and pre-execution selection. Countermeasure for19: bound
output before printing, stop report polishing, inspect Cargo profiles from
source/docs rather than invoking the managed help shim, and credit only the
original job's terminal result rather than a pipeline's last status. Preserve
first failures (SM-11) and distinguish compiler, library and live transport
proof (RH-2). No unobserved history sweep is claimed;
CASS reports not_initialized.

## November 5 fresh-eyes continuation: checkpoint and worker-start failures

`br-kp1in.96` closed first at06:28:58UTC on November6; `.95` closed afterward
at06:28:59UTC. The qualified candidate is frozen
in a separate checkout at `5ec128ceb55259eab9ced0183485b0c635f02298`
with five source overlays and three documentation overlays. Four source
overlays are test-only; the fifth preserves ATC guard semantics. Commit
`971c1d2e` already contains the six reviewed files, including bounded journal-claim
release waiting, the exact constructor-spawn panic reason and the progress-clock
correction. Main and the frozen candidate contained identical reviewed code at
preparation. Subsequent strict Clippy found two errors in incoming ATC code;
the final overlay adds only an isomorphic lazy Option branch and documentation
paragraph split, preserving guard priority. Final compiler qualification now
passes after the test-only corrections below. Final native execution passes
81 unique tests plus8 fresh solo repeats. The earlier78-case positive remains
its own source cohort.
Earlier `.94` receipts do not qualify this candidate. Raw attempts are retained under
`/data/tmp/am-fresh-eyes-20261105.n3gaE0Mt`.

Canonical checkpoint repair6684bcfd and CLI admission workd903bf5a arrived
during qualification. Both are preserved. The combined checkpoint file is
byte-for-byte incoming6684 plus the original77-line regression block, also
byte-identical to the demonstrated negative. All three incoming cases remain;
the final family therefore selects55, and the obsolete-checkpoint fresh pass5.
The earlier whole-test-module identity assertion applies only before these
additions. The incoming CLI work is partial progress on br-hb3mk, whose native
and full engine-exclusion criteria remain open. Stopped intermediate jobs earn
no test credit: v8 native/Clippy and preliminary main production clients143;
exact recoveries137/120/120, respectively, all acknowledged. The shared index
was initially empty, the incoming change had no file deletions, and both
fast-forwards preserved all reviewed source bytes and the original regression.

The unchanged old-code regression actually fails at the intended assertion:
0 passed, 1 failed, 0 ignored, 1,043 filtered, 3.16 seconds, exit101. After a
real blocked checkpoint publication, obsolete disk state erases independently
current memory `(Some(925), None)`. Source receipt `3f008c11` freshly matches
3,911 records; protected 104,500,840-byte ELF SHA-256 is
`c94cc0404d0ea538b436c271647fdd3e9389e1032682316e1d58ec6aeef3399b`.
Remote compilation succeeded, while client retrieval failed; exact recovery
returned0 with terminal acknowledgement. These are separate results. The
candidate rejects all foreign disk hints, preserving memory only when the
previous pass already independently bound it to the current source. Today's
new binding is installed after restore, so it cannot bless unknown old progress.

Fresh review also found WBQ/backlog thread-start panics and an unowned partial
coalescer startup. The candidate retains staged operations/journal ownership
when a consumer cannot start and joins every earlier constructor worker before
a later spawn refusal unwinds. Three new Linux x86_64 tests request an impossible
stack allocation, using the real operating-system error, retained signals,
real message archives and an observed live first worker. All three pass, as
do the eight normal worker/queue controls. Global lazy-pool restartability and
loaded shutdown are not claimed.

The earlier native build receipt `fe78d8a2e748be4655e058199e6513bbe37dc8c24df668ccdffbf93416756803`
freshly matches all3,917 input records. Cargo and client exit0, terminal
acknowledgement true. The 105,417,096-byte storage test image has SHA-256
`befb9603cd6ca8435d8755cf5e8e0969308ee8dc400714c270513b3b9d502372`;
the 147,628,992-byte DB image is
`a3a43978dc75cd134c2a0e6f9858531aeb5ed40a514023a7c1d16cb3b8761bff`.
Producer and worker-retained/local images matched at capture; local hashes
remained unchanged after execution.
The earlier build after the ATC-only correction produces those same images.

That cohort's actual unique native results are checkpoint55pass (113.16s), real search5pass
(0.89s), reservation-source5pass (18.11s), real spawn failures3pass (0.20s),
normal worker controls8pass (1.61s) and typed FD classifiers2pass. Fresh solo
checkpoint5pass (21.03s) and spawn3pass (0.32s) are repeats, not independent
review. Total78 unique tests, eight repeats, zero failures or ignores.
Correction: planning wrongly described the existing reservation CPU comparison
as ignored. It is an ordinary test in the pinned source and also passed; its
child1pass is not another unique test. No test or ignore changed. Its real
incumbent/candidate query measurements remain query-only evidence, with no
running-server performance claim. The other77 cases match the predeclared
functional selection.

Full final-source compiler qualification subsequently found and corrected
test-only similar names, a stale TUI fixture's `TempDir`/`SnapshotDirGuard` type
mismatch, three empty-result assertion styles, and the new callback's complex
type. The real stale fixture directory transfers through `TempDir::keep()` to
the existing guard; its DB path, all assertions and cleanup ownership remain.
The callback receives a test-only type alias. Migration assertions compare the
same returned `Vec<String>` with a typed empty vector. No runtime deadline,
historical checksum, source/receipt assertion, ignore or compiler allowance changes.

Strict Clippy exits0 (receipt `d5c1fde4`), all-target check exits0 (`de58c219`),
and the separate production lib/bin graph exits0 (`6509ac34`). Each freshly
matches the same3,917 records, zero mismatches. Cargo `--keep-going` only
collects independent target errors; `-D warnings` still applies. Formatting0;
final eight-file scan0critical/12,561warning/4,021info, no failed module.
Failed101 receipts `c807be1c`, `cfc71c52`, `86fac71d` and `8396c144` remain
retained with fresh3,917-record/zero-mismatch comparisons.

The final native selection adds the existing three lifecycle migration cases.
All81 unique tests plus8 fresh solo repeats pass, with zero failures or ignores.
Native build receipt `8809b5311d9569c8e191f490627142cbe98107d07e23f167a701f25f42392b40`
freshly matches the same3,917 exact inputs as all three passing compiler gates.
The105,416,688-byte storage image is
`685b33913f64254d2bda4c64f7fbf82632d3d5d03121bc8511298a44c00f26fe`;
the147,628,888-byte DB image is
`14ad6a1da3c0453614a1bfbd3129dd22c188ac9ad5702aedb456b4fefa69cb25`.
Both producer descriptors remained stable across byte-identical copy. Protected
worker copies and executed local copies still matched after all cases. A later
managed-producer lookup failed because both paths were unavailable; its cause
is unproved. That failed lookup is retained, with no successful post-execution
producer recheck claimed. This is why the protected images remain necessary.

| Final native selection | Passed | Seconds |
| --- | --- | --- |
| Checkpoint family, including original regression and pool-reopen controls | 55 | 139.03 |
| Real OS worker-start failures and recovery | 3 | 0.26 |
| Existing normal queue/backlog/coalescer controls | 8 | 1.80 |
| Reservation source, including ordinary CPU comparison | 5 | 21.55 |
| Typed FD-error classifier controls | 2 | 0.00 |
| Real search writer/cancellation controls | 5 | 1.31 |
| Canonical SQLite lifecycle compatibility | 3 | 0.09 |
| Fresh solo obsolete-checkpoint replay | 5 | 23.33 |
| Fresh solo spawn-failure replay | 3 | 0.23 |

The last two rows are repeats, not additional unique cases or independent review.
The CPU comparison's child summary is not counted twice. Migration cases use
real canonical SQLite for compatibility;
they do not qualify the runtime engine's complete migration path. Corrected
server test fixtures are compiler-checked, with no new server-runtime claim.
An owned compile-onlyj1 attempt was stopped through supported recovery
(client143, earlier daemon130, recovered120/acktrue, zero cases) after actual
dependency recompilation disproved assumed warmth. The normalj3 build uses
the three advertised worker slots; source/profile/features/runtime bounds and
resource floors stay unchanged. No build or wait is a test pass.

Raw four-file UBS first reported4critical. Two actual worker panic boundaries
are repaired. The remaining deliberate constructor refusal now has rollback;
the permission test executes only a retained current-executable descriptor,
then drops uid/gid. Its original root branch passed1/0/0 on the identified old
ELF. Both narrowly annotated lines retain their behavior. A separate planted
untrusted executable and unconditional panic still produce exactly2critical
in the same annotated countercontrol file. Earlier five-file UBS exits0 with
0critical,9,854warning,3,410info and no failed modules; this is not warning-free
or release clearance. The earlier seven-file scan exits0 with0critical,
11,711warning,3,930info and no failed module. All three planted hazards remain
red in the expanded countercontrol. Formatting exits0. All first failures are
retained, including ATC Clippy101 receipt `0ea4bd37` (3,917fresh/0mismatch),
the incorrect ignored-case planning/parser assumption and normal disk-admission
refusals. No compiler allowance, resource floor or runtime deadline changed.

Newer incoming peer ATC/population and installer work arrived after this source
freeze. It retains separate qualification; these receipts do not establish a
latest-main, full-server, installer, platform, capacity or release pass.
Qualified implementation commit `2c54ddb05f6594c4059b7dde29ab34966da3d353`
records the reviewed test corrections and the two evidence-based closures.
Merge `8d7ac0861706c1d22124b96966798dfc04d756c0` preserves the newer peer
`52b07086`/`c20d9dbe`/`d35decb6` history. It merges the ATC assertion change
without conflict and deletes no files. Post-merge formatting and four individual
installer Bash syntax checks pass; these are separate from the frozen receipts.
Five individual Bash syntax checks passed. An earlier multi-file `bash -n`
command parsed only its first argument; the individual checks correct that
verification mistake. Live mailbox health was freshly healthy, with860/860
reservation parity and zero drift; no repair was applied. All53 owned RCH
journals examined after native execution were terminal and acknowledged.

### Filled anti-ceremony worksheets (solo; final disposition)

Process worksheet, November5 review block: running code does not branch on
this report or TODO. Consumer: the operator who explicitly requested a detailed
review and granular TODO. Gate: the requested review/completion report for
`.95/.96`; observed defect: actual lost checkpoint progress and worker-start
ownership failure. Retirement: execution history after implementation or honest
handoff, with no recurring gate. Raw source/executable copies additionally meet
the minimal integrity exception: managed binary retrieval actually failed;
copies are necessary at this scale, minimal exact inputs/images, and no cheaper
reliable identity-preserving replay exists. Opportunity cost: finish the real
checkpoint/worker fixes; another report-polishing hour would add less value.
Verdict: minimum requested report only; freeze additional status machinery.

Real-work audit, main's12commits through `cc480333`: README purpose is a
mail-like coordination layer with durable Git archive and SQLite indexing.
Mechanical log classification: `d77a2672`, `98211dcb`, `24ffa2de`, `805ce223`,
`0018483c`, `a9253d72`, `64cf2ee5` are USER (peer runtime changes);
`db6ce671`/`e7c2db6b` are ENABLER integration merges; `cc480333`, `1adf2bec`,
`283245fe` are PROCESS. Tally7/2/3/0; peer work earns this agent no credit.
Shared-checkout commit `971c1d2e` subsequently integrated the reviewed repairs;
`6fb538fa` records process metadata. Their publication supplies no test credit.
(1) The checkpoint and worker-start repairs now have the named native and compiler proof.
(2) Omitting status edits changes no runtime behavior. (3) Compiler/static
checks gate the named fixes; no new speculative framework was built.
(4) Original `.35` live-load convergence remains in progress since October8;
the narrow progress-loss defect was reproducible, but does not prove its cause.
(5) No delegated panes. (6) Metadata commits stand for reporting, never
implementation; `.95` depends on `.96`, and unmet original criteria stay open.
Verdict: DRIFTING. Excessive analysis, repeated cold builds and infrastructure
diagnostics consumed hours before integrating a current-block fix. Correction:
freeze scope and reporting machinery; finish the two concrete fixes and their
original checks. Waiting and receipts earn zero capability credit.
At the pre-publication refresh through `323763a3`, the latest12commits classify
as6 USER (`9c065222`, `971c1d2e`, `69451f2f`, `51a96dcc`, `d903bf5a`, `6684bcfd`),
2 ENABLER (`a33f95a7`, `5ec128ce`) and4 PROCESS (`323763a3`, `5264350f`,
`6fb538fa`, `cc480333`). Peer work earns no credit here. The current test-only
compiler repairs enable qualification; metadata reports the result. DRIFTING
remains the overhead verdict: excessive analysis, setup and repeated cold builds
were avoidable. Two bounded repairs are now complete on their cited evidence.

Honesty inventory, current fresh-eyes block:

1. No (checked: all owned test diffs, unchanged original regression block plus
   byte-exact preservation of the complete incoming checkpoint file,
   untouched limits/config/CI and main reflog); no removed assertion or new ignore.
2. No (checked: actual SQLite/Git fixtures and real OS spawn failures); observation
   hooks arrange filesystem/timing failures, never fabricate backend results.
3. No (checked: owned diff and retained golden review); no golden regeneration.
4. Yes: three line-scoped UBS annotations, separately justified above with the
   root native permission case, constructor rollback and a still-red countercontrol.
   The third is the existing synthetic token in a cfg(test) temporary-DB fixture;
   its exact value and assertions remain. Expanded countercontrol flags all
   three hazards, including a separate unannotated credential-shaped literal.
   Also corrected an ignored `_TIMEOUT_SECS` typo to documented `_TIMEOUT_SEC=7200`
   for compilation only; no test/SLO budget or runtime retry bound changed.
5. No (checked: exact old/new regression body and unchanged authority/budgets);
   test-only impossible-stack selection exercises genuine OS failure.
6. No (checked: actual native summaries); compilation/refusal attempts ran zero
   cases and are never counted as successful test runs.
7. Yes: an intermediate Bead comment said “still compiling” after the job ended,
   and another assumed exit120 where recovery established137. Corrections4055
   and4058 retain both original statements and the corrected observations.
8. No (checked: source manifests, identified ELF and real failure assertion);
   the eight fresh solo rechecks are not independent review.
9. Yes: any happy-path summary without first failures would hide material limits;
   raw Clippy101, UBS4critical, admission refusals, monitor137 and client retrieval
   failures are retained separately. Final scoped native/compiler positives do
   not erase those failures or the unqualified broader gates.
10. No (checked: retained stdout/stderr logs and recovery receipts); evidence
    stderr was not discarded. An initial formatting command masked its status
    with a later shell command; its diff is retained and direct fmt0 rerun used.
11. No unsupported closure (checked: original acceptance, native images, all
    compiler receipts and authoritative Beads); `.96` then `.95` close after
    their named checks. `.35/.87/.90` and broader criteria stay open.
12. No (checked: `.95/.96` acceptance and TODO diff); no original requirement
    lowered. The predeclared sequencing adjustment overlapped separate old and
    candidate compilation, preserving the same original negative before closure.
13–17. N/A: this block is solo, with no subagents, pane closes, accepted reports
    or inter-agent agreement used as evidence. Peer commits remain peer work.
18. No (checked: exact executable lists and all nine native summaries);
    actual81 unique/8repeat/0ignored counts are reported, excluding the nested
    CPU child. The mistaken ignored-case assumption is corrected explicitly.
Part4: `cass` reports `not_initialized`; no historical search/index or historical
honesty clearance is claimed.
19. Yes: the timeout typo wasted a cold build; initial source-copy selection was
    wrong and corrected before runtime evidence; one formatting status was masked;
    a read-only worker diagnostic had shell-quoting errors before corrected reads;
    warm-cache expectations and the CPU-ignore assumption were wrong; a multi-file
    Bash syntax check needed individual invocations; a broad journal read hit a
    malformed foreign record before the corrected own-only inventory. A generic
    Python sequencer was blocked by DCG; direct statically visible commands were
    used without overrides. Build/report overhead has been excessive.
    These are corrected on record, not justification for lowering any gate.
20. Strongest completed evidence: the exact old ELF reproduces the intended
    progress-loss assertion after an actual failed save. A skeptic can replay
    the retained named case. Final identified images now pass the unchanged
    checkpoint family, real spawn faults, normal controls and fresh solo replays.

Disposition: mistakes are corrected and disclosed here to the operator. RH-1,
RH-2, RH-13 and RH-15 countermeasures are unchanged original assertions/budgets,
separate proof classes, line-specific annotation review plus planted hazards,
and exact timeout-variable checks. SM-11 preserves first failures; RH-17 and
SM-12 preserve history and frozen-source identity. No history rewrite, deletion,
foreign-process kill, resource-floor override or new hosted workflow dispatch
occurred in this block. A prior erroneous workflow dispatch remains separately
recorded as queued, without execution credit. Final disposition: the two scoped
repairs close on their actual native and compiler evidence. Mistakes and first
failures remain disclosed; avoidable process overhead is not capability. Broader
qualification and newer peer changes retain their original gates.

## November 4 continuation: timestamp seeks and current dependency qualification

### Production change and preserved contracts

The shared header query now binds `created_ts >= i64::MIN` and orders by
`created_ts DESC, id DESC`, retaining the optional inclusive timestamp ceiling,
128-row limit, and boundary-timestamp offset. The pinned runtime engine recognizes
this shape as a reverse composite-index range seek. The initial page seeks the
project prefix; continuations seek the project and timestamp ceiling instead of
walking newer timestamps again.

This is a source-supported query-plan improvement, not a measured latency win.
The 4,096-unit speculation quota, increasing page charge, exact receipt fallback,
complete-timestamp cutoff, snapshots, cancellation checks, and archive publication
rules are unchanged. Long equal-timestamp runs can still exhaust speculation and
use the exact fallback; full archive reconciliation still requires scale evidence.
The timestamp equivalence applies to valid non-null integer timestamps, not every
possible malformed-database error outcome.

The new native regression checks every candidate page against independent Rust
ordering at `i64::MIN`, negative, zero, and `i64::MAX` timestamps, with 129-row
ties and foreign-project witnesses. It checks the generated `SeekLE`/`Prev` path,
two-field continuation keys, absence of a sorter/history-start instruction,
complete incumbent row-set equivalence, and query-only transaction cleanup.
Existing shared-inbox paging and concurrent peer-write hooks were updated to the
new SQL text so their assertions continue to exercise the intended operations.

The published `fsqlite-vdbe 0.4.13` crate was downloaded and verified against
Cargo.lock checksum
`1ebdb969e559db53f5ff2fed49fdfc4cd651a3fd50e6eefc3350ea46a708d125`.
Its VCS revision is `e4137af304002ac0b57a948c9e0005f9161475d7`; the
`src/codegen.rs` blob is `b9bd72adadd70ecd3a01846a518ad03467a03d08`.
The reverse composite-range implementation is therefore in the actual published
runtime engine, independently of the separate patched Beads 0.3.18 engine.

### Cancellation dependency and CI correction

Current main selects `sqlmodel-pool 0.5.1` and `sqlmodel-core` at immutable Git
revision `c996710674d92c00d23f30fe17ecc5d9644818c7`. The vendored 0.5.0 copy is
historical evidence and no longer provides the application's runtime pool.
Source review confirms the selected pool owns the pending counter through
`WaiterGuard`, factory reservations through `ActiveSlotGuard`, and connections
being validated/retired through `ActiveConnectionGuard`. Its outer acquisition
drops those owned operations before reporting cancellation or timeout. These
findings identify cleanup mechanisms, not a newly executed no-leak proof.

The CI pool step now runs the two existing native Agent Mail initializer
cancellation/reuse tests using the application's locked dependency graph and
records the selected pool's manifest/lock identity. The unchanged HTTP cancellation
test separately requires queued-request cleanup, dispatch release, metadata/body
reads after timeout, zero pending counts, reusable capacity, and unchanged receipts.
The workflow also selects the adjacent shared-inbox tests and requires the storage
large-fixture JSON witness to say `large_mailbox: true` and `messages: 25100`.
All existing large-workload, integrity, latency, production, and strict Clippy
gates remain. Historical vendor formatting is checked directly with rustfmt.

### Continuation validation

| Check | Observed result |
| --- | --- |
| Changed Rust files and historical pool rustfmt | Passed |
| `git diff --check` | Passed before executor loss |
| Workflow YAML and all 23 shell blocks | Parsed; shell syntax checks passed |
| Published runtime engine checksum/VCS and independent source reviews | Passed source/provenance checks |
| Focused native header-seek regression | No test result observed; executor disconnected during dependency compilation |
| Workspace `cargo fmt --check` | Attempt interrupted, exit 130; no completed formatting verdict |
| All-target/production workspace checks and strict Clippy | Not executed in this continuation before executor loss |
| 25,100-message archive and HTTP variants | No new execution result; production-scale qualification remains open |

The standard native attempt started at 22:31:09 UTC and was interrupted during
dependency acquisition (exit 130). A retry started at 22:35:42 UTC with Cargo's
`-Zgit=shallow-deps`, the pinned compiler, one build job, no incremental compilation,
and zero debug information. It downloaded the locked crates and reached dependency
compilation. Its terminal status and resource summary were lost when the executor
reported `exec-server transport disconnected`; the environment then became
unavailable. A configured 1,200-second build budget is not an observed timeout
result. No native case count, workload latency, Git-lock contention rate,
reconciliation throughput, or workload resource peak is claimed.

The current application-graph commands are:

```sh
cargo test --locked -p mcp-agent-mail-db --lib --no-default-features --features portable pool_impl::tests::cancelled_ -- --nocapture --test-threads=1
cargo test --locked -p mcp-agent-mail-db --lib --no-default-features --features portable gh374_ -- --nocapture --test-threads=1
cargo test --locked -p mcp-agent-mail-db --lib --no-default-features --features portable project_mailbox::tests::shared_inbox_ -- --nocapture --test-threads=1
```

The unchanged native HTTP cancellation and opt-in large-workload commands below
remain required. Workflow configuration is not evidence that those jobs executed.

### Exact continuation commit plan and publication

1. `perf(inbox): seek header pages by timestamp and id (#374)` — published directly
   to main as `145d4286ba453f2b8a3a22e400b082ad844d6cdf`, parent
   `1d12d78c26c87afecd08617f0c52d6a44127aa71`. Files:
   `crates/mcp-agent-mail-db/src/sync/inbox.rs` and
   `crates/mcp-agent-mail-db/src/project_mailbox.rs`. GitHub tree
   `cc63d2ed471ae0dea76d7a11caae3f1585021619` exactly matches the reviewed
   local tree.
2. `ci(gh374): qualify the selected pool and shared inbox paths` — this evidence
   update accompanies the CI correction. Files:
   `.github/workflows/issue374-native.yml`,
   `docs/evidence/gh374/VALIDATION.md`, and
   `docs/evidence/gh374/validation.json`. Publish with a checked non-forced
   fast-forward from the first commit; retain intervening work if main moves.

Neither commit claims issue closure or successful native production-scale
qualification. No pull request is part of this plan.

## Initial implementation changes

### 1. Cancel-safe pending-request ownership

The published `sqlmodel-pool 0.5.0` increments `waiter_count` before awaiting a
connection notification/timer and decrements it afterward. Dropping the acquire
future during the await bypasses the decrement. A private `WaiterGuard` now
owns precisely that increment. `AcquireAction::Wait` holds the guard, whose
destructor decrements the count on wake, timeout, cancellation, unwind, or hard
future drop. The two former manual decrements are removed.

No connection slot is reserved in this wait state. The guard does not return a
connection, change active/total capacity, alter acquisition deadlines, or touch
transactions, recovery admission, source-generation checks, archive publication,
or receipt state.

This ownership strategy already exists in SQLModel's unreleased
[`ee08466f00162ea21c0f9cf05da1f05f04d44758`](https://github.com/Dicklesworthstone/sqlmodel_rust/commit/ee08466f00162ea21c0f9cf05da1f05f04d44758).
That commit also contains a substantially broader acquisition-deadline rewrite.
The local change backports only waiter accounting to the exact published 0.5.0
source, rather than importing those unrelated changes.

The reproducible copy is in `vendor/sqlmodel-pool`. Its README records the
archive checksum, original VCS commit, upstream guard commit, and source delta.
The archive SHA-256 is
`d090572646f14c79363c6a78fe073afdb2213b77421b1815b31a6761e1f3ffa2`, matching the
original root lockfile. The published VCS commit is
`12d490b22ce3a82533591b5b39d2318b21da1f0f`; the original license blob is
`3d414da6ff83defe084707be6030866e8a7fb794`.

In the original backport, all published files other than `src/lib.rs` and the
extended README are byte-identical. The upstream license is included unchanged.
Its proposed root Cargo used a local `[patch.crates-io]` for this one crate.
Among that root lockfile's 868
packages, only the pool's registry source/checksum are removed; package versions
and dependency edges are unchanged. That proposed dependency selection was
superseded at integration as described above; it is not merged main's graph.

### 2. Regressions distinguish accounting from capacity

Three dependency tests use the actual pool and Asupersync notification code,
with an in-memory test connection implementation:

- Repeatedly drop 32 queued acquisitions while preserving one live waiter and
  the caller holding the only connection. Assert exact pending, active, and
  total counts.
- Independently of pending statistics, require a new notification after the
  holder returns, acquisition by the surviving waiter, and subsequent fresh
  acquisition without another factory call. This test can pass on the old
  implementation even when the accounting tests fail.
- Close the pool after dropping one queued caller. Require the other waiter
  to observe closure, pending to drain, and the foreign holder to remain owned
  until it is returned.

A control copy contains the unmodified published production code plus the same
three test bodies. Its intended red/green comparison must be executed before
claiming that the distinction has been established experimentally.

The existing native HTTP cancellation test now records dispatch drain and the
pending count while the single contended lease remains held. It then releases
that lease, requires successful body and metadata reads within the existing
30-second deadline, and checks one total/idle connection and zero active
connections. Finally it requires both pending snapshots to equal zero. This
retains the accounting gate while allowing an unfixed control to collect
post-timeout usability evidence before failing.

### 3. Large-mailbox evidence and an inactive-agent workload

The archive test now reports seeded/converged phases, periodic progress,
scanned and repaired work, transient deferrals, maximum retry debt, budget
yields, first-repair time, maximum batch time, and live Git-lock ownership
checks. It separates convergence time from post-convergence integrity and repeat
verification. Per-reader latency histograms are bounded and use milliseconds
rounded upward before reporting sample p50/p95/p99/max values.

The HTTP test requires at least 32 successful reads, giving at least eight
samples per large/small inbox and metadata/body combination across the entire
observation window. It separately reports latency distributions for requests
whose lifetimes overlap a repair-positive batch. At least one overlapping
large-inbox metadata request and one overlapping large-inbox body request are
required; the eight-sample minimum does not apply to the overlapping subset.
Any subset with zero samples reports null quantiles and maximum latency.
Per-batch work, overlap counts, maximum time across all batches, maximum
repair-positive batch time, and `full_convergence_checked: false` are also
reported. These are observed sample quantiles, not production percentile
estimates. HTTP child output is retained
until the child finishes; archive progress is streamed through inherited stderr.

The additional DB test seeds 25,100 GreenStone receipts followed by 1,024 newer
project messages to RedFox. It requires the exact five IDs
`25100, 25099, 25098, 25097, 25096`, with accurate, understated, and missing
count hints. It records the eight/one/one speculative header pages and the
99-page exact receipt fallback. Cancellation during that fallback must stop
further read queries, release the savepoint, preserve query-only mode, and permit a fresh
exact read on the same connection. This fixture uses the native in-memory
database engine, metadata-only reads, and tiny message bodies, without HTTP or
concurrent reconciliation. It qualifies fallback exactness and measured query
work; it does not establish production mailbox latency or resource consumption.

All original safety and timing assertions remain: complete recovery, durable
retry restoration/drain, live Git ownership, unrelated staging preservation,
exact archive bytes and BCC handling, unchanged source messages and receipts,
integrity checks, no duplicate repeat commits, and bounded inbox deadlines.

## Findings that require careful interpretation

### The leaked counter has a secondary operational consumer

The issue comment described pending accounting as statistics-only. Source
inspection identifies an additional consumer:
[`HealthSignals::from_snapshot`](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/blob/4b8ddf30f34a73cc2553bc95db8f906dd95328a6/crates/mcp-agent-mail-core/src/backpressure.rs)
suppresses cumulative acquisition p95 only when both active and pending counts
are zero. A stale pending sample can keep historical latency in health
classification. If health is Red and `BACKPRESSURE_SHEDDING_ENABLED=true`,
eligible lower-priority tools can be rejected. Per-project inbox reads are not
in the shedable set.

This is a source-supported possible secondary effect, not a reproduced
production shedding incident. It does not imply that the pending counter
controls pool admission: capacity is based on total connections; actual waiting
is owned separately by `Notify`. Existing guards also own slots/resources
during pending creation, validation, and retirement. The native liveness tests
are needed to distinguish these mechanisms experimentally.

The backport does not import SQLModel #43's broader deadline handling for
stalled/uncooperative factories or checkout pings. Hard-drop ownership and
independent driver deadlines are separate guarantees.

### Full missing-archive repair has quadratic identity work

For each missing message bundle,
[`identity::prepare_committed`](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/blob/4b8ddf30f34a73cc2553bc95db8f906dd95328a6/crates/mcp-agent-mail-storage/src/recovery/message_reconcile/identity.rs)
checks existing committed canonical identities. The
[working-tree collision check](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/blob/4b8ddf30f34a73cc2553bc95db8f906dd95328a6/crates/mcp-agent-mail-storage/src/recovery/message_reconcile.rs)
then scans existing canonical frontmatter under publication/project locks.

An idealized empty-archive model with no retries and one successful publication
per message implies `25,100 * 25,099 / 2 = 314,992,450` prior-message visits in
each full identity scan. This model excludes directory entries, staged-index
inspection, and repeated attempts. Git identity verification materializes and
hashes full blobs, so the 3 KiB bodies alone imply about 968 GB of logical blob
bytes processed in that model. These are source-derived operation/byte counts,
not measured elapsed time, physical disk I/O, or memory use. A mostly complete
production archive follows different paths; already committed bundles with all
message copies present can avoid full history scans.

No unsafe cache shortcut was added. Existing archive epochs are observational,
not publication authority; lock generations are process-local. A cached OID
summary can also conceal an object that was removed or corrupted after warming.
Any optimization needs negative controls for malformed/duplicate old-shard
evidence, same-length edits with preserved mtime, symlink replacements,
unavailable/corrupt Git objects, namespace drift, budgets, and cancellation.

### Bounded speculation is not a constant-work inbox guarantee

The existing dense recent-message fixtures find five recipient messages in the
first header page. A recently inactive agent can exhaust speculation and then
read all of its receipts to preserve exactness. The added workload explicitly
measures this fallback; it must not be described as constant-work merely because
the returned limit is five. Synchronous SQL checkpoints also do not preempt a
single long-running engine query.

## Native fixture realism limits

Even a successful opt-in run has a defined scope:

- Storage proves full convergence and direct query-only inbox responsiveness;
  HTTP transport is supplied by the separate server test.
- The HTTP test reads a 25,100-message DB during 8–64 repair batches (at most
  256 repaired messages). It does not prove late-stage HTTP latency or full
  convergence.
- Both tests hold a real `git update-index --index-info` writer's index lock
  continuously. Intermittent release and reacquisition against a fully
  populated 25,100-message index remain untested.
- The storage writer updates an agent heartbeat and an archive note; it does
  not add sustained new-message arrivals.
- Cursor restart occurs inside the same process, not through killing and
  restarting the whole daemon.
- The storage test allows 45 minutes for convergence and another 45 for repeat
  verification. Test RSS includes full-mailbox integrity witnesses and other
  fixture overhead.

## Validation environment and current results

Host: Ubuntu 24.04.3 LTS, Linux 6.18.57, x86-64; cgroup CPU quota 8 CPUs, memory maximum
8,589,934,592 bytes, no swap. Pinned compiler:
`rustc 1.100.0-nightly (908501772 2026-08-30)`, toolchain
`nightly-2026-08-31`. Native build attempts use one Cargo build job, incremental
compilation disabled, and dev/test debuginfo disabled.

The workspace initially lacked Rust. The pinned compiler was installed. A
normal Cargo fetch spent several minutes processing a 440 MiB Git-history
download; the exact immutable FrankenSQLite revision was then fetched with a
shallow Git CLI fetch. The broader graph subsequently reached Rust dependency
compilation. The shared cgroup reached its memory ceiling and recorded OOM
events; these observations must not be attributed to Agent Mail runtime use.

The first offline pool control attempt exited 101 after 293.522 seconds waiting
on the package cache and encountering an unavailable `aes 0.9.3` download.
An isolated control build then expired after 600.953 seconds while compiling
dependencies, before any pool test ran. A warmed offline retry also expired,
after 1,200.467 seconds, during dependency compilation. Maximum single-process
child RSS was 37,120, 384,016, and 301,876 KiB respectively. These are build
observations, not mailbox workload consumption. The machine-readable companion
records each attempt's command, exit, wall time, CPU, and RSS scope.

A subsequent bounded candidate `cargo check --locked --offline --lib --tests`
was started before the executor became unavailable. After recovery, its log was
empty and no metrics/exit receipt existed. Its outcome is unknown; it supplies
neither a successful type check nor a test result. A final control attempt after
environment recovery expired after 300.190 seconds during dependency compilation,
with 327,764 KiB maximum single-process child RSS, 42.966 seconds user CPU, and
14.483 seconds system CPU. It also executed zero tests. None of these attempts
reached a pool regression test body. No further native run is pending in this
qualification record. Aggregate process-tree RSS is not claimed: the executor's
`/proc` PID-namespace view did not support a reliable sampler.

| Check | Current result |
| --- | --- |
| Workspace `cargo fmt --check` | Passed |
| Standalone vendored crate formatting | Passed |
| `git diff --check` | Passed |
| Cargo manifests/locks and workflow YAML parse | Passed |
| Locked, offline workspace `cargo metadata --no-deps` | Passed; manifest/workspace inspection only |
| Exact archive checksum, unchanged source files, license blob | Passed |
| Root dependency delta: same 868 packages/versions/edges | Passed |
| Pool unpatched accounting/liveness controls | Not executed; four attempts ended before test execution |
| Patched pool tests | Not executed |
| Candidate standalone `cargo check --lib --tests` | Unknown after executor loss; no exit receipt or compiler output |
| Native Agent Mail DB/storage/HTTP regressions | Not executed in this workspace |
| Opt-in 25,100-message archive and HTTP tests | Not executed; production-scale verdict open |
| Workspace type checks and strict Clippy | Not completed |

## Reproduction and acceptance commands

Use the repository-pinned toolchain and preserve `--locked`. Run commands
sequentially on a host with sufficient memory. Capture compiler identity, exit
status, the emitted JSON, wall time, CPU, and peak RSS separately from builds.
Do not accept zero selected tests as qualification.

```sh
cargo test --locked -p mcp-agent-mail-db --lib --no-default-features --features portable pool_impl::tests::cancelled_ -- --nocapture --test-threads=1

cargo test --locked -p mcp-agent-mail-db --lib --no-default-features --features portable gh374_ -- --nocapture --test-threads=1
cargo test --locked -p mcp-agent-mail-db --lib --no-default-features --features portable sync::inbox::tests:: -- --nocapture --test-threads=1
cargo test --locked -p mcp-agent-mail-db --lib --no-default-features --features portable project_mailbox::tests::shared_inbox_ -- --nocapture --test-threads=1

cargo test --locked -p mcp-agent-mail-storage --lib --no-default-features --features portable recovery::message_reconcile::database:: -- --nocapture --test-threads=1
cargo test --locked -p mcp-agent-mail-storage --lib --no-default-features --features portable recovery::message_reconcile::cancellable_tests:: -- --nocapture --test-threads=1
cargo test --locked -p mcp-agent-mail-storage --lib --no-default-features --features portable recovery::reservation_reconcile::repair_lock::fence::tests:: -- --nocapture --test-threads=1
cargo test --locked -p mcp-agent-mail-storage --lib --no-default-features --features portable archive_index_sync_ -- --nocapture --test-threads=1

AM_TEST_GH374_LARGE_MAILBOX=1 cargo test --locked -p mcp-agent-mail-storage --test it --no-default-features --features portable archive::gh374_reconciliation_converges_with_live_git_writer_and_inbox_readers -- --exact --nocapture --test-threads=1

cargo test --locked -p mcp-agent-mail-server --lib --no-default-features --features portable tests::issue374_http:: -- --nocapture --test-threads=1
AM_TEST_GH374_LARGE_MAILBOX=1 cargo test --locked -p mcp-agent-mail-server --lib --no-default-features --features portable tests::issue374_http::gh374_http_inbox_stays_responsive_during_reconciliation -- --exact --nocapture --test-threads=1

cargo check --locked --workspace --all-targets
cargo check --locked --workspace --lib --bins
cargo clippy --locked --workspace --all-targets -- -D warnings
```

## Initial authorized main-branch commit plan (completed)

The user's earlier follow-up authorized these two commits directly on main,
without a pull request. They were published as
`a79b72f5676cab8b5ea2f624b84e4f99632801da` and
`aaf8c0e47f70fab1d9e44327909b18df621a964c`, with their trees checked against
the reviewed local files. The historical plan below records their scope;
subsequent dependency selection is described in the continuation above.
Publication does not change validation results or remove qualification gaps.

1. **`fix(pool): reclaim cancelled acquisition waiters (#374)`**

   Files: `.gitignore`, `Cargo.toml`, `Cargo.lock`,
   `.github/workflows/issue374-native.yml`, and these nine vendor files:
   `vendor/sqlmodel-pool/.cargo_vcs_info.json`,
   `vendor/sqlmodel-pool/Cargo.lock`, `vendor/sqlmodel-pool/Cargo.toml`,
   `vendor/sqlmodel-pool/Cargo.toml.orig`, `vendor/sqlmodel-pool/LICENSE`,
   `vendor/sqlmodel-pool/README.md`, `vendor/sqlmodel-pool/src/lib.rs`,
   `vendor/sqlmodel-pool/src/replica.rs`, and
   `vendor/sqlmodel-pool/src/sharding.rs`.

   Includes the narrow ownership fix, three dependency regressions, provenance,
   original license, unchanged dependency versions, and standalone native test
   and formatting gates. No release version bump or broad SQLModel upgrade.

2. **`test(gh374): strengthen large-mailbox recovery qualification`**

   Files: `crates/mcp-agent-mail-db/src/sync/inbox.rs`,
   `crates/mcp-agent-mail-server/src/tests/issue374_http.rs`,
   `crates/mcp-agent-mail-storage/tests/archive.rs`,
   `docs/evidence/gh374/VALIDATION.md`, and
   `docs/evidence/gh374/validation.json`.

   Includes inactive-inbox exact fallback/cancellation, HTTP post-timeout
   usability and latency distributions, archive progress/latency observability,
   and the exact executed-versus-unverified validation record. Does not declare
   #374 resolved or change recovery-safety contracts.
