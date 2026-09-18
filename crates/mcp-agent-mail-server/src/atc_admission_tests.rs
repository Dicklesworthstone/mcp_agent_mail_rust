//! Regressions through the actual report-admission boundary, not a model of it.

use super::*;
use std::collections::HashSet;

const SECOND: i64 = 1_000_000;

fn effect(agent: &str, family: &str) -> AtcEffectPlan {
    AtcEffectPlan {
        decision_id: 1,
        effect_id: format!("effect:{family}:{agent}"),
        experience_id: None,
        claim_id: "claim".into(),
        evidence_id: "evidence".into(),
        trace_id: "trace".into(),
        timestamp_micros: SECOND,
        kind: if family == "liveness_probe" {
            "probe_agent"
        } else {
            "send_advisory"
        }
        .into(),
        category: "coordination".into(),
        agent: agent.into(),
        project_key: Some("project".into()),
        policy_id: None,
        policy_revision: 1,
        message: Some(format!("{family}:{agent}")),
        expected_loss: None,
        semantics: AtcEffectSemantics {
            family: family.into(),
            risk_level: "low".into(),
            utility_model: "test".into(),
            operator_action: "test".into(),
            remediation: "test".into(),
            escalation_policy: "test".into(),
            evidence_summary: "test".into(),
            cooldown_key: format!("{family}:project:{agent}"),
            cooldown_micros: 1,
            requires_project: true,
            ack_required: false,
            high_risk_intervention: false,
            preconditions: Vec::new(),
        },
    }
}

fn actions(effects: &[AtcEffectPlan]) -> Vec<AtcTickAction> {
    effects
        .iter()
        .filter_map(|effect| match effect.kind.as_str() {
            "probe_agent" => Some(AtcTickAction::ProbeAgent {
                agent: effect.agent.clone(),
            }),
            "send_advisory" => Some(AtcTickAction::SendAdvisory {
                agent: effect.agent.clone(),
                message: effect.message.clone().expect("fixture advisory body"),
            }),
            "release_reservations_requested" => Some(AtcTickAction::ReleaseReservations {
                agent: effect.agent.clone(),
            }),
            _ => None,
        })
        .collect()
}

#[test]
fn recurring_conflicts_cannot_starve_the_tail_of_a_stable_population() {
    const POPULATION: usize = 940;
    let mut admission = NotificationAdmission::new(0);
    let mut cursor = None;
    let mut seen = HashSet::new();
    for tick in 0..POPULATION.div_ceil(delivery::MAX_NOTIFICATIONS_PER_TICK) {
        // All cooldowns expire before every tick. Deduplication alone cannot
        // prevent the old source-order scheduler from choosing Agent0000..0015
        // forever. Change decision IDs too: fairness is keyed by intent.
        let now = (200 + i64::try_from(tick).unwrap() * 16) * SECOND;
        admission.begin_tick(now);
        let mut effects: Vec<_> = (0..POPULATION)
            .map(|agent| {
                let mut proposal = effect(&format!("Agent{agent:04}"), "deadlock_remediation");
                proposal.decision_id = u64::try_from(tick).unwrap() + 1;
                proposal
            })
            .collect();
        let mut actions = actions(&effects);
        admit_effects(&mut effects, &mut actions, &mut admission, &mut cursor, now, |_| {
            panic!("conflict admission must not consult liveness");
        });
        assert!(effects.len() <= delivery::MAX_NOTIFICATIONS_PER_TICK);
        assert_eq!(actions.len(), effects.len());
        for effect in effects {
            seen.insert(effect.agent);
        }
    }
    assert_eq!(seen.len(), POPULATION);
}

#[test]
fn cursor_survives_reordering_and_removal_without_reordering_effects() {
    let mut admission = NotificationAdmission::new(0);
    let mut cursor = Some("deadlock_remediation:project:Agent0015".to_string());
    admission.begin_tick(200 * SECOND);
    // Previous recipient 0015 is absent, and planner order is now reversed.
    let mut effects: Vec<_> = (20..52)
        .rev()
        .map(|agent| effect(&format!("Agent{agent:04}"), "deadlock_remediation"))
        .collect();
    let mut actions = actions(&effects);
    admit_effects(&mut effects, &mut actions, &mut admission, &mut cursor, 200 * SECOND, |_| None);
    let actual: Vec<_> = effects.iter().map(|effect| effect.agent.clone()).collect();
    let expected: Vec<_> = (20..36).rev().map(|agent| format!("Agent{agent:04}")).collect();
    assert_eq!(actual, expected, "admission order must not leak into execution order");
    assert_eq!(cursor.as_deref(), Some("deadlock_remediation:project:Agent0035"));
    assert_eq!(actions.len(), expected.len());
}

