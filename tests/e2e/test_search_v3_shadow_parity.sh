#!/usr/bin/env bash
# test_search_v3_shadow_parity.sh - E2E: Shadow parity between legacy and Search V3
#
# br-2tnl.7.10: Add shadow-parity E2E script comparing legacy and Search V3 outputs
#
# This suite runs canonical query scenarios through three execution modes:
#   1) Legacy (`AM_SEARCH_ENGINE=legacy`)
#   2) Search V3 (`AM_SEARCH_ENGINE=lexical`)
#   3) Shadow (`AM_SEARCH_ENGINE=shadow`, log-only compare path)
#
# For each scenario we produce deterministic parity artifacts:
#   - Legacy/V3/Shadow raw RPC transcripts
#   - Parsed tool payloads
#   - Ranking/order diff reports
#   - Score-delta summaries (when score factors are present)
#   - Filter behavior deltas (sender/thread/importance facet checks)
#   - Classification under explicit divergence policy
#
# Cutover readiness is then evaluated with objective thresholds:
#   - unacceptable cases == 0
#   - acceptance rate == 100%
#   - mean overlap >= 70%
#   - filter mismatches == 0
#   - call errors == 0

set -euo pipefail

E2E_SUITE="search_v3_shadow_parity"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_search_v3_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_search_v3_lib.sh"

e2e_init_artifacts
search_v3_init

search_v3_banner "Search V3 Shadow Parity E2E (br-2tnl.7.10)"

e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
search_v3_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

WORK="$(e2e_mktemp "e2e_search_v3_shadow_parity")"
SEARCH_DB="${WORK}/search_v3_shadow_parity.sqlite3"
PROJECT_ALPHA="/tmp/e2e_search_v3_shadow_alpha_${E2E_TIMESTAMP}_$$"
PROJECT_BETA="/tmp/e2e_search_v3_shadow_beta_${E2E_TIMESTAMP}_$$"
PRODUCT_KEY="test-search-v3-shadow-product-${E2E_TIMESTAMP}-$$"

PARITY_DIR="${SEARCH_V3_RUN_DIR}/shadow_parity"
PARITY_REPORTS_JSONL="${PARITY_DIR}/case_reports.jsonl"
mkdir -p "${PARITY_DIR}"
: > "${PARITY_REPORTS_JSONL}"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-search-v3-shadow-parity","version":"1.0"}}}'

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

