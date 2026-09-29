//! Arming the runtime-owned resolver a parked call named, and discharging the
//! cancel obligation its wait leaves behind.
//!
//! A [`ToolOutcome::Pending`](crate::ToolOutcome::Pending) that carries a
//! [`PendingResolver`](crate::PendingResolver) makes the *runtime* responsible
//! for delivering the outcome, not an out-of-band actor. This module is the one
//! place that discharges that responsibility, and every park site calls it
//! immediately before parking so the arming and the park are adjacent in the
//! journal.
//!
//! Arming runs on the first park **and on every redrive of the parked turn**.
//! It has to: a recorded attempt body does not re-run when its turn is
//! re-driven, so the tool that named the resolver never gets a second chance to
//! arm it, and an in-process watcher does not survive a crash. Because the
//! declaration is journaled with the pending launch, the redrive re-derives the
//! identical arming from the identical bytes, and the boundary's own idempotence
//! — resolving a wait that is already resolved reports
//! [`ResolveOutcome::AlreadyResolved`](crate::ResolveOutcome::AlreadyResolved) —
//! makes the repetition harmless.
//!
//! # Declared starts (ADR 0116 §3)
//!
//! A [`DeclaredStart`](crate::DeclaredStart) is launched here, before its
//! terminal is armed. The launch is the start's own journaled admission,
//! `process:start:{start key}`, under the call's lineage: it is admitted under
//! the call's cancel fence, so exactly one of the launch and a cancel decision
//! lands first, and a launch admitted before the decision still realizes on a
//! redrive. The registrar mints the child's id inside the registration; a
//! redrive presents the same start key and gets the same child back. The
//! realized start, or its typed refusal, is the call's launch receipt: its
//! intent outcome for index 0. Then the terminal of the realized id is armed,
//! from the recorded receipt, on the park and on every redrive.
//!
//! Every park site arms through here, so this is where a declaration is bound
//! to the call that admitted it, before anything is journaled or registered.
//! A declared start decodes without its constructor, so its bytes may name
//! another session, another call's identity or a nonzero index. The start
//! must name the admitted session, and its identity must be exactly the one
//! the declaring attempt derives for index 0
//! ([`PendingToolDispatchOutcome::declaring_identity`](crate::tool_dispatch::PendingToolDispatchOutcome::declaring_identity)).
//! A mismatch is the call's typed launch refusal, with nothing registered.
//!
//! # The cancel obligation (ADR 0116 §3.4)
//!
//! A parked wait on a process terminal whose wait is cancelled or times out
//! cancels that process when its [`CancelHint`](crate::CancelHint) is
//! `CancelExternalWork`. The cancel is one replay-keyed process command under
//! `{call id}:cancel-work`, so a redrive re-issues the same command and the
//! registry answers the same outcome. `CancelHint::Ignore` drops only the wait.

/// What arming a parked call's resolver left the call with.
#[derive(Debug)]
pub enum ResolverArming {
    /// The resolver is armed, and the call parks on its completion key.
    Armed(ArmedResolver),
    /// The call settles now, as `failure`, and never parks: its declared
    /// start was refused, or its resolver could not be armed.
    Settled {
        failure: Box<crate::ToolFailure>,
        armed: ArmedResolver,
    },
}

/// What an armed resolver launched.
#[derive(Clone, Debug, Default)]
pub struct ArmedResolver {
    /// A declared start's launch receipt, when the call declared one.
    pub launch: Option<LaunchReceipt>,
}

/// A declared start's launch receipt.
#[derive(Clone, Debug)]
pub struct LaunchReceipt {
    /// The realized start, or its typed refusal, as the call's intent
    /// outcome for index 0. It is host-facing metadata: the model sees the
    /// child's value only.
    pub outcome: crate::ToolIntentExecutionOutcome,
    /// The child the start registered, when it realized.
    pub process_id: Option<crate::ProcessId>,
}

