#!/usr/bin/env bash
# e2e_spill_determinism.sh - Spill-path determinism E2E suite (br-1i11.1.6)
#
# Usage (authoritative):
#   am e2e run --project . spill_determinism
# Compatibility fallback:
#   AM_E2E_FORCE_LEGACY=1 ./scripts/e2e_test.sh spill_determinism
#   bash scripts/e2e_spill_determinism.sh
#
# Artifacts:
#   tests/artifacts/spill_determinism/<timestamp>/*
#   - case_01_stress_stdout.txt
#   - case_02_replay_stdout.txt
#   - case_03_bundle_status.txt
#   - spill_replay_bundle.json

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Suite identity (used for artifact directory naming inside e2e_lib.sh)
E2E_SUITE="spill_determinism"

# Safety: default to keeping temp dirs so shared harness cleanup doesn't run `rm -rf`.
: "${AM_E2E_KEEP_TMP:=1}"

# shellcheck source=./e2e_lib.sh
source "${SCRIPT_DIR}/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Spill-Path Determinism E2E Suite (br-1i11.1.6)"
e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"

for cmd in cargo python3; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        e2e_log "$cmd not found; skipping suite"
        e2e_skip "$cmd required"
        e2e_summary
        exit 0
    fi
done

# Ensure this suite is deterministic and reproducible.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/data/tmp/cargo-target}"

STRESS_CMD=(
    cargo test -p mcp-agent-mail-storage
    spill_drain_repeated_seeded_permutations_are_stable
    -- --nocapture
)
REPLAY_CMD=(
    cargo test -p mcp-agent-mail-storage
    spill_drain_seed_replay_contract
    -- --nocapture
)

e2e_case_banner "stress_seeded_permutation_sweep"
set +e
STRESS_OUT="$("${STRESS_CMD[@]}" 2>&1)"
STRESS_RC=$?
set -e
e2e_save_artifact "case_01_stress_stdout.txt" "$STRESS_OUT"
e2e_assert_exit_code "stress sweep exits 0" "0" "$STRESS_RC"

e2e_case_banner "seed_replay_contract"
set +e
REPLAY_OUT="$("${REPLAY_CMD[@]}" 2>&1)"
REPLAY_RC=$?
set -e
REPLAY_OUT_PATH="${E2E_ARTIFACT_DIR}/case_02_replay_stdout.txt"
e2e_save_artifact "case_02_replay_stdout.txt" "$REPLAY_OUT"
e2e_assert_exit_code "seed replay exits 0" "0" "$REPLAY_RC"

e2e_case_banner "bundle_generation_and_order_validation"
BUNDLE_PATH="${E2E_ARTIFACT_DIR}/spill_replay_bundle.json"
set +e
BUNDLE_STATUS="$(python3 - "$REPLAY_OUT_PATH" "$BUNDLE_PATH" <<'PY'
import json
import pathlib
import re
import sys

replay_stdout_path = pathlib.Path(sys.argv[1])
bundle_path = pathlib.Path(sys.argv[2])

try:
    text = replay_stdout_path.read_text(encoding="utf-8", errors="replace")
except FileNotFoundError:
    print(f"ERROR: replay stdout file not found: {replay_stdout_path}")
    raise SystemExit(2)

line = None
for raw in text.splitlines():
    if raw.startswith("spill replay seed="):
        line = raw
        break

if line is None:
    print("ERROR: replay output missing 'spill replay seed=' line")
    raise SystemExit(2)

match = re.match(
    r'^spill replay seed=(\d+)\s+input=(\[[^\n]*\])\s+output=(\[[^\n]*\])$',
    line.strip(),
)
if not match:
    print(f"ERROR: unable to parse replay line: {line}")
    raise SystemExit(3)

seed = int(match.group(1))
insertion_sequence = json.loads(match.group(2))
observed_order = json.loads(match.group(3))
expected_canonical_order = sorted(set(insertion_sequence))

bundle = {
    "suite": "spill_determinism",
    "seed": seed,
    "insertion_sequence": insertion_sequence,
    "observed_order": observed_order,
    "expected_canonical_order": expected_canonical_order,
    "reproduction_command": "cargo test -p mcp-agent-mail-storage spill_drain_seed_replay_contract -- --nocapture",
    "stress_command": "cargo test -p mcp-agent-mail-storage spill_drain_repeated_seeded_permutations_are_stable -- --nocapture",
}

bundle_path.write_text(json.dumps(bundle, indent=2) + "\n", encoding="utf-8")
print(f"WROTE_BUNDLE={bundle_path}")
print(f"SEED={seed}")
print(f"INPUT_COUNT={len(insertion_sequence)}")

if observed_order != expected_canonical_order:
    print("ERROR: observed_order does not match expected_canonical_order")
    raise SystemExit(4)

print("ORDER_VALID=true")
PY
)"
BUNDLE_RC=$?
set -e
e2e_save_artifact "case_03_bundle_status.txt" "$BUNDLE_STATUS"
e2e_assert_exit_code "bundle generation exits 0" "0" "$BUNDLE_RC"
e2e_assert_file_exists "spill replay bundle artifact exists" "$BUNDLE_PATH"

if [ "$BUNDLE_RC" -eq 0 ]; then
    e2e_pass "determinism bundle generated and validated"
else
    e2e_fail "determinism bundle validation failed"
fi

e2e_summary
