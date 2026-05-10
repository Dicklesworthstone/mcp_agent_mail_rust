#!/usr/bin/env bash
# Helpers for starting isolated E2E server processes.

set -euo pipefail

spawn_server_help() {
    cat <<'EOF'
Usage:
  source tests/e2e/lib/spawn_server.sh
  spawn_server_with_storage <label> <work_dir> <storage_root> <db_path> <stdout> <stderr> -- <command> [args...]
  stop_spawned_server <pid>

The spawned command receives DATABASE_URL and STORAGE_ROOT for an isolated test mailbox.
The function prints the spawned process id.
EOF
}

spawn_server_with_storage() {
    local label="$1"
    local work_dir="$2"
    local storage_root="$3"
    local db_path="$4"
    local stdout_path="$5"
    local stderr_path="$6"
    shift 6
    if [ "${1:-}" = "--" ]; then
        shift
    fi
    if [ "$#" -eq 0 ]; then
        echo "spawn_server_with_storage: missing command" >&2
        return 2
    fi

    mkdir -p "${work_dir}" "${storage_root}" "$(dirname "${stdout_path}")" "$(dirname "${stderr_path}")"
    local pid_path="${work_dir}/${label}.pid"
    (
        env \
            "DATABASE_URL=sqlite://${db_path}" \
            "STORAGE_ROOT=${storage_root}" \
            "$@" >"${stdout_path}" 2>"${stderr_path}" &
        echo "$!" >"${pid_path}"
    )
    cat "${pid_path}"
}

stop_spawned_server() {
    local pid="$1"
    if kill -0 "${pid}" 2>/dev/null; then
        kill "${pid}" 2>/dev/null || true
        wait "${pid}" 2>/dev/null || true
    fi
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
    case "${1:-}" in
        -h|--help|"")
            spawn_server_help
            ;;
        *)
            spawn_server_help >&2
            exit 2
            ;;
    esac
fi
