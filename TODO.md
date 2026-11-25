# MCP Agent Mail Rust Port — TODO (Granular)

## Qualification completed (2026-11-25, `br-u9bhg`)

- [x] Close the original Bead acceptance on canonical main
  `0e0d5334da9be1e417b3916e17bb122ca5968baf`, after the three required compiler
  gates, formatting, complete normal static coverage and all named native
  regressions finish. Bead closed at 14:21:50 UTC with cited evidence.
- [x] Verify all four compiler source maps (including native preparation):
  3,957 identical hashes, sizes and executable modes. Exact receipt-command
  digests match the dispatch commands including `CARGO_INCREMENTAL=0` and
  Clippy's `--keep-going`; logged worker argv preserves every strict gate
  argument. All actual exits are zero, terminal acknowledged and released.
- [x] Verify all nine non-compilation runtime jobs: original 240-second
  process-group deadline, serial runner, prepared executable hashes checked
  before/after, complete returned results and released ownership.

| Component | Distinct targeted native tests passed |
|-----------|---------------------------------------|
| DB | 228 |
| Storage | 15 |
| Tools | 12 |
| Server | 2 |
| CLI | 20 |
| **Total** | **277** |

There are 283 outer test invocations including the separate six-case ACK run;
those repeated cases and nested child summaries add no distinct-test credit.
Both real database drivers pass all three explicit-publication-retry schedules.
All six ACK SQL/archive fixtures are retained. Normal static coverage remains
complete on 55 Rust files / 35 reports with every finding retained.

Full commands, source receipts, exact IDs/wrappers, executable hashes, test
names/counts and coverage audit:
`/data/tmp/am-mossytern-20261123/main-0e0d-native-final-report.json`, SHA-256
`ef98228bca6fc8caaa4388f5b4847609ce7794b364253a18fa19e8eba5857159`.
Coverage is the specified GH#404 regression set; broader workspace runtime,
release, live-mailbox and performance qualification remain separate. Earlier
checklists retain historical failures, unexecuted probes and independent work.
The additional bespoke-resolver investigation below remains guard-blocked.

## Fresh review resumed (2026-11-25 03:13 UTC, `br-u9bhg`)

- [x] Refresh Agent Mail identity/inbox, Beads/BV and CASS; preserve the dirty
  shared checkout. Fast-forward only the clean review checkout to `5922131d`.
  Governing README/AGENTS content is unchanged from the full earlier reading.
- [x] Reread all five repaired queue assertions and the complete ACK helper
  against the actual publication-retry contract. No further observed defect.
- [x] Reproduce a further bug in my E2E helper: a rejected forced rebuild
  returns success with an existing executable when called in a conditional.
  The real remote/local policy refuses with 64, but the helper returns 0.
  The new regression fails all ten forced-build cases on the unchanged
  helper; its ten prepared-artifact controls and the other 15 scenarios pass.
  Retain `fresh-nov25-force-build-baseline.log` and
  `fresh-nov25-forced-build-red.log` with their private fixtures.
- [x] Propagate the actual build failure at both helper build call sites.
  Preserve prepared-artifact behavior and existing fallback policy.
- [x] Complete the full helper suite: 16/16 scenarios pass, including ten
  rejected forced rebuilds and ten accepted prepared-artifact controls.
  Existing 46 routing, ten artifact and ten initialization controls pass.
  Bash syntax and fresh diff checks pass. Normal UBS completed both Bash
  files: zero critical, 14 retained warnings on unchanged counters explicitly
  guarded with `|| true`, no failed modules. Report
  `fresh-nov25-forced-build-ubs.json`, SHA-256
  `886b8c4a4c4aaf49c40cf04f6350e1d7bf8f8b9d4b0130cf5b76c4c7bc94a67d`.
  Green log: `fresh-nov25-forced-build-green.log`; retained fixtures
  `/data/tmp/am-e2e-lib-self-test.20261125T032225Z.3845055`.
  These controls execute no compiler or stale executable; native proof is
  separate. Publish the helper repair after final scoped review/checks below.
- [x] Trace real callers and reproduce stdout contamination after a successful
  forced build: scripts capture the helper output as one executable path, but
  the helper also prints build logs there. Preserve the first direct control
  and the failing 16/17 full suite in `fresh-nov25-build-stdout-baseline.log`
  and `fresh-nov25-build-stdout-red.log`. The I/O control uses an explicitly
  recording Cargo function and never compiles or executes an application.
- [x] Send build diagnostics to stderr at both checked call sites and document
  the path-only stdout contract; correct two stale caller comments. Full suite
  now passes 17/17, retaining all previous controls. Bash syntax and diff checks
  pass. Normal UBS completes all four changed Bash files: zero critical,
  15 retained preexisting warnings (14 guarded counters and one clock capture).
  Report `fresh-nov25-build-stdout-ubs.json`, SHA-256
  `8289069646f09c0b6635368af09f6ff539424a69e06477c12a6d649c00ba7cb0`.
  Green fixtures: `/data/tmp/am-e2e-lib-self-test.20261125T035220Z.190273`.
- [x] Publish the first helper correction as draft PR #412, `a9c5baf7`.
  Add the stdout correction to that same review; workspace/native
  qualifications remain separate, pending work below.
- [x] Check the live stale-backup finding: full integrity failures persist;
  the existing `br-lqz87` fix already improves its diagnosis. No live repair,
  daemon replacement or unrelated backup-policy change is attempted here.
- [x] Build the current review candidate's native DB tests, then execute the
  corrected six retry cases and original DB recovery/cursor/admission suites.
  The warm native worker `vmi1149989` lacks ordinary headroom. A fresh normal
  census on `vmi1153651` admits native work: 137,080,143,872 bytes free versus
  96,636,764,160 required. Reuse the existing compiler checkout/cache there.
  Preserve the `00b35dde` baseline and cache; its known unsupported automatic
  durability expectation is supplementary investigation, not a replacement
  for the original acceptance. No original acceptance is removed or weakened.
- [x] Preserve the first `71ffa099` compiler-only preparation failure:
  `fresh-nov25-v1-db-compile.log`, build `30097323753222311`, wrapper
  `rchw-0ad00334-da21-489e-89c7-7a4c8fa05954`. It timed out at 3,600 seconds
  before a DB test binary or assertion. Recovery CLI zero acknowledges actual
  command exit 120 with source/pair ownership released; log
  `fresh-nov25-v1-db-recovery.log`. Completed dependencies remain in the same
  worker cache. The actual compiler/lock identity is retained in
  `fresh-nov25-v1-db-live-compiler-tree.json` (SHA-256
  `b2a87b89a1de979df89202c9271e0a76b1c91e8b0910dd9c935104f324bae64b`).
  After release, ordinary admission still fits on the healthy worker:
  123,228,721,152 bytes free versus 107,374,182,400 required. Preserve every
  live reservation and all original profiles, dependencies and runtime limits.
- [x] Observe the integrated formatter failure in `mailbox_verdict.rs` and
  storage `lib.rs`; apply only its reported line wrapping manually. Preserve
  `fresh-nov25-current-fmt-red.log`; workspace formatting now passes in
  `fresh-nov25-current-fmt-green.log`. No Rust behavior or assertion changed.
- [x] Refresh the existing normal Rust static gate for the current candidate:
  39 of the original 50 source hashes still match; 11 need fresh scans.
  Retain completed pre-format storage output as historical, rescan that file,
  and include the newly touched `mailbox_verdict.rs` (51 total Rust paths).
  No rules, findings or modules may be suppressed; static work is not native
  proof. The source transition changes no manifest, lockfile, toolchain or
  Cargo/Nextest settings, and removes no checked-out file.
  All 51 source/report/count bindings verify on `292a2f43`: 39 unchanged
  paths reuse their exact reports; 11 replacements and one added path use
  fresh normal scans. The final report retains 55 critical, 61,697 warning
  and 21,717 scoped informational findings, plus 128 unscoped diagnostics;
  no failed modules. Fresh critical flags concern unchanged fixture/cache/time
  source lines, verified against `71ffa099`. This is completed static coverage,
  not a clean scanner verdict or native proof. Report
  `fresh-nov25-ubs-final-report.json`, SHA-256
  `c5db7ac9d50c831b87b23588b37d9c3a21badee08311bcede6a2a2ed2e91996b`.
- [x] Correct the runner/cache assumption. RCH uses a separate disposable
  source/pool for external `cargo nextest`; it did not reuse the prepared
  direct-Cargo dependencies. Stop only my own cold request via wrapper-scoped
  cancellation and recovery. Build `30097323753222348`, wrapper
  `rchw-d6599047-e4b4-4e69-ba0a-becc74d6280d`, has terminal acknowledgement
  and released source/pair ownership. Recovery reports delivery exit 120
  versus the daemon's earlier cancellation exit 130; preserve both without
  calling either a native pass. No assertions executed. Logs:
  `fresh-nov25-v2-native-ack-wrapper-cancel.log` and
  `fresh-nov25-v2-native-ack-recovery.log`.
- [x] Prepare all five required component library test packages together
  through direct Cargo in the retained RCH source/pool. Then keep that same
  package/feature graph for focused runtime batches with a 240-second RCH
  process-group deadline, bounding every test at least as strictly as the
  default four-minute Nextest limit. No test case, profile or assertion is
  removed. The first request, build `30097323753222357`, wrapper
  `rchw-470a2842-666e-4676-a771-bd7146102443`, hit an SSH reset before confirmed
  execution; recover ownership before retrying. Preserve
  `fresh-nov25-v3-native-compile.log` and its recovery log.
  Recovery acknowledges failed exit 1 and source/pair release; no test ran.
  Later peer reservations make ordinary admission on `vmi1153651` negative:
  121,901,449,216 bytes free versus 150,323,855,360 required for native.
- [x] Run the six corrected canonical/runtime ACK controls with actual test
  counts and retained SQL/archive fixtures; zero-test runs cannot qualify them.
  Exact-main prepared DB ELF SHA-256
  `1576d7d176540e7ff8305595d35ed7f7be308448446f21dee1da5ea28199a5a0`:
  6/6 pass in 14.16s, zero failed/ignored, build `30097323753222566`, wrapper
  `rchw-3e34f2b6-3183-4335-88a0-1f99dc111c8f`. Runtime envelope confirms
  remote exit zero; executable hashes match before/after, terminal acknowledged
  and source/pair released. All six SQL/archive fixture directories returned in
  `target/native-regression-results/ack-six-v1`. Runtime is bound to the native
  compiler receipt, not a separate runtime source-content receipt.
- [x] Run all 32 `recovery_state::` tests: zero failed/ignored, 18.86s,
  build `30097323753222571`, wrapper
  `rchw-80be73d0-f17d-4ee2-b4ae-9d0d21f159ee`, same DB ELF/hash binding.
  Both hash checks, native counts, result retrieval and terminal release verify.
- [x] Run all 15 `reconstruct_state::` tests, seven `delivery_event_snapshot`
  cases, the pruned-cursor boundary and both named promotion-admission cases:
  25/25 pass, zero failed/ignored; build `30097323753222573`, wrapper
  `rchw-5572febd-17ef-4c98-916b-6ef4ed19ff08`. Same verified executable,
  complete result retrieval and terminal release. Original 240-second deadline.
- [x] Run DB `recovery_state`, `reconstruct_state`, `reconstruct`, GH404 cursor
  and sync-admission cases, all seven `delivery_event_snapshot` cases, and
  `durable_cursor_schema_refuses_legacy_fallback_after_all_events_are_pruned`.
  All 228 distinct required DB tests pass on `0e0d5334` (234 invocations
  including the independent six-case ACK run). `reconstruct::` passes 171/171
  in 19.38s, build `30097323753222575`, wrapper
  `rchw-7252b19e-74f3-499f-a071-1f27e65da382`; hashes, delivery and terminal
  release verify. No failed/ignored cases or altered runtime thresholds.
- [x] Correct the private result checker after its first-summary search
  selected a one-case child summary instead of the real 171-case outer result.
  Validate the final nonempty line, retaining the exact positive count and
  zero failed/ignored requirements. The real nested log and plain valid output
  are accepted; failed/zero/wrong-count/ignored/trailing-error controls remain
  rejected. These are parser controls, not extra native assertion evidence.
