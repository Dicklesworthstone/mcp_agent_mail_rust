#!/usr/bin/env bash
# test_alias_detection.sh - E2E coverage for install.sh alias detection/displacement
#
# Validates detect_python_alias() and displace_python_alias() behavior against
# common shell-rc patterns and edge-case filesystems/content.

E2E_SUITE="alias_detection"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Installer Alias Detection/Displacement E2E Suite"

WORK="$(e2e_mktemp "e2e_alias_detection")"
INSTALL_SH="${SCRIPT_DIR}/../../install.sh"
DEST="${WORK}/bin"
mkdir -p "$DEST"

# Quiet stubs expected by install.sh helper functions under test.
info() { :; }
ok() { :; }
warn() { :; }

extract_alias_helpers() {
    local out="$1"
    awk '
        /^# Result variables set by detect_python_\*/ { capture=1 }
        capture { print }
        /^# T1\.5: Stop running Python server processes/ { exit }
    ' "$INSTALL_SH" > "$out"
}

ALIAS_HELPERS="${WORK}/install_alias_helpers.sh"
extract_alias_helpers "$ALIAS_HELPERS"
if [ ! -s "$ALIAS_HELPERS" ]; then
    e2e_fail "failed to extract alias helper functions from install.sh"
    e2e_summary
    exit 1
fi

# shellcheck source=/dev/null
source "$ALIAS_HELPERS"

new_home() {
    local name="$1"
    local home_dir="${WORK}/${name}"
    mkdir -p "${home_dir}/.config/fish"
    echo "$home_dir"
}

file_mode() {
    local path="$1"
    stat -c '%a' "$path" 2>/dev/null || stat -f '%Lp' "$path" 2>/dev/null || echo ""
}

latest_backup_for() {
    local rc="$1"
    local dir base
    dir="$(dirname "$rc")"
    base="$(basename "$rc")"
    find "$dir" -maxdepth 1 -type f -name "${base}.bak.mcp-agent-mail-*" -print | sort | tail -n 1
}

assert_detected() {
    local label="$1"
    local expected_file="$2"
    local expected_kind="$3"
    local expected_markers="$4"

    detect_python_alias
    e2e_assert_eq "${label}: found" "1" "${PYTHON_ALIAS_FOUND}"
    e2e_assert_eq "${label}: file" "$expected_file" "${PYTHON_ALIAS_FILE}"
    e2e_assert_eq "${label}: kind" "$expected_kind" "${PYTHON_ALIAS_KIND}"
    e2e_assert_eq "${label}: marker flag" "$expected_markers" "${PYTHON_ALIAS_HAS_MARKERS}"
}

assert_not_detected() {
    local label="$1"
    detect_python_alias
    e2e_assert_eq "${label}: found" "0" "${PYTHON_ALIAS_FOUND}"
    e2e_assert_eq "${label}: file empty" "" "${PYTHON_ALIAS_FILE}"
}

# ===========================================================================
# Detection cases
# ===========================================================================
e2e_case_banner "1) Marker block present -> detected"
HOME="$(new_home case01)"
cat > "${HOME}/.zshrc" <<'EOF'
# >>> MCP Agent Mail alias
alias am='cd "/tmp/py-clone" && python -m mcp_agent_mail'
# <<< MCP Agent Mail alias
EOF
assert_detected "marker block detection" "${HOME}/.zshrc" "alias" "1"
e2e_assert_contains "marker content captured" "${PYTHON_ALIAS_CONTENT}" "alias am="

e2e_case_banner "2) Bare alias am= without markers -> detected"
HOME="$(new_home case02)"
printf "alias am='python -m mcp_agent_mail'\n" > "${HOME}/.zshrc"
assert_detected "bare alias detection" "${HOME}/.zshrc" "alias" "0"
e2e_assert_eq "bare alias line number" "1" "${PYTHON_ALIAS_LINE}"

e2e_case_banner "3) No alias at all -> not detected"
HOME="$(new_home case03)"
printf "export PATH=\"\$HOME/.local/bin:\$PATH\"\n" > "${HOME}/.zshrc"
assert_not_detected "no alias"

e2e_case_banner "4) Commented-out alias -> not detected"
HOME="$(new_home case04)"
printf "# alias am='python -m mcp_agent_mail'\n" > "${HOME}/.zshrc"
assert_not_detected "commented alias"

