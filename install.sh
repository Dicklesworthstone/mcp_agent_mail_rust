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
#   --no-gum           Disable gum formatting even if available
#   --no-verify        Skip checksum + signature verification (for testing only)
#   --offline          Skip network preflight checks
#   --force            Force reinstall even if already at version
#
set -euo pipefail
umask 022
shopt -s lastpipe 2>/dev/null || true

VERSION="${VERSION:-}"
OWNER="${OWNER:-Dicklesworthstone}"
REPO="${REPO:-mcp_agent_mail_rust}"
DEST_DEFAULT="$HOME/.local/bin"
DEST="${DEST:-$DEST_DEFAULT}"
EASY=0
QUIET=0
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
  if [ -n "$VERSION" ]; then return 0; fi

  info "Resolving latest version..."
  local latest_url="https://api.github.com/repos/${OWNER}/${REPO}/releases/latest"
  local tag
  if ! tag=$(curl -fsSL -H "Accept: application/vnd.github.v3+json" "$latest_url" 2>/dev/null | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/'); then
    tag=""
  fi

  if [ -n "$tag" ]; then
    VERSION="$tag"
    info "Resolved latest version: $VERSION"
  else
    # Try redirect-based resolution as fallback
    local redirect_url="https://github.com/${OWNER}/${REPO}/releases/latest"
    if tag=$(curl -fsSL -o /dev/null -w '%{url_effective}' "$redirect_url" 2>/dev/null | sed -E 's|.*/tag/||'); then
      if [ -n "$tag" ] && [[ "$tag" =~ ^v[0-9] ]] && [[ "$tag" != *"/"* ]]; then
        VERSION="$tag"
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
        info "Resolved latest version via tags: $VERSION"
        return 0
      fi
    fi

    VERSION="v0.1.0"
    warn "Could not resolve latest version; defaulting to $VERSION"
  fi
}

detect_platform() {
  OS=$(uname -s | tr 'A-Z' 'a-z')
  ARCH=$(uname -m)
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
}

set_artifact_url() {
  TAR=""
  URL=""
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
  if [ -x "$DEST/$BIN_CLI" ]; then
    local current
    current=$("$DEST/$BIN_CLI" --version 2>/dev/null | head -1 || echo "")
    if [ -n "$current" ]; then
      info "Existing am detected: $current"
    fi
  fi
  if [ -x "$DEST/$BIN_SERVER" ]; then
    local current
    current=$("$DEST/$BIN_SERVER" --version 2>/dev/null | head -1 || echo "")
    if [ -n "$current" ]; then
      info "Existing mcp-agent-mail detected: $current"
    fi
  fi
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
    if grep -q '# >>> MCP Agent Mail' "$rc" 2>/dev/null; then
      PYTHON_ALIAS_FOUND=1
      PYTHON_ALIAS_FILE="$rc"
      PYTHON_ALIAS_HAS_MARKERS=1
      PYTHON_ALIAS_LINE=$(grep -n '# >>> MCP Agent Mail' "$rc" | head -1 | cut -d: -f1)
      # Extract the alias content from within the marker block
      PYTHON_ALIAS_CONTENT=$(sed -n '/# >>> MCP Agent Mail/,/# <<< MCP Agent Mail/p' "$rc" | grep -E "^alias am=|^alias am " | head -1)
      return 0
    fi

    # Check for bare "alias am=" (bash/zsh) or "alias am " (fish) outside markers
    local alias_line=""
    alias_line=$(grep -n -E "^[[:space:]]*(alias am=|alias am )" "$rc" 2>/dev/null | grep -iv "disabled\|#.*alias am" | head -1)
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
      PYTHON_ALIAS_HAS_MARKERS=0
      return 0
    fi

    # Check for function definition: "function am()" or "am()" (bash/zsh)
    # Or "function am" (fish)
    local func_line=""
    func_line=$(grep -n -E "^[[:space:]]*(function am[[:space:](]|am[[:space:]]*\(\))" "$rc" 2>/dev/null | grep -v "^[[:space:]]*#" | head -1)
    if [ -n "$func_line" ]; then
      local line_content
      line_content=$(echo "$func_line" | cut -d: -f2-)
      if ! echo "$line_content" | grep -q "^[[:space:]]*#"; then
        PYTHON_ALIAS_FOUND=1
        PYTHON_ALIAS_FILE="$rc"
        PYTHON_ALIAS_LINE=$(echo "$func_line" | cut -d: -f1)
        PYTHON_ALIAS_CONTENT="$line_content"
        PYTHON_ALIAS_HAS_MARKERS=0
        return 0
      fi
    fi
  done
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
        return 0
      fi
    fi

    # Check if it's in a Python virtualenv or site-packages directory
    if echo "$am_path" | grep -qiE "venv|virtualenv|site-packages|\.local/lib/python"; then
      PYTHON_BINARY_FOUND=1
      PYTHON_BINARY_PATH="$am_path"
      return 0
    fi
  done <<< "$all_am"

  # Also check for python -m mcp_agent_mail availability
  if command -v python3 >/dev/null 2>&1 && python3 -c "import mcp_agent_mail" 2>/dev/null; then
    PYTHON_BINARY_FOUND=1
    PYTHON_BINARY_PATH="python3 -m mcp_agent_mail"
  elif command -v python >/dev/null 2>&1 && python -c "import mcp_agent_mail" 2>/dev/null; then
    PYTHON_BINARY_FOUND=1
    PYTHON_BINARY_PATH="python -m mcp_agent_mail"
  fi
}

