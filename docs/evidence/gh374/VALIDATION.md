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

## November 5 fresh-eyes continuation: checkpoint and worker-start failures

`br-kp1in.95` and `.96` remain in progress. The compiler candidate is frozen
in a separate checkout at `5ec128ceb55259eab9ced0183485b0c635f02298`
with committed runtime code and three documentation-only overlays. Commit
`971c1d2e` already contains the six reviewed files, including bounded journal-claim
release waiting, the exact constructor-spawn panic reason and the progress-clock
correction. Main and the frozen candidate contained identical reviewed code at
preparation. Formatting passes; native/compiler qualification remains pending.
Native positive execution and final
compiler qualification are pending; the earlier `.94` receipts do not qualify
this candidate. Raw attempts are retained under
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
real message archives and an observed live first worker. They are integrated,
not yet executed. Global lazy-pool restartability and loaded shutdown are not
claimed.

Raw four-file UBS first reported4critical. Two actual worker panic boundaries
are repaired. The remaining deliberate constructor refusal now has rollback;
the permission test executes only a retained current-executable descriptor,
then drops uid/gid. Its original root branch passed1/0/0 on the identified old
ELF. Both narrowly annotated lines retain their behavior. A separate planted
untrusted executable and unconditional panic still produce exactly2critical
in the same annotated countercontrol file. Final five-file UBS exits0 with
0critical,9,854warning,3,410info and no failed modules; this is not warning-free
or release clearance. Formatting exits0. All first failures are retained.

### Filled anti-ceremony worksheets (solo; provisional until qualification)

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
(1) The checkpoint and worker-start repairs are committed but still unqualified.
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
   fresh solo rechecks will not be called independent review.
9. Yes: any happy-path summary without first failures would hide material limits;
   raw Clippy101, UBS4critical, admission refusals, monitor137 and client retrieval
   failures are retained separately. No final positive qualification yet.
10. No (checked: retained stdout/stderr logs and recovery receipts); evidence
    stderr was not discarded. An initial formatting command masked its status
    with a later shell command; its diff is retained and direct fmt0 rerun used.
11. No (checked: authoritative Beads); no new closure; `.35/.87/.90` stay open.
12. No (checked: `.95/.96` acceptance and TODO diff); no original requirement
    lowered. The predeclared sequencing adjustment overlapped separate old and
    candidate compilation, preserving the same original negative before closure.
13–17. N/A: this block is solo, with no subagents, pane closes, accepted reports
    or inter-agent agreement used as evidence. Peer commits remain peer work.
18. No (checked: selectors and predeclared family/three-fault acceptance);
    actual selected/ignored counts must be reported, not inferred from exit0.
Part4: `cass` reports `not_initialized`; no historical search/index or historical
honesty clearance is claimed.
19. Yes: the timeout typo wasted a cold build; initial source-copy selection was
    wrong and corrected before runtime evidence; one formatting status was masked;
    a read-only worker diagnostic had shell-quoting errors before corrected reads;
    warm-cache expectations were wrong; build/report overhead has been excessive.
    These are corrected on record, not justification for lowering any gate.
20. Strongest completed evidence: the exact old ELF reproduces the intended
    progress-loss assertion after an actual failed save. A skeptic can replay
    the retained named case. Positive repaired execution is still required.

Disposition: mistakes are corrected and disclosed here to the operator. RH-1,
RH-2, RH-13 and RH-15 countermeasures are unchanged original assertions/budgets,
separate proof classes, line-specific annotation review plus planted hazards,
and exact timeout-variable checks. SM-11 preserves first failures; RH-17 and
SM-12 preserve history and frozen-source identity. No history rewrite, deletion,
foreign-process kill, resource-floor override or new hosted workflow dispatch
occurred in this block. A prior erroneous workflow dispatch remains separately
recorded as queued, without execution credit. Final disposition must be updated
after the pending native/compiler runs, with no unsupported done declaration.

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