- [x] Run storage GH404 flush/envelope/exited-producer/backlog cases and
  `deferred_message_identity_rechecks_a_foreign_historical_month`.
- [x] Include the existing storage envelope/order and foreign-generation
  controls explicitly: `prepared_envelope_uses_the_shared_archive_key_order`,
  `foreign_generation_and_conflicting_identity_remain_byte_identical`, and
  `rejects_foreign_generations_roots_replacements_and_invalid_debt`.
  The GH404 flush case itself verifies a real Git clone and recovered witness.
- [x] Run tools failed-publication auto-read, consuming-inbox admission and
  committed-ACK replay cases, plus all three modified release-intent cases.
- [x] Run both modified server ACK/explicit-ID retention cases.
- [x] Run CLI missing-project-metadata repair, missing/malformed-project JSON
  detector/fixer cases, archive-count status, and the remote-required helper
  artifact case. Keep their actual dependencies and original assertions.
- [x] Verify publication: all three repair commits through `292a2f43` pushed.
  PR #412 was merged externally at 04:51:22 UTC as `4b938d1b`; I had left
  it draft pending qualification. Merge is not completion evidence. The
  original Bead acceptance and all pending native/compiler gates remain open.
- [x] Execute original storage/tools/CLI acceptance and all five modified
  queue cases. Preserve actual failures; fix only demonstrated defects.
- [x] Qualify any subsequent integrated source separately, publish actual
  outcomes and keep the Bead open until its complete acceptance passes.
  At the first main transition, the isolated checkout held `4b938d1b`; its
  tracked tree equalled reviewed `292a2f43`. The required production
  `cargo check --locked --workspace --lib --bins -j1` was submitted through
  strict RCH on `hz4` with its ordinary 10 GiB growth/10 GiB floor admission.
  Preserve `fresh-nov25-v4-production.log`. All-targets and strict Clippy were
  then pending independently; the completed exact-main results appear above.
- [x] Complete the required production-only graph on main `4b938d1b`:
  `cargo check --locked --workspace --lib --bins -j1` passed through strict
  RCH on `hz4`, build `30097323753222359`, wrapper
  `rchw-4e52d4e3-e601-43bd-83f5-9f031ec82ac9`. Cargo finished in 21m46s;
  command exit zero, terminal acknowledged, source/pair ownership released.
  All 3,957 source hashes, sizes and executable modes verify against the
  frozen checkout; receipt root
  `fefc7d21899fe73afcbd27cac4a7a15826500c24d02caddfe321f5e3a49e0e59`.
  The existing third-party `proc-macro-error2` future-incompatibility warning
  remains retained. Actual live Cargo/rustc executable hashes match the pinned
  nightly; `fresh-nov25-v4-cargo-identity.json`, SHA-256
  `c6cc6e1201129b41a26af98d73746efab96329f48b5896ec0270c1dbcd5e7257`.
  The earlier identity probe matched no process because it missed RCH's real
  Cargo executable name; retain its empty observation without using it as
  compiler attestation. No native or release claim follows from this check.
- [x] Complete the separate all-targets graph on the same frozen main source:
  `cargo check --locked --workspace --all-targets -j1`, strict RCH `hz4`;
  retain `fresh-nov25-v5-all-targets.log`. Reuse the production worker's pool
  without changing profiles, features, dependencies or admission policy.
  Build `30097323753222377`, wrapper
  `rchw-b3565573-c01e-49bc-86f7-1ef5b19fa9b0`, passed in Cargo 8m18s,
  actual exit zero, terminal acknowledged and source/pair released. All 3,957
  source hashes/sizes/executable modes verify and match the production map
  exactly; receipt root
  `26e08a03400deaafc079566f35e4fe6646eabfb65477cbda978cc43a55a3443f`.
  The existing third-party future-incompatibility warning remains retained.
- [x] Complete `cargo clippy --locked --workspace --all-targets -j1 -- -D warnings`
  on that same candidate, preserving and repairing actual diagnostics.
  Strict RCH `hz4` submitted after the all-targets lease was acknowledged and
  released; retain `fresh-nov25-v6-clippy.log`.
  That gate failed with actual 101 on `4b938d1b`, build
  `30097323753222384`, wrapper `rchw-45f0f3f4-473a-4dbc-bc51-ad3ec1814551`:
  `clippy::redundant_pub_crate` at `pool.rs:18596`. All 3,957 source hashes,
  sizes and executable modes match; receipt root
  `a8e8b141c5e0a0227e229f98688baa692f719ef4c38431787efd081510e04698`.
  Terminal acknowledged with source/pair released. Trace the private
  `pool_impl` module, public registry glob and its internal retention caller;
  change only this helper to `pub(super)`, preserving the parent DB crate
  boundary and function body. No suppression, dependency or assertion change.
- [x] Qualify that visibility correction with the same strict Clippy gate,
  then repeat both required compiler graphs on the repaired source. Refresh
  only the pool file's static binding (`fresh-nov25-rust-14-pool-visibility`).
  No new test is needed for this visibility-only edit; original native cases
  remain mandatory. Retain every failed gate and all earlier static reports.
  The `pub(super)` attempt `d348267a` also failed the same lint: Clippy resolves
  that restriction to the parent crate here. Preserve build
  `30097323753222386`, wrapper `rchw-f033b6ae-3e99-4d32-80a2-8275cb922789`,
  actual 101, terminal acknowledged and source/pair released, all 3,957 inputs
  verified; receipt root
  `b8d080376779a06737cfc18c8016d6e6c2cfbcff5d60f51339a3d962c07caf39`.
  Log: `fresh-nov25-v7-clippy.log`. Its completed pool scan/51-file aggregate
  are historical coverage only, not qualification of the next candidate.
  Follow the module's existing public recovery-helper convention instead:
  make the unchanged suffix iterator public with its purpose documented.
  The registry will expose this read-only helper alongside the existing
  recovery helpers. No runtime implementation, assertion or lint policy changes.
  Formatting passes; the replacement normal scan is
  `fresh-nov25-rust-15-pool-public.json`. Rerun the original strict command.
  Candidate `05463de2` passes the helper's DB lint but fails later on three
  `implicit_clone` diagnostics in `idempotency_tool_acceptance.rs:809/816/818`.
  Preserve `fresh-nov25-v8-clippy.log`, build `30097323753222388`, wrapper
  `rchw-5e848950-8a1a-416f-964f-877b4c442b8c`, actual 101, acknowledged and
  released; all 3,957 inputs verify, receipt root
  `e419c1f00487791b23806943a486dcc91846f4a3e76d2467781e09f342d5d39b`.
  Replace only these known `Vec` copies with explicit `clone()` calls.
  Preserve attachment reordering, duplicate requests, conflict refusal and
  all SQL/archive/inbox/source-byte assertions. No lint suppression or new test.
  Formatting passes; scan the newly touched acceptance file with normal rules
  (`fresh-nov25-rust-16-idempotency-clones`). Original native acceptance stays
  mandatory; these mechanical test-input copies add no new runtime behavior.
- [x] Qualify the repaired `434c798d` compiler source through strict RCH `hz4`:
  Clippy with `--keep-going` and unchanged `-D warnings` passed (Cargo 3m35s),
  build `30097323753222392`, wrapper
  `rchw-ff3baa60-13ef-4f75-b7a6-fc92f4d697ff`, receipt root
  `c58cddd37ce14ca9a1de516ff0b85bfd6ce63aa2d9e4f8fe6ddc33472eccbf76`.
  The production-only graph passed separately (1m14s), build
  `30097323753222394`, wrapper `rchw-0dd7ef9c-df69-4b8d-b580-2665fcbae9a7`,
  receipt `42c5429477098b80755978cd7c63a5e0291e02e88f8143382c6bb81fe80e0b61`.
  The all-targets graph passed separately (3m56s), build
  `30097323753222395`, wrapper `rchw-29708474-b1f7-473b-af96-38e84c46b1d9`,
  receipt `6a0fbc5f26728ef90810dc315ad459a3257049ce728bfdb9529afd48c6a20668`.
  Every command exited zero, is acknowledged with ownership released, and
  verifies the same 3,957 file hashes/sizes/executable modes. Retain all failed
  attempts and the existing third-party future-incompatibility warning.
- [x] Complete the final normal static refresh on `434c798d`: 52 Rust paths,
  50 exact reused bindings plus the fresh pool replacement and acceptance-file
  addition, all matching the accepted Clippy source map. All 33 bound reports
  are complete with no failed modules. Retain 55 critical, 61,898 warning,
  21,790 scoped informational and 132 unscoped diagnostics without suppression.
  New pool/acceptance scans have zero critical flags. Report
  `fresh-nov25-final-ubs-report.json`, SHA-256
  `f6aac1edd9609b8798fab00f419956e61bd6aacb7b33811e9fce2cd7fb182d33`.
  Earlier `pub(super)` coverage remains historical, not current evidence.
- [x] Preserve the unsupported `00b35dde` baseline as a separate clean detached
  checkout at `/data/projects/am-mossytern-00b35dde-preserved-20261125`.
  Its former native request `30097323753222033` is acknowledged with actual
  120 and source/pair released. After verifying no removed files and identical
  manifests/lock/toolchain/Cargo/Nextest settings, fast-forward only my clean
  compiler working copy `/data/projects/am-mossytern-native-20261124` to
  `434c798d`. Reuse its existing `vmi1149989` direct-Cargo source/pool `76f74c28`;
  no cache copy, namespace override, peer cleanup or policy change.
- [ ] Finish the five-library compiler-only native preparation on that worker.
  A fresh normal census admits native work: 125,234,085,888 bytes free versus
  99,857,989,632 required, including every live reservation and protected floor.
  Submit `cargo test --locked` for DB/storage/tools/server/CLI with
  `--lib --no-run -j1`, strict RCH/source receipt and the original default
  graph, 3,600-second compiler-only envelope. Retain
  `fresh-nov25-v12-native-compile.log`. Then execute the corrected six and
  original acceptance above under the unchanged 240-second runtime envelopes.
  The `vmi1153651` cache remains preserved; new peer reservations there again
  prevent ordinary native admission. No native assertion has passed yet.
- [x] Publish the compiler corrections as draft PR #413 at `434c798d` after
  all three compiler gates passed. The first create attempt raced the pending
  branch push and failed without creating a PR; wait for push exit zero and
  retry successfully. Original native acceptance remains pending; no merge or
  Bead closure requested. The pending local TODO/Bead evidence update does
  not change the tested Rust source.
- [x] Observe the actual native compiler process on `vmi1149989`: pinned
  Cargo/rustc executable hashes match, default `opt-level=1`, `debuginfo=2`
  and debug assertions remain enabled. Report
  `fresh-nov25-v12-compiler-identity.json`, SHA-256
  `98e5187d94b83441118b8fac1ee140120bdc39996df8451e943a5e5d53d83e2c`.
  Active compiler-only build `30097323753222397`, wrapper
  `rchw-1b00eb90-171b-4e4b-8f84-f9c1705831aa`, retains frozen `434c798d`.
- [ ] At the current command boundary, integrate main's separate `126f8544`
  archive process-lock change once, qualify its source and run the four
  existing `archive_process_lock_cache` cases, including the previously
  unexecuted real filesystem-generation case. Its changed code and tests have
  been reread; no further concrete defect found. Retain the original GH404
  suites and do not close peer Bead `br-navqh`. Current `434c798d` receipts
  do not qualify that later main source or demonstrate those four tests.
- [x] Preserve the compiler-only preparation deadline failure: build
  `30097323753222397` timed out at 3,600 seconds in the tensor CPU dependency,
  after progressing through Asupersync and other real default dependencies.
  RCH killed and verified the remote group. Supported recovery acknowledges
  delivery exit 120 versus the daemon's earlier 130; retain both, source/pair
  released. Logs `fresh-nov25-v12-native-{compile,recovery}.log`. No native
  assertion executed and no source error was reported. Retain the dependency
  outputs; resume with the same five-package graph and original envelope.
