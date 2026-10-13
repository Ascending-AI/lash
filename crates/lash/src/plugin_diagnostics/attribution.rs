use super::*;
use lash_core::durable_port::domain::RunFactKind;
use lash_core_store::checkpoint_graph::GraphRefusal;

fn unavailable(reason: EvidenceAbsence) -> Evidence<RecordedAttribution> {
    Evidence::Unavailable { reason }
}
fn registry(owner: &OwnerKey) -> crate::plugins::RegistryKind {
    match owner {
        OwnerKey::Process(_) => crate::plugins::RegistryKind::Process,
        OwnerKey::Turn(..) | OwnerKey::Cell(..) => crate::plugins::RegistryKind::Session,
    }
}
impl PluginDiagnostics {
    /// Join only retained attribution with today's independent installed view.
    /// A callback key absent from the record is never guessed from the roster.
    pub async fn attribute(
        &self,
        decision: &DecisionRef,
    ) -> Result<DecisionAttribution, DiagnosticError> {
        let (evidence, kind) = match decision {
            DecisionRef::Tool {
                record,
                call,
                selection,
            } => {
                let RecordLocator::RunFact {
                    owner,
                    run,
                    ordinal,
                } = record
                else {
                    return Ok(DecisionAttribution {
                        decision: decision.clone(),
                        evidence: unavailable(EvidenceAbsence::UnsupportedRecordKind),
                        installed_now: Vec::new(),
                    });
                };
                let facts = self
                    .core
                    .backend
                    .durable()
                    .run_facts_within(
                        owner,
                        Some(*run),
                        4096,
                        lash_core_store::checkpoint_graph::DecodeLimits::within_total(
                            16 * 1024 * 1024,
                        ),
                    )
                    .await?;
                let evidence = match facts.rows.iter().find(|row| row.ordinal == *ordinal) {
                    None => unavailable(EvidenceAbsence::NotRetainedOrNeverRecorded),
                    Some(row)
                        if matches!(
                            selection,
                            ToolDecisionSelection::BeforeCheck { .. }
                                | ToolDecisionSelection::ResultCheck { .. }
                        ) =>
                    {
                        retained_check_attribution(&facts, row, record, call, selection)?
                    }
                    Some(row) if row.kind == RunFactKind::Admit => {
                        let bindings =
                            lash_core::admitted_bindings_of(&facts, row).map_err(|reason| {
                                lash_core::durable_port::DurableError::Graph(
                                    GraphRefusal::InvalidBody {
                                        kind:
                                            lash_core_store::checkpoint_graph::NodeKind::ROUND_FACT
                                                .0,
                                        reason,
                                    },
                                )
                            })?;
                        match bindings
                            .into_iter()
                            .find(|(id, _)| id == call)
                            .and_then(|(_, binding)| binding)
                        {
                            None => unavailable(EvidenceAbsence::AttributionNotRecorded),
                            Some(binding) => {
                                let value = match selection {
                                    ToolDecisionSelection::Executable => {
                                        RecordedAttribution::Binding {
                                            selected: binding.executable.clone(),
                                            binding,
                                        }
                                    }
                                    ToolDecisionSelection::Preparation => {
                                        RecordedAttribution::Binding {
                                            selected: binding.preparation.clone(),
                                            binding,
                                        }
                                    }
                                    ToolDecisionSelection::Presentation => {
                                        RecordedAttribution::Presentation(binding.presentation)
                                    }
                                    _ => {
                                        return Ok(DecisionAttribution {
                                            decision: decision.clone(),
                                            evidence: unavailable(
                                                EvidenceAbsence::AttributionNotRecorded,
                                            ),
                                            installed_now: Vec::new(),
                                        });
                                    }
                                };
                                Evidence::Recorded {
                                    source: record.clone(),
                                    value,
                                }
                            }
                        }
                    }
                    Some(_) => unavailable(EvidenceAbsence::UnsupportedRecordKind),
                };
                (evidence, registry(owner))
            }
            DecisionRef::Prompt {
                call,
                section,
                wrapper,
            } => {
                let snapshot = self.prompt_snapshot(call).await?;
                let evidence = snapshot
                    .and_then(|snapshot| {
                        snapshot.snapshot.plan.sections.into_iter().find(|record| {
                            record.section == *section
                                && wrapper.as_ref().is_none_or(|wrapper| {
                                    record.wraps.iter().any(|wrap| wrap.wrap == *wrapper)
                                })
                        })
                    })
                    .map_or_else(
                        || unavailable(EvidenceAbsence::NotRetainedOrNeverRecorded),
                        |record| Evidence::Recorded {
                            source: RecordLocator::Prompt(call.clone()),
                            value: RecordedAttribution::Prompt(record),
                        },
                    );
                (evidence, crate::plugins::RegistryKind::Session)
            }
            DecisionRef::Callback { record, ordinal } => {
                let RecordLocator::TurnPhase { session, run } = record else {
                    return Ok(DecisionAttribution {
                        decision: decision.clone(),
                        evidence: unavailable(EvidenceAbsence::UnsupportedRecordKind),
                        installed_now: Vec::new(),
                    });
                };
                let phase = lash_core::runtime::durable::read_plugin_phase(
                    self.core.backend.durable().as_ref(),
                    session,
                    run,
                )
                .await?;
                let evidence = phase
                    .and_then(|phase| {
                        usize::try_from(*ordinal)
                            .ok()
                            .and_then(|ordinal| phase.before_turn.get(ordinal).cloned())
                    })
                    .map_or_else(
                        || unavailable(EvidenceAbsence::NotRetainedOrNeverRecorded),
                        |value| Evidence::Recorded {
                            source: record.clone(),
                            value: RecordedAttribution::Owner(value),
                        },
                    );
                (evidence, crate::plugins::RegistryKind::Session)
            }
            DecisionRef::Surface { record, tool } => {
                let RecordLocator::TurnPhase { session, run } = record else {
                    return Ok(DecisionAttribution {
                        decision: decision.clone(),
                        evidence: unavailable(EvidenceAbsence::UnsupportedRecordKind),
                        installed_now: Vec::new(),
                    });
                };
                let phase = lash_core::runtime::durable::read_plugin_phase(
                    self.core.backend.durable().as_ref(),
                    session,
                    run,
                )
                .await?;
                let entry = phase.and_then(|phase| phase.surface).and_then(|surface| {
                    surface
                        .tools()
                        .values()
                        .find(|entry| entry.tool() == tool)
                        .cloned()
                });
                (
                    entry.map_or_else(
                        || unavailable(EvidenceAbsence::NotRetainedOrNeverRecorded),
                        |entry| Evidence::Recorded {
                            source: record.clone(),
                            value: RecordedAttribution::Surface(Box::new(entry)),
                        },
                    ),
                    crate::plugins::RegistryKind::Session,
                )
            }
        };
        let recorded = match &evidence {
            Evidence::Recorded { value, .. } | Evidence::Partial { value, .. } => Some(value),
            Evidence::Unavailable { .. } => None,
        };
        let installed_now = match (recorded, self.installed(&InventoryScope::Deployment(kind))) {
            (Some(recorded), InstalledObservation::Present(inventory)) => inventory
                .entries
                .into_iter()
                .filter(|entry| matches_recorded(&entry.key, recorded))
                .collect(),
            _ => Vec::new(),
        };
        Ok(DecisionAttribution {
            decision: decision.clone(),
            evidence,
            installed_now,
        })
    }
}
fn matches_recorded(key: &ContributionKey, record: &RecordedAttribution) -> bool {
    match record {
        RecordedAttribution::Binding { selected, .. } => {
            matches!(key, ContributionKey::Callback(key) if key == selected)
        }
        RecordedAttribution::Presentation(binding) => {
            matches!(key, ContributionKey::Callback(key) if binding.callbacks().any(|callback| callback == key))
        }
        RecordedAttribution::Owner(owner) => key.owner() == owner.plugin_id,
        RecordedAttribution::Surface(entry) => {
            matches!(key, ContributionKey::Callback(key) if *key == entry.provider.callback_identity())
        }
        RecordedAttribution::Prompt(section) => match key {
            ContributionKey::PromptSection(key) => *key == section.section,
            ContributionKey::PromptFamily(key) => {
                key.owner == section.section.owner
                    && section
                        .section
                        .key
                        .as_str()
                        .strip_prefix(key.key.as_str())
                        .is_some_and(|tail| tail.starts_with('.') && tail.len() > 1)
            }
            ContributionKey::PromptWrapper(key) => {
                section.wraps.iter().any(|wrap| wrap.wrap == *key)
            }
            _ => false,
        },
        RecordedAttribution::BeforeCheck { checks, .. } => {
            matches!(key, ContributionKey::Callback(key) if checks.replies().iter().any(|reply| reply.callback == *key))
        }
        RecordedAttribution::ResultCheck { checks, .. } => {
            matches!(key, ContributionKey::Callback(key) if checks.replies().iter().any(|reply| reply.callback == *key))
        }
    }
}

