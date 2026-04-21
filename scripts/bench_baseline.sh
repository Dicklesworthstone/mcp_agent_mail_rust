#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
BENCH_ROOT="${PROJECT_ROOT}"
BEAD_ID="br-8qdh0.8"

RUN_ID="$(date -u '+%s_%6N')"
OUTPUT_DIR="${PROJECT_ROOT}/tests/artifacts/bench/archive_baseline/${RUN_ID}"
BENCH_COMMAND="MCP_AGENT_MAIL_ARCHIVE_PROFILE=1 rch exec -- cargo bench -p mcp-agent-mail --bench benchmarks -- archive_write_batch"
OUTPUT_DIR_EXPLICIT=0

usage() {
    cat <<'USAGE'
Usage: scripts/bench_baseline.sh [--output-dir <path>] [--run-id <value>]

Captures the current hardware/environment fingerprint, runs the archive batch
benchmark with the profiling side-artifacts enabled, and writes a timestamped
baseline bundle under tests/artifacts/bench/archive_baseline/<run_id>/.

Options:
  --bench-root <path>    Run the benchmark from a clean checkout/worktree at <path>
                         while still writing the artifact bundle back into this repo.
USAGE
}

require_cmd() {
    local cmd="$1"
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        printf 'error: required command not found: %s\n' "${cmd}" >&2
        exit 1
    fi
}

require_value() {
    local label="$1"
    local value="$2"
    if [ -z "${value}" ]; then
        printf 'error: missing fingerprint field: %s\n' "${label}" >&2
        exit 1
    fi
}

trim() {
    sed 's/^[[:space:]]*//; s/[[:space:]]*$//'
}

while [ $# -gt 0 ]; do
    case "$1" in
        --output-dir)
            OUTPUT_DIR="${2:-}"
            OUTPUT_DIR_EXPLICIT=1
            shift 2
            ;;
        --run-id)
            RUN_ID="${2:-}"
            shift 2
            ;;
        --target-dir)
            printf 'error: --target-dir is not supported for rch remote benchmarks; let rch manage the worker target dir.\n' >&2
            exit 2
            ;;
        --bench-root)
            BENCH_ROOT="${2:-}"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            printf 'error: unknown argument: %s\n' "$1" >&2
            usage >&2
            exit 1
            ;;
    esac
done

if [ "${OUTPUT_DIR_EXPLICIT}" -eq 0 ]; then
    OUTPUT_DIR="${PROJECT_ROOT}/tests/artifacts/bench/archive_baseline/${RUN_ID}"
fi

BENCH_COMMAND="MCP_AGENT_MAIL_ARCHIVE_PROFILE=1 rch exec -- cargo bench -p mcp-agent-mail --bench benchmarks -- archive_write_batch"

require_cmd jq
require_cmd rch
require_cmd rustc
require_cmd git
require_cmd lscpu
require_cmd findmnt
require_cmd lsblk
require_cmd free

BENCH_ROOT="$(cd "${BENCH_ROOT}" && pwd)"

if [ ! -f "${BENCH_ROOT}/Cargo.toml" ]; then
    printf 'error: bench root does not look like a cargo workspace: %s\n' "${BENCH_ROOT}" >&2
    exit 1
fi

cd "${PROJECT_ROOT}"

