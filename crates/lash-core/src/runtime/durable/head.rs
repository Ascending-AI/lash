//! A session's committed head as a turn starts from it, and the head commit
//! a finished turn's `turn.commit` writes over it (ADR 0132 §4). Owned by L3
//! (FIG-5172).
//!
//! The head is read from the session store's current window, never from the
//! history before it, so what a turn loads does not grow with the turns the
//! session committed before its window.
//!
//! A turn starts from the head's window pinned by the head it is (FIG-5206):
//! its checkpoint names the window by that pin instead of holding it, and a
//! restore reads the window again at the pinned head, however far the live
//! head has moved since.
//!
//! The session actor keeps the head it loaded in its owner cache
//! ([`SessionOwner`], FIG-5207), so a turn reads its session's window from the
//! store at most once: the start or the restore loads it, and the turn's
//! head commit is built over the same head, never read again. The turn's
//! prelude takes its base window from that head when it names it, and every
//! later sync from the base the driver already holds (FIG-6634).

use std::sync::Arc;

use lash_sansio::TurnWindowPin;

use lash_durable::{ActorKey, Epoch};

use super::session::{TurnCommit, TurnDone, TurnError};
use crate::ActorContext;
use crate::Clock;
use crate::CommitBudget;
use crate::TurnOutcome;
use crate::runtime::{RuntimeSessionState, TurnBoundary};
use crate::store::{SessionHeadRef, SessionStore, StoreError, WindowSelector};
use lash_execution_contracts::backend::Backend;
use lash_kernel_value::identity::SessionId;
use lash_vocabulary::identity::TurnId;

/// The committed window a session's turn starts from.
pub type TurnWindow = lash_sansio::TurnWindow<crate::ProtocolEvent>;

/// A session's committed head.
#[derive(Clone)]
pub struct SessionHead {
    state: Arc<RuntimeSessionState>,
    fleet: crate::FleetFormat,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for SessionHead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionHead")
            .field("session", &self.state.session_id)
            .field("revision", &self.state.head_revision)
            .finish_non_exhaustive()
    }
}

impl SessionHead {
    /// `session`'s head in `backend`'s session store.
    ///
    /// # Errors
    ///
    /// [`TurnError::Runtime`]: the terminal `SessionCreationUnrecorded`
    /// when the session has no head, or the store's own typed refusal
    /// ([`refused`]), terminal when no pass would clear it.
    pub async fn load(backend: &Backend, session: &SessionId) -> Result<Self, TurnError> {
        let store = session_store(backend, session).await?;
        let state = load_state(&store, session, WindowSelector::Current).await?;
        Ok(Self {
            state: Arc::new(state),
            fleet: store.fleet_format(),
            clock: backend.clock(),
        })
    }

    /// The head's revision.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.state.head_revision
    }

    /// The head's state.
    #[must_use]
    pub fn state(&self) -> &RuntimeSessionState {
        &self.state
    }

    /// The pin of this head's window: the head itself.
    ///
    /// # Errors
    ///
    /// [`TurnError::Exec`] when the pin does not encode.
    pub fn pin(&self) -> Result<TurnWindowPin, TurnError> {
        window_pin(&self.state)
            .map_err(|error| TurnError::Exec(format!("the window's pin does not encode: {error}")))
    }

    /// The head's window, pinned by this head: what a turn that starts here
    /// starts from ([`TurnMachine::in_window`](crate::TurnMachine::in_window)).
    ///
    /// # Errors
    ///
    /// [`TurnError::Exec`] when the pin does not encode.
    pub fn window(&self) -> Result<TurnWindow, TurnError> {
        Ok(window_of(&self.state, self.pin()?))
    }

    /// The head's window as a turn prelude's base ([`restore_prelude`]):
    /// the graph head it is, with no checkpoint, and its read model's
    /// messages over its render cache.
    #[must_use]
    pub fn prelude_base(&self) -> (SessionHeadRef, crate::MessageSequence) {
        let read = self.state.read_model();
        (
            SessionHeadRef {
                generation: 0,
                revision: self.state.head_revision,
                leaf: self.state.session_graph.leaf_node_id.clone(),
                checkpoint: None,
            },
            crate::MessageSequence::from_base(read.messages)
                .with_base_render_cache(read.prompt_render_cache),
        )
    }

    /// The commit of `run` that publishes `done`'s messages and outcome as
    /// the head's next revision, written under `budget`. It replaces this
    /// head: the store refuses it with `HeadMoved` once the head is
    /// elsewhere.
    ///
    /// # Errors
    ///
    /// [`TurnError::Exec`] when the commit cannot be assembled.
    pub async fn commit(
        &self,
        run: &TurnId,
        done: TurnDone,
        budget: CommitBudget,
    ) -> Result<TurnCommit, TurnError> {
        lash_core_ids::profile_history!(DurableCommitSql, {
            let outcome = done
                .outcome
                .unwrap_or(TurnOutcome::Stopped(crate::TurnStop::Incomplete));
            let mut boundary = TurnBoundary::from_state_with_clock(
                self.state.as_ref().clone(),
                Arc::clone(&self.clock),
                crate::ExecutionScope::turn(self.state.session_id.clone(), run.clone()),
                budget,
            )
            .with_fleet_format(self.fleet);
            let commit = boundary
                .durable_commit(done.messages, &outcome, &[], None, None)
                .await
                .map_err(|error| TurnError::Exec(format!("the turn's head commit: {error}")))?;
            Ok(TurnCommit {
                postimage: boundary.state().clone(),
                expected_head: commit.expected_head_revision,
                commit: crate::store::encode_session_commit(&commit)
                    .map_err(|error| TurnError::Exec(error.to_string()))?,
            })
        })
    }
}

