//! An effect-group child's waits raced against its durable cancel fact as a
//! journaled arm (ADR 0105 §4, FIG-3904). The arm is the child's
//! `ChildCancel` notice, subscribed at its group index with an awakeable of
//! the child's own journal (FIG-4344).

use std::time::Duration;

use lash_core::Resolution;
use restate_sdk::errors::TerminalError;
use restate_sdk::serde::Json;

use super::{GateRaceWinner, GateWait, first_of_gate_race, is_engine_cancellation};
use crate::durable_wait::RestateDurableWaitAwaitRequest;
use crate::effect_group::{EffectGroupNotification, EffectGroupSubscribeResponse};

/// The cancel fact a group child's wait races: its group and its position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupChildCancelArm {
    pub group_key: String,
    pub position: usize,
}

/// The two races of a group child's waits against its cancel fact, a part of
/// every controller context. A context the controller never executes a group
/// child on refuses them.
pub trait GroupChildCancelRace<'ctx>: Send + Sync + 'ctx {
    /// A durable timer of an effect-group child, raced against the child's
    /// cancel fact as a journaled arm (FIG-3904): `None` when the cancel won.
    fn sleep_or_group_child_cancel<'run>(
        &'run self,
        namespace: &'run crate::RestateNamespace,
        duration: Duration,
        cancel: GroupChildCancelArm,
    ) -> crate::JournaledFuture<'run, Option<()>>
    where
        'ctx: 'run,
    {
        let _ = (namespace, duration, cancel);
        Box::pin(async {
            Err(TerminalError::new(
                "this context carries no effect-group child cancel race",
            ))
        })
    }

    /// A durable await of an effect-group child, raced against the child's
    /// cancel fact as a journaled arm (FIG-3904): `None` when the cancel won.
    fn await_event_or_group_child_cancel<'run>(
        &'run self,
        namespace: &'run crate::RestateNamespace,
        request: RestateDurableWaitAwaitRequest,
        replay_key: String,
        cancel: GroupChildCancelArm,
    ) -> crate::JournaledFuture<'run, Option<Resolution>>
    where
        'ctx: 'run,
    {
        let _ = (namespace, (request, replay_key), cancel);
        Box::pin(async {
            Err(TerminalError::new(
                "this context carries no effect-group child cancel race",
            ))
        })
    }
}

/// Race one wait of an effect-group child against the child's durable cancel
/// fact (ADR 0105 §4, FIG-3904): `None` when the cancel won.
///
/// Journal order is the deployed contract: the guarded wait's command, then
/// the subscription of the child's `ChildCancel` notice. A notice the index
/// already answers needs no race: a decided cancel or a retirement won, and a
/// seated child's wait finishes on its own terms. Otherwise the VM's
/// first-completed await records which completed first, so a replay takes the
/// branch the first execution took; a guarded wait that won drops its
/// subscriber with a one-way unsubscribe. The engine's own cancellation of the
/// child's invocation, surfacing at this race, is the same decided cancel.
pub(super) async fn race_group_child_cancel<'run, T>(
    subscribed: Result<EffectGroupSubscribeResponse, TerminalError>,
    group_key: &str,
    notification: GateWait<'run, Json<EffectGroupNotification>>,
    guarded: GateWait<'run, T>,
    unsubscribe: impl FnOnce(),
) -> Result<Option<T>, TerminalError> {
    match subscribed {
        Ok(EffectGroupSubscribeResponse::Notified { notification }) => {
            if notification.is_child_cancel() {
                return Ok(None);
            }
        }
        Ok(EffectGroupSubscribeResponse::Refused { outstanding }) => {
            return Err(crate::effect_group::subscription_refused(
                group_key,
                outstanding,
            ));
        }
        Ok(EffectGroupSubscribeResponse::Subscribed) => {
            match first_of_gate_race(&*guarded, &*notification).await {
                Ok(GateRaceWinner::Guarded) => unsubscribe(),
                Ok(GateRaceWinner::Gate) => match notification.await {
                    Ok(Json(notification)) => {
                        if notification.is_child_cancel() {
                            return Ok(None);
                        }
                    }
                    Err(error) if is_engine_cancellation(&error) => return Ok(None),
                    Err(error) => return Err(error),
                },
                Err(error) if is_engine_cancellation(&error) => return Ok(None),
                Err(error) => return Err(error),
            }
        }
        Err(error) if is_engine_cancellation(&error) => return Ok(None),
        Err(error) => return Err(error),
    }
    guarded.await.map(Some)
}

