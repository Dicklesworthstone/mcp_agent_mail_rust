#!/usr/bin/env bash
#
# Verifies that install.sh's LaunchAgent plist writer refuses symlinked service
# paths before writing. The full installer is intentionally not sourced because
# it performs network and installation work at the bottom of the script.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
INSTALL_SH="$REPO_ROOT/install.sh"

if [ ! -f "$INSTALL_SH" ]; then
    echo "FATAL: $INSTALL_SH not found" >&2
    exit 2
fi

ts() { date -u +%Y-%m-%dT%H:%M:%SZ; }
step() { echo "[LAUNCHD_PLIST_SAFETY_TEST $(ts)] $*" >&2; }
fail() {
    echo "FAIL: $*" >&2
    exit 1
}

tmp="$(mktemp -d "${TMPDIR:-/tmp}/am-install-launchd-safety.XXXXXX")"
trap 'step "temp left for inspection: $tmp"' EXIT

extract="$tmp/launchd_plist_functions.sh"
extract_function() {
    local fn="$1"
    awk -v fn="$fn" '
        $0 == fn "() {" {
            in_fn = 1
            print
            next
        }
        in_fn {
            print
            if ($0 == "}") {
                exit
            }
        }
    ' "$INSTALL_SH"
}

{
    printf '%s\n' 'warn() { printf "%s\n" "$*" >&2; }'
    printf '%s\n' 'verbose() { printf "%s\n" "$*" >&2; }'
    extract_function trim_ascii_whitespace
    extract_function strip_wrapping_quotes
    extract_function parse_env_assignment_rhs
    extract_function read_env_assignment_value
    extract_function desired_service_bind_host
    extract_function desired_service_bind_port
    extract_function plist_xml_escape
    extract_function plist_string_entry
    extract_function plist_env_entry
    extract_function ensure_real_directory_tree
    extract_function ensure_real_file_target_path
    extract_function write_launchd_service_plist
    extract_function repair_launchd_service_env_from_rust_config
} >"$extract"

for required in plist_xml_escape ensure_real_directory_tree ensure_real_file_target_path write_launchd_service_plist repair_launchd_service_env_from_rust_config; do
    if ! grep -q "^${required}()" "$extract"; then
        fail "could not extract ${required} from install.sh"
    fi
done

# shellcheck source=/dev/null
source "$extract"

write_test_plist() {
    local plist_path="$1"
    local home="$2"
    local storage_root="$3"
    write_launchd_service_plist \
        "$plist_path" \
        "/opt/agent mail/bin/am" \
        "$home" \
        "$storage_root" \
        "sqlite:///$storage_root/storage.sqlite3" \
        "tok&en" \
        "127.0.0.1" \
        "8765" \
        "/mcp/?x=<y>"
}

step "scenario A: normal plist write succeeds and XML-escapes values"
home_a="$tmp/home-a"
storage_a="$tmp/storage-a"
plist_a="$home_a/Library/LaunchAgents/com.agent-mail.plist"
write_test_plist "$plist_a" "$home_a" "$storage_a" || fail "normal plist write failed"
grep -q '<string>/opt/agent mail/bin/am</string>' "$plist_a" || fail "missing am binary argument"
grep -q '<string>tok&amp;en</string>' "$plist_a" || fail "token was not XML-escaped"
grep -q '<string>/mcp/?x=&lt;y&gt;</string>' "$plist_a" || fail "HTTP_PATH was not XML-escaped"

step "scenario B: symlinked plist target is rejected without mutating target"
home_b="$tmp/home-b"
storage_b="$tmp/storage-b"
mkdir -p "$home_b/Library/LaunchAgents"
outside_plist="$tmp/outside.plist"
plist_b="$home_b/Library/LaunchAgents/com.agent-mail.plist"
printf 'do not overwrite\n' >"$outside_plist"
ln -s "$outside_plist" "$plist_b"
if write_test_plist "$plist_b" "$home_b" "$storage_b"; then
    fail "symlinked plist target unexpectedly succeeded"
fi
[ "$(cat "$outside_plist")" = "do not overwrite" ] || fail "symlinked plist target was overwritten"

