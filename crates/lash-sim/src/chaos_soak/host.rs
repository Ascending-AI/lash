//! The host's own handler work in a soak: process starts and session
//! deletions, each run inside a handler of the host's service on the server
//! double, as a deployment's endpoints run them.
//!
//! A soak world replays these handlers when the deployment dies
//! ([`CrashWorld::replay_host_handlers`]), as Restate retries a host
//! service's invocation on the next deployment: every attempt takes the core
//! of whichever deployment is up, so a replay runs on the live one and
//! journals the same commands as the attempt that died.

use std::sync::Arc;

use lash_core::{ProcessId, SessionId};

use crate::crash_matrix::world::CrashWorld;

/// What a host handler answered: its result, or `None` when the host died
/// inside it (the handler then replays on the next deployment, unobserved).
pub type Answer<T> = Option<Result<T, String>>;

/// Start `request` under the runtime operation `operation`. The start's
/// idempotency is the operation's: a replay, or a host's retry under the same
/// operation, answers the same process.
pub async fn start_process(
    world: &CrashWorld,
    request: lash_core::ProcessStartRequest,
    operation: &str,
) -> Answer<ProcessId> {
    let live = world.live_core();
    let restate = world.engine().clone();
    let operation = operation.to_owned();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let ran = world
        .host_op(async move {
            restate
                .run_in_handler(
                    lash_core::AdmittedScope::runtime_operation(operation),
                    Arc::new(move |scoped| {
                        let live = live.clone();
                        let request = request.clone();
                        let tx = tx.clone();
                        Box::pin(async move {
                            let started = match live.get().await {
                                Ok(core) => core
                                    .processes()
                                    .start(request, scoped)
                                    .await
                                    .map(|receipt| receipt.process_id)
                                    .map_err(|error| error.to_string()),
                                Err(error) => Err(error),
                            };
                            let _ = tx.send(started);
                        })
                    }),
                )
                .await
        })
        .await;
    answer(ran, rx.try_recv().ok())
}

/// A deletion's handler execution: the live deployment's administration over
/// the handler's own controller.
struct HandlerExecution<'a> {
    admin: lash_core::SessionAdministration,
    scoped: lash_core::ScopedEffectController<'a>,
}

impl lash_core::SessionDeleteExecution for HandlerExecution<'_> {
    fn administration(&self) -> &lash_core::SessionAdministration {
        &self.admin
    }

    fn scoped<'run>(
        &'run self,
        _: lash_core::AdmittedScope,
    ) -> Result<lash_core::ScopedEffectController<'run>, lash_core::RuntimeError> {
        Ok(self.scoped.clone())
    }
}

/// Delete `session`.
pub async fn delete_session(world: &CrashWorld, session: &SessionId) -> Answer<()> {
    let live = world.live_core();
    let restate = world.engine().clone();
    let session = session.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Result<(), String>>();
    let ran = world
        .host_op(async move {
            let id = session.clone();
            restate
                .run_in_handler(
                    lash_core::AdmittedScope::session_delete(&session),
                    Arc::new(move |scoped| {
                        let live = live.clone();
                        let id = id.clone();
                        let tx = tx.clone();
                        Box::pin(async move {
                            let core = match live.get().await {
                                Ok(core) => core,
                                Err(error) => {
                                    let _ = tx.send(Err(error));
                                    return;
                                }
                            };
                            let execution = HandlerExecution {
                                admin: core.session_administration().await,
                                scoped,
                            };
                            let outcome = match lash_core::SessionDeleteContext::from_execution(
                                &execution,
                                id.as_str(),
                            ) {
                                Ok(context) => lash::LashCore::delete_session(context)
                                    .await
                                    .map(|_| ())
                                    .map_err(|error| error.to_string()),
                                Err(error) => Err(error.to_string()),
                            };
                            let _ = tx.send(outcome);
                        })
                    }),
                )
                .await
        })
        .await;
    answer(ran, rx.try_recv().ok())
}

/// The handler's answer: what its last attempt sent, unless the host died
/// before it answered.
fn answer<T>(ran: Option<Result<(), String>>, sent: Option<Result<T, String>>) -> Answer<T> {
    match (ran, sent) {
        (None, _) => None,
        (Some(_), Some(sent)) => Some(sent),
        (Some(Ok(())), None) => Some(Err("the handler ran without answering".to_owned())),
        (Some(Err(error)), None) => Some(Err(format!("the handler failed: {error}"))),
    }
}
