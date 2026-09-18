//! Air Traffic Controller planning and notification admission.
//!
//! The decision engine remains responsible for liveness inference, evidence,
//! calibration, and release safety. The public tick boundary additionally admits
//! best-effort notifications before the operator creates durable experiences or
//! messages. Keeping these responsibilities separate prevents a fresh decision
//! ID on every tick from becoming a fresh delivery entitlement (GH258/GH264).

#[path = "atc_engine.rs"]
mod engine;
#[cfg(test)]
pub(crate) use engine::GLOBAL_ATC_TEST_LOCK;
pub use engine::*;

#[path = "atc_delivery.rs"]
mod delivery;
pub use delivery::AtcDeliveryStats;
use delivery::{Admission, Notification, NotificationAdmission, NotificationClass};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

struct DeliveryState {
    admission: NotificationAdmission,
    last_tick_count: Option<u64>,
    last_effect_count: usize,
}

impl DeliveryState {
    fn new(probe_interval_micros: i64) -> Self {
        Self {
            admission: NotificationAdmission::new(probe_interval_micros),
            last_tick_count: None,
            last_effect_count: 0,
        }
    }
}

static DELIVERY: OnceLock<Mutex<DeliveryState>> = OnceLock::new();

fn delivery_state() -> &'static Mutex<DeliveryState> {
    DELIVERY.get_or_init(|| {
        Mutex::new(DeliveryState::new(
            AtcConfig::default().probe_interval_micros,
        ))
    })
}

fn reset_delivery(probe_interval_micros: i64) {
    *delivery_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        DeliveryState::new(probe_interval_micros);
}

/// Initialize planning and its process-local delivery admission together.
pub fn init_global_atc(config: &mcp_agent_mail_core::Config) {
    engine::init_global_atc(config);
    reset_delivery(AtcEngine::config_from_env(config).probe_interval_micros);
}

#[cfg(test)]
pub(crate) fn reset_global_atc_state_for_test(config: &mcp_agent_mail_core::Config) {
    engine::reset_global_atc_state_for_test(config);
    reset_delivery(AtcEngine::config_from_env(config).probe_interval_micros);
}

/// Delivery counters are in-memory only: suppressed mail must not generate
/// replacement durable telemetry. They reset with the global engine.
#[must_use]
pub fn atc_delivery_stats() -> AtcDeliveryStats {
    delivery_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .admission
        .stats()
}

/// Run the engine and admit optional notifications before any executor I/O.
/// Independent reservation mutations and their outcome notices are preserved.
#[must_use]
pub fn atc_tick_report(now_micros: i64) -> Option<AtcTickReport> {
    // The engine lock is released before admission. MCP activity hooks only
    // acquire the engine lock and never wait for notification admission.
    let mut report = engine::atc_tick_report(now_micros)?;
    let mut state = delivery_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let before = state.admission.stats();
    state.admission.begin_tick(now_micros);
    let generated = report.effects.len();
    admit_effects(
        &mut report.effects,
        &mut report.actions,
        &mut state.admission,
        now_micros,
        engine::atc_agent_last_activity,
    );
    report.summary.kernel.pending_effects = report.effects.len();
    state.last_tick_count = Some(report.summary.tick_count);
    state.last_effect_count = report.effects.len();
    let after = state.admission.stats();
    if before.capacity_suppressed == 0 && after.capacity_suppressed > 0 {
        tracing::warn!(
            tracked_keys = after.tracked_keys,
            "ATC notification admission is full; optional notices are suppressed, not evicted"
        );
    }
    if generated != report.effects.len() {
        tracing::debug!(
            generated,
            admitted = report.effects.len(),
            duplicate_total = after.duplicate,
            deferred_total = after.deferred,
            no_activity_total = after.no_activity,
            recently_active_total = after.recently_active,
            "ATC notification admission coalesced or deferred proposals"
        );
    }
    Some(report)
}

/// The action-only entry point must not bypass notification admission.
#[must_use]
pub fn atc_tick(now_micros: i64) -> Vec<AtcTickAction> {
    atc_tick_report(now_micros).map_or_else(Vec::new, |report| report.actions)
}

