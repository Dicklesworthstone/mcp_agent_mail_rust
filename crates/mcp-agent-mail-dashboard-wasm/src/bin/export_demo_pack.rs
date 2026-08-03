//! Export aggregate-only mailbox counts into a sanitized public replay pack.

#[cfg(target_arch = "wasm32")]
fn main() {
    compile_error!("am-export-dashboard-demo is a native Unix-only offline exporter");
}

#[cfg(all(not(target_arch = "wasm32"), unix))]
use std::fs::File;
#[cfg(not(target_arch = "wasm32"))]
use std::io::Write;
#[cfg(not(target_arch = "wasm32"))]
use std::path::{Path, PathBuf};

#[cfg(not(target_arch = "wasm32"))]
use clap::Parser;
#[cfg(not(target_arch = "wasm32"))]
use mcp_agent_mail_dashboard_wasm::demo_pack::{DemoOperation, curated_public_demo};
#[cfg(not(target_arch = "wasm32"))]
use mcp_agent_mail_dashboard_wasm::exporter::{
    AggregateCounts, open_source_read_only, read_aggregates_snapshot,
};
#[cfg(not(target_arch = "wasm32"))]
use mcp_agent_mail_dashboard_wasm::tui_events::{DbStatSnapshot, MailEvent};

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Parser)]
#[command(
    name = "am-export-dashboard-demo",
    about = "Export privacy-bounded aggregate Agent Mail dashboard replay data"
)]
struct Args {
    /// Source Agent Mail SQLite database. Its path is never written to output.
    #[arg(long)]
    source: PathBuf,
    /// New output JSON path. The exporter refuses to overwrite existing files.
    #[arg(long)]
    output: PathBuf,
    /// Public source Git revision: exactly 40 lowercase hexadecimal characters.
    #[arg(long)]
    source_revision: String,
    /// Public ISO-8601 capture timestamp supplied by the release process.
    #[arg(long)]
    captured_at: String,
}

#[cfg(not(target_arch = "wasm32"))]
fn apply_aggregates(
    snapshot: &mut DbStatSnapshot,
    counts: AggregateCounts,
    message_delta: u64,
    ack_delta: i64,
    reservation_delta: i64,
) {
    snapshot.projects = counts.projects;
    snapshot.agents = counts.agents;
    snapshot.messages = counts.messages.saturating_add(message_delta);
    snapshot.file_reservations = counts
        .file_reservations
        .saturating_add_signed(reservation_delta);
    snapshot.contact_links = counts.contact_links;
    snapshot.ack_pending = counts.ack_pending.saturating_add_signed(ack_delta);
}

#[cfg(not(target_arch = "wasm32"))]
fn output_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

#[cfg(not(target_arch = "wasm32"))]
fn absolute_leaf_path(path: &Path, label: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let file_name = path
        .file_name()
        .ok_or_else(|| format!("{label} must name a file"))?;
    Ok(output_parent(path).canonicalize()?.join(file_name))
}

#[cfg(not(target_arch = "wasm32"))]
fn sqlite_sidecar_path(source: &Path, suffix: &str) -> PathBuf {
    let mut sidecar = source.as_os_str().to_owned();
    sidecar.push(suffix);
    PathBuf::from(sidecar)
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, PartialEq, Eq)]
struct ResolvedExportPaths {
    source: PathBuf,
    output: PathBuf,
}

