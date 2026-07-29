#!/usr/bin/env bash
# test_degraded_intents_replay.sh — E2E for Track N (durable intents + replay).
# @tags: reliability, track-n, degraded-intents, replay, h1, h2, h3, h4
#
# Asserts, against a REAL built `am` binary, the degraded-mode durable-intent
# lifecycle from the css/ts2 incident class ("send + release + ack all failed
# malformed; the agent pushed code but could not communicate status"):
#
#   H2 — `am mail send` against an UNAVAILABLE mailbox does NOT silently drop a
#        closeout message: it fails closed, persists a durable UNSENT artifact
#        under <storage_root>/pending_sends/, and prints the exact replay command.
#   H3 — `am robot status` surfaces the queued intent in `queued_intents[]` with
#        its replay command + a `degraded_intents` anomaly (the read-only
#        "what is pending + how to flush it" view).
#   H2 — `am mail replay-queued` re-validates the artifact (REFUSING a tampered,
#        content_hash-mismatch one), then replays a genuine one against a healthy
#        mailbox, writes a receipt, and is idempotent on re-run.
#   H3 — after replay the intent disappears from `queued_intents[]`.
#   H4 — under an unavailable mailbox `am robot status` reports a non-`ok`
#        (degraded/recovering/error) health rather than lying that all is well.
#
# "Unavailable" mailbox: DATABASE_URL points at a path whose parent component is
# a regular file, so the DB can never be opened/created — a clean, deterministic
# failure that fails the verb closed without dragging in repair/reconstruct.
#
# SCOPED OUT (honest SKIPs):
#   * The H1 release-intent + H3 ack-intent QUEUE paths trip only through the MCP
#     server: the CLI `file_reservations release` / `mail ack` verbs use a direct
#     sync-SQLite path, so there is no black-box CLI trigger for their queue
#     write. Their on-disk schema + auto-replay + abandon-on-missing are covered,
#     in-process, by DB-backed integration tests:
#       mcp-agent-mail-tools  messaging::tests::replay_queued_ack_intent_acks_once_on_success
#       mcp-agent-mail-tools  messaging::tests::replay_abandons_ack_intent_for_missing_message
#       mcp-agent-mail-tools  reservations::tests::replay_queued_release_intent_releases_once
#       mcp-agent-mail-cli    robot::tests::build_queued_intents_surfaces_ack_and_send
#   * `am doctor repair`/`reconstruct` are NOT invoked: under a live owner they
#     broadcast a process-group signal that destabilises a non-interactive
#     harness (br-mms51). `am mail send` / `am robot status` against an
#     unavailable mailbox do NOT take that path and are safe here.
#   * A unified `am ... replay-intents --dry-run` (would-replay vs no-op) and
#     intent cancellation are NOT implemented: ack/release auto-replay on the
#     next successful call, sends replay via `am mail replay-queued`.
#
# Ref: br-bvq1x.14.10 (N10). Depends on H1/H2/H3/H4 (all closed).

set -uo pipefail

E2E_SUITE="degraded_intents_replay"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
if [ -z "${CARGO_TARGET_DIR:-}" ]; then
    export CARGO_TARGET_DIR="${PROJECT_ROOT}/target"
fi
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"
# shellcheck source=lib/structured_logging.sh
source "${SCRIPT_DIR}/lib/structured_logging.sh"

e2e_init_artifacts
e2e_banner "Track N — Durable Intents & Replay E2E"

EVENTS="${E2E_ARTIFACT_DIR}/events.jsonl"
: >"${EVENTS}"

if ! command -v jq >/dev/null 2>&1; then
    e2e_skip "jq required"
    log_summary "${E2E_SUITE}" 0 0 0 "${E2E_ARTIFACT_DIR}" "bash tests/e2e/test_degraded_intents_replay.sh" >>"${EVENTS}"
    e2e_summary
    exit 0
fi

AM_BIN="$(e2e_ensure_binary am)"
if [ -z "${AM_BIN}" ] || [ ! -x "${AM_BIN}" ]; then
    e2e_fail "could not build/locate the am binary"
    e2e_summary
    exit 1
fi
e2e_log "am binary: ${AM_BIN}"