send_jsonrpc_session() {
    local db_path="$1"
    shift

    local env_vars=()
    while [ $# -gt 0 ] && [ "${1:-}" != "--" ]; do
        env_vars+=("$1")
        shift
    done
    if [ "${1:-}" = "--" ]; then
        shift
    fi
    local requests=("$@")

    local output_file="${WORK}/session_response_$RANDOM.txt"
    local srv_work
    srv_work="$(mktemp -d "${WORK}/srv.XXXXXX")"
    local fifo="${srv_work}/stdin_fifo"
    mkfifo "$fifo"

    env "${env_vars[@]}" \
        DATABASE_URL="sqlite:////${db_path}" \
        RUST_LOG=error \
        WORKTREES_ENABLED=true \
        am serve-stdio < "$fifo" > "$output_file" 2>"${srv_work}/stderr.txt" &
    local srv_pid=$!
    sleep 0.3

    {
        echo "$INIT_REQ"
        sleep 0.1
        for req in "${requests[@]}"; do
            echo "$req"
            sleep 0.05
        done
        sleep 0.2
    } > "$fifo"

    wait "$srv_pid" 2>/dev/null || true
    cat "$output_file"
}

extract_content_text() {
    local response="$1"
    local req_id="$2"
    echo "$response" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        d = json.loads(line)
        if d.get('id') == ${req_id} and 'result' in d:
            content = d['result'].get('content', [])
            if content:
                print(content[0].get('text', ''))
            break
    except Exception:
        continue
" 2>/dev/null
}

is_error_result() {
    local response="$1"
    local req_id="$2"
    echo "$response" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        d = json.loads(line)
        if d.get('id') == ${req_id}:
            if 'error' in d:
                print('true')
                sys.exit(0)
            if 'result' in d and d['result'].get('isError', False):
                print('true')
                sys.exit(0)
    except Exception:
        pass
print('false')
" 2>/dev/null
}

mcp_tool() {
    local id="$1"
    local tool="$2"
    local args="$3"
    echo "{\"jsonrpc\":\"2.0\",\"id\":${id},\"method\":\"tools/call\",\"params\":{\"name\":\"${tool}\",\"arguments\":${args}}}"
}

extract_ranking_json() {
    local payload="$1"
    echo "$payload" | python3 -c "
import json, sys
raw = sys.stdin.read().strip()
if not raw:
    print('[]')
    sys.exit(0)
try:
    data = json.loads(raw)
except Exception:
    print('[]')
    sys.exit(0)
if isinstance(data, dict):
    items = data.get('result', [])
elif isinstance(data, list):
    items = data
else:
    items = []
if not isinstance(items, list):
    items = []
def score_of(item):
    if not isinstance(item, dict):
        return None
    for key in ('score', 'bm25_score', 'rank_score'):
        val = item.get(key)
        if isinstance(val, (int, float)):
            return float(val)
    factors = item.get('score_factors')
    if isinstance(factors, dict):
        for key in ('score', 'bm25', 'total', 'total_score'):
            val = factors.get(key)
            if isinstance(val, (int, float)):
                return float(val)
    return None
rows = []
for row in items:
    if not isinstance(row, dict):
        continue
    rid = row.get('id', row.get('message_id'))
    rows.append({
        'id': rid,
        'subject': row.get('subject', ''),
        'score': score_of(row)
    })
print(json.dumps(rows[:20], ensure_ascii=False))
" 2>/dev/null
}

build_parity_report() {
    local case_id="$1"
    local tool_name="$2"
    local policy="$3"
    local args_file="$4"
    local legacy_file="$5"
    local v3_file="$6"
    local shadow_file="$7"
    local report_file="$8"
    local legacy_call_error="$9"
    local v3_call_error="${10}"
    local shadow_call_error="${11}"

    python3 - "$case_id" "$tool_name" "$policy" "$args_file" "$legacy_file" "$v3_file" "$shadow_file" "$report_file" "$legacy_call_error" "$v3_call_error" "$shadow_call_error" <<'PY'
import json
import math
import statistics
import sys
from pathlib import Path

(
    case_id,
    tool_name,
    policy,
    args_path,
    legacy_path,
    v3_path,
    shadow_path,
    report_path,
    legacy_call_error,
    v3_call_error,
    shadow_call_error,
) = sys.argv[1:]


def load_json(path: str):
    text = Path(path).read_text(encoding="utf-8").strip()
    if not text:
        return {}, "empty_payload"
    try:
        return json.loads(text), None
    except Exception as exc:  # pragma: no cover - defensive parser
        return {"_raw_text": text}, f"json_parse_error:{exc}"


def get_rows(data):
    if isinstance(data, dict):
        rows = data.get("result", [])
    elif isinstance(data, list):
        rows = data
    else:
        rows = []
    if not isinstance(rows, list):
        return []
    return rows


def score_of(item):
    if not isinstance(item, dict):
        return None
    for key in ("score", "bm25_score", "rank_score"):
        value = item.get(key)
        if isinstance(value, (int, float)):
            return float(value)
    factors = item.get("score_factors")
    if isinstance(factors, dict):
        for key in ("score", "bm25", "total", "total_score"):
            value = factors.get(key)
            if isinstance(value, (int, float)):
                return float(value)
    return None


def normalize_rows(rows):
    normalized = []
    for idx, row in enumerate(rows):
        if not isinstance(row, dict):
            continue
        raw_id = row.get("id", row.get("message_id", f"idx-{idx}"))
        id_key = str(raw_id)
        normalized.append(
            {
                "id": raw_id,
                "id_key": id_key,
                "subject": str(row.get("subject", "")),
                "from": str(row.get("from", "")),
                "importance": str(row.get("importance", "")),
                "thread_id": row.get("thread_id"),
                "project_id": row.get("project_id"),
                "score": score_of(row),
            }
        )
    return normalized


def compute_mismatch_counts(rows, query_args):
    sender = (
        query_args.get("sender")
        or query_args.get("from_agent")
        or query_args.get("sender_name")
        or ""
    )
    thread_id = query_args.get("thread_id") or ""
    importance_raw = query_args.get("importance") or ""
    importance_set = {x.strip().lower() for x in importance_raw.split(",") if x.strip()}

    sender_mismatch = 0
    thread_mismatch = 0
    importance_mismatch = 0

    for row in rows:
        if sender and row.get("from") != sender:
            sender_mismatch += 1
        if thread_id and str(row.get("thread_id")) != thread_id:
            thread_mismatch += 1
        if importance_set and str(row.get("importance", "")).lower() not in importance_set:
            importance_mismatch += 1

    return {
        "sender_mismatch": sender_mismatch,
        "thread_mismatch": thread_mismatch,
        "importance_mismatch": importance_mismatch,
        "total_mismatch": sender_mismatch + thread_mismatch + importance_mismatch,
    }


def make_policy(policy_name):
    defaults = {
        "min_overlap": 0.0,
        "max_count_delta": 0,
        "require_same_set": False,
        "require_exact_order": False,
        "force_zero": False,
    }
    table = {
        "strict_exact": {
            "min_overlap": 1.0,
            "max_count_delta": 0,
            "require_same_set": True,
            "require_exact_order": True,
        },
        "strict_set": {
            "min_overlap": 1.0,
            "max_count_delta": 0,
            "require_same_set": True,
            "require_exact_order": False,
        },
        "rank_relaxed": {
            "min_overlap": 0.60,
            "max_count_delta": 3,
            "require_same_set": False,
            "require_exact_order": False,
        },
        "zero_exact": {"force_zero": True},
        "product_relaxed": {
            "min_overlap": 0.50,
            "max_count_delta": 4,
            "require_same_set": False,
            "require_exact_order": False,
        },
    }
    config = dict(defaults)
    config.update(table.get(policy_name, table["rank_relaxed"]))
    return config


query_args, query_args_err = load_json(args_path)
legacy_data, legacy_parse_err = load_json(legacy_path)
v3_data, v3_parse_err = load_json(v3_path)
shadow_data, shadow_parse_err = load_json(shadow_path)

legacy_rows = normalize_rows(get_rows(legacy_data))
v3_rows = normalize_rows(get_rows(v3_data))
shadow_rows = normalize_rows(get_rows(shadow_data))

legacy_ids = [row["id_key"] for row in legacy_rows]
v3_ids = [row["id_key"] for row in v3_rows]
shadow_ids = [row["id_key"] for row in shadow_rows]

legacy_set = set(legacy_ids)
v3_set = set(v3_ids)
shadow_set = set(shadow_ids)

overlap_ids = sorted(legacy_set & v3_set)
overlap_count = len(overlap_ids)
max_count = max(len(legacy_ids), len(v3_ids), 1)
union_count = max(len(legacy_set | v3_set), 1)
overlap_ratio = overlap_count / max_count
jaccard = overlap_count / union_count
count_delta = abs(len(legacy_ids) - len(v3_ids))

max_pos = max(len(legacy_ids), len(v3_ids))
position_diffs = []
for idx in range(max_pos):
    legacy_id = legacy_ids[idx] if idx < len(legacy_ids) else None
    v3_id = v3_ids[idx] if idx < len(v3_ids) else None
    if legacy_id != v3_id:
        position_diffs.append(
            {"position": idx, "legacy_id": legacy_id, "v3_id": v3_id}
        )

legacy_scores = {row["id_key"]: row.get("score") for row in legacy_rows}
v3_scores = {row["id_key"]: row.get("score") for row in v3_rows}
score_deltas = []
for key in overlap_ids:
    lval = legacy_scores.get(key)
    rval = v3_scores.get(key)
    if isinstance(lval, (int, float)) and isinstance(rval, (int, float)):
        score_deltas.append(float(rval) - float(lval))

if score_deltas:
    score_summary = {
        "available": True,
        "common_scored_ids": len(score_deltas),
        "mean_delta": statistics.fmean(score_deltas),
        "median_delta": statistics.median(score_deltas),
        "max_abs_delta": max(abs(x) for x in score_deltas),
    }
else:
    score_summary = {
        "available": False,
        "common_scored_ids": 0,
        "mean_delta": 0.0,
        "median_delta": 0.0,
        "max_abs_delta": 0.0,
    }

legacy_mismatch = compute_mismatch_counts(legacy_rows, query_args if isinstance(query_args, dict) else {})
v3_mismatch = compute_mismatch_counts(v3_rows, query_args if isinstance(query_args, dict) else {})
shadow_mismatch = compute_mismatch_counts(shadow_rows, query_args if isinstance(query_args, dict) else {})

policy_cfg = make_policy(policy)
reasons = []
acceptable = True

legacy_call_failed = legacy_call_error == "true"
v3_call_failed = v3_call_error == "true"
shadow_call_failed = shadow_call_error == "true"

if legacy_call_failed or v3_call_failed or shadow_call_failed:
    acceptable = False
    reasons.append("call_error")

if query_args_err:
    acceptable = False
    reasons.append("query_args_parse_error")
if legacy_parse_err:
    acceptable = False
    reasons.append("legacy_parse_error")
if v3_parse_err:
    acceptable = False
    reasons.append("v3_parse_error")
if shadow_parse_err:
    acceptable = False
    reasons.append("shadow_parse_error")

if policy_cfg["force_zero"]:
    if len(legacy_ids) != 0 or len(v3_ids) != 0:
        acceptable = False
        reasons.append("zero_policy_violation")
else:
    if count_delta > policy_cfg["max_count_delta"]:
        acceptable = False
        reasons.append("count_delta_exceeds_policy")
    if max(len(legacy_ids), len(v3_ids)) > 0 and overlap_ratio < policy_cfg["min_overlap"]:
        acceptable = False
        reasons.append("overlap_below_policy")
    if policy_cfg["require_same_set"] and legacy_set != v3_set:
        acceptable = False
        reasons.append("result_set_mismatch")
    if policy_cfg["require_exact_order"] and legacy_ids != v3_ids:
        acceptable = False
        reasons.append("ordering_mismatch")

if v3_mismatch["total_mismatch"] > 0:
    acceptable = False
    reasons.append("v3_filter_behavior_mismatch")
if legacy_mismatch["total_mismatch"] > 0:
    acceptable = False
    reasons.append("legacy_filter_behavior_mismatch")

classification = "unacceptable"
if acceptable:
    if legacy_ids == v3_ids:
        classification = "acceptable_exact"
    elif legacy_set == v3_set:
        classification = "acceptable_reorder"
    elif len(v3_ids) >= len(legacy_ids):
        classification = "acceptable_expansion"
    else:
        classification = "acceptable_relaxed"

shadow_overlap_count = len(legacy_set & shadow_set)
shadow_overlap_ratio = shadow_overlap_count / max(len(legacy_set), len(shadow_set), 1)

report = {
    "schema_version": 1,
    "case_id": case_id,
    "tool_name": tool_name,
    "policy": policy,
    "policy_thresholds": policy_cfg,
    "classification": classification,
    "acceptable": acceptable,
    "reasons": reasons,
    "counts": {
        "legacy": len(legacy_ids),
        "v3": len(v3_ids),
        "shadow": len(shadow_ids),
        "count_delta": count_delta,
    },
    "overlap": {
        "count": overlap_count,
        "ratio": overlap_ratio,
        "jaccard": jaccard,
    },
    "shadow_overlap": {
        "count": shadow_overlap_count,
        "ratio": shadow_overlap_ratio,
    },
    "ranking_diff": {
        "legacy_order": legacy_ids,
        "v3_order": v3_ids,
        "position_diffs": position_diffs,
        "dropped_ids": sorted(legacy_set - v3_set),
        "new_ids": sorted(v3_set - legacy_set),
    },
    "score_delta": score_summary,
    "filter_behavior": {
        "legacy": legacy_mismatch,
        "v3": v3_mismatch,
        "shadow": shadow_mismatch,
        "delta_v3_minus_legacy": {
            "sender_mismatch": v3_mismatch["sender_mismatch"] - legacy_mismatch["sender_mismatch"],
            "thread_mismatch": v3_mismatch["thread_mismatch"] - legacy_mismatch["thread_mismatch"],
            "importance_mismatch": v3_mismatch["importance_mismatch"] - legacy_mismatch["importance_mismatch"],
            "total_mismatch": v3_mismatch["total_mismatch"] - legacy_mismatch["total_mismatch"],
        },
    },
    "call_status": {
        "legacy_call_error": legacy_call_failed,
        "v3_call_error": v3_call_failed,
        "shadow_call_error": shadow_call_failed,
    },
    "parse_status": {
        "query_args_error": query_args_err,
        "legacy_payload_error": legacy_parse_err,
        "v3_payload_error": v3_parse_err,
        "shadow_payload_error": shadow_parse_err,
    },
}

Path(report_path).write_text(json.dumps(report, indent=2, ensure_ascii=False), encoding="utf-8")
print(json.dumps(report, separators=(",", ":"), ensure_ascii=False))
PY
}

summarize_report_for_assertions() {
    local report_file="$1"
    python3 - "$report_file" <<'PY'
import json
import sys

report = json.load(open(sys.argv[1], encoding="utf-8"))
classification = report.get("classification", "")
overlap_bps = int(round(float(report.get("overlap", {}).get("ratio", 0.0)) * 10000))
min_overlap_bps = int(round(float(report.get("policy_thresholds", {}).get("min_overlap", 0.0)) * 10000))
count_delta = int(report.get("counts", {}).get("count_delta", 0))
max_count_delta = int(report.get("policy_thresholds", {}).get("max_count_delta", 0))
legacy_filter_total = int(report.get("filter_behavior", {}).get("legacy", {}).get("total_mismatch", 0))
v3_filter_total = int(report.get("filter_behavior", {}).get("v3", {}).get("total_mismatch", 0))
legacy_call_error = 1 if report.get("call_status", {}).get("legacy_call_error") else 0
v3_call_error = 1 if report.get("call_status", {}).get("v3_call_error") else 0
shadow_call_error = 1 if report.get("call_status", {}).get("shadow_call_error") else 0
legacy_parse_error = 1 if report.get("parse_status", {}).get("legacy_payload_error") else 0
v3_parse_error = 1 if report.get("parse_status", {}).get("v3_payload_error") else 0
print(
    "|".join(
        [
            classification,
            str(overlap_bps),
            str(min_overlap_bps),
            str(count_delta),
            str(max_count_delta),
            str(legacy_filter_total),
            str(v3_filter_total),
            str(legacy_call_error),
            str(v3_call_error),
            str(shadow_call_error),
            str(legacy_parse_error),
            str(v3_parse_error),
        ]
    )
)
PY
}

assert_numeric_ge() {
    local label="$1"
    local actual="$2"
    local expected="$3"
    if [ "$actual" -ge "$expected" ]; then
        e2e_pass "${label} (actual=${actual}, expected>=${expected})"
    else
        e2e_fail "${label} (actual=${actual}, expected>=${expected})"
    fi
}

assert_numeric_le() {
    local label="$1"
    local actual="$2"
    local expected="$3"
    if [ "$actual" -le "$expected" ]; then
        e2e_pass "${label} (actual=${actual}, expected<=${expected})"
    else
        e2e_fail "${label} (actual=${actual}, expected<=${expected})"
    fi
}

run_parity_case() {
    local case_id="$1"
    local tool_name="$2"
    local policy="$3"
    local description="$4"
    local args_json="$5"

    e2e_case_banner "Shadow parity: ${case_id}"
    search_v3_log "[${case_id}] ${description}"

    local case_dir="${SEARCH_V3_RUN_DIR}/cases/${case_id}"
    mkdir -p "${case_dir}"

    local query_val limit_val project_val
    local metadata_line
    metadata_line="$(
        echo "$args_json" | python3 -c "
import json, sys
try:
    args = json.loads(sys.stdin.read())
except Exception:
    args = {}
query = str(args.get('query', ''))
limit = args.get('limit')
project = str(args.get('project_key', args.get('project', args.get('product_key', ''))))
print(f'{query}\t{limit if limit is not None else \"\"}\t{project}')
" 2>/dev/null
    )"
    IFS=$'\t' read -r query_val limit_val project_val <<< "$metadata_line"

    search_v3_capture_params "${case_id}" \
        --mode "shadow_compare" \
        --query "${query_val}" \
        --limit "${limit_val:-20}" \
        --project "${project_val}"

    printf '%s' "$args_json" > "${case_dir}/query_args.json"

    local request
    request="$(mcp_tool 2 "${tool_name}" "${args_json}")"

    local legacy_resp v3_resp shadow_resp
    legacy_resp="$(
        send_jsonrpc_session "$SEARCH_DB" \
            AM_SEARCH_ENGINE=legacy \
            AM_SEARCH_SHADOW_MODE=off \
            AM_SEARCH_ENGINE_FOR_SEARCH_MESSAGES=legacy \
            AM_SEARCH_ENGINE_FOR_SEARCH_MESSAGES_PRODUCT=legacy \
            -- "$request"
    )"
    v3_resp="$(
        send_jsonrpc_session "$SEARCH_DB" \
            AM_SEARCH_ENGINE=lexical \
            AM_SEARCH_SHADOW_MODE=off \
            AM_SEARCH_ENGINE_FOR_SEARCH_MESSAGES=lexical \
            AM_SEARCH_ENGINE_FOR_SEARCH_MESSAGES_PRODUCT=lexical \
            -- "$request"
    )"
    shadow_resp="$(
        send_jsonrpc_session "$SEARCH_DB" \
            AM_SEARCH_ENGINE=shadow \
            AM_SEARCH_SHADOW_MODE=log_only \
            AM_SEARCH_ENGINE_FOR_SEARCH_MESSAGES=shadow \
            AM_SEARCH_ENGINE_FOR_SEARCH_MESSAGES_PRODUCT=shadow \
            -- "$request"
    )"

    printf '%s\n' "$legacy_resp" > "${case_dir}/legacy_rpc.jsonl"
    printf '%s\n' "$v3_resp" > "${case_dir}/v3_rpc.jsonl"
    printf '%s\n' "$shadow_resp" > "${case_dir}/shadow_rpc.jsonl"

    local legacy_err v3_err shadow_err
    legacy_err="$(is_error_result "$legacy_resp" 2)"
    v3_err="$(is_error_result "$v3_resp" 2)"
    shadow_err="$(is_error_result "$shadow_resp" 2)"

    local case_failed=0

    if [ "$legacy_err" = "false" ]; then
        e2e_pass "[${case_id}] legacy call succeeded"
    else
        e2e_fail "[${case_id}] legacy call failed"
        case_failed=1
    fi
    if [ "$v3_err" = "false" ]; then
        e2e_pass "[${case_id}] v3 call succeeded"
    else
        e2e_fail "[${case_id}] v3 call failed"
        case_failed=1
    fi
    if [ "$shadow_err" = "false" ]; then
        e2e_pass "[${case_id}] shadow call succeeded"
    else
        e2e_fail "[${case_id}] shadow call failed"
        case_failed=1
    fi

    local legacy_text v3_text shadow_text
    legacy_text="$(extract_content_text "$legacy_resp" 2)"
    v3_text="$(extract_content_text "$v3_resp" 2)"
    shadow_text="$(extract_content_text "$shadow_resp" 2)"

    printf '%s' "${legacy_text}" > "${case_dir}/legacy_payload.json"
    printf '%s' "${v3_text}" > "${case_dir}/v3_payload.json"
    printf '%s' "${shadow_text}" > "${case_dir}/shadow_payload.json"

    local legacy_ranking v3_ranking
    legacy_ranking="$(extract_ranking_json "$legacy_text")"
    v3_ranking="$(extract_ranking_json "$v3_text")"
    search_v3_capture_ranking "${case_id}_legacy" "${legacy_ranking}"
    search_v3_capture_ranking "${case_id}_v3" "${v3_ranking}"

    local report_file
    report_file="${case_dir}/parity_report.json"
    build_parity_report \
        "${case_id}" \
        "${tool_name}" \
        "${policy}" \
        "${case_dir}/query_args.json" \
        "${case_dir}/legacy_payload.json" \
        "${case_dir}/v3_payload.json" \
        "${case_dir}/shadow_payload.json" \
        "${report_file}" \
        "${legacy_err}" \
        "${v3_err}" \
        "${shadow_err}" \
        >> "${PARITY_REPORTS_JSONL}"

    local metric_line
    metric_line="$(summarize_report_for_assertions "${report_file}")"
    local report_class overlap_bps min_overlap_bps count_delta max_count_delta
    local legacy_filter_total v3_filter_total legacy_call_error v3_call_error shadow_call_error
    local legacy_parse_error v3_parse_error

    IFS='|' read -r \
        report_class \
        overlap_bps \
        min_overlap_bps \
        count_delta \
        max_count_delta \
        legacy_filter_total \
        v3_filter_total \
        legacy_call_error \
        v3_call_error \
        shadow_call_error \
        legacy_parse_error \
        v3_parse_error \
        <<< "$metric_line"

    if [[ "$report_class" == acceptable* ]]; then
        e2e_pass "[${case_id}] policy classification acceptable (${report_class})"
    else
        e2e_fail "[${case_id}] policy classification unacceptable (${report_class})"
        case_failed=1
    fi

    assert_numeric_ge "[${case_id}] overlap meets policy (basis points)" "$overlap_bps" "$min_overlap_bps"
    if [ "$overlap_bps" -lt "$min_overlap_bps" ]; then
        case_failed=1
    fi

    assert_numeric_le "[${case_id}] count delta within policy" "$count_delta" "$max_count_delta"
    if [ "$count_delta" -gt "$max_count_delta" ]; then
        case_failed=1
    fi

    e2e_assert_eq "[${case_id}] legacy filter mismatch total" "0" "$legacy_filter_total"
    if [ "$legacy_filter_total" -ne 0 ]; then
        case_failed=1
    fi

    e2e_assert_eq "[${case_id}] v3 filter mismatch total" "0" "$v3_filter_total"
    if [ "$v3_filter_total" -ne 0 ]; then
        case_failed=1
    fi

    e2e_assert_eq "[${case_id}] legacy call error flag" "0" "$legacy_call_error"
    if [ "$legacy_call_error" -ne 0 ]; then
        case_failed=1
    fi

    e2e_assert_eq "[${case_id}] v3 call error flag" "0" "$v3_call_error"
    if [ "$v3_call_error" -ne 0 ]; then
        case_failed=1
    fi

    e2e_assert_eq "[${case_id}] shadow call error flag" "0" "$shadow_call_error"
    if [ "$shadow_call_error" -ne 0 ]; then
        case_failed=1
    fi

    e2e_assert_eq "[${case_id}] legacy parse error flag" "0" "$legacy_parse_error"
    if [ "$legacy_parse_error" -ne 0 ]; then
        case_failed=1
    fi

    e2e_assert_eq "[${case_id}] v3 parse error flag" "0" "$v3_parse_error"
    if [ "$v3_parse_error" -ne 0 ]; then
        case_failed=1
    fi

    if [ "$case_failed" -eq 0 ]; then
        search_v3_case_summary "${case_id}" "pass" \
            --message "classification=${report_class} overlap_bps=${overlap_bps} count_delta=${count_delta}"
    else
        search_v3_case_summary "${case_id}" "fail" \
            --message "classification=${report_class} overlap_bps=${overlap_bps} count_delta=${count_delta}"
    fi
}