# T1.3: Detect Python virtualenv and git clone
detect_python_installation() {
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
}

# Run all Python detection in sequence
detect_python() {
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
    # Comment out the bare alias line
    local line_num="$PYTHON_ALIAS_LINE"
    awk -v line="$line_num" -v dest="$DEST" '
      NR == line { print "# Disabled by mcp-agent-mail Rust installer: " $0; print "# Rust binary at: " dest "/am"; next }
      { print }
    ' "$rc" > "$tmpfile"
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

# ── End Python detection & displacement ────────────────────────────────────

preflight_checks() {
  info "Running preflight checks"
  check_disk_space
  check_write_permissions
  check_existing_install
  check_network
}

maybe_add_path() {
  case ":$PATH:" in
    *:"$DEST":*) return 0;;
    *)
      if [ "$EASY" -eq 1 ]; then
        UPDATED=0
        for rc in "$HOME/.zshrc" "$HOME/.bashrc"; do
          if [ -e "$rc" ] && [ -w "$rc" ]; then
            if ! grep -F "$DEST" "$rc" >/dev/null 2>&1; then
              echo "export PATH=\"$DEST:\$PATH\"" >> "$rc"
            fi
            UPDATED=1
          fi
        done
        if [ "$UPDATED" -eq 1 ]; then
          warn "PATH updated in ~/.zshrc/.bashrc; restart shell to use am / mcp-agent-mail"
        else
          warn "Add $DEST to PATH to use am / mcp-agent-mail"
        fi
      else
        warn "Add $DEST to PATH to use am / mcp-agent-mail"
      fi
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
    err "Checksum verification FAILED!"
    err "Expected: $expected"
    err "Got:      $actual"
    err "The downloaded file may be corrupted or tampered with."
    rm -f "$file"
    return 1
  fi

  ok "Checksum verified: ${actual:0:16}..."
  return 0
}