step "scenario C: symlinked LaunchAgents directory is rejected"
home_c="$tmp/home-c"
outside_agents="$tmp/outside-agents"
storage_c="$tmp/storage-c"
mkdir -p "$home_c/Library" "$outside_agents"
ln -s "$outside_agents" "$home_c/Library/LaunchAgents"
plist_c="$home_c/Library/LaunchAgents/com.agent-mail.plist"
if write_test_plist "$plist_c" "$home_c" "$storage_c"; then
    fail "symlinked LaunchAgents directory unexpectedly succeeded"
fi
[ ! -e "$outside_agents/com.agent-mail.plist" ] || fail "plist was written through symlinked LaunchAgents directory"

step "scenario D: symlinked storage root is rejected before plist is written"
home_d="$tmp/home-d"
outside_storage="$tmp/outside-storage"
storage_d="$tmp/storage-link"
plist_d="$home_d/Library/LaunchAgents/com.agent-mail.plist"
mkdir -p "$outside_storage"
ln -s "$outside_storage" "$storage_d"
if write_test_plist "$plist_d" "$home_d" "$storage_d"; then
    fail "symlinked storage root unexpectedly succeeded"
fi
[ ! -e "$plist_d" ] || fail "plist was written despite symlinked storage root"

step "scenario E: literal parent traversal path is rejected"
if ensure_real_directory_tree ".." "LaunchAgent directory"; then
    fail "literal parent traversal path unexpectedly succeeded"
fi

step "scenario F: glob metacharacters are treated literally"
glob_dir="$tmp/glob-[literal]-*"
ensure_real_directory_tree "$glob_dir" "LaunchAgent directory" || fail "literal glob path was rejected"
[ -d "$glob_dir" ] || fail "literal glob path was not created"

step "scenario G: LaunchAgent env repair does not depend on python plistlib"
home_g="$tmp/home-g"
storage_g="$tmp/storage from config"
plist_g="$home_g/Library/LaunchAgents/com.agent-mail.plist"
fake_bin="$tmp/fake-bin"
python_marker="$tmp/python-was-invoked"
launchctl_log="$tmp/launchctl.log"
mkdir -p "$home_g/Library/LaunchAgents" "$home_g/.config/mcp-agent-mail" "$fake_bin"
printf 'stale plist\n' >"$plist_g"
cat >"$home_g/.config/mcp-agent-mail/config.env" <<EOF
STORAGE_ROOT="$storage_g"
DATABASE_URL="sqlite:///$storage_g/storage.sqlite3"
HTTP_BEARER_TOKEN="repair&token"
HTTP_HOST=127.0.0.1
HTTP_PORT=9876
HTTP_PATH="/mcp/repair"
EOF
cat >"$fake_bin/python3" <<EOF
#!/usr/bin/env bash
printf 'python3 should not be invoked\n' >"$python_marker"
exit 70
EOF
cat >"$fake_bin/launchctl" <<EOF
#!/usr/bin/env bash
printf '%s\n' "\$*" >>"$launchctl_log"
exit 0
EOF
chmod 755 "$fake_bin/python3" "$fake_bin/launchctl"
(
    export OS=darwin
    export HOME="$home_g"
    export DEST="/opt/agent mail/bin"
    export BIN_CLI="am"
    export PATH="$fake_bin:$PATH"
    unset RUST_STORAGE_ROOT
    repair_launchd_service_env_from_rust_config
)
[ ! -e "$python_marker" ] || fail "repair path invoked python3"
grep -Fq '<string>/opt/agent mail/bin/am</string>' "$plist_g" || fail "repair path did not write am ProgramArguments"
grep -Fq "<string>sqlite:///$storage_g/storage.sqlite3</string>" "$plist_g" || fail "repair path did not use DATABASE_URL from config.env"
grep -Fq '<string>repair&amp;token</string>' "$plist_g" || fail "repair path did not XML-escape HTTP_BEARER_TOKEN"
grep -Fq '<string>/mcp/repair</string>' "$plist_g" || fail "repair path did not use HTTP_PATH from config.env"
grep -q "^bootout " "$launchctl_log" || fail "repair path did not restart existing launchd service"
grep -q "^bootstrap " "$launchctl_log" || fail "repair path did not bootstrap launchd service"

step "ALL SCENARIOS PASSED"