build_cutover_readiness_report() {
    local reports_jsonl="$1"
    local output_file="$2"

    python3 - "$reports_jsonl" "$output_file" <<'PY'
import json
import statistics
import sys
from pathlib import Path

reports_path = Path(sys.argv[1])
output_path = Path(sys.argv[2])

reports = []
for line in reports_path.read_text(encoding="utf-8").splitlines():
    line = line.strip()
    if not line:
        continue
    reports.append(json.loads(line))

total_cases = len(reports)
acceptable_cases = [r for r in reports if str(r.get("classification", "")).startswith("acceptable")]
unacceptable_cases = [r for r in reports if not str(r.get("classification", "")).startswith("acceptable")]

acceptance_rate = (len(acceptable_cases) / total_cases) if total_cases else 0.0
overlap_values = [float(r.get("overlap", {}).get("ratio", 0.0)) for r in reports]
mean_overlap = statistics.fmean(overlap_values) if overlap_values else 0.0
min_overlap = min(overlap_values) if overlap_values else 0.0

v3_filter_mismatch_total = sum(
    int(r.get("filter_behavior", {}).get("v3", {}).get("total_mismatch", 0))
    for r in reports
)
legacy_filter_mismatch_total = sum(
    int(r.get("filter_behavior", {}).get("legacy", {}).get("total_mismatch", 0))
    for r in reports
)

call_error_total = 0
for r in reports:
    call = r.get("call_status", {})
    if call.get("legacy_call_error"):
        call_error_total += 1
    if call.get("v3_call_error"):
        call_error_total += 1
    if call.get("shadow_call_error"):
        call_error_total += 1

thresholds = {
    "max_unacceptable_cases": 0,
    "min_acceptance_rate": 1.0,
    "min_mean_overlap": 0.70,
    "max_v3_filter_mismatch_total": 0,
    "max_call_error_total": 0,
}

gates = {
    "unacceptable_cases_ok": len(unacceptable_cases) <= thresholds["max_unacceptable_cases"],
    "acceptance_rate_ok": acceptance_rate >= thresholds["min_acceptance_rate"],
    "mean_overlap_ok": mean_overlap >= thresholds["min_mean_overlap"],
    "v3_filter_mismatch_ok": v3_filter_mismatch_total <= thresholds["max_v3_filter_mismatch_total"],
    "call_errors_ok": call_error_total <= thresholds["max_call_error_total"],
}

cutover_ready = all(gates.values()) and total_cases > 0

report = {
    "schema_version": 1,
    "suite": "search_v3_shadow_parity",
    "total_cases": total_cases,
    "acceptable_cases": len(acceptable_cases),
    "unacceptable_cases": len(unacceptable_cases),
    "acceptance_rate": acceptance_rate,
    "mean_overlap": mean_overlap,
    "min_overlap": min_overlap,
    "legacy_filter_mismatch_total": legacy_filter_mismatch_total,
    "v3_filter_mismatch_total": v3_filter_mismatch_total,
    "call_error_total": call_error_total,
    "thresholds": thresholds,
    "gates": gates,
    "cutover_ready": cutover_ready,
    "unacceptable_case_ids": [r.get("case_id") for r in unacceptable_cases],
}

output_path.write_text(json.dumps(report, indent=2), encoding="utf-8")
print(
    "|".join(
        [
            str(int(cutover_ready)),
            str(len(unacceptable_cases)),
            str(int(round(acceptance_rate * 10000))),
            str(int(round(mean_overlap * 10000))),
            str(v3_filter_mismatch_total),
            str(call_error_total),
        ]
    )
)
PY
}