/// The journaled arm on an effect-group child's cancel fact: an awakeable,
/// then its subscription at the group index, awaited. Evaluates to the
/// subscription's answer, the awakeable erased for the race, and the
/// one-way unsubscribe a won guarded wait sends.
macro_rules! group_child_cancel_arm {
    ($ctx:expr, $namespace:expr, $cancel:expr) => {{
        let cancel: GroupChildCancelArm = $cancel;
        let (awakeable_id, awakeable) = restate_sdk::context::ContextAwakeables::awakeable::<
            Json<crate::effect_group::EffectGroupNotification>,
        >($ctx);
        let subscribed = $namespace
            .effect_group_state($ctx, cancel.group_key.clone())
            .subscribe(crate::effect_group::EffectGroupSubscribeRequest {
                notice: crate::effect_group::EffectGroupNotice::ChildCancel {
                    position: cancel.position,
                },
                awakeable_id: awakeable_id.clone(),
            })
            .call()
            .await
            .map(crate::compat::Reply::into_body);
        let group_key = cancel.group_key;
        let unsubscribe_key = group_key.clone();
        let unsubscribe = move || {
            let _unsubscribe = $namespace
                .effect_group_state($ctx, unsubscribe_key)
                .unsubscribe(crate::effect_group::EffectGroupUnsubscribeRequest { awakeable_id })
                .send();
        };
        (
            subscribed,
            group_key,
            erase_gate_wait(awakeable),
            unsubscribe,
        )
    }};
}

/// The two controller-context methods that race a group child's wait against
/// its cancel fact, for every context the controller runs on.
macro_rules! group_child_cancel_methods {
    ($ctx:lifetime) => {
        fn sleep_or_group_child_cancel<'run>(
            &'run self,
            namespace: &'run crate::RestateNamespace,
            duration: Duration,
            cancel: GroupChildCancelArm,
        ) -> crate::JournaledFuture<'run, Option<()>>
        where
            $ctx: 'run,
        {
            Box::pin(async move {
                // `sleep()` journals `sys_sleep` at construction,
                // ahead of the subscription it races.
                let timer =
                    erase_gate_wait(restate_sdk::context::ContextTimers::sleep(self, duration));
                let (subscribed, group_key, notification, unsubscribe) =
                    group_child_cancel_arm!(self, namespace, cancel);
                race_group_child_cancel(subscribed, &group_key, notification, timer, unsubscribe)
                    .await
            })
        }

        fn await_event_or_group_child_cancel<'run>(
            &'run self,
            namespace: &'run crate::RestateNamespace,
            request: RestateDurableWaitAwaitRequest,
            replay_key: String,
            cancel: GroupChildCancelArm,
        ) -> crate::JournaledFuture<'run, Option<Resolution>>
        where
            $ctx: 'run,
        {
            Box::pin(async move {
                // The event wait's CallCommand, then the subscription's.
                let event_address = RestateDurableWaitAddress::for_key(&request.key);
                let event = namespace
                    .durable_wait_workflow(self, event_address.workflow_key)
                    .await_resolution(request.into())
                    .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key);
                let event = erase_gate_wait(event.call());
                let (subscribed, group_key, notification, unsubscribe) =
                    group_child_cancel_arm!(self, namespace, cancel);
                Ok(race_group_child_cancel(
                    subscribed,
                    &group_key,
                    notification,
                    event,
                    unsubscribe,
                )
                .await?
                .map(crate::compat::Reply::into_body))
            })
        }
    };
}