#[test]
fn exhausted_admission_preserves_mutations_outcome_notices_and_unknown_effects_in_order() {
    let mut admission = NotificationAdmission::new(0);
    let mut cursor = None;
    let now = 200 * SECOND;
    admission.begin_tick(now);
    let mut fill: Vec<_> = (0..delivery::MAX_NOTIFICATIONS_PER_TICK)
        .map(|agent| effect(&format!("Fill{agent:04}"), "deadlock_remediation"))
        .collect();
    let mut fill_actions = actions(&fill);
    admit_effects(&mut fill, &mut fill_actions, &mut admission, &mut cursor, now, |_| None);
    let previous_cursor = cursor.clone();

    let mut release = effect("BlueFox", "reservation_release");
    release.kind = "release_reservations_requested".into();
    release.semantics.high_risk_intervention = true;
    let mut unknown = effect("BlueFox", "future_family");
    unknown.kind = "future_mutation".into();
    let mut effects = vec![
        release,
        effect("BlueFox", "liveness_probe"),
        effect("BlueFox", "release_notice"),
        unknown,
        effect("BlueFox", "liveness_monitoring"),
    ];
    let expected: Vec<_> = [0, 2, 3].into_iter().map(|i| effects[i].effect_id.clone()).collect();
    let mut actions = actions(&effects);
    admit_effects(&mut effects, &mut actions, &mut admission, &mut cursor, now, |_| Some(SECOND));
    assert_eq!(effects.iter().map(|effect| effect.effect_id.clone()).collect::<Vec<_>>(), expected);
    assert_eq!(actions.len(), 2);
    assert!(matches!(actions[0], AtcTickAction::ReleaseReservations { .. }));
    assert!(matches!(&actions[1], AtcTickAction::SendAdvisory { message, .. } if message == "release_notice:BlueFox"));
    assert_eq!(cursor, previous_cursor, "bypassed effects do not consume scheduling priority");
}

#[test]
fn liveness_families_share_one_activity_lookup_and_conflicts_need_none() {
    let mut admission = NotificationAdmission::new(0);
    let mut cursor = None;
    let now = 200 * SECOND;
    admission.begin_tick(now);
    let mut effects = vec![
        effect("BlueFox", "liveness_probe"),
        effect("BlueFox", "liveness_monitoring"),
        effect("RedFox", "deadlock_remediation"),
        effect("GreenFox", "release_notice"),
    ];
    let mut actions = actions(&effects);
    let mut lookups = Vec::new();
    admit_effects(&mut effects, &mut actions, &mut admission, &mut cursor, now, |agent| {
        lookups.push(agent.to_owned());
        Some(SECOND)
    });
    assert_eq!(lookups, ["BlueFox"]);
    assert_eq!(effects.len(), 2);
    assert_eq!(actions.len(), 2);
    assert_eq!(admission.stats().passive_liveness, 2);
    assert_eq!(cursor.as_deref(), Some("deadlock_remediation:project:RedFox"));
    assert!(effects.iter().all(|effect| {
        matches!(effect.semantics.family.as_str(), "deadlock_remediation" | "release_notice")
    }));
}

#[test]
fn duplicate_effects_keep_exact_action_multiplicity_after_cursor_wrap() {
    let mut admission = NotificationAdmission::new(0);
    let mut cursor = Some("zzzz".to_string());
    let now = 200 * SECOND;
    admission.begin_tick(now);
    let mut effects = vec![effect("BlueFox", "deadlock_remediation"); 3];
    let mut actions = actions(&effects);
    admit_effects(&mut effects, &mut actions, &mut admission, &mut cursor, now, |_| None);
    assert_eq!(effects.len(), 1);
    assert_eq!(actions.len(), 1);
    assert_eq!(admission.stats().duplicate, 2);
    assert_eq!(cursor.as_deref(), Some("deadlock_remediation:project:BlueFox"));
}

#[test]
fn empty_and_unobserved_batches_do_not_advance_the_fairness_cursor() {
    let mut admission = NotificationAdmission::new(0);
    let mut cursor = Some("liveness_probe:project:BlueFox".to_string());
    let expected = cursor.clone();
    let now = 200 * SECOND;
    admission.begin_tick(now);
    for mut effects in [Vec::new(), vec![effect("RedFox", "liveness_probe")]] {
        let mut actions = actions(&effects);
        admit_effects(&mut effects, &mut actions, &mut admission, &mut cursor, now, |_| None);
        assert!(effects.is_empty());
        assert!(actions.is_empty());
        assert_eq!(cursor, expected);
    }
    assert_eq!(admission.stats().no_activity, 1);
    assert_eq!(admission.stats().admitted, 0);
}

#[test]
fn high_risk_notification_shaped_effects_bypass_the_scheduler() {
    let mut admission = NotificationAdmission::new(0);
    let mut cursor = None;
    // No begin_tick: optional notification budget is zero.
    let mut critical = effect("BlueFox", "liveness_monitoring");
    critical.semantics.high_risk_intervention = true;
    let mut effects = vec![critical];
    let mut actions = actions(&effects);
    admit_effects(&mut effects, &mut actions, &mut admission, &mut cursor, 200 * SECOND, |_| {
        panic!("critical effect must not consult notification activity");
    });
    assert_eq!(effects.len(), 1);
    assert_eq!(actions.len(), 1);
    assert!(cursor.is_none());
    assert_eq!(admission.stats().admitted, 0);
}
