use super::*;
use lash_core::durable_port::{ActorKind, ClaimFacts};

pub(super) fn explain(
    node: Option<&NodeServingSnapshot>,
    actor: ActorKey,
    facts: ClaimFacts,
) -> ClaimExplanation {
    let mut blockers = Vec::new();
    let mut explanation = ClaimExplanation {
        node: node.cloned(),
        actor,
        observed_at: facts.observed_at,
        actor_revision: facts.actor.as_ref().map(|row| row.revision),
        actor_epoch: facts.actor.as_ref().map(|row| row.epoch),
        eligibility: ClaimEligibility::Unknown {
            reason: EvidenceAbsence::NodeEvidenceUnavailable,
        },
        blockers: Vec::new(),
        run_blockers: Vec::new(),
        requirements: Vec::new(),
        activation: ActivationKnowledge::NotEvaluated,
    };
    if let Some(row) = &facts.actor {
        explanation.requirements = row
            .requires
            .iter()
            .map(|token| RequirementExplanation {
                token: token.clone(),
                served: node.map(|node| node.serves.contains(token)),
                uses: Evidence::Unavailable {
                    reason: EvidenceAbsence::DetailsNotRequested,
                },
                local_detail: node
                    .and_then(|node| node.inventory.as_ref())
                    .and_then(|inventory| {
                        inventory.entries.iter().zip(&inventory.readiness).find_map(
                            |(entry, readiness)| match &entry.key {
                                crate::plugins::ContributionKey::Callback(key)
                                    if crate::plugins::ObligationToken::for_callback(
                                        entry.registry,
                                        key,
                                    )
                                    .as_str()
                                        == token =>
                                {
                                    Some(readiness.clone())
                                }
                                _ => None,
                            },
                        )
                    }),
            })
            .collect();
    }
    let Some(node) = node else {
        return explanation;
    };
    match (&node.owner, &facts.node) {
        (None, _) | (_, None) => blockers.push(ClaimBlocker::NodeNotRegistered),
        (Some(owner), Some(stored)) => {
            if owner != &stored.owner {
                blockers.push(ClaimBlocker::BootReplaced);
            }
            if stored.draining {
                blockers.push(ClaimBlocker::NodeDraining);
            }
        }
    }
    let purpose = if let Some(row) = &facts.actor {
        let due = match row.state {
            ActorState::Ready => row.ready_at,
            ActorState::Waiting => row.next_due,
            state => {
                blockers.push(ClaimBlocker::ActorNotReady { state });
                None
            }
        };
        if matches!(row.state, ActorState::Ready | ActorState::Waiting)
            && due.is_none_or(|at| at > facts.observed_at)
        {
            blockers.push(ClaimBlocker::NotDue { at: due });
        }
        let decoded = node.decodes.contains(&row.formats);
        if !decoded {
            explanation
                .run_blockers
                .push(ClaimBlocker::FormatNotDecoded {
                    stored: row.formats.clone(),
                });
        }
        explanation.run_blockers.extend(
            row.requires
                .iter()
                .filter(|token| !node.serves.contains(*token))
                .map(|token| ClaimBlocker::Unserved {
                    token: token.clone(),
                }),
        );
        let served = row.requires.iter().all(|token| node.serves.contains(token));
        let cancel = match row.actor.kind() {
            ActorKind::Process => row.process_cancel,
            ActorKind::Session => {
                decoded && (row.immediate_cancel || row.close_mail || row.unfinished_close)
            }
        };
        if decoded && served {
            Some(ClaimPurpose::Run)
        } else if cancel {
            Some(ClaimPurpose::CancelOnly)
        } else {
            if !decoded {
                blockers.push(ClaimBlocker::FormatNotDecoded {
                    stored: row.formats.clone(),
                });
            }
            blockers.extend(
                row.requires
                    .iter()
                    .filter(|token| !node.serves.contains(*token))
                    .map(|token| ClaimBlocker::Unserved {
                        token: token.clone(),
                    }),
            );
            None
        }
    } else {
        blockers.push(ClaimBlocker::ActorNotFound);
        None
    };
    explanation.eligibility = match purpose {
        Some(purpose) if blockers.is_empty() => ClaimEligibility::Eligible { purpose },
        _ => ClaimEligibility::Ineligible,
    };
    explanation.blockers = blockers;
    explanation
}
