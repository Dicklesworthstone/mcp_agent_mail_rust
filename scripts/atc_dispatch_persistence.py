#!/usr/bin/env python3
"""Apply and regression-test GH258's deferred ATC experience persistence.

This is a guarded, source-native migration, not a build-time source rewrite.
Run `python3 scripts/atc_dispatch_persistence.py apply` once, then commit lib.rs.
`test` compiles the actual admission/dispatch loops extracted from lib.rs with
counting persistence/execution adapters. It requires rustc, but no crate deps.
No production text is written by `check`, `harness`, or `test`.
"""
from __future__ import annotations

import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
from textwrap import indent

SOURCE = Path("crates/mcp-agent-mail-server/src/lib.rs")
START = "        for mut effect in new_effects {"
NEW_START = "        // GH258: admission is memory-only; persist only budgeted dispatches.\n        for effect in new_effects {"
MIDDLE = "        let mut processed_this_tick = 0_usize;"
END = "        // Periodic resolution sweep for open experiences"
APPEND = '''            if durable_writes_enabled {
                if let Some(pool) = atc_db_pool.as_ref() {
                    match append_atc_experience_for_effect(pool, &effect) {
                        Ok(experience) => {
                            effect.experience_id = Some(experience.experience_id);
                        }
                        Err(error) => {
                            tracing::warn!(
                                decision_id = effect.decision_id,
                                effect_id = %effect.effect_id,
                                %error,
                                "failed to append ATC experience"
                            );
                        }
                    }
                }
            }
'''
DISPATCH = '''            let Some(effect) = pending_effects.pop_front() else {
                continue;
            };
            pending_effect_keys.remove(&cooldown_key);
            let status = execute_atc_effect('''
NEW_DISPATCH = '''            let Some(mut effect) = pending_effects.pop_front() else {
                continue;
            };
            pending_effect_keys.remove(&cooldown_key);
            // GH258: this point is after deduplication, eviction, and cooldown
            // suppression, and inside ATC_OPERATOR_ACTION_CAPACITY. Never create
            // orphan Planned rows for proposals that cannot be dispatched.
            if effect.experience_id.is_none() {
''' + indent(APPEND, "    ") + '''            }
            let status = execute_atc_effect('''


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise ValueError(f"{label}: expected exactly one source match, found {count}; no source written")
    return text.replace(old, new, 1)


def loop_region(source: str) -> tuple[int, int, str]:
    operator = source.index("fn run_atc_operator_loop(")
    end = source.index(END, operator)
    before = source[operator:end]
    start_marker = NEW_START if NEW_START in before else START
    start = source.index(start_marker, operator, end)
    return start, end, source[start:end]


def check(source: str) -> None:
    _, _, region = loop_region(source)
    admission, dispatch = region.split(MIDDLE, 1)
    if "append_atc_experience_for_effect(" in admission:
        raise ValueError("GH258 remains: persistence occurs before admission/deduplication")
    if dispatch.count("append_atc_experience_for_effect(") != 1:
        raise ValueError("expected exactly one dispatch-side experience append")
    cooldown = dispatch.index("if throttled {")
    dequeue = dispatch.index("let Some(mut effect) = pending_effects.pop_front()")
    append = dispatch.index("append_atc_experience_for_effect(")
    execute = dispatch.index("let status = execute_atc_effect(")
    if not cooldown < dequeue < append < execute:
        raise ValueError("experience append is not after cooldown and before execution")
    if "while processed_this_tick < ATC_OPERATOR_ACTION_CAPACITY" not in dispatch:
        raise ValueError("dispatch action budget is missing")
    if "dropped.experience_id.is_some()" not in admission:
        raise ValueError("eviction capture must skip intentionally unpersisted effects")
    if "effect.experience_id.is_some()" not in dispatch[cooldown:dequeue]:
        raise ValueError("cooldown capture must skip intentionally unpersisted effects")


def transform(source: str) -> str:
    if NEW_START in source:
        check(source)
        return source
    start, end, region = loop_region(source)
    region = replace_once(region, START, NEW_START, "admission loop")
    region = replace_once(region, APPEND, "", "eager experience append")
    region = replace_once(region, DISPATCH, NEW_DISPATCH, "dispatch append")
    region = replace_once(
        region,
        "if durable_writes_enabled && let Some(pool) = atc_db_pool.as_ref() {\n                            capture_atc_execution_result(",
        "if durable_writes_enabled\n                            && dropped.experience_id.is_some()\n                            && let Some(pool) = atc_db_pool.as_ref()\n                        {\n                            capture_atc_execution_result(",
        "eviction capture guard",
    )
    region = replace_once(
        region,
        "if durable_writes_enabled && let Some(pool) = atc_db_pool.as_ref() {\n                    capture_atc_execution_result(",
        "if durable_writes_enabled\n                    && effect.experience_id.is_some()\n                    && let Some(pool) = atc_db_pool.as_ref()\n                {\n                    capture_atc_execution_result(",
        "cooldown capture guard",
    )
    region = region.replace(
        "// Throttled outcomes still perform durable work, so they must\n                // count against the per-tick action budget.",
        "// Suppressed outcomes still count against the action budget,\n                // even when they intentionally have no durable experience.",
    )
    result = source[:start] + region + source[end:]
    check(result)
    return result


