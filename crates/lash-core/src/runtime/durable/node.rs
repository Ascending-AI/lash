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
//!   on this node ([`Backend::commit_mail`]) or of a producer commit of the
//!   store set's (an accepted send: its
//!   [`StoreSet::wake_sink`](lash_execution_contracts::backend::StoreSet::wake_sink)), published
//!   to the owner's node when the store set has node wakes and the notifier
//!   is `AfterCommit`, or by
//!   the owner's own read at every claim poll: the poll is the correctness
//!   backstop, the hint only cuts latency. With node wakes the node also holds
//!   a liveness lock, so a crashed node is reaped as soon as its session
//!   ends.
//! - **Idle eviction and release** belong to each activation: a session
//!   stays hot for `idle_evict` with nothing to do, and releases as
//!   `waiting` with the earliest due time its sources noted.
//! - **Formats:** the node decodes the backend's format sets, so it claims
//!   only actors whose state this build decodes (ADR 0106 §1).
//! - **Retired formats:** a build that retires the format sets of the build
//!   before it does not start while an actor is still in one: serving, it
//!   would strand it (ADR 0115 §3.5). It refuses with
//!   [`DurableError::Unmigrated`], naming how many and the command that
//!   carries them forward.
//! - **Packages** (FIG-6918): the node installs the package code its
//!   backend holds, keeps each retained artifact a dependent still lists,
//!   and advertises what it holds, so it claims only actors whose package
//!   set it holds. It does not start while a dependent lists an identity no
//!   artifact it holds declares ([`DurableError::RetiredDependents`]).
//! - **Drain:** once the host starts [`NodeServe::drain`], the node claims
//!   nothing more, each activation releases its actor `ready` at its next
//!   committed phase, and `serve` returns [`Stopped::Drained`] when none is
//!   left.
//! - **Liveness** (FIG-6310): a session whose turn has not ended is owned
//!   by a live boot, claimable (ready, or waiting on mail or a due time a
//!   reap, a wake or a timer readies), or parked. A park another build may
//!   clear ([`SessionParkReason::REDRIVEN_AT_BOOT`]) holds only until the
//!   next boot of any node, which redrives it before its first claim, so a
//!   turn wedged by a deterministic failure a deploy fixes resumes on its
//!   own. A park that may have taken nodes down with it
//!   (`ActivationLoop`, `CancelLoop`) waits for an operator's redrive.

use std::future::Future;
use std::sync::Arc;

use lash_durable::domain::SessionParksRedrive;
use lash_durable::runner::{Activation, Drain, Runner, RunnerConfig, Stopped};
use lash_durable::{
    ActorDispatch, CommitLabel, DurableError, MailAnswer, MailDomainWrite, MailTx, NodeId, Notifier,
};

use super::session::{SessionActivation, SessionParkReason};
use lash_execution_contracts::backend::Backend;

/// What one node serves.
pub struct NodeServe {
    /// This node's registered boot for host inspection.
    pub registration: lash_durable::runner::NodeRegistration,
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
    /// The claim requirements the node's process components serve
    /// ([`ProcessComponentRegistry::serves`](crate::plugin::ProcessComponentRegistry::serves)):
    /// it runs a process only when it serves every requirement the process
    /// carries (FIG-6034 D3).
    pub serves: Vec<String>,
}

/// Serve `backend` as one node until `stop` completes or the node loses its
/// lease, under the backend's validated substrate parameters.
///
/// # Errors
///
/// [`DurableError::Unmigrated`] when an actor is still in a format set the
/// backend's build retires, [`DurableError::RetiredDependents`] when a
/// dependent lists an identity no artifact the node holds declares,
/// and the store's refusal of the node's registration.
pub async fn serve(
    backend: &Backend,
    serve: NodeServe,
    stop: impl Future<Output = ()> + Send,
) -> Result<Stopped, DurableError> {
    let settings = backend.config().settings();
    // The node decodes the backend's format sets: every one when it runs
    // processes, the session's alone when it runs none.
    let formats = backend.formats();
    let unmigrated = unmigrated_actors(backend).await?;
    if unmigrated > 0 {
        return Err(DurableError::Unmigrated {
            unmigrated,
            command: formats.sweep().unwrap_or_default().to_owned(),
        });
    }
    redrive_session_parks(backend).await;
    let mut decodes = formats.session_decodes();
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
            serves: serve.serves,
            lease: backend.config().lease(),
            max_active: settings.max_active,
            claim_batch: settings.claim_batch,
            retirement: settings.retirement,
        },
        activation,
    )
    .with_hints(backend.hints().clone())
    .with_drain(serve.drain)
    .with_registration(serve.registration);
    if settings.notifier == Notifier::AfterCommit
        && let Some(node_wakes) = backend.stores().node_wakes()
    {
        runner = runner.with_node_wakes(node_wakes);
    }
    // While the node serves, what its store set's producers commit wakes
    // its actors through its hints.
    let _wakes = backend.stores().wake_sink().attach(backend.hints());
    runner.run(stop).await
}

/// Redrive every session parked for a reason another build may clear
/// ([`SessionParkReason::REDRIVEN_AT_BOOT`]), as an operator's redrive
/// would, before this boot claims anything (FIG-6310): its next claim runs
/// the unfinished turn again from its committed state, and a session that
/// still fails parks again at its budget. A refused redrive leaves the parks
/// to the next boot or an operator; the node serves regardless.
async fn redrive_session_parks(backend: &Backend) {
    let mut tx = MailTx::new();
    tx.write(MailDomainWrite::RedriveSessionParks(SessionParksRedrive {
        reasons: SessionParkReason::REDRIVEN_AT_BOOT
            .map(str::to_owned)
            .to_vec(),
    }));
    match backend
        .durable()
        .commit_mail(tx, CommitLabel::MAIL_SESSION)
        .await
    {
        Ok(commit) => {
            for answer in commit.answers {
                if let MailAnswer::RedriveSessionParks(redriven) = answer {
                    for actor in redriven {
                        tracing::info!(%actor, "the boot redrove a parked session");
                    }
                }
            }
        }
        Err(error) => tracing::warn!(
            %error,
            "the boot's redrive of parked sessions failed; they stay parked"
        ),
    }
}

/// How many actors are still in a format set `backend`'s build retires
/// (ADR 0115 §3.5): none when it retires none.
///
/// # Errors
///
/// The store's.
pub async fn unmigrated_actors(backend: &Backend) -> Result<u64, DurableError> {
    const PAGE: usize = 256;
    let mut unmigrated = 0_u64;
    for set in backend.formats().retired() {
        let mut after = None;
        loop {
            let page = backend
                .durable()
                .actors_in(set, after.as_ref(), PAGE)
                .await?;
            unmigrated = unmigrated.saturating_add(page.len() as u64);
            if page.len() < PAGE {
                break;
            }
            after = page.last().cloned();
        }
    }
    Ok(unmigrated)
}
