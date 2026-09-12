# Dependency Upgrade Log

## September 12, 2026 — next release (br-xql74)

Status: in progress; no new version, tag, or publication yet. Baseline source
`5d2f6af863de1d021fb6ee862fb5f928f6168d0e`, version 0.3.35.
The older June entries below are historical evidence, not current release gates.

Live registry research covered 89 direct packages: 54 already current and 35
newer packages grouped into 18 coherent upgrade units. Preserve Tantivy's git
revision and the gated FrankenSearch path dependencies. FrankenSQLite 0.3.18
is already latest stable.

- In progress: Comrak 0.54.0 → 0.55.0. Fixes two autolink denial-of-service
  issues ([GHSA-xg9p-p4jc-c46g](https://github.com/kivikakk/comrak/security/advisories/GHSA-xg9p-p4jc-c46g)).
  No use of its deprecated `tagfilter` option was found. Remote Markdown tests
  must pass before the next dependency update.
- Pending, individually tested: indexmap 2.14.2; plist 1.10.1; smallvec 1.16.1;
  toml_edit 0.25.15; tru 0.2.4; franken-agent-detection 0.2.4;
  tokenizers 0.23.2; wide 1.7.0; blake2 0.11.0; dirs 7.0.0;
  winsafe 0.0.29; beads_rust 0.5.12; SQLModel family 0.4.3; FTUI family 0.7.0.
- Constrained: latest FastMCP 0.9.0 requires asupersync exactly 0.4.10 and
  Rust 1.100, while even latest beads_rust requires asupersync exactly 0.4.9.
  The Rust floor is a declared-MSRV concern, not yet a compiler refusal: the
  pinned nightly can be newer than the workspace's advertised Rust 1.95 floor.
  Latest asupersync 0.4.11 satisfies neither exact pin. Keep the coherent
  existing runtime family; do not substitute mutable sibling checkouts.
- Compatible FastMCP candidate: 0.8.1 keeps asupersync exactly 0.4.9 and
  isolates `block_on` runtimes per thread, directly relevant to AM dispatch.
  The installed nightly is Rust 1.100.0-nightly and satisfies its floor.
  Existing explicit-context runner calls appear compatible; compile and real
  transport tests are pending. Both current and candidate FastMCP clients pin
  TOML exactly 1.1.4, preventing a standalone upgrade to TOML 1.1.6.
- Release gates pending: complete workspace tests, check, strict Clippy,
  formatting, security audit, six-platform portable builds, signed artifacts,
  installation/update checks, and applicable distribution venue verification.
- Initial post-Comrak `cargo audit --json` completed with zero vulnerabilities.
  Actual Markdown tests are still compiling remotely; audit success does not
  establish runtime compatibility.
- Venue preflight: GitHub Actions permissions report `enabled=false`.
  Homebrew is already at 0.3.35 with four platform hashes. GHCR `latest` has
  amd64/arm64 images labelled 0.3.34 at source `0125f0505fa0ba604dbbbb580fad56d283e46488`.
  The three probed Agent Mail package names return crates.io 404 and every
  active workspace member remains `publish=false`; gated path/git dependencies
  still prevent treating this as an existing crates.io publication stream.


**Date:** 2026-06-11 | **Project:** mcp_agent_mail_rust | **Language:** Rust

## Summary

- **Updated:** local `/dp` dependency alignment, direct manifest floors, and compatible lockfile refreshes.
- **Skipped:** incompatible major-version lockfile upgrades constrained by upstream dependency ranges; crates.io publishing is intentionally disabled for all workspace members.
- **Failed:** none in the completed compiler, lint, and workspace test gates.
- **Needs attention:** binary release packaging and external distribution verification remain separate release gates.

## Updates

### Local /dp dependency alignment

- `fastmcp*`: `0.3.0` -> `0.3.1` to match `/dp/fastmcp_rust`.
- `sqlmodel*`: `0.2.1` -> `0.2.2` to match `/dp/sqlmodel_rust`.
- `ftui*`: `0.3.1` -> `0.4.0` to match `/dp/frankentui`.
- `beads_rust`: `0.2.6` -> `0.2.7` to match `/dp/beads_rust`.
- Removed the unused `sqlmodel-frankensqlite` patch entry because the active workspace graph does not depend on that package; keeping it caused Cargo patch warnings.

### 2026-06-11 local /dp refresh

- `asupersync`: `0.3.3` -> `0.3.4` to match `/dp/asupersync`.
- `beads_rust`: manifest constraint `0.2.10` -> `0.2.15` to match `/dp/beads_rust`.
- `franken-agent-detection`: `0.1.7` -> `0.1.8` to match `/dp/franken_agent_detection`.
- `frankensearch`: `0.3.0` -> `0.3.2` to match `/dp/frankensearch/frankensearch`.
- `frankensearch-core`, `frankensearch-embed`, `frankensearch-index`, `frankensearch-fusion`: `0.2.0` -> `0.2.1` to match the current local `/dp/frankensearch` crate versions.

### Compatible registry updates

- `tru`/`toon`: `0.2.2` -> `0.2.3`.
- `zip`: manifest floor tightened to `8.6.0` after lockfile resolution selected it.

### Manifest-level latest-stable updates

- `comrak`: `0.50.0` -> `0.52.0`.
- `crossterm`: `0.28.1` -> `0.29.0`.
- `getrandom`: `0.2.17` -> `0.4.2`; call sites now use `getrandom::fill`.
- `insta`: manifest floor tightened from `1.38` to the resolved current `1.47.2`.
- `json5`: `0.4.1` -> `1.3.1`.
- `plist`: `1.8.0` -> `1.9.0`.
- `sha1`: `0.10.6` -> `0.11.0`.
- `sha2`: `0.10.9` -> `0.11.0`.
- `similar`: `2.7.0` -> `3.1.0`.
- `tantivy`: `0.25.0` -> `0.26.1`.
- `tokenizers`: `0.22.2` -> `0.23.1`.
- `unicode-width`: `0.1.14` -> `0.2.2`.

### 2026-06-11 latest-stable direct registry refresh

- `git2`: `0.20.4` -> `0.21.0`; current source uses stable repository/status/diff APIs.
- `toml_edit`: `0.23.10+spec-1.0.0` -> `0.25.12+spec-1.1.0`; current source uses `DocumentMut`, `Item`, `Array`, and `value` APIs.
- `safetensors`: `0.7.0` -> `0.8.0`; no direct source call sites, optional dependency only.
- `wide`: `1.4.0` -> `1.5.0`; no direct source call sites, dependency surface only.

### 2026-06-11 compatible transitive refresh

- Ran `cargo update`, resolving 54 package changes including `chrono`, `dashmap`, `minijinja`, `regex`, `uuid`, `wasm-bindgen`, `zerocopy`, and related transitive crates to their latest compatible stable versions.

### 2026-06-11 final compatible lockfile refresh

- Ran a final `cargo update` after the local `/dp` fixes and workspace test corrections.
- Compatible lockfile updates included `block-buffer` `0.12.0` -> `0.12.1`, `insta` `1.47.2` -> `1.48.0`, `memchr` `2.8.1` -> `2.8.2`, and `smallvec` `1.15.1` -> `1.15.2`.
- Remaining known newer versions are constrained by dependency ranges rather than local pins: `generic-array` `0.14.7` (latest `0.14.9`) and `shlex` `1.3.0` (latest `2.0.1`).

## Verification

- `cargo fmt --check`: passed.
- `cargo check --workspace --all-targets`: passed.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed.
- `cargo test --workspace`: passed.
- `cargo test --workspace --all-features`: passed, including doctests.

## Failed

- None in the dependency update, format, compile, lint, default test, or all-features test pass.

## Needs Attention

- `cargo outdated` cannot inspect this workspace directly because it copies the manifest to a temporary directory where relative `/dp` patches like `../asupersync` resolve to missing paths such as `/tmp/asupersync`. I am using `cargo update --dry-run --verbose`, `cargo metadata`, and direct local manifest checks instead.
- All workspace crates currently set `publish = false`. `.github/workflows/publish.yml` is a manual publishability preflight only and documents that crates.io publication is blocked until unpublished sibling path dependencies are independently published and the workspace publication policy changes.
