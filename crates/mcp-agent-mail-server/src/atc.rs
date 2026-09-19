//! Air Traffic Controller planning, passive liveness, and actionable mail.
//!
//! Liveness inference still consumes attributed tool activity and keeps its
//! bounded evidence ledger. Routine probes and monitoring notices are passive:
//! neither Live mode nor restarting grants permission to append mailbox rows.
//! Only actionable notifications cross the delivery admission boundary.
//! Population hydration yields between bounded slices before inference resumes.

#[path = "atc_engine.rs"]
mod engine;
#[cfg(test)]
pub(crate) use engine::GLOBAL_ATC_TEST_LOCK;
pub use engine::*;

#[path = "atc_population.rs"]
mod population;
pub use population::{
    AtcPopulationHydrationStats, atc_population_hydration_stats, atc_sync_population_from_db,
};

#[path = "atc_delivery.rs"]
mod delivery;
pub use delivery::AtcDeliveryStats;
use delivery::{Admission, Notification, NotificationAdmission, NotificationClass};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

struct DeliveryState {
    admission: NotificationAdmission,
    last_tick_count: Option<u64>,
    last_effect_count: usize,
    last_notification_key: Option<String>,
}

impl DeliveryState {
    fn new(probe_interval_micros: i64) -> Self {
        Self {
            admission: NotificationAdmission::new(probe_interval_micros),
            last_tick_count: None,
            last_effect_count: 0,
            last_notification_key: None,
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

/// Initialize planning, hydration, and process-local delivery admission together.
pub fn init_global_atc(config: &mcp_agent_mail_core::Config) {
    population::reset_with(|| engine::init_global_atc(config));
    reset_delivery(AtcEngine::config_from_env(config).probe_interval_micros);
}

#[cfg(test)]
pub(crate) fn reset_global_atc_state_for_test(config: &mcp_agent_mail_core::Config) {
    population::reset_with(|| engine::reset_global_atc_state_for_test(config));
    reset_delivery(AtcEngine::config_from_env(config).probe_interval_micros);
}

/// Passive checks and suppression are counted in memory, not as replacement
/// durable telemetry. Counters reset with the global engine.
#[must_use]
pub fn atc_delivery_stats() -> AtcDeliveryStats {
    delivery_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .admission
        .stats()
}

/// Advance bounded hydration or inference, then admit actionable notifications.
///
/// Inference yields while a population refresh is incomplete; already-generated
/// reservation mutations and their outcome notices retain their delivery policy.
#[must_use]
pub fn atc_tick_report(now_micros: i64) -> Option<AtcTickReport> {
    let mut report = population::tick_report(now_micros)?;
    let mut state = delivery_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let before = state.admission.stats();
    state.admission.begin_tick(now_micros);
    let generated = report.effects.len();
    {
        let DeliveryState {
            admission,
            last_notification_key,
            ..
        } = &mut *state;
        admit_effects(
            &mut report.effects,
            &mut report.actions,
            admission,
            last_notification_key,
            now_micros,
            engine::atc_agent_last_activity,
        );
    }
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
            passive_liveness_total = after.passive_liveness,
            duplicate_total = after.duplicate,
            deferred_total = after.deferred,
            no_activity_total = after.no_activity,
            recently_active_total = after.recently_active,
            "ATC observed passive liveness checks or suppressed optional mail"
        );
    }
    Some(report)
}

/// The action-only entry point must not bypass passive liveness or admission.
#[must_use]
pub fn atc_tick(now_micros: i64) -> Vec<AtcTickAction> {
    atc_tick_report(now_micros).map_or_else(Vec::new, |report| report.actions)
}

/// Keep the standalone snapshot consistent with admission and hydration progress.
#[must_use]
pub fn atc_summary() -> Option<AtcSummarySnapshot> {
    let mut summary = engine::atc_summary()?;
    let state = delivery_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if state.last_tick_count == Some(summary.tick_count) {
        summary.kernel.pending_effects = state.last_effect_count;
    }
    drop(state);
    population::annotate_summary(&mut summary);
    Some(summary)
}

fn notification_class(kind: &str, family: &str, high_risk: bool) -> Option<NotificationClass> {
    // Mutations, outcome reports, and unknown effect kinds are not optional mail.
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

fn retain_actions(actions: &mut Vec<AtcTickAction>, retained: &mut HashMap<ActionKey, usize>) {
    actions.retain(|action| {
        let key = match action {
            AtcTickAction::ProbeAgent { agent } => ActionKey::Probe(agent.clone()),
            AtcTickAction::SendAdvisory { agent, message } => {
                ActionKey::Advisory(agent.clone(), message.clone())
            }
            AtcTickAction::ReleaseReservations { .. } => return true,
        };
        // An action can outlive the bounded ledger entry needed to construct
        // its effect. Without a retained effect it has no delivery entitlement:
        // otherwise the action-only API could bypass the passive policy.
        let Some(remaining) = retained.get_mut(&key) else {
            return false;
        };
        if *remaining == 0 {
            return false;
        }
        *remaining -= 1;
        true
    });
}

fn admit_effects(
    effects: &mut Vec<AtcEffectPlan>,
    actions: &mut Vec<AtcTickAction>,
    admission: &mut NotificationAdmission,
    last_notification_key: &mut Option<String>,
    now_micros: i64,
    mut last_activity: impl FnMut(&str) -> Option<i64>,
) {
    // Rotate admission priority, never execution order. Preserve fairness for
    // recurring conflict notices even when all cooldowns expire between ticks.
    // Passive checks never move the cursor or consume a delivery slot.
    let mut candidates: Vec<_> = effects
        .iter()
        .enumerate()
        .filter_map(|(index, effect)| {
            notification_class(
                &effect.kind,
                &effect.semantics.family,
                effect.semantics.high_risk_intervention,
            )
            .map(|class| (index, class))
        })
        .collect();
    candidates.sort_unstable_by(|left, right| {
        effects[left.0]
            .semantics
            .cooldown_key
            .cmp(&effects[right.0].semantics.cooldown_key)
            .then_with(|| left.0.cmp(&right.0))
    });
    let start = last_notification_key.as_deref().map_or(0, |last| {
        candidates
            .partition_point(|(index, _)| effects[*index].semantics.cooldown_key.as_str() <= last)
    });
    let mut accepted = vec![true; effects.len()];
    let mut activity_by_agent = HashMap::new();
    for &(index, class) in candidates[start..].iter().chain(&candidates[..start]) {
        let effect = &effects[index];
        let activity = if class == NotificationClass::Conflict {
            None
        } else {
            *activity_by_agent
                .entry(effect.agent.clone())
                .or_insert_with(|| last_activity(&effect.agent))
        };
        accepted[index] = admission.admit(
            Notification {
                key: &effect.semantics.cooldown_key,
                class,
                last_activity_micros: activity,
                cooldown_micros: effect.semantics.cooldown_micros,
            },
            now_micros,
        ) == Admission::Admitted;
        if accepted[index] {
            *last_notification_key = Some(effect.semantics.cooldown_key.clone());
        }
    }
    let mut retained = HashMap::new();
    let mut index = 0;
    effects.retain(|effect| {
        let keep = accepted[index];
        index += 1;
        if keep && let Some(key) = effect_action_key(effect) {
            *retained.entry(key).or_insert(0_usize) += 1;
        }
        keep
    });
    // Exact body matching protects genuine outcome notices for the same agent.
    retain_actions(actions, &mut retained);
}

#[cfg(test)]
#[path = "atc_admission_tests.rs"]
mod fair_admission_tests;

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
        for family in ["liveness_monitoring", "withheld_release_notice"] {
            assert_eq!(
                notification_class("send_advisory", family, false),
                Some(NotificationClass::Liveness)
            );
        }
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
        let key = ActionKey::Advisory("BlueFox".into(), "released".into());
        retain_actions(&mut actions, &mut HashMap::from([(key, 1)]));
        assert_eq!(actions.len(), 2);
        assert!(matches!(
            actions[0],
            AtcTickAction::ReleaseReservations { .. }
        ));
        assert!(
            matches!(&actions[1], AtcTickAction::SendAdvisory { message, .. } if message == "released")
        );
    }

