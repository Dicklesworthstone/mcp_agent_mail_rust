# Conformance Fixtures (Python MCP Agent Mail)

This directory contains fixture-based conformance tests that compare Rust outputs
against the legacy Python MCP Agent Mail behavior.

Alongside the Python reference lane, the Rust harness also loads dedicated
Rust-native fixtures from `tests/conformance/fixtures/rust_native/*.json` for
tools that intentionally have no Python analogue.

## Fixture Schema

```json
{
  "version": "legacy-python@0.3.0",
  "generated_at": "ISO-8601",
  "tools": {
    "health_check": {
      "cases": [
        {
          "name": "default_env",
          "input": {},
          "expect": {
            "ok": {
              "status": "ok",
              "environment": "development",
              "http_host": "127.0.0.1",
              "http_port": 8765,
              "database_url": "sqlite:///./storage.sqlite3"
            }
          }
        }
      ]
    }
  },
  "resources": {
    "resource://config/environment": {
      "cases": [
        {
          "name": "default_env",
          "input": {},
          "expect": {
            "ok": {
              "environment": "development",
              "database_url": "sqlite:///./storage.sqlite3",
              "http": { "host": "127.0.0.1", "port": 8765, "path": "/api/" }
            }
          }
        }
      ]
    }
  }
}
```

Notes:
- Each tool/resource can have multiple `cases` (happy path + error cases).
- `input` is the tool args object (for tools) or resource query input (for resources).
- `expect` must contain exactly one of `ok` or `err`.

## Rust-native Fixture Schema

Rust-native fixtures keep the same router execution path, but compare against
Rust-owned golden outputs instead of Python parity data.

```json
{
  "version": "rust-native@2026-04-18",
  "generated_at": "ISO-8601",
  "tool": "resolve_pane_identity",
  "classification": "rust_native",
  "cases": [
    {
      "name": "known_pane_canonical_identity",
      "input": {
        "project_key": "__FIXTURE_ROOT__/projects/resolve-known",
        "pane_id": "main:0:2"
      },
      "setup": {
        "identity_writes": [
          {
            "project_key": "__FIXTURE_ROOT__/projects/resolve-known",
            "pane_id": "main:0:2",
            "agent_name": "BlueLake"
          }
        ]
      },
      "expect": {
        "ok_golden_output_path": "rust_native/resolve_pane_identity/known_pane_canonical_identity.output.json"
      }
    }
  ]
}
```

Rust-native `setup` supports:
- `tool_calls`: seed the database/archive through real MCP tool dispatch
- `identity_writes`: create canonical pane identity files before the tool runs
- `tmux_list_panes_lines`: feed a deterministic fake `tmux list-panes` output to cleanup cases

## Generating Fixtures (Python Reference)

Preferred (Rust wrapper, still runs the Python generator under the hood):

```bash
cargo run -p mcp-agent-mail-conformance -- regen
```

By default, `regen` writes fixtures to a timestamped path under the OS temp dir
(so it does not implicitly dirty the repo).

To update the tracked fixtures in-repo, pass `--output` explicitly:

```bash
cargo run -p mcp-agent-mail-conformance -- regen \
  --output crates/mcp-agent-mail-conformance/tests/conformance/fixtures/python_reference.json
```

Write fixtures to an explicit temp file path:

```bash
cargo run -p mcp-agent-mail-conformance -- regen --output /tmp/python_reference.json
```

The wrapper sets `MCP_AGENT_MAIL_CONFORMANCE_FIXTURE_PATH` for the Python generator.

Scratch outputs (legacy server storage + DB) are also written outside the repo by default.
To keep scratch artifacts in a known location for debugging, use:

```bash
cargo run -p mcp-agent-mail-conformance -- regen --scratch-root /tmp/am-conformance-scratch
```

Direct (legacy venv):

```
legacy_python_mcp_agent_mail_code/mcp_agent_mail/.venv/bin/python \
  crates/mcp-agent-mail-conformance/tests/conformance/python_reference/generate_fixtures.py
```

Notes:
- Use the legacy project venv Python. The generator imports `mcp_agent_mail`, which is not available in the system `python3` env.

The generator should:
- Start legacy Python MCP Agent Mail in a controlled mode.
- Call each tool and resource endpoint.
- Record JSON output for parity comparisons.

## Automated Refresh Pipeline

For upstream drift checks against the live Python reference repo, use the repo-level wrapper:

```bash
bash scripts/regen_python_parity_fixtures.sh --dry-run
```

Real run, keeping outputs in temp files:

```bash
bash scripts/regen_python_parity_fixtures.sh
```

Apply the refreshed fixture set into the tracked paths:

```bash
bash scripts/regen_python_parity_fixtures.sh --apply
```

What the wrapper does:
- clones or refreshes `https://github.com/Dicklesworthstone/mcp_agent_mail`
- builds an isolated venv and installs the upstream Python package
- runs `python_reference/generate_fixtures.py` against that upstream checkout
- emits a reduced `scenario_catalog.json` manifest alongside a markdown drift report
- only copies the tracked files when the fixture/catalog payload actually changed

The scheduled GitHub Actions lane lives at `.github/workflows/conformance-fixture-regen.yml`.
It runs weekly and opens a review PR when drift is detected; it does not auto-merge.

## Running Conformance Tests

```
cargo test -p mcp-agent-mail-conformance
```
