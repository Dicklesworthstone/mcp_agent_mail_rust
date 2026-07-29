#!/usr/bin/env bash
set -euo pipefail

if [[ "${1:-}" == "--version" ]]; then
  echo "git version 2.51.0"
  exit 0
fi

REAL_GIT="${AM_TEST_REAL_GIT:-}"
if [[ -z "$REAL_GIT" ]]; then
  REAL_GIT="$(command -v git)"
fi

PROB="${AM_TEST_RACER_PROB:-0.05}"
TRIGGERS="${AM_TEST_RACER_TRIGGERS:-update-ref,commit}"
STATE="${AM_TEST_RACER_STATE:-${TMPDIR:-/tmp}/am-git-segfault-shim.state}"
EVENT_LOG="${AM_TEST_RACER_EVENT_LOG:-}"
SCENARIO="${AM_TEST_RACER_SCENARIO:-scenario_b}"

matches_trigger=0
for trig in ${TRIGGERS//,/ }; do
  if [[ " $* " == *" $trig "* ]]; then
    matches_trigger=1
    break
  fi
done

json_event() {
  local level="$1" message="$2" injected="$3" sequence="$4"
  if [[ -z "$EVENT_LOG" ]]; then
    return
  fi
  mkdir -p "$(dirname "$EVENT_LOG")"
  printf '{"ts":"%s","test":"stress_known_bad_git","scenario":"%s","level":"%s","message":"%s","data":{"injected":%s,"sequence":%s,"argv":"%s"}}\n' \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    "$SCENARIO" \
    "$level" \
    "$message" \
    "$injected" \
    "$sequence" \
    "$(printf '%s' "$*" | sed 's/\\/\\\\/g; s/"/\\"/g')" >> "$EVENT_LOG"
}

should_inject=0
sequence=0
if [[ "$matches_trigger" == 1 ]]; then
  mkdir -p "$(dirname "$STATE")"
  {
    flock 9
    if [[ -f "$STATE" ]]; then
      sequence="$(cat "$STATE")"
    fi
    sequence=$((sequence + 1))
    printf '%s\n' "$sequence" > "$STATE"
  } 9>"$STATE.lock"

  case "$PROB" in
    0|0.0|0.00)
      should_inject=0
      ;;
    1|1.0|1.00)
      should_inject=1
      ;;
    *)
      period="$(awk -v p="$PROB" 'BEGIN { if (p <= 0) print 0; else printf "%d\n", int((1 / p) + 0.5) }')"
      if [[ "$period" -gt 0 && $((sequence % period)) -eq 0 ]]; then
        should_inject=1
      fi
      ;;
  esac
fi

if [[ "$should_inject" == 1 ]]; then
  json_event warn synthetic_sigsegv true "$sequence"
  kill -SEGV $$ 2>/dev/null || exit 139
fi

json_event debug pass_through false "$sequence"
exec "$REAL_GIT" "$@"
