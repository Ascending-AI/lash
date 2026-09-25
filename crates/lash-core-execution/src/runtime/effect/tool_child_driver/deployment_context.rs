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
//! What a lent context carries and a built one cannot, and what the built path
//! does about each:
//!
//! * **The opener's cooperative cancellation.** A live opener fires the stop
//!   it lends its children when its drive records the turn's cancellation. A
//!   reconstructed child has no running opener to do that, so its context's
//!   stop is fired by a watch of its turn's durable gate instead
//!   ([`watch_turn_stop`]), the way a recorded step body watches it
//!   (FIG-3672 P9): the tool gets the stop as its token, and every durable
//!   wait the child takes observes the gate too. What the child does with the
//!   stop is recorded in its own journal, as on the live path. A tool body
//!   that ignores its token runs to its end, and the group's close then
//!   reaches it through the engine's cancel of the child's invocation.
//! * **The opener's stream.** A reconstructed child's stream events are
//!   recorded instead: they ride its settlement as a bounded
//!   [`RecordedChildStream`], and the opener emits them when it incorporates
//!   the settlement.
//! * **Sources with no recorded form.** Tools a turn's context overlay added,
//!   plugins or a provider a particular open supplied, plugins forked from a
//!   parent, and plugin state are live objects on the opener's session. The
//!   child's request records only that one was present, and a built context
//!   refuses such a child before it runs
//!   ([`UnrecordedSessionSources::rebuild_refusal`](super::super::UnrecordedSessionSources::rebuild_refusal)).
//! * **The turn's session state and draft.** A built context refuses any
//!   session read or change the child makes; see [`SessionServicesRefusal`].
//!
//! A refused child is not run to a result on this path and is not settled.
//! Its attempt ends with a typed
//! [`ToolChildRebuildRefusal`](super::super::ToolChildRebuildRefusal), a live
//! fault the engine retries, and it keeps waiting until a retry finds its
//! opener live.

use std::sync::Arc;

use lash_sansio::sync::MutexExt;

use tokio::sync::{mpsc, oneshot};

use super::super::recorded_stream::{RecordedChildStream, RecordedChildStreamBuilder};
use super::super::tool_child::ToolChildRequest;
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

/// Records the stream events a reconstructed child emits, for its settlement
/// to carry.
///
/// The child's context gets the two senders. A collector task moves every
/// event into a bounded [`RecordedChildStream`] as it arrives, so a busy child
/// never blocks on a full channel. [`finish`](Self::finish) is called once the
/// child's drive has returned: every send the drive made has completed by
/// then, so the collector drains what is still buffered and hands the stream
/// back.
///
/// Order is kept within each channel, not across the two: the collector reads
/// whichever channel has an event. A live opener's two channels make the same
/// promise, since their consumers read them independently.
pub(super) struct ChildStreamRecorder {
    session_tx: mpsc::Sender<crate::SessionStreamEvent>,
    activity_tx: mpsc::Sender<crate::TurnActivity>,
    finish: oneshot::Sender<()>,
    collected: tokio::task::JoinHandle<RecordedChildStream>,
}

