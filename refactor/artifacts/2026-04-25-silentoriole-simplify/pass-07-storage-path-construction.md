# Pass 7 - Storage Path Construction

## Files Changed
- `crates/mcp-agent-mail-storage/src/lib.rs`
- `refactor/artifacts/2026-04-25-silentoriole-simplify/pass-07-storage-path-construction.md`

## Candidate
- Type II clone in `message_paths`: canonical, outbox, and each inbox path all
  appended the same `YYYY/MM/<filename>.md` suffix after selecting different
  archive base directories.
- Score: `(LOC_saved 2 * Confidence 5) / Risk 1 = 10.0`.

## Simplification
- Added one local `dated_file` closure inside `message_paths`.
- Replaced repeated `.join(&y).join(&m).join(&filename)` chains with calls to
  `dated_file(base)`.

## Isomorphism Card
- Inputs covered: `message_paths` callers through the focused
  `test_write_message_bundle` filter, including canonical, outbox, inbox,
  BCC-redacted inbox, body preservation, path traversal rejection, unsafe extra
  path rejection, and forged-root guards.
- Ordering preserved: yes. `message_paths` still constructs canonical, then
  outbox, then inbox paths in recipient order. `append_message_bundle_files`
  write order is unchanged.
- Error semantics: unchanged. Sender validation still happens before archive
  root lookup and recipient validation still happens inside the recipient loop
  before each inbox path is built.
- BCC redaction: unchanged. Only path construction changed; message rendering
  and inbox redaction code were not touched.
- Git write ordering: unchanged. No commit coalescer, rel path, or enqueue code
  was modified.
- On-disk paths: unchanged. Each base path is the same as before, and the shared
  helper appends the same `year/month/filename` suffix.
- Observable side effects: unchanged. No logs, commits, archive writes, or
  message bundle content generation changed.

## Verification
- `cargo fmt --check`
- `git diff --check`
- `cargo test -p mcp-agent-mail-storage test_write_message_bundle --locked`

## Proof Limits
- Full workspace tests and golden-output hashing were not rerun for this pass:
  the simplification loop baseline already records pre-existing red workspace
  failures, and this pass was intentionally bounded to the storage path suffix
  cleanup.
- Preservation was verified by source-diff inspection plus focused storage
  message-bundle tests that cover the affected path construction, validation,
  BCC redaction, forged-root guards, and message body content.
