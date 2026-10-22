//! Inventory-driven laws; lack of an exercised branch is never a pass.
use super::*;
use crate::plugins::CallbackSlot;
use crate::plugins::InstalledInventory;
use crate::plugins::PluginCallbackIdentity;
use crate::plugins::diagnostics::*;
use lash_vocabulary::claims::RegistryKind;

/// The applicable durability or observation contracts for each callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginLaw {
    ReplayInvokesNoRecordedCallback,
    StateSurvivesReopen,
    UnservedIsUnclaimable,
    RecordedDecisionsStayAttributable,
    /// An advisory observer completes the lifecycle scenario successfully.
    LifecycleObserverCompletes,
}
impl std::fmt::Display for PluginLaw {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ReplayInvokesNoRecordedCallback => "REPLAY-INVOKES-NO-RECORDED-CALLBACK",
            Self::StateSurvivesReopen => "STATE-SURVIVES-REOPEN",
            Self::UnservedIsUnclaimable => "UNSERVED-IS-UNCLAIMABLE",
            Self::RecordedDecisionsStayAttributable => "RECORDED-DECISIONS-STAY-ATTRIBUTABLE",
            Self::LifecycleObserverCompletes => "LIFECYCLE-OBSERVER-COMPLETES",
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CertificationUnavailable {
    /// No author scenario was supplied for a callback requiring one.
    NoScenario,
    /// The scenario reached its cut without invoking this callback. Counts
    /// exclude completed setup turns; `invocations_after_reopen` counts new
    /// entries during reopen and continuation, rather than the cumulative total.
    /// `supplied` distinguishes an author scenario from the automatic scenario.
    /// For a deferred provider, entries count resolutions, not preparation or
    /// execution of its selected tool. Standard protocol tool calls do not
    /// resolve deferred paths; exercise them in an RLM cell under a supporting
    /// protocol installed with [`PluginHarness::install_protocol`].
    ScenarioNotExercised {
        supplied: bool,
        invocations_before_cut: u64,
        invocations_after_reopen: u64,
    },
    NeedsVmBackend,
    MissingService {
        service: String,
    },
    MissingCredential {
        credential: String,
    },
    AttributionNotRecorded,
    NotRecordedAtCut,
    CounterUnavailable,
    /// The callback can publish state, but the scenario left its namespace
    /// empty. A plugin whose recipe is declared
    /// [`stateless`](crate::plugins::PluginRecipe::stateless) cannot publish,
    /// so its state law is not applicable instead.
    ScenarioDidNotPublish,
}
/// A law has no subject in this exercised contribution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CertificationNotApplicable {
    NoPublishedState,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PluginLawResult {
    Passed,
    Failed { message: String },
    NotApplicable { reason: CertificationNotApplicable },
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
    /// Every applicable callback law was exercised and passed.
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
    /// Lifecycle observer scenarios instead run through turn completion.
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
        // Lifecycle observers are advisory, post-commit notifications. They
        // publish no decision or state and have no recorded crash cut. Prove
        // completion through the real engine instead of demanding replay
        // evidence that their contract does not promise (ADR 0132 §4).
        if slot == CallbackSlot::RuntimeEvent {
            let result = if let Some(reason) = scenario.and_then(|s| s.availability().err()) {
                unavailable(reason)
            } else {
                lifecycle_observer_law(Arc::clone(&factory), &policy, &callback, scenario).await
            };
            reports.push(PluginLawReport {
                callback,
                law: PluginLaw::LifecycleObserverCompletes,
                result,
            });
            continue;
        }
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
        let initial = callback_entries(&harness, &callback);
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
        let recorded = callback_entries(&harness, &callback);
        let invoked = recorded.0.saturating_sub(initial.0);
        let state = harness.plugin_state().await?;
        let decisions = recorded_decisions(&harness, &callback).await?;
        harness.crash();
        harness.reopen_without_plugin().await?;
        let attribution = attribution_law(&harness, &callback, &decisions).await?;
        harness.reopen_at_model_pause().await?;
        let restored = harness.plugin_state().await?;
        let state_result = if state.plugins.get(factory.id()) != restored.plugins.get(factory.id())
        {
            PluginLawResult::Failed {
                message: "published namespace changed on fresh materialization".into(),
            }
        } else if state
            .plugins
            .get(factory.id())
            .is_none_or(|namespace| namespace.values.is_empty())
        {
            let has_reducer = inventory.entries.iter().any(|entry| {
                entry.key.owner() == factory.id()
                    && entry.kind == crate::plugins::ContributionKind::StateReducer
            });
            // Resolver entries are decision-only. A provider's actual tool
            // body can publish commands even though its slot is DecisionOnly.
            let tool_body = slot == CallbackSlot::ToolProvider
                && (!harness.deferred_resolution_counts().contains_key(&callback)
                    || recorded.1 > 0);
            // A plugin declared stateless has every command refused, so none
            // of its callbacks can publish.
            if inventory.keeps_state(factory.id())
                && (slot.state_authority() == lash_vocabulary::providers::StateAuthority::Commands
                    || tool_body
                    || has_reducer)
            {
                unavailable(CertificationUnavailable::ScenarioDidNotPublish)
            } else {
                PluginLawResult::NotApplicable {
                    reason: CertificationNotApplicable::NoPublishedState,
                }
            }
        } else {
            PluginLawResult::Passed
        };
        // Reattach the accepted input: no new send and no new turn is admitted.
        harness.release_model();
        let outcome = tokio::time::timeout(
            policy.scenario_timeout,
            harness.session().await?.attach(input_id).output(),
        )
        .await;
        let after = callback_entries(&harness, &callback);
        if invoked == 0 {
            let reason = CertificationUnavailable::ScenarioNotExercised {
                supplied: scenario.is_some(),
                invocations_before_cut: invoked,
                invocations_after_reopen: after.0.saturating_sub(recorded.0),
            };
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
        let replay = if !recorded_at_cut(slot) {
            unavailable(CertificationUnavailable::NotRecordedAtCut)
        } else if after != recorded {
            PluginLawResult::Failed {
                message: format!(
                    "recorded callback invoked again: {recorded:?} before crash, {after:?} after reopen"
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
// Session materialization delivers SessionRestored, then the engine sends
// TurnFinalized after committing a scripted turn. The caller's answer alone
// is insufficient: wait for the counted observer's own completion receipt.
async fn lifecycle_observer_law(
    factory: Arc<dyn PluginFactory>,
    policy: &HostPolicy,
    callback: &PluginCallbackIdentity,
    scenario: Option<&Arc<dyn PluginScenario>>,
) -> PluginLawResult {
    let outcome = tokio::time::timeout(policy.scenario_timeout, async {
        let mut harness = PluginHarness::new(factory, policy.clone()).await?;
        if let Some(scenario) = scenario {
            scenario.prepare(&mut harness).await?;
            // Setup turns may answer before their advisory observers finish.
            // End that runtime before starting fresh completion evidence, so
            // even a late setup notification cannot certify this turn.
            harness.crash();
            harness.counts = CallbackCounts::default();
            harness.reopen().await?;
            harness.script(scenario.responses());
        } else {
            harness
                .create(crate::plugins::PluginOptions::default())
                .await?;
            harness.script([text_response("certified")]);
        }
        let input = scenario.map_or_else(
            || crate::TurnInput::text("certify lifecycle observer"),
            |scenario| scenario.input(),
        );
        harness.send(input).await?.output().await?;
        harness
            .counts
            .lifecycle_completed(callback)
            .await
            .map_err(PluginHarnessError::Worker)
    })
    .await;
    match outcome {
        Ok(Ok(())) => PluginLawResult::Passed,
        Ok(Err(error)) => PluginLawResult::Failed {
            message: format!("lifecycle observer scenario failed: {error}"),
        },
        Err(_) => PluginLawResult::Failed {
            message: "lifecycle observer did not complete within the host scenario timeout".into(),
        },
    }
}
// Resolver laws require resolution, not a callable placeholder under the same
// provider identity. Check both counters for replay when it also executes tools.
fn callback_entries(harness: &PluginHarness, callback: &PluginCallbackIdentity) -> (u64, u64) {
    let executable = harness
        .callback_counts()
        .get(callback)
        .copied()
        .unwrap_or(0);
    harness
        .deferred_resolution_counts()
        .get(callback)
        .map_or((executable, 0), |resolved| (*resolved, executable))
}
fn unavailable(reason: CertificationUnavailable) -> PluginLawResult {
    PluginLawResult::Unavailable { reason }
}
fn recorded_at_cut(slot: CallbackSlot) -> bool {
    matches!(
        slot,
        CallbackSlot::BeforeTurn
            | CallbackSlot::CellFailureFeedback
            | CallbackSlot::ToolProvider
            | CallbackSlot::DeferredPackageProvider
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
        lash_vocabulary::claims::ObligationToken::for_callback(RegistryKind::Session, callback)
            .to_string();
    let writer = store
        .register_node(&NodeSpec {
            node: NodeId::new(format!("callback-writer-{ordinal}")),
            decodes: formats.clone(),
            serves: vec![token.clone()],
            capabilities: Default::default(),
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
            capabilities: Default::default(),
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
            &lash_vocabulary::identity::SessionId::parse(actor.id())
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
// Keep cell report addresses across removal, rather than materializing a
// report before the crash and then again for every call it contains.
enum RecordedDecision {
    Direct(DecisionRef),
    DeferredReport(RecordLocator),
}
async fn recorded_decisions(
    harness: &PluginHarness,
    callback: &PluginCallbackIdentity,
) -> Result<Vec<RecordedDecision>, PluginHarnessError> {
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
                decisions.push(RecordedDecision::Direct(DecisionRef::Callback {
                    record: RecordLocator::TurnPhase {
                        session: harness.session.clone(),
                        run: run.clone(),
                    },
                    ordinal: u32::try_from(ordinal)
                        .map_err(|error| PluginHarnessError::Worker(error.to_string()))?,
                }));
            }
        }
    }
    if CallbackSlot::of(callback) == Some(CallbackSlot::CellFailureFeedback) {
        let record = RecordLocator::TurnPhase {
            session: harness.session.clone(),
            run: run.clone(),
        };
        for attribution in harness
            .core()?
            .plugin_diagnostics()
            .cell_failure_attributions(&record)
            .await?
        {
            if matches!(&attribution.evidence, Evidence::Recorded { value: RecordedAttribution::CellFailureFeedback { selected, .. }, .. } if selected == callback)
            {
                decisions.push(RecordedDecision::Direct(attribution.decision));
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
        if row.kind == crate::durable::domain::RunFactKind::CellReport {
            let record = RecordLocator::RunFact {
                owner: owner.clone(),
                run: row.run,
                ordinal: row.ordinal,
            };
            if harness.deferred_resolution_counts().contains_key(callback) {
                decisions.push(RecordedDecision::DeferredReport(record));
            }
            continue;
        }
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
                decisions.push(RecordedDecision::Direct(DecisionRef::Tool {
                    record: RecordLocator::RunFact {
                        owner: owner.clone(),
                        run: row.run,
                        ordinal: row.ordinal,
                    },
                    call,
                    selection,
                }));
            }
        }
    }
    Ok(decisions)
}
async fn attribution_law(
    harness: &PluginHarness,
    callback: &PluginCallbackIdentity,
    decisions: &[RecordedDecision],
) -> Result<PluginLawResult, PluginHarnessError> {
    if decisions.is_empty() {
        return Ok(unavailable(
            CertificationUnavailable::AttributionNotRecorded,
        ));
    }
    let diagnostics = harness.core()?.plugin_diagnostics();
    let mut evaluated = false;
    for decision in decisions {
        let attributions = match decision {
            RecordedDecision::Direct(decision) => vec![diagnostics.attribute(decision).await?],
            RecordedDecision::DeferredReport(record) => diagnostics
                .deferred_attributions(record)
                .await?
                .into_iter()
                .filter(|result| matches!(&result.evidence,
                    Evidence::Recorded { value: RecordedAttribution::DeferredTool { resolver, .. }, .. }
                        if resolver == callback))
                .collect(),
        };
        for result in attributions {
            evaluated = true;
            let decision = &result.decision;
            let matches = match result.evidence {
                Evidence::Recorded {
                    value:
                        RecordedAttribution::Binding { selected, .. }
                        | RecordedAttribution::CellFailureFeedback { selected, .. },
                    ..
                } => selected == *callback,
                Evidence::Recorded {
                    value: RecordedAttribution::DeferredTool { resolver, .. },
                    ..
                } => resolver == *callback,
                Evidence::Recorded {
                    value: RecordedAttribution::Presentation(binding),
                    ..
                } => {
                    binding.presenter.as_ref() == Some(callback) || binding.steps.contains(callback)
                }
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
                    message: format!(
                        "recorded binding lost its callback after removal: {decision:?}"
                    ),
                });
            }
        }
    }
    Ok(if evaluated {
        PluginLawResult::Passed
    } else {
        unavailable(CertificationUnavailable::AttributionNotRecorded)
    })
}

#[cfg(all(test, feature = "rlm"))]
#[path = "certification/workflows_create.rs"]
mod workflows_create;

#[cfg(all(test, feature = "rlm"))]
#[path = "certification/packages.rs"]
mod packages;

#[cfg(all(test, feature = "rlm"))]
mod tests {
    use super::packages::*;
    use super::workflows_create;
    use crate as lash;
    use lash::plugins::*;
    use lash::testing::*;
    use lash::tools::*;
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Resolver {
        empty: bool,
        resolutions: Arc<AtomicUsize>,
        definition: ToolDefinition,
        workflow: ToolDefinition,
    }
    #[lash::async_trait]
    impl ToolProvider for Resolver {
        fn tool_manifests(&self, _: &OwnerConfigView) -> Result<Vec<ToolManifest>, PluginError> {
            Ok(Vec::new())
        }
        fn resolve_contract(&self, _: &str, _: &OwnerConfigView) -> Option<Arc<ToolContract>> {
            None
        }
        async fn execute(&self, _: ToolCall<'_>) -> ToolAttemptOutcome {
            panic!("the resolver is not an executable")
        }
    }
    fn definition(id: &str, operation: &str) -> Result<ToolDefinition, PluginError> {
        ToolDefinition::raw(
            id,
            operation,
            "A certification fixture tool.",
            serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
            serde_json::json!({"type":"string"}),
        )
        .map(|draft| draft.with_execution(std::time::Duration::from_secs(1)))
        .map_err(|error| PluginError::Registration(error.to_string()))
    }
    #[lash::async_trait]
    impl DeferredToolProvider for Resolver {
        fn searchable_names(&self, _: &DeferredResolveContext<'_>) -> BTreeMap<String, String> {
            BTreeMap::from([("workflows_create".into(), "workflows.create".into())])
        }
        async fn resolve(
            &self,
            _: &DeferredResolveContext<'_>,
            paths: &[&str],
        ) -> BTreeMap<String, DeferredOutcome> {
            self.resolutions.fetch_add(1, Ordering::SeqCst);
            paths
                .iter()
                .filter(|path| {
                    !self.empty && matches!(**path, "tools.send_email" | "workflows.create")
                })
                .map(|path| {
                    (
                        (*path).into(),
                        DeferredOutcome::resolved(
                            ToolProviderHandle::new(
                                RegistryKind::Session,
                                "host",
                                lash::tool_provider_key!("tools"),
                            ),
                            if *path == "workflows.create" {
                                self.workflow.clone()
                            } else {
                                self.definition.clone()
                            },
                            None,
                        ),
                    )
                })
                .collect()
        }
    }
    struct Host;
    #[lash::async_trait]
    impl ToolProvider for Host {
        fn tool_manifests(&self, _: &OwnerConfigView) -> Result<Vec<ToolManifest>, PluginError> {
            Ok(vec![definition("host:lookup", "local.lookup")?.manifest])
        }
        fn resolve_contract(&self, name: &str, _: &OwnerConfigView) -> Option<Arc<ToolContract>> {
            if name != "local.lookup" {
                return None;
            }
            Some(Arc::new(
                definition("host:lookup", "local.lookup").ok()?.contract,
            ))
        }
        async fn execute(&self, _: ToolCall<'_>) -> ToolAttemptOutcome {
            ToolOutcome::ok(serde_json::json!("ok")).into()
        }
    }
    struct Scenario {
        code: &'static str,
        callback: PluginCallbackIdentity,
        deferred: Option<Arc<AtomicUsize>>,
        packages: bool,
    }
    #[lash::async_trait]
    impl PluginScenario for Scenario {
        fn callback(&self) -> PluginCallbackIdentity {
            self.callback.clone()
        }
        async fn prepare(&self, harness: &mut PluginHarness) -> Result<(), PluginHarnessError> {
            let protocol: Arc<dyn PluginFactory> = Arc::new(
                lash::rlm::RlmProtocolPluginFactory::new(
                    lash::rlm::RlmProtocolPluginConfig::builder()
                        .channel(lash::rlm::RlmChannel::NativeTool)
                        .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
                        .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
                        .build(),
                    lash::rlm::CellDialect::typescript(),
                )
                .with_worker_service(lash::vm::WorkerService::default()),
            );
            harness.install_protocol(protocol).await?;
            harness
                .install(Arc::new(StaticPluginFactory::new(
                    PluginDeclaration::initial("host"),
                    if self.packages {
                        deferred_host(Arc::new(Host))
                    } else {
                        PluginSpec::new()
                            .with_tool_provider(lash::tool_provider_key!("tools"), Arc::new(Host))
                    },
                )))
                .await?;
            if let Some(resolutions) = &self.deferred {
                harness
                    .install(factory(false, Arc::clone(resolutions))?)
                    .await?;
            }
            harness.create(PluginOptions::default()).await
        }
        fn input(&self) -> lash::TurnInput {
            lash::TurnInput::text("exercise the cell")
        }
        fn responses(&self) -> Vec<lash::provider::LlmResponse> {
            vec![
                lash::provider::LlmResponse {
                    parts: vec![lash::direct::LlmOutputPart::ToolCall {
                        call_id: "certification-cell".into(),
                        tool_name: "execute_code".into(),
                        input_json: serde_json::json!({"code": self.code}).to_string(),
                        replay: None,
                    }],
                    ..Default::default()
                },
                text_response("done"),
            ]
        }
        fn pause_after_responses(&self) -> usize {
            1
        }
    }
    fn factory(
        empty: bool,
        resolutions: Arc<AtomicUsize>,
    ) -> Result<Arc<dyn PluginFactory>, PluginError> {
        Ok(Arc::new(StaticPluginFactory::new(
            PluginDeclaration::initial("resolver"),
            PluginSpec::new().with_deferred_provider(
                lash::tool_provider_key!("deferred"),
                crate::tools::DeferredToolProviderRegistration::new(Arc::new(Resolver {
                    empty,
                    resolutions,
                    definition: definition("remote:send_email", "tools.send_email")?,
                    workflow: workflows_create::definition()?,
                })),
            ),
        )))
    }

    /// CELL-REPORT-DECODE-IS-LINEAR: attribution after factory removal reads
    /// one report once, including a static call and three deferred calls.
    #[tokio::test]
    async fn certification_decodes_each_cell_report_once() {
        let decoded = Arc::new(AtomicUsize::new(0));
        let scenario: Arc<dyn PluginScenario> = Arc::new(Scenario {
            code: "await local.lookup({}); await tools.sendEmail({}); await tools.sendEmail({}); await tools.sendEmail({});",
            callback: resolver_callback(),
            deferred: None,
            packages: true,
        });
        let report = crate::plugin_diagnostics::CELL_REPORT_DECODES
            .scope(
                Arc::clone(&decoded),
                certify_plugin_with(
                    Arc::new(StaticPluginFactory::new(
                        PluginDeclaration::initial("resolver"),
                        PluginSpec::new().with_deferred_package_provider(
                            lash::tool_provider_key!("deferred"),
                            Arc::new(PackageResolver),
                        ),
                    )),
                    HostPolicy::standard(),
                    &[scenario],
                ),
            )
            .await
            .unwrap();
        assert_eq!(
            report.verdict(),
            PluginCertificationVerdict::Full,
            "{:#?}",
            report.reports
        );
        assert_eq!(
            decoded.load(Ordering::SeqCst),
            1,
            "one immutable report must be decoded once after removal"
        );
    }

    fn repair_factory(hook: CellFailureFeedbackHook) -> Arc<dyn PluginFactory> {
        Arc::new(StaticPluginFactory::new(
            PluginDeclaration::initial("repair"),
            PluginSpec::new().with_cell_failure_feedback(lash::hook_key!("names"), hook),
        ))
    }
    fn repair_scenario(deferred: Option<Arc<AtomicUsize>>) -> Arc<dyn PluginScenario> {
        Arc::new(Scenario {
            code: "await workflows_create({});",
            callback: PluginCallbackIdentity {
                owner: "repair".into(),
                key: "cell_failure_feedback:names".into(),
            },
            deferred,
            packages: false,
        })
    }

    /// FIG-6362: an unknown bare name sees the unloaded searchable tool,
    /// without admitting it to the cell or invoking its resolver.
    #[tokio::test]
    async fn cell_failure_diagnostic_names_unloaded_deferred_tools() {
        let seen = Arc::new(AtomicUsize::new(0));
        let hook: CellFailureFeedbackHook = {
            let seen = Arc::clone(&seen);
            Arc::new(move |diagnostic| {
                assert_eq!(diagnostic.unknown_name, Some("workflows_create"));
                assert!(!diagnostic.grants.contains_key("workflows_create"));
                if diagnostic
                    .deferred
                    .get("workflows_create")
                    .map(String::as_str)
                    == Some("workflows.create")
                {
                    seen.fetch_add(1, Ordering::SeqCst);
                }
                vec!["Use workflows.create".into()]
            })
        };
        let resolutions = Arc::new(AtomicUsize::new(0));
        let report = certify_plugin_with(
            repair_factory(hook),
            HostPolicy::standard(),
            &[repair_scenario(Some(Arc::clone(&resolutions)))],
        )
        .await
        .unwrap();
        assert!(!report.reports.is_empty());
        assert_eq!(
            resolutions.load(Ordering::SeqCst),
            0,
            "names must not resolve or authorize tools"
        );
        assert_eq!(
            seen.load(Ordering::SeqCst),
            1,
            "the hook must see the deferred call path"
        );
    }

    /// FIG-6362: the recorded repair certifies replay and exact callback
    /// attribution after its factory has been removed.
    #[tokio::test]
    async fn cell_failure_feedback_alone_certifies_fully() {
        let report = certify_plugin_with(
            repair_factory(Arc::new(|_| vec!["Use the dotted callable".into()])),
            HostPolicy::standard(),
            &[repair_scenario(None)],
        )
        .await
        .unwrap();
        assert_eq!(report.reports.len(), 4);
        assert_eq!(
            report.verdict(),
            PluginCertificationVerdict::Full,
            "{:#?}",
            report.reports
        );
        for law in [
            PluginLaw::ReplayInvokesNoRecordedCallback,
            PluginLaw::RecordedDecisionsStayAttributable,
        ] {
            assert!(
                report
                    .reports
                    .iter()
                    .any(|report| report.law == law && report.result == PluginLawResult::Passed)
            );
        }
    }
    struct PackageResolver;
    #[lash::async_trait]
    impl lash::package::DeferredPackageProvider for PackageResolver {
        fn validate_grant(
            &self,
            grant: &lash::package::DeferredGrant,
            registry: RegistryKind,
        ) -> Result<(), lash::package::AdmissionRefusal> {
            validate_resolver_grant(grant, registry)
        }
        async fn resolve(
            &self,
            _: &DeferredResolveContext<'_>,
            _: &lash::package::DeferredGrant,
            _: &lash::package::QualifiedName,
        ) -> Result<lash::package::DeferredAnswer, lash::package::ResolverFault> {
            let definition =
                definition("remote:send_email", "tools.send_email").map_err(|error| {
                    lash::package::ResolverFault {
                        message: error.to_string(),
                    }
                })?;
            Ok(lash::package::DeferredAnswer::Resolved {
                route: Box::new(lash::tools::OperationRoute::resolved(
                    ToolProviderHandle::new(
                        RegistryKind::Session,
                        "host",
                        lash::tool_provider_key!("tools"),
                    ),
                    definition.clone(),
                    None,
                )),
                definition: Box::new(definition),
            })
        }
    }
}