impl ArmedResolver {
    /// The process whose terminal this wait awaits, if the runtime owns one.
    pub fn awaited_process<'a>(
        &'a self,
        pending: &'a crate::PendingCompletion,
    ) -> Option<&'a crate::ProcessId> {
        match pending.resolved_by.as_ref()? {
            crate::PendingResolver::ProcessTerminal { process_id } => Some(process_id),
            crate::PendingResolver::DeclaredStart(_) => self
                .launch
                .as_ref()
                .and_then(|launch| launch.process_id.as_ref()),
        }
    }

    /// The call's intent outcomes: the launch receipt, when there is one.
    pub fn intent_outcomes(&self) -> Vec<crate::ToolIntentExecutionOutcome> {
        self.launch
            .iter()
            .map(|launch| launch.outcome.clone())
            .collect()
    }
}

/// Where one parked call arms its resolver.
pub struct ParkSite<'a, 'scope> {
    pub processes: &'a dyn crate::ProcessService,
    /// The session the call runs in.
    pub session_id: &'a crate::SessionId,
    /// The call's id: the key material of its cancel obligation.
    pub call_id: &'a lash_sansio::ToolCallId,
    /// The call's lineage: the start, the arming and the cancel obligation
    /// are journaled beneath it.
    pub scope: crate::ProcessOpScope<'scope>,
    /// Fired once the declared start realizes, so a trace links call and
    /// child.
    pub child_trace_hook: Option<&'a crate::ToolChildExecutionTraceHook>,
}

/// Arms the resolver a parked call named.
///
/// A call with no resolver is left exactly as it was: nothing is armed and the
/// caller parks on a key only an external actor can resolve. A failure to arm
/// settles the call rather than parking it — parking on a wait whose resolver
/// was never armed hangs the call for the lifetime of the turn. An `Err` is a
/// controller refusal the caller returns rather than settles: a replay
/// divergence, a cancel decided before the launch, or an infrastructure fault
/// that recovery retries.
pub async fn arm_pending_resolver(
    site: &ParkSite<'_, '_>,
    parked: &crate::tool_dispatch::PendingToolDispatchOutcome,
) -> Result<ResolverArming, crate::RuntimeEffectControllerError> {
    let pending = &parked.pending;
    let key = &parked.key;
    match pending.resolved_by.as_ref() {
        None => Ok(ResolverArming::Armed(ArmedResolver::default())),
        Some(crate::PendingResolver::ProcessTerminal { process_id }) => {
            Ok(arm_terminal(site, process_id, key, ArmedResolver::default()).await)
        }
        Some(crate::PendingResolver::DeclaredStart(start)) => {
            let launch = match start.bound_to(&parked.declaring_identity) {
                Ok(()) => {
                    let cancels = pending.on_cancel == crate::CancelHint::CancelExternalWork;
                    launch_declared_start(site, start, key, cancels).await?
                }
                Err(refusal) => unbound_declaration(&parked.declaring_identity, refusal),
            };
            let Some(process_id) = launch.process_id.clone() else {
                let failure = launch_refusal(&launch.outcome);
                return Ok(ResolverArming::Settled {
                    failure: Box::new(failure),
                    armed: ArmedResolver {
                        launch: Some(launch),
                    },
                });
            };
            let armed = ArmedResolver {
                launch: Some(launch),
            };
            match arm_terminal(site, &process_id, key, armed).await {
                armed @ ResolverArming::Armed(_) => Ok(armed),
                // The call will not wait for the child it launched, so the
                // child is cancelled rather than left running unobserved.
                ResolverArming::Settled { failure, armed } => {
                    if cancel_owned_process(site, &process_id).await? == CancelDischarge::Met {
                        release_consumer_hold(site, &armed, key).await;
                    }
                    Ok(ResolverArming::Settled { failure, armed })
                }
            }
        }
    }
}

async fn arm_terminal(
    site: &ParkSite<'_, '_>,
    process_id: &crate::ProcessId,
    key: &crate::AwaitEventKey,
    armed: ArmedResolver,
) -> ResolverArming {
    match site
        .processes
        .attach_process_terminal(process_id, key, site.scope.clone())
        .await
    {
        Ok(()) => ResolverArming::Armed(armed),
        Err(error) => ResolverArming::Settled {
            failure: Box::new(crate::ToolFailure::runtime(
                crate::ToolFailureClass::Internal,
                "pending_tool_resolver_unarmed",
                format!("the declared resolver for this tool call could not be armed: {error}"),
            )),
            armed,
        },
    }
}

