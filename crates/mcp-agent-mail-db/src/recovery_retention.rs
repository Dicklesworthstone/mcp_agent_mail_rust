//! Bounded retention for doctor recovery debris (br-mudrv).
//!
//! `am doctor repair`/`reconstruct` and the startup self-heal capture a
//! forensic bundle + quarantine the bad DB on every recovery event, with NO
//! retention. Across repeated corruption events this grows without bound (a
//! prod box reached ~19 GB of `doctor/` dumps before startup wedged). This
//! module bounds that growth.
//!
//! Two surfaces consume it:
//! - the background integrity-guard maintenance sweep, which OBSERVES + ALERTS
//!   (it never deletes — the forensic-bundle manifest declares
//!   `automatic_deletion: false` and RULE 1 forbids it), and
//! - the operator-explicit `am doctor reclaim` verb, which CONSOLIDATES the
//!   excess into one reversible `doctor/reclaimable/<ts>/` directory (a
//!   rename, never a delete — matching the doctor's quarantine philosophy).
//!
//! The selection logic ([`select_recovery_debris_to_reclaim`]) is PURE so it is
//! exhaustively unit-testable; the filesystem enumeration and the move are
//! thin IO wrappers around it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Which kind of recovery debris an artifact is. Retention is applied
/// independently per category so a burst of one kind cannot evict the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DebrisCategory {
    /// A `doctor/forensics/<db_family>/<cmd>-<ts>/` bundle directory (each holds
    /// a full copy of the DB + sidecars, so they are large).
    ForensicBundle,
    /// A quarantined corrupt DB / sidecar sibling of the live database, e.g.
    /// `storage.sqlite3.corrupt-<ts>` or `storage.sqlite3-wal.reconstruct-failed-<ts>`.
    CorruptQuarantine,
    /// A direct `storage.sqlite3.archive-reconcile-<ts>` incident snapshot.
    /// These are also known to server-side `backup_rotation`, but reclaim
    /// consolidates them through the reversible, move-only doctor path rather
    /// than adding a new automatic file-deletion rule.
    ArchiveReconcileBackup,
    /// A startup-time WAL/SHM sidecar snapshot next to the live database, e.g.
    /// `storage.sqlite3-wal.startup-precheckpoint-<ts>` (copied before the
    /// startup `wal_checkpoint(TRUNCATE)`) or `*.startup-quarantine-<ts>`.
    /// These are written on every startup that finds a non-empty stale WAL and
    /// previously had NO retention at all, so they accumulate without bound
    /// (GH#185 reported 66 of them dating back weeks).
    SidecarSnapshot,
    /// A `.stale`-marked artifact next to the live database: a lock or
    /// sidecar renamed aside with a `.stale` suffix/infix (e.g.
    /// `storage.sqlite3.lock.stale`, `storage.sqlite3-wal.stale-<ts>`), the
    /// quarantine spelling the operator runbook prescribes for dead-owner
    /// locks (`mv <lock> <lock>.stale`). GH#210's incident host had 534
    /// `*.stale*` artifacts (mostly an echo of the GH#202 crash loop) that no
    /// retention category accounted for.
    StaleArtifact,
}

impl DebrisCategory {
    /// Stable, lower-case label for logs / JSON output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ForensicBundle => "forensic_bundle",
            Self::CorruptQuarantine => "corrupt_quarantine",
            Self::ArchiveReconcileBackup => "archive_reconcile_backup",
            Self::SidecarSnapshot => "sidecar_snapshot",
            Self::StaleArtifact => "stale_artifact",
        }
    }
}

/// One reclaimable artifact: a forensic bundle directory or a quarantine file.
#[derive(Debug, Clone)]
pub struct DebrisArtifact {
    pub path: PathBuf,
    /// Total bytes on disk (recursive for bundle directories).
    pub bytes: u64,
    /// Last-modified time in microseconds since the Unix epoch (`0` if unknown).
    pub modified_us: i64,
    pub category: DebrisCategory,
}

/// Retention policy for one recovery-debris category.
///
/// The count/age rules retain the `keep_min` newest artifacts plus anything
/// younger than `max_age_secs`. An optional byte ceiling adds a second,
/// independent selection rule: when a category's resident bytes exceed it,
/// oldest artifacts are selected until the category fits the ceiling. This
/// byte rule deliberately takes precedence over count/age retention: keeping
/// three incident-time multi-gigabyte copies forever is the size-blind failure
/// GH#210 exposed. Selection still only feeds the move-only reclaim surface;
/// it never deletes forensic evidence.
#[derive(Debug, Clone, Copy)]
pub struct RetentionPolicy {
    pub keep_min: usize,
    pub max_age_secs: u64,
    /// Maximum resident bytes per [`DebrisCategory`]. `None` disables the
    /// size rule, preserving the legacy count-and-age-only behavior.
    pub max_total_bytes_per_category: Option<u64>,
}

/// Compute the effective per-category ceiling for recovery debris.
///
/// A nonzero configured floor is scaled to at least five times the live
/// database size. This keeps a legitimately large mailbox from shedding its
/// incident evidence too aggressively while preventing a recovered tiny DB
/// from retaining multi-gigabyte incident copies indefinitely. A configured
/// floor of zero explicitly disables byte-based selection.
#[must_use]
pub fn effective_byte_budget_per_category(
    configured_floor_bytes: u64,
    live_database_bytes: u64,
) -> Option<u64> {
    (configured_floor_bytes > 0)
        .then(|| configured_floor_bytes.max(live_database_bytes.saturating_mul(5)))
}

/// The result of applying a [`RetentionPolicy`] to a set of artifacts.
#[derive(Debug, Clone, Default)]
pub struct ReclaimPlan {
    /// Artifacts selected for reclamation, oldest-first.
    pub prune: Vec<DebrisArtifact>,
    pub kept_count: usize,
    pub total_count: usize,
    pub reclaimable_bytes: u64,
    pub total_bytes: u64,
}

impl ReclaimPlan {
    /// Whether any artifact is reclaimable under the policy.
    #[must_use]
    pub fn has_reclaimable(&self) -> bool {
        !self.prune.is_empty()
    }
}

/// PURE: choose which debris artifacts to reclaim under `policy`.
///
/// Within each [`DebrisCategory`], the count-and-age rule selects artifacts
/// that are BOTH older than `max_age_secs` AND beyond the `keep_min` newest.
/// A negative/zero/unknown `modified_us` is treated as very old. If
/// `max_total_bytes_per_category` is set, the planner additionally selects
/// oldest artifacts from any category above that ceiling, even when those
/// artifacts would otherwise be protected by count or age. The returned prune
/// list is ordered oldest-first for a stable reclaim order.
#[must_use]
pub fn select_recovery_debris_to_reclaim(
    mut artifacts: Vec<DebrisArtifact>,
    policy: RetentionPolicy,
    now_us: i64,
) -> ReclaimPlan {
    let total_count = artifacts.len();
    let total_bytes = artifacts
        .iter()
        .map(|a| a.bytes)
        .fold(0_u64, u64::saturating_add);
    let max_age_us = i128::from(policy.max_age_secs).saturating_mul(1_000_000);

    // Newest-first so `keep_min` protects the most recent artifacts. Stable so
    // equal-mtime ties keep a deterministic order. Grouping keeps the byte
    // budget genuinely per category rather than letting a large forensic
    // bundle evict a small sidecar snapshot.
    artifacts.sort_by_key(|art| std::cmp::Reverse(art.modified_us));
    let mut by_category: BTreeMap<DebrisCategory, Vec<DebrisArtifact>> = BTreeMap::new();
    for artifact in artifacts {
        by_category
            .entry(artifact.category)
            .or_default()
            .push(artifact);
    }

    let mut prune = Vec::new();
    for (_category, artifacts) in by_category {
        let mut selected = vec![false; artifacts.len()];
        for (rank, artifact) in artifacts.iter().enumerate() {
            let within_keep_min = rank < policy.keep_min;
            let age_us = i128::from(now_us).saturating_sub(i128::from(artifact.modified_us));
            let young = age_us < max_age_us;
            selected[rank] = !within_keep_min && !young;
        }

        if let Some(max_bytes) = policy.max_total_bytes_per_category {
            let mut retained_bytes = artifacts
                .iter()
                .zip(&selected)
                .filter(|(_, selected)| !**selected)
                .map(|(artifact, _)| artifact.bytes)
                .fold(0_u64, u64::saturating_add);

            // Artifacts are newest-first; select oldest retained artifacts
            // first so a category converges on its byte ceiling with the most
            // recent evidence left in place whenever the budget allows it.
            for index in (0..artifacts.len()).rev() {
                if retained_bytes <= max_bytes {
                    break;
                }
                if !selected[index] {
                    selected[index] = true;
                    retained_bytes = retained_bytes.saturating_sub(artifacts[index].bytes);
                }
            }
        }

        prune.extend(
            artifacts
                .into_iter()
                .zip(selected)
                .filter_map(|(artifact, selected)| selected.then_some(artifact)),
        );
    }

    prune.sort_by(|left, right| {
        left.modified_us
            .cmp(&right.modified_us)
            .then_with(|| left.path.cmp(&right.path))
    });
    let reclaimable_bytes = prune
        .iter()
        .map(|a| a.bytes)
        .fold(0_u64, u64::saturating_add);
    let kept_count = total_count - prune.len();
    ReclaimPlan {
        prune,
        kept_count,
        total_count,
        reclaimable_bytes,
        total_bytes,
    }
}