/// Expiry records are already retained in the selected output. They carry
/// runtime verdicts, not the admission journal's verdict representation.
fn retained_check_attribution(
    facts: &lash_core::durable_port::domain::RunFacts,
    row: &lash_core::durable_port::domain::RunFactRow,
    source: &RecordLocator,
    call: &crate::ToolCallId,
    selection: &ToolDecisionSelection,
) -> Result<Evidence<RecordedAttribution>, DiagnosticError> {
    use crate::plugins::{AfterToolDecision, BeforeToolDecision};
    let Some(output) = lash_core::tool_outcome_of(facts, row, call)
        .map_err(lash_core::durable_port::DurableError::Graph)?
    else {
        return Ok(unavailable(EvidenceAbsence::AttributionNotRecorded));
    };
    let (raw, expired) = match output.outcome {
        lash_core::ToolCallOutcome::Failure(failure) => {
            let expired = matches!(
                failure.cause.as_deref(),
                Some(lash_core::ToolFailureCause::ControlPhaseExpired { .. })
            );
            (failure.raw, expired)
        }
        lash_core::ToolCallOutcome::Cancelled(cancellation) => (cancellation.raw, false),
        lash_core::ToolCallOutcome::Success(_) => (None, false),
    };
    let Some(raw) = raw else {
        return Ok(unavailable(EvidenceAbsence::AttributionNotRecorded));
    };
    // The graph read is bounded above; impose a tighter bound on the optional
    // diagnostic representation and use serde's depth-limited JSON reader.
    let bytes = match serde_json::to_vec(&raw.to_json_value()) {
        Ok(bytes) if bytes.len() <= 1024 * 1024 => bytes,
        Ok(_) => return Ok(unavailable(EvidenceAbsence::ReadBoundExceeded)),
        Err(_) => {
            return Ok(unavailable(
                EvidenceAbsence::UnsupportedRetainedRepresentation,
            ));
        }
    };
    let is_record = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .is_some_and(|value| {
            value.as_array().is_some_and(|replies| {
                replies
                    .iter()
                    .any(|reply| reply.get("callback").is_some() && reply.get("verdict").is_some())
            })
        });
    if !is_record {
        return Ok(unavailable(if expired {
            EvidenceAbsence::UnsupportedRetainedRepresentation
        } else {
            EvidenceAbsence::AttributionNotRecorded
        }));
    }
    let (value, missing) = match selection {
        ToolDecisionSelection::BeforeCheck { reply } => {
            let checks = match decode_expiry_checks::<BeforeToolDecision>(
                &bytes,
                "tool_args_check",
                "tool_args",
                |verdict| match verdict {
                    BeforeToolDecision::Deny(failure) => Some(failure),
                    _ => None,
                },
            ) {
                Ok(checks) => checks,
                Err(reason) => return Ok(unavailable(reason)),
            };
            let selected = select_reply(&checks, *reply);
            let missing = if reply.is_some() && selected.is_none() {
                vec![EvidenceAbsence::NotRetainedOrNeverRecorded]
            } else {
                Vec::new()
            };
            (
                RecordedAttribution::BeforeCheck { checks, selected },
                missing,
            )
        }
        ToolDecisionSelection::ResultCheck { reply, .. } => {
            let checks = match decode_expiry_checks::<AfterToolDecision>(
                &bytes,
                "tool_result_check",
                "tool_result",
                |verdict| match verdict {
                    AfterToolDecision::Deny(failure) => Some(failure),
                    _ => None,
                },
            ) {
                Ok(checks) => checks,
                Err(reason) => return Ok(unavailable(reason)),
            };
            let selected = select_reply(&checks, *reply);
            let mut missing = vec![EvidenceAbsence::OccurrenceNotRecorded];
            if reply.is_some() && selected.is_none() {
                missing.push(EvidenceAbsence::NotRetainedOrNeverRecorded);
            }
            (
                RecordedAttribution::ResultCheck { checks, selected },
                missing,
            )
        }
        _ => return Ok(unavailable(EvidenceAbsence::UnsupportedRecordKind)),
    };
    Ok(if missing.is_empty() {
        Evidence::Recorded {
            source: source.clone(),
            value,
        }
    } else {
        Evidence::Partial {
            source: source.clone(),
            value,
            missing,
        }
    })
}