/// Launches the one start a pending call declared, under the start's own
/// journaled admission (see the module documentation).
async fn launch_declared_start(
    site: &ParkSite<'_, '_>,
    start: &crate::DeclaredStart,
    key: &crate::AwaitEventKey,
    cancels: bool,
) -> Result<LaunchReceipt, crate::RuntimeEffectControllerError> {
    let parent = launch_parent(site, start.identity());
    // The child is registered under the call's hold, owned by the scope the
    // call runs under, so the row outlives every redrive of the start.
    let hold = consumer_hold_owner(&site.scope).map(|owner| crate::ConsumerHold {
        key: key.key_id.clone(),
        owner,
        cancels,
    });
    let scope = site
        .scope
        .clone()
        .with_parent_invocation(Some(parent))
        .with_consumer_hold(hold);
    let outcome = super::intent_executor::realize_declared_start(
        site.processes,
        start,
        scope,
        site.child_trace_hook,
    )
    .await?;
    let process_id = match &outcome {
        crate::ToolIntentExecutionOutcome::Executed { result, .. } => {
            crate::process_id_from_handle_json(result).ok()
        }
        _ => None,
    };
    Ok(LaunchReceipt {
        outcome,
        process_id,
    })
}

/// The launch receipt of a declaration that does not belong to its call:
/// refused under the call's own identity for index 0, before anything was
/// journaled or registered.
fn unbound_declaration(
    declaring: &crate::ToolIntentIdentity,
    refusal: crate::ToolIntentRefusalReason,
) -> LaunchReceipt {
    tracing::warn!(
        target: "lash::tool_intent",
        tool_call_id = %declaring.tool_call_id,
        refusal_reason = refusal.code(),
        "a declared start that does not belong to its call was refused before launch"
    );
    LaunchReceipt {
        outcome: crate::ToolIntentExecutionOutcome::Refused {
            identity: Some(declaring.clone()),
            intent_index: declaring.intent_index,
            kind: crate::ToolIntentKind::StartProcess,
            refusal,
        },
        process_id: None,
    }
}

/// The scope that owns a call's consumer hold: the starter the call's
/// declared start is admitted under. A scope with no start context owns no
/// hold, and its calls launch unheld.
pub fn consumer_hold_owner(scope: &crate::ProcessOpScope<'_>) -> Option<crate::ScopeId> {
    scope
        .start_cx()
        .ok()
        .flatten()
        .map(|cx| cx.starter().id().clone())
}

/// The invocation a declared start is journaled beneath: the call's lineage,
/// or, for a call that carries none, an effect keyed by the start's intent
/// identity, exactly as an intent drain keys it.
#[expect(
    clippy::expect_used,
    reason = "the scope comes from the caller's own live effect controller, which is admitted by construction"
)]
fn launch_parent(
    site: &ParkSite<'_, '_>,
    identity: &crate::ToolIntentIdentity,
) -> crate::RuntimeInvocation {
    site.scope.parent_invocation.clone().unwrap_or_else(|| {
        let scoped = site.scope.effect_controller.scoped();
        let execution_scope = scoped.execution_scope();
        crate::RuntimeInvocation::effect(
            crate::EffectAddress::new(execution_scope.clone(), identity.replay_key.clone())
                .expect("a declared start carries an admitted effect scope"),
            execution_scope
                .session_id()
                .map(crate::RuntimeAttribution::for_session)
                .unwrap_or_else(crate::RuntimeAttribution::none),
            format!("tool-intent:{}", identity.intent_index),
        )
    })
}

/// The failure a refused launch settles its call with. A start the registry
/// or its scope refused keeps that refusal's own code; a declaration refused
/// because it does not belong to its call is the declaring tool's fault, and
/// fails under the refusal's typed code.
fn launch_refusal(outcome: &crate::ToolIntentExecutionOutcome) -> crate::ToolFailure {
    let (class, code, message) = match outcome {
        crate::ToolIntentExecutionOutcome::Refused {
            refusal: crate::ToolIntentRefusalReason::CommandFailed { code, message },
            ..
        } => (
            crate::ToolFailureClass::Unavailable,
            code.clone(),
            message.clone(),
        ),
        crate::ToolIntentExecutionOutcome::Refused { refusal, .. } => (
            crate::ToolFailureClass::Internal,
            refusal.code().to_string(),
            format!(
                "the declared start did not launch: {}",
                outcome.model_addendum()
            ),
        ),
        other => (
            crate::ToolFailureClass::Unavailable,
            "declared_start_refused".to_string(),
            format!(
                "the declared start did not launch: {}",
                other.model_addendum()
            ),
        ),
    };
    crate::ToolFailure::runtime(class, code, message)
}

