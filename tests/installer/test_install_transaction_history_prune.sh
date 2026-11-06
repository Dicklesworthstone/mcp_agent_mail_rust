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
    echo 'err() { printf "ERROR %s\n" "$*" >&2; }'
    for fn in installer_path_owner_uid installer_path_mode ensure_real_directory_tree \
        installer_directory_identity installer_lock_fd_identity \
        installer_kernel_lock verify_installer_lock_authorities \
        release_installer_lock acquire_installer_lock \
        binary_transaction_history_in_use prune_binary_transaction_history; do
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

# Exercise the actual kernel lock and production pruning functions. The
# mid-prune case changes the pathname after a real liveness check, so an entry
# check alone cannot protect a later deletion. Preserve both directory trees
# and compare their contents with independent copies of the expected evidence.
test_locked_history_prune() (
    trap - EXIT
    local scenario="$1" case_dir="$tmp/locked-$1"
    local locked_dest="$case_dir/dest" retained="$case_dir/retained"
    local replacement="$case_dir/replacement" original_after="$locked_dest"
    local original_expected="$case_dir/original-expected"
    local replacement_expected="$case_dir/replacement-expected"
    local path="" nonce="" entry="" rc=0 contender_rc=0 keep=1
    local LOCKED=0 DESTINATION_LOCKED=0
    local BINARY_TRANSACTION_LAST_ARCHIVE_PATH=""

    mkdir -p "$locked_dest" "$replacement" "$original_expected" \
        || fail "$scenario: cannot prepare directories"
    for path in "$locked_dest" "$replacement"; do
        for nonce in 501 502 503; do
            entry=$(make_entry "$path" committed "$nonce" "2026050${nonce#50}0000") \
                || fail "$scenario: cannot seed history"
            printf '%s:%s:cli\n' "${path##*/}" "$nonce" >"$entry/old-cli" \
                || fail "$scenario: cannot write CLI evidence"
            printf '%s:%s:server\n' "${path##*/}" "$nonce" >"$entry/old-server" \
                || fail "$scenario: cannot write server evidence"
        done
    done
    cp -R "$replacement" "$replacement_expected" \
        || fail "$scenario: cannot preserve replacement evidence"
    for nonce in 501 502 503; do
        case "$scenario:$nonce" in unchanged:501|unchanged:502|during-prune:502) continue ;; esac
        cp -R "$locked_dest/$prefix.committed.$nonce" "$original_expected/" \
            || fail "$scenario: cannot preserve original evidence"
    done

    acquire_installer_lock "$locked_dest" destination \
        || fail "$scenario: cannot acquire the actual destination lock"
    mkdir -m 700 "$original_expected/${DESTINATION_LOCK_DIR##*/}" \
        || fail "$scenario: cannot preserve private lock directory expectation"
    # A separately opened descriptor must be refused by the kernel. Closing
    # the child's inherited descriptor does not release the parent's owner.
    (exec 8<&-; exec 8<"$DESTINATION_LOCK_DIR" || exit 91; installer_kernel_lock 8 acquire) \
        || contender_rc=$?
    [ "$contender_rc" -eq 1 ] || fail "$scenario: independent contender was not excluded"

    case "$scenario" in
        directory|symlink|no-deletion)
            # Even when retention would remove nothing, a foreign destination
            # must not be reported as successfully handled under the old lock.
            if [ "$scenario" = no-deletion ]; then keep=3; fi
            mv "$locked_dest" "$retained" || fail "$scenario: cannot retain locked directory"
            original_after="$retained"
            if [ "$scenario" = symlink ]; then
                ln -s "$replacement" "$locked_dest" || fail "$scenario: cannot replace destination"
            else
                mv "$replacement" "$locked_dest" || fail "$scenario: cannot replace destination"
            fi
            ;;
        during-prune)
            original_after="$retained"
            binary_transaction_history_in_use() {
                local in_use_rc=0
                # Reload the unchanged production function in a subshell;
                # only the real directory rename below is injected.
                (source "$extract"; binary_transaction_history_in_use "$1") || in_use_rc=$?
                if [ "$1" = "$locked_dest/$prefix.committed.501" ]; then
                    [ "$in_use_rc" -eq 1 ] || fail "mid-prune entry is unexpectedly in use"
                    [ ! -e "$locked_dest/$prefix.committed.502" ] \
                        || fail "mid-prune swap did not follow an actual earlier deletion"
                    mv "$locked_dest" "$retained" || fail "cannot retain locked directory mid-prune"
                    mv "$replacement" "$locked_dest" || fail "cannot replace destination mid-prune"
                    printf 'swapped\n' >"$case_dir/swapped" || fail "cannot record actual swap"
                fi
                return "$in_use_rc"
            }
            ;;
        unchanged) ;;
        *) fail "unknown lock-authority scenario: $scenario" ;;
    esac

    AM_INSTALL_TRANSACTION_HISTORY_KEEP="$keep" prune_binary_transaction_history "$locked_dest" \
        >"$case_dir/prune.stdout" 2>"$case_dir/prune.stderr" || rc=$?
    if [ "$scenario" = during-prune ]; then
        [ -f "$case_dir/swapped" ] || fail "$scenario: the actual directory swap was not reached"
    fi
    diff -r "$original_expected" "$original_after" \
        || fail "$scenario: evidence in the held directory changed unexpectedly"
    if [ "$scenario" = unchanged ]; then
        [ "$rc" -eq 0 ] || fail "unchanged destination refused valid history pruning"
        diff -r "$replacement_expected" "$replacement" \
            || fail "valid pruning changed an unrelated directory"
    else
        diff -r "$replacement_expected" "$locked_dest" \
            || fail "$scenario: pruning modified replacement evidence"
        if [ "$scenario" = symlink ]; then
            [ -L "$locked_dest" ] && [ "$(readlink "$locked_dest")" = "$replacement" ] \
                || fail "$scenario: pruning changed the replacement symlink"
        fi
        [ "$rc" -ne 0 ] || fail "$scenario: changed destination was not refused"
        grep -Fq 'Install destination changed after lock acquisition' "$case_dir/prune.stderr" \
            || fail "$scenario: refusal did not identify lost destination authority"
        # The replacement is not the inode protected by our held descriptor.
        (exec 8<&-; exec 8<"$locked_dest" || exit 91; installer_kernel_lock 8 acquire) \
            || fail "$scenario: replacement unexpectedly has the original lock"
    fi
    contender_rc=0
    (exec 8<&-; exec 8<"$original_after/${DESTINATION_LOCK_DIR##*/}" || exit 91; installer_kernel_lock 8 acquire) \
        || contender_rc=$?
    [ "$contender_rc" -eq 1 ] || fail "$scenario: pruning released the held authority"
    release_installer_lock destination || fail "$scenario: cannot release the owned descriptor"
    (exec 8<"$original_after/${DESTINATION_LOCK_DIR##*/}" || exit 91; installer_kernel_lock 8 acquire) \
        || fail "$scenario: original directory stayed locked after final release"
)

authority_failures=0
for scenario in unchanged directory symlink no-deletion during-prune; do
    if test_locked_history_prune "$scenario"; then
        echo "PASS: locked history pruning ($scenario)"
    else
        authority_failures=$((authority_failures + 1))
    fi
done
[ "$authority_failures" -eq 0 ] || fail "$authority_failures destination authority regression(s)"

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
cp "$BASH" "$in_use/old-cli"
chmod 755 "$in_use/old-cli"
# A renamed multicall utility may exit before the liveness check. A real Bash
# executable waiting in its read builtin remains mapped at this exact copy,
# with no child process to orphan when the holder is stopped.
mkfifo "$tmp/holder.fifo"
"$in_use/old-cli" -c 'IFS= read -r -t 300 <&3' 3<>"$tmp/holder.fifo" &
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
