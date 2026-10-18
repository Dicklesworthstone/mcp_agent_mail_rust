//! Authorization of automatic ATC proposals against one database population.
//!
//! The inference engine retains historical agents and currently keys its state
//! by name. Neither a remembered agent nor a project on a proposed effect is
//! authority to act. The installed population is a bounded observation, not an
//! exhaustive census: absence (including truncation by the population limit)
//! withholds automation; it never means the agent died or should be deleted.
//!
//! Names observed in multiple project identities are unresolved here. Do not
//! choose whichever row happened to hydrate last. Fully project-scoped inference
//! remains separate work; this gate prevents that ambiguity reaching execution.
//! Release requests also need activity evidence no newer than the proposal.
//! A fresh population can revoke a queued release without retracting notices
//! about effects that already happened. A notice paired with a request in this
//! report is different: it must not announce a request that scope withdrew.
//! Execution must still revalidate changes that arrive after this snapshot.

use super::engine::{AtcEffectPlan, AtcTickAction};
use mcp_agent_mail_db::models::AtcPopulationAgentRow;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectIdentity {
    id: i64,
    key: String,
}

#[derive(Debug, Clone)]
struct AgentObservation {
    project: ProjectIdentity,
    last_active_ts: i64,
}

#[derive(Debug, Default)]
pub(super) struct PopulationScope {
    // None is an invalid or ambiguous name, not a missing project wildcard.
    // The map is replaced per successful refresh, never accumulated over time.
    by_name: HashMap<String, Option<AgentObservation>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct ScopeDisposition {
    pub effects_withheld: usize,
    pub actions_withheld: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ActionKey {
    Advisory(String, String),
    Release(String),
    Probe(String),
}

/// Release and notice variants have different effect IDs but share their
/// decision identity. Do not associate unrelated notices by agent or copy alone.
#[derive(Debug, PartialEq, Eq, Hash)]
struct ReleaseFlow {
    decision_id: u64,
    trace_id: String,
    agent: String,
    project_key: Option<String>,
}

impl ReleaseFlow {
    fn from_effect(effect: &AtcEffectPlan) -> Self {
        Self {
            decision_id: effect.decision_id,
            trace_id: effect.trace_id.clone(),
            agent: effect.agent.clone(),
            project_key: effect
                .project_key
                .as_deref()
                .map(str::trim)
                .map(str::to_string),
        }
    }
}

impl PopulationScope {
    pub(super) fn from_rows(rows: &[AtcPopulationAgentRow]) -> Self {
        let mut by_name: HashMap<String, Option<AgentObservation>> =
            HashMap::with_capacity(rows.len());
        for row in rows {
            let project = row.project_key.trim();
            let identity = (!row.name.trim().is_empty()
                && row.project_id > 0
                && !project.is_empty())
            .then(|| ProjectIdentity {
                id: row.project_id,
                key: project.to_string(),
            });
            let observation = identity.map(|project| AgentObservation {
                project,
                last_active_ts: row.last_active_ts,
            });
            by_name
                .entry(row.name.clone())
                .and_modify(|previous| match (previous.as_mut(), observation.as_ref()) {
                    (Some(previous), Some(next)) if previous.project == next.project => {
                        // Duplicate rows are the same identity even when their
                        // activity differs. Never let row order weaken evidence.
                        previous.last_active_ts = previous.last_active_ts.max(next.last_active_ts);
                    }
                    _ => *previous = None,
                })
                .or_insert(observation);
        }
        Self { by_name }
    }

    pub(super) fn unresolved_names(&self) -> usize {
        self.by_name
            .values()
            .filter(|identity| identity.is_none())
            .count()
    }

    pub(super) fn permits(&self, name: &str, project_key: Option<&str>) -> bool {
        let Some(project) = project_key.map(str::trim).filter(|key| !key.is_empty()) else {
            return false;
        };
        matches!(self.by_name.get(name), Some(Some(observation)) if observation.project.key == project)
    }

    fn permits_effect(&self, effect: &AtcEffectPlan) -> bool {
        if !self.permits(&effect.agent, effect.project_key.as_deref()) {
            return false;
        }
        if effect.kind != "release_reservations_requested" {
            // This gate revokes release requests, not completed outcome notices
            // or unrelated proposals. Their identity scope is still required.
            return true;
        }
        self.by_name
            .get(&effect.agent)
            .and_then(Option::as_ref)
            .is_some_and(|observation| {
                observation.last_active_ts > 0
                    && observation.last_active_ts <= effect.timestamp_micros
            })
    }

    pub(super) fn retain_authorized(
        &self,
        effects: &mut Vec<AtcEffectPlan>,
        actions: &mut Vec<AtcTickAction>,
    ) -> ScopeDisposition {
        let effect_count = effects.len();
        let action_count = actions.len();
        // The engine emits a release_notice beside its proposed request, before
        // any reservation mutation. Preserve the original decision pairing
        // before filtering: a withdrawn request must not leave its "requested"
        // advisory behind. Independent outcome notices remain independent.
        // Any authorized instance keeps that flow alive, regardless of order.
        // This map is bounded by this report's requests, not lifetime history.
        let mut release_flows: HashMap<ReleaseFlow, bool> = HashMap::new();
        for effect in effects.iter() {
            if effect.kind == "release_reservations_requested" {
                let permitted = self.permits_effect(effect);
                *release_flows
                    .entry(ReleaseFlow::from_effect(effect))
                    .or_insert(false) |= permitted;
            }
        }
        // Scope validation is separate from optional-notification budgeting.
        // High-risk releases and future effect kinds do not bypass authority.
        effects.retain(|effect| {
            self.permits_effect(effect)
                && (effect.kind != "send_advisory"
                    || effect.semantics.family != "release_notice"
                    || release_flows
                        .get(&ReleaseFlow::from_effect(effect))
                        .copied()
                        .unwrap_or(true))
        });

        let mut admitted = HashMap::new();
        for effect in effects.iter() {
            let key = match effect.kind.as_str() {
                "send_advisory" => effect
                    .message
                    .as_ref()
                    .map(|body| ActionKey::Advisory(effect.agent.clone(), body.clone())),
                "release_reservations_requested" => Some(ActionKey::Release(effect.agent.clone())),
                "probe_agent" => Some(ActionKey::Probe(effect.agent.clone())),
                _ => None,
            };
            if let Some(key) = key {
                *admitted.entry(key).or_insert(0_usize) += 1;
            }
        }
        // Legacy actions contain no project. They may survive only as the
        // exact counterpart of a retained, unambiguously scoped effect. Count
        // multiplicity so one authorized effect cannot authorize ten actions.
        actions.retain(|action| {
            let key = match action {
                AtcTickAction::SendAdvisory { agent, message } => {
                    ActionKey::Advisory(agent.clone(), message.clone())
                }
                AtcTickAction::ReleaseReservations { agent } => ActionKey::Release(agent.clone()),
                AtcTickAction::ProbeAgent { agent } => ActionKey::Probe(agent.clone()),
            };
            let Some(remaining) = admitted.get_mut(&key) else {
                return false;
            };
            if *remaining == 0 {
                return false;
            }
            *remaining -= 1;
            true
        });
        ScopeDisposition {
            effects_withheld: effect_count - effects.len(),
            actions_withheld: action_count - actions.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::engine::{self, AtcEffectSemantics};
    use super::super::{
        AtcPopulationHydrationStats, HydrationState, atc_population_hydration_stats, hydration,
        sync_population_with,
    };
    use super::*;
    use crate::atc;

    fn row(name: &str, id: i64, project: &str) -> AtcPopulationAgentRow {
        AtcPopulationAgentRow {
            project_id: id,
            project_key: project.into(),
            name: name.into(),
            program: "codex-cli".into(),
            last_active_ts: 1_000_000,
        }
    }

    fn effect(name: &str, project: Option<&str>, kind: &str, body: Option<&str>) -> AtcEffectPlan {
        AtcEffectPlan {
            decision_id: 1,
            effect_id: format!("effect:{name}:{kind}"),
            experience_id: None,
            claim_id: "claim".into(),
            evidence_id: "evidence".into(),
            trace_id: "trace".into(),
            timestamp_micros: 1_000_000,
            kind: kind.into(),
            category: "coordination".into(),
            agent: name.into(),
            project_key: project.map(str::to_string),
            policy_id: None,
            policy_revision: 1,
            message: body.map(str::to_string),
            expected_loss: None,
            semantics: AtcEffectSemantics {
                family: "reservation_release".into(),
                risk_level: "high".into(),
                utility_model: "test".into(),
                operator_action: "test".into(),
                remediation: "test".into(),
                escalation_policy: "test".into(),
                evidence_summary: "test".into(),
                cooldown_key: "test".into(),
                cooldown_micros: 1,
                requires_project: true,
                ack_required: false,
                high_risk_intervention: true,
                preconditions: Vec::new(),
            },
        }
    }

    #[test]
    fn only_the_current_project_and_agent_pair_is_authorized() {
        let scope = PopulationScope::from_rows(&[row("BlueLake", 1, "/alpha")]);
        assert!(scope.permits("BlueLake", Some("/alpha")));
        for (name, project) in [
            ("BlueLake", Some("/beta")),
            ("RetiredAgent", Some("/alpha")),
            ("BlueLake", None),
            ("BlueLake", Some("")),
            ("BlueLake", Some("  ")),
        ] {
            assert!(!scope.permits(name, project));
        }
    }

    #[test]
    fn empty_population_never_authorizes_a_remembered_release() {
        let scope = PopulationScope::from_rows(&[]);
        let mut effects = vec![effect(
            "RetiredAgent",
            Some("/alpha"),
            "release_reservations_requested",
            None,
        )];
        let mut actions = vec![AtcTickAction::ReleaseReservations {
            agent: "RetiredAgent".into(),
        }];
        assert_eq!(
            scope.retain_authorized(&mut effects, &mut actions),
            ScopeDisposition {
                effects_withheld: 1,
                actions_withheld: 1,
            }
        );
        assert!(effects.is_empty() && actions.is_empty());
    }

    #[test]
    fn ambiguity_is_order_independent_and_cannot_be_overwritten_by_a_duplicate() {
        let a = row("BlueLake", 1, "/alpha");
        let b = row("BlueLake", 2, "/beta");
        for rows in [vec![a.clone(), b.clone(), a.clone()], vec![b.clone(), a, b]] {
            let scope = PopulationScope::from_rows(&rows);
            assert_eq!(scope.unresolved_names(), 1);
            assert!(!scope.permits("BlueLake", Some("/alpha")));
            assert!(!scope.permits("BlueLake", Some("/beta")));
        }
    }

    #[test]
    fn identical_duplicates_are_not_cross_project_ambiguity() {
        let a = row("BlueLake", 1, " /alpha ");
        let mut repeated = a.clone();
        repeated.last_active_ts += 1;
        let scope = PopulationScope::from_rows(&[a, repeated]);
        assert_eq!(scope.unresolved_names(), 0);
        assert!(scope.permits("BlueLake", Some("/alpha")));
    }

    #[test]
    fn numeric_project_identity_and_project_key_must_agree() {
        for other in [row("BlueLake", 2, "/alpha"), row("BlueLake", 1, "/renamed")] {
            let scope = PopulationScope::from_rows(&[row("BlueLake", 1, "/alpha"), other]);
            assert_eq!(scope.unresolved_names(), 1);
            assert!(!scope.permits("BlueLake", Some("/alpha")));
        }
    }

    #[test]
    fn invalid_snapshot_identity_is_not_a_wildcard() {
        for invalid in [
            row("BlueLake", 0, "/alpha"),
            row("BlueLake", 1, " "),
            row(" ", 1, "/alpha"),
        ] {
            let scope = PopulationScope::from_rows(std::slice::from_ref(&invalid));
            assert_eq!(scope.unresolved_names(), 1);
            assert!(!scope.permits(&invalid.name, Some("/alpha")));
        }
    }

    #[test]
    fn high_risk_and_unknown_effects_still_require_population_authority() {
        let scope = PopulationScope::from_rows(&[row("BlueLake", 1, "/alpha")]);
        for kind in [
            "release_reservations_requested",
            "future_mutation",
            "send_advisory",
        ] {
            let mut effects = vec![
                effect("BlueLake", Some("/beta"), kind, Some("notice")),
                effect("BlueLake", Some("/alpha"), kind, Some("notice")),
            ];
            let mut actions = Vec::new();
            let result = scope.retain_authorized(&mut effects, &mut actions);
            assert_eq!(result.effects_withheld, 1);
            assert_eq!(effects.len(), 1);
            assert_eq!(effects[0].project_key.as_deref(), Some("/alpha"));
        }
    }

    #[test]
    fn all_legacy_actions_need_an_exact_retained_effect() {
        let scope = PopulationScope::from_rows(&[row("BlueLake", 1, "/alpha")]);
        let mut effects = vec![effect(
            "BlueLake",
            Some("/alpha"),
            "send_advisory",
            Some("authorized notice"),
        )];
        let mut actions = vec![
            AtcTickAction::SendAdvisory {
                agent: "BlueLake".into(),
                message: "different notice".into(),
            },
            AtcTickAction::ReleaseReservations {
                agent: "BlueLake".into(),
            },
            AtcTickAction::ProbeAgent {
                agent: "BlueLake".into(),
            },
            AtcTickAction::SendAdvisory {
                agent: "BlueLake".into(),
                message: "authorized notice".into(),
            },
        ];
        let result = scope.retain_authorized(&mut effects, &mut actions);
        assert_eq!(result.actions_withheld, 3);
        assert_eq!(actions.len(), 1);
    }

    #[test]
    fn one_scoped_release_cannot_authorize_duplicate_legacy_releases() {
        let scope = PopulationScope::from_rows(&[row("BlueLake", 1, "/alpha")]);
        let mut effects = vec![effect(
            "BlueLake",
            Some("/alpha"),
            "release_reservations_requested",
            None,
        )];
        let mut actions = vec![
            AtcTickAction::ReleaseReservations {
                agent: "BlueLake".into(),
            };
            100
        ];
        assert_eq!(
            scope
                .retain_authorized(&mut effects, &mut actions)
                .actions_withheld,
            99
        );
        assert_eq!(actions.len(), 1);
    }

    #[test]
    fn legitimate_release_probe_and_notices_keep_their_order() {
        let scope = PopulationScope::from_rows(&[row("BlueLake", 1, "/alpha")]);
        let mut effects = vec![
            effect(
                "BlueLake",
                Some("/alpha"),
                "release_reservations_requested",
                None,
            ),
            effect("BlueLake", Some("/alpha"), "send_advisory", Some("outcome")),
            effect("BlueLake", Some("/alpha"), "probe_agent", None),
        ];
        let mut actions = vec![
            AtcTickAction::ReleaseReservations {
                agent: "BlueLake".into(),
            },
            AtcTickAction::SendAdvisory {
                agent: "BlueLake".into(),
                message: "outcome".into(),
            },
            AtcTickAction::ProbeAgent {
                agent: "BlueLake".into(),
            },
        ];
        let before = effects
            .iter()
            .map(|effect| effect.effect_id.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            scope.retain_authorized(&mut effects, &mut actions),
            ScopeDisposition::default()
        );
        assert_eq!(
            effects
                .iter()
                .map(|effect| effect.effect_id.clone())
                .collect::<Vec<_>>(),
            before
        );
        assert!(matches!(
            actions[0],
            AtcTickAction::ReleaseReservations { .. }
        ));
        assert!(matches!(actions[1], AtcTickAction::SendAdvisory { .. }));
        assert!(matches!(actions[2], AtcTickAction::ProbeAgent { .. }));
    }

    #[test]
    fn replacing_population_drops_old_authority_without_accumulating_names() {
        for generation in 0..1_000 {
            let name = format!("Agent{generation}");
            let scope = PopulationScope::from_rows(&[row(&name, 1, "/alpha")]);
            assert_eq!(scope.by_name.len(), 1);
            assert!(scope.permits(&name, Some("/alpha")));
            if generation > 0 {
                assert!(!scope.permits(&format!("Agent{}", generation - 1), Some("/alpha")));
            }
        }
    }

    #[test]
    fn newer_activity_revokes_release_and_legacy_action_but_keeps_outcome_notices() {
        let mut active = row("BlueLake", 1, "/alpha");
        active.last_active_ts += 1;
        let scope = PopulationScope::from_rows(&[active]);
        let mut effects = vec![
            effect(
                "BlueLake",
                Some("/alpha"),
                "release_reservations_requested",
                None,
            ),
            effect(
                "BlueLake",
                Some("/alpha"),
                "send_advisory",
                Some("released earlier"),
            ),
        ];
        let mut actions = vec![
            AtcTickAction::ReleaseReservations {
                agent: "BlueLake".into(),
            },
            AtcTickAction::SendAdvisory {
                agent: "BlueLake".into(),
                message: "released earlier".into(),
            },
        ];
        assert_eq!(
            scope.retain_authorized(&mut effects, &mut actions),
            ScopeDisposition {
                effects_withheld: 1,
                actions_withheld: 1,
            },
        );
        assert_eq!(effects.len(), 1);
        assert_eq!(effects[0].message.as_deref(), Some("released earlier"));
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], AtcTickAction::SendAdvisory { .. }));
    }

    #[test]
    fn release_activity_boundary_is_inclusive_and_does_not_subtract_timestamps() {
        for last_active in [1, 1_000_000, i64::MAX] {
            let mut active = row("BlueLake", 1, "/alpha");
            active.last_active_ts = last_active;
            let scope = PopulationScope::from_rows(&[active]);
            for planned_at in [i64::MIN, -1, 0, last_active - 1, last_active, i64::MAX] {
                let mut release = effect(
                    "BlueLake",
                    Some("/alpha"),
                    "release_reservations_requested",
                    None,
                );
                release.timestamp_micros = planned_at;
                assert_eq!(scope.permits_effect(&release), planned_at >= last_active);
            }
        }
    }

    #[test]
    fn unobserved_activity_never_authorizes_release_but_does_not_erase_identity() {
        for last_active in [i64::MIN, -1, 0] {
            let mut active = row("BlueLake", 1, "/alpha");
            active.last_active_ts = last_active;
            let scope = PopulationScope::from_rows(&[active]);
            assert!(scope.permits("BlueLake", Some("/alpha")));
            assert_eq!(scope.unresolved_names(), 0);
            let mut release = effect(
                "BlueLake",
                Some("/alpha"),
                "release_reservations_requested",
                None,
            );
            release.timestamp_micros = i64::MAX;
            assert!(!scope.permits_effect(&release));
            assert!(scope.permits_effect(&effect(
                "BlueLake",
                Some("/alpha"),
                "send_advisory",
                Some("completed outcome"),
            )));
        }
    }

    #[test]
    fn duplicate_identity_keeps_newest_activity_independently_of_row_order() {
        let old = row("BlueLake", 1, "/alpha");
        let mut new = old.clone();
        new.last_active_ts += 1;
        let release = effect(
            "BlueLake",
            Some("/alpha"),
            "release_reservations_requested",
            None,
        );
        for rows in [
            vec![old.clone(), new.clone(), old.clone()],
            vec![new.clone(), old, new],
        ] {
            let scope = PopulationScope::from_rows(&rows);
            assert_eq!(scope.unresolved_names(), 0);
            assert!(scope.permits("BlueLake", Some("/alpha")));
            assert!(!scope.permits_effect(&release));
        }
    }

    #[test]
    fn activity_only_refresh_revokes_old_proposals_and_preserves_scope_accounting() {
        with_global_atc(|| {
            let mut state = HydrationState::default();
            let old = row("BlueLake", 1, "/alpha");
            state.refresh_with(10, || Ok(vec![old.clone()])).unwrap();
            finish_local_hydration(&mut state);
            let mut original = release_report(&["BlueLake"], "/alpha");
            state.filter_report_scope(&mut original);
            assert_eq!(original.effects.len(), 1);

            let mut new = old;
            new.last_active_ts += 1;
            state.refresh_with(20, || Ok(vec![new])).unwrap();
            assert_eq!(state.progress.unchanged_agents, 0);
            finish_local_hydration(&mut state);
            let mut superseded = release_report(&["BlueLake"], "/alpha");
            state.filter_report_scope(&mut superseded);
            state.annotate(&mut superseded.summary, 20);
            assert!(superseded.effects.is_empty() && superseded.actions.is_empty());
            assert_eq!(superseded.summary.kernel.pending_effects, 0);
            assert_eq!(state.progress.scope_effects_withheld, 1);
            assert_eq!(state.progress.scope_actions_withheld, 1);
            assert_eq!(state.progress.refresh_failures, 0);
            assert_eq!(
                superseded.summary.completeness,
                engine::SnapshotCompleteness::Partial,
            );

            let mut fresh = release_report(&["BlueLake"], "/alpha");
            fresh.effects[0].timestamp_micros += 1;
            state.filter_report_scope(&mut fresh);
            assert_eq!(fresh.effects.len(), 1);
            assert_eq!(fresh.actions.len(), 1);
            assert_eq!(state.progress.last_scope_effects_withheld, 0);
            assert_eq!(state.progress.scope_effects_withheld, 1);
        });
    }

    #[test]
    fn a_940_agent_refresh_revokes_only_superseded_release_proposals_in_order() {
        let rows: Vec<_> = (0..940)
            .map(|index| {
                let mut active = row(&format!("Agent{index:04}"), 1, "/alpha");
                if index % 2 == 0 {
                    active.last_active_ts += 1;
                }
                active
            })
            .collect();
        let scope = PopulationScope::from_rows(&rows);
        let mut effects: Vec<_> = rows
            .iter()
            .map(|row| {
                effect(
                    &row.name,
                    Some("/alpha"),
                    "release_reservations_requested",
                    None,
                )
            })
            .collect();
        let mut actions: Vec<_> = rows
            .iter()
            .map(|row| AtcTickAction::ReleaseReservations {
                agent: row.name.clone(),
            })
            .collect();
        assert_eq!(
            scope.retain_authorized(&mut effects, &mut actions),
            ScopeDisposition {
                effects_withheld: 470,
                actions_withheld: 470,
            },
        );
        let expected: Vec<_> = rows
            .iter()
            .skip(1)
            .step_by(2)
            .map(|row| row.name.as_str())
            .collect();
        assert_eq!(
            effects
                .iter()
                .map(|effect| effect.agent.as_str())
                .collect::<Vec<_>>(),
            expected,
        );
        assert_eq!(actions.len(), 470);
        for (action, expected) in actions.iter().zip(expected) {
            assert!(
                matches!(action, AtcTickAction::ReleaseReservations { agent } if agent == expected)
            );
        }
    }

    fn with_global_atc(run: impl FnOnce()) {
        let _guard = engine::GLOBAL_ATC_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config {
            atc_enabled: true,
            ..Default::default()
        };
        atc::reset_global_atc_state_for_test(&config);
        struct Reset(mcp_agent_mail_core::Config);
        impl Drop for Reset {
            fn drop(&mut self) {
                atc::reset_global_atc_state_for_test(&self.0);
            }
        }
        let _reset = Reset(config);
        run();
    }

    fn release_report(names: &[&str], project: &str) -> engine::AtcTickReport {
        let effects = names
            .iter()
            .map(|name| effect(name, Some(project), "release_reservations_requested", None))
            .collect();
        let actions = names
            .iter()
            .map(|name| AtcTickAction::ReleaseReservations {
                agent: (*name).to_string(),
            })
            .collect();
        let mut summary = engine::atc_summary().expect("enabled fixture summary");
        // Make summary annotation independently observable in this typed-report
        // fixture; this is not a claim about a mounted server's health.
        summary.completeness = engine::SnapshotCompleteness::Full;
        summary.policy.fallback_active = false;
        summary.policy.fallback_reason = None;
        engine::AtcTickReport {
            actions,
            effects,
            summary,
        }
    }

    fn finish_local_hydration(state: &mut HydrationState) {
        while !state.pending.is_empty() {
            state.drain_with(|_| {}, || true);
        }
        state.resume_pending = false;
    }

    #[test]
    fn hydration_reduction_withdraws_old_release_authority_before_delivery() {
        with_global_atc(|| {
            let mut state = HydrationState::default();
            state
                .refresh_with(10, || {
                    Ok(vec![
                        row("BlueLake", 1, "/alpha"),
                        row("RedFox", 1, "/alpha"),
                    ])
                })
                .unwrap();
            finish_local_hydration(&mut state);
            let mut original = release_report(&["BlueLake", "RedFox"], "/alpha");
            state.filter_report_scope(&mut original);
            assert_eq!(original.effects.len(), 2);
            assert_eq!(original.actions.len(), 2);
            assert_eq!(state.progress.scope_effects_withheld, 0);

            state
                .refresh_with(20, || Ok(vec![row("BlueLake", 1, "/alpha")]))
                .unwrap();
            assert_eq!(state.progress.unchanged_agents, 1);
            finish_local_hydration(&mut state);
            let mut reduced = release_report(&["RedFox", "BlueLake"], "/alpha");
            state.filter_report_scope(&mut reduced);
            state.annotate(&mut reduced.summary, 20);
            assert_eq!(reduced.effects.len(), 1);
            assert_eq!(reduced.effects[0].agent, "BlueLake");
            assert_eq!(reduced.actions.len(), 1);
            assert_eq!(reduced.summary.kernel.pending_effects, 1);
            assert_eq!(
                reduced.summary.completeness,
                engine::SnapshotCompleteness::Partial
            );
            assert_eq!(
                reduced.summary.policy.fallback_reason.as_deref(),
                Some("population_scope_restricted")
            );
            assert_eq!(state.progress.scope_effects_withheld, 1);
            assert_eq!(state.progress.scope_actions_withheld, 1);

            state.refresh_with(30, || Ok(Vec::new())).unwrap();
            let mut empty = release_report(&["BlueLake"], "/alpha");
            state.filter_report_scope(&mut empty);
            assert!(empty.effects.is_empty() && empty.actions.is_empty());
            assert_eq!(state.progress.scope_effects_withheld, 2);
            assert_eq!(state.progress.scope_actions_withheld, 2);
        });
    }

    #[test]
    fn failed_refresh_preserves_scope_and_success_preserves_saturating_history() {
        with_global_atc(|| {
            let mut state = HydrationState::default();
            state
                .refresh_with(10, || Ok(vec![row("BlueLake", 1, "/alpha")]))
                .unwrap();
            finish_local_hydration(&mut state);
            state.progress.scope_effects_withheld = u64::MAX - 1;
            state.progress.scope_actions_withheld = u64::MAX - 1;
            let mut blocked = release_report(&["RetiredOne", "RetiredTwo"], "/alpha");
            state.filter_report_scope(&mut blocked);
            assert_eq!(state.progress.scope_effects_withheld, u64::MAX);
            assert_eq!(state.progress.scope_actions_withheld, u64::MAX);
            assert_eq!(state.progress.last_scope_effects_withheld, 2);
            assert!(
                state
                    .refresh_with(20, || Err("unavailable DB".into()))
                    .is_err()
            );
            assert!(state.progress.refresh_failed);
            assert!(
                state
                    .scope
                    .as_ref()
                    .unwrap()
                    .permits("BlueLake", Some("/alpha"))
            );
            assert_eq!(state.progress.last_scope_effects_withheld, 2);
            state
                .refresh_with(30, || Ok(vec![row("BlueLake", 1, "/alpha")]))
                .unwrap();
            assert!(!state.progress.refresh_failed);
            assert_eq!(state.progress.scope_effects_withheld, u64::MAX);
            assert_eq!(state.progress.scope_actions_withheld, u64::MAX);
            assert_eq!(state.progress.last_scope_effects_withheld, 0);
            assert_eq!(state.progress.last_scope_actions_withheld, 0);
        });
    }

    #[test]
    fn public_tick_withholds_actual_remembered_agent_proposals_after_empty_population() {
        with_global_atc(|| {
            let now = mcp_agent_mail_core::timestamps::now_micros();
            engine::atc_sync_agent_snapshot(
                "RetiredAgent",
                "codex-cli",
                Some("/alpha"),
                now.saturating_sub(3_600_000_000),
            );
            assert_eq!(engine::atc_summary().unwrap().tracked_agents.len(), 1);
            sync_population_with(now, || Ok(Vec::new())).unwrap();
            for step in 1..=8 {
                let report =
                    atc::atc_tick_report(now.saturating_add(step * 86_400_000_000)).unwrap();
                assert!(report.effects.is_empty());
                assert!(report.actions.is_empty());
                assert_eq!(report.summary.kernel.pending_effects, 0);
                if atc_population_hydration_stats().scope_effects_withheld > 0 {
                    assert_eq!(
                        report.summary.completeness,
                        engine::SnapshotCompleteness::Partial
                    );
                    assert_eq!(
                        atc::atc_summary().unwrap().completeness,
                        engine::SnapshotCompleteness::Partial
                    );
                    break;
                }
            }
            assert!(
                atc_population_hydration_stats().scope_effects_withheld > 0,
                "an idle engine with no proposals is not proof that public scope filtering ran"
            );
            assert_eq!(engine::atc_summary().unwrap().tracked_agents.len(), 1);
        });
    }

    #[test]
    fn public_ambiguity_reporting_and_reset_follow_installed_scope() {
        with_global_atc(|| {
            let now = mcp_agent_mail_core::timestamps::now_micros();
            sync_population_with(now, || {
                Ok(vec![
                    row("BlueLake", 1, "/alpha"),
                    row("BlueLake", 2, "/beta"),
                ])
            })
            .unwrap();
            // Small snapshots can yield after one row under a tight host budget.
            // Drain through the actual public boundary, not a fabricated success.
            for _ in 0..3 {
                let report = atc::atc_tick_report(now).unwrap();
                assert!(report.effects.is_empty() && report.actions.is_empty());
                assert_eq!(
                    report.summary.completeness,
                    engine::SnapshotCompleteness::Partial
                );
                if atc_population_hydration_stats().pending_agents == 0 {
                    break;
                }
            }
            assert_eq!(atc_population_hydration_stats().pending_agents, 0);
            let report = atc::atc_tick_report(now).unwrap();
            assert!(report.effects.is_empty() && report.actions.is_empty());
            assert_eq!(
                report.summary.completeness,
                engine::SnapshotCompleteness::Partial
            );
            assert_eq!(atc_population_hydration_stats().scope_unresolved_names, 1);
            assert!(atc::atc_summary().unwrap().policy.fallback_active);
            let config = mcp_agent_mail_core::Config {
                atc_enabled: true,
                ..Default::default()
            };
            atc::reset_global_atc_state_for_test(&config);
            assert_eq!(
                atc_population_hydration_stats(),
                AtcPopulationHydrationStats::default()
            );
            assert!(hydration().lock().unwrap().scope.is_none());
        });
    }

    #[test]
    fn late_pre_reset_read_cannot_restore_an_old_scope() {
        with_global_atc(|| {
            let now = mcp_agent_mail_core::timestamps::now_micros();
            let pre_reset_read = sync_population_with(now, || {
                let config = mcp_agent_mail_core::Config {
                    atc_enabled: true,
                    ..Default::default()
                };
                atc::reset_global_atc_state_for_test(&config);
                sync_population_with(now + 1, || Ok(vec![row("FreshAgent", 2, "/beta")])).unwrap();
                Ok(vec![row("OldAgent", 1, "/alpha")])
            });
            assert_eq!(
                pre_reset_read.unwrap_err(),
                "population refresh discarded after ATC reset"
            );
            let state = hydration().lock().unwrap();
            let scope = state.scope.as_ref().expect("new epoch scope");
            assert!(scope.permits("FreshAgent", Some("/beta")));
            assert!(!scope.permits("OldAgent", Some("/alpha")));
            assert!(!state.progress.refresh_failed);
            assert_eq!(state.progress.refresh_failures, 0);
        });
    }

    #[test]
    fn scope_annotation_cannot_erase_a_stronger_runtime_guard_reason() {
        with_global_atc(|| {
            let mut state = HydrationState::default();
            state.refresh_with(10, || Ok(Vec::new())).unwrap();
            let mut report = release_report(&["RetiredAgent"], "/alpha");
            report.summary.policy.fallback_reason = Some("calibration_miscalibrated".into());
            state.filter_report_scope(&mut report);
            state.annotate(&mut report.summary, 10);
            assert_eq!(
                report.summary.completeness,
                engine::SnapshotCompleteness::Partial
            );
            assert!(report.summary.policy.fallback_active);
            assert_eq!(
                report.summary.policy.fallback_reason.as_deref(),
                Some("calibration_miscalibrated")
            );
            assert_eq!(
                state.progress.refresh_failures, 0,
                "withholding is not a failed database read"
            );
        });
    }

    fn request_notice(request: &AtcEffectPlan, body: &str) -> AtcEffectPlan {
        let mut notice = request.clone();
        notice.effect_id = format!("{}:notice", request.effect_id);
        notice.kind = "send_advisory".into();
        notice.message = Some(body.into());
        notice.semantics.family = "release_notice".into();
        notice.semantics.high_risk_intervention = false;
        notice
    }

    fn proposal_actions(effects: &[AtcEffectPlan]) -> Vec<AtcTickAction> {
        effects
            .iter()
            .filter_map(|effect| match effect.kind.as_str() {
                "release_reservations_requested" => Some(AtcTickAction::ReleaseReservations {
                    agent: effect.agent.clone(),
                }),
                "send_advisory" => {
                    effect
                        .message
                        .as_ref()
                        .map(|message| AtcTickAction::SendAdvisory {
                            agent: effect.agent.clone(),
                            message: message.clone(),
                        })
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn withdrawn_request_drops_its_notice_but_keeps_independent_outcomes() {
        let mut active = row("BlueLake", 1, "/alpha");
        active.last_active_ts += 1;
        let scope = PopulationScope::from_rows(&[active]);
        let request = effect(
            "BlueLake",
            Some("/alpha"),
            "release_reservations_requested",
            None,
        );
        let paired = request_notice(&request, "ATC requested reservation release");
        let mut completed = request_notice(&request, "a previous release completed");
        completed.decision_id = 99;
        completed.trace_id = "previous-decision".into();
        let mut conflict = request_notice(&request, "inspect the conflict");
        conflict.semantics.family = "deadlock_remediation".into();
        let mut effects = vec![request, paired, completed, conflict];
        let mut actions = proposal_actions(&effects);
        assert_eq!(
            scope.retain_authorized(&mut effects, &mut actions),
            ScopeDisposition {
                effects_withheld: 2,
                actions_withheld: 2
            }
        );
        assert_eq!(effects.len(), 2);
        assert_eq!(
            effects[0].message.as_deref(),
            Some("a previous release completed")
        );
        assert_eq!(effects[1].message.as_deref(), Some("inspect the conflict"));
        assert_eq!(actions.len(), 2);
        assert!(
            actions
                .iter()
                .all(|action| matches!(action, AtcTickAction::SendAdvisory { .. }))
        );
    }

    #[test]
    fn request_notice_pairing_uses_project_agent_decision_and_trace_not_copy() {
        let mut active = row("BlueLake", 1, "/alpha");
        active.last_active_ts += 1;
        let scope = PopulationScope::from_rows(&[active, row("RedFox", 1, "/alpha")]);
        let request = effect(
            "BlueLake",
            Some("/alpha"),
            "release_reservations_requested",
            None,
        );
        for dimension in ["decision", "trace", "agent", "project"] {
            let mut rejected = request.clone();
            let mut independent = request_notice(&request, "same body for distinct decisions");
            match dimension {
                "decision" => independent.decision_id += 1,
                "trace" => independent.trace_id = "other-trace".into(),
                "agent" => independent.agent = "RedFox".into(),
                "project" => rejected.project_key = Some("/other-project".into()),
                _ => unreachable!(),
            }
            let mut effects = vec![rejected, independent];
            let mut actions = proposal_actions(&effects);
            assert_eq!(
                scope.retain_authorized(&mut effects, &mut actions),
                ScopeDisposition {
                    effects_withheld: 1,
                    actions_withheld: 1
                },
                "dimension={dimension}"
            );
            assert_eq!(effects.len(), 1);
            assert_eq!(effects[0].kind, "send_advisory");
            assert_eq!(actions.len(), 1);
        }
        let mut normalized_request = request.clone();
        normalized_request.project_key = Some(" /alpha ".into());
        let mut effects = vec![request_notice(&request, "paired"), normalized_request];
        let mut actions = proposal_actions(&effects);
        scope.retain_authorized(&mut effects, &mut actions);
        assert!(effects.is_empty() && actions.is_empty());
    }

    #[test]
    fn an_authorized_request_keeps_its_notice_regardless_of_duplicate_order() {
        let mut active = row("BlueLake", 1, "/alpha");
        active.last_active_ts += 1;
        let scope = PopulationScope::from_rows(&[active]);
        let old = effect(
            "BlueLake",
            Some("/alpha"),
            "release_reservations_requested",
            None,
        );
        let mut fresh = old.clone();
        fresh.timestamp_micros += 1;
        fresh.effect_id = "fresh-request".into();
        let proposals = [
            old,
            fresh,
            request_notice(
                &effect(
                    "BlueLake",
                    Some("/alpha"),
                    "release_reservations_requested",
                    None,
                ),
                "request notice",
            ),
        ];
        for order in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            let mut effects: Vec<_> = order.iter().map(|&i| proposals[i].clone()).collect();
            let expected: Vec<_> = order
                .iter()
                .filter(|&&i| i != 0)
                .map(|&i| proposals[i].effect_id.clone())
                .collect();
            let mut actions = proposal_actions(&effects);
            assert_eq!(
                scope.retain_authorized(&mut effects, &mut actions),
                ScopeDisposition {
                    effects_withheld: 1,
                    actions_withheld: 1
                }
            );
            assert_eq!(
                effects
                    .iter()
                    .map(|effect| effect.effect_id.clone())
                    .collect::<Vec<_>>(),
                expected
            );
            assert_eq!(actions.len(), 2);
        }
    }

    #[test]
    fn independent_release_notices_and_unknown_effects_do_not_need_a_new_release() {
        let mut unobserved = row("BlueLake", 1, "/alpha");
        unobserved.last_active_ts = 0;
        let scope = PopulationScope::from_rows(&[unobserved]);
        let request = effect(
            "BlueLake",
            Some("/alpha"),
            "release_reservations_requested",
            None,
        );
        let completed = request_notice(&request, "completed independently");
        let mut effects = vec![completed.clone()];
        let mut actions = proposal_actions(&effects);
        assert_eq!(
            scope.retain_authorized(&mut effects, &mut actions),
            ScopeDisposition::default()
        );
        assert_eq!(effects.len(), 1);
        assert_eq!(actions.len(), 1);

        let mut unknown = completed;
        unknown.kind = "future_mutation".into();
        let mut unrelated = request_notice(&request, "other outcome");
        unrelated.semantics.family = "completed_outcome".into();
        let mut effects = vec![request, unknown, unrelated];
        let mut actions = proposal_actions(&effects);
        let disposition = scope.retain_authorized(&mut effects, &mut actions);
        assert_eq!(disposition.effects_withheld, 1);
        assert_eq!(disposition.actions_withheld, 1);
        assert_eq!(effects.len(), 2);
        assert_eq!(effects[0].kind, "future_mutation");
        assert_eq!(actions.len(), 1);
    }

    #[test]
    fn late_delivery_revalidation_withdraws_a_request_and_its_notice_together() {
        with_global_atc(|| {
            sync_population_with(1_000_000, || Ok(vec![row("BlueLake", 1, "/alpha")])).unwrap();
            let epoch = atc::capture_delivery_epoch().unwrap();
            let mut report = super::super::tick_report(1_000_000).unwrap();
            let request = effect(
                "BlueLake",
                Some("/alpha"),
                "release_reservations_requested",
                None,
            );
            report.effects = vec![
                request.clone(),
                request_notice(&request, "release requested"),
            ];
            report.actions = proposal_actions(&report.effects);
            let mut active = row("BlueLake", 1, "/alpha");
            active.last_active_ts = 2_000_000;
            sync_population_with(2_000_000, || Ok(vec![active])).unwrap();
            // Resume inference, but do not admit a newer tick. The old report
            // must lose both its request and its dependent notice to scope.
            let _ = super::super::tick_report(3_000_000).unwrap();
            let admitted = atc::admit_report(&epoch, report, 3_000_000).unwrap();
            assert!(admitted.effects.is_empty() && admitted.actions.is_empty());
            assert_eq!(admitted.summary.kernel.pending_effects, 0);
            let progress = atc_population_hydration_stats();
            assert_eq!(progress.scope_effects_withheld, 2);
            assert_eq!(progress.scope_actions_withheld, 2);
            assert_eq!(atc::atc_delivery_stats().admitted, 0);
        });
    }

    #[test]
    fn a_940_agent_withdrawal_cannot_leave_false_request_notices_behind() {
        let population: Vec<_> = (0..940)
            .map(|index| {
                let mut active = row(&format!("Agent{index:04}"), 1, "/alpha");
                active.last_active_ts += i64::from(index % 2 == 0);
                active
            })
            .collect();
        let scope = PopulationScope::from_rows(&population);
        let mut effects = Vec::new();
        for agent in &population {
            let request = effect(
                &agent.name,
                Some("/alpha"),
                "release_reservations_requested",
                None,
            );
            let notice = request_notice(&request, "release requested");
            effects.extend([request, notice]);
        }
        let mut actions = proposal_actions(&effects);
        assert_eq!(
            scope.retain_authorized(&mut effects, &mut actions),
            ScopeDisposition {
                effects_withheld: 940,
                actions_withheld: 940
            }
        );
        assert_eq!(effects.len(), 940);
        assert_eq!(actions.len(), 940);
        for (pair, agent) in effects
            .as_chunks::<2>()
            .0
            .iter()
            .zip(population.iter().skip(1).step_by(2))
        {
            assert_eq!(pair[0].agent, agent.name);
            assert_eq!(pair[0].kind, "release_reservations_requested");
            assert_eq!(pair[1].agent, agent.name);
            assert_eq!(pair[1].semantics.family, "release_notice");
        }
        assert_eq!(
            scope.retain_authorized(&mut effects, &mut actions),
            ScopeDisposition::default()
        );
    }
}
