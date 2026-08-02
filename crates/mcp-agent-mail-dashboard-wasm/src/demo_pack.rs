//! Versioned, privacy-bounded data pack and deterministic replay actions.

use sha2::{Digest, Sha256};

use crate::state::{ConfigSnapshot, RequestCounters, TuiSharedState};
use crate::tui_events::{
    AgentSummary, ContactSummary, DbStatSnapshot, EventSource, MailEvent, ProjectSummary,
    ReservationSnapshot,
};

pub const DEMO_PACK_SCHEMA_V1: &str = "agent_mail.demo_pack.v1";
pub const PUBLIC_PRIVACY_POLICY_V1: &str = "agent-mail-dashboard-public-demo-v1";
/// Hard ceiling on the serialized pack, enforced BEFORE deserialization so a
/// hostile pack cannot drive allocation. Public so the exporter, native
/// runner, and website loader all share one number.
pub const MAX_SERIALIZED_PACK_BYTES: usize = 8 * 1024 * 1024;
/// Hard ceiling on any single text field inside a pack (titles, labels,
/// console lines, event strings). Shared across all loaders.
pub const MAX_TEXT_FIELD_BYTES: usize = 4 * 1024;
const MAX_ACTIONS: usize = 10_000;
/// Bound synchronous work at any single replay instant. The curated opening
/// frame intentionally applies 192 history events at t=0, so retain generous
/// headroom without allowing an entire 10,000-action pack to block one frame.
const MAX_ACTIONS_PER_TIMESTAMP: usize = 512;
const MAX_DURATION_MS: u64 = 30 * 60 * 1_000;
const MAX_SPARKLINE_SAMPLES: usize = 240;
// Match the native reference's populated roster/contact rails. Timed snapshots
// below omit these repeated detail vectors and merge them from bootstrap.
const STARTUP_HISTORY_EVENTS: u64 = 192;
const STARTUP_AGENT_ROWS: usize = 500;
const STARTUP_PROJECT_ROWS: usize = 41;
const STARTUP_CONTACT_ROWS: usize = 200;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DemoProvenance {
    pub source_label: String,
    pub captured_at: String,
    pub source_revision: String,
    pub privacy_policy: String,
    pub content_sha256: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DemoBootstrap {
    pub db_stats: DbStatSnapshot,
    pub requests: RequestCounters,
    pub latency_samples_ms: Vec<f64>,
    pub console_lines: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct DemoAction {
    pub at_ms: u64,
    #[serde(flatten)]
    pub operation: DemoOperation,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DemoOperation {
    PublishEvent {
        event: MailEvent,
    },
    SetDbStats {
        snapshot: DbStatSnapshot,
    },
    /// Replace scalar/reservation data while inheriting omitted heavyweight
    /// roster, project, and contact detail vectors from the prior snapshot.
    MergeDbStats {
        snapshot: DbStatSnapshot,
    },
    RecordRequest {
        status: u16,
        duration_ms: u64,
    },
    ConsoleLine {
        text: String,
    },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DemoPack {
    pub schema: String,
    pub title: String,
    pub replay_label: String,
    pub duration_ms: u64,
    pub loop_replay: bool,
    pub provenance: DemoProvenance,
    pub bootstrap: DemoBootstrap,
    pub actions: Vec<DemoAction>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DemoPackError {
    Json(String),
    OversizedPack {
        len: usize,
        max: usize,
    },
    OversizedField {
        field: String,
        len: usize,
        max: usize,
    },
    InvalidDigest {
        digest: String,
    },
    UnsupportedSchema(String),
    EmptyMetadata(&'static str),
    InvalidDuration(u64),
    TooManyActions(usize),
    TooManyActionsAtTimestamp {
        at_ms: u64,
        count: usize,
        max: usize,
    },
    TooManySparklineSamples(usize),
    NonFiniteLatency(usize),
    NonMonotonicAction {
        index: usize,
        previous_ms: u64,
        at_ms: u64,
    },
    ActionPastDuration {
        index: usize,
        at_ms: u64,
        duration_ms: u64,
    },
    InvalidHttpStatus {
        index: usize,
        status: u16,
    },
    InvalidRequestCounters {
        total: u64,
        classified: Option<u64>,
        latency_total_ms: u64,
        reason: &'static str,
    },
    UnredactedEvent(usize),
    UnsafeText {
        field: String,
        reason: String,
    },
    DigestMismatch {
        expected: String,
        actual: String,
    },
    UnknownField(String),
}

impl std::fmt::Display for DemoPackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for DemoPackError {}

impl DemoPack {
    /// Minimal inert value used only while the host fetches and verifies the
    /// external public pack. It avoids generating the full built-in demo in
    /// `DashboardRunnerCore::new()` and is replaced before initialization.
    #[must_use]
    #[cfg(feature = "browser-dashboard")]
    pub(crate) fn unloaded_runner_placeholder() -> Self {
        Self {
            schema: DEMO_PACK_SCHEMA_V1.to_string(),
            title: "Agent Mail browser runner".to_string(),
            replay_label: "awaiting verified public pack".to_string(),
            duration_ms: 1,
            loop_replay: false,
            provenance: DemoProvenance {
                source_label: "aggregate counts pending; details synthetic".to_string(),
                captured_at: "1970-01-01T00:00:00Z".to_string(),
                source_revision: "unloaded".to_string(),
                privacy_policy: PUBLIC_PRIVACY_POLICY_V1.to_string(),
                content_sha256: String::new(),
            },
            bootstrap: DemoBootstrap {
                db_stats: DbStatSnapshot::default(),
                requests: RequestCounters::default(),
                latency_samples_ms: Vec::new(),
                console_lines: Vec::new(),
            },
            actions: Vec::new(),
        }
    }

    pub fn from_json(json: &str) -> Result<Self, DemoPackError> {
        // Size gate BEFORE parsing: nothing about a pack larger than the
        // contract ceiling is worth allocating for.
        if json.len() > MAX_SERIALIZED_PACK_BYTES {
            return Err(DemoPackError::OversizedPack {
                len: json.len(),
                max: MAX_SERIALIZED_PACK_BYTES,
            });
        }
        let raw: serde_json::Value =
            serde_json::from_str(json).map_err(|error| DemoPackError::Json(error.to_string()))?;
        let pack: Self = serde_json::from_value(raw.clone())
            .map_err(|error| DemoPackError::Json(error.to_string()))?;
        // The content digest is computed from the typed, canonical pack. Any
        // member silently ignored during deserialization would otherwise be
        // absent from both validation and the digest while remaining present
        // in the public JSON response. Compare the complete raw shape against
        // the typed re-serialization as a defense-in-depth backstop for every
        // nested schema, including flattened actions and external structs.
        let typed =
            serde_json::to_value(&pack).map_err(|error| DemoPackError::Json(error.to_string()))?;
        reject_unknown_json_members(&raw, &typed, "$")?;
        pack.validate()?;
        Ok(pack)
    }

    pub fn to_pretty_json(&self) -> Result<String, DemoPackError> {
        serde_json::to_string_pretty(self).map_err(|error| DemoPackError::Json(error.to_string()))
    }

    #[must_use]
    pub fn computed_content_sha256(&self) -> String {
        let mut canonical = self.clone();
        canonical.provenance.content_sha256.clear();
        let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
        hex::encode(Sha256::digest(bytes))
    }

    pub fn finalize_digest(&mut self) {
        self.provenance.content_sha256 = self.computed_content_sha256();
    }

    pub fn validate(&self) -> Result<(), DemoPackError> {
        if self.schema != DEMO_PACK_SCHEMA_V1 {
            return Err(DemoPackError::UnsupportedSchema(self.schema.clone()));
        }
        for (name, value) in [
            ("title", self.title.as_str()),
            ("replay_label", self.replay_label.as_str()),
            ("source_label", self.provenance.source_label.as_str()),
            ("captured_at", self.provenance.captured_at.as_str()),
            ("source_revision", self.provenance.source_revision.as_str()),
            ("privacy_policy", self.provenance.privacy_policy.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(DemoPackError::EmptyMetadata(name));
            }
            validate_public_text(name, value)?;
        }
        if self.provenance.privacy_policy != PUBLIC_PRIVACY_POLICY_V1 {
            return Err(DemoPackError::UnsafeText {
                field: "privacy_policy".to_string(),
                reason: "public pack must use the approved privacy policy".to_string(),
            });
        }
        if !self.provenance.source_label.contains("aggregate counts")
            || !self.provenance.source_label.contains("details synthetic")
        {
            return Err(DemoPackError::UnsafeText {
                field: "source_label".to_string(),
                reason: "public pack must distinguish aggregate counts from synthetic details"
                    .to_string(),
            });
        }
        if self.duration_ms == 0 || self.duration_ms > MAX_DURATION_MS {
            return Err(DemoPackError::InvalidDuration(self.duration_ms));
        }
        if self.actions.len() > MAX_ACTIONS {
            return Err(DemoPackError::TooManyActions(self.actions.len()));
        }
        if self.bootstrap.latency_samples_ms.len() > MAX_SPARKLINE_SAMPLES {
            return Err(DemoPackError::TooManySparklineSamples(
                self.bootstrap.latency_samples_ms.len(),
            ));
        }
        for (index, latency) in self.bootstrap.latency_samples_ms.iter().enumerate() {
            if !latency.is_finite() || *latency < 0.0 {
                return Err(DemoPackError::NonFiniteLatency(index));
            }
        }
        validate_request_counters(self.bootstrap.requests)?;
        validate_snapshot(&self.bootstrap.db_stats)?;
        for (index, line) in self.bootstrap.console_lines.iter().enumerate() {
            validate_public_text(&format!("bootstrap.console_lines[{index}]"), line)?;
        }
        let mut previous_ms = 0;
        let mut actions_at_timestamp = 0_usize;
        for (index, action) in self.actions.iter().enumerate() {
            if index > 0 && action.at_ms < previous_ms {
                return Err(DemoPackError::NonMonotonicAction {
                    index,
                    previous_ms,
                    at_ms: action.at_ms,
                });
            }
            if action.at_ms > self.duration_ms {
                return Err(DemoPackError::ActionPastDuration {
                    index,
                    at_ms: action.at_ms,
                    duration_ms: self.duration_ms,
                });
            }
            if index == 0 || action.at_ms != previous_ms {
                actions_at_timestamp = 1;
            } else {
                actions_at_timestamp = actions_at_timestamp.saturating_add(1);
            }
            if actions_at_timestamp > MAX_ACTIONS_PER_TIMESTAMP {
                return Err(DemoPackError::TooManyActionsAtTimestamp {
                    at_ms: action.at_ms,
                    count: actions_at_timestamp,
                    max: MAX_ACTIONS_PER_TIMESTAMP,
                });
            }
            match &action.operation {
                DemoOperation::PublishEvent { event } => validate_event(index, event)?,
                DemoOperation::SetDbStats { snapshot }
                | DemoOperation::MergeDbStats { snapshot } => validate_snapshot(snapshot)?,
                DemoOperation::RecordRequest { status, .. } => {
                    if !(100..=599).contains(status) {
                        return Err(DemoPackError::InvalidHttpStatus {
                            index,
                            status: *status,
                        });
                    }
                }
                DemoOperation::ConsoleLine { text } => {
                    validate_public_text(&format!("actions[{index}].text"), text)?;
                }
            }
            previous_ms = action.at_ms;
        }
        // The public website contract requires the provenance digest, so an
        // absent or malformed digest is a validation failure, not a skip: an
        // empty digest previously bypassed integrity verification entirely.
        let digest = &self.provenance.content_sha256;
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(DemoPackError::InvalidDigest {
                digest: digest.clone(),
            });
        }
        let actual = self.computed_content_sha256();
        if *digest != actual {
            return Err(DemoPackError::DigestMismatch {
                expected: digest.clone(),
                actual,
            });
        }
        Ok(())
    }

    pub fn apply_bootstrap(&self, state: &TuiSharedState) {
        state.reset();
        state.set_config(ConfigSnapshot::default());
        state.update_db_stats(self.bootstrap.db_stats.clone());
        state.set_request_counters(self.bootstrap.requests);
        state.set_sparkline(self.bootstrap.latency_samples_ms.clone());
        for line in &self.bootstrap.console_lines {
            state.push_console_log(line.clone());
        }
    }

    pub fn apply_action(&self, index: usize, state: &TuiSharedState) {
        let Some(action) = self.actions.get(index) else {
            return;
        };
        match &action.operation {
            DemoOperation::PublishEvent { event } => {
                let _ = state.push_event(event.clone());
            }
            DemoOperation::SetDbStats { snapshot } => state.update_db_stats(snapshot.clone()),
            DemoOperation::MergeDbStats { snapshot } => {
                let mut merged = snapshot.clone();
                if let Some(current) = state.db_stats_snapshot() {
                    if merged.agents_list.is_empty() {
                        merged.agents_list = current.agents_list;
                    }
                    if merged.projects_list.is_empty() {
                        merged.projects_list = current.projects_list;
                    }
                    if merged.contacts_list.is_empty() {
                        merged.contacts_list = current.contacts_list;
                    }
                }
                state.update_db_stats(merged);
            }
            DemoOperation::RecordRequest {
                status,
                duration_ms,
            } => {
                state.record_request(*status, *duration_ms);
            }
            DemoOperation::ConsoleLine { text } => state.push_console_log(text.clone()),
        }
    }
}

fn validate_snapshot(snapshot: &DbStatSnapshot) -> Result<(), DemoPackError> {
    for (index, agent) in snapshot.agents_list.iter().enumerate() {
        validate_public_text(&format!("agents[{index}].project"), &agent.project)?;
        validate_public_text(&format!("agents[{index}].name"), &agent.name)?;
        validate_public_text(&format!("agents[{index}].program"), &agent.program)?;
        validate_public_text(&format!("agents[{index}].model"), &agent.model)?;
    }
    for (index, project) in snapshot.projects_list.iter().enumerate() {
        validate_public_text(&format!("projects[{index}].slug"), &project.slug)?;
        validate_public_path(&format!("projects[{index}].human_key"), &project.human_key)?;
    }
    for (index, contact) in snapshot.contacts_list.iter().enumerate() {
        validate_public_text(
            &format!("contacts[{index}].from_agent"),
            &contact.from_agent,
        )?;
        validate_public_text(&format!("contacts[{index}].to_agent"), &contact.to_agent)?;
        validate_public_text(
            &format!("contacts[{index}].from_project_slug"),
            &contact.from_project_slug,
        )?;
        validate_public_text(
            &format!("contacts[{index}].to_project_slug"),
            &contact.to_project_slug,
        )?;
        validate_public_text(&format!("contacts[{index}].status"), &contact.status)?;
        validate_public_text(&format!("contacts[{index}].reason"), &contact.reason)?;
    }
    for (index, reservation) in snapshot.reservation_snapshots.iter().enumerate() {
        validate_public_text(
            &format!("reservations[{index}].project_slug"),
            &reservation.project_slug,
        )?;
        validate_public_text(
            &format!("reservations[{index}].agent_name"),
            &reservation.agent_name,
        )?;
        validate_public_path(
            &format!("reservations[{index}].path_pattern"),
            &reservation.path_pattern,
        )?;
    }
    Ok(())
}

fn validate_request_counters(counters: RequestCounters) -> Result<(), DemoPackError> {
    let classified = counters
        .status_2xx
        .checked_add(counters.status_4xx)
        .and_then(|count| count.checked_add(counters.status_5xx));
    let Some(classified) = classified else {
        return Err(DemoPackError::InvalidRequestCounters {
            total: counters.total,
            classified: None,
            latency_total_ms: counters.latency_total_ms,
            reason: "status bucket sum overflowed",
        });
    };
    if classified > counters.total {
        return Err(DemoPackError::InvalidRequestCounters {
            total: counters.total,
            classified: Some(classified),
            latency_total_ms: counters.latency_total_ms,
            reason: "classified status buckets exceed total requests",
        });
    }
    if counters.total == 0 && counters.latency_total_ms != 0 {
        return Err(DemoPackError::InvalidRequestCounters {
            total: counters.total,
            classified: Some(classified),
            latency_total_ms: counters.latency_total_ms,
            reason: "zero requests cannot carry aggregate latency",
        });
    }
    Ok(())
}

fn reject_unknown_json_members(
    raw: &serde_json::Value,
    typed: &serde_json::Value,
    path: &str,
) -> Result<(), DemoPackError> {
    match (raw, typed) {
        (serde_json::Value::Object(raw_fields), serde_json::Value::Object(typed_fields)) => {
            for (key, raw_value) in raw_fields {
                let field_path = format!("{path}.{key}");
                let Some(typed_value) = typed_fields.get(key) else {
                    return Err(DemoPackError::UnknownField(field_path));
                };
                reject_unknown_json_members(raw_value, typed_value, &field_path)?;
            }
        }
        (serde_json::Value::Array(raw_items), serde_json::Value::Array(typed_items)) => {
            for (index, (raw_value, typed_value)) in
                raw_items.iter().zip(typed_items.iter()).enumerate()
            {
                reject_unknown_json_members(raw_value, typed_value, &format!("{path}[{index}]"))?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_event(index: usize, event: &MailEvent) -> Result<(), DemoPackError> {
    if !event.redacted() {
        return Err(DemoPackError::UnredactedEvent(index));
    }
    let encoded =
        serde_json::to_value(event).map_err(|error| DemoPackError::Json(error.to_string()))?;
    validate_json_strings(&format!("actions[{index}].event"), &encoded)
}

fn validate_json_strings(field: &str, value: &serde_json::Value) -> Result<(), DemoPackError> {
    match value {
        serde_json::Value::String(text) => validate_public_text(field, text),
        serde_json::Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                validate_json_strings(&format!("{field}[{index}]"), value)?;
            }
            Ok(())
        }
        serde_json::Value::Object(values) => {
            for (key, value) in values {
                let nested_field = format!("{field}.{key}");
                if matches!(key.as_str(), "path" | "path_pattern")
                    && let serde_json::Value::String(path) = value
                {
                    validate_public_path(&nested_field, path)?;
                    continue;
                }
                if key == "paths"
                    && let serde_json::Value::Array(paths) = value
                {
                    for (index, path) in paths.iter().enumerate() {
                        if let serde_json::Value::String(path) = path {
                            validate_public_path(&format!("{nested_field}[{index}]"), path)?;
                        } else {
                            validate_json_strings(&format!("{nested_field}[{index}]"), path)?;
                        }
                    }
                    continue;
                }
                validate_json_strings(&nested_field, value)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn validate_public_path(field: &str, value: &str) -> Result<(), DemoPackError> {
    // Reject every representation a traversal can hide in, not just the
    // embedded `../` form: absolute paths, backslash separators, Windows
    // drive prefixes (`C:/...` has no leading slash), percent escapes (which
    // could decode into `.`/`/` at a consumer that URL-decodes), and any
    // `..`/`.`/empty path segment — which covers terminal `..`, `x/..`,
    // `a//b`, and trailing-slash forms in one rule.
    let has_drive_prefix = value.len() >= 2 && value.as_bytes()[1] == b':';
    if value.starts_with('/')
        || value.contains('\\')
        || value.contains('%')
        || has_drive_prefix
        || value
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(DemoPackError::UnsafeText {
            field: field.to_string(),
            reason: "path must be project-relative and traversal-free".to_string(),
        });
    }
    validate_public_text(field, value)
}

fn validate_public_text(field: &str, value: &str) -> Result<(), DemoPackError> {
    if value.len() > MAX_TEXT_FIELD_BYTES {
        return Err(DemoPackError::OversizedField {
            field: field.to_string(),
            len: value.len(),
            max: MAX_TEXT_FIELD_BYTES,
        });
    }
    let lower = value.to_ascii_lowercase();
    let forbidden = [
        ("/users/", "absolute macOS home path"),
        ("/home/", "absolute Unix home path"),
        ("c:\\users\\", "absolute Windows home path"),
        ("authorization:", "authorization header"),
        ("bearer ", "bearer credential"),
        ("ghp_", "GitHub token"),
        ("sk-", "API-key-like token"),
        ("password=", "password assignment"),
        ("api_key=", "API key assignment"),
    ];
    if let Some((_, reason)) = forbidden
        .iter()
        .find(|(pattern, _)| lower.contains(pattern))
    {
        return Err(DemoPackError::UnsafeText {
            field: field.to_string(),
            reason: (*reason).to_string(),
        });
    }
    if value.contains('\0') || value.len() > 32_768 {
        return Err(DemoPackError::UnsafeText {
            field: field.to_string(),
            reason: "NUL byte or excessive text length".to_string(),
        });
    }
    Ok(())
}

fn synthetic_startup_history(base_ts: i64) -> Vec<DemoAction> {
    const AGENTS: [&str; 16] = [
        "AmberDeer",
        "RubyPrairie",
        "GrayElk",
        "RedHarbor",
        "CoralDog",
        "BrownGlacier",
        "WindyLynx",
        "JadePine",
        "SilverCove",
        "BlueMeadow",
        "IvoryPeak",
        "GoldenReef",
        "VioletBrook",
        "CopperField",
        "TealFalcon",
        "CrimsonLake",
    ];
    const PROJECTS: [&str; 8] = [
        "mcp-agent-mail-rust",
        "frankentui",
        "frankensqlite",
        "agent-tooling",
        "edge-runtime",
        "skills-library",
        "release-automation",
        "browser-runtime",
    ];
    const SUBJECTS: [&str; 12] = [
        "Dashboard parity review is ready",
        "Reservation handoff completed",
        "Browser frame hash verified",
        "Release validation passed",
        "Shared renderer integration update",
        "Replay fixture privacy review",
        "Responsive layout test results",
        "Mailbox migration checkpoint",
        "Coordination thread follow-up",
        "Performance trace summary",
        "Cross-project dependency resolved",
        "Fresh-eyes review findings",
    ];
    const PATHS: [&str; 8] = [
        "crates/dashboard/**",
        "crates/ftui-web/**",
        "crates/storage/**",
        "docs/browser-dashboard.md",
        "components/terminal/**",
        "tests/replay/**",
        "release/**",
        "scripts/verification/**",
    ];
    const MESSAGE_BODIES: [&str; 4] = [
        "Parity review complete.\n\nThe browser frame now uses the production tab chrome and DashboardScreen. Mouse hit regions are derived from the rendered layout, so resizing cannot desynchronize clicks.",
        "Fresh-eyes pass found the input queue waiting on the replay clock.\n\nPointer events now wake the runner on the next animation frame while the deterministic replay remains throttled.",
        "Release candidate is ready for verification.\n\nPlease compare the 220-column terminal buffer, then check Dashboard filters, screen tabs, wheel scrolling, and fullscreen restoration.",
        "Reservation handoff confirmed.\n\nThe public replay remains read-only: aggregate counts come from the snapshot exporter while all names, paths, messages, and event details are synthetic.",
    ];
    const TOOL_NAMES: [&str; 4] = [
        "send_message",
        "file_reservation",
        "search_messages",
        "health_check",
    ];
    const HTTP_PATHS: [&str; 4] = ["api/mcp", "api/messages", "api/agents", "health/ready"];

    (0_u64..STARTUP_HISTORY_EVENTS)
        .map(|index| {
            let actor_index = usize::try_from(index).unwrap_or_default() % AGENTS.len();
            let peer_index = (actor_index + 3) % AGENTS.len();
            let project_index = usize::try_from(index / 3).unwrap_or_default() % PROJECTS.len();
            let subject_index = usize::try_from(index).unwrap_or_default() % SUBJECTS.len();
            let timestamp_micros = base_ts
                - i64::try_from(STARTUP_HISTORY_EVENTS.saturating_sub(index)).unwrap_or_default()
                    * 750_000;
            let seq = 20_000 + index;
            let id = 30_000 + i64::try_from(index).unwrap_or_default();
            let event_slot = index % 16;
            let paired_index = if matches!(event_slot, 5 | 11) {
                index.saturating_sub(1)
            } else {
                index
            };
            let paired_actor_index =
                usize::try_from(paired_index).unwrap_or_default() % AGENTS.len();
            let paired_project_index =
                usize::try_from(paired_index / 3).unwrap_or_default() % PROJECTS.len();
            let tool_index =
                usize::try_from(paired_index / 2).unwrap_or_default() % TOOL_NAMES.len();
            let operation = match event_slot {
                0 => DemoOperation::PublishEvent {
                    event: MailEvent::AgentRegistered {
                        seq,
                        timestamp_micros,
                        source: EventSource::Lifecycle,
                        redacted: true,
                        name: AGENTS[actor_index].to_string(),
                        program: if index.is_multiple_of(3) {
                            "claude-code".to_string()
                        } else {
                            "codex-cli".to_string()
                        },
                        model_name: if index.is_multiple_of(3) {
                            "opus".to_string()
                        } else {
                            "gpt-5".to_string()
                        },
                        project: PROJECTS[project_index].to_string(),
                    },
                },
                1 | 13 => DemoOperation::PublishEvent {
                    event: MailEvent::ReservationGranted {
                        seq,
                        timestamp_micros,
                        source: EventSource::Reservations,
                        redacted: true,
                        agent: AGENTS[actor_index].to_string(),
                        paths: vec![PATHS[project_index].to_string()],
                        exclusive: !index.is_multiple_of(3),
                        ttl_s: 7_200,
                        project: PROJECTS[project_index].to_string(),
                    },
                },
                2 | 8 | 14 => DemoOperation::PublishEvent {
                    event: MailEvent::MessageReceived {
                        seq,
                        timestamp_micros,
                        source: EventSource::Mail,
                        redacted: true,
                        id,
                        from: AGENTS[actor_index].to_string(),
                        to: vec![AGENTS[peer_index].to_string()],
                        subject: SUBJECTS[subject_index].to_string(),
                        thread_id: format!("public-coordination-{}", index % 12),
                        project: PROJECTS[project_index].to_string(),
                        body_md: MESSAGE_BODIES[subject_index % MESSAGE_BODIES.len()].to_string(),
                    },
                },
                3 | 9 | 15 => DemoOperation::PublishEvent {
                    event: MailEvent::MessageSent {
                        seq,
                        timestamp_micros,
                        source: EventSource::Mail,
                        redacted: true,
                        id,
                        from: AGENTS[actor_index].to_string(),
                        to: vec![AGENTS[peer_index].to_string()],
                        subject: SUBJECTS[subject_index].to_string(),
                        thread_id: format!("public-coordination-{}", index % 12),
                        project: PROJECTS[project_index].to_string(),
                        body_md: MESSAGE_BODIES[subject_index % MESSAGE_BODIES.len()].to_string(),
                    },
                },
                4 | 10 => DemoOperation::PublishEvent {
                    event: MailEvent::ToolCallStart {
                        seq,
                        timestamp_micros,
                        source: EventSource::Tooling,
                        redacted: true,
                        tool_name: TOOL_NAMES[tool_index].to_string(),
                        params_json: serde_json::json!({
                            "mode": "synthetic_public_replay",
                            "scope": PROJECTS[paired_project_index],
                        }),
                        project: Some(PROJECTS[paired_project_index].to_string()),
                        agent: Some(AGENTS[paired_actor_index].to_string()),
                    },
                },
                5 | 11 => DemoOperation::PublishEvent {
                    event: MailEvent::ToolCallEnd {
                        seq,
                        timestamp_micros,
                        source: EventSource::Tooling,
                        redacted: true,
                        tool_name: TOOL_NAMES[tool_index].to_string(),
                        duration_ms: 24 + (index % 9) * 13,
                        result_preview: Some("synthetic public result".to_string()),
                        queries: 1 + index % 4,
                        query_time_ms: 4.0
                            + f64::from(u32::try_from(index % 7).unwrap_or_default()) * 2.5,
                        per_table: vec![
                            ("messages".to_string(), 1 + index % 12),
                            ("agents".to_string(), 1),
                        ],
                        project: Some(PROJECTS[paired_project_index].to_string()),
                        agent: Some(AGENTS[paired_actor_index].to_string()),
                    },
                },
                6 | 12 => DemoOperation::PublishEvent {
                    event: MailEvent::HttpRequest {
                        seq,
                        timestamp_micros,
                        source: EventSource::Http,
                        redacted: true,
                        method: if index.is_multiple_of(4) {
                            "POST"
                        } else {
                            "GET"
                        }
                        .to_string(),
                        path: HTTP_PATHS
                            [usize::try_from(index).unwrap_or_default() % HTTP_PATHS.len()]
                        .to_string(),
                        status: match index % 5 {
                            0 => 503,
                            1 => 409,
                            2 => 202,
                            _ => 200,
                        },
                        duration_ms: 18 + (index % 11) * 17,
                        client_ip: "synthetic-public-client".to_string(),
                    },
                },
                7 => DemoOperation::PublishEvent {
                    event: MailEvent::GitSegfaultRetry {
                        seq,
                        timestamp_micros,
                        source: EventSource::Tooling,
                        redacted: true,
                        name: "git_status".to_string(),
                        repo_slug: PROJECTS[project_index].to_string(),
                        attempt_n: 1 + u32::try_from(index % 3).unwrap_or_default(),
                        signal: Some(11),
                        exhausted: index % 64 == 55,
                    },
                },
                _ => unreachable!("event slot is modulo 16"),
            };
            DemoAction {
                at_ms: 0,
                operation,
            }
        })
        .collect()
}

/// Curated public replay using production-scale aggregate shapes and fully
/// synthetic message/path details. No mailbox database is embedded.
#[must_use]
pub fn curated_public_demo() -> DemoPack {
    const BASE_TS: i64 = 1_772_668_800_000_000;
    let agent_templates = [
        ("AmberDeer", "codex-cli", "gpt-5", "mcp-agent-mail-rust"),
        ("RubyPrairie", "claude-code", "opus", "frankentui"),
        ("GrayElk", "codex-cli", "gpt-5", "mcp-agent-mail-rust"),
        ("RedHarbor", "cursor", "composer", "frankensqlite"),
        ("CoralDog", "codex-cli", "gpt-5", "agent-tooling"),
        ("BrownGlacier", "claude-code", "sonnet", "frankentui"),
        ("WindyLynx", "codex-cli", "gpt-5", "edge-runtime"),
        ("JadePine", "claude-code", "opus", "skills-library"),
        ("SilverCove", "codex-cli", "gpt-5", "release-automation"),
        ("BlueMeadow", "claude-code", "sonnet", "browser-runtime"),
        ("IvoryPeak", "codex-cli", "gpt-5", "mcp-agent-mail-rust"),
        ("GoldenReef", "claude-code", "opus", "frankentui"),
        ("VioletBrook", "cursor", "composer", "agent-tooling"),
        ("CopperField", "codex-cli", "gpt-5", "frankensqlite"),
        ("TealFalcon", "claude-code", "sonnet", "edge-runtime"),
        ("CrimsonLake", "codex-cli", "gpt-5", "browser-runtime"),
    ];
    let agents = (0..STARTUP_AGENT_ROWS)
        .map(|index| {
            let (name, program, model, project) = agent_templates[index % agent_templates.len()];
            let cycle = index / agent_templates.len();
            AgentSummary {
                project: project.to_string(),
                name: if cycle == 0 {
                    name.to_string()
                } else {
                    format!("{name}{cycle:02}")
                },
                program: program.to_string(),
                model: model.to_string(),
                last_active_ts: BASE_TS - i64::try_from(index).unwrap_or(0) * 7_000_000,
                health: None,
            }
        })
        .collect::<Vec<_>>();

    let project_templates = [
        (1, "mcp-agent-mail-rust", 1_825, 3),
        (2, "frankentui", 1_971, 2),
        (3, "frankensqlite", 1_123, 1),
        (4, "agent-tooling", 523, 0),
        (5, "edge-runtime", 243, 0),
        (6, "skills-library", 139, 0),
        (7, "release-automation", 318, 2),
        (8, "browser-runtime", 476, 2),
        (9, "observability", 284, 1),
        (10, "integration-tests", 197, 1),
    ];
    let projects = (0..STARTUP_PROJECT_ROWS)
        .map(|index| {
            let (slug, message_count, reservation_count) = project_templates
                .get(index)
                .map(|(_, slug, messages, reservations)| {
                    ((*slug).to_string(), *messages, *reservations)
                })
                .unwrap_or_else(|| {
                    (
                        format!("public-project-{:02}", index + 1),
                        180 + u64::try_from((index * 137) % 3_600).unwrap_or_default(),
                        u64::try_from(index % 4).unwrap_or_default(),
                    )
                });
            ProjectSummary {
                id: 1 + i64::try_from(index).unwrap_or_default(),
                human_key: format!("public-demo/{slug}"),
                slug,
                agent_count: 8 + u64::try_from(index % 37).unwrap_or_default(),
                message_count,
                reservation_count,
                created_at: BASE_TS
                    - (1 + i64::try_from(index).unwrap_or_default()) * 86_400_000_000,
            }
        })
        .collect::<Vec<_>>();

    let reservation_rows = [
        ("mcp-agent-mail-rust", "AmberDeer", "crates/dashboard/**"),
        ("frankentui", "RubyPrairie", "crates/ftui-web/**"),
        (
            "mcp-agent-mail-rust",
            "GrayElk",
            "docs/browser-dashboard.md",
        ),
        ("frankensqlite", "RedHarbor", "crates/storage/**"),
        ("agent-tooling", "CoralDog", "crates/coordination/**"),
        ("frankentui", "BrownGlacier", "crates/renderer/**"),
        ("edge-runtime", "WindyLynx", "crates/browser/**"),
        ("skills-library", "JadePine", "skills/agent-mail/**"),
        ("release-automation", "SilverCove", "release/**"),
        ("browser-runtime", "BlueMeadow", "tests/replay/**"),
    ];
    let reservations = reservation_rows
        .into_iter()
        .enumerate()
        .map(|(index, (project, agent, path))| ReservationSnapshot {
            id: 101 + i64::try_from(index).unwrap_or_default(),
            project_slug: project.to_string(),
            agent_name: agent.to_string(),
            path_pattern: path.to_string(),
            exclusive: !index.is_multiple_of(3),
            granted_ts: BASE_TS
                - (40_000_000 + i64::try_from(index).unwrap_or_default() * 3_000_000),
            expires_ts: BASE_TS
                + (3_600_000_000 + i64::try_from(index).unwrap_or_default() * 240_000_000),
            released_ts: None,
        })
        .collect::<Vec<_>>();

    let contact_templates = [
        (
            "AmberDeer",
            "RubyPrairie",
            "mcp-agent-mail-rust",
            "frankentui",
        ),
        (
            "GrayElk",
            "RedHarbor",
            "mcp-agent-mail-rust",
            "frankensqlite",
        ),
        ("CoralDog", "BrownGlacier", "agent-tooling", "frankentui"),
        ("WindyLynx", "JadePine", "edge-runtime", "skills-library"),
        (
            "SilverCove",
            "BlueMeadow",
            "release-automation",
            "browser-runtime",
        ),
        (
            "IvoryPeak",
            "GoldenReef",
            "mcp-agent-mail-rust",
            "frankentui",
        ),
        (
            "VioletBrook",
            "CopperField",
            "agent-tooling",
            "frankensqlite",
        ),
        (
            "TealFalcon",
            "CrimsonLake",
            "edge-runtime",
            "browser-runtime",
        ),
    ];
    let contacts = (0..STARTUP_CONTACT_ROWS)
        .map(|index| {
            let (from, to, from_project, to_project) =
                contact_templates[index % contact_templates.len()];
            ContactSummary {
                from_agent: from.to_string(),
                to_agent: to.to_string(),
                from_project_slug: from_project.to_string(),
                to_project_slug: to_project.to_string(),
                status: "approved".to_string(),
                reason: "synthetic public demo coordination".to_string(),
                updated_ts: BASE_TS - i64::try_from(index).unwrap_or_default() * 4_000_000,
                expires_ts: None,
            }
        })
        .collect::<Vec<_>>();

    let base_stats = DbStatSnapshot {
        projects: 10,
        agents: 1_550,
        messages: 1_825,
        file_reservations: 10,
        contact_links: 64,
        ack_pending: 28,
        agents_list: agents,
        projects_list: projects,
        contacts_list: contacts,
        reservation_snapshots: reservations,
        timestamp_micros: BASE_TS,
    };

    let mut stats_after_message = base_stats.clone();
    stats_after_message.messages += 1;
    stats_after_message.ack_pending += 1;
    stats_after_message.timestamp_micros = BASE_TS + 2_000_000;

    let mut stats_after_ack = stats_after_message.clone();
    stats_after_ack.ack_pending = stats_after_ack.ack_pending.saturating_sub(1);
    stats_after_ack.timestamp_micros = BASE_TS + 6_000_000;

    let mut stats_after_release = stats_after_ack.clone();
    stats_after_release.file_reservations = stats_after_ack.file_reservations.saturating_sub(1);
    if let Some(reservation) = stats_after_release.reservation_snapshots.get_mut(1) {
        reservation.released_ts = Some(BASE_TS + 10_000_000);
    }
    stats_after_release.timestamp_micros = BASE_TS + 10_000_000;

    // Timed updates change scalar counters and reservations only. Leaving the
    // large roster/project/contact vectors in every snapshot multiplied the
    // public artifact and startup validation cost without adding information.
    for snapshot in [
        &mut stats_after_message,
        &mut stats_after_ack,
        &mut stats_after_release,
    ] {
        snapshot.agents_list.clear();
        snapshot.projects_list.clear();
        snapshot.contacts_list.clear();
    }

    let synthetic_message = MailEvent::MessageSent {
        seq: 10_001,
        timestamp_micros: BASE_TS + 2_000_000,
        source: EventSource::Mail,
        redacted: true,
        id: 8_001,
        from: "AmberDeer".to_string(),
        to: vec!["RubyPrairie".to_string()],
        subject: "WASM dashboard parity frame is ready".to_string(),
        thread_id: "demo-browser-dashboard".to_string(),
        project: "mcp-agent-mail-rust".to_string(),
        body_md: "Synthetic public demo message: compare the deterministic frame hash.".to_string(),
    };
    let reservation_release = MailEvent::ReservationReleased {
        seq: 10_002,
        timestamp_micros: BASE_TS + 10_000_000,
        source: EventSource::Reservations,
        redacted: true,
        agent: "RubyPrairie".to_string(),
        paths: vec!["crates/ftui-web/**".to_string()],
        project: "frankentui".to_string(),
    };
    let health = MailEvent::HealthPulse {
        seq: 10_003,
        timestamp_micros: BASE_TS + 14_000_000,
        source: EventSource::Lifecycle,
        redacted: true,
        db_stats: stats_after_release.clone(),
    };

    let mut actions = synthetic_startup_history(BASE_TS);
    actions.extend([
        DemoAction {
            at_ms: 1_000,
            operation: DemoOperation::RecordRequest {
                status: 200,
                duration_ms: 48,
            },
        },
        DemoAction {
            at_ms: 2_000,
            operation: DemoOperation::PublishEvent {
                event: synthetic_message,
            },
        },
        DemoAction {
            at_ms: 2_000,
            operation: DemoOperation::MergeDbStats {
                snapshot: stats_after_message,
            },
        },
        DemoAction {
            at_ms: 6_000,
            operation: DemoOperation::MergeDbStats {
                snapshot: stats_after_ack,
            },
        },
        DemoAction {
            at_ms: 7_000,
            operation: DemoOperation::RecordRequest {
                status: 409,
                duration_ms: 112,
            },
        },
        DemoAction {
            at_ms: 10_000,
            operation: DemoOperation::PublishEvent {
                event: reservation_release,
            },
        },
        DemoAction {
            at_ms: 10_000,
            operation: DemoOperation::MergeDbStats {
                snapshot: stats_after_release.clone(),
            },
        },
        DemoAction {
            at_ms: 14_000,
            operation: DemoOperation::PublishEvent { event: health },
        },
        DemoAction {
            at_ms: 16_000,
            operation: DemoOperation::ConsoleLine {
                text: "replay loop complete; resetting to verified bootstrap".to_string(),
            },
        },
    ]);

    let mut pack = DemoPack {
        schema: DEMO_PACK_SCHEMA_V1.to_string(),
        title: "Agent Mail coordination dashboard".to_string(),
        replay_label: "Interactive sanitized snapshot replay".to_string(),
        duration_ms: 18_000,
        loop_replay: true,
        provenance: DemoProvenance {
            source_label: "approved aggregate counts projection; all details synthetic".to_string(),
            captured_at: "2026-08-01T00:00:00Z".to_string(),
            source_revision: "public-demo-v1".to_string(),
            privacy_policy: PUBLIC_PRIVACY_POLICY_V1.to_string(),
            content_sha256: String::new(),
        },
        bootstrap: DemoBootstrap {
            db_stats: base_stats,
            requests: RequestCounters {
                total: 42_000,
                status_2xx: 41_790,
                status_4xx: 190,
                status_5xx: 20,
                latency_total_ms: 3_402_000,
            },
            latency_samples_ms: vec![
                42.0, 58.0, 47.0, 86.0, 64.0, 51.0, 73.0, 49.0, 56.0, 67.0, 45.0, 91.0, 62.0, 54.0,
                78.0, 44.0, 52.0, 69.0, 48.0, 83.0, 61.0, 57.0, 75.0, 46.0, 59.0, 71.0, 43.0, 88.0,
                66.0, 53.0, 80.0, 50.0,
            ],
            console_lines: vec![
                "demo pack verified; public replay is read-only".to_string(),
                "FrankenTUI browser renderer connected".to_string(),
            ],
        },
        actions,
    };
    pack.finalize_digest();
    debug_assert!(pack.validate().is_ok());
    pack
}

#[cfg(test)]
mod tests {
    use super::{
        DemoOperation, DemoPack, DemoPackError, STARTUP_AGENT_ROWS, STARTUP_CONTACT_ROWS,
        STARTUP_HISTORY_EVENTS, STARTUP_PROJECT_ROWS, curated_public_demo,
    };
    use crate::state::TuiSharedState;
    use crate::tui_events::MailEventKind;

    #[test]
    fn curated_pack_round_trips_and_digest_verifies() {
        let pack = curated_public_demo();
        let json = pack.to_pretty_json().unwrap();
        assert!(
            json.len() < 500_000,
            "public replay pack regressed to {} bytes",
            json.len()
        );
        assert_eq!(DemoPack::from_json(&json).unwrap(), pack);
    }

    #[test]
    fn curated_pack_opens_at_terminal_reference_density() {
        let pack = curated_public_demo();
        let startup_events = pack
            .actions
            .iter()
            .take_while(|action| action.at_ms == 0)
            .count();
        assert_eq!(
            startup_events,
            usize::try_from(STARTUP_HISTORY_EVENTS).unwrap()
        );
        assert_eq!(
            pack.bootstrap.db_stats.agents_list.len(),
            STARTUP_AGENT_ROWS
        );
        assert_eq!(
            pack.bootstrap.db_stats.projects_list.len(),
            STARTUP_PROJECT_ROWS
        );
        assert_eq!(
            pack.bootstrap.db_stats.contacts_list.len(),
            STARTUP_CONTACT_ROWS
        );

        let event_count = |kind| {
            pack.actions
                .iter()
                .filter_map(|action| match &action.operation {
                    DemoOperation::PublishEvent { event } => Some(event.kind()),
                    _ => None,
                })
                .filter(|event_kind| *event_kind == kind)
                .count()
        };
        let tool_starts = event_count(MailEventKind::ToolCallStart);
        let tool_ends = event_count(MailEventKind::ToolCallEnd);
        assert_eq!(
            tool_starts, tool_ends,
            "tool lifecycle rows must be balanced"
        );
        assert!(
            tool_starts >= 12,
            "tool filters need a dense opening history"
        );
        assert!(event_count(MailEventKind::HttpRequest) >= 12);
        assert!(event_count(MailEventKind::GitSegfaultRetry) >= 6);
    }

    #[test]
    fn compact_stats_merge_is_explicit_and_full_snapshots_can_clear_details() {
        let mut pack = curated_public_demo();
        let action_index = pack
            .actions
            .iter()
            .position(|action| matches!(&action.operation, DemoOperation::MergeDbStats { .. }))
            .expect("curated pack should contain a compact stats merge");
        let state = TuiSharedState::new();
        pack.apply_bootstrap(&state);
        pack.apply_action(action_index, &state);
        let merged = state.db_stats_snapshot().expect("merged stats snapshot");
        assert_eq!(merged.agents_list.len(), STARTUP_AGENT_ROWS);
        assert_eq!(merged.projects_list.len(), STARTUP_PROJECT_ROWS);
        assert_eq!(merged.contacts_list.len(), STARTUP_CONTACT_ROWS);

        let snapshot = match &pack.actions[action_index].operation {
            DemoOperation::MergeDbStats { snapshot } => snapshot.clone(),
            _ => unreachable!("selected operation changed"),
        };
        pack.actions[action_index].operation = DemoOperation::SetDbStats { snapshot };
        pack.apply_bootstrap(&state);
        pack.apply_action(action_index, &state);
        let replaced = state.db_stats_snapshot().expect("full stats snapshot");
        assert!(replaced.agents_list.is_empty());
        assert!(replaced.projects_list.is_empty());
        assert!(replaced.contacts_list.is_empty());
    }

    #[test]
    fn digest_tampering_fails_closed() {
        let mut pack = curated_public_demo();
        pack.title.push('!');
        assert!(matches!(
            pack.validate(),
            Err(DemoPackError::DigestMismatch { .. })
        ));
    }

    #[test]
    fn absolute_home_paths_fail_privacy_gate() {
        let mut pack = curated_public_demo();
        pack.provenance.source_label = "/Users/private/mailbox.sqlite3".to_string();
        pack.finalize_digest();
        assert!(matches!(
            pack.validate(),
            Err(DemoPackError::UnsafeText { .. })
        ));
    }

    #[test]
    fn absolute_event_paths_fail_privacy_gate() {
        let mut pack = curated_public_demo();
        let release = pack
            .actions
            .iter_mut()
            .find_map(|action| match &mut action.operation {
                super::DemoOperation::PublishEvent {
                    event: crate::tui_events::MailEvent::ReservationReleased { paths, .. },
                } => Some(paths),
                _ => None,
            })
            .unwrap();
        release[0] = "/tmp/private".to_string();
        pack.finalize_digest();
        assert!(matches!(
            pack.validate(),
            Err(DemoPackError::UnsafeText { .. })
        ));
    }

    #[test]
    fn project_human_keys_must_be_public_relative_paths() {
        for private_path in [
            "/Volumes/private/mailbox",
            "/private/work/agent-mail",
            "D:/work/private-mailbox",
            "D:\\work\\private-mailbox",
        ] {
            let mut pack = curated_public_demo();
            pack.bootstrap.db_stats.projects_list[0].human_key = private_path.to_string();
            pack.finalize_digest();
            assert!(
                matches!(pack.validate(), Err(DemoPackError::UnsafeText { .. })),
                "absolute project human_key was accepted: {private_path}"
            );
        }
    }

    #[test]
    fn unknown_members_fail_before_privacy_or_digest_bypass() {
        let pack = curated_public_demo();
        let base = serde_json::to_value(pack).unwrap();
        let mut cases = Vec::new();

        let mut top_level = base.clone();
        top_level["private_dump"] = serde_json::json!("/Users/private/mailbox.sqlite3");
        cases.push(("top-level", top_level));

        let mut provenance = base.clone();
        provenance["provenance"]["private_dump"] =
            serde_json::json!("/Users/private/mailbox.sqlite3");
        cases.push(("provenance", provenance));

        let mut counters = base.clone();
        counters["bootstrap"]["requests"]["private_dump"] =
            serde_json::json!("/Users/private/mailbox.sqlite3");
        cases.push(("request counters", counters));

        let mut snapshot = base.clone();
        snapshot["bootstrap"]["db_stats"]["projects_list"][0]["private_dump"] =
            serde_json::json!("/Users/private/mailbox.sqlite3");
        cases.push(("nested snapshot row", snapshot));

        let mut action = base.clone();
        action["actions"][0]["private_dump"] = serde_json::json!("/Users/private/mailbox.sqlite3");
        cases.push(("flattened action", action));

        let mut event = base;
        event["actions"][0]["event"]["private_dump"] =
            serde_json::json!("/Users/private/mailbox.sqlite3");
        cases.push(("nested event", event));

        for (label, value) in cases {
            let result = DemoPack::from_json(&serde_json::to_string(&value).unwrap());
            assert!(result.is_err(), "unknown {label} member was accepted");
            assert!(
                !matches!(result, Err(DemoPackError::DigestMismatch { .. })),
                "unknown {label} member reached the typed digest instead of failing at the schema boundary"
            );
        }
    }

    #[test]
    fn impossible_bootstrap_request_counters_fail_closed() {
        let mut zero_total_latency = curated_public_demo();
        zero_total_latency.bootstrap.requests = crate::state::RequestCounters {
            total: 0,
            latency_total_ms: 1,
            ..crate::state::RequestCounters::default()
        };
        zero_total_latency.finalize_digest();
        assert!(matches!(
            zero_total_latency.validate(),
            Err(DemoPackError::InvalidRequestCounters { .. })
        ));

        let mut buckets_exceed_total = curated_public_demo();
        buckets_exceed_total.bootstrap.requests.total = 1;
        buckets_exceed_total.bootstrap.requests.status_2xx = 2;
        buckets_exceed_total.bootstrap.requests.status_4xx = 0;
        buckets_exceed_total.bootstrap.requests.status_5xx = 0;
        buckets_exceed_total.finalize_digest();
        assert!(matches!(
            buckets_exceed_total.validate(),
            Err(DemoPackError::InvalidRequestCounters { .. })
        ));

        let mut overflowing_buckets = curated_public_demo();
        overflowing_buckets.bootstrap.requests.total = u64::MAX;
        overflowing_buckets.bootstrap.requests.status_2xx = u64::MAX;
        overflowing_buckets.bootstrap.requests.status_4xx = 1;
        overflowing_buckets.bootstrap.requests.status_5xx = 0;
        overflowing_buckets.finalize_digest();
        assert!(matches!(
            overflowing_buckets.validate(),
            Err(DemoPackError::InvalidRequestCounters { .. })
        ));
    }

    #[test]
    fn unapproved_privacy_policy_fails_closed() {
        let mut pack = curated_public_demo();
        pack.provenance.privacy_policy = "public-demo-unreviewed".to_string();
        pack.finalize_digest();
        assert!(matches!(
            pack.validate(),
            Err(DemoPackError::UnsafeText { .. })
        ));
    }

    #[test]
    fn privacy_corpus_rejects_defined_identity_and_secret_classes() {
        for prohibited in [
            "/Users/private/mailbox.sqlite3",
            "/home/private/.agent-mail/storage.sqlite3",
            "C:\\Users\\private\\mailbox.sqlite3",
            "Authorization: Basic synthetic",
            "Bearer synthetic-token",
            "ghp_synthetic_token",
            "sk-synthetic-key",
            "password=synthetic",
            "api_key=synthetic",
        ] {
            let mut pack = curated_public_demo();
            pack.provenance.source_label = prohibited.to_string();
            pack.finalize_digest();
            assert!(
                matches!(pack.validate(), Err(DemoPackError::UnsafeText { .. })),
                "privacy corpus entry was accepted: {prohibited}"
            );
        }
    }

    #[test]
    fn unredacted_events_fail_closed() {
        let mut pack = curated_public_demo();
        let event = pack
            .actions
            .iter_mut()
            .find_map(|action| match &mut action.operation {
                super::DemoOperation::PublishEvent {
                    event: crate::tui_events::MailEvent::MessageSent { redacted, .. },
                } => Some(redacted),
                _ => None,
            })
            .unwrap();
        *event = false;
        pack.finalize_digest();
        assert!(matches!(
            pack.validate(),
            Err(DemoPackError::UnredactedEvent(_))
        ));
    }

    #[test]
    fn malformed_replay_contracts_fail_closed() {
        let mut non_finite = curated_public_demo();
        non_finite.bootstrap.latency_samples_ms[0] = f64::NAN;
        non_finite.finalize_digest();
        assert!(matches!(
            non_finite.validate(),
            Err(DemoPackError::NonFiniteLatency(0))
        ));

        let mut non_monotonic = curated_public_demo();
        non_monotonic.actions[1].at_ms = 9_000;
        non_monotonic.finalize_digest();
        assert!(matches!(
            non_monotonic.validate(),
            Err(DemoPackError::NonMonotonicAction { .. })
        ));

        let mut past_duration = curated_public_demo();
        past_duration.actions[0].at_ms = past_duration.duration_ms + 1;
        past_duration.finalize_digest();
        assert!(matches!(
            past_duration.validate(),
            Err(DemoPackError::ActionPastDuration { .. })
        ));

        let mut invalid_status = curated_public_demo();
        let status = invalid_status
            .actions
            .iter_mut()
            .find_map(|action| match &mut action.operation {
                super::DemoOperation::RecordRequest { status, .. } => Some(status),
                _ => None,
            })
            .unwrap();
        *status = 99;
        invalid_status.finalize_digest();
        assert!(matches!(
            invalid_status.validate(),
            Err(DemoPackError::InvalidHttpStatus { status: 99, .. })
        ));

        let mut action_burst = curated_public_demo();
        let repeated = action_burst.actions[0].clone();
        action_burst.actions = vec![repeated; super::MAX_ACTIONS_PER_TIMESTAMP + 1];
        action_burst.finalize_digest();
        assert!(matches!(
            action_burst.validate(),
            Err(DemoPackError::TooManyActionsAtTimestamp {
                at_ms: 0,
                count,
                max,
            }) if count == super::MAX_ACTIONS_PER_TIMESTAMP + 1
                && max == super::MAX_ACTIONS_PER_TIMESTAMP
        ));
    }

    #[test]
    fn missing_or_malformed_digest_fails_closed() {
        let mut pack = curated_public_demo();
        pack.provenance.content_sha256.clear();
        assert!(
            matches!(pack.validate(), Err(DemoPackError::InvalidDigest { .. })),
            "empty digest must no longer bypass integrity verification"
        );

        for malformed in [
            "deadbeef",                      // too short
            &"a".repeat(63),                 // 63 chars
            &"a".repeat(65),                 // 65 chars
            &format!("{}G", "a".repeat(63)), // non-hex
            &curated_public_demo()
                .provenance
                .content_sha256
                .to_uppercase(), // uppercase
        ] {
            pack.provenance.content_sha256 = malformed.to_string();
            assert!(
                matches!(pack.validate(), Err(DemoPackError::InvalidDigest { .. })),
                "malformed digest {malformed:?} must be rejected"
            );
        }
    }

    #[test]
    fn oversized_pack_is_rejected_before_parsing() {
        let oversized = "x".repeat(super::MAX_SERIALIZED_PACK_BYTES + 1);
        let result = DemoPack::from_json(&oversized);
        assert!(
            matches!(
                result,
                Err(DemoPackError::OversizedPack { len, max })
                    if len == super::MAX_SERIALIZED_PACK_BYTES + 1
                        && max == super::MAX_SERIALIZED_PACK_BYTES
            ),
            "an oversized pack must be rejected by the size gate, not the JSON parser"
        );
    }

    #[test]
    fn oversized_text_field_fails_closed() {
        let mut pack = curated_public_demo();
        pack.bootstrap
            .console_lines
            .push("y".repeat(super::MAX_TEXT_FIELD_BYTES + 1));
        pack.finalize_digest();
        assert!(matches!(
            pack.validate(),
            Err(DemoPackError::OversizedField { .. })
        ));
    }

    #[test]
    fn curated_pack_fits_comfortably_inside_size_budget() {
        let json = curated_public_demo().to_pretty_json().unwrap();
        assert!(
            json.len() <= super::MAX_SERIALIZED_PACK_BYTES / 2,
            "curated pack ({} bytes) must keep at least 2x headroom under the \
             {}-byte contract ceiling",
            json.len(),
            super::MAX_SERIALIZED_PACK_BYTES
        );
    }

    #[test]
    fn traversal_paths_fail_in_every_representation() {
        for hostile in [
            "..",
            "../x",
            "x/..",
            "a/../b",
            "a/./b",
            ".",
            "a//b",
            "src/",
            "/absolute",
            "a\\..\\b",
            "..%2f",
            "%2e%2e/",
            "a/%2e%2e",
            "C:/windows",
            "c:relative",
        ] {
            assert!(
                super::validate_public_path("test.path", hostile).is_err(),
                "hostile path {hostile:?} must be rejected"
            );
        }
        for benign in [
            "src/**",
            "crates/foo/src/lib.rs",
            "docs/browser-dashboard.md",
        ] {
            assert!(
                super::validate_public_path("test.path", benign).is_ok(),
                "benign path {benign:?} must stay valid"
            );
        }
    }
}