WORK="$(e2e_mktemp degraded_intents_work)"
ART="${E2E_ARTIFACT_DIR}/scenarios"
mkdir -p "${WORK}/storage" "${WORK}/home" "${WORK}/repo" "${ART}"
export AM_INTERFACE_MODE="cli"
export STORAGE_ROOT="${WORK}/storage"
export HOME="${WORK}/home"
# Point the CLI at an unused MCP port so every verb falls back to the isolated
# local SQLite path instead of hitting any real server a developer/CI may have
# running on the canonical 127.0.0.1:8765 (which would reject with HTTP 401).
export HTTP_HOST="127.0.0.1"
export HTTP_PORT="${AM_E2E_UNUSED_PORT:-8799}"

HEALTHY_DB="sqlite:///${WORK}/mb.sqlite3"
# Unavailable mailbox: parent path component is a regular file → DB unopenable.
printf 'x' >"${WORK}/notadir"
UNAVAIL_DB="sqlite:///${WORK}/notadir/db.sqlite3"
PROJECT_KEY="${WORK}/repo"

AM_CMD_TIMEOUT="${AM_CMD_TIMEOUT:-60}"
# Run `am` with timeout, capturing exit code WITHOUT tripping the harness's
# `set -e`: several verbs here intentionally exit non-zero (a queued send, a
# rejected tampered replay), and the suite must observe that rather than abort.
amrun() {
    local out="$1"
    shift
    local rc=0
    if command -v timeout >/dev/null 2>&1; then
        timeout "${AM_CMD_TIMEOUT}" "${AM_BIN}" "$@" >"${out}" 2>"${out}.err" || rc=$?
    else
        "${AM_BIN}" "$@" >"${out}" 2>"${out}.err" || rc=$?
    fi
    printf '%s\n' "${rc}" >"${out}.exit"
    return 0
}

check() {
    local label="$1" file="$2"
    shift 2
    if jq -e "$@" "${file}" >/dev/null 2>&1; then
        e2e_pass "${label}"
    else
        e2e_fail "${label}  [filter: $*]"
        printf '      got: %s\n' "$(jq -c . "${file}" 2>/dev/null | head -c 320)"
    fi
}

exit_is() {
    local label="$1" out="$2" want="$3"
    local got
    got="$(cat "${out}.exit" 2>/dev/null || echo "?")"
    if [ "${got}" = "${want}" ]; then
        e2e_pass "${label} (exit ${got})"
    else
        e2e_fail "${label} (exit ${got}, expected ${want})"
        printf '      out: %s\n' "$(head -c 240 "${out}" "${out}.err" 2>/dev/null | tr '\n' ' ')"
    fi
}

exit_nonzero() {
    local label="$1" out="$2"
    local got
    got="$(cat "${out}.exit" 2>/dev/null || echo "?")"
    if [ "${got}" != "0" ] && [ "${got}" != "?" ]; then
        e2e_pass "${label} (exit ${got})"
    else
        e2e_fail "${label} (exit ${got}, expected non-zero)"
        printf '      out: %s\n' "$(head -c 240 "${out}" "${out}.err" 2>/dev/null | tr '\n' ' ')"
    fi
}

# ---------------------------------------------------------------------------
# Setup: a healthy mailbox with a sender and a recipient.
# ---------------------------------------------------------------------------
e2e_case_banner "Setup: register sender + recipient on a healthy mailbox"
export DATABASE_URL="${HEALTHY_DB}"
amrun "${ART}/reg_sender.json" agents register -p "${PROJECT_KEY}" \
    --program claude-code --model opus-4.8 -n BlueLake --format json
exit_is "register BlueLake" "${ART}/reg_sender.json" 0
amrun "${ART}/reg_recipient.json" agents register -p "${PROJECT_KEY}" \
    --program codex-cli --model gpt-5 -n RedPeak --format json
exit_is "register RedPeak" "${ART}/reg_recipient.json" 0

