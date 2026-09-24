#![forbid(unsafe_code)]

use mcp_agent_mail_db::DbConn;
use mcp_agent_mail_tools::reservation_parity::check_reservation_parity_with_db_conn;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

const MANIFEST_JSON: &str =
    include_str!("../../../tests/fixtures/reservation_regression/manifest.json");
const CORRUPTION_MANIFEST_JSON: &str =
    include_str!("../../../tests/fixtures/corruption_corpus/manifest.json");

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
    drift_mode: String,
    expected_detector: String,
    consumers: Vec<String>,
    expected_mismatches: Vec<String>,
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
    TextFile { file: String },
    CorruptionCorpusFixture { corpus: String, fixture_id: String },
}

#[derive(Debug, Deserialize)]
struct CorruptionManifest {
    fixtures: Vec<CorruptionFixture>,
}

#[derive(Debug, Deserialize)]
struct CorruptionFixture {
    id: String,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR should be crates/mcp-agent-mail-tools")
        .to_path_buf()
}

fn corpus_dir() -> PathBuf {
    repo_root().join("tests/fixtures/reservation_regression")
}

fn manifest() -> Manifest {
    serde_json::from_str(MANIFEST_JSON).expect("reservation regression manifest must parse")
}

fn assert_relative_path(path: &str) {
    let path = Path::new(path);
    assert!(
        !path.as_os_str().is_empty(),
        "fixture path must not be empty"
    );
    assert!(
        !path.is_absolute(),
        "fixture path must be relative: {}",
        path.display()
    );
    assert!(
        path.components().all(|component| {
            !matches!(
                component,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        }),
        "fixture path must not contain absolute, prefix, or `..` components: {}",
        path.display()
    );
}

fn assert_artifact_path_matches_fixture(fixture_id: &str, artifact_path: &str) {
    assert_relative_path(fixture_id);
    assert_relative_path(artifact_path);
    assert!(
        Path::new(artifact_path).starts_with(Path::new(fixture_id)),
        "artifact path `{artifact_path}` must live under fixture id `{fixture_id}`"
    );
}

fn recipe_text(relative: &str) -> String {
    assert_relative_path(relative);
    let path = corpus_dir().join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("read recipe {}: {err}", path.display()))
}