HARNESS_PREFIX = r'''
#![allow(dead_code, unused_variables)]
use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
extern crate self as tracing;
#[macro_export]
macro_rules! warn { ($($tokens:tt)*) => {{}}; }
const ATC_OPERATOR_ACTION_CAPACITY: usize = 64;
const MAX_PENDING_EFFECTS: usize = 512;
const ATC_QUEUE_BACKPRESSURE_STATUS: &str = "throttled:pending_queue_capacity";
#[derive(Clone, Debug)]
struct Semantics { cooldown_key: String, cooldown_micros: i64, family: String }
#[derive(Clone, Debug)]
struct Effect { experience_id: Option<u64>, semantics: Semantics }
#[derive(Default)]
struct Pool { appends: Cell<usize>, captures: Cell<usize>, missing: Cell<usize>, fail: bool }
struct Experience { experience_id: u64 }
#[derive(Clone, Copy)]
struct Mode;
impl Mode { fn as_str(self) -> &'static str { "live" } }
fn atc_effect_semantic_key(effect: &Effect) -> String { effect.semantics.cooldown_key.clone() }
fn append_atc_experience_for_effect(pool: &Pool, _: &Effect) -> Result<Experience, &'static str> {
    pool.appends.set(pool.appends.get() + 1);
    if pool.fail { return Err("injected append failure"); }
    Ok(Experience { experience_id: pool.appends.get() as u64 })
}
fn capture_atc_execution_result(pool: &Pool, id: Option<u64>, _: &str, _: &str, _: i64) {
    if id.is_some() { pool.captures.set(pool.captures.get() + 1); }
    else { pool.missing.set(pool.missing.get() + 1); }
}
fn atc_execution_snapshot(_: i64, _: &Effect, _: &str, status: &str) -> String { status.into() }
fn record_atc_operator_execution(recent: &mut VecDeque<String>, actions: &mut VecDeque<String>, visible: &mut Vec<String>, status: String) {
    for queue in [recent, actions] {
        if queue.len() == ATC_OPERATOR_ACTION_CAPACITY { queue.pop_front(); }
        queue.push_back(status.clone());
    }
    if visible.len() < ATC_OPERATOR_ACTION_CAPACITY { visible.push(status); }
}
fn execute_atc_effect(_: Option<&()>, _: Mode, seen: &mut Vec<Option<u64>>, effect: &Effect) -> String {
    seen.push(effect.experience_id);
    "executed".into()
}
fn atc_status_consumes_cooldown(status: &str) -> bool {
    !status.starts_with("failed:") && status != "suppressed:missing_project_precondition"
}
fn effect(index: usize) -> Effect {
    Effect { experience_id: None, semantics: Semantics {
        cooldown_key: format!("project:agent:{index}"), cooldown_micros: 60_000_000,
        family: "liveness_probe".into(),
    } }
}
struct ResultCounts { appends: usize, captures: usize, missing: usize, executions: Vec<Option<u64>>, pending: usize, processed: usize, visible: Vec<String> }
fn run(new_effects: Vec<Effect>, mut last_action_by_key: HashMap<String, i64>, durable_writes_enabled: bool, fail: bool) -> ResultCounts {
    let pool = Pool { fail, ..Pool::default() };
    let atc_db_pool = Some(pool);
    let now_micros: i64 = 10_000_000;
    let executor_mode = Mode;
    let executor_runtime = Some(());
    let mut executor_registered_projects = Vec::new();
    let mut pending_effects: VecDeque<Effect> = VecDeque::new();
    let mut pending_effect_keys: HashSet<String> = HashSet::new();
    let mut recent_executions = VecDeque::new();
    let mut recent_actions = VecDeque::new();
    let mut visible_actions = Vec::new();
'''
HARNESS_SUFFIX = r'''
    let pool = atc_db_pool.unwrap();
    ResultCounts { appends: pool.appends.get(), captures: pool.captures.get(), missing: pool.missing.get(), executions: executor_registered_projects, pending: pending_effects.len(), processed: processed_this_tick, visible: visible_actions }
}
#[test]
fn burst_940_agents_persists_only_64_dispatches() {
    let r = run((0..940).map(effect).collect(), HashMap::new(), true, false);
    assert_eq!(r.appends, 64, "admission must not issue unbudgeted DB writes");
    assert_eq!(r.captures, 64);
    assert_eq!(r.missing, 0, "evictions are intentional, not failed appends");
    assert_eq!(r.executions.len(), 64);
    assert!(r.executions.iter().all(Option::is_some));
    assert_eq!(r.pending, 448);
    assert!(r.visible.iter().any(|s| s == ATC_QUEUE_BACKPRESSURE_STATUS));
}
#[test]
fn ten_thousand_duplicate_proposals_persist_once() {
    let r = run(vec![effect(0); 10_000], HashMap::new(), true, false);
    assert_eq!(r.appends, 1);
    assert_eq!(r.captures, 1);
    assert_eq!(r.executions.len(), 1);
    assert_eq!(r.pending, 0);
}
#[test]
fn cooled_down_burst_does_not_write_or_report_missing_experiences() {
    let effects: Vec<_> = (0..940).map(effect).collect();
    let cooldowns = effects.iter().map(|e| (atc_effect_semantic_key(e), 10_000_000)).collect();
    let r = run(effects, cooldowns, true, false);
    assert_eq!((r.appends, r.captures, r.missing), (0, 0, 0));
    assert!(r.executions.is_empty());
    assert_eq!(r.processed, 64, "throttled effects must consume the processing budget");
    assert_eq!(r.pending, 448);
}
#[test]
fn disabled_write_gate_is_preserved() {
    let r = run((0..940).map(effect).collect(), HashMap::new(), false, false);
    assert_eq!((r.appends, r.captures, r.missing), (0, 0, 0));
    assert!(r.executions.iter().all(Option::is_none));
}
#[test]
fn append_failure_does_not_prevent_execution() {
    let r = run(vec![effect(0)], HashMap::new(), true, true);
    assert_eq!(r.appends, 1);
    assert_eq!(r.executions, vec![None]);
    assert_eq!(r.missing, 1, "a genuine append failure must retain its diagnostic");
}
#[test]
fn existing_experience_is_not_appended_again() {
    let mut e = effect(0);
    e.experience_id = Some(123);
    let r = run(vec![e], HashMap::new(), true, false);
    assert_eq!(r.appends, 0);
    assert_eq!(r.executions, vec![Some(123)]);
    assert_eq!(r.captures, 1);
}
#[test]
fn empty_tick_is_write_free() {
    let r = run(Vec::new(), HashMap::new(), true, false);
    assert_eq!((r.appends, r.captures, r.missing), (0, 0, 0));
    assert_eq!(r.processed, 0);
}
'''


