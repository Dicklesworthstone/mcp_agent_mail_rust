//! `fm-search-index-state-corrupt-index` — P2.
//!
//! **Subsystem**: search_index_state (br-2vdg9; B6 audit
//! `docs/DOCTOR_FM_DISPOSITION.md` — historical FM `search-v3-index-corrupt`).
//!
//! ## What's broken
//!
//! The Search V3 (frankensearch / Tantivy) on-disk index under
//! `$SEARCH_V3_INDEX_DIR` is corrupt, incomplete, or unopenable. Unlike a
//! corrupt `storage.sqlite3`, this is **low-risk**: the index is a *derived
//! artifact* — SQLite remains the source of truth and the index is rebuilt
//! from it. But until the index is rebuilt, lexical search silently returns
//! wrong or empty results, so an operator should know.
//!
//! `legacy_fts_residue` (db_state_files) only cleans *old* SQLite FTS5
//! artifacts; nothing detected a corrupt frankensearch index. This FM fills
//! that gap.
//!
//! ## Detection (pure over a filesystem path)
//!
//! The index layout (see `mcp_agent_mail_db::search_index_layout`) is:
//!
//! ```text
//! $SEARCH_V3_INDEX_DIR/indexes/{scope}/          # global | project-N | product-N
//!   active-lexical  -> lexical/{schema_hash}      # symlink to the live build
//!   active-semantic -> semantic/{schema_hash}
//!   lexical/{schema_hash}/  meta.json checkpoint.json ...tantivy files...
//! ```
//!
//! For each scope dir under `{root}/indexes/`, and each engine
//! (`lexical`/`semantic`) that has an `active-{engine}` link, the detector
//! flags the first of:
//!
//! 1. **dangling-active-link** — the `active-{engine}` link resolves to an
//!    index dir that does not exist.
//! 2. **missing-tantivy-manifest** (lexical only) — the active dir exists
//!    but lacks Tantivy's `meta.json`, so the index cannot be opened.
//! 3. **incomplete-build** — `checkpoint.json` parses but marks the active
//!    build `success=false` or has no `completed_ts` (a crashed/partial
//!    build was left active).
//! 4. **unreadable-checkpoint** — `checkpoint.json` is present but does not
//!    parse (metadata corruption).
//!
//! A scope/engine with no `active-{engine}` link is simply *not built* —
//! that is not corruption, so it is skipped. If `{root}/indexes/` does not
//! exist (no search has ever run), the detector emits nothing.
//!
//! ## Fix — detect-only
//!
//! The index is rebuildable, and quarantining it while a live server holds
//! it open would race the writer (owner coordination needed — the same
//! constraint as the WAL/reservation FMs). There is also no dedicated `am`
//! reindex verb today: a missing/corrupt index is rebuilt from SQLite when
//! the server restarts and the next search runs. So the finding carries the
//! restart-to-rebuild remediation and `fix()` is a no-op. (A future
//! enhancement could Op::Rename the corrupt active dir to quarantine after a
//! live-owner check.)

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub const FM_ID: &str = "fm-search-index-state-corrupt-index";
const FM_SEVERITY: &str = "P2";
const FM_SUBSYSTEM: &str = "search_index_state";

/// Why a given (scope, engine) index is considered corrupt/unusable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CorruptionReason {
    /// `active-{engine}` resolves to an index dir that does not exist.
    DanglingActiveLink,
    /// Active lexical dir is missing Tantivy's `meta.json` manifest.
    MissingTantivyManifest,
    /// `checkpoint.json` marks the build not-successful or never-completed.
    IncompleteBuild,
    /// `checkpoint.json` present but unparseable (metadata corruption).
    UnreadableCheckpoint,
}

