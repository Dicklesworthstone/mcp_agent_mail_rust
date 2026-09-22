//! Air Traffic Controller planning, passive liveness, and actionable mail.
//!
//! Liveness inference still consumes attributed tool activity and keeps its
//! bounded evidence ledger. Routine probes and monitoring notices are passive:
//! neither Live mode nor restarting grants permission to append mailbox rows.
//! Only actionable notifications cross the delivery admission boundary.
//! Population hydration yields between bounded slices before inference resumes.
//! A reset invalidates in-flight reports before replacing planning state. Old
//! reports cannot spend the new epoch's budget or publish reservation actions.

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
use std::sync::{Arc, Mutex, OnceLock};

struct DeliveryState {
    // An in-flight caller retains its Arc, so reset cannot reuse its identity.
    // This is process-local admission identity, not a database generation stamp.
    epoch: Arc<()>,
    reset_in_progress: bool,
    admission: NotificationAdmission,
    last_tick_count: Option<u64>,
    last_effect_count: usize,
    last_notification_key: Option<String>,
}

impl DeliveryState {
    fn new(probe_interval_micros: i64) -> Self {
        Self {
            epoch: Arc::new(()),
            reset_in_progress: false,
            admission: NotificationAdmission::new(probe_interval_micros),
            last_tick_count: None,
            last_effect_count: 0,
            last_notification_key: None,
        }
    }

