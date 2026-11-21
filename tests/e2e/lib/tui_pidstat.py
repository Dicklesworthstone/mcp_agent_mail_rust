"""Read pidstat's horizontal reports without depending on the clock format."""

import math
import re
from pathlib import Path


_TIME = re.compile(r"^\d{2}:\d{2}:\d{2}(?:\s+(?:AM|PM))?\s+")


def read_samples(path, pid, metrics, *, threads=False):
    """Return (task id, metrics) rows for the exact captured process.

    With -t, pidstat emits a process summary and then its threads. A thread
    row has a TID but may leave the TGID blank (a dash in the report).
    """
    if not pid or not Path(path).is_file():
        return []

    samples = []
    columns = []
    group = None
    group_time = None
    with Path(path).open(encoding="utf-8", errors="replace") as report:
        for raw in report:
            line = raw.strip()
            if line.startswith("# Time "):
                columns = line.split()[2:]
                group = group_time = None
                continue
            clock = _TIME.match(line)
            if not clock or not columns:
                continue
            values = line[clock.end():].split(None, len(columns) - 1)
            if len(values) != len(columns):
                continue
            row = dict(zip(columns, values))
            tid = row.get("TID", "-")
            if "TGID" in row:
                timestamp = clock.group().strip()
                identity = row["TGID"]
                if tid == "-":
                    group, group_time = identity, timestamp
                elif identity == "-":
                    identity = group if group_time == timestamp else None
            else:
                identity = row.get("PID")
            if identity != str(pid) or threads != (tid != "-"):
                continue
            if threads and not tid.isdecimal():
                continue
            try:
                measured = tuple(float(row[field]) for field in metrics)
            except (KeyError, ValueError):
                continue
            if all(math.isfinite(value) and value >= 0 for value in measured):
                samples.append((tid if threads else identity, measured))
    return samples