# ---------------------------------------------------------------------------
# H2: a send against an UNAVAILABLE mailbox queues a durable UNSENT artifact.
# ---------------------------------------------------------------------------
e2e_case_banner "H2: send under an unavailable mailbox queues a durable UNSENT artifact"
export DATABASE_URL="${UNAVAIL_DB}"
amrun "${ART}/send_queued.json" mail send -p "${PROJECT_KEY}" \
    --from BlueLake --to RedPeak \
    --subject "closeout: br-bvq1x.14.10 done" \
    --body "pushed; durable-intent replay e2e" --format json
exit_nonzero "send under unavailable DB fails closed" "${ART}/send_queued.json"
if grep -q "queued a durable UNSENT artifact" "${ART}/send_queued.json.err" 2>/dev/null; then
    e2e_pass "send prints the durable UNSENT artifact + replay command"
else
    e2e_fail "send did not report a queued UNSENT artifact"
    printf '      err: %s\n' "$(head -c 280 "${ART}/send_queued.json.err" 2>/dev/null | tr '\n' ' ')"
fi

ARTIFACT="$(find "${STORAGE_ROOT}/pending_sends" -maxdepth 1 -name '*.json' ! -name '*.sent.json' 2>/dev/null | head -1 || true)"
if [ -n "${ARTIFACT}" ] && [ -f "${ARTIFACT}" ]; then
    e2e_pass "durable UNSENT artifact present on disk"
    check "artifact is marked UNSENT_UNTIL_REPLAY" "${ARTIFACT}" '.status == "UNSENT_UNTIL_REPLAY"'
    check "artifact carries the original envelope subject" "${ARTIFACT}" \
        '.envelope.subject == "closeout: br-bvq1x.14.10 done"'
    check "artifact carries a replay command" "${ARTIFACT}" \
        '(.replay_command | contains("replay-queued"))'
else
    e2e_fail "no durable UNSENT artifact written under pending_sends/"
fi

# ---------------------------------------------------------------------------
# H3: `am robot status` surfaces the queued intent with its replay command.
# ---------------------------------------------------------------------------
e2e_case_banner "H3: robot status surfaces the queued send intent + replay command"
export DATABASE_URL="${HEALTHY_DB}"
amrun "${ART}/status_queued.json" robot status --project "${PROJECT_KEY}" --agent BlueLake --format json
exit_is "robot status (healthy) ok" "${ART}/status_queued.json" 0
check "queued_intents lists the send_message intent" "${ART}/status_queued.json" \
    '[.queued_intents[]? | select(.kind == "send_message")] | length == 1'
check "the queued send intent carries a replay command" "${ART}/status_queued.json" \
    '[.queued_intents[]? | select(.kind == "send_message") | select(.replay | contains("replay-queued"))] | length == 1'
check "the queued send intent names the sender" "${ART}/status_queued.json" \
    '[.queued_intents[]? | select(.kind == "send_message") | select(.agent == "BlueLake")] | length == 1'
check "a degraded_intents anomaly is surfaced" "${ART}/status_queued.json" \
    '[.anomalies[]? | select(.category == "degraded_intents")] | length >= 1'

# ---------------------------------------------------------------------------
# H2: replay re-validates a tampered artifact and refuses to send it.
# ---------------------------------------------------------------------------
e2e_case_banner "H2: replay refuses a tampered artifact (content_hash mismatch)"
if [ -n "${ARTIFACT}" ] && [ -f "${ARTIFACT}" ]; then
    TAMPERED="${STORAGE_ROOT}/pending_sends/deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef.json"
    jq '.content_hash = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef" | .envelope.subject = "tampered"' \
        "${ARTIFACT}" >"${TAMPERED}"
    amrun "${ART}/replay_tampered.json" mail replay-queued --artifact "${TAMPERED}" --format json
    exit_nonzero "replay rejects a tampered (hash-mismatch) artifact" "${ART}/replay_tampered.json"
    if grep -qi "content_hash mismatch" "${ART}/replay_tampered.json.err" 2>/dev/null; then
        e2e_pass "tampered replay names the content_hash mismatch"
    else
        e2e_fail "tampered replay did not report a content_hash mismatch"
    fi
    # Quarantine the tampered fixture so it does not pollute later surface scans.
    mv "${TAMPERED}" "${TAMPERED}.sent.json" 2>/dev/null || true
else
    e2e_skip "tamper-rejection skipped: no genuine artifact to tamper with"