# Verify Sigstore/cosign bundle for a file (best-effort)
verify_sigstore_bundle() {
  local file="$1"
  local artifact_url="$2"

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
  if ! curl -fsSL "$bundle_url" -o "$bundle_file"; then
    warn "Sigstore bundle not found; skipping signature verification"
    return 0
  fi

  if ! cosign verify-blob \
    --bundle "$bundle_file" \
    --certificate-identity-regexp "$COSIGN_IDENTITY_RE" \
    --certificate-oidc-issuer "$COSIGN_OIDC_ISSUER" \
    "$file"; then
    return 1
  fi

  ok "Signature verified (cosign)"
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
                  [--offline] [--no-gum] [--no-verify] [--force] [--from-source]

Installs mcp-agent-mail and am (CLI) binaries.

Options:
  --version vX.Y.Z   Install specific version (default: latest)
  --dest DIR         Install to DIR (default: ~/.local/bin)
  --system           Install to /usr/local/bin (requires sudo)
  --easy-mode        Auto-update PATH in shell rc files
  --verify           Run self-test after install
  --from-source      Build from source instead of downloading binary
  --quiet            Suppress non-error output
  --offline          Skip network preflight checks
  --no-gum           Disable gum formatting even if available
  --no-verify        Skip checksum + signature verification (for testing only)
  --force            Force reinstall even if same version is installed
EOFU
}

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="$2"; shift 2;;
    --dest) DEST="$2"; shift 2;;
    --system) SYSTEM=1; DEST="/usr/local/bin"; shift;;
    --easy-mode) EASY=1; shift;;
    --verify) VERIFY=1; shift;;
    --artifact-url) ARTIFACT_URL="$2"; shift 2;;
    --checksum) CHECKSUM="$2"; shift 2;;
    --checksum-url) CHECKSUM_URL="$2"; shift 2;;
    --from-source) FROM_SOURCE=1; shift;;
    --quiet|-q) QUIET=1; shift;;
    --offline) OFFLINE=1; shift;;
    --no-gum) NO_GUM=1; shift;;
    --no-verify) NO_CHECKSUM=1; shift;;
    --force) FORCE_INSTALL=1; shift;;
    -h|--help) usage; exit 0;;
    *) shift;;
  esac
done

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
  rm -rf "$TMP"
  if [ "$LOCKED" -eq 1 ]; then rm -rf "$LOCK_DIR"; fi
}

TMP=$(mktemp -d)
trap cleanup EXIT

if [ "$FROM_SOURCE" -eq 0 ]; then
  info "Downloading $URL"
  if ! curl -fsSL "$URL" -o "$TMP/$TAR"; then
    warn "Binary download failed (release may not exist for $VERSION)"
    warn "Attempting build from source as fallback..."
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
  install -m 0755 "$local_server" "$DEST/$BIN_SERVER"
  install -m 0755 "$local_cli" "$DEST/$BIN_CLI"
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
    if ! curl -fsSL "$CHECKSUM_URL" -o "$CHECKSUM_FILE"; then
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

install -m 0755 "$SERVER_BIN" "$DEST/$BIN_SERVER"
install -m 0755 "$CLI_BIN" "$DEST/$BIN_CLI"
ok "Installed to $DEST"
ok "  $DEST/$BIN_SERVER"
ok "  $DEST/$BIN_CLI"
maybe_add_path

# Displace Python installation if detected
if [ "$PYTHON_DETECTED" -eq 1 ]; then
  stop_python_server
  displace_python_alias
  resolve_database_path
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

# Run database migration if we copied a Python DB
if [ -n "$PYTHON_DB_MIGRATED_PATH" ] && [ -f "$PYTHON_DB_MIGRATED_PATH" ]; then
  info "Running database migration on copied Python database"
  if DATABASE_URL="sqlite+aiosqlite:///$PYTHON_DB_MIGRATED_PATH" "$DEST/$BIN_CLI" migrate 2>&1; then
    ok "Database schema migrated"
  else
    warn "Database migration had issues (you can retry with: am migrate)"
  fi
fi

if [ "$VERIFY" -eq 1 ]; then
  "$DEST/$BIN_CLI" --version || true
  ok "Self-test complete"
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