created_at_utc="$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
host_name="$(hostname)"
uname_full="$(uname -a)"
os_pretty="$(
    . /etc/os-release
    printf '%s' "${PRETTY_NAME:-${NAME:-unknown}}"
)"
kernel_release="$(uname -r)"
cpu_model="$(lscpu | awk -F: '/Model name/ {print $2; exit}' | trim)"
cpu_count="$(nproc --all | trim)"
threads_per_core="$(lscpu | awk -F: '/Thread\(s\) per core/ {print $2; exit}' | trim)"
cores_per_socket="$(lscpu | awk -F: '/Core\(s\) per socket/ {print $2; exit}' | trim)"
socket_count="$(lscpu | awk -F: '/Socket\(s\)/ {print $2; exit}' | trim)"
cpu_max_mhz="$(lscpu | awk -F: '/CPU max MHz/ {print $2; exit}' | trim)"
cpu_min_mhz="$(lscpu | awk -F: '/CPU min MHz/ {print $2; exit}' | trim)"
memory_total="$(free -h | awk '/^Mem:/ {print $2; exit}' | trim)"
swap_total="$(free -h | awk '/^Swap:/ {print $2; exit}' | trim)"
data_source="$(findmnt -no SOURCE /data | trim)"
data_fstype="$(findmnt -no FSTYPE /data | trim)"
data_mount_options="$(findmnt -no OPTIONS /data | trim)"
root_source="$(findmnt -no SOURCE / | trim)"
root_fstype="$(findmnt -no FSTYPE / | trim)"
root_mount_options="$(findmnt -no OPTIONS / | trim)"
storage_model="$(lsblk -ndo MODEL "${data_source}" 2>/dev/null | trim)"
storage_transport="$(lsblk -ndo TRAN "${data_source}" 2>/dev/null | trim)"
storage_size="$(lsblk -ndo SIZE "${data_source}" 2>/dev/null | trim)"
cpu_governor="$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor | trim)"
rustc_verbose="$(rustc -Vv)"
rust_toolchain_file="$(cat "${BENCH_ROOT}/rust-toolchain.toml")"
bench_git_commit="$(git -C "${BENCH_ROOT}" rev-parse HEAD | trim)"
bench_git_subject="$(git -C "${BENCH_ROOT}" show -s --format=%s HEAD | trim)"

require_value host_name "${host_name}"
require_value uname_full "${uname_full}"
require_value os_pretty "${os_pretty}"
require_value kernel_release "${kernel_release}"
require_value cpu_model "${cpu_model}"
require_value cpu_count "${cpu_count}"
require_value threads_per_core "${threads_per_core}"
require_value cores_per_socket "${cores_per_socket}"
require_value socket_count "${socket_count}"
require_value memory_total "${memory_total}"
require_value data_source "${data_source}"
require_value data_fstype "${data_fstype}"
require_value data_mount_options "${data_mount_options}"
require_value storage_model "${storage_model}"
require_value storage_transport "${storage_transport}"
require_value storage_size "${storage_size}"
require_value cpu_governor "${cpu_governor}"
require_value rustc_verbose "${rustc_verbose}"
require_value bench_git_commit "${bench_git_commit}"

mkdir -p "${OUTPUT_DIR}"

fingerprint_json="$(jq -n \
    --arg bead_id "${BEAD_ID}" \
    --arg run_id "${RUN_ID}" \
    --arg created_at_utc "${created_at_utc}" \
    --arg host_name "${host_name}" \
    --arg uname_full "${uname_full}" \
    --arg os_pretty "${os_pretty}" \
    --arg kernel_release "${kernel_release}" \
    --arg cpu_model "${cpu_model}" \
    --arg cpu_count "${cpu_count}" \
    --arg threads_per_core "${threads_per_core}" \
    --arg cores_per_socket "${cores_per_socket}" \
    --arg socket_count "${socket_count}" \
    --arg cpu_max_mhz "${cpu_max_mhz}" \
    --arg cpu_min_mhz "${cpu_min_mhz}" \
    --arg memory_total "${memory_total}" \
    --arg swap_total "${swap_total}" \
    --arg data_source "${data_source}" \
    --arg data_fstype "${data_fstype}" \
    --arg data_mount_options "${data_mount_options}" \
    --arg root_source "${root_source}" \
    --arg root_fstype "${root_fstype}" \
    --arg root_mount_options "${root_mount_options}" \
    --arg storage_model "${storage_model}" \
    --arg storage_transport "${storage_transport}" \
    --arg storage_size "${storage_size}" \
    --arg cpu_governor "${cpu_governor}" \
    --arg target_dir "rch-managed-default" \
    --arg rust_toolchain_file "${rust_toolchain_file}" \
    --arg rustc_verbose "${rustc_verbose}" \
    --arg bench_git_commit "${bench_git_commit}" \
    --arg bench_git_subject "${bench_git_subject}" \
    '{
      bead_id: $bead_id,
      run_id: $run_id,
      created_at_utc: $created_at_utc,
      host_name: $host_name,
      uname_full: $uname_full,
      os: {
        pretty: $os_pretty,
        kernel_release: $kernel_release
      },
      cpu: {
        model: $cpu_model,
        logical_cpus: ($cpu_count | tonumber),
        threads_per_core: ($threads_per_core | tonumber),
        cores_per_socket: ($cores_per_socket | tonumber),
        sockets: ($socket_count | tonumber),
        max_mhz: ($cpu_max_mhz | tonumber),
        min_mhz: ($cpu_min_mhz | tonumber),
        governor: $cpu_governor
      },
      memory: {
        total_human: $memory_total,
        swap_human: $swap_total
      },
      storage: {
        workspace_mount: {
          source: $data_source,
          filesystem: $data_fstype,
          mount_options: $data_mount_options
        },
        root_mount: {
          source: $root_source,
          filesystem: $root_fstype,
          mount_options: $root_mount_options
        },
        device: {
          model: $storage_model,
          transport: $storage_transport,
          size_human: $storage_size
        }
      },
      rust: {
        toolchain_file: $rust_toolchain_file,
        rustc_verbose: $rustc_verbose,
        cargo_target_dir: $target_dir
      },
      benchmark_source: {
        git_commit: $bench_git_commit,
        git_subject: $bench_git_subject
      }
    }')"

