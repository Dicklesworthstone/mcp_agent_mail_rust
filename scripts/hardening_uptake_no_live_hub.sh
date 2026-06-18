#!/usr/bin/env bash
# Stage and prove an Agent Mail hardening uptake without touching the live hub.
#
# This script intentionally installs into target/ by default. It never replaces
# the currently resolved `am`/`mcp-agent-mail` binaries and never starts/stops a
# service. Use the staged bin directory only for explicit canary shells.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

PROFILE="release"
STAGE_ROOT=""
ALLOW_SOURCE_CHANGES=0

usage() {
    cat <<'USAGE'
Usage: scripts/hardening_uptake_no_live_hub.sh [options]

Build and validate a staged Agent Mail binary without mutating the live install.

Options:
  --profile <release|debug>  Build profile to stage (default: release)
  --stage-root <path>        Artifact/staging root (default: target/hardening-uptake/<run>)
  --allow-source-changes     Permit a dirty tree for validating this script pre-commit
  -h, --help                 Show help

Validation:
  - cargo test -p mcp-agent-mail-db wal_classify -- --nocapture
  - cargo test -p mcp-agent-mail-cli wal_shm_sidecar_drift -- --nocapture
  - install-local.sh into <stage-root>/bin, refusing the current live bin dir
  - installed-binary parity against the staged <stage-root>/bin/am

Set CARGO_TARGET_DIR to place Cargo build artifacts outside the repo target
directory. This is useful on hosts where the source filesystem is under disk
pressure but a tmpfs or other staging filesystem has enough space.

The script writes logs, exit codes, command inventory, and uptake_report.txt
under <stage-root>/artifacts/.
USAGE
}

die() {
    printf 'ERROR: %s\n' "$*" >&2
    exit 1
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --profile)
            PROFILE="${2:-}"
            shift 2
            ;;
        --stage-root)
            STAGE_ROOT="${2:-}"
            shift 2
            ;;
        --allow-source-changes)
            ALLOW_SOURCE_CHANGES=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            usage >&2
            die "unknown argument: $1"
            ;;
    esac
done

case "${PROFILE}" in
    release|debug) ;;
    *) die "--profile must be release or debug" ;;
esac