fn artifact_recipe_file<'a>(fixture: &'a Fixture, kind: &str) -> &'a str {
    fixture
        .artifacts
        .iter()
        .find_map(|artifact| match (&artifact.kind[..], &artifact.recipe) {
            (artifact_kind, Recipe::TextFile { file }) if artifact_kind == kind => {
                Some(file.as_str())
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("{} missing {kind} text artifact", fixture.id))
}

fn materialize_sql_fixture(fixture: &Fixture, sql: &str) -> (tempfile::TempDir, DbConn) {
    let tempdir = tempfile::tempdir()
        .unwrap_or_else(|err| panic!("create tempdir for {}: {err}", fixture.id));
    let db_path = tempdir.path().join(format!("{}.sqlite3", fixture.id));
    let conn = DbConn::open_file(db_path.display().to_string())
        .unwrap_or_else(|err| panic!("open SQLite fixture {}: {err}", db_path.display()));
    conn.execute_raw("PRAGMA foreign_keys = ON")
        .unwrap_or_else(|err| panic!("enable foreign keys for {}: {err}", fixture.id));
    conn.execute_raw(sql)
        .unwrap_or_else(|err| panic!("execute SQL recipe for {}: {err}", fixture.id));
    (tempdir, conn)
}

fn materialize_parity_fixture(fixture: &Fixture) -> (tempfile::TempDir, DbConn) {
    let sql = recipe_text(artifact_recipe_file(fixture, "sqlite_seed_sql"));
    let (tempdir, conn) = materialize_sql_fixture(fixture, &sql);
    let archive_recipe = artifact_recipe_file(fixture, "archive_reservation_json");
    let archive_json = recipe_text(archive_recipe);
    let reservation_id = serde_json::from_str::<Value>(&archive_json)
        .expect("archive JSON should parse")
        .get("id")
        .and_then(Value::as_i64)
        .expect("archive JSON should carry id");
    let archive_dir = tempdir
        .path()
        .join("projects/reservation-regression/file_reservations");
    std::fs::create_dir_all(&archive_dir).expect("create archive reservation dir");
    std::fs::write(
        archive_dir.join(format!("id-{reservation_id}.json")),
        archive_json,
    )
    .expect("write archive reservation JSON");
    (tempdir, conn)
}

fn scalar_i64(conn: &DbConn, sql: &str, column: &str) -> i64 {
    let rows = conn
        .query_sync(sql, &[])
        .unwrap_or_else(|err| panic!("query scalar `{sql}`: {err}"));
    rows.first()
        .unwrap_or_else(|| panic!("query scalar `{sql}` returned no rows"))
        .get_named::<i64>(column)
        .unwrap_or_else(|err| panic!("read scalar column `{column}` from `{sql}`: {err}"))
}

fn corruption_fixture_ids() -> BTreeSet<String> {
    let manifest: CorruptionManifest =
        serde_json::from_str(CORRUPTION_MANIFEST_JSON).expect("corruption manifest must parse");
    manifest
        .fixtures
        .into_iter()
        .map(|fixture| fixture.id)
        .collect()
}

fn assert_sql_seed_contract(fixture: &Fixture, recipe_file: &str) {
    let sql = recipe_text(recipe_file);
    for required_fragment in [
        "CREATE TABLE projects",
        "CREATE TABLE agents",
        "CREATE TABLE file_reservations",
        "CREATE TABLE file_reservation_releases",
        "INSERT INTO file_reservations",
    ] {
        assert!(
            sql.contains(required_fragment),
            "{} SQL recipe must contain `{required_fragment}`",
            fixture.id
        );
    }

    let (_tempdir, conn) = materialize_sql_fixture(fixture, &sql);
    assert_eq!(
        scalar_i64(&conn, "SELECT COUNT(*) AS n FROM projects", "n"),
        1,
        "{} SQL recipe should seed exactly one project",
        fixture.id
    );
    assert_eq!(
        scalar_i64(&conn, "SELECT COUNT(*) AS n FROM file_reservations", "n"),
        1,
        "{} SQL recipe should seed exactly one reservation",
        fixture.id
    );

    match fixture.id.as_str() {
        "stale_agent_id_row" => {
            assert!(sql.contains("CorrectHolder"));
            assert!(sql.contains("StaleHolder"));
            assert!(sql.contains("101, 1, 2"));
            assert_eq!(
                scalar_i64(
                    &conn,
                    "SELECT agent_id FROM file_reservations WHERE id = 101",
                    "agent_id"
                ),
                2
            );
            assert_eq!(
                scalar_i64(
                    &conn,
                    "SELECT COUNT(*) AS n FROM agents WHERE name = 'CorrectHolder'",
                    "n"
                ),
                1
            );
        }
        "stuck_null_released_ts" => {
            assert!(sql.contains("201, 1, 1"));
            assert!(sql.contains("NULL"));
            assert!(!sql.contains("INSERT INTO file_reservation_releases (reservation_id"));
            assert_eq!(
                scalar_i64(
                    &conn,
                    "SELECT released_ts IS NULL AS is_null FROM file_reservations WHERE id = 201",
                    "is_null"
                ),
                1
            );
            assert_eq!(
                scalar_i64(
                    &conn,
                    "SELECT COUNT(*) AS n FROM file_reservation_releases",
                    "n"
                ),
                0
            );
        }
        "db_archive_active_state_mismatch" => {
            assert!(sql.contains("301, 1, 1"));
            assert!(sql.contains("INSERT INTO file_reservation_releases"));
            assert!(sql.contains("1700003010000000"));
            assert_eq!(
                scalar_i64(
                    &conn,
                    "SELECT released_ts FROM file_reservations WHERE id = 301",
                    "released_ts"
                ),
                1_700_003_010_000_000
            );
            assert_eq!(
                scalar_i64(
                    &conn,
                    "SELECT released_ts FROM file_reservation_releases WHERE reservation_id = 301",
                    "released_ts"
                ),
                1_700_003_010_000_000
            );
        }
        other => panic!("unexpected SQL-backed reservation fixture `{other}`"),
    }
}

fn assert_archive_json_contract(fixture: &Fixture, recipe_file: &str) {
    let json: Value = serde_json::from_str(&recipe_text(recipe_file))
        .unwrap_or_else(|err| panic!("{} archive JSON must parse: {err}", fixture.id));
    let object = json
        .as_object()
        .unwrap_or_else(|| panic!("{} archive JSON must be an object", fixture.id));

    for key in [
        "id",
        "project",
        "agent",
        "path_pattern",
        "exclusive",
        "reason",
        "created_ts",
        "expires_ts",
        "released_ts",
    ] {
        assert!(
            object.contains_key(key),
            "{} archive JSON missing `{key}`",
            fixture.id
        );
    }

    match fixture.id.as_str() {
        "stale_agent_id_row" => {
            assert_eq!(json["id"], 101);
            assert_eq!(json["agent"], "CorrectHolder");
            assert!(json["released_ts"].is_null());
        }
        "stuck_null_released_ts" => {
            assert_eq!(json["id"], 201);
            assert_eq!(json["agent"], "ReleaseHolder");
            assert_eq!(json["released_ts"], 1_700_002_010_000_000_i64);
        }
        "db_archive_active_state_mismatch" => {
            assert_eq!(json["id"], 301);
            assert_eq!(json["agent"], "ArchiveActiveHolder");
            assert!(json["released_ts"].is_null());
        }
        other => panic!("unexpected archive-backed reservation fixture `{other}`"),
    }
}

fn assert_corruption_reference_contract(
    fixture: &Fixture,
    corpus: &str,
    fixture_id: &str,
    corruption_ids: &BTreeSet<String>,
) {
    assert_eq!(
        fixture.id, "btree_page_2288_release_malformed",
        "only the release-path malformed-B-tree fixture should reference the corruption corpus"
    );
    assert_eq!(corpus, "tests/fixtures/corruption_corpus/manifest.json");
    assert_eq!(fixture_id, "btree_page_type_zero");
    assert!(
        corruption_ids.contains(fixture_id),
        "referenced corruption fixture `{fixture_id}` must exist"
    );
    assert!(fixture.incident_anchor.contains("2288"));
    assert!(
        fixture
            .expected_mismatches
            .iter()
            .any(|mismatch| mismatch == "btree_page=2288")
    );
}

#[test]
fn reservation_regression_manifest_covers_required_drift_modes() {
    let manifest = manifest();
    assert_eq!(manifest.schema_version, 1);
    assert_eq!(
        manifest.corpus_id,
        "agent-mail-reservation-regression-corpus"
    );
    assert_eq!(manifest.generated_by, "br-bvq1x.6.4");

    let fixtures_by_id: BTreeMap<&str, &Fixture> = manifest
        .fixtures
        .iter()
        .map(|fixture| (fixture.id.as_str(), fixture))
        .collect();

    let required_ids = BTreeSet::from([
        "stale_agent_id_row",
        "stuck_null_released_ts",
        "db_archive_active_state_mismatch",
        "btree_page_2288_release_malformed",
    ]);
    let actual_ids = fixtures_by_id.keys().copied().collect::<BTreeSet<_>>();
    assert_eq!(actual_ids, required_ids);

    let consumer_beads = BTreeSet::from(["br-bvq1x.6.1", "br-bvq1x.6.2", "br-bvq1x.6.3"]);
    let mut covered_consumers = BTreeSet::new();
    let corruption_ids = corruption_fixture_ids();

    for fixture in &manifest.fixtures {
        assert_ne!(fixture.title.trim(), "");
        assert_ne!(fixture.incident_anchor.trim(), "");
        assert_ne!(fixture.drift_mode.trim(), "");
        assert_ne!(fixture.expected_detector.trim(), "");
        assert_ne!(fixture.expected_mismatches, [] as [String; 0]);
        assert!(
            fixture
                .consumers
                .iter()
                .any(|consumer| consumer_beads.contains(consumer.as_str())),
            "{} must be consumed by at least one Track F implementation bead",
            fixture.id
        );
        covered_consumers.extend(
            fixture
                .consumers
                .iter()
                .filter(|consumer| consumer_beads.contains(consumer.as_str()))
                .map(String::as_str),
        );
        assert!(
            !fixture.artifacts.is_empty(),
            "{} must have at least one artifact",
            fixture.id
        );

        for artifact in &fixture.artifacts {
            assert_artifact_path_matches_fixture(&fixture.id, &artifact.path);
            match (&artifact.kind[..], &artifact.recipe) {
                ("sqlite_seed_sql", Recipe::TextFile { file }) => {
                    assert_sql_seed_contract(fixture, file);
                }
                ("archive_reservation_json", Recipe::TextFile { file }) => {
                    assert_archive_json_contract(fixture, file);
                }
                (
                    "corruption_corpus_reference",
                    Recipe::CorruptionCorpusFixture { corpus, fixture_id },
                ) => {
                    assert_corruption_reference_contract(
                        fixture,
                        corpus,
                        fixture_id,
                        &corruption_ids,
                    );
                }
                (kind, recipe) => panic!(
                    "{} has unsupported artifact kind/recipe pairing: {kind} / {recipe:?}",
                    fixture.id
                ),
            }
        }
    }

    assert_eq!(covered_consumers, consumer_beads);
}

#[test]
fn reservation_parity_checker_reports_f4_field_drift() {
    let manifest = manifest();
    let fixtures_by_id = manifest
        .fixtures
        .iter()
        .map(|fixture| (fixture.id.as_str(), fixture))
        .collect::<BTreeMap<_, _>>();

    let stale_agent = fixtures_by_id["stale_agent_id_row"];
    let (storage_root, conn) = materialize_parity_fixture(stale_agent);
    let report =
        check_reservation_parity_with_db_conn(&conn, storage_root.path()).expect("parity report");
    assert!(!report.ok);
    assert_eq!(report.drift.agent_id_mismatches, 1);
    assert_eq!(report.drift.released_ts_mismatches, 0);
    assert!(
        report.examples.iter().any(|example| {
            example.detail.contains("reservation_id=101")
                && example.detail.contains("db_agent=StaleHolder")
                && example.detail.contains("archive_agent=CorrectHolder")
        }),
        "stale-agent report should include exact holder drift: {report:#?}"
    );

    let stuck_null = fixtures_by_id["stuck_null_released_ts"];
    let (storage_root, conn) = materialize_parity_fixture(stuck_null);
    let report =
        check_reservation_parity_with_db_conn(&conn, storage_root.path()).expect("parity report");
    assert!(!report.ok);
    assert_eq!(report.drift.released_ts_mismatches, 1);
    assert_eq!(report.drift.active_status_mismatches, 1);
    assert!(
        report.examples.iter().any(|example| {
            example.detail.contains("reservation_id=201")
                && example.detail.contains("db_released_ts=NULL")
                && example
                    .detail
                    .contains("archive_released_ts=1700002010000000")
        }),
        "stuck-null report should include exact release drift: {report:#?}"
    );

    let active_state = fixtures_by_id["db_archive_active_state_mismatch"];
    let (storage_root, conn) = materialize_parity_fixture(active_state);
    let report =
        check_reservation_parity_with_db_conn(&conn, storage_root.path()).expect("parity report");
    assert!(!report.ok);
    assert_eq!(report.drift.released_ts_mismatches, 1);
    assert_eq!(report.drift.active_status_mismatches, 1);
    assert!(
        report.examples.iter().any(|example| {
            example.detail.contains("reservation_id=301")
                && example.detail.contains("db_released_ts=1700003010000000")
                && example.detail.contains("archive_released_ts=NULL")
        }),
        "active-state report should include exact release drift: {report:#?}"
    );
}

// The guard deliberately reads the real process identity, so exercise it in a
// child process instead of mutating environment variables in parallel tests.
#[test]
fn reservation_release_guard_probe() {
    let Ok(root) = std::env::var("AM_RELEASE_GUARD_PROBE_ROOT") else {
        return;
    };
    let paths = (401..417)
        .map(|id| format!("src/{id}.rs"))
        .collect::<Vec<_>>();
    let conflicts = mcp_agent_mail_guard::guard_check(
        &Path::new(&root).join("projects/project"),
        Path::new(&root),
        &paths,
        false,
    )
    .expect("native guard reads reservation artifacts");
    if std::env::var("AM_RELEASE_GUARD_EXPECT_BLOCKED").as_deref() == Ok("1") {
        let blocked = conflicts
            .iter()
            .map(|conflict| &conflict.path)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            blocked,
            paths.iter().collect(),
            "every lost release blocks its path"
        );
    } else {
        assert!(
            conflicts.is_empty(),
            "repaired releases must unblock the guard: {conflicts:?}"
        );
    }
    println!("reservation-release-guard-probe-executed");
}