- [ ] Integrate fixed main checkpoint `733ff1c7` at this released-command
  boundary. PR #413 was merged externally as `82dd2922`; later upstream
  messaging/product changes are included in that checkpoint. Compiler results
  above still qualify `434c798d` only. Manifests, lockfile, toolchain and Cargo/
  Nextest settings are unchanged and no tracked file is removed. Requalify the
  integrated source, refresh affected static bindings and complete the original
  native acceptance plus the four existing archive-cache cases. Keep this
  checkpoint stable through the next validation run rather than chasing heads.
- [x] Prepare integrated source `2b050817` containing checkpoint `733ff1c7`:
  its Rust sources and build inputs match main's checkpoint exactly; only own
  TODO/Bead evidence differs. All three worktrees advance without removed files;
  original manifests, lockfile, toolchain and Cargo/Nextest settings remain.
  Formatting passes in `fixed-main-v1-fmt.log`.
- [x] Refresh affected normal static coverage on `2b050817`: 53 Rust paths,
  50 verified unchanged bindings, two fresh replacements and the added products
  scan. All 34 bound reports are complete, retaining 55 critical, 62,544 warning,
  21,905 scoped informational and 136 unscoped diagnostics without suppression.
  Fresh critical flags are unchanged synthetic tokens in messaging tests. Report
  `fixed-main-v1-ubs-final-report.json`, SHA-256
  `8e4f45599545f1790d74abe543c33d152f6cc3047cb7a0e7e0f28597265ed29c`.
  This is static completion, not a clean verdict or compiler/native result.
- [ ] Complete the three integrated-source compiler gates. Ordinary `hz4`
  headroom is negative; the subsequent `vmi1153651` request ended with 103
  before admission, no remote build ID or source hold. Wrapper
  `rchw-4aac9dfb-0e8a-4b46-ab0a-ee188c20d5b5` is terminal acknowledged;
  preserve its empty `fixed-main-v1-clippy.log` and local admission journal.
  The unchanged strict Clippy command is admitted on `vmi1149989`, build
  `30097323753222438`, wrapper `rchw-a20ee77c-99c2-44bb-8362-773663e523be`,
  frozen source `2b050817`, log `fixed-main-v2-clippy.log`. Its ordinary census
  fits static work; no limit, bypass, quarantine, namespace or profile changed.
  The separate production/all-target graphs and all original native cases
  remain mandatory. The `434c798d` passes do not qualify this new source.

- [x] Preserve the integrated-source Clippy failure on `2b050817`: actual
  101, build `30097323753222438`, wrapper
  `rchw-a20ee77c-99c2-44bb-8362-773663e523be`, terminal acknowledged and
  source/pair released. All 3,957 hashes, sizes and executable modes verify;
  receipt `f28a5e74ddeebc11e33aa32d4313352640ea265d4dc0ec7f97353df79ec6a516`.
  Log `fixed-main-v2-clippy.log` reports two E0599 errors: the archive-lock
  cache refactor left two shutdown/admission tests using the old map type.
- [x] Read the cache implementation and both complete contention controls;
  search every `ARCHIVE_LOCK_MAP` use. Under narrow leases, update the two
  stale `.write()` calls to `.locks.write()` in active-reservation and message
  retention tests. Preserve their real registry latch, project mutex owner,
  shutdown/admission observation, resume and SQL/archive assertions. Formatting
  and diff checks pass; no production behavior or assertion changes.
- [x] Requalify repaired source `3803dd85` through all three required compiler
  gates on the retained worker pool with unchanged flags/policies. Full strict
  Clippy was admitted as build `30097323753222469`, wrapper
  `rchw-858b45e7-df21-4761-b270-518541bc8dfc`; source stayed frozen.
- [x] All three repaired-source compiler gates pass on `3803dd85`:
  strict Clippy (8m19s), build `30097323753222469`, wrapper
  `rchw-858b45e7-df21-4761-b270-518541bc8dfc`, receipt
  `512a240d5b89aff7b68417afe5d0cdfb3f5787607f2ef20d96bc49cf53cb3363`;
  production-only check (15m24s), build `30097323753222474`, wrapper
  `rchw-67684f3e-76b4-4104-93f2-7090a31f046c`, receipt
  `bb6270cc83db34e99944757089afa2a4ff0eaa678a7dbe6c6ca2e9aaf4b55c37`;
  all-target check (11m17s), build `30097323753222477`, wrapper
  `rchw-872ccc09-b591-4759-a3db-5f8897a6d6d8`, receipt
  `3b5d363ff1af36a3a230ed3ba41d5e41e76093026e534fdd5ace10d55498157f`.
  Every command exits zero, is terminal acknowledged with source/pair released,
  and verifies the same 3,957 file hashes, sizes and executable modes. Logs
  `fixed-main-v3-clippy.log`, `fixed-main-v4-production.log` and
  `fixed-main-v5-all-targets.log` retain the third-party future-incompatibility
  warning. This qualifies the repaired candidate, not native execution or any
  later main commit. The original exact-main acceptance remains mandatory.
- [ ] Finish the same five-library native preparation on `vmi1149989`:
  build `30097323753222482`, wrapper
  `rchw-0b3b1dda-16aa-4615-bff2-1a3d33c0479b`, frozen `3803dd85`, log
  `fixed-main-v6-native-compile.log`. Ordinary admission fits again after peer
  reservations finish: 109,564,952,576 bytes free versus 64,424,509,440 required.
  Retain the existing source/pool and default graph; compiler-only timeout stays
  3,600 seconds. All native assertions remain unexecuted; runtime batches will
  keep the original 240-second process-group deadline and five-package graph.
- [x] Observe the actual current-source native compiler at 09:42 UTC:
  FrankenSQLite core emits `dep-info,metadata,link` with `opt-level=1`,
  `debuginfo=2` and debug assertions enabled. Actual Cargo/rustc executable
  hashes match the earlier pinned nightly identities and the original lock
  hash remains `98b26497fb52c79582497c745654e6a7433a7a8b9178e5965650c1e37e751938`.
  Report `fixed-main-v6-compiler-identity.json`, SHA-256
  `31e3f1ee5bfa0db5912b726c13a657855a61dce59ddcb785ca451757e114eed6`.
  Tensor CPU/API and other completed dependencies have advanced into the real
  engine core; this is compiler progress, not native assertion evidence.
- [x] Preserve native preparation v6's 3,600-second timeout at 09:59:42 UTC:
  tensor CPU/API and FrankenSQLite core compiled, and the log reached Agent
  Mail core before later dependencies. Core completion before timeout was not
  independently established. No source diagnostic or
  native assertion was reported. RCH verified its process group dead and
  refused local fallback. Supported recovery confirms delivery exit 120,
  terminal acknowledgement and source/pair release; recovery CLI exits zero.
  Retain `fixed-main-v6-native-{compile,recovery}.log`. The clean compiler
  checkout remains at `3803dd85`; preserve all completed cache outputs and
  resume the same five-package/default graph only after ordinary admission.
- [x] Follow admitted native preparation v7 through its terminal timeout,
  build `30097323753222507`, wrapper
  `rchw-8ca78609-a11d-4ba0-a37d-5dc4f3417f8e`, still frozen at `3803dd85` on
  the original worker/root/pool. Ordinary census: 125,733,163,008 bytes free
  versus 64,424,509,440 required. Original command, graph, profiles and timeout
  are unchanged; log `fixed-main-v7-native-compile.log`. Heavy engine/tensor
  outputs are reused. Core has no build script; the initially guessed path was
  absent. Remote source mtimes reset during materialization, but no cache defect
  is claimed without checking Cargo's real fingerprints and warm execution.
  Cargo's actual current core fingerprint uses `mtime` freshness in the newer
  `debug/build/<package>/<variant>/fingerprint` layout. Check a warm native
  request before concluding whether materialization forces recompilation;
  source timestamps and cache/tool configuration are left unchanged.
- [x] After native preparation succeeds and is acknowledged/released, capture
  the actual test-executable paths and hashes from that completed build. Use
  RCH's documented `--job` mode for already-built test shards, with both
  supported build/test timeouts set to 240 seconds, package working directory
  and private fixture/result directories. No compiler runs inside runtime
  jobs. Keep the original five-package compile graph, profiles, dependencies
  and every assertion. Bind runtime hashes to the compiler's source receipt;
  `--job` conflicts with source-content-receipt mode, so do not claim a runtime
  source-content receipt. Require six actual ACK passes and retain their real
  fixtures; then execute all original, repaired-shutdown and cache cases.
- [x] Preserve v7's one-hour timeout at 11:02:42 UTC during CLI unit-test
  code generation. No assertion ran; group termination was verified. Supported
  recovery confirms delivery exit 120, terminal acknowledgement and source/pair
  release. Retain `fixed-main-v7-native-{compile,recovery}.log`; do not count
  recovery CLI zero as compiler/test success.
- [x] Land the two-line compile repair whose source passed all three compiler
  gates, then pin canonical main and repeat those gates on that exact source.
  Native execution and original Bead acceptance remain pending throughout.
- [x] Land PR #414 normally at main `0e0d5334da9be1e417b3916e17bb122ca5968baf`,
  using the expected reviewed head `a57ca105`. The head's additional commit is
  tracking metadata only. The connected GitHub API corrected the PR body after
  the CLI's deprecated Projects field prevented its edit. No check/admin
  bypass or branch-deletion request was used. Native acceptance remains open.
- [x] Verify canonical main against the published review through the GitHub
  comparison API: no file differences from `a57ca105`; only TODO and two Bead
  metadata files differ from compiler-qualified `3803dd85`. An optional generic
  Python Git wrapper was blocked for unverified argument spreading; it was not
  executed or retried. The structured read-only API supplied the comparison.
  Fast-forward only the two clean private worktrees, import JSONL, and verify
  both main and the legacy mirror point to `0e0d5334`. Shared checkout preserved.
- [x] Main formatting passes. Rebind all 55 identical source/report hashes to
  main without rescanning unchanged code: 35 complete normal reports with all
  findings retained. Report `main-0e0d-ubs-final-report.json`, SHA-256
  `2e69230d97fd253530b46b0f945a779ac6e50abc71b1a7f65021d114d922249b`.
  This is non-partial static coverage, not a clean verdict or native proof.
- [x] Complete exact-main Clippy, production-only check and all-target check.
  Keep original compiler flags and full source-content receipts. The earlier
  `3803dd85` passes remain scoped to that candidate; exact-main results follow.
- [x] Complete exact-main strict Clippy on `0e0d5334`: build
  `30097323753222531`, wrapper `rchw-1a01085d-68ad-4710-945f-538101b62783`,
  actual exit zero after Cargo 10m05s. All 3,957 source hashes, sizes and
  executable modes verify; receipt root
  `19984950e394d044de86c79bc908aec5f84ffbafd96c71f4f7e9b11bdf55a70b`.
  Terminal acknowledged and source/pair ownership released. The production
  check is separately running on `hz4`; all-targets and native remain pending.
- [x] Complete the exact-main production-only workspace check on `hz4`:
  `cargo check --locked --workspace --lib --bins -j1`, build
  `30097323753222535`, wrapper `rchw-9fd5ce01-c1e3-48fb-ae37-ea7d0b6ca133`,
  actual exit zero after Cargo 20m40s. All 3,957 source hashes, sizes and modes
  verify and match the Clippy map; receipt root
  `aec88c4baf0cf8ba5ba4598d4239d43617aa4fe52614ff6ef0c10b102f333ec9`.
  Terminal acknowledged with source/pair released. Actual Cargo/rustc hashes
  match the pinned nightly in `main-0e0d-hz4-compiler-identity-v3.json`.
- [x] Complete `main-0e0d-v3-all-targets.log`, separately submitted after the
  production request released: Cargo 8m16s, actual exit zero, build
  `30097323753222543`, wrapper `rchw-8db2578e-c100-4bde-a318-337bfa7f715b`.
  All 3,957 source hashes, sizes and modes verify against `0e0d5334` and match
  both prior compiler maps; receipt root
  `4f9c87de34801a62e7cefc10f2a5c2ab9237fc253dda4c41420cf57812a4a81d`.
  Terminal acknowledged with source/pair released; no native claim follows.
