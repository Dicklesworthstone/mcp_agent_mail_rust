#!/usr/bin/env bash
# Deterministic stub encoder that always fails.
# Passes validation (--help/--version) but fails on --encode.
#
# Used for testing fallback behavior.

set -euo pipefail

for arg in "$@"; do
  case "$arg" in
    --help)
      echo "toon_stub_encoder_fail — reference implementation in rust (stub)"
      exit 0
      ;;
    --version)
      echo "tru 0.0.0-stub-fail"
      exit 0
      ;;
    --encode)
      # Read stdin then fail
      cat > /dev/null
      echo "error: simulated encoder failure" >&2
      exit 1
      ;;
    *) ;;
  esac
done