/// Enumerate all recovery debris under `storage_root` / next to `db_path`.
///
/// Combines forensic bundles (`doctor/forensics/.../`) with quarantine siblings
/// (`<db>.corrupt-*`, `<db>.reconstruct-failed-*`), archive-reconcile incident
/// snapshots (`<db>.archive-reconcile-*`), startup sidecar snapshots (`<db>-wal.startup-precheckpoint-*`,
/// `<db>-shm.startup-precheckpoint-*`, `<db>*.startup-quarantine-*`), and
/// `.stale`-marked artifacts (`<db>*.stale`, `<db>*.stale-*`, `<db>*.stale.*`).
#[must_use]
pub fn enumerate_recovery_debris(storage_root: &Path, db_path: &Path) -> Vec<DebrisArtifact> {
    let mut out = enumerate_forensic_bundles(storage_root);
    out.extend(enumerate_corrupt_quarantines(db_path));
    out
}

/// Bytes still resident under the move-only reclaim staging area.
///
/// Consolidation deliberately does not free disk space: it moves evidence to
/// `doctor/reclaimable/` for an operator to inspect and eventually remove.
/// Health must include those bytes in its resident/live-DB ratio rather than
/// reporting a false all-clear immediately after a successful consolidation.
#[must_use]
pub fn reclaimable_staging_bytes(storage_root: &Path) -> u64 {
    let root = storage_root.join("doctor").join("reclaimable");
    let Ok(metadata) = std::fs::symlink_metadata(&root) else {
        return 0;
    };
    if metadata.file_type().is_dir() {
        dir_size_bytes(&root)
    } else {
        0
    }
}

/// Enumerate forensic bundle directories under `<storage_root>/doctor/forensics/`.
#[must_use]
pub fn enumerate_forensic_bundles(storage_root: &Path) -> Vec<DebrisArtifact> {
    let root = storage_root.join("doctor").join("forensics");
    let mut out = Vec::new();
    let Ok(families) = std::fs::read_dir(&root) else {
        return out;
    };
    for family in families.flatten() {
        let family_path = family.path();
        // Each db-family directory holds the per-command bundle directories.
        let Ok(meta) = std::fs::symlink_metadata(&family_path) else {
            continue;
        };
        if !meta.file_type().is_dir() {
            continue;
        }
        let Ok(bundles) = std::fs::read_dir(&family_path) else {
            continue;
        };
        for bundle in bundles.flatten() {
            let bundle_path = bundle.path();
            let Ok(meta) = std::fs::symlink_metadata(&bundle_path) else {
                continue;
            };
            if !meta.file_type().is_dir() {
                continue;
            }
            out.push(DebrisArtifact {
                bytes: dir_size_bytes(&bundle_path),
                modified_us: mtime_us(&meta),
                path: bundle_path,
                category: DebrisCategory::ForensicBundle,
            });
        }
    }
    out
}

/// Classify only an artifact suffix of this exact configured database family.
///
/// Recovery words in the database basename carry no ownership information:
/// `mail.corrupt-live.db` and all of its live companions must remain live.
/// Likewise, `mail.db2.corrupt-*` and `mail.db.notes.corrupt-*` do not belong
/// to `mail.db`. Keep the basename byte-exact, including non-Unicode names.
fn classify_recovery_debris_file(
    database_name: &std::ffi::OsStr,
    file_name: &std::ffi::OsStr,
) -> Option<DebrisCategory> {
    let remainder = file_name
        .as_encoded_bytes()
        .strip_prefix(database_name.as_encoded_bytes())?;
    for companion in [
        "",
        "-wal",
        "-shm",
        "-journal",
        "-wal-cert",
        "-wal-cert-head",
        "-fsqlite-ns-gate",
        "-fsqlite-ns-use",
        ".lock",
    ] {
        let Some(suffix) = remainder
            .strip_prefix(companion.as_bytes())
            .and_then(|suffix| suffix.strip_prefix(b"."))
        else {
            continue;
        };
        let has_payload = |prefix: &[u8]| {
            suffix
                .strip_prefix(prefix)
                .is_some_and(|payload| !payload.is_empty())
        };
        if has_payload(b"archive-reconcile-") {
            return Some(DebrisCategory::ArchiveReconcileBackup);
        }
        if has_payload(b"corrupt-") || has_payload(b"reconstruct-failed-") {
            return Some(DebrisCategory::CorruptQuarantine);
        }
        if has_payload(b"startup-precheckpoint-") || has_payload(b"startup-quarantine-") {
            return Some(DebrisCategory::SidecarSnapshot);
        }
        if suffix == b"stale" || has_payload(b"stale-") || has_payload(b"stale.") {
            return Some(DebrisCategory::StaleArtifact);
        }
    }
    None
}

/// Enumerate quarantined corrupt-DB siblings next to `db_path`.
#[must_use]
pub fn enumerate_corrupt_quarantines(db_path: &Path) -> Vec<DebrisArtifact> {
    let mut out = Vec::new();
    let Some(db_name) = db_path.file_name() else {
        return out;
    };
    let parent = db_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let Ok(entries) = std::fs::read_dir(parent) else {
        return out;
    };
    for entry in entries.flatten() {
        let Some(category) = classify_recovery_debris_file(db_name, &entry.file_name()) else {
            continue;
        };
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.file_type().is_file() {
            continue;
        }
        out.push(DebrisArtifact {
            bytes: meta.len(),
            modified_us: mtime_us(&meta),
            path,
            category,
        });
    }
    out
}

/// Whether a filename is a recovery quarantine (corrupt / reconstruct-failed /
/// archive-reconcile-restore), as opposed to the live DB, a `.bak`, or a live
/// `-wal`/`-shm` sidecar.
///
/// This lexical helper does not establish ownership by a configured database;
/// the reclaim inventory uses `classify_recovery_debris_file` for that boundary.
#[must_use]
pub fn is_quarantine_name(name: &str) -> bool {
    name.contains(".corrupt-") || name.contains(".reconstruct-failed-")
}

/// Whether a filename is a direct archive-reconcile incident snapshot.
///
/// These files are count-rotated by the server, but count alone does not bound
/// their incident-time size. Include them in the doctor retention inventory so
/// an explicit reclaim or a successful-heal auto-reclaim can consolidate the
/// oldest oversized copies without deleting evidence.
#[must_use]
pub fn is_archive_reconcile_backup_name(name: &str) -> bool {
    name.contains(".archive-reconcile-")
}

/// Whether a filename is a startup-time WAL/SHM sidecar snapshot.
///
/// Matches `*.startup-precheckpoint-<ts>` / `*.startup-quarantine-<ts>`, as
/// opposed to the live DB, a `.bak`, or a live `-wal`/`-shm` sidecar. These
/// snapshots are forensic copies taken before the startup checkpoint and are
/// safe to reclaim under the same bounded-retention policy as quarantines
/// (GH#185).
#[must_use]
pub fn is_sidecar_snapshot_name(name: &str) -> bool {
    name.contains(".startup-precheckpoint-") || name.contains(".startup-quarantine-")
}

/// Whether a filename is a `.stale`-marked artifact (GH#210).
///
/// Matches the `.stale` suffix and the `.stale-<ts>` / `.stale.<ext>` infix
/// spellings this codebase produces or prescribes: the operator runbook's
/// dead-owner lock quarantine (`mv <lock> <lock>.stale`) and test/e2e
/// quarantines like `parity.stale-wal.quarantine`. The dot-anchored match
/// deliberately does NOT trip on words that merely contain "stale"
/// (`*.staleness`, `stale_report.json`), on the live DB, or on live
/// `-wal`/`-shm` sidecars.
#[must_use]
pub fn is_stale_artifact_name(name: &str) -> bool {
    // Deliberately case-sensitive: every producer of `.stale` names in this
    // codebase (runbook + e2e quarantines) mints lowercase, and a
    // case-insensitive match would be the only category matcher here that
    // deviates from the byte-exact conventions above.
    #[allow(clippy::case_sensitive_file_extension_comparisons)]
    let stale_suffix = name.ends_with(".stale");
    stale_suffix || name.contains(".stale-") || name.contains(".stale.")
}

