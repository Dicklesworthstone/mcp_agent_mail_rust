#!/usr/bin/env bash
#
# mcp-agent-mail installer
#
# One-liner install (with cache buster):
#   curl -fsSL "https://raw.githubusercontent.com/Dicklesworthstone/mcp_agent_mail_rust/main/install.sh?$(date +%s)" | bash
#
# Or without cache buster:
#   curl -fsSL https://raw.githubusercontent.com/Dicklesworthstone/mcp_agent_mail_rust/main/install.sh | bash
#
# Options:
#   --version vX.Y.Z   Install specific version (default: latest)
#   --dest DIR         Install to DIR (default: ~/.local/bin)
#   --system           Install to /usr/local/bin (requires sudo)
#   --easy-mode        Auto-update PATH in shell rc files
#   --no-easy          Do not auto-update PATH, even for piped installs
#   --verify           Run self-test after install
#   --from-source      Build from source instead of downloading binary
#   --quiet            Suppress non-error output
#   --verbose          Enable detailed installer diagnostics
#   --no-gum           Disable gum formatting even if available
#   --no-verify        Skip checksum + signature verification (for testing only)
#   --offline          Skip network preflight checks
#   --force            Force reinstall even if already at version
#   --migrate          Force Python->Rust migration/displacement when Python install detected
#   --no-migrate       Skip Python->Rust migration/displacement even when detected
#   --uninstall        Remove installed binaries/configuration helpers
#   --yes              Non-interactive mode (skip all confirmations)
#   --purge            With --uninstall, also delete data directories/database
#   --dry-run          Preview what the installer would do without making changes
#   --preview          Alias for --dry-run
#
set -Eeuo pipefail
umask 022
shopt -s lastpipe 2>/dev/null || true

VERSION="${VERSION:-}"
OWNER="${OWNER:-Dicklesworthstone}"
REPO="${REPO:-mcp_agent_mail_rust}"
ISSUES_URL="${ISSUES_URL:-https://github.com/${OWNER}/${REPO}/issues}"
INSTALL_SCRIPT_URL="${INSTALL_SCRIPT_URL:-https://raw.githubusercontent.com/${OWNER}/${REPO}/main/install.sh}"
DEST_DEFAULT="$HOME/.local/bin"
DEST="${DEST:-$DEST_DEFAULT}"
EASY=0
QUIET=0
VERBOSE=0
VERIFY=0
FROM_SOURCE=0
CHECKSUM="${CHECKSUM:-}"
CHECKSUM_URL="${CHECKSUM_URL:-}"
SIGSTORE_BUNDLE_URL="${SIGSTORE_BUNDLE_URL:-}"
COSIGN_IDENTITY_RE="${COSIGN_IDENTITY_RE:-^https://github.com/${OWNER}/${REPO}/.github/workflows/dist.yml@refs/tags/.*$}"
COSIGN_OIDC_ISSUER="${COSIGN_OIDC_ISSUER:-https://token.actions.githubusercontent.com}"
ARTIFACT_URL="${ARTIFACT_URL:-}"
LOCK_FILE="/tmp/mcp-agent-mail-install.lock"
SYSTEM=0
NO_GUM=0
NO_CHECKSUM=0
FORCE_INSTALL=0
FORCE_MIGRATE=0
FORCE_NO_MIGRATE=0
UNINSTALL=0
ASSUME_YES=0
PURGE=0
DRY_RUN=0
OFFLINE="${AM_OFFLINE:-0}"
VERBOSE_DUMP_LINES=20
LOG_FILE="${LOG_FILE:-/tmp/am-install-$(date -u +%Y%m%dT%H%M%SZ)-$$.log}"
LOG_INITIALIZED=0
ERROR_TAIL_EMITTED=0
ORIGINAL_ARGS=("$@")
UNINSTALL_SUMMARY=()
REMOTE_HTTP_PROBE_DETAIL=""

# T2.1: Auto-enable easy-mode for pipe installs (stdin is not a terminal)
# Also auto-enable in CI environments.
if [ ! -t 0 ] || [ "${CI:-}" = "true" ] || [ -n "${GITHUB_ACTIONS:-}" ] || [ -n "${GITLAB_CI:-}" ] || [ -n "${JENKINS_URL:-}" ]; then
  EASY=1
fi

# Binary names in this project
BIN_SERVER="mcp-agent-mail"
BIN_CLI="am"

# Detect gum for fancy output (https://github.com/charmbracelet/gum)
HAS_GUM=0
if command -v gum &> /dev/null && [ -t 1 ]; then
  HAS_GUM=1
fi

# Logging functions with optional gum formatting
log() { [ "$QUIET" -eq 1 ] && return 0; echo -e "$@"; }

info() {
  [ "$QUIET" -eq 1 ] && return 0
  if [ "$HAS_GUM" -eq 1 ] && [ "$NO_GUM" -eq 0 ]; then
    gum style --foreground 39 -- "-> $*"
  else
    echo -e "\033[0;34m->\033[0m $*"
  fi
}

ok() {
  [ "$QUIET" -eq 1 ] && return 0
  if [ "$HAS_GUM" -eq 1 ] && [ "$NO_GUM" -eq 0 ]; then
    gum style --foreground 42 "ok $*"
  else
    echo -e "\033[0;32mok\033[0m $*"
  fi
}

warn() {
  [ "$QUIET" -eq 1 ] && return 0
  if [ "$HAS_GUM" -eq 1 ] && [ "$NO_GUM" -eq 0 ]; then
    gum style --foreground 214 "!! $*"
  else
    echo -e "\033[1;33m!!\033[0m $*"
  fi
}

err() {
  if [ "$HAS_GUM" -eq 1 ] && [ "$NO_GUM" -eq 0 ]; then
    gum style --foreground 196 "ERR $*"
  else
    echo -e "\033[0;31mERR\033[0m $*"
  fi
}

error_usage_hint() {
  err "Run './install.sh --help' for full option details."
  err "Example: ./install.sh --version vX.Y.Z --dest \"\$HOME/.local/bin\""
}

error_support_hint() {
  err "Try re-running with --verbose for detailed diagnostics."
  err "Inspect the log with: tail -n ${VERBOSE_DUMP_LINES} \"${LOG_FILE}\""
  err "If this persists, report at ${ISSUES_URL} and include log: ${LOG_FILE}"
}

init_verbose_log() {
  [ "$LOG_INITIALIZED" -eq 1 ] && return 0
  local log_dir
  log_dir=$(dirname "$LOG_FILE")
  mkdir -p "$log_dir" 2>/dev/null || true
  if ! : > "$LOG_FILE" 2>/dev/null; then
    LOG_FILE="/tmp/am-install-$(date -u +%Y%m%dT%H%M%SZ)-$$.log"
    : > "$LOG_FILE" 2>/dev/null || return 0
  fi
  LOG_INITIALIZED=1
  printf '%s [VERBOSE] initialized pid=%s shell=%s\n' \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    "$$" \
    "${SHELL:-unknown}" >> "$LOG_FILE" || true
}

verbose() {
  init_verbose_log
  local ts msg
  ts=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  msg="$*"
  if [ "$LOG_INITIALIZED" -eq 1 ]; then
    printf '%s [VERBOSE] %s\n' "$ts" "$msg" >> "$LOG_FILE" || true
  fi
  if [ "$VERBOSE" -eq 1 ] && [ "$QUIET" -eq 0 ]; then
    echo "[VERBOSE] $msg"
  fi
}

dump_verbose_tail() {
  [ "$ERROR_TAIL_EMITTED" -eq 1 ] && return 0
  ERROR_TAIL_EMITTED=1
  [ "$LOG_INITIALIZED" -eq 1 ] || return 0
  [ -f "$LOG_FILE" ] || return 0
  err "Verbose log: $LOG_FILE"
  if [ "$VERBOSE" -eq 0 ]; then
    err "Last ${VERBOSE_DUMP_LINES} verbose log lines:"
    tail -n "$VERBOSE_DUMP_LINES" "$LOG_FILE" >&2 || true
  fi
}

on_error() {
  local exit_code=$?
  local line_no="${1:-unknown}"
  trap - ERR
  if [ "$exit_code" -ne 0 ]; then
    err "Installer failed (exit ${exit_code}) at line ${line_no}"
    err "Unexpected installer error."
    error_support_hint
    dump_verbose_tail
  fi
  exit "$exit_code"
}

early_exit_dump() {
  local rc=$?
  if [ "$rc" -ne 0 ]; then
    dump_verbose_tail
  fi
}

download_to_file() {
  local url="$1"
  local out="$2"
  local label="${3:-download}"
  local start_ts end_ts duration_s size_bytes rc=0
  start_ts=$(date +%s)
  verbose "${label}:start url=${url} out=${out}"
  if [ "$VERBOSE" -eq 1 ] && [ "$QUIET" -eq 0 ]; then
    curl -fL --progress-bar "$url" -o "$out" || rc=$?
  else
    curl -fsSL "$url" -o "$out" 2>/dev/null || rc=$?
  fi
  end_ts=$(date +%s)
  duration_s=$((end_ts - start_ts))
  if [ "$rc" -ne 0 ]; then
    # curl may leave behind an empty/partial file on failure — clean it up
    rm -f "$out" 2>/dev/null || true
    verbose "${label}:failed rc=${rc} duration_s=${duration_s}"
    return "$rc"
  fi
  size_bytes=$(wc -c < "$out" 2>/dev/null || echo 0)
  verbose "${label}:done bytes=${size_bytes} duration_s=${duration_s} out=${out}"
}

# Spinner wrapper for long operations
run_with_spinner() {
  local title="$1"
  shift
  if [ "$HAS_GUM" -eq 1 ] && [ "$NO_GUM" -eq 0 ] && [ "$QUIET" -eq 0 ]; then
    gum spin --spinner dot --title "$title" -- "$@"
  else
    info "$title"
    "$@"
  fi
}

# Draw a box around text with automatic width calculation
draw_box() {
  local color="$1"
  shift
  local lines=("$@")
  local max_width=0
  local esc
  esc=$(printf '\033')
  local strip_ansi_sed="s/${esc}\\[[0-9;]*m//g"

  for line in "${lines[@]}"; do
    local stripped
    stripped=$(printf '%b' "$line" | LC_ALL=C sed "$strip_ansi_sed")
    local len=${#stripped}
    if [ "$len" -gt "$max_width" ]; then
      max_width=$len
    fi
  done

  local inner_width=$((max_width + 4))
  local border=""
  for ((i=0; i<inner_width; i++)); do
    border+="="
  done

  printf "\033[%sm+%s+\033[0m\n" "$color" "$border"

  for line in "${lines[@]}"; do
    local stripped
    stripped=$(printf '%b' "$line" | LC_ALL=C sed "$strip_ansi_sed")
    local len=${#stripped}
    local padding=$((max_width - len))
    local pad_str=""
    for ((i=0; i<padding; i++)); do
      pad_str+=" "
    done
    printf "\033[%sm|\033[0m  %b%s  \033[%sm|\033[0m\n" "$color" "$line" "$pad_str" "$color"
  done

  printf "\033[%sm+%s+\033[0m\n" "$color" "$border"
}

resolve_version() {
  verbose "resolve_version:start preset=${VERSION:-<unset>}"
  if [ -n "$VERSION" ]; then return 0; fi

  info "Resolving latest version..."
  local latest_url="https://api.github.com/repos/${OWNER}/${REPO}/releases/latest"
  local tag
  if ! tag=$(curl -fsSL -H "Accept: application/vnd.github.v3+json" "$latest_url" 2>/dev/null | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/'); then
    tag=""
  fi

  if [ -n "$tag" ]; then
    VERSION="$tag"
    verbose "resolve_version:github_latest tag=${VERSION}"
    info "Resolved latest version: $VERSION"
  else
    # Try redirect-based resolution as fallback
    local redirect_url="https://github.com/${OWNER}/${REPO}/releases/latest"
    if tag=$(curl -fsSL -o /dev/null -w '%{url_effective}' "$redirect_url" 2>/dev/null | sed -E 's|.*/tag/||'); then
      if [ -n "$tag" ] && [[ "$tag" =~ ^v[0-9] ]] && [[ "$tag" != *"/"* ]]; then
        VERSION="$tag"
        verbose "resolve_version:redirect_latest tag=${VERSION}"
        info "Resolved latest version via redirect: $VERSION"
        return 0
      fi
    fi

    # Try git tags API as last resort (works even without releases)
    local tags_url="https://api.github.com/repos/${OWNER}/${REPO}/tags?per_page=10"
    if tag=$(curl -fsSL -H "Accept: application/vnd.github.v3+json" "$tags_url" 2>/dev/null \
         | grep '"name":' | head -1 | sed -E 's/.*"([^"]+)".*/\1/'); then
      if [ -n "$tag" ] && [[ "$tag" =~ ^v[0-9] ]]; then
        VERSION="$tag"
        verbose "resolve_version:tags_api tag=${VERSION}"
        info "Resolved latest version via tags: $VERSION"
        return 0
      fi
    fi

    VERSION="v0.1.0"
    verbose "resolve_version:fallback_default tag=${VERSION}"
    warn "Could not resolve latest version; defaulting to $VERSION"
  fi
  verbose "resolve_version:done resolved=${VERSION}"
}

detect_platform() {
  OS=$(uname -s | tr 'A-Z' 'a-z')
  ARCH=$(uname -m)
  verbose "detect_platform:raw os=${OS} arch=${ARCH}"
  case "$ARCH" in
    x86_64|amd64) ARCH="x86_64" ;;
    arm64|aarch64) ARCH="aarch64" ;;
    *) warn "Unknown arch $ARCH, using as-is" ;;
  esac

  TARGET=""
  # For linux x86_64 we prefer the statically-linked musl artifact so the
  # binary runs on every modern Linux regardless of host glibc (Debian 12,
  # Ubuntu 22.04, RHEL 9, Amazon Linux 2023, Alpine). If the musl asset is
  # missing (e.g., installing an older release that only shipped gnu), the
  # download step falls back to the gnu artifact automatically.
  case "${OS}-${ARCH}" in
    linux-x86_64) TARGET="x86_64-unknown-linux-musl" ;;
    linux-aarch64) TARGET="aarch64-unknown-linux-gnu" ;;
    darwin-x86_64) TARGET="x86_64-apple-darwin" ;;
    darwin-aarch64) TARGET="aarch64-apple-darwin" ;;
    *) :;;
  esac

  if [ -z "$TARGET" ] && [ "$FROM_SOURCE" -eq 0 ] && [ -z "$ARTIFACT_URL" ]; then
    warn "No prebuilt artifact for ${OS}/${ARCH}; falling back to build-from-source"
    FROM_SOURCE=1
  fi
  verbose "detect_platform:normalized os=${OS} arch=${ARCH} target=${TARGET:-<none>} from_source=${FROM_SOURCE}"
}

set_artifact_url() {
  TAR=""
  URL=""
  verbose "set_artifact_url:start artifact_url=${ARTIFACT_URL:-<unset>} target=${TARGET:-<none>} from_source=${FROM_SOURCE}"
  if [ "$FROM_SOURCE" -eq 0 ]; then
    if [ -n "$ARTIFACT_URL" ]; then
      TAR=$(basename "$ARTIFACT_URL")
      URL="$ARTIFACT_URL"
    elif [ -n "$TARGET" ]; then
      TAR="mcp-agent-mail-${TARGET}.tar.xz"
      URL="https://github.com/${OWNER}/${REPO}/releases/download/${VERSION}/${TAR}"
    else
      warn "No prebuilt artifact for ${OS}/${ARCH}; falling back to build-from-source"
      FROM_SOURCE=1
    fi
  fi
  verbose "set_artifact_url:done tar=${TAR:-<none>} url=${URL:-<none>} from_source=${FROM_SOURCE}"
}

check_disk_space() {
  local min_kb=20480  # 20MB minimum for binaries alone
  local path="$DEST"
  if [ ! -d "$path" ]; then
    path=$(dirname "$path")
  fi
  if command -v df >/dev/null 2>&1; then
    local avail_kb
    avail_kb=$(df -Pk "$path" | awk 'NR==2 {print $4}')
    if [ -n "$avail_kb" ] && [ "$avail_kb" -lt "$min_kb" ]; then
      err "Insufficient disk space in $path (need at least 20MB, have $(( avail_kb / 1024 ))MB)"
      err "Free disk space or choose a different install directory with --dest."
      exit 1
    fi
    # Also check the storage root where the database lives (may be different partition)
    local storage_dir="${STORAGE_ROOT:-$HOME/.mcp_agent_mail_git_mailbox_repo}"
    if [ -d "$storage_dir" ]; then
      local storage_avail_kb
      storage_avail_kb=$(df -Pk "$storage_dir" | awk 'NR==2 {print $4}')
      if [ -n "$storage_avail_kb" ] && [ "$storage_avail_kb" -lt 102400 ]; then
        warn "Low disk space in $storage_dir ($(( storage_avail_kb / 1024 ))MB free)."
        warn "Database migration requires ~200-500MB of free space."
        warn "Consider cleaning old backups: rm ${storage_dir}/storage.sqlite3.bak.* ${storage_dir}/storage.sqlite3.pre-python-import-* ${storage_dir}/storage.sqlite3.corrupt-*"
      fi
    fi
  else
    warn "df not found; skipping disk space check"
  fi
}

# Helper: check if there is sufficient space to copy a file to a destination directory
check_copy_disk_space() {
  local src_path="$1"
  local dest_dir="$2"
  local multiplier="${3:-2}"
  command -v df >/dev/null 2>&1 || return 0
  command -v du >/dev/null 2>&1 || return 0
  local src_kb dest_avail_kb
  src_kb=$(du -k "$src_path" 2>/dev/null | awk '{print $1}')
  dest_avail_kb=$(df -Pk "$dest_dir" 2>/dev/null | awk 'NR==2 {print $4}')
  if [ -z "$src_kb" ] || [ -z "$dest_avail_kb" ]; then
    return 0  # Cannot determine sizes; assume OK
  fi
  local needed_kb=$(( src_kb * multiplier ))
  if [ "$dest_avail_kb" -lt "$needed_kb" ]; then
    return 1
  fi
  return 0
}

check_write_permissions() {
  if [ ! -d "$DEST" ]; then
    if ! mkdir -p "$DEST" 2>/dev/null; then
      err "Cannot create $DEST (insufficient permissions)"
      err "Try running with sudo or choose a writable --dest"
      exit 1
    fi
  fi
  if [ ! -w "$DEST" ]; then
    err "No write permission to $DEST"
    err "Try running with sudo or choose a writable --dest"
    exit 1
  fi
}

capture_command_with_timeout() {
  local timeout_secs="$1"
  shift

  CAPTURED_CMD_OUTPUT=""
  CAPTURED_CMD_STATUS=0

  if command -v python3 >/dev/null 2>&1; then
    local output_file status_file tmp_root rc
    tmp_root="${TMP:-/tmp}"
    output_file="${tmp_root}/am-install-capture.$$.$RANDOM.out"
    status_file="${tmp_root}/am-install-capture.$$.$RANDOM.status"

    if python3 - "$timeout_secs" "$output_file" "$status_file" "$@" <<'PY'
import subprocess
import sys
import time

timeout_secs = float(sys.argv[1])
output_path = sys.argv[2]
status_path = sys.argv[3]
cmd = sys.argv[4:]

proc = subprocess.Popen(
    cmd,
    stdout=subprocess.PIPE,
    stderr=subprocess.STDOUT,
    text=True,
)

deadline = time.time() + timeout_secs
timed_out = False
while time.time() < deadline:
    if proc.poll() is not None:
        break
    time.sleep(0.05)

if proc.poll() is None:
    timed_out = True
    proc.kill()

output, _ = proc.communicate()
with open(output_path, "w", encoding="utf-8") as handle:
    handle.write(output or "")
with open(status_path, "w", encoding="utf-8") as handle:
    handle.write("124" if timed_out else str(proc.returncode))

sys.exit(124 if timed_out else proc.returncode)
PY
    then
      rc=0
    else
      rc=$?
    fi

    [ -f "$output_file" ] && CAPTURED_CMD_OUTPUT=$(cat "$output_file")
    [ -f "$status_file" ] && CAPTURED_CMD_STATUS=$(cat "$status_file")
    rm -f "$output_file" "$status_file" 2>/dev/null || true

    if [ -z "${CAPTURED_CMD_STATUS:-}" ]; then
      CAPTURED_CMD_STATUS="$rc"
    fi
    return "$CAPTURED_CMD_STATUS"
  fi

  if CAPTURED_CMD_OUTPUT=$("$@" 2>&1); then
    CAPTURED_CMD_STATUS=0
    return 0
  fi
  CAPTURED_CMD_STATUS=$?
  return "$CAPTURED_CMD_STATUS"
}

check_existing_install() {
  verbose "check_existing_install:start dest=${DEST}"
  if [ -x "$DEST/$BIN_CLI" ]; then
    local current
    if capture_command_with_timeout 3 "$DEST/$BIN_CLI" --version; then
      current=$(printf '%s\n' "$CAPTURED_CMD_OUTPUT" | head -1)
    else
      current=$(printf '%s\n' "$CAPTURED_CMD_OUTPUT" | head -1)
      if [ "$CAPTURED_CMD_STATUS" -eq 124 ]; then
        verbose "check_existing_install:am timeout path=${DEST}/$BIN_CLI"
      fi
    fi
    if [ -n "$current" ]; then
      info "Existing am detected: $current"
      verbose "check_existing_install:am version=${current}"
    fi
  fi
  if [ -x "$DEST/$BIN_SERVER" ]; then
    local current
    if capture_command_with_timeout 3 "$DEST/$BIN_SERVER" --version; then
      current=$(printf '%s\n' "$CAPTURED_CMD_OUTPUT" | head -1)
    else
      current=$(printf '%s\n' "$CAPTURED_CMD_OUTPUT" | head -1)
      if [ "$CAPTURED_CMD_STATUS" -eq 124 ]; then
        verbose "check_existing_install:server timeout path=${DEST}/$BIN_SERVER"
      fi
    fi
    if [ -n "$current" ]; then
      info "Existing mcp-agent-mail detected: $current"
      verbose "check_existing_install:mcp-agent-mail version=${current}"
    fi
  fi
  verbose "check_existing_install:done"
}

check_network() {
  if [ "$OFFLINE" -eq 1 ]; then
    info "Offline mode enabled; skipping network preflight"
    return 0
  fi
  if [ "$FROM_SOURCE" -eq 1 ]; then
    return 0
  fi
  if [ -z "$URL" ]; then
    return 0
  fi
  if ! command -v curl >/dev/null 2>&1; then
    warn "curl not found; skipping network check"
    return 0
  fi
  if ! curl -fsSI --connect-timeout 3 --max-time 5 -o /dev/null "$URL" 2>/dev/null; then
    warn "Network check failed for $URL"
    warn "Continuing; download may fail"
  fi
}

# ── Python installation detection (T1.1, T1.2, T1.3) ──────────────────────

# Result variables set by detect_python_*
PYTHON_ALIAS_FOUND=0
PYTHON_ALIAS_FILE=""
PYTHON_ALIAS_LINE=0
PYTHON_ALIAS_CONTENT=""
PYTHON_ALIAS_KIND=""
PYTHON_ALIAS_HAS_MARKERS=0
PYTHON_BINARY_FOUND=0
PYTHON_BINARY_PATH=""
PYTHON_CLONE_FOUND=0
PYTHON_CLONE_PATH=""
PYTHON_VENV_PATH=""
PYTHON_PID=""
PYTHON_DETECTED=0
PYTHON_DB_FOUND=0
PYTHON_DB_PATH=""
PYTHON_DB_MIGRATED_PATH=""
PYTHON_DB_FORMAT=""
PYTHON_DB_PROBE_OUTPUT=""
MIGRATED_BEARER_TOKEN=""
RUST_DB_PATH=""
PYTHON_ALIAS_DISPLACED_COUNT=0
PYTHON_CURRENT_SHELL_TAKEOVER_POSSIBLE=1
LEGACY_LAUNCHER_SHIM_COUNT=0
MAC_DIRECT_EXEC_COMPAT_MODE=0
MAC_DIRECT_EXEC_COMPAT_REASON=""
MAC_DIRECT_EXEC_COMPAT_LAUNCHER=""
CAPTURED_CMD_OUTPUT=""
CAPTURED_CMD_STATUS=0
PYTHON_MIGRATION_MARKER="${PYTHON_MIGRATION_MARKER:-$HOME/.config/mcp-agent-mail/.python-migration-complete}"

path_resolves_to_installed_am() {
  local candidate="$1"
  [ -n "$candidate" ] || return 1

  local dest_real candidate_real
  dest_real=$(readlink -f "$DEST/$BIN_CLI" 2>/dev/null || printf '%s\n' "$DEST/$BIN_CLI")
  candidate_real=$(readlink -f "$candidate" 2>/dev/null || printf '%s\n' "$candidate")

  [ "$candidate_real" = "$dest_real" ]
}

# T1.1: Detect Python am alias in shell rc files
detect_python_alias() {
  PYTHON_ALIAS_FOUND=0
  PYTHON_ALIAS_FILE=""
  PYTHON_ALIAS_LINE=0
  PYTHON_ALIAS_CONTENT=""
  PYTHON_ALIAS_KIND=""
  PYTHON_ALIAS_HAS_MARKERS=0

  local rc_files=(
    "$HOME/.zshrc"
    "$HOME/.zprofile"
    "$HOME/.zshenv"
    "$HOME/.zlogin"
    "$HOME/.bashrc"
    "$HOME/.bash_profile"
    "$HOME/.profile"
    "$HOME/.aliases"
    "$HOME/.zsh_aliases"
    "$HOME/.config/zsh/.zshrc"
    "$HOME/.config/zsh/aliases.zsh"
  )
  # Fish uses different syntax; check config.fish too
  local fish_config="$HOME/.config/fish/config.fish"
  if [ -f "$fish_config" ]; then
    rc_files+=("$fish_config")
  fi
  if [ -d "$HOME/.config/fish/conf.d" ]; then
    while IFS= read -r fish_file; do
      [ -n "$fish_file" ] && rc_files+=("$fish_file")
    done < <(find "$HOME/.config/fish/conf.d" -maxdepth 1 -type f -name "*.fish" 2>/dev/null | sort || true)
  fi

  # Follow source/. directives in primary rc files to find aliases in sourced configs
  # This catches ACFS (~/.acfs/zsh/acfs.zshrc) and similar framework-managed configs
  local sourced_files=()
  for rc in "${rc_files[@]}"; do
    [ -f "$rc" ] || continue
    while IFS= read -r sourced; do
      # Resolve $HOME and ~ in source paths
      sourced="${sourced/\$HOME/$HOME}"
      sourced="${sourced/#\~/$HOME}"
      # Remove surrounding quotes
      sourced="${sourced#\"}"
      sourced="${sourced%\"}"
      sourced="${sourced#\'}"
      sourced="${sourced%\'}"
      if [ -f "$sourced" ] && [[ ! " ${rc_files[*]} " =~ " ${sourced} " ]]; then
        sourced_files+=("$sourced")
      fi
    done < <(grep -oE '^\s*(source|\.)\s+"?[^"#]+"?' "$rc" 2>/dev/null | sed -E 's/^\s*(source|\.)\s+//' | sed 's/#.*//' | sed 's/[[:space:]]*$//' || true)
  done
  rc_files+=("${sourced_files[@]}")

  # Also directly check ACFS paths (common agent framework that defines am alias)
  local acfs_zshrc="$HOME/.acfs/zsh/acfs.zshrc"
  if [ -f "$acfs_zshrc" ] && [[ ! " ${rc_files[*]} " =~ " ${acfs_zshrc} " ]]; then
    rc_files+=("$acfs_zshrc")
  fi
  local acfs_bashrc="$HOME/.acfs/bash/acfs.bashrc"
  if [ -f "$acfs_bashrc" ] && [[ ! " ${rc_files[*]} " =~ " ${acfs_bashrc} " ]]; then
    rc_files+=("$acfs_bashrc")
  fi

  for rc in "${rc_files[@]}"; do
    [ -f "$rc" ] || continue

    # Check for marker block: "# >>> MCP Agent Mail alias" ... "# <<< MCP Agent Mail alias"
    # Only treat as active if the block still contains a live alias/function line.
    if grep -q '# >>> MCP Agent Mail' "$rc" 2>/dev/null; then
      local marker_line
      marker_line=$(grep -n '# >>> MCP Agent Mail' "$rc" | head -1 | cut -d: -f1)
      local marker_payload
      marker_payload=$(sed -n '/# >>> MCP Agent Mail/,/# <<< MCP Agent Mail/p' "$rc")
      local active_entry
      active_entry=$(printf '%s\n' "$marker_payload" | grep -E "^[[:space:]]*(alias am=|alias am |function am($|[[:space:](])|am[[:space:]]*\\(\\))" | head -1 || true)

      if [ -n "$active_entry" ]; then
        PYTHON_ALIAS_FOUND=1
        PYTHON_ALIAS_FILE="$rc"
        PYTHON_ALIAS_HAS_MARKERS=1
        PYTHON_ALIAS_LINE="$marker_line"
        PYTHON_ALIAS_CONTENT="$active_entry"
        if echo "$active_entry" | grep -qE "^[[:space:]]*(function am($|[[:space:](])|am[[:space:]]*\\(\\))"; then
          PYTHON_ALIAS_KIND="function"
        else
          PYTHON_ALIAS_KIND="alias"
        fi
        verbose "detect_python_alias:found file=${PYTHON_ALIAS_FILE} line=${PYTHON_ALIAS_LINE} kind=${PYTHON_ALIAS_KIND} markers=1"
        return 0
      fi
    fi

    # Check for bare "alias am=" (bash/zsh) or "alias am " (fish) outside markers
    local alias_line=""
    alias_line=$(grep -n -E "^[[:space:]]*(alias am=|alias am )" "$rc" 2>/dev/null | grep -iv "disabled\|#.*alias am" | head -1 || true)
    if [ -n "$alias_line" ]; then
      # Skip commented-out aliases
      local line_content
      line_content=$(echo "$alias_line" | cut -d: -f2-)
      if echo "$line_content" | grep -q "^[[:space:]]*#"; then
        continue
      fi
      PYTHON_ALIAS_FOUND=1
      PYTHON_ALIAS_FILE="$rc"
      PYTHON_ALIAS_LINE=$(echo "$alias_line" | cut -d: -f1)
      PYTHON_ALIAS_CONTENT="$line_content"
      PYTHON_ALIAS_KIND="alias"
      PYTHON_ALIAS_HAS_MARKERS=0
      verbose "detect_python_alias:found file=${PYTHON_ALIAS_FILE} line=${PYTHON_ALIAS_LINE} kind=${PYTHON_ALIAS_KIND} markers=0"
      return 0
    fi

    # Check for function definition: "function am()" or "am()" (bash/zsh)
    # Or "function am" (fish)
    local func_line=""
    func_line=$(grep -n -E "^[[:space:]]*(function am($|[[:space:](])|am[[:space:]]*\(\))" "$rc" 2>/dev/null | grep -v "^[[:space:]]*#" | head -1 || true)
    if [ -n "$func_line" ]; then
      local line_content
      line_content=$(echo "$func_line" | cut -d: -f2-)
      if ! echo "$line_content" | grep -q "^[[:space:]]*#"; then
        PYTHON_ALIAS_FOUND=1
        PYTHON_ALIAS_FILE="$rc"
        PYTHON_ALIAS_LINE=$(echo "$func_line" | cut -d: -f1)
        PYTHON_ALIAS_CONTENT="$line_content"
        PYTHON_ALIAS_KIND="function"
        PYTHON_ALIAS_HAS_MARKERS=0
        verbose "detect_python_alias:found file=${PYTHON_ALIAS_FILE} line=${PYTHON_ALIAS_LINE} kind=${PYTHON_ALIAS_KIND} markers=0"
        return 0
      fi
    fi
  done
  verbose "detect_python_alias:not_found"
}

