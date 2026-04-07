#!/usr/bin/env python3
"""Generate deterministic share/export conformance fixtures.

This wrapper delegates to the standalone generator, which now owns the actual
fixture-building implementation.
"""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path


def main() -> int:
    script_dir = Path(__file__).resolve().parent
    standalone = script_dir / "generate_fixtures_standalone.py"
    proc = subprocess.run([sys.executable, str(standalone), *sys.argv[1:]], check=False)
    return int(proc.returncode)


if __name__ == "__main__":
    raise SystemExit(main())