// ────────────────────────────────────────────────────────────────────────────
// Direct storage-root backup inventory (single implementation; GH#210).
//
// This lived in `mcp-agent-mail-server::backup_rotation` and was lifted here
// so BOTH `am doctor health` (CLI) and the MCP `health_check` retention block
// (tools crate, which cannot depend on the server crate) consume one
// classifier. The server re-exports these for its rotation machinery.
// ────────────────────────────────────────────────────────────────────────────

/// Buckets that [`classify_backup_file`] maps filenames into. Each bucket has
/// an independent retention count — we don't want a corruption storm to
/// evict unrelated pre-migration backups, and vice versa.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BackupKind {
    /// `storage.sqlite3.corrupt-*` (plus -wal/-shm siblings)
    Corrupt,
    /// `storage.sqlite3.reconstruct-failed-*` and `storage.sqlite3.reconstructing-*`
    Reconstruct,
    /// `storage.sqlite3.archive-reconcile-*` (incl. -failed, -restore)
    ArchiveReconcile,
    /// `storage.sqlite3.salvage-*`
    Salvage,
    /// `storage.sqlite3.manual-backup-*` plus bare `.bak*`
    ManualBackup,
    /// `.pre-migrate.*`, `.pre-python-import-*`, `.pre-acfs-import-*`, `.pre-reindex-*`
    PreMigration,
}

impl BackupKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Corrupt => "corrupt",
            Self::Reconstruct => "reconstruct",
            Self::ArchiveReconcile => "archive_reconcile",
            Self::Salvage => "salvage",
            Self::ManualBackup => "manual_backup",
            Self::PreMigration => "pre_migration",
        }
    }
}

/// Read-only inventory of backup files eligible for rotation.
///
/// This excludes the live DB and its sidecars by way of
/// [`classify_backup_file`], so consumers such as `am doctor health` can
/// report retention resident bytes without accidentally treating mailbox state
/// as disposable backup material.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BackupInventory {
    pub artifact_count: usize,
    pub resident_bytes: u64,
    /// Individual classified paths, for read-only consumers that need to
    /// de-duplicate these files from another retention inventory.
    pub artifacts: Vec<BackupInventoryArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupInventoryArtifact {
    pub path: PathBuf,
    pub bytes: u64,
}

/// Classify a filename belonging to the configured database into a [`BackupKind`].
///
/// Returns `None` for the live DB, Codex DB, and any file that doesn't
/// match our known backup patterns. This is the *single* place that
/// decides whether a file counts as backup material at all; rotation
/// additionally skips [`BackupKind::ArchiveReconcile`] (owned by the
/// recovery-retention reclaim planner) even though it classifies here so
/// inventory consumers still see it.
#[must_use]
pub fn classify_backup_file(
    database_name: &std::ffi::OsStr,
    file_name: &std::ffi::OsStr,
) -> Option<BackupKind> {
    let (family_stem, suffix) = ["", "-wal", "-shm"].into_iter().find_map(|sidecar| {
        let mut stem = database_name.to_os_string();
        stem.push(sidecar);
        let mut prefix = stem.clone();
        prefix.push(".");
        file_name
            .as_encoded_bytes()
            .strip_prefix(prefix.as_encoded_bytes())
            .map(|suffix| (stem, suffix))
    })?;
    // Only the ASCII recovery suffix needs decoding. The configured basename
    // stays byte-exact, including non-Unicode Unix filenames.
    let after_stem = std::str::from_utf8(suffix).ok()?;

    // `archive-reconcile-` covers the bare, `-failed-*`, and `-restore-*`
    // variants in one check (they all share the prefix).
    if after_stem.starts_with("archive-reconcile-") {
        return Some(BackupKind::ArchiveReconcile);
    }
    if after_stem.starts_with("reconstruct-") || after_stem.starts_with("reconstructing-") {
        return Some(BackupKind::Reconstruct);
    }
    if after_stem.starts_with("corrupt-") {
        return Some(BackupKind::Corrupt);
    }
    if after_stem.starts_with("salvage-") {
        return Some(BackupKind::Salvage);
    }
    // Manual-backup naming is either `manual-backup-<ts>` or the bare `bak`
    // / `bak.<ts>` legacy variant. Using a bare `starts_with("bak")` would
    // false-positive future filenames like `backup-plan.txt` that happen to
    // share the prefix — match exact variants only.
    let strict_bak =
        mcp_agent_mail_core::disk::classify_sqlite_recovery_candidate_name(&family_stem, file_name)
            .is_some_and(|candidate| {
                matches!(
                    candidate.kind(),
                    mcp_agent_mail_core::disk::SqliteRecoveryCandidateKind::ProactiveBak
                        | mcp_agent_mail_core::disk::SqliteRecoveryCandidateKind::TimestampedBak
                )
            });
    if after_stem.starts_with("manual-backup-") || strict_bak {
        return Some(BackupKind::ManualBackup);
    }
    if after_stem.starts_with("pre-migrate")
        || after_stem.starts_with("pre-python-import")
        || after_stem.starts_with("pre-acfs-import")
        || after_stem.starts_with("pre-reindex")
    {
        return Some(BackupKind::PreMigration);
    }

    None
}

/// Return the count and bytes of direct backup files eligible for rotation.
///
/// Backups live beside the resolved primary, even outside the archive root.
/// Missing database parents produce an empty inventory, matching rotation's
/// no-op behavior during first-run bootstrap.
pub fn inspect_storage_backups(database_path: &Path) -> std::io::Result<BackupInventory> {
    let Some(database_name) = database_path.file_name() else {
        return Ok(BackupInventory::default());
    };
    let parent = database_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let entries = match std::fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BackupInventory::default());
        }
        Err(error) => return Err(error),
    };
    let mut inventory = BackupInventory::default();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.file_type().is_file() {
            continue;
        }
        if classify_backup_file(database_name, &entry.file_name()).is_some() {
            inventory.artifact_count = inventory.artifact_count.saturating_add(1);
            inventory.resident_bytes = inventory.resident_bytes.saturating_add(metadata.len());
            inventory.artifacts.push(BackupInventoryArtifact {
                path,
                bytes: metadata.len(),
            });
        }
    }
    Ok(inventory)
}

// ────────────────────────────────────────────────────────────────────────────
// Combined retention resident footprint (GH#210).
// ────────────────────────────────────────────────────────────────────────────

/// Read-only retention footprint for one mailbox.
///
/// Combines recovery debris + direct backups (de-duplicated —
/// archive-reconcile files are visible to both inventories) + move-only
/// reclaim staging. This is the ONE implementation behind both
/// `am doctor health`'s `retention_resident` line and the MCP `health_check`
/// `retention` block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetentionResidentStats {
    pub recovery_debris_artifacts: usize,
    pub direct_backup_only_artifacts: usize,
    pub reclaimable_staging_bytes: u64,
    /// Total resident bytes: recovery debris + de-duplicated direct backups
    /// + reclaim staging.
    pub resident_bytes: u64,
    pub live_database_bytes: Option<u64>,
    /// Resident bytes per category label: the [`DebrisCategory::as_str`]
    /// labels, plus `"direct_backup"` (rotation-eligible files not already
    /// counted as recovery debris) and `"reclaimable_staging"`. Only nonzero
    /// categories are present.
    pub resident_bytes_by_category: BTreeMap<&'static str, u64>,
    /// Bytes the retention policy would consolidate right now (`0` when the
    /// caller passed no policy). Compared against the operator alert
    /// threshold, this is the "reclaimable attention" signal the integrity
    /// guard's observe-and-alert sweep warns on.
    pub reclaimable_bytes: u64,
}

