#![forbid(unsafe_code)]

use mcp_agent_mail_db::{CanonicalDbConn, DbConn};
use serde::{Deserialize, Serialize};
use sqlmodel_core::{Row, Value};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

const MANIFEST_JSON: &str = include_str!("../../../tests/fixtures/corruption_corpus/manifest.json");
const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";

#[derive(Debug, Deserialize)]
struct Manifest {
    fixtures: Vec<Fixture>,
}

#[derive(Debug, Deserialize)]
struct Fixture {
    id: String,
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

#[derive(Debug, Clone, Copy)]
struct Probe {
    id: &'static str,
    sql: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ConformanceStatus {
    Conformant,
    Divergent,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ProbeOutcome {
    Ok { rows: Vec<Vec<String>> },
    Err { text: String },
}

#[derive(Debug, Serialize)]
struct ProbeReport {
    fixture_id: String,
    db_path: String,
    probe_id: &'static str,
    sql: &'static str,
    canonical: ProbeOutcome,
    frankensqlite: ProbeOutcome,
    status: ConformanceStatus,
}

#[derive(Debug, Serialize)]
struct ConformanceReport {
    schema_version: u32,
    generated_by: &'static str,
    probes: Vec<ProbeReport>,
    totals: BTreeMap<&'static str, usize>,
}

const PRAGMA_PROBES: &[Probe] = &[
    Probe {
        id: "quick_check",
        sql: "PRAGMA quick_check;",
    },
    Probe {
        id: "integrity_check",
        sql: "PRAGMA integrity_check;",
    },
    Probe {
        id: "foreign_key_check",
        sql: "PRAGMA foreign_key_check;",
    },
    Probe {
        id: "foreign_key_list_messages",
        sql: "PRAGMA foreign_key_list(messages);",
    },
    Probe {
        id: "foreign_key_list_child",
        sql: "PRAGMA foreign_key_list(child);",
    },
    Probe {
        id: "journal_mode_read",
        sql: "PRAGMA journal_mode;",
    },
    Probe {
        id: "wal_autocheckpoint_read",
        sql: "PRAGMA wal_autocheckpoint;",
    },
    Probe {
        id: "user_version_read",
        sql: "PRAGMA user_version;",
    },
    Probe {
        id: "schema_version_read",
        sql: "PRAGMA schema_version;",
    },
    Probe {
        id: "legacy_fts_metadata",
        sql: "SELECT name, type FROM sqlite_master \
              WHERE name LIKE 'fts_messages%' \
                 OR name LIKE 'fts_agents%' \
                 OR name LIKE 'fts_projects%' \
              ORDER BY name, type;",
    },
];

#[test]
fn frankensqlite_pragma_matrix_reports_known_divergences() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let dbs = materialize_probe_databases(temp.path());
    assert!(
        dbs.iter()
            .any(|db| db.fixture_id == "bootstrap_fresh_no_fk"),
        "M1 must keep its no-L1-cycle bootstrap fixture"
    );
    assert!(
        dbs.iter()
            .any(|db| db.fixture_id == "fk_false_positive_valid_db"),
        "M1 must include the L1 foreign-key false-positive fixture"
    );

    let report = run_conformance_matrix(&dbs);
    let report_json =
        serde_json::to_string_pretty(&report).expect("pragma conformance report serializes");
    let report_path = write_conformance_report(temp.path(), &report_json);
    assert_eq!(
        std::fs::read_to_string(&report_path).expect("read persisted conformance report"),
        report_json,
        "persisted conformance report must match the in-memory matrix"
    );
    eprintln!(
        "frankensqlite pragma conformance report: {}",
        report_path.display()
    );
    assert!(
        report_json.contains("\"schema_version\": 1"),
        "report must be machine-readable JSON: {report_json}"
    );
    assert_eq!(
        report.probes.len(),
        dbs.len() * PRAGMA_PROBES.len(),
        "every fixture/probe pair must be represented"
    );
    assert!(
        *report.totals.get("divergent").unwrap_or(&0) > 0,
        "matrix should expose current canonical/frankensqlite divergences"
    );

    let foreign_key_check_regressed_to_false_malformed = report.probes.iter().any(|entry| {
        entry.probe_id == "foreign_key_check"
            && matches!(
                entry.fixture_id.as_str(),
                "bootstrap_fresh_no_fk" | "fk_false_positive_valid_db"
            )
            && !matches!(entry.status, ConformanceStatus::Conformant)
    });
    assert!(
        !foreign_key_check_regressed_to_false_malformed,
        "foreign_key_check must not regress to the old false-malformed behavior; report={report_json}"
    );

    let known_fk_list_ordering_divergences = report
        .probes
        .iter()
        .filter(|entry| {
            entry.probe_id == "foreign_key_list_messages"
                && matches!(entry.status, ConformanceStatus::Divergent)
                && is_fk_list_ordering_divergence(entry)
        })
        .count();
    assert!(
        known_fk_list_ordering_divergences > 0,
        "current frankensqlite should expose PRAGMA foreign_key_list ordering/id divergence until upstream fixes it; report={report_json}"
    );
}

fn write_conformance_report(root: &Path, report_json: &str) -> PathBuf {
    let report_dir = std::env::var_os("CARGO_TARGET_TMPDIR")
        .or_else(|| std::env::var_os("CARGO_TARGET_DIR"))
        .map_or_else(|| root.to_path_buf(), PathBuf::from);
    std::fs::create_dir_all(&report_dir)
        .unwrap_or_else(|err| panic!("create report dir {}: {err}", report_dir.display()));
    let report_path = report_dir.join("frankensqlite_pragma_conformance_report.json");
    std::fs::write(&report_path, report_json)
        .unwrap_or_else(|err| panic!("write report {}: {err}", report_path.display()));
    report_path
}

fn is_fk_list_ordering_divergence(entry: &ProbeReport) -> bool {
    let (
        ProbeOutcome::Ok {
            rows: canonical_rows,
        },
        ProbeOutcome::Ok {
            rows: frankensqlite_rows,
        },
    ) = (&entry.canonical, &entry.frankensqlite)
    else {
        return false;
    };
    if canonical_rows == frankensqlite_rows {
        return false;
    }
    let mut canonical_sorted = canonical_rows
        .iter()
        .map(|row| fk_list_row_without_order_fields(row))
        .collect::<Vec<_>>();
    let mut frankensqlite_sorted = frankensqlite_rows
        .iter()
        .map(|row| fk_list_row_without_order_fields(row))
        .collect::<Vec<_>>();
    canonical_sorted.sort();
    frankensqlite_sorted.sort();
    canonical_sorted == frankensqlite_sorted
}

fn fk_list_row_without_order_fields(row: &[String]) -> Vec<String> {
    row.iter()
        .filter(|cell| !cell.starts_with("id=") && !cell.starts_with("seq="))
        .cloned()
        .collect()
}

#[derive(Debug)]
struct ProbeDatabase {
    fixture_id: String,
    path: PathBuf,
}

fn run_conformance_matrix(dbs: &[ProbeDatabase]) -> ConformanceReport {
    let mut probes = Vec::new();
    for db in dbs {
        for probe in PRAGMA_PROBES {
            let canonical = run_canonical_probe(&db.path, probe.sql);
            let frankensqlite = run_frankensqlite_probe(&db.path, probe.sql);
            let status = classify_status(&canonical, &frankensqlite);
            probes.push(ProbeReport {
                fixture_id: db.fixture_id.clone(),
                db_path: redact_path(&db.path),
                probe_id: probe.id,
                sql: probe.sql,
                canonical,
                frankensqlite,
                status,
            });
        }
    }

    let mut totals = BTreeMap::new();
    for entry in &probes {
        let key = match entry.status {
            ConformanceStatus::Conformant => "conformant",
            ConformanceStatus::Divergent => "divergent",
            ConformanceStatus::Unsupported => "unsupported",
        };
        *totals.entry(key).or_insert(0) += 1;
    }

    ConformanceReport {
        schema_version: 1,
        generated_by: "br-bvq1x.13.1",
        probes,
        totals,
    }
}

fn classify_status(canonical: &ProbeOutcome, frankensqlite: &ProbeOutcome) -> ConformanceStatus {
    if is_unsupported(frankensqlite) {
        ConformanceStatus::Unsupported
    } else if canonical == frankensqlite {
        ConformanceStatus::Conformant
    } else {
        ConformanceStatus::Divergent
    }
}

fn is_unsupported(outcome: &ProbeOutcome) -> bool {
    let ProbeOutcome::Err { text } = outcome else {
        return false;
    };
    let lower = text.to_ascii_lowercase();
    lower.contains("unsupported")
        || lower.contains("not implemented")
        || lower.contains("no such module")
}

fn run_canonical_probe(path: &Path, sql: &str) -> ProbeOutcome {
    match CanonicalDbConn::open_file(path.to_string_lossy().into_owned()) {
        Ok(conn) => normalize_rows(conn.query_sync(sql, &[]), path),
        Err(err) => ProbeOutcome::Err {
            text: normalize_error(&err, path),
        },
    }
}

fn run_frankensqlite_probe(path: &Path, sql: &str) -> ProbeOutcome {
    match DbConn::open_file(path.to_string_lossy().into_owned()) {
        Ok(conn) => normalize_rows(conn.query_sync(sql, &[]), path),
        Err(err) => ProbeOutcome::Err {
            text: normalize_error(&err, path),
        },
    }
}

fn normalize_rows(result: Result<Vec<Row>, sqlmodel_core::Error>, db_path: &Path) -> ProbeOutcome {
    match result {
        Ok(rows) => ProbeOutcome::Ok {
            rows: rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|(column, value)| format!("{column}={}", normalize_value(value)))
                        .collect()
                })
                .collect(),
        },
        Err(err) => ProbeOutcome::Err {
            text: normalize_error(&err, db_path),
        },
    }
}

