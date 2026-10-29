#!/usr/bin/env python3
"""Run the production path-prefix validator with only the native Rust stdlib.

This isolates GH369's filesystem hypothesis when the full workspace cannot yet
compile. It is diagnostic evidence, not a substitute for checkpoint lifecycle
tests. The production function is extracted verbatim; no repository file is edited.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import sys


STORAGE = Path("crates/mcp-agent-mail-storage/src/lib.rs")

PROBE = r'''
fn describe_metadata(path: &Path) {
    match fs::symlink_metadata(path) {
        Ok(metadata) => println!(
            "symlink_metadata({path:?}) = ok: directory={} file={} symlink={}",
            metadata.is_dir(), metadata.is_file(), metadata.file_type().is_symlink()
        ),
        Err(error) => println!(
            "symlink_metadata({path:?}) = error: kind={:?} raw_os_error={:?} {error}",
            error.kind(), error.raw_os_error()
        ),
    }
}

#[test]
fn native_checkpoint_path_validation() {
    println!("native target: {} {}", std::env::consts::OS, std::env::consts::ARCH);
    let fixture = PathBuf::from(std::env::var_os("CHECKPOINT_PATH_PROBE_ROOT").unwrap());
    fs::create_dir_all(&fixture).unwrap();
    let canonical = fs::canonicalize(&fixture).unwrap();
    println!("canonical fixture: {canonical:?}");
    #[cfg(windows)]
    assert!(matches!(
        canonical.components().next(),
        Some(Component::Prefix(prefix)) if matches!(
            prefix.kind(),
            std::path::Prefix::VerbatimDisk(_) | std::path::Prefix::VerbatimUNC(_, _)
        )
    ), "the native Windows probe must exercise a canonical verbatim path");

    // Diagnose every original component even if one metadata call fails. The
    // bare drive prefix and its rooted successor must both appear in the log.
    let mut prefix = PathBuf::new();
    for component in canonical.components() {
        prefix.push(component.as_os_str());
        println!("component: {component:?}");
        describe_metadata(&prefix);
    }
    assert!(fs::symlink_metadata(&canonical).unwrap().is_dir());

    let sidecar = canonical.join(".am-message-reconcile-native-probe.json");
    assert!(!sidecar.exists(), "probe requires a fresh, absent sidecar");
    let prefix_result = path_existing_prefix_has_symlink(&sidecar);
    println!("production path_existing_prefix_has_symlink({sidecar:?}) = {prefix_result:?}");
    assert!(
        matches!(prefix_result, Ok(false)),
        "the production prefix validator rejected a canonical directory: {prefix_result:?}"
    );
    fs::write(&sidecar, b"path probe fixture").unwrap();
    assert!(matches!(path_existing_prefix_has_symlink(&sidecar), Ok(false)));
    assert_eq!(fs::read(&sidecar).unwrap(), b"path probe fixture");
}
'''


def extract_function(path: Path, name: str) -> bytes:
    """Extract one complete top-level function without changing any of its bytes."""
    source = path.read_bytes()
    starts = list(re.finditer(rb"(?m)^fn " + name.encode() + rb"\(", source))
    if len(starts) != 1:
        raise ValueError(f"expected exactly one top-level {name} in {path}")
    start = starts[0].start()
    closing = re.search(rb"(?m)^}\r?\n", source[start:])
    if closing is None:
        raise ValueError(f"missing top-level closing brace for {name} in {path}")
    end = start + closing.end()
    return source[start:end]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    repo, output = args.repo.resolve(), args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    receipt = {
        "scope": "native production path-validation diagnostic; not full checkpoint qualification",
        "platform": platform.platform(),
        "source_sha256": {},
        "functions": {},
        "steps": [],
    }
    parts = [b"use std::fs;\nuse std::path::{Component, Path, PathBuf};\n"]
    production = repo / STORAGE
    name = "path_existing_prefix_has_symlink"
    function = extract_function(production, name)
    receipt["source_sha256"][str(STORAGE)] = hashlib.sha256(production.read_bytes()).hexdigest()
    receipt["functions"][name] = hashlib.sha256(function).hexdigest()
    parts.append(function)
    source = output / "checkpoint_path_probe.rs"
    source.write_bytes(b"\n".join(parts) + PROBE.encode())
    binary = output / ("checkpoint_path_probe.exe" if os.name == "nt" else "checkpoint_path_probe")
    env = dict(os.environ, CHECKPOINT_PATH_PROBE_ROOT=str(output / "mailbox"))
    commands = (
        ("compiler", ["rustc", "-vV"], 30),
        ("compile", ["rustc", "--edition=2024", "--test", str(source), "-o", str(binary)], 120),
        ("probe", [str(binary), "--nocapture"], 30),
    )
    exit_code = 0
    for label, command, timeout in commands:
        result = subprocess.run(
            command, cwd=repo, env=env, stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT, timeout=timeout, check=False,
        )
        (output / f"{label}.log").write_bytes(result.stdout)
        sys.stdout.write(result.stdout.decode("utf-8", errors="replace"))
        receipt["steps"].append({"name": label, "command": command, "exit_code": result.returncode})
        (output / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n", encoding="utf-8")
        if result.returncode:
            exit_code = result.returncode
            break
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main())
