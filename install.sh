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
#   --verify           Run self-test after install
#   --from-source      Build from source instead of downloading binary
#   --quiet            Suppress non-error output
#   --verbose          Enable detailed installer diagnostics
#   --no-gum           Disable gum formatting even if available
#   --no-verify        Skip checksum + signature verification (for testing only)
#   --offline          Skip network preflight checks
#   --force            Force reinstall even if already at version
#
set -Eeuo pipefail
umask 022
shopt -s lastpipe 2>/dev/null || true

VERSION="${VERSION:-}"
OWNER="${OWNER:-Dicklesworthstone}"
REPO="${REPO:-mcp_agent_mail_rust}"
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
OFFLINE="${AM_OFFLINE:-0}"
VERBOSE_DUMP_LINES=20
LOG_FILE="${LOG_FILE:-/tmp/am-install-$(date -u +%Y%m%dT%H%M%SZ)-$$.log}"
LOG_INITIALIZED=0
ERROR_TAIL_EMITTED=0
ORIGINAL_ARGS=("$@")

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
  local start_ts end_ts duration_s size_bytes
  start_ts=$(date +%s)
  verbose "${label}:start url=${url} out=${out}"
  if [ "$VERBOSE" -eq 1 ] && [ "$QUIET" -eq 0 ]; then
    curl -fL --progress-bar "$url" -o "$out"
  else
    curl -fsSL "$url" -o "$out"
  fi
  end_ts=$(date +%s)
  duration_s=$((end_ts - start_ts))
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
  case "${OS}-${ARCH}" in
    linux-x86_64) TARGET="x86_64-unknown-linux-gnu" ;;
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
  local min_kb=20480  # 20MB for two binaries
  local path="$DEST"
  if [ ! -d "$path" ]; then
    path=$(dirname "$path")
  fi
  if command -v df >/dev/null 2>&1; then
    local avail_kb
    avail_kb=$(df -Pk "$path" | awk 'NR==2 {print $4}')
    if [ -n "$avail_kb" ] && [ "$avail_kb" -lt "$min_kb" ]; then
      err "Insufficient disk space in $path (need at least 20MB)"
      exit 1
    fi
  else
    warn "df not found; skipping disk space check"
  fi
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

