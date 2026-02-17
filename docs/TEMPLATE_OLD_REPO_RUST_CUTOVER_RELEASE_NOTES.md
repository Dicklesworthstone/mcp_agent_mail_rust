# Template: Release Notes for Rust Cutover (`v2.0.0`)

## MCP Agent Mail `v2.0.0`: Rust Is Now Canonical

This release switches the canonical `mcp_agent_mail` repository from the legacy Python implementation to the Rust implementation.

## What This Means

- The default `main` branch is now Rust.
- Existing install URLs continue to work.
- Existing Python-era mailbox data can be imported with built-in CLI commands.
- The legacy Python code remains preserved on branch `legacy-python` and tag `python-final-v1.x`.

## Fast Upgrade Path

### One command

```bash
am upgrade --yes
```

### Inspect first (recommended)

```bash
am legacy detect --json
am legacy import --auto --dry-run --yes
```

### Perform import explicitly

```bash
am legacy import --auto --yes
```

## New Migration Commands

- `am legacy detect`
- `am legacy import`
- `am legacy status`
- `am upgrade`

## Compatibility Notes

- Native command surface: `am` (CLI) and `mcp-agent-mail` (MCP server).
- Existing installer URL remains stable.
- Legacy wrapper behavior, if any, is transitional and documented.

## Breaking / Behavior Changes

- [Fill in behavior changes relevant to this cutover]
- [Fill in removed Python-only paths, if any]

## Rollback Guidance

If migration/import fails in your environment:

1. Check latest receipt and backup location:

```bash
am legacy status --json
```

2. Restore backup DB and storage root from the receipt's `backup_root`.
3. Validate:

```bash
am doctor check --json
```

## Known Issues

- [List known issues with workarounds]

## Reporting Migration Problems

Please include all of the following:

- OS + architecture
- Exact command run
- Full stderr/stdout
- `am legacy detect --json` output
- `am legacy status --json` output
- Whether you used in-place or copy mode

## Acknowledgments

Thank you to everyone who used and stress-tested the Python version and helped drive this rewrite.