python_alias_entry_body() {
  [ "$PYTHON_ALIAS_FOUND" -eq 1 ] || return 1
  [ -n "${PYTHON_ALIAS_FILE:-}" ] || return 1
  [ -f "$PYTHON_ALIAS_FILE" ] || return 1

  if [ "$PYTHON_ALIAS_HAS_MARKERS" -eq 1 ]; then
    sed -n '/# >>> MCP Agent Mail/,/# <<< MCP Agent Mail/p' "$PYTHON_ALIAS_FILE" 2>/dev/null || true
    return 0
  fi

  if [ "${PYTHON_ALIAS_KIND:-alias}" = "function" ] && [ "${PYTHON_ALIAS_LINE:-0}" -gt 0 ]; then
    sed -n "${PYTHON_ALIAS_LINE},$((PYTHON_ALIAS_LINE + 20))p" "$PYTHON_ALIAS_FILE" 2>/dev/null || true
    return 0
  fi

  printf '%s\n' "${PYTHON_ALIAS_CONTENT:-}"
}

python_alias_targets_rewritable_helper() {
  [ "$PYTHON_CLONE_FOUND" -eq 1 ] || return 1
  [ -n "${PYTHON_CLONE_PATH:-}" ] || return 1

  local alias_body=""
  local expected_helper="${PYTHON_CLONE_PATH%/}/scripts/run_server_with_token.sh"
  local helper_path=""
  local clone_path=""

  alias_body="$(python_alias_entry_body 2>/dev/null || true)"
  [ -z "$alias_body" ] && alias_body="${PYTHON_ALIAS_CONTENT:-}"
  [ -n "$alias_body" ] || return 1

  helper_path=$(printf '%s\n' "$alias_body" | sed -n "s|.*['\"]\{0,1\}\([^\"'[:space:]]*/scripts/run_server_with_token\\.sh\).*|\1|p" | tail -1)
  helper_path="${helper_path/#\~/$HOME}"
  if [ -n "$helper_path" ] && [ "${helper_path%/}" = "${expected_helper%/}" ]; then
    return 0
  fi

  clone_path=$(printf '%s\n' "$alias_body" | sed -n "s/.*cd [\"']*\([^\"';&|]*\)[\"']*.*/\1/p" | tail -1)
  clone_path="${clone_path/#\~/$HOME}"
  if [ -n "$clone_path" ] && [ "${clone_path%/}" = "${PYTHON_CLONE_PATH%/}" ]; then
    printf '%s\n' "$alias_body" | grep -Eq '(^|[[:space:];&|])(\./)?scripts/run_server_with_token\.sh([[:space:];&|)"'"'"'$]|$)'
    return $?
  fi

  return 1
}

# T1.2: Detect Python am binary/script in PATH
detect_python_binary() {
  PYTHON_BINARY_FOUND=0
  PYTHON_BINARY_PATH=""

  # Check for am binaries/scripts in PATH that are NOT the Rust binary
  local all_am
  all_am=$(which -a am 2>/dev/null || true)
  [ -z "$all_am" ] && return 0

  while IFS= read -r am_path; do
    [ -z "$am_path" ] && continue
    # Skip our own install destination
    [ "$am_path" = "$DEST/$BIN_CLI" ] && continue
    [ "$am_path" = "$DEST/am" ] && continue

    # Check if it's a Python-related am
    if [ -L "$am_path" ]; then
      local link_target
      link_target=$(readlink -f "$am_path" 2>/dev/null || readlink "$am_path" 2>/dev/null || true)
      if path_resolves_to_installed_am "$link_target"; then
        verbose "detect_python_binary:skip_rust_symlink path=${am_path} target=${link_target}"
        continue
      fi
      if echo "$link_target" | grep -qiE '(^|/)(python[0-9.]*|venv|\.venv|virtualenv|site-packages)(/|$)|/\.local/lib/python'; then
        PYTHON_BINARY_FOUND=1
        PYTHON_BINARY_PATH="$am_path"
        verbose "detect_python_binary:found symlink_path=${PYTHON_BINARY_PATH}"
        return 0
      fi
    fi

    # Check shebang/content for Python references, but only for text files.
    # Reading compiled binaries into command substitution can emit warnings
    # like "ignored null byte in input" on macOS bash.
    if [ -f "$am_path" ] && [ -r "$am_path" ]; then
      if LC_ALL=C grep -Iq . "$am_path" 2>/dev/null; then
        if head -5 "$am_path" 2>/dev/null | LC_ALL=C grep -qiE "python|#!/.*python"; then
          PYTHON_BINARY_FOUND=1
          PYTHON_BINARY_PATH="$am_path"
          verbose "detect_python_binary:found script_path=${PYTHON_BINARY_PATH}"
          return 0
        fi
      else
        verbose "detect_python_binary:skip_binary path=${am_path}"
      fi
    fi

    # Check if it's in a Python virtualenv or site-packages directory
    if echo "$am_path" | grep -qiE "venv|virtualenv|site-packages|\.local/lib/python"; then
      PYTHON_BINARY_FOUND=1
      PYTHON_BINARY_PATH="$am_path"
      verbose "detect_python_binary:found pythonish_path=${PYTHON_BINARY_PATH}"
      return 0
    fi
  done <<< "$all_am"

  # Also check for python -m mcp_agent_mail availability
  if command -v python3 >/dev/null 2>&1 && python3 -c "import mcp_agent_mail" 2>/dev/null; then
    PYTHON_BINARY_FOUND=1
    PYTHON_BINARY_PATH="python3 -m mcp_agent_mail"
    verbose "detect_python_binary:found importable=${PYTHON_BINARY_PATH}"
  elif command -v python >/dev/null 2>&1 && python -c "import mcp_agent_mail" 2>/dev/null; then
    PYTHON_BINARY_FOUND=1
    PYTHON_BINARY_PATH="python -m mcp_agent_mail"
    verbose "detect_python_binary:found importable=${PYTHON_BINARY_PATH}"
  fi
  if [ "$PYTHON_BINARY_FOUND" -eq 0 ]; then verbose "detect_python_binary:not_found"; fi
}

# Internal FrankenSQLite-backed DB helpers. These intentionally route through
# the freshly installed `am` binary so installer repair logic uses the same
# runtime stack as the shipped product.
installed_am_db_query_scalar() {
  local db_path="$1"
  local sql="$2"
  local cli_bin="${DEST:-}/${BIN_CLI:-am}"

  [ -x "$cli_bin" ] || return 1
  "$cli_bin" tooling db-query --db "$db_path" --sql "$sql" --first
}

installed_am_db_exec() {
  local db_path="$1"
  local sql="${2:-}"
  local cli_bin="${DEST:-}/${BIN_CLI:-am}"

  [ -x "$cli_bin" ] || return 1
  if [ -n "$sql" ]; then
    "$cli_bin" tooling db-exec --db "$db_path" --sql "$sql" >/dev/null
  else
    "$cli_bin" tooling db-exec --db "$db_path" >/dev/null
  fi
}

installed_am_db_backup() {
  local db_path="$1"
  local output_path="$2"
  local cli_bin="${DEST:-}/${BIN_CLI:-am}"

  [ -x "$cli_bin" ] || return 1
  "$cli_bin" tooling db-backup --db "$db_path" --output "$output_path" >/dev/null
}

# Copy a SQLite database as a consistent snapshot.
# Prefer the FrankenSQLite-backed CLI helper so WAL content is checkpointed
# before copying and the snapshot is coherent.
copy_sqlite_snapshot() {
  local src_db="$1"
  local dest_db="$2"

  rm -f "$dest_db" "${dest_db}-wal" "${dest_db}-shm" 2>/dev/null || true

  if installed_am_db_backup "$src_db" "$dest_db"; then
    return 0
  fi

  warn "FrankenSQLite backup helper unavailable; copying DB file and sidecars directly"
  cp -p "$src_db" "$dest_db"
  [ -f "${src_db}-wal" ] && cp -p "${src_db}-wal" "${dest_db}-wal"
  [ -f "${src_db}-shm" ] && cp -p "${src_db}-shm" "${dest_db}-shm"
}

count_matching_backup_files() {
  local prefix="$1"
  local dir
  local base
  dir=$(dirname "$prefix")
  base=$(basename "$prefix")
  find "$dir" -maxdepth 1 -type f -name "${base}*" 2>/dev/null | wc -l | tr -d ' '
}

extract_migrate_check_format() {
  local output="$1"
  printf "%s\n" "$output" | sed -n 's/^Database format: //p' | head -1
}

strip_wrapping_quotes() {
  local value="${1:-}"
  value="${value%\"}"
  value="${value#\"}"
  value="${value%\'}"
  value="${value#\'}"
  printf '%s\n' "$value"
}

trim_ascii_whitespace() {
  local value="${1:-}"
  value="${value#"${value%%[![:space:]]*}"}"
  value="${value%"${value##*[![:space:]]}"}"
  printf '%s\n' "$value"
}

parse_env_assignment_rhs() {
  local raw
  raw=$(trim_ascii_whitespace "${1:-}")

  local out=""
  local quote=""
  local prev=""
  local char=""
  local i=0
  local raw_len=${#raw}

  while [ "$i" -lt "$raw_len" ]; do
    char="${raw:$i:1}"
    if [ -n "$quote" ]; then
      out="${out}${char}"
      if [ "$char" = "$quote" ]; then
        quote=""
      fi
    else
      if [ "$char" = '"' ] || [ "$char" = "'" ]; then
        quote="$char"
        out="${out}${char}"
      elif [ "$char" = "#" ]; then
        if [ -z "$prev" ] || [[ "$prev" =~ [[:space:]] ]]; then
          break
        fi
        out="${out}${char}"
      else
        out="${out}${char}"
      fi
    fi
    prev="$char"
    i=$((i + 1))
  done

  out=$(trim_ascii_whitespace "$out")
  strip_wrapping_quotes "$out"
}

read_env_assignment_value() {
  local file="$1"
  local key="$2"
  local value=""

  [ -f "$file" ] || return 0
  value=$(grep -E "^[[:space:]]*(export[[:space:]]+)?${key}[[:space:]]*=" "$file" 2>/dev/null | tail -1 | sed -E "s/^[[:space:]]*(export[[:space:]]+)?${key}[[:space:]]*=[[:space:]]*//" || true)
  [ -n "$value" ] || return 0
  parse_env_assignment_rhs "$value"
}

python_db_format_needs_import() {
  local format="$1"
  case "$format" in
    TEXT\ timestamps\ \(*|mixed\ format\ \(*) return 0 ;;
    *) return 1 ;;
  esac
}

probe_database_format_with_sqlite() {
  local db_path="$1"
  local saw_integer=0
  local saw_text=0
  local saw_rows=0
  local text_tables=""
  local table=""
  local column=""
  local type_str=""
  local row_present=""
  local type_query=""
  local row_query=""

  [ -f "$db_path" ] || return 1
  installed_am_db_query_scalar "$db_path" "SELECT 1;" >/dev/null 2>&1 || return 1

  while IFS=: read -r table column; do
    installed_am_db_query_scalar "$db_path" "SELECT 1 FROM sqlite_master WHERE type='table' AND name='${table}' LIMIT 1;" >/dev/null 2>&1 || continue
    installed_am_db_query_scalar "$db_path" "SELECT 1 FROM pragma_table_info('${table}') WHERE name='${column}' LIMIT 1;" >/dev/null 2>&1 || continue

    row_query="SELECT 1 FROM ${table} LIMIT 1;"
    row_present=$(installed_am_db_query_scalar "$db_path" "$row_query" 2>/dev/null || true)
    [ -n "$row_present" ] && saw_rows=1

    type_query="SELECT typeof(${column}) FROM ${table} WHERE ${column} IS NOT NULL LIMIT 1;"
    type_str=$(installed_am_db_query_scalar "$db_path" "$type_query" 2>/dev/null || true)
    case "$type_str" in
      integer|real)
        saw_integer=1
        ;;
      text)
        saw_text=1
        case ",${text_tables}," in
          *",${table},"*) ;;
          *) text_tables="${text_tables}${text_tables:+, }${table}" ;;
        esac
        ;;
      *)
        ;;
    esac
  done <<'EOF'
projects:created_at
products:created_at
product_project_links:created_at
agents:inception_ts
agents:last_active_ts
messages:created_ts
message_recipients:read_ts
message_recipients:ack_ts
file_reservations:created_ts
file_reservations:expires_ts
file_reservations:released_ts
agent_links:created_ts
agent_links:updated_ts
agent_links:expires_ts
project_sibling_suggestions:created_ts
project_sibling_suggestions:evaluated_ts
project_sibling_suggestions:confirmed_ts
project_sibling_suggestions:dismissed_ts
EOF

  if [ "$saw_text" -eq 1 ] && [ "$saw_integer" -eq 0 ]; then
    PYTHON_DB_FORMAT="TEXT timestamps (installer fallback, needs migration)"
    return 0
  fi
  if [ "$saw_text" -eq 1 ] && [ "$saw_integer" -eq 1 ]; then
    PYTHON_DB_FORMAT="mixed format (TEXT in: ${text_tables})"
    return 0
  fi
  if [ "$saw_integer" -eq 1 ]; then
    PYTHON_DB_FORMAT="i64 microseconds (installer fallback)"
    return 0
  fi
  if [ "$saw_rows" -eq 1 ]; then
    PYTHON_DB_FORMAT="unknown format: existing rows without readable timestamp columns"
    return 0
  fi

  return 1
}

probe_database_format_with_installed_am() {
  local db_path="$1"
  local cli_bin="${DEST}/${BIN_CLI}"
  local output="" fallback_output="" cli_format=""
  PYTHON_DB_FORMAT=""
  PYTHON_DB_PROBE_OUTPUT=""

  [ -x "$cli_bin" ] || return 1

  if capture_command_with_timeout 5 env AM_INTERFACE_MODE=cli DATABASE_URL="sqlite:///$db_path" "$cli_bin" migrate --check; then
    output="$CAPTURED_CMD_OUTPUT"
  else
    output="$CAPTURED_CMD_OUTPUT"
    verbose "db_probe:primary_nonzero db=${db_path}"
    if [ "$CAPTURED_CMD_STATUS" -eq 124 ]; then
      verbose "db_probe:primary_timeout db=${db_path}"
    fi
  fi
  PYTHON_DB_FORMAT=$(extract_migrate_check_format "$output")

  if [ -z "$PYTHON_DB_FORMAT" ]; then
    if capture_command_with_timeout 5 env AM_INTERFACE_MODE=cli DATABASE_URL="sqlite+aiosqlite:///$db_path" "$cli_bin" migrate --check; then
      fallback_output="$CAPTURED_CMD_OUTPUT"
    else
      fallback_output="$CAPTURED_CMD_OUTPUT"
      verbose "db_probe:fallback_nonzero db=${db_path}"
      if [ "$CAPTURED_CMD_STATUS" -eq 124 ]; then
        verbose "db_probe:fallback_timeout db=${db_path}"
      fi
    fi
    if [ -n "$fallback_output" ]; then
      if [ -n "$output" ]; then
        output="${output}"$'\n'"${fallback_output}"
      else
        output="$fallback_output"
      fi
    fi
    PYTHON_DB_FORMAT=$(extract_migrate_check_format "$fallback_output")
  fi

  PYTHON_DB_PROBE_OUTPUT="$output"
  cli_format="$PYTHON_DB_FORMAT"

  if [ -n "$cli_format" ] && [ "${cli_format#empty database (}" = "$cli_format" ]; then
    verbose "db_probe:format db=${db_path} format=${PYTHON_DB_FORMAT}"
    return 0
  fi

  if probe_database_format_with_sqlite "$db_path"; then
    if [ -n "$cli_format" ] && [ "$cli_format" != "$PYTHON_DB_FORMAT" ]; then
      verbose "db_probe:sqlite_override db=${db_path} cli_format=${cli_format} sqlite_format=${PYTHON_DB_FORMAT}"
    else
      verbose "db_probe:sqlite_format db=${db_path} format=${PYTHON_DB_FORMAT}"
    fi
    return 0
  fi

  if [ -n "$cli_format" ]; then
    PYTHON_DB_FORMAT="$cli_format"
    verbose "db_probe:format db=${db_path} format=${PYTHON_DB_FORMAT}"
    return 0
  fi

  while IFS= read -r line; do
    [ -n "$line" ] && verbose "db_probe:output ${line}"
  done <<< "$output"
  return 1
}

# T1.3: Detect Python virtualenv and git clone
detect_python_installation() {
  verbose "detect_python_installation:start"
  PYTHON_CLONE_FOUND=0
  PYTHON_CLONE_PATH=""
  PYTHON_VENV_PATH=""
  PYTHON_PID=""

  # Check common clone locations
  local candidates=(
    "$HOME/mcp_agent_mail"
    "$HOME/mcp-agent-mail"
    "$HOME/projects/mcp_agent_mail"
    "$HOME/code/mcp_agent_mail"
  )

  # If we found an alias, extract the path from it
  if [ "$PYTHON_ALIAS_FOUND" -eq 1 ] && [ -n "$PYTHON_ALIAS_CONTENT" ]; then
    local alias_payload
    alias_payload="$(python_alias_entry_body 2>/dev/null || true)"
    [ -z "$alias_payload" ] && alias_payload="$PYTHON_ALIAS_CONTENT"
    local alias_path
    # Extract path from patterns like: alias am='cd "/path/to/dir" && ...'
    alias_path=$(printf '%s\n' "$alias_payload" | sed -n "s/.*cd [\"']*\([^\"'&]*\)[\"']*.*/\1/p" | head -1)
    [ -n "$alias_path" ] && candidates+=("$alias_path")
    # Also try: alias am='cd /path/to/dir && ...'
    alias_path=$(printf '%s\n' "$alias_payload" | sed -n 's/.*cd \([^ &"'"'"']*\).*/\1/p' | head -1)
    [ -n "$alias_path" ] && candidates+=("$alias_path")
    # If the helper path itself is referenced directly, infer the clone root from it.
    alias_path=$(printf '%s\n' "$alias_payload" | sed -n "s|.*['\"]\{0,1\}\([^\"'[:space:]]*/scripts/run_server_with_token\\.sh\).*|\1|p" | head -1)
    if [ -n "$alias_path" ]; then
      candidates+=("$(dirname "$(dirname "$alias_path")")")
    fi
  fi

  for dir in "${candidates[@]}"; do
    # Expand ~ if present
    dir="${dir/#\~/$HOME}"
    [ -d "$dir" ] || continue

    # Check for Python mcp_agent_mail markers
    if [ -f "$dir/pyproject.toml" ] && grep -q "mcp.agent.mail\|mcp_agent_mail" "$dir/pyproject.toml" 2>/dev/null; then
      PYTHON_CLONE_FOUND=1
      PYTHON_CLONE_PATH="$dir"
      # Check for virtualenv
      if [ -d "$dir/.venv" ]; then
        PYTHON_VENV_PATH="$dir/.venv"
      elif [ -d "$dir/venv" ]; then
        PYTHON_VENV_PATH="$dir/venv"
      fi
      break
    fi

    # Also check for src/mcp_agent_mail/ (source package layout)
    if [ -d "$dir/src/mcp_agent_mail" ]; then
      PYTHON_CLONE_FOUND=1
      PYTHON_CLONE_PATH="$dir"
      [ -d "$dir/.venv" ] && PYTHON_VENV_PATH="$dir/.venv"
      [ -d "$dir/venv" ] && PYTHON_VENV_PATH="$dir/venv"
      break
    fi
  done

  # Check for running Python server processes
  local pids
  pids=$(pgrep -f "mcp_agent_mail\|mcp.agent.mail" 2>/dev/null | head -5 || true)
  if [ -n "$pids" ]; then
    # Filter to actual Python processes
    while IFS= read -r pid; do
      [ -z "$pid" ] && continue
      local cmdline
      cmdline=$(ps -p "$pid" -o command= 2>/dev/null || true)
      if echo "$cmdline" | grep -qiE "python|uvicorn"; then
        PYTHON_PID="$pid"
        break
      fi
    done <<< "$pids"
  fi

  # Set overall detection flag
  if [ "$PYTHON_ALIAS_FOUND" -eq 1 ] || [ "$PYTHON_BINARY_FOUND" -eq 1 ] || [ "$PYTHON_CLONE_FOUND" -eq 1 ]; then
    PYTHON_DETECTED=1
  fi
  verbose "detect_python_installation:done clone_found=${PYTHON_CLONE_FOUND} clone=${PYTHON_CLONE_PATH:-<none>} venv=${PYTHON_VENV_PATH:-<none>} pid=${PYTHON_PID:-<none>}"
}

# Run all Python detection in sequence
detect_python() {
  verbose "detect_python:start"
  detect_python_alias
  detect_python_binary
  detect_python_installation

  if [ "$PYTHON_DETECTED" -eq 1 ]; then
    info "Existing Python mcp-agent-mail detected"
    [ "$PYTHON_ALIAS_FOUND" -eq 1 ] && info "  Alias: $PYTHON_ALIAS_FILE:$PYTHON_ALIAS_LINE"
    [ "$PYTHON_BINARY_FOUND" -eq 1 ] && info "  Binary: $PYTHON_BINARY_PATH"
    [ "$PYTHON_CLONE_FOUND" -eq 1 ] && info "  Clone: $PYTHON_CLONE_PATH"
    [ -n "$PYTHON_VENV_PATH" ] && info "  Venv: $PYTHON_VENV_PATH"
    [ -n "$PYTHON_PID" ] && info "  Running PID: $PYTHON_PID"
  fi
  verbose "detect_python:done detected=${PYTHON_DETECTED} alias=${PYTHON_ALIAS_FOUND} binary=${PYTHON_BINARY_FOUND} clone=${PYTHON_CLONE_FOUND} pid=${PYTHON_PID:-<none>}"
}

