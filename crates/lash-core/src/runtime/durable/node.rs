//! The node runtime: one serving lash process on the durable substrate
//! (ADR 0132 §3). Owned by L3 (FIG-5172).
//!
//! [`serve`] runs the production runner over a backend's durable store:
//! it keeps the node's lease (heartbeat, and self-stop when renewals fail),
//! reaps dead nodes, claims ready and due actors, and runs each claimed
//! actor's activation by its kind. Tests run the same runner through
//! `lash_durable_test::SimNodes`.
//!
//! - **Claims** take at most `claim_batch` actors at once and never more
//!   than `max_active` in all.
//! - **Mail** reaches a hot owner by the wake hint of a mailbox commit made
//!   on this node ([`Backend::commit_mail`]), published to the owner's node
//!   when the store set has signals and the notifier is `AfterCommit`, or by
//!   the owner's own read at every claim poll: the poll is the correctness
//!   backstop, the hint only cuts latency. With signals the node also holds
//!   a liveness lock, so a crashed node is reaped as soon as its session
//!   ends.
//! - **Idle eviction and release** belong to each activation: a session
//!   stays hot for `idle_evict` with nothing to do, and releases as
//!   `waiting` with the earliest due time its sources noted.
//! - **Formats:** the node decodes the backend's format sets, so it claims
//!   only actors whose state this build decodes (ADR 0106 §1).
//! - **Drain:** once the host starts [`NodeServe::drain`], the node claims
//!   nothing more, each activation releases its actor `ready` at its next
//!   committed phase, and `serve` returns [`Stopped::Drained`] when none is
//!   left.

use std::future::Future;
use std::sync::Arc;

use lash_durable::runner::{Activation, Drain, Runner, RunnerConfig, Stopped};
use lash_durable::{ActorDispatch, DurableError, NodeId, Notifier};

use super::session::SessionActivation;
use crate::Backend;

/// What one node serves.
pub struct NodeServe {
    /// The node's stable name: a new boot of the same name fences the old.
    pub node: NodeId,
    /// The node's drain switch: the host starts it to drain the node by
    /// release.
    pub drain: Drain,
    /// Runs the claimed sessions.
    pub sessions: Arc<SessionActivation>,
    /// Runs the claimed processes. A node given none claims no process: it
    /// decodes only the session actor's formats.
    pub processes: Option<Arc<dyn Activation>>,
}

/// Serve `backend` as one node until `stop` completes or the node loses its
/// lease, under the backend's validated substrate parameters.
///
/// # Errors
///
/// The store's refusal of the node's registration.
pub async fn serve(
    backend: &Backend,
    serve: NodeServe,
    stop: impl Future<Output = ()> + Send,
) -> Result<Stopped, DurableError> {
    let settings = backend.config().settings();
    // The node decodes the backend's format sets: every one when it runs
    // processes, the session's alone when it runs none.
    let formats = backend.formats();
    let mut decodes = vec![formats.session().clone()];
    let activation: Arc<dyn Activation> = match serve.processes {
        Some(process) => {
            decodes = formats.decodes();
            Arc::new(ActorDispatch {
                session: serve.sessions,
                process,
            })
        }
        None => serve.sessions,
    };
    let mut runner = Runner::new(
        Arc::clone(backend.durable()),
        backend.clock(),
        RunnerConfig {
            node: serve.node,
            decodes,
            lease: backend.config().lease(),
            max_active: settings.max_active,
            claim_batch: settings.claim_batch,
        },
        activation,
    )
    .with_hints(backend.hints().clone())
    .with_drain(serve.drain);
    if settings.notifier == Notifier::AfterCommit
        && let Some(signals) = backend.stores().durable_signals()
    {
        runner = runner.with_signals(signals);
    }
    runner.run(stop).await
}