e2e_case_banner "5) Multiple aliases in one file -> all discovered over repeated passes"
HOME="$(new_home case05)"
cat > "${HOME}/.zshrc" <<'EOF'
alias am='python -m mcp_agent_mail first'
alias am='python -m mcp_agent_mail second'
EOF
assert_detected "first alias in multi-alias file" "${HOME}/.zshrc" "alias" "0"
e2e_assert_contains "first alias content selected" "${PYTHON_ALIAS_CONTENT}" "first"
displace_python_alias
assert_detected "second alias still discoverable after first displaced" "${HOME}/.zshrc" "alias" "0"
e2e_assert_contains "second alias content selected" "${PYTHON_ALIAS_CONTENT}" "second"

e2e_case_banner "6) Alias in .zshrc but not .bashrc -> .zshrc reported"
HOME="$(new_home case06)"
printf "export ZDOTDIR=\"%s\"\n" "$HOME" > "${HOME}/.bashrc"
printf "alias am='python -m zsh_only'\n" > "${HOME}/.zshrc"
assert_detected "zshrc precedence" "${HOME}/.zshrc" "alias" "0"

e2e_case_banner "7) Alias in both .zshrc and .bashrc -> both reported across passes"
HOME="$(new_home case07)"
printf "alias am='python -m zsh_first'\n" > "${HOME}/.zshrc"
printf "alias am='python -m bash_second'\n" > "${HOME}/.bashrc"
assert_detected "both files pass 1 (zsh preferred)" "${HOME}/.zshrc" "alias" "0"
displace_python_alias
assert_detected "both files pass 2 (bash discovered)" "${HOME}/.bashrc" "alias" "0"
e2e_assert_contains "bash alias content surfaced" "${PYTHON_ALIAS_CONTENT}" "bash_second"

e2e_case_banner "8) Function definition am() -> detected"
HOME="$(new_home case08)"
cat > "${HOME}/.zshrc" <<'EOF'
am() {
  python -m mcp_agent_mail "$@"
}
EOF
assert_detected "function detection" "${HOME}/.zshrc" "function" "0"

e2e_case_banner "9) Alias quoting styles -> all detected"
HOME="$(new_home case09)"
styles=(
    "alias am='cd \"/tmp/single\" && python -m foo'"
    "alias am=\"cd '/tmp/double' && python -m bar\""
    "alias am=python\\ -m\\ baz"
)
idx=1
for line in "${styles[@]}"; do
    printf "%s\n" "$line" > "${HOME}/.zshrc"
    detect_python_alias
    e2e_assert_eq "quote style ${idx}: found" "1" "${PYTHON_ALIAS_FOUND}"
    idx=$((idx + 1))
done

e2e_case_banner "10) File with no trailing newline -> handled"
HOME="$(new_home case10)"
printf "alias am='python -m mcp_agent_mail no_newline'" > "${HOME}/.zshrc"
assert_detected "no newline detection" "${HOME}/.zshrc" "alias" "0"

# ===========================================================================
# Displacement cases
# ===========================================================================
e2e_case_banner "11) Displace marker block -> block commented out"
HOME="$(new_home case11)"
cat > "${HOME}/.zshrc" <<'EOF'
# >>> MCP Agent Mail alias
alias am='cd "/tmp/py-clone" && python -m mcp_agent_mail'
# <<< MCP Agent Mail alias
EOF
assert_detected "marker block pre-displace" "${HOME}/.zshrc" "alias" "1"
displace_python_alias
post_marker_content="$(cat "${HOME}/.zshrc")"
e2e_assert_contains "marker header rewritten as disabled" "$post_marker_content" "# >>> MCP Agent Mail alias (DISABLED"
e2e_assert_contains "alias line commented in marker block" "$post_marker_content" "# alias am='cd \"/tmp/py-clone\" && python -m mcp_agent_mail'"
e2e_assert_contains "restore hint appended" "$post_marker_content" "To restore Python version"
assert_not_detected "marker block no longer active"

e2e_case_banner "12) Displace bare alias -> line commented out"
HOME="$(new_home case12)"
printf "alias am='python -m bare_alias_case'\n" > "${HOME}/.zshrc"
original_case12="$(cat "${HOME}/.zshrc")"
assert_detected "bare alias pre-displace" "${HOME}/.zshrc" "alias" "0"
displace_python_alias
post_bare_content="$(cat "${HOME}/.zshrc")"
e2e_assert_contains "bare alias disabled comment present" "$post_bare_content" "# Disabled by mcp-agent-mail Rust installer: alias am='python -m bare_alias_case'"
e2e_assert_contains "rust binary path hint present" "$post_bare_content" "# Rust binary at: ${DEST}/am"
assert_not_detected "bare alias no longer active"

