# Corrupted-DB Fixture Corpus

This corpus is the shared source of truth for the Agent Mail SQLite
corruption incidents captured in `br-bvq1x.12.1`. It is intentionally
recipe-first: tests materialize small databases and sidecars in an
isolated temp directory, never in a live mailbox root.

## Contract

- `manifest.json` lists every fixture, its incident anchor, expected
  Track A classification, and the canonical SQLite verdict.
- `recipes/*.sql` and text recipes are deterministic inputs used by
  tests to materialize fixture artifacts.
- Paths in the manifest are relative to the materialization root and must
  start with their fixture id directory. Absolute paths and `..` components
  are invalid.
- No recipe may point at `~/.mcp_agent_mail_git_mailbox_repo` or any
  other real operator mailbox.

## Materialization

The loader in `crates/mcp-agent-mail-cli/tests/corruption_corpus.rs`
executes SQL recipes with `sqlmodel_sqlite`, writes deterministic byte
fixtures, and creates symlink fixtures on Unix. Future Track A, M1, B6,
and F4 tests should consume this manifest instead of creating new
one-off corruption fixtures.