check_existing_install() {
  verbose "check_existing_install:start dest=${DEST}"
  if [ -x "$DEST/$BIN_CLI" ]; then
    local current
    current=$("$DEST/$BIN_CLI" --version 2>/dev/null | head -1 || echo "")
    if [ -n "$current" ]; then
      info "Existing am detected: $current"
      verbose "check_existing_install:am version=${current}"
    fi
  fi
  if [ -x "$DEST/$BIN_SERVER" ]; then
    local current
    current=$("$DEST/$BIN_SERVER" --version 2>/dev/null | head -1 || echo "")
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

# T1.1: Detect Python am alias in shell rc files
detect_python_alias() {
  PYTHON_ALIAS_FOUND=0
  PYTHON_ALIAS_FILE=""
  PYTHON_ALIAS_LINE=0
  PYTHON_ALIAS_CONTENT=""
  PYTHON_ALIAS_KIND=""
  PYTHON_ALIAS_HAS_MARKERS=0

  local rc_files=("$HOME/.zshrc" "$HOME/.bashrc" "$HOME/.profile" "$HOME/.bash_profile")
  # Fish uses different syntax; check config.fish too
  local fish_config="$HOME/.config/fish/config.fish"
  if [ -f "$fish_config" ]; then
    rc_files+=("$fish_config")
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
      active_entry=$(printf '%s\n' "$marker_payload" | grep -E "^[[:space:]]*(alias am=|alias am |function am[[:space:](]|am[[:space:]]*\\(\\))" | head -1 || true)

      if [ -n "$active_entry" ]; then
        PYTHON_ALIAS_FOUND=1
        PYTHON_ALIAS_FILE="$rc"
        PYTHON_ALIAS_HAS_MARKERS=1
        PYTHON_ALIAS_LINE="$marker_line"
        PYTHON_ALIAS_CONTENT="$active_entry"
        if echo "$active_entry" | grep -qE "^[[:space:]]*(function am[[:space:](]|am[[:space:]]*\\(\\))"; then
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
    func_line=$(grep -n -E "^[[:space:]]*(function am[[:space:](]|am[[:space:]]*\(\))" "$rc" 2>/dev/null | grep -v "^[[:space:]]*#" | head -1 || true)
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
      if echo "$link_target" | grep -qiE "python|venv|site-packages|mcp.agent.mail"; then
        PYTHON_BINARY_FOUND=1
        PYTHON_BINARY_PATH="$am_path"
        verbose "detect_python_binary:found symlink_path=${PYTHON_BINARY_PATH}"
        return 0
      fi
    fi

    # Check shebang or content for Python references
    if [ -f "$am_path" ] && [ -r "$am_path" ]; then
      local first_lines
      first_lines=$(head -5 "$am_path" 2>/dev/null || true)
      if echo "$first_lines" | grep -qiE "python|#!/.*python"; then
        PYTHON_BINARY_FOUND=1
        PYTHON_BINARY_PATH="$am_path"
        verbose "detect_python_binary:found script_path=${PYTHON_BINARY_PATH}"
        return 0
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
  [ "$PYTHON_BINARY_FOUND" -eq 0 ] && verbose "detect_python_binary:not_found"
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
    local alias_path
    # Extract path from patterns like: alias am='cd "/path/to/dir" && ...'
    alias_path=$(echo "$PYTHON_ALIAS_CONTENT" | sed -n "s/.*cd [\"']*\([^\"'&]*\)[\"']*.*/\1/p")
    [ -n "$alias_path" ] && candidates+=("$alias_path")
    # Also try: alias am='cd /path/to/dir && ...'
    alias_path=$(echo "$PYTHON_ALIAS_CONTENT" | sed -n 's/.*cd \([^ &"'"'"']*\).*/\1/p')
    [ -n "$alias_path" ] && candidates+=("$alias_path")
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
displace_python_alias() {
  [ "$PYTHON_ALIAS_FOUND" -eq 0 ] && return 0

  local rc="$PYTHON_ALIAS_FILE"
  [ -z "$rc" ] && return 0
  [ -f "$rc" ] || return 0

  # Create timestamped backup
  local timestamp
  timestamp=$(date +%Y%m%d_%H%M%S)
  local backup="${rc}.bak.mcp-agent-mail-${timestamp}"
  cp -p "$rc" "$backup"
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
  mv "$tmpfile" "$rc"
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

# T1.5: Stop running Python server processes
stop_python_server() {
  [ -z "$PYTHON_PID" ] && return 0

  info "Stopping Python mcp-agent-mail server (PID $PYTHON_PID)"
  kill "$PYTHON_PID" 2>/dev/null || true

  # Wait up to 5 seconds for graceful shutdown
  local waited=0
  while [ "$waited" -lt 5 ] && kill -0 "$PYTHON_PID" 2>/dev/null; do
    sleep 1
    waited=$((waited + 1))
  done

  # Force-kill if still running
  if kill -0 "$PYTHON_PID" 2>/dev/null; then
    warn "Python server did not stop gracefully; sending SIGKILL"
    kill -9 "$PYTHON_PID" 2>/dev/null || true
    sleep 1
  fi

  if ! kill -0 "$PYTHON_PID" 2>/dev/null; then
    ok "Python server stopped"
  else
    warn "Could not stop Python server (PID $PYTHON_PID)"
  fi
}

# T5.2: Resolve database path differences between Python and Rust
# Python stores DB at clone_dir/storage.sqlite3 (via cd in alias)
# Rust resolves via DATABASE_URL (default: ./storage.sqlite3 relative to CWD)
# or STORAGE_ROOT (default: ~/.mcp_agent_mail_git_mailbox_repo)
resolve_database_path() {
  PYTHON_DB_FOUND=0
  PYTHON_DB_PATH=""
  RUST_STORAGE_ROOT="${STORAGE_ROOT:-$HOME/.mcp_agent_mail_git_mailbox_repo}"

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
    "$HOME/.env"
  )
  [ "$PYTHON_CLONE_FOUND" -eq 1 ] && [ -n "$PYTHON_CLONE_PATH" ] && env_files+=("$PYTHON_CLONE_PATH/.env")

  for env_file in "${env_files[@]}"; do
    if [ -f "$env_file" ]; then
      local db_url
      db_url=$(grep -E '^DATABASE_URL=' "$env_file" 2>/dev/null | head -1 | cut -d= -f2-)
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
        PYTHON_DB_FOUND=1
        PYTHON_DB_PATH="$candidate"
        break
      fi
    fi
  done

  if [ "$PYTHON_DB_FOUND" -eq 0 ]; then
    return 0
  fi

  info "Found Python database at: $PYTHON_DB_PATH"

  # Determine if the DB is already in the Rust storage root
  local rust_db="$RUST_STORAGE_ROOT/storage.sqlite3"
  local abs_python_db
  abs_python_db=$(cd "$(dirname "$PYTHON_DB_PATH")" 2>/dev/null && echo "$(pwd)/$(basename "$PYTHON_DB_PATH")")
  local abs_rust_db
  abs_rust_db=$(cd "$(dirname "$rust_db")" 2>/dev/null && echo "$(pwd)/$(basename "$rust_db")" 2>/dev/null || echo "$rust_db")

  if [ "$abs_python_db" = "$abs_rust_db" ]; then
    info "Python database is already at the Rust storage location"
    return 0
  fi

  # Copy the Python DB to the Rust storage root (don't move — safer)
  mkdir -p "$RUST_STORAGE_ROOT"

  if [ -f "$rust_db" ] && [ -s "$rust_db" ]; then
    # Rust DB already exists — don't overwrite
    info "Rust database already exists at $rust_db"
    info "Python database preserved at $PYTHON_DB_PATH"
    info "To migrate manually: am migrate-python-db $PYTHON_DB_PATH"
    return 0
  fi

  cp -p "$PYTHON_DB_PATH" "$rust_db"
  # Also copy WAL/SHM files if present
  [ -f "${PYTHON_DB_PATH}-wal" ] && cp -p "${PYTHON_DB_PATH}-wal" "${rust_db}-wal"
  [ -f "${PYTHON_DB_PATH}-shm" ] && cp -p "${PYTHON_DB_PATH}-shm" "${rust_db}-shm"
  ok "Copied Python database to $rust_db"

  # Set DATABASE_URL so Rust binary finds it
  export DATABASE_URL="sqlite+aiosqlite:///$rust_db"
  PYTHON_DB_MIGRATED_PATH="$rust_db"
}

# T5.3: Migrate .env configuration from Python to Rust
# Python .env may live in clone dir or storage root. Rust reads the same
# env vars but DATABASE_URL format differs (no aiosqlite prefix).
migrate_env_config() {
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

  if [ -z "$env_file" ]; then
    return 0  # No .env found, nothing to migrate
  fi

  info "Found Python .env at: $env_file"

  # Rust config location
  local rust_config_dir="$HOME/.config/mcp-agent-mail"
  local rust_env="$rust_config_dir/config.env"

  # Don't overwrite if Rust config already exists
  if [ -f "$rust_env" ]; then
    info "Rust config already exists at $rust_env — preserving"
    return 0
  fi

  mkdir -p "$rust_config_dir"

  # Vars that are compatible between Python and Rust
  local compat_vars="HTTP_HOST HTTP_PORT HTTP_PATH HTTP_BEARER_TOKEN STORAGE_ROOT DATABASE_URL TUI_ENABLED LLM_ENABLED LLM_DEFAULT_MODEL WORKTREES_ENABLED"
  # Python-only vars to skip
  local skip_pattern="^(SQLALCHEMY_|ALEMBIC_|UVICORN_|ASYNC_)"

  local tmpfile="${rust_env}.tmp.$$"
  {
    echo "# Migrated from Python .env: $env_file"
    echo "# Migration date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo ""

    while IFS= read -r line || [ -n "$line" ]; do
      # Skip comments and empty lines
      case "$line" in
        \#*|"") echo "$line"; continue;;
      esac

      local key val
      key="${line%%=*}"
      val="${line#*=}"

      # Skip Python-specific vars
      if echo "$key" | grep -qE "$skip_pattern"; then
        echo "# Skipped (Python-only): $line"
        continue
      fi

      # Transform DATABASE_URL: strip aiosqlite prefix, resolve path
      if [ "$key" = "DATABASE_URL" ]; then
        # Strip sqlite+aiosqlite:/// prefix → just the file path
        local db_path
        db_path=$(echo "$val" | sed 's|^sqlite[+a-z]*:///||')
        # If relative path, make it relative to storage root
        case "$db_path" in
          /*) echo "DATABASE_URL=sqlite:///$db_path";;
          *)  echo "DATABASE_URL=sqlite:///$RUST_STORAGE_ROOT/$db_path";;
        esac
        continue
      fi

      # Pass through compatible vars as-is
      echo "$line"
    done < "$env_file"
  } > "$tmpfile"

  mv "$tmpfile" "$rust_env"
  chmod 600 "$rust_env"  # Restrict access (may contain tokens)
  ok "Migrated .env config to $rust_env"
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
}

maybe_add_path() {
  verbose "maybe_add_path:start path=${PATH} dest=${DEST} easy=${EASY}"
  case ":$PATH:" in
    *:"$DEST":*)
      verbose "maybe_add_path:dest_already_in_path"
      return 0
      ;;
    *)
      if [ "$EASY" -eq 1 ]; then
        UPDATED=0
        for rc in "$HOME/.zshrc" "$HOME/.bashrc"; do
          if [ -e "$rc" ] && [ -w "$rc" ]; then
            if ! grep -F "$DEST" "$rc" >/dev/null 2>&1; then
              echo "export PATH=\"$DEST:\$PATH\"" >> "$rc"
              verbose "maybe_add_path:appended rc=${rc} export PATH=\"$DEST:\$PATH\""
            fi
            UPDATED=1
          fi
        done
        if [ "$UPDATED" -eq 1 ]; then
          warn "PATH updated in ~/.zshrc/.bashrc; restart shell to use am / mcp-agent-mail"
          verbose "maybe_add_path:updated_shell_rc=1"
        else
          warn "Add $DEST to PATH to use am / mcp-agent-mail"
          verbose "maybe_add_path:updated_shell_rc=0"
        fi
      else
        warn "Add $DEST to PATH to use am / mcp-agent-mail"
        verbose "maybe_add_path:easy_mode_disabled_no_update"
      fi
    ;;
  esac
  verbose "maybe_add_path:done"
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
    # Claude Code / Claude Desktop
    candidates+=("claude|${home_dir}/.claude/settings.json")
    candidates+=("claude|${home_dir}/.claude/settings.local.json")
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
  candidates+=("claude|${project_dir}/.claude/settings.json")
  candidates+=("claude|${project_dir}/.claude/settings.local.json")
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
  installed_version=$("$DEST/$BIN_CLI" --version 2>/dev/null | head -1 | sed 's/.*\([0-9]\+\.[0-9]\+\.[0-9]\+\).*/\1/')

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

usage() {
  cat <<EOFU
Usage: install.sh [--version vX.Y.Z] [--dest DIR] [--system] [--easy-mode] [--verify] \\
                  [--artifact-url URL] [--checksum HEX] [--checksum-url URL] [--quiet] \\
                  [--offline] [--no-gum] [--no-verify] [--force] [--from-source] [--verbose]

Installs mcp-agent-mail and am (CLI) binaries.

Options:
  --version vX.Y.Z   Install specific version (default: latest)
  --dest DIR         Install to DIR (default: ~/.local/bin)
  --system           Install to /usr/local/bin (requires sudo)
  --easy-mode        Auto-update PATH in shell rc files
  --verify           Run self-test after install
  --from-source      Build from source instead of downloading binary
  --quiet            Suppress non-error output
  --verbose          Enable detailed installer diagnostics
  --offline          Skip network preflight checks
  --no-gum           Disable gum formatting even if available
  --no-verify        Skip checksum + signature verification (for testing only)
  --force            Force reinstall even if same version is installed
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
        dump_verbose_tail
        exit 2
      fi
      VERSION="$2"; shift 2;;
    --dest)
      if [ $# -lt 2 ]; then
        err "Option --dest requires a value"
        dump_verbose_tail
        exit 2
      fi
      DEST="$2"; shift 2;;
    --system) SYSTEM=1; DEST="/usr/local/bin"; shift;;
    --easy-mode) EASY=1; shift;;
    --verify) VERIFY=1; shift;;
    --artifact-url)
      if [ $# -lt 2 ]; then
        err "Option --artifact-url requires a value"
        dump_verbose_tail
        exit 2
      fi
      ARTIFACT_URL="$2"; shift 2;;
    --checksum)
      if [ $# -lt 2 ]; then
        err "Option --checksum requires a value"
        dump_verbose_tail
        exit 2
      fi
      CHECKSUM="$2"; shift 2;;
    --checksum-url)
      if [ $# -lt 2 ]; then
        err "Option --checksum-url requires a value"
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
    -h|--help) usage; exit 0;;
    *) shift;;
  esac
