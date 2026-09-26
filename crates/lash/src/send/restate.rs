//! The Restate host's durable submit-and-wait binding (FIG-3837, D5).
//!
//! A host handler never runs a turn: the session's engine drives every root.
//! The handler journals what it needs to wait durably:
//!
//! 1. **A stable input id.** [`SendBuilder::accept_restate`] journals the
//!    host's id (or a fresh one) before it accepts anything, so every replay
//!    of the handler submits under the same id, and the acceptance itself is
//!    one journaled step. A replay that re-runs the acceptance (the handler
//!    died before its journal entry committed) resubmits the same id and the
//!    same content, which the store answers with the original acceptance.
//! 2. **A wait made of bounded probes.** [`SendHandle::outcome_restate`]
//!    follows the input's root one probe window at a time. Each probe is a
//!    journaled step that answers the outcome, or where the follower stands
//!    (its replay cursor and the gaps it met). A replayed handler reads every
//!    finished probe back and follows on from the last position, so the wait
//!    survives suspension, replay, a restart after acceptance, and a turn that
//!    outlives the invocation's inactivity and abort timers: no single probe
//!    runs longer than its window, and nothing of the wait lives in process
//!    memory between probes.
//!
//! Retryable refusals stay retryable: a step whose error
//! [`is_retryable`](crate::EmbedError::is_retryable) ends the attempt without
//! a journal entry, and the invocation's retry runs it again. Other errors
//! are journaled and end the handler terminally.
//!
//! ## The exclusive-object dependency cycle
//!
//! A turn may call its host's own virtual object. A host that waited for that
//! turn inside the object's exclusive handler would hold the object's lock
//! while the turn queues behind it, and neither would ever finish: suspension
//! and replay do not release an exclusive handler's lock. So the wait is only
//! offered to contexts that hold no exclusive lock ([`RestateWaitContext`]:
//! services, workflows, shared object and shared workflow handlers). An
//! exclusive handler accepts, returns the receipt, and a shared handler (or
//! the caller) waits.

use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use lash_core::SessionCursor;
use lash_core::runtime::TurnInputAcceptanceReceipt;
use lash_restate::RestateControllerContext;
use lash_restate::restate_sdk::context::{
    Context, SharedObjectContext, SharedWorkflowContext, WorkflowContext,
};
use lash_restate::restate_sdk::errors::{HandlerResult, TerminalError};
use lash_restate::restate_sdk::serde::Json;

use super::follow::{self, Followed, Position, Subject, Tap};
use super::{HandleShared, RootHandle, SendBuilder, SendHandle, SendOutcome, SendTarget};
use crate::support::TurnActivitySink;

mod sealed {
    pub trait WaitContext {}
}

/// A Restate handler context that holds no exclusive object lock, and so may
/// wait for a root (see the module docs on the dependency cycle).
pub trait RestateWaitContext<'ctx>: sealed::WaitContext + RestateControllerContext<'ctx> {}

macro_rules! wait_context {
    ($($context:ident),* $(,)?) => {$(
        impl sealed::WaitContext for $context<'_> {}
        impl<'ctx> RestateWaitContext<'ctx> for $context<'ctx> {}
    )*};
}
wait_context!(
    Context,
    SharedObjectContext,
    WorkflowContext,
    SharedWorkflowContext
);

/// The default probe window: well inside Restate's default inactivity
/// timeout, so a waiting handler journals progress long before the server
/// asks it to suspend.
const PROBE_WINDOW: Duration = Duration::from_secs(10);

/// How a Restate handler waits for an accepted input's root.
pub struct RestateWait<'a> {
    sink: Option<&'a dyn TurnActivitySink>,
    probe_window: Duration,
}

impl Default for RestateWait<'_> {
    fn default() -> Self {
        Self {
            sink: None,
            probe_window: PROBE_WINDOW,
        }
    }
}

impl<'a> RestateWait<'a> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forward the root's live activity to `sink` while waiting. Activity a
    /// replayed probe already forwarded is not forwarded again, and the
    /// journaled outcome carries no activity list: the sink has it.
    pub fn sink(mut self, sink: &'a dyn TurnActivitySink) -> Self {
        self.sink = Some(sink);
        self
    }

    /// The longest one journaled probe follows the root before it records
    /// where it stands. Keep it below the deployment's inactivity timeout.
    pub fn probe_window(mut self, window: Duration) -> Self {
        self.probe_window = window;
        self
    }
}

/// One step, journaled on the host's handler: a retryable error ends the
/// attempt unjournaled, any other is journaled and ends the handler.
fn journal_host<'ctx: 'a, 'a, C, T>(
    ctx: &'a C,
    name: &str,
    future: BoxFuture<'a, crate::Result<T>>,
) -> BoxFuture<'a, HandlerResult<T>>
where
    C: RestateControllerContext<'ctx> + ?Sized,
    T: serde::Serialize + serde::de::DeserializeOwned + Send + 'static,
{
    let step = ctx.run_json_or_retry_send(name.to_owned(), async move {
        match future.await {
            Ok(value) => Ok(Ok(value)),
            Err(error) if error.is_retryable() => Err(error.to_string()),
            Err(error) => Ok(Err(error.to_string())),
        }
    });
    Box::pin(async move {
        let Json(result) = step.await?;
        result.map_err(|message| TerminalError::new(message).into())
    })
}