# ---------------------------------------------------------------------------
# Case 1: Setup corpus for parity comparisons
# ---------------------------------------------------------------------------

e2e_case_banner "Setup: seed deterministic corpus for shadow parity"
search_v3_log "Seeding alpha/beta projects, agents, threads, product links"

SETUP_RESP="$(send_jsonrpc_session "$SEARCH_DB" -- \
    "$(mcp_tool 2 ensure_project "{\"human_key\":\"$PROJECT_ALPHA\"}")" \
    "$(mcp_tool 3 ensure_project "{\"human_key\":\"$PROJECT_BETA\"}")" \
    "$(mcp_tool 4 register_agent "{\"project_key\":\"$PROJECT_ALPHA\",\"program\":\"e2e\",\"model\":\"test\",\"name\":\"GoldFox\"}")" \
    "$(mcp_tool 5 register_agent "{\"project_key\":\"$PROJECT_ALPHA\",\"program\":\"e2e\",\"model\":\"test\",\"name\":\"SilverWolf\"}")" \
    "$(mcp_tool 6 register_agent "{\"project_key\":\"$PROJECT_ALPHA\",\"program\":\"e2e\",\"model\":\"test\",\"name\":\"RedPeak\"}")" \
    "$(mcp_tool 7 register_agent "{\"project_key\":\"$PROJECT_BETA\",\"program\":\"e2e\",\"model\":\"test\",\"name\":\"GoldFox\"}")" \
    "$(mcp_tool 8 register_agent "{\"project_key\":\"$PROJECT_BETA\",\"program\":\"e2e\",\"model\":\"test\",\"name\":\"SilverWolf\"}")" \
    "$(mcp_tool 9 register_agent "{\"project_key\":\"$PROJECT_BETA\",\"program\":\"e2e\",\"model\":\"test\",\"name\":\"RedPeak\"}")" \
    "$(mcp_tool 10 send_message "{\"project_key\":\"$PROJECT_ALPHA\",\"sender_name\":\"GoldFox\",\"to\":[\"SilverWolf\"],\"subject\":\"Deployment pipeline update\",\"body_md\":\"Deployment pipeline updated with staged rollout checks.\"}")" \
    "$(mcp_tool 11 send_message "{\"project_key\":\"$PROJECT_ALPHA\",\"sender_name\":\"GoldFox\",\"to\":[\"SilverWolf\",\"RedPeak\"],\"subject\":\"Critical database migration\",\"body_md\":\"Urgent database migration required before midnight.\",\"importance\":\"urgent\",\"ack_required\":true}")" \
    "$(mcp_tool 12 send_message "{\"project_key\":\"$PROJECT_ALPHA\",\"sender_name\":\"SilverWolf\",\"to\":[\"GoldFox\"],\"subject\":\"API redesign proposal\",\"body_md\":\"Propose REST + GraphQL dual endpoint strategy.\",\"thread_id\":\"thread-api-redesign\"}")" \
    "$(mcp_tool 13 send_message "{\"project_key\":\"$PROJECT_ALPHA\",\"sender_name\":\"GoldFox\",\"to\":[\"SilverWolf\"],\"subject\":\"Re: API redesign proposal\",\"body_md\":\"GraphQL schema follow-up for API redesign.\",\"thread_id\":\"thread-api-redesign\"}")" \
    "$(mcp_tool 14 send_message "{\"project_key\":\"$PROJECT_ALPHA\",\"sender_name\":\"RedPeak\",\"to\":[\"GoldFox\",\"SilverWolf\"],\"subject\":\"Internationales Datenupdate\",\"body_md\":\"Lokalisierungsdaten bereit. Unicode: Straße, café, élève.\"}")" \
    "$(mcp_tool 15 send_message "{\"project_key\":\"$PROJECT_ALPHA\",\"sender_name\":\"GoldFox\",\"to\":[\"RedPeak\"],\"subject\":\"Quantum entanglement research\",\"body_md\":\"Quantum entanglement experiment results are ready.\"}")" \
    "$(mcp_tool 16 send_message "{\"project_key\":\"$PROJECT_ALPHA\",\"sender_name\":\"SilverWolf\",\"to\":[\"GoldFox\"],\"subject\":\"Blockchain consensus protocol\",\"body_md\":\"Consensus mechanism update for distributed ledger.\",\"thread_id\":\"thread-blockchain\"}")" \
    "$(mcp_tool 17 send_message "{\"project_key\":\"$PROJECT_ALPHA\",\"sender_name\":\"RedPeak\",\"to\":[\"GoldFox\"],\"subject\":\"Deployment checklist complete\",\"body_md\":\"Deployment prerequisites verified for production rollout.\"}")" \
    "$(mcp_tool 18 send_message "{\"project_key\":\"$PROJECT_ALPHA\",\"sender_name\":\"GoldFox\",\"to\":[\"SilverWolf\"],\"subject\":\"Performance benchmarking results\",\"body_md\":\"Query latency improved by 3x after optimization.\"}")" \
    "$(mcp_tool 19 send_message "{\"project_key\":\"$PROJECT_ALPHA\",\"sender_name\":\"SilverWolf\",\"to\":[\"GoldFox\",\"RedPeak\"],\"subject\":\"Security audit findings\",\"body_md\":\"High severity security issue requires immediate remediation.\",\"importance\":\"urgent\",\"ack_required\":true}")" \
    "$(mcp_tool 20 send_message "{\"project_key\":\"$PROJECT_ALPHA\",\"sender_name\":\"GoldFox\",\"to\":[\"RedPeak\"],\"subject\":\"Rollout postmortem notes\",\"body_md\":\"Postmortem for rollout issue with mitigation tasks.\",\"importance\":\"low\"}")" \
    "$(mcp_tool 21 send_message "{\"project_key\":\"$PROJECT_ALPHA\",\"sender_name\":\"RedPeak\",\"to\":[\"GoldFox\"],\"subject\":\"Deployment rollback plan\",\"body_md\":\"Rollback strategy for deployment incident.\",\"importance\":\"high\"}")" \
    "$(mcp_tool 22 send_message "{\"project_key\":\"$PROJECT_BETA\",\"sender_name\":\"GoldFox\",\"to\":[\"SilverWolf\"],\"subject\":\"Beta deployment notes\",\"body_md\":\"Deployment notes for beta staging environment.\"}")" \
    "$(mcp_tool 23 send_message "{\"project_key\":\"$PROJECT_BETA\",\"sender_name\":\"SilverWolf\",\"to\":[\"GoldFox\"],\"subject\":\"Beta search integration\",\"body_md\":\"Cross-project search integration checks for beta.\"}")" \
    "$(mcp_tool 24 send_message "{\"project_key\":\"$PROJECT_BETA\",\"sender_name\":\"RedPeak\",\"to\":[\"GoldFox\"],\"subject\":\"Beta security checklist\",\"body_md\":\"Security checklist completed for beta release.\",\"importance\":\"urgent\"}")" \
    "$(mcp_tool 25 send_message "{\"project_key\":\"$PROJECT_BETA\",\"sender_name\":\"GoldFox\",\"to\":[\"RedPeak\"],\"subject\":\"Lokalisierungsdaten Beta\",\"body_md\":\"Unicode beta payload: Straße and café tokens.\"}")" \
    "$(mcp_tool 26 ensure_product "{\"product_key\":\"$PRODUCT_KEY\"}")" \
    "$(mcp_tool 27 products_link "{\"product_key\":\"$PRODUCT_KEY\",\"project_key\":\"$PROJECT_ALPHA\"}")" \
    "$(mcp_tool 28 products_link "{\"product_key\":\"$PRODUCT_KEY\",\"project_key\":\"$PROJECT_BETA\"}")" \
)"

