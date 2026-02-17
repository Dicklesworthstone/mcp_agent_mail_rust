# Template: Old Repo Rust Cutover PR Checklist

Use this checklist in the canonical high-star Python repo (`mcp_agent_mail`) when replacing `main` with the Rust implementation.

PR title recommendation:

`Cutover: replace Python main with Rust implementation (v2)`

## A. Metadata

- [ ] Cutover owner assigned
- [ ] Backup owner assigned
- [ ] Start time (UTC) recorded
- [ ] End time (UTC) recorded
- [ ] Python freeze commit SHA recorded
- [ ] Rust source commit SHA recorded
- [ ] Cutover branch name recorded

## B. Pre-Cutover Gates

- [ ] `am legacy detect/import/status` implemented in Rust release candidate
- [ ] `am upgrade` implemented and documented
- [ ] Canary installs validated on Linux x86_64
- [ ] Canary installs validated on Linux aarch64
- [ ] Canary installs validated on macOS x86_64
- [ ] Canary installs validated on macOS arm64
- [ ] Real legacy data import validated on at least 3 environments
- [ ] Rollback drill executed end-to-end

## C. Git Topology Preservation

- [ ] Create immutable Python final tag (`python-final-v1.x`)
- [ ] Create/refresh `legacy-python` branch from frozen Python commit
- [ ] Import Rust tree into cutover branch
- [ ] Verify default branch target is `main`
- [ ] Ensure compatibility mirror remains synchronized (`main -> master`)

Command reminder:

```bash
git push origin main:master
```

## D. Installer + Entrypoint Compatibility

- [ ] Old repo `scripts/install.sh` replaced with Rust installer bootstrap
- [ ] Existing public install URL still works unchanged
- [ ] Installer places `am` and `mcp-agent-mail` binaries correctly
- [ ] Installer optionally invokes migration workflow (`am upgrade` or `am legacy import --auto`)
- [ ] Installer runs setup refresh (`am setup run --yes`)

## E. Documentation Cutover

- [ ] README updated to explain Rust-first status
- [ ] Migration section added with one-command upgrade flow
- [ ] Legacy Python branch/tag clearly documented
- [ ] Rollback instructions included
- [ ] Breaking changes called out explicitly

## F. Validation in Cutover Branch

- [ ] Build passes
- [ ] Smoke tests pass
- [ ] Install-from-URL test passes
- [ ] Legacy import dry-run passes
- [ ] Legacy import in-place pass
- [ ] Legacy import copy-mode pass
- [ ] `am legacy status` receipt verification pass

## G. Release Execution

- [ ] Publish `v2.0.0-rc1` (if staged release)
- [ ] Monitor incoming issues for 24h
- [ ] Publish `v2.0.0` GA
- [ ] Pin migration guidance issue/discussion

## H. Go/No-Go Sign-Off

- [ ] Engineering sign-off
- [ ] Operations sign-off
- [ ] Documentation sign-off
- [ ] Rollback owner sign-off

## I. Rollback Trigger Conditions

Define concrete thresholds before merge:

- [ ] Import failure rate threshold documented
- [ ] Install failure threshold documented
- [ ] Data integrity regression threshold documented
- [ ] SLO/latency regression threshold documented

## J. Rollback Command Ledger

If rollback is triggered, record exactly what was run:

- [ ] Repoint `main` to pre-cutover SHA
- [ ] Restore Python installer entrypoint
- [ ] Publish rollback advisory
- [ ] Keep incident timeline with UTC timestamps

## K. Post-Cutover Stabilization (T+2 weeks)

- [ ] Daily issue triage for migration/import label
- [ ] Weekly import success metrics summary
- [ ] Patch release process documented
- [ ] Final migration retrospective filed
