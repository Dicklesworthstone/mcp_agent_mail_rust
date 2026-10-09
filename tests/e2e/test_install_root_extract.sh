#!/usr/bin/env bash
# test_install_root_extract.sh — GH#339 regression harness for install.sh
#
# As root (containers, Dockerfile RUN, CI images), GNU tar defaults to
# --same-owner, so a plain `tar -xf` keeps the archive's recorded uid/gid and
# the installer's staged-file ownership check refuses every binary. Proves:
#   * extract_release_archive (the installer's extraction step) yields
#     root-owned staged files that pass validate_installer_owned_regular_file;
#   * planted negative: a plain `tar -xf` of the same archive keeps the foreign
#     owner and is refused (so this harness catches a regression to it);
#   * the ownership check itself still refuses a genuinely foreign staged file.
#
# Method: extract the functions from install.sh verbatim (as
# test_install_no_service.sh does) and drive them on an archive whose members
# are owned by uid 4242. Needs root: re-executes itself under `sudo -n` when
# available, otherwise reports SKIP (exit 0) rather than a false pass.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
INSTALL_SH="${REPO_ROOT}/install.sh"
[ -f "$INSTALL_SH" ] || { echo "FATAL: install.sh not found at ${INSTALL_SH}" >&2; exit 1; }

if [ "$(id -u)" != "0" ]; then
  if command -v sudo >/dev/null 2>&1 && sudo -n true 2>/dev/null; then
    exec sudo -n bash "$0" "$@"
  fi
  echo "SKIP: GH#339 harness needs root (run as root or with passwordless sudo)"
  exit 0
fi

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/am-root-extract-harness.XXXXXX")"
trap 'rm -rf "$WORKDIR"' EXIT

err() { printf 'ERR %s\n' "$*" >&2; }
# shellcheck disable=SC1090
source <(sed -n \
  -e '/^installer_path_owner_uid() {/,/^}/p' \
  -e '/^installer_path_mode() {/,/^}/p' \
  -e '/^installer_path_link_count() {/,/^}/p' \
  -e '/^extract_release_archive() {/,/^}/p' \
  -e '/^validate_installer_owned_regular_file() {/,/^}/p' \
  "$INSTALL_SH")
for fn in installer_path_owner_uid installer_path_link_count extract_release_archive \
          validate_installer_owned_regular_file; do
  declare -F "$fn" >/dev/null || { echo "FATAL: $fn not found in install.sh" >&2; exit 1; }
done

pass=0
fail=0
ok() { printf 'PASS %s\n' "$1"; pass=$((pass + 1)); }
bad() { printf 'FAIL %s\n' "$1"; fail=$((fail + 1)); }

# A release-shaped archive whose two members are owned by a foreign uid.
mkdir -p "$WORKDIR/src"
printf '#!/bin/sh\necho am 0.0.0\n' > "$WORKDIR/src/am"
printf '#!/bin/sh\necho mcp-agent-mail 0.0.0\n' > "$WORKDIR/src/mcp-agent-mail"
chmod 0755 "$WORKDIR/src/am" "$WORKDIR/src/mcp-agent-mail"
tar --owner=4242 --group=4242 -cf "$WORKDIR/release.tar" -C "$WORKDIR/src" am mcp-agent-mail

# 1. The installer's extraction step stages root-owned files that pass the check.
mkdir "$WORKDIR/fixed"
extract_release_archive "$WORKDIR/release.tar" "$WORKDIR/fixed"
owner=$(installer_path_owner_uid "$WORKDIR/fixed/am")
if [ "$owner" = "0" ] \
   && validate_installer_owned_regular_file "$WORKDIR/fixed/am" "Staged CLI" \
   && validate_installer_owned_regular_file "$WORKDIR/fixed/mcp-agent-mail" "Staged server"; then
  ok "extract_release_archive stages root-owned binaries that pass the ownership check"
else
  bad "extract_release_archive staged owner=$owner (want 0) or the check refused it"
fi

# 2. Planted negative: plain tar as root keeps the archive owner and is refused.
mkdir "$WORKDIR/plain"
tar -xf "$WORKDIR/release.tar" -C "$WORKDIR/plain"
owner=$(installer_path_owner_uid "$WORKDIR/plain/am")
if [ "$owner" = "4242" ] \
   && ! validate_installer_owned_regular_file "$WORKDIR/plain/am" "Staged CLI" 2>/dev/null; then
  ok "plain tar -xf as root keeps uid 4242 and the check refuses it (the GH#339 failure)"
else
  bad "plain tar -xf staged owner=$owner; expected 4242 and a refusal"
fi

# 3. The ownership check still refuses a genuinely foreign staged file.
cp "$WORKDIR/fixed/am" "$WORKDIR/foreign-am"
chown 4242:4242 "$WORKDIR/foreign-am"
if ! validate_installer_owned_regular_file "$WORKDIR/foreign-am" "Staged CLI" 2>/dev/null; then
  ok "a foreign-owned staged file is still refused"
else
  bad "a foreign-owned staged file was accepted"
fi

printf '\n%s passed, %s failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