/// Keep the standalone snapshot's pending count consistent with the tick report.
#[must_use]
pub fn atc_summary() -> Option<AtcSummarySnapshot> {
    let mut summary = engine::atc_summary()?;
    let state = delivery_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if state.last_tick_count == Some(summary.tick_count) {
        summary.kernel.pending_effects = state.last_effect_count;
    }
    Some(summary)
}

fn notification_class(kind: &str, family: &str, high_risk: bool) -> Option<NotificationClass> {
    // Fail open for mutations, their outcome reports, and future effect types.
    // Only the following explicitly best-effort notification families are gated.
    if high_risk {
        return None;
    }
    match (kind, family) {
        ("probe_agent", "liveness_probe") => Some(NotificationClass::Probe),
        ("send_advisory", "liveness_monitoring" | "withheld_release_notice") => {
            Some(NotificationClass::Liveness)
        }
        ("send_advisory", "deadlock_remediation") => Some(NotificationClass::Conflict),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ActionKey {
    Probe(String),
    Advisory(String, String),
}

fn effect_action_key(effect: &AtcEffectPlan) -> Option<ActionKey> {
    match effect.kind.as_str() {
        "probe_agent" => Some(ActionKey::Probe(effect.agent.clone())),
        "send_advisory" => effect
            .message
            .as_ref()
            .map(|body| ActionKey::Advisory(effect.agent.clone(), body.clone())),
        _ => None,
    }
}

fn retain_actions(
    actions: &mut Vec<AtcTickAction>,
    suppressed: &HashSet<ActionKey>,
    retained: &mut HashMap<ActionKey, usize>,
) {
    actions.retain(|action| {
        let key = match action {
            AtcTickAction::ProbeAgent { agent } => ActionKey::Probe(agent.clone()),
            AtcTickAction::SendAdvisory { agent, message } => {
                ActionKey::Advisory(agent.clone(), message.clone())
            }
            AtcTickAction::ReleaseReservations { .. } => return true,
        };
        if let Some(remaining) = retained.get_mut(&key) {
            if *remaining == 0 {
                return false;
            }
            *remaining -= 1;
            return true;
        }
        !suppressed.contains(&key)
    });
}

fn admit_effects(
    effects: &mut Vec<AtcEffectPlan>,
    actions: &mut Vec<AtcTickAction>,
    admission: &mut NotificationAdmission,
    now_micros: i64,
    mut last_activity: impl FnMut(&str) -> Option<i64>,
) {
    let mut activity_by_agent = HashMap::new();
    let mut suppressed = HashSet::new();
    let mut retained = HashMap::new();
    effects.retain(|effect| {
        let accepted = notification_class(
            &effect.kind,
            &effect.semantics.family,
            effect.semantics.high_risk_intervention,
        )
        .is_none_or(|class| {
            // Conflict notices need no activity lookup. Liveness candidates
            // share one lookup per agent, even when they have multiple families.
            let activity = if class == NotificationClass::Conflict {
                None
            } else {
                *activity_by_agent
                    .entry(effect.agent.clone())
                    .or_insert_with(|| last_activity(&effect.agent))
            };
            admission.admit(
                Notification {
                    key: &effect.semantics.cooldown_key,
                    class,
                    last_activity_micros: activity,
                    cooldown_micros: effect.semantics.cooldown_micros,
                },
                now_micros,
            ) == Admission::Admitted
        });
        if let Some(key) = effect_action_key(effect) {
            if accepted {
                *retained.entry(key).or_insert(0_usize) += 1;
            } else {
                suppressed.insert(key);
            }
        }
        accepted
    });
    // Match the exact advisory body, not just its recipient: a suppressed
    // monitoring prompt must not hide the same agent's genuine release notice.
    // Counts also prevent the action-only API from replaying dropped duplicates.
    retain_actions(actions, &suppressed, &mut retained);
}

#[cfg(test)]
mod admission_boundary_tests {
    use super::*;

    #[test]
    fn critical_and_unknown_effects_cannot_be_admission_limited() {
        for (kind, family) in [
            ("release_reservations_requested", "reservation_release"),
            ("send_advisory", "release_notice"),
            ("future_mutation", "future_family"),
        ] {
            assert_eq!(notification_class(kind, family, false), None);
            assert_eq!(notification_class(kind, family, true), None);
        }
        assert_eq!(
            notification_class("send_advisory", "liveness_monitoring", true),
            None
        );
    }

    #[test]
    fn only_recognized_notification_kind_and_family_pairs_are_gated() {
        assert_eq!(
            notification_class("probe_agent", "liveness_probe", false),
            Some(NotificationClass::Probe)
        );
        assert_eq!(
            notification_class("send_advisory", "liveness_monitoring", false),
            Some(NotificationClass::Liveness)
        );
        assert_eq!(
            notification_class("send_advisory", "withheld_release_notice", false),
            Some(NotificationClass::Liveness)
        );
        assert_eq!(
            notification_class("send_advisory", "deadlock_remediation", false),
            Some(NotificationClass::Conflict)
        );
        assert_eq!(
            notification_class(
                "release_reservations_requested",
                "liveness_monitoring",
                false
            ),
            None
        );
    }

    #[test]
    fn suppressing_monitoring_preserves_release_and_its_exact_notice() {
        let mut actions = vec![
            AtcTickAction::ProbeAgent {
                agent: "BlueFox".into(),
            },
            AtcTickAction::SendAdvisory {
                agent: "BlueFox".into(),
                message: "monitoring".into(),
            },
            AtcTickAction::ReleaseReservations {
                agent: "BlueFox".into(),
            },
            AtcTickAction::SendAdvisory {
                agent: "BlueFox".into(),
                message: "released".into(),
            },
        ];
        let suppressed = HashSet::from([
            ActionKey::Probe("BlueFox".into()),
            ActionKey::Advisory("BlueFox".into(), "monitoring".into()),
        ]);
        retain_actions(&mut actions, &suppressed, &mut HashMap::new());
        assert_eq!(actions.len(), 2);
        assert!(matches!(
            actions[0],
            AtcTickAction::ReleaseReservations { .. }
        ));
        assert!(matches!(
            &actions[1],
            AtcTickAction::SendAdvisory { message, .. } if message == "released"
        ));
    }

    #[test]
    fn one_retained_effect_cannot_replay_multiple_identical_actions() {
        let key = ActionKey::Advisory("BlueFox".into(), "notice".into());
        let mut actions = vec![
            AtcTickAction::SendAdvisory {
                agent: "BlueFox".into(),
                message: "notice".into(),
            };
            10_000
        ];
        retain_actions(
            &mut actions,
            &HashSet::from([key.clone()]),
            &mut HashMap::from([(key, 1)]),
        );
        assert_eq!(actions.len(), 1);
    }

    #[test]
    fn multiple_genuinely_retained_notices_preserve_their_multiplicity() {
        let key = ActionKey::Advisory("BlueFox".into(), "notice".into());
        let mut actions = vec![
            AtcTickAction::SendAdvisory {
                agent: "BlueFox".into(),
                message: "notice".into(),
            };
            3
        ];
        retain_actions(
            &mut actions,
            &HashSet::from([key.clone()]),
            &mut HashMap::from([(key, 2)]),
        );
        assert_eq!(actions.len(), 2);
    }

    #[test]
    fn shared_global_reset_also_resets_notification_admission() {
        let _guard = GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config {
            atc_enabled: true,
            atc_probe_interval_secs: 1,
            ..mcp_agent_mail_core::Config::default()
        };
        reset_global_atc_state_for_test(&config);
        {
            let mut state = delivery_state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.admission.begin_tick(200_000_000);
            assert_eq!(
                state.admission.admit(
                    Notification {
                        key: "liveness_probe:project:BlueFox",
                        class: NotificationClass::Probe,
                        last_activity_micros: Some(1_000_000),
                        cooldown_micros: 1_000_000,
                    },
                    200_000_000,
                ),
                Admission::Admitted
            );
        }
        assert_eq!(atc_delivery_stats().admitted, 1);
        reset_global_atc_state_for_test(&config);
        assert_eq!(atc_delivery_stats(), AtcDeliveryStats::default());
    }

    #[test]
    fn disabled_public_tick_entrypoints_remain_inert() {
        let _guard = GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config {
            atc_enabled: false,
            ..mcp_agent_mail_core::Config::default()
        };
        reset_global_atc_state_for_test(&config);
        assert!(atc_tick_report(200_000_000).is_none());
        assert!(atc_tick(200_000_000).is_empty());
        assert_eq!(atc_delivery_stats(), AtcDeliveryStats::default());
    }
}