done

verbose "config VERSION=${VERSION:-latest} DEST=${DEST} SYSTEM=${SYSTEM} EASY=${EASY} VERIFY=${VERIFY} FROM_SOURCE=${FROM_SOURCE} QUIET=${QUIET} VERBOSE=${VERBOSE} OFFLINE=${OFFLINE} FORCE_INSTALL=${FORCE_INSTALL}"

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
  ok "mcp-agent-mail $VERSION is already installed at $DEST"
  info "Use --force to reinstall"
  exit 0
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
    warn "Binary download failed (release may not exist for $VERSION)"
    warn "Attempting build from source as fallback..."
    verbose "binary-download:fallback_to_source version=${VERSION} url=${URL}"
    FROM_SOURCE=1
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
    err "  curl -fsSL https://raw.githubusercontent.com/Dicklesworthstone/mcp_agent_mail_rust/main/install.sh | bash"
    err ""
    err "If no release exists yet, check https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases"
    exit 1
  fi

  if ! (cd "$TMP/src" && cargo build --release -p mcp-agent-mail -p mcp-agent-mail-cli); then
    err "Build failed. Check compiler output above for details."
    exit 1
  fi
  local_server="$TMP/src/target/release/$BIN_SERVER"
  local_cli="$TMP/src/target/release/$BIN_CLI"
  [ -x "$local_server" ] || { err "Build failed: $BIN_SERVER not found"; exit 1; }
  [ -x "$local_cli" ] || { err "Build failed: $BIN_CLI not found"; exit 1; }
  atomic_install "$local_server" "$DEST/$BIN_SERVER"
  atomic_install "$local_cli" "$DEST/$BIN_CLI"
  ok "Installed to $DEST (source build)"
  ok "  $DEST/$BIN_SERVER"
  ok "  $DEST/$BIN_CLI"
  maybe_add_path
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
    [ -z "$CHECKSUM_URL" ] && CHECKSUM_URL="${URL}.sha256"
    info "Fetching checksum from ${CHECKSUM_URL}"
    CHECKSUM_FILE="$TMP/checksum.sha256"
    if ! download_to_file "$CHECKSUM_URL" "$CHECKSUM_FILE" "checksum-download"; then
      warn "Checksum file not available; skipping verification"
      warn "Use --checksum <hex> to provide one manually"
      CHECKSUM=""
    else
      CHECKSUM=$(awk '{print $1}' "$CHECKSUM_FILE")
      if [ -z "$CHECKSUM" ]; then
        warn "Empty checksum file; skipping verification"
      fi
    fi
  fi

  if [ -n "$CHECKSUM" ]; then
    if ! verify_checksum "$TMP/$TAR" "$CHECKSUM"; then
      err "Installation aborted due to checksum failure"
      exit 1
    fi
  fi

  if ! verify_sigstore_bundle "$TMP/$TAR" "$URL"; then
    err "Signature verification failed"
    err "The downloaded file may be corrupted or tampered with."
    exit 1
  fi