impl ChildStreamRecorder {
    pub(super) fn start() -> Self {
        let (session_tx, mut session_rx) = mpsc::channel::<crate::SessionStreamEvent>(64);
        let (activity_tx, mut activity_rx) = mpsc::channel::<crate::TurnActivity>(64);
        let (finish, mut finished) = oneshot::channel::<()>();
        let collected = crate::task::spawn(async move {
            let mut stream = RecordedChildStreamBuilder::default();
            loop {
                tokio::select! {
                    event = session_rx.recv(), if !session_rx.is_closed() => {
                        if let Some(event) = event {
                            stream.push_session(&event);
                        }
                    }
                    activity = activity_rx.recv(), if !activity_rx.is_closed() => {
                        if let Some(activity) = activity {
                            stream.push_activity(&activity);
                        }
                    }
                    _ = &mut finished => break,
                }
            }
            while let Ok(event) = session_rx.try_recv() {
                stream.push_session(&event);
            }
            while let Ok(activity) = activity_rx.try_recv() {
                stream.push_activity(&activity);
            }
            stream.finish()
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
    pub(super) async fn finish(self) -> RecordedChildStream {
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

/// The session services a deployment-built context serves: none that read
/// session state or change the session (FIG-3712).
///
/// On its live opener, a child reads the turn's read model and its graph
/// appends ride the turn's draft, committed with the turn. A context built
/// outside that turn has neither: a read would see a session the turn has
/// moved past, and a write would race the turn's own commit. So every such
/// call fires this latch and then never returns, and the latch aborts the
/// child's drive ([`abandoning`](Self::abandoning)). The driver ends the
/// attempt with
/// [`ToolChildRebuildRefusal::SessionServices`](super::super::ToolChildRebuildRefusal::SessionServices):
/// the call's step is abandoned the way a crash abandons it, so nothing the
/// refused call produced is recorded, and the engine runs the child again, on
/// its live opener when that opener is live.
///
/// Answering the tool an error instead would be recorded as its result, and
/// the live path would have answered differently.
pub(super) struct SessionServicesRefusal {
    abort: futures_util::future::AbortHandle,
    registration: std::sync::Mutex<Option<futures_util::future::AbortRegistration>>,
}

impl Default for SessionServicesRefusal {
    fn default() -> Self {
        let (abort, registration) = futures_util::future::AbortHandle::new_pair();
        Self {
            abort,
            registration: std::sync::Mutex::new(Some(registration)),
        }
    }
}

impl SessionServicesRefusal {
    /// Points `dispatch`'s session services at refusals of this latch. Trace
    /// events still reach the built context's graph service: they are
    /// observations, not session changes.
    pub(super) fn attach(&self, dispatch: &mut ToolDispatchContext<'static>) {
        let services = Arc::new(RefusingSessionServices {
            abort: self.abort.clone(),
            traces: Arc::clone(&dispatch.session_graph),
        });
        dispatch.sessions = services.clone();
        dispatch.session_lifecycle = services.clone();
        dispatch.session_graph = services;
    }

    /// Runs `drive` until it finishes or a session service call fires the
    /// latch, which abandons it where it stands. Called once per latch.
    pub(super) async fn abandoning<F: std::future::Future>(
        &self,
        drive: F,
    ) -> Result<F::Output, super::super::ToolChildRebuildRefusal> {
        let registration = self.registration.lock_recover().take();
        let Some(registration) = registration else {
            return Err(super::super::ToolChildRebuildRefusal::SessionServices);
        };
        futures_util::future::Abortable::new(drive, registration)
            .await
            .map_err(|_| super::super::ToolChildRebuildRefusal::SessionServices)
    }

    /// Whether a session service call fired the latch.
    #[cfg(test)]
    pub(super) fn fired(&self) -> bool {
        self.abort.is_aborted()
    }
}

struct RefusingSessionServices {
    abort: futures_util::future::AbortHandle,
    traces: Arc<dyn crate::plugin::SessionGraphService>,
}

impl RefusingSessionServices {
    async fn refuse<T>(&self) -> T {
        self.abort.abort();
        std::future::pending().await
    }
}

#[async_trait::async_trait]
impl crate::plugin::SessionStateService for RefusingSessionServices {
    async fn turn_scope(
        &self,
        _session_id: &crate::SessionId,
        _turn_id: &crate::TurnId,
    ) -> Result<crate::ExecutionScope, crate::PluginError> {
        self.refuse().await
    }

    async fn snapshot_current(&self) -> Result<crate::SessionSnapshot, crate::PluginError> {
        self.refuse().await
    }

    async fn snapshot_session(
        &self,
        _session_id: &crate::SessionId,
    ) -> Result<crate::SessionSnapshot, crate::PluginError> {
        self.refuse().await
    }

    async fn tool_catalog(
        &self,
        _session_id: &crate::SessionId,
    ) -> Result<Vec<serde_json::Value>, crate::PluginError> {
        self.refuse().await
    }

    async fn session_plugin_init(
        &self,
        _session_id: &crate::SessionId,
    ) -> Result<crate::SessionPluginInit, crate::PluginError> {
        self.refuse().await
    }

    async fn tool_state(
        &self,
        _session_id: &crate::SessionId,
    ) -> Result<crate::ToolState, crate::PluginError> {
        self.refuse().await
    }

    async fn apply_tool_state(
        &self,
        _session_id: &crate::SessionId,
        _snapshot: crate::ToolState,
    ) -> Result<u64, crate::PluginError> {
        self.refuse().await
    }
}

#[async_trait::async_trait]
impl crate::plugin::SessionLifecycleService for RefusingSessionServices {
    async fn create_session(
        &self,
        _request: crate::SessionCreateRequest,
    ) -> Result<crate::SessionHandle, crate::PluginError> {
        self.refuse().await
    }
}

#[async_trait::async_trait]
impl crate::plugin::SessionGraphService for RefusingSessionServices {
    async fn append_session_nodes(
        &self,
        _session_id: &crate::SessionId,
        _request: crate::AppendSessionNodesRequest,
    ) -> Result<crate::AppendSessionNodesOutcome, crate::PluginError> {
        self.refuse().await
    }

    async fn emit_trace_event(
        &self,
        context: lash_trace::TraceContext,
        event: lash_trace::TraceEvent,
    ) -> Result<(), crate::PluginError> {
        self.traces.emit_trace_event(context, event).await
    }

    async fn switch_agent_frame(
        &self,
        _session_id: &crate::SessionId,
        _request: crate::SwitchAgentFrameRequest,
    ) -> Result<crate::OpenAgentFrameResult, crate::PluginError> {
        self.refuse().await
    }
}

/// Fires `stop` when the turn named by `scope` is asked to stop now, watched
/// on its durable gate pair over `host`'s resolver (FIG-3672 P9). The watch
/// runs until the returned guard is dropped. A scope that is not a turn has
/// no gate, and a watch that cannot attach or gives up never fires the stop:
/// the child's durable waits still observe the gate themselves.
pub(super) fn watch_turn_stop(
    host: Arc<dyn crate::EffectHost>,
    scope: crate::ExecutionScope,
    stop: tokio_util::sync::CancellationToken,
) -> Option<TurnStopWatch> {
    let crate::ExecutionScope::Turn {
        session_id,
        turn_id,
    } = scope
    else {
        return None;
    };
    let task = crate::task::spawn(async move {
        let resolver = host.await_event_resolver();
        let control = match crate::runtime::turn_control::ActiveTurnControl::new(
            resolver,
            crate::runtime::TurnAddress::new(session_id, turn_id),
        )
        .await
        {
            Ok(control) => control,
            Err(error) => {
                tracing::debug!(%error, "a rebuilt tool child cannot watch its turn's gate");
                return;
            }
        };
        match control.watch_immediate(resolver).await {
            Ok(Some(_)) => stop.cancel(),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%error, "a rebuilt tool child lost its turn's cancellation watch");
            }
        }
    });
    Some(TurnStopWatch(task))
}

/// Ends a [`watch_turn_stop`] watch when dropped.
pub(super) struct TurnStopWatch(tokio::task::JoinHandle<()>);

impl Drop for TurnStopWatch {
    fn drop(&mut self) {
        self.0.abort();
    }
}