e2e_case_banner "13) Backup created with original content"
backup_case12="$(latest_backup_for "${HOME}/.zshrc")"
e2e_assert_file_exists "backup file exists" "$backup_case12"
backup_case12_content="$(cat "$backup_case12")"
e2e_assert_eq "backup content matches original" "$original_case12" "$backup_case12_content"

e2e_case_banner "14) Displacement idempotent when rerun"
before_second_run="$(cat "${HOME}/.zshrc")"
displace_python_alias
after_second_run="$(cat "${HOME}/.zshrc")"
e2e_assert_eq "second displacement leaves file unchanged" "$before_second_run" "$after_second_run"

e2e_case_banner "15) RC file permissions preserved after modification"
HOME="$(new_home case15)"
printf "alias am='python -m perms_case'\n" > "${HOME}/.zshrc"
chmod 600 "${HOME}/.zshrc"
mode_before="$(file_mode "${HOME}/.zshrc")"
assert_detected "perms pre-displace" "${HOME}/.zshrc" "alias" "0"
displace_python_alias
mode_after="$(file_mode "${HOME}/.zshrc")"
e2e_assert_eq "file mode preserved" "$mode_before" "$mode_after"

# ===========================================================================
# Edge-case cases from review
# ===========================================================================
e2e_case_banner "16) CRLF line endings handled"
HOME="$(new_home case16)"
printf "alias am='python -m crlf_case'\r\n" > "${HOME}/.zshrc"
assert_detected "crlf detection" "${HOME}/.zshrc" "alias" "0"
displace_python_alias
e2e_assert_file_exists "crlf file remains after displacement" "${HOME}/.zshrc"

e2e_case_banner "17) RC file as symlink handled"
HOME="$(new_home case17)"
target_rc="${HOME}/real_zshrc"
printf "alias am='python -m symlink_case'\n" > "$target_rc"
ln -s "$target_rc" "${HOME}/.zshrc"
assert_detected "symlink detection" "${HOME}/.zshrc" "alias" "0"
displace_python_alias
symlink_path_content="$(cat "${HOME}/.zshrc")"
e2e_assert_contains "symlink path now contains displaced alias content" "$symlink_path_content" "Disabled by mcp-agent-mail Rust installer"
if [ ! -L "${HOME}/.zshrc" ]; then
    e2e_pass "symlink path converted to regular file during displacement"
else
    e2e_fail "expected symlink path to be rewritten as regular file"
fi

e2e_case_banner "18) Read-only RC file handled"
HOME="$(new_home case18)"
printf "alias am='python -m readonly_case'\n" > "${HOME}/.zshrc"
chmod 444 "${HOME}/.zshrc"
readonly_mode_before="$(file_mode "${HOME}/.zshrc")"
assert_detected "readonly detection" "${HOME}/.zshrc" "alias" "0"
displace_python_alias
readonly_mode_after="$(file_mode "${HOME}/.zshrc")"
e2e_assert_eq "readonly mode preserved" "$readonly_mode_before" "$readonly_mode_after"

e2e_case_banner "19) Very large RC file (1000+ lines) handled"
HOME="$(new_home case19)"
for i in $(seq 1 1200); do
    printf "# filler line %s\n" "$i" >> "${HOME}/.zshrc"
done
printf "alias am='python -m large_case'\n" >> "${HOME}/.zshrc"
assert_detected "large file detection" "${HOME}/.zshrc" "alias" "0"
if [ "${PYTHON_ALIAS_LINE}" -gt 1000 ]; then
    e2e_pass "large file reports alias line > 1000"
else
    e2e_fail "large file alias line should be > 1000 (actual: ${PYTHON_ALIAS_LINE})"
fi

e2e_case_banner "20) Unicode path in HOME handled"
HOME="${WORK}/home_ユニコード_测试"
mkdir -p "${HOME}/.config/fish"
printf "alias am='python -m unicode_case'\n" > "${HOME}/.zshrc"
assert_detected "unicode home path detection" "${HOME}/.zshrc" "alias" "0"
e2e_assert_contains "unicode path retained in reported file" "${PYTHON_ALIAS_FILE}" "ユニコード"

e2e_save_artifact "final_rc_example.txt" "$(cat "${HOME}/.zshrc")"
e2e_summary