- [x] Allow up to 7,200 seconds for compiler-only native preparation on main,
  using `--lib --no-run -j1` with the same five packages/default graph. Repeated
  one-hour attempts reached CLI test code generation without completing the
  workspace test executables. This revises the chosen compiler preparation
  budget; actual test-execution jobs keep their original 240-second deadline.
  Retain ordinary disk/slot admission, original profiles/dependencies/features
  and every required assertion and probe.
- [x] Complete `main-0e0d-v4-native-compile.log` on exact main `0e0d5334`,
  build `30097323753222545`, wrapper
  `rchw-a4793b80-8875-426b-895f-0b6e6e92fba7`, submitted at 12:13 UTC on the
  original native worker/root/pool. Fresh normal
  census admits native preparation: 171,870,392,320 bytes free versus
  64,424,509,440 required including all live growth reservations and the floor.
  This prepares all five library test packages without executing assertions;
  the source checkout remains frozen until terminal acknowledgement/release.
  Actual CLI test compilation was observed with the pinned Cargo/rustc/lock
  hashes and original opt-level 1, debug info 2 and debug assertions. Identity
  report `main-0e0d-v4-native-compiler-identity.json`, SHA-256
  `42fd3b0f533c587be1a4ba3edcd41c87aadbc5ab388a9d733564e22de6909e63`.
  Remote compilation finished with exit zero at 13:33:42 UTC after 80m05s,
  producing all five test executables. All 3,957 source hashes, sizes and modes
  verify and match the three compiler gate maps; receipt root
  `117964d9bdd48328848c5b9e907a0ad5ac344ddb36e64832120d512c6a79b7b5`.
  RCH reports artifact delivery of 21 files / 2,058,977,324 transferred bytes,
  with a selected-file budget of 14,178,520,940 bytes. Preserve both figures;
  they describe different transfer measurements.
  Client exit zero, terminal acknowledged and source/pair released by 13:44:39.
  Native assertions remain separate, pending work below.
- [x] Push `3803dd85` and create draft PR #414 after all three compiler gates
  pass; verify its head, base `main`, open state and draft flag. Its Rust/build
  diff against main checkpoint `733ff1c7` is exactly two one-line test repairs.
  Keep native and exact-main acceptance pending in the review. Notify the peer
  of the single admitted compiler and release code leases 4179/4180; retain the
  TODO lease. The shared dirty checkout and peer Beads remain untouched.
- [x] Reread the common Bash builder, resolver and server-launcher call paths.
  The server launcher explicitly checks build status and selects the last
  output line; no additional observed defect there. Three bespoke suite
  resolvers prefer installed/historical candidates before the common helper.
- [ ] Investigate that possible remote-required fallback gap in
  `test_search_quality_integration.sh`, `test_tools_search.sh` and
  `test_product_isolation.sh`, preserving their explicit override contracts.
  No file in this review is changed and no dynamic failure is claimed. The
  proposed real-definition comparison was rejected before execution by DCG
  rule `heredoc.python:subprocess_run.argv_unverified` because its Python
  subprocess argument count could not be statically verified. Retain
  `fresh-resolver-dcg-explain.log`; do not reshape/retry the blocked probe or
  weaken the guard. Continue original native acceptance independently.
- [x] Both changed test files complete the normal scan with zero critical,
  1,242 warning and 131 informational diagnostics, no failed modules and no
  suppressions. Report `fixed-main-v3-ubs-lock-tests.json`, SHA-256
  `77b54b45c1496ac57bd48df31a9562c95c1574bead7a9a2a7c59b9ec9ce2842b`.
  This is static evidence only; native behavior remains unexecuted.
- [x] Verify the full updated static scope: these two files add to all 53
  unchanged bindings, giving 55 Rust paths and 35 complete bound reports.
  Retain 55 critical, 63,786 warning, 22,032 scoped informational and 140
  unscoped diagnostics without suppression. Report
  `fixed-main-v3-ubs-final-report.json`, SHA-256
  `275e3844eaa13994dc89e0057a8b137710ccb73d489bacbe115cddd8a6cc40c5`.
  This completes source coverage, not a clean verdict or native qualification.
- [x] Execute both repaired native controls:
  `shutdown_during_project_contention_retains_the_candidate_for_resume` and
  `retention_admission_shutdown_during_project_wait_preserves_unconsumed_work`,
  in addition to the original acceptance and four archive-cache cases. Their
  compile failure was observed before the two-line repair. Both now pass on
  `0e0d5334` in 2.75s / 2.71s, and all four cache cases pass in 0.02s.
  Build `30097323753222577`, wrapper
  `rchw-a703eb18-933e-4b9d-8057-3bbc8f390572`: hashes, remote zero exit,
  result retrieval, terminal acknowledgement and ownership release verify.

## Resumed fresh review (2026-11-24 15:12 UTC, `br-u9bhg`)

- [x] Verify the former hz3 request is now terminal and acknowledged, exit 120,
  with its source pair released. Retain the prior failed recovery receipts.
- [x] Refresh registration/inbox, preserve shared-checkout peer edits, review
  Beads triage and the CASS hit. Continue the existing recovery qualification
  before opening another lane. Bead status and acceptance remain unchanged.
- [x] Check actual Git ancestry: shared local main is an older ancestor, not
  new removals. Fetch remote main and integrate its two search-scope commits
  once as `00b35dde`; governing docs and pinned dependency graph are unchanged.
- [x] Reread the published helper/stamp/type-inference diff and pass formatting.
- [x] Rerun the existing helper suite: 15/15 scenarios, 46/46 routing controls,
  ten artifact controls and ten initialize-metadata controls passed. Retain
  `/data/tmp/am-e2e-lib-self-test.20261124T152313Z.1303962` and the full log;
  recording commands execute no compiler or stale binary.
- [x] Complete both integrated search-file scans and verify the prior 48
  source/report/count bindings. All 50 Rust paths are covered, retaining
  55 critical, 60,992 warning and 21,515 informational source findings without
  suppression. Report: `resumed-review-v1-ubs-final-report.json`.
- [x] Observe the actual in-flight Rust/Cargo executable hashes on the worker;
  they match the earlier pinned nightly identities. Preserve observation time
  and process IDs in `resumed-review-v1-compiler-identity.json`.
- [x] Find and correct an unsupported assumption in my ACK probes. The recovery
  runbook and `is_publication_error` explicitly require retry after an archive
  publication error, even if SQL committed. Prepare an unconditional publication
  retry after contention, preserving ACK 999 and first read 444. Add a check that
  the contact publisher completes its own revision before that retry, so the
  retry cannot mask its failure. The same six engine/schedule cases remain;
  their new filter is `test(concurrent_ack_retry)`. No production change.
  Preserve original baseline commit `00b35dde`; the compiler checkout now holds
  the corrected candidate for its required gates.
  Its original filter is `test(reconciliation_retains_concurrent_ack)`; any
  failure of that unsupported automatic-durability assertion is not by itself
  a production race. The corrected candidate is still uncompiled/unexecuted.
  Corrected source SHA-256:
  `68c1dadfcf793ef075e7b072790d009275bbac2cb4fffbc35b578716220b9f01`.
  Formatting passes after manually collapsing two renamed helper calls; the
  initial formatter failure is retained. Fresh final-file normal UBS completed
  with zero critical, 499 warning and 124 informational findings. The two added
  assertion warnings are intentional test checks and remain unsuppressed.
  Report: `ack-retry-contract-v2-ubs.summary.json`, SHA-256
  `2f0d372160245379633fc2c5b9d5b60fd74cfc574c9a10a4b6cdbfe6f7eee1a5`.
  The earlier pre-format v1 scan is retained as superseded, not current evidence.
- [x] Complete strict Clippy on the repaired candidate; preserve every actual
  failure and repair concrete diagnostics.
  Baseline `00b35dde` Clippy completed with exit 101 on vmi1153651, build
  `30097323753221956`, wrapper `rchw-3bb6af03-6284-488b-b83a-58c5ade916f4`.
  Three `assert_is_empty` errors in reservation tests require values on failure.
  Replace those assertions with equality to a typed empty array, preserving
  the empty-queue condition and improving diagnostics without suppression.
  All 3,952 source entries match; receipt root
  `9d3bada7d82e2dcbda434d40f1d6cf272e65dbe68dd96cc06ffffe32be8bbc73`.
  Wrapper is terminal and acknowledged. The assertion repair passes formatting
  and normal UBS: zero critical, 1,465 warning and 584 informational findings.
  Current reservation source SHA-256
  `9c2b091704697d8bff733ba256a78599ef6eb2c68b58e40c5745bc3d7106a125`.
  Preserve the failed gate and rerun it on both corrections together.
  The corrected `a4860459` retry also completed with exit 101: build
  `30097323753221990`, wrapper `rchw-33f5d91c-fd00-47c3-b613-19ac584d4c72`.
  The next reported failures are two server retention assertions with the same
  denied lint. Preserve their empty-queue conditions using typed
  empty arrays and keep all surrounding identity, terminal-state and timestamp
  checks. The failed run's 3,952 source entries match, with receipt root
  `dca772815787d57763559a25fd3c15983f5ffd6a625836d66a8b9202a08194e9`;
  ownership is terminal and acknowledged. Both repaired files pass formatting
  and normal UBS: zero critical, 286 warning and 67 scoped informational
  findings, plus four retained unscoped informational diagnostics. Report
  `server-empty-assertions-v1-ubs.summary.json`, SHA-256
  `46e29adac0de2b39f51626dba9acb1a659afdbe662af2fef5d9c6f35eab6d3d0`.
  The focused queued-intent assertion search has no remaining matches.
  Rerun the same strict workspace gate after this second concrete repair.
  That unchanged retry passed on `a6472714`: build `30097323753222008`, wrapper
  `rchw-a6e2d95b-db50-4bf7-81c0-6d7855ffe4b9`, Cargo 10m24s, exit zero,
  terminal and acknowledged. All 3,952 source hashes, sizes and executable modes
  match; receipt root
  `6e0854c0103d2d71a8969922fea3f3f685e54b621282f796c7d9df28321edac5`.
  Preserve the third-party future-incompatibility notice for
  `proc-macro-error2 v2.0.1`. This is compiler/lint evidence, not native execution.
- [x] Integrate the externally merged main `71ffa099` into both isolated
  checkouts. Its Rust sources, lockfile, toolchain and Cargo/Nextest settings
  match `a6472714` exactly; three CI files are added and the readiness
  qualification document is updated.
  The shared checkout and its peer changes remain untouched.
- [x] Complete the production compiler graph on merged main
  `71ffa099`. The preceding `a6472714` production request timed out in queue,
  exit 103, before worker allocation or Cargo; no active owned lease remains.
  The first merged-main request on vmi1153651 also timed out in queue, exit
  103 before allocation/Cargo. A fresh positive ordinary static census on hz4
  admitted the replacement, build `30097323753222020`, wrapper
  `rchw-2affdbd5-b313-42cb-9942-d04ba7d657ac`. It passed: Cargo 23m59s, exit zero,
  terminal and acknowledged. All 3,955 source hashes, lengths and executable
  modes match; receipt root
  `ae5f8d8ced81060dfad1287f82e43df0cda1e952192cd39d8d175aab3ee2347a`.
  The difference from the 3,952-entry Clippy receipt is precisely three added
  CI files and the changed readiness document, with no removed entry.
  Log: `merged-main-v2-production.log`; retain both preceding queue failures
  and the third-party future-incompatibility notice.
  The actual in-flight Cargo and rustc were observed at 17:21 UTC on hz4:
  nightly 2026-08-31, rustc 1.100.0-nightly / LLVM 23.1.0, matching the earlier
  executable hashes, with main `71ffa099` in rustc's build-source environment.
  `merged-main-v2-live-compiler-tree.json`, SHA-256
  `eba8baf2ad266c2108bdd54084d5de53b738fced007a8fd02b523c7c3f0bef6f`.
  Earlier empty/Cargo-only observations are retained and do not prove a rustc
  identity. These observations establish no compiler pass or native result.
- [x] Complete strict Clippy on merged main `71ffa099`: vmi1153651 build
  `30097323753222065`, wrapper `rchw-764cf573-f43e-4f0a-9044-8bc0e58110fa`,
  Cargo 6.92s, exit zero, terminal and acknowledged. All 3,955 file hashes,
  lengths and executable modes match; receipt root
  `deb75704455b18567037d24b75db3b65c620e7bc6e3cb614ccf6f522f9c667c9`.
  Log: `merged-main-v1-clippy.log`. Retain the third-party future notice.
