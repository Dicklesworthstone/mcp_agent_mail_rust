# GH354 native Rust validation status

**Native application qualification is blocked.** Three compiler attempts on
2026-11-05 exited 101 after the compiler for `asupersync 0.5.0` received SIGKILL.
None reached the changed storage sources. There is no changed-storage typecheck
verdict, no native Rust test binary, and no native unit or integration test
execution from this run. The separate [CLI SQL experiment](README.md) supplies
SQL-level evidence only; it does not replace these application gates.

## Environment and source scope

The attempts used pinned `nightly-2026-08-31`, Rust
`1.100.0-nightly (908501772 2026-08-30)`, Cargo
`1.100.0-nightly (e8cb624d5 2026-08-22)`, one build job, debug information disabled,
and incremental compilation disabled. Storage used `--no-default-features` and
`--locked`. Git dependencies were fetched at their existing locked revisions
using Cargo's nightly shallow-dependency support. No dependency, manifest,
lockfile, feature, test assertion, timeout, or ignore state was changed to obtain
a result. Attempt 2 alone applied an ephemeral Asupersync package optimization
override, described below; it is not an acceptance profile.

The local checkout started at `8239ae081b0f6c5432cc27ee4a9c381e943b4f0e` and
included the incoming reservation/message query fixes and tests from
`3455c62c53365ddf51d269973979cf00e40e3ca1` and
`f4c3cdf5339bcc885cfa4ad4209353aba6a399f3`, plus this follow-up's five Rust
files. Later main `283245fe318a2011669f9088d186a7fc2a3fa7fe` contains additional
non-overlapping changes. This record does **not** claim that final main, or its
full workspace, compiled or passed formatting. The exact local dependency
hashes and reviewed Rust-file hashes are in [validation.json](validation.json);
source hashes do not identify successfully compiled artifacts.

## Three failed compiler attempts

These commands ran from the repository after sourcing the task-owned toolchain
environment. Each had `CARGO_BUILD_JOBS=1`, `CARGO_PROFILE_DEV_DEBUG=0`,
`CARGO_PROFILE_TEST_DEBUG=0`, and `CARGO_INCREMENTAL=0`.

```bash
# Attempt 1: repository optimization profile; compile the native storage tests.
CARGO_NET_GIT_FETCH_WITH_CLI=false cargo -Zgit=shallow-deps test -p mcp-agent-mail-storage --no-default-features --lib --no-run --locked

# Attempt 2: only Asupersync dev/test package optimization reduced to zero.
CARGO_NET_GIT_FETCH_WITH_CLI=false cargo -Zgit=shallow-deps --config profile.dev.package.asupersync.opt-level=0 --config profile.test.package.asupersync.opt-level=0 test -p mcp-agent-mail-storage --no-default-features --lib --no-run --locked

# Attempt 3: metadata check with the original optimization profile restored.
CARGO_NET_GIT_FETCH_WITH_CLI=false cargo -Zgit=shallow-deps check -p mcp-agent-mail-storage --no-default-features --all-targets --locked
```

| Attempt | Asupersync compiler mode | Exit | Result | Shared `oom_kill` observations |
|---|---|---:|---|---|
| 1 | `dep-info,metadata,link`; optimization 1 | 101 | Signal 9, SIGKILL | 19 → 20 |
| 2 | `dep-info,metadata,link`; optimization 0 | 101 | Signal 9, SIGKILL | 20 → 23 |
| 3 | `dep-info,metadata`; optimization 1 | 101 | Signal 9, SIGKILL | 23 → 29 |

The shared cgroup limit was **8,589,934,592 bytes (8 GiB)** and observed usage
was close to that limit. The SIGKILL results and increasing shared OOM counters
are consistent with memory pressure, but those counters include unrelated
processes; the kernel victim was not independently attributed to each compiler
PID. Observed compiler RSS values are samples, not peak-memory measurements.
The third failure occurred without LLVM code generation, so the evidence does
not establish an LLVM optimization-specific failure. No additional compiler
retry or gate relaxation was used.

