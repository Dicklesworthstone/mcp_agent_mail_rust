# Security Policy

Thanks for helping keep MCP Agent Mail safe. This file explains how to report a
vulnerability and summarizes the project's security posture and hardening knobs.

## Reporting a vulnerability

**Please report security issues privately — do not open a public issue for an
unpatched vulnerability.**

- Preferred: use GitHub's **[Private Vulnerability Reporting](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/security/advisories/new)**
  ("Report a vulnerability" under the repository's *Security* tab). This opens a
  confidential advisory thread between you and the maintainer.
- Alternative: email the maintainer (see the GitHub profile for
  [@Dicklesworthstone](https://github.com/Dicklesworthstone)) with `SECURITY
  [mcp_agent_mail_rust]` in the subject.

When reporting, include where possible: affected version/commit, the deployment
shape (loopback-only vs exposed, auth configured or not), a `file:line`
reference or minimal reproduction, and the impact you believe it has. Static
review notes are welcome too — see [#149](https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/149)
for the kind of report that is genuinely useful.

There is no formal SLA (this is a solo-maintained project), but credible reports
are taken seriously and acknowledged as bandwidth allows. Please give a
reasonable window to ship a fix before public disclosure.

## Supported versions

Only the latest released version on `main` is supported. Fixes ship forward in a
new release rather than being backported.

## Threat model (read this first)

The **default and documented deployment model is a single-user or tightly
trusted local operator**: the HTTP server binds to loopback (`127.0.0.1`) and
coordinates the operator's own agents on the same machine. Identity and project
scoping are **advisory by design** in that model — a self-asserted `agent_name`
and a client-chosen `project_key` are conveniences for cooperating local agents,
not a tenancy boundary between mutually untrusted parties.

The full, living threat model — assets, adversaries, attack surfaces, and
mitigations — lives in [`docs/SPEC-threat-model.md`](docs/SPEC-threat-model.md).

## Security posture

- `#![forbid(unsafe_code)]` across all workspace crates — no `unsafe` in
  production code.
- Fully parameterized SQL (`?` binds) throughout the DB crate; no string-built
  query values.
- Git is invoked via libgit2 / argv arrays, never a shell — no command
  injection via message subjects, bodies, or branch names.
- Path inputs are slugified and component-validated against traversal
  (`..`, absolute paths, `.git`) and symlink escape from `STORAGE_ROOT`.
- Markdown → HTML is sanitized with `ammonia` (tag/scheme/attribute allowlists);
  templates use autoescaping.
- HTTP handlers run under a panic firewall (`catch_unwind` + admission control)
  so a malformed request cannot crash the daemon.
- Mutating web-UI (`/mail/`) routes are CSRF-guarded (require
  `Content-Type: application/json` and a same-origin/allowlisted
  `Origin`/`Referer`).
- Baseline security response headers (`X-Content-Type-Options: nosniff`,
  `Referrer-Policy: no-referrer`, `X-Frame-Options: DENY`) ride on every
  response.
- Bearer-token comparison is constant-time.
- `cargo audit` + `cargo deny` run weekly in CI
  ([`.github/workflows/supply-chain-audit.yml`](.github/workflows/supply-chain-audit.yml)),
  and release binaries are checksummed (`SHA256SUMS`) with optional Sigstore
  cosign verification (`install.sh --verify`).

## Hardening for deployments beyond loopback

The frictionless local defaults (no token, permissive dev-mode CORS, rate
limiting off) are deliberate for the trusted-local case. **Before exposing the
server beyond loopback or sharing one instance among less-trusted agents**, you
should:

- Set `HTTP_BEARER_TOKEN` (or enable JWT) — auth is off by default. The server
  logs a loud warning when bound to a non-loopback host with no auth configured.
- Set `APP_ENVIRONMENT=production` to disable development-mode CORS reflection.
- Enable rate limiting (`HTTP_RATE_LIMIT_ENABLED=true`).
- Front the deployment with a vetted reverse proxy terminating TLS and, ideally,
  enforcing auth/rate limits and a strict `Content-Security-Policy`.
- Treat the SQLite DB, Git archive, and env files as plaintext at rest —
  confidentiality relies on OS account and filesystem permissions.

See the [Configuration](README.md#configuration) and
[Limitations](README.md#limitations) sections of the README for the full set of
knobs.
