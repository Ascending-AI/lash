//! Deferred dispatches arm completion keys and wait through removable observers.

use super::*;
use crate::durable_wait::{RestateDurableWaitAwakeableRequest, RestateDurableWaitRegistration};
use lash_core::{ToolCompletionEvent, ToolCompletionWait, ToolDispatchCursor};

pub(super) async fn arm<'ctx, C>(
    context: &C,
    namespace: &crate::RestateNamespace,
    key: lash_core::AwaitEventKey,
) -> Result<(), TerminalError>
where
    C: ContextClient<'ctx>,
{
    let address = RestateDurableWaitAddress::for_key(&key);
    let registry =
        namespace.durable_wait_registry(context, durable_wait_index_object_key(&address));
    let registered = registry
        .register(RestateDurableWaitIndexRequest { key: key.clone() })
        .call()
        .await?
        .into_body();
    if matches!(registered, RestateDurableWaitRegistration::Revoked) {
        return Err(revoked());
    }
    Ok(())
}

pub(super) struct WaitRequest {
    pub waits: Vec<ToolCompletionWait>,
    pub dispatch: Option<ToolDispatchCursor>,
    pub turn_cancel: Option<RestateDurableWaitAwaitRequest>,
    pub generation: Option<lash_core::engine::BuildGeneration>,
}

pub(super) enum CompletionWake {
    Completion { position: usize },
    DispatchReady,
    HandedOver,
    Revoked,
}