#[cfg(not(target_arch = "wasm32"))]
fn validate_output_path(
    source: &Path,
    output: &Path,
) -> Result<ResolvedExportPaths, Box<dyn std::error::Error>> {
    let source = absolute_leaf_path(source, "source")?;
    let output = absolute_leaf_path(output, "output")?;
    let source_parent = output_parent(&source);
    let output_parent = output_parent(&output);

    if same_file::is_same_file(source_parent, output_parent)? {
        return Err(
            "output must not share the source database's physical directory; use a dedicated public export directory"
                .into(),
        );
    }
    validate_output_parent(output_parent)?;

    let reserved_paths = [
        source.clone(),
        sqlite_sidecar_path(&source, "-wal"),
        sqlite_sidecar_path(&source, "-shm"),
        sqlite_sidecar_path(&source, "-journal"),
    ];

    if reserved_paths.iter().any(|reserved| reserved == &output) {
        return Err(
            "output must not be the source database or one of its SQLite sidecar paths".into(),
        );
    }
    match std::fs::symlink_metadata(&output) {
        Ok(_) => {
            return Err(format!("output already exists at {}", output.display()).into());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    Ok(ResolvedExportPaths { source, output })
}

#[cfg(not(target_arch = "wasm32"))]
fn validate_output_parent(parent: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let metadata = parent.metadata()?;
    if !metadata.is_dir() {
        return Err("output parent must be a directory".into());
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        if metadata.mode() & 0o022 != 0 {
            return Err("output parent must not be group- or world-writable".into());
        }
        let owner_probe = tempfile::Builder::new()
            .prefix(".agent-mail-output-owner-check-")
            .tempfile_in(parent)?;
        if owner_probe.as_file().metadata()?.uid() != metadata.uid() {
            return Err("output parent must be owned by the exporting user".into());
        }
    }

    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
fn validate_export_source_revision(revision: &str) -> Result<(), Box<dyn std::error::Error>> {
    if revision.len() == 40
        && revision
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Ok(());
    }
    Err("source revision must be exactly 40 lowercase hexadecimal characters".into())
}

#[cfg(not(target_arch = "wasm32"))]
fn validate_export_captured_at(captured_at: &str) -> Result<(), Box<dyn std::error::Error>> {
    chrono::DateTime::parse_from_rfc3339(captured_at)
        .map(|_| ())
        .map_err(|_| "captured-at must be a valid RFC 3339 timestamp".into())
}

#[cfg(not(target_arch = "wasm32"))]
fn publish_output_with<F>(path: &Path, write_staging: F) -> Result<(), Box<dyn std::error::Error>>
where
    F: FnOnce(&mut std::fs::File) -> std::io::Result<()>,
{
    let parent = output_parent(path);
    let mut staging = tempfile::Builder::new()
        .prefix(".agent-mail-dashboard-export-")
        .tempfile_in(parent)?;
    write_staging(staging.as_file_mut())?;
    staging.as_file_mut().sync_all()?;
    let published = staging
        .persist_noclobber(path)
        .map_err(|error| error.error)?;
    published.sync_all().map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!(
                "output was published at {}, but durability confirmation failed while syncing the file: {error}",
                path.display()
            ),
        )
    })?;

    #[cfg(unix)]
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!(
                    "output was published at {}, but durability confirmation failed while syncing its parent directory: {error}",
                    path.display()
                ),
            )
        })?;

    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
fn publish_output_noclobber(path: &Path, json: &str) -> Result<(), Box<dyn std::error::Error>> {
    publish_output_with(path, |staging| {
        staging.write_all(json.as_bytes())?;
        staging.write_all(b"\n")
    })
}

#[cfg(not(target_arch = "wasm32"))]
fn run_with<R, H>(
    args: Args,
    read_counts: R,
    before_publish: H,
) -> Result<(), Box<dyn std::error::Error>>
where
    R: FnOnce(&Path) -> Result<AggregateCounts, Box<dyn std::error::Error>>,
    H: FnOnce(&Path) -> Result<(), Box<dyn std::error::Error>>,
{
    validate_export_source_revision(&args.source_revision)?;
    validate_export_captured_at(&args.captured_at)?;
    // Resolve and validate the exact publication leaf before touching the
    // source. Never return to the caller-provided path: its CWD or a symlinked
    // parent may change while the private snapshot is being aggregated.
    let paths = validate_output_path(&args.source, &args.output)?;

    // Read-only, fail-closed open plus a single-snapshot aggregate read: the
    // exporter never holds write capability on the private source, and all
    // six published counts describe one database state (br-h44pp).
    let counts = read_counts(&paths.source)?;

    let mut pack = curated_public_demo();
    pack.provenance.source_label =
        "aggregate counts exported from Agent Mail SQLite; all details synthetic".to_string();
    pack.provenance.source_revision = args.source_revision;
    pack.provenance.captured_at = args.captured_at;
    apply_aggregates(&mut pack.bootstrap.db_stats, counts, 0, 0, 0);

    for action in &mut pack.actions {
        match &mut action.operation {
            DemoOperation::SetDbStats { snapshot } | DemoOperation::MergeDbStats { snapshot } => {
                let message_delta = u64::from(action.at_ms >= 2_000);
                let ack_delta = if action.at_ms < 2_000 {
                    0
                } else if action.at_ms < 6_000 {
                    1
                } else {
                    0
                };
                let reservation_delta = if action.at_ms >= 10_000 { -1 } else { 0 };
                apply_aggregates(
                    snapshot,
                    counts,
                    message_delta,
                    ack_delta,
                    reservation_delta,
                );
            }
            DemoOperation::PublishEvent {
                event: MailEvent::HealthPulse { db_stats, .. },
            } => apply_aggregates(db_stats, counts, 1, 0, -1),
            _ => {}
        }
    }

    pack.finalize_digest();
    pack.validate()?;
    let json = pack.to_pretty_json()?;

    before_publish(&paths.output)?;
    publish_output_noclobber(&paths.output, &json)?;
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    run_with(
        args,
        |source| {
            let connection = open_source_read_only(source)?;
            read_aggregates_snapshot(&connection)
        },
        |_| Ok(()),
    )
}

