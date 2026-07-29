#!/usr/bin/env bash

set -euo pipefail

TEST_NAME="archive_perf_baseline"
PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
RUN_ID="$(date -u '+%Y%m%dT%H%M%SZ')"
LOG_DIR="${PROJECT_ROOT}/tests/artifacts/perf/${TEST_NAME}/${RUN_ID}"
EVENTS_JSONL="${LOG_DIR}/events.jsonl"

mkdir -p "${LOG_DIR}"

passed=0
failed=0

json_message() {
    python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))'
}

log_event() {
    local level="$1" scenario="$2" message="$3" data="${4:-{}}"
    printf '{"ts":"%s","test":"%s","scenario":"%s","level":"%s","message":%s,"data":%s}\n' \
        "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" \
        "${TEST_NAME}" \
        "${scenario}" \
        "${level}" \
        "$(printf '%s' "${message}" | json_message)" \
        "${data}" | tee -a "${EVENTS_JSONL}"
}

pass() {
    passed=$((passed + 1))
    log_event "pass" "$1" "$2"
}

fail() {
    failed=$((failed + 1))
    log_event "fail" "$1" "$2"
}

assert_cmd() {
    local scenario="$1"
    shift
    local stdout="${LOG_DIR}/${scenario}.stdout"
    local stderr="${LOG_DIR}/${scenario}.stderr"
    if "$@" >"${stdout}" 2>"${stderr}"; then
        pass "${scenario}" "command exited 0"
    else
        fail "${scenario}" "command failed; see ${stderr}"
    fi
}

write_summary_fixture() {
    local path="$1"
    local single_p50="$2"
    local single_p95="$3"
    local single_p99="$4"
    cat >"${path}" <<JSON
{
  "run_id": "fixture-${single_p95}",
  "arch": "x86_64",
  "os": "linux",
  "budget_regressions": 0,
  "results": [
    {
      "scenario": "single_inline_attachment",
      "elements_per_op": 1,
      "samples_us": [${single_p50}, ${single_p95}, ${single_p99}],
      "p50_us": ${single_p50},
      "p95_us": ${single_p95},
      "p99_us": ${single_p99},
      "p99_9_us": ${single_p99},
      "p99_99_us": ${single_p99},
      "max_us": ${single_p99},
      "budget_p95_us": 25000,
      "budget_p99_us": 30000,
      "p95_within_budget": true,
      "p99_within_budget": true,
      "p95_delta_us": -1000,
      "p99_delta_us": -1000,
      "throughput_elements_per_sec": 40.0
    }
  ]
}
JSON
}

write_scaling_fixture() {
    local path="$1"
    local factor="$2"
    python3 - "${path}" "${factor}" <<'PY'
import csv
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
factor = int(sys.argv[2])
rows = [
    (1, 12000, 13000, 14000, 40, 70.0),
    (10, 28000, 30000, 32000, 25, 330.0),
    (100, 210000, 230000, 240000, 12, 420.0),
    (1000, 750000, 780000, 800000, 4, 1280.0),
]
with path.open("w", encoding="utf-8", newline="") as handle:
    writer = csv.writer(handle)
    writer.writerow([
        "batch_size",
        "p50_us",
        "p95_us",
        "p99_us",
        "sample_count",
        "p99_9_us",
        "p99_99_us",
        "max_us",
        "throughput_elements_per_sec",
    ])
    for batch_size, p50, p95, p99, samples, throughput in rows:
        writer.writerow([
            batch_size,
            p50 // factor,
            p95 // factor,
            p99 // factor,
            samples,
            p99 // factor,
            p99 // factor,
            p99 // factor,
            throughput * factor,
        ])
PY
}

write_stress_fixture() {
    local path="$1"
    local factor="$2"
    python3 - "${path}" "${factor}" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
factor = int(sys.argv[2])
payload = {
    "status": "measured",
    "metrics": {
        "agent_count": 30,
        "message_count": 900,
        "sample_count": 3,
        "p50_ms": 120 // factor,
        "p95_ms": 180 // factor,
        "p99_ms": 210 // factor,
        "max_ms": 210 // factor,
        "throughput_elements_per_sec": 300.0 * factor,
    },
}
path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
PY
}

fixture_dir="${LOG_DIR}/fixtures"
mkdir -p "${fixture_dir}"