# T1.4: Displace Python alias (comment out with backup)
displace_single_python_alias() {
  [ "$PYTHON_ALIAS_FOUND" -eq 0 ] && return 0

  local rc="$PYTHON_ALIAS_FILE"
  [ -z "$rc" ] && return 0
  [ -f "$rc" ] || return 0
  if [ ! -r "$rc" ]; then
    warn "Cannot read shell config file: $rc"
    return 1
  fi
  if [ ! -w "$rc" ]; then
    warn "Cannot modify shell config file (not writable): $rc"
    return 1
  fi
  local rc_dir
  rc_dir=$(dirname "$rc")
  if [ ! -w "$rc_dir" ]; then
    warn "Cannot write alongside shell config file (directory not writable): $rc_dir"
    return 1
  fi

  # Create timestamped backup
  local timestamp
  timestamp=$(date +%Y%m%d_%H%M%S)
  local backup="${rc}.bak.mcp-agent-mail-${timestamp}-${RANDOM}"
  if ! cp -p "$rc" "$backup"; then
    warn "Failed to create backup before modifying alias file: $backup"
    return 1
  fi
  verbose "displace_python_alias:backup rc=${rc} backup=${backup}"
  info "Backed up $rc -> $backup"

  # Write to a temp file, then atomic rename
  local tmpfile="${rc}.tmp.mcp-agent-mail.$$"

  if [ "$PYTHON_ALIAS_HAS_MARKERS" -eq 1 ]; then
    # Replace the marker block with a commented-out version
    awk -v dest="$DEST" -v date="$(date -u +%Y-%m-%dT%H:%M:%SZ)" '
      /# >>> MCP Agent Mail/ { in_block=1; print "# >>> MCP Agent Mail alias (DISABLED by Rust installer on " date ")"; next }
      /# <<< MCP Agent Mail/ { in_block=0; print "# Rust binary installed at: " dest "/am"; print "# To restore Python version: uncomment the alias line(s) above"; print "# <<< MCP Agent Mail alias (DISABLED)"; next }
      in_block && /^[^#]/ { print "# " $0; next }
      { print }
    ' "$rc" > "$tmpfile"
  else
    # Comment out the bare alias line or function block
    local line_num="$PYTHON_ALIAS_LINE"
    if [ "${PYTHON_ALIAS_KIND:-alias}" = "function" ]; then
      awk -v line="$line_num" -v dest="$DEST" '
        function brace_delta(str,    opens, closes, tmp) {
          tmp=str
          opens=gsub(/\{/, "{", tmp)
          tmp=str
          closes=gsub(/\}/, "}", tmp)
          return opens - closes
        }
        NR < line { print; next }
        NR == line {
          print "# Disabled by mcp-agent-mail Rust installer: " $0
          print "# Rust binary at: " dest "/am"
          in_block=1
          is_fish = ($0 ~ /^[[:space:]]*function am([[:space:]]|$)/ && $0 !~ /\(/ && $0 !~ /\{/)
          if (!is_fish) {
            saw_open = ($0 ~ /\{/)
            depth = brace_delta($0)
            if (saw_open && depth <= 0) {
              in_block=0
            }
          }
          next
        }
        in_block {
          print "# Disabled by mcp-agent-mail Rust installer: " $0
          if (is_fish) {
            if ($0 ~ /^[[:space:]]*end([[:space:]]|$)/) {
              in_block=0
            }
          } else {
            if ($0 ~ /\{/) {
              saw_open=1
            }
            depth += brace_delta($0)
            if (saw_open && depth <= 0) {
              in_block=0
            }
          }
          next
        }
        { print }
      ' "$rc" > "$tmpfile"
    else
      awk -v line="$line_num" -v dest="$DEST" '
        NR == line { print "# Disabled by mcp-agent-mail Rust installer: " $0; print "# Rust binary at: " dest "/am"; next }
        { print }
      ' "$rc" > "$tmpfile"
    fi
  fi

  # Verify the temp file is valid (non-empty, at least as many lines as original)
  local orig_lines new_lines
  orig_lines=$(wc -l < "$rc")
  new_lines=$(wc -l < "$tmpfile")
  if [ "$new_lines" -lt "$orig_lines" ]; then
    warn "Displacement produced fewer lines ($new_lines < $orig_lines); aborting rc modification"
    rm -f "$tmpfile"
    return 1
  fi

  # Preserve original permissions
  chmod --reference="$rc" "$tmpfile" 2>/dev/null || chmod "$(stat -f '%A' "$rc" 2>/dev/null || echo 644)" "$tmpfile" 2>/dev/null || true

  # Atomic rename
  if ! mv "$tmpfile" "$rc"; then
    warn "Failed to atomically replace shell config file: $rc"
    rm -f "$tmpfile" 2>/dev/null || true
    return 1
  fi
  if command -v diff >/dev/null 2>&1; then
    local diff_out
    diff_out=$(diff -u "$backup" "$rc" 2>/dev/null || true)
    if [ -n "$diff_out" ]; then
      while IFS= read -r line; do
        verbose "displace_python_alias:diff ${line}"
      done <<< "$diff_out"
    fi
  fi
  ok "Python alias disabled in $rc"
  ok "Backup at $backup"
}

displace_python_alias() {
  local pass=0
  local max_passes=32
  local displaced=0

  PYTHON_ALIAS_DISPLACED_COUNT=0
  PYTHON_CURRENT_SHELL_TAKEOVER_POSSIBLE=1

  while [ "$pass" -lt "$max_passes" ]; do
    detect_python_alias
    [ "$PYTHON_ALIAS_FOUND" -eq 1 ] || break

    if ! python_alias_targets_rewritable_helper; then
      PYTHON_CURRENT_SHELL_TAKEOVER_POSSIBLE=0
    fi

    if ! displace_single_python_alias; then
      warn "Failed to disable one of the detected 'am' alias/function entries."
      break
    fi

    displaced=$((displaced + 1))
    pass=$((pass + 1))
  done

  PYTHON_ALIAS_DISPLACED_COUNT="$displaced"

  detect_python_alias
  if [ "$PYTHON_ALIAS_FOUND" -eq 1 ]; then
    warn "Could not fully disable all 'am' alias/function definitions."
    warn "Remaining entry: ${PYTHON_ALIAS_FILE}:${PYTHON_ALIAS_LINE}"
    return 1
  fi

  if [ "$PYTHON_CURRENT_SHELL_TAKEOVER_POSSIBLE" -eq 1 ] && \
     { [ "$PYTHON_CLONE_FOUND" -ne 1 ] || [ -z "${PYTHON_CLONE_PATH:-}" ]; }; then
    PYTHON_CURRENT_SHELL_TAKEOVER_POSSIBLE=0
  fi

  if [ "$PYTHON_ALIAS_DISPLACED_COUNT" -gt 0 ] && [ "$PYTHON_CURRENT_SHELL_TAKEOVER_POSSIBLE" -eq 0 ]; then
    warn "If this shell already loaded an old 'am' alias/function, clear it now:"
    warn "  unalias am 2>/dev/null || true"
    warn "  unset -f am 2>/dev/null || true"
    warn "  hash -r 2>/dev/null || true"
  fi

  return 0
}

# Displace a legacy am launcher binary/script that appears earlier in PATH
# than the freshly installed Rust binary. This is especially important when a
# Python virtualenv prepends its own `am` script.
displace_python_binary() {
  local candidates=()
  local seen=""
  local displaced_count=0

  if [ "$PYTHON_BINARY_FOUND" -eq 1 ] && [ -n "$PYTHON_BINARY_PATH" ]; then
    case "$PYTHON_BINARY_PATH" in
      python\ *|python3\ *|*"-m mcp_agent_mail"*)
        verbose "displace_python_binary:skip non-file launcher=${PYTHON_BINARY_PATH}"
        ;;
      *)
        candidates+=("$PYTHON_BINARY_PATH")
        ;;
    esac
  fi
  if [ -n "${PYTHON_VENV_PATH:-}" ]; then
    candidates+=("$PYTHON_VENV_PATH/bin/am")
  fi
  if [ "$PYTHON_CLONE_FOUND" -eq 1 ] && [ -n "${PYTHON_CLONE_PATH:-}" ]; then
    candidates+=("$PYTHON_CLONE_PATH/.venv/bin/am")
    candidates+=("$PYTHON_CLONE_PATH/venv/bin/am")
  fi

  local bin_path
  for bin_path in "${candidates[@]}"; do
    [ -n "$bin_path" ] || continue
    case "$seen" in
      *"|$bin_path|"*) continue;;
    esac
    seen="${seen}|${bin_path}|"

    [ "$bin_path" = "$DEST/$BIN_CLI" ] && continue
    [ "$bin_path" = "$DEST/am" ] && continue
    [ -e "$bin_path" ] || continue

    local bin_dir
    bin_dir=$(dirname "$bin_path")
    if [ ! -w "$bin_dir" ] || [ ! -w "$bin_path" ]; then
      warn "Cannot displace legacy am launcher (not writable): $bin_path"
      return 1
    fi

    local timestamp backup tmpfile
    timestamp=$(date +%Y%m%d_%H%M%S)
    backup="${bin_path}.bak.mcp-agent-mail-${timestamp}-${RANDOM}"
    if ! cp -p "$bin_path" "$backup"; then
      warn "Failed to backup legacy am launcher before displacement: $bin_path"
      return 1
    fi

    tmpfile="${bin_path}.tmp.mcp-agent-mail.$$"
    cat > "$tmpfile" <<EOF
#!/usr/bin/env bash
exec "$DEST/$BIN_CLI" "\$@"
EOF
    chmod 0755 "$tmpfile"
    if ! mv "$tmpfile" "$bin_path"; then
      warn "Failed to replace legacy am launcher at: $bin_path"
      rm -f "$tmpfile" 2>/dev/null || true
      return 1
    fi

    ok "Legacy am launcher displaced at $bin_path"
    ok "Backup at $backup"
    displaced_count=$((displaced_count + 1))
  done

  if [ "$displaced_count" -eq 0 ]; then
    verbose "displace_python_binary:no_displacements"
  fi
}

install_legacy_launcher_takeover_shims() {
  LEGACY_LAUNCHER_SHIM_COUNT=0

  [ "$PYTHON_CLONE_FOUND" -eq 1 ] || return 0
  [ -n "${PYTHON_CLONE_PATH:-}" ] || return 0

  local helper_path="${PYTHON_CLONE_PATH}/scripts/run_server_with_token.sh"
  local helper_dir
  helper_dir=$(dirname "$helper_path")

  if [ ! -d "$helper_dir" ]; then
    if ! mkdir -p "$helper_dir"; then
      warn "Failed to create legacy helper directory for current-shell takeover: $helper_dir"
      return 1
    fi
  fi

  if [ -e "$helper_path" ] && [ ! -w "$helper_path" ]; then
    warn "Cannot replace legacy helper launcher (not writable): $helper_path"
    return 1
  fi
  if [ ! -w "$helper_dir" ]; then
    warn "Cannot write legacy helper launcher directory: $helper_dir"
    return 1
  fi

  local timestamp
  timestamp=$(date +%Y%m%d_%H%M%S)
  local backup=""
  if [ -f "$helper_path" ]; then
    backup="${helper_path}.bak.mcp-agent-mail-${timestamp}-${RANDOM}"
    if ! cp -p "$helper_path" "$backup"; then
      warn "Failed to backup legacy helper launcher before takeover: $helper_path"
      return 1
    fi
    info "Backed up legacy helper $helper_path -> $backup"
  fi

  local tmpfile="${helper_path}.tmp.mcp-agent-mail.$$"
cat > "$tmpfile" <<EOF
#!/usr/bin/env bash
set -euo pipefail

AM_RUST_BIN="${DEST}/${BIN_CLI}"
AM_RUST_ENV_FILE_DEFAULT="${HOME}/.config/mcp-agent-mail/config.env"
AM_RUST_ENV_FILE="\${AM_RUST_ENV_FILE:-\$AM_RUST_ENV_FILE_DEFAULT}"
if [ ! -f "\$AM_RUST_ENV_FILE" ] && [ -f "\${HOME}/.config/mcp-agent-mail/config.env" ]; then
  AM_RUST_ENV_FILE="\${HOME}/.config/mcp-agent-mail/config.env"
fi
if [ ! -f "\$AM_RUST_ENV_FILE" ] && [ -f "\${HOME}/.config/mcp-agent-mail/.env" ]; then
  AM_RUST_ENV_FILE="\${HOME}/.config/mcp-agent-mail/.env"
fi

trim_ascii_whitespace() {
  local value="\${1:-}"
  value="\${value#\"\${value%%[![:space:]]*}\"}"
  value="\${value%\"\${value##*[![:space:]]}\"}"
  printf '%s\n' "\$value"
}

load_env_key() {
  local key="\$1"
  [ -f "\$AM_RUST_ENV_FILE" ] || return 0

  local raw
  raw=\$(grep -E "^[[:space:]]*(export[[:space:]]+)?\${key}[[:space:]]*=" "\$AM_RUST_ENV_FILE" 2>/dev/null | tail -1 | sed -E "s/^[[:space:]]*(export[[:space:]]+)?\${key}[[:space:]]*=[[:space:]]*//" || true)
  [ -n "\$raw" ] || return 0

  raw=\$(trim_ascii_whitespace "\$raw")
  local parsed="" quote="" prev="" char=""
  local raw_len=\${#raw}
  local i=0
  while [ "\$i" -lt "\$raw_len" ]; do
    char="\${raw:\$i:1}"
    if [ -n "\$quote" ]; then
      parsed="\${parsed}\${char}"
      if [ "\$char" = "\$quote" ]; then
        quote=""
      fi
    else
      if [ "\$char" = '"' ] || [ "\$char" = "'" ]; then
        quote="\$char"
        parsed="\${parsed}\${char}"
      elif [ "\$char" = "#" ]; then
        if [ -z "\$prev" ] || [[ "\$prev" =~ [[:space:]] ]]; then
          break
        fi
        parsed="\${parsed}\${char}"
      else
        parsed="\${parsed}\${char}"
      fi
    fi
    prev="\$char"
    i=\$((i + 1))
  done

  raw=\$(trim_ascii_whitespace "\$parsed")
  raw="\${raw%\"}"
  raw="\${raw#\"}"
  raw="\${raw%\\'}"
  raw="\${raw#\\'}"
  export "\${key}=\${raw}"
}

for key in DATABASE_URL STORAGE_ROOT HTTP_HOST HTTP_PORT HTTP_PATH HTTP_BEARER_TOKEN TUI_ENABLED LLM_ENABLED LLM_DEFAULT_MODEL WORKTREES_ENABLED; do
  load_env_key "\$key"
done

if [ ! -x "\$AM_RUST_BIN" ]; then
  echo "mcp-agent-mail Rust CLI not found at \$AM_RUST_BIN" >&2
  exit 1
fi

exec "\$AM_RUST_BIN" "\$@"
EOF
  chmod 0755 "$tmpfile"
  if ! mv "$tmpfile" "$helper_path"; then
    warn "Failed to install legacy helper takeover shim at: $helper_path"
    rm -f "$tmpfile" 2>/dev/null || true
    return 1
  fi

  ok "Legacy helper now hands off to Rust at $helper_path"
  [ -n "$backup" ] && ok "Backup at $backup"
  LEGACY_LAUNCHER_SHIM_COUNT=1
  return 0
}

# T1.5: Stop running Python server processes
stop_python_server() {
  # Stop any Python systemd user service for mcp_agent_mail first
  # (cron-launched or systemd-managed Python servers will respawn if not disabled)
  local py_service_names=("mcp-agent-mail-python" "mcp_agent_mail" "agent-mail-python")
  for svc in "${py_service_names[@]}"; do
    if systemctl --user is-active "$svc" &>/dev/null 2>&1; then
      info "Stopping Python systemd service: $svc"
      systemctl --user stop "$svc" 2>/dev/null || true
      systemctl --user disable "$svc" 2>/dev/null || true
    fi
  done

  # Also remove any crontab entries that start the Python server
  if command -v crontab &>/dev/null; then
    local cron_before cron_after
    cron_before=$(crontab -l 2>/dev/null || true)
    if echo "$cron_before" | grep -qE "mcp_agent_mail.*serve|run_server_with_token"; then
      cron_after=$(echo "$cron_before" | grep -vE "mcp_agent_mail.*serve|run_server_with_token")
      echo "$cron_after" | crontab - 2>/dev/null || true
      ok "Removed Python mcp_agent_mail crontab entries"
    fi
  fi

  # Kill all Python mcp_agent_mail processes, not just the single detected PID
  local killed_any=0
  local all_py_pids
  all_py_pids=$(pgrep -f "mcp_agent_mail|mcp.agent.mail" 2>/dev/null || true)
  if [ -n "$all_py_pids" ]; then
    while IFS= read -r pid; do
      [ -z "$pid" ] && continue
      local cmdline
      cmdline=$(ps -p "$pid" -o command= 2>/dev/null || true)
      if echo "$cmdline" | grep -qiE "python|uvicorn"; then
        info "Stopping Python mcp-agent-mail process (PID $pid)"
        kill "$pid" 2>/dev/null || true
        killed_any=1
      fi
    done <<< "$all_py_pids"

    if [ "$killed_any" -eq 1 ]; then
      # Wait up to 5 seconds for graceful shutdown
      local waited=0
      while [ "$waited" -lt 5 ]; do
        local still_running=0
        while IFS= read -r pid; do
          [ -z "$pid" ] && continue
          local cmdline
          cmdline=$(ps -p "$pid" -o command= 2>/dev/null || true)
          if echo "$cmdline" | grep -qiE "python|uvicorn" && kill -0 "$pid" 2>/dev/null; then
            still_running=1
            break
          fi
        done <<< "$all_py_pids"
        [ "$still_running" -eq 0 ] && break
        sleep 1
        waited=$((waited + 1))
      done

      # Force-kill any survivors
      while IFS= read -r pid; do
        [ -z "$pid" ] && continue
        if kill -0 "$pid" 2>/dev/null; then
          local cmdline
          cmdline=$(ps -p "$pid" -o command= 2>/dev/null || true)
          if echo "$cmdline" | grep -qiE "python|uvicorn"; then
            warn "Force-killing Python server (PID $pid)"
            kill -9 "$pid" 2>/dev/null || true
          fi
        fi
      done <<< "$all_py_pids"
    fi
  fi

  # Also handle the single detected PID if it wasn't caught above
  if [ -n "$PYTHON_PID" ] && kill -0 "$PYTHON_PID" 2>/dev/null; then
    info "Stopping Python mcp-agent-mail server (PID $PYTHON_PID)"
    kill "$PYTHON_PID" 2>/dev/null || true
    killed_any=1
    local waited=0
    while [ "$waited" -lt 5 ] && kill -0 "$PYTHON_PID" 2>/dev/null; do
      sleep 1
      waited=$((waited + 1))
    done
    if kill -0 "$PYTHON_PID" 2>/dev/null; then
      warn "Python server did not stop gracefully; sending SIGKILL"
      kill -9 "$PYTHON_PID" 2>/dev/null || true
      sleep 1
    fi
  fi

  # Verify port 8765 is free (only check for Python processes, never kill Rust)
  if command -v ss &>/dev/null; then
    local port_holder
    port_holder=$(ss -tlnp 2>/dev/null | grep ":8765 " || true)
    if echo "$port_holder" | grep -qiE "python|uvicorn"; then
      local holder_pid
      holder_pid=$(echo "$port_holder" | grep -oE 'pid=[0-9]+' | head -1 | cut -d= -f2)
      if [ -n "$holder_pid" ]; then
        warn "Port 8765 still held by Python process (PID $holder_pid); force-killing"
        kill -9 "$holder_pid" 2>/dev/null || true
        killed_any=1
        sleep 1
      fi
    fi
  fi

  if [ "${killed_any:-0}" -eq 1 ]; then
    ok "Python server stopped"
  else
    verbose "stop_python_server:no_python_processes_found"
  fi
}

# T5.2: Resolve database path differences between Python and Rust
# Python stores DB at clone_dir/storage.sqlite3 (via cd in alias)
# Rust resolves via DATABASE_URL (default: ./storage.sqlite3 relative to CWD)
# or STORAGE_ROOT (default: ~/.mcp_agent_mail_git_mailbox_repo)
resolve_database_path() {
  PYTHON_DB_FOUND=0
  PYTHON_DB_PATH=""
  PYTHON_DB_FORMAT=""
  PYTHON_DB_PROBE_OUTPUT=""
  RUST_STORAGE_ROOT="${STORAGE_ROOT:-$HOME/.mcp_agent_mail_git_mailbox_repo}"
  RUST_DB_PATH=""

  # If a Rust config already exists, prefer its DB/storage target so import
  # lands where `am` will actually read after installation.
  local rust_env="$HOME/.config/mcp-agent-mail/config.env"
  if [ -f "$rust_env" ]; then
    local cfg_db_url cfg_db_path cfg_storage_root
    cfg_db_url=$(read_env_assignment_value "$rust_env" "DATABASE_URL")
    if [ -n "$cfg_db_url" ]; then
      cfg_db_path=$(echo "$cfg_db_url" | sed -n 's|^sqlite[^:]*:///||p')
      cfg_db_path="${cfg_db_path/#\~/$HOME}"
      if [ -n "$cfg_db_path" ] && [ "$cfg_db_path" != ":memory:" ] && [ "$cfg_db_path" != "/:memory:" ]; then
        case "$cfg_db_path" in
          /*) RUST_DB_PATH="$cfg_db_path";;
        esac
      fi
    fi
    if [ -z "$RUST_DB_PATH" ]; then
      cfg_storage_root=$(read_env_assignment_value "$rust_env" "STORAGE_ROOT")
      if [ -n "$cfg_storage_root" ]; then
        cfg_storage_root="${cfg_storage_root/#\~/$HOME}"
        case "$cfg_storage_root" in
          /*) RUST_STORAGE_ROOT="$cfg_storage_root";;
        esac
      fi
    fi
  fi
  [ -z "$RUST_DB_PATH" ] && RUST_DB_PATH="$RUST_STORAGE_ROOT/storage.sqlite3"
  RUST_STORAGE_ROOT="$(dirname "$RUST_DB_PATH")"

  # Candidate paths where Python might have stored the database
  local candidates=()

  # 1. Check the Python clone directory (most common)
  if [ "$PYTHON_CLONE_FOUND" -eq 1 ] && [ -n "$PYTHON_CLONE_PATH" ]; then
    candidates+=("$PYTHON_CLONE_PATH/storage.sqlite3")
    candidates+=("$PYTHON_CLONE_PATH/db/storage.sqlite3")
  fi

  # 2. Common Python default locations
  candidates+=(
    "$HOME/mcp_agent_mail/storage.sqlite3"
    "$HOME/mcp-agent-mail/storage.sqlite3"
    "$HOME/projects/mcp_agent_mail/storage.sqlite3"
    "$HOME/code/mcp_agent_mail/storage.sqlite3"
  )

  # 3. Check CWD (Python might have been started from a project dir)
  candidates+=("./storage.sqlite3")

  # 4. Extract path from DATABASE_URL env var if set
  if [ -n "${DATABASE_URL:-}" ]; then
    local url_path
    # Strip protocol prefix: sqlite+aiosqlite:///./path -> ./path
    url_path=$(echo "$DATABASE_URL" | sed -n 's|^sqlite[^:]*:///||p')
    [ -n "$url_path" ] && candidates+=("$url_path")
  fi

  # 5. Check .env files in common locations for DATABASE_URL
  local env_files=(
    "$HOME/mcp_agent_mail/.env"
    "$HOME/mcp-agent-mail/.env"
    "$HOME/.mcp_agent_mail/.env"
    "$HOME/.env"
  )
  [ "$PYTHON_CLONE_FOUND" -eq 1 ] && [ -n "$PYTHON_CLONE_PATH" ] && env_files+=("$PYTHON_CLONE_PATH/.env")

  for env_file in "${env_files[@]}"; do
    if [ -f "$env_file" ]; then
      local db_url
      db_url=$(read_env_assignment_value "$env_file" "DATABASE_URL")
      if [ -n "$db_url" ]; then
        local env_path
        env_path=$(echo "$db_url" | sed -n 's|^sqlite[^:]*:///||p')
        [ -n "$env_path" ] && candidates+=("$env_path")
      fi
    fi
  done

  # Deduplicate and check each candidate
  local seen=""
  for candidate in "${candidates[@]}"; do
    # Expand ~ if present
    candidate="${candidate/#\~/$HOME}"
    # Skip if already checked
    case "$seen" in
      *"|$candidate|"*) continue;;
    esac
    seen="${seen}|${candidate}|"

    if [ -f "$candidate" ] && [ -s "$candidate" ]; then
      # Verify it's actually a SQLite file
      local magic
      magic=$(head -c 16 "$candidate" 2>/dev/null | strings 2>/dev/null | head -1)
      if echo "$magic" | grep -q "SQLite format"; then
        if ! probe_database_format_with_installed_am "$candidate"; then
          warn "Skipping automatic database import from $candidate because the installed Rust CLI could not determine its timestamp format safely."
          continue
        fi
        case "$PYTHON_DB_FORMAT" in
          TEXT\ timestamps\ \(*|mixed\ format\ \(*|i64\ microseconds\ \(*)
            PYTHON_DB_FOUND=1
            PYTHON_DB_PATH="$candidate"
            break
            ;;
          empty\ database\ \(*)
            verbose "resolve_database_path:skip_non_migratable candidate=${candidate} format=${PYTHON_DB_FORMAT}"
            ;;
          *)
            warn "Skipping automatic database import from $candidate because the detected format is '${PYTHON_DB_FORMAT}'."
            ;;
        esac
      fi
    fi
  done

  if [ "$PYTHON_DB_FOUND" -eq 0 ]; then
    if [ "$PYTHON_DETECTED" -eq 1 ]; then
      info "No legacy Python database snapshot found for automatic takeover"
    fi
    return 0
  fi

  info "Found Python database at: $PYTHON_DB_PATH"
  info "Detected database format: $PYTHON_DB_FORMAT"

  # Determine if the DB is already in the Rust storage root
  local rust_db="$RUST_DB_PATH"
  local abs_python_db
  abs_python_db=$(cd "$(dirname "$PYTHON_DB_PATH")" 2>/dev/null && echo "$(pwd)/$(basename "$PYTHON_DB_PATH")")
  local abs_rust_db
  abs_rust_db=$(cd "$(dirname "$rust_db")" 2>/dev/null && echo "$(pwd)/$(basename "$rust_db")" 2>/dev/null || echo "$rust_db")

  if [ "$abs_python_db" = "$abs_rust_db" ]; then
    if python_db_format_needs_import "$PYTHON_DB_FORMAT"; then
      info "Legacy Python database is already at the Rust storage location"
      export DATABASE_URL="sqlite+aiosqlite:///$rust_db"
      PYTHON_DB_MIGRATED_PATH="$rust_db"
    else
      info "Database at the Rust storage location does not require migration"
    fi
    return 0
  fi

  # Copy the Python DB to the Rust storage root (don't move — safer)
  mkdir -p "$RUST_STORAGE_ROOT"

  if [ -f "$rust_db" ] && [ -s "$rust_db" ]; then
    # CRITICAL: Before overwriting the Rust DB, check if it is already in i64
    # (integer) timestamp format. If so, the migration was already done on a
    # previous run and we must NOT overwrite the live Rust database with the
    # stale Python snapshot — that would lose all data added since migration.
    local rust_db_format_check=""
    local _saved_python_db_format="$PYTHON_DB_FORMAT"
    if probe_database_format_with_sqlite "$rust_db" 2>/dev/null; then
      rust_db_format_check="$PYTHON_DB_FORMAT"
    elif probe_database_format_with_installed_am "$rust_db" 2>/dev/null; then
      rust_db_format_check="$PYTHON_DB_FORMAT"
    fi
    # Restore PYTHON_DB_FORMAT — the probes above clobber this global
    PYTHON_DB_FORMAT="$_saved_python_db_format"
    verbose "resolve_database_path:rust_db_format format='${rust_db_format_check}'"

    case "$rust_db_format_check" in
      i64\ microseconds\ *)
        info "Rust database already contains i64 timestamps — migration was already completed."
        info "Skipping Python database import to preserve existing Rust data."
        # Write the migration marker if it doesn't exist yet (retroactive)
        if [ ! -f "${PYTHON_MIGRATION_MARKER:-$HOME/.config/mcp-agent-mail/.python-migration-complete}" ]; then
          local _marker="${PYTHON_MIGRATION_MARKER:-$HOME/.config/mcp-agent-mail/.python-migration-complete}"
          mkdir -p "$(dirname "$_marker")" 2>/dev/null || true
          printf 'migrated_at=%s\nrust_db=%s\nnote=retroactive marker from resolve_database_path\n' \
            "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$rust_db" \
            > "$_marker" 2>/dev/null || true
        fi
        return 0
        ;;
      TEXT\ timestamps\ *|mixed\ format\ *)
        # Rust DB has TEXT timestamps — it needs migration, proceed with overwrite
        verbose "resolve_database_path:rust_db_needs_migration format='${rust_db_format_check}'"
        ;;
      "")
        # Could not determine Rust DB format (corrupt or locked).
        # SAFE DEFAULT: do NOT overwrite a database we can't read — it may contain
        # valuable data that just needs repair. Let `am doctor` handle recovery.
        warn "Cannot determine format of existing Rust database at $rust_db"
        warn "Skipping Python database import to avoid overwriting potentially recoverable data."
        warn "Run 'am doctor repair' or 'am doctor reconstruct' to fix the database."
        return 0
        ;;
      *)
        # Unknown format — also don't overwrite
        warn "Unexpected Rust database format: ${rust_db_format_check}"
        warn "Skipping Python database import to preserve existing data."
        return 0
        ;;
    esac

    # Check disk space before creating backup + copy (need ~3x the DB size)
    local python_db_size_kb=0
    local rust_db_size_kb=0
    if command -v du >/dev/null 2>&1; then
      python_db_size_kb=$(du -k "$PYTHON_DB_PATH" 2>/dev/null | awk '{print $1}')
      rust_db_size_kb=$(du -k "$rust_db" 2>/dev/null | awk '{print $1}')
    fi
    local needed_kb=$(( (python_db_size_kb + rust_db_size_kb) * 3 ))
    if [ "$needed_kb" -gt 0 ] && command -v df >/dev/null 2>&1; then
      local avail_kb
      avail_kb=$(df -Pk "$RUST_STORAGE_ROOT" 2>/dev/null | awk 'NR==2 {print $4}')
      if [ -n "$avail_kb" ] && [ "$avail_kb" -lt "$needed_kb" ]; then
        local needed_mb=$(( needed_kb / 1024 ))
        local avail_mb=$(( avail_kb / 1024 ))
        warn "Insufficient disk space for database migration."
        warn "  Need ~${needed_mb}MB, have ${avail_mb}MB free."
        warn "  Free disk space and re-run the installer."
        warn "Skipping database import to avoid filling disk."
        return 0
      fi
    fi

    # Cap backup count: don't create more than 3 pre-python-import backups
    local existing_import_backups=0
    existing_import_backups=$(count_matching_backup_files "${rust_db}.pre-python-import-")
    if [ "$existing_import_backups" -ge 3 ]; then
      warn "Already ${existing_import_backups} pre-python-import backups exist. Skipping backup creation."
      warn "Clean old backups with: rm ${rust_db}.pre-python-import-*"
    else
      local rust_backup_ts rust_backup
      rust_backup_ts=$(date -u +%Y%m%dT%H%M%SZ)
      rust_backup="${rust_db}.pre-python-import-${rust_backup_ts}"
      copy_sqlite_snapshot "$rust_db" "$rust_backup"
      ok "Backed up existing Rust database to $rust_backup"
    fi

    copy_sqlite_snapshot "$PYTHON_DB_PATH" "$rust_db"
    ok "Replaced Rust database with Python snapshot at $rust_db"
    export DATABASE_URL="sqlite+aiosqlite:///$rust_db"
    PYTHON_DB_MIGRATED_PATH="$rust_db"
    return 0
  fi

  # Check disk space before copying (need ~2x the Python DB size)
  local python_db_size_kb=0
  if command -v du >/dev/null 2>&1; then
    python_db_size_kb=$(du -k "$PYTHON_DB_PATH" 2>/dev/null | awk '{print $1}')
  fi
  local needed_kb=$(( python_db_size_kb * 2 ))
  if [ "$needed_kb" -gt 0 ] && command -v df >/dev/null 2>&1; then
    local avail_kb
    avail_kb=$(df -Pk "$RUST_STORAGE_ROOT" 2>/dev/null | awk 'NR==2 {print $4}')
    if [ -n "$avail_kb" ] && [ "$avail_kb" -lt "$needed_kb" ]; then
      local needed_mb=$(( needed_kb / 1024 ))
      local avail_mb=$(( avail_kb / 1024 ))
      warn "Insufficient disk space for database copy (need ~${needed_mb}MB, have ${avail_mb}MB)."
      warn "Skipping database import."
      return 0
    fi
  fi

  copy_sqlite_snapshot "$PYTHON_DB_PATH" "$rust_db"
  ok "Copied Python database to $rust_db"

  # Set DATABASE_URL so Rust binary finds it
  export DATABASE_URL="sqlite+aiosqlite:///$rust_db"
  PYTHON_DB_MIGRATED_PATH="$rust_db"
}

# T5.3: Migrate .env configuration from Python to Rust
# Python .env may live in clone dir or storage root. Rust reads the same
# env vars but DATABASE_URL format differs (no aiosqlite prefix).
migrate_env_config() {
  [ -z "${RUST_STORAGE_ROOT:-}" ] && RUST_STORAGE_ROOT="${STORAGE_ROOT:-$HOME/.mcp_agent_mail_git_mailbox_repo}"
  [ -z "${RUST_DB_PATH:-}" ] && RUST_DB_PATH="$RUST_STORAGE_ROOT/storage.sqlite3"

  # Find Python .env file
  local env_file=""
  local candidates=()
  [ "$PYTHON_CLONE_FOUND" -eq 1 ] && [ -n "$PYTHON_CLONE_PATH" ] && candidates+=("$PYTHON_CLONE_PATH/.env")
  candidates+=(
    "$HOME/mcp_agent_mail/.env"
    "$HOME/mcp-agent-mail/.env"
    "$HOME/.mcp_agent_mail/.env"
  )

  for f in "${candidates[@]}"; do
    if [ -f "$f" ]; then
      env_file="$f"
      break
    fi
  done

  # Rust config location
  local rust_config_dir="$HOME/.config/mcp-agent-mail"
  local rust_env="$rust_config_dir/config.env"
  local rust_env_compat="$rust_config_dir/.env"
  mkdir -p "$rust_config_dir"

  backup_envfile_if_present() {
    local path="$1"
    [ -f "$path" ] || return 0

    local timestamp backup
    timestamp=$(date +%Y%m%d_%H%M%S)
    backup="${path}.bak.mcp-agent-mail-${timestamp}-${RANDOM}"
    if ! cp -p "$path" "$backup"; then
      warn "Failed to back up existing Rust config before rewrite: $path"
      return 1
    fi
    info "Backed up $path -> $backup"
  }

  local source_env=""
  local updating_existing=0
  local legacy_http_bearer_token="${MIGRATED_BEARER_TOKEN:-}"
  if [ -f "$rust_env" ]; then
    source_env="$rust_env"
    updating_existing=1
  elif [ -f "$rust_env_compat" ]; then
    source_env="$rust_env_compat"
    updating_existing=1
  elif [ -n "$env_file" ]; then
    source_env="$env_file"
  fi

  if [ -n "$env_file" ]; then
    info "Found Python .env at: $env_file"
    if [ -z "$legacy_http_bearer_token" ]; then
      legacy_http_bearer_token=$(read_env_assignment_value "$env_file" "HTTP_BEARER_TOKEN")
    fi
  fi
  if [ -f "$rust_env" ] && [ -z "$legacy_http_bearer_token" ]; then
    legacy_http_bearer_token=$(read_env_assignment_value "$rust_env" "HTTP_BEARER_TOKEN")
  fi
  MIGRATED_BEARER_TOKEN="$legacy_http_bearer_token"

  if [ "$updating_existing" -eq 1 ]; then
    backup_envfile_if_present "$rust_env" || return 1
    if [ "$rust_env_compat" != "$rust_env" ]; then
      backup_envfile_if_present "$rust_env_compat" || return 1
    fi
    info "Updating Rust config at $rust_env to adopt legacy Python data paths"
  elif [ -n "$env_file" ]; then
    info "Migrating Python .env config into $rust_env"
  else
    info "Writing Rust config at $rust_env with adopted legacy data paths"
  fi

  # Vars that are compatible between Python and Rust
  local compat_vars="HTTP_HOST HTTP_PORT HTTP_PATH HTTP_BEARER_TOKEN STORAGE_ROOT DATABASE_URL TUI_ENABLED LLM_ENABLED LLM_DEFAULT_MODEL WORKTREES_ENABLED"
  # Python-only vars to skip
  local skip_pattern="^(SQLALCHEMY_|ALEMBIC_|UVICORN_|ASYNC_)"
  local seen_database_url=0
  local seen_storage_root=0
  local seen_http_bearer_token=0

  local tmpfile="${rust_env}.tmp.$$"
  {
    if [ "$updating_existing" -eq 1 ]; then
      echo "# Updated by Rust installer to adopt legacy Python data paths"
    elif [ -n "$env_file" ]; then
      echo "# Migrated from Python .env: $env_file"
    else
      echo "# Created by Rust installer during Python takeover"
    fi
    echo "# Update date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo ""

    while IFS= read -r line || [ -n "$line" ]; do
      # Skip comments and empty lines
      if printf '%s\n' "$line" | grep -qE '^[[:space:]]*(#|$)'; then
        echo "$line"
        continue
      fi

      local raw_key key val
      raw_key="${line%%=*}"
      key=$(printf '%s\n' "$raw_key" | sed -E 's/^[[:space:]]*(export[[:space:]]+)?//; s/[[:space:]]+$//')
      val="${line#*=}"

      # Skip Python-specific vars
      if echo "$key" | grep -qE "$skip_pattern"; then
        echo "# Skipped (Python-only): $line"
        continue
      fi

      # Transform DATABASE_URL: strip aiosqlite prefix, resolve path
      if [ "$key" = "DATABASE_URL" ]; then
        seen_database_url=1
        echo "DATABASE_URL=sqlite:///$RUST_DB_PATH"
        continue
      fi

      if [ "$key" = "STORAGE_ROOT" ]; then
        seen_storage_root=1
        echo "STORAGE_ROOT=$RUST_STORAGE_ROOT"
        continue
      fi

      if [ "$key" = "HTTP_BEARER_TOKEN" ]; then
        seen_http_bearer_token=1
        if [ -z "${legacy_http_bearer_token:-}" ]; then
          legacy_http_bearer_token=$(strip_wrapping_quotes "$val")
          MIGRATED_BEARER_TOKEN="$legacy_http_bearer_token"
        fi
        if [ -n "${legacy_http_bearer_token:-}" ]; then
          echo "HTTP_BEARER_TOKEN=$legacy_http_bearer_token"
        else
          echo "HTTP_BEARER_TOKEN=$val"
        fi
        continue
      fi

      # Pass through compatible vars as-is
      echo "$line"
    done < <(if [ -n "$source_env" ] && [ -f "$source_env" ]; then cat "$source_env"; fi)

    if [ "$seen_database_url" -eq 0 ]; then
      echo "DATABASE_URL=sqlite:///$RUST_DB_PATH"
    fi
    if [ "$seen_storage_root" -eq 0 ]; then
      echo "STORAGE_ROOT=$RUST_STORAGE_ROOT"
    fi
    if [ "$seen_http_bearer_token" -eq 0 ] && [ -n "${legacy_http_bearer_token:-}" ]; then
      echo "HTTP_BEARER_TOKEN=$legacy_http_bearer_token"
    fi
  } > "$tmpfile"

  if ! grep -qE '^[[:space:]]*HTTP_BEARER_TOKEN=' "$tmpfile" 2>/dev/null && [ -n "${legacy_http_bearer_token:-}" ]; then
    printf '\nHTTP_BEARER_TOKEN=%s\n' "$legacy_http_bearer_token" >> "$tmpfile"
  fi

  mv "$tmpfile" "$rust_env"
  chmod 600 "$rust_env"  # Restrict access (may contain tokens)
  local compat_tmp="${rust_env_compat}.tmp.$$"
  cp "$rust_env" "$compat_tmp"
  mv "$compat_tmp" "$rust_env_compat"
  chmod 600 "$rust_env_compat"
  if [ "$updating_existing" -eq 1 ]; then
    ok "Updated Rust config at $rust_env"
  else
    ok "Wrote Rust config to $rust_env"
  fi
  ok "Synced compatibility env mirror to $rust_env_compat"
}

resolve_migrated_bearer_token() {
  if [ -n "${MIGRATED_BEARER_TOKEN:-}" ]; then
    printf '%s' "$MIGRATED_BEARER_TOKEN"
    return 0
  fi

  local rust_env="$HOME/.config/mcp-agent-mail/config.env"
  if [ -f "$rust_env" ]; then
    local token
    token=$(read_env_assignment_value "$rust_env" "HTTP_BEARER_TOKEN")
    printf '%s' "$token"
    return 0
  fi

  printf ''
}

# T2.3: Atomic binary installation (crash-safe)
# Writes to a temp file, syncs, then renames atomically.
# Cleans up stale tmp files from previous failed installs.
atomic_install() {
  local src="$1"
  local dest="$2"
  local tmp_dest="${dest}.tmp.$$"

  # Clean up stale tmp files from previous failed installs
  for stale in "${dest}".tmp.*; do
    [ -f "$stale" ] && rm -f "$stale" 2>/dev/null
  done

  # Write to temp file
  install -m 0755 "$src" "$tmp_dest"

  # Sync to disk if available
  sync "$tmp_dest" 2>/dev/null || sync 2>/dev/null || true

  # Atomic rename
  mv -f "$tmp_dest" "$dest"
}

# ── End Python detection & displacement ────────────────────────────────────

preflight_checks() {
  info "Running preflight checks"
  check_disk_space
  check_write_permissions
  check_existing_install
  check_network
  check_git_version_known_bad
}

# ──────────────────────────────────────────────────────────────────────────
# check_git_version_known_bad — br-8ujfs.1.3 (A3)
#
# Warn (non-fatal) if the system git is a version known to have multi-agent
# concurrency bugs that corrupt .git/index. The primary motivating bug is
# git 2.51.0's cache_entry index-race (SIGSEGV at ip 0x1db250; see
# docs/GIT_251_FINDINGS.md).
#
# Detection honors AM_GIT_BINARY if the operator has already set it, so
# existing mitigations don't spuriously re-trigger this warning.
#
# Output: stderr warning only. NEVER aborts install. Emits a marker
# string (AM_INSTALL_WARN GIT_2_51_0_KNOWN_BAD) so tests + ops can grep
# for it reliably.
# ──────────────────────────────────────────────────────────────────────────
check_git_version_known_bad() {
  local git_bin="${AM_GIT_BINARY:-git}"
  local git_version=""
  if command -v "$git_bin" >/dev/null 2>&1; then
    git_version=$("$git_bin" --version 2>/dev/null | awk '{print $3}' | head -n1 || true)
  fi

  if [ -z "$git_version" ]; then
    verbose "check_git_version_known_bad: git not found on PATH — skipping"
    return 0
  fi
  verbose "check_git_version_known_bad: detected $git_bin = $git_version"

  # Currently only 2.51.0 is flagged. Additional versions are data-driven
  # at runtime via AM_EXTRA_KNOWN_BAD_GIT_JSON (see bead A7); keeping the
  # installer check narrow avoids coupling the installer to a runtime
  # catalog that isn't available until after install.
  #
  # The glob patterns below catch:
  #   - upstream       : "2.51.0"
  #   - Git for Windows: "2.51.0.windows.1" (and .2, .3, ...) via 2.51.0.*
  #   - distro suffix  : "2.51.0-1ubuntu1", "2.51.0-1debian1" via 2.51.0-*
  #   - build metadata : "2.51.0+git20260101" via 2.51.0+*
  # Any version string that STARTS with "2.51.0" plus a separator ('.',
  # '-', or '+') is flagged. Bare "2.51.0.rc1" and "2.51.0-rc1" also
  # match — pre-release or not, if you're shipping a 2.51.0 derivative
  # the race is presumed present until proven otherwise.
  case "$git_version" in
    2.51.0 | 2.51.0.* | 2.51.0-* | 2.51.0+*)
      warn "[AM_INSTALL_WARN GIT_2_51_0_KNOWN_BAD]"
      warn ""
      warn "Detected git 2.51.0 ($git_bin) which has a concurrency bug"
      warn "that corrupts .git/index under multi-agent load."
      warn "Symptoms: kernel-log segfaults, 'fatal: bad object HEAD',"
      warn "orphan stashes, ahead/behind counts showing -1."
      warn ""
      warn "Remediation (in order):"
      warn "  1. Set AM_GIT_BINARY=/path/to/git-2.50.x in your shell profile"
      warn "  2. Upgrade system git to >= 2.51.1 once it is released"
      warn "  3. Run 'am doctor fix-orphan-refs --all --dry-run' on damaged repos"
      warn ""
      warn "Details: RECOVERY_RUNBOOK.md#git-2-51-0-index-race"
      warn ""
      ;;
    *)
      verbose "check_git_version_known_bad: $git_version is not on the known-bad list"
      ;;
  esac
}

maybe_add_path() {
  verbose "maybe_add_path:start path=${PATH} dest=${DEST} easy=${EASY}"
  local dest_in_path=0
  local updated=0
  case ":$PATH:" in
    *:"$DEST":*) dest_in_path=1 ;;
  esac

  # Helper: idempotently add a PATH guard to a file (creates it if needed)
  _ensure_path_in_file() {
    local target="$1"
    local guard_line='[ -d "'"$DEST"'" ] && case ":$PATH:" in *:"'"$DEST"'":*) ;; *) export PATH="'"$DEST"':$PATH" ;; esac'
    # Check for the expanded path, $HOME form, and ~ form
    if [ -e "$target" ]; then
      local dest_home_form="${DEST/#$HOME/\$HOME}"
      local dest_tilde_form="${DEST/#$HOME/\~}"
      if grep -qF "$DEST" "$target" 2>/dev/null \
         || grep -qF "$dest_home_form" "$target" 2>/dev/null \
         || grep -qF "$dest_tilde_form" "$target" 2>/dev/null; then
        verbose "maybe_add_path:already_in ${target}"
        return 0
      fi
    fi
    # Check parent directory is writable (for file creation) and file is writable (if exists)
    local target_dir
    target_dir=$(dirname "$target")
    if [ -e "$target" ] && [ ! -w "$target" ]; then
      verbose "maybe_add_path:not_writable ${target}"
      return 0
    fi
    if [ ! -w "$target_dir" ]; then
      verbose "maybe_add_path:dir_not_writable ${target_dir}"
      return 0
    fi
    # Append with a blank line separator
    { [ ! -s "$target" ] || echo ""; echo "# Ensure $DEST is in PATH"; echo "$guard_line"; } >> "$target"
    verbose "maybe_add_path:appended guard to ${target}"
    return 1  # signal that we made a change
  }

  if [ "$EASY" -eq 1 ]; then
    # Interactive shell rc files (zsh, bash)
    local dest_home_form="${DEST/#$HOME/\$HOME}"
    local dest_tilde_form="${DEST/#$HOME/\~}"
    for rc in "$HOME/.zshrc" "$HOME/.bashrc"; do
      if [ -e "$rc" ] && [ -w "$rc" ]; then
        if ! grep -qF "$DEST" "$rc" 2>/dev/null \
           && ! grep -qF "$dest_home_form" "$rc" 2>/dev/null \
           && ! grep -qF "$dest_tilde_form" "$rc" 2>/dev/null; then
          echo "export PATH=\"$DEST:\$PATH\"" >> "$rc"
          verbose "maybe_add_path:appended rc=${rc} export PATH=\"$DEST:\$PATH\""
          updated=1
        fi
      fi
    done

    # Login/env files: .zshenv (ALL zsh instances), .profile (bash login shells)
    # .zshenv is critical because zsh login shells (zsh -l) do NOT source .zshrc
    for env_file in "$HOME/.zshenv" "$HOME/.profile"; do
      if _ensure_path_in_file "$env_file"; then
        : # already present
      else
        updated=1
      fi
    done

    if [ "$updated" -eq 1 ]; then
      warn "PATH updated in shell config files; restart shell to use Rust am/mcp-agent-mail"
      verbose "maybe_add_path:updated_shell_rc=1"
    elif [ "$dest_in_path" -eq 0 ]; then
      warn "Add $DEST to PATH to use am / mcp-agent-mail"
      verbose "maybe_add_path:updated_shell_rc=0"
    else
      verbose "maybe_add_path:path_already_configured"
    fi
  else
    if [ "$dest_in_path" -eq 0 ]; then
      warn "Add $DEST to PATH to use am / mcp-agent-mail"
      verbose "maybe_add_path:easy_mode_disabled_no_update"
    else
      verbose "maybe_add_path:path_present_no_easy_mode"
    fi
  fi
  verbose "maybe_add_path:done"
}

install_local_bin_link() {
  local binary_name="$1"
  local source_path="$DEST/$binary_name"
  local link_dir="${AM_LINK_DIR:-$HOME/.local/bin}"
  local link_path="$link_dir/$binary_name"

  [ -x "$source_path" ] || return 1
  [ "$source_path" = "$link_path" ] && return 0

  if ! mkdir -p "$link_dir"; then
    warn "Unable to create local bin directory for $binary_name: $link_dir"
    return 1
  fi
  if [ -e "$link_path" ] && [ ! -w "$link_path" ]; then
    warn "Unable to update PATH shim for $binary_name (not writable): $link_path"
    return 1
  fi
  if ! ln -sfn "$source_path" "$link_path"; then
    warn "Unable to link $link_path -> $source_path"
    return 1
  fi
  ok "Linked $link_path -> $source_path"
}

install_local_bin_links() {
  local issues=0
  install_local_bin_link "$BIN_CLI" || issues=$((issues + 1))
  install_local_bin_link "$BIN_SERVER" || issues=$((issues + 1))
  return "$issues"
}

detect_mac_direct_exec_compat_mode() {
  MAC_DIRECT_EXEC_COMPAT_MODE=0
  MAC_DIRECT_EXEC_COMPAT_REASON=""

  [ "${PLATFORM:-}" = "darwin" ] || return 1
  [ "$PYTHON_CLONE_FOUND" -eq 1 ] || return 1
  [ -n "${PYTHON_CLONE_PATH:-}" ] || return 1
  [ -x "$DEST/$BIN_CLI" ] || return 1

  local help_output=""
  if capture_command_with_timeout 3 "$DEST/$BIN_CLI" --help; then
    help_output="$CAPTURED_CMD_OUTPUT"
    if printf '%s\n' "$help_output" | grep -qE '(^|[[:space:]])serve-http([[:space:]]|$)'; then
      return 1
    fi
    MAC_DIRECT_EXEC_COMPAT_REASON="installed Rust CLI did not expose the expected command surface on this macOS host"
  else
    help_output="$CAPTURED_CMD_OUTPUT"
    if [ "$CAPTURED_CMD_STATUS" -eq 124 ]; then
      MAC_DIRECT_EXEC_COMPAT_REASON="direct execution of installed Rust binaries timed out on this macOS host"
    else
      MAC_DIRECT_EXEC_COMPAT_REASON="installed Rust CLI exited non-zero during a direct execution probe on this macOS host"
    fi
  fi

  MAC_DIRECT_EXEC_COMPAT_MODE=1
  verbose "mac_exec_compat:enabled reason=${MAC_DIRECT_EXEC_COMPAT_REASON}"
  return 0
}

install_mac_python_cli_compat_launcher() {
  [ "$MAC_DIRECT_EXEC_COMPAT_MODE" -eq 1 ] || return 0
  [ "$PYTHON_CLONE_FOUND" -eq 1 ] || return 1
  [ -n "${PYTHON_CLONE_PATH:-}" ] || return 1

  local compat_dir="$HOME/.config/mcp-agent-mail"
  local compat_launcher="${compat_dir}/am-python-compat.sh"
  local python_exec=""

  mkdir -p "$compat_dir"
  if [ -n "${PYTHON_VENV_PATH:-}" ] && [ -x "${PYTHON_VENV_PATH}/bin/python" ]; then
    python_exec="${PYTHON_VENV_PATH}/bin/python"
  elif [ -x "${PYTHON_CLONE_PATH}/.venv/bin/python" ]; then
    python_exec="${PYTHON_CLONE_PATH}/.venv/bin/python"
  elif command -v python3 >/dev/null 2>&1; then
    python_exec="$(command -v python3)"
  else
    warn "Python compatibility mode requested, but no python3 interpreter was found"
    return 1
  fi

  cat > "$compat_launcher" <<EOF
#!/usr/bin/env bash
set -euo pipefail
cd "${PYTHON_CLONE_PATH}"
exec "${python_exec}" -m mcp_agent_mail.cli "\$@"
EOF
  chmod 0644 "$compat_launcher"
  MAC_DIRECT_EXEC_COMPAT_LAUNCHER="$compat_launcher"
  verbose "mac_exec_compat:launcher path=${compat_launcher}"
}

install_mac_python_cli_shell_alias() {
  [ "$MAC_DIRECT_EXEC_COMPAT_MODE" -eq 1 ] || return 0
  [ -n "${MAC_DIRECT_EXEC_COMPAT_LAUNCHER:-}" ] || return 1

  local rc_files=("$HOME/.zshrc" "$HOME/.bashrc")
  local rc marker_start marker_end block timestamp backup
  marker_start="# >>> mcp-agent-mail mac exec compat >>>"
  marker_end="# <<< mcp-agent-mail mac exec compat <<<"
  block="${marker_start}
am() { /bin/bash \"${MAC_DIRECT_EXEC_COMPAT_LAUNCHER}\" \"\$@\"; }
${marker_end}"

  for rc in "${rc_files[@]}"; do
    if [ -f "$rc" ] && grep -Fq "$marker_start" "$rc" 2>/dev/null; then
      continue
    fi

    if [ -f "$rc" ]; then
      timestamp=$(date +%Y%m%d_%H%M%S)
      backup="${rc}.bak.mcp-agent-mail-${timestamp}-${RANDOM}"
      cp -p "$rc" "$backup"
      verbose "mac_exec_compat:rc_backup rc=${rc} backup=${backup}"
    fi

    {
      [ ! -f "$rc" ] || [ ! -s "$rc" ] || echo ""
      printf '%s\n' "$block"
    } >> "$rc"
    MAC_DIRECT_EXEC_COMPAT_REASON="${MAC_DIRECT_EXEC_COMPAT_REASON}; shell alias installed in ${rc}"
  done
}

activate_mac_python_cli_compat_shell() {
  [ "$MAC_DIRECT_EXEC_COMPAT_MODE" -eq 1 ] || return 0
  [ -t 0 ] || return 0

  local shell_bin="${SHELL:-/bin/zsh}"
  case "$(basename "$shell_bin")" in
    zsh|bash)
      warn "Launching a fresh interactive shell so 'am' resolves via the Python compatibility entrypoint on this Mac"
      exec "$shell_bin" -il
      ;;
    *)
      warn "Open a fresh shell to use the Python compatibility entrypoint for 'am' on this Mac"
      ;;
  esac
}

detect_mcp_configs() {
  local project_dir="${1:-$PWD}"
  local home_dir="${HOME:-}"
  local app_data_dir="${APPDATA:-}"
  local seen=""
  local entry
  local tool
  local path
  local key
  local exists_flag
  local -a candidates=()

  if [ -n "$home_dir" ]; then
    # Claude Desktop only. Claude Code does NOT read MCP server
    # registrations from ~/.claude/settings.json — those files are for
    # hooks/statusLine/env and the mcpServers key there is silently
    # ignored. Claude Code is configured via the `claude mcp add` CLI
    # (writes to ~/.claude.json), handled by setup_claude_code_mcp_via_cli
    # before the candidate scan. See #97.
    candidates+=("claude|${home_dir}/.claude/claude_desktop_config.json")
    candidates+=("claude|${home_dir}/.config/Claude/claude_desktop_config.json")
    candidates+=("claude|${home_dir}/Library/Application Support/Claude/claude_desktop_config.json")

    # Codex CLI
    candidates+=("codex|${home_dir}/.codex/config.toml")
    candidates+=("codex|${home_dir}/.codex/config.json")
    candidates+=("codex|${home_dir}/.config/codex/config.toml")

    # Cursor
    candidates+=("cursor|${home_dir}/.cursor/mcp.json")
    candidates+=("cursor|${home_dir}/.cursor/mcp_config.json")

    # Gemini CLI
    candidates+=("gemini|${home_dir}/.gemini/settings.json")
    candidates+=("gemini|${home_dir}/.gemini/mcp.json")

    # GitHub Copilot / VS Code settings
    candidates+=("github-copilot|${home_dir}/.config/Code/User/settings.json")
    candidates+=("github-copilot|${home_dir}/Library/Application Support/Code/User/settings.json")

    # Other supported tools
    candidates+=("windsurf|${home_dir}/.windsurf/mcp.json")
    candidates+=("cline|${home_dir}/.cline/mcp.json")
    candidates+=("opencode|${home_dir}/.opencode/opencode.json")
    candidates+=("factory|${home_dir}/.factory/mcp.json")
    candidates+=("factory|${home_dir}/.factory/settings.json")
  fi

  if [ -n "$app_data_dir" ]; then
    candidates+=("claude|${app_data_dir}/Claude/claude_desktop_config.json")
    candidates+=("github-copilot|${app_data_dir}/Code/User/settings.json")
  fi

  # Project-local config files.
  # Claude Code project-scope MCP is configured via `<project>/.mcp.json`, not
  # settings.json — see #97. We don't auto-write project-scope MCP from the
  # installer; users run `claude mcp add --scope project` themselves if they
  # want a project-pinned entry.
  candidates+=("codex|${project_dir}/.codex/config.toml")
  candidates+=("codex|${project_dir}/codex.mcp.json")
  candidates+=("cursor|${project_dir}/cursor.mcp.json")
  candidates+=("gemini|${project_dir}/gemini.mcp.json")
  candidates+=("github-copilot|${project_dir}/.vscode/mcp.json")
  candidates+=("windsurf|${project_dir}/windsurf.mcp.json")
  candidates+=("cline|${project_dir}/cline.mcp.json")
  candidates+=("opencode|${project_dir}/opencode.json")
  candidates+=("factory|${project_dir}/factory.mcp.json")

  for entry in "${candidates[@]}"; do
    tool="${entry%%|*}"
    path="${entry#*|}"
    key="${tool}|${path}"
    case "|${seen}|" in
      *"|${key}|"*) continue ;;
    esac
    seen="${seen}|${key}"

    if [ -e "$path" ]; then
      exists_flag=1
    else
      exists_flag=0
    fi
    printf '%s\t%s\t%s\n' "$tool" "$path" "$exists_flag"
  done
}

generate_bearer_token() {
  if command -v openssl >/dev/null 2>&1; then
    openssl rand -hex 32
  elif [ -r /dev/urandom ]; then
    head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n'
  else
    # Fallback: use date-based hash (weak but functional)
    printf '%s' "$(date +%s%N)$$" | sha256sum 2>/dev/null | cut -d' ' -f1 || echo "placeholder-token-replace-me"
  fi
}

normalize_mcp_http_path() {
  local value="${1:-/mcp/}"
  case "$value" in
    mcp|/mcp|/mcp/)
      printf '/mcp/'
      ;;
    api|/api|/api/)
      printf '/api/'
      ;;
    *)
      if [ -z "$value" ]; then
        value="/mcp/"
      fi
      case "$value" in
        /*) ;;
        *) value="/${value}" ;;
      esac
      case "$value" in
        */) ;;
        *) value="${value}/" ;;
      esac
      printf '%s' "$value"
      ;;
  esac
}

desired_mcp_http_url() {
  local host
  host="$(mcp_client_connect_host "${HTTP_HOST:-127.0.0.1}")"
  local port="${HTTP_PORT:-8765}"
  local path
  path="$(normalize_mcp_http_path "${HTTP_PATH:-/mcp/}")"
  printf 'http://%s:%s%s' "$host" "$port" "$path"
}

mcp_client_connect_host() {
  local host="${1:-127.0.0.1}"
  host="${host#"${host%%[![:space:]]*}"}"
  host="${host%"${host##*[![:space:]]}"}"

  if [ -z "$host" ]; then
    printf '127.0.0.1'
    return 0
  fi

  local unbracketed="$host"
  if [[ "$host" == \[*\] ]]; then
    unbracketed="${host#[}"
    unbracketed="${unbracketed%]}"
  fi

  case "$unbracketed" in
    0.0.0.0)
      printf '127.0.0.1'
      ;;
    ::)
      printf '[::1]'
      ;;
    *:*)
      if [[ "$host" == \[*\] ]]; then
        printf '%s' "$host"
      else
        printf '[%s]' "$unbracketed"
      fi
      ;;
    *)
      printf '%s' "$host"
      ;;
  esac
}

