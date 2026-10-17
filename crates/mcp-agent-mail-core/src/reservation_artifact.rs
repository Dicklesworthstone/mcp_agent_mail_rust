//! Generation-stable naming for file-reservation archive artifacts.
//!
//! # Why this exists (br-n8qh6)
//!
//! A file reservation is persisted to the git archive as a stable per-row
//! artifact under `projects/<slug>/file_reservations/`. Historically that
//! artifact was named `id-<id>.json`, where `<id>` is the reservation's global
//! `SQLite` `AUTOINCREMENT` rowid.
//!
//! That key is **not** stable across *database generations*. When a mailbox DB
//! is wiped and re-created (recovery, scratch re-init, a fresh `:memory:` in a
//! test), `AUTOINCREMENT` restarts at 1, so the new generation writes
//! `id-1.json`, `id-2.json`, … that collide with a previous generation's
//! artifacts — different reservations sharing the same on-disk name. Because the
//! global id is also reused across *projects* in the new generation, stale
//! artifacts left under old project directories present as cross-project id
//! collisions to the parity checker (csd forensics: 607 `archive_id_collision`)
//! and make reconstruct-from-archive lossy (the artifacts are keyed by a rowid
//! that no longer maps 1:1 to a live row).
//!
//! The fix keys the stable artifact by the composite `(db_generation_id, rowid)`
//! encoded in the filename: `id-<id>-g<generation>.json`. `<generation>` is a
//! per-physical-DB random hex token (see the `db_identity` table). A new DB
//! generation gets a fresh token, so its `id-1` artifact lands at a different
//! path than the prior generation's and can never overwrite or collide with it.
//!
//! Legacy `id-<id>.json` names (written before this change, or by a DB whose
//! `db_identity` row has not been seeded) are still recognized by
//! [`parse_reservation_artifact_filename`] with `generation == None`, so every
//! reader migrates transparently.

use std::path::{Path, PathBuf};

/// A parsed stable reservation artifact filename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedReservationArtifact {
    /// The reservation's `SQLite` rowid (always `> 0`).
    pub id: i64,
    /// The DB generation token this artifact was written by, or `None` for a
    /// legacy `id-<id>.json` artifact that predates generation stamping.
    pub generation: Option<String>,
}

/// Build the on-disk filename for a reservation's stable archive artifact.
///
/// With a non-empty `generation`, returns the generation-stamped
/// `id-<id>-g<generation>.json`; otherwise the legacy `id-<id>.json`. Callers
/// that lack a generation (an unseeded / legacy DB) fall back to the legacy
/// name, preserving prior behavior exactly.
#[must_use]
pub fn reservation_artifact_filename(generation: Option<&str>, id: i64) -> String {
    match generation {
        Some(generation) if !generation.is_empty() => format!("id-{id}-g{generation}.json"),
        _ => format!("id-{id}.json"),
    }
}

/// Parse a `file_reservations/` entry name into its `(id, generation)` parts.
///
/// Recognizes both the generation-stamped `id-<id>-g<generation>.json` form and
/// the legacy `id-<id>.json` form. Returns `None` for any name that is not a
/// well-formed stable reservation artifact (e.g. the legacy
/// `<sha1(pattern)>.json` digest artifact, or a hand-authored file). The id must
/// be a positive integer and, when present, the generation must be a non-empty
/// lowercase-hex token — the exact shape emitted by `lower(hex(randomblob(...)))`.
#[must_use]
pub fn parse_reservation_artifact_filename(name: &str) -> Option<ParsedReservationArtifact> {
    let stem = name.strip_prefix("id-")?.strip_suffix(".json")?;
    // A reservation rowid is pure digits and never contains '-', so the first
    // "-g" after the id is unambiguously the generation delimiter.
    if let Some((id_part, generation)) = stem.split_once("-g") {
        let id = id_part.parse::<i64>().ok()?;
        if id <= 0 || generation.is_empty() || !is_hex_token(generation) {
            return None;
        }
        Some(ParsedReservationArtifact {
            id,
            generation: Some(generation.to_string()),
        })
    } else {
        let id = stem.parse::<i64>().ok()?;
        if id <= 0 {
            return None;
        }
        Some(ParsedReservationArtifact {
            id,
            generation: None,
        })
    }
}

