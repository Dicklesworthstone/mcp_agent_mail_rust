#!/usr/bin/env bash
# test_share_wizard.sh - E2E wrapper for native share wizard suite (br-18tuh)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
bash "${SCRIPT_DIR}/../../scripts/e2e_share_wizard.sh"
