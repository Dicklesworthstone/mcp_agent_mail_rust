//! Offline known-bad / obsolete `am` (and `mcp-agent-mail`) binary-version
//! classification — J2 (`br-bvq1x.10.2`, Track J).
//!
//! ## Why
//!
//! Agents reproduced ALREADY-FIXED bugs because the running (or PATH-resolved)
//! binary was an older version. The css incident is the canonical example: a
//! systemd service kept running `am 0.3.12`, which predates both the ATC
//! row-ceiling and the bind-before-recovery fix, so it grew `atc_experiences`
//! to 2.41 GB and wedged startup — even though the source was already fixed.
//!
//! A hard, OFFLINE drift warning breaks that loop: it does not depend on a
//! network update check (those are best-effort and were not configured on the
//! affected host). The known-bad ranges are bundled into the binary, so the
//! warning fires the moment a stale/known-bad build runs `am doctor check` /
//! `am health`.
//!
//! ## Design
//!
//! Deliberately mirrors the proven git-binary catalog in
//! [`crate::git_binary`]: a data-driven JSON catalog
//! (`data/known_bad_am_versions.json`) compiled in via `include_str!`, an
//! optional per-operator extension at `AM_EXTRA_KNOWN_BAD_AM_JSON`, and a
//! suppress list at `AM_IGNORE_KNOWN_BAD_AM`. The generic
//! [`KnownBadSeverity`](crate::git_binary::KnownBadSeverity) enum is reused; the
//! entry/matcher types are `am`-specific only because they match an
//! [`AmVersion`] rather than a `GitVersion`.

#![forbid(unsafe_code)]

use crate::git_binary::KnownBadSeverity;
use std::sync::OnceLock;

/// Canonical, copy-paste install/upgrade command for `am`.
///
/// Carried verbatim in every drift warning so an operator (or agent) can repair
/// without guessing. Offline-printable; running it is the operator's next step.
pub const AM_INSTALL_REPAIR_COMMAND: &str =
    "curl -LsSf https://raw.githubusercontent.com/Dicklesworthstone/mcp_agent_mail_rust/main/install.sh | bash";

/// Parsed semver triple for an `am` binary. Lightweight (no `semver` dep) — we
/// only need ordered-tuple comparison for the known-bad ranges. Mirrors
/// [`crate::git_binary::GitVersion`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AmVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl AmVersion {
    #[must_use]
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// The version of THIS binary (the running classifier), from
    /// `CARGO_PKG_VERSION`. This is the workspace version and serves as the
    /// "latest-locally-known" reference for obsolescence checks. Always valid
    /// semver at build time, so the lax parse never fails in practice; falls
    /// back to `0.0.0` defensively if it somehow does.
    #[must_use]
    pub fn current() -> Self {
        Self::parse_lax(env!("CARGO_PKG_VERSION")).unwrap_or(Self::new(0, 0, 0))
    }

    /// Lex-ordered tuple for range comparisons.
    #[must_use]
    pub const fn as_tuple(self) -> (u32, u32, u32) {
        (self.major, self.minor, self.patch)
    }

    /// Is this version known-bad (after applying the `AM_IGNORE_KNOWN_BAD_AM`
    /// suppress list)? Call [`match_known_bad_am`] for the structured entry.
    #[must_use]
    pub fn is_known_bad(self) -> bool {
        match_known_bad_am(self).is_some()
    }

    /// Parse `"X.Y.Z"` tolerating `-rc1`/`+build`/trailing suffixes. Returns
    /// `None` if there are fewer than two dot-separated integer segments.
    #[must_use]
    pub fn parse_lax(s: &str) -> Option<Self> {
        let head = s.trim().trim_start_matches('v');
        let head = head.split_terminator(['-', '+', ' ']).next().unwrap_or(head);
        let mut it = head.split('.');
        let major = it.next()?.parse().ok()?;
        let minor = it.next()?.parse().ok()?;
        let patch = it.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        Some(Self::new(major, minor, patch))
    }
}

