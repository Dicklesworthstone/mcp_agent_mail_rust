# Decommission Validation Report

## Scope

This report records the assist slice for `bd-3un.37.2` completed in child bead `bd-3un.37.2.2`.

Primary outcomes in this slice:
1. Deterministic cleanup verification script added: `scripts/e2e/post_cutover_cleanup_agent_mail.sh`.
2. Live smoke validation lane executed successfully with structured artifacts.
3. Search-core compatibility drift fixed for the new `frankensearch::ScoredResult.explanation` field.

## Start Gates Snapshot

1. `bd-3un.37` closed: **met** (parent dependency state).
2. `bd-3un.37.3` closed: **met** (parent dependency state).
3. Search V3/progressive behavior validated: **partially evidenced in this slice** via retained-path smoke lane; broader gate remains owned by parent bead.
4. Rollback/feature-flag decision documented: **owned by parent bead**.

## Validation Commands and Results

1. `bash -n scripts/e2e/post_cutover_cleanup_agent_mail.sh` -> PASS.
2. `scripts/e2e/post_cutover_cleanup_agent_mail.sh --mode smoke --execution dry` -> PASS.
3. `POST_CUTOVER_FORCE_LOCAL_CIRCUIT=1 scripts/e2e/post_cutover_cleanup_agent_mail.sh --mode smoke --execution live` -> PASS.
4. `RCH_MOCK_CIRCUIT_OPEN=1 rch exec -- cargo check -p mcp-agent-mail-search-core --all-targets --features hybrid` -> PASS.
5. `rustfmt --edition 2024 --check crates/mcp-agent-mail-search-core/src/fusion.rs crates/mcp-agent-mail-search-core/src/fs_bridge.rs crates/mcp-agent-mail-search-core/src/two_tier.rs` -> PASS.

Live run details:
- run id: `post-cutover-cleanup-agent-mail-20260216T005908Z-1771203548545421330-2664254`
- status: `ok`
- reason code: `post_cutover_cleanup.lane.passed`
- stage accounting: `stage_started_count=6`, `stage_completed_count=6`

## Artifact Locations

1. `test_logs/post_cutover_cleanup/post-cutover-cleanup-agent-mail-20260216T005908Z-1771203548545421330-2664254/summary.json`
2. `test_logs/post_cutover_cleanup/post-cutover-cleanup-agent-mail-20260216T005908Z-1771203548545421330-2664254/events.jsonl`
3. `test_logs/post_cutover_cleanup/post-cutover-cleanup-agent-mail-20260216T005908Z-1771203548545421330-2664254/terminal_transcript.txt`
4. `test_logs/post_cutover_cleanup/post-cutover-cleanup-agent-mail-20260216T005908Z-1771203548545421330-2664254/decommission_replay_command.txt`

## UBS Note

A repo-wide `ubs --diff` run is operationally constrained by repository footprint and baseline scanner debt. A targeted rust scan of search-core was executed and reported pre-existing baseline findings not introduced by this slice. Parent-bead owner should decide final UBS gating policy for closure.