desired_mcp_http_base_url() {
  local host
  host="$(mcp_client_connect_host "${HTTP_HOST:-127.0.0.1}")"
  local port="${HTTP_PORT:-8765}"
  printf 'http://%s:%s' "$host" "$port"
}

desired_service_bind_host() {
  printf '%s' "${HTTP_HOST:-127.0.0.1}"
}

desired_service_bind_port() {
  printf '%s' "${HTTP_PORT:-8765}"
}

platform_supports_user_service_management() {
  case "${OS:-$(uname -s | tr 'A-Z' 'a-z')}" in
    linux|darwin) return 0 ;;
    *) return 1 ;;
  esac
}

has_remote_http_client_targets() {
  if command -v codex >/dev/null 2>&1 || [ -d "${HOME}/.codex" ] || [ -d "${HOME}/.config/codex" ]; then
    return 0
  fi

  local scan
  scan="$(detect_mcp_configs "$PWD" 2>/dev/null || true)"
  [ -z "$scan" ] && return 1

  local tool path exists_flag
  while IFS=$'\t' read -r tool path exists_flag; do
    [ -z "${tool:-}" ] && continue
    [ "$tool" = "codex" ] || continue
    if [ "$exists_flag" = "1" ] && [ -f "$path" ]; then
      return 0
    fi
  done <<< "$scan"

  return 1
}

probe_remote_http_endpoint() {
  REMOTE_HTTP_PROBE_DETAIL=""

  local base_url
  base_url="$(desired_mcp_http_base_url)"
  local bearer_token
  bearer_token="$(resolve_setup_http_bearer_token)"
  local curl_args=(--silent --show-error --connect-timeout 1 --max-time 4)
  if [ -n "$bearer_token" ]; then
    curl_args+=(-H "Authorization: Bearer ${bearer_token}")
  fi

  local health_url
  for health_url in "${base_url}/health/readiness" "${base_url}/health"; do
    local health_body=""
    if health_body=$(curl "${curl_args[@]}" "$health_url" 2>/dev/null); then
      if printf '%s' "$health_body" | grep -Eq '"status"[[:space:]]*:[[:space:]]*"(ok|ready)"'; then
        REMOTE_HTTP_PROBE_DETAIL="healthy via ${health_url}"
        return 0
      fi
      REMOTE_HTTP_PROBE_DETAIL="unexpected health payload from ${health_url}"
    else
      REMOTE_HTTP_PROBE_DETAIL="could not reach ${health_url}"
    fi
  done

  if curl "${curl_args[@]}" "${base_url}/healthz" >/dev/null 2>&1; then
    REMOTE_HTTP_PROBE_DETAIL="${REMOTE_HTTP_PROBE_DETAIL}; liveness endpoint responds but readiness is not healthy"
  fi

  return 1
}

wait_for_remote_http_endpoint() {
  local max_attempts="${1:-20}"
  local attempt=1

  while [ "$attempt" -le "$max_attempts" ]; do
    if probe_remote_http_endpoint; then
      return 0
    fi
    sleep 0.5
    attempt=$((attempt + 1))
  done

  return 1
}

repair_launchd_service_env_from_rust_config() {
  [ "${OS:-$(uname -s | tr 'A-Z' 'a-z')}" = "darwin" ] || return 0

  local plist_path="$HOME/Library/LaunchAgents/com.agent-mail.plist"
  [ -f "$plist_path" ] || return 0

  if ! command -v python3 >/dev/null 2>&1; then
    warn "python3 not found; cannot patch LaunchAgent environment for config.env compatibility."
    return 0
  fi

  local rust_env="$HOME/.config/mcp-agent-mail/config.env"
  local storage_root database_url bearer_token host port http_path
  storage_root="${RUST_STORAGE_ROOT:-}"
  [ -z "$storage_root" ] && storage_root=$(read_env_assignment_value "$rust_env" "STORAGE_ROOT")
  [ -z "$storage_root" ] && storage_root="$HOME/.mcp_agent_mail_git_mailbox_repo"

  database_url=$(read_env_assignment_value "$rust_env" "DATABASE_URL")
  [ -z "$database_url" ] && database_url="sqlite:///$storage_root/storage.sqlite3"

  bearer_token=$(read_env_assignment_value "$rust_env" "HTTP_BEARER_TOKEN")
  host=$(read_env_assignment_value "$rust_env" "HTTP_HOST")
  [ -z "$host" ] && host="$(desired_service_bind_host)"
  port=$(read_env_assignment_value "$rust_env" "HTTP_PORT")
  [ -z "$port" ] && port="$(desired_service_bind_port)"
  http_path=$(read_env_assignment_value "$rust_env" "HTTP_PATH")
  [ -z "$http_path" ] && http_path="${HTTP_PATH:-/mcp/}"

  if ! python3 - "$plist_path" "$HOME" "$storage_root" "$database_url" "$bearer_token" "$host" "$port" "$http_path" <<'PY'
import plistlib
import sys

plist_path, home, storage_root, database_url, bearer_token, host, port, http_path = sys.argv[1:]

with open(plist_path, "rb") as fh:
    data = plistlib.load(fh)

env = dict(data.get("EnvironmentVariables") or {})
env["RUST_LOG"] = env.get("RUST_LOG", "info")
env["HOME"] = home

for key, value in (
    ("STORAGE_ROOT", storage_root),
    ("DATABASE_URL", database_url),
    ("HTTP_BEARER_TOKEN", bearer_token),
    ("HTTP_HOST", host),
    ("HTTP_PORT", port),
    ("HTTP_PATH", http_path),
):
    if value:
        env[key] = value
    else:
        env.pop(key, None)

data["EnvironmentVariables"] = env
data["WorkingDirectory"] = storage_root or home

with open(plist_path, "wb") as fh:
    plistlib.dump(data, fh)
PY
  then
    warn "Failed to inject Rust config environment into LaunchAgent plist."
    return 0
  fi

  local uid
  uid="$(id -u)"
  launchctl bootout "gui/${uid}" "$plist_path" >/dev/null 2>&1 || true
  if ! launchctl bootstrap "gui/${uid}" "$plist_path" >/dev/null 2>&1; then
    warn "LaunchAgent plist was updated, but launchctl could not restart it automatically."
    return 0
  fi

  verbose "remote_http_readiness:launchd_env_repaired plist=${plist_path}"
  return 0
}

ensure_remote_http_client_readiness() {
  if ! has_remote_http_client_targets; then
    verbose "remote_http_readiness:skip reason=no_codex_targets"
    return 0
  fi

  local desired_url
  desired_url="$(desired_mcp_http_url)"
  info "Verifying local MCP HTTP endpoint for remote clients"

  if probe_remote_http_endpoint; then
    ok "Remote MCP endpoint ready at ${desired_url}"
    verbose "remote_http_readiness:healthy detail=${REMOTE_HTTP_PROBE_DETAIL}"
    return 0
  fi

  warn "Remote MCP endpoint is not healthy at ${desired_url}"
  [ -n "${REMOTE_HTTP_PROBE_DETAIL:-}" ] && warn "  Probe detail: ${REMOTE_HTTP_PROBE_DETAIL}"

  if ! platform_supports_user_service_management; then
    err "Automatic background service setup is not supported on this platform."
    err "Start a local HTTP server with: ${DEST}/${BIN_CLI} serve-http --no-tui"
    return 1
  fi

  if ! "$DEST/$BIN_CLI" service install --help >/dev/null 2>&1; then
    err "This build does not expose 'am service install'; cannot start the required background service."
    err "Start a local HTTP server with: ${DEST}/${BIN_CLI} serve-http --no-tui"
    return 1
  fi

  info "Installing or restarting the background Agent Mail HTTP service"
  local service_output=""
  if service_output=$("$DEST/$BIN_CLI" service install --host "$(desired_service_bind_host)" --port "$(desired_service_bind_port)" 2>&1); then
    while IFS= read -r line; do
      [ -n "$line" ] && verbose "remote_http_readiness:service ${line}"
    done <<< "$service_output"
    repair_launchd_service_env_from_rust_config
  else
    err "Automatic background service setup failed."
    if [ -n "$service_output" ]; then
      while IFS= read -r line; do
        [ -n "$line" ] && err "  ${line}"
      done <<< "$service_output"
    fi
    err "You can still start a local HTTP server manually with: ${DEST}/${BIN_CLI} serve-http --no-tui"
    return 1
  fi

  if wait_for_remote_http_endpoint 20; then
    ok "Background Agent Mail HTTP service is ready for remote clients"
    verbose "remote_http_readiness:service_ready detail=${REMOTE_HTTP_PROBE_DETAIL}"
    return 0
  fi

  # The service install returned 0 but the endpoint never came up. This was
  # the exact failure mode in #96 (systemd CHDIR crash-loop) where the
  # installer said "ok" while every MCP client saw zero tools. Surface it as
  # an ERR and fail the installer so orchestrators such as ACFS cannot mark a
  # broken remote-MCP setup as successful.
  err "Background service was installed, but the MCP HTTP endpoint is still not healthy."
  # `service status` returns non-zero when the service is failing (which is
  # exactly the case that lands us here), so capture its output
  # unconditionally — otherwise a `if service_output=$(...); then` would drop
  # the diagnostic lines on the floor precisely when they're most needed.
  local service_output=""
  service_output=$("$DEST/$BIN_CLI" service status 2>&1 || true)
  if [ -n "$service_output" ]; then
    while IFS= read -r line; do
      if [ -n "$line" ]; then
        err "  status: ${line}"
        verbose "remote_http_readiness:status ${line}"
      fi
    done <<< "$service_output"
  fi
  err "Diagnose with:  ${DEST}/${BIN_CLI} service status"
  err "Or start a foreground server manually with:  ${DEST}/${BIN_CLI} serve-http --no-tui"
  err "MCP clients (Claude Code, Cursor, Codex, …) will see zero tools until this is resolved."
  return 1
}

resolve_setup_http_bearer_token() {
  if [ -n "${HTTP_BEARER_TOKEN:-}" ]; then
    printf '%s' "$HTTP_BEARER_TOKEN"
    return 0
  fi
  resolve_migrated_bearer_token
}