/// The session actor's owner cache of its committed head (ADR 0132 §3),
/// keyed by `(actor, epoch)`: the head the owner loaded for the turn it runs,
/// which the turn's start or restore reads its window from and its head
/// commit is built over.
///
/// It is never a grant. Every write still commits in a fenced transaction,
/// and the head commit is the store's compare-and-set against this head's
/// revision. The context owns the acknowledged projection; this handle borrows
/// its immutable head for preparation and publication. Every successful write
/// adopts its exact postimage. Errors invalidate the whole projection, and a
/// new claim or recovery loads it coherently from durable rows.
#[derive(Default)]
pub struct SessionOwner {
    held: Option<CachedHead>,
    terminal_read_refusal: bool,
}

struct CachedHead {
    actor: ActorKey,
    epoch: Epoch,
    head: SessionHead,
}

impl std::fmt::Debug for SessionOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionOwner")
            .field("head", &self.held.as_ref().map(|held| &held.head))
            .finish()
    }
}

impl SessionOwner {
    /// `session`'s head as `cx`'s owner loaded it, loading it when nothing
    /// is held under `cx`'s actor and epoch.
    ///
    /// # Errors
    ///
    /// [`SessionHead::load`]'s.
    pub fn head<'a>(
        &'a mut self,
        cx: &'a ActorContext,
        session: &'a SessionId,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<&'a SessionHead, TurnError>> + Send + 'a>,
    > {
        Box::pin(async move {
            lash_core_ids::profile_history!(OwnerRetention, {
                if let Some(projection) = cx.session_projection().await? {
                    let head = SessionHead {
                        state: projection.head,
                        fleet: projection.fleet,
                        clock: cx.backend().clock(),
                    };
                    self.held = Some(CachedHead {
                        actor: cx.actor().clone(),
                        epoch: cx.epoch(),
                        head,
                    });
                } else {
                    let loaded = lash_core_ids::operational_metrics::sql::classify_owner_reads(
                        lash_core_ids::operational_metrics::sql::OwnerReadClass::Recovery,
                        async {
                            let basis = cx.begin().await?;
                            cx.probe().owner_read(
                                cx.actor(),
                                cx.epoch(),
                                lash_durable::OwnerRead::RecoveryLoads,
                                1,
                            );

                            let mut head = SessionHead::load(cx.backend(), session).await?;
                            Arc::make_mut(&mut head.state)
                                .session_graph
                                .detach_owner_prompt_rendering();
                            let turn = cx.durable_reads()?.turn(session).await?;
                            // TurnRestore owns checkpoint decoding and its typed refusal.
                            // Trace names remain a cold on-demand read.
                            let checked = cx.begin().await?;
                            if checked.revision() != basis.revision() {
                                return Err(TurnError::Exec(
                                    "session changed during coherent hydration".into(),
                                ));
                            }
                            cx.load_session_projection(
                            &basis,
                            lash_core_execution::runtime::actor::owner_state::SessionProjection {
                                prompt: Default::default(),
                                configuration: Default::default(),
                                composition: None,
                    environment: None,
                                phase: None,
                                head: Arc::clone(&head.state),
                                fleet: head.fleet,
                                turn,
                            },
                        )
                        .await?;
                            Ok::<_, TurnError>(head)
                        },
                    )
                    .await;
                    let head = match loaded {
                        Ok(head) => head,
                        Err(error) => {
                            self.terminal_read_refusal = matches!(
                                &error,
                                TurnError::Runtime(refusal) if refusal.is_terminal()
                            );
                            return Err(error);
                        }
                    };
                    self.held = Some(CachedHead {
                        actor: cx.actor().clone(),
                        epoch: cx.epoch(),
                        head,
                    });
                }
                self.terminal_read_refusal = false;
                self.held
                    .as_ref()
                    .map(|held| &held.head)
                    .ok_or_else(|| TurnError::Exec("session projection was not installed".into()))
            })
        })
    }