/// `true` when `s` is non-empty and every byte is an ASCII hex digit.
fn is_hex_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Locate the stable archive artifact for reservation `id` inside a
/// `file_reservations/` directory, matching either the generation-stamped
/// (`id-<id>-g<generation>.json`) or legacy (`id-<id>.json`) name.
///
/// A generation-stamped match is preferred over a legacy one, and among
/// generation-stamped matches the lexicographically smallest name is chosen for
/// determinism. Returns `None` when the directory is unreadable or holds no
/// matching artifact. Symlinked entries are ignored (never dereferenced), so a
/// reader can safely use the returned path.
#[must_use]
pub fn find_reservation_artifact(reservations_dir: &Path, id: i64) -> Option<PathBuf> {
    let mut legacy: Option<PathBuf> = None;
    let mut stamped: Option<PathBuf> = None;
    for_each_artifact_of_id(reservations_dir, id, |path, parsed| {
        if parsed.generation.is_some() {
            match &stamped {
                Some(existing) if existing <= &path => {}
                _ => stamped = Some(path),
            }
        } else if legacy.is_none() {
            legacy = Some(path);
        }
    })?;
    stamped.or(legacy)
}

/// Locate the stable archive artifact for reservation `id` **as attributed to
/// the live database generation** — the artifact the reservation parity
/// checker compares against the DB row (GH#311).
///
/// [`find_reservation_artifact`] is generation-blind: it prefers *any*
/// generation-stamped name, including debris left by a superseded DB
/// generation. The parity checker, by contrast, excludes foreign-generation
/// artifacts from comparison and reports drift against the current-generation
/// or legacy artifact. A fixer that mutates the artifact behind a parity
/// finding must therefore resolve the same file the checker did — otherwise
/// it rewrites prior-generation history and leaves the flagged drift in place.
///
/// Selection mirrors the checker's admission rule exactly:
///
/// - `current_generation == Some(g)`: the `id-<id>-g<g>.json` artifact wins;
///   otherwise the legacy `id-<id>.json`; otherwise `None`. A stamped artifact
///   with any *other* generation is never returned.
/// - `current_generation == None` (unseeded `db_identity`): generations cannot
///   be attributed, so a single stamped artifact wins over legacy (as in
///   [`find_reservation_artifact`]). When **several** stamped artifacts share
///   the id, the checker's pick is directory-order dependent and cannot be
///   reproduced safely, so this returns `None` — callers must treat that as
///   "skip explicitly", never guess.
///
/// The exact current-generation filename is checked directly, without walking
/// unrelated history (GH#362). Every call rechecks its file type; no cached path
/// becomes evidence. Legacy, missing, and unknown-generation lookups retain the
/// full scan, including older noncanonical numeric spellings. When both exact
/// and noncanonical current-generation spellings exist, the emitted exact name
/// wins deterministically rather than depending on directory iteration order.
///
/// Symlinked entries are ignored (never dereferenced), matching the other
/// finder.
#[must_use]
pub fn find_reservation_artifact_for_generation(
    reservations_dir: &Path,
    id: i64,
    current_generation: Option<&str>,
) -> Option<PathBuf> {
    let current_generation = current_generation.filter(|generation| !generation.is_empty());
    // Only validated tokens may enter a path; untrusted separators must never
    // turn a lookup into a probe outside this reservation directory.
    if id > 0
        && let Some(generation) = current_generation.filter(|value| is_hex_token(value))
    {
        let path = reservations_dir.join(reservation_artifact_filename(Some(generation), id));
        if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_file()) {
            return Some(path);
        }
    }
    scan_reservation_artifact_for_generation(reservations_dir, id, current_generation)
}

// The original complete selection remains the fallback and performance control.
// In particular, an unseeded generation cannot bypass ambiguity detection.
fn scan_reservation_artifact_for_generation(
    reservations_dir: &Path,
    id: i64,
    current_generation: Option<&str>,
) -> Option<PathBuf> {
    let mut legacy: Option<PathBuf> = None;
    let mut current: Option<PathBuf> = None;
    // Only consulted when the live generation is unknown.
    let mut unattributed_stamped: Vec<PathBuf> = Vec::new();
    for_each_artifact_of_id(reservations_dir, id, |path, parsed| {
        match (current_generation, parsed.generation.as_deref()) {
            (_, None) => {
                if legacy.is_none() {
                    legacy = Some(path);
                }
            }
            (Some(live), Some(stamped)) => {
                if stamped == live && current.is_none() {
                    current = Some(path);
                }
                // A differently-stamped artifact is prior-generation debris:
                // never a candidate.
            }
            (None, Some(_)) => unattributed_stamped.push(path),
        }
    })?;
    if current_generation.is_some() {
        return current.or(legacy);
    }
    match unattributed_stamped.len() {
        0 => legacy,
        1 => unattributed_stamped.pop(),
        _ => None,
    }
}

