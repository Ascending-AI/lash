//! Deferred dispatches arm durable deadlines and wait through removable observers.

use super::*;
use crate::durable_wait::{RestateDurableWaitAwakeableRequest, RestateDurableWaitRegistration};
use lash_core::{ToolCompletionEvent, ToolCompletionWait, ToolDispatchCursor};

pub(super) async fn arm<'ctx, C>(
    context: &C,
    namespace: &crate::RestateNamespace,
    key: lash_core::AwaitEventKey,
    deadline_ms: Option<u64>,
    now_ms: u64,
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
    if let Some(deadline_ms) = deadline_ms {
        registry
            .resolve(RestateDurableWaitResolveRequest {
                key,
                resolution: Resolution::Timeout,
            })
            .send_after(Duration::from_millis(deadline_ms.saturating_sub(now_ms)))
            .await?;
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
    let mut events: Vec<GateWait<'run, CompletionWake>> = Vec::new();
    let mut observers = Vec::new();
    for (position, wait) in waits.iter().enumerate() {
        let (awakeable_id, awakeable) = (awakeables.completion)(position);
        let entry = RestateDurableWaitAwakeableRequest {
            key: wait.key.clone(),
            awakeable_id,
            hand_over: None,
        };
        let address = RestateDurableWaitAddress::for_key(&entry.key);
        let registered = namespace
            .durable_wait_registry(context, durable_wait_index_object_key(&address))
            .register_awakeable(entry.clone())
            .call()
            .await?
            .into_body();
        if matches!(registered, RestateDurableWaitRegistration::Revoked) {
            return Err(revoked());
        }
        observers.push(entry);
        events.push(awakeable);
    }
    let mut subscription = None;
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
    let outcome = match turn_cancel {
        Some(turn_cancel) => {
            let session_id =
                turn_cancel.key.scope.session_id().cloned().ok_or_else(|| {
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
        None => {
            let inner = events
                .first()
                .ok_or_else(|| TerminalError::new("a tool completion race has no events"))?
                .inner_context();
            let mut handles = events.iter().map(|wait| wait.handle()).collect::<Vec<_>>();
            if let Some(promise) = &process_cancel {
                handles.push(promise.handle());
            }
            let winner = inner.select(handles).await?;
            let winner = if winner == events.len() {
                let payload = process_cancel
                    .ok_or_else(|| {
                        TerminalError::new("a tool completion race returned an invalid branch")
                    })?
                    .await?;
                if crate::process::process_cancel_promise_verdict(Some(payload)) {
                    None
                } else {
                    Some(
                        inner
                            .select(events.iter().map(|wait| wait.handle()).collect())
                            .await?,
                    )
                }
            } else {
                Some(winner)
            };
            match winner {
                None => Ok(RestateTurnCancelRaceOutcome::ProcessCancelled),
                Some(winner) if winner < events.len() => Ok(
                    RestateTurnCancelRaceOutcome::Completed(events.remove(winner).await?),
                ),
                Some(_) => Err(TerminalError::new(
                    "the tool completion race returned an invalid branch",
                )),
            }
        }
    };
    // A handover retires only these observers. The original keys and native delayed sends remain armed.
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