Cargo accepted attempt 2's configuration with `metadata --no-deps --locked
--format-version 1` (exit 0), and the actual compiler invocation omitted the
optimization flag, confirming Rust's default optimization level 0. That
configuration parse is not a compilation pass.

Full executor logs remain in `/workspace/agent-mail-gh354-tools`; the committed
record retains their identities rather than their full contents:

| Log basename | SHA-256 |
|---|---|
| `storage-setup-shallow-build.log` | `db15f16f284385b58dccb38c189ba5fc81134134e04a169bbc670f9618d62e36` |
| `storage-asupersync-opt0-build.log` | `04b7c6181f53ca14e18048a273abf1d782970210fd03310952d1d5d43cdd6ad7` |
| `storage-check-all-targets.log` | `cf010fda096472855704bf808ac4b2911a711bd44a49d6db0b247ce07c8b925d` |

## Checks that did pass

`cargo fmt --all -- --check` exited 0 on the local workspace before the final
small source integrations. Its empty log has SHA-256
`e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`.
The integrator separately checked formatting of the final five changed Rust
files with the following pinned-toolchain command, and ran `git diff --check`;
both exited 0. This is a formatting result for those owned source bytes, not a
final-main workspace or compiler verdict.

```bash
rustfmt --edition 2024 --check --config skip_children=true \
  crates/mcp-agent-mail-db/src/queries.rs \
  crates/mcp-agent-mail-storage/src/recovery.rs \
  crates/mcp-agent-mail-storage/src/recovery/active_reservations.rs \
  crates/mcp-agent-mail-storage/src/recovery/reservation_reconcile.rs \
  crates/mcp-agent-mail-storage/src/recovery/message_reconcile/database/source.rs
```

## Native acceptance commands still to run

Use a sufficiently provisioned native Linux host, the pinned toolchain and
unchanged locked dependencies. Keep the original optimization profile; do not
carry over attempt 2's package override. Run from the repository root:

```bash
export CARGO_BUILD_JOBS=1
export CARGO_PROFILE_DEV_DEBUG=0
export CARGO_PROFILE_TEST_DEBUG=0
export CARGO_INCREMENTAL=0
export CARGO_NET_GIT_FETCH_WITH_CLI=false

gh354_storage() {
  cargo +nightly-2026-08-31 -Zgit=shallow-deps test \
    -p mcp-agent-mail-storage --no-default-features --lib --locked "$1" \
    -- --exact --nocapture --test-threads=1
}

# Five shared reservation-source regressions; CPU test isolates its own child.
for gh354_test in \
  reservation_source_mailbox_scale_matches_legacy_and_seeks \
  reservation_source_byte_limit_counts_utf8_and_every_authority \
  reservation_source_preserves_missing_and_malformed_authority \
  reservation_source_observations_follow_native_snapshot_boundaries \
  reservation_source_mailbox_scale_cpu
do
  gh354_storage "recovery::tests::$gh354_test" || exit
done

gh354_storage recovery::active_reservations::tests::mailbox_scale_active_repair_converges_with_bounded_source_queries || exit
gh354_storage recovery::reservation_reconcile::tests::mailbox_scale_targeted_release_queries_remain_bounded_and_idempotent || exit

# Four retained incoming reservation regressions.
for gh354_test in \
  recovery::active_reservations::tests::active_source_point_lookup_preserves_projection_with_unrelated_history \
  recovery::active_reservations::tests::active_source_point_lookup_preserves_byte_bound_holder_and_release_authority \
  recovery::reservation_reconcile::tests::release_source_point_lookup_preserves_projection_with_unrelated_history \
  recovery::reservation_reconcile::tests::release_source_point_lookup_preserves_byte_bound_and_holder_authority
do
  gh354_storage "$gh354_test" || exit
done

# Four message-boundary/snapshot tests, including the two incoming tests.
for gh354_test in \
  source_lookup_preserves_cross_table_utf8_byte_boundary \
  source_lookup_keeps_one_snapshot_across_an_independent_commit \
  source_statement_selects_one_message_from_populated_delivery_tables \
  source_statement_preserves_byte_bounds_after_message_selection