- [x] Complete the all-targets compiler graph on merged main `71ffa099`:
  vmi1153651 build `30097323753222067`, wrapper
  `rchw-d7d6fb2c-4680-4c02-b77f-54aa8f40c6ab`, Cargo 15m58s, exit zero,
  terminal and acknowledged. All 3,955 hashes, lengths and executable modes
  match; receipt root
  `08cc8b794522aa1167b3777cae940d9b7eff1475a91c9bbaeae52ed9de5775c4`.
  Log: `merged-main-v1-all-targets.log`. The three main compiler receipt file
  maps are identical. Native acceptance remains independently open.
- [x] Rebind the existing normal-rule static gate to merged main: 50 Rust
  files, 22 complete raw reports, 55 critical / 60,994 warning / 21,515 scoped
  informational findings. Four updated source/report/count bindings replace
  stale ones; all 46 reused bindings and every report hash/count are verified.
  No findings or rules are suppressed. Final report
  `resumed-review-v2-ubs-final-report.json`, SHA-256
  `cb5d9ca8e32d06bf24553adb8465a08b4761b70d9755c6425b0090d1bbcc8ad8`.
  This is static self-review, without native or release qualification.
- [x] Trace the caller-visible consuming-read publication error: it already
  returns affected message IDs, `read_may_have_committed` and explicit retry
  guidance. Preserve the existing wiring; its native test remains in the
  original tools acceptance below.
- [ ] Obtain ordinary native admission and capture the old six-case baseline,
  then execute the corrected six retry controls with retained fixtures. Require
  actual native results plus the documented contract before any production
  repair. These probes do not justify imposing new writer exclusion. If a
  separate valid failure requires it, any guard must outlive the archive lock
  and preserve caller transactions and generation initialization.
  Native compiler-only request `30097323753221988`, wrapper
  `rchw-ffa526a1-a585-4b3e-b039-b23fea25ec8a`, was refused before Cargo at
  ordinary disk admission, exit 120, terminal and acknowledged. The preceding
  census had 129,221,058,560 bytes free versus 128,849,018,880 required; admission
  later saw 127,688,105,984 free. No budget or policy changed. Native assertions
  remain zero. Preserve original baseline commit `00b35dde`; finish corrected
  static gates while awaiting a native window, and execute both six-case forms
  when ordinary capacity permits. Runtime test limits remain 1,800 seconds;
  the 3,600-second setting applies only to literal compiler-only --no-run.
  RCH's supported clean-baseline mode was refused before Cargo by its namespace
  guard: CLI exit 103, lease exit 1, build `30097323753222032`, wrapper
  `rchw-38a858b7-3ea9-4d9e-9b52-c2f2933a997a`; terminal, acknowledged and all
  source ownership released. No guard or budget was changed. Preserve that
  failure. The safe replacement is a dedicated read-only baseline worktree,
  `/data/projects/am-mossytern-native-20261124`, pinned to original `00b35dde`.
  Its normal RCH compiler-only request is actually compiling on vmi1149989:
  build `30097323753222033`, wrapper
  `rchw-5e59dc16-451c-4ef6-8f48-d88c9caa09f3`, log
  `retained-base-v3-db-compile.log`. Freeze that worktree until terminal.
  Nextest 0.9.143 (60fa45f638ffc3f35e74afa65737f45fcd32db2a) is present there.
  That compiler-only request hit its actual 3,600-second deadline before
  producing a test binary. CLI exit 1 retained ownership; exact supported
  recovery returned zero and acknowledged actual command exit 120 with all
  sources/pair/tree released. Preserve both `retained-base-v3-db-compile.log`
  and `retained-base-v3-db-recovery.log`. Recovery is not a compile pass.
  Actual baseline compiler identities are retained in
  `native-baseline-v3-live-compiler-tree.json`, SHA-256
  `3ed6390d4d20e468a35887001181397c4468acc24d254ee5d8afa8b7db49236f`.
  The native worktree remains at `00b35dde`, with completed dependency work
  retained in its worker cache. RCH temporarily quarantined vmi1149989 after
  timeout; normal health checks later reported it healthy. No quarantine,
  guard, profile, dependency, budget or timeout policy was overridden.
  Use a fresh ordinary admission check before its cached compiler retry.
  At 19:07 UTC the healthy warm worker had 163,974,504,448 bytes free versus
  195,421,011,968 required, including all live reservations. Its ledger has
  no reservation attributable to the terminated native request. Do not edit
  the ledger, discount live groups or change the admission policy.
  Native runs retain 1,800 seconds and existing Nextest limits; Nextest is also
  verified on the original Cargo process's PATH.
  No native assertions have executed. After old-case execution, this worktree
  can advance to `71ffa099`, preserving the worker cache without deleting
  checked-out files; verify the source transition before advancing it.
- [ ] Execute original DB/storage/tools/CLI regression acceptance plus the
  modified helper and foreign-writer tests under unchanged runtime limits.
  Include the three modified reservation cases:
  `replay_release_intent_does_not_recreate_a_missing_project`,
  `replay_release_intent_rejects_ambiguous_terminal_selectors`, and
  `replay_release_intent_finishes_old_siblings_after_partial_apply`.
  Also include the two repaired server cases:
  `ack_replay_refuses_post_intent_messages_and_replacement_agents` and
  `explicit_id_replay_rejects_newer_foreign_and_terminal_selectors`.
  The DB filter must also include `delivery_event_snapshot` cases and
  `durable_cursor_schema_refuses_legacy_fallback_after_all_events_are_pruned`;
  they do not all carry a `gh404` prefix. Keep the named sync-admission case
  `gh404_sync_receipts_refuse_promotion_before_opening_the_live_database`.
  Include storage's modified
  `deferred_message_identity_rechecks_a_foreign_historical_month`, and CLI's
  `e2e_remote_required_helpers_reject_missing_artifacts_without_local_build`
  plus its original project-metadata preservation cases.
- [x] Recheck final diffs/static bindings, publish scoped evidence and update
  the existing Bead according to actual execution, without replacing acceptance.
  GitHub reports PR #410 merged externally at 15:51 UTC as `00b35dde` in
  merge commit `ab252a31`, then PR #411 at 16:43 UTC as `a6472714` in
  `71ffa099`. Publication is not native or final gate evidence.
  Final formatting and diff checks pass; the full 50-file static binding
  verifier passes again. Beads has no dependency cycles. Only this TODO and
  the claimed Bead's evidence comments are included in the handoff commit.
  Remote main advanced to `ace116df` during qualification, changing 13 paths,
  including production DB/storage/server code. The compiler and static
  results above qualify pinned `71ffa099`; they do not qualify that newer
  source. Preserve the native baseline and warm cache rather than replacing
  them with another unexecuted candidate. Native acceptance remains open.

## ACK reconciliation schedule review (`br-u9bhg`)

- [x] Trace both real read-snapshot releases through reconciliation and
  selected-contact publication to the completed revision seal. The original
  competing-ACK controls exercised only the open reconciliation snapshot.
- [x] Add six independent engine/schedule controls: canonical and runtime
  databases, each during the read, after reconciliation, and before sealing.
  Forward actual SQL before attempting the competing commit; retain the
  committed SQL ACK versus recovered-artifact comparison and mandatory retry
  after legitimate writer exclusion. Production code is unchanged.
- [x] Retain each private database/archive fixture and print its path. These
  controls have not executed; no concurrency defect is yet demonstrated.
- [x] Pass formatting and diff checks. Complete the normal single-file scan:
  zero critical, 497 warnings, 124 informational findings, no module failures.
  Review edited-line findings: test assertions, unwraps and fixture-path
  diagnostics; retain them without suppression. Candidate source SHA-256
  `40540f9617ffae7dc308b5a96b2833eb74b25bad8a3f5afe114f39a528880bb1`;
  report `ack-schedule-gaps-v1-ubs.summary.json` SHA-256
  `5e5bc4034aac4618e0362c4c647113c0c1bb4c9c77847052b92eb8edf17b3dd3`.
- [x] Recover the previous exact E9 DB compiler request. It actually timed out
  after 1,800 seconds: Cargo test --no-run uses RCH's test-command cap, so the
  build-timeout setting alone did not supply the intended compiler envelope.
  Wrapper `rchw-3938324a-4768-42f9-8dc5-d502ec21076c` is terminal and
  acknowledged, delivery exit 120. Recovery CLI exit 0 is not a compile pass.
- [x] Verify the supported per-invocation `RCH_TEST_TIMEOUT_SEC=3600` setting
  for literal compiler-only --no-run. Actual native test executions retain
  1,800 seconds, existing Nextest per-test limits and normal 30+10 GiB admission.
- [ ] Compile this six-case candidate remotely, then execute all six controls
  with nonzero counts. Preserve failures before any production repair.
- [ ] Fix any reproduced defect and rerun the decisive controls plus original
  DB/storage/tools/CLI acceptance. Respect caller-owned transactions and error
  recovery if database admission needs changing.
- [ ] Qualify the final source with both workspace compiler graphs, strict
  Clippy, formatting and updated scanner bindings. E9's passes below qualify
  that earlier source only; these new test changes are still uncompiled.
  The first strict workspace Clippy request on `51bbbb5b` exited 101 at
  `reservation_reconcile.rs:737`: the explicit closure return tuple triggered
  denied `type_complexity`. All 3,952 receipt entries match the frozen source;
  receipt root `55cfb8476b3e25c13369d5dacbaa90f22ca29200fd5b3b85d7cfd9e3e1a63083`.
  Preserve this failure. Remove only the redundant annotation, letting Rust
  infer the same return type, and rerun the unchanged strict gate.
  Annotation-only repair is commit `b8974575`. Formatting passes. The full
  48-file static scope is rebound with two fresh reports and 46 verified
  unchanged source/report/count bindings: 55 critical, 59,335 warning and
  21,183 informational source findings, retained without suppression.
  `ack-schedule-v2-ubs-final-report.json` SHA-256
  `c6498e3a377533feb83ff31c7c8f22b63d8a6a987522988b50fb96cbe848b6d3`.
  A preliminary reuse check assumed relative report paths and failed; the
  corrected check accepts the same exact repository path under original
  checkout roots and verifies every count. No finding was dropped.
- [ ] Resolve exact RCH recovery before any additional compiler request.
  Strict Clippy retry on `b8974575`, hz3 build `30097323753221899`, wrapper
  `rchw-2d0f74cc-881e-48ef-8580-13713dd3adb4`, was refused before Cargo by
  `monitor_unavailable`. Wrapper exit 1 retained ownership. Both supported
  recovery attempts failed with RCH-E504: cannot verify source-pair release
  receipt, status 73. Recipe records command exit 120, sources released and
  tree retired, but pair release remains false and terminal acknowledgment
  remains false. Preserve the frozen compiler checkout and both recoveries.
  Read-only capability probes find hz3's deployed rch-wkr 2.1.37 (550f62e7f4d9)
  rejects `source-pair`; the current local CLI also reports 2.1.37 while the
  daemon reports 2.1.36. These are current observations, not reconstructed
  past executable identities. No global binary, daemon, lock, reservation or
  ledger was changed. Agent Mail message 329 requests peer coordination.
- [ ] Publish exact source and executed evidence; keep original Bead
  acceptance open until every required native lane has passed.

## Second review of strict build routing (`br-u9bhg`, PR #410)

The initial numeric-only routing fix at `ea4ba71f` was incomplete for RCH's
case-insensitive truthy values. Its sixteen-case evidence remains historical.

- [x] Read RCH's actual flag parsers and reproduce twenty-four failures among
  46 routing cases plus eight failures among ten artifact-fallback cases.
  The retained baseline reports 13/15 helper scenarios passed.
- [x] Use one runtime predicate for both remote settings at every build,
  missing-RCH, SQLite-helper and binary-fallback boundary. Preserve false
  settings, command errors and original response/build timeout defaults.
- [x] Pass all 46 routing controls, ten artifact controls and 15 helper
  scenarios. Recording commands never invoke a compiler or stale executable.