# Insert or create an mcp-agent-mail entry in a TOML config file.
# Handles Codex CLI's ~/.codex/config.toml with [mcp_servers.mcp_agent_mail].
# Returns: 0=configured, 1=unchanged, 2=error.
setup_single_toml_config() {
  local tool="$1"
  local config_path="$2"
  local _binary_path="${3:-}"
  local section_header='[mcp_servers.mcp_agent_mail]'
  local desired_url
  desired_url="$(desired_mcp_http_url)"
  local desired_startup_timeout_sec="30"
  local bearer_token
  bearer_token="$(resolve_setup_http_bearer_token)"
  local desired_auth_header=""
  local tmp_file="${config_path}.tmp.mcp-agent-mail.$$"
  local backup=""

  if [ -n "$bearer_token" ]; then
    desired_auth_header="Bearer ${bearer_token}"
  fi

  if [ ! -f "$config_path" ]; then
    # File doesn't exist — create it with just the MCP section
    local parent_dir
    parent_dir=$(dirname "$config_path")
    mkdir -p "$parent_dir" 2>/dev/null || true

    cat > "$config_path" <<TOMLEOF
${section_header}
url = "${desired_url}"
startup_timeout_sec = ${desired_startup_timeout_sec}
TOMLEOF
    if [ -n "$desired_auth_header" ]; then
      cat >> "$config_path" <<TOMLEOF
http_headers = { Authorization = "${desired_auth_header}" }
TOMLEOF
    fi
    verbose "setup_toml_config:created tool=${tool} path=${config_path}"
    return 0
  fi

  if ! awk \
    -v section_header="$section_header" \
    -v desired_url="$desired_url" \
    -v desired_startup_timeout_sec="$desired_startup_timeout_sec" \
    -v desired_auth_header="$desired_auth_header" '
    function flush_section() {
      if (!saw_url_in_section) {
        print "url = \"" desired_url "\""
      }
      if (!saw_startup_timeout_in_section) {
        print "startup_timeout_sec = " desired_startup_timeout_sec
      }
      if (!saw_http_headers_in_section && desired_auth_header != "") {
        print "http_headers = { Authorization = \"" desired_auth_header "\" }"
      }
    }

    BEGIN {
      in_section = 0
      in_subtable = 0
      saw_section = 0
      saw_url_in_section = 0
      saw_startup_timeout_in_section = 0
      saw_http_headers_in_section = 0
      duplicate_section = 0
    }

    # Match the canonical section header (underscore form).
    /^\[mcp_servers\.mcp_agent_mail\]([[:space:]]*#.*)?[[:space:]]*$/ {
      if (in_section || in_subtable) {
        flush_section()
      }
      if (saw_section) {
        # Duplicate section — skip it entirely.  The first occurrence
        # already has all the desired keys.
        duplicate_section = 1
        in_section = 0
        in_subtable = 0
        next
      }
      in_section = 1
      in_subtable = 0
      saw_section = 1
      saw_url_in_section = 0
      saw_startup_timeout_in_section = 0
      saw_http_headers_in_section = 0
      duplicate_section = 0
      print
      next
    }

    # Match the hyphenated variant — bare or quoted (normalize to underscore).
    /^\[mcp_servers\.(mcp-agent-mail|"mcp-agent-mail")\]([[:space:]]*#.*)?[[:space:]]*$/ {
      if (in_section || in_subtable) {
        flush_section()
      }
      if (saw_section) {
        duplicate_section = 1
        in_section = 0
        in_subtable = 0
        next
      }
      in_section = 1
      in_subtable = 0
      saw_section = 1
      saw_url_in_section = 0
      saw_startup_timeout_in_section = 0
      saw_http_headers_in_section = 0
      duplicate_section = 0
      # Normalize hyphenated form to underscore
      print section_header
      next
    }

    # Match sub-tables like [mcp_servers.mcp_agent_mail.http_headers].
    # These are part of the mcp_agent_mail section and must be absorbed
    # (our output uses inline table syntax instead).
    /^\[mcp_servers\.mcp_agent_mail\./ || /^\[mcp_servers\.(mcp-agent-mail|"mcp-agent-mail")\./ {
      in_subtable = 1
      in_section = 0
      # Do not print the sub-table header — we use inline form.
      # Mark http_headers as seen so flush_section does not re-emit it.
      if ($0 ~ /http_headers/) {
        saw_http_headers_in_section = 1
        if (desired_auth_header != "") {
          # Emit the inline form instead (will appear at the end of
          # the main section via flush_section if not already emitted).
        }
      }
      next
    }

    # Any other [section] header ends our section/subtable.
    /^\[/ {
      if (in_section) {
        flush_section()
      }
      in_section = 0
      in_subtable = 0
      duplicate_section = 0
    }

    {
      # Skip lines belonging to a duplicate section.
      if (duplicate_section) {
        next
      }
      # Skip lines belonging to a sub-table we are absorbing.
      if (in_subtable) {
        # Capture Authorization value from the sub-table if present.
        if ($0 ~ /^[[:space:]]*Authorization[[:space:]]*=/) {
          # Already handled via desired_auth_header
        }
        next
      }

      if (in_section && $0 ~ /^[[:space:]]*(url|httpUrl)[[:space:]]*=/) {
        print "url = \"" desired_url "\""
        saw_url_in_section = 1
        next
      }
      if (in_section && $0 ~ /^[[:space:]]*startup_timeout_sec[[:space:]]*=/) {
        print "startup_timeout_sec = " desired_startup_timeout_sec
        saw_startup_timeout_in_section = 1
        next
      }
      if (in_section && $0 ~ /^[[:space:]]*http_headers[[:space:]]*=/) {
        if (desired_auth_header != "") {
          print "http_headers = { Authorization = \"" desired_auth_header "\" }"
        } else {
          print
        }
        saw_http_headers_in_section = 1
        next
      }
      if (in_section && $0 ~ /^[[:space:]]*bearer_token_env_var[[:space:]]*=/) {
        if (desired_auth_header != "") {
          print "http_headers = { Authorization = \"" desired_auth_header "\" }"
          saw_http_headers_in_section = 1
        }
        next
      }
      if (in_section && $0 ~ /^[[:space:]]*(command|args)[[:space:]]*=/) {
        next
      }
      print
    }

    END {
      if (in_section) {
        flush_section()
      }
      if (!saw_section) {
        if (NR > 0) {
          print ""
        }
        print section_header
        print "url = \"" desired_url "\""
        print "startup_timeout_sec = " desired_startup_timeout_sec
        if (desired_auth_header != "") {
          print "http_headers = { Authorization = \"" desired_auth_header "\" }"
        }
      }
    }
  ' "$config_path" > "$tmp_file"; then
    rm -f "$tmp_file"
    verbose "setup_toml_config:error tool=${tool} path=${config_path}"
    return 2
  fi

  if cmp -s "$config_path" "$tmp_file"; then
    rm -f "$tmp_file"
    verbose "setup_toml_config:unchanged tool=${tool} path=${config_path}"
    return 1
  fi

  backup="${config_path}.$(date -u +%Y%m%d_%H%M%S).bak"
  cp -p "$config_path" "$backup"
  chmod --reference="$config_path" "$tmp_file" 2>/dev/null || true
  mv "$tmp_file" "$config_path"
  verbose "setup_toml_config:updated tool=${tool} path=${config_path} backup=${backup}"
  return 0
}

setup_single_codex_json_config() {
  local tool="$1"
  local config_path="$2"
  local desired_url
  desired_url="$(desired_mcp_http_url)"
  local bearer_token
  bearer_token="$(resolve_setup_http_bearer_token)"
  local desired_auth_header=""

  if [ -n "$bearer_token" ]; then
    desired_auth_header="Bearer ${bearer_token}"
  fi

  if ! command -v python3 >/dev/null 2>&1; then
    verbose "setup_codex_json:skip_no_python3 tool=${tool} path=${config_path}"
    return 2
  fi

  local result
  result=$(python3 - "$config_path" "$desired_url" "$desired_auth_header" <<'PY'
import json
import os
import re
import shutil
import sys
from datetime import UTC, datetime

config_path, desired_url, desired_auth_header = sys.argv[1:4]


def load_text(path: str) -> str:
    try:
        with open(path, "r", encoding="utf-8") as handle:
            return handle.read()
    except FileNotFoundError:
        return ""


def parse_json(text: str):
    if text.startswith("\ufeff"):
        text = text[1:]
    if not text.strip():
        return {}
    try:
        return json.loads(text)
    except json.JSONDecodeError:
        cleaned = re.sub(r"//.*?\n", "\n", text)
        cleaned = re.sub(r"/\*.*?\*/", "", cleaned, flags=re.DOTALL)
        cleaned = re.sub(r",\s*([}\]])", r"\1", cleaned)
        return json.loads(cleaned)


def dump_json(doc) -> str:
    return json.dumps(doc, indent=2, ensure_ascii=False) + "\n"


text = load_text(config_path)
doc = parse_json(text)
if not isinstance(doc, dict):
    print("ERROR:not_object")
    raise SystemExit(0)

container_key = None
for key in ("mcpServers", "servers", "mcp", "mcp_servers"):
    value = doc.get(key)
    if isinstance(value, dict):
        container_key = key
        break
if container_key is None:
    container_key = "mcpServers"
    doc[container_key] = {}

container = doc[container_key]
entry_key = "mcp-agent-mail"
for candidate in ("mcp-agent-mail", "mcp_agent_mail"):
    value = container.get(candidate)
    if isinstance(value, dict):
      entry_key = candidate
      break

existing_entry = container.get(entry_key)
if not isinstance(existing_entry, dict):
    existing_entry = {}

new_entry = {
    key: value
    for key, value in existing_entry.items()
    if key not in {"command", "args", "transport", "httpUrl", "bearer_token_env_var"}
}
new_entry["type"] = "http"
new_entry["url"] = desired_url

headers = new_entry.get("headers")
if headers is not None and not isinstance(headers, dict):
    headers = None
if headers is None:
    headers = {}

if desired_auth_header:
    headers["Authorization"] = desired_auth_header

if headers:
    new_entry["headers"] = headers
else:
    new_entry.pop("headers", None)

container[entry_key] = new_entry
new_text = dump_json(doc)
if new_text == dump_json(parse_json(text)):
    print("SKIP:unchanged")
    raise SystemExit(0)

parent_dir = os.path.dirname(config_path)
if parent_dir:
    os.makedirs(parent_dir, exist_ok=True)
if os.path.exists(config_path):
    stamp = datetime.now(UTC).strftime("%Y%m%d_%H%M%S")
    backup = f"{config_path}.{stamp}.bak"
    shutil.copy2(config_path, backup)
else:
    backup = ""
with open(config_path, "w", encoding="utf-8") as handle:
    handle.write(new_text)

if backup:
    print(f"OK:updated backup={backup}")
else:
    print("OK:created")
PY
) || true

  case "$result" in
    SKIP:unchanged)
      verbose "setup_codex_json:unchanged tool=${tool} path=${config_path}"
      return 1
      ;;
    OK:*)
      verbose "setup_codex_json:configured tool=${tool} path=${config_path} ${result}"
      return 0
      ;;
    ERROR:*)
      verbose "setup_codex_json:error tool=${tool} path=${config_path} ${result}"
      return 2
      ;;
    *)
      verbose "setup_codex_json:unknown_result tool=${tool} path=${config_path} ${result}"
      return 2
      ;;
  esac
}

# Insert or create an mcp-agent-mail entry in a JSON config file.
# Uses python3/jq for JSON manipulation if available, otherwise sed-based.
setup_single_mcp_config() {
  local tool="$1"
  local config_path="$2"
  local binary_path="$3"
  local bearer_token="$4"
  local storage_root="${5:-}"

  verbose "setup_mcp_config:start tool=${tool} path=${config_path}"

  # TOML configs (e.g. Codex ~/.codex/config.toml) — handle separately
  case "$config_path" in
    *.toml)
      setup_single_toml_config "$tool" "$config_path" "$binary_path"
      return $?
      ;;
  esac

  if [ "$tool" = "codex" ]; then
    setup_single_codex_json_config "$tool" "$config_path"
    return $?
  fi

  # Build the server entry JSON
  local env_block=""
  if [ -n "$bearer_token" ] && [ -n "$storage_root" ]; then
    env_block="\"env\": {\"HTTP_BEARER_TOKEN\": \"${bearer_token}\", \"STORAGE_ROOT\": \"${storage_root}\"}"
  elif [ -n "$bearer_token" ]; then
    env_block="\"env\": {\"HTTP_BEARER_TOKEN\": \"${bearer_token}\"}"
  elif [ -n "$storage_root" ]; then
    env_block="\"env\": {\"STORAGE_ROOT\": \"${storage_root}\"}"
  fi

  local entry_json
  if [ -n "$env_block" ]; then
    entry_json="{\"command\": \"${binary_path}\", \"args\": [], ${env_block}}"
  else
    entry_json="{\"command\": \"${binary_path}\", \"args\": []}"
  fi

  if [ ! -f "$config_path" ]; then
    # Create a new config file
    local parent_dir
    parent_dir=$(dirname "$config_path")
    mkdir -p "$parent_dir" 2>/dev/null || true

    if command -v python3 >/dev/null 2>&1; then
      python3 -c "
import json, sys
entry = json.loads(sys.argv[1])
doc = {'mcpServers': {'mcp-agent-mail': entry}}
print(json.dumps(doc, indent=2))
" "$entry_json" > "$config_path"
    else
      cat > "$config_path" <<MCPEOF
{
  "mcpServers": {
    "mcp-agent-mail": ${entry_json}
  }
}
MCPEOF
    fi
    verbose "setup_mcp_config:created tool=${tool} path=${config_path}"
    return 0
  fi

  # File exists — check if mcp-agent-mail entry already present
  if command -v python3 >/dev/null 2>&1; then
    local result
    result=$(python3 -c "
import json, sys, os

config_path = sys.argv[1]
entry_json = sys.argv[2]

with open(config_path, 'r') as f:
    text = f.read()

# Strip BOM
if text.startswith('\ufeff'):
    text = text[1:]

try:
    doc = json.loads(text)
except json.JSONDecodeError:
    # Try stripping comments and trailing commas (basic JSON5 compat)
    import re
    cleaned = re.sub(r'//.*?\n', '\n', text)
    cleaned = re.sub(r'/\*.*?\*/', '', cleaned, flags=re.DOTALL)
    cleaned = re.sub(r',\s*([}\]])', r'\1', cleaned)
    doc = json.loads(cleaned)

if not isinstance(doc, dict):
    print('ERROR:not_object')
    sys.exit(0)

# Find existing server container
container_key = None
for key in ['mcpServers', 'servers', 'mcp', 'mcp_servers']:
    if key in doc and isinstance(doc[key], dict):
        container_key = key
        break

if container_key and 'mcp-agent-mail' in doc[container_key]:
    print('SKIP:already_present')
    sys.exit(0)

# Backup
import shutil
from datetime import datetime
stamp = datetime.utcnow().strftime('%Y%m%d_%H%M%S')
backup = config_path + '.' + stamp + '.bak'
shutil.copy2(config_path, backup)

# Insert entry
entry = json.loads(entry_json)
if container_key is None:
    container_key = 'mcpServers'
    doc[container_key] = {}
doc[container_key]['mcp-agent-mail'] = entry

with open(config_path, 'w') as f:
    json.dump(doc, f, indent=2)
    f.write('\n')

print('OK:inserted backup=' + backup)
" "$config_path" "$entry_json" 2>&1) || true

    case "$result" in
      SKIP:already_present)
        verbose "setup_mcp_config:skip_existing tool=${tool} path=${config_path}"
        return 1
        ;;
      OK:inserted*)
        verbose "setup_mcp_config:inserted tool=${tool} path=${config_path} ${result}"
        return 0
        ;;
      ERROR:*)
        verbose "setup_mcp_config:error tool=${tool} path=${config_path} ${result}"
        return 2
        ;;
      *)
        verbose "setup_mcp_config:unknown_result tool=${tool} result=${result}"
        return 2
        ;;
    esac
  else
    # No python3 — skip JSON manipulation to avoid corruption
    verbose "setup_mcp_config:skip_no_python3 tool=${tool} path=${config_path}"
    return 2
  fi
}

# Register mcp-agent-mail with Claude Code via the supported `claude mcp add`
# CLI. Claude Code reads MCP registrations from ~/.claude.json (a single
# dotfile whose full schema we don't want to hand-edit). This is the supported
# path, handles scoping, and avoids the silent-failure mode reported in #97
# where entries were written to ~/.claude/settings.json and ignored by the
# Claude Code loader.
#
# Return codes:
#   0 — registered (or already present with matching transport/url)
#   1 — skipped cleanly (claude CLI not installed — don't fail the install)
#   2 — attempted but failed
setup_claude_code_mcp_via_cli() {
  # $1: bearer token to register (must match what the other MCP client configs
  # in this installer run are using, so Claude Code and Codex/Cursor/etc. all
  # agree on the same Authorization header).
  local bearer_token="${1:-}"

  if ! command -v claude >/dev/null 2>&1; then
    verbose "setup_claude_code_mcp:skip reason=claude_cli_not_available"
    return 1
  fi

  local desired_url
  desired_url="$(desired_mcp_http_url)"

  # Best-effort remove first so the add always takes effect with the current
  # URL + Authorization header. Skipping remove and relying on `add` to update
  # an existing entry doesn't work — `claude mcp add` refuses to overwrite an
  # existing name on most `claude` versions, and gating remove on a regex
  # match against `claude mcp list` output makes us fragile to any future
  # change to that CLI's listing format. `claude mcp remove` is a no-op when
  # the entry is absent, so the unconditional call is safe on first install
  # too; both `|| true` fallbacks swallow the expected non-zero exit code.
  claude mcp remove mcp-agent-mail --scope user >/dev/null 2>&1 \
    || claude mcp remove mcp-agent-mail >/dev/null 2>&1 \
    || true

  local -a add_args=(mcp add --scope user --transport http mcp-agent-mail "$desired_url")
  if [ -n "$bearer_token" ]; then
    add_args+=(--header "Authorization: Bearer ${bearer_token}")
  fi

  if claude "${add_args[@]}" >/dev/null 2>&1; then
    ok "[claude-code] Registered mcp-agent-mail via 'claude mcp add' (user scope, HTTP transport)"
    verbose "setup_claude_code_mcp:ok url=${desired_url}"
    return 0
  fi

  verbose "setup_claude_code_mcp:fail url=${desired_url}"
  return 2
}

# Set up MCP configs for all detected tools.
# For fresh installs: create configs where missing, insert entries where absent.
setup_mcp_configs() {
  local binary_path="$1"
  local scan
  scan=$(detect_mcp_configs "$PWD" || true)

  local bearer_token
  bearer_token=$(generate_bearer_token)
  verbose "setup_mcp_configs:generated_token len=${#bearer_token}"

  local storage_root="${STORAGE_ROOT:-}"
  local configured=0
  local skipped=0
  local failed=0
  local tool path exists_flag

  # Track which tools we've already configured (prefer existing configs).
  # Note: Claude Code is handled separately via setup_claude_code_mcp_via_cli
  # below and does NOT appear in the candidate scan. The `tool=claude` entries
  # in the scan therefore all refer to Claude Desktop's claude_desktop_config.json
  # (a different product), and the loop below gets to handle them normally.
  local configured_tools=""

  # Claude Code gets its own path: the `claude mcp add` CLI writes to
  # ~/.claude.json, which is the file Claude Code actually reads. Writing to
  # ~/.claude/settings.json is silently ignored (#97). We attempt this first
  # regardless of whether candidate files exist, because Claude Code doesn't
  # surface in the candidate scan any more. Pass the same $bearer_token the
  # rest of this function writes into other tools' configs so every MCP
  # client in this install agrees on the same Authorization header.
  if setup_claude_code_mcp_via_cli "$bearer_token"; then
    configured=$((configured + 1))
  fi

  [ -z "$scan" ] && {
    if [ "$configured" -gt 0 ]; then
      ok "Configured $configured MCP config(s)"
    fi
    return 0
  }

  # First pass: handle existing configs
  while IFS=$'\t' read -r tool path exists_flag; do
    [ -z "${tool:-}" ] && continue
    [ "$exists_flag" != "1" ] && continue

    # Skip if we already configured this tool
    case "|${configured_tools}|" in
      *"|${tool}|"*) continue ;;
    esac

    if setup_single_mcp_config "$tool" "$path" "$binary_path" "$bearer_token" "$storage_root"; then
      ok "[$tool] Configured MCP entry in $path"
      configured=$((configured + 1))
      configured_tools="${configured_tools}|${tool}"
    else
      local rc=$?
      if [ "$rc" -eq 1 ]; then
        verbose "setup_mcp_configs:skip tool=${tool} path=${path} reason=already_present"
        skipped=$((skipped + 1))
        configured_tools="${configured_tools}|${tool}"
      else
        verbose "setup_mcp_configs:fail tool=${tool} path=${path}"
        failed=$((failed + 1))
      fi
    fi
  done <<< "$scan"

  # Second pass: create configs for detected tools without existing configs
  # Only create for tools that have their config directory parent present
  # (indicating the tool is likely installed)
  while IFS=$'\t' read -r tool path exists_flag; do
    [ -z "${tool:-}" ] && continue
    [ "$exists_flag" = "1" ] && continue

    # Skip if already configured
    case "|${configured_tools}|" in
      *"|${tool}|"*) continue ;;
    esac

    # Only create if the tool's config parent directory exists
    # (indicates the tool is likely installed)
    local parent_dir
    parent_dir=$(dirname "$path")
    local grandparent_dir
    grandparent_dir=$(dirname "$parent_dir")
    if [ -d "$parent_dir" ] || [ -d "$grandparent_dir" ]; then
      if setup_single_mcp_config "$tool" "$path" "$binary_path" "$bearer_token" "$storage_root"; then
        ok "[$tool] Created fresh MCP config at $path"
        configured=$((configured + 1))
        configured_tools="${configured_tools}|${tool}"
      fi
    fi
  done <<< "$scan"

  if [ "$configured" -gt 0 ]; then
    ok "Configured $configured MCP config(s)"
  fi
  if [ "$skipped" -gt 0 ]; then
    info "$skipped MCP config(s) already had mcp-agent-mail entry"
  fi
  verbose "setup_mcp_configs:done configured=${configured} skipped=${skipped} failed=${failed}"
}

sync_codex_http_configs() {
  local binary_path="$1"
  local scan
  scan=$(detect_mcp_configs "$PWD" || true)
  [ -z "$scan" ] && return 0

  local synced=0
  local failed=0
  local tool path exists_flag

  while IFS=$'\t' read -r tool path exists_flag; do
    [ -z "${tool:-}" ] && continue
    [ "$tool" != "codex" ] && continue

    if [ "$exists_flag" != "1" ]; then
      local parent_dir
      parent_dir=$(dirname "$path")
      local grandparent_dir
      grandparent_dir=$(dirname "$parent_dir")
      if [ ! -d "$parent_dir" ] && [ ! -d "$grandparent_dir" ]; then
        continue
      fi
    fi

    if setup_single_mcp_config "$tool" "$path" "$binary_path" "" ""; then
      synced=$((synced + 1))
    else
      local rc=$?
      if [ "$rc" -ne 1 ]; then
        failed=$((failed + 1))
        warn "[codex] Failed to sync HTTP MCP config at $path"
      fi
    fi
  done <<< "$scan"

  if [ "$synced" -gt 0 ]; then
    ok "[codex] Synced $synced HTTP MCP config(s)"
  fi
  if [ "$failed" -gt 0 ]; then
    warn "[codex] Failed to sync $failed HTTP MCP config(s)"
  fi
}

# Update existing MCP configs that point to Python to use the Rust binary.
# Called after binary installation + migration, using the newly-installed am CLI.
update_mcp_configs() {
  local binary_path="$1"
  local am_cli="${2:-}"

  # Resolve am CLI path: prefer explicit, then adjacent, then PATH
  if [ -z "$am_cli" ]; then
    local dest_dir
    dest_dir="$(dirname "$binary_path")"
    if [ -x "${dest_dir}/am" ]; then
      am_cli="${dest_dir}/am"
    elif command -v am >/dev/null 2>&1; then
      am_cli="am"
    else
      verbose "update_mcp_configs:skip reason=no_am_cli"
      warn "Could not find 'am' CLI to update MCP configs."
      warn "Run 'am setup run' manually after installation."
      return 0
    fi
  fi

  verbose "update_mcp_configs:start binary=${binary_path} cli=${am_cli}"

  # Check that setup subcommand exists (graceful degradation for older builds)
  if ! AM_INTERFACE_MODE=cli "$am_cli" setup --help >/dev/null 2>&1; then
    verbose "update_mcp_configs:skip reason=no_setup_subcommand"
    return 0
  fi

  set +e
  local setup_out
  local setup_token
  setup_token="$(resolve_setup_http_bearer_token)"
  if [ -n "$setup_token" ]; then
    setup_out=$(AM_INTERFACE_MODE=cli HTTP_BEARER_TOKEN="$setup_token" "$am_cli" setup run --yes --no-hooks 2>&1)
  else
    setup_out=$(AM_INTERFACE_MODE=cli "$am_cli" setup run --yes --no-hooks 2>&1)
  fi
  local setup_rc=$?
  set -e

  verbose "update_mcp_configs:result rc=${setup_rc}"
  if [ -n "$setup_out" ]; then
    verbose "update_mcp_configs:output ${setup_out}"
  fi

  if [ "$setup_rc" -eq 0 ]; then
    # Parse counts from output (e.g., "7 config files processed: 2 created, 1 updated, 4 unchanged")
    local counts_line created updated
    counts_line=$(echo "$setup_out" | command grep "config files processed" 2>/dev/null || true)
    created="0"
    updated="0"
    if [ -n "$counts_line" ]; then
      # Extract numbers portably: "N created" and "N updated"
      created=$(echo "$counts_line" | sed -n 's/.*[^0-9]\([0-9][0-9]*\) created.*/\1/p' 2>/dev/null || echo "0")
      updated=$(echo "$counts_line" | sed -n 's/.*[^0-9]\([0-9][0-9]*\) updated.*/\1/p' 2>/dev/null || echo "0")
      [ -z "$created" ] && created="0"
      [ -z "$updated" ] && updated="0"
    fi
    if [ "$created" -gt 0 ] || [ "$updated" -gt 0 ]; then
      ok "MCP configs updated: ${created} created, ${updated} updated"
    else
      verbose "update_mcp_configs:no_changes"
    fi
  else
    warn "MCP config update returned exit code $setup_rc"
    warn "Run 'am setup run' manually to configure MCP integrations."
  fi
}

record_uninstall_summary() {
  UNINSTALL_SUMMARY+=("$1")
  verbose "uninstall:summary $1"
}

confirm_uninstall_step() {
  local prompt="$1"
  if [ "$ASSUME_YES" -eq 1 ]; then
    verbose "uninstall:confirm auto_yes prompt=${prompt}"
    return 0
  fi

  if [ ! -t 0 ]; then
    return 1
  fi

  printf "%s [y/N] " "$prompt"
  local answer=""
  read -r answer </dev/tty 2>/dev/null || answer="n"
  case "$answer" in
    y|Y|yes|YES) return 0 ;;
    *) return 1 ;;
  esac
}

backup_file_for_uninstall() {
  local path="$1"
  local ts backup
  ts=$(date -u +%Y%m%dT%H%M%SZ)
  backup="${path}.bak.mcp-agent-mail-uninstall-${ts}"
  cp -p "$path" "$backup"
  echo "$backup"
}

remove_path_exports_from_rc() {
  local rc="$1"
  [ -f "$rc" ] || return 1

  local tmp
  tmp="${rc}.tmp.mcp-agent-mail-uninstall.$$"

  awk -v dest="$DEST" '
    function trim(line) {
      sub(/^[[:space:]]+/, "", line)
      sub(/[[:space:]]+$/, "", line)
      return line
    }
    {
      line = trim($0)
      expected_double = "export PATH=\"" dest ":$PATH\""
      expected_single = "export PATH='\''" dest ":$PATH'\''"
      expected_bare = "export PATH=" dest ":$PATH"
      if (line == expected_double || line == expected_single || line == expected_bare) {
        next
      }
      print
    }
  ' "$rc" > "$tmp"

  if cmp -s "$rc" "$tmp"; then
    rm -f "$tmp"
    return 1
  fi

  local backup
  backup=$(backup_file_for_uninstall "$rc")
  mv "$tmp" "$rc"
  record_uninstall_summary "Removed PATH export from ${rc} (backup: ${backup})"
  return 0
}

remove_mcp_entries_from_toml() {
  local input="$1"
  local output="$2"
  awk '
    BEGIN { skip = 0 }
    {
      if (skip && $0 ~ /^[[:space:]]*\[/) {
        skip = 0
      }
      if (skip) {
        next
      }

      if ($0 ~ /^[[:space:]]*\[(mcpServers|mcp_servers)\.(mcp-agent-mail|mcp_agent_mail|mcp_agent_mail_rust)(\..*)?\][[:space:]]*$/) {
        skip = 1
        next
      }

      if ($0 ~ /(mcp-agent-mail|mcp_agent_mail|mcp_agent_mail_rust)/) {
        next
      }

      print
    }
  ' "$input" > "$output"
}

remove_mcp_entries_from_json_like() {
  local input="$1"
  local output="$2"
  awk '
    function brace_delta(str, tmp, opens, closes) {
      tmp = str
      opens = gsub(/\{/, "{", tmp)
      tmp = str
      closes = gsub(/\}/, "}", tmp)
      return opens - closes
    }
    BEGIN {
      skip = 0
      depth = 0
    }
    {
      line = $0
      if (skip) {
        depth += brace_delta(line)
        if (depth <= 0) {
          skip = 0
          next
        }
        next
      }

      if (line ~ /"(mcp-agent-mail|mcp_agent_mail|mcp_agent_mail_rust)"[[:space:]]*:[[:space:]]*\{/) {
        skip = 1
        depth = brace_delta(line)
        if (depth <= 0) {
          skip = 0
        }
        next
      }

      if (line ~ /(mcp-agent-mail|mcp_agent_mail|mcp_agent_mail_rust)/) {
        next
      }

      print
    }
  ' "$input" \
    | sed -E 's/,[[:space:]]*([}\]])/\1/g' \
    > "$output"
}

cleanup_mcp_config_file() {
  local tool="$1"
  local config_path="$2"
  [ -f "$config_path" ] || return 1

  if ! grep -Eq 'mcp-agent-mail|mcp_agent_mail|mcp_agent_mail_rust' "$config_path"; then
    return 1
  fi

  local backup tmp
  backup=$(backup_file_for_uninstall "$config_path")
  tmp="${config_path}.tmp.mcp-agent-mail-uninstall.$$"

  case "$config_path" in
    *.toml)
      remove_mcp_entries_from_toml "$config_path" "$tmp"
      ;;
    *)
      remove_mcp_entries_from_json_like "$config_path" "$tmp"
      ;;
  esac

  if cmp -s "$config_path" "$tmp"; then
    rm -f "$tmp"
    return 1
  fi

  mv "$tmp" "$config_path"
  record_uninstall_summary "Removed MCP config entries from ${config_path} (backup: ${backup})"
  return 0
}

cleanup_mcp_configs() {
  local scan tool path exists_flag
  local cleaned=0
  scan=$(detect_mcp_configs "$PWD" || true)
  if [ -z "$scan" ]; then
    record_uninstall_summary "No MCP config candidates found"
    return 0
  fi

  while IFS=$'\t' read -r tool path exists_flag; do
    [ -z "${tool:-}" ] && continue
    [ "$exists_flag" = "1" ] || continue
    if cleanup_mcp_config_file "$tool" "$path"; then
      cleaned=$((cleaned + 1))
    fi
  done <<< "$scan"

  if [ "$cleaned" -eq 0 ]; then
    record_uninstall_summary "No MCP config entries referencing mcp-agent-mail were found"
  fi
}

remove_update_cache_and_logs() {
  local removed=0
  local cache_file="${HOME}/.cache/mcp-agent-mail/update-check.json"
  if [ -f "$cache_file" ]; then
    rm -f "$cache_file"
    record_uninstall_summary "Removed update cache ${cache_file}"
    removed=$((removed + 1))
  fi

  local log_count=0
  while IFS= read -r log_path; do
    [ -z "$log_path" ] && continue
    [ "$log_path" = "$LOG_FILE" ] && continue
    rm -f "$log_path"
    log_count=$((log_count + 1))
  done < <(find /tmp -maxdepth 1 -type f -name 'am-install-*' 2>/dev/null || true)

  if [ "$log_count" -gt 0 ]; then
    record_uninstall_summary "Removed ${log_count} installer log file(s) from /tmp"
    removed=$((removed + log_count))
  fi

  if [ "$removed" -eq 0 ]; then
    record_uninstall_summary "No update cache or installer log files were found"
  fi
}

path_size_bytes() {
  local path="$1"
  if [ -d "$path" ]; then
    du -sk "$path" 2>/dev/null | awk '{print $1 * 1024}'
    return 0
  fi
  if [ -f "$path" ]; then
    wc -c < "$path" 2>/dev/null | awk '{print $1}'
    return 0
  fi
  echo 0
}

human_size_bytes() {
  local bytes="$1"
  if command -v numfmt >/dev/null 2>&1; then
    numfmt --to=iec --suffix=B "$bytes"
  else
    echo "${bytes}B"
  fi
}

collect_uninstall_data_paths() {
  local configured_storage="${STORAGE_ROOT:-$HOME/.mcp_agent_mail}"
  local legacy_storage="$HOME/.mcp_agent_mail_git_mailbox_repo"
  local default_storage="$HOME/.mcp_agent_mail"
  local db_from_env=""
  local seen=""
  local candidate

  if [ -n "${DATABASE_URL:-}" ]; then
    db_from_env=$(printf '%s' "$DATABASE_URL" | sed -n 's|^sqlite[^:]*:///||p')
  fi

  local -a candidates=(
    "$configured_storage"
    "$default_storage"
    "$legacy_storage"
  )
  [ -n "$db_from_env" ] && candidates+=("$db_from_env")

  for candidate in "${candidates[@]}"; do
    candidate="${candidate/#\~/$HOME}"
    [ -n "$candidate" ] || continue
    case "|$seen|" in
      *"|${candidate}|"*) continue ;;
    esac
    seen="${seen}|${candidate}"
    if [ -e "$candidate" ]; then
      printf '%s\n' "$candidate"
    fi
  done
}

purge_data_paths() {
  local -a purge_paths=()
  mapfile -t purge_paths < <(collect_uninstall_data_paths)

  if [ "${#purge_paths[@]}" -eq 0 ]; then
    record_uninstall_summary "No storage/database paths were found to purge"
    return 0
  fi

  local total_bytes=0
  local path bytes
  info "Data purge candidates:"
  for path in "${purge_paths[@]}"; do
    bytes=$(path_size_bytes "$path")
    total_bytes=$((total_bytes + bytes))
    info "  - ${path} ($(human_size_bytes "$bytes"))"
  done
  info "Total purge size: $(human_size_bytes "$total_bytes")"

  if [ "$ASSUME_YES" -eq 0 ]; then
    if ! confirm_uninstall_step "Delete the data paths listed above?"; then
      record_uninstall_summary "Skipped --purge data deletion"
      return 0
    fi
  fi

  for path in "${purge_paths[@]}"; do
    case "$path" in
      ""|"/"|"$HOME")
        warn "Skipping dangerous purge path: ${path}"
        continue
        ;;
    esac
    rm -rf "$path"
    record_uninstall_summary "Purged ${path}"
  done
}

find_latest_python_alias_backup() {
  local rc="$1"
  ls -1t "${rc}.bak.mcp-agent-mail-"* 2>/dev/null | head -1 || true
}

restore_python_alias_backups() {
  local rc_files=("$HOME/.zshrc" "$HOME/.bashrc" "$HOME/.profile" "$HOME/.bash_profile" "$HOME/.config/fish/config.fish" "$HOME/.acfs/zsh/acfs.zshrc" "$HOME/.acfs/bash/acfs.bashrc")
  local restored=0
  local rc backup pre_restore ts

  for rc in "${rc_files[@]}"; do
    backup=$(find_latest_python_alias_backup "$rc")
    [ -n "$backup" ] || continue

    if [ -f "$rc" ]; then
      ts=$(date -u +%Y%m%dT%H%M%SZ)
      pre_restore="${rc}.bak.before-python-restore-${ts}"
      cp -p "$rc" "$pre_restore"
    else
      pre_restore="none"
    fi

    cp -p "$backup" "$rc"
    record_uninstall_summary "Restored Python alias backup ${backup} -> ${rc} (previous backup: ${pre_restore})"
    restored=$((restored + 1))
  done

  if [ "$restored" -eq 0 ]; then
    record_uninstall_summary "No Python alias backups were found to restore"
  fi
}