/// What became of a cancel obligation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CancelDischarge {
    /// The obligation is met, or there was none.
    Met,
    /// The call's invocation could journal no cancel: the obligation stays
    /// with the hold, for the opener that abandoned the call to drain.
    LeftToOpener,
}

/// Discharges a parked call's cancel obligation once its wait has ended.
///
/// A wait that ended cancelled or timed out, on a process the runtime owns
/// and under [`CancelHint::CancelExternalWork`](crate::CancelHint), cancels
/// that process with one replay-keyed command. A terminal that won the race
/// settles the call normally and issues nothing; a cancel of a child that has
/// already ended is the registry's no-op.
async fn discharge_cancel_obligation(
    site: &ParkSite<'_, '_>,
    pending: &crate::PendingCompletion,
    armed: &ArmedResolver,
    resolution: &crate::Resolution,
) -> Result<CancelDischarge, crate::RuntimeEffectControllerError> {
    if !matches!(
        resolution,
        crate::Resolution::Cancelled | crate::Resolution::Timeout
    ) || pending.on_cancel != crate::CancelHint::CancelExternalWork
    {
        return Ok(CancelDischarge::Met);
    }
    let Some(process_id) = armed.awaited_process(pending) else {
        return Ok(CancelDischarge::Met);
    };
    cancel_owned_process(site, process_id).await
}

/// Releases the call's hold on the child its declared start launched (ADR
/// 0116 §3.6), once its wait has ended: from here a redrive replays the
/// start's recorded receipt rather than registering it again, so the row may
/// be pruned. The release is an idempotent registry write, repeated
/// harmlessly by every redrive; a failure is logged and leaves the hold to
/// the owning scope's close.
async fn release_consumer_hold(
    site: &ParkSite<'_, '_>,
    armed: &ArmedResolver,
    key: &crate::AwaitEventKey,
) {
    let Some(process_id) = armed
        .launch
        .as_ref()
        .and_then(|launch| launch.process_id.as_ref())
    else {
        return;
    };
    if let Err(error) = site
        .processes
        .release_consumer_hold(process_id, &key.key_id)
        .await
    {
        tracing::warn!(
            process_id = %process_id,
            error = %error,
            "a parked call could not release its hold on the child it launched"
        );
    }
}

/// Ends a parked call's wait: discharges its cancel obligation, then releases
/// its hold on the child it launched. Every park site calls this once the
/// wait has resolved, before it settles the call. An obligation left to the
/// opener keeps the hold: the hold is how the opener finds it.
pub async fn finish_parked_wait(
    site: &ParkSite<'_, '_>,
    pending: &crate::PendingCompletion,
    armed: &ArmedResolver,
    key: &crate::AwaitEventKey,
    resolution: &crate::Resolution,
) -> Result<(), crate::RuntimeEffectControllerError> {
    if discharge_cancel_obligation(site, pending, armed, resolution).await? == CancelDischarge::Met
    {
        release_consumer_hold(site, armed, key).await;
    }
    Ok(())
}

impl crate::tool_dispatch::PendingToolDispatchOutcome {
    /// Settles a call whose named resolver could not be armed as a failure,
    /// carrying the launch receipt when the start realized first.
    ///
    /// Deliberately a failure and not a park: a wait nobody is going to
    /// resolve is indistinguishable from a hang, and the turn would hold until
    /// it was cancelled. Reporting it here keeps the fault at the site that
    /// knows what it was trying to arm.
    pub fn settle_unarmed(
        self,
        failure: crate::ToolFailure,
        armed: &ArmedResolver,
    ) -> crate::tool_dispatch::ToolDispatchOutcome {
        crate::tool_dispatch::ToolDispatchOutcome {
            record: crate::ToolCallRecord {
                call_id: self.call_id,
                provider_call_id: self.provider_call_id,
                tool: self.tool_name,
                args: self.args,
                output: crate::ToolCallOutput::failure(failure),
            },
            attempts: self.attempts,
            intents: crate::ToolIntents::default(),
            intent_outcomes: armed.intent_outcomes(),
            captures: self.captures,
            triggers: self.triggers,
        }
    }
}