/// Compute the [`RetentionResidentStats`] for `storage_root` / `database_path`.
///
/// `reclaim_policy` optionally applies [`select_recovery_debris_to_reclaim`]
/// (with the supplied `now_us`) to the enumerated recovery debris so the
/// caller also learns how many bytes are reclaimable under the operator's
/// configured retention policy.
pub fn retention_resident_stats(
    storage_root: &Path,
    database_path: &Path,
    reclaim_policy: Option<(RetentionPolicy, i64)>,
) -> std::io::Result<RetentionResidentStats> {
    let recovery_debris = enumerate_recovery_debris(storage_root, database_path);
    let recovery_paths = recovery_debris
        .iter()
        .map(|artifact| {
            std::fs::canonicalize(&artifact.path).unwrap_or_else(|_| artifact.path.clone())
        })
        .collect::<std::collections::BTreeSet<_>>();

    let mut resident_bytes_by_category: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut recovery_bytes = 0_u64;
    for artifact in &recovery_debris {
        recovery_bytes = recovery_bytes.saturating_add(artifact.bytes);
        let slot = resident_bytes_by_category
            .entry(artifact.category.as_str())
            .or_default();
        *slot = slot.saturating_add(artifact.bytes);
    }

    let backup_inventory = inspect_storage_backups(database_path)?;
    let mut direct_backup_only_artifacts = 0_usize;
    let mut direct_backup_only_bytes = 0_u64;
    for artifact in &backup_inventory.artifacts {
        let identity =
            std::fs::canonicalize(&artifact.path).unwrap_or_else(|_| artifact.path.clone());
        if recovery_paths.contains(&identity) {
            continue;
        }
        direct_backup_only_artifacts += 1;
        direct_backup_only_bytes = direct_backup_only_bytes.saturating_add(artifact.bytes);
    }
    if direct_backup_only_bytes > 0 {
        resident_bytes_by_category.insert("direct_backup", direct_backup_only_bytes);
    }

    let staging_bytes = reclaimable_staging_bytes(storage_root);
    if staging_bytes > 0 {
        resident_bytes_by_category.insert("reclaimable_staging", staging_bytes);
    }

    let reclaimable_bytes = reclaim_policy.map_or(0, |(policy, now_us)| {
        select_recovery_debris_to_reclaim(recovery_debris.clone(), policy, now_us).reclaimable_bytes
    });

    Ok(RetentionResidentStats {
        recovery_debris_artifacts: recovery_debris.len(),
        direct_backup_only_artifacts,
        reclaimable_staging_bytes: staging_bytes,
        resident_bytes: recovery_bytes
            .saturating_add(direct_backup_only_bytes)
            .saturating_add(staging_bytes),
        live_database_bytes: std::fs::metadata(database_path)
            .ok()
            .map(|metadata| metadata.len()),
        resident_bytes_by_category,
        reclaimable_bytes,
    })
}

/// Outcome of [`consolidate_debris`].
#[derive(Debug, Clone, Default)]
pub struct ReclaimOutcome {
    /// Moves whose source and destination parent-directory syncs completed.
    pub moved: usize,
    pub moved_bytes: u64,
    /// `(source_path, error_message)` for incomplete operations. If a rename
    /// completed but a subsequent directory sync failed, the message names
    /// the retained destination explicitly. Such evidence must not be retried
    /// or described as still present at the source.
    pub failures: Vec<(PathBuf, String)>,
}

/// Consolidate (MOVE — never delete) the planned debris into `dest_dir`.
///
/// Each move uses atomic no-replace publication, including when the source is
/// an entire forensic directory. Existing and raced destination entries are
/// never replaced. A bounded collision budget leaves the source in place on
/// exhaustion. Completed moves are counted only after both parent-directory
/// syncs succeed; a sync failure preserves the moved evidence and reports its
/// actual destination instead of attempting a destructive rollback.
///
/// New directories are private at creation on Unix. This is not a claim of
/// full ancestor anchoring: callers must still supply a trusted storage-root
/// namespace. Non-Unix platforms fail closed until directory durability has
/// a supported implementation; an empty plan remains a side-effect-free no-op.
pub fn consolidate_debris(plan: &ReclaimPlan, dest_dir: &Path) -> std::io::Result<ReclaimOutcome> {
    if plan.prune.is_empty() {
        return Ok(ReclaimOutcome::default());
    }
    if !cfg!(unix) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "durable recovery-debris consolidation is unsupported on this platform; sources retained",
        ));
    }
    if let Some(parent) = dest_dir
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        create_private_reclaim_directory(parent, true)?;
    }
    create_private_reclaim_directory(dest_dir, false)?;
    let mut outcome = ReclaimOutcome::default();
    for art in &plan.prune {
        match move_recovery_debris(&art.path, dest_dir) {
            Ok(_) => {
                outcome.moved += 1;
                outcome.moved_bytes = outcome.moved_bytes.saturating_add(art.bytes);
            }
            Err(err) => outcome.failures.push((art.path.clone(), err.to_string())),
        }
    }
    Ok(outcome)
}

const MAX_RECLAIM_MOVE_ATTEMPTS: u32 = 128;

fn create_private_reclaim_directory(path: &Path, recursive: bool) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(recursive);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

/// Open the directory itself without following a final-component symlink.
/// Preflight sync support before any source can be moved. The retained file
/// handle also prevents a later pathname substitution from redirecting the
/// durability operation to a different directory.
fn open_reclaim_sync_directory(path: &Path) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use rustix::fs::{Mode, OFlags};
        let fd = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?;
        let directory = std::fs::File::from(fd);
        directory.sync_all()?;
        Ok(directory)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "recovery-debris directory sync is unsupported on this platform",
        ))
    }
}

fn sync_reclaim_move_parents(
    source_parent: &std::fs::File,
    destination_parent: &std::fs::File,
) -> std::io::Result<()> {
    // Attempt BOTH syncs even if one fails. Never undo a published move.
    let destination_result = destination_parent.sync_all();
    let source_result = source_parent.sync_all();
    destination_result.and(source_result)
}

fn move_recovery_debris(source: &Path, destination_dir: &Path) -> std::io::Result<PathBuf> {
    move_recovery_debris_with(
        source,
        destination_dir,
        crate::pool::rename_noreplace_preserving_source,
        sync_reclaim_move_parents,
    )
}

fn move_recovery_debris_with<R, S>(
    source: &Path,
    destination_dir: &Path,
    mut rename: R,
    mut sync: S,
) -> std::io::Result<PathBuf>
where
    R: FnMut(&Path, &Path) -> std::io::Result<()>,
    S: FnMut(&std::fs::File, &std::fs::File) -> std::io::Result<()>,
{
    let name = source.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "artifact has no file name")
    })?;
    let metadata = std::fs::symlink_metadata(source)?;
    if !(metadata.file_type().is_file() || metadata.file_type().is_dir()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "recovery artifact is not a regular file or real directory",
        ));
    }
    let source_parent_path = source
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let source_parent = open_reclaim_sync_directory(source_parent_path)?;
    let destination_parent = open_reclaim_sync_directory(destination_dir)?;
    // Persist the newly claimed quarantine directory's own directory entry
    // as well as the entries that will be placed inside it.
    let quarantine_parent_path = destination_dir
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let _quarantine_parent = open_reclaim_sync_directory(quarantine_parent_path)?;
    for suffix in 0..MAX_RECLAIM_MOVE_ATTEMPTS {
        let mut leaf = name.to_os_string();
        if suffix != 0 {
            leaf.push(format!(".{suffix}"));
        }
        let destination = destination_dir.join(leaf);
        match rename(source, &destination) {
            Ok(()) => {
                sync(&source_parent, &destination_parent).map_err(|error| {
                    std::io::Error::new(
                        error.kind(),
                        format!(
                            "artifact moved to {}; directory durability is unconfirmed: {error}; evidence retained at destination, do not retry or roll back this move",
                            destination.display()
                        ),
                    )
                })?;
                return Ok(destination);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!(
            "reclaim collision budget exhausted for {} under {}; source retained",
            source.display(),
            destination_dir.display()
        ),
    ))
}