/// The target a journaled step reads through: no resident runtime is needed
/// to follow a root from the store.
fn durable_target(target: &SendTarget) -> SendTarget {
    match target {
        SendTarget::Live(session) => SendTarget::Durable(session.durable()),
        SendTarget::Durable(session) => SendTarget::Durable(session.clone()),
    }
}

impl SendBuilder {
    /// Accept this input on a Restate handler's journal.
    ///
    /// The id is journaled first, so a replayed handler submits under the
    /// same id; the acceptance is one journaled step answering the receipt
    /// and the replay cursor the handle follows from. Any Restate handler may
    /// accept, an exclusive object handler included: it then returns the
    /// receipt rather than wait (see the module docs).
    pub fn accept_restate<'ctx: 'a, 'a, C>(
        mut self,
        ctx: &'a C,
    ) -> BoxFuture<'a, HandlerResult<SendHandle>>
    where
        C: RestateControllerContext<'ctx>,
    {
        Box::pin(async move {
            let requested = self.id.take().or_else(|| self.input.trace_turn_id.take());
            let Json(id) = ctx
                .run_json_send("lash.host.input-id".to_owned(), None, async move {
                    requested.unwrap_or_else(crate::turn::fresh_turn_id)
                })
                .await?;
            self.id = Some(id.clone());
            let target = durable_target(&self.target);
            let (receipt, cursor) = journal_host(
                ctx,
                "lash.host.accept",
                Box::pin(async move {
                    let handle = self.await?;
                    Ok::<(TurnInputAcceptanceReceipt, SessionCursor), _>((
                        handle.receipt.clone(),
                        handle.cursor.clone(),
                    ))
                }),
            )
            .await?;
            Ok(SendHandle {
                target,
                receipt,
                id: Some(id),
                cursor,
                shared: Arc::new(HandleShared::pending(None)),
            })
        })
    }
}

/// What one probe answered.
#[derive(serde::Serialize, serde::Deserialize)]
enum Probe {
    Answered(Box<SendOutcome>),
    Pending(Position),
}

impl SendHandle {
    /// Wait for this input's root on a Restate handler's journal, in
    /// journaled probes of at most the wait's window (see the module docs).
    /// No turn runs in this handler.
    ///
    /// A cursor taken in another process (the acceptance ran before a
    /// restart) is not this process's replay: the follower reports the gap
    /// and observes on from the replay's head.
    pub fn outcome_restate<'ctx: 'a, 'a, C>(
        self,
        ctx: &'a C,
        wait: RestateWait<'a>,
    ) -> BoxFuture<'a, HandlerResult<SendOutcome>>
    where
        C: RestateWaitContext<'ctx>,
    {
        wait_restate(
            ctx,
            durable_target(&self.target),
            Subject::Input(self.receipt.clone()),
            self.cursor.clone(),
            wait,
        )
    }
}

impl RootHandle {
    /// Wait for this root on a Restate handler's journal, as
    /// [`SendHandle::outcome_restate`] waits for an input's.
    pub fn outcome_restate<'ctx: 'a, 'a, C>(
        self,
        ctx: &'a C,
        wait: RestateWait<'a>,
    ) -> BoxFuture<'a, HandlerResult<SendOutcome>>
    where
        C: RestateWaitContext<'ctx>,
    {
        wait_restate(
            ctx,
            durable_target(&self.target),
            Subject::Root(self.root.clone()),
            self.cursor.clone(),
            wait,
        )
    }
}

/// Follow `subject` in journaled probes until it answers.
fn wait_restate<'ctx: 'a, 'a, C>(
    ctx: &'a C,
    target: SendTarget,
    subject: Subject,
    cursor: SessionCursor,
    wait: RestateWait<'a>,
) -> BoxFuture<'a, HandlerResult<SendOutcome>>
where
    C: RestateWaitContext<'ctx>,
{
    Box::pin(async move {
        let RestateWait { sink, probe_window } = wait;
        let mut position = Position::at(cursor);
        loop {
            let target = target.clone();
            let subject = subject.clone();
            let from = position;
            let probe = journal_host(
                ctx,
                "lash.host.outcome",
                Box::pin(async move {
                    let context = target.context().await?;
                    let mut tap = match sink {
                        Some(sink) => Tap::Sink(sink),
                        None => Tap::Quiet,
                    };
                    let followed =
                        follow::follow(&context, &subject, from, &mut tap, Some(probe_window))
                            .await?;
                    Ok(match followed {
                        Followed::Answered(mut outcome) => {
                            // The journal keeps the report, never the
                            // activity list: it grows with the turn.
                            if let Some(output) = outcome.output.as_mut() {
                                output.activities.clear();
                            }
                            Probe::Answered(outcome)
                        }
                        Followed::Pending(position) => Probe::Pending(position),
                    })
                }),
            )
            .await?;
            match probe {
                Probe::Answered(outcome) => return Ok(*outcome),
                Probe::Pending(next) => position = next,
            }
        }
    })
}