    #[test]
    fn one_retained_effect_cannot_replay_multiple_identical_actions() {
        let key = ActionKey::Advisory("BlueFox".into(), "notice".into());
        let mut actions = vec![
            AtcTickAction::SendAdvisory {
                agent: "BlueFox".into(),
                message: "notice".into()
            };
            10_000
        ];
        retain_actions(&mut actions, &mut HashMap::from([(key, 1)]));
        assert_eq!(actions.len(), 1);
    }

    #[test]
    fn multiple_genuinely_retained_notices_preserve_their_multiplicity() {
        let key = ActionKey::Advisory("BlueFox".into(), "notice".into());
        let mut actions = vec![
            AtcTickAction::SendAdvisory {
                agent: "BlueFox".into(),
                message: "notice".into()
            };
            3
        ];
        retain_actions(&mut actions, &mut HashMap::from([(key, 2)]));
        assert_eq!(actions.len(), 2);
    }

    #[test]
    fn orphaned_notification_actions_cannot_bypass_passive_admission() {
        let mut actions = vec![
            AtcTickAction::ProbeAgent {
                agent: "BlueFox".into(),
            },
            AtcTickAction::SendAdvisory {
                agent: "BlueFox".into(),
                message: "monitoring".into(),
            },
        ];
        retain_actions(&mut actions, &mut HashMap::new());
        assert!(actions.is_empty());
    }

    #[test]
    fn shared_global_reset_also_resets_notification_admission() {
        let _guard = GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config {
            atc_enabled: true,
            atc_probe_interval_secs: 1,
            ..Default::default()
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
                    200_000_000
                ),
                Admission::Passive
            );
        }
        assert_eq!(atc_delivery_stats().admitted, 0);
        assert_eq!(atc_delivery_stats().passive_liveness, 1);
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
            ..Default::default()
        };
        reset_global_atc_state_for_test(&config);
        assert!(atc_tick_report(200_000_000).is_none());
        assert!(atc_tick(200_000_000).is_empty());
        assert_eq!(atc_delivery_stats(), AtcDeliveryStats::default());
    }
}