impl CorruptionReason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DanglingActiveLink => "dangling_active_link",
            Self::MissingTantivyManifest => "missing_tantivy_manifest",
            Self::IncompleteBuild => "incomplete_build",
            Self::UnreadableCheckpoint => "unreadable_checkpoint",
        }
    }

    #[must_use]
    pub fn confidence(self) -> f32 {
        match self {
            // A dangling link or a missing Tantivy manifest is a hard
            // "this index cannot be opened" signal.
            Self::DanglingActiveLink | Self::MissingTantivyManifest => 0.9,
            Self::IncompleteBuild => 0.85,
            Self::UnreadableCheckpoint => 0.8,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CorruptSearchIndexFinding {
    pub index_root: String,
    /// Scope dir name: `global` / `project-N` / `product-N`.
    pub scope: String,
    /// `lexical` or `semantic`.
    pub engine: String,
    /// The resolved active index dir (the corrupt/missing target).
    pub active_dir: String,
    pub reason: CorruptionReason,
    pub detail: String,
}

impl CorruptSearchIndexFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "Search V3 {} index for scope `{}` is corrupt ({}): {}",
            self.engine,
            self.scope,
            self.reason.as_str(),
            self.detail,
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            confidence: self.reason.confidence(),
            evidence: serde_json::json!({
                "index_root": self.index_root,
                "scope": self.scope,
                "engine": self.engine,
                "active_dir": self.active_dir,
                "reason": self.reason.as_str(),
                "detail": self.detail,
                "impact": "the Search V3 index is a derived artifact (SQLite is the source of truth); message data is safe, but lexical search returns wrong/empty results until the index is rebuilt",
                "remediation_steps": [
                    "Restart the server (`am serve-http` / `mcp-agent-mail serve`): a missing/corrupt Search V3 index is rebuilt from SQLite on the next search. No data loss — the index is derived.",
                    format!("To force a clean rebuild, stop the server and move the corrupt active index dir aside (do NOT delete): mv {} {}.corrupt — then restart.", self.active_dir, self.active_dir),
                    "There is no dedicated `am` reindex command today; rebuild is restart-driven.",
                ],
            }),
            remediation: FindingRemediation {
                // Detect-only: rebuild is restart-driven; auto-quarantine
                // would race a live writer (owner coordination needed).
                command: format!("am doctor explain {FM_ID}"),
                explain_command: format!("am doctor explain {FM_ID}"),
                auto_fixable: false,
                estimated_actions: 0,
            },
        }
    }
}

/// Detector. PURE over the supplied index-root path (reads the filesystem
/// under `{index_root}/indexes/`, no DB/network). Emits one finding per
/// corrupt (scope, engine). Stable ordering: scopes sorted, engines in
/// `lexical, semantic` order.
#[must_use]
pub fn detect(index_root: &Path) -> Vec<CorruptSearchIndexFinding> {
    let indexes_dir = index_root.join("indexes");
    let Ok(entries) = std::fs::read_dir(&indexes_dir) else {
        // No `indexes/` dir → no search index has ever been built → not corrupt.
        return Vec::new();
    };
    let mut scopes: Vec<String> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    scopes.sort();

    let root_str = index_root.to_string_lossy().into_owned();
    let mut out = Vec::new();
    for scope in &scopes {
        let scope_dir = indexes_dir.join(scope);
        for engine in ["lexical", "semantic"] {
            if let Some(f) = probe_engine(&root_str, scope, &scope_dir, engine) {
                out.push(f);
            }
        }
    }
    out
}

fn probe_engine(
    index_root: &str,
    scope: &str,
    scope_dir: &Path,
    engine: &str,
) -> Option<CorruptSearchIndexFinding> {
    let active_link = scope_dir.join(format!("active-{engine}"));
    // No active link → this engine is simply not built → not corrupt.
    let short = read_active_short(&active_link)?;
    let active_dir = scope_dir.join(engine).join(&short);
    let active_dir_str = active_dir.to_string_lossy().into_owned();

    let mk = |reason: CorruptionReason, detail: String| {
        Some(CorruptSearchIndexFinding {
            index_root: index_root.to_string(),
            scope: scope.to_string(),
            engine: engine.to_string(),
            active_dir: active_dir_str.clone(),
            reason,
            detail,
        })
    };

    if !active_dir.is_dir() {
        return mk(
            CorruptionReason::DanglingActiveLink,
            format!("active-{engine} resolves to `{short}`, which does not exist"),
        );
    }

    // Lexical: Tantivy refuses to open a dir without its `meta.json` manifest.
    if engine == "lexical" && !active_dir.join("meta.json").exists() {
        return mk(
            CorruptionReason::MissingTantivyManifest,
            "active lexical index dir is missing Tantivy's meta.json manifest (the index cannot be opened)".to_string(),
        );
    }

    // Checkpoint metadata: only flag when present-but-bad (an absent
    // checkpoint is tolerated — older/partial layouts may lack it).
    let checkpoint_path = active_dir.join("checkpoint.json");
    if checkpoint_path.exists() {
        match mcp_agent_mail_db::search_index_layout::IndexCheckpoint::read_from(&active_dir) {
            Ok(cp) if !cp.success => {
                return mk(
                    CorruptionReason::IncompleteBuild,
                    "checkpoint.json marks the active build as not successful (success=false)"
                        .to_string(),
                );
            }
            Ok(cp) if cp.completed_ts.is_none() => {
                return mk(
                    CorruptionReason::IncompleteBuild,
                    "checkpoint.json has no completed_ts — the active build never finished"
                        .to_string(),
                );
            }
            Ok(_) => {}
            Err(error) => {
                return mk(
                    CorruptionReason::UnreadableCheckpoint,
                    format!("checkpoint.json present but could not be parsed: {error}"),
                );
            }
        }
    }

    None
}