#[cfg(not(target_arch = "wasm32"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    run(Args::parse())
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use std::io::Write as _;
    use std::path::PathBuf;

    use super::{
        Args, publish_output_noclobber, publish_output_with, run, run_with, sqlite_sidecar_path,
        validate_export_captured_at, validate_export_source_revision, validate_output_path,
    };

    fn representative_counts() -> super::AggregateCounts {
        super::AggregateCounts {
            projects: 44,
            agents: 1_550,
            messages: 7_916,
            file_reservations: 12,
            contact_links: 24,
            ack_pending: 3,
        }
    }

    fn create_safe_output_directory(path: &std::path::Path) {
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;

            builder.mode(0o700);
        }
        builder.create(path).expect("create safe output directory");
    }

    fn collision_args(source: PathBuf, output: PathBuf) -> Args {
        Args {
            source,
            output,
            source_revision: "0123456789abcdef0123456789abcdef01234567".to_string(),
            captured_at: "2026-08-02T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn exporter_revision_requires_a_full_lowercase_git_sha() {
        let valid = "0123456789abcdef0123456789abcdef01234567";
        validate_export_source_revision(valid).expect("valid Git revision");

        for invalid in [
            "public-demo-v1",
            "0123456789abcdef0123456789abcdef0123456",
            "0123456789abcdef0123456789abcdef012345678",
            "0123456789abcdef0123456789abcdef0123456g",
            "0123456789abcdef0123456789abcdef0123456A",
        ] {
            assert!(
                validate_export_source_revision(invalid).is_err(),
                "revision {invalid:?} must be rejected"
            );
        }
    }

    #[test]
    fn exporter_rejects_invalid_capture_time_before_reading_the_database() {
        let directory = tempfile::tempdir().expect("tempdir");
        let source_directory = directory.path().join("private");
        let output_directory = directory.path().join("public");
        std::fs::create_dir(&source_directory).expect("create source directory");
        create_safe_output_directory(&output_directory);
        let mut args = collision_args(
            source_directory.join("mailbox.sqlite3"),
            output_directory.join("demo-pack.json"),
        );
        args.captured_at = "not-a-timestamp".to_string();

        let error = run_with(
            args,
            |_| panic!("invalid metadata must fail before the database reader runs"),
            |_| panic!("invalid metadata must fail before publication"),
        )
        .expect_err("invalid timestamp");
        assert!(error.to_string().contains("RFC 3339"));
        assert!(validate_export_captured_at("2026-08-02T00:00:00Z").is_ok());
    }

    #[test]
    fn output_must_not_collide_with_the_database_or_absent_sidecars() {
        let directory = tempfile::tempdir().expect("tempdir");
        let source = directory.path().join("mailbox.sqlite3");
        let original_database = b"private database sentinel";
        std::fs::write(&source, original_database).expect("write source sentinel");

        assert!(run(collision_args(source.clone(), source.clone())).is_err());
        assert_eq!(
            std::fs::read(&source).expect("read source sentinel"),
            original_database
        );

        for suffix in ["-wal", "-shm", "-journal"] {
            let sidecar = sqlite_sidecar_path(&source, suffix);
            assert!(!sidecar.exists(), "test sidecar must begin absent");
            assert!(
                run(collision_args(source.clone(), sidecar.clone())).is_err(),
                "output collision with {suffix} must be rejected"
            );
            assert!(
                !sidecar.exists(),
                "rejecting output collision with {suffix} must not create the sidecar"
            );
        }
    }

    #[test]
    fn output_validation_preserves_existing_sqlite_sidecars() {
        let directory = tempfile::tempdir().expect("tempdir");
        let source = directory.path().join("mailbox.sqlite3");
        std::fs::write(&source, b"database").expect("write source");

        for suffix in ["-wal", "-shm", "-journal"] {
            let sidecar = sqlite_sidecar_path(&source, suffix);
            let sentinel = format!("private {suffix} sentinel");
            std::fs::write(&sidecar, sentinel.as_bytes()).expect("write sidecar sentinel");
            assert!(run(collision_args(source.clone(), sidecar.clone())).is_err());
            assert_eq!(
                std::fs::read(&sidecar).expect("read preserved sidecar"),
                sentinel.as_bytes()
            );
        }

        let output_directory = directory.path().join("public");
        create_safe_output_directory(&output_directory);
        let output = output_directory.join("public-demo.json");
        let expected_output = output_directory
            .canonicalize()
            .expect("canonical output directory")
            .join("public-demo.json");
        assert_eq!(
            validate_output_path(&source, &output)
                .expect("unrelated output path")
                .output,
            expected_output
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolved_paths_ignore_parent_symlink_retargets() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("tempdir");
        let source_directory = directory.path().join("private");
        let safe_output_directory = directory.path().join("public");
        std::fs::create_dir(&source_directory).expect("create source directory");
        create_safe_output_directory(&safe_output_directory);
        let source = source_directory.join("mailbox.sqlite3");
        std::fs::write(&source, b"private database sentinel").expect("write source sentinel");

        let source_parent_link = directory.path().join("current-source");
        let parked_source_link = directory.path().join("original-source-link");
        let output_parent_link = directory.path().join("current-output");
        let parked_output_link = directory.path().join("original-output-link");
        symlink(&source_directory, &source_parent_link).expect("create source parent symlink");
        symlink(&safe_output_directory, &output_parent_link).expect("create output parent symlink");
        let requested_source = source_parent_link.join("mailbox.sqlite3");
        let requested_output = output_parent_link.join("mailbox.sqlite3-wal");
        let safe_output = safe_output_directory.join("mailbox.sqlite3-wal");
        let expected_resolved_source = source_directory
            .canonicalize()
            .expect("canonical source directory")
            .join("mailbox.sqlite3");
        let resolved_safe_output = safe_output_directory
            .canonicalize()
            .expect("canonical output directory")
            .join("mailbox.sqlite3-wal");
        let private_sidecar = sqlite_sidecar_path(&source, "-wal");

        run_with(
            collision_args(requested_source, requested_output),
            |resolved_source| {
                assert_eq!(resolved_source, expected_resolved_source);
                std::fs::rename(&source_parent_link, &parked_source_link)?;
                symlink(&safe_output_directory, &source_parent_link)?;
                Ok(representative_counts())
            },
            |resolved_output| {
                assert_eq!(resolved_output, resolved_safe_output);
                std::fs::rename(&output_parent_link, &parked_output_link)?;
                symlink(&source_directory, &output_parent_link)?;
                Ok(())
            },
        )
        .expect("publish to the prevalidated physical destination");

        assert!(safe_output.is_file());
        assert!(
            !private_sidecar.exists(),
            "retargeting the caller's parent symlink must not publish a SQLite sidecar"
        );
        assert_eq!(
            std::fs::read(&source).expect("read preserved source"),
            b"private database sentinel"
        );
    }

    #[test]
    fn existing_output_fails_before_database_aggregation() {
        let directory = tempfile::tempdir().expect("tempdir");
        let source_directory = directory.path().join("private");
        let output_directory = directory.path().join("public");
        std::fs::create_dir(&source_directory).expect("create source directory");
        create_safe_output_directory(&output_directory);
        let output = output_directory.join("demo-pack.json");
        std::fs::write(&output, b"existing public output").expect("write existing output");

        let error = run_with(
            collision_args(source_directory.join("mailbox.sqlite3"), output.clone()),
            |_| panic!("an occupied output must fail before database aggregation"),
            |_| panic!("an occupied output must fail before publication"),
        )
        .expect_err("occupied output");
        assert!(error.to_string().contains("already exists"));
        assert_eq!(
            std::fs::read(output).expect("read existing output"),
            b"existing public output"
        );
    }

    #[cfg(unix)]
    #[test]
    fn output_parent_must_not_be_group_or_world_writable() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("tempdir");
        let source_directory = directory.path().join("private");
        let output_directory = directory.path().join("public");
        std::fs::create_dir(&source_directory).expect("create source directory");
        std::fs::create_dir(&output_directory).expect("create output directory");
        std::fs::set_permissions(&output_directory, std::fs::Permissions::from_mode(0o777))
            .expect("make output directory unsafe");

        let error = validate_output_path(
            &source_directory.join("mailbox.sqlite3"),
            &output_directory.join("demo-pack.json"),
        )
        .expect_err("unsafe output parent");
        assert!(error.to_string().contains("group- or world-writable"));
    }

    #[test]
    fn failed_staging_write_never_publishes_the_final_path() {
        let directory = tempfile::tempdir().expect("tempdir");
        let output = directory.path().join("demo-pack.json");

        let result = publish_output_with(&output, |staging| {
            staging.write_all(b"partial")?;
            Err(std::io::Error::other("injected staging failure"))
        });

        assert!(result.is_err());
        assert!(
            !output.exists(),
            "a staging failure must leave the promised final path absent"
        );
    }

    #[test]
    fn staged_output_publication_never_clobbers_existing_path() {
        let directory = tempfile::tempdir().expect("tempdir");
        let output = directory.path().join("demo-pack.json");

        publish_output_noclobber(&output, "first").expect("publish first output");
        assert_eq!(
            std::fs::read_to_string(&output).expect("read output"),
            "first\n"
        );

        assert!(
            publish_output_noclobber(&output, "second").is_err(),
            "an existing output must never be overwritten"
        );
        assert_eq!(
            std::fs::read_to_string(&output).expect("read preserved output"),
            "first\n"
        );
    }
}
