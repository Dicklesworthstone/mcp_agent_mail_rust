# SPEC: Threat Model

**Status:** living security posture document as of 2026-04-18

## Purpose

This document consolidates the security posture for MCP Agent Mail so reviews do
not have to reconstruct it from scattered README notes, auth tests, installer
docs, share/export specs, and one-off security beads.

It is a threat model for the current Rust implementation, not a formal proof.
The goal is to make tradeoffs explicit:

- what assets matter,
- which adversaries are in scope,
- where the attack surfaces are,
- which mitigations already exist,
- which gaps remain open,
- and which risks are intentionally out of scope for a single-user agent tool.

## Security Goals

The project primarily protects:

- **Confidentiality** of message content, bearer tokens, JWT material, and
  share/export secrets.
- **Integrity** of mailbox state, archive history, reservation state, auth
  decisions, and operator-visible summaries.
- **Availability** of the coordination layer under normal agent load, including
  auth dependencies such as JWKS refresh and storage dependencies such as SQLite
  WAL + Git archive access.

The project does **not** attempt strong multi-tenant isolation on a hostile
shared machine. That boundary is documented below as out of scope.

## Deployment Assumptions

- The default operating model is a **single user or tightly trusted local
  operator environment**.
- HTTP access is expected to be either:
  - localhost-only, or
  - behind a trusted HTTPS terminator / reverse proxy.
- File confidentiality at rest depends on normal OS account boundaries and disk
  controls. The project does not provide transparent archive or SQLite
  encryption by itself.
- Agents with code execution under the same OS account are high-trust by
  default; the system adds guardrails, not full sandboxing.

## Assets

| Asset | Why it matters | Primary risks |
|---|---|---|
| Agent identities | Link activity, ownership, and audit history to specific workers | spoofing, impersonation, misleading audit trails |
| Message bodies and attachments | May contain code, secrets, architecture details, credentials, or operator instructions | disclosure, tampering, exfiltration |
| Reservation state and build slots | Coordinates concurrent edits and serialized build work | false contention, bypass, sabotage, misleading state |
| SQLite operational state | Drives reads, search, tool outputs, metrics, and policy decisions | corruption, unauthorized reads, tampered metadata |
| Git archive history | Human-auditable durable record of messages and mailbox artifacts | tampering, rollback confusion, unintended disclosure |
| Bearer tokens and JWT/JWKS config | Protect HTTP transport and browser/admin routes | token theft, auth bypass, weak-token misuse |
| Share/export bundles | Portable mailbox snapshots may contain redacted or encrypted sensitive material | bundle leakage, unsigned or tampered exports |
| ATC learning and operator snapshots | Summarize agent behavior, policy state, and intervention history | privacy leakage, poisoned feedback, misleading summaries |
| Installer and release artifacts | Bootstrap trust for deployed binaries and shell installer paths | supply-chain substitution, stale or malicious artifact delivery |

## Adversaries

| Adversary | Typical capability | Main goals |
|---|---|---|
| Curious user on the same machine | Can inspect files the OS account exposes to them | read mailbox contents, inspect tokens, learn who is editing what |
| Malicious agent under the same project | Can call tools, write files, and attempt policy abuse | spam, impersonate, bypass reservations, harvest sensitive content |
| Network eavesdropper | Can observe traffic if the deployment is not localhost-only or not behind TLS | steal bearer tokens, read metadata, replay requests |
| Compromised local process | Has local execution and can read/write user files | corrupt SQLite/archive state, poison logs, tamper with config |
| Error-channel exfiltrator | Relies on verbose logs, stack traces, or status pages leaking secrets | extract tokens, request bodies, or sensitive paths |
| Supply-chain attacker | Controls or influences an upstream dependency, installer delivery path, or release asset | execute arbitrary code, alter security-sensitive behavior |
| Physical-access attacker | Has access to the disk or unlocked machine | read archives, DBs, env files, exported bundles |

## Attack Surfaces

### 1. MCP and HTTP transport

The project exposes both stdio MCP and HTTP transport surfaces. The HTTP side
includes the MCP endpoint, `/mail/*` human-facing routes, and the deferred
browser-mirror namespace under `/web-dashboard/*`.

Risks:

- bearer-token theft or accidental disclosure,
- JWT validation mistakes,
- auth bypass on localhost convenience settings,
- route confusion between browser and API response types,
- replay or observation on non-TLS networks.

### 2. Browser-facing web UI

The supported browser surface is the server-rendered `/mail/*` UI. The browser
TUI mirror under `/web-dashboard/*` is **deferred** and should not be treated
as a supported surface until its separate review bead closes.

Risks:

- unauthorized route access,
- HTML/XSS style rendering bugs,
- query-token leakage in logs or browser history,
- stale assumptions if deferred routes are reactivated without a review update.

### 3. Local archive and SQLite storage

Mailbox content lives in Git-backed archive files plus SQLite indexes and WAL
files. This is a rich local attack surface because local processes can target:

- archive traversal and path confusion,
- symlink redirection,
- stale lockfile manipulation,
- WAL or backup artifact leakage,
- direct file reads by other processes on the same machine.

### 4. CLI, config, and env-file handling

The CLI and installer touch persistent config paths, bearer tokens, and local
MCP client configuration.

Risks:

- weak bearer token configuration,
- token leakage via env files or shell history,
- unsafe defaults when operators bypass auth for localhost,
- stale or malicious installer paths.

### 5. Messaging and coordination policy

The messaging surface is intentionally constrained because agent swarms create
new abuse modes.

Risks:

- spam or noisy fan-out,
- forged sender identity,
- reservation bypass or conflict denial,
- accidental leakage via oversized attachments or status chatter.

### 6. Share/export and deploy verification

The share/export system creates portable bundles and optionally signs manifests
or encrypts archives for transport.

Risks:

- exporting sensitive content without adequate scoping,
- tampered bundles,
- unsafe recipient handling for encrypted exports,
- operators trusting unsigned or unverified deployments.

### 7. Installer and release supply chain

The project supports curl-installed flows and downloadable release artifacts.

Risks:

- raw installer substitution,
- compromised release assets,
- operators skipping verification,
- stale mirrors or mismatched checksums.

### 8. ATC learning and operator summaries

ATC is currently **architecture-phase, not learning-phase**. The policy engine,
schema, and operator surfaces exist, but hot-path experience writes and outcome
resolution are not yet the normal production path.

Risks:

- privacy over-collection when the learning loop becomes live,
- poisoned evidence or misleading summaries,
- operators assuming stronger live-learning guarantees than currently exist.

## Existing Mitigations and Residual Gaps

| Surface | Existing mitigation | Residual risk / gap |
|---|---|---|
| HTTP transport | Static bearer auth via `HTTP_BEARER_TOKEN`; optional JWT auth via `HTTP_JWT_SECRET` or `HTTP_JWT_JWKS_URL`; startup checks catch weak/misaligned JWT config; JWKS fetches are cached and bounded | Localhost bypass is intentionally available and must stay opt-in; non-TLS network confidentiality is external to the app |
| `/mail/*` browser UI | Same auth model as HTTP transport; route-specific HTML vs JSON denial behavior is tested | Query-token use remains sensitive in browser history and logs; browser-session hardening is intentionally lightweight |
| `/web-dashboard/*` | Deferred status documented; not a supported surface in current docs | Code still exists and could drift; any reactivation must update this threat model and complete the dedicated browser review |
| Explicit messaging | `broadcast=true` is intentionally rejected to prevent default spam; explicit recipients are required | A malicious agent can still send targeted abusive traffic to approved contacts |
| Sender identity | Registered agent identities and optional sender-token verification | Agents running under the same OS account remain high-trust; this is not cryptographic non-repudiation |
| File reservation integrity | Pre-commit guard plus reservation TTLs reduce accidental overlap and stale locks | Reservations are advisory, not hard isolation; `--no-verify` or direct file writes still bypass the guard |
| Archive path safety | Storage layer rejects symlinked roots/paths and validates repo-relative paths before writes or reads | Local attackers with direct filesystem access can still inspect non-encrypted content permitted by OS permissions |
| Git commit path | Commit coalescing and scoped archive writes reduce uncontrolled archive churn | Archive integrity is audit-friendly but not cryptographically tamper-proof by default; commit signing is not the current baseline |
| SQLite operational state | WAL-backed DB with startup/doctor checks and recovery tooling | No built-in at-rest encryption; local file compromise still exposes state |
| Share/export confidentiality | `age` encryption is supported for zipped exports; manifests can be signed and verified | Safe recipient selection remains operator-dependent; unencrypted exports are still possible by design |
| Installer / release integrity | Installer supports `--verify` with checksum + Sigstore cosign verification; release docs emphasize validation | Verification is optional; users can still curl-pipe without proving provenance |
| Secrets in logs / diagnostics | Startup checks avoid some footguns and many auth errors are normalized | Any future verbose logging change can regress this; reviewers must keep secret-bearing values out of logs and diagnostics |
| ATC data minimization | Current ATC is not yet on the full hot-path learning loop; dedicated privacy follow-up exists under `br-bn0vb.22` | Privacy posture must be revisited before learning rows and outcome resolution become production-default |