/// Resolve the `active-{engine}` link to the active build's directory name
/// (the schema-hash short). Mirrors `IndexLayout::active_schema`: a Unix
/// symlink target's basename, or the file contents on non-Unix. Returns
/// `None` when the link is absent (engine not built).
fn read_active_short(active_link: &Path) -> Option<String> {
    if let Ok(target) = std::fs::read_link(active_link) {
        return target.file_name().map(|n| n.to_string_lossy().into_owned());
    }
    let raw = std::fs::read_to_string(active_link).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Path::new(trimmed)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
}

/// Detect-only FM. `fix()` is a no-op — rebuild is restart-driven and
/// auto-quarantine would race a live server holding the index.
pub fn fix(
    _ctx: &crate::doctor::mutate::MutateContext,
    _finding: &CorruptSearchIndexFinding,
) -> Result<FixOutcome, crate::doctor::mutate::MutateError> {
    Ok(FixOutcome {
        actions_taken: 0,
        actions_skipped: 1,
        quarantined_paths: Vec::new(),
    })
}

/// Resolve the production Search V3 index root from the environment.
///
/// `SEARCH_V3_INDEX_DIR` is the single source (see
/// `mcp_agent_mail_db::search_tantivy::init_tantivy_backend`). `None` (unset
/// or empty) means no Tantivy index is configured, so the FM is skipped.
#[must_use]
pub fn index_root_from_env() -> Option<PathBuf> {
    std::env::var("SEARCH_V3_INDEX_DIR")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Build `{root}/indexes/{scope}/` and an `active-{engine}` symlink (unix)
    /// or file-fallback pointing at `{engine}/{short}`. Returns the active dir.
    fn make_active(root: &Path, scope: &str, engine: &str, short: &str) -> PathBuf {
        let scope_dir = root.join("indexes").join(scope);
        let active_dir = scope_dir.join(engine).join(short);
        fs::create_dir_all(&active_dir).unwrap();
        let link = scope_dir.join(format!("active-{engine}"));
        #[cfg(unix)]
        std::os::unix::fs::symlink(scope_dir.join(engine).join(short), &link).unwrap();
        #[cfg(not(unix))]
        fs::write(
            &link,
            scope_dir
                .join(engine)
                .join(short)
                .to_string_lossy()
                .as_bytes(),
        )
        .unwrap();
        active_dir
    }

    #[test]
    fn no_indexes_dir_is_clean() {
        let td = TempDir::new().unwrap();
        assert!(detect(td.path()).is_empty());
    }

    #[test]
    fn healthy_lexical_index_is_clean() {
        let td = TempDir::new().unwrap();
        let active = make_active(td.path(), "global", "lexical", "abc123def456");
        fs::write(active.join("meta.json"), b"{}").unwrap();
        assert!(detect(td.path()).is_empty(), "healthy index must not flag");
    }

    #[test]
    fn engine_with_no_active_link_is_skipped() {
        // A scope dir exists with index subdirs but no active-* link → the
        // engine is simply not activated; not corrupt.
        let td = TempDir::new().unwrap();
        fs::create_dir_all(td.path().join("indexes/global/lexical/xyz")).unwrap();
        assert!(detect(td.path()).is_empty());
    }

    #[test]
    fn dangling_active_link_flags() {
        let td = TempDir::new().unwrap();
        let scope_dir = td.path().join("indexes/global");
        fs::create_dir_all(&scope_dir).unwrap();
        let link = scope_dir.join("active-lexical");
        #[cfg(unix)]
        std::os::unix::fs::symlink(scope_dir.join("lexical/missinghash"), &link).unwrap();
        #[cfg(not(unix))]
        fs::write(
            &link,
            scope_dir
                .join("lexical/missinghash")
                .to_string_lossy()
                .as_bytes(),
        )
        .unwrap();

        let findings = detect(td.path());
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].reason, CorruptionReason::DanglingActiveLink);
        assert_eq!(findings[0].engine, "lexical");
        assert_eq!(findings[0].scope, "global");
    }

    #[test]
    fn missing_tantivy_manifest_flags_lexical() {
        let td = TempDir::new().unwrap();
        // Active lexical dir exists but has no meta.json.
        make_active(td.path(), "project-1", "lexical", "deadbeef0001");
        let findings = detect(td.path());
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].reason, CorruptionReason::MissingTantivyManifest);
        let g = findings[0].to_finding();
        assert_eq!(g.id, FM_ID);
        assert_eq!(g.severity, "P2");
        assert_eq!(g.subsystem, "search_index_state");
        assert!(!g.remediation.auto_fixable);
        assert!((g.confidence - 0.9).abs() < 1e-6);
    }

    #[test]
    fn semantic_engine_does_not_require_meta_json() {
        // Semantic is a vector index, not Tantivy — no meta.json requirement.
        let td = TempDir::new().unwrap();
        make_active(td.path(), "global", "semantic", "vec000111222");
        assert!(
            detect(td.path()).is_empty(),
            "semantic dir without meta.json must not be flagged as missing-manifest"
        );
    }

    #[test]
    fn incomplete_build_checkpoint_flags() {
        let td = TempDir::new().unwrap();
        let active = make_active(td.path(), "global", "lexical", "abc123def456");
        fs::write(active.join("meta.json"), b"{}").unwrap();
        // success=false, completed_ts present → IncompleteBuild via success.
        let checkpoint = serde_json::json!({
            "schema_hash": "abc123def456",
            "docs_indexed": 0,
            "started_ts": 1_700_000_000_000_000i64,
            "completed_ts": 1_700_000_001_000_000i64,
            "max_version": 0,
            "success": false,
        });
        fs::write(
            active.join("checkpoint.json"),
            serde_json::to_string_pretty(&checkpoint).unwrap(),
        )
        .unwrap();

        let findings = detect(td.path());
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].reason, CorruptionReason::IncompleteBuild);
    }

    #[test]
    fn never_completed_build_flags() {
        let td = TempDir::new().unwrap();
        let active = make_active(td.path(), "global", "lexical", "abc123def456");
        fs::write(active.join("meta.json"), b"{}").unwrap();
        let checkpoint = serde_json::json!({
            "schema_hash": "abc123def456",
            "docs_indexed": 10,
            "started_ts": 1_700_000_000_000_000i64,
            "completed_ts": serde_json::Value::Null,
            "max_version": 5,
            "success": true,
        });
        fs::write(
            active.join("checkpoint.json"),
            serde_json::to_string(&checkpoint).unwrap(),
        )
        .unwrap();
        let findings = detect(td.path());
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].reason, CorruptionReason::IncompleteBuild);
    }

    #[test]
    fn unreadable_checkpoint_flags() {
        let td = TempDir::new().unwrap();
        let active = make_active(td.path(), "global", "lexical", "abc123def456");
        fs::write(active.join("meta.json"), b"{}").unwrap();
        fs::write(active.join("checkpoint.json"), b"{ this is not json").unwrap();
        let findings = detect(td.path());
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].reason, CorruptionReason::UnreadableCheckpoint);
    }

    #[test]
    fn healthy_checkpoint_is_clean() {
        let td = TempDir::new().unwrap();
        let active = make_active(td.path(), "global", "lexical", "abc123def456");
        fs::write(active.join("meta.json"), b"{}").unwrap();
        let checkpoint = serde_json::json!({
            "schema_hash": "abc123def456",
            "docs_indexed": 100,
            "started_ts": 1_700_000_000_000_000i64,
            "completed_ts": 1_700_000_001_000_000i64,
            "max_version": 500,
            "success": true,
        });
        fs::write(
            active.join("checkpoint.json"),
            serde_json::to_string(&checkpoint).unwrap(),
        )
        .unwrap();
        assert!(detect(td.path()).is_empty());
    }

    #[test]
    fn finding_evidence_carries_remediation_and_is_detect_only() {
        let f = CorruptSearchIndexFinding {
            index_root: "/srv/idx".into(),
            scope: "global".into(),
            engine: "lexical".into(),
            active_dir: "/srv/idx/indexes/global/lexical/abc".into(),
            reason: CorruptionReason::MissingTantivyManifest,
            detail: "missing meta.json".into(),
        };
        let g = f.to_finding();
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains("rebuilt from SQLite"));
        assert!(s.contains("/srv/idx/indexes/global/lexical/abc"));
        assert!(!g.remediation.auto_fixable);
        assert_eq!(g.remediation.estimated_actions, 0);
    }

    #[test]
    fn reason_tokens_stable() {
        assert_eq!(
            CorruptionReason::DanglingActiveLink.as_str(),
            "dangling_active_link"
        );
        assert_eq!(
            CorruptionReason::MissingTantivyManifest.as_str(),
            "missing_tantivy_manifest"
        );
        assert_eq!(
            CorruptionReason::IncompleteBuild.as_str(),
            "incomplete_build"
        );
        assert_eq!(
            CorruptionReason::UnreadableCheckpoint.as_str(),
            "unreadable_checkpoint"
        );
    }
}
