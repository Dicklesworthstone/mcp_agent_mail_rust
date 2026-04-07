#!/usr/bin/env bash
# truth_oracle_lib.sh
#
# Shared shell helpers for truth-oracle scripts.
# Callers may set:
#   VERBOSE=0|1
#   TRUTH_ORACLE_LOG_PREFIX=<label>

truth_oracle_sqlite3_compat_bin() {
    local tmp_root="${TMPDIR:-/tmp}"
    local target_dir="${CARGO_TARGET_DIR:-${tmp_root%/}/cargo-target}"
    local bin="${target_dir}/debug/test_db"
    if [ -x "${bin}" ]; then
        printf '%s\n' "${bin}"
        return 0
    fi

    mkdir -p "${target_dir}" >/dev/null 2>&1 || true
    if ! CARGO_TARGET_DIR="${target_dir}" cargo build -q -p mcp-agent-mail-cli --bin test_db >/dev/null 2>&1; then
        return 127
    fi
    if [ ! -x "${bin}" ]; then
        return 127
    fi
    printf '%s\n' "${bin}"
}

sqlite3() {
    local bin
    bin="$(truth_oracle_sqlite3_compat_bin)" || return 127
    "${bin}" "$@"
}

truth_oracle_db_query_first() {
    local db_path="$1"
    local sql="$2"
    AM_INTERFACE_MODE=cli am tooling db-query --db "${db_path}" --sql "${sql}" --first
}

truth_oracle_db_query_json() {
    local db_path="$1"
    local sql="$2"
    AM_INTERFACE_MODE=cli am tooling db-query --db "${db_path}" --sql "${sql}" --json
}

truth_oracle_db_exec() {
    local db_path="$1"
    local sql="${2:-}"
    if [ -n "${sql}" ]; then
        AM_INTERFACE_MODE=cli am tooling db-exec --db "${db_path}" --sql "${sql}"
    else
        AM_INTERFACE_MODE=cli am tooling db-exec --db "${db_path}"
    fi
}

log() {
    if [ "${VERBOSE:-0}" -eq 1 ]; then
        printf '[%s] %s\n' "${TRUTH_ORACLE_LOG_PREFIX:-truth-oracle}" "$*" >&2
    fi
}

die() {
    printf 'ERROR: %s\n' "$*" >&2
    exit 1
}

require_cmd() {
    local cmd="$1"
    command -v "${cmd}" >/dev/null 2>&1 || die "missing required command: ${cmd}"
}

require_cmds() {
    local cmd
    for cmd in "$@"; do
        require_cmd "${cmd}"
    done
}

require_non_empty() {
    local name="$1"
    local value="${2:-}"
    [ -n "${value}" ] || die "${name} cannot be empty"
}

require_int() {
    local name="$1"
    local value="${2:-}"
    case "${value}" in
        ''|*[!0-9]*)
            die "${name} must be an integer >= 0"
            ;;
    esac
}

require_int_vars() {
    local name
    for name in "$@"; do
        require_int "${name}" "${!name-}"
    done
}

require_port_or_empty() {
    local name="$1"
    local value="${2:-}"
    [ -z "${value}" ] && return 0
    case "${value}" in
        ''|*[!0-9]*)
            die "${name} must be a positive integer"
            ;;
    esac
    if [ "${value}" -lt 1 ] || [ "${value}" -gt 65535 ]; then
        die "${name} must be in [1, 65535]"
    fi
}

json_get_top_key() {
    local path="$1"
    local key="$2"
    local default="${3:-}"
    python3 - "${path}" "${key}" "${default}" <<'PY'
import json
import sys

path, key, default = sys.argv[1:4]
try:
    with open(path, "r", encoding="utf-8") as handle:
        parsed = json.load(handle)
except FileNotFoundError:
    print("" if default is None else default)
    sys.exit(0)
except Exception as exc:
    print(f"json_get_top_key: failed to parse {path}: {exc}", file=sys.stderr)
    sys.exit(2)

if not isinstance(parsed, dict):
    print(f"json_get_top_key: expected top-level object in {path}", file=sys.stderr)
    sys.exit(2)

value = parsed.get(key, default)

print("" if value is None else value)
PY
}
