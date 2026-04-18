# SPEC: Threat Model

**Status:** living security posture document as of 2026-04-18

This document consolidates the security posture for MCP Agent Mail so security
reviews do not have to reconstruct it from scattered auth notes, installer
docs, share/export specs, ATC design work, and one-off beads.

It is a threat model for the current Rust implementation, not a formal proof.
The default deployment assumption is a single-user or tightly trusted local
operator environment. HTTP is expected to be localhost-only or behind a trusted
HTTPS terminator. SQLite, the Git archive, and env files rely on normal OS
account boundaries and disk controls for confidentiality at rest.

## Assets

| Asset | Why it matters | Primary risks |
|---|---|---|
| Agent identities | Link actions, ownership, and audit history to workers | spoofing, impersonation, misleading audit trails |
| Message content and attachments | May contain code, secrets, credentials, architecture details, or PII | disclosure, tampering, exfiltration |
| Reservation state and build slots | Coordinates concurrent edits and serialized build work | false contention, sabotage, misleading state |
| ATC learning data | Aggregates agent behavior, intervention history, and operator summaries over time | privacy leakage, poisoned feedback, misleading summaries |
| Bearer tokens and JWT signing / verification material | Protect HTTP transport and browser-facing routes | token theft, auth bypass, replay, weak-token misuse |
| Git archive history | Durable audit record of messages, reservations, and identities | tampering, rollback confusion, unintended disclosure |
| SQLite operational state | Drives reads, search, metrics, and policy-visible summaries | corruption, unauthorized reads, tampered metadata |
| Share/export bundles | Portable mailbox snapshots may contain redacted or encrypted sensitive material | bundle leakage, unsigned export trust, recipient mistakes |
| Installer and release artifacts | Bootstrap trust for deployed binaries and shell installer flows | supply-chain substitution, stale or malicious artifact delivery |

## Adversaries

| Adversary | Typical capability | Main goals |
|---|---|---|
| Curious user on the same machine | Can inspect files the OS account exposes to them | read other users' mailboxes, inspect tokens, learn who is editing what |
| Malicious agent | Can call tools, write files, and attempt policy abuse | spam others, impersonate senders, bypass reservations, harvest sensitive content |
| Network eavesdropper | Can observe traffic if deployment is not localhost-only or not behind TLS | steal bearer tokens, read metadata, replay requests |
| Compromised local process | Has local execution and can read or write user files | corrupt SQLite or archive state, poison ATC ledger, tamper with config |
| Exfiltration via error channels | Relies on verbose logs, stack traces, or status pages leaking secrets | extract tokens, request bodies, sensitive paths, or message content |
| Supply-chain attacker | Controls or influences an upstream dependency, installer delivery path, or release artifact | execute arbitrary code, alter security-sensitive behavior |
| Physical-access attacker | Has access to the disk or unlocked machine | read archives, databases, env files, or exported bundles |

## Attack Surfaces

| Surface | Trust boundary | Main risks |
|---|---|---|
| MCP transport (`stdio`, HTTP MCP) | Client-to-server command execution boundary | auth bypass, bearer-token disclosure, request replay, route confusion |
| `/mail/*` web UI routes | Browser-facing human oversight surface authenticated via bearer or JWT | unauthorized access, XSS-style rendering bugs, token leakage via logs or browser history |
| `/web-dashboard/*` routes | Deferred browser-mirror namespace, not a supported production surface today | stale unaudited code path, accidental reactivation without security review |
| CLI commands reading config or archive state | Local operator and agent shell boundary | token leakage via env files or shell history, over-broad local reads |
| Git archive files and permissions | Filesystem boundary for durable mailbox artifacts | symlink/path confusion, local disclosure, tampering |
| SQLite files, WAL, and backup or restore paths | Local persistence boundary for indexed state | local disclosure, corruption, stale WAL leakage, tampered metadata |
| Installer script (`curl | bash`) | Bootstrapping trust path for first install or upgrade | installer substitution, skipped provenance checks, stale mirrors |
| Release artifacts and signatures | Trust path for downloadable binaries | malicious artifact delivery, missing verification, checksum drift |
| ATC data collection and summaries | Behavioral telemetry and operator-trust boundary | privacy over-collection, poisoned evidence, misleading summaries |

## Mitigations (Existing + Gaps)