remove_installed_binaries() {
  local removed=0
  local target
  for target in "$DEST/$BIN_CLI" "$DEST/$BIN_SERVER"; do
    if [ -e "$target" ]; then
      rm -f "$target"
      record_uninstall_summary "Removed binary ${target}"
      removed=$((removed + 1))
    fi
  done

  if [ "$removed" -eq 0 ]; then
    record_uninstall_summary "No binaries were found in ${DEST}"
  fi
}

remove_path_exports() {
  local rc_files=("$HOME/.zshrc" "$HOME/.bashrc" "$HOME/.zshenv" "$HOME/.profile" "$HOME/.bash_profile" "$HOME/.config/fish/config.fish")
  local removed=0
  local rc
  for rc in "${rc_files[@]}"; do
    if remove_path_exports_from_rc "$rc"; then
      removed=$((removed + 1))
    fi
  done

  if [ "$removed" -eq 0 ]; then
    record_uninstall_summary "No PATH entries for ${DEST} were found in shell rc files"
  fi
}

print_uninstall_summary() {
  echo ""
  echo "Uninstall summary:"
  if [ "${#UNINSTALL_SUMMARY[@]}" -eq 0 ]; then
    echo "  - No changes were applied"
    return 0
  fi

  local item
  for item in "${UNINSTALL_SUMMARY[@]}"; do
    echo "  - ${item}"
  done
}

uninstall() {
  verbose "uninstall:start dest=${DEST} yes=${ASSUME_YES} purge=${PURGE}"

  if [ "$ASSUME_YES" -eq 0 ] && [ ! -t 0 ]; then
    err "--uninstall without --yes requires an interactive terminal"
    err "Re-run with --yes for non-interactive uninstall"
    err "Example: ./install.sh --uninstall --yes [--purge]"
    exit 2
  fi

  if [ "$QUIET" -eq 0 ]; then
    echo ""
    info "Running uninstall mode"
    info "Target binary directory: ${DEST}"
    [ "$PURGE" -eq 1 ] && info "Data purge is enabled (--purge)"
  fi

  if confirm_uninstall_step "Remove installed binaries from ${DEST}?"; then
    remove_installed_binaries
  else
    record_uninstall_summary "Skipped binary removal"
  fi

  if confirm_uninstall_step "Remove installer PATH entries from shell rc files?"; then
    remove_path_exports
  else
    record_uninstall_summary "Skipped PATH cleanup"
  fi

  if confirm_uninstall_step "Remove MCP config entries for mcp-agent-mail?"; then
    cleanup_mcp_configs
  else
    record_uninstall_summary "Skipped MCP config cleanup"
  fi

  if confirm_uninstall_step "Remove updater cache and /tmp installer logs?"; then
    remove_update_cache_and_logs
  else
    record_uninstall_summary "Skipped cache/log cleanup"
  fi

  if [ "$ASSUME_YES" -eq 1 ]; then
    record_uninstall_summary "Skipped Python alias restore in --yes mode"
  elif confirm_uninstall_step "Restore Python alias backups (if available)?"; then
    restore_python_alias_backups
  else
    record_uninstall_summary "Skipped Python alias restore"
  fi

  if [ "$PURGE" -eq 1 ]; then
    purge_data_paths
  else
    record_uninstall_summary "Skipped data purge (pass --purge to remove storage/database data)"
  fi

  print_uninstall_summary
}

ensure_rust() {
  if [ "${RUSTUP_INIT_SKIP:-0}" != "0" ]; then
    info "Skipping rustup install (RUSTUP_INIT_SKIP set)"
    return 0
  fi
  if command -v cargo >/dev/null 2>&1 && rustc --version 2>/dev/null | grep -q nightly; then return 0; fi
  if [ "$EASY" -ne 1 ]; then
    if [ -t 0 ]; then
      echo -n "Install Rust nightly via rustup? (y/N): "
      read -r ans
      case "$ans" in y|Y) :;; *) warn "Skipping rustup install"; return 0;; esac
    fi
  fi
  info "Installing rustup (nightly)"
  curl -fsSL https://sh.rustup.rs | sh -s -- -y --default-toolchain nightly --profile minimal
  export PATH="$HOME/.cargo/bin:$PATH"
  rustup component add rustfmt clippy || true
}

# Verify SHA256 checksum of a file
verify_checksum() {
  local file="$1"
  local expected="$2"
  local actual=""
  verbose "verify_checksum:start file=${file} expected=${expected}"

  if [ ! -f "$file" ]; then
    err "File not found: $file"
    err "Re-run the installer to download a fresh artifact."
    error_support_hint
    return 1
  fi

  if command -v sha256sum &>/dev/null; then
    actual=$(sha256sum "$file" | cut -d' ' -f1)
  elif command -v shasum &>/dev/null; then
    actual=$(shasum -a 256 "$file" | cut -d' ' -f1)
  else
    warn "No SHA256 tool found (sha256sum or shasum), skipping verification"
    return 0
  fi

  if [ "$actual" != "$expected" ]; then
    verbose "verify_checksum:failed actual=${actual}"
    err "Checksum verification FAILED!"
    err "Expected: $expected"
    err "Got:      $actual"
    err "The downloaded file may be corrupted or tampered with."
    err "Try re-running the installer to fetch a fresh artifact."
    err "If you passed --checksum manually, verify it matches the release asset."
    err "Use --no-verify only for local testing with trusted artifacts."
    error_support_hint
    rm -f "$file"
    return 1
  fi

  ok "Checksum verified: ${actual:0:16}..."
  verbose "verify_checksum:ok actual=${actual}"
  return 0
}

# Verify Sigstore/cosign bundle for a file (best-effort)
verify_sigstore_bundle() {
  local file="$1"
  local artifact_url="$2"
  verbose "verify_sigstore_bundle:start file=${file} artifact_url=${artifact_url}"

  if ! command -v cosign &>/dev/null; then
    warn "cosign not found; skipping signature verification (install cosign for stronger authenticity checks)"
    return 0
  fi

  local bundle_url="$SIGSTORE_BUNDLE_URL"
  if [ -z "$bundle_url" ]; then
    bundle_url="${artifact_url}.sigstore.json"
  fi

  local bundle_file="$TMP/$(basename "$bundle_url")"
  info "Fetching sigstore bundle from ${bundle_url}"
  if ! download_to_file "$bundle_url" "$bundle_file" "sigstore-bundle"; then
    warn "Sigstore bundle not found; skipping signature verification"
    verbose "verify_sigstore_bundle:bundle_missing url=${bundle_url}"
    return 0
  fi

  # Guard: verify the bundle file actually exists and is non-empty after download
  if [ ! -f "$bundle_file" ]; then
    warn "Sigstore bundle file missing after download; skipping signature verification"
    verbose "verify_sigstore_bundle:file_missing_after_download path=${bundle_file}"
    return 0
  fi
  if [ ! -s "$bundle_file" ]; then
    warn "Sigstore bundle file is empty; skipping signature verification"
    verbose "verify_sigstore_bundle:file_empty path=${bundle_file}"
    return 0
  fi

  if ! cosign verify-blob \
    --bundle "$bundle_file" \
    --certificate-identity-regexp "$COSIGN_IDENTITY_RE" \
    --certificate-oidc-issuer "$COSIGN_OIDC_ISSUER" \
    "$file"; then
    verbose "verify_sigstore_bundle:cosign_failed bundle=${bundle_file}"
    return 1
  fi

  ok "Signature verified (cosign)"
  verbose "verify_sigstore_bundle:ok bundle=${bundle_file}"
  return 0
}

# Check if installed version matches target
check_installed_version() {
  local target_version="$1"
  if [ ! -x "$DEST/$BIN_CLI" ]; then
    return 1
  fi

  local installed_version
  if capture_command_with_timeout 3 "$DEST/$BIN_CLI" --version; then
    installed_version=$(printf '%s\n' "$CAPTURED_CMD_OUTPUT" | head -1 | sed 's/.*\([0-9]\+\.[0-9]\+\.[0-9]\+\).*/\1/')
  else
    if [ "$CAPTURED_CMD_STATUS" -eq 124 ]; then
      verbose "check_installed_version:timeout path=${DEST}/$BIN_CLI"
    fi
    return 1
  fi

  if [ -z "$installed_version" ]; then
    return 1
  fi

  local target_clean="${target_version#v}"
  local installed_clean="${installed_version#v}"

  if [ "$target_clean" = "$installed_clean" ]; then
    return 0
  fi

  return 1
}

EXISTING_INSTALL_REPAIR_REASON=""

interactive_shell_am_descriptor() {
  local shell_name
  shell_name=$(basename "${SHELL:-/bin/sh}")

  case "$shell_name" in
    zsh)
      zsh -i -c 'command -V am 2>/dev/null || echo NOT_FOUND' 2>/dev/null || echo "NOT_FOUND"
      ;;
    bash)
      bash -i -c 'command -V am 2>/dev/null || echo NOT_FOUND' 2>/dev/null || echo "NOT_FOUND"
      ;;
    fish)
      fish -i -c 'type -a am 2>/dev/null | head -1; or echo NOT_FOUND' 2>/dev/null || echo "NOT_FOUND"
      ;;
    *)
      sh -c 'command -V am 2>/dev/null || echo NOT_FOUND' 2>/dev/null || echo "NOT_FOUND"
      ;;
  esac
}

python_alias_points_to_installed_am() {
  [ "$PYTHON_ALIAS_FOUND" -eq 1 ] || return 1

  local alias_body candidate
  alias_body="$(python_alias_entry_body 2>/dev/null || true)"
  [ -z "$alias_body" ] && alias_body="${PYTHON_ALIAS_CONTENT:-}"
  [ -n "$alias_body" ] || return 1

  candidate=$(printf '%s\n' "$alias_body" | sed -n -E \
    -e "s/^[[:space:]]*alias[[:space:]]+am=['\"]?([^'\"[:space:];]+).*/\1/p" \
    -e "s/^[[:space:]]*alias[[:space:]]+am[[:space:]]+['\"]?([^'\"[:space:];]+).*/\1/p" \
    | head -1)
  [ -n "$candidate" ] || return 1

  path_resolves_to_installed_am "$candidate"
}

interactive_shell_descriptor_matches_installed_am() {
  local descriptor="$1"
  [ -n "$descriptor" ] || return 1

  if printf '%s\n' "$descriptor" | grep -Fq "$DEST/$BIN_CLI"; then
    return 0
  fi

  local line candidate
  while IFS= read -r line; do
    candidate=""
    case "$line" in
      "am is an alias for "*) candidate="${line#am is an alias for }" ;;
      "am is aliased to "*) candidate="${line#am is aliased to }" ;;
      "am is "*) candidate="${line#am is }" ;;
    esac
    [ -n "$candidate" ] || continue

    candidate="${candidate#\'}"
    candidate="${candidate%\'}"
    candidate="${candidate#\"}"
    candidate="${candidate%\"}"
    case "$candidate" in
      /*)
        if path_resolves_to_installed_am "$candidate"; then
          return 0
        fi
        ;;
    esac
  done <<< "$descriptor"

  return 1
}

existing_rust_binaries_are_skip_safe() {
  EXISTING_INSTALL_REPAIR_REASON=""

  if [ ! -x "$DEST/$BIN_CLI" ]; then
    EXISTING_INSTALL_REPAIR_REASON="$DEST/$BIN_CLI is missing or not executable"
    return 1
  fi
  if [ ! -x "$DEST/$BIN_SERVER" ]; then
    EXISTING_INSTALL_REPAIR_REASON="$DEST/$BIN_SERVER is missing or not executable"
    return 1
  fi

  local cli_help=""
  if capture_command_with_timeout 3 "$DEST/$BIN_CLI" --help; then
    cli_help="$CAPTURED_CMD_OUTPUT"
  else
    cli_help="$CAPTURED_CMD_OUTPUT"
    if [ "$CAPTURED_CMD_STATUS" -eq 124 ]; then
      EXISTING_INSTALL_REPAIR_REASON="'$DEST/$BIN_CLI --help' timed out"
      return 1
    fi
  fi
  if ! printf '%s\n' "$cli_help" | grep -qE '(^|[[:space:]])serve-http([[:space:]]|$)'; then
    EXISTING_INSTALL_REPAIR_REASON="'$DEST/$BIN_CLI --help' is missing the expected CLI surface"
    return 1
  fi

  local server_help=""
  if capture_command_with_timeout 3 "$DEST/$BIN_SERVER" --help; then
    server_help="$CAPTURED_CMD_OUTPUT"
  else
    server_help="$CAPTURED_CMD_OUTPUT"
    if [ "$CAPTURED_CMD_STATUS" -eq 124 ]; then
      EXISTING_INSTALL_REPAIR_REASON="'$DEST/$BIN_SERVER --help' timed out"
      return 1
    fi
  fi
  if ! printf '%s\n' "$server_help" | grep -qE '^Usage: mcp-agent-mail ' || \
     ! printf '%s\n' "$server_help" | grep -qE '(^|[[:space:]])serve([[:space:]]|$)'; then
    EXISTING_INSTALL_REPAIR_REASON="'$DEST/$BIN_SERVER --help' is missing the expected server surface"
    return 1
  fi

  local actual_resolution=""
  actual_resolution=$(interactive_shell_am_descriptor)
  if [ -z "$actual_resolution" ] || [ "$actual_resolution" = "NOT_FOUND" ]; then
    EXISTING_INSTALL_REPAIR_REASON="interactive shell cannot resolve 'am'"
    return 1
  fi
  if ! interactive_shell_descriptor_matches_installed_am "$actual_resolution"; then
    if printf '%s\n' "$actual_resolution" | grep -qiE 'alias|function'; then
      EXISTING_INSTALL_REPAIR_REASON="interactive shell still resolves 'am' via ${actual_resolution}"
    else
      EXISTING_INSTALL_REPAIR_REASON="interactive shell resolves 'am' to ${actual_resolution}"
    fi
    return 1
  fi

  return 0
}

existing_install_can_skip() {
  EXISTING_INSTALL_REPAIR_REASON=""

  if [ "$PYTHON_DETECTED" -eq 1 ] && [ "$FORCE_NO_MIGRATE" -eq 0 ]; then
    local python_shadow_active=0
    if [ "$PYTHON_ALIAS_FOUND" -eq 1 ] && ! python_alias_points_to_installed_am; then
      python_shadow_active=1
    fi
    [ -n "${PYTHON_PID:-}" ] && python_shadow_active=1
    if [ "$PYTHON_BINARY_FOUND" -eq 1 ]; then
      case "${PYTHON_BINARY_PATH:-}" in
        python\ *|python3\ *|*"-m mcp_agent_mail"*) ;;
        *) python_shadow_active=1 ;;
      esac
    fi

    if [ "$python_shadow_active" -eq 1 ] || [ ! -f "$PYTHON_MIGRATION_MARKER" ]; then
      EXISTING_INSTALL_REPAIR_REASON="legacy Python installation is still present and takeover/displacement has not been re-run"
      return 1
    fi
    verbose "existing_install_can_skip:legacy clone remains but migration marker is present and no active Python launcher shadows Rust"
  fi

  existing_rust_binaries_are_skip_safe
}

usage() {
  cat <<EOFU
Usage: install.sh [--version vX.Y.Z] [--dest DIR] [--system] [--easy-mode] [--verify] \\
                  [--artifact-url URL] [--checksum HEX] [--checksum-url URL] [--quiet] \\
                  [--offline] [--no-gum] [--no-verify] [--force] [--from-source] [--verbose] \\
                  [--migrate|--no-migrate] [--uninstall] [--yes] [--purge] [--dry-run]

Installs mcp-agent-mail and am (CLI) binaries.

Options:
  --version vX.Y.Z   Install specific version (default: latest)
  --dest DIR         Install to DIR (default: ~/.local/bin)
  --system           Install to /usr/local/bin (requires sudo)
  --easy-mode        Auto-update PATH in shell rc files
  --no-easy          Do not auto-update PATH in shell rc files
  --verify           Run self-test after install
  --from-source      Build from source instead of downloading binary
  --quiet            Suppress non-error output
  --verbose          Enable detailed installer diagnostics
  --offline          Skip network preflight checks
  --no-gum           Disable gum formatting even if available
  --no-verify        Skip checksum + signature verification (for testing only)
  --force            Force reinstall even if same version is installed
  --migrate          Force Python->Rust migration/displacement when Python install is detected
  --no-migrate       Skip Python->Rust migration/displacement even if Python install is detected
  --uninstall        Remove installed binaries/configuration helpers
  --yes              Non-interactive mode (skip all confirmations)
  --purge            With --uninstall, also delete storage/database data
  --dry-run          Preview what the installer would do without making changes
  --preview          Alias for --dry-run
EOFU
}

trap 'on_error $LINENO' ERR
trap early_exit_dump EXIT
init_verbose_log
verbose "argv=${ORIGINAL_ARGS[*]:-(none)}"

while [ $# -gt 0 ]; do
  case "$1" in
    --version)
      if [ $# -lt 2 ]; then
        err "Option --version requires a value"
        error_usage_hint
        dump_verbose_tail
        exit 2
      fi
      VERSION="$2"; shift 2;;
    --dest)
      if [ $# -lt 2 ]; then
        err "Option --dest requires a value"
        error_usage_hint
        dump_verbose_tail
        exit 2
      fi
      DEST="$2"; shift 2;;
    --system) SYSTEM=1; DEST="/usr/local/bin"; shift;;
    --easy-mode) EASY=1; shift;;
    --no-easy) EASY=0; shift;;
    --verify) VERIFY=1; shift;;
    --artifact-url)
      if [ $# -lt 2 ]; then
        err "Option --artifact-url requires a value"
        error_usage_hint
        dump_verbose_tail
        exit 2
      fi
      ARTIFACT_URL="$2"; shift 2;;
    --checksum)
      if [ $# -lt 2 ]; then
        err "Option --checksum requires a value"
        error_usage_hint
        dump_verbose_tail
        exit 2
      fi
      CHECKSUM="$2"; shift 2;;
    --checksum-url)
      if [ $# -lt 2 ]; then
        err "Option --checksum-url requires a value"
        error_usage_hint
        dump_verbose_tail
        exit 2
      fi
      CHECKSUM_URL="$2"; shift 2;;
    --from-source) FROM_SOURCE=1; shift;;
    --quiet|-q) QUIET=1; shift;;
    --verbose) VERBOSE=1; shift;;
    --offline) OFFLINE=1; shift;;
    --no-gum) NO_GUM=1; shift;;
    --no-verify) NO_CHECKSUM=1; shift;;
    --force) FORCE_INSTALL=1; shift;;
    --migrate) FORCE_MIGRATE=1; shift;;
    --no-migrate) FORCE_NO_MIGRATE=1; shift;;
    --uninstall) UNINSTALL=1; shift;;
    --yes|-y) ASSUME_YES=1; shift;;
    --purge) PURGE=1; shift;;
    --dry-run|--preview) DRY_RUN=1; shift;;
    -h|--help) usage; exit 0;;
    *)
      err "Unknown option: $1"
      error_usage_hint
      exit 2
      ;;
  esac
done

if [ "$FORCE_MIGRATE" -eq 1 ] && [ "$FORCE_NO_MIGRATE" -eq 1 ]; then
  err "Cannot combine --migrate and --no-migrate"
  err "Choose one behavior: --migrate (force migration) OR --no-migrate (skip migration)."
  error_usage_hint
  exit 2
fi

verbose "config VERSION=${VERSION:-latest} DEST=${DEST} SYSTEM=${SYSTEM} EASY=${EASY} VERIFY=${VERIFY} FROM_SOURCE=${FROM_SOURCE} QUIET=${QUIET} VERBOSE=${VERBOSE} OFFLINE=${OFFLINE} FORCE_INSTALL=${FORCE_INSTALL} FORCE_MIGRATE=${FORCE_MIGRATE} FORCE_NO_MIGRATE=${FORCE_NO_MIGRATE} UNINSTALL=${UNINSTALL} ASSUME_YES=${ASSUME_YES} PURGE=${PURGE} DRY_RUN=${DRY_RUN}"

if [ "$UNINSTALL" -eq 1 ]; then
  uninstall
  exit 0
fi

# Show fancy header
if [ "$QUIET" -eq 0 ]; then
  if [ "$HAS_GUM" -eq 1 ] && [ "$NO_GUM" -eq 0 ]; then
    gum style \
      --border normal \
      --border-foreground 39 \
      --padding "0 1" \
      --margin "1 0" \
      "$(gum style --foreground 42 --bold 'mcp-agent-mail installer')" \
      "$(gum style --foreground 245 'Multi-agent coordination via MCP')"
  else
    echo ""
    echo -e "\033[1;32mmcp-agent-mail installer\033[0m"
    echo -e "\033[0;90mMulti-agent coordination via MCP\033[0m"
    echo ""
  fi
fi

resolve_version
detect_platform
set_artifact_url

# Ensure the destination directory hierarchy exists before preflight checks
mkdir -p "$DEST" 2>/dev/null || true

preflight_checks

# Detect existing Python installation (T1.1, T1.2, T1.3)
detect_python

# Check if already at target version (skip download if so, unless --force)
if [ "$FORCE_INSTALL" -eq 0 ] && check_installed_version "$VERSION"; then
  if existing_install_can_skip; then
    ok "mcp-agent-mail $VERSION is already installed at $DEST"
    info "Use --force to reinstall"
    exit 0
  fi

  warn "Installed version matches $VERSION, but the existing install still needs repair."
  [ -n "$EXISTING_INSTALL_REPAIR_REASON" ] && warn "  Reason: $EXISTING_INSTALL_REPAIR_REASON"
  info "Continuing with reinstall/remediation instead of exiting early."
fi

# ── Install plan preview / dry-run / piped confirmation ─────────────────────

print_install_plan() {
  local header_color="1;36"
  local section_color="1;33"

  echo ""
  echo -e "\033[${header_color}m=== Installation Plan ===\033[0m"
  echo ""

  # Section 1: Binaries
  echo -e "\033[${section_color}m[Binaries]\033[0m"
  echo "  Version:    ${VERSION}"
  echo "  Target:     ${TARGET:-source build}"
  echo "  Dest:       $DEST"
  echo "  Install:    $DEST/$BIN_SERVER (MCP server)"
  echo "              $DEST/$BIN_CLI (CLI tool)"
  if [ "$FROM_SOURCE" -eq 1 ]; then
    echo "  Method:     Build from source"
  else
    echo "  Method:     Download pre-built binary"
    [ -n "${URL:-}" ] && echo "  URL:        $URL"
  fi
  echo ""

  # Section 2: PATH changes
  echo -e "\033[${section_color}m[PATH]\033[0m"
  local dest_in_path=0
  case ":$PATH:" in
    *:"$DEST":*) dest_in_path=1 ;;
  esac
  if [ "$dest_in_path" -eq 1 ]; then
    echo "  $DEST is already in PATH (no changes needed)"
  elif [ "$EASY" -eq 1 ]; then
    for rc in "$HOME/.zshrc" "$HOME/.bashrc"; do
      if [ -e "$rc" ] && [ -w "$rc" ]; then
        if ! grep -qF "export PATH=\"$DEST:\$PATH\"" "$rc" 2>/dev/null; then
          echo "  Will append PATH export to: $rc"
        else
          echo "  PATH export already present in: $rc"
        fi
      fi
    done
  else
    echo "  $DEST is NOT in PATH (manual action needed)"
  fi
  echo ""

  # Section 3: Python migration
  if [ "$PYTHON_DETECTED" -eq 1 ]; then
    local _plan_marker="$HOME/.config/mcp-agent-mail/.python-migration-complete"
    if [ -f "$_plan_marker" ] && [ "$FORCE_MIGRATE" -ne 1 ]; then
      echo -e "\033[${section_color}m[Python Migration]\033[0m"
      echo "  Already migrated — skipping (marker: $_plan_marker)"
      echo ""
    else
      echo -e "\033[${section_color}m[Python Migration]\033[0m"
      [ "$PYTHON_ALIAS_FOUND" -eq 1 ] && echo "  Will disable alias in:  $PYTHON_ALIAS_FILE (line $PYTHON_ALIAS_LINE)"
      [ "$PYTHON_BINARY_FOUND" -eq 1 ] && echo "  Will displace binary:   $PYTHON_BINARY_PATH"
      [ -n "$PYTHON_PID" ] && echo "  Will stop Python server: PID $PYTHON_PID"
      [ "$PYTHON_CLONE_FOUND" -eq 1 ] && echo "  Python clone detected:  $PYTHON_CLONE_PATH (not modified)"
      echo ""
    fi
  fi

  # Section 4: MCP config
  local mcp_scan
  mcp_scan="$(detect_mcp_configs "$PWD" 2>/dev/null || true)"
  if [ -n "$mcp_scan" ]; then
    echo -e "\033[${section_color}m[MCP Configurations]\033[0m"
    local tool path exists_flag
    while IFS=$'\t' read -r tool path exists_flag; do
      [ -z "${tool:-}" ] && continue
      if [ "$exists_flag" = "1" ]; then
        echo "  Will update: [$tool] $path"
      else
        local parent_dir
        parent_dir=$(dirname "$path")
        local grandparent_dir
        grandparent_dir=$(dirname "$parent_dir")
        if [ -d "$parent_dir" ] || [ -d "$grandparent_dir" ]; then
          echo "  Will create: [$tool] $path"
        fi
      fi
    done <<< "$mcp_scan"
    echo ""
  fi

  # Section 5: Remote HTTP readiness for Codex/other HTTP MCP clients
  if has_remote_http_client_targets; then
    echo -e "\033[${section_color}m[Remote HTTP]\033[0m"
    echo "  Connect URL: $(desired_mcp_http_url)"
    echo "  Will verify the local MCP HTTP endpoint after install"
    if platform_supports_user_service_management; then
      echo "  If needed, will install/start a background per-user service automatically"
    else
      echo "  Automatic background service management is not supported on this platform"
    fi
    echo ""
  fi

  # Section 6: Verification
  if [ "$VERIFY" -eq 1 ]; then
    echo -e "\033[${section_color}m[Post-install]\033[0m"
    echo "  Will run verification checks after installation"
    echo ""
  fi
}

# Dry-run mode: show plan and exit
if [ "$DRY_RUN" -eq 1 ]; then
  print_install_plan
  echo -e "\033[1;36m=== Dry run complete (no changes made) ===\033[0m"
  echo ""
  exit 0
fi

# Piped install (EASY=1) confirmation: show plan and ask before proceeding,
# unless --yes was passed or stdin is not a terminal (true pipe).
if [ "$EASY" -eq 1 ] && [ "$ASSUME_YES" -eq 0 ] && [ -t 1 ] && [ -e /dev/tty ]; then
  print_install_plan
  printf "Proceed with installation? [Y/n] "
  read -r confirm </dev/tty 2>/dev/null || confirm="y"
  case "$confirm" in
    [nN]*)
      info "Installation cancelled."
      exit 0
      ;;
  esac
fi

# Cross-platform locking using mkdir (atomic on all POSIX systems)
LOCK_DIR="${LOCK_FILE}.d"
LOCKED=0
if mkdir "$LOCK_DIR" 2>/dev/null; then
  LOCKED=1
  echo $$ > "$LOCK_DIR/pid"
else
  if [ -f "$LOCK_DIR/pid" ]; then
    OLD_PID=$(cat "$LOCK_DIR/pid" 2>/dev/null || echo "")
    if [ -n "$OLD_PID" ] && ! kill -0 "$OLD_PID" 2>/dev/null; then
      rm -rf "$LOCK_DIR"
      if mkdir "$LOCK_DIR" 2>/dev/null; then
        LOCKED=1
        echo $$ > "$LOCK_DIR/pid"
      fi
    fi
  fi
  if [ "$LOCKED" -eq 0 ]; then
    err "Another installer is running (lock $LOCK_DIR)"
    err "Wait for the other install to finish, or remove a stale lock after confirming no installer is active."
    err "Check lock owner with: cat \"$LOCK_DIR/pid\" && ps -p \"\$(cat \"$LOCK_DIR/pid\" 2>/dev/null)\""
    err "Stale-lock cleanup: rmdir \"$LOCK_DIR\""
    exit 1
  fi
fi

cleanup() {
  local rc=$?
  [ -n "${TMP:-}" ] && rm -rf "$TMP"
  if [ "${LOCKED:-0}" -eq 1 ]; then
    rm -rf "${LOCK_DIR:-}"
  fi
  if [ "$rc" -ne 0 ]; then
    dump_verbose_tail
  fi
  return "$rc"
}

TMP=$(mktemp -d)
trap cleanup EXIT

if [ "$FROM_SOURCE" -eq 0 ]; then
  info "Downloading $URL"
  if ! download_to_file "$URL" "$TMP/$TAR" "binary-download"; then
    # If we preferred a musl artifact that isn't published for this release
    # (older tags only shipped gnu), fall back to the gnu artifact before
    # giving up and attempting a source build.
    if [ "$TARGET" = "x86_64-unknown-linux-musl" ] && [ -z "${ARTIFACT_URL:-}" ]; then
      warn "musl artifact not available for $VERSION; falling back to gnu artifact"
      verbose "binary-download:musl_fallback_to_gnu version=${VERSION} url=${URL}"
      TARGET="x86_64-unknown-linux-gnu"
      TAR="mcp-agent-mail-${TARGET}.tar.xz"
      URL="https://github.com/${OWNER}/${REPO}/releases/download/${VERSION}/${TAR}"
      info "Downloading $URL"
      if ! download_to_file "$URL" "$TMP/$TAR" "binary-download"; then
        warn "Binary download failed (release may not exist for $VERSION)"
        warn "Attempting build from source as fallback..."
        verbose "binary-download:fallback_to_source version=${VERSION} url=${URL}"
        FROM_SOURCE=1
      fi
    else
      warn "Binary download failed (release may not exist for $VERSION)"
      warn "Attempting build from source as fallback..."
      verbose "binary-download:fallback_to_source version=${VERSION} url=${URL}"
      FROM_SOURCE=1
    fi
  fi
fi

if [ "$FROM_SOURCE" -eq 1 ]; then
  info "Building from source (requires git, rust nightly, and all local dependencies)"
  ensure_rust
  git clone --depth 1 "https://github.com/${OWNER}/${REPO}.git" "$TMP/src"

  # Check for local dependency paths required by [patch.crates-io] in Cargo.toml.
  # These exist only on the project's build server; external users must use pre-built binaries.
  if [ ! -d "/dp/asupersync" ]; then
    err "Build from source requires local dependency checkouts under /dp/ that are"
    err "only available on the project build server."
    err ""
    err "For end-user installation, use pre-built release binaries:"
    err "  curl -fsSL ${INSTALL_SCRIPT_URL} | bash"
    err ""
    err "If no release exists yet, check https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases"
    exit 1
  fi

  if ! (cd "$TMP/src" && cargo build --release -p mcp-agent-mail -p mcp-agent-mail-cli); then
    err "Build failed. Check compiler output above for details."
    error_support_hint
    exit 1
  fi
  local_server="$TMP/src/target/release/$BIN_SERVER"
  local_cli="$TMP/src/target/release/$BIN_CLI"
  [ -x "$local_server" ] || {
    err "Build failed: $BIN_SERVER not found"
    err "Retry with --verbose and ensure cargo build completed successfully."
    error_support_hint
    exit 1
  }
  [ -x "$local_cli" ] || {
    err "Build failed: $BIN_CLI not found"
    err "Retry with --verbose and ensure cargo build completed successfully."
    error_support_hint
    exit 1
  }
  atomic_install "$local_server" "$DEST/$BIN_SERVER"
  atomic_install "$local_cli" "$DEST/$BIN_CLI"
  ok "Installed to $DEST (source build)"
  ok "  $DEST/$BIN_SERVER"
  ok "  $DEST/$BIN_CLI"
  maybe_add_path
  install_local_bin_links
  if [ "$VERIFY" -eq 1 ]; then
    "$DEST/$BIN_CLI" --version || true
    ok "Self-test complete"
  fi
  exit 0
fi

# Checksum verification (can be skipped with --no-verify for testing)
if [ "$NO_CHECKSUM" -eq 1 ]; then
  warn "Verification skipped (--no-verify)"
else
  if [ -z "$CHECKSUM" ]; then
    CHECKSUM_FILE="$TMP/checksum.sha256"
    CHECKSUM_RESOLVED=0

    # Strategy 1: explicit checksum URL when the caller supplied one.
    if [ -n "$CHECKSUM_URL" ]; then
      info "Fetching checksum from ${CHECKSUM_URL}"
      if download_to_file "$CHECKSUM_URL" "$CHECKSUM_FILE" "checksum-download" && [ -f "$CHECKSUM_FILE" ]; then
        CHECKSUM=$(awk '{print $1}' "$CHECKSUM_FILE")
        if [ -n "$CHECKSUM" ]; then
          CHECKSUM_RESOLVED=1
          verbose "checksum:resolved_via_explicit_url sha256=${CHECKSUM}"
        fi
      fi
    fi

    # Strategy 2: consolidated SHA256SUMS file from release (default path).
    if [ "$CHECKSUM_RESOLVED" -eq 0 ]; then
      SHA256SUMS_URL="$(dirname "$URL")/SHA256SUMS"
      SHA256SUMS_FILE="$TMP/SHA256SUMS"
      verbose "checksum:trying_sha256sums url=${SHA256SUMS_URL}"
      info "Fetching checksum manifest from ${SHA256SUMS_URL}"
      if download_to_file "$SHA256SUMS_URL" "$SHA256SUMS_FILE" "sha256sums-download" && [ -f "$SHA256SUMS_FILE" ]; then
        CHECKSUM=$(awk -v artifact="$TAR" '$2 == artifact || $2 == ("./" artifact) || $2 == ("*" artifact) {print $1; exit}' "$SHA256SUMS_FILE")
        if [ -n "$CHECKSUM" ]; then
          CHECKSUM_RESOLVED=1
          verbose "checksum:resolved_via_SHA256SUMS sha256=${CHECKSUM}"
        else
          verbose "checksum:SHA256SUMS_no_match artifact=${TAR}"
          warn "SHA256SUMS file found but no entry for ${TAR}"
        fi
      fi
    fi

    # Strategy 3: per-artifact .sha256 sidecar (older/manual release layouts).
    if [ "$CHECKSUM_RESOLVED" -eq 0 ] && [ -z "$CHECKSUM_URL" ]; then
      CHECKSUM_URL="${URL}.sha256"
      verbose "checksum:trying_sidecar url=${CHECKSUM_URL}"
      info "Trying per-artifact checksum sidecar ${CHECKSUM_URL}"
      if download_to_file "$CHECKSUM_URL" "$CHECKSUM_FILE" "checksum-download" && [ -f "$CHECKSUM_FILE" ]; then
        CHECKSUM=$(awk '{print $1}' "$CHECKSUM_FILE")
        if [ -n "$CHECKSUM" ]; then
          CHECKSUM_RESOLVED=1
          verbose "checksum:resolved_via_sidecar sha256=${CHECKSUM}"
        fi
      fi
    fi

    if [ "$CHECKSUM_RESOLVED" -eq 0 ]; then
      warn "Checksum file not available; skipping verification"
      warn "Use --checksum <hex> to provide one manually"
      CHECKSUM=""
    fi
  fi

  if [ -n "$CHECKSUM" ]; then
    if ! verify_checksum "$TMP/$TAR" "$CHECKSUM"; then
      err "Installation aborted due to checksum failure"
      err "Re-run the installer to fetch a fresh artifact and checksum."
      exit 1
    fi
  fi

  if ! verify_sigstore_bundle "$TMP/$TAR" "$URL"; then
    err "Signature verification failed"
    err "The downloaded file may be corrupted or tampered with."
    err "Retry with a fresh download, or use --no-verify only for trusted local testing."
    error_support_hint
    exit 1
  fi
fi

info "Extracting"
tar -xf "$TMP/$TAR" -C "$TMP"

# Nested-archive detection: some releases accidentally nest a .tar.gz inside
# the outer .tar.xz.  If we find one after the first extraction, unpack it too.
shopt -s nullglob
for nested in "$TMP"/*.tar.gz "$TMP"/mcp-agent-mail-*/*.tar.gz; do
  if [ -f "$nested" ]; then
    verbose "extract:nested_archive detected=${nested}"
    warn "Nested archive detected (${nested##*/}); extracting inner archive"
    tar -xzf "$nested" -C "$(dirname "$nested")"
    rm -f "$nested"
  fi