fi

info "Extracting"
tar -xf "$TMP/$TAR" -C "$TMP"

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

SERVER_BIN=$(find_bin "$BIN_SERVER") || { err "Binary $BIN_SERVER not found in archive"; exit 1; }
CLI_BIN=$(find_bin "$BIN_CLI") || { err "Binary $BIN_CLI not found in archive"; exit 1; }

atomic_install "$SERVER_BIN" "$DEST/$BIN_SERVER"
atomic_install "$CLI_BIN" "$DEST/$BIN_CLI"
ok "Installed to $DEST"
ok "  $DEST/$BIN_SERVER"
ok "  $DEST/$BIN_CLI"
maybe_add_path

# Displace Python installation if detected (T2.2)
if [ "$PYTHON_DETECTED" -eq 1 ]; then
  MIGRATE_PYTHON=1
  if [ "$EASY" -eq 0 ] && [ -t 0 ]; then
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
    displace_python_alias
    resolve_database_path
    migrate_env_config
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

collect_migration_counts() {
  local db_path="$1"
  if ! command -v sqlite3 >/dev/null 2>&1 || [ ! -f "$db_path" ]; then
    echo "sqlite3_unavailable"
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
    count=$(sqlite3 "$db_path" "SELECT COUNT(*) FROM ${table};" 2>/dev/null || echo "na")
    summary+="${table}=${count};"
  done
  echo "$summary"
}