- [x] Update the existing Rust fixture to source the complete embedded helper
  and test both settings with five truthy spellings. Its shell dependencies
  are covered above; the Rust test itself still requires native execution.
- [x] Complete the original main `462914b7` all-targets check: exit 0, build
  30097323753221799 on vmi1227854. All 3,948 receipt files match local SHA-256,
  size and executable mode; receipt root
  `36ca6dbb5835984d0a4105ceeedbfde4a42fe7fbe2e1b46edc48c1e7560c2e8f`.
  This qualifies that older snapshot's compiler graph, not this follow-up.
- [x] Fix its unused-helper warning by retaining the stamp utility only for
  its existing unit-test callers. Preserve the Unix nonempty-stamp check and
  foreign-message conflict checks; non-Unix's deliberately absent stamp no
  longer causes an unconditional unwrap panic. Windows runtime is untested.
- [x] Pass workspace formatting and Bash syntax after these repairs; complete
  normal scans of both edited Rust files and both shell files with zero new
  critical findings, retaining all warnings.
- [x] Integrate pinned main `42acb599` once and review its Rust changes, keeping
  the current peer updates and all original test assertions.
- [x] Publish the follow-up through `e9fad95a` on draft PR #410. GitHub confirms
  that exact head is open, unmerged and conflict-free against pinned main.
  Rewrite the PR description around the final truthy-value repair and actual
  validation limits, preserving the earlier failed controls as history.
- [x] Exercise this final helper against signed v0.3.41: five correlated
  replies, 45 tools, intended message 2/body, unchanged first ACK and persisted
  delivery receipt. Client SHA-256
  `bbe7c299e83c20167a6082a7a640bfcc2f0729035654bc2df415ebb77953254e`.
  This is final shell-client evidence, not current Rust server qualification.
  An initial invocation used the wrong artifact variable and failed before
  server launch; preserve that failed log alongside the corrected run.
- [x] Run the integrated readiness harness tests: 28/28 pass, no skips.
  These validate the harness model and do not qualify native readiness.
- [x] Complete the three-file JavaScript scanner run and review its one
  critical token-comparison finding in the loopback harness surrogate.
  Retain that finding, all 23 warnings and all 292 informational findings.
- [x] Complete the updated 48-file Rust scanner scope. The grouped run timed
  out after 300 seconds and is partial. Individual storage and mail UI scans
  completed; the individual CLI scan also exceeded 300 seconds. Its unchanged
  rules completed in 794 seconds with a 1,200-second scanner envelope. Verify
  all 44 reused source/report hashes and per-file counts; four refreshed files
  complete the scope. Retain 55 critical, 59,335 warning and 21,181 informational
  source findings. The three added criticals compare public JSON-RPC versions
  and request IDs; all 52 previous critical identities remain. No suppression.
  Final report `final-review-v3-ubs-final-report.json` SHA-256
  `fc53c67670401e24b66b22b5e5bc0a0ec3a5a2332054f471cdf975f3bf18d40e`.
  The guard could not evaluate the original combined report script, which did
  not run. Separate read-only checks and an explicit artifact patch completed
  the report without changing guard policy or touching code files.
- [x] Pass the final locked production graph (`--workspace --lib --bins -j1`)
  on frozen `e9fad95a`: hz3 build `30097323753221872`, exit 0, Cargo 26m54s.
  Verify all 3,952 receipt entries with zero hash/length/executable-mode drift;
  receipt root `fa7d4371a069a35be44a6d9da64809e8764c9d487c26b98379bfdf3616da3845`.
  No project warning remains; retain the third-party future-incompatibility
  notice for `proc-macro-error2 v2.0.1`. This is compiler evidence, not runtime.
  Final production request on vmi1153651 was refused at disk admission (exit
  120, Cargo not started). I submitted it before inspecting a refreshed census
  that already showed insufficient capacity; no pass is claimed.
  A later positive static-budget census on hz3 preceded a new strict request;
  that request supplied the successful result above and is terminal.
- [x] Pass final locked workspace `--all-targets` on this exact source; the
  older main462 result does not satisfy this item. The warm vmi1227854 request
  timed out in queue: exit 103, Cargo not started, wrapper
  `rchw-ee78ac40-7c61-4a38-915c-0b056016b1ce` terminal and acknowledged.
  A fresh static-budget census and free-slot check permit submitting the
  unchanged strict gate to vmi1153651. Actual Cargo compilation began there;
  build `30097323753221882`, wrapper
  `rchw-9c631dbd-0853-471f-8a3c-b23dbca635c5`. It hit the 3,600-second
  compiler deadline before completion, with no compiler diagnostic reported.
  Wrapper exit 1 retained ownership; exact-job recovery later acknowledged
  the actual delivery exit 120 (daemon previously recorded 130). Recovery's
  own exit 0 means reconciliation succeeded, not that compilation passed.
  Retain all logs; recheck ordinary capacity before any retry or next gate.
  A fresh positive static census admitted the unchanged cached retry:
  vmi1153651 build `30097323753221887`, exit 0, Cargo 13m37s, wrapper
  `rchw-461a2f93-018d-4370-8c7b-4e5ae10b022b` terminal and acknowledged.
  All 3,952 file hashes/lengths/modes match local source and the successful
  production map exactly. Receipt root
  `6ef91bf62bfc18d510c25ca7eaf2ea5f639b409cfe35c7be2162d5b32a895e56`.
  The source is still `e9fad95a`; no project warnings were reported. Preserve
  the third-party future-incompatibility notice separately.
- [ ] Pass final locked workspace Clippy `--all-targets -- -D warnings` on
  this exact source. Preserve the current source checkout between requests.
- [x] Observe both workers' compiler identity: Rust 1.100.0-nightly,
  commit `90850177249efe0321573c569aec5d12b257f8d6`, LLVM 23.1.0;
  identical rustc SHA-256
  `49f9fa06f3e2b93aba85273433fef67b984ddb4cacb4500de56e85f70ec2e339`
  and Cargo SHA-256
  `19008157a0089896bc835c5ceb0b5bfbe26e70cdd5579c32bd09538fb68c0b76`.
  Source Cargo.lock SHA-256
  `98b26497fb52c79582497c745654e6a7433a7a8b9178e5965650c1e37e751938`.
  Observation is after production completion and during all-targets execution;
  retain this limit with the full source receipts.
- [ ] Obtain ordinary native admission and execute the recovery and helper
  regressions with nonzero counts and unchanged limits. Recent censuses still
  show insufficient unreserved native disk despite apparent free worker slots.
  Include the modified tests explicitly:
  `e2e_remote_required_helpers_reject_missing_artifacts_without_local_build`
  and `deferred_message_identity_rechecks_a_foreign_historical_month`, as
  well as every original DB/storage/tools/CLI acceptance lane. A successful
  compiler check does not execute these tests.
  The final DB compiler-only request on hz3 was also refused before Cargo:
  exit 120, normal 30 GiB growth plus 10 GiB floor. A fresh earlier census had
  room, but admission found competing reservations. Native assertions remain
  zero. All requests preceding the next compiler gate are terminal. Fresh
  hz3/vmi1227854/vmi1152480
  censuses after the production pass still fail the ordinary native budget.
  Later, after the all-targets pass, a fresh vmi1227854 census shows enough
  normal native capacity (110,274,899,968 free; 59,055,800,320 active budgets;
  102,005,473,280 required including native growth and floor) plus free slots.
  The DB lib `cargo test --no-run -j1` request actually entered compilation:
  wrapper `rchw-3938324a-4768-42f9-8dc5-d502ec21076c`. It timed out after
  1,800 seconds because --no-run uses the test-command cap, despite the
  requested 3,600-second build setting. Recovery acknowledged delivery exit
  120; no test ran. See the schedule review above for the corrected literal
  compiler-only setting. Actual native test limits remain unchanged.
- [x] Verify the real worker runner before test dispatch: Nextest 0.9.143 at
  `/root/.cargo/bin/cargo-nextest` is present on RCH's actual process PATH.
  Use the documented `--build-jobs 1`, `--test-threads 1`, `--no-tests fail`
  and `--no-fail-fast` flags. Preserve the existing Nextest configuration and
  original test envelope; runner-help verification executes no tests.
- [ ] Publish final scoped scanner and gate outcomes, retaining the original
  Bead acceptance until all required native lanes execute successfully.

## Fresh review after PR #409 merge (`br-u9bhg`, `br-1w2z0`)

Review base: main `462914b7`. The first native request used preserved clean
`a025434e`; its recovery source matches this main exactly. After that terminal
refusal, the compiler checkout fast-forwarded to `462914b7` and stays frozen
until the current owned RCH request reaches a terminal state.

- [x] Verify PR #409 merged, the shared checkout's peer edits are preserved,
  governing project docs are unchanged, and there is no unread coordination.
- [x] Reread every stdio metadata-validation and self-test hunk, then trace
  initialization, queued writes, response correlation and EOF cleanup again.
  No additional defect has been established in those changes.
- [x] Refresh Beads/BV and CASS history; retain existing owners and original
  acceptance rather than claim an unrelated task while recovery is active.
- [x] Run required read-only mailbox health. It reports leaked-pages-only and
  a proactive backup 389 hours old; no live repair or vacuum was performed.
- [x] Check ordinary disk reservations before a new native request. The
  vmi1152480 census showed room for normal 30 GiB growth plus 10 GiB floor;
  that disk observation did not guarantee a worker slot or actual admission.
- [x] Submit one strict remote DB nextest request with one build job, one
  test thread, unchanged 1800-second native envelope and per-test limits.
- [x] Observe the terminal result of wrapper
  `rchw-3d39869d-55a3-4339-9f94-17fc530c4a3b`: queue timeout, exit 103,
  Cargo not started, native assertions zero. Preserve its refusal log.
- [x] Start actual all-targets Cargo compilation of main `462914b7` on
  vmi1227854 through ordinary strict RCH admission, with one build job.
- [x] Scan both changed shell files with normal Bash UBS rules: zero critical,
  14 retained warnings, each an explicitly guarded post-increment.
- [x] Reproduce six formatting failures in the newly merged journal scanner,
  manually apply the reported whitespace changes in this isolated checkout,
  and pass the full workspace formatting check without changing Rust tokens.
- [x] Complete normal-rule static UBS over the 46-path merged scope: eight
  refreshed paths (journal scanner refreshed after formatting) and 38 identical
  source/report reuses. Retain all 52 critical, 58,430 warning and 20,973
  informational source findings; critical identities match the prior report.
  Report `fresh-review-v1-ubs-final-report.json` SHA-256:
  `added5433d974465c821dc5e856640278ce999da1ccf2ecc7a27e8a37075c0cd`.
- [x] Inspect the surrounding build helper and reproduce both strict-routing
  defects with recording executables: missing `--require-remote` on every
  RCH branch, and force-local taking precedence over either requirement.
  The baseline failed 11 of 16 declared routing controls (13/14 scenarios).
- [x] Propagate the strict RCH option through timeout and mock/non-mock paths;
  reject contradictory local/remote options with exit 64 before any command.
  Preserve optional routing, missing-RCH exit 127 and remote failure status.
- [x] Pass all 16 routing controls and all 14 helper scenarios, including the
  original ten initialization controls. Recording commands invoke no compiler;
  these controls do not qualify the Rust server or replace live RCH evidence.
- [x] Pass Bash syntax and diff whitespace checks after the routing repair.
- [x] Complete the final normal Bash scan (zero critical, 14 retained guarded
  post-increment warnings), syntax checks and real stdio regression with the
  changed helper. Five correlated replies from signed v0.3.41 verified the
  45-tool catalog, actual message 2/body, unchanged ACK and delivery receipt.
  This validates the client, not current Rust source or live build routing.
  Client SHA-256 `aebf956df94c280fe3f95b33f1f03aa3148ca98a08aae88bab44c916861aceff`;
  self-test SHA-256 `37a7b5b117a1668e91122c107ec2bbf1539532c6703b2021b4c119737fa8c3ff`.
- [x] Observe the main462 compiler result and verify its exact source receipt;
  see the completed all-targets item above. That result is historical after
  the truthy-policy, fixture and integration changes.