## Boundary Notes by Surface

### HTTP auth boundary

- Production-safe posture is bearer or JWT auth on HTTP routes.
- `HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=true` is a convenience mode, not a
  secure deployment baseline.
- JWT mode assumes correct algorithm selection, correct JWKS or shared-secret
  configuration, and trusted upstream network access to the JWKS endpoint.

### Local filesystem boundary

- The storage layer does meaningful hardening against symlink traversal and
  path confusion.
- It does **not** turn same-user local execution into a hostile multi-tenant
  sandbox.
- Archive, SQLite, and env-file confidentiality still depend on OS-level file
  permissions, process isolation, and disk controls.

### Browser mirror boundary

- `/mail/*` is the supported web surface.
- `/web-dashboard/*` is currently a deferred namespace and must not be marketed
  or treated as a security-reviewed production surface.

### Coordination-abuse boundary

- The system explicitly chooses **targeted routing over broadcast**.
- That design reduces spam and accidental disclosure, but it is not equivalent
  to full abuse prevention for malicious local agents.

## Out of Scope

The following are explicitly outside the current security contract:

- Strong multi-user or multi-tenant isolation on a hostile shared machine
- Kernel-level or VM-level sandboxing for agent processes
- Transparent at-rest encryption for SQLite or the Git archive
- End-to-end network encryption without an external TLS terminator
- Full PII compliance programs or regulated-data controls
- Protection against an attacker who already fully owns the same OS account

These are not denied as important; they are simply not promises the current
project posture should imply.

## Review Triggers and Cadence

Review this document:

- at least annually,
- whenever a new public-facing route or transport is added,
- whenever auth semantics change,
- whenever share/export crypto or installer verification changes,
- whenever ATC moves from architecture-phase into live learning-phase,
- and after any real security incident or near miss.

## Related Specs and Beads

- `docs/SPEC-browser-parity-contract-deferred.md`
- `docs/SPEC-doctor-forensic-bundle-schema.md`
- `docs/SPEC-ephemeral-root-policy.md`
- `docs/SPEC-verify-live-contract.md`
- `br-il53l.8` — browser dashboard security review work
- `br-bn0vb.22` — ATC privacy / data-minimization follow-up

## Incident and Lessons Log

No consolidated security-incident lessons are recorded in this spec yet. When a
real incident, escape, or near miss occurs, append a dated subsection here with:

- what happened,
- which asset or boundary failed,
- what changed,
- and which test or checklist item was added to prevent recurrence.

## Review Checklist

Use this checklist for PR review whenever a change touches auth, browser
surfaces, storage, share/export, installer logic, or ATC data collection:

- [ ] Does the change introduce a new externally reachable route or transport?
- [ ] If yes, does this document describe that route and its trust boundary?
- [ ] Does the change alter bearer-token, JWT, or JWKS behavior?
- [ ] Are localhost convenience modes still clearly non-production?
- [ ] Could the change leak secrets, tokens, or message content into logs,
      metrics, panic text, or HTML error pages?
- [ ] Could the change expand archive or SQLite read/write scope beyond the
      intended project root?
- [ ] Are symlink, path-traversal, and file-permission assumptions still valid?
- [ ] If the change touches share/export, are signing and optional encryption
      behaviors still honest and documented?
- [ ] If the change touches ATC, does it increase behavioral data collection or
      operator trust claims beyond current reality?
- [ ] If the change touches `/web-dashboard/*`, was the dedicated browser review
      updated as part of the same work?
- [ ] If this spec became stale because of the change, was it updated in the
      same patch?
