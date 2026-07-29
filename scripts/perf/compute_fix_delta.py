#!/usr/bin/env python3
"""Compute br-q8yaa pre/post archive perf deltas."""

from __future__ import annotations

import argparse
import json
import pathlib
import sys
from typing import Any


MANDATED_POINTS = (
    "batch-1",
    "batch-10",
    "batch-100",
    "batch-1000",
    "single-attachment",
    "30-agent-stress",
)

PERCENTILE_FIELDS = ("p50_us", "p95_us", "p99_us")


def load_baseline(path: pathlib.Path) -> dict[str, Any]:
    payload = json.loads(path.read_text(encoding="utf-8"))
    if payload.get("schema_version") != 1:
        raise ValueError(f"{path} has unsupported schema_version")
    bench_points = payload.get("bench_points")
    if not isinstance(bench_points, dict):
        raise ValueError(f"{path} missing bench_points object")
    missing = [point for point in MANDATED_POINTS if point not in bench_points]
    if missing:
        raise ValueError(f"{path} missing mandated point(s): {', '.join(missing)}")
    return payload


def percent_improvement(pre_value: int, post_value: int) -> float:
    if pre_value <= 0:
        return 0.0
    return round(((pre_value - post_value) / pre_value) * 100.0, 2)


def point_delta(point: str, pre: dict[str, Any], post: dict[str, Any]) -> dict[str, Any]:
    result: dict[str, Any] = {
        "point": point,
        "status": "pass",
        "pre_status": pre.get("status", "unknown"),
        "post_status": post.get("status", "unknown"),
    }

    for field in PERCENTILE_FIELDS:
        pre_value = int(pre.get(field, 0))
        post_value = int(post.get(field, 0))
        delta = post_value - pre_value
        result[f"pre_{field}"] = pre_value
        result[f"post_{field}"] = post_value
        result[f"delta_{field}"] = delta
        result[f"percent_improvement_{field}"] = percent_improvement(pre_value, post_value)

    result["percent_improvement"] = result["percent_improvement_p95_us"]
    if result["delta_p95_us"] > 0:
        result["status"] = "regression"
    elif result["delta_p95_us"] < 0:
        result["status"] = "improved"
    return result


def compute(pre_path: pathlib.Path, post_path: pathlib.Path) -> dict[str, Any]:
    pre = load_baseline(pre_path)
    post = load_baseline(post_path)
    pre_points = pre["bench_points"]
    post_points = post["bench_points"]

    deltas = [
        point_delta(point, pre_points[point], post_points[point])
        for point in MANDATED_POINTS
    ]
    regression_count = sum(1 for item in deltas if item["status"] == "regression")
    improved_count = sum(1 for item in deltas if item["status"] == "improved")

    return {
        "schema_version": 1,
        "bead_id": "br-q8yaa",
        "status": "regression" if regression_count else "pass",
        "pre_baseline": str(pre_path),
        "post_baseline": str(post_path),
        "pre_meta": pre.get("meta", {}),
        "post_meta": post.get("meta", {}),
        "summary": {
            "points": len(deltas),
            "improved": improved_count,
            "regressed": regression_count,
        },
        "deltas": deltas,
    }


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Compute per-point archive perf deltas from pre/post baseline JSON artifacts."
    )
    parser.add_argument("pre_baseline", type=pathlib.Path)
    parser.add_argument("post_baseline", type=pathlib.Path)
    parser.add_argument(
        "-o",
        "--output",
        type=pathlib.Path,
        help="Write JSON here instead of stdout.",
    )
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    try:
        payload = compute(args.pre_baseline, args.post_baseline)
    except (OSError, ValueError, json.JSONDecodeError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2

    text = json.dumps(payload, indent=2, sort_keys=True) + "\n"
    if args.output is None:
        print(text, end="")
    else:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(text, encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