require_cmd() {
    command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

canonical_dir() {
    local path="$1"
    if [ -d "${path}" ]; then
        (cd "${path}" && pwd -P)
    else
        printf '%s\n' "${path}"
    fi
}

command_line() {
    printf '%q ' "$@"
}

run_step() {
    local step_id="$1"
    shift

    local stdout_log="${ARTIFACT_DIR}/${step_id}.stdout.log"
    local stderr_log="${ARTIFACT_DIR}/${step_id}.stderr.log"
    local exit_code_file="${ARTIFACT_DIR}/${step_id}.exit_code"

    printf '%s\t' "${step_id}" >> "${COMMANDS_TSV}"
    command_line "$@" >> "${COMMANDS_TSV}"
    printf '\n' >> "${COMMANDS_TSV}"

    printf '\n==> %s\n' "${step_id}"
    set +e
    "$@" > >(tee "${stdout_log}") 2> >(tee "${stderr_log}" >&2)
    local rc=$?
    set -e
    printf '%s\n' "${rc}" > "${exit_code_file}"

    if [ "${rc}" -ne 0 ]; then
        write_report "failed" "${step_id}"
        printf 'FAILED: %s exited %s\n' "${step_id}" "${rc}" >&2
        printf 'Logs: %s %s\n' "${stdout_log}" "${stderr_log}" >&2
        exit "${rc}"
    fi
}

write_report() {
    local status="$1"
    local failed_step="${2:-}"
    local report="${ARTIFACT_DIR}/uptake_report.txt"

    {
        printf 'schema=agent-mail-hardening-uptake-no-live-hub-v1\n'
        printf 'status=%s\n' "${status}"
        if [ -n "${failed_step}" ]; then
            printf 'failed_step=%s\n' "${failed_step}"
        fi
        printf 'created_at_utc=%s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
        printf 'repo_root=%s\n' "${REPO_ROOT}"
        printf 'git_sha=%s\n' "${GIT_SHA}"
        printf 'profile=%s\n' "${PROFILE}"
        printf 'stage_root=%s\n' "${STAGE_ROOT}"
        printf 'stage_bin=%s\n' "${STAGE_BIN}"
        printf 'build_target_dir=%s\n' "${BUILD_TARGET_DIR:-not-set}"
        printf 'artifacts=%s\n' "${ARTIFACT_DIR}"
        printf 'live_am_path=%s\n' "${LIVE_AM_PATH:-not-found}"
        printf 'live_am_version=%s\n' "${LIVE_AM_VERSION:-not-found}"
        printf 'staged_am_path=%s\n' "${STAGE_BIN}/am"
        printf 'staged_am_version=%s\n' "${STAGED_AM_VERSION:-not-built}"
        printf 'staged_server_path=%s\n' "${STAGE_BIN}/mcp-agent-mail"
        printf 'version_gate=%s\n' "${VERSION_GATE:-not-evaluated}"
        printf 'install_gate=%s\n' "${INSTALL_GATE:-not-evaluated}"
        printf 'min_free_kib=%s\n' "${MIN_FREE_KIB:-not-checked}"
        printf 'repo_free_kib=%s\n' "${REPO_FREE_KIB:-not-checked}"
        printf 'stage_free_kib=%s\n' "${STAGE_FREE_KIB:-not-checked}"
        printf 'build_free_kib=%s\n' "${BUILD_FREE_KIB:-not-checked}"
        printf 'commands_tsv=%s\n' "${COMMANDS_TSV}"
        if [ -s "${SOURCE_CHANGES_TXT:-}" ]; then
            printf 'source_changes=%s\n' "${SOURCE_CHANGES_TXT}"
        fi
        printf 'no_live_install=true\n'
        printf 'live_install_dir_refused=%s\n' "${LIVE_AM_DIR:-not-found}"
        printf 'source_changes_allowed=%s\n' "${ALLOW_SOURCE_CHANGES}"
        printf 'canary_shell=PATH=%q:"$PATH" am --version\n' "${STAGE_BIN}"
    } > "${report}"

    printf '\nReport: %s\n' "${report}"
}

require_cmd cargo
require_cmd df
require_cmd git
require_cmd tee

cd "${REPO_ROOT}"

GIT_SHA="$(git rev-parse HEAD)"
GIT_SHORT="${GIT_SHA:0:8}"
RUN_ID="$(date -u '+%Y%m%dT%H%M%SZ')-${GIT_SHORT}-${PROFILE}"

if [ -z "${STAGE_ROOT}" ]; then
    STAGE_ROOT="${REPO_ROOT}/target/hardening-uptake/${RUN_ID}"
fi

STAGE_BIN="${STAGE_ROOT}/bin"
ARTIFACT_DIR="${STAGE_ROOT}/artifacts"
COMMANDS_TSV="${ARTIFACT_DIR}/commands.tsv"
SOURCE_CHANGES_TXT="${ARTIFACT_DIR}/source_changes.txt"
BUILD_TARGET_DIR="${CARGO_TARGET_DIR:-${REPO_ROOT}/target}"
case "${BUILD_TARGET_DIR}" in
    /*) ;;
    *) BUILD_TARGET_DIR="${REPO_ROOT}/${BUILD_TARGET_DIR}" ;;
esac
export CARGO_TARGET_DIR="${BUILD_TARGET_DIR}"

mkdir -p "${STAGE_BIN}" "${ARTIFACT_DIR}" "${BUILD_TARGET_DIR}"
: > "${COMMANDS_TSV}"

LIVE_AM_PATH="$(command -v am || true)"
LIVE_AM_DIR=""
LIVE_AM_VERSION=""
VERSION_GATE="not-evaluated"
INSTALL_GATE="not-evaluated"
MIN_FREE_KIB="${AM_HARDENING_UPTAKE_MIN_FREE_KIB:-67108864}"
REPO_FREE_KIB="not-checked"
STAGE_FREE_KIB="not-checked"
BUILD_FREE_KIB="not-checked"
if [ -n "${LIVE_AM_PATH}" ]; then
    LIVE_AM_DIR="$(dirname "${LIVE_AM_PATH}")"
    LIVE_AM_VERSION="$("${LIVE_AM_PATH}" --version 2>/dev/null || true)"
fi

if [ -n "${LIVE_AM_DIR}" ] \
    && [ "$(canonical_dir "${STAGE_BIN}")" = "$(canonical_dir "${LIVE_AM_DIR}")" ]; then
    die "stage bin resolves to the live am directory: ${LIVE_AM_DIR}"
fi

case ":${PATH}:" in
    *":${STAGE_BIN}:"*)
        die "stage bin is already in PATH; run from a shell that resolves live am normally"
        ;;
esac

SOURCE_CHANGES="$(git status --porcelain --untracked-files=all -- . ':!tests/artifacts' ':!target')"
if [ -n "${SOURCE_CHANGES}" ]; then
    printf '%s\n' "${SOURCE_CHANGES}" > "${SOURCE_CHANGES_TXT}"
    if [ "${ALLOW_SOURCE_CHANGES}" -eq 1 ]; then
        printf 'WARNING: source changes are present; continuing because --allow-source-changes was set.\n' >&2
    else
        die "worktree has source changes; commit/stash or choose a clean checkout before uptake proof"
    fi
fi

available_kib() {
    df -Pk "$1" | awk 'NR == 2 { print $4 }'
}

REPO_FREE_KIB="$(available_kib "${REPO_ROOT}")"
STAGE_FREE_KIB="$(available_kib "${STAGE_ROOT}")"
BUILD_FREE_KIB="$(available_kib "${BUILD_TARGET_DIR}")"

if [ "${STAGE_FREE_KIB}" -lt "${MIN_FREE_KIB}" ] || [ "${BUILD_FREE_KIB}" -lt "${MIN_FREE_KIB}" ]; then
    write_report "failed" "disk_preflight"
    die "insufficient free disk for hardening uptake: require ${MIN_FREE_KIB} KiB, stage has ${STAGE_FREE_KIB} KiB, build target has ${BUILD_FREE_KIB} KiB"
fi

INSTALL_ARGS=()
if [ "${PROFILE}" = "debug" ]; then
    INSTALL_ARGS+=(--debug)
fi

run_step wal_classifier_tests \
    cargo test -p mcp-agent-mail-db wal_classify -- --nocapture

run_step doctor_wal_detector_tests \
    cargo test -p mcp-agent-mail-cli wal_shm_sidecar_drift -- --nocapture

run_step stage_install \
    env DEST="${STAGE_BIN}" "${REPO_ROOT}/install-local.sh" "${INSTALL_ARGS[@]}"

STAGED_AM_VERSION="$("${STAGE_BIN}/am" --version 2>/dev/null || true)"
if [ -z "${LIVE_AM_VERSION}" ]; then
    VERSION_GATE="no-live-binary-detected"
    INSTALL_GATE="hold-until-operator-identifies-live-binary"
elif [ "${STAGED_AM_VERSION}" = "${LIVE_AM_VERSION}" ]; then
    VERSION_GATE="blocked-same-version-as-live"
    INSTALL_GATE="hold-until-candidate-version-is-distinguishable"
else
    VERSION_GATE="ok-distinguishable-version"
    INSTALL_GATE="candidate-version-distinguishable"
fi

run_step installed_binary_parity \
    env AM_INSTALLED_BINARY_PARITY_BIN="${STAGE_BIN}/am" \
    cargo test -p mcp-agent-mail-cli --test integration_runs \
    installed_binary_parity_probe_compares_source_and_installed_am -- --ignored --nocapture

write_report "passed"

printf '\nStaged candidate is ready for explicit canary shells only:\n'
printf '  PATH=%q:"$PATH" am --version\n' "${STAGE_BIN}"
printf '\nInstall gate: %s (%s)\n' "${INSTALL_GATE}" "${VERSION_GATE}"
printf '\nThis script did not replace the live am binary or touch live hub state.\n'