| Surface | Existing mitigation | Current coverage / gap |
|---|---|---|
| HTTP transport | Bearer token via `HTTP_BEARER_TOKEN`; localhost unauthenticated mode is explicit opt-in | `✓` Auth required by default; network confidentiality still depends on localhost-only deployment or external TLS |
| JWT auth | Shared-secret or JWKS verification with `kid` handling, startup validation, and regression tests | `✓` Present; JWT configuration mistakes remain operator-sensitive and require review when auth semantics change |
| `/mail/*` routes | Same auth boundary as HTTP transport plus tested route-specific denial behavior | `✓` Present; browser token handling and rendered-output review remain ongoing responsibilities |
| `/web-dashboard/*` routes | Deferred status documented; not a supported reviewed surface in current docs | Gap by design; any reactivation must update this threat model and complete `br-il53l.8` or successor review work |
| Broadcast messaging | `broadcast=true` is intentionally hard-rejected | `✓` Critical invariant; do not remove or weaken |
| Sender identity | Registered identities and optional sender-token verification | Partial; not cryptographic non-repudiation for agents sharing the same OS account |
| Pre-commit guard | Reservation enforcement at commit time plus reservation TTLs | `✓` Defense in depth; reservations remain advisory and direct writes or `--no-verify` can still bypass guardrails |
| Git archive integrity | Repo-relative path validation, scoped archive writes, audit-friendly history | Partial; Git commit signing is not the current baseline and tamper evidence depends on repo hygiene |
| SQLite at-rest protection | OS-level file permissions and disk controls are the documented baseline | Partial; no built-in database encryption |
| Secrets in logs and diagnostics | Many auth errors are normalized and startup checks catch some footguns | Partial; verbose logging changes can still leak secrets if reviewers are careless |
| ATC data minimization | ATC learning is a validated opt-in hot path (`shadow`/`live` via `AM_ATC_WRITE_MODE`); privacy follow-up tracked in `br-bn0vb.22` | Partial; durable learning exists, but redaction/minimization work should land before any default-on promotion |
| Installer integrity | `install.sh` supports `--verify` with checksum and Sigstore cosign validation | `✓` Available; provenance checks are still optional and operators can skip them |
| Supply chain | Explicit `Cargo.lock`, release process, and documented verification paths | Partial; recommend recurring `cargo audit` / dependency review cadence |

## Out of Scope

The following are explicitly outside the current security contract:

- Strong multi-user or multi-tenant isolation on a hostile shared machine
- Kernel-level or VM-level sandboxing for agent processes
- Transparent at-rest encryption for SQLite or the Git archive
- Network-level encryption without an external TLS terminator
- Persistent PII compliance programs or regulated-data controls
- Protection against an attacker who already fully owns the same OS account

These are not denied as important. They are simply not promises the current
project should imply.

## Review Cadence

Review this document:

- at least annually,
- whenever a new public-facing endpoint, transport, or browser route lands,
- whenever bearer-token, JWT, or JWKS behavior changes,
- whenever share/export crypto or installer verification changes,
- whenever ATC write-mode defaults, rollout semantics, or data-minimization policy change,
- and after any real security incident or near miss.

Security incidents should append a dated lessons entry with:

- what happened,
- which asset or boundary failed,
- what changed,
- and which test or checklist item was added to prevent recurrence.

### Related Specs and Beads

- `docs/SPEC-browser-parity-contract-deferred.md`
- `docs/SPEC-doctor-forensic-bundle-schema.md`
- `docs/SPEC-ephemeral-root-policy.md`
- `docs/SPEC-verify-live-contract.md`
- `br-il53l.8` — browser dashboard security review work
- `br-bn0vb.22` — ATC privacy / data-minimization follow-up

### Review Checklist

Use this checklist for PR review whenever a change touches auth, browser
surfaces, storage, share/export, installer logic, or ATC data collection:

- [ ] Does the change introduce a new externally reachable route or transport?
- [ ] If yes, does this document describe that route and its trust boundary?
- [ ] Does the change alter bearer-token, JWT, or JWKS behavior?
- [ ] Are localhost convenience modes still clearly non-production?
- [ ] Could the change leak secrets, tokens, or message content into logs, metrics, panic text, or HTML error pages?
- [ ] Could the change expand archive or SQLite read or write scope beyond the intended project root?
- [ ] Are symlink, path-traversal, and file-permission assumptions still valid?
- [ ] If the change touches share/export, are signing and optional encryption behaviors still honest and documented?
- [ ] If the change touches ATC, does it increase behavioral data collection or operator trust claims beyond current reality?
- [ ] If the change touches `/web-dashboard/*`, was the dedicated browser review updated as part of the same work?
- [ ] If this spec became stale because of the change, was it updated in the same patch?