    fn accepts_epoch(&self, epoch: &Arc<()>) -> bool {
        !self.reset_in_progress && Arc::ptr_eq(&self.epoch, epoch)
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

fn capture_delivery_epoch() -> Option<Arc<()>> {
    let state = delivery_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    (!state.reset_in_progress).then(|| Arc::clone(&state.epoch))
}

fn delivery_epoch_is_current(epoch: &Arc<()>) -> bool {
    delivery_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .accepts_epoch(epoch)
}

fn reset_with_delivery(probe_interval_micros: i64, reset_engine: impl FnOnce()) {
    // The hydration lock serializes concurrent resets and inference. Invalidate
    // delivery BEFORE changing the engine, then publish fresh admission BEFORE
    // releasing hydration. No delivery lock is held across engine callbacks.
    population::reset_with(|| {
        {
            let mut state = delivery_state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.reset_in_progress = true;
        }
        // If this unwinds, leave admission closed. Poison recovery alone is
        // not evidence of a complete reset; a subsequent successful reset heals it.
        reset_engine();
        *delivery_state()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            DeliveryState::new(probe_interval_micros);
    });
}

/// Initialize planning, hydration, and process-local delivery admission together.
pub fn init_global_atc(config: &mcp_agent_mail_core::Config) {
    reset_with_delivery(AtcEngine::config_from_env(config).probe_interval_micros, || {
        engine::init_global_atc(config);
    });
}

#[cfg(test)]
pub(crate) fn reset_global_atc_state_for_test(config: &mcp_agent_mail_core::Config) {
    reset_with_delivery(AtcEngine::config_from_env(config).probe_interval_micros, || {
        engine::reset_global_atc_state_for_test(config);
    });
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
/// Inference yields while a population refresh is incomplete. A concurrent reset
/// cancels this call's publication, including critical reservation actions; their
/// optional-mail exemption is not permission to cross a planning reset.
#[must_use]
pub fn atc_tick_report(now_micros: i64) -> Option<AtcTickReport> {
    let epoch = capture_delivery_epoch()?;
    let report = population::tick_report(now_micros)?;
    admit_report(&epoch, report, now_micros)
}

fn admit_report(
    epoch: &Arc<()>,
    report: AtcTickReport,
    now_micros: i64,
) -> Option<AtcTickReport> {
    admit_report_with_activity(epoch, report, now_micros, engine::atc_agent_last_activity)
}

fn admit_report_with_activity(
    epoch: &Arc<()>,
    mut report: AtcTickReport,
    now_micros: i64,
    mut last_activity: impl FnMut(&str) -> Option<i64>,
) -> Option<AtcTickReport> {
    if !delivery_epoch_is_current(epoch) {
        return None;
    }
    // Never acquire the engine mutex while owning delivery admission. Engine
    // diagnostics can themselves inspect delivery metrics. These observations
    // only classify passive liveness counters; they authorize no mail or release.
    let mut activity_by_agent = HashMap::new();
    for effect in &report.effects {
        if matches!(
            notification_class(
                &effect.kind,
                &effect.semantics.family,
                effect.semantics.high_risk_intervention,
            ),
            Some(NotificationClass::Probe | NotificationClass::Liveness)
        ) {
            activity_by_agent
                .entry(effect.agent.clone())
                .or_insert_with(|| last_activity(&effect.agent));
        }
    }
    let mut state = delivery_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !state.accepts_epoch(epoch) {
        // Do not start a tick, consume cooldowns, move fairness, or overwrite
        // the successor epoch's summary for a discarded planning result.
        return None;
    }
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
            |agent| activity_by_agent.get(agent).copied().flatten(),
        );
    }
    report.summary.kernel.pending_effects = report.effects.len();
    state.last_tick_count = Some(report.summary.tick_count);
    state.last_effect_count = report.effects.len();
    let after = state.admission.stats();
    drop(state);
    // Subscribers may inspect delivery metrics. Never invoke them under the
    // admission mutex, including the first capacity warning.
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
    let epoch = capture_delivery_epoch()?;
    let mut summary = engine::atc_summary()?;
    finish_summary(&epoch, &mut summary)?;
    Some(summary)
}

fn finish_summary(epoch: &Arc<()>, summary: &mut AtcSummarySnapshot) -> Option<()> {
    let state = delivery_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !state.accepts_epoch(epoch) {
        return None;
    }
    if state.last_tick_count == Some(summary.tick_count) {
        summary.kernel.pending_effects = state.last_effect_count;
    }
    drop(state);
    population::annotate_summary(summary);
    // Annotation takes a different lock. A reset between those observations
    // must not combine one engine's summary with another population's status.
    delivery_epoch_is_current(epoch).then_some(())
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

    fn enabled_config() -> mcp_agent_mail_core::Config {
        mcp_agent_mail_core::Config {
            atc_enabled: true,
            atc_probe_interval_secs: 1,
            ..Default::default()
        }
    }

    fn staged_report(now: i64) -> AtcTickReport {
        let mut report = population::tick_report(now).expect("enabled planning report");
        let release = AtcEffectPlan {
            decision_id: 1,
            effect_id: "release:BlueFox".into(),
            experience_id: None,
            claim_id: "claim".into(),
            evidence_id: "evidence".into(),
            trace_id: "trace".into(),
            timestamp_micros: now,
            kind: "release_reservations_requested".into(),
            category: "coordination".into(),
            agent: "BlueFox".into(),
            project_key: Some("/project".into()),
            policy_id: None,
            policy_revision: 1,
            message: None,
            expected_loss: None,
            semantics: AtcEffectSemantics {
                family: "reservation_release".into(),
                risk_level: "high".into(),
                utility_model: "test".into(),
                operator_action: "test".into(),
                remediation: "test".into(),
                escalation_policy: "test".into(),
                evidence_summary: "test".into(),
                cooldown_key: "release:project:BlueFox".into(),
                cooldown_micros: 1_000_000,
                requires_project: true,
                ack_required: false,
                high_risk_intervention: true,
                preconditions: Vec::new(),
            },
        };
        report.effects = vec![release];
        report.actions = vec![AtcTickAction::ReleaseReservations {
            agent: "BlueFox".into(),
        }];
        report
    }

    #[test]
    fn pre_reset_critical_report_cannot_enter_successor_admission() {
        let _guard = GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = enabled_config();
        reset_global_atc_state_for_test(&config);
        let old_epoch = capture_delivery_epoch().unwrap();
        let old_report = staged_report(200_000_000);
        assert_eq!(old_report.effects.len(), 1);
        assert_eq!(old_report.actions.len(), 1);
        reset_global_atc_state_for_test(&config);
        let new_epoch = capture_delivery_epoch().unwrap();
        assert!(!Arc::ptr_eq(&old_epoch, &new_epoch));
        assert!(admit_report(&old_epoch, old_report, 201_000_000).is_none());
        {
            let state = delivery_state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(state.last_tick_count.is_none());
            assert!(state.last_notification_key.is_none());
            assert_eq!(state.last_effect_count, 0);
            assert_eq!(state.admission.stats(), AtcDeliveryStats::default());
        }
        // This is an actual report from the replacement engine, not an old
        // report silently rebound to a fresh token. Critical work still runs.
        let fresh = admit_report(&new_epoch, staged_report(202_000_000), 202_000_000)
            .expect("current epoch admission");
        assert_eq!(fresh.effects.len(), 1);
        assert_eq!(fresh.actions.len(), 1);
        reset_global_atc_state_for_test(&config);
    }

    #[test]
    fn in_progress_reset_is_inert_without_holding_the_delivery_mutex() {
        let _guard = GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = enabled_config();
        reset_global_atc_state_for_test(&config);
        let epoch = capture_delivery_epoch().unwrap();
        let report = staged_report(200_000_000);
        reset_with_delivery(1_000_000, || {
            assert!(capture_delivery_epoch().is_none());
            assert!(!delivery_epoch_is_current(&epoch));
            assert!(admit_report(&epoch, report, 201_000_000).is_none());
            // These must stop before acquiring the hydration/engine locks
            // already owned by reset. Metrics remain callable by observers.
            assert!(atc_tick_report(201_000_000).is_none());
            assert!(atc_tick(201_000_000).is_empty());
            assert!(atc_summary().is_none());
            assert_eq!(atc_delivery_stats(), AtcDeliveryStats::default());
            engine::reset_global_atc_state_for_test(&config);
        });
        assert!(capture_delivery_epoch().is_some());
        assert!(atc_tick_report(202_000_000).is_some());
        reset_global_atc_state_for_test(&config);
    }

    #[test]
    fn unwound_reset_stays_closed_until_an_entire_reset_succeeds() {
        let _guard = GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = enabled_config();
        reset_global_atc_state_for_test(&config);
        let old_epoch = capture_delivery_epoch().unwrap();
        assert!(std::panic::catch_unwind(|| {
            reset_with_delivery(1_000_000, || panic!("interrupted engine reset"));
        }).is_err());
        for _ in 0..3 {
            assert!(capture_delivery_epoch().is_none());
            assert!(atc_tick_report(200_000_000).is_none());
            assert!(atc_summary().is_none());
            assert!(!delivery_epoch_is_current(&old_epoch));
        }
        reset_global_atc_state_for_test(&config);
        let replacement = capture_delivery_epoch().unwrap();
        assert!(!Arc::ptr_eq(&old_epoch, &replacement));
        assert!(atc_tick_report(201_000_000).is_some());
        reset_global_atc_state_for_test(&config);
    }

    #[test]
    fn equal_tick_counts_do_not_join_summaries_across_reset() {
        let _guard = GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = enabled_config();
        reset_global_atc_state_for_test(&config);
        let epoch = capture_delivery_epoch().unwrap();
        let mut summary = engine::atc_summary().expect("enabled summary");
        let original_pending = summary.kernel.pending_effects;
        reset_global_atc_state_for_test(&config);
        {
            let mut state = delivery_state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.last_tick_count = Some(summary.tick_count);
            state.last_effect_count = 77;
        }
        assert!(finish_summary(&epoch, &mut summary).is_none());
        assert_eq!(summary.kernel.pending_effects, original_pending);
        reset_global_atc_state_for_test(&config);
    }

    fn passive_report(now: i64) -> AtcTickReport {
        let mut report = staged_report(now);
        let effect = &mut report.effects[0];
        effect.kind = "probe_agent".into();
        effect.semantics.family = "liveness_probe".into();
        effect.semantics.high_risk_intervention = false;
        report.actions = vec![AtcTickAction::ProbeAgent { agent: "BlueFox".into() }];
        report
    }

    #[test]
    fn activity_observer_can_read_metrics_and_reset_before_admission() {
        let _guard = GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = enabled_config();
        reset_global_atc_state_for_test(&config);
        let epoch = capture_delivery_epoch().unwrap();
        let report = passive_report(200_000_000);
        let mut observed = 0;
        assert!(admit_report_with_activity(&epoch, report, 200_000_000, |_| {
            // An engine-side observer can reenter delivery without reversing
            // the engine/admission lock order. Check before calling metrics so
            // a regression fails instead of deliberately deadlocking this test.
            match delivery_state().try_lock() {
                Ok(guard) => drop(guard),
                Err(std::sync::TryLockError::Poisoned(error)) => drop(error.into_inner()),
                Err(std::sync::TryLockError::WouldBlock) => panic!("activity under delivery lock"),
            }
            assert_eq!(atc_delivery_stats(), AtcDeliveryStats::default());
            observed += 1;
            reset_global_atc_state_for_test(&config);
            Some(1_000_000)
        }).is_none());
        assert_eq!(observed, 1);
        assert_eq!(atc_delivery_stats(), AtcDeliveryStats::default());
        assert!(!delivery_epoch_is_current(&epoch));
        assert!(atc_tick_report(201_000_000).is_some());
        reset_global_atc_state_for_test(&config);
    }

    #[test]
    fn stalled_activity_lookup_does_not_block_reset_or_publish_after_it() {
        let _guard = GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = enabled_config();
        reset_global_atc_state_for_test(&config);
        let epoch = capture_delivery_epoch().unwrap();
        let report = passive_report(200_000_000);
        std::thread::scope(|scope| {
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (resume_tx, resume_rx) = std::sync::mpsc::channel();
            let worker = scope.spawn(move || {
                admit_report_with_activity(&epoch, report, 200_000_000, |_| {
                    started_tx.send(()).unwrap();
                    resume_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
                    Some(1_000_000)
                })
            });
            started_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
            // This must complete while the old lookup is suspended. The
            // scoped worker is joined on success and failure, never detached.
            reset_global_atc_state_for_test(&config);
            resume_tx.send(()).unwrap();
            assert!(worker.join().unwrap().is_none());
        });
        assert_eq!(atc_delivery_stats(), AtcDeliveryStats::default());
        assert!(atc_tick_report(201_000_000).is_some());
        reset_global_atc_state_for_test(&config);
    }

    #[test]
    fn failed_activity_observation_does_not_charge_or_poison_admission() {
        let _guard = GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = enabled_config();
        reset_global_atc_state_for_test(&config);
        let epoch = capture_delivery_epoch().unwrap();
        let report = passive_report(200_000_000);
        let was_poisoned = delivery_state().is_poisoned();
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            admit_report_with_activity(&epoch, report, 200_000_000, |_| {
                panic!("interrupted activity observation")
            })
        })).is_err());
        assert_eq!(delivery_state().is_poisoned(), was_poisoned);
        assert!(delivery_epoch_is_current(&epoch));
        assert_eq!(atc_delivery_stats(), AtcDeliveryStats::default());
        let state = delivery_state()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(state.last_tick_count.is_none());
        drop(state);
        assert!(atc_tick_report(201_000_000).is_some());
        reset_global_atc_state_for_test(&config);
    }

    #[test]
    fn already_stale_report_skips_activity_observation_entirely() {
        let _guard = GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = enabled_config();
        reset_global_atc_state_for_test(&config);
        let old_epoch = capture_delivery_epoch().unwrap();
        let report = passive_report(200_000_000);
        reset_global_atc_state_for_test(&config);
        assert!(admit_report_with_activity(&old_epoch, report, 201_000_000, |_| {
            panic!("stale report must not query the replacement engine")
        }).is_none());
        assert_eq!(atc_delivery_stats(), AtcDeliveryStats::default());
        reset_global_atc_state_for_test(&config);
    }

    #[test]
    fn passive_activity_is_sampled_once_per_agent_and_never_for_releases() {
        let _guard = GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = enabled_config();
        reset_global_atc_state_for_test(&config);
        let epoch = capture_delivery_epoch().unwrap();
        let mut report = passive_report(200_000_000);
        let duplicate = passive_report(201_000_000);
        report.effects.extend(duplicate.effects);
        report.actions.extend(duplicate.actions);
        let mut calls = 0;
        let passive = admit_report_with_activity(&epoch, report, 202_000_000, |agent| {
            assert_eq!(agent, "BlueFox");
            calls += 1;
            Some(1_000_000)
        }).unwrap();
        assert_eq!(calls, 1);
        assert!(passive.effects.is_empty() && passive.actions.is_empty());
        assert_eq!(atc_delivery_stats().passive_liveness, 2);
        let critical = admit_report_with_activity(
            &epoch, staged_report(203_000_000), 203_000_000,
            |_| panic!("release admission must not load passive activity"),
        ).unwrap();
        assert_eq!(critical.effects.len(), 1);
        assert_eq!(critical.actions.len(), 1);
        reset_global_atc_state_for_test(&config);
    }

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