impl std::fmt::Display for AmVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// One catalog entry as loaded from the embedded JSON or a user override.
/// Schema-identical to [`crate::git_binary::KnownBadEntry`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AmKnownBadEntry {
    pub code: String,
    #[serde(rename = "match")]
    pub matcher: AmKnownBadMatcher,
    pub severity: KnownBadSeverity,
    pub summary: String,
    #[serde(default)]
    pub fingerprint: Option<String>,
    pub remediation_ref: String,
}

/// How an entry matches a version.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum AmKnownBadMatcher {
    /// Single exact match.
    Exact { version: String },
    /// `[min, max_exclusive)` half-open range.
    Range { min: String, max_exclusive: String },
}

impl AmKnownBadMatcher {
    fn matches(&self, v: AmVersion) -> bool {
        match self {
            Self::Exact { version } => {
                matches!(AmVersion::parse_lax(version), Some(target) if target == v)
            }
            Self::Range { min, max_exclusive } => {
                let Some(lo) = AmVersion::parse_lax(min) else {
                    return false;
                };
                let Some(hi) = AmVersion::parse_lax(max_exclusive) else {
                    return false;
                };
                v.as_tuple() >= lo.as_tuple() && v.as_tuple() < hi.as_tuple()
            }
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
struct AmKnownBadFile {
    entries: Vec<AmKnownBadEntry>,
}

/// Embedded default catalog, compiled in at build time.
const EMBEDDED_AM_CATALOG: &str = include_str!("../data/known_bad_am_versions.json");

static AM_CATALOG_CACHE: OnceLock<Vec<AmKnownBadEntry>> = OnceLock::new();

fn load_am_catalog() -> &'static [AmKnownBadEntry] {
    AM_CATALOG_CACHE.get_or_init(|| {
        let mut out: Vec<AmKnownBadEntry> = Vec::new();

        match serde_json::from_str::<AmKnownBadFile>(EMBEDDED_AM_CATALOG) {
            Ok(file) => out.extend(file.entries),
            Err(e) => tracing::error!(
                target: "mcp_agent_mail::am_version",
                err = %e,
                "known_bad_am_embedded_parse_failed"
            ),
        }

        // Optionally merge user extensions. User entries with the same `code`
        // override defaults; new codes are appended.
        if let Ok(user_path) = std::env::var("AM_EXTRA_KNOWN_BAD_AM_JSON")
            && !user_path.trim().is_empty()
        {
            match std::fs::read_to_string(&user_path) {
                Ok(text) => match serde_json::from_str::<AmKnownBadFile>(&text) {
                    Ok(file) => {
                        for entry in file.entries {
                            if let Some(existing) = out.iter_mut().find(|e| e.code == entry.code) {
                                tracing::info!(
                                    target: "mcp_agent_mail::am_version",
                                    code = %entry.code,
                                    path = %user_path,
                                    "known_bad_am_user_override"
                                );
                                *existing = entry;
                            } else {
                                out.push(entry);
                            }
                        }
                    }
                    Err(e) => tracing::error!(
                        target: "mcp_agent_mail::am_version",
                        err = %e,
                        path = %user_path,
                        "known_bad_am_user_parse_failed"
                    ),
                },
                Err(e) => tracing::warn!(
                    target: "mcp_agent_mail::am_version",
                    err = %e,
                    path = %user_path,
                    "known_bad_am_user_file_unreadable"
                ),
            }
        }

        tracing::info!(
            target: "mcp_agent_mail::am_version",
            entries = out.len(),
            "known_bad_am_catalog_loaded"
        );
        out
    })
}

fn am_suppress_list() -> &'static [String] {
    static SUPPRESS: OnceLock<Vec<String>> = OnceLock::new();
    SUPPRESS.get_or_init(|| {
        std::env::var("AM_IGNORE_KNOWN_BAD_AM")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    })
}

/// Return the catalog entry (if any) that matches `version` and is not
/// suppressed by `AM_IGNORE_KNOWN_BAD_AM`. Cached after first call.
#[must_use]
pub fn match_known_bad_am(version: AmVersion) -> Option<&'static AmKnownBadEntry> {
    let suppress = am_suppress_list();
    load_am_catalog()
        .iter()
        .find(|entry| !suppress.iter().any(|c| c == &entry.code) && entry.matcher.matches(version))
}

