#![forbid(unsafe_code)]
#![allow(dead_code)]

#[path = "../benches/benchmarks.rs"]
mod benchmarks;

use std::collections::BTreeMap;
use std::path::Path;

use benchmarks::{
    ArchivePerfCategory, ArchivePerfComparison, ArchivePerfEnvironment,
    ArchivePerfHypothesisEvaluation, ArchivePerfReport, ArchivePerfReproduction, ChromeTraceEvent,
    RecordedSpan, archive_perf_logging_requirements, archive_perf_profile_markdown,
    archive_scaling_csv, inferred_span_parent, run_archive_perf_profile_once_for_testing,
};

fn perf_artifact_path(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/artifacts/perf")
        .join(name)
}

fn comparison(batch_size: usize, sample_count: usize) -> ArchivePerfComparison {
    ArchivePerfComparison {
        batch_size,
        samples_us: vec![200; sample_count],
        p50_us: 200,
        p95_us: 240,
        p99_us: 250,
        p99_9_us: 250,
        p99_99_us: 250,
        max_us: 250,
        throughput_elements_per_sec: 123.45,
    }
}

#[test]
fn inferred_span_parent_maps_batch_children() {
    assert_eq!(
        inferred_span_parent("archive_batch.write_message_batch_bundle"),
        Some("archive_batch.write_message_batch".to_string())
    );
    assert_eq!(
        inferred_span_parent("archive_batch.flush_async_commits"),
        Some("archive_batch.sample".to_string())
    );
    assert_eq!(inferred_span_parent("archive_batch.sample"), None);
}

#[test]
fn archive_scaling_csv_preserves_script_compatible_column_order() {
    let csv = archive_scaling_csv(&[comparison(100, 3)]);
    let mut lines = csv.lines();
    assert_eq!(
        lines.next(),
        Some(
            "batch_size,p50_us,p95_us,p99_us,sample_count,p99_9_us,p99_99_us,max_us,throughput_elements_per_sec"
        )
    );

    let row: Vec<_> = lines.next().expect("row").split(',').collect();
    assert_eq!(row[0], "100");
    assert_eq!(row[4], "3");
    assert_eq!(row[8], "123.45");
}

#[test]
fn archive_perf_profile_markdown_includes_reproduction_environment_and_logging() {
    let report = ArchivePerfReport {
        run_id: "run".to_string(),
        arch: "x86_64".to_string(),
        os: "linux".to_string(),
        warm_db: true,
        environment: ArchivePerfEnvironment {
            repo_root: "/repo".to_string(),
            cargo_target_dir: Some("/target".to_string()),
            rustc_version: Some("rustc 1.0.0".to_string()),
            kernel_release: Some("6.0.0".to_string()),
            filesystem: Some("btrfs".to_string()),
            mount_source: Some("/dev/nvme0n1".to_string()),
            mount_options: Some("rw,noatime".to_string()),
            storage_model: Some("SSD".to_string()),
            storage_transport: Some("nvme".to_string()),
        },
        reproduction: ArchivePerfReproduction {
            warm_profile_command: "rch exec -- env cargo bench".to_string(),
            flamegraph_command: "cargo flamegraph".to_string(),
            cross_engineer_target: "within 10%".to_string(),
            dry_run_validated: true,
        },
        structured_logging_requirements: archive_perf_logging_requirements(),
        comparison: vec![comparison(100, 3)],
        batch_100_spans: vec![RecordedSpan {
            name: "archive_batch.sample".to_string(),
            parent: None,
            count_per_request: 1,
            fields: BTreeMap::new(),
            duration_us: 240,
        }],
        trace_events: vec![ChromeTraceEvent {
            name: "archive_batch.sample".to_string(),
            cat: "archive_batch".to_string(),
            ph: "X".to_string(),
            ts: 0,
            dur: 240,
            pid: 1,
            tid: 0,
            args: BTreeMap::new(),
        }],
        top_categories: vec![ArchivePerfCategory {
            category: "archive_batch.sample".to_string(),
            cumulative_us: 240,
            count: 1,
            p50_us: 240,
            p95_us: 240,
            avg_us: 240,
            max_us: 240,
        }],
        hypothesis_evaluations: vec![ArchivePerfHypothesisEvaluation {
            name: "file layout".to_string(),
            supports_or_rejects: "supports".to_string(),
            evidence: "archive burst dominates".to_string(),
        }],
        scaling_law_note: "sublinear".to_string(),
    };

    let markdown = archive_perf_profile_markdown(&report);

    assert!(markdown.contains("## Reproduction"));
    assert!(markdown.contains("## Environment"));
    assert!(markdown.contains("## Structured Logging Requirements"));
    assert!(markdown.contains("## Top 10 Spans by Cumulative Duration"));
    assert!(markdown.contains("## Chrome Trace"));
    assert!(markdown.contains("## Hypothesis Evaluation"));
    assert!(markdown.contains("## Scaling Law"));
}

#[test]
fn archive_perf_profile_run_emits_extended_artifacts() {
    run_archive_perf_profile_once_for_testing();

    let scaling_csv = std::fs::read_to_string(perf_artifact_path("archive_batch_scaling.csv"))
        .expect("scaling csv");
    assert!(scaling_csv.contains(
        "batch_size,p50_us,p95_us,p99_us,sample_count,p99_9_us,p99_99_us,max_us,throughput_elements_per_sec"
    ));
    assert!(scaling_csv.lines().any(|line| line.starts_with("50,")));
    assert!(scaling_csv.lines().any(|line| line.starts_with("500,")));
    assert!(scaling_csv.lines().any(|line| line.starts_with("1000,")));

    let spans_json = std::fs::read_to_string(perf_artifact_path("archive_batch_100_spans.json"))
        .expect("spans json");
    assert!(spans_json.contains("\"traceEvents\""));
    assert!(spans_json.contains("\"hypothesis_evaluations\""));
    assert!(spans_json.contains("\"count_per_request\""));
    assert!(spans_json.contains("\"parent\""));

    let profile_md = std::fs::read_to_string(perf_artifact_path("archive_batch_100_profile.md"))
        .expect("profile markdown");
    assert!(profile_md.contains("## Reproduction"));
    assert!(profile_md.contains("## Structured Logging Requirements"));
    assert!(profile_md.contains("## Hypothesis Evaluation"));

    assert!(perf_artifact_path("archive_batch_scaling_2026-04-18.csv").exists());
    assert!(perf_artifact_path("archive_batch_100_spans_2026-04-18.json").exists());
}
