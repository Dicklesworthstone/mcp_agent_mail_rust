#!/usr/bin/env bash
# Deterministic stub encoder for TOON tests.
# Reads JSON from stdin, outputs a fixed TOON-encoded string.
# Supports: --encode, --stats, --help, --version, --fail, --fail-code=N
#
# Usage:
#   echo '{"id":1}' | ./toon_stub_encoder.sh --encode
#   echo '{"id":1}' | ./toon_stub_encoder.sh --encode --stats
#   ./toon_stub_encoder.sh --help
#   ./toon_stub_encoder.sh --version
#   echo '{"id":1}' | ./toon_stub_encoder.sh --encode --fail         # exit 1
#   echo '{"id":1}' | ./toon_stub_encoder.sh --encode --fail-code=42 # exit 42

set -euo pipefail

EMIT_STATS=false
FAIL=false
FAIL_CODE=1

for arg in "$@"; do
  case "$arg" in
    --encode) ;;
    --stats) EMIT_STATS=true ;;
    --help)
      echo "toon_stub_encoder — reference implementation in rust (stub)"
      echo "Usage: toon_stub_encoder --encode [--stats]"
      exit 0
      ;;
    --version)
      echo "tru 0.0.0-stub"
      exit 0
      ;;
    --fail)
      FAIL=true
      ;;
    --fail-code=*)
      FAIL=true
      FAIL_CODE="${arg#--fail-code=}"
      ;;
    *)
      echo "unknown arg: $arg" >&2
      exit 1
      ;;
  esac
done

# Read stdin (the JSON payload)
INPUT=$(cat)

if [ "$FAIL" = true ]; then
  echo "error: simulated encoder failure" >&2
  exit "$FAIL_CODE"
fi

# Output a deterministic TOON-encoded representation
# (a simplified TOON-like text; real tru would produce actual TOON)
echo "~stub_toon_output"
echo "  payload_length: ${#INPUT}"
echo "  checksum: stub"

if [ "$EMIT_STATS" = true ]; then
  # Emit stats to stderr in the expected format
  echo "Token estimates: ~25 (JSON) -> ~12 (TOON)" >&2
  echo "Saved ~13 tokens (-52.0%)" >&2
fi
