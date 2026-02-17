# Cutover: replace Python main with Rust implementation (v2)

## Summary

This PR cuts over the canonical high-star `mcp_agent_mail` repository from the legacy Python implementation to the Rust implementation while preserving:

- existing install URL paths,
- existing user data (`storage.sqlite3` + storage root),
- clear rollback and legacy-code preservation.

Rust migration commands are now available and validated in the Rust repo:

- `am legacy detect`
- `am legacy import`
- `am legacy status`
- `am upgrade`

## Source of Truth for This Cutover

- Transition plan: `AGENT_MAIL_RUST_VERSION_REPO_TRANSITION_PLAN.md`
- Operator runbook: `docs/RUNBOOK_LEGACY_PYTHON_TO_RUST_IMPORT.md`
- Cutover checklist template: `docs/TEMPLATE_OLD_REPO_RUST_CUTOVER_PR_CHECKLIST.md`
- Release notes template: `docs/TEMPLATE_OLD_REPO_RUST_CUTOVER_RELEASE_NOTES.md`

## A. Metadata

- [ ] Cutover owner assigned: `<name>`
- [ ] Backup owner assigned: `<name>`
- [ ] Start time (UTC) recorded: `<YYYY-MM-DDTHH:MM:SSZ>`
- [ ] End time (UTC) recorded: `<YYYY-MM-DDTHH:MM:SSZ>`
- [ ] Python freeze commit SHA recorded: `<sha>`
- [ ] Rust source commit SHA recorded: `<sha>`
- [ ] Cutover branch name recorded: `<branch>`

## B. Pre-Cutover Gates

- [x] `am legacy detect/import/status` implemented in Rust release candidate
- [x] `am upgrade` implemented and documented
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
- [ ] Installer invokes migration workflow (`am upgrade --yes` or `am legacy import --auto --yes`)
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

Define thresholds before merge:

- [ ] Import failure rate threshold documented
- [ ] Install failure threshold documented
- [ ] Data integrity regression threshold documented
- [ ] SLO/latency regression threshold documented

## J. Rollback Command Ledger

- [ ] Repoint `main` to pre-cutover SHA
- [ ] Restore Python installer entrypoint
- [ ] Publish rollback advisory
- [ ] Keep incident timeline with UTC timestamps

## K. Post-Cutover Stabilization (T+2 weeks)

- [ ] Daily issue triage for migration/import label
- [ ] Weekly import success metrics summary
- [ ] Patch release process documented
- [ ] Final migration retrospective filed

## Cutover Execution Commands

```bash
# 1) Freeze Python main and preserve state
git checkout main
git pull --ff-only origin main
git tag -a python-final-v1.x -m "Final Python snapshot before Rust cutover"
git push origin python-final-v1.x
git branch -f legacy-python HEAD
git push origin legacy-python

# 2) Bring Rust code into cutover branch
git remote add rust-source <rust-repo-url>
git fetch rust-source
git checkout -b cutover/rust-v2
# (import strategy: subtree/squash/overlay according to team policy)

# 3) Validate critical commands
am legacy detect --json
am legacy import --auto --dry-run --yes
am upgrade --yes

# 4) Promote
# merge cutover branch -> main
git push origin main:master
```

## User-Facing Upgrade Commands (for release notes and pinned issue)

```bash
# Recommended inspect-first path
am legacy detect --json
am legacy import --auto --dry-run --yes

# One-command upgrade
am upgrade --yes

# Check migration receipt
am legacy status --json
```