- [ ] Execute the native probes with nonzero selected test counts; verify
  their exact source receipts and preserve every failure.
- [ ] Fix any actual compiler or behavioral failures from that run and repeat
  the affected real-dependency controls without relaxing assertions.
- [ ] Complete current-source workspace compiler/Clippy and original native
  acceptance lanes from `br-u9bhg`; an admission or older-source result does
  not close this task.
- [x] Compare the installed and signed v0.3.41 read-only doctor diagnoses.
  The signed CLI reports 264 backup images against a limit of 6, clear staging
  (0/3), a live owner and one leaked page. No retained guard failure log is
  available, so its exact failure chain remains unattributed. Preserve the
  diagnosis without removing data, changing limits or repairing a live owner.
- [x] Record scoped evidence in Bead comments 4794/4795 and inspect the full
  final diff again. Package the routing repair, regression controls and manual
  formatter repairs together; Git and Agent Mail record publication identity.
  Preserve all unfinished compiler and native acceptance above.

## Shared stdio client initialization (`br-1w2z0`, scoped contribution)

This shell-client change started on main `615f4b2f` and integrates `6eb6264d`.
Its real transport
control uses a privately extracted, signature-verified v0.3.41 server.
It does not qualify current Rust source or close the original direct-suite
task. EmeraldEagle's ownership and shared-root path-safety work are preserved.
The separate recovery candidate is published at `a025434e`; its native and
current compiler qualification remains blocked by ordinary RCH admission.

- [x] Read the original task and shared client's lifecycle/EOF behavior.
- [x] Verify the release checksum manifest with the pinned Minisign key,
  verify the Linux archive hash, and retain both private binary hashes.
- [x] Execute a real initialize/tools-list baseline without building Rust or
  replacing installed executables; retain isolated storage and all artifacts.
- [x] Demonstrate seven malformed initialize results accepted by the old
  client, including NULL and missing or mistyped required metadata.
- [x] Reject malformed required metadata before notifications/initialized or
  queued tools, preserving valid extensions and JSON-RPC initialization errors.
- [x] Add ten peer controls with exact success/failure exits, diagnostics and
  observed request order. Negative peer fixtures are not live-server proof.
- [x] Pass all 13 helper self-tests, including those ten initialization cases.
- [x] Exercise ordered and pipelined sessions against the signed real server:
  20 correlated replies, coordination tools, resource reads and an expected
  post-initialize unknown-tool error.
- [x] Use the returned mail ID after automatic contact mail; verify the intended
  message body and unchanged ACK through separate server process restarts.
- [x] Confirm the delivery receipt and committed canonical/inbox/outbox Git
  blobs. Read the committed tree directly; lock-free publication leaves the
  shared Git index empty.
- [x] Preserve original client deadlines and raw protocol-negative clients;
  pass Bash syntax and whitespace checks for the two changed source files.
- [x] Record scoped evidence on the existing Bead, retain its original
  acceptance/status/owner, and hand off the exact client source and limits.
- [x] Prepare the reviewed client fix, regression controls and scoped Bead
  evidence for publication on `mossytern/stdio-initialize-validation-20261124`.
- [x] Publish the client fix as `ca78ce6f` and draft PR #409. Preserve both
  stdio and recovery TODO sections when integrating main's concurrent update.
- [ ] Complete the inherited direct-client inventory and current-source suite
  qualification under the original task; do not count this contribution as
  completion of peer path-safety work or Rust recovery qualification.

Evidence and retained replay state:
`/data/tmp/am-mossytern-stdio-release-g0zrolbg/`. The baseline and final helper
logs retain all ten cases. `verified-binaries.json` binds release identity;
`real-client-verification.json` binds the client/self-test hashes, actual mail
ID, correlated replies, restarted receipt and committed archive copies.

## Fresh review of concurrent recovery publication (`br-u9bhg`)

This checkout integrates main `615f4b2f`, including incremental recovery
seals, profile durability repair and Git contention changes. PR #408 was
merged as `13ef2cea`; its older static receipts do not qualify this tree.
The concurrent probes are still uncompiled and have executed zero assertions.

The original `c4e665f7` probe was invalid: replacing ACK 444 with ACK 999
conflicted with the intentional first-ACK/earliest-timestamp contract. Its
source and refusal logs remain preserved. The corrected scenario publishes
a first read at 444, then attempts a first ACK at 999 with the production
`COALESCE` rule and an explicit SQL precondition that ACK is still NULL.

- [x] Reread every published Rust hunk, the storage flush/clone regression,
  durable Git debt, state-lock ordering and incomplete-publication repair.
- [x] Refresh Beads/BV, git and peer activity; keep the original qualification
  task in progress rather than taking a second unfinished soak task.
- [x] Verify live disk reservations before the normal native request.
- [x] Preserve the first DB request's terminal queue-timeout refusal: strict
  RCH on `vmi1149989`, exit 103, no Cargo execution or native assertions.
- [x] Preserve the new probes' unavailable-worker refusal (exit 103) and
  subsequent selected-worker disk-admission refusal (exit 120, RCH-E330).
  Both happened before Cargo; native assertions remain zero.
- [x] Isolate a new baseline probe from the published checkout's source freeze.
- [x] Reuse the real database setup for file-backed canonical and runtime
  connections, with concurrent autocommit enabled on the runtime peer.
- [x] Correct the invalid timestamp-overwrite model without changing the
  production first-ACK or earliest-timestamp behavior.
- [x] Add the interrupted first-read control, requiring an actual read-444,
  ACK-NULL artifact and incomplete witness before the competing commit.
- [x] Require ACK NULL immediately before the peer's real COALESCE update;
  retain actual SQL ACK-999 and recovered ACK-999 assertions.
- [x] Execute the supporting system-SQLite WAL schedule: seven SQL assertions
  confirm a first ACK can commit while an older read snapshot remains open,
  and a later COALESCE update preserves it. This is not Rust runtime evidence.
- [x] Retain the positive retry control when writer admission excludes the peer.
- [x] Integrate current main and read the recovery seal/publication changes;
  preserve all older source and failed receipts.
- [x] Repair observed workspace formatting failures in five upstream storage
  paths by applying the reported whitespace changes manually.
- [x] Pass the current workspace formatting check.
- [x] Complete normal-rule UBS across the 41-path current scope: 11 fresh
  scans and 30 byte-identical report reuses, with all 52 reviewed critical,
  56,497 warning and 20,651 informational source findings retained.
- [x] Attempt the locked current-tree all-targets check through strict RCH:
  hz4 refused at disk admission (E330/exit 120), before Cargo, growth zero.
- [ ] Compile and execute both new baseline probes through ordinary strict
  RCH admission; distinguish a target failure from an invalid control.
- [ ] If the target fails, implement the smallest production repair supported
  by that failure, preserving cancellation and caller-owned transactions.
- [ ] Execute both probes again, retain the original failure, and require
  successful peer commit/publication after the repaired operation.
- [ ] Pass all three required locked workspace compiler/Clippy graphs on the
  exact final source and complete its changed-file scanner review.
- [ ] Execute the original DB, storage, tools and CLI acceptance lanes; keep
  their nonzero counts separate from compile, scanner and admission results.
- [x] Record the invalid-probe correction and current scope in the original
  bead, flush only intended graph records, confirm zero cycles, and hand off
  the exact source and limitations. Comment 4793 supersedes 4792; the prior
  probe and mail 310 are preserved as historical, unexecuted evidence.

The superseded probe's source SHA-256 was
`617c462a2874e101ee0934662997a3838657f944ba871f5748d656216d906e80`.
Historical `concurrent-ack-v2-*` evidence remains under
`/data/tmp/am-mossytern-20261123/`. It supplies no runtime or corrected-source
qualification. Current failures and results use the `current-main-*` prefix.
No test limit, disk reservation, worker drain or peer job was changed.
Current probe source SHA-256:
`f8e46b6dd405627d5f4e443f8d19ef7afe6ac49993ece89942047ac712c896ff`.
The complete static report is `current-main-v1-ubs-final-report.json`
(SHA-256 `39c368878852ad8323437e139ca68de097db525f7d22be6b61f481e9087e2da2`).
The initial aggregate failed to normalize absolute scanner paths; its original
bytes remain explicitly marked invalid, and the corrected aggregate asserts
the prior finding counts for all reused files. No finding was suppressed.

## Published recovery qualification history (`br-u9bhg`, MossyTern)

Current base: `c5672259`. Its durable Git outbox is upstream work, reviewed
here without claiming authorship or native qualification. The superseded
inventory candidate and its complete source/Beads history remain intact at
`/data/projects/am-mossytern-b4-v5-preserved-20261124`; earlier work remains at
`/data/projects/mcp_agent_mail_rust-mossytern-20261123`.

- [x] Read the new upstream outbox, publication ordering, input bounds,
  compaction, runtime-file exclusion, and real cross-process controls.
- [x] Incorporate and read the disjoint upstream readiness refresh and its
  real-supervisor regression; keep its native proof separate and unclaimed.
- [x] Preserve all failed RCH/scanner receipts and the older candidate;
  recover the timed-out wrapper with terminal ownership acknowledged.
- [x] Carry only observed compiler repairs: separate the credential lookup's
  doc summary, use equivalent lifecycle error patterns, and retain the
  conformance nonempty credential assertion with strict-Clippy syntax.
- [x] Apply the equivalent byte-string newline comparison in the upstream
  outbox and retain its nonempty-debt test with a concrete byte-slice assertion.
- [x] Preserve the outbox-empty and zero-hint requirements with concrete
  comparisons in the DB and exited-producer storage regressions.
- [x] Strengthen the existing production flush/clone regression by draining
  old hints, discarding later receipt/contact/cursor hints, restoring the old
  batch, and requiring the cloned completion witness. Keep all prior checks.
- [x] Correct the README FAQ native-tool count to 9 (47 total minus 38
  compatibility tools). No inventory or compatibility gate was weakened.
- [x] Pass formatting and whitespace on this current candidate.
- [x] Complete normal-rule UBS across the same 36 Rust paths, refreshing
  changed files and reusing completed reports only for identical source.
- [x] Pass locked workspace all-targets check through strict RCH.
- [x] Pass locked workspace production lib/bins check through strict RCH.
- [x] Pass strict locked workspace all-targets Clippy through RCH.
- [x] Verify identical maps of 3,946 files across the three final receipts,
  with zero local hash, length or executable-mode mismatches.
- [x] Attempt the original DB native suite through strict RCH with unchanged
  limits: admission refused before Cargo (E330, exit 120), zero assertions.
- [ ] Execute DB outbox/reconstruction/cursor/sync regressions with nonzero
  counts, including malformed inputs, interrupted publication and retries.
- [ ] Execute storage Git/flush/clone, exited-producer, envelope and backlog
  controls; execute tools consuming-read/admission/committed-ACK replay and
  CLI metadata preservation lanes from the original acceptance.
- [ ] Resolve the retained concurrent-commit publication uncertainty through
  actual runtime evidence; the earlier writer-admission experiment is unrun.
- [x] Verify final source identity, retain every failure, update only this
  bead, flush its scoped JSONL and confirm zero dependency cycles.
- [x] Commit and push the reviewable work as draft PR #408, with native
  assertions explicitly zero and `br-u9bhg` still in progress.
- [ ] Complete original native acceptance and current integration qualification
  after PR #408's merge; publication does not establish release readiness.
- [x] Hand off current results in Agent Mail message 303 and release all
  nine source/TODO reservations (2026-11-24 01:46:06 UTC).

Draft: https://github.com/Dicklesworthstone/mcp_agent_mail_rust/pull/408
Code commit: `44787e6b`. Latest target main was `bad3b904`; GitHub reports
the draft merges without conflicts. The checks above qualify the named
`c5672259` candidate, and do not claim the moving target's runtime result.

The final `c5672259` candidate passed all three static graphs with identical
maps of 3,946 files. Evidence is retained in
`/data/tmp/am-mossytern-20261123/mainc567-v1-three-gates-verified.json` and
`mainc567-v1-ubs-final-report.json`. The qualified pre-handoff source patch
SHA-256 is `1b5c383cebce69fee610b1da3998e793388e7e0195d049aa003e35e52f33b99e`.
Native assertions remain zero: disk admission and a draining worker refused
the requests. Compiler calls use a 3600-second outer envelope after a cold
1800-second timeout; test limits, disk budgets and assertions are unchanged.

