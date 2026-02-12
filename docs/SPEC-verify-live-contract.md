# Spec: Native Verify-Live Contract

**Bead:** br-y3sk
**Date:** 2026-02-12
**Status:** Active

## Scope

Defines the canonical contract for `am share deploy verify-live <URL>`:
check categories, severity rules, JSON report schema, exit codes, and
timeout/retry semantics. Implementation (br-2cph, br-1td4) and CLI wiring
(br-2y35) must conform to this contract.

## Check Categories

Verification runs in two sequential stages. Local checks (Stage 1) may
short-circuit remote checks (Stage 2) when `--fail-fast` is set and any
local check has `error` severity.

### Stage 1: Local Bundle Integrity

These checks run against the local bundle directory (if `--bundle <path>`
is provided). They reuse `share::deploy::validate_bundle()` output
normalized into the verify-live report schema.

| Check ID | Description | Severity | Short-circuits |
|----------|-------------|----------|----------------|
| `bundle.manifest` | `manifest.json` exists and is valid JSON | error | yes |
| `bundle.index` | `index.html` exists | error | yes |
| `bundle.nojekyll` | `.nojekyll` exists (GitHub Pages) | warning | no |
| `bundle.headers` | `_headers` file exists | warning | no |
| `bundle.viewer` | `viewer/index.html` exists | info | no |
| `bundle.integrity` | SHA256 checksums computed for key files | info | no |

When `--bundle` is omitted, Stage 1 is skipped entirely.

### Stage 2: Remote Endpoint Probes

These checks perform HTTP requests against the deployed URL.

| Check ID | Description | Method | Expected | Severity |
|----------|-------------|--------|----------|----------|
| `remote.root` | Root page accessible | GET `/` | 200 | error |
| `remote.viewer` | Viewer page accessible | GET `/viewer/` | 200 | warning |
| `remote.manifest` | Manifest accessible | GET `/manifest.json` | 200 | error |
| `remote.coop` | `Cross-Origin-Opener-Policy` header present | GET `/` | header exists | warning |
| `remote.coep` | `Cross-Origin-Embedder-Policy` header present | GET `/` | header exists | warning |
| `remote.database` | Database accessible (when bundle has DB) | GET `/mailbox.sqlite3` | 200 | info |
| `remote.content_match` | Root page content matches bundle | GET `/` | SHA256 match | warning |
| `remote.tls` | HTTPS connection succeeds | GET `/` | no TLS error | error |

#### Content Match Semantics

`remote.content_match` compares the SHA256 of the fetched response body
against the local bundle's `index.html` checksum (from `bundle.integrity`).
This check is only evaluated when both `--bundle` is provided and
`remote.root` passes. If the bundle is not provided, this check is
reported as `skipped` (not `failed`).

### Stage 3: Security Header Audit (optional)

When `--security` is passed, additional header checks run. These are
advisory and do not affect the exit code unless `--strict` is also set.

| Check ID | Description | Expected | Severity |
|----------|-------------|----------|----------|
| `security.coop_value` | COOP is `same-origin` | exact match | warning |
| `security.coep_value` | COEP is `require-corp` | exact match | warning |
| `security.corp` | `Cross-Origin-Resource-Policy` header | header exists | info |
| `security.hsts` | `Strict-Transport-Security` header | header exists | info |
| `security.x_content_type` | `X-Content-Type-Options: nosniff` | exact match | info |
| `security.x_frame` | `X-Frame-Options` header | header exists | info |

## Severity Mapping

| Level | Meaning | Affects exit code | JSON value |
|-------|---------|-------------------|------------|
| `error` | Critical failure — deployment is broken or unreachable | yes | `"error"` |
| `warning` | Non-critical issue — deployment works but has gaps | no (unless `--strict`) | `"warning"` |
| `info` | Informational — observation with no action required | no | `"info"` |
| `skipped` | Check precondition not met — not evaluated | no | `"skipped"` |

### Strict Mode

When `--strict` is passed, `warning`-severity checks are promoted to
`error` for exit code purposes. Their JSON severity remains `"warning"`.

## JSON Report Schema

```json
{
  "schema_version": "1.0.0",
  "generated_at": "2026-02-12T05:00:00Z",
  "url": "https://example.github.io/agent-mail",
  "bundle_path": "/path/to/bundle",
  "verdict": "pass",
  "stages": {
    "local": {
      "ran": true,
      "checks": [
        {
          "id": "bundle.manifest",
          "description": "manifest.json exists and is valid JSON",
          "severity": "error",
          "passed": true,
          "message": "manifest.json found, schema version 1.2.0",
          "elapsed_ms": 2
        }
      ]
    },
    "remote": {
      "ran": true,
      "checks": [
        {
          "id": "remote.root",
          "description": "Root page accessible",
          "severity": "error",
          "passed": true,
          "message": "GET / → 200 (142ms)",
          "elapsed_ms": 142,
          "http_status": 200,
          "headers_captured": {
            "content-type": "text/html; charset=utf-8",
            "cross-origin-opener-policy": "same-origin"
          }
        }
      ]
    },
    "security": {
      "ran": false,
      "checks": []
    }
  },
  "summary": {
    "total": 8,
    "passed": 7,
    "failed": 0,
    "warnings": 1,
    "skipped": 0,
    "elapsed_ms": 1247
  },
  "config": {
    "strict": false,
    "fail_fast": false,
    "timeout_ms": 10000,
    "retries": 2,
    "security_audit": false
  }
}
```

### Required Fields

Every check object must include:

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `id` | string | yes | Dotted check identifier (e.g., `remote.root`) |
| `description` | string | yes | Human-readable check description |
| `severity` | string | yes | One of: `error`, `warning`, `info`, `skipped` |
| `passed` | bool | yes | Whether the check passed |
| `message` | string | yes | Result detail (success or failure reason) |
| `elapsed_ms` | u64 | yes | Time taken for this check |