e2e_save_artifact "shadow_parity/setup_rpc.jsonl" "$SETUP_RESP"

SETUP_OK=0
SETUP_FAIL=0
for i in $(seq 2 28); do
    ERR_FLAG="$(is_error_result "$SETUP_RESP" "$i")"
    if [ "$ERR_FLAG" = "false" ]; then
        SETUP_OK=$((SETUP_OK + 1))
    else
        SETUP_FAIL=$((SETUP_FAIL + 1))
    fi
done

if [ "$SETUP_FAIL" -eq 0 ]; then
    e2e_pass "setup: all 27 setup requests succeeded"
    search_v3_case_summary "setup" "pass" --message "ok=${SETUP_OK}"
else
    e2e_fail "setup: ${SETUP_FAIL} setup requests failed"
    search_v3_case_summary "setup" "fail" --message "failed=${SETUP_FAIL}"
fi

search_v3_capture_index_meta "initial_shadow_parity_index" \
    --doc-count 16 \
    --last-commit "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" \
    --consistency "ok"

# ---------------------------------------------------------------------------
# Parity matrix
# ---------------------------------------------------------------------------

readarray -t PARITY_CASES <<'EOF'
basic_deployment|search_messages|rank_relaxed|Baseline deployment query|{"project_key":"__ALPHA__","query":"deployment","limit":12,"explain":true}
phrase_migration|search_messages|strict_set|Phrase query for migration|{"project_key":"__ALPHA__","query":"\"database migration\"","limit":10,"explain":true}
sender_filter|search_messages|strict_set|Sender filter parity|{"project_key":"__ALPHA__","query":"consensus","sender":"SilverWolf","limit":10,"explain":true}
thread_filter|search_messages|strict_set|Thread filter parity|{"project_key":"__ALPHA__","query":"GraphQL","thread_id":"thread-api-redesign","limit":10,"explain":true}
importance_filter|search_messages|strict_set|Importance filter parity|{"project_key":"__ALPHA__","query":"security","importance":"urgent","limit":10,"explain":true}
not_operator|search_messages|rank_relaxed|NOT-operator parity|{"project_key":"__ALPHA__","query":"deployment NOT checklist","limit":10,"explain":true}
unicode_query|search_messages|strict_set|Unicode query parity|{"project_key":"__ALPHA__","query":"Lokalisierungsdaten","limit":10,"explain":true}
date_window|search_messages|rank_relaxed|Date-filter parity|{"project_key":"__ALPHA__","query":"deployment","date_start":"2020-01-01T00:00:00Z","limit":12,"explain":true}
zero_result|search_messages|zero_exact|Zero-result parity|{"project_key":"__ALPHA__","query":"xyznonexistentterm123","limit":10,"explain":true}
product_cross_project|search_messages_product|product_relaxed|Cross-project product search parity|{"product_key":"__PRODUCT__","query":"deployment","limit":20}
EOF