# Run database migration if we copied a Python DB
if [ -n "$PYTHON_DB_MIGRATED_PATH" ] && [ -f "$PYTHON_DB_MIGRATED_PATH" ]; then
  info "Running database migration on copied Python database"
  migration_start=0
  migration_end=0
  migration_seconds=0
  migration_output=""
  migration_before_counts=""
  migration_after_counts=""
  migration_before_counts=$(collect_migration_counts "$PYTHON_DB_MIGRATED_PATH")
  migration_start=$(date +%s)
  if migration_output=$(DATABASE_URL="sqlite+aiosqlite:///$PYTHON_DB_MIGRATED_PATH" "$DEST/$BIN_CLI" migrate 2>&1); then
    migration_end=$(date +%s)
    migration_seconds=$((migration_end - migration_start))
    migration_after_counts=$(collect_migration_counts "$PYTHON_DB_MIGRATED_PATH")
    verbose "migration:ok duration_s=${migration_seconds} db=${PYTHON_DB_MIGRATED_PATH}"
    verbose "migration:row_counts_before ${migration_before_counts}"
    verbose "migration:row_counts_after ${migration_after_counts}"
    while IFS= read -r line; do
      [ -n "$line" ] && verbose "migration:output ${line}"
    done <<< "$migration_output"
    ok "Database schema migrated"
  else
    migration_end=$(date +%s)
    migration_seconds=$((migration_end - migration_start))
    verbose "migration:failed duration_s=${migration_seconds} db=${PYTHON_DB_MIGRATED_PATH}"
    verbose "migration:row_counts_before ${migration_before_counts}"
    while IFS= read -r line; do
      [ -n "$line" ] && verbose "migration:output ${line}"
    done <<< "$migration_output"
    warn "Database migration had issues (you can retry with: am migrate)"
  fi
