//! The dispatch context a group tool child builds for itself when its opener
//! is not live in this process (FIG-3712, decision 46).
//!
//! A tool child borrows its opener's live context through the
//! [`LiveOpenerRegistry`](super::LiveOpenerRegistry) whenever it can: that is
//! the fast path, and the only one on which the child's events reach the
//! opener's stream as they happen. But an opener is not always live where its
//! child runs. An engine may suspend the opener while it waits on its children
//! (Restate does, past its inactivity timeout), a child's attempt may land on
//! another worker, and a crashed opener is live nowhere until it is redriven.
//! A child that could run only beside its live opener would wait for an opener
//! that is itself waiting on the child.
//!
//! So the deployment can build the context instead. A
//! [`ToolChildContextSource`] is the deployment's answer to "what does a tool
//! child of this session run under": the plugin session, tool providers and
//! registries, session services, process service, provider and attachment
//! wiring. It builds those from its own wiring and the child's recorded
//! execution environment, the way a process worker builds a process's runtime.
//! The recorded facts are still bound over it by the one rebind site, exactly
//! as they are over a lent context, so the child runs under its own recorded
//! authority on either path.
//!
//! Two things a lent context carries have no deployment-built equivalent:
//!
//! * **The opener's cooperative cancellation.** A reconstructed child's body
//!   token is its own and nothing local cancels it. The opener's turn cancel
//!   reaches the child only through the durable gate its recorded
//!   cancellation authority observes, and the group's close reaches it through
//!   the engine's cancel of the child's invocation.
//! * **The opener's stream.** A reconstructed child's stream events are
//!   recorded instead: they ride its settlement as
//!   [`ChildStreamEvent`]s, and the opener emits them when it incorporates the
//!   settlement. They are late, never dropped.

use std::sync::{Arc, Mutex};

use lash_sansio::sync::MutexExt;
use tokio::sync::{mpsc, oneshot};

use super::super::tool_child::ToolChildRequest;
use super::super::tool_settlement::ChildStreamEvent;
use crate::ScopedEffectController;
use crate::tool_dispatch::ToolDispatchContext;

/// The deployment's builder of a group tool child's dispatch context, for a
/// child whose opener is not live where it runs.
///
/// Installed on the [`ToolChildHost`](super::ToolChildHost) by the embedder
/// that owns session wiring. It must not depend on which engine runs the
/// child: it answers from deployment wiring and the recorded facts it is
/// handed, nothing else.
#[async_trait::async_trait]
pub trait ToolChildContextSource: Send + Sync {
    /// Builds the context `request` runs under.
    ///
    /// `execution_env` is the environment the child was admitted under,
    /// already read from its recorded reference. `lent_controller` fills the
    /// built context's controller slots, as it does for a lent opener context;
    /// the rebind replaces them with the child's own admitted controller.
    ///
    /// The returned context's stream channels are replaced by the driver's
    /// recorder, so whatever the source puts there is never read.
    async fn tool_child_context(
        &self,
        request: &ToolChildRequest,
        execution_env: &crate::ProcessExecutionEnvSpec,
        lent_controller: ScopedEffectController<'static>,
    ) -> Result<DeploymentToolChildContext, crate::PluginError>;
}

/// A dispatch context a [`ToolChildContextSource`] built, and whatever must
/// stay alive for as long as the context is used.
pub struct DeploymentToolChildContext {
    dispatch: ToolDispatchContext<'static>,
    keepalive: Arc<dyn std::any::Any + Send + Sync>,
}

impl DeploymentToolChildContext {
    /// `keepalive` is held until the child finishes: the runtime the source
    /// built the context from, say, whose services the context borrows.
    #[must_use]
    pub fn new(
        dispatch: ToolDispatchContext<'static>,
        keepalive: Arc<dyn std::any::Any + Send + Sync>,
    ) -> Self {
        Self {
            dispatch,
            keepalive,
        }
    }

    pub(super) fn into_parts(
        self,
    ) -> (
        ToolDispatchContext<'static>,
        Arc<dyn std::any::Any + Send + Sync>,
    ) {
        (self.dispatch, self.keepalive)
    }
}

impl std::fmt::Debug for DeploymentToolChildContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeploymentToolChildContext")
            .finish_non_exhaustive()
    }
}

/// Records the stream events a reconstructed child emits, in the order they
/// arrive, for its settlement to carry.
///
/// The child's context gets the two senders. A collector task moves every
/// event into a list as it arrives, so a busy child never blocks on a full
/// channel. [`finish`](Self::finish) is called once the child's drive has
/// returned: every send the drive made has completed by then, so the collector
/// drains what is still buffered and hands the list back.
pub(super) struct ChildStreamRecorder {
    session_tx: mpsc::Sender<crate::SessionStreamEvent>,
    activity_tx: mpsc::Sender<crate::TurnActivity>,
    finish: oneshot::Sender<()>,
    collected: tokio::task::JoinHandle<Vec<ChildStreamEvent>>,
}

impl ChildStreamRecorder {
    pub(super) fn start() -> Self {
        let (session_tx, mut session_rx) = mpsc::channel::<crate::SessionStreamEvent>(64);
        let (activity_tx, mut activity_rx) = mpsc::channel::<crate::TurnActivity>(64);
        let (finish, mut finished) = oneshot::channel::<()>();
        let events = Arc::new(Mutex::new(Vec::new()));
        let collected = crate::task::spawn({
            let events = Arc::clone(&events);
            async move {
                loop {
                    tokio::select! {
                        biased;
                        event = session_rx.recv(), if !session_rx.is_closed() => {
                            if let Some(event) = event {
                                events.lock_recover().push(ChildStreamEvent::Session(event));
                            }
                        }
                        activity = activity_rx.recv(), if !activity_rx.is_closed() => {
                            if let Some(activity) = activity {
                                events.lock_recover().push(ChildStreamEvent::Activity(activity));
                            }
                        }
                        _ = &mut finished => break,
                    }
                }
                while let Ok(event) = session_rx.try_recv() {
                    events.lock_recover().push(ChildStreamEvent::Session(event));
                }
                while let Ok(activity) = activity_rx.try_recv() {
                    events
                        .lock_recover()
                        .push(ChildStreamEvent::Activity(activity));
                }
                std::mem::take(&mut *events.lock_recover())
            }
        });
        Self {
            session_tx,
            activity_tx,
            finish,
            collected,
        }
    }

    /// Points `dispatch`'s stream channels at this recorder.
    pub(super) fn attach(&self, dispatch: &mut ToolDispatchContext<'static>) {
        dispatch.event_tx = self.session_tx.clone();
        dispatch.turn_activity_tx = Some(self.activity_tx.clone());
    }

    /// Every event the child emitted, once its drive has returned.
    pub(super) async fn finish(self) -> Vec<ChildStreamEvent> {
        let Self {
            session_tx,
            activity_tx,
            finish,
            collected,
        } = self;
        drop((session_tx, activity_tx));
        let _ = finish.send(());
        collected.await.unwrap_or_default()
    }
}
