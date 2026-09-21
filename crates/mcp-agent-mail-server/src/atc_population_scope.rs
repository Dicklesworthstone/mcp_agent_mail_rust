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

use super::engine::{AtcEffectPlan, AtcTickAction};
use mcp_agent_mail_db::models::AtcPopulationAgentRow;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectIdentity {
    id: i64,
    key: String,
}

#[derive(Debug, Default)]
pub(super) struct PopulationScope {
    // None is an invalid or ambiguous name, not a missing project wildcard.
    // The map is replaced per successful refresh, never accumulated over time.
    by_name: HashMap<String, Option<ProjectIdentity>>,
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

impl PopulationScope {
    pub(super) fn from_rows(rows: &[AtcPopulationAgentRow]) -> Self {
        let mut by_name: HashMap<String, Option<ProjectIdentity>> =
            HashMap::with_capacity(rows.len());
        for row in rows {
            let project = row.project_key.trim();
            let identity =
                (!row.name.trim().is_empty() && row.project_id > 0 && !project.is_empty()).then(|| {
                    ProjectIdentity {
                        id: row.project_id,
                        key: project.to_string(),
                    }
                });
            by_name
                .entry(row.name.clone())
                .and_modify(|previous| {
                    if *previous != identity {
                        *previous = None;
                    }
                })
                .or_insert(identity);
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
        matches!(self.by_name.get(name), Some(Some(identity)) if identity.key == project)
    }

    pub(super) fn retain_authorized(
        &self,
        effects: &mut Vec<AtcEffectPlan>,
        actions: &mut Vec<AtcTickAction>,
    ) -> ScopeDisposition {
        let effect_count = effects.len();
        let action_count = actions.len();
        // Scope validation is separate from optional-notification budgeting.
        // High-risk releases and future effect kinds do not bypass authority.
        effects.retain(|effect| self.permits(&effect.agent, effect.project_key.as_deref()));

        let mut admitted = HashMap::new();
        for effect in effects.iter() {
            let key = match effect.kind.as_str() {
                "send_advisory" => effect.message.as_ref().map(|body| {
                    ActionKey::Advisory(effect.agent.clone(), body.clone())
                }),
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
        AtcPopulationHydrationStats, HydrationState, atc_population_hydration_stats,
        hydration, sync_population_with,
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

    fn effect(
        name: &str,
        project: Option<&str>,
        kind: &str,
        body: Option<&str>,
    ) -> AtcEffectPlan {
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
        for rows in [
            vec![a.clone(), b.clone(), a.clone()],
            vec![b.clone(), a.clone(), b.clone()],
        ] {
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
        for other in [
            row("BlueLake", 2, "/alpha"),
            row("BlueLake", 1, "/renamed"),
        ] {
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
            let scope = PopulationScope::from_rows(&[invalid.clone()]);
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
            scope.retain_authorized(&mut effects, &mut actions).actions_withheld,
            99
        );
        assert_eq!(actions.len(), 1);
    }

    #[test]
    fn legitimate_release_probe_and_notices_keep_their_order() {
        let scope = PopulationScope::from_rows(&[row("BlueLake", 1, "/alpha")]);
        let mut effects = vec![
            effect("BlueLake", Some("/alpha"), "release_reservations_requested", None),
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
            effects.iter().map(|effect| effect.effect_id.clone()).collect::<Vec<_>>(),
            before
        );
        assert!(matches!(actions[0], AtcTickAction::ReleaseReservations { .. }));
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
        engine::AtcTickReport { effects, actions, summary }
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
            state.refresh_with(10, || Ok(vec![
                row("BlueLake", 1, "/alpha"), row("RedFox", 1, "/alpha"),
            ])).unwrap();
            finish_local_hydration(&mut state);
            let mut original = release_report(&["BlueLake", "RedFox"], "/alpha");
            state.filter_report_scope(&mut original);
            assert_eq!(original.effects.len(), 2);
            assert_eq!(original.actions.len(), 2);
            assert_eq!(state.progress.scope_effects_withheld, 0);

            state.refresh_with(20, || Ok(vec![row("BlueLake", 1, "/alpha")])).unwrap();
            assert_eq!(state.progress.unchanged_agents, 1);
            finish_local_hydration(&mut state);
            let mut reduced = release_report(&["RedFox", "BlueLake"], "/alpha");
            state.filter_report_scope(&mut reduced);
            state.annotate(&mut reduced.summary, 20);
            assert_eq!(reduced.effects.len(), 1);
            assert_eq!(reduced.effects[0].agent, "BlueLake");
            assert_eq!(reduced.actions.len(), 1);
            assert_eq!(reduced.summary.kernel.pending_effects, 1);
            assert_eq!(reduced.summary.completeness, engine::SnapshotCompleteness::Partial);
            assert_eq!(reduced.summary.policy.fallback_reason.as_deref(), Some("population_scope_restricted"));
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
            state.refresh_with(10, || Ok(vec![row("BlueLake", 1, "/alpha")])).unwrap();
            finish_local_hydration(&mut state);
            state.progress.scope_effects_withheld = u64::MAX - 1;
            state.progress.scope_actions_withheld = u64::MAX - 1;
            let mut blocked = release_report(&["RetiredOne", "RetiredTwo"], "/alpha");
            state.filter_report_scope(&mut blocked);
            assert_eq!(state.progress.scope_effects_withheld, u64::MAX);
            assert_eq!(state.progress.scope_actions_withheld, u64::MAX);
            assert_eq!(state.progress.last_scope_effects_withheld, 2);
            assert!(state.refresh_with(20, || Err("unavailable DB".into())).is_err());
            assert!(state.progress.refresh_failed);
            assert!(state.scope.as_ref().unwrap().permits("BlueLake", Some("/alpha")));
            assert_eq!(state.progress.last_scope_effects_withheld, 2);
            state.refresh_with(30, || Ok(vec![row("BlueLake", 1, "/alpha")])).unwrap();
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
                "RetiredAgent", "codex-cli", Some("/alpha"),
                now.saturating_sub(3_600_000_000),
            );
            assert_eq!(engine::atc_summary().unwrap().tracked_agents.len(), 1);
            sync_population_with(now, || Ok(Vec::new())).unwrap();
            for step in 1..=8 {
                let report = atc::atc_tick_report(now.saturating_add(step * 86_400_000_000)).unwrap();
                assert!(report.effects.is_empty());
                assert!(report.actions.is_empty());
                assert_eq!(report.summary.kernel.pending_effects, 0);
                if atc_population_hydration_stats().scope_effects_withheld > 0 {
                    assert_eq!(report.summary.completeness, engine::SnapshotCompleteness::Partial);
                    assert_eq!(atc::atc_summary().unwrap().completeness, engine::SnapshotCompleteness::Partial);
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
            sync_population_with(now, || Ok(vec![
                row("BlueLake", 1, "/alpha"), row("BlueLake", 2, "/beta"),
            ])).unwrap();
            // Small snapshots can yield after one row under a tight host budget.
            // Drain through the actual public boundary, not a fabricated success.
            for _ in 0..3 {
                let report = atc::atc_tick_report(now).unwrap();
                assert!(report.effects.is_empty() && report.actions.is_empty());
                assert_eq!(report.summary.completeness, engine::SnapshotCompleteness::Partial);
                if atc_population_hydration_stats().pending_agents == 0 {
                    break;
                }
            }
            assert_eq!(atc_population_hydration_stats().pending_agents, 0);
            let report = atc::atc_tick_report(now).unwrap();
            assert!(report.effects.is_empty() && report.actions.is_empty());
            assert_eq!(report.summary.completeness, engine::SnapshotCompleteness::Partial);
            assert_eq!(atc_population_hydration_stats().scope_unresolved_names, 1);
            assert!(atc::atc_summary().unwrap().policy.fallback_active);
            let config = mcp_agent_mail_core::Config { atc_enabled: true, ..Default::default() };
            atc::reset_global_atc_state_for_test(&config);
            assert_eq!(atc_population_hydration_stats(), AtcPopulationHydrationStats::default());
            assert!(hydration().lock().unwrap().scope.is_none());
        });
    }

    #[test]
    fn late_pre_reset_read_cannot_restore_an_old_scope() {
        with_global_atc(|| {
            let now = mcp_agent_mail_core::timestamps::now_micros();
            let stale = sync_population_with(now, || {
                let config = mcp_agent_mail_core::Config { atc_enabled: true, ..Default::default() };
                atc::reset_global_atc_state_for_test(&config);
                sync_population_with(now + 1, || Ok(vec![row("FreshAgent", 2, "/beta")])).unwrap();
                Ok(vec![row("OldAgent", 1, "/alpha")])
            });
            assert_eq!(stale.unwrap_err(), "population refresh discarded after ATC reset");
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
            assert_eq!(report.summary.completeness, engine::SnapshotCompleteness::Partial);
            assert!(report.summary.policy.fallback_active);
            assert_eq!(report.summary.policy.fallback_reason.as_deref(), Some("calibration_miscalibrated"));
            assert_eq!(state.progress.refresh_failures, 0, "withholding is not a failed database read");
        });
    }
}