pub(super) struct WaitAwakeables<'run> {
    pub completion:
        Box<dyn Fn(usize) -> (String, GateWait<'run, CompletionWake>) + Send + Sync + 'run>,
    pub dispatch: Box<dyn Fn() -> (String, GateWait<'run, CompletionWake>) + Send + Sync + 'run>,
    pub gate:
        Box<dyn Fn() -> (String, GateWait<'run, Json<RestateTurnCancelWake>>) + Send + Sync + 'run>,
}

pub(super) async fn wait<'run, C>(
    context: &'run C,
    namespace: &'run crate::RestateNamespace,
    request: WaitRequest,
    awakeables: WaitAwakeables<'run>,
    process_cancel: Option<GateWait<'run, String>>,
    process_hand_over: Option<GateWait<'run, String>>,
) -> Result<RestateTurnCancelRaceOutcome<ToolCompletionEvent>, TerminalError>
where
    C: ContextClient<'run> + ContextAwakeables<'run>,
{
    let WaitRequest {
        waits,
        dispatch,
        turn_cancel,
        generation,
    } = request;
    let mut observers = Vec::new();
    let mut subscription = None;
    let outcome = async {
        let mut events: Vec<GateWait<'run, CompletionWake>> = Vec::new();
        for (position, wait) in waits.iter().enumerate() {
            let (awakeable_id, awakeable) = (awakeables.completion)(position);
            let entry = RestateDurableWaitAwakeableRequest {
                key: wait.key.clone(),
                awakeable_id,
                hand_over: None,
            };
            let address = RestateDurableWaitAddress::for_key(&entry.key);
            observers.push(entry.clone());
            let registered = namespace
                .durable_wait_registry(context, durable_wait_index_object_key(&address))
                .register_awakeable(entry.clone())
                .call()
                .await?
                .into_body();
            if matches!(registered, RestateDurableWaitRegistration::Revoked) {
                return Err(revoked());
            }
            events.push(awakeable);
        }
        if let Some(dispatch) = dispatch {
            let (awakeable_id, awakeable) = (awakeables.dispatch)();
            let subscribed = namespace
                .effect_group_state(context, dispatch.group_key.clone())
                .subscribe(EffectGroupSubscribeRequest {
                    notice: EffectGroupNotice::Rank {
                        rank: dispatch.rank,
                    },
                    awakeable_id: awakeable_id.clone(),
                })
                .call()
                .await?
                .into_body();
            match subscribed {
                EffectGroupSubscribeResponse::Notified { notification } => {
                    context.resolve_awakeable(&awakeable_id, Json(notification))
                }
                EffectGroupSubscribeResponse::Refused { outstanding } => {
                    return Err(crate::effect_group::subscription_refused(
                        &dispatch.group_key,
                        outstanding,
                    ));
                }
                EffectGroupSubscribeResponse::Subscribed => {}
            }
            subscription = Some((dispatch.group_key, awakeable_id));
            events.push(awakeable);
        }
        if events.is_empty() {
            return Err(TerminalError::new("a tool completion race has no events"));
        }
        match turn_cancel {
            Some(turn_cancel) => {
                let session_id = turn_cancel.key.scope.session_id().cloned().ok_or_else(|| {
                    TerminalError::new("a tool completion wait has no session id")
                })?;
                match gate_race::race_turn_gate_many(
                    context,
                    namespace,
                    &session_id,
                    turn_cancel,
                    generation,
                    || (awakeables.gate)(),
                    move || events,
                )
                .await?
                {
                    TurnGateRace::HandedOver => Ok(RestateTurnCancelRaceOutcome::Completed(
                        CompletionWake::HandedOver,
                    )),
                    TurnGateRace::Ended(outcome) => Ok(outcome),
                }
            }
            None => match segment_wait::race_segment_wait(
                events,
                process_cancel,
                process_hand_over,
                generation.as_ref(),
            )
            .await?
            {
                TurnGateRace::HandedOver => Ok(RestateTurnCancelRaceOutcome::Completed(
                    CompletionWake::HandedOver,
                )),
                TurnGateRace::Ended(outcome) => Ok(outcome),
            },
        }
    }
    .await;
    // A handover retires only these observers. The original keys remain armed.
    for entry in &observers {
        let address = RestateDurableWaitAddress::for_key(&entry.key);
        namespace
            .durable_wait_registry(context, durable_wait_index_object_key(&address))
            .unregister_awakeable(entry.clone())
            .call()
            .await?;
    }
    if let Some((group_key, awakeable_id)) = subscription {
        namespace
            .effect_group_state(context, group_key)
            .unsubscribe(EffectGroupUnsubscribeRequest { awakeable_id })
            .call()
            .await?;
    }
    if matches!(
        &outcome,
        Ok(RestateTurnCancelRaceOutcome::TurnCancelled
            | RestateTurnCancelRaceOutcome::ProcessCancelled)
    ) {
        for entry in observers {
            let address = RestateDurableWaitAddress::for_key(&entry.key);
            namespace
                .durable_wait_registry(context, durable_wait_index_object_key(&address))
                .resolve(RestateDurableWaitResolveRequest {
                    key: entry.key,
                    resolution: Resolution::Cancelled,
                })
                .call()
                .await?;
        }
    }
    Ok(match outcome? {
        RestateTurnCancelRaceOutcome::Completed(CompletionWake::Completion { position }) => {
            let wait = waits.get(position).ok_or_else(|| {
                TerminalError::new("a tool completion notice returned an invalid position")
            })?;
            let address = RestateDurableWaitAddress::for_key(&wait.key);
            let resolution = namespace
                .durable_wait_workflow(context, address.workflow_key)
                .peek()
                .call()
                .await?
                .into_body()
                .ok_or_else(|| {
                    TerminalError::new("a tool completion notice has no retained terminal")
                })?;
            RestateTurnCancelRaceOutcome::Completed(ToolCompletionEvent::Resolved {
                position,
                resolution,
            })
        }
        RestateTurnCancelRaceOutcome::Completed(CompletionWake::DispatchReady) => {
            RestateTurnCancelRaceOutcome::Completed(ToolCompletionEvent::DispatchReady)
        }
        RestateTurnCancelRaceOutcome::Completed(CompletionWake::HandedOver) => {
            RestateTurnCancelRaceOutcome::Completed(ToolCompletionEvent::HandedOver)
        }
        RestateTurnCancelRaceOutcome::Completed(CompletionWake::Revoked) => return Err(revoked()),
        RestateTurnCancelRaceOutcome::TurnCancelled => RestateTurnCancelRaceOutcome::TurnCancelled,
        RestateTurnCancelRaceOutcome::ProcessCancelled => {
            RestateTurnCancelRaceOutcome::ProcessCancelled
        }
        RestateTurnCancelRaceOutcome::SessionRevoked { session_id } => {
            RestateTurnCancelRaceOutcome::SessionRevoked { session_id }
        }
    })
}

fn revoked() -> TerminalError {
    TerminalError::new(
        lash_core::RuntimeEffectControllerError::from(
            crate::durable_wait::restate_unknown_or_revoked(),
        )
        .to_record(),
    )
}