## Current operational reliability improvements

- [x] Bound project identity cache retention by freshness, entry count, aggregate
  string bytes, and per-identity bytes; return oversized discovery results
  unchanged without retaining them (`br-xai5l`).
- [x] Prepare and validate reconciliation attachments before global archive
  publication admission, with cancellation checkpoints and final under-lock
  disk/Git/ancestor authority rechecks (`br-5a5s0`).
- [x] Revalidate queued ATC releases against the current engine, population,
  policy and activity; use a bounded holder snapshot and guarded database
  mutation, with atomic grant/renewal activity and no voluntary-release intent
  fallback (`br-qo8j5`).
- [x] Carry request-token and request-scope liveness through database checkout,
  writer admission and pre-commit checks for automatic releases; roll back
  observed cancellation before commit admission (`br-qo8j5`).
- [x] Run native identity regressions: 150 passed, 0 failed, 1 ignored,
  including all five new cache cases (`br-xai5l`).
- [x] Pass the core production library check and strict core all-targets Clippy.
- [ ] Complete archive/ATC native regressions and final workspace compiler/lint
  validation. On this executor, both workspace check graphs (all targets and production
  libraries/binaries) and strict workspace Clippy were killed with SIGKILL
  while compiling the pinned Asupersync dependency before reaching the
  application changes, including a single-job, no-debug, dependency opt-level
  zero retry. Broader daemon memory attribution and archive soak acceptance
  remain open.
- [ ] Prevent concurrent deliveries from losing notification wakeups during
  inbox reads, including cross-process debounce and filtered/peek behavior
  (`br-dksvp`).
- [x] Bind ATC write mode and kill-switch authority to each server lifetime;
  prevent delayed observations or controls from reusing another mailbox policy
  and fail closed when the current marker authority cannot be inspected.
- [x] Make archive flushes honor FIFO persistence barriers and report unresolved
  write failures; retain failure status across unrelated successful writes,
  shutdown, and restart until the existing repair ledger verifies recovery.
- [x] Reject invalid or unresolved query filters before search routing and caching;
  preserve explicit facet precedence and propagate project lookup failures and
  cancellation without broadening the search.
- [x] Publish archive epoch and index-sync control markers through private,
  exclusively created staging files; preserve foreign staging entries and reject
  symlinked control paths and nonregular destinations.
- [x] Make glob reservation discovery lazy and bounded without hashing tracked
  file contents; honor Git ignore rules and preserve unknown activity for partial
  scans, nested repositories, and submodules before force-release decisions.
- [x] Keep descriptor-floor monitoring safe during declining counts, sampler
  restarts, and missed windows; use observed minima timestamps for growth and
  expose a conditional soft-limit exhaustion estimate in health and logs.
- [ ] Complete the broader `br-kp1in.29` operation-normalized resource-growth,
  statistical calibration, shared soak predicates, and release-overhead acceptance.

## 0. Coordination / Meta
- [x] Read `AGENTS.md` and `README.md` (baseline orientation).
- [x] Register with MCP Agent Mail and announce to active agents.
- [x] Check inbox from other agents at least once per session.
- [x] Reserve files before edits (Agent Mail leases) and release on completion.
- [x] Keep `FEATURE_PARITY.md` aligned with completed work.
- [x] Keep `TODO.md` updated after each batch of changes.
- [x] Confirm MCP Agent Mail server stability (health check path, lock handling).

## 1. Workspace & Crate Scaffolding
- [x] Workspace `Cargo.toml` with required path deps.
- [x] `rust-toolchain.toml` with nightly channel.
- [x] Crate directories for core/db/storage/server/cli/guard/share/conformance.
- [x] Scaffold minimal `Cargo.toml` for each crate.
- [x] Placeholder `lib.rs`/`main.rs` for each crate.
- [x] README for Rust port (overview, usage, dev commands).

## 2. Conformance Harness (Skeleton → Full)
- [x] `crates/mcp-agent-mail-conformance/tests/conformance/README.md` with schema.
- [x] Fixtures placeholder JSON + loader.
- [x] Python fixture generator stub + full runner.
- [x] Rust test to load fixture schema.
- [x] Python reference runner: start legacy server + capture tool outputs.
- [x] Python reference runner: capture resources + deterministic ordering.
- [x] Canonical fixture schema for ok/err shapes.
- [x] Rust conformance runner: tools + resources + diff reporting.
- [x] Tool-filter fixtures (profiles + include/exclude).
- [x] Archive artifact assertions (profiles, messages, inbox/outbox, reservations).
- [x] Notification signal conformance (to/cc signals, bcc excluded, fetch_inbox clears).
- [x] Conformance CLI helper to regenerate fixtures (Rust wrapper or script).
- [x] CI step for conformance tests (ensure fixtures are current).
- [x] Add missing error-case fixtures (invalid params, unknown project/agent, malformed inputs).
- [x] Add resource query-param fixtures (format/filter params for static resources).

## 3. DB Layer (sqlmodel_rust) — Models & Schema
- [x] SQLModel structs for all tables (`src/models.rs`).
- [x] JSON attachments field in message schema.
- [x] Timestamp conversion utilities (`timestamps.rs`).
- [x] Error types (`error.rs`).
- [x] Explicit indexes + unique constraints matching legacy.
- [x] FTS triggers installed and validated.
- [x] Schema migration story using `sqlmodel_schema::MigrationRunner`.
- [x] WAL PRAGMA setup SQL (`schema.rs`).
- [x] Pool config defaults (`pool.rs`).
- [x] Pool wired to `sqlmodel_pool` + `sqlmodel_sqlite`.
- [x] Query tracking + slow query logging (per-tool instrumentation).
- [x] Verify tracked connection coverage for any direct `query_sync` tool paths.

## 4. Server Layer (fastmcp_rust)
- [x] All tools registered with correct names and clusters.
- [x] All resources registered with correct URI templates.
- [x] Tool filtering profiles wired (full/core/minimal/messaging/custom).
- [x] Identity tools: ensure_project/register_agent/create_agent_identity/whois.
- [x] Messaging tools: send_message/reply_message/fetch_inbox/mark_message_read/acknowledge_message.
- [x] Search tools: search_messages/summarize_thread.
- [x] File reservation tools: file_reservation_paths/release/renew/force_release.
- [x] Contact tools: request_contact/respond_contact/list_contacts/set_contact_policy.
- [x] Product tools: ensure_product/products_link/search_messages_product/fetch_inbox_product/summarize_thread_product.
- [x] Build slots: acquire/renew/release.
- [x] Macros: start_session/prepare_thread/file_reservation_cycle/contact_handshake.
- [x] TOON output handling for tool + resource responses.
- [x] Instrumentation wrapper for per-tool query stats logging.
- [x] Validate resource query matching behavior against fastmcp router (edge cases).
- [x] Implement query parsing for static resources (`resource://projects?format=...`).

## 5. Storage Layer (Git Archive)
- [x] Archive root creation + `.gitattributes`.
- [x] Per-project locks (`.archive.lock`) + commit locks.
- [x] Commit queue batching.
- [x] Message write pipeline (canonical + inbox/outbox + commit).
- [x] File reservation artifacts (digest + id files).
- [x] Attachment pipeline (WebP conversion, originals, manifests, audit logs).
- [x] Notification signals (debounced files).
- [x] Read helpers (message by id, thread digest, commit history).
- [x] Storage wired into identity, messaging, reservation, resources.

## 6. Guard (Pre-commit/Pre-push)
- [x] Guard install/uninstall (chain-runner hook).
- [x] Pre-commit check (staged paths, rename handling).
- [x] Pre-push check (commit diff scanning).
- [x] Path matching (pathspec + fnmatch fallback).
- [x] Advisory mode + bypass env var.
- [x] Rust-native conflict detection (guard_check, guard_check_full).
- [x] Guard status command.

## 7. Share / Export
- [x] SQLite snapshot (WAL checkpoint + backup).
- [x] Scrub presets (standard/strict/archive).
- [x] FTS + materialized views for static viewer.
- [x] Attachment bundling (inline/detach thresholds).
- [x] Chunking for large DB.
- [x] Manifest + signing + encryption (age/Ed25519).
- [x] Preview server + verify/decrypt.

## 8. CLI
- [x] `serve-http`, `serve-stdio`.
- [x] `lint`, `typecheck`.
- [x] `guard` subcommands (install/uninstall/status/check).
- [x] `acks` subcommands (pending/remind/overdue).
- [x] `file_reservations` subcommands (list/active/soon).
- [x] `config` subcommands (set-port/show-port).
- [x] `doctor check` (--verbose, --json).
- [x] `projects` subcommands (mark-identity/discovery-init/adopt).
- [x] `list-projects` (--json, --include-agents).
- [x] `list-acks`.
- [x] `mail status`.
- [x] `migrate`.
- [x] `clear-and-reset-everything`.
- [x] `am-run` and `amctl env`.
- [x] `share` subcommands (export/update/preview/verify/decrypt/wizard).
- [x] `archive` subcommands (save/list/restore).
- [x] `doctor` subcommands (repair/backups/restore).
- [x] `products` subcommands (ensure/link/status/search/inbox/summarize-thread).
- [x] `docs insert-blurbs`.

## 9. Benchmarks (Perf + Conformance)
- [x] Bench suite reusing conformance fixtures.
- [x] Tool latency baseline (8 representative tools).
- [x] Initial baseline recorded (most tools sub-100µs).
- [x] Add archive write throughput benchmarks (send_message + commit queue).
- [x] Capture hyperfine baselines for CLI commands (serve + send_message + fetch_inbox).
- [x] Run `cargo flamegraph` for hot paths (search + send_message + storage writes).
- [x] Record golden outputs + checksums for optimization validation.
- [x] Maintain perf regression notes in `FEATURE_PARITY.md` or dedicated doc.

## 10. Dependency Integration (Local /dp Crates)
- [x] Confirm zero direct `tokio` usage in code paths (only via optional deps).
- [x] Use `sqlmodel_rust` for SQLite access (no direct `rusqlite`).
- [x] Use `asupersync` for HTTP + async runtime.
- [x] Use `frankentui` (`ftui`) for CLI rendering.
- [x] Integrate `beads_rust` for issue/task awareness in CLI or server.
- [x] Integrate `coding_agent_session_search` for agent detection (or port logic to avoid tokio).
- [x] Evaluate fastmcp resource query param handling; add adapters if needed (validated by router-level conformance tests; no adapter required).

## 11. Testing & QA
- [x] Unit tests for tool validation rules (input constraints, error messages).
- [x] Integration tests for DB + storage pipelines (archive + SQLite consistency).
- [x] E2E tests for MCP tool calls (stdio + HTTP).
- [x] Guard tests for conflict detection (pathspec + rename + advisory).
- [x] Run full toolchain: `cargo check --all-targets`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`, `cargo test`.

## 12. Publishing & Release (Crates.io + CI)
- [x] Confirm Cargo metadata (license/readme/repository) for publishable crates.
- [x] Add crates.io publish workflow gated by tags (`CARGO_REGISTRY_TOKEN`).
- [x] Document publish order for workspace crates (`cargo publish -p ...`).

### Publish Order (dependency-respecting)

```bash
# Phase 1: Foundation (no internal deps)
cargo publish -p mcp-agent-mail-core

# Phase 2: Database + storage layers
cargo publish -p mcp-agent-mail-db
cargo publish -p mcp-agent-mail-storage

# Phase 3: Utilities
cargo publish -p mcp-agent-mail-guard
cargo publish -p mcp-agent-mail-share

# Phase 4: High-level APIs
cargo publish -p mcp-agent-mail-tools
cargo publish -p mcp-agent-mail-server
cargo publish -p mcp-agent-mail-cli

# Phase 5: Binaries
cargo publish -p mcp-agent-mail

# Not published: mcp-agent-mail-conformance (publish = false)
```

**Note**: All external deps (fastmcp, sqlmodel, asupersync, ftui) are path dependencies
and must be published to crates.io first or replaced with registry versions before
workspace crates can be published.