/// Drains the cancel obligation of a call its opener abandoned (ADR 0116
/// §3.4): a group that decided the cancel cancels, from the opener's own
/// journal, each process in `owed` — what the call's hold `key` held and owed
/// a cancel when the opener abandoned the hold, which also refuses any later
/// start under it — then releases the hold. The call's own invocation was
/// cancelled with it and may never reach its park site's discharge.
pub async fn discharge_abandoned_call(
    site: &ParkSite<'_, '_>,
    key: &crate::AwaitEventKey,
    owed: &[crate::ProcessId],
) -> Result<(), crate::RuntimeEffectControllerError> {
    for process_id in owed {
        // An opener that could not journal the cancel either leaves the hold
        // to its scope's close.
        if cancel_owned_process(site, process_id).await? == CancelDischarge::LeftToOpener {
            continue;
        }
        if let Err(error) = site
            .processes
            .release_consumer_hold(process_id, &key.key_id)
            .await
        {
            tracing::warn!(
                process_id = %process_id,
                error = %error,
                "an abandoned call could not release its hold on the child it launched"
            );
        }
    }
    Ok(())
}

/// Issues the replay-keyed cancel of a process the call owns, under the
/// call's `cancel-work` key.
async fn cancel_owned_process(
    site: &ParkSite<'_, '_>,
    process_id: &crate::ProcessId,
) -> Result<CancelDischarge, crate::RuntimeEffectControllerError> {
    let scoped = site.scope.effect_controller.scoped();
    let suffix = crate::runtime::effect::tool_cancel_work_replay_suffix(site.call_id);
    let parent = site.scope.parent_invocation.as_ref().map(|parent| {
        let parent_effect_id = parent.effect_id().unwrap_or("tool");
        crate::runtime::causal::child_effect_invocation(
            scoped.execution_scope(),
            parent,
            format!("{parent_effect_id}:{suffix}"),
            &suffix,
        )
        .into_runtime_invocation()
    });
    let scope = site.scope.clone().with_parent_invocation(parent);
    match site
        .processes
        .cancel(site.session_id, process_id, scope)
        .await
    {
        Ok(_) => Ok(CancelDischarge::Met),
        Err(error) => match super::intent_executor::declared_start_fault(&error) {
            // The invocation that owns the call can journal nothing more: a
            // group child whose group decided the cancel. That group's
            // opener drains the obligation from its own journal
            // (`discharge_abandoned_call`).
            Some(fault)
                if matches!(
                    fault.code,
                    crate::RuntimeErrorCode::RuntimeEffectGroupChildCancelDecided
                        | crate::RuntimeErrorCode::RuntimeEffectGroupChildCancelled
                ) =>
            {
                tracing::warn!(
                    process_id = %process_id,
                    error = %fault,
                    "a parked call's cancel obligation is left to its opener"
                );
                Ok(CancelDischarge::LeftToOpener)
            }
            // Any other fault ends the attempt; its redrive issues the same
            // replay-keyed cancel again.
            Some(fault) => Err(fault),
            None => {
                tracing::warn!(
                    process_id = %process_id,
                    error = %error,
                    "a parked call's cancel obligation found nothing to cancel"
                );
                Ok(CancelDischarge::Met)
            }
        },
    }
}

/// The intent outcomes a call's model-facing return reports.
///
/// A parked call declares no intents, so the one outcome it can carry is its
/// declared start's launch receipt: host-facing metadata that names the child
/// (ADR 0116 §3.8). The model sees the child's value only, so the receipt is
/// left out of the return's addenda.
pub fn model_visible_intent_outcomes(
    outcome: &super::ToolDispatchOutcome,
) -> &[crate::ToolIntentExecutionOutcome] {
    let launch_receipt = outcome.intents.intents.is_empty()
        && matches!(
            outcome.intent_outcomes.as_slice(),
            [crate::ToolIntentExecutionOutcome::Executed {
                kind: crate::ToolIntentKind::StartProcess,
                ..
            } | crate::ToolIntentExecutionOutcome::Refused {
                kind: crate::ToolIntentKind::StartProcess,
                ..
            }]
        );
    if launch_receipt {
        &[]
    } else {
        &outcome.intent_outcomes
    }
}
