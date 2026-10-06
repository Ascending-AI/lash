//! A node's runner: the one loop that keeps a node's lease, reaps dead
//! nodes, claims actors and runs each claimed actor's activation.
//!
//! The runner owns every activation it starts. When the node stops serving,
//! for any reason, the runner returns and every activation is dropped with
//! it: a node that lost its lease keeps nothing, and a reaped node's actors
//! are already fenced by their bumped epochs.
//!
//! All of its time is the injected [`Clock`]'s, so a simulated deployment
//! runs the same loop on virtual time.

use crate::config::LeaseConfig;
use crate::error::DurableError;
use crate::ids::{ActorKey, CommitLabel, Epoch, FormatSet, NodeId};
use crate::port::{
    ActorCommit, ClaimCause, Claimed, DurableStore, HeartbeatOutcome, NodeSpec, Woken,
};
use crate::tx::ActorTx;
use lash_core_ids::clock::Clock;
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use tokio::sync::Notify;
use tokio::task::JoinSet;

/// What a runner runs as.
#[derive(Clone, Debug)]
pub struct RunnerConfig {
    /// The node's stable name.
    pub node: NodeId,
    /// The format sets this build decodes.
    pub decodes: Vec<FormatSet>,
    /// The node-lease timings.
    pub lease: LeaseConfig,
    /// How many actors the node runs at once; claims never take more.
    pub max_active: usize,
    /// The most actors one claim takes.
    pub claim_batch: usize,
}

/// Why a runner stopped serving.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stopped {
    /// The host asked it to stop; its actors were released.
    Requested,
    /// Its lease is gone: another node reaped it, or a newer boot of the
    /// same node replaced it.
    LeaseLost,
    /// No heartbeat succeeded for `self_stop_after`, so it stopped before
    /// anyone may reap it.
    Unrenewed,
}

/// What a node does with one actor it claimed.
#[async_trait::async_trait]
pub trait Activation: Send + Sync + 'static {
    /// Run `owned` until the activation releases the actor, ends it, or
    /// loses it. A commit refused with [`DurableError::OwnershipLost`] means
    /// the actor is someone else's: return at once, keeping nothing.
    ///
    /// The runner drops this future, mid-await, when the node stops serving.
    async fn activate(&self, owned: Owned);
}

/// One claimed actor, as its activation holds it.
pub struct Owned {
    store: Arc<dyn DurableStore>,
    clock: Arc<dyn Clock>,
    claimed: Claimed,
    hint: Arc<Notify>,
    poll: std::time::Duration,
}

impl Owned {
    /// The actor.
    #[must_use]
    pub fn actor(&self) -> &ActorKey {
        &self.claimed.actor
    }

    /// The epoch the node owns it under.
    #[must_use]
    pub fn epoch(&self) -> Epoch {
        self.claimed.epoch
    }

    /// Why it was claimable.
    #[must_use]
    pub fn cause(&self) -> ClaimCause {
        self.claimed.cause
    }

    /// Open an owner transaction over the actor at the owned epoch.
    ///
    /// # Errors
    ///
    /// [`DurableError::OwnershipLost`] once the actor is someone else's.
    pub async fn begin(&self) -> Result<ActorTx, DurableError> {
        self.store
            .begin(&self.claimed.actor, self.claimed.epoch)
            .await
    }

    /// Commit `tx` under `label`.
    ///
    /// # Errors
    ///
    /// The store's refusal; [`DurableError::OwnershipLost`] once the actor
    /// is someone else's.
    pub async fn commit(
        &self,
        tx: ActorTx,
        label: CommitLabel,
    ) -> Result<ActorCommit, DurableError> {
        self.store.commit(tx, label).await
    }

    /// Wait until mail may have arrived for the actor: a wake hint, or one
    /// claim-poll interval, whichever comes first. A hint is only a hint;
    /// the next [`Owned::begin`] reads what actually arrived.
    pub async fn wait_for_mail(&self) {
        self.mail_waker().wait().await;
    }

    /// What [`Owned::wait_for_mail`] waits on, for the activation's context
    /// to wait on while it runs: a wake hint, or one claim-poll interval.
    #[must_use]
    pub fn mail_waker(&self) -> MailWaker {
        MailWaker {
            hint: Arc::clone(&self.hint),
            clock: Arc::clone(&self.clock),
            poll: self.poll,
        }
    }

    /// The node's clock.
    #[must_use]
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// The node's store: every owner write and read the activation makes
    /// goes through it.
    #[must_use]
    pub fn store(&self) -> &Arc<dyn DurableStore> {
        &self.store
    }
}

/// Waits until mail may have arrived for one owned actor: a wake hint, or
/// one claim-poll interval, whichever comes first. The per-actor poll is
/// the correctness backstop for a hint that never came (a wake committed on
/// another node); a hint only shortens the wait.
#[derive(Clone)]
pub struct MailWaker {
    hint: Arc<Notify>,
    clock: Arc<dyn Clock>,
    poll: std::time::Duration,
}

impl MailWaker {
    /// Wait for a hint or one poll interval.
    pub async fn wait(&self) {
        tokio::select! {
            () = self.hint.notified() => {}
            () = self.clock.sleep(self.poll) => {}
        }
    }
}

impl std::fmt::Debug for MailWaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MailWaker")
            .field("poll", &self.poll)
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct HintsInner {
    running: Mutex<HashMap<ActorKey, Arc<Notify>>>,
    claim: Notify,
}

/// The in-process half of a wake: a mailbox writer on this node hands the
/// runner what its commit woke, so the actor's activation, or the next
/// claim, sees the mail without waiting for its poll.
#[derive(Clone, Default)]
pub struct Hints {
    inner: Arc<HintsInner>,
}