/// Visit every regular-file (non-symlink) stable artifact for reservation
/// `id` in `reservations_dir`. Returns `None` when the directory is unreadable
/// so callers can distinguish "no such artifact" from "could not look".
fn for_each_artifact_of_id(
    reservations_dir: &Path,
    id: i64,
    mut visit: impl FnMut(PathBuf, ParsedReservationArtifact),
) -> Option<()> {
    #[cfg(test)]
    tests::DIRECTORY_SCANS.with(|scans| scans.set(scans.get() + 1));
    let entries = std::fs::read_dir(reservations_dir).ok()?;
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() || file_type.is_symlink() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(parsed) = parse_reservation_artifact_filename(name) else {
            continue;
        };
        if parsed.id != id {
            continue;
        }
        visit(entry.path(), parsed);
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    std::thread_local! {
        pub(super) static DIRECTORY_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    #[test]
    fn exact_lookup_matches_scan_across_generation_and_artifact_combinations() {
        for mask in 0..32 {
            let dir = tempfile::tempdir().unwrap();
            for (bit, name) in [
                "id-5.json",
                "id-5-gaabb.json",
                "id-5-gccdd.json",
                "id-05-geeff.json",
                "id-8-gaabb.json",
            ]
            .into_iter()
            .enumerate()
            {
                if mask & (1 << bit) != 0 {
                    std::fs::write(dir.path().join(name), b"{}").unwrap();
                }
            }
            for id in [-1, 0, 5, 8, 99] {
                for generation in [
                    None,
                    Some(""),
                    Some("aabb"),
                    Some("ccdd"),
                    Some("eeff"),
                    Some("AABB"),
                    Some("a/../b"),
                ] {
                    let reference = scan_reservation_artifact_for_generation(
                        dir.path(),
                        id,
                        generation.filter(|value| !value.is_empty()),
                    );
                    assert_eq!(
                        find_reservation_artifact_for_generation(dir.path(), id, generation),
                        reference,
                        "mask={mask} id={id} generation={generation:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn exact_name_wins_over_noncanonical_numeric_alias_without_hiding_unknown_ambiguity() {
        let dir = tempfile::tempdir().unwrap();
        let exact = dir.path().join("id-7-gaabb.json");
        for name in ["id-0007-gaabb.json", "id-+7-gaabb.json", "id-7-gaabb.json"] {
            std::fs::write(dir.path().join(name), b"{}").unwrap();
        }
        assert_eq!(
            find_reservation_artifact_for_generation(dir.path(), 7, Some("aabb")),
            Some(exact)
        );
        assert!(find_reservation_artifact_for_generation(dir.path(), 7, None).is_none());
    }

    #[test]
    #[ignore = "explicit same-invocation real-directory lookup benchmark"]
    fn exact_generation_lookup_history_scaling() {
        use std::time::{Duration, Instant};
        for total in [1_000_i64, 16_500] {
            let dir = tempfile::tempdir().unwrap();
            for id in 1..=total {
                std::fs::write(
                    dir.path()
                        .join(reservation_artifact_filename(Some("aabb"), id)),
                    b"{}",
                )
                .unwrap();
            }
            let ids: Vec<_> = (0..50).map(|index| 1 + index * (total - 1) / 49).collect();
            let run = |optimized: bool, generation| {
                let start = Instant::now();
                let paths: Vec<_> = ids
                    .iter()
                    .map(|&id| {
                        if optimized {
                            find_reservation_artifact_for_generation(dir.path(), id, generation)
                        } else {
                            scan_reservation_artifact_for_generation(dir.path(), id, generation)
                        }
                    })
                    .collect();
                (start.elapsed(), paths)
            };
            let (_, expected) = run(false, Some("aabb"));
            assert_eq!(run(true, Some("aabb")).1, expected);
            let mut baseline = Vec::new();
            let mut candidate = Vec::new();
            for round in 0..7 {
                for optimized in [round % 2 == 0, round % 2 != 0] {
                    let (elapsed, paths) = run(optimized, Some("aabb"));
                    assert_eq!(paths, expected);
                    if optimized {
                        candidate.push(elapsed);
                    } else {
                        baseline.push(elapsed);
                    }
                }
            }
            baseline.sort_unstable();
            candidate.sort_unstable();
            let old = baseline[3];
            let new = candidate[3];
            eprintln!(
                "reservation lookup: entries={total} lookups=50 samples=7 scan_median_us={} exact_median_us={} ratio={:.2}",
                old.as_micros(),
                new.as_micros(),
                old.as_secs_f64() / new.as_secs_f64()
            );
            assert!(
                new.saturating_mul(10) < old,
                "exact lookup must eliminate the history-sized scan, not just marginally improve it"
            );
            // A miss still enumerates history. Record it explicitly; do not
            // infer the exact-hit improvement for legacy or absent artifacts.
            let (missing_old, paths) = run(false, Some("ccdd"));
            let (missing_new, optimized_paths) = run(true, Some("ccdd"));
            assert_eq!(paths, optimized_paths);
            assert!(paths.iter().all(Option::is_none));
            eprintln!(
                "reservation lookup miss: entries={total} lookups=50 scan_us={} fallback_us={}",
                missing_old.as_micros(),
                missing_new.as_micros()
            );
            assert!(old > Duration::ZERO && new > Duration::ZERO);
        }
    }

    #[test]
    fn known_generation_exact_artifact_does_not_enumerate_unrelated_history() {
        let dir = tempfile::tempdir().unwrap();
        for id in 1..=256 {
            std::fs::write(
                dir.path()
                    .join(reservation_artifact_filename(Some("aabb"), id)),
                b"{}",
            )
            .unwrap();
        }
        std::fs::write(dir.path().join("id-42.json"), b"legacy").unwrap();
        std::fs::write(dir.path().join("id-42-gcccc.json"), b"foreign").unwrap();
        DIRECTORY_SCANS.with(|scans| scans.set(0));
        for id in [1, 42, 256] {
            assert_eq!(
                find_reservation_artifact_for_generation(dir.path(), id, Some("aabb")),
                Some(
                    dir.path()
                        .join(reservation_artifact_filename(Some("aabb"), id))
                )
            );
        }
        assert_eq!(
            DIRECTORY_SCANS.with(std::cell::Cell::get),
            0,
            "a fully named current-generation artifact must not enumerate directory history"
        );
    }

    #[test]
    fn exact_lookup_observes_replacement_and_preserves_legacy_and_noncanonical_fallbacks() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("id-7-gab12.json");
        let legacy = dir.path().join("id-7.json");
        std::fs::write(&current, b"current").unwrap();
        std::fs::write(&legacy, b"legacy").unwrap();
        assert_eq!(
            find_reservation_artifact_for_generation(dir.path(), 7, Some("ab12")),
            Some(current.clone())
        );
        let retained = dir.path().join("retained-current");
        std::fs::rename(&current, &retained).unwrap();
        assert_eq!(
            find_reservation_artifact_for_generation(dir.path(), 7, Some("ab12")),
            Some(legacy.clone())
        );
        let padded = dir.path().join("id-0007-gab12.json");
        std::fs::rename(&retained, &padded).unwrap();
        assert_eq!(
            find_reservation_artifact_for_generation(dir.path(), 7, Some("ab12")),
            Some(padded)
        );
        std::fs::create_dir(&current).unwrap();
        assert_eq!(
            find_reservation_artifact_for_generation(dir.path(), 7, Some("ab12")),
            Some(dir.path().join("id-0007-gab12.json"))
        );
        for generation in ["../../outside", "ab/12", ".", "not-hex"] {
            assert_eq!(
                find_reservation_artifact_for_generation(dir.path(), 7, Some(generation)),
                Some(legacy.clone())
            );
        }
        assert!(find_reservation_artifact_for_generation(dir.path(), 0, Some("ab12")).is_none());
        assert!(find_reservation_artifact_for_generation(dir.path(), -7, Some("ab12")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn exact_lookup_never_follows_a_symlinked_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("outside.json");
        std::fs::write(&target, b"unrelated evidence").unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join("id-9-gaabb.json")).unwrap();
        let legacy = dir.path().join("id-9.json");
        std::fs::write(&legacy, b"legacy").unwrap();
        assert_eq!(
            find_reservation_artifact_for_generation(dir.path(), 9, Some("aabb")),
            Some(legacy)
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"unrelated evidence");
        std::os::unix::fs::symlink(
            outside.path().join("missing"),
            dir.path().join("id-10-gaabb.json"),
        )
        .unwrap();
        assert!(find_reservation_artifact_for_generation(dir.path(), 10, Some("aabb")).is_none());
    }

    #[test]
    fn filename_legacy_when_no_generation() {
        assert_eq!(reservation_artifact_filename(None, 42), "id-42.json");
        assert_eq!(reservation_artifact_filename(Some(""), 42), "id-42.json");
    }

    #[test]
    fn filename_stamped_with_generation() {
        assert_eq!(
            reservation_artifact_filename(Some("ab12cd"), 7),
            "id-7-gab12cd.json"
        );
    }

    #[test]
    fn roundtrip_generation_stamped() {
        let name = reservation_artifact_filename(Some("00ff9a"), 128);
        let parsed = parse_reservation_artifact_filename(&name).expect("parse");
        assert_eq!(parsed.id, 128);
        assert_eq!(parsed.generation.as_deref(), Some("00ff9a"));
    }

    #[test]
    fn roundtrip_legacy() {
        let name = reservation_artifact_filename(None, 9);
        assert_eq!(name, "id-9.json");
        let parsed = parse_reservation_artifact_filename(&name).expect("parse");
        assert_eq!(parsed.id, 9);
        assert_eq!(parsed.generation, None);
    }

    #[test]
    fn parse_rejects_non_artifacts() {
        // Legacy sha1(pattern) digest artifacts must not parse as id artifacts.
        assert!(
            parse_reservation_artifact_filename("da39a3ee5e6b4b0d3255bfef95601890afd80709.json")
                .is_none()
        );
        assert!(parse_reservation_artifact_filename("id-.json").is_none());
        assert!(parse_reservation_artifact_filename("id-abc.json").is_none());
        assert!(parse_reservation_artifact_filename("id-5.txt").is_none());
        assert!(parse_reservation_artifact_filename("reservation-5.json").is_none());
        assert!(parse_reservation_artifact_filename("id-0.json").is_none());
        assert!(parse_reservation_artifact_filename("id--5.json").is_none());
    }

    #[test]
    fn parse_rejects_malformed_generation() {
        // Empty generation.
        assert!(parse_reservation_artifact_filename("id-5-g.json").is_none());
        // Non-hex generation token.
        assert!(parse_reservation_artifact_filename("id-5-gxyz.json").is_none());
        // Non-numeric id with a generation.
        assert!(parse_reservation_artifact_filename("id-x-gab12.json").is_none());
        // Non-positive id with a generation.
        assert!(parse_reservation_artifact_filename("id-0-gab12.json").is_none());
    }

    #[test]
    fn find_prefers_generation_stamped_over_legacy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join("id-5.json"), "{}").expect("legacy");
        std::fs::write(root.join("id-5-gab12cd.json"), "{}").expect("stamped");
        // An unrelated id and a non-artifact must be ignored.
        std::fs::write(root.join("id-6.json"), "{}").expect("other id");
        std::fs::write(
            root.join("da39a3ee5e6b4b0d3255bfef95601890afd80709.json"),
            "{}",
        )
        .expect("sha1 digest artifact");

        let found = find_reservation_artifact(root, 5).expect("locate id 5");
        assert_eq!(
            found.file_name().and_then(|name| name.to_str()),
            Some("id-5-gab12cd.json"),
            "generation-stamped artifact must win over the legacy name"
        );
    }

    #[test]
    fn find_falls_back_to_legacy_when_no_stamped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join("id-9.json"), "{}").expect("legacy");
        let found = find_reservation_artifact(root, 9).expect("locate id 9");
        assert_eq!(
            found.file_name().and_then(|name| name.to_str()),
            Some("id-9.json")
        );
    }

    #[test]
    fn find_returns_none_for_missing_id_or_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(find_reservation_artifact(dir.path(), 404).is_none());
        assert!(find_reservation_artifact(&dir.path().join("does-not-exist"), 1).is_none());
    }

    fn file_name(path: &Path) -> &str {
        path.file_name().and_then(|name| name.to_str()).unwrap()
    }

    #[test]
    fn find_for_generation_ignores_foreign_stamped_and_picks_legacy() {
        // GH#311: a stale legacy artifact next to prior-generation debris. The
        // parity checker compares the legacy file against the live row, so the
        // generation-aware finder must return it — NOT the foreign artifact
        // that the generation-blind finder prefers.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let current = "a".repeat(64);
        let foreign = "b".repeat(64);
        std::fs::write(root.join("id-1.json"), "{}").expect("legacy");
        std::fs::write(root.join(format!("id-1-g{foreign}.json")), "{}").expect("foreign");

        let blind = find_reservation_artifact(root, 1).expect("blind");
        assert_eq!(file_name(&blind), format!("id-1-g{foreign}.json"));

        let aware =
            find_reservation_artifact_for_generation(root, 1, Some(&current)).expect("aware");
        assert_eq!(file_name(&aware), "id-1.json");
    }

    #[test]
    fn find_for_generation_prefers_current_stamped_over_legacy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join("id-5.json"), "{}").expect("legacy");
        std::fs::write(root.join("id-5-gaaaa.json"), "{}").expect("current");
        std::fs::write(root.join("id-5-gbbbb.json"), "{}").expect("foreign");
        let found = find_reservation_artifact_for_generation(root, 5, Some("aaaa")).expect("find");
        assert_eq!(file_name(&found), "id-5-gaaaa.json");
    }

    #[test]
    fn find_for_generation_returns_none_when_only_foreign_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join("id-7-gbbbb.json"), "{}").expect("foreign");
        assert!(find_reservation_artifact_for_generation(root, 7, Some("aaaa")).is_none());
        // ...and an empty generation token is treated as unknown, not as a
        // literal generation, matching `reservation_artifact_filename`.
        assert_eq!(
            find_reservation_artifact_for_generation(root, 7, Some(""))
                .map(|p| file_name(&p).to_owned()),
            Some("id-7-gbbbb.json".to_owned())
        );
    }

    #[test]
    fn find_for_generation_without_live_generation_matches_blind_finder_when_unambiguous() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join("id-9.json"), "{}").expect("legacy");
        // Legacy only → legacy.
        let found = find_reservation_artifact_for_generation(root, 9, None).expect("legacy");
        assert_eq!(file_name(&found), "id-9.json");
        // One stamped + legacy → the stamped one (checker admits both, stamped wins).
        std::fs::write(root.join("id-9-gcccc.json"), "{}").expect("stamped");
        let found = find_reservation_artifact_for_generation(root, 9, None).expect("stamped");
        assert_eq!(file_name(&found), "id-9-gcccc.json");
    }

    #[test]
    fn find_for_generation_without_live_generation_refuses_ambiguous_stamped_set() {
        // Two stamped artifacts and no live generation: the checker's pick is
        // directory-order dependent, so the finder must refuse rather than guess.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join("id-3.json"), "{}").expect("legacy");
        std::fs::write(root.join("id-3-gaaaa.json"), "{}").expect("stamped a");
        std::fs::write(root.join("id-3-gbbbb.json"), "{}").expect("stamped b");
        assert!(find_reservation_artifact_for_generation(root, 3, None).is_none());
        // With the live generation known, the ambiguity disappears.
        let found = find_reservation_artifact_for_generation(root, 3, Some("bbbb")).expect("b");
        assert_eq!(file_name(&found), "id-3-gbbbb.json");
    }

    #[test]
    fn find_for_generation_returns_none_for_missing_id_or_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(find_reservation_artifact_for_generation(dir.path(), 404, Some("aaaa")).is_none());
        assert!(
            find_reservation_artifact_for_generation(&dir.path().join("missing"), 1, None)
                .is_none()
        );
    }

    #[test]
    fn parse_accepts_full_width_generation_token() {
        // 32 lowercase-hex chars, the shape of lower(hex(randomblob(16))).
        let generation = "0123456789abcdef0123456789abcdef";
        let name = reservation_artifact_filename(Some(generation), 314);
        let parsed = parse_reservation_artifact_filename(&name).expect("parse");
        assert_eq!(parsed.id, 314);
        assert_eq!(parsed.generation.as_deref(), Some(generation));
    }
}
