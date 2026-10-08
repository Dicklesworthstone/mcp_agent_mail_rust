#!/usr/bin/env bash
#
# br-gq0gx: install.sh archived every binary transaction (each keeping the
# previous ~200 MB binary pair) and never pruned them. After a successful
# install it now keeps the newest few entries and prunes older ones, except
# an entry a running process executes from.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
INSTALL_SH="$REPO_ROOT/install.sh"

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

tmp="$(mktemp -d)"
holder_pid=""
cleanup() {
    if [ -n "$holder_pid" ]; then
        kill "$holder_pid" 2>/dev/null || true
        wait "$holder_pid" 2>/dev/null || true
    fi
    if [ "${AM_E2E_KEEP_TMP:-0}" != "1" ]; then
        rm -rf -- "$tmp"
    fi
}
trap cleanup EXIT

extract="$tmp/prune_functions.sh"
{
    echo 'info() { printf "INFO %s\n" "$*" >&2; }'
    echo 'warn() { printf "WARN %s\n" "$*" >&2; }'
    for fn in binary_transaction_history_in_use prune_binary_transaction_history; do
        body=$(sed -n "/^${fn}() {/,/^}/p" "$INSTALL_SH")
        [ -n "$body" ] || fail "install.sh no longer defines $fn"
        printf '%s\n' "$body"
    done
} >"$extract"
# shellcheck source=/dev/null
source "$extract"

prefix=".mcp-agent-mail-install-transaction"

# make_entry DIR OUTCOME NONCE STAMP: an archived transaction with an
# old binary pair, dated STAMP ([[CC]YY]MMDDhhmm).
make_entry() {
    local entry="$1/$prefix.$2.$3"
    mkdir -p "$entry"
    printf 'old cli\n' >"$entry/old-cli"
    printf 'old server\n' >"$entry/old-server"
    touch -t "$4" "$entry"
    printf '%s\n' "$entry"
}

remaining() {
    find "$1" -maxdepth 1 -name "$prefix.*" | sort
}

# --- Keep the newest two of six, plus never touch foreign entries. ---
dest="$tmp/dest"
mkdir -p "$dest"
e1=$(make_entry "$dest" committed 101 202601010000)
e2=$(make_entry "$dest" rolled-back 102 202601020000)
e3=$(make_entry "$dest" committed 103 202601030000)
e4=$(make_entry "$dest" committed 104 202601040000)
e5=$(make_entry "$dest" rolled-back 105 202601050000)
e6=$(make_entry "$dest" committed 106 202601060000)
mkdir -p "$dest/$prefix.active"
mkdir -p "$tmp/elsewhere"
ln -s "$tmp/elsewhere" "$dest/$prefix.committed.symlinked"
touch -h -t 202501010000 "$dest/$prefix.committed.symlinked" 2>/dev/null || true
printf 'not ours\n' >"$dest/$prefix.committed.regular-file"
touch -t 202501010000 "$dest/$prefix.committed.regular-file"

BINARY_TRANSACTION_LAST_ARCHIVE_PATH="$e6"
prune_binary_transaction_history "$dest"

for kept in "$e5" "$e6"; do
    [ -d "$kept" ] || fail "newest entry $kept was pruned"
done
for pruned in "$e1" "$e2" "$e3" "$e4"; do
    [ ! -e "$pruned" ] || fail "old entry $pruned was not pruned"
done
[ -d "$dest/$prefix.active" ] || fail "the active journal must never be touched"
[ -L "$dest/$prefix.committed.symlinked" ] || fail "a symlinked entry must be skipped"
[ -d "$tmp/elsewhere" ] || fail "a symlink target must never be touched"
[ -f "$dest/$prefix.committed.regular-file" ] || fail "a non-directory entry must be skipped"

# --- A second run is a no-op. ---
before=$(remaining "$dest")
prune_binary_transaction_history "$dest"
[ "$before" = "$(remaining "$dest")" ] || fail "pruning is not idempotent"

# --- The entry this run archived survives even when it is not the newest. ---
dest2="$tmp/dest2"
mkdir -p "$dest2"
older_archived=$(make_entry "$dest2" committed 201 202602010000)
make_entry "$dest2" committed 202 202602020000 >/dev/null
make_entry "$dest2" committed 203 202602030000 >/dev/null
BINARY_TRANSACTION_LAST_ARCHIVE_PATH="$older_archived"
AM_INSTALL_TRANSACTION_HISTORY_KEEP=1 prune_binary_transaction_history "$dest2"
[ -d "$older_archived" ] || fail "the entry this run archived was pruned"
[ "$(remaining "$dest2" | wc -l | tr -d '[:space:]')" -eq 2 ] \
    || fail "keep=1 should leave the newest plus this run's entry"

# --- A non-numeric or zero keep falls back safely (never prunes everything). ---
dest3="$tmp/dest3"
mkdir -p "$dest3"
make_entry "$dest3" committed 301 202603010000 >/dev/null
newest3=$(make_entry "$dest3" committed 302 202603020000)
BINARY_TRANSACTION_LAST_ARCHIVE_PATH=""
AM_INSTALL_TRANSACTION_HISTORY_KEEP=0 prune_binary_transaction_history "$dest3"
[ -d "$newest3" ] || fail "keep=0 must still keep the newest entry"
AM_INSTALL_TRANSACTION_HISTORY_KEEP=abc prune_binary_transaction_history "$dest3"
[ -d "$newest3" ] || fail "a malformed keep must fall back to the default"

# --- An entry a live process executes from is never pruned. ---
dest4="$tmp/dest4"
mkdir -p "$dest4"
in_use=$(make_entry "$dest4" committed 401 202604010000)
make_entry "$dest4" committed 402 202604020000 >/dev/null
newest4=$(make_entry "$dest4" committed 403 202604030000)
sleep_bin=$(command -v sleep)
cp "$sleep_bin" "$in_use/old-cli"
chmod 755 "$in_use/old-cli"
"$in_use/old-cli" 300 &
holder_pid=$!
# Give the exec a moment to land so /proc/<pid>/exe names the copy.
for _ in 1 2 3 4 5 6 7 8 9 10; do
    if binary_transaction_history_in_use "$in_use"; then
        break
    fi
    sleep 0.2
done
binary_transaction_history_in_use "$in_use" || fail "a running binary must mark its entry in use"
BINARY_TRANSACTION_LAST_ARCHIVE_PATH="$newest4"
AM_INSTALL_TRANSACTION_HISTORY_KEEP=1 prune_binary_transaction_history "$dest4"
[ -d "$in_use" ] || fail "an entry a running process executes from was pruned"
[ "$(remaining "$dest4" | wc -l | tr -d '[:space:]')" -eq 2 ] \
    || fail "the unused old entry should have been pruned"

kill "$holder_pid"
wait "$holder_pid" 2>/dev/null || true
holder_pid=""
AM_INSTALL_TRANSACTION_HISTORY_KEEP=1 prune_binary_transaction_history "$dest4"
[ ! -e "$in_use" ] || fail "an entry no longer in use should be pruned"
[ -d "$newest4" ] || fail "the newest entry must survive"

echo "PASS: install transaction history pruning"
