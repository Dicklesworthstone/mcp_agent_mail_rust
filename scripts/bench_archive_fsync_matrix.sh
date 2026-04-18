#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)"
OUTPUT_DIR="${PROJECT_ROOT}/tests/artifacts/perf/archive_fsync_matrix/${RUN_ID}"
FILESYSTEM=""
TARGET_DIR="${TMPDIR:-/tmp}/target-$(whoami)-am"

usage() {
    cat <<'USAGE'
Usage: scripts/bench_archive_fsync_matrix.sh --filesystem <id> [--output-dir <path>] [--target-dir <path>]

Supported filesystems:
  Linux:  ext4-ordered, ext4-journal, xfs, btrfs, tmpfs
  macOS:  apfs
USAGE
}

require_cmd() {
    local cmd="$1"
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        printf 'error: required command not found: %s\n' "${cmd}" >&2
        exit 1
    fi
}

cleanup() {
    set +e
    if [ -n "${MOUNT_POINT:-}" ] && mount | grep -q " on ${MOUNT_POINT} "; then
        sudo umount "${MOUNT_POINT}" >/dev/null 2>&1 || true
    fi
    if [ -n "${MOUNT_POINT:-}" ] && [ -d "${MOUNT_POINT}" ]; then
        rmdir "${MOUNT_POINT}" >/dev/null 2>&1 || true
    fi
    if [ -n "${IMAGE_PATH:-}" ] && [ -f "${IMAGE_PATH}" ]; then
        rm -f "${IMAGE_PATH}" >/dev/null 2>&1 || true
    fi
}

while [ $# -gt 0 ]; do
    case "$1" in
        --filesystem)
            FILESYSTEM="${2:-}"
            shift 2
            ;;
        --output-dir)
            OUTPUT_DIR="${2:-}"
            shift 2
            ;;
        --target-dir)
            TARGET_DIR="${2:-}"
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

if [ -z "${FILESYSTEM}" ]; then
    printf 'error: --filesystem is required\n' >&2
    usage >&2
    exit 1
fi

trap cleanup EXIT

require_cmd cargo
require_cmd python3

mkdir -p "${OUTPUT_DIR}"

OS_NAME="$(uname -s)"
STORAGE_ROOT=""
FS_LABEL=""
MOUNT_OPTIONS=""
FSYNC_MODE=""
SINGLE_BUDGET_MS=""
BATCH_BUDGET_MS=""
MOUNT_POINT=""
IMAGE_PATH=""

linux_mount_loopback() {
    local image_size="$1"
    local filesystem_kind="$2"
    local mount_opts="$3"

    require_cmd sudo
    MOUNT_POINT="$(mktemp -d "/tmp/archive-fsync-${FILESYSTEM}.XXXXXX")"
    IMAGE_PATH="${MOUNT_POINT}.img"
    truncate -s "${image_size}" "${IMAGE_PATH}"
    case "${filesystem_kind}" in
        ext4)
            mkfs.ext4 -F "${IMAGE_PATH}" >/dev/null
            ;;
        xfs)
            mkfs.xfs -f "${IMAGE_PATH}" >/dev/null
            ;;
        btrfs)
            mkfs.btrfs -f "${IMAGE_PATH}" >/dev/null
            ;;
        *)
            printf 'error: unsupported mkfs kind: %s\n' "${filesystem_kind}" >&2
            exit 1
            ;;
    esac
    sudo mount ${mount_opts} "${IMAGE_PATH}" "${MOUNT_POINT}"
    sudo chown "$(id -u):$(id -g)" "${MOUNT_POINT}"
    STORAGE_ROOT="${MOUNT_POINT}/storage"
    mkdir -p "${STORAGE_ROOT}"
    FS_LABEL="$(findmnt -no FSTYPE -T "${MOUNT_POINT}")"
    MOUNT_OPTIONS="$(findmnt -no OPTIONS -T "${MOUNT_POINT}")"
}