/// Recursive on-disk byte total for a directory, without following symlinks
/// (so a symlinked entry contributes nothing and cannot cause a cycle).
#[must_use]
fn dir_size_bytes(root: &Path) -> u64 {
    let mut total = 0_u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            let file_type = meta.file_type();
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                stack.push(path);
            } else {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

fn mtime_us(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_micros()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR_US: i64 = 3_600 * 1_000_000;
    const DAY_US: i64 = 24 * HOUR_US;

    fn art(name: &str, bytes: u64, modified_us: i64, category: DebrisCategory) -> DebrisArtifact {
        DebrisArtifact {
            path: PathBuf::from(name),
            bytes,
            modified_us,
            category,
        }
    }

    #[test]
    fn keeps_keep_min_newest_even_when_old() {
        // 4 bundles, all older than max_age; keep_min=2 must retain the 2 newest.
        let now = 100 * DAY_US;
        let artifacts = vec![
            art("b1", 10, now - 40 * DAY_US, DebrisCategory::ForensicBundle),
            art("b2", 10, now - 30 * DAY_US, DebrisCategory::ForensicBundle),
            art("b3", 10, now - 20 * DAY_US, DebrisCategory::ForensicBundle),
            art("b4", 10, now - 10 * DAY_US, DebrisCategory::ForensicBundle),
        ];
        let policy = RetentionPolicy {
            keep_min: 2,
            max_age_secs: 14 * 24 * 3_600,
            max_total_bytes_per_category: None,
        };
        let plan = select_recovery_debris_to_reclaim(artifacts, policy, now);
        // b3 + b4 are the 2 newest → kept; b1 + b2 are old + beyond keep_min → pruned.
        assert_eq!(plan.prune.len(), 2);
        assert_eq!(plan.kept_count, 2);
        assert_eq!(plan.reclaimable_bytes, 20);
        assert_eq!(plan.total_bytes, 40);
        // oldest-first ordering
        assert_eq!(plan.prune[0].path, PathBuf::from("b1"));
        assert_eq!(plan.prune[1].path, PathBuf::from("b2"));
    }

    #[test]
    fn keeps_young_artifacts_beyond_keep_min() {
        let now = 100 * DAY_US;
        let policy = RetentionPolicy {
            keep_min: 1,
            max_age_secs: 14 * 24 * 3_600,
            max_total_bytes_per_category: None,
        };
        let artifacts = vec![
            art(
                "old",
                10,
                now - 30 * DAY_US,
                DebrisCategory::CorruptQuarantine,
            ),
            art(
                "young1",
                10,
                now - DAY_US,
                DebrisCategory::CorruptQuarantine,
            ),
            art(
                "young2",
                10,
                now - 2 * DAY_US,
                DebrisCategory::CorruptQuarantine,
            ),
        ];
        let plan = select_recovery_debris_to_reclaim(artifacts, policy, now);
        // keep_min=1 keeps young1 (newest); young2 is <14d → kept by age; old is pruned.
        assert_eq!(plan.prune.len(), 1);
        assert_eq!(plan.prune[0].path, PathBuf::from("old"));
    }

    #[test]
    fn retention_is_per_category() {
        let now = 100 * DAY_US;
        let policy = RetentionPolicy {
            keep_min: 1,
            max_age_secs: 0, // nothing is "young"; only keep_min protects
            max_total_bytes_per_category: None,
        };
        let artifacts = vec![
            art(
                "f_old",
                5,
                now - 30 * DAY_US,
                DebrisCategory::ForensicBundle,
            ),
            art(
                "f_new",
                5,
                now - 10 * DAY_US,
                DebrisCategory::ForensicBundle,
            ),
            art(
                "q_old",
                7,
                now - 30 * DAY_US,
                DebrisCategory::CorruptQuarantine,
            ),
            art(
                "q_new",
                7,
                now - 10 * DAY_US,
                DebrisCategory::CorruptQuarantine,
            ),
        ];
        let plan = select_recovery_debris_to_reclaim(artifacts, policy, now);
        // keep_min=1 per category keeps f_new + q_new; prunes f_old + q_old.
        let pruned: Vec<_> = plan
            .prune
            .iter()
            .map(|a| a.path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(plan.prune.len(), 2);
        assert!(pruned.contains(&"f_old".to_string()));
        assert!(pruned.contains(&"q_old".to_string()));
        assert_eq!(plan.reclaimable_bytes, 12);
    }

    #[test]
    fn byte_budget_selects_oldest_young_artifacts_per_category() {
        let now = 100 * DAY_US;
        let policy = RetentionPolicy {
            // The legacy count/age rules would keep every one of these: all
            // forensic bundles are young and within the count target.
            keep_min: 3,
            max_age_secs: 14 * 24 * 3_600,
            max_total_bytes_per_category: Some(250),
        };
        let artifacts = vec![
            art(
                "forensic-old",
                100,
                now - 3 * DAY_US,
                DebrisCategory::ForensicBundle,
            ),
            art(
                "forensic-mid",
                100,
                now - 2 * DAY_US,
                DebrisCategory::ForensicBundle,
            ),
            art(
                "forensic-new",
                100,
                now - DAY_US,
                DebrisCategory::ForensicBundle,
            ),
            // This category is below its own ceiling even though the combined
            // resident total exceeds 250 bytes; budgets must not cross-evict.
            art(
                "quarantine-only",
                200,
                now - DAY_US,
                DebrisCategory::CorruptQuarantine,
            ),
        ];

        let plan = select_recovery_debris_to_reclaim(artifacts, policy, now);

        assert_eq!(plan.prune.len(), 1);
        assert_eq!(plan.prune[0].path, PathBuf::from("forensic-old"));
        assert_eq!(plan.reclaimable_bytes, 100);
        assert_eq!(plan.kept_count, 3);
        assert_eq!(plan.total_bytes, 500);
    }

    #[test]
    fn byte_budget_can_select_the_only_incident_size_artifact() {
        let now = 100 * DAY_US;
        let plan = select_recovery_debris_to_reclaim(
            vec![art(
                "single-huge-bundle",
                1_024,
                now - DAY_US,
                DebrisCategory::ForensicBundle,
            )],
            RetentionPolicy {
                keep_min: 5,
                max_age_secs: 14 * 24 * 3_600,
                max_total_bytes_per_category: Some(512),
            },
            now,
        );

        // A hard ceiling must not reproduce the old "three enormous copies
        // are retained forever" failure mode. Reclaim selection remains a
        // move to doctor/reclaimable, never a deletion.
        assert_eq!(plan.prune.len(), 1);
        assert_eq!(plan.prune[0].path, PathBuf::from("single-huge-bundle"));
        assert_eq!(plan.reclaimable_bytes, 1_024);
    }

    #[test]
    fn byte_budget_scales_with_live_database_and_can_be_disabled() {
        assert_eq!(
            effective_byte_budget_per_category(1_024, 100),
            Some(1_024),
            "configured floor protects small live databases"
        );
        assert_eq!(
            effective_byte_budget_per_category(1_024, 1_000),
            Some(5_000),
            "large live databases self-scale the evidence ceiling"
        );
        assert_eq!(effective_byte_budget_per_category(0, 1_000), None);
    }

    #[test]
    fn empty_input_is_noop() {
        let plan = select_recovery_debris_to_reclaim(
            Vec::new(),
            RetentionPolicy {
                keep_min: 5,
                max_age_secs: 0,
                max_total_bytes_per_category: None,
            },
            0,
        );
        assert!(!plan.has_reclaimable());
        assert_eq!(plan.total_count, 0);
    }

    #[test]
    fn unknown_mtime_is_treated_as_old() {
        let now = 100 * DAY_US;
        let policy = RetentionPolicy {
            keep_min: 1,
            max_age_secs: 14 * 24 * 3_600,
            max_total_bytes_per_category: None,
        };
        let artifacts = vec![
            art(
                "known_new",
                10,
                now - DAY_US,
                DebrisCategory::ForensicBundle,
            ),
            art("unknown", 10, 0, DebrisCategory::ForensicBundle),
        ];
        let plan = select_recovery_debris_to_reclaim(artifacts, policy, now);
        // known_new kept by keep_min; unknown (mtime 0 → very old) pruned.
        assert_eq!(plan.prune.len(), 1);
        assert_eq!(plan.prune[0].path, PathBuf::from("unknown"));
    }

    #[test]
    fn quarantine_name_classification() {
        assert!(is_quarantine_name(
            "storage.sqlite3.corrupt-20260618_145230_042"
        ));
        assert!(is_quarantine_name(
            "storage.sqlite3-wal.corrupt-20260618_145230_042"
        ));
        assert!(is_quarantine_name(
            "storage.sqlite3.reconstruct-failed-20260618_145230_042"
        ));
        assert!(!is_quarantine_name(
            "storage.sqlite3.archive-reconcile-restore-20260618_145230_042"
        ));
        // Not quarantines: live DB, backup, live sidecars.
        assert!(!is_quarantine_name("storage.sqlite3"));
        assert!(!is_quarantine_name("storage.sqlite3.bak"));
        assert!(!is_quarantine_name("storage.sqlite3-wal"));
        assert!(!is_quarantine_name("storage.sqlite3-shm"));
        assert!(!is_quarantine_name("storage.sqlite3.bak.meta.json"));
    }

    #[test]
    fn archive_reconcile_backup_name_classification() {
        assert!(is_archive_reconcile_backup_name(
            "storage.sqlite3.archive-reconcile-20260618_145230_042"
        ));
        assert!(is_archive_reconcile_backup_name(
            "storage.sqlite3.archive-reconcile-restore-20260618_145230_042"
        ));
        assert!(!is_archive_reconcile_backup_name("storage.sqlite3"));
        assert!(!is_archive_reconcile_backup_name("storage.sqlite3.bak"));
    }

    #[test]
    fn enumerates_archive_reconcile_backup_as_its_own_byte_budget_category() {
        let dir = tempfile::tempdir().unwrap();
        let storage_root = dir.path();
        let db_path = storage_root.join("storage.sqlite3");
        std::fs::write(&db_path, b"live-db").unwrap();
        let backup = storage_root.join("storage.sqlite3.archive-reconcile-20260618_145230_042");
        std::fs::write(&backup, vec![0_u8; 100]).unwrap();

        let debris = enumerate_recovery_debris(storage_root, &db_path);
        assert!(debris.iter().any(|artifact| {
            artifact.path == backup && artifact.category == DebrisCategory::ArchiveReconcileBackup
        }));
    }

    #[test]
    fn sidecar_snapshot_name_classification() {
        assert!(is_sidecar_snapshot_name(
            "storage.sqlite3-wal.startup-precheckpoint-20260625_120000_000"
        ));
        assert!(is_sidecar_snapshot_name(
            "storage.sqlite3-shm.startup-precheckpoint-20260625_120000_000"
        ));
        assert!(is_sidecar_snapshot_name(
            "storage.sqlite3-wal.startup-quarantine-20260625_120000_000"
        ));
        // Live DB / backup / live sidecars are NOT snapshots.
        assert!(!is_sidecar_snapshot_name("storage.sqlite3"));
        assert!(!is_sidecar_snapshot_name("storage.sqlite3.bak"));
        assert!(!is_sidecar_snapshot_name("storage.sqlite3-wal"));
        assert!(!is_sidecar_snapshot_name("storage.sqlite3-shm"));
        // Quarantines are a different category.
        assert!(!is_sidecar_snapshot_name(
            "storage.sqlite3.corrupt-20260618_145230_042"
        ));
    }

    #[test]
    fn stale_artifact_name_classification() {
        // Operator-runbook dead-owner lock quarantine (`mv <lock> <lock>.stale`).
        assert!(is_stale_artifact_name("storage.sqlite3.lock.stale"));
        assert!(is_stale_artifact_name("index.lock.stale"));
        // Suffixed sidecar quarantines with a timestamp / extension tail.
        assert!(is_stale_artifact_name(
            "storage.sqlite3-wal.stale-20260621_131955_667"
        ));
        assert!(is_stale_artifact_name("storage.sqlite3.stale"));
        assert!(is_stale_artifact_name("parity.stale-wal.quarantine"));
        assert!(is_stale_artifact_name("storage.sqlite3.stale.bak"));
        // Live DB / sidecars / backups and mere "stale" substrings are NOT
        // stale artifacts.
        assert!(!is_stale_artifact_name("storage.sqlite3"));
        assert!(!is_stale_artifact_name("storage.sqlite3-wal"));
        assert!(!is_stale_artifact_name("storage.sqlite3-shm"));
        assert!(!is_stale_artifact_name("storage.sqlite3.bak"));
        assert!(!is_stale_artifact_name("storage.sqlite3.staleness"));
        assert!(!is_stale_artifact_name("stale_report.json"));
        // Quarantine / snapshot spellings stay in their own categories.
        assert!(!is_stale_artifact_name(
            "storage.sqlite3.corrupt-20260618_145230_042"
        ));
    }

    #[test]
    fn enumerates_stale_artifacts_as_their_own_byte_budget_category() {
        let dir = tempfile::tempdir().unwrap();
        let storage_root = dir.path();
        let db_path = storage_root.join("storage.sqlite3");
        std::fs::write(&db_path, b"live-db").unwrap();
        std::fs::write(storage_root.join("storage.sqlite3-wal"), b"live-wal").unwrap();
        std::fs::write(
            storage_root.join("storage.sqlite3-wal.stale-20260621_131955_667"),
            vec![0_u8; 100],
        )
        .unwrap();
        std::fs::write(
            storage_root.join("storage.sqlite3.lock.stale"),
            vec![0_u8; 10],
        )
        .unwrap();

        let debris = enumerate_recovery_debris(storage_root, &db_path);
        let stale = debris
            .iter()
            .filter(|a| a.category == DebrisCategory::StaleArtifact)
            .count();
        assert_eq!(stale, 2, "debris: {debris:?}");
        // Live DB and live WAL are never debris.
        assert!(debris.iter().all(|a| a.path != db_path));
        assert!(
            debris
                .iter()
                .all(|a| a.path != storage_root.join("storage.sqlite3-wal"))
        );

        // The byte budget applies per category, so the stale bucket is
        // reclaimable under a zero-retention policy like every other bucket.
        let plan = select_recovery_debris_to_reclaim(
            debris,
            RetentionPolicy {
                keep_min: 0,
                max_age_secs: 0,
                max_total_bytes_per_category: Some(0),
            },
            i64::MAX,
        );
        assert_eq!(plan.prune.len(), 2);
        assert_eq!(plan.reclaimable_bytes, 110);
    }

    #[test]
    fn retention_resident_stats_inventory_follows_external_database() {
        let dir = tempfile::tempdir().unwrap();
        let storage_root = dir.path().join("archive");
        let database_dir = dir.path().join("database");
        std::fs::create_dir(&storage_root).unwrap();
        std::fs::create_dir(&database_dir).unwrap();
        let database_path = database_dir.join("custom.db");
        std::fs::write(&database_path, [0_u8; 7]).unwrap();
        std::fs::write(
            database_dir.join("custom.db.bak.20260101_000000"),
            [0_u8; 11],
        )
        .unwrap();
        std::fs::write(
            database_dir.join("custom.db-wal.bak.20260101_000000"),
            [0_u8; 13],
        )
        .unwrap();
        std::fs::write(
            database_dir.join("custom.db-shm.bak.20260101_000000"),
            [0_u8; 17],
        )
        .unwrap();
        std::fs::write(
            database_dir.join("other.db.bak.20260101_000000"),
            [0_u8; 19],
        )
        .unwrap();
        std::fs::write(
            storage_root.join("storage.sqlite3.bak.20260101_000000"),
            [0_u8; 23],
        )
        .unwrap();
        let stats = retention_resident_stats(&storage_root, &database_path, None).unwrap();
        assert_eq!(stats.direct_backup_only_artifacts, 3);
        assert_eq!(stats.resident_bytes, 41);
        assert_eq!(stats.live_database_bytes, Some(7));
    }

    #[test]
    fn retention_resident_stats_reports_bytes_by_category_and_reclaimable() {
        let dir = tempfile::tempdir().unwrap();
        let storage_root = dir.path();
        let db_path = storage_root.join("storage.sqlite3");
        std::fs::write(&db_path, vec![0_u8; 7]).unwrap();
        // Recovery debris: one quarantine + one stale artifact.
        std::fs::write(
            storage_root.join("storage.sqlite3.corrupt-20260101_000000_000"),
            vec![0_u8; 500],
        )
        .unwrap();
        std::fs::write(
            storage_root.join("storage.sqlite3-wal.stale-20260101_000000_000"),
            vec![0_u8; 40],
        )
        .unwrap();
        // Direct backup only (rotation-eligible, not recovery debris).
        std::fs::write(
            storage_root.join("storage.sqlite3.manual-backup-20260101_000000_000"),
            vec![0_u8; 60],
        )
        .unwrap();
        // Move-only reclaim staging residue.
        let staging = storage_root.join("doctor").join("reclaimable").join("run");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("old-artifact"), vec![0_u8; 30]).unwrap();

        let stats = retention_resident_stats(
            storage_root,
            &db_path,
            Some((
                RetentionPolicy {
                    keep_min: 0,
                    max_age_secs: 0,
                    max_total_bytes_per_category: None,
                },
                i64::MAX,
            )),
        )
        .unwrap();

        assert_eq!(stats.recovery_debris_artifacts, 2);
        assert_eq!(stats.direct_backup_only_artifacts, 1);
        assert_eq!(stats.reclaimable_staging_bytes, 30);
        assert_eq!(stats.resident_bytes, 500 + 40 + 60 + 30);
        assert_eq!(stats.live_database_bytes, Some(7));
        assert_eq!(
            stats.resident_bytes_by_category.get("corrupt_quarantine"),
            Some(&500)
        );
        assert_eq!(
            stats.resident_bytes_by_category.get("stale_artifact"),
            Some(&40)
        );
        assert_eq!(
            stats.resident_bytes_by_category.get("direct_backup"),
            Some(&60)
        );
        assert_eq!(
            stats.resident_bytes_by_category.get("reclaimable_staging"),
            Some(&30)
        );
        // Zero categories are omitted (compact, null-free block downstream).
        assert!(
            !stats
                .resident_bytes_by_category
                .contains_key("forensic_bundle")
        );
        // Zero-retention policy makes all recovery debris reclaimable.
        assert_eq!(stats.reclaimable_bytes, 540);

        // Archive-reconcile files are visible to both inventories; they must
        // be counted once (as recovery debris), never twice.
        std::fs::write(
            storage_root.join("storage.sqlite3.archive-reconcile-20260101_000000_000"),
            vec![0_u8; 200],
        )
        .unwrap();
        let stats = retention_resident_stats(storage_root, &db_path, None).unwrap();
        assert_eq!(stats.resident_bytes, 500 + 40 + 60 + 30 + 200);
        assert_eq!(
            stats
                .resident_bytes_by_category
                .get("archive_reconcile_backup"),
            Some(&200)
        );
        assert_eq!(stats.direct_backup_only_artifacts, 1);
        assert_eq!(stats.reclaimable_bytes, 0, "no policy → no reclaim probe");
    }

    #[cfg(unix)]
    #[test]
    fn enumerates_startup_precheckpoint_snapshots_as_sidecar_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let storage_root = dir.path();
        let db_path = storage_root.join("storage.sqlite3");
        std::fs::write(&db_path, b"live-db").unwrap();
        std::fs::write(storage_root.join("storage.sqlite3-wal"), b"live-wal").unwrap();
        for ts in [
            "20260625_120000_000",
            "20260626_120000_000",
            "20260627_120000_000",
        ] {
            std::fs::write(
                storage_root.join(format!("storage.sqlite3-wal.startup-precheckpoint-{ts}")),
                vec![0_u8; 100],
            )
            .unwrap();
        }
        std::fs::write(
            storage_root.join("storage.sqlite3-shm.startup-precheckpoint-20260625_120000_000"),
            vec![0_u8; 50],
        )
        .unwrap();

        let debris = enumerate_recovery_debris(storage_root, &db_path);
        let snapshots = debris
            .iter()
            .filter(|a| a.category == DebrisCategory::SidecarSnapshot)
            .count();
        assert_eq!(snapshots, 4, "debris: {debris:?}");
        // The live DB and live WAL sidecar are never enumerated as debris.
        assert!(debris.iter().all(|a| a.path != db_path));
        assert!(
            debris
                .iter()
                .all(|a| a.path != storage_root.join("storage.sqlite3-wal"))
        );

        // All are reclaimable under keep_min=0/max_age=0 and consolidate cleanly.
        let plan = select_recovery_debris_to_reclaim(
            debris,
            RetentionPolicy {
                keep_min: 0,
                max_age_secs: 0,
                max_total_bytes_per_category: None,
            },
            i64::MAX,
        );
        assert_eq!(plan.prune.len(), 4);
        let dest = storage_root.join("doctor").join("reclaimable").join("run");
        let outcome = consolidate_debris(&plan, &dest).unwrap();
        assert_eq!(outcome.moved, 4, "failures: {:?}", outcome.failures);
        assert!(
            !storage_root
                .join("storage.sqlite3-wal.startup-precheckpoint-20260625_120000_000")
                .exists()
        );
    }

    #[cfg(unix)]
    #[test]
    fn enumerate_and_consolidate_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let storage_root = dir.path();
        let db_path = storage_root.join("storage.sqlite3");
        std::fs::write(&db_path, b"live-db").unwrap();

        // Two forensic bundles, each with a nested file.
        let forensics = storage_root
            .join("doctor")
            .join("forensics")
            .join("storage.sqlite3");
        for bundle in ["repair-20260101_000000_000", "repair-20260102_000000_000"] {
            let b = forensics.join(bundle).join("sqlite");
            std::fs::create_dir_all(&b).unwrap();
            std::fs::write(b.join("storage.sqlite3"), vec![0_u8; 1000]).unwrap();
        }
        // Two quarantine files + a .bak that must be ignored.
        std::fs::write(
            storage_root.join("storage.sqlite3.corrupt-20260101_000000_000"),
            vec![0_u8; 500],
        )
        .unwrap();
        std::fs::write(
            storage_root.join("storage.sqlite3.corrupt-20260102_000000_000"),
            vec![0_u8; 500],
        )
        .unwrap();
        std::fs::write(storage_root.join("storage.sqlite3.bak"), b"backup").unwrap();

        let debris = enumerate_recovery_debris(storage_root, &db_path);
        // 2 bundles + 2 quarantines (the .bak and live DB are excluded).
        assert_eq!(debris.len(), 4, "debris: {debris:?}");
        let bundles = debris
            .iter()
            .filter(|a| a.category == DebrisCategory::ForensicBundle)
            .count();
        let quarantines = debris
            .iter()
            .filter(|a| a.category == DebrisCategory::CorruptQuarantine)
            .count();
        assert_eq!(bundles, 2);
        assert_eq!(quarantines, 2);
        // each bundle includes the 1000-byte nested file
        assert!(
            debris
                .iter()
                .filter(|a| a.category == DebrisCategory::ForensicBundle)
                .all(|a| a.bytes >= 1000)
        );

        // Reclaim everything (keep_min=0, max_age=0) by consolidating into a dest.
        let plan = select_recovery_debris_to_reclaim(
            debris,
            RetentionPolicy {
                keep_min: 0,
                max_age_secs: 0,
                max_total_bytes_per_category: None,
            },
            i64::MAX,
        );
        assert_eq!(plan.prune.len(), 4);
        let dest = storage_root
            .join("doctor")
            .join("reclaimable")
            .join("run-x");
        let outcome = consolidate_debris(&plan, &dest).unwrap();
        assert_eq!(outcome.moved, 4, "failures: {:?}", outcome.failures);
        assert_eq!(outcome.failures, [] as [(PathBuf, String); 0]);
        // The live DB and .bak survive; quarantines are gone from the DB dir.
        assert!(db_path.exists());
        assert!(storage_root.join("storage.sqlite3.bak").exists());
        assert!(
            !storage_root
                .join("storage.sqlite3.corrupt-20260101_000000_000")
                .exists()
        );
        // Consolidated copies landed in the dest.
        assert!(
            dest.join("storage.sqlite3.corrupt-20260101_000000_000")
                .exists()
        );
        assert_eq!(
            reclaimable_staging_bytes(storage_root),
            3_000,
            "move-only consolidation remains resident until an operator removes the staging dir"
        );
    }

    #[cfg(unix)]
    #[test]
    fn consolidate_refuses_to_reuse_an_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("storage.sqlite3.corrupt-incident");
        std::fs::write(&source, b"new evidence").unwrap();
        let dest = dir.path().join("doctor").join("reclaimable").join("run");
        std::fs::create_dir_all(&dest).unwrap();
        let sentinel = dest.join("storage.sqlite3.corrupt-incident");
        std::fs::write(&sentinel, b"prior evidence").unwrap();
        let plan = ReclaimPlan {
            prune: vec![DebrisArtifact {
                path: source.clone(),
                bytes: 12,
                modified_us: 1,
                category: DebrisCategory::CorruptQuarantine,
            }],
            kept_count: 0,
            total_count: 1,
            reclaimable_bytes: 12,
            total_bytes: 12,
        };

        let error = consolidate_debris(&plan, &dest).expect_err("existing destination must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&source).unwrap(), b"new evidence");
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"prior evidence");
    }

    #[cfg(unix)]
    #[test]
    fn reclaim_directory_collision_preserves_both_forensic_bundles() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("bundle");
        let dest = root.path().join("quarantine");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&dest).unwrap();
        std::fs::write(source.join("evidence"), b"source bundle").unwrap();
        let mut attempts = 0;
        let moved = move_recovery_debris_with(
            &source,
            &dest,
            |from, to| {
                attempts += 1;
                if attempts == 1 {
                    std::fs::create_dir(to).unwrap();
                    std::fs::write(to.join("evidence"), b"raced bundle").unwrap();
                }
                crate::pool::rename_noreplace_preserving_source(from, to)
            },
            sync_reclaim_move_parents,
        )
        .unwrap();
        assert_eq!(attempts, 2);
        assert_eq!(moved, dest.join("bundle.1"));
        assert_eq!(std::fs::read(moved.join("evidence")).unwrap(), b"source bundle");
        assert_eq!(std::fs::read(dest.join("bundle/evidence")).unwrap(), b"raced bundle");
    }

    #[cfg(unix)]
    #[test]
    fn reclaim_preserves_dangling_destination_symlink() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("evidence");
        let dest = root.path().join("quarantine");
        std::fs::create_dir(&dest).unwrap();
        std::fs::write(&source, b"source").unwrap();
        std::os::unix::fs::symlink("absent", dest.join("evidence")).unwrap();
        let moved = move_recovery_debris(&source, &dest).unwrap();
        assert_eq!(moved, dest.join("evidence.1"));
        assert_eq!(std::fs::read_link(dest.join("evidence")).unwrap(), Path::new("absent"));
        assert_eq!(std::fs::read(moved).unwrap(), b"source");
    }

    #[cfg(unix)]
    #[test]
    fn reclaim_sync_failure_reports_the_retained_destination_without_retry() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("evidence");
        let dest = root.path().join("quarantine");
        std::fs::create_dir(&dest).unwrap();
        std::fs::write(&source, b"source evidence").unwrap();
        let mut renames = 0;
        let error = move_recovery_debris_with(
            &source,
            &dest,
            |from, to| {
                renames += 1;
                crate::pool::rename_noreplace_preserving_source(from, to)
            },
            |_, _| Err(std::io::Error::other("injected directory sync failure")),
        )
        .unwrap_err();
        assert_eq!(renames, 1);
        assert!(!source.exists());
        assert_eq!(std::fs::read(dest.join("evidence")).unwrap(), b"source evidence");
        let message = error.to_string();
        assert!(message.contains(&dest.join("evidence").display().to_string()));
        assert!(message.contains("durability is unconfirmed"));
        assert!(message.contains("do not retry or roll back"));
        assert_eq!(std::fs::read_dir(dest).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn reclaim_exhaustion_preserves_source_and_all_destinations() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("evidence");
        let dest = root.path().join("quarantine");
        std::fs::create_dir(&dest).unwrap();
        std::fs::write(&source, b"source").unwrap();
        for suffix in 0..MAX_RECLAIM_MOVE_ATTEMPTS {
            let name = if suffix == 0 {
                "evidence".to_string()
            } else {
                format!("evidence.{suffix}")
            };
            std::fs::write(dest.join(name), b"existing").unwrap();
        }
        let error = move_recovery_debris(&source, &dest).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(source).unwrap(), b"source");
        for entry in std::fs::read_dir(dest).unwrap() {
            assert_eq!(std::fs::read(entry.unwrap().path()).unwrap(), b"existing");
        }
    }

    #[cfg(unix)]
    #[test]
    fn reclaim_rejects_symlink_sources_and_destination_directory() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        let source = root.path().join("evidence");
        let dest = root.path().join("quarantine");
        std::fs::create_dir(&dest).unwrap();
        std::fs::write(&target, b"unrelated").unwrap();
        std::os::unix::fs::symlink(&target, &source).unwrap();
        assert!(move_recovery_debris(&source, &dest).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"unrelated");
        assert!(std::fs::symlink_metadata(source).unwrap().file_type().is_symlink());
        let linked_dest = root.path().join("linked-quarantine");
        std::os::unix::fs::symlink(&dest, &linked_dest).unwrap();
        assert!(move_recovery_debris(&target, &linked_dest).is_err());
        assert_eq!(std::fs::read(target).unwrap(), b"unrelated");
        assert_eq!(std::fs::read_dir(dest).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn reclaim_directories_are_private_when_created() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("doctor/reclaimable/run");
        create_private_reclaim_directory(&dest, true).unwrap();
        for path in [root.path().join("doctor"), root.path().join("doctor/reclaimable"), dest] {
            assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
        }
    }

    #[cfg(not(unix))]
    #[test]
    fn reclaim_without_supported_directory_sync_refuses_before_mutation() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("evidence");
        std::fs::write(&source, b"source").unwrap();
        let plan = ReclaimPlan {
            prune: vec![DebrisArtifact {
                path: source.clone(),
                bytes: 6,
                modified_us: 1,
                category: DebrisCategory::CorruptQuarantine,
            }],
            ..ReclaimPlan::default()
        };
        let dest = root.path().join("quarantine");
        let error = consolidate_debris(&plan, &dest).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
        assert_eq!(std::fs::read(source).unwrap(), b"source");
        assert!(!dest.exists());
        assert_eq!(consolidate_debris(&ReclaimPlan::default(), &dest).unwrap().moved, 0);
        assert!(!dest.exists());
    }

    #[test]
    fn recovery_words_in_database_basename_never_select_the_live_family() {
        use std::ffi::OsStr;
        for database in [
            "mail.corrupt-live.db",
            "mail.reconstruct-failed-live.db",
            "mail.archive-reconcile-live.db",
            "mail.startup-precheckpoint-live.db",
            "mail.startup-quarantine-live.db",
            "mail.stale",
            "mail.stale-live.db",
        ] {
            for suffix in [
                "", "-wal", "-shm", "-journal", "-wal-cert", "-wal-cert-head",
                "-fsqlite-ns-gate", "-fsqlite-ns-use", ".lock", ".bak", ".bak.meta.json",
            ] {
                let name = format!("{database}{suffix}");
                assert_eq!(
                    classify_recovery_debris_file(OsStr::new(database), OsStr::new(&name)),
                    None,
                    "live/control file must not enter reclaim: {name}"
                );
            }
        }
    }

    #[test]
    fn recovery_debris_requires_an_exact_family_and_complete_artifact_suffix() {
        use std::ffi::OsStr;
        for name in [
            "mail.db2.corrupt-incident",
            "mail.db-other.corrupt-incident",
            "mail.db.notes.corrupt-incident",
            "mail.db.bak.corrupt-incident",
            "mail.db-wal2.stale",
            "mail.db.corrupt-",
            "mail.db.reconstruct-failed-",
            "mail.db.archive-reconcile-",
            "mail.db.startup-quarantine-",
            "mail.db.stale-",
            "mail.db.stale.",
            "mail.db.staleness",
            "mail.db.CORRUPT-incident",
        ] {
            assert_eq!(
                classify_recovery_debris_file(OsStr::new("mail.db"), OsStr::new(name)),
                None,
                "unowned or incomplete spelling: {name}"
            );
        }
    }

    #[test]
    fn recovery_debris_classifies_known_companions_after_the_database_name() {
        use std::ffi::OsStr;
        for companion in [
            "", "-wal", "-shm", "-journal", "-wal-cert", "-wal-cert-head",
            "-fsqlite-ns-gate", "-fsqlite-ns-use", ".lock",
        ] {
            for (suffix, category) in [
                ("corrupt-incident", DebrisCategory::CorruptQuarantine),
                ("reconstruct-failed-incident", DebrisCategory::CorruptQuarantine),
                ("archive-reconcile-incident", DebrisCategory::ArchiveReconcileBackup),
                ("startup-precheckpoint-incident", DebrisCategory::SidecarSnapshot),
                ("startup-quarantine-incident", DebrisCategory::SidecarSnapshot),
                ("stale", DebrisCategory::StaleArtifact),
                ("stale-incident", DebrisCategory::StaleArtifact),
                ("stale.evidence", DebrisCategory::StaleArtifact),
            ] {
                let name = format!("mail.corrupt-live.db{companion}.{suffix}");
                assert_eq!(
                    classify_recovery_debris_file(
                        OsStr::new("mail.corrupt-live.db"),
                        OsStr::new(&name),
                    ),
                    Some(category),
                    "{name}"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn reclaim_inventory_cannot_move_a_live_database_with_recovery_words() {
        for database in ["mail.corrupt-live.db", "mail.archive-reconcile-live.db", "mail.stale"] {
            let root = tempfile::tempdir().unwrap().keep();
            let primary = root.join(database);
            let protected = [
                primary.clone(),
                root.join(format!("{database}-wal")),
                root.join(format!("{database}-shm")),
                root.join(format!("{database}.bak")),
                root.join(format!("{database}.bak.meta.json")),
                root.join(format!("{database}2.corrupt-neighbor")),
            ];
            for path in &protected {
                std::fs::write(path, b"protected mailbox state").unwrap();
            }
            let artifact = root.join(format!("{database}.corrupt-incident"));
            std::fs::write(&artifact, b"incident evidence").unwrap();
            let debris = enumerate_recovery_debris(&root, &primary);
            assert_eq!(debris.len(), 1, "unexpected reclaim inventory: {debris:?}");
            assert_eq!(debris[0].path, artifact);
            let plan = select_recovery_debris_to_reclaim(
                debris,
                RetentionPolicy {
                    keep_min: 0,
                    max_age_secs: 0,
                    max_total_bytes_per_category: Some(0),
                },
                i64::MAX,
            );
            let destination = root.join("doctor/reclaimable/scoped-run");
            let outcome = consolidate_debris(&plan, &destination).unwrap();
            assert_eq!(outcome.moved, 1, "{:?}", outcome.failures);
            assert!(outcome.failures.is_empty());
            for path in protected {
                assert_eq!(std::fs::read(&path).unwrap(), b"protected mailbox state");
            }
            assert_eq!(
                std::fs::read(destination.join(artifact.file_name().unwrap())).unwrap(),
                b"incident evidence"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn recovery_inventory_preserves_non_unicode_database_identity() {
        use std::os::unix::ffi::OsStringExt;
        let root = tempfile::tempdir().unwrap().keep();
        let name = std::ffi::OsString::from_vec(b"mail-\xff.corrupt-live.db".to_vec());
        let primary = root.join(&name);
        std::fs::write(&primary, b"live state").unwrap();
        let mut artifact_name = name.clone();
        artifact_name.push(".corrupt-incident");
        let artifact = root.join(&artifact_name);
        std::fs::write(&artifact, b"owned evidence").unwrap();
        let alias = root.join("mail-�.corrupt-live.db.corrupt-incident");
        std::fs::write(&alias, b"different mailbox").unwrap();
        let debris = enumerate_corrupt_quarantines(&primary);
        assert_eq!(debris.len(), 1);
        assert_eq!(debris[0].path, artifact);
        assert_eq!(debris[0].category, DebrisCategory::CorruptQuarantine);
        assert_eq!(std::fs::read(&primary).unwrap(), b"live state");
        assert_eq!(std::fs::read(alias).unwrap(), b"different mailbox");
    }
}
