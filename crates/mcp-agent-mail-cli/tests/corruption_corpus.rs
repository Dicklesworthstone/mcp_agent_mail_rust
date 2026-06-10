#![forbid(unsafe_code)]

use serde::Deserialize;
use sqlmodel_sqlite::SqliteConnection;
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

const MANIFEST_JSON: &str = include_str!("../../../tests/fixtures/corruption_corpus/manifest.json");
const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";

#[derive(Debug, Deserialize)]
struct Manifest {
    schema_version: u32,
    corpus_id: String,
    generated_by: String,
    fixtures: Vec<Fixture>,
}

#[derive(Debug, Deserialize)]
struct Fixture {
    id: String,
    title: String,
    incident_anchor: String,
    track_a_classification: String,
    canonical_sqlite_verdict: String,
    frankensqlite_expected_signal: String,
    tags: Vec<String>,
    artifacts: Vec<Artifact>,
}

#[derive(Debug, Deserialize)]
struct Artifact {
    path: String,
    kind: String,
    recipe: Recipe,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Recipe {
    SqliteSql {
        file: String,
    },
    EmptyFile,
    DeterministicBytes {
        len: usize,
        fill: u8,
        sqlite_header: bool,
    },
    TextFile {
        file: String,
    },
    Symlink {
        target: String,
    },
    SqliteHeaderWithFreelist {
        len: usize,
        first_freelist_trunk_page: u32,
        freelist_page_count: u32,
    },
    SqliteHeaderWithPageCount {
        len: usize,
        page_size: u16,
        page_count: u32,
    },
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR should be crates/mcp-agent-mail-cli")
        .to_path_buf()
}

fn corpus_dir() -> PathBuf {
    repo_root().join("tests/fixtures/corruption_corpus")
}

fn manifest() -> Manifest {
    serde_json::from_str(MANIFEST_JSON).expect("corruption corpus manifest must parse")
}

fn assert_relative_fixture_path(path: &str) {
    let path = Path::new(path);
    assert!(
        !path.as_os_str().is_empty(),
        "corruption fixture path must not be empty"
    );
    assert!(
        !path.is_absolute(),
        "corruption fixture path must be relative: {}",
        path.display()
    );
    assert!(
        path.components().all(|component| {
            !matches!(
                component,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        }),
        "corruption fixture path must not contain absolute, prefix, or `..` components: {}",
        path.display()
    );
}

fn assert_artifact_path_matches_fixture(fixture_id: &str, artifact_path: &str) {
    assert_relative_fixture_path(fixture_id);
    assert_relative_fixture_path(artifact_path);
    assert!(
        Path::new(artifact_path).starts_with(Path::new(fixture_id)),
        "artifact path `{artifact_path}` must live under fixture id `{fixture_id}`"
    );
}

fn assert_relative_symlink_target(path: &str) {
    assert_relative_fixture_path(path);
    assert!(
        !path.trim().is_empty(),
        "corruption fixture symlink target must not be empty"
    );
}

fn recipe_text(relative: &str) -> String {
    assert_relative_fixture_path(relative);
    let path = corpus_dir().join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("read recipe {}: {err}", path.display()))
}

fn write_sqlite_from_recipe(target: &Path, recipe_file: &str) {
    let sql = recipe_text(recipe_file);
    let conn = SqliteConnection::open_file(target.to_string_lossy().into_owned())
        .unwrap_or_else(|err| panic!("open sqlite fixture {}: {err}", target.display()));
    conn.execute_raw(&sql).unwrap_or_else(|err| {
        panic!(
            "execute recipe {recipe_file} into {}: {err}",
            target.display()
        )
    });
}

fn write_deterministic_bytes(target: &Path, len: usize, fill: u8, sqlite_header: bool) {
    let mut bytes = vec![fill; len];
    if sqlite_header {
        assert!(
            bytes.len() >= SQLITE_HEADER.len(),
            "sqlite_header byte fixture must be at least {} bytes",
            SQLITE_HEADER.len()
        );
        bytes[..SQLITE_HEADER.len()].copy_from_slice(SQLITE_HEADER);
    }
    std::fs::write(target, bytes)
        .unwrap_or_else(|err| panic!("write byte fixture {}: {err}", target.display()));
}

fn write_sqlite_header_with_freelist(
    target: &Path,
    len: usize,
    first_freelist_trunk_page: u32,
    freelist_page_count: u32,
) {
    let mut bytes = vec![0_u8; len];
    assert!(
        bytes.len() >= 100,
        "sqlite header fixture must have room for the 100-byte header"
    );
    bytes[..SQLITE_HEADER.len()].copy_from_slice(SQLITE_HEADER);
    bytes[16..18].copy_from_slice(&4096_u16.to_be_bytes());
    bytes[32..36].copy_from_slice(&first_freelist_trunk_page.to_be_bytes());
    bytes[36..40].copy_from_slice(&freelist_page_count.to_be_bytes());
    bytes[96..100].copy_from_slice(&1_u32.to_be_bytes());
    std::fs::write(target, bytes)
        .unwrap_or_else(|err| panic!("write freelist fixture {}: {err}", target.display()));
}

fn write_sqlite_header_with_page_count(target: &Path, len: usize, page_size: u16, page_count: u32) {
    let mut bytes = vec![0_u8; len];
    assert!(
        bytes.len() >= 100,
        "sqlite page-count fixture must have room for the 100-byte header"
    );
    bytes[..SQLITE_HEADER.len()].copy_from_slice(SQLITE_HEADER);
    bytes[16..18].copy_from_slice(&page_size.to_be_bytes());
    bytes[28..32].copy_from_slice(&page_count.to_be_bytes());
    bytes[96..100].copy_from_slice(&1_u32.to_be_bytes());
    std::fs::write(target, bytes)
        .unwrap_or_else(|err| panic!("write short-read fixture {}: {err}", target.display()));
}

fn materialize_fixture(fixture: &Fixture, root: &Path) -> Vec<PathBuf> {
    let mut written = Vec::new();
    for artifact in &fixture.artifacts {
        assert_artifact_path_matches_fixture(&fixture.id, &artifact.path);
        let target = root.join(&artifact.path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .unwrap_or_else(|err| panic!("create fixture dir {}: {err}", parent.display()));
        }
        match &artifact.recipe {
            Recipe::SqliteSql { file } => write_sqlite_from_recipe(&target, file),
            Recipe::EmptyFile => std::fs::write(&target, b"")
                .unwrap_or_else(|err| panic!("write empty fixture {}: {err}", target.display())),
            Recipe::DeterministicBytes {
                len,
                fill,
                sqlite_header,
            } => write_deterministic_bytes(&target, *len, *fill, *sqlite_header),
            Recipe::TextFile { file } => {
                let contents = recipe_text(file);
                std::fs::write(&target, contents)
                    .unwrap_or_else(|err| panic!("write text fixture {}: {err}", target.display()));
            }
            Recipe::Symlink {
                target: link_target,
            } => {
                assert_relative_symlink_target(link_target);
                write_symlink_fixture(&target, link_target);
            }
            Recipe::SqliteHeaderWithFreelist {
                len,
                first_freelist_trunk_page,
                freelist_page_count,
            } => write_sqlite_header_with_freelist(
                &target,
                *len,
                *first_freelist_trunk_page,
                *freelist_page_count,
            ),
            Recipe::SqliteHeaderWithPageCount {
                len,
                page_size,
                page_count,
            } => write_sqlite_header_with_page_count(&target, *len, *page_size, *page_count),
        }
        written.push(target);
    }
    written
}

#[cfg(unix)]
fn write_symlink_fixture(path: &Path, target: &str) {
    std::os::unix::fs::symlink(target, path)
        .unwrap_or_else(|err| panic!("create symlink fixture {}: {err}", path.display()));
}

#[cfg(not(unix))]
fn write_symlink_fixture(path: &Path, target: &str) {
    std::fs::write(path, format!("symlink target: {target}\n"))
        .unwrap_or_else(|err| panic!("write symlink placeholder {}: {err}", path.display()));
}

#[test]
fn manifest_covers_required_corruption_incidents() {
    let manifest = manifest();
    assert_eq!(manifest.schema_version, 1);
    assert_eq!(manifest.corpus_id, "agent-mail-corrupted-db-corpus");
    assert_eq!(manifest.generated_by, "br-bvq1x.12.1");

    let ids = manifest
        .fixtures
        .iter()
        .map(|fixture| fixture.id.as_str())
        .collect::<BTreeSet<_>>();
    let required = [
        "fk_false_positive_valid_db",
        "zero_byte_wal",
        "short_corrupt_wal_header",
        "btree_page_type_zero",
        "freelist_leaf_exceeds_db_size",
        "short_read_fetching_page",
        "recover_text_recipient_id",
        "missing_messages_recipients_json",
        "missing_required_tables",
        "legacy_text_timestamps",
        "sqlite_sidecar_symlink",
        "analyze_reindex_busy_or_corrupt",
    ];
    for required_id in required {
        assert!(
            ids.contains(required_id),
            "corruption corpus missing required fixture `{required_id}`"
        );
    }
}

#[test]
fn manifest_entries_are_documented_and_path_safe() {
    let manifest = manifest();
    let mut ids = BTreeSet::new();
    for fixture in &manifest.fixtures {
        assert_relative_fixture_path(&fixture.id);
        assert!(
            ids.insert(fixture.id.as_str()),
            "duplicate fixture id {}",
            fixture.id
        );
        assert!(
            !fixture.title.trim().is_empty(),
            "{} missing title",
            fixture.id
        );
        assert!(
            !fixture.incident_anchor.trim().is_empty(),
            "{} missing incident anchor",
            fixture.id
        );
        assert!(
            !fixture.track_a_classification.trim().is_empty(),
            "{} missing Track A classification",
            fixture.id
        );
        assert!(
            !fixture.canonical_sqlite_verdict.trim().is_empty(),
            "{} missing canonical SQLite verdict",
            fixture.id
        );
        assert!(
            !fixture.frankensqlite_expected_signal.trim().is_empty(),
            "{} missing frankensqlite signal",
            fixture.id
        );
        assert!(!fixture.tags.is_empty(), "{} missing tags", fixture.id);
        assert!(
            !fixture.artifacts.is_empty(),
            "{} missing artifacts",
            fixture.id
        );
        for artifact in &fixture.artifacts {
            assert_artifact_path_matches_fixture(&fixture.id, &artifact.path);
            assert!(
                !artifact.kind.trim().is_empty(),
                "{} has blank artifact kind",
                fixture.id
            );
            if let Recipe::Symlink { target } = &artifact.recipe {
                assert_relative_symlink_target(target);
            }
        }
    }
}

#[test]
fn corpus_materializes_every_fixture_in_isolated_tempdir() {
    let manifest = manifest();
    let temp = tempfile::TempDir::new().expect("tempdir");

    for fixture in &manifest.fixtures {
        let paths = materialize_fixture(fixture, temp.path());
        assert_eq!(
            paths.len(),
            fixture.artifacts.len(),
            "{} did not materialize every artifact",
            fixture.id
        );
        for path in paths {
            let meta = std::fs::symlink_metadata(&path).unwrap_or_else(|err| {
                panic!("stat materialized fixture {}: {err}", path.display())
            });
            assert!(
                meta.file_type().is_file() || meta.file_type().is_symlink(),
                "materialized fixture must be file or symlink: {}",
                path.display()
            );
        }
    }

    let temp_string = temp.path().to_string_lossy();
    assert!(
        !temp_string.contains(".mcp_agent_mail_git_mailbox_repo"),
        "corruption corpus materialization must not use the live mailbox root"
    );
}