EXPECTED_CASE_COUNT="${#PARITY_CASES[@]}"

for case_entry in "${PARITY_CASES[@]}"; do
    IFS='|' read -r case_id tool_name policy description args_template <<< "$case_entry"

    case_args="${args_template//__ALPHA__/$PROJECT_ALPHA}"
    case_args="${case_args//__BETA__/$PROJECT_BETA}"
    case_args="${case_args//__PRODUCT__/$PRODUCT_KEY}"

    run_parity_case "$case_id" "$tool_name" "$policy" "$description" "$case_args"
done

# ---------------------------------------------------------------------------
# Cutover gating + artifact contract
# ---------------------------------------------------------------------------

e2e_case_banner "Cutover readiness gate"

CUTOVER_REPORT="${PARITY_DIR}/cutover_readiness.json"
cutover_line="$(build_cutover_readiness_report "${PARITY_REPORTS_JSONL}" "${CUTOVER_REPORT}")"

IFS='|' read -r \
    cutover_ready_flag \
    unacceptable_cases \
    acceptance_bps \
    mean_overlap_bps \
    v3_filter_mismatch_total \
    call_error_total \
    <<< "$cutover_line"

e2e_assert_eq "cutover: unacceptable cases" "0" "$unacceptable_cases"
e2e_assert_eq "cutover: acceptance rate == 100%" "10000" "$acceptance_bps"
assert_numeric_ge "cutover: mean overlap >= 70%" "$mean_overlap_bps" "7000"
e2e_assert_eq "cutover: v3 filter mismatch total" "0" "$v3_filter_mismatch_total"
e2e_assert_eq "cutover: call error total" "0" "$call_error_total"
e2e_assert_eq "cutover: readiness flag" "1" "$cutover_ready_flag"