fn normalize_value(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Bool(value) => value.to_string(),
        Value::TinyInt(value) => i64::from(*value).to_string(),
        Value::SmallInt(value) => i64::from(*value).to_string(),
        Value::Int(value) => i64::from(*value).to_string(),
        Value::BigInt(value) => value.to_string(),
        Value::Float(value) => f64::from(*value).to_string(),
        Value::Double(value) => value.to_string(),
        Value::Decimal(value) | Value::Text(value) => value.clone(),
        Value::Bytes(value) => format!("0x{}", hex::encode(value)),
        Value::Date(value) => value.to_string(),
        Value::Time(value) | Value::Timestamp(value) | Value::TimestampTz(value) => {
            value.to_string()
        }
        Value::Uuid(value) => format!("uuid:{}", hex::encode(value)),
        Value::Json(value) => value.to_string(),
        Value::Array(values) => values
            .iter()
            .map(normalize_value)
            .collect::<Vec<_>>()
            .join(","),
        Value::Default => "DEFAULT".to_string(),
    }
}

fn normalize_error(err: &sqlmodel_core::Error, db_path: &Path) -> String {
    let mut text = err.to_string();
    if let Some(path) = db_path.to_str() {
        text = text.replace(path, "$DB");
    }
    text
}

fn redact_path(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("storage.sqlite3")
        .to_string()
}