    /// Discover an idle claim's head for recovery publication. A head that
    /// already refused preparation is not read again just to announce it.
    pub(super) async fn discover(&mut self, cx: &ActorContext, session: &SessionId) {
        if !self.terminal_read_refusal {
            let _ = self.head(cx, session).await;
        }
    }

    /// The head held under `cx`'s actor and epoch, without loading one.
    #[must_use]
    pub fn held(&self, cx: &ActorContext) -> Option<&SessionHead> {
        self.held
            .as_ref()
            .filter(|held| held.actor == *cx.actor() && held.epoch == cx.epoch())
            .map(|held| &held.head)
    }

    /// The committed window `pin` names in `session`: the held head's when
    /// `pin` is the head ([`Self::head`]), else read at the pin
    /// ([`load_window`]).
    ///
    /// # Errors
    ///
    /// [`Self::head`]'s and [`load_window`]'s.
    pub async fn window_at(
        &mut self,
        cx: &ActorContext,
        session: &SessionId,
        pin: &TurnWindowPin,
    ) -> Result<TurnWindow, TurnError> {
        // An explicit pin can be read independently of a serving claim.
        // Only an acknowledged current projection is an owner cache hit.
        if cx.session_projection().await?.is_some() {
            let head = self.head(cx, session).await?;
            if head.pin()? == *pin {
                return head.window();
            }
        }
        cx.probe().owner_read(
            cx.actor(),
            cx.epoch(),
            lash_durable::OwnerRead::HistoricalPinReads,
            1,
        );
        lash_core_ids::operational_metrics::sql::classify_owner_reads(
            lash_core_ids::operational_metrics::sql::OwnerReadClass::Historical,
            load_window(cx.backend(), session, pin),
        )
        .await
    }

    pub(crate) async fn acknowledged(
        &mut self,
        cx: &ActorContext,
        session: &SessionId,
    ) -> Result<(), TurnError> {
        if let Some(projection) = cx.session_projection().await? {
            let fleet = projection.fleet;
            self.held = Some(CachedHead {
                actor: cx.actor().clone(),
                epoch: cx.epoch(),
                head: SessionHead {
                    state: projection.head,
                    fleet,
                    clock: cx.backend().clock(),
                },
            });
        } else {
            self.evict();
        }
        let _ = session;
        Ok(())
    }

    /// Drop the held head: the next [`Self::head`] reloads it.
    pub fn evict(&mut self) {
        self.held = None;
        self.terminal_read_refusal = false;
    }
}

/// `session`'s store in `backend`'s catalog.
pub(crate) async fn session_store(
    backend: &Backend,
    session: &SessionId,
) -> Result<SessionStore, TurnError> {
    let catalog = backend.session_store_factory();
    match catalog.lookup_session(session).await.map_err(refused)? {
        crate::store::SessionLookup::Live(_) => {
            let runtime: Arc<dyn crate::store::RuntimeStore> = catalog;
            SessionStore::new(runtime, session.clone()).map_err(refused)
        }
        crate::store::SessionLookup::Deleted => Err(refused(StoreError::SessionDeleted {
            session_id: session.clone(),
        })),
        crate::store::SessionLookup::Absent => Err(refused(StoreError::SessionNotFound {
            session_id: session.clone(),
        })),
    }
}

/// The store's `error` as the open's typed refusal: its own code and cause,
/// so a refusal no pass would clear (a fenced writer, an unsupported or
/// newer generation, a foreign window, corrupt stored data) answers the run
/// it blocks, and a transient fault fails the pass for the next one to
/// retry (FIG-4597, FIG-5358).
fn refused(error: StoreError) -> TurnError {
    TurnError::Runtime(error.runtime_error())
}