Remote checks additionally include:

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `http_status` | u16 | when applicable | HTTP response status code |
| `headers_captured` | object | when applicable | Relevant response headers |

### Verdict Values

| Value | Meaning |
|-------|---------|
| `"pass"` | All checks passed (or only info/skipped failures) |
| `"warn"` | No error-severity failures, but warning-severity failures exist |
| `"fail"` | At least one error-severity check failed |

## Exit Code Policy

| Code | Condition |
|------|-----------|
| 0 | Verdict is `pass` or `warn` (no error-severity failures) |
| 1 | Verdict is `fail` (at least one error-severity failure) |
| 1 | Verdict is `warn` and `--strict` is set |
| 2 | Usage error (bad arguments, missing URL) |

Exit code 1 enables CI release gates: `am share deploy verify-live $URL || exit 1`.

## CLI Interface

```
am share deploy verify-live <URL> [OPTIONS]

Arguments:
  <URL>  Deployed URL to verify (e.g., https://example.github.io/agent-mail)

Options:
      --bundle <PATH>      Local bundle directory for content-match checks
      --json               Output JSON report instead of human-readable summary
      --strict             Promote warnings to errors for exit code
      --fail-fast          Stop after first error-severity failure
      --security           Run security header audit (Stage 3)
      --timeout <MS>       Per-request timeout in milliseconds [default: 10000]
      --retries <N>        Retry count for failed HTTP requests [default: 2]
      --retry-delay <MS>   Delay between retries in milliseconds [default: 1000]
  -h, --help               Print help
```

### Human-Readable Output

Default output (no `--json`) renders a compact summary:

```
verify-live: https://example.github.io/agent-mail

  [PASS] remote.root         GET / → 200 (142ms)
  [PASS] remote.viewer       GET /viewer/ → 200 (89ms)
  [PASS] remote.manifest     GET /manifest.json → 200 (67ms)
  [WARN] remote.coop         Cross-Origin-Opener-Policy header missing
  [WARN] remote.coep         Cross-Origin-Embedder-Policy header missing
  [PASS] remote.database     GET /mailbox.sqlite3 → 200 (203ms)
  [PASS] remote.tls          HTTPS connection succeeded

Result: 5/7 passed, 2 warnings (1247ms)
```

Severity indicators:
- `[PASS]` — green
- `[WARN]` — yellow
- `[FAIL]` — red
- `[SKIP]` — dim/gray

## Timeout and Retry Semantics

### Per-Request Timeout

Each HTTP request has an independent timeout (default: 10,000ms).
The timeout covers DNS resolution, TCP connect, TLS handshake, and
response body receipt. If a request times out, the check fails with
message `"request timed out after {timeout}ms"`.

### Retry Policy

Failed HTTP requests (connection error, timeout, 5xx status) are retried
up to `--retries` times (default: 2). Each retry waits `--retry-delay`
milliseconds (default: 1,000ms). 4xx responses are NOT retried (client
errors are deterministic).

### Total Timeout

No global timeout is enforced. Worst case for N checks with R retries:
`N * (timeout * (R + 1) + R * retry_delay)`. With defaults (8 remote
checks, 10s timeout, 2 retries, 1s delay): 8 * (10 * 3 + 2 * 1) = 256s.
Callers needing a wall-clock cap should use external timeout wrappers.

## Redirect Policy

HTTP redirects are followed up to 5 hops. The final status code and URL
are reported. If the final URL differs from the requested URL, the check
message includes `"(redirected to {final_url})"`.

HTTPS upgrades (HTTP → HTTPS) are followed silently. Cross-domain
redirects are followed but noted in the message.

## Integration with Existing Code

### Reuse of `DeployCheck`

The existing `share::deploy::DeployCheck` struct is extended with optional
fields for remote checks. The `CheckSeverity` enum gains a `Skipped`
variant. The `VerifyResult` struct is superseded by the new
`VerifyLiveReport` struct defined by this contract.

### Compatibility with `validate_bundle()`

Stage 1 consumes the output of `validate_bundle()` by mapping each
`DeployCheck` into the verify-live check format. The mapping is:
- `check.name` → `"bundle.{name}"` (prefixed)
- `check.severity` → preserved
- `check.passed` → preserved
- `check.message` → preserved

### HTTP Client Requirements (br-2cph)

The HTTP probe module must:
1. Use `asupersync::http::h1::HttpClient` (not reqwest/curl).
2. Support configurable timeout, retry, and redirect policy.
3. Capture status code, relevant headers, and body bytes.
4. Produce structured error reasons (timeout, DNS, TLS, connection refused).
5. Be stateless and reentrant (no shared mutable state between probes).

## Test Requirements

### Unit Tests

1. Severity mapping: `error` → exit 1, `warning` → exit 0, `warning` + strict → exit 1.
2. Verdict computation: all-pass → `pass`, warning-only → `warn`, any-error → `fail`.
3. Check ID uniqueness: no duplicate IDs in a report.
4. JSON schema: output matches schema_version `1.0.0`.
5. Content match: SHA256 comparison with/without bundle.

### Integration Tests

1. Full pipeline with mock HTTP server returning known responses.
2. Timeout behavior: mock server with delayed response.
3. Retry behavior: mock server failing then succeeding.
4. Redirect following: mock server with 301/302 chains.
5. TLS error handling: connection to non-TLS endpoint.

### E2E Tests

1. `am share deploy verify-live` with a real local HTTP server.
2. JSON output parseable and matches schema.
3. Exit code correctness for pass/warn/fail scenarios.
4. `--strict` flag promotes warnings to exit 1.
5. `--fail-fast` stops after first error.
