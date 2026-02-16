# Gemini Fixes Summary (Part 2) - 2026-02-16

## Critical Bug Fixes

### 1. Git Author Batching Bug in `CommitCoalescer`
- **Component:** `crates/mcp-agent-mail-storage`
- **Issue:** The `CommitCoalescer` blindly batched up to `COALESCER_MAX_BATCH_SIZE` commit requests per repository without checking if they originated from the same git author. `coalescer_commit_batch` then used the author info from the *first* request for the entire batch commit. This meant that if multiple agents (or users) triggered commits to the same repo in rapid succession, the resulting git commit would misattribute authorship for all but the first requester.
- **Fix:** Modified `coalescer_pool_worker` in `crates/mcp-agent-mail-storage/src/lib.rs` to inspect the author of the next candidate item in the queue. If the author (name or email) differs from the current batch, the batch is closed immediately. This ensures every git commit has a single, correct author, preserving audit trail integrity even under high concurrency.

## Verification
- Reviewed `coalescer_commit_batch` to confirm it uses the first request's author, validating that the worker-side split is the correct fix location.
- Verified that the fallback path (sequential processing) correctly respects individual authors.
