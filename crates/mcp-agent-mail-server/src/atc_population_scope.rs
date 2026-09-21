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
    use super::super::engine::AtcEffectSemantics;
    use super::*;

    fn row(name: &str, id: i64, project: &str) -> AtcPopulationAgentRow {
        AtcPopulationAgentRow {
            project_id: id,
            project_key: project.into(),
            name: name.into(),
            program: "codex-cli".into(),
            last_active_ts: 1_000_000,
        }
    }

    pub(in super::super) fn effect(
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
}