/// The window `pin` names in `session`'s store ([`SessionHead::window`]),
/// read at the pinned head however far the live head has moved since.
/// Revision zero is the session before any head existed, whose window is
/// empty; any other is read at the pinned leaf (ADR 0112 §5).
///
/// # Errors
///
/// [`TurnError::Exec`] when the pin does not decode or the store no longer
/// retains the pinned head; the store's refusal.
pub async fn load_window(
    backend: &Backend,
    session: &SessionId,
    pin: &TurnWindowPin,
) -> Result<TurnWindow, TurnError> {
    let head: SessionHeadRef = serde_json::from_str(pin.as_str()).map_err(|error| {
        TurnError::Exec(format!(
            "window pin `{}` does not decode: {error}",
            pin.as_str()
        ))
    })?;
    if head.revision == 0 {
        return Ok(TurnWindow::new(
            pin.clone(),
            lash_protocol_types::AppendVec::new(),
            lash_protocol_types::AppendVec::new(),
        ));
    }
    let store = session_store(backend, session).await?;
    let state = load_state(&store, session, WindowSelector::Admitted(head)).await?;
    Ok(window_of(&state, pin.clone()))
}

/// The views `prelude` records over the committed window it names, in
/// `store`'s session (FIG-6634): over `held`'s base when `held`'s head is
/// that window, else over the window read from `store` with its state
/// version. A window is immutable, so the same graph head (generation,
/// revision and leaf) always reads back the same messages; the checkpoint
/// a head carries is no part of its messages. `held` must be a base of
/// `store`'s session, as a store read of its head gave it.
///
/// # Errors
///
/// [`TurnPrelude::restore`]'s and [`TurnPrelude::restore_over`]'s.
pub(crate) async fn restore_prelude(
    prelude: &crate::runtime::effect::TurnPrelude,
    held: Option<(&SessionHeadRef, &crate::MessageSequence)>,
    store: &SessionStore,
) -> Result<
    (
        crate::MessageSequence,
        crate::MessageSequence,
        crate::session_model::context::PreparedContext,
    ),
    crate::RuntimeEffectControllerError,
> {
    match held {
        Some((head, base)) if same_window(head, &prelude.history) => {
            prelude.restore_over(store.session_id(), base.clone())
        }
        _ => prelude.restore(store).await,
    }
}

/// Whether `a` and `b` name the same committed window, whatever
/// checkpoint either carries.
fn same_window(a: &SessionHeadRef, b: &SessionHeadRef) -> bool {
    a.generation == b.generation && a.revision == b.revision && a.leaf == b.leaf
}

/// The pin of the window of `state`, a committed head: the head itself.
pub(crate) fn window_pin(state: &RuntimeSessionState) -> Result<TurnWindowPin, serde_json::Error> {
    let head = SessionHeadRef {
        generation: 0,
        revision: state.head_revision,
        leaf: state.session_graph.leaf_node_id.clone(),
        checkpoint: state.checkpoint_ref.clone(),
    };
    Ok(TurnWindowPin::new(serde_json::to_string(&head)?))
}

/// `state`'s window under `pin`.
fn window_of(state: &RuntimeSessionState, pin: TurnWindowPin) -> TurnWindow {
    let read = state.read_model();
    TurnWindow::new(pin, read.messages, read.active_events)
        .with_render_cache(read.prompt_render_cache)
}

/// The session's window `selector` names as runtime state.
pub(crate) async fn load_state(
    store: &SessionStore,
    session: &SessionId,
    selector: WindowSelector,
) -> Result<RuntimeSessionState, TurnError> {
    let current = matches!(selector, WindowSelector::Current);
    let loaded = crate::store::load_session_window_state(store, selector)
        .await
        .map_err(refused)?
        .ok_or_else(|| {
            // A catalog row no create committed a head for recorded no
            // config: its turn is refused for good, never run on defaults
            // (FIG-4553).
            if current {
                TurnError::Runtime(crate::RuntimeError::new(
                    crate::RuntimeErrorCode::SessionCreationUnrecorded,
                    format!("session {session} has no head: its creation recorded no config"),
                ))
            } else {
                TurnError::Exec(format!("session {session} has no head"))
            }
        })?;
    Ok(loaded.state)
}

#[cfg(test)]
#[path = "head_tests.rs"]
mod tests;