fi

# T2.4: Post-install verification
verify_installation() {
  local issues=0
  verbose "verify_installation:start dest=${DEST} shell=${SHELL:-unknown}"

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

  # 3. Check that 'am' resolves to the Rust binary in a login shell
  local shell_name
  shell_name=$(basename "${SHELL:-/bin/sh}")
  local resolve_cmd
  case "$shell_name" in
    zsh)  resolve_cmd="zsh -l -c 'whence -p am 2>/dev/null || which am 2>/dev/null || echo NOT_FOUND'" ;;
    bash) resolve_cmd="bash -l -c 'type -P am 2>/dev/null || which am 2>/dev/null || echo NOT_FOUND'" ;;
    *)    resolve_cmd="sh -l -c 'which am 2>/dev/null || echo NOT_FOUND'" ;;
  esac
  verbose "verify_installation:path_resolution_command shell=${shell_name} cmd=${resolve_cmd}"
  local resolved_path
  resolved_path=$(eval "$resolve_cmd" 2>/dev/null || echo "NOT_FOUND")
  verbose "verify_installation:path_resolution_result resolved=${resolved_path:-NOT_FOUND} expected=${DEST}/${BIN_CLI}"

  if [ "$resolved_path" = "NOT_FOUND" ] || [ -z "$resolved_path" ]; then
    warn "VERIFY: 'am' not found in login shell PATH"
    warn "  You may need to restart your shell or run: export PATH=\"$DEST:\$PATH\""
    issues=$((issues + 1))
  elif [ "$resolved_path" != "$DEST/$BIN_CLI" ]; then
    # Check if it's an alias shadowing the binary
    local alias_check
    alias_check=$(eval "$shell_name -l -c 'alias am 2>/dev/null || true'" 2>/dev/null || true)
    if [ -n "$alias_check" ]; then
      warn "VERIFY: 'am' is still aliased in your shell!"
      warn "  Alias: $alias_check"
      warn "  Expected: $DEST/$BIN_CLI"
      warn "  Fix: restart your shell or run: unalias am"
      issues=$((issues + 1))
    else
      info "VERIFY: 'am' resolves to $resolved_path (expected $DEST/$BIN_CLI)"
    fi
  else
    ok "VERIFY: 'am' resolves to $DEST/$BIN_CLI"
  fi

  # 4. If Python was displaced, verify the alias is gone
  if [ "$PYTHON_DETECTED" -eq 1 ] && [ "${MIGRATE_PYTHON:-0}" -eq 1 ]; then
    if [ "$PYTHON_ALIAS_FOUND" -eq 1 ] && [ -n "$PYTHON_ALIAS_FILE" ]; then
      if grep -qE "^[[:space:]]*(alias am=|alias am |function am[[:space:](]|am[[:space:]]*\\(\\))" "$PYTHON_ALIAS_FILE" 2>/dev/null; then
        warn "VERIFY: Python 'am' alias/function still active in $PYTHON_ALIAS_FILE"
        issues=$((issues + 1))
      else
        ok "VERIFY: Python alias/function displaced in $PYTHON_ALIAS_FILE"
      fi
    fi
  fi

  # 5. Summary
  if [ "$issues" -gt 0 ]; then
    warn "Verification found $issues issue(s). See warnings above."
  else
    ok "All verification checks passed"
  fi
  verbose "verify_installation:done issues=${issues}"
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
    gum style --foreground 245 --italic "To uninstall: rm $DEST/$BIN_SERVER $DEST/$BIN_CLI"
  else
    echo -e "\033[0;90mTo uninstall: rm $DEST/$BIN_SERVER $DEST/$BIN_CLI\033[0m"
  fi
fi
