#!/usr/bin/env bash
# Fixture corpus helpers for E2E search suites.

set -euo pipefail

seed_corpus_help() {
    cat <<'EOF'
Usage:
  source tests/e2e/lib/seed_corpus.sh
  seed_mixed_quality_corpus <target_dir> [quality_count] [fallback_count]

Creates a small corpus with quality and fallback-only documents plus manifest.jsonl.
EOF
}

seed_mixed_quality_corpus() {
    local target_dir="$1"
    local quality_count="${2:-5}"
    local fallback_count="${3:-5}"
    mkdir -p "${target_dir}"
    python3 - "${target_dir}" "${quality_count}" "${fallback_count}" <<'PY'
import json
import sys
from pathlib import Path

target = Path(sys.argv[1])
quality_count = int(sys.argv[2])
fallback_count = int(sys.argv[3])
quality_dir = target / "quality"
fallback_dir = target / "fallback"
quality_dir.mkdir(parents=True, exist_ok=True)
fallback_dir.mkdir(parents=True, exist_ok=True)
manifest = []

for idx in range(quality_count):
    doc_id = f"quality-{idx + 1}"
    path = quality_dir / f"{doc_id}.md"
    path.write_text(
        f"# Quality document {idx + 1}\nneedle semantic refinement quality embedding\n",
        encoding="utf-8",
    )
    manifest.append({"id": doc_id, "quality_embedding": True, "path": str(path)})

for idx in range(fallback_count):
    doc_id = f"fallback-{idx + 1}"
    path = fallback_dir / f"{doc_id}.md"
    path.write_text(
        f"# Fallback document {idx + 1}\nneedle fast-tier fallback only\n",
        encoding="utf-8",
    )
    manifest.append({"id": doc_id, "quality_embedding": False, "path": str(path)})

(target / "manifest.jsonl").write_text(
    "".join(json.dumps(item, sort_keys=True) + "\n" for item in manifest),
    encoding="utf-8",
)
PY
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
    case "${1:-}" in
        -h|--help|"")
            seed_corpus_help
            ;;
        *)
            seed_corpus_help >&2
            exit 2
            ;;
    esac
fi