done
for nested in "$TMP"/*.tar.xz "$TMP"/mcp-agent-mail-*/*.tar.xz; do
  # Skip the original download artifact itself
  [ "$nested" = "$TMP/$TAR" ] && continue
  if [ -f "$nested" ]; then
    verbose "extract:nested_archive detected=${nested}"
    warn "Nested archive detected (${nested##*/}); extracting inner archive"
    tar -xJf "$nested" -C "$(dirname "$nested")"
    rm -f "$nested"
  fi
done
shopt -u nullglob

# Find binaries in the extracted archive
find_bin() {
  local name="$1"
  local bin="$TMP/$name"
  if [ -x "$bin" ]; then echo "$bin"; return 0; fi
  bin="$TMP/mcp-agent-mail-${TARGET}/$name"
  if [ -x "$bin" ]; then echo "$bin"; return 0; fi
  bin=$(find "$TMP" -maxdepth 3 -type f -name "$name" -perm -111 | head -n 1)
  if [ -x "$bin" ]; then echo "$bin"; return 0; fi
  return 1
}

SERVER_BIN=$(find_bin "$BIN_SERVER") || {
  err "Binary $BIN_SERVER not found in archive"
  err "The release artifact may be malformed or incomplete."
  err "Re-run installer with a cache buster or pin a different --version."
  error_support_hint
  exit 1
}
CLI_BIN=$(find_bin "$BIN_CLI") || {
  err "Binary $BIN_CLI not found in archive"
  err "The release artifact may be malformed or incomplete."
  err "Re-run installer with a cache buster or pin a different --version."
  error_support_hint
  exit 1
}

atomic_install "$SERVER_BIN" "$DEST/$BIN_SERVER"
atomic_install "$CLI_BIN" "$DEST/$BIN_CLI"
ok "Installed to $DEST"
ok "  $DEST/$BIN_SERVER"
ok "  $DEST/$BIN_CLI"
maybe_add_path
install_local_bin_links

if detect_mac_direct_exec_compat_mode; then
  warn "macOS direct execution compatibility mode enabled"
  warn "  Reason: ${MAC_DIRECT_EXEC_COMPAT_REASON}"
  if install_mac_python_cli_compat_launcher && install_mac_python_cli_shell_alias; then
    ok "Installed Python compatibility launcher for interactive 'am' usage"
  else
    err "Failed to install the Python compatibility launcher for this macOS host"
    error_support_hint
    exit 1
  fi
fi

# T5.0: Check if Python→Rust migration was already completed on a previous run.
# A marker file records that the database was successfully migrated, so we never
# attempt to overwrite the live Rust database with the old Python snapshot again.
PYTHON_MIGRATION_ALREADY_DONE=0
if [ -f "$PYTHON_MIGRATION_MARKER" ]; then
  PYTHON_MIGRATION_ALREADY_DONE=1
  verbose "python_migration_marker:found path=${PYTHON_MIGRATION_MARKER}"
fi

# Displace Python installation if detected (T2.2)
if [ "$PYTHON_DETECTED" -eq 1 ]; then
  if [ "$PYTHON_MIGRATION_ALREADY_DONE" -eq 1 ] && [ "$FORCE_MIGRATE" -ne 1 ]; then
    MIGRATE_PYTHON=0
    info "Python installation detected but migration was already completed previously."
    info "  Marker: $PYTHON_MIGRATION_MARKER"
    info "  To force re-migration: re-run with --migrate"
    # Still displace alias/binary so `am` resolves to Rust, but skip DB migration.
    # Do NOT call stop_python_server here — if migration is already done, the
    # running server (if any) is the Rust server, not Python.
    if [ "$MAC_DIRECT_EXEC_COMPAT_MODE" -eq 0 ]; then
      displace_python_alias 2>/dev/null || true
      displace_python_binary 2>/dev/null || true
    fi
  elif [ "$MAC_DIRECT_EXEC_COMPAT_MODE" -eq 1 ]; then
    MIGRATE_PYTHON=0
    warn "Keeping the existing Python installation active because this macOS host blocks direct execution of the newly installed Rust binaries."
  else
    MIGRATE_PYTHON=1
    if [ "$FORCE_NO_MIGRATE" -eq 1 ]; then
      MIGRATE_PYTHON=0
      warn "Skipping Python displacement due to --no-migrate."
    elif [ "$FORCE_MIGRATE" -eq 1 ]; then
      MIGRATE_PYTHON=1
      info "Forcing Python displacement due to --migrate."
    elif [ "${ASSUME_YES:-0}" -eq 1 ]; then
      MIGRATE_PYTHON=1
      info "Auto-accepting Python→Rust migration (--yes)."
    elif [ "$EASY" -eq 0 ] && [ -t 0 ]; then
      # Interactive mode: ask the user
      echo ""
      info "An existing Python mcp-agent-mail installation was detected."
      info "The Rust binary has been installed. To ensure 'am' resolves to the"
      info "new Rust version, the Python alias/binary should be displaced."
      echo ""
      printf "%s" "Migrate from Python to Rust? [Y/n] "
      read -r answer </dev/tty 2>/dev/null || answer="y"
      case "$answer" in
        [nN]*)
          MIGRATE_PYTHON=0
          warn "Skipping Python displacement."
          if [ "$PYTHON_ALIAS_FOUND" -eq 1 ]; then
            warn "The shell alias 'am' still points to the Python version."
            warn "The Rust binary is available as: $DEST/$BIN_CLI"
            warn "To use Rust: remove the alias from $PYTHON_ALIAS_FILE or run:"
            warn "  $DEST/$BIN_CLI <command>"
          fi
          ;;
      esac
    fi

    if [ "$MIGRATE_PYTHON" -eq 1 ]; then
      stop_python_server
      if ! displace_python_alias; then
        err "Failed to fully displace legacy 'am' alias/function definitions."
        err "Please remove remaining alias/function manually, then rerun installer."
        err "You can still use the Rust binary directly at: $DEST/$BIN_CLI"
        error_support_hint
        exit 1
      fi
      if ! displace_python_binary; then
        err "Failed to displace legacy 'am' launcher in PATH."
        err "Please remove or rename the legacy launcher manually, then rerun installer."
        err "You can still use the Rust binary directly at: $DEST/$BIN_CLI"
        error_support_hint
        exit 1
      fi
      resolve_database_path
      # Only migrate env config if resolve_database_path actually copied a Python DB.
      # PYTHON_DB_MIGRATED_PATH is set only when a DB was copied and needs migration.
      if [ -n "$PYTHON_DB_MIGRATED_PATH" ]; then
        migrate_env_config
      else
        # No Python DB to migrate — alias/binary displacement was the entire migration.
        # Write the marker now so future runs skip the migration prompt entirely.
        # (Without this, the clone directory still triggers PYTHON_DETECTED=1 and the
        # user is prompted on every `acfs update` — see GitHub issue #258.)
        if [ ! -f "$PYTHON_MIGRATION_MARKER" ]; then
          mkdir -p "$(dirname "$PYTHON_MIGRATION_MARKER")" 2>/dev/null || true
          printf 'migrated_at=%s\nnote=no Python DB found; alias/binary displacement only\ninstaller_version=%s\n' \
            "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
            "${VERSION:-unknown}" \
            > "$PYTHON_MIGRATION_MARKER" 2>/dev/null || true
          verbose "python_migration_marker:written path=${PYTHON_MIGRATION_MARKER} (no-db displacement-only)"
          ok "Migration marker saved (no Python database found; alias/binary displaced)"
        fi
      fi
    fi
  fi
fi

MCP_CONFIG_SCAN="$(detect_mcp_configs "$PWD" || true)"
if [ "$QUIET" -eq 0 ] && [ -n "$MCP_CONFIG_SCAN" ]; then
  SHOWN_MCP_CONFIGS=0
  while IFS=$'\t' read -r tool path exists_flag; do
    [ -z "${tool:-}" ] && continue
    if [ "${AM_INSTALL_LIST_ALL_MCP_CONFIGS:-0}" != "1" ] && [ "$exists_flag" != "1" ]; then
      continue
    fi
    if [ "$SHOWN_MCP_CONFIGS" -eq 0 ]; then
      info "Detected MCP config files"
    fi
    SHOWN_MCP_CONFIGS=$((SHOWN_MCP_CONFIGS + 1))
    if [ "$exists_flag" = "1" ]; then
      ok "[$tool] $path"
    else
      info "[$tool] $path (missing)"
    fi
  done <<< "$MCP_CONFIG_SCAN"
fi

# Set up MCP configs for fresh installs (non-interactive, auto-detect).
# Codex is written directly in HTTP URL mode here so the one-liner does not
# depend on a particular released `am setup` implementation.
if [ "${AM_INSTALL_SKIP_MCP_SETUP:-0}" != "1" ] && [ "$MAC_DIRECT_EXEC_COMPAT_MODE" -eq 0 ]; then
  setup_mcp_configs "$DEST/$BIN_SERVER"
fi

# Update existing MCP configs via the newly-installed `am setup run`.
# This still handles the broader non-Codex migration work:
#   - Python→Rust command rewriting
#   - env var preservation (bearer token, storage root)
#   - BOM/JSONC/trailing-comma tolerance
#   - Backup before modification
if [ "${AM_INSTALL_SKIP_MCP_SETUP:-0}" != "1" ] && [ "$MAC_DIRECT_EXEC_COMPAT_MODE" -eq 0 ]; then
  update_mcp_configs "$DEST/$BIN_SERVER" "$DEST/$BIN_CLI"
fi

# Re-sync Codex last so an older released `am` cannot leave Codex in stdio or
# mixed transport mode after the installer has already chosen HTTP.
if [ "${AM_INSTALL_SKIP_MCP_SETUP:-0}" != "1" ] && [ "$MAC_DIRECT_EXEC_COMPAT_MODE" -eq 0 ]; then
  sync_codex_http_configs "$DEST/$BIN_SERVER"
fi

collect_migration_counts() {
  local db_path="$1"
  if [ ! -f "$db_path" ]; then
    echo "db_helper_unavailable"
    return 0
  fi
  local tables=(
    projects
    agents
    messages
    message_recipients
    file_reservations
    agent_links
    message_summaries
    product_project_links
  )
  local table count summary=""
  for table in "${tables[@]}"; do
    count=$(installed_am_db_query_scalar "$db_path" "SELECT COUNT(*) FROM ${table};" 2>/dev/null || echo "na")
    summary+="${table}=${count};"
  done
  echo "$summary"
}

migration_count_value_from_summary() {
  local summary="$1"
  local table="$2"
  if [ -z "$summary" ] || [ "$summary" = "db_helper_unavailable" ]; then
    echo "na"
    return 0
  fi
  printf "%s" "$summary" | tr ';' '\n' | awk -F= -v key="$table" '
    $1 == key { print $2; found=1; exit }
    END { if (!found) print "na" }
  '
}

migration_core_counts_preserved() {
  local before_summary="$1"
  local after_summary="$2"
  local mode="${3:-strict}"
  local core_tables=(
    projects
    agents
    messages
    message_recipients
  )
  if [ "$mode" != "recovery_relaxed" ]; then
    core_tables+=(file_reservations)
  fi
  local table before after
  for table in "${core_tables[@]}"; do
    before=$(migration_count_value_from_summary "$before_summary" "$table")
    after=$(migration_count_value_from_summary "$after_summary" "$table")
    if [[ "$before" =~ ^[0-9]+$ ]]; then
      if ! [[ "$after" =~ ^[0-9]+$ ]]; then
        warn "Migration row count verification could not read a post-migration count for ${table}: before=${before} after=${after:-<missing>}"
        return 1
      fi
      if [ "$after" -lt "$before" ]; then
        warn "Migration row count regressed for ${table}: before=${before} after=${after}"
        return 1
      fi
    fi
  done
  return 0
}