do
  gh354_storage "recovery::message_reconcile::database::source::tests::$gh354_test" || exit
done

# Run this message CPU/query-frequency test alone, in its own test process.
gh354_storage recovery::message_reconcile::database::source::tests::source_lookup_mailbox_scale_matches_legacy_in_one_query
```

There are **16 focused tests** above. Each exact invocation must report one
executed, non-ignored test and a passing result; zero-match or ignored-only runs
receive no credit. Preserve CPU/query-count assertions and the reservation CPU
child's 180-second deadline. A timing sample by itself is not repair-convergence
or running-server idle-CPU evidence.

Run the following existing authority, concurrent-write and publication-fence
regressions with the same helper, one exact test per invocation:

```bash
for gh354_test in \
  recovery::active_reservations::tests::active_checkout_contention_revalidates_release_renewal_and_generation \
  recovery::active_reservations::tests::captured_active_state_cannot_override_a_release_renewal_or_new_generation \
  recovery::active_reservations::tests::staged_renewal_observes_independent_source_commits_and_preserves_archive_winners \
  recovery::active_reservations::tests::busy_global_fence_does_not_initialize_an_archive_or_hold_database_admission \
  recovery::reservation_reconcile::tests::source_changed_during_checkout_cannot_publish_captured_release \
  recovery::reservation_reconcile::tests::changed_source_generation_cannot_publish_a_captured_release \
  recovery::reservation_reconcile::tests::targeted_release_requires_live_release_even_with_a_captured_terminal_row \
  recovery::reservation_reconcile::tests::busy_publication_fence_preserves_archive_evidence_and_releases_database_admission \
  recovery::reservation_reconcile::tests::busy_publication_fence_cannot_initialize_a_missing_archive_before_admission \
  recovery::message_reconcile::database::source::tests::durable_reply_metadata_refuses_malformed_or_conflicting_authority \
  recovery::message_reconcile::database::source::tests::populated_cache_cannot_replace_missing_or_foreign_delivery_authority \
  recovery::message_reconcile::tests::publication_collision_preserves_both_generations
do
  gh354_storage "$gh354_test" || exit
done

# Retention: preserve all 25 mutation cases and finite-round assertions.
for gh354_test in \
  prune_verified_messages_rechecks_source_and_settlement_before_cascade \
  message_prune_candidates_are_bounded_and_finish_finite_rounds
do
  cargo +nightly-2026-08-31 -Zgit=shallow-deps test \
    -p mcp-agent-mail-db --no-default-features --lib --locked \
    "queries::tests::$gh354_test" -- --exact --nocapture --test-threads=1 || exit
done

# Default 96-message live-writer/inbox-reader integration; retain 120s deadline.
env -u AM_TEST_GH374_LARGE_MAILBOX \
  cargo +nightly-2026-08-31 -Zgit=shallow-deps test \
  -p mcp-agent-mail-storage --no-default-features --test it --locked \
  archive::gh374_reconciliation_converges_with_live_git_writer_and_inbox_readers \
  -- --exact --nocapture --test-threads=1
```

The mandatory workspace checks and Clippy gate remain **outstanding, not
passed**. They were not separately retried after the narrower common-dependency
failure. Use their full default feature graphs and also recheck formatting on
the final merged tree:

```bash
cargo +nightly-2026-08-31 -Zgit=shallow-deps check --workspace --all-targets --locked
cargo +nightly-2026-08-31 -Zgit=shallow-deps check --workspace --lib --bins --locked
cargo +nightly-2026-08-31 -Zgit=shallow-deps clippy --workspace --all-targets --locked -- -D warnings
cargo +nightly-2026-08-31 fmt --all -- --check
```

Issue #354 and its broad acceptance beads remain open. Application-level CPU,
query frequency, typed source equivalence, snapshot/concurrent-write behavior,
repair convergence, publication-fence behavior and retention acceptance still
require the native runs above and a running-server observation. The contributor's
projected savings and the separate measured CLI speedups are not native-server
performance results.