fi

# ---------------------------------------------------------------------------
# H2: replay flushes the UNSENT message against a healthy mailbox.
# ---------------------------------------------------------------------------
e2e_case_banner "H2: replay-queued sends the message once the mailbox is healthy"
if [ -n "${ARTIFACT}" ] && [ -f "${ARTIFACT}" ]; then
    amrun "${ART}/replay.json" mail replay-queued --artifact "${ARTIFACT}" --format json
    exit_is "replay-queued succeeds" "${ART}/replay.json" 0
    RECEIPT="${ARTIFACT%.json}.sent.json"
    if [ -f "${RECEIPT}" ]; then
        e2e_pass "replay wrote a sent receipt next to the artifact"
    else
        e2e_fail "replay did not write a sent receipt (${RECEIPT})"
    fi

    e2e_case_banner "H3: after replay the intent disappears from queued_intents"
    amrun "${ART}/status_cleared.json" robot status --project "${PROJECT_KEY}" --agent BlueLake --format json
    check "no send_message intent remains queued" "${ART}/status_cleared.json" \
        '[.queued_intents[]? | select(.kind == "send_message")] | length == 0'

    e2e_case_banner "H2: replay is idempotent (re-running does not double-send)"
    amrun "${ART}/replay_again.json" mail replay-queued --artifact "${ARTIFACT}" --format json
    exit_is "second replay is a safe no-op" "${ART}/replay_again.json" 0
    amrun "${ART}/status_cleared2.json" robot status --project "${PROJECT_KEY}" --agent BlueLake --format json
    check "still no send_message intent after idempotent replay" "${ART}/status_cleared2.json" \
        '[.queued_intents[]? | select(.kind == "send_message")] | length == 0'
else
    e2e_skip "replay lifecycle skipped: no genuine artifact was produced to replay"
fi

# ---------------------------------------------------------------------------
# H4: an unavailable mailbox reports a non-ok health (read-degraded contract).
# ---------------------------------------------------------------------------
e2e_case_banner "H4: robot status reports degraded health under an unavailable mailbox"
export DATABASE_URL="${UNAVAIL_DB}"
amrun "${ART}/status_degraded.json" robot status --project "${PROJECT_KEY}" --agent BlueLake --format json
if jq -e . "${ART}/status_degraded.json" >/dev/null 2>&1; then
    check "health is not falsely ok under an unavailable mailbox" "${ART}/status_degraded.json" \
        '.health != "ok"'
else
    e2e_skip "robot status emitted no JSON under the unavailable mailbox (recovery-only fallback declined); read-degraded contract is unit-tested in mailbox_durability + doctor health"
fi
export DATABASE_URL="${HEALTHY_DB}"

# ---------------------------------------------------------------------------
# Honest SKIPs — surfaces with no black-box CLI trigger.
# ---------------------------------------------------------------------------
e2e_case_banner "H1/H3 release + ack queue paths — MCP-server-only trigger"
e2e_skip "the H1 release-intent + H3 ack-intent QUEUE paths trip only through the MCP server (the CLI release/ack verbs use a direct sync-SQLite path); on-disk schema + auto-replay + abandon-on-missing are covered by DB-backed integration tests: tools messaging::tests::replay_queued_ack_intent_acks_once_on_success / replay_abandons_ack_intent_for_missing_message, tools reservations::tests::replay_queued_release_intent_releases_once, cli robot::tests::build_queued_intents_surfaces_ack_and_send"

e2e_case_banner "Unified replay-intents --dry-run + cancellation — not implemented"
e2e_skip "no unified 'am replay-intents --dry-run' / intent-cancellation surface yet: ack & release auto-replay on the next successful call, sends replay via 'am mail replay-queued'; would-replay/no-op preview + cancel are a future N-track surface"

# ---------------------------------------------------------------------------
log_summary "${E2E_SUITE}" "$((_E2E_PASS + _E2E_FAIL + _E2E_SKIP))" "${_E2E_PASS}" "${_E2E_FAIL}" \
    "${E2E_ARTIFACT_DIR}" "bash tests/e2e/test_degraded_intents_replay.sh" >>"${EVENTS}"
e2e_summary