extract_migration_error_line() {
  local output="$1"
  local line

  line=$(printf "%s\n" "$output" | awk '
    {
      lower = tolower($0)
    }
    lower ~ /error:|failed|panic|aborted|integrity_check|timestamp conversion failed|unknown timestamp format/ {
      print
      exit
    }
  ')
  if [ -n "$line" ]; then
    printf "%s" "$line"
    return 0
  fi

  printf "%s\n" "$output" | awk '
    NF &&
    $0 !~ /^Database format:/ &&
    $0 !~ /^Backup created:/ &&
    $0 !~ /^Converting timestamps/ &&
    $0 !~ /^Migration complete/ &&
    $0 !~ /^Migration needed:/ &&
    $0 !~ /^No migration needed/ &&
    $0 !~ /^Database does not contain migratable TEXT timestamps/ &&
    $0 !~ /^  (Converted|Skipped|NULLs|Backup|Format|Row count):/ {
      print
      exit
    }
  '
}

migration_output_has_unresolved_warnings() {
  local output="$1"
  printf "%s\n" "$output" | grep -qiE "database still contains TEXT timestamps|migration completed with errors|migration needed: run|unknown timestamp format"
}

migration_output_has_schema_instability() {
  local output="$1"
  printf "%s\n" "$output" | grep -qiE "schema migration hit sqlite engine instability|schema migration path was skipped due to backend instability"
}

sqlite_table_exists() {
  local db_path="$1"
  local table="$2"
  local exists
  exists=$(installed_am_db_query_scalar "$db_path" "SELECT 1 FROM sqlite_master WHERE type='table' AND name='${table}' LIMIT 1;" 2>/dev/null || true)
  [ "$exists" = "1" ]
}

sqlite_column_exists() {
  local db_path="$1"
  local table="$2"
  local column="$3"
  local exists
  exists=$(installed_am_db_query_scalar "$db_path" "SELECT 1 FROM pragma_table_info('${table}') WHERE name='${column}' LIMIT 1;" 2>/dev/null || true)
  [ "$exists" = "1" ]
}

sqlite_text_timestamp_columns_remaining() {
  local db_path="$1"
  local timestamp_columns=(
    "projects:created_at"
    "agents:inception_ts"
    "agents:last_active_ts"
    "messages:created_ts"
    "message_recipients:read_ts"
    "message_recipients:ack_ts"
    "file_reservations:created_ts"
    "file_reservations:expires_ts"
    "file_reservations:released_ts"
    "agent_links:created_ts"
    "agent_links:updated_ts"
    "agent_links:expires_ts"
    "products:created_at"
    "product_project_links:created_at"
  )
  local remaining="" pair table column detected

  if [ ! -f "$db_path" ]; then
    printf ''
    return 0
  fi

  for pair in "${timestamp_columns[@]}"; do
    table="${pair%%:*}"
    column="${pair##*:}"
    sqlite_table_exists "$db_path" "$table" || continue
    sqlite_column_exists "$db_path" "$table" "$column" || continue
    detected=$(installed_am_db_query_scalar "$db_path" "SELECT 1 FROM ${table} WHERE typeof(${column}) = 'text' LIMIT 1;" 2>/dev/null || true)
    if [ "$detected" = "1" ]; then
      if [ -n "$remaining" ]; then
        remaining="${remaining}, "
      fi
      remaining="${remaining}${table}.${column}"
    fi
  done

  printf '%s' "$remaining"
}

SQLITE_LAST_PRAGMA_FAILURE=""
SQLITE_POST_MIGRATION_FAILURES=""
SQLITE_POST_MIGRATION_REMAINING_TEXT_COLUMNS=""

sqlite_pragma_reports_ok() {
  local db_path="$1"
  local pragma="$2"
  local output="" line trimmed normalized
  local seen=0

  SQLITE_LAST_PRAGMA_FAILURE=""

  if ! installed_am_db_query_scalar "$db_path" "SELECT 1;" >/dev/null 2>&1; then
    SQLITE_LAST_PRAGMA_FAILURE="db helper unavailable"
    return 1
  fi
  if [ ! -f "$db_path" ]; then
    SQLITE_LAST_PRAGMA_FAILURE="database missing: $db_path"
    return 1
  fi

  output=$(installed_am_db_query_scalar "$db_path" "PRAGMA ${pragma};" 2>/dev/null || true)

  while IFS= read -r line; do
    trimmed=$(printf '%s' "$line" | sed -E 's/^[[:space:]]+//; s/[[:space:]]+$//')
    [ -z "$trimmed" ] && continue
    seen=1
    normalized=$(printf '%s' "$trimmed" | tr '[:upper:]' '[:lower:]')
    if [ "$normalized" != "ok" ]; then
      SQLITE_LAST_PRAGMA_FAILURE="$trimmed"
      return 1
    fi
  done <<< "$output"

  if [ "$seen" -eq 0 ]; then
    SQLITE_LAST_PRAGMA_FAILURE="<empty>"
    return 1
  fi

  return 0
}

append_sqlite_verification_failure() {
  local failure="$1"
  if [ -z "$failure" ]; then
    return 0
  fi
  if [ -n "$SQLITE_POST_MIGRATION_FAILURES" ]; then
    SQLITE_POST_MIGRATION_FAILURES="${SQLITE_POST_MIGRATION_FAILURES}; "
  fi
  SQLITE_POST_MIGRATION_FAILURES="${SQLITE_POST_MIGRATION_FAILURES}${failure}"
}

sqlite_post_migration_verify() {
  local db_path="$1"
  local before_counts="$2"
  local after_counts="$3"
  local count_mode="${4:-strict}"

  SQLITE_POST_MIGRATION_FAILURES=""
  SQLITE_POST_MIGRATION_REMAINING_TEXT_COLUMNS=""

  if ! sqlite_pragma_reports_ok "$db_path" "quick_check"; then
    append_sqlite_verification_failure "quick_check=${SQLITE_LAST_PRAGMA_FAILURE:-<empty>}"
  fi
  if ! sqlite_pragma_reports_ok "$db_path" "integrity_check"; then
    append_sqlite_verification_failure "integrity_check=${SQLITE_LAST_PRAGMA_FAILURE:-<empty>}"
  fi

  SQLITE_POST_MIGRATION_REMAINING_TEXT_COLUMNS=$(sqlite_text_timestamp_columns_remaining "$db_path")
  if [ -n "$SQLITE_POST_MIGRATION_REMAINING_TEXT_COLUMNS" ]; then
    append_sqlite_verification_failure "text_timestamps=${SQLITE_POST_MIGRATION_REMAINING_TEXT_COLUMNS}"
  fi

  if ! migration_core_counts_preserved "$before_counts" "$after_counts" "$count_mode"; then
    append_sqlite_verification_failure "core_row_counts_regressed"
  fi

  [ -z "$SQLITE_POST_MIGRATION_FAILURES" ]
}

sqlite_lightweight_self_heal() {
  local db_path="$1"
  local output=""

  if ! installed_am_db_query_scalar "$db_path" "SELECT 1;" >/dev/null 2>&1; then
    warn "FrankenSQLite DB helper is unavailable; cannot run installer structural self-heal."
    return 1
  fi
  if [ ! -f "$db_path" ]; then
    warn "SQLite structural self-heal target not found: $db_path"
    return 1
  fi

  if output=$(installed_am_db_exec "$db_path" <<'SQL' 2>&1
PRAGMA busy_timeout=60000;
PRAGMA wal_checkpoint(TRUNCATE);
REINDEX;
PRAGMA optimize;
SQL
); then
    while IFS= read -r line; do
      [ -n "$line" ] && verbose "migration:self_heal_db ${line}"
    done <<< "$output"
    ok "Applied DB checkpoint/reindex/optimize self-heal"
    return 0
  else
    warn "DB structural self-heal failed: $(printf '%s\n' "$output" | sed -n '1p')"
    while IFS= read -r line; do
      [ -n "$line" ] && verbose "migration:self_heal_db ${line}"
    done <<< "$output"
    return 1
  fi
}

installer_reconstruct_database_from_archive() {
  local db_path="$1"
  local storage_root="$2"
  local output=""

  if [ ! -x "$DEST/$BIN_CLI" ]; then
    warn "Rust CLI not found at $DEST/$BIN_CLI; cannot run archive reconstruction."
    return 1
  fi

  if output=$(AM_INTERFACE_MODE=cli DATABASE_URL="sqlite:///$db_path" STORAGE_ROOT="$storage_root" "$DEST/$BIN_CLI" doctor reconstruct --yes 2>&1); then
    while IFS= read -r line; do
      [ -n "$line" ] && verbose "migration:doctor_reconstruct ${line}"
    done <<< "$output"
    ok "Archive-backed database reconstruction completed"
    return 0
  else
    warn "Archive reconstruction failed: $(printf '%s\n' "$output" | sed -n '1p')"
    while IFS= read -r line; do
      [ -n "$line" ] && verbose "migration:doctor_reconstruct ${line}"
    done <<< "$output"
    return 1
  fi
}

INSTALLER_ARCHIVE_PARITY_FAILURE=""
installer_archive_parity_verify() {
  local db_path="$1"
  local storage_root="$2"
  local output=""
  local parse_status=0

  INSTALLER_ARCHIVE_PARITY_FAILURE=""

  if [ ! -x "$DEST/$BIN_CLI" ]; then
    warn "Rust CLI not found at $DEST/$BIN_CLI; cannot run archive parity verification."
    return 1
  fi

  if ! output=$(AM_INTERFACE_MODE=cli DATABASE_URL="sqlite:///$db_path" STORAGE_ROOT="$storage_root" "$DEST/$BIN_CLI" doctor check --json 2>&1); then
    warn "Post-migration doctor check failed: $(printf '%s\n' "$output" | sed -n '1p')"
    while IFS= read -r line; do
      [ -n "$line" ] && verbose "migration:doctor_check ${line}"
    done <<< "$output"
    return 1
  fi

  while IFS= read -r line; do
    [ -n "$line" ] && verbose "migration:doctor_check ${line}"
  done <<< "$output"

  INSTALLER_ARCHIVE_PARITY_FAILURE=$(
    INSTALLER_DOCTOR_CHECK_JSON="$output" python3 - <<'PY'
import json
import os
import sys

text = os.environ.get("INSTALLER_DOCTOR_CHECK_JSON", "")
start = text.find("{")
if start < 0:
    sys.exit(2)

try:
    payload = json.loads(text[start:])
except Exception:
    sys.exit(2)

for check in payload.get("checks", []):
    if check.get("check") == "archive_db_parity" and check.get("status") == "fail":
        print(check.get("detail") or "archive_db_parity failed")
        sys.exit(0)

sys.exit(1)
PY
  )
  parse_status=$?

  if [ "$parse_status" -eq 0 ]; then
    warn "Post-migration archive parity check failed: ${INSTALLER_ARCHIVE_PARITY_FAILURE}"
    return 1
  fi
  if [ "$parse_status" -eq 1 ]; then
    return 0
  fi

  warn "Post-migration archive parity check could not be parsed safely."
  return 1
}

installer_apply_schema_migration() {
  local db_path="$1"
  local storage_root="$2"
  local output="" output_fallback="" summary_line="" success_output="" success_label=""

  if [ ! -x "$DEST/$BIN_CLI" ]; then
    warn "Rust CLI not found at $DEST/$BIN_CLI; cannot reapply schema migration."
    return 1
  fi

  if output=$(AM_INTERFACE_MODE=cli DATABASE_URL="sqlite:///$db_path" STORAGE_ROOT="$storage_root" "$DEST/$BIN_CLI" migrate --force 2>&1); then
    success_output="$output"
    success_label="primary"
    while IFS= read -r line; do
      [ -n "$line" ] && verbose "migration:schema_refresh ${line}"
    done <<< "$output"
  elif output_fallback=$(AM_INTERFACE_MODE=cli DATABASE_URL="sqlite+aiosqlite:///$db_path" STORAGE_ROOT="$storage_root" "$DEST/$BIN_CLI" migrate --force 2>&1); then
    success_output="$output_fallback"
    success_label="fallback"
    while IFS= read -r line; do
      [ -n "$line" ] && verbose "migration:schema_refresh_primary ${line}"
    done <<< "$output"
    while IFS= read -r line; do
      [ -n "$line" ] && verbose "migration:schema_refresh_fallback ${line}"
    done <<< "$output_fallback"
  else
    summary_line=$(extract_migration_error_line "$output_fallback")
    [ -z "$summary_line" ] && summary_line=$(extract_migration_error_line "$output")
    [ -z "$summary_line" ] && summary_line=$(printf '%s\n%s\n' "$output" "$output_fallback" | sed -n '1p')
    warn "Schema refresh failed after database repair: ${summary_line:-<empty>}"
    while IFS= read -r line; do
      [ -n "$line" ] && verbose "migration:schema_refresh_primary ${line}"
    done <<< "$output"
    while IFS= read -r line; do
      [ -n "$line" ] && verbose "migration:schema_refresh_fallback ${line}"
    done <<< "$output_fallback"
    return 1
  fi

  if migration_output_has_schema_instability "$success_output" || migration_output_has_unresolved_warnings "$success_output"; then
    summary_line=$(extract_migration_error_line "$success_output")
    [ -z "$summary_line" ] && summary_line=$(printf '%s\n' "$success_output" | sed -n '1p')
    warn "Schema refresh reported unresolved warnings after database repair (${success_label}): ${summary_line:-<empty>}"
    return 1
  fi

  ok "Reapplied schema migration after database repair"
  return 0
}

sqlite_timestamp_fallback_migration() {
  local db_path="$1"
  local integrity_failure=""
  local post_heal_failure=""
  SQLITE_FALLBACK_BACKUP_PATH=""

  if ! installed_am_db_query_scalar "$db_path" "SELECT 1;" >/dev/null 2>&1; then
    warn "FrankenSQLite DB helper is unavailable; cannot run installer fallback timestamp migration."
    return 1
  fi
  if [ ! -f "$db_path" ]; then
    warn "Fallback timestamp migration target not found: $db_path"
    return 1
  fi

  # Cap backup count to prevent disk fill-up during cascading recovery.
  local existing_bak_count=0
  existing_bak_count=$(ls -1 "${db_path}.bak."* 2>/dev/null | wc -l | tr -d ' ')
  if [ "$existing_bak_count" -ge 3 ]; then
    verbose "sqlite_timestamp_fallback_migration:backup_skipped existing_count=${existing_bak_count}"
    warn "Skipping fallback backup (${existing_bak_count} backups already exist). Clean old backups to reclaim space."
  else
    # Check disk space before creating backup
    local _db_size_kb=0 _avail_kb=0
    _db_size_kb=$(du -k "$db_path" 2>/dev/null | awk '{print $1}')
    _avail_kb=$(df -Pk "$(dirname "$db_path")" 2>/dev/null | awk 'NR==2 {print $4}')
    if [ "${_avail_kb:-0}" -gt 0 ] && [ "$_avail_kb" -lt "$(( _db_size_kb * 2 ))" ]; then
      warn "Insufficient disk space for fallback backup (need ~$(( _db_size_kb * 2 / 1024 ))MB, have $(( _avail_kb / 1024 ))MB). Skipping."
    else
      local backup_ts backup_path
      backup_ts=$(date -u +%Y%m%d_%H%M%S)
      backup_path="${db_path}.bak.${backup_ts}"
      if copy_sqlite_snapshot "$db_path" "$backup_path"; then
        SQLITE_FALLBACK_BACKUP_PATH="$backup_path"
        ok "Created fallback migration backup at $backup_path"
      else
        warn "Failed to create fallback migration backup at $backup_path"
      fi
    fi
  fi

  local sql_file updates
  if command -v mktemp >/dev/null 2>&1; then
    sql_file=$(mktemp "${TMPDIR:-/tmp}/am-sqlite-fallback.XXXXXX.sql")
  else
    sql_file="${TMPDIR:-/tmp}/am-sqlite-fallback.$$.$RANDOM.sql"
    : > "$sql_file"
  fi
  updates=0

  {
    echo "PRAGMA busy_timeout=5000;"
    echo "BEGIN IMMEDIATE;"
  } > "$sql_file"

  local timestamp_columns=(
    "projects:created_at"
    "agents:inception_ts"
    "agents:last_active_ts"
    "messages:created_ts"
    "message_recipients:read_ts"
    "message_recipients:ack_ts"
    "file_reservations:created_ts"
    "file_reservations:expires_ts"
    "file_reservations:released_ts"
    "agent_links:created_ts"
    "agent_links:updated_ts"
    "agent_links:expires_ts"
    "products:created_at"
    "product_project_links:created_at"
  )
  local pair table column
  for pair in "${timestamp_columns[@]}"; do
    table="${pair%%:*}"
    column="${pair##*:}"
    sqlite_table_exists "$db_path" "$table" || continue
    sqlite_column_exists "$db_path" "$table" "$column" || continue
    cat >> "$sql_file" <<SQL
UPDATE ${table}
SET ${column} =
  CAST(strftime('%s', ${column}) AS INTEGER) * 1000000
  + CASE
      WHEN instr(${column}, '.') > 0
      THEN CAST(substr(${column} || '000000', instr(${column}, '.') + 1, 6) AS INTEGER)
      ELSE 0
    END
WHERE typeof(${column}) = 'text';
SQL
    updates=$((updates + 1))
  done

  echo "COMMIT;" >> "$sql_file"

  if ! installed_am_db_exec "$db_path" < "$sql_file" >/dev/null 2>&1; then
    warn "installer fallback timestamp migration failed."
    rm -f "$sql_file" 2>/dev/null || true
    return 1
  fi
  rm -f "$sql_file" 2>/dev/null || true

  # Ensure subsequent readers don't see stale sidecars from failed attempts.
  rm -f "${db_path}-wal" "${db_path}-shm" 2>/dev/null || true

  if ! sqlite_pragma_reports_ok "$db_path" "integrity_check"; then
    integrity_failure="${SQLITE_LAST_PRAGMA_FAILURE:-<empty>}"
    warn "installer fallback migration produced integrity_check='${integrity_failure}'"
    warn "Attempting installer fallback self-heal before escalating to archive-backed recovery."
    if sqlite_lightweight_self_heal "$db_path" && sqlite_pragma_reports_ok "$db_path" "integrity_check"; then
      ok "installer fallback self-heal cleared post-migration integrity failures"
    else
      post_heal_failure="${SQLITE_LAST_PRAGMA_FAILURE:-$integrity_failure}"
      warn "installer fallback self-heal could not clear integrity_check='${post_heal_failure}'"
      return 1
    fi
  fi

  local remaining_text_columns
  remaining_text_columns=$(sqlite_text_timestamp_columns_remaining "$db_path")
  if [ -n "$remaining_text_columns" ]; then
    warn "installer fallback left TEXT timestamps in: ${remaining_text_columns}"
    return 1
  fi

  verbose "migration:fallback_sqlite ok db=${db_path} update_statements=${updates} backup=${SQLITE_FALLBACK_BACKUP_PATH:-<none>}"
  ok "Database timestamps normalized (installer fallback)"
  return 0
}

# Run database migration if we copied a Python DB
if [ "$MAC_DIRECT_EXEC_COMPAT_MODE" -eq 0 ] && [ -n "$PYTHON_DB_MIGRATED_PATH" ] && [ -f "$PYTHON_DB_MIGRATED_PATH" ]; then
  info "Running database migration on copied Python database"
  migration_start=0
  migration_end=0
  migration_seconds=0
  migration_output=""
  migration_output_fallback=""
  migration_before_counts=""
  migration_after_counts=""
  migration_integrity=""
  migration_has_unresolved_warnings=0
  migration_requires_fallback=0
  migration_schema_refresh_failed=0
  migration_pristine_backup=""
  SQLITE_FALLBACK_BACKUP_PATH=""
  migration_restore_ok=0
  migration_fallback_ok=0
  migration_succeeded=0
  migration_final_verification_failed=0
  migration_recovery_needed=0

  migration_pristine_ts=$(date -u +%Y%m%d_%H%M%S)
  migration_pristine_backup="${PYTHON_DB_MIGRATED_PATH}.pre-migrate.${migration_pristine_ts}"

  # Check disk space and existing backup count before creating pristine snapshot
  _pre_migrate_count=0
  _pre_migrate_count=$(count_matching_backup_files "${PYTHON_DB_MIGRATED_PATH}.pre-migrate.")
  if [ "$_pre_migrate_count" -ge 3 ]; then
    migration_pristine_backup=""
    warn "Skipping pristine backup (${_pre_migrate_count} pre-migrate backups already exist)."
    warn "Clean old backups: rm ${PYTHON_DB_MIGRATED_PATH}.pre-migrate.*"
  elif ! check_copy_disk_space "$PYTHON_DB_MIGRATED_PATH" "$(dirname "$PYTHON_DB_MIGRATED_PATH")" 2; then
    migration_pristine_backup=""
    warn "Insufficient disk space for pristine migration snapshot. Skipping backup."
  elif copy_sqlite_snapshot "$PYTHON_DB_MIGRATED_PATH" "$migration_pristine_backup"; then
    verbose "migration:pristine_backup path=${migration_pristine_backup}"
  else
    migration_pristine_backup=""
    warn "Failed to create pristine migration snapshot before am migrate."
  fi

  migration_before_counts=$(collect_migration_counts "$PYTHON_DB_MIGRATED_PATH")
  migration_start=$(date +%s)
  if migration_output=$(AM_INTERFACE_MODE=cli DATABASE_URL="sqlite:///$PYTHON_DB_MIGRATED_PATH" "$DEST/$BIN_CLI" migrate --force 2>&1); then
    migration_end=$(date +%s)
    migration_seconds=$((migration_end - migration_start))
    migration_after_counts=$(collect_migration_counts "$PYTHON_DB_MIGRATED_PATH")
    verbose "migration:ok duration_s=${migration_seconds} db=${PYTHON_DB_MIGRATED_PATH}"
    verbose "migration:row_counts_before ${migration_before_counts}"
    verbose "migration:row_counts_after ${migration_after_counts}"
    while IFS= read -r line; do
      [ -n "$line" ] && verbose "migration:output ${line}"
    done <<< "$migration_output"
    if printf "%s\n" "$migration_output" | grep -qiE "database still contains TEXT timestamps|migration completed with errors|migration needed: run"; then
      migration_has_unresolved_warnings=1
    fi
    if printf "%s\n" "$migration_output" | grep -qiE "schema migration hit sqlite engine instability|schema migration path was skipped due to backend instability"; then
      migration_requires_fallback=1
    fi
    info "Database migration command completed; verifying results"
    if [ "$migration_has_unresolved_warnings" -eq 1 ]; then
      warn "Database migration completed with unresolved warnings."
      warn "Review migration output with --verbose and retry if needed:"
      warn "  AM_INTERFACE_MODE=cli DATABASE_URL=sqlite:///$PYTHON_DB_MIGRATED_PATH am migrate --force"
    fi
    migration_succeeded=1
  elif migration_output_fallback=$(AM_INTERFACE_MODE=cli DATABASE_URL="sqlite+aiosqlite:///$PYTHON_DB_MIGRATED_PATH" "$DEST/$BIN_CLI" migrate --force 2>&1); then
    migration_end=$(date +%s)
    migration_seconds=$((migration_end - migration_start))
    migration_after_counts=$(collect_migration_counts "$PYTHON_DB_MIGRATED_PATH")
    verbose "migration:ok_fallback duration_s=${migration_seconds} db=${PYTHON_DB_MIGRATED_PATH}"
    verbose "migration:row_counts_before ${migration_before_counts}"
    verbose "migration:row_counts_after ${migration_after_counts}"
    while IFS= read -r line; do
      [ -n "$line" ] && verbose "migration:output_primary ${line}"
    done <<< "$migration_output"
    while IFS= read -r line; do
      [ -n "$line" ] && verbose "migration:output_fallback ${line}"
    done <<< "$migration_output_fallback"
    if printf "%s\n%s\n" "$migration_output" "$migration_output_fallback" | grep -qiE "database still contains TEXT timestamps|migration completed with errors|migration needed: run"; then
      migration_has_unresolved_warnings=1
    fi
    if printf "%s\n%s\n" "$migration_output" "$migration_output_fallback" | grep -qiE "schema migration hit sqlite engine instability|schema migration path was skipped due to backend instability"; then
      migration_requires_fallback=1
    fi
    info "Database migration command completed; verifying results"
    if [ "$migration_has_unresolved_warnings" -eq 1 ]; then
      warn "Database migration completed with unresolved warnings."
      warn "Review migration output with --verbose and retry if needed:"
      warn "  AM_INTERFACE_MODE=cli DATABASE_URL=sqlite:///$PYTHON_DB_MIGRATED_PATH am migrate --force"
    fi
    migration_succeeded=1
  else
    first_error_line=""
    retry_error_line=""
    migration_end=$(date +%s)
    migration_seconds=$((migration_end - migration_start))
    verbose "migration:failed duration_s=${migration_seconds} db=${PYTHON_DB_MIGRATED_PATH}"
    verbose "migration:row_counts_before ${migration_before_counts}"
    while IFS= read -r line; do
      [ -n "$line" ] && verbose "migration:output_primary ${line}"
    done <<< "$migration_output"
    while IFS= read -r line; do
      [ -n "$line" ] && verbose "migration:output_fallback ${line}"
    done <<< "$migration_output_fallback"
    first_error_line=$(extract_migration_error_line "$migration_output")
    retry_error_line=$(extract_migration_error_line "$migration_output_fallback")
    [ -n "$first_error_line" ] && warn "Primary migration failure summary: $first_error_line"
    [ -n "$retry_error_line" ] && warn "Fallback migration failure summary: $retry_error_line"
    if [ -z "$first_error_line" ] && [ -n "$migration_output" ]; then
      warn "Primary migration command exited non-zero; see --verbose log for full output."
    fi
    if [ -z "$retry_error_line" ] && [ -n "$migration_output_fallback" ]; then
      warn "Fallback migration command exited non-zero; see --verbose log for full output."
    fi
    if [ -n "$migration_pristine_backup" ] && [ -f "$migration_pristine_backup" ]; then
      if copy_sqlite_snapshot "$migration_pristine_backup" "$PYTHON_DB_MIGRATED_PATH"; then
        migration_restore_ok=1
        warn "Restored database from pristine snapshot after failed am migrate."
      else
        migration_restore_ok=1
        warn "Failed to restore pristine snapshot after am migrate failure; attempting installer fallback in-place."
      fi
    fi

    if [ "$migration_restore_ok" -eq 1 ] && sqlite_timestamp_fallback_migration "$PYTHON_DB_MIGRATED_PATH"; then
      migration_fallback_ok=1
      migration_succeeded=1
      if installer_apply_schema_migration "$PYTHON_DB_MIGRATED_PATH" "$RUST_STORAGE_ROOT"; then
        migration_schema_refresh_failed=0
      else
        migration_schema_refresh_failed=1
      fi
      migration_after_counts=$(collect_migration_counts "$PYTHON_DB_MIGRATED_PATH")
      verbose "migration:row_counts_after_fallback ${migration_after_counts}"
      if [ -n "${SQLITE_FALLBACK_BACKUP_PATH:-}" ]; then
        ok "Database backup created at ${SQLITE_FALLBACK_BACKUP_PATH}"
      fi
    fi

    if [ "$migration_fallback_ok" -eq 0 ]; then
      migration_recovery_needed=1
      warn "Database migration had issues. Retry with:"
      warn "  AM_INTERFACE_MODE=cli DATABASE_URL=sqlite:///$PYTHON_DB_MIGRATED_PATH am migrate --force"
      warn "Installer will continue with automatic repair/reconstruction."
    fi
  fi

  if [ "$migration_succeeded" -eq 1 ]; then
    if ! sqlite_pragma_reports_ok "$PYTHON_DB_MIGRATED_PATH" "integrity_check"; then
      migration_requires_fallback=1
      migration_integrity="${SQLITE_LAST_PRAGMA_FAILURE:-<empty>}"
      warn "am migrate produced integrity_check='${migration_integrity}'; forcing installer fallback."
    fi
    if ! migration_core_counts_preserved "$migration_before_counts" "$migration_after_counts"; then
      migration_requires_fallback=1
      warn "am migrate reduced core legacy row counts; forcing installer fallback."
    fi
  fi

  if [ "$migration_succeeded" -eq 1 ] && [ "$migration_requires_fallback" -eq 1 ]; then
    warn "Reverting to pristine snapshot and running installer fallback migration."
    if [ -n "$migration_pristine_backup" ] && [ -f "$migration_pristine_backup" ]; then
      if copy_sqlite_snapshot "$migration_pristine_backup" "$PYTHON_DB_MIGRATED_PATH"; then
        migration_restore_ok=1
        warn "Restored pristine migration snapshot before installer fallback."
      else
        migration_restore_ok=1
        warn "Failed to restore pristine snapshot prior to installer fallback migration; attempting installer fallback in-place."
      fi
    else
      migration_restore_ok=1
      warn "Pristine migration snapshot missing; running installer fallback migration in-place."
    fi

    if [ "$migration_restore_ok" -eq 1 ] && sqlite_timestamp_fallback_migration "$PYTHON_DB_MIGRATED_PATH"; then
      migration_fallback_ok=1
      migration_succeeded=1
      if installer_apply_schema_migration "$PYTHON_DB_MIGRATED_PATH" "$RUST_STORAGE_ROOT"; then
        migration_schema_refresh_failed=0
      else
        migration_schema_refresh_failed=1
      fi
      migration_after_counts=$(collect_migration_counts "$PYTHON_DB_MIGRATED_PATH")
      verbose "migration:row_counts_after_fallback ${migration_after_counts}"
      if [ -n "${SQLITE_FALLBACK_BACKUP_PATH:-}" ]; then
        ok "Database backup created at ${SQLITE_FALLBACK_BACKUP_PATH}"
      fi
    else
      migration_succeeded=0
      migration_fallback_ok=0
      migration_recovery_needed=1
      warn "installer fallback migration could not fully verify the database; escalating to automatic repair/reconstruction."
    fi
  fi

  # Final post-migration invariants: even after fallback, the database must
  # be healthy and core legacy row counts must be preserved.
  if [ "$migration_succeeded" -eq 1 ] || [ "$migration_recovery_needed" -eq 1 ]; then
    migration_final_verification_failed=0
    if [ "$migration_succeeded" -ne 1 ]; then
      migration_final_verification_failed=1
      warn "Initial migration path did not finish cleanly; continuing automatic recovery."
    else
      if [ "$migration_schema_refresh_failed" -eq 1 ]; then
        migration_final_verification_failed=1
        warn "Final migration verification detected a schema refresh failure after database repair."
      fi
      if ! sqlite_post_migration_verify "$PYTHON_DB_MIGRATED_PATH" "$migration_before_counts" "$migration_after_counts"; then
        migration_final_verification_failed=1
        warn "Final migration verification failed: ${SQLITE_POST_MIGRATION_FAILURES}"
      elif ! installer_archive_parity_verify "$PYTHON_DB_MIGRATED_PATH" "$RUST_STORAGE_ROOT"; then
        migration_final_verification_failed=1
      fi
    fi

    if [ "$migration_final_verification_failed" -eq 1 ]; then
      warn "Attempting automatic database self-heal."
      warn "Dual-track recovery path: restore pristine snapshot, normalize timestamps, stabilize SQLite, then reconstruct from the Git archive if needed."

      migration_restore_ok=1
      if [ -n "$migration_pristine_backup" ] && [ -f "$migration_pristine_backup" ]; then
        if copy_sqlite_snapshot "$migration_pristine_backup" "$PYTHON_DB_MIGRATED_PATH"; then
          warn "Restored pristine migration snapshot before automatic self-heal."
        else
          warn "Failed to restore pristine snapshot before automatic self-heal; continuing self-heal in-place."
        fi
      else
        warn "Pristine migration snapshot missing; continuing self-heal in-place."
      fi

      if [ "$migration_restore_ok" -eq 1 ]; then
        if sqlite_timestamp_fallback_migration "$PYTHON_DB_MIGRATED_PATH"; then
          migration_fallback_ok=1
          if installer_apply_schema_migration "$PYTHON_DB_MIGRATED_PATH" "$RUST_STORAGE_ROOT"; then
            migration_schema_refresh_failed=0
          else
            migration_schema_refresh_failed=1
          fi
          if [ -n "${SQLITE_FALLBACK_BACKUP_PATH:-}" ]; then
            ok "Database backup created at ${SQLITE_FALLBACK_BACKUP_PATH}"
          fi
        else
          warn "Timestamp-only fallback could not fully normalize the migrated database."
        fi
      fi

      sqlite_lightweight_self_heal "$PYTHON_DB_MIGRATED_PATH" || warn "SQLite structural self-heal could not fully repair the migrated database."
      migration_after_counts=$(collect_migration_counts "$PYTHON_DB_MIGRATED_PATH")
      verbose "migration:row_counts_after_self_heal ${migration_after_counts}"

      if sqlite_post_migration_verify "$PYTHON_DB_MIGRATED_PATH" "$migration_before_counts" "$migration_after_counts"; then
        migration_final_verification_failed=0
        if [ "$migration_schema_refresh_failed" -eq 1 ]; then
          migration_final_verification_failed=1
          warn "Post-self-heal verification passed SQLite checks, but schema refresh still failed."
        fi
        if [ "$migration_final_verification_failed" -eq 0 ] && ! installer_archive_parity_verify "$PYTHON_DB_MIGRATED_PATH" "$RUST_STORAGE_ROOT"; then
          migration_final_verification_failed=1
          warn "Archive parity still failed after self-heal; attempting archive reconstruction with salvage."
          if installer_reconstruct_database_from_archive "$PYTHON_DB_MIGRATED_PATH" "$RUST_STORAGE_ROOT"; then
            if installer_apply_schema_migration "$PYTHON_DB_MIGRATED_PATH" "$RUST_STORAGE_ROOT"; then
              migration_schema_refresh_failed=0
            else
              migration_schema_refresh_failed=1
            fi
            migration_after_counts=$(collect_migration_counts "$PYTHON_DB_MIGRATED_PATH")
            verbose "migration:row_counts_after_parity_reconstruct ${migration_after_counts}"
            if sqlite_post_migration_verify "$PYTHON_DB_MIGRATED_PATH" "$migration_before_counts" "$migration_after_counts" "recovery_relaxed"; then
              migration_final_verification_failed=0
              if [ "$migration_schema_refresh_failed" -eq 1 ]; then
                migration_final_verification_failed=1
                warn "Archive reconstruction after parity failure passed SQLite checks, but schema refresh still failed."
              fi
              if [ "$migration_final_verification_failed" -eq 0 ] && ! installer_archive_parity_verify "$PYTHON_DB_MIGRATED_PATH" "$RUST_STORAGE_ROOT"; then
                migration_final_verification_failed=1
                warn "Archive reconstruction after parity failure completed, but archive parity still failed: ${INSTALLER_ARCHIVE_PARITY_FAILURE:-unknown failure}"
              fi
            else
              migration_final_verification_failed=1
              warn "Archive reconstruction after parity failure completed, but verification still failed: ${SQLITE_POST_MIGRATION_FAILURES}"
            fi
          fi
        fi
      else
        warn "Post-self-heal verification still failed: ${SQLITE_POST_MIGRATION_FAILURES}"

        migration_restore_ok=1
        if [ -n "$migration_pristine_backup" ] && [ -f "$migration_pristine_backup" ]; then
          if copy_sqlite_snapshot "$migration_pristine_backup" "$PYTHON_DB_MIGRATED_PATH"; then
            warn "Restored pristine migration snapshot before archive reconstruction."
          else
            warn "Failed to restore pristine snapshot before archive reconstruction; continuing from the current database state."
          fi
        fi

        if [ "$migration_restore_ok" -eq 1 ] && sqlite_timestamp_fallback_migration "$PYTHON_DB_MIGRATED_PATH"; then
          migration_fallback_ok=1
        fi

        if [ "$migration_restore_ok" -eq 1 ] && installer_reconstruct_database_from_archive "$PYTHON_DB_MIGRATED_PATH" "$RUST_STORAGE_ROOT"; then
          if installer_apply_schema_migration "$PYTHON_DB_MIGRATED_PATH" "$RUST_STORAGE_ROOT"; then
            migration_schema_refresh_failed=0
          else
            migration_schema_refresh_failed=1
          fi
          migration_after_counts=$(collect_migration_counts "$PYTHON_DB_MIGRATED_PATH")
          verbose "migration:row_counts_after_reconstruct ${migration_after_counts}"
          if sqlite_post_migration_verify "$PYTHON_DB_MIGRATED_PATH" "$migration_before_counts" "$migration_after_counts" "recovery_relaxed"; then
            migration_final_verification_failed=0
            if [ "$migration_schema_refresh_failed" -eq 1 ]; then
              migration_final_verification_failed=1
              warn "Archive reconstruction passed SQLite checks, but schema refresh still failed."
            fi
            if [ "$migration_final_verification_failed" -eq 0 ] && ! installer_archive_parity_verify "$PYTHON_DB_MIGRATED_PATH" "$RUST_STORAGE_ROOT"; then
              migration_final_verification_failed=1
            fi
          else
            warn "Archive reconstruction completed, but verification still failed: ${SQLITE_POST_MIGRATION_FAILURES}"
          fi
        fi
      fi
    fi

    if [ "$migration_final_verification_failed" -eq 0 ]; then
      migration_succeeded=1
      migration_recovery_needed=0
      ok "Database schema migrated"
    else
      migration_succeeded=0
    fi
  fi

  if [ -n "$migration_pristine_backup" ] && [ -f "$migration_pristine_backup" ]; then
    if [ "$migration_succeeded" -eq 1 ]; then
      rm -f "$migration_pristine_backup" "${migration_pristine_backup}-wal" "${migration_pristine_backup}-shm" 2>/dev/null || true
      verbose "migration:pristine_backup_removed path=${migration_pristine_backup}"
    else
      warn "Preserving pristine migration snapshot for manual recovery:"
      warn "  $migration_pristine_backup"
    fi
  fi

  if [ "$migration_succeeded" -eq 1 ]; then
    # Write migration-complete marker so future installer runs skip Python migration entirely.
    mkdir -p "$(dirname "$PYTHON_MIGRATION_MARKER")" 2>/dev/null || true
    printf 'migrated_at=%s\nrust_db=%s\npython_db=%s\ninstaller_version=%s\n' \
      "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
      "${RUST_DB_PATH:-unknown}" \
      "${PYTHON_DB_PATH:-unknown}" \
      "${VERSION:-unknown}" \
      > "$PYTHON_MIGRATION_MARKER" 2>/dev/null || true
    verbose "python_migration_marker:written path=${PYTHON_MIGRATION_MARKER}"
    ok "Python→Rust migration marker saved (future installs will skip migration)"
  fi

  if [ "$migration_succeeded" -ne 1 ]; then
    err "Database migration could not be completed safely."
    err "Aborting install to avoid running with a potentially inconsistent migrated database."
    err "Retry with --verbose after reviewing migration diagnostics above."
    if [ -n "$migration_pristine_backup" ] && [ -f "$migration_pristine_backup" ]; then
      err "Pristine backup preserved at: $migration_pristine_backup"
      err "Manual restore command: cp \"$migration_pristine_backup\" \"$PYTHON_DB_MIGRATED_PATH\""
    fi
    error_support_hint
    exit 1
  fi
fi

if [ "$MAC_DIRECT_EXEC_COMPAT_MODE" -eq 1 ]; then
  warn "Rust database migration was skipped because this macOS host rejected direct execution of the installed Rust binaries."
  warn "The existing Python data path remains active through the compatibility launcher."
fi

if [ "${MIGRATE_PYTHON:-0}" -eq 1 ] && [ "$PYTHON_CURRENT_SHELL_TAKEOVER_POSSIBLE" -eq 1 ]; then
  if ! install_legacy_launcher_takeover_shims; then
    err "Failed to install the legacy current-shell handoff shim."
    err "Without this shim, an already-loaded legacy 'am' alias may continue to run Python in the current shell."
    error_support_hint
    exit 1
  fi
  if [ "$LEGACY_LAUNCHER_SHIM_COUNT" -gt 0 ]; then
    ok "Already-loaded legacy 'am' aliases that call run_server_with_token.sh now hand off to Rust automatically"
  fi
fi

if [ "$MAC_DIRECT_EXEC_COMPAT_MODE" -eq 0 ]; then
  ensure_remote_http_client_readiness
else
  warn "Skipping Rust HTTP endpoint readiness checks because this macOS host is using the Python compatibility launcher."
fi

# T2.4: Post-install verification
verify_installation() {
  local issues=0
  verbose "verify_installation:start dest=${DEST} shell=${SHELL:-unknown}"

  if [ "$MAC_DIRECT_EXEC_COMPAT_MODE" -eq 1 ]; then
    ok "VERIFY: macOS Python compatibility mode is active"
    warn "VERIFY: Rust direct-exec probes were skipped because this Mac blocked the installed Rust binaries"
    local am_descriptor=""
    am_descriptor=$(interactive_shell_am_descriptor)
    if printf '%s\n' "$am_descriptor" | grep -qiE 'alias|function'; then
      ok "VERIFY: interactive shell resolves 'am' via shell compatibility glue"
    else
      warn "VERIFY: open a fresh shell so the compatibility 'am' function is loaded"
    fi
    return 0
  fi

  # Surface guard helpers: ensure CLI/server binaries were not swapped.
  local cli_help=""
  local server_help=""
  local cli_surface_ok=0
  local server_surface_ok=0

  # 1. Check binaries exist and are executable
  if [ ! -x "$DEST/$BIN_SERVER" ]; then
    warn "VERIFY: $DEST/$BIN_SERVER is missing or not executable"
    issues=$((issues + 1))
  fi
  if [ ! -x "$DEST/$BIN_CLI" ]; then
    warn "VERIFY: $DEST/$BIN_CLI is missing or not executable"
    issues=$((issues + 1))
  fi

  # 2. Check version output
  local version_out
  version_out=$("$DEST/$BIN_CLI" --version 2>&1 || true)
  if [ -z "$version_out" ]; then
    warn "VERIFY: 'am --version' produced no output"
    issues=$((issues + 1))
  else
    ok "VERIFY: $version_out"
  fi

  # 3. Check binary command surfaces (prevents swapped/mispackaged installs)
  cli_help=$("$DEST/$BIN_CLI" --help 2>&1 || true)
  if printf "%s\n" "$cli_help" | grep -qE '(^|[[:space:]])serve-http([[:space:]]|$)'; then
    cli_surface_ok=1
    ok "VERIFY: '$BIN_CLI' exposes CLI command surface"
  else
    warn "VERIFY: '$BIN_CLI --help' missing expected CLI command 'serve-http'"
    issues=$((issues + 1))
  fi

  server_help=$("$DEST/$BIN_SERVER" --help 2>&1 || true)
  if printf "%s\n" "$server_help" | grep -qE '^Usage: mcp-agent-mail ' && \
     printf "%s\n" "$server_help" | grep -qE '(^|[[:space:]])serve([[:space:]]|$)'; then
    server_surface_ok=1
    ok "VERIFY: '$BIN_SERVER' exposes server command surface"
  else
    warn "VERIFY: '$BIN_SERVER --help' missing expected server command surface"
    issues=$((issues + 1))
  fi
  verbose "verify_installation:surface_guard cli_ok=${cli_surface_ok} server_ok=${server_surface_ok}"

  # 4. Check that 'am' resolves to the Rust binary in an interactive shell.
  local am_descriptor=""
  am_descriptor=$(interactive_shell_am_descriptor)
  verbose "verify_installation:path_resolution_result descriptor=${am_descriptor:-NOT_FOUND} expected=${DEST}/${BIN_CLI}"

  if [ "$am_descriptor" = "NOT_FOUND" ] || [ -z "$am_descriptor" ]; then
    warn "VERIFY: 'am' not found in interactive shell PATH"
    warn "  You may need to restart your shell or run: export PATH=\"$DEST:\$PATH\""
    issues=$((issues + 1))
  elif printf '%s\n' "$am_descriptor" | grep -qiE 'alias|function'; then
    warn "VERIFY: interactive shell still resolves 'am' via:"
    warn "  $am_descriptor"
    warn "  Expected binary: $DEST/$BIN_CLI"
    warn "  Fix: restart your shell or run: unalias am"
    issues=$((issues + 1))
  elif ! printf '%s\n' "$am_descriptor" | grep -Fq "$DEST/$BIN_CLI"; then
    warn "VERIFY: interactive shell resolves 'am' to:"
    warn "  $am_descriptor"
    warn "  Expected binary: $DEST/$BIN_CLI"
    issues=$((issues + 1))
  else
    ok "VERIFY: interactive shell resolves 'am' to $DEST/$BIN_CLI"
  fi

  # 5. If Python was displaced, verify the alias is gone
  if [ "$PYTHON_DETECTED" -eq 1 ] && [ "${MIGRATE_PYTHON:-0}" -eq 1 ]; then
    if [ "$PYTHON_ALIAS_FOUND" -eq 1 ] && [ -n "$PYTHON_ALIAS_FILE" ]; then
      if grep -qE "^[[:space:]]*(alias am=|alias am |function am($|[[:space:](])|am[[:space:]]*\\(\\))" "$PYTHON_ALIAS_FILE" 2>/dev/null; then
        warn "VERIFY: Python 'am' alias/function still active in $PYTHON_ALIAS_FILE"
        issues=$((issues + 1))
      else
        ok "VERIFY: Python alias/function displaced in $PYTHON_ALIAS_FILE"
      fi
    fi
  fi

  # 6. If remote HTTP MCP clients were configured, verify the local endpoint is healthy.
  if has_remote_http_client_targets; then
    if probe_remote_http_endpoint; then
      ok "VERIFY: remote MCP endpoint ready at $(desired_mcp_http_url)"
    else
      warn "VERIFY: remote MCP endpoint is not healthy at $(desired_mcp_http_url)"
      [ -n "${REMOTE_HTTP_PROBE_DETAIL:-}" ] && warn "  Probe detail: ${REMOTE_HTTP_PROBE_DETAIL}"
      issues=$((issues + 1))
    fi
  fi

  # 7. Summary
  if [ "$issues" -gt 0 ]; then
    warn "Verification found $issues issue(s). See warnings above."
    return 1
  else
    ok "All verification checks passed"
  fi
  verbose "verify_installation:done issues=${issues}"
  return 0
}

if [ "$VERIFY" -eq 1 ]; then
  verify_installation
fi

# Final summary
echo ""
if [ "$QUIET" -eq 0 ]; then
  if [ "$HAS_GUM" -eq 1 ] && [ "$NO_GUM" -eq 0 ]; then
    {
      gum style --foreground 42 --bold "mcp-agent-mail is installed!"
      echo ""
      gum style --foreground 245 "Binaries:"
      gum style --foreground 245 "  mcp-agent-mail  MCP server (stdio/HTTP)"
      gum style --foreground 245 "  am              CLI operator tool + TUI"
      echo ""
      gum style --foreground 245 "Quick start:"
      gum style --foreground 39  "  am                    # Auto-detect agents, start server + TUI"
      gum style --foreground 39  "  am serve-http         # HTTP transport"
      gum style --foreground 39  "  mcp-agent-mail        # stdio transport (MCP client integration)"
      gum style --foreground 39  "  am --help             # Full operator CLI"
    } | gum style --border normal --border-foreground 42 --padding "1 2"
  else
    draw_box "0;32" \
      "\033[1;32mmcp-agent-mail is installed!\033[0m" \
      "" \
      "Binaries:" \
      "  mcp-agent-mail  MCP server (stdio/HTTP)" \
      "  am              CLI operator tool + TUI" \
      "" \
      "Quick start:" \
      "  am                    # Auto-detect agents, start server + TUI" \
      "  am serve-http         # HTTP transport" \
      "  mcp-agent-mail        # stdio transport (MCP client integration)" \
      "  am --help             # Full operator CLI"
  fi

  echo ""
  if [ "$HAS_GUM" -eq 1 ] && [ "$NO_GUM" -eq 0 ]; then
    gum style --foreground 245 --italic "To uninstall: ./install.sh --uninstall --dest $DEST"
  else
    echo -e "\033[0;90mTo uninstall: ./install.sh --uninstall --dest $DEST\033[0m"
  fi
fi

if [ "$MAC_DIRECT_EXEC_COMPAT_MODE" -eq 1 ]; then
  echo ""
  warn "This macOS host is using the Python compatibility launcher for 'am'."
  warn "The installed Rust binaries remain on disk, but direct execution was blocked by the host."
  activate_mac_python_cli_compat_shell
fi
