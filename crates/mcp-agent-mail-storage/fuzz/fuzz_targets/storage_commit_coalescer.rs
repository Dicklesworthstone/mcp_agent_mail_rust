#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use mcp_agent_mail_storage::{commit_lock_path, fuzz_coalescer_batch_summary};
use std::path::Path;

#[derive(Arbitrary, Debug)]
struct CoalescerInput {
    wall_offsets_seconds: Vec<i64>,
    rel_paths: Vec<String>,
}

fuzz_target!(|input: CoalescerInput| {
    let summary = fuzz_coalescer_batch_summary(&input.wall_offsets_seconds);
    let expected_count = input.wall_offsets_seconds.len().min(128);
    if expected_count == 0 {
        assert_eq!(summary, "batch: 0 events");
    } else {
        assert!(
            summary.starts_with(&format!("batch: {expected_count} events (")),
            "batch summary should report the bounded request count"
        );
    }

    let bounded_paths: Vec<String> = input.rel_paths.into_iter().take(64).collect();
    let refs: Vec<&str> = bounded_paths.iter().map(String::as_str).collect();
    let lock_path = commit_lock_path(Path::new("/tmp/mcp-agent-mail-fuzz-repo"), &refs);
    assert!(
        lock_path.starts_with("/tmp/mcp-agent-mail-fuzz-repo"),
        "commit lock paths must stay under the supplied repo root"
    );
});
