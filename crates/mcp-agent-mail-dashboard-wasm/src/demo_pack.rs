//! Versioned, privacy-bounded data pack and deterministic replay actions.

use sha2::{Digest, Sha256};

use crate::state::{ConfigSnapshot, RequestCounters, TuiSharedState};
use crate::tui_events::{
    AgentSummary, ContactSummary, DbStatSnapshot, EventSource, MailEvent, ProjectSummary,
    ReservationSnapshot,
};

pub const DEMO_PACK_SCHEMA_V1: &str = "agent_mail.demo_pack.v1";
pub const PUBLIC_PRIVACY_POLICY_V1: &str = "agent-mail-dashboard-public-demo-v1";
const MAX_ACTIONS: usize = 10_000;
const MAX_DURATION_MS: u64 = 30 * 60 * 1_000;
const MAX_SPARKLINE_SAMPLES: usize = 240;
// Match the native reference's populated roster/contact rails. Timed snapshots
// below omit these repeated detail vectors and merge them from bootstrap.
const STARTUP_HISTORY_EVENTS: u64 = 192;
const STARTUP_AGENT_ROWS: usize = 500;
const STARTUP_PROJECT_ROWS: usize = 41;
const STARTUP_CONTACT_ROWS: usize = 200;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct DemoProvenance {
    pub source_label: String,
    pub captured_at: String,
    pub source_revision: String,
    pub privacy_policy: String,
    pub content_sha256: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
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
#[serde(tag = "type", rename_all = "snake_case")]
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
    UnsupportedSchema(String),
    EmptyMetadata(&'static str),
    InvalidDuration(u64),
    TooManyActions(usize),
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
    UnredactedEvent(usize),
    UnsafeText {
        field: String,
        reason: String,
    },
    DigestMismatch {
        expected: String,
        actual: String,
    },
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
        let pack: Self =
            serde_json::from_str(json).map_err(|error| DemoPackError::Json(error.to_string()))?;
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
        validate_snapshot(&self.bootstrap.db_stats)?;
        for (index, line) in self.bootstrap.console_lines.iter().enumerate() {
            validate_public_text(&format!("bootstrap.console_lines[{index}]"), line)?;
        }
        let mut previous_ms = 0;
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
        if !self.provenance.content_sha256.is_empty() {
            let actual = self.computed_content_sha256();
            if self.provenance.content_sha256 != actual {
                return Err(DemoPackError::DigestMismatch {
                    expected: self.provenance.content_sha256.clone(),
                    actual,
                });
            }
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
        validate_public_text(&format!("projects[{index}].human_key"), &project.human_key)?;
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
    if value.starts_with('/') || value.contains("../") || value.contains("\\") {
        return Err(DemoPackError::UnsafeText {
            field: field.to_string(),
            reason: "path must be project-relative and traversal-free".to_string(),
        });
    }
    validate_public_text(field, value)
}

fn validate_public_text(field: &str, value: &str) -> Result<(), DemoPackError> {
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
            let operation = match index % 8 {
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
                1 => DemoOperation::PublishEvent {
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
                2 | 5 => DemoOperation::PublishEvent {
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
                _ => DemoOperation::PublishEvent {
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
        pack.provenance.content_sha256.clear();
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
        pack.provenance.content_sha256.clear();
        assert!(matches!(
            pack.validate(),
            Err(DemoPackError::UnsafeText { .. })
        ));
    }

    #[test]
    fn unapproved_privacy_policy_fails_closed() {
        let mut pack = curated_public_demo();
        pack.provenance.privacy_policy = "public-demo-unreviewed".to_string();
        pack.provenance.content_sha256.clear();
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
            pack.provenance.content_sha256.clear();
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
        pack.provenance.content_sha256.clear();
        assert!(matches!(
            pack.validate(),
            Err(DemoPackError::UnredactedEvent(_))
        ));
    }

    #[test]
    fn malformed_replay_contracts_fail_closed() {
        let mut non_finite = curated_public_demo();
        non_finite.bootstrap.latency_samples_ms[0] = f64::NAN;
        non_finite.provenance.content_sha256.clear();
        assert!(matches!(
            non_finite.validate(),
            Err(DemoPackError::NonFiniteLatency(0))
        ));

        let mut non_monotonic = curated_public_demo();
        non_monotonic.actions[1].at_ms = 9_000;
        non_monotonic.provenance.content_sha256.clear();
        assert!(matches!(
            non_monotonic.validate(),
            Err(DemoPackError::NonMonotonicAction { .. })
        ));

        let mut past_duration = curated_public_demo();
        past_duration.actions[0].at_ms = past_duration.duration_ms + 1;
        past_duration.provenance.content_sha256.clear();
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
        invalid_status.provenance.content_sha256.clear();
        assert!(matches!(
            invalid_status.validate(),
            Err(DemoPackError::InvalidHttpStatus { status: 99, .. })
        ));
    }
}
