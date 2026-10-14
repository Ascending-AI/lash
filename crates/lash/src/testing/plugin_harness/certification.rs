//! Inventory-driven laws; lack of an exercised branch is never a pass.
use super::*;
use crate::plugins::diagnostics::*;
use crate::plugins::{CallbackSlot, InstalledInventory, PluginCallbackIdentity, RegistryKind};

/// The four durability contracts, reported independently for every callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginLaw {
    ReplayInvokesNoRecordedCallback,
    StateSurvivesReopen,
    UnservedIsUnclaimable,
    RecordedDecisionsStayAttributable,
}
impl std::fmt::Display for PluginLaw {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ReplayInvokesNoRecordedCallback => "REPLAY-INVOKES-NO-RECORDED-CALLBACK",
            Self::StateSurvivesReopen => "STATE-SURVIVES-REOPEN",
            Self::UnservedIsUnclaimable => "UNSERVED-IS-UNCLAIMABLE",
            Self::RecordedDecisionsStayAttributable => "RECORDED-DECISIONS-STAY-ATTRIBUTABLE",
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CertificationUnavailable {
    NoScenario,
    NeedsVmBackend,
    MissingService { service: String },
    MissingCredential { credential: String },
    AttributionNotRecorded,
    NoPublishedState,
    NotRecordedAtCut,
    CounterUnavailable,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PluginLawResult {
    Passed,
    Failed { message: String },
    Unavailable { reason: CertificationUnavailable },
}
#[derive(Clone, Debug)]
pub struct PluginLawReport {
    pub callback: PluginCallbackIdentity,
    pub law: PluginLaw,
    pub result: PluginLawResult,
}
impl std::fmt::Display for PluginLawReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {}/{}: {:?}",
            self.law, self.callback.owner, self.callback.key, self.result
        )
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginCertificationVerdict {
    /// Every callback law was exercised and passed.
    Full,
    /// At least one callback law was unavailable.
    Partial,
    /// At least one callback law failed.
    Failed,
    /// The factory's inventory declares no callbacks for this battery to certify.
    /// Inventory admission alone proves no execution, replay or reopen behavior.
    NoCertifiableContributions,
}
#[derive(Clone, Debug)]
pub struct PluginCertification {
    pub inventory: InstalledInventory,
    pub reports: Vec<PluginLawReport>,
}
impl PluginCertification {
    pub fn verdict(&self) -> PluginCertificationVerdict {
        if self
            .reports
            .iter()
            .any(|report| matches!(report.result, PluginLawResult::Failed { .. }))
        {
            PluginCertificationVerdict::Failed
        } else if self.reports.is_empty() {
            // The battery emits reports for every callback, including unavailable ones.
            PluginCertificationVerdict::NoCertifiableContributions
        } else if self
            .reports
            .iter()
            .any(|report| matches!(report.result, PluginLawResult::Unavailable { .. }))
        {
            PluginCertificationVerdict::Partial
        } else {
            PluginCertificationVerdict::Full
        }
    }
    pub fn failures(&self) -> impl Iterator<Item = &PluginLawReport> {
        self.reports
            .iter()
            .filter(|report| matches!(report.result, PluginLawResult::Failed { .. }))
    }
}

/// Inputs, configuration and preconditions for one installed callback.
/// Setup runs through the harness's real core (for example, append a pressure
/// history). No callback is invoked directly by the battery.
#[async_trait::async_trait]
pub trait PluginScenario: Send + Sync {
    fn callback(&self) -> PluginCallbackIdentity;
    fn availability(&self) -> Result<(), CertificationUnavailable> {
        Ok(())
    }
    async fn prepare(&self, harness: &mut PluginHarness) -> Result<(), PluginHarnessError>;
    fn input(&self) -> crate::TurnInput;
    fn responses(&self) -> Vec<crate::provider::LlmResponse>;
    /// Completed model answers before the crash cut at the next model call.
    fn pause_after_responses(&self) -> usize;
}

/// Run the battery with standard host limits. Callbacks needing supplied
/// input or services appear as unavailable; inspect the verdict to gate a host.
pub async fn certify_plugin(
    factory: Arc<dyn PluginFactory>,
) -> Result<PluginCertification, PluginHarnessError> {
    certify_plugin_with(factory, HostPolicy::standard(), &[]).await
}
/// The same battery under the host's publication/reopen limits and optional
/// typed author scenarios. A scenario never turns an unexecuted callback green.
pub async fn certify_plugin_with(
    factory: Arc<dyn PluginFactory>,
    policy: HostPolicy,
    scenarios: &[Arc<dyn PluginScenario>],
) -> Result<PluginCertification, PluginHarnessError> {
    let mut harness = PluginHarness::new(Arc::clone(&factory), policy.clone()).await?;
    let InstalledObservation::Present(inventory) = harness
        .core()?
        .plugin_diagnostics()
        .installed(&InventoryScope::Deployment(RegistryKind::Session))
    else {
        return Err(PluginHarnessError::Worker(
            "installed inventory is unavailable".into(),
        ));
    };
    let callbacks: Vec<_> = inventory
        .entries
        .iter()
        .filter_map(|entry| {
            if entry.key.owner() != factory.id() {
                return None;
            }
            match (&entry.key, &entry.kind) {
                (
                    crate::plugins::ContributionKey::Callback(callback),
                    crate::plugins::ContributionKind::Callback(slot),
                ) => Some((callback.clone(), *slot)),
                _ => None,
            }
        })
        .collect();
    let mut reports = Vec::new();
    for (callback, slot) in callbacks {
        let claim_result = unserved_law(&harness, &callback, reports.len()).await?;
        reports.push(PluginLawReport {
            callback: callback.clone(),
            law: PluginLaw::UnservedIsUnclaimable,
            result: claim_result,
        });
        let scenario = scenarios
            .iter()
            .find(|scenario| scenario.callback() == callback);
        if let Some(reason) = scenario.and_then(|scenario| scenario.availability().err()) {
            for law in [
                PluginLaw::ReplayInvokesNoRecordedCallback,
                PluginLaw::StateSurvivesReopen,
                PluginLaw::RecordedDecisionsStayAttributable,
            ] {
                reports.push(PluginLawReport {
                    callback: callback.clone(),
                    law,
                    result: unavailable(reason.clone()),
                });
            }
            continue;
        }
        let automatic = matches!(
            slot,
            CallbackSlot::BeforeTurn
                | CallbackSlot::ContextPressure
                | CallbackSlot::AttachmentOmission
                | CallbackSlot::ToolCatalog
        );
        if scenario.is_none() && !automatic {
            for law in [
                PluginLaw::ReplayInvokesNoRecordedCallback,
                PluginLaw::StateSurvivesReopen,
                PluginLaw::RecordedDecisionsStayAttributable,
            ] {
                reports.push(PluginLawReport {
                    callback: callback.clone(),
                    law,
                    result: unavailable(CertificationUnavailable::NoScenario),
                });
            }
            continue;
        }
        harness = PluginHarness::new(Arc::clone(&factory), policy.clone()).await?;
        if let Some(scenario) = scenario {
            scenario.prepare(&mut harness).await?;
            harness.script(scenario.responses());
            harness.pause_model_after(scenario.pause_after_responses());
        } else {
            harness
                .create(crate::plugins::PluginOptions::default())
                .await?;
            harness.script([text_response("certified")]);
            harness.pause_model_after(0);
        }
        let input = scenario.map_or_else(
            || crate::TurnInput::text("certify plugin"),
            |scenario| scenario.input(),
        );
        // Setup may have completed other turns. Only this turn's callbacks
        // establish replay evidence at the requested crash cut.
        let initial = harness
            .callback_counts()
            .get(&callback)
            .copied()
            .unwrap_or(0);
        let handle = harness.send(input).await?;
        let input_id = handle.input_id().clone();
        // Do not retain an old runtime handle across the crash.
        drop(handle);
        let accepted = harness.session().await?.attach(input_id.clone());
        let pause = tokio::select! {
            pause = harness.wait_for_model_pause() => pause,
            result = accepted.output() => Err(PluginHarnessError::Worker(format!("turn ended before the recorded cut: {result:?}"))),
        };
        if let Err(error) = pause {
            reports.push(PluginLawReport {
                callback: callback.clone(),
                law: PluginLaw::StateSurvivesReopen,
                result: PluginLawResult::Failed {
                    message: format!("publication did not reach the crash cut: {error}"),
                },
            });
            for law in [
                PluginLaw::ReplayInvokesNoRecordedCallback,
                PluginLaw::RecordedDecisionsStayAttributable,
            ] {
                reports.push(PluginLawReport {
                    callback: callback.clone(),
                    law,
                    result: unavailable(CertificationUnavailable::NotRecordedAtCut),
                });
            }
            continue;
        }
        let counts = harness.callback_counts();
        let recorded = counts.get(&callback).copied().unwrap_or(0);
        let invoked = recorded.saturating_sub(initial);
        let state = harness.plugin_state().await?;
        let decisions = recorded_decisions(&harness, &callback).await?;
        harness.crash();
        harness.reopen_without_plugin().await?;
        let attribution = attribution_law(&harness, &callback, &decisions).await?;
        harness.reopen_at_model_pause().await?;
        let restored = harness.plugin_state().await?;
        let state_result = if invoked == 0 {
            unavailable(CertificationUnavailable::NoScenario)
        } else if state
            .plugins
            .get(factory.id())
            .is_none_or(|namespace| namespace.values.is_empty())
        {
            unavailable(CertificationUnavailable::NoPublishedState)
        } else if state.plugins.get(factory.id()) == restored.plugins.get(factory.id()) {
            PluginLawResult::Passed
        } else {
            PluginLawResult::Failed {
                message: "published namespace changed on fresh materialization".into(),
            }
        };
        // Reattach the accepted input: no new send and no new turn is admitted.
        harness.release_model();
        let outcome = tokio::time::timeout(
            policy.scenario_timeout,
            harness.session().await?.attach(input_id).output(),
        )
        .await;
        let after = harness
            .callback_counts()
            .get(&callback)
            .copied()
            .unwrap_or(0);
        let replay = if invoked == 0 {
            unavailable(CertificationUnavailable::NoScenario)
        } else if !recorded_at_cut(slot) {
            unavailable(CertificationUnavailable::NotRecordedAtCut)
        } else if after != recorded {
            PluginLawResult::Failed {
                message: format!(
                    "recorded callback invoked again: {recorded} before crash, {after} after reopen"
                ),
            }
        } else if !matches!(outcome, Ok(Ok(_))) {
            PluginLawResult::Failed {
                message: format!("reopened turn did not finish: {outcome:?}"),
            }
        } else {
            PluginLawResult::Passed
        };
        reports.push(PluginLawReport {
            callback: callback.clone(),
            law: PluginLaw::ReplayInvokesNoRecordedCallback,
            result: replay,
        });
        reports.push(PluginLawReport {
            callback: callback.clone(),
            law: PluginLaw::StateSurvivesReopen,
            result: state_result,
        });
        reports.push(PluginLawReport {
            callback,
            law: PluginLaw::RecordedDecisionsStayAttributable,
            result: attribution,
        });
    }
    Ok(PluginCertification { inventory, reports })
}
fn unavailable(reason: CertificationUnavailable) -> PluginLawResult {
    PluginLawResult::Unavailable { reason }
}
fn recorded_at_cut(slot: CallbackSlot) -> bool {
    matches!(
        slot,
        CallbackSlot::BeforeTurn
            | CallbackSlot::ToolProvider
            | CallbackSlot::ToolArgsCheck
            | CallbackSlot::ToolResultCheck
            | CallbackSlot::ToolArgsTransform
            | CallbackSlot::ToolResultTransform
            | CallbackSlot::PresentationStep
            | CallbackSlot::PresentationPresenter
    )
}
/// A compact model answer for an author's scripted session.
pub fn text_response(text: impl Into<String>) -> crate::provider::LlmResponse {
    crate::provider::LlmResponse {
        parts: vec![crate::direct::LlmOutputPart::Text {
            text: text.into(),
            response_meta: None,
        }],
        ..Default::default()
    }
}

async fn unserved_law(
    harness: &PluginHarness,
    callback: &PluginCallbackIdentity,
    ordinal: usize,
) -> Result<PluginLawResult, PluginHarnessError> {
    use crate::durable::{ActorKey, CommitLabel, MailTx, NodeId, NodeSpec, Release};
    let store = harness.backend.durable();
    let actor = ActorKey::session(&format!("owed-callback-{ordinal}"))
        .map_err(|error| PluginHarnessError::Worker(error.to_string()))?;
    let formats = vec![crate::durable::FormatSet::new("plugin-certification-claim")];
    let Some(format) = formats.first() else {
        return Ok(unavailable(CertificationUnavailable::MissingService {
            service: "session format decoder".into(),
        }));
    };
    let token =
        crate::plugins::ObligationToken::for_callback(RegistryKind::Session, callback).to_string();
    let writer = store
        .register_node(&NodeSpec {
            node: NodeId::new(format!("callback-writer-{ordinal}")),
            decodes: formats.clone(),
            serves: vec![token.clone()],
            ttl_millis: 60000,
        })
        .await?;
    let mut mail = MailTx::new();
    mail.create_actor(actor.clone(), format.clone());
    store.commit_mail(mail, CommitLabel::TURN_ADMIT).await?;
    let owned = store
        .claim(&writer, 128)
        .await?
        .into_iter()
        .find(|owned| owned.actor == actor)
        .ok_or_else(|| {
            PluginHarnessError::Worker("writer did not claim certification actor".into())
        })?;
    let mut tx = store.begin(&actor, owned.epoch).await?;
    tx.set_requires(vec![token.clone()]);
    tx.give_up(Release::Ready);
    store.commit(tx, CommitLabel::TURN_ADMIT).await?;
    let candidate = store
        .register_node(&NodeSpec {
            node: NodeId::new(format!("unserved-callback-{ordinal}")),
            decodes: formats,
            serves: Vec::new(),
            ttl_millis: 60000,
        })
        .await?;
    let snapshot = NodeServingSnapshot {
        owner: Some(candidate.owner.clone()),
        observed_at: store.now().await?,
        decodes: candidate.decodes.clone(),
        serves: candidate.serves.iter().cloned().collect(),
        inventory: None,
    };
    let explanation = harness
        .core()?
        .plugin_diagnostics()
        .explain_claimability(
            Some(&snapshot),
            &crate::SessionId::parse(actor.id())
                .map_err(|error| PluginHarnessError::Worker(error.to_string()))?,
        )
        .await?;
    let claimed = store
        .claim(&candidate, 128)
        .await?
        .into_iter()
        .any(|owned| owned.actor == actor);
    if claimed
        || explanation.eligibility != ClaimEligibility::Ineligible
        || !explanation
            .run_blockers
            .contains(&ClaimBlocker::Unserved { token })
    {
        Ok(PluginLawResult::Failed {
            message: format!(
                "node lacking the declared callback can claim or is explained eligible: {explanation:?}"
            ),
        })
    } else {
        Ok(PluginLawResult::Passed)
    }
}
async fn recorded_decisions(
    harness: &PluginHarness,
    callback: &PluginCallbackIdentity,
) -> Result<Vec<DecisionRef>, PluginHarnessError> {
    let Some(run) = harness.session().await?.current_turn().await? else {
        return Ok(Vec::new());
    };
    let store = harness.backend.durable();
    let mut decisions = Vec::new();
    if CallbackSlot::of(callback) == Some(CallbackSlot::BeforeTurn)
        && let Some(phase) =
            lash_core::runtime::durable::read_plugin_phase(store.as_ref(), &harness.session, &run)
                .await?
    {
        for (ordinal, contribution) in phase.before_turn.iter().enumerate() {
            if contribution.plugin_id == callback.owner {
                decisions.push(DecisionRef::Callback {
                    record: RecordLocator::TurnPhase {
                        session: harness.session.clone(),
                        run: run.clone(),
                    },
                    ordinal: u32::try_from(ordinal)
                        .map_err(|error| PluginHarnessError::Worker(error.to_string()))?,
                });
            }
        }
    }
    let owner = OwnerKey::Turn(harness.session.clone(), run.clone());
    let facts = store
        .run_facts_within(
            &owner,
            None,
            4096,
            lash_core_store::checkpoint_graph::DecodeLimits::within_total(16 * 1024 * 1024),
        )
        .await?;
    for row in &facts.rows {
        if row.kind != crate::durable::domain::RunFactKind::Admit {
            continue;
        }
        for (call, binding) in
            lash_core::admitted_bindings_of(&facts, row).map_err(PluginHarnessError::Worker)?
        {
            let selection = binding.and_then(|binding| {
                if binding.executable == *callback {
                    Some(ToolDecisionSelection::Executable)
                } else if binding.presentation.presenter.as_ref() == Some(callback)
                    || binding.presentation.steps.contains(callback)
                {
                    Some(ToolDecisionSelection::Presentation)
                } else {
                    None
                }
            });
            if let Some(selection) = selection {
                decisions.push(DecisionRef::Tool {
                    record: RecordLocator::RunFact {
                        owner: owner.clone(),
                        run: row.run,
                        ordinal: row.ordinal,
                    },
                    call,
                    selection,
                });
            }
        }
    }
    Ok(decisions)
}
async fn attribution_law(
    harness: &PluginHarness,
    callback: &PluginCallbackIdentity,
    decisions: &[DecisionRef],
) -> Result<PluginLawResult, PluginHarnessError> {
    if decisions.is_empty() {
        return Ok(unavailable(
            CertificationUnavailable::AttributionNotRecorded,
        ));
    }
    for decision in decisions {
        let result = harness
            .core()?
            .plugin_diagnostics()
            .attribute(decision)
            .await?;
        let matches = match result.evidence {
            Evidence::Recorded {
                value: RecordedAttribution::Binding { selected, .. },
                ..
            } => selected == *callback,
            Evidence::Recorded {
                value: RecordedAttribution::Presentation(binding),
                ..
            } => binding.presenter.as_ref() == Some(callback) || binding.steps.contains(callback),
            // Owner attribution records no callback key, so cannot certify
            // per-callback attribution even if this owner had only one hook.
            Evidence::Recorded {
                value: RecordedAttribution::Owner(_),
                ..
            } => {
                return Ok(unavailable(
                    CertificationUnavailable::AttributionNotRecorded,
                ));
            }
            Evidence::Unavailable { reason } => {
                return Ok(PluginLawResult::Failed {
                    message: format!(
                        "a retained decision became unavailable after removal: {reason:?}; {decision:?}"
                    ),
                });
            }
            _ => false,
        };
        if !matches
            || result
                .installed_now
                .iter()
                .any(|entry| entry.key.owner() == callback.owner)
        {
            return Ok(PluginLawResult::Failed {
                message: format!("recorded binding lost its callback after removal: {decision:?}"),
            });
        }
    }
    Ok(PluginLawResult::Passed)
}