case "${OS_NAME}" in
    Linux)
        require_cmd findmnt
        require_cmd truncate
        case "${FILESYSTEM}" in
            ext4-ordered)
                require_cmd mkfs.ext4
                FSYNC_MODE="normal"
                SINGLE_BUDGET_MS="25"
                BATCH_BUDGET_MS="250"
                linux_mount_loopback "1536M" "ext4" "-o loop,data=ordered"
                ;;
            ext4-journal)
                require_cmd mkfs.ext4
                FSYNC_MODE="normal"
                SINGLE_BUDGET_MS="40"
                BATCH_BUDGET_MS="350"
                linux_mount_loopback "1536M" "ext4" "-o loop,data=journal"
                ;;
            xfs)
                require_cmd mkfs.xfs
                FSYNC_MODE="normal"
                SINGLE_BUDGET_MS="25"
                BATCH_BUDGET_MS="250"
                linux_mount_loopback "1536M" "xfs" "-o loop"
                ;;
            btrfs)
                require_cmd mkfs.btrfs
                FSYNC_MODE="normal"
                SINGLE_BUDGET_MS="50"
                BATCH_BUDGET_MS="500"
                linux_mount_loopback "2048M" "btrfs" "-o loop"
                ;;
            tmpfs)
                require_cmd sudo
                MOUNT_POINT="$(mktemp -d "/tmp/archive-fsync-${FILESYSTEM}.XXXXXX")"
                sudo mount -t tmpfs -o size=1536m tmpfs "${MOUNT_POINT}"
                sudo chown "$(id -u):$(id -g)" "${MOUNT_POINT}"
                STORAGE_ROOT="${MOUNT_POINT}/storage"
                mkdir -p "${STORAGE_ROOT}"
                FS_LABEL="$(findmnt -no FSTYPE -T "${MOUNT_POINT}")"
                MOUNT_OPTIONS="$(findmnt -no OPTIONS -T "${MOUNT_POINT}")"
                FSYNC_MODE="buffered"
                SINGLE_BUDGET_MS="15"
                BATCH_BUDGET_MS="150"
                ;;
            *)
                printf 'error: unsupported filesystem on Linux: %s\n' "${FILESYSTEM}" >&2
                exit 1
                ;;
        esac
        ;;
    Darwin)
        if [ "${FILESYSTEM}" != "apfs" ]; then
            printf 'error: macOS only supports --filesystem apfs\n' >&2
            exit 1
        fi
        STORAGE_ROOT="${TMPDIR%/}/archive-fsync-apfs-${RUN_ID}/storage"
        mkdir -p "${STORAGE_ROOT}"
        FS_LABEL="apfs"
        MOUNT_OPTIONS="runner-managed"
        FSYNC_MODE="barrier_only"
        SINGLE_BUDGET_MS="35"
        BATCH_BUDGET_MS="300"
        ;;
    *)
        printf 'error: unsupported OS for archive fsync matrix: %s\n' "${OS_NAME}" >&2
        exit 1
        ;;
esac

export OUTPUT_DIR RUN_ID OS_NAME FILESYSTEM FS_LABEL MOUNT_OPTIONS FSYNC_MODE STORAGE_ROOT TARGET_DIR

python3 <<'PY'
import json
import os
from pathlib import Path

output = Path(os.environ["OUTPUT_DIR"])
output.mkdir(parents=True, exist_ok=True)
(output / "environment.json").write_text(
    json.dumps(
        {
            "bead_id": "br-8qdh0.11",
            "run_id": os.environ["RUN_ID"],
            "os_name": os.environ["OS_NAME"],
            "filesystem_arg": os.environ["FILESYSTEM"],
            "fs_label": os.environ["FS_LABEL"],
            "mount_options": os.environ["MOUNT_OPTIONS"],
            "fsync_mode": os.environ["FSYNC_MODE"],
            "storage_root": os.environ["STORAGE_ROOT"],
            "cargo_target_dir": os.environ["TARGET_DIR"],
        },
        indent=2,
    )
    + "\n",
    encoding="utf-8",
)
PY

printf '==> archive fsync matrix on %s (%s)\n' "${FS_LABEL}" "${FSYNC_MODE}"

cd "${PROJECT_ROOT}"
AM_FSYNC_MATRIX_FS_LABEL="${FS_LABEL}" \
AM_FSYNC_MATRIX_MOUNT_OPTIONS="${MOUNT_OPTIONS}" \
AM_FSYNC_MATRIX_FSYNC_MODE="${FSYNC_MODE}" \
AM_FSYNC_MATRIX_STORAGE_ROOT="${STORAGE_ROOT}" \
AM_FSYNC_MATRIX_ARTIFACT_DIR="${OUTPUT_DIR}" \
AM_FSYNC_MATRIX_SINGLE_P95_BUDGET_MS="${SINGLE_BUDGET_MS}" \
AM_FSYNC_MATRIX_BATCH_100_P95_BUDGET_MS="${BATCH_BUDGET_MS}" \
CARGO_TARGET_DIR="${TARGET_DIR}" \
cargo test -p mcp-agent-mail-storage --test fsync_matrix archive_fsync_matrix_probe -- --ignored --exact --nocapture

printf 'artifact summary: %s\n' "${OUTPUT_DIR}/summary.json"
