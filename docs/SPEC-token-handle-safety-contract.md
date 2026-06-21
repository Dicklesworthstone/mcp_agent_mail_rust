# SPEC — Token / Handle Safety Contract

> **Bead:** `br-bvq1x.5.5` (E5). **Origin:** `ntm#126` ("redact misses Agent
> Mail token-handle safety contract"). **Contract version:** `1.0`.
> **Audited:** 2026-06-21.

## Purpose

Downstream tools that surface or redact Agent Mail output — screencast/log
redactors (e.g. NTM), dashboards, paste/share helpers — need an **authoritative,
deterministic** statement of which Agent Mail tokens / handles / identifiers are
safe to show versus which must be hidden. This contract is that statement.

It is intentionally conservative: when in doubt, redact. A downstream tool that
honors this contract will never surface a credential, and will redact
filesystem-structure leaks by default while still being able to show the benign
coordination identifiers (agent names, thread ids, message ids) that make Agent
Mail output useful.

## Classes

| Class | Downstream rule |
|-------|-----------------|
| `SECRET-NEVER-SURFACE` | A credential or key. MUST be redacted in every context (logs, screencasts, bundles, error text, dashboards). Never display, even partially. |
| `SENSITIVE-CONTEXT-REDACT-BY-DEFAULT` | Not a credential, but leaks host/workspace structure or other context an operator may not want public. Redact by default; an operator may opt in to showing it in a trusted context. |
| `SAFE-TO-SURFACE` | A non-secret coordination identifier with no confidentiality value. Safe to display. |

## Classification

### SECRET — NEVER SURFACE

| Token / key | Where defined | Notes |
|-------------|---------------|-------|
| `HTTP_BEARER_TOKEN` | `crates/mcp-agent-mail-core/src/config.rs` | HTTP/MCP auth credential. Leaking it grants full API access. |
| JWT signing secret (`http_jwt_secret`) | `crates/mcp-agent-mail-core/src/config.rs` | Forges/validates JWTs → auth bypass. |
| JWT JWKS material / cached keys | `crates/mcp-agent-mail-core/src/config.rs` | Verification key material; do not log fetched keys. |
| `DATABASE_URL` (path + any creds) | `crates/mcp-agent-mail-core/src/config.rs` | Direct DB access; also a filesystem path. |
| Per-agent registration token (`registration_token`, presented as `sender_token`) | `crates/mcp-agent-mail-db/src/models.rs:148-151` | Per-agent sender-identity secret; leaking it enables impersonation of that agent. |
| Doctor-undo HMAC key | `crates/mcp-agent-mail-cli/src/doctor/manifest.rs` (`~/.config/mcp-agent-mail/doctor-undo-hmac.key`) | Per-install key sealing `am doctor undo` chain-of-custody manifests; leaking it lets an attacker forge a manifest for tampered run artifacts. |
| Age encryption keys | `crates/mcp-agent-mail-share/src/crypto.rs` | Share-bundle encryption key material. |
| Ed25519 manifest signing seed | `crates/mcp-agent-mail-share/src/crypto.rs` | Private key for share-bundle signatures. |

### SENSITIVE CONTEXT — REDACT BY DEFAULT

| Identifier | Where defined | Notes |
|------------|---------------|-------|
| Project `human_key` (absolute path) | `crates/mcp-agent-mail-core/src/models.rs` | Absolute project path; leaks workspace layout and (often) the user home. |
| `STORAGE_ROOT` / archive path | `crates/mcp-agent-mail-core/src/config.rs` | Absolute mailbox-archive path. |
| Config / data / cache paths (XDG, legacy) | `crates/mcp-agent-mail-core/src/config.rs` | Absolute filesystem paths revealing OS/user layout. |
| Message subjects & bodies, attachment names/contents | `crates/mcp-agent-mail-core/src/models.rs` | User content; may contain anything. Redact by default (subjects are operator-toggleable in the support bundle via `--redact-subjects`). |

### SAFE TO SURFACE

| Identifier | Where defined | Notes |
|------------|---------------|-------|
| Project `slug` | `crates/mcp-agent-mail-core/src/models.rs` | Non-secret, path-derived, lowercased; used in URLs/logs. |
| Agent `name` (adjective+noun, e.g. `GreenCastle`) | `crates/mcp-agent-mail-core/src/models.rs` | Public coordination identifier. |
| Agent `program` / `model` | `crates/mcp-agent-mail-core/src/models.rs` | Public capability identifiers, not credentials. |
| Message `id` | `crates/mcp-agent-mail-core/src/models.rs` | Internal sequential PK. |
| `thread_id` (`^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$`) | `crates/mcp-agent-mail-core/src/models.rs` | Conversation grouping (often a `br-###`). |
| `content_sha256` / message digest | `crates/mcp-agent-mail-tools/src/degraded_intents.rs` | Integrity hash; no plaintext leakage. |
| `intent_id` (16-hex prefix of a SHA-256) | `crates/mcp-agent-mail-tools/src/degraded_intents.rs` | Dedup id; not a credential. |
| Pane key (`session:window:pane`) | `crates/mcp-agent-mail-core/src/pane_identity.rs` | TUI pane→agent mapping. |
| Build-slot / file-reservation id | `crates/mcp-agent-mail-core/src/models.rs` | Advisory-lease PK. |
| Contact handle (agent name or project+agent) | `crates/mcp-agent-mail-core/src/models.rs` | Non-secret link identifier. |

> Rule of thumb for unlisted strings: an **absolute filesystem path** →
> `SENSITIVE-CONTEXT`; anything that gates auth, identity, or decryption →
> `SECRET`; a short opaque coordination id or a public name → `SAFE`. When
> genuinely unsure, treat as `SENSITIVE-CONTEXT` (redact).

## How Agent Mail already enforces this internally

This contract documents the policy; two existing surfaces enforce it for the
artifacts Agent Mail itself emits, and are the reference implementations a
downstream redactor can mirror:

- **`am share` scrub** — `crates/mcp-agent-mail-share/src/scrub.rs`: 16
  compiled `SECRET_PATTERNS` (GitHub/Slack/Anthropic/OpenAI/Stripe/AWS/GCP/JWT/
  PEM/… tokens), `ATTACHMENT_REDACT_KEYS` (download_url, authorization,
  signed_url, bearer_token, …), project-path redaction, and three presets
  (standard / strict / archive).
- **`am doctor support-bundle`** — `crates/mcp-agent-mail-cli/src/doctor/mod.rs`:
  `redact_support_text` / `redact_support_json` + key detectors
  (`support_key_is_secret`, `…_is_body`, `…_is_subject`, `…_is_attachment`)
  redact `database_url`, `storage_root`, home paths, tokens, secrets, bodies,
  and attachment names; raw SQLite, message bodies, canonical message files, and
  attachment contents are omitted entirely by design.

A downstream tool should treat the `SECRET-NEVER-SURFACE` regex family above as
the minimum bar and additionally redact the `SENSITIVE-CONTEXT` paths.

## Stability & consumption

- This document (path-stable: `docs/SPEC-token-handle-safety-contract.md`) and
  its `Contract version` are the stable anchor. Downstream tools should vendor
  the classification or pin to this contract version; the class names
  (`SECRET-NEVER-SURFACE`, `SENSITIVE-CONTEXT-REDACT-BY-DEFAULT`,
  `SAFE-TO-SURFACE`) are stable identifiers.
- Any change to a token/handle's class, or any new secret/identifier, MUST bump
  the `Contract version` and update this table. The threat-model change-control
  checklist (`docs/SPEC-threat-model.md`) gates changes that "leak secrets,
  tokens, or message content".
- A machine-readable mirror of this table in `am doctor capabilities --json`
  (so integrations can consume it without parsing markdown) is tracked as a
  follow-up (`br-1aq3f`).

## Cross-references

- `docs/SPEC-threat-model.md` — assets, adversaries, and the secret-handling
  change-control checklist.
- `docs/SPEC-doctor-forensic-bundle-schema.md` — support-bundle redaction policy.