def harness(source: str) -> str:
    # These are the production Rust statements, not a Python model of the queue.
    _, _, region = loop_region(source)
    return HARNESS_PREFIX + region + HARNESS_SUFFIX


def run_rust_tests(source: str, expect_failure: bool) -> None:
    compiler = shutil.which("rustc")
    if compiler is None:
        raise RuntimeError("rustc is required; no Rust tests were run")
    with tempfile.TemporaryDirectory(prefix="atc-dispatch-regressions-") as tmp:
        root = Path(tmp)
        rs = root / "atc_dispatch_regressions.rs"
        binary = root / ("atc_dispatch_regressions.exe" if os.name == "nt" else "atc_dispatch_regressions")
        rs.write_text(harness(source), encoding="utf-8")
        # A compile failure is always a failure, including the negative control.
        subprocess.run([compiler, "--edition=2024", "--test", str(rs), "-o", str(binary)], check=True)
        result = subprocess.run([str(binary), "--nocapture"], check=False)
        if expect_failure:
            if result.returncode == 0:
                raise RuntimeError("negative control unexpectedly passed")
            print("Negative control reproduced the pre-fix assertions.")
        elif result.returncode != 0:
            raise RuntimeError("ATC dispatch persistence regressions failed")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("apply", "check", "test", "harness"))
    parser.add_argument("--source", type=Path, default=SOURCE)
    parser.add_argument("--expect-failure", action="store_true", help="test-only negative control; compilation must still succeed")
    args = parser.parse_args()
    source = args.source.read_text(encoding="utf-8")
    if args.expect_failure and args.command != "test":
        parser.error("--expect-failure requires test")
    if args.command == "apply":
        patched = transform(source)
        if source == patched:
            print("Already applied; source invariants checked.")
            return
        # Validate every replacement before touching the source; preserve mode.
        with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=args.source.parent, delete=False) as tmp:
            tmp.write(patched)
            temporary = Path(tmp.name)
        try:
            temporary.chmod(args.source.stat().st_mode)
            temporary.replace(args.source)
        finally:
            if temporary.exists():
                temporary.unlink()
        print(f"Applied GH258 deferred persistence to {args.source}")
    elif args.command == "check":
        check(source)
        print("ATC experience persistence is after admission/cooldown and within the dispatch budget.")
    elif args.command == "harness":
        sys.stdout.write(harness(source))
    else:
        run_rust_tests(source, args.expect_failure)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, RuntimeError, OSError, subprocess.CalledProcessError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        sys.exit(1)