/// The full (post-suppression) catalog, for reflection surfaces.
#[must_use]
pub fn known_bad_am_versions() -> Vec<&'static AmKnownBadEntry> {
    let suppress = am_suppress_list();
    load_am_catalog()
        .iter()
        .filter(|e| !suppress.iter().any(|c| c == &e.code))
        .collect()
}

/// Offline classification of an `am` binary version.
///
/// Compares `installed` against `latest_known` (typically
/// [`AmVersion::current`]). Serializes as a tagged `state` so the doctor/robot
/// health surfaces can render it directly.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AmVersionVerdict {
    /// Up to date and not known-bad — nothing to do.
    Current,
    /// Older than the latest-locally-known version, but not in a known-bad
    /// range. A soft drift warning.
    Obsolete { latest_known: String },
    /// In a bundled known-bad range — emit loudly with the repair command.
    KnownBad {
        code: String,
        severity: KnownBadSeverity,
        summary: String,
        remediation_ref: String,
    },
}

impl AmVersionVerdict {
    /// Whether this verdict warrants surfacing a warning (anything but
    /// [`Current`](Self::Current)).
    #[must_use]
    pub const fn is_actionable(&self) -> bool {
        !matches!(self, Self::Current)
    }

    /// A short status label for compact (toon) surfaces.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Obsolete { .. } => "obsolete",
            Self::KnownBad { .. } => "known_bad",
        }
    }
}

/// Classify `installed` against the known-bad catalog and `latest_known`.
///
/// A known-bad match takes precedence over plain obsolescence (it is the more
/// actionable, higher-severity signal). This is fully offline.
#[must_use]
pub fn classify_am_version(installed: AmVersion, latest_known: AmVersion) -> AmVersionVerdict {
    if let Some(entry) = match_known_bad_am(installed) {
        return AmVersionVerdict::KnownBad {
            code: entry.code.clone(),
            severity: entry.severity,
            summary: entry.summary.clone(),
            remediation_ref: entry.remediation_ref.clone(),
        };
    }
    if installed.as_tuple() < latest_known.as_tuple() {
        return AmVersionVerdict::Obsolete {
            latest_known: latest_known.to_string(),
        };
    }
    AmVersionVerdict::Current
}

