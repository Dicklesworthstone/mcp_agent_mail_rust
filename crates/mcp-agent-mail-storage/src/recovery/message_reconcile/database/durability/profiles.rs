//! Failed mutable profiles are obligations of one database generation and agent
//! incarnation. Recovery may publish a newer authoritative policy or retirement;
//! replaying the failed payload would instead overwrite that newer state.
//!
//! Only the complete non-secret production projection is eligible. The ledger
//! retains identifiers, an incarnation digest and permanent lifecycle evidence,
//! never profile text, configuration, credentials or a replayable payload.

use std::time::Instant;

use mcp_agent_mail_core::Config;
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use super::{Fingerprint, Scope, Ticket, Witness, WitnessKind, ledger};

const MAX_PROFILE_BYTES: usize = 128 * 1024;
const MAX_READY_PROFILES: usize = 16;
const PROFILE_FIELDS: [&str; 13] = [
    "id",
    "project_id",
    "name",
    "program",
    "model",
    "task_description",
    "inception_ts",
    "last_active_ts",
    "attachments_policy",
    "contact_policy",
    "reaper_exempt",
    "retired_at",
    "deregistered_at",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Binding {
    generation: String,
    deregistered_at: Option<i64>,
}

struct Projection {
    id: i64,
    digest: [u8; 32],
    deregistered_at: Option<i64>,
}

fn projection(profile: &Value) -> Option<Projection> {
    let fields = profile.as_object()?;
    // Credentials, response-only metadata and unknown extensions do not
    // acquire automatic failure-forgiveness authority.
    if fields.len() != PROFILE_FIELDS.len()
        || fields
            .keys()
            .any(|key| !PROFILE_FIELDS.contains(&key.as_str()))
    {
        return None;
    }
    let id = profile.get("id")?.as_i64().filter(|id| *id > 0)?;
    let project_id = profile.get("project_id")?.as_i64().filter(|id| *id > 0)?;
    let name = profile.get("name")?.as_str()?;
    crate::validate_archive_component("agent name", name).ok()?;
    for field in [
        "program",
        "model",
        "task_description",
        "attachments_policy",
        "contact_policy",
    ] {
        profile.get(field)?.as_str()?;
    }
    profile.get("reaper_exempt")?.as_bool()?;
    let inception = crate::agent_profile_publication_inception(profile).ok()??;
    crate::archive_publication_timestamp(profile.get("last_active_ts")?)?;
    for field in ["retired_at", "deregistered_at"] {
        let value = profile.get(field)?;
        if !value.is_null() {
            crate::archive_publication_timestamp(value)?;
        }
    }
    let deregistered_at = crate::agent_profile_publication_deregistration(profile).ok()?;
    // Bound even fields that a later authoritative update may supersede.
    let mut bounded = Fingerprint {
        hash: Sha256::new(),
        remaining: MAX_PROFILE_BYTES,
    };
    bounded.json(profile, 0)?;
    let mut identity = Fingerprint {
        hash: Sha256::new(),
        remaining: MAX_PROFILE_BYTES,
    };
    identity.feed(b"agent-mail-wbq-agent-incarnation-v1")?;
    identity.feed(&id.to_le_bytes())?;
    identity.feed(&project_id.to_le_bytes())?;
    identity.text(name)?;
    identity.feed(&inception.to_le_bytes())?;
    Some(Projection {
        id,
        digest: identity.hash.finalize().into(),
        deregistered_at,
    })
}

fn generation(config: &Config) -> Option<String> {
    Some(
        mcp_agent_mail_db::recovery_state::read_revision_witness(&config.storage_root)
            .ok()??
            .generation()
            .to_owned(),
    )
}

pub(super) fn witness(config: &Config, project: &str, profile: &Value) -> Option<Witness> {
    if project.len() > 1024 {
        return None;
    }
    crate::validate_archive_component("project slug", project).ok()?;
    let projected = projection(profile)?;
    Some(Witness {
        kind: WitnessKind::AgentProfile(Binding {
            generation: generation(config)?,
            deregistered_at: projected.deregistered_at,
        }),
        scope: Scope::capture(config)?,
        project: project.to_owned(),
        id: projected.id,
        digest: projected.digest,
    })
}

fn matches(
    witness: &Witness,
    scope: &Scope,
    generation: &str,
    project: &str,
    projected: &Projection,
) -> bool {
    let WitnessKind::AgentProfile(binding) = &witness.kind else {
        return false;
    };
    witness.scope == *scope
        && binding.generation == generation
        && witness.project == project
        && witness.id == projected.id
        && witness.digest == projected.digest
        // Reversible retirement, policy changes and heartbeat activity are
        // superseded by the verified live profile. Permanent deregistration
        // cannot be forgiven by a source that has lost its tombstone.
        && binding
            .deregistered_at
            .is_none_or(|at| projected.deregistered_at == Some(at))
}

/// Opaque sequence snapshots taken before the agent's publication/proof begins.
pub(crate) struct Tickets(Vec<Ticket>);

impl Tickets {
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

pub(crate) fn tickets_for(config: &Config, project: &str, profile: &Value) -> Tickets {
    let capture = || {
        let projected = projection(profile)?;
        let scope = Scope::capture(config)?;
        let generation = generation(config)?;
        Some(
            ledger()
                .entries
                .iter()
                .filter(|entry| matches(&entry.witness, &scope, &generation, project, &projected))
                .cloned()
                .collect(),
        )
    };
    if !crate::durability_degraded() || super::disk_is_critical() {
        return Tickets(Vec::new());
    }
    Tickets(capture().unwrap_or_default())
}

/// Check the complete current incarnation against the captured obligations.
/// The caller independently reads the generation from the writable live DB;
/// a same-inode restore cannot borrow the old archive manifest's authority.
pub(crate) fn matches_current(
    config: &Config,
    live_generation: &str,
    project: &str,
    profile: &Value,
    tickets: &Tickets,
) -> bool {
    let Some(projected) = projection(profile) else {
        return false;
    };
    let Some(scope) = Scope::capture(config) else {
        return false;
    };
    let Some(generation) = generation(config) else {
        return false;
    };
    live_generation == generation
        && !tickets.is_empty()
        && tickets
            .0
            .iter()
            .all(|ticket| matches(&ticket.witness, &scope, &generation, project, &projected))
}

/// Called only after the profile reconciler's source/working/Git verification.
/// A newer failure replaces its sequence, so an older proof cannot clear it.
pub(crate) fn complete(tickets: &Tickets) {
    for ticket in &tickets.0 {
        super::complete(ticket);
    }
}

pub(crate) fn postpone(tickets: &Tickets) {
    let now = Instant::now();
    let mut state = ledger();
    for ticket in &tickets.0 {
        state.postpone(ticket, now);
    }
}

/// Seed the existing bounded agent retry lane, without owning a second worker
/// or replaying a captured profile. The admission callback only touches its
/// in-memory cursor. Already queued retries keep their existing backoff and
/// do not consume this batch limit. Stable conflicts still pass through the
/// ordinary fresh-authority/fingerprint checks; reinjection backs off one minute.
pub(crate) fn seed_retries(config: &Config, mut admit: impl FnMut(i64) -> bool) {
    if !crate::durability_degraded() || super::disk_is_critical() {
        return;
    }
    let Some(scope) = Scope::capture(config) else {
        return;
    };
    let Some(generation) = generation(config) else {
        return;
    };
    let now = Instant::now();
    let mut state = ledger();
    let mut admitted = Vec::with_capacity(MAX_READY_PROFILES);
    for entry in &state.entries {
        if admitted.len() == MAX_READY_PROFILES {
            break;
        }
        let WitnessKind::AgentProfile(binding) = &entry.witness.kind else {
            continue;
        };
        if entry.witness.scope != scope
            || binding.generation != generation
            || entry.ready_at > now
            || admitted
                .iter()
                .any(|prior: &Ticket| prior.witness.id == entry.witness.id)
            || !admit(entry.witness.id)
        {
            continue;
        }
        admitted.push(entry.clone());
    }
    for ticket in admitted {
        state.postpone(&ticket, now);
    }
}