fn select_reply<V: Clone + lash_core::tool_run::RankedVerdict>(
    checks: &CheckRecord<V>,
    reply: Option<u32>,
) -> Option<AttributedVerdict<V>> {
    reply.map_or_else(
        || checks.winner().cloned(),
        |index| checks.replies().get(index as usize).cloned(),
    )
}

fn decode_expiry_checks<V: serde::de::DeserializeOwned + lash_core::tool_run::RankedVerdict>(
    bytes: &[u8],
    slot: &str,
    phase: &str,
    failure: impl Fn(&V) -> Option<&lash_core::ToolFailure>,
) -> Result<CheckRecord<V>, EvidenceAbsence> {
    let checks: CheckRecord<V> = serde_json::from_slice(bytes)
        .map_err(|_| EvidenceAbsence::UnsupportedRetainedRepresentation)?;
    if checks.replies().len() > 4096 {
        return Err(EvidenceAbsence::ReadBoundExceeded);
    }
    if !checks.is_reduced() {
        return Err(EvidenceAbsence::UnsupportedRetainedRepresentation);
    }
    let prefix = format!("{slot}:");
    if checks
        .replies()
        .iter()
        .any(|reply| !reply.callback.key.starts_with(&prefix))
    {
        return Err(EvidenceAbsence::AttributionNotRecorded);
    }
    let retained_expiry = checks.replies().iter().any(|reply| {
        failure(&reply.verdict).is_some_and(|failure| matches!(failure.cause.as_deref(),
            Some(lash_core::ToolFailureCause::ControlPhaseExpired { expiry })
                if expiry.phase == phase && expiry.plugin == reply.callback.owner && expiry.callback == reply.callback.key))
    });
    if !retained_expiry {
        return Err(EvidenceAbsence::UnsupportedRetainedRepresentation);
    }
    Ok(checks)
}