fn probe_release_guard(root: &Path, blocked: bool) {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "reservation_regression_fixtures::reservation_release_guard_probe",
            "--nocapture",
        ])
        .env("AGENT_NAME", "GreenStone")
        .env("AM_RELEASE_GUARD_PROBE_ROOT", root)
        .env(
            "AM_RELEASE_GUARD_EXPECT_BLOCKED",
            if blocked { "1" } else { "0" },
        )
        .output()
        .expect("run guard probe");
    assert!(
        output.status.success(),
        "native guard probe failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("reservation-release-guard-probe-executed"),
        "guard probe must execute its assertions, not merely exit successfully"
    );
}

#[test]
fn reservation_release_background_repair_survives_reopen_without_client_reads() {
    // Isolate the actual global archive queue and mutation fence used by the
    // failed release writer; no other test may flush this fixture's Git work.
    if std::env::var("AM_RELEASE_REOPEN_TEST_CHILD").as_deref() != Ok("1") {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "reservation_regression_fixtures::reservation_release_background_repair_survives_reopen_without_client_reads", "--nocapture"])
            .env("AM_RELEASE_REOPEN_TEST_CHILD", "1")
            .output().unwrap();
        assert!(
            output.status.success(),
            "release repair child failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("reservation-release-reopen-executed"),
            "integration child must execute the complete failed-write/reopen/repair scenario"
        );
        return;
    }
    use asupersync::{Cx, Outcome};
    use fastmcp_core::block_on;
    use mcp_agent_mail_core::{Config, reservation_artifact::reservation_artifact_filename};
    use mcp_agent_mail_db::{DbPoolConfig, create_pool, micros_to_iso, now_micros};
    use mcp_agent_mail_storage::recovery::reservation_reconcile::{
        ReservationReconcileCursor, reconcile_reservation_releases,
    };
    use std::sync::atomic::AtomicBool;

    mcp_agent_mail_core::config::with_isolated_default_storage_root_for_test(|_| {
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            database_url: mcp_agent_mail_core::disk::sqlite_url_from_path(
                &temp.path().join("mail.sqlite3"),
            ),
            storage_root: temp.path().join("archive"),
            ..Default::default()
        };
        std::fs::create_dir_all(&config.storage_root).unwrap();
        let settings = DbPoolConfig {
            database_url: config.database_url.clone(),
            storage_root: Some(config.storage_root.clone()),
            min_connections: 1,
            max_connections: 1,
            ..Default::default()
        };
        let pool = create_pool(&settings).unwrap();
        let cx = Cx::for_testing();
        let Outcome::Ok(conn) = block_on(pool.acquire(&cx)) else {
            panic!("fixture connection")
        };
        conn.execute_raw("INSERT INTO projects(id, slug, human_key, created_at) VALUES(71, 'project', '/project', 1)").unwrap();
        conn.execute_raw("INSERT INTO agents(id, project_id, name, program, model, inception_ts, last_active_ts) VALUES(81, 71, 'BlueLake', 'test', 'test', 1, 1)").unwrap();
        conn.execute_raw(
            "INSERT OR REPLACE INTO db_identity(singleton, generation_id) VALUES(0, 'aabb')",
        )
        .unwrap();
        let created = now_micros() - 60_000_000;
        let expires = created + 86_400_000_000;
        let mut active = Vec::new();
        for id in 401..417 {
            conn.execute_raw(&format!("INSERT INTO file_reservations(id, project_id, agent_id, path_pattern, \"exclusive\", reason, created_ts, expires_ts, released_ts) VALUES({id}, 71, 81, 'src/{id}.rs', 1, 'release-test', {created}, {expires}, NULL)")).unwrap();
            active.push(
                serde_json::json!({"id": id, "project": "/project", "agent": "BlueLake",
                "path_pattern": format!("src/{id}.rs"), "exclusive": true, "reason": "release-test",
                "created_ts": micros_to_iso(created), "expires_ts": micros_to_iso(expires),
                "released_ts": null, "db_generation": "aabb"}),
            );
        }
        drop(conn);
        let archive = mcp_agent_mail_storage::ensure_archive(&config, "project").unwrap();
        mcp_agent_mail_storage::write_file_reservation_records(&archive, &config, &active).unwrap();
        mcp_agent_mail_storage::flush_async_commits();
        let ids = (401..417).collect::<Vec<_>>();
        let released = block_on(mcp_agent_mail_db::queries::release_reservations_by_ids(
            &cx, &pool, &ids,
        ));
        assert!(
            matches!(released, Outcome::Ok(16)),
            "DB releases succeed: {released:?}"
        );
        let Outcome::Ok(conn) = block_on(pool.acquire(&cx)) else {
            panic!("release connection")
        };
        let mut released_records = active;
        for record in &mut released_records {
            let id = record["id"].as_i64().unwrap();
            let timestamp = scalar_i64(
                &conn,
                &format!(
                    "SELECT released_ts FROM file_reservation_releases WHERE reservation_id={id}"
                ),
                "released_ts",
            );
            record["released_ts"] = serde_json::json!(micros_to_iso(timestamp));
        }
        drop(conn);
        // A directory at the stable target makes the real writer fail even
        // under root. Restore the old ACTIVE file as a crash would leave it.
        let stable = archive
            .root
            .join("file_reservations")
            .join(reservation_artifact_filename(Some("aabb"), 401));
        let held = stable.with_extension("held");
        std::fs::rename(&stable, &held).unwrap();
        std::fs::create_dir(&stable).unwrap();
        assert!(
            mcp_agent_mail_storage::write_file_reservation_records(
                &archive,
                &config,
                &released_records
            )
            .is_err()
        );
        std::fs::rename(&stable, stable.with_extension("blocked")).unwrap();
        std::fs::rename(&held, &stable).unwrap();
        probe_release_guard(&config.storage_root, true);
        let Outcome::Ok(conn) = block_on(pool.acquire(&cx)) else {
            panic!("parity connection")
        };
        let before = check_reservation_parity_with_db_conn(&conn, &config.storage_root).unwrap();
        assert_eq!(before.drift.released_ts_mismatches, 16, "{before:?}");
        assert_eq!(before.drift.active_status_mismatches, 16, "{before:?}");
        drop(conn);
        drop(pool);

        // Reopen only the durable DB, with a fresh maintenance cursor. No
        // reservation read or mutation tool participates in convergence.
        let reopened = create_pool(&settings).unwrap();
        let mut cursor = ReservationReconcileCursor::default();
        let mut repaired = 0;
        for _ in 0..4 {
            let report = reconcile_reservation_releases(
                &cx,
                &reopened,
                &config,
                &mut cursor,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(report.repaired, 4, "{report:?}");
            assert_eq!(report.deferred, 0, "{report:?}");
            assert!(report.scanned <= 32);
            repaired += report.repaired;
        }
        assert_eq!(repaired, 16);
        probe_release_guard(&config.storage_root, false);
        let Outcome::Ok(conn) = block_on(reopened.acquire(&cx)) else {
            panic!("final parity connection")
        };
        let after = check_reservation_parity_with_db_conn(&conn, &config.storage_root).unwrap();
        assert!(
            after.ok,
            "background repair restores actual doctor parity: {after:?}"
        );
        assert_eq!(
            scalar_i64(&conn, "SELECT COUNT(*) AS n FROM file_reservations", "n"),
            16
        );
        assert_eq!(
            scalar_i64(
                &conn,
                "SELECT COUNT(*) AS n FROM file_reservation_releases",
                "n"
            ),
            16
        );
    });
    println!("reservation-release-reopen-executed");
}