fn materialize_probe_databases(root: &Path) -> Vec<ProbeDatabase> {
    let mut dbs = Vec::new();
    dbs.push(materialize_bootstrap_no_fk(root));

    let manifest: Manifest =
        serde_json::from_str(MANIFEST_JSON).expect("corruption corpus manifest parses");
    for fixture in manifest.fixtures {
        for artifact in &fixture.artifacts {
            assert_artifact_path_matches_fixture(&fixture.id, &artifact.path);
            let target = root.join(&artifact.path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .unwrap_or_else(|err| panic!("create {}: {err}", parent.display()));
            }
            materialize_artifact(artifact, &target);
        }
        for artifact in fixture
            .artifacts
            .iter()
            .filter(|artifact| artifact.kind.starts_with("sqlite_db"))
        {
            dbs.push(ProbeDatabase {
                fixture_id: fixture.id.clone(),
                path: root.join(&artifact.path),
            });
        }
    }
    dbs
}

fn materialize_bootstrap_no_fk(root: &Path) -> ProbeDatabase {
    let path = root.join("bootstrap_fresh_no_fk").join("storage.sqlite3");
    std::fs::create_dir_all(path.parent().expect("bootstrap db parent"))
        .expect("create bootstrap db parent");
    let conn = CanonicalDbConn::open_file(path.to_string_lossy().into_owned())
        .expect("open bootstrap canonical db");
    conn.execute_raw(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE fresh_mailbox_probe (
             id INTEGER PRIMARY KEY,
             body TEXT NOT NULL
         );
         INSERT INTO fresh_mailbox_probe (id, body) VALUES (1, 'no-fk bootstrap');",
    )
    .expect("seed no-fk bootstrap db");
    ProbeDatabase {
        fixture_id: "bootstrap_fresh_no_fk".to_string(),
        path,
    }
}

fn materialize_artifact(artifact: &Artifact, target: &Path) {
    match &artifact.recipe {
        Recipe::SqliteSql { file } => write_sqlite_from_recipe(target, file),
        Recipe::EmptyFile => std::fs::write(target, b"")
            .unwrap_or_else(|err| panic!("write empty fixture {}: {err}", target.display())),
        Recipe::DeterministicBytes {
            len,
            fill,
            sqlite_header,
        } => write_deterministic_bytes(target, *len, *fill, *sqlite_header),
        Recipe::TextFile { file } => {
            let contents = recipe_text(file);
            std::fs::write(target, contents)
                .unwrap_or_else(|err| panic!("write text fixture {}: {err}", target.display()));
        }
        Recipe::Symlink {
            target: link_target,
        } => {
            assert_relative_symlink_target(link_target);
            write_symlink_fixture(target, link_target);
        }
        Recipe::SqliteHeaderWithFreelist {
            len,
            first_freelist_trunk_page,
            freelist_page_count,
        } => write_sqlite_header_with_freelist(
            target,
            *len,
            *first_freelist_trunk_page,
            *freelist_page_count,
        ),
        Recipe::SqliteHeaderWithPageCount {
            len,
            page_size,
            page_count,
        } => write_sqlite_header_with_page_count(target, *len, *page_size, *page_count),
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR should be crates/mcp-agent-mail-db")
        .to_path_buf()
}

fn corpus_dir() -> PathBuf {
    repo_root().join("tests/fixtures/corruption_corpus")
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
    let conn = CanonicalDbConn::open_file(target.to_string_lossy().into_owned())
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
        .unwrap_or_else(|err| panic!("write page-count fixture {}: {err}", target.display()));
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