impl Hints {
    /// Hint one woken actor: its activation when this node runs it, else
    /// the claim loop.
    pub fn wake(&self, woken: &Woken) {
        let running = self
            .inner
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&woken.actor)
            .cloned();
        match running {
            Some(hint) => hint.notify_one(),
            None => self.inner.claim.notify_one(),
        }
    }

    fn start(&self, actor: ActorKey) -> Arc<Notify> {
        let hint = Arc::new(Notify::new());
        self.inner
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(actor, Arc::clone(&hint));
        hint
    }

    fn end(&self, actor: &ActorKey) {
        self.inner
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(actor);
    }

    fn running(&self, actor: &ActorKey) -> bool {
        self.inner
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(actor)
    }
}

/// One node's serving loop.
pub struct Runner {
    store: Arc<dyn DurableStore>,
    clock: Arc<dyn Clock>,
    config: RunnerConfig,
    activation: Arc<dyn Activation>,
    hints: Hints,
}

impl Runner {
    #[must_use]
    pub fn new(
        store: Arc<dyn DurableStore>,
        clock: Arc<dyn Clock>,
        config: RunnerConfig,
        activation: Arc<dyn Activation>,
    ) -> Self {
        Self {
            store,
            clock,
            config,
            activation,
            hints: Hints::default(),
        }
    }

    /// This runner taking its wake hints from `hints`, which this node's
    /// mailbox writers already hold.
    #[must_use]
    pub fn with_hints(mut self, hints: Hints) -> Self {
        self.hints = hints;
        self
    }

    /// The runner's hint handle, for this node's mailbox writers.
    #[must_use]
    pub fn hints(&self) -> Hints {
        self.hints.clone()
    }

    /// Serve until `stop` completes or the node loses its lease.
    ///
    /// # Errors
    ///
    /// The store's refusal of the node's registration.
    pub async fn run(self, stop: impl Future<Output = ()> + Send) -> Result<Stopped, DurableError> {
        let settings = self.config.lease.settings();
        let lease = self
            .store
            .register_node(&NodeSpec {
                node: self.config.node.clone(),
                decodes: self.config.decodes.clone(),
                ttl_millis: self.config.lease.ttl_millis(),
            })
            .await?;
        let mut active: JoinSet<()> = JoinSet::new();
        let start = self.clock.now();
        let mut last_renewed = start;
        let mut next_heartbeat = start + settings.heartbeat_every;
        let mut next_reap = start + settings.reap_every;
        let mut next_claim = start;
        let mut adopt = false;
        tokio::pin!(stop);
        let stopped = loop {
            let next = next_heartbeat.min(next_reap).min(next_claim);
            tokio::select! {
                biased;
                () = &mut stop => {
                    active.abort_all();
                    while active.join_next().await.is_some() {}
                    self.store.release_node(&lease).await?;
                    break Stopped::Requested;
                }
                Some(_) = active.join_next(), if !active.is_empty() => continue,
                () = self.hints.inner.claim.notified() => {
                    next_claim = self.clock.now();
                }
                () = self.clock.sleep_until(next) => {}
            }
            let now = self.clock.now();
            if now >= next_heartbeat {
                next_heartbeat = now + settings.heartbeat_every;
                match self.store.heartbeat(&lease).await {
                    Ok(HeartbeatOutcome::Renewed { .. }) => last_renewed = now,
                    Ok(HeartbeatOutcome::Reaped) | Err(DurableError::NodeLeaseLost { .. }) => {
                        break Stopped::LeaseLost;
                    }
                    Err(_) => {}
                }
            }
            if self.clock.now().saturating_duration_since(last_renewed) >= settings.self_stop_after
            {
                break Stopped::Unrenewed;
            }
            if now >= next_reap {
                next_reap = now + settings.reap_every;
                if let Err(DurableError::NodeLeaseLost { .. }) = self.store.reap(&lease).await {
                    break Stopped::LeaseLost;
                }
            }
            if now >= next_claim {
                next_claim = now + settings.claim_poll;
                let claimed = if adopt {
                    self.store.owned(&lease).await.map(|owned| {
                        owned
                            .into_iter()
                            .filter(|claimed| !self.hints.running(&claimed.actor))
                            .collect()
                    })
                } else {
                    let room = self
                        .config
                        .max_active
                        .saturating_sub(active.len())
                        .min(self.config.claim_batch);
                    if room == 0 {
                        continue;
                    }
                    self.store.claim(&lease, room).await
                };
                match claimed {
                    Ok(claimed) => {
                        adopt = false;
                        for claimed in claimed {
                            self.activate(&mut active, claimed);
                        }
                    }
                    Err(DurableError::NodeLeaseLost { .. }) => break Stopped::LeaseLost,
                    // A claim whose reply is lost may have taken actors:
                    // adopt whatever this boot owns before claiming more.
                    Err(_) => adopt = true,
                }
            }
        };
        active.abort_all();
        while active.join_next().await.is_some() {}
        Ok(stopped)
    }

    fn activate(&self, active: &mut JoinSet<()>, claimed: Claimed) {
        let actor = claimed.actor.clone();
        let owned = Owned {
            store: Arc::clone(&self.store),
            clock: Arc::clone(&self.clock),
            hint: self.hints.start(actor.clone()),
            claimed,
            poll: self.config.lease.settings().claim_poll,
        };
        let activation = Arc::clone(&self.activation);
        let running = Running {
            hints: self.hints.clone(),
            actor,
        };
        active.spawn(async move {
            let _running = running;
            activation.activate(owned).await;
        });
    }
}

/// Ends an activation's hint registration however its task ends: returned,
/// panicked or aborted.
struct Running {
    hints: Hints,
    actor: ActorKey,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.hints.end(&self.actor);
    }
}