printf '%s\n' "${fingerprint_json}" > "${OUTPUT_DIR}/fingerprint.json"

bench_log="${OUTPUT_DIR}/bench_command.log"

printf '==> %s\n' "${BENCH_COMMAND}"
cd "${BENCH_ROOT}"
set +e
MCP_AGENT_MAIL_ARCHIVE_PROFILE=1 \
    rch exec -- cargo bench -p mcp-agent-mail --bench benchmarks -- archive_write_batch \
    2>&1 | tee "${bench_log}"
bench_exit="${PIPESTATUS[0]}"
set -e
cd "${PROJECT_ROOT}"

if [ "${bench_exit}" -ne 0 ]; then
    failure_summary_json="$(jq -n \
        --argjson fingerprint "${fingerprint_json}" \
        --arg bead_id "${BEAD_ID}" \
        --arg run_id "${RUN_ID}" \
        --arg created_at_utc "${created_at_utc}" \
        --arg command "${BENCH_COMMAND}" \
        --arg project_root "${PROJECT_ROOT}" \
        --arg bench_root "${BENCH_ROOT}" \
        --arg output_dir "${OUTPUT_DIR}" \
        --arg rollback_runbook "docs/OPERATOR_RUNBOOK.md#emergency-roll-back-archive-batch-write-optimization" \
        --arg failure_log "bench_command.log" \
        --arg failure_excerpt "$(tail -n 80 "${bench_log}")" \
        --argjson exit_code "${bench_exit}" \
        '{
          schema_version: 1,
          status: "build_blocked",
          bead_id: $bead_id,
          run_id: $run_id,
          created_at_utc: $created_at_utc,
          project_root: $project_root,
          bench_root: $bench_root,
          output_dir: $output_dir,
          command: $command,
          fingerprint: $fingerprint,
          baseline_references: {
            budgets_doc: "benches/BUDGETS.md",
            rollback_runbook: $rollback_runbook
          },
          results: [],
          failure: {
            mode: "cargo_bench_failed_before_measurement",
            exit_code: $exit_code,
            log_path: $failure_log,
            excerpt: $failure_excerpt
          }
        }')"
    printf '%s\n' "${failure_summary_json}" > "${OUTPUT_DIR}/summary.json"
    printf 'wrote blocked baseline bundle: %s\n' "${OUTPUT_DIR}" >&2
    exit "${bench_exit}"
fi

csv_source="${BENCH_ROOT}/tests/artifacts/perf/archive_batch_scaling.csv"
profile_md_source="${BENCH_ROOT}/tests/artifacts/perf/archive_batch_100_profile.md"
spans_json_source="${BENCH_ROOT}/tests/artifacts/perf/archive_batch_100_spans.json"
flamegraph_source="${BENCH_ROOT}/tests/artifacts/perf/archive_batch_100_flamegraph.svg"

