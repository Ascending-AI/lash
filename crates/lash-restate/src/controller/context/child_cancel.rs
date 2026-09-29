//! An effect-group child's waits raced against its durable cancel fact as a
//! journaled arm (ADR 0105 §4, FIG-3904).

use std::time::Duration;

use lash_core::Resolution;
use restate_sdk::errors::TerminalError;

use super::{GateRaceWinner, GateWait, first_of_gate_race, is_engine_cancellation};
use crate::durable_wait::RestateDurableWaitAwaitRequest;

/// The two races of a group child's waits against its cancel fact, a part of
/// every controller context. A context the controller never drives a group
/// child on refuses them.
pub trait GroupChildCancelRace<'ctx>: Send + Sync + 'ctx {
    /// A durable timer of an effect-group child, raced against the child's
    /// cancel fact as a journaled arm (FIG-3904): `None` when the cancel won.
    fn sleep_or_group_child_cancel<'run>(
        &'run self,
        namespace: &'run crate::RestateNamespace,
        duration: Duration,
        cancel: RestateDurableWaitAwaitRequest,
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
        cancel: RestateDurableWaitAwaitRequest,
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
/// `cancel` is a call on the child's cancel wait. Journal order is the
/// deployed contract: the guarded wait's command, then the cancel call. The
/// VM's first-completed await records which completed first, so a replay
/// takes the branch the first execution took. A cancel wait that ends
/// `Settled` is no cancel: the guarded wait finishes on its own terms. The
/// engine's own cancellation of the child's invocation, surfacing at this
/// race, is the same decided cancel.
pub(super) async fn race_group_child_cancel<'run, T>(
    cancel: GateWait<'run, crate::compat::Reply<Resolution>>,
    guarded: GateWait<'run, T>,
) -> Result<Option<T>, TerminalError> {
    match first_of_gate_race(&*guarded, &*cancel).await {
        Ok(GateRaceWinner::Guarded) => {}
        Ok(GateRaceWinner::Gate) => match cancel.await {
            Ok(reply) => {
                if crate::effect_group::group_child_cancel_verdict(reply.into_body()) {
                    return Ok(None);
                }
            }
            Err(error) if is_engine_cancellation(&error) => return Ok(None),
            Err(error) => return Err(error),
        },
        Err(error) if is_engine_cancellation(&error) => return Ok(None),
        Err(error) => return Err(error),
    }
    guarded.await.map(Some)
}

/// The journaled arm on an effect-group child's cancel wait, erased for the
/// race (FIG-3904).
macro_rules! group_child_cancel_call {
    ($ctx:expr, $namespace:expr, $cancel:expr) => {{
        let cancel: RestateDurableWaitAwaitRequest = $cancel;
        let address = RestateDurableWaitAddress::for_key(&cancel.key);
        let replay_key = cancel.key.key_id.clone();
        erase_gate_wait(
            $namespace
                .durable_wait_workflow($ctx, address.workflow_key)
                .await_resolution(cancel.into())
                .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key)
                .call(),
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
            cancel: RestateDurableWaitAwaitRequest,
        ) -> crate::JournaledFuture<'run, Option<()>>
        where
            $ctx: 'run,
        {
            Box::pin(async move {
                // `sleep()` journals `sys_sleep` at construction,
                // ahead of the cancel call it races.
                let timer =
                    erase_gate_wait(restate_sdk::context::ContextTimers::sleep(self, duration));
                let cancel = group_child_cancel_call!(self, namespace, cancel);
                race_group_child_cancel(cancel, timer).await
            })
        }

        fn await_event_or_group_child_cancel<'run>(
            &'run self,
            namespace: &'run crate::RestateNamespace,
            request: RestateDurableWaitAwaitRequest,
            replay_key: String,
            cancel: RestateDurableWaitAwaitRequest,
        ) -> crate::JournaledFuture<'run, Option<Resolution>>
        where
            $ctx: 'run,
        {
            Box::pin(async move {
                // The event wait's CallCommand, then the cancel's.
                let event_address = RestateDurableWaitAddress::for_key(&request.key);
                let event = namespace
                    .durable_wait_workflow(self, event_address.workflow_key)
                    .await_resolution(request.into())
                    .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key);
                let event = erase_gate_wait(event.call());
                let cancel = group_child_cancel_call!(self, namespace, cancel);
                Ok(race_group_child_cancel(cancel, event)
                    .await?
                    .map(crate::compat::Reply::into_body))
            })
        }
    };
}