if [ "$cutover_ready_flag" = "1" ]; then
    search_v3_case_summary "cutover_gate" "pass" --message "ready=true overlap_bps=${mean_overlap_bps}"
else
    search_v3_case_summary "cutover_gate" "fail" --message "ready=false overlap_bps=${mean_overlap_bps}"
fi

# Generate suite-level Search V3 summary artifacts before contract checks.
search_v3_suite_summary || true

e2e_case_banner "Artifact contract"

e2e_assert_file_exists "parity case reports jsonl" "${PARITY_REPORTS_JSONL}"
e2e_assert_file_exists "cutover readiness report" "${CUTOVER_REPORT}"
e2e_assert_file_exists "search_v3 run manifest" "${SEARCH_V3_RUN_DIR}/run_manifest.json"
e2e_assert_file_exists "search_v3 suite summary json" "${SEARCH_V3_RUN_DIR}/summaries/suite_summary.json"
e2e_assert_file_exists "search_v3 suite summary log" "${SEARCH_V3_RUN_DIR}/logs/summary.log"

REPORT_COUNT="$(wc -l < "${PARITY_REPORTS_JSONL}" | tr -d ' ')"
e2e_assert_eq "parity report count matches case count" "${EXPECTED_CASE_COUNT}" "${REPORT_COUNT}"

if [ "$REPORT_COUNT" -eq "$EXPECTED_CASE_COUNT" ]; then
    search_v3_case_summary "artifact_contract" "pass" --message "reports=${REPORT_COUNT}"
else
    search_v3_case_summary "artifact_contract" "fail" --message "reports=${REPORT_COUNT} expected=${EXPECTED_CASE_COUNT}"
fi

# ---------------------------------------------------------------------------
# Final summary
# ---------------------------------------------------------------------------

e2e_summary
search_v3_log "Shadow parity artifacts: ${PARITY_DIR}"