for required_file in "${csv_source}" "${profile_md_source}" "${spans_json_source}"; do
    if [ ! -f "${required_file}" ]; then
        printf 'error: expected benchmark artifact missing: %s\n' "${required_file}" >&2
        exit 1
    fi
done

results_json="$(tail -n +2 "${csv_source}" | jq -Rsc '
    split("\n")
    | map(select(length > 0))
    | map(split(","))
    | map({
        scenario: ("batch-" + .[0]),
        batch_size: (.[0] | tonumber),
        p50_us: (.[1] | tonumber),
        p95_us: (.[2] | tonumber),
        p99_us: (.[3] | tonumber),
        sample_count: (.[4] | tonumber)
      })
')"

cp "${csv_source}" "${OUTPUT_DIR}/archive_batch_scaling.csv"
cp "${profile_md_source}" "${OUTPUT_DIR}/archive_batch_100_profile.md"
cp "${spans_json_source}" "${OUTPUT_DIR}/archive_batch_100_spans.json"
if [ -f "${flamegraph_source}" ]; then
    cp "${flamegraph_source}" "${OUTPUT_DIR}/archive_batch_100_flamegraph.svg"
    flamegraph_artifact="${OUTPUT_DIR}/archive_batch_100_flamegraph.svg"
else
    flamegraph_artifact=""
fi

summary_json="$(jq -n \
    --argjson fingerprint "${fingerprint_json}" \
    --argjson results "${results_json}" \
    --arg bead_id "${BEAD_ID}" \
    --arg run_id "${RUN_ID}" \
    --arg created_at_utc "${created_at_utc}" \
    --arg command "${BENCH_COMMAND}" \
    --arg project_root "${PROJECT_ROOT}" \
    --arg bench_root "${BENCH_ROOT}" \
    --arg output_dir "${OUTPUT_DIR}" \
    --arg rollback_runbook "docs/OPERATOR_RUNBOOK.md#emergency-roll-back-archive-batch-write-optimization" \
    --arg flamegraph_artifact "${flamegraph_artifact}" \
    '{
      schema_version: 1,
      status: "ok",
      bead_id: $bead_id,
      run_id: $run_id,
      created_at_utc: $created_at_utc,
      project_root: $project_root,
      bench_root: $bench_root,
      output_dir: $output_dir,
      command: $command,
      fingerprint: $fingerprint,
      build_profile: {
        cargo_profile: "bench",
        inherits: "release",
        release_settings: {
          opt_level: 3,
          lto: "thin",
          codegen_units: 1,
          panic: "abort",
          strip: "symbols"
        }
      },
      runtime: {
        measurement: "wall-clock",
        harness: "criterion archive_write_batch + MCP_AGENT_MAIL_ARCHIVE_PROFILE side-artifacts",
        workload_isolation: "bare host, CPU governor=performance, no taskset/cgroup pinning applied by the harness"
      },
      tolerance: {
        same_host_expected_variance_pct: 10,
        regression_threshold_pct: 10,
        sustained_regression_threshold_pct: 20,
        guidance: "Treat <=10% batch-100 p95 drift on the same host/storage/kernel/governor as noise. Investigate >10% drift; escalate at >=20% or if the delta persists across three runs. Do not compare across different CPU/storage/filesystem stacks without restaging the baseline."
      },
      baseline_references: {
        budgets_doc: "benches/BUDGETS.md",
        rollback_runbook: $rollback_runbook
      },
      results: $results,
      artifacts: {
        scaling_csv: "archive_batch_scaling.csv",
        profile_md: "archive_batch_100_profile.md",
        spans_json: "archive_batch_100_spans.json",
        flamegraph_svg: $flamegraph_artifact
      }
    }')"

printf '%s\n' "${summary_json}" > "${OUTPUT_DIR}/summary.json"

printf 'wrote baseline bundle: %s\n' "${OUTPUT_DIR}"