pre_summary="${fixture_dir}/pre-summary.json"
post_summary="${fixture_dir}/post-summary.json"
pre_csv="${fixture_dir}/pre-scaling.csv"
post_csv="${fixture_dir}/post-scaling.csv"
pre_stress="${fixture_dir}/pre-stress.json"
post_stress="${fixture_dir}/post-stress.json"

write_summary_fixture "${pre_summary}" 20000 24000 26000
write_summary_fixture "${post_summary}" 10000 12000 13000
write_scaling_fixture "${pre_csv}" 1
write_scaling_fixture "${post_csv}" 2
write_stress_fixture "${pre_stress}" 1
write_stress_fixture "${post_stress}" 2

pre_baseline="${LOG_DIR}/baseline_pre_fix_20260509.json"
post_baseline="${LOG_DIR}/baseline_post_fix_20260509.json"
delta_json="${LOG_DIR}/fix_delta.json"

assert_cmd \
    "s1_capture_pre_baseline" \
    bash "${PROJECT_ROOT}/scripts/perf/capture_archive_baseline.sh" \
    pre \
    --skip-bench \
    --date 20260509 \
    --source-summary "${pre_summary}" \
    --source-scaling-csv "${pre_csv}" \
    --source-stress-json "${pre_stress}" \
    --output "${pre_baseline}"

assert_cmd \
    "s2_capture_post_baseline" \
    bash "${PROJECT_ROOT}/scripts/perf/capture_archive_baseline.sh" \
    post \
    --skip-bench \
    --date 20260509 \
    --source-summary "${post_summary}" \
    --source-scaling-csv "${post_csv}" \
    --source-stress-json "${post_stress}" \
    --output "${post_baseline}"

assert_cmd \
    "s3_compute_fix_delta" \
    python3 "${PROJECT_ROOT}/scripts/perf/compute_fix_delta.py" \
    "${pre_baseline}" \
    "${post_baseline}" \
    --output "${delta_json}"

if python3 - "${pre_baseline}" "${post_baseline}" "${delta_json}" <<'PY'
import json
import pathlib
import sys

required = {
    "batch-1",
    "batch-10",
    "batch-100",
    "batch-1000",
    "single-attachment",
    "30-agent-stress",
}
for raw in sys.argv[1:3]:
    payload = json.loads(pathlib.Path(raw).read_text(encoding="utf-8"))
    assert payload["status"] == "complete"
    assert set(payload["bench_points"]) == required
    for field in [
        "phase",
        "date",
        "host",
        "kernel",
        "cpu_model",
        "mem_total_gb",
        "rustc_version",
        "cargo_version",
        "profile",
    ]:
        assert field in payload["meta"], field

delta = json.loads(pathlib.Path(sys.argv[3]).read_text(encoding="utf-8"))
assert delta["status"] == "pass"
assert delta["summary"]["points"] == 6
assert delta["summary"]["improved"] == 6
assert all("percent_improvement" in item for item in delta["deltas"])
PY
then
    pass "s4_validate_artifact_shapes" "baseline and delta JSON shapes match br-q8yaa contract"
else
    fail "s4_validate_artifact_shapes" "baseline or delta JSON shape validation failed"
fi

if python3 "${PROJECT_ROOT}/scripts/perf/compute_fix_delta.py" --help >/dev/null; then
    pass "s5_delta_help" "compute_fix_delta.py exposes help"
else
    fail "s5_delta_help" "compute_fix_delta.py help failed"
fi

if bash "${PROJECT_ROOT}/scripts/perf/capture_archive_baseline.sh" --help >/dev/null; then
    pass "s6_capture_help" "capture_archive_baseline.sh exposes help"
else
    fail "s6_capture_help" "capture_archive_baseline.sh help failed"
fi

printf '{"ts":"%s","test":"%s","summary":{"scenarios":6,"passed":%d,"failed":%d,"log_dir":"%s","replay":"bash tests/e2e/test_archive_perf_baseline.sh"}}\n' \
    "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" \
    "${TEST_NAME}" \
    "${passed}" \
    "${failed}" \
    "${LOG_DIR}" | tee -a "${EVENTS_JSONL}"

if [ "${failed}" -ne 0 ]; then
    exit 1
fi