/// Build the full operator-facing drift finding as a JSON object.
///
/// Carries path + versions + repair command + verdict, or `None` when the
/// binary is current. Reused by both the `am robot health` and `am doctor
/// check` runtime-identity surfaces so the warning text is identical everywhere.
#[must_use]
pub fn drift_warning_json(
    installed: AmVersion,
    latest_known: AmVersion,
    binary_path: &str,
) -> Option<serde_json::Value> {
    let verdict = classify_am_version(installed, latest_known);
    if !verdict.is_actionable() {
        return None;
    }
    Some(serde_json::json!({
        "state": verdict.label(),
        "binary_path": binary_path,
        "installed_version": installed.to_string(),
        "latest_known_version": latest_known.to_string(),
        "repair_command": AM_INSTALL_REPAIR_COMMAND,
        "verdict": verdict,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lax_handles_plain_and_decorated() {
        assert_eq!(AmVersion::parse_lax("0.3.13"), Some(AmVersion::new(0, 3, 13)));
        assert_eq!(
            AmVersion::parse_lax("v0.3.12"),
            Some(AmVersion::new(0, 3, 12))
        );
        assert_eq!(
            AmVersion::parse_lax("0.3.13-rc1"),
            Some(AmVersion::new(0, 3, 13))
        );
        assert_eq!(
            AmVersion::parse_lax("am 0.3.13"),
            None,
            "leading non-numeric segment does not parse"
        );
        assert_eq!(AmVersion::parse_lax("0.3"), Some(AmVersion::new(0, 3, 0)));
        assert_eq!(AmVersion::parse_lax("garbage"), None);
    }

    #[test]
    fn embedded_catalog_parses_and_is_nonempty() {
        let entries = known_bad_am_versions();
        assert!(
            !entries.is_empty(),
            "embedded known-bad am catalog must load"
        );
    }

    #[test]
    fn pre_0_3_13_is_known_bad_offline() {
        // The css incident versions: anything < 0.3.13 is known-bad.
        for v in [
            AmVersion::new(0, 3, 12),
            AmVersion::new(0, 3, 7),
            AmVersion::new(0, 1, 0),
            AmVersion::new(0, 0, 0),
        ] {
            assert!(v.is_known_bad(), "{v} should be known-bad");
            let entry = match_known_bad_am(v).expect("match");
            assert_eq!(entry.code, "AM_PRE_0_3_13_ATC_BLOAT_STARTUP_WEDGE");
            assert_eq!(entry.severity, KnownBadSeverity::Fail);
        }
    }

    #[test]
    fn fixed_versions_are_not_known_bad() {
        for v in [
            AmVersion::new(0, 3, 13),
            AmVersion::new(0, 3, 14),
            AmVersion::new(0, 4, 0),
            AmVersion::new(1, 0, 0),
        ] {
            assert!(!v.is_known_bad(), "{v} should be clean");
            assert!(match_known_bad_am(v).is_none());
        }
    }

    #[test]
    fn classify_prefers_known_bad_over_obsolete() {
        let latest = AmVersion::new(0, 4, 0);
        // 0.3.12 is both obsolete vs 0.4.0 AND known-bad → known-bad wins.
        let verdict = classify_am_version(AmVersion::new(0, 3, 12), latest);
        match &verdict {
            AmVersionVerdict::KnownBad { code, severity, .. } => {
                assert_eq!(code, "AM_PRE_0_3_13_ATC_BLOAT_STARTUP_WEDGE");
                assert_eq!(*severity, KnownBadSeverity::Fail);
            }
            other => panic!("expected known_bad, got {other:?}"),
        }
        assert!(verdict.is_actionable());
        assert_eq!(verdict.label(), "known_bad");
    }

    #[test]
    fn classify_obsolete_when_clean_but_behind() {
        let verdict = classify_am_version(AmVersion::new(0, 3, 13), AmVersion::new(0, 4, 0));
        assert_eq!(
            verdict,
            AmVersionVerdict::Obsolete {
                latest_known: "0.4.0".to_string()
            }
        );
        assert!(verdict.is_actionable());
    }

    #[test]
    fn classify_current_when_at_or_ahead_of_latest() {
        assert_eq!(
            classify_am_version(AmVersion::new(0, 4, 0), AmVersion::new(0, 4, 0)),
            AmVersionVerdict::Current
        );
        // Ahead of latest-known (e.g. a dev build) is still "current".
        assert_eq!(
            classify_am_version(AmVersion::new(0, 5, 0), AmVersion::new(0, 4, 0)),
            AmVersionVerdict::Current
        );
    }

    #[test]
    fn drift_warning_json_carries_path_versions_and_repair_command() {
        let warning = drift_warning_json(
            AmVersion::new(0, 3, 12),
            AmVersion::new(0, 4, 0),
            "/home/u/.local/bin/am",
        )
        .expect("known-bad version yields a warning");
        assert_eq!(warning["state"], "known_bad");
        assert_eq!(warning["installed_version"], "0.3.12");
        assert_eq!(warning["latest_known_version"], "0.4.0");
        assert_eq!(warning["binary_path"], "/home/u/.local/bin/am");
        assert_eq!(warning["repair_command"], AM_INSTALL_REPAIR_COMMAND);
        assert!(
            warning["repair_command"]
                .as_str()
                .unwrap()
                .contains("install.sh")
        );
    }

    #[test]
    fn drift_warning_json_none_when_current() {
        assert!(
            drift_warning_json(
                AmVersion::new(0, 4, 0),
                AmVersion::new(0, 4, 0),
                "/home/u/.local/bin/am",
            )
            .is_none()
        );
    }
}
