use super::*;
use crate::TurnId;
use crate::facade_support::RuntimeSessionStateFacadeOps;
use lash_sansio::sync::MutexExt;

impl ManagedSessionCapability {
    pub(in crate::runtime::session_manager) async fn start_turn(
        &self,
        current: &CurrentSessionCapability,
        usage: &UsageCapability,
        request: crate::SessionTurnRequest<'_>,
    ) -> Result<AssembledTurn, crate::PluginError> {
        let (
            crate::SessionTurnInput {
                session_id,
                turn_id,
                input,
            },
            scoped_effect_controller,
        ) = request.into_parts();
        self.run_admitted_turn(
            current,
            usage,
            session_id,
            turn_id,
            input,
            scoped_effect_controller,
            CancellationToken::new(),
        )
        .await
    }

    /// Initialize a brand-new session and run its first turn as one operation.
    ///
    /// This is the process-origin initialization port (FIG-3377): the only
    /// caller is `run_process_session_turn`, which hands over the durable
    /// `SessionCreateRequest` recorded on the process row, the process's own
    /// execution authority, and its cancellation token. No facade or worker
    /// type reaches this layer.
    ///
    /// Ordering contract (the crash/replay boundary on both substrates):
    ///
    /// 1. The child session's create commit lands durably in the child store.
    /// 2. The first turn is accepted and committed inside the child session
    ///    under the ordinary session execution lease — never the process
    ///    registry.
    /// 3. Only the runner's caller records the process terminal. A committed
    ///    child turn is not itself a recorded process result.
    ///
    /// Cancellation is the standard turn cancellation, not a teardown path:
    ///
    /// * Observed before the create commit, nothing is created. A cancelled
    ///   redelivery still reconciles first: a previous attempt may have
    ///   committed the child and accepted this turn's input before crashing,
    ///   so any open input scoped to this turn is durably settled before the
    ///   cancellation is reported.
    /// * Observed after the create commit but before turn admission, the
    ///   session is retained idle — an empty durable row that is never
    ///   reclaimed by lash.
    /// * Observed while the turn runs, the supplied token is the turn's own
    ///   cancellation token, so the turn settles `Cancelled` through the same
    ///   path as every other cancelled turn: the cancelled turn commits, the
    ///   accepted turn-input row is settled, and nothing remains claimable.
    ///   The child session stays durable and reusable; lash never deletes a
    ///   session because a process was cancelled.
    ///
    /// Settlement is the fence around process terminalization: a cancelled
    /// outcome is only ever returned once this turn's accepted child input is
    /// terminal and unclaimed. A reconciliation or commit failure surfaces as
    /// a retryable init error so the substrate keeps the process recoverable
    /// instead of writing a `Cancelled` terminal over an unsettled child.
    ///
    /// Admission is the shared managed-turn registry: the process's first
    /// turn claims a `ManagedTurnLease` under `turn_concurrency_limit`, so a
    /// subagent spawning another subagent receives a fresh Tokio task stack
    /// and counts against the same bound as every other managed turn.
    #[expect(
        clippy::too_many_arguments,
        reason = "the port takes the create request, the process authority, and the turn parameters as distinct inputs; folding them would invent a wrapper type for one caller"
    )]
    pub(in crate::runtime::session_manager) async fn initialize_session_and_run_turn(
        &self,
        current: &CurrentSessionCapability,
        usage: &UsageCapability,
        create_request: crate::SessionCreateRequest,
        process_id: &crate::ProcessId,
        turn_id: TurnId,
        turn_input: crate::TurnInput,
        scoped_effect_controller: crate::ScopedEffectController<'_>,
        cancellation: CancellationToken,
    ) -> Result<InitializedSessionTurn, SessionTurnInitError> {
        let requested_session_id = create_request.session_id.clone();
        if cancellation.is_cancelled() {
            self.settle_cancelled_process_child_inputs(
                current,
                requested_session_id.as_ref(),
                process_id,
                &turn_id,
            )
            .await
            .map_err(|source| SessionTurnInitError::Reconcile {
                session_id: requested_session_id.clone(),
                source: Box::new(source),
            })?;
            return Err(SessionTurnInitError::CancelledBeforeCreate);
        }
        // A retry that finds its child still resident in this registry skips
        // the create commit entirely: re-committing over an existing head
        // would only ever produce a conflict, and the turn itself is
        // idempotent on `(session_id, turn_id)`.
        let session_id = match requested_session_id.as_ref() {
            Some(session_id) if self.registry.lock().await.contains_key(session_id) => {
                session_id.clone()
            }
            _ => {
                self.create_session(current, create_request)
                    .await
                    .map_err(|source| SessionTurnInitError::Create {
                        session_id: requested_session_id.clone(),
                        source: Box::new(source),
                    })?
                    .session_id
            }
        };
        if cancellation.is_cancelled() {
            self.settle_cancelled_process_child_inputs(
                current,
                Some(&session_id),
                process_id,
                &turn_id,
            )
            .await
            .map_err(|source| SessionTurnInitError::Reconcile {
                session_id: Some(session_id.clone()),
                source: Box::new(source),
            })?;
            return Err(SessionTurnInitError::CancelledAfterCreate { session_id });
        }
        let request = crate::SessionTurnRequest::new_process_backed(
            &session_id,
            turn_id.clone(),
            turn_input,
            process_id,
            scoped_effect_controller,
        )
        .map_err(|source| SessionTurnInitError::Request {
            session_id: session_id.clone(),
            source: Box::new(source),
        })?;
        let turn = self
            .run_admitted_turn(
                current,
                usage,
                session_id.clone(),
                turn_id,
                request.input().clone(),
                request.into_parts().1,
                cancellation,
            )
            .await
            .map_err(|source| SessionTurnInitError::Turn {
                session_id: session_id.clone(),
                source: Box::new(source),
            })?;
        Ok(InitializedSessionTurn { session_id, turn })
    }

    /// Durably settle this turn's still-open input on the cancelled process's
    /// retained child session(s) without running a turn.
    ///
    /// This is the redelivery reconcile: a previous attempt may have
    /// committed the child session and accepted the input before crashing or
    /// observing the durable cancellation. Opening the recorded child (plus
    /// any session the catalog attributes to this process, covering an id a
    /// crashed attempt minted but never recorded) and cancelling every open
    /// row scoped to `turn_id` leaves terminal receipts behind and nothing
    /// claimable — the precondition for the caller to write a `Cancelled`
    /// process terminal.
    ///
    /// A row still held under a live session-execution-lease claim refuses
    /// cancellation and surfaces as an error: a live holder can still settle
    /// it, so the process stays recoverable rather than terminalizing over an
    /// input a survivor might complete.
    async fn settle_cancelled_process_child_inputs(
        &self,
        current: &CurrentSessionCapability,
        requested_session_id: Option<&SessionId>,
        process_id: &crate::ProcessId,
        turn_id: &TurnId,
    ) -> Result<(), crate::PluginError> {
        let Some(factory) = current.host.session_store_factory.as_ref() else {
            return Ok(());
        };
        let mut candidates: Vec<SessionId> = requested_session_id.cloned().into_iter().collect();
        match factory
            .list_sessions(&crate::SessionListFilter {
                caused_by: Some(crate::CausalRef::Process {
                    process_id: process_id.clone(),
                }),
                ..Default::default()
            })
            .await
        {
            Ok(summaries) => {
                for session_id in summaries.into_iter().map(|summary| summary.session_id) {
                    if !candidates.contains(&session_id) {
                        candidates.push(session_id);
                    }
                }
            }
            Err(crate::StoreError::UnsupportedStoreOperation { .. }) => {}
            Err(error) => {
                return Err(crate::PluginError::Session(format!(
                    "failed to enumerate sessions caused by cancelled process `{process_id}`: {error}"
                )));
            }
        }
        for session_id in candidates {
            self.settle_open_process_child_turn_input(
                factory.as_ref(),
                &session_id,
                process_id,
                turn_id,
            )
            .await?;
        }
        Ok(())
    }

    async fn settle_open_process_child_turn_input(
        &self,
        factory: &dyn crate::SessionStoreFactory,
        session_id: &SessionId,
        process_id: &crate::ProcessId,
        turn_id: &TurnId,
    ) -> Result<(), crate::PluginError> {
        let Some(store) = factory
            .open_existing_store_by_id(session_id)
            .await
            .map_err(|error| {
                crate::PluginError::Session(format!(
                    "failed to inspect cancelled process `{process_id}` child session `{session_id}`: {error}"
                ))
            })?
        else {
            return Ok(());
        };
        // A session the catalog attributes to this process exists solely to
        // run its turn, so every open row under it is the dead attempt's work.
        // A session not caused by this process is foreign — only rows scoped
        // to this exact turn may be touched.
        let owned_by_process = store
            .load_session_meta()
            .await
            .map_err(|error| {
                crate::PluginError::Session(format!(
                    "failed to read cancelled process `{process_id}` child session `{session_id}` metadata: {error}"
                ))
            })?
            .is_some_and(|meta| {
                matches!(
                    &meta.relation,
                    crate::SessionRelation::Child {
                        caused_by: Some(crate::CausalRef::Process {
                            process_id: owner_process_id,
                        }),
                        ..
                    } if owner_process_id == process_id
                )
            });
        let pending = store
            .list_pending_turn_inputs(session_id)
            .await
            .map_err(|error| {
                crate::PluginError::Session(format!(
                    "failed to list cancelled process `{process_id}` child session `{session_id}` inputs: {error}"
                ))
            })?;
        let targets: Vec<crate::PendingTurnInputCancelTarget> = pending
            .iter()
            .filter(|read| {
                !read.input.state.is_terminal()
                    && (owned_by_process || read.input.state.active_turn_id() == Some(turn_id))
            })
            .map(|read| {
                crate::PendingTurnInputCancelTarget::input_id(read.input.input_id.to_string())
            })
            .collect();
        if targets.is_empty() {
            return Ok(());
        }
        let receipts = store
            .cancel_pending_turn_inputs(session_id, &targets)
            .await
            .map_err(|error| {
                crate::PluginError::Session(format!(
                    "failed to settle cancelled process `{process_id}` child session `{session_id}` inputs: {error}"
                ))
            })?;
        for receipt in &receipts {
            if let crate::PendingTurnInputCancelOutcome::AlreadyClaimed { claim, .. } =
                &receipt.outcome
            {
                return Err(crate::PluginError::Session(format!(
                    "cancelled process `{process_id}` child session `{session_id}` still holds this turn's input under a live claim: {claim:?}"
                )));
            }
        }
        Ok(())
    }

    /// The shared managed-turn drive: registry admission, event drain, task
    /// stack, and post-turn usage persistence. `cancel` is the turn's own
    /// cancellation token — a fresh one for `start_turn`, the process's token
    /// for `initialize_session_and_run_turn`.
    #[expect(
        clippy::too_many_arguments,
        reason = "the shared body takes the capabilities, the turn identity, and the caller's authority as distinct inputs; folding them would invent a wrapper type shared by only two callers"
    )]
    async fn run_admitted_turn(
        &self,
        current: &CurrentSessionCapability,
        usage: &UsageCapability,
        session_id: SessionId,
        turn_id: TurnId,
        input: crate::TurnInput,
        scoped_effect_controller: crate::ScopedEffectController<'_>,
        cancel: CancellationToken,
    ) -> Result<AssembledTurn, crate::PluginError> {
        let runtime = {
            let registry = self.registry.lock().await;
            registry.get(&session_id).cloned()
        }
        .ok_or_else(|| crate::PluginError::Session(format!("unknown session `{session_id}`")))?;
        // Registration is owned by this lease for the rest of the turn. Every
        // exit — return, error, panic, or a dropped future when the owning
        // process is cancelled — releases it, because release happens in `Drop`
        // rather than at a statement after the child await. It is claimed before
        // the event plumbing so a denied turn allocates nothing.
        let lease = ManagedTurnLease::register_with_limit(
            &self.turns,
            &session_id,
            &turn_id,
            self.turn_concurrency_limit,
        )?;
        let (event_tx, mut event_rx) = mpsc::channel::<SessionStreamEvent>(100);
        let sink = ChannelEventSink { tx: event_tx };
        let event_drain =
            crate::task::spawn(async move { while event_rx.recv().await.is_some() {} });
        let turn = match scoped_effect_controller.into_static() {
            Ok(scoped_effect_controller) => {
                // Canonical recursion-growth seam: every shareable child turn
                // gets a fresh Tokio task stack here. The registry admission
                // cap bounds how many of those independent stacks may be live
                // in this registry; future turn-path growth belongs behind this
                // boundary, rather than in new boxes at whichever recursive
                // poll site happens to overflow next.
                let task = crate::task::spawn(
                    crate::runtime::process_permit::inherit_process_execution_permit(
                        run_managed_session_turn(
                            runtime,
                            input,
                            cancel,
                            scoped_effect_controller,
                            sink.clone(),
                        ),
                    ),
                );
                let mut abort_on_drop = AbortTaskOnDrop::new(task.abort_handle());
                let joined = task.await;
                abort_on_drop.disarm();
                match joined {
                    Ok(turn) => turn,
                    Err(err) if err.is_panic() => child_turn_panicked(err.into_panic()),
                    Err(err) => Err(crate::PluginError::Session(format!(
                        "child session turn task was cancelled: {err}"
                    ))),
                }
            }
            Err(scoped_effect_controller) => {
                // Handler-scoped durable controllers cannot outlive their host
                // invocation and therefore cannot cross Tokio's `'static`
                // spawn contract. Preserve their exact journal semantics by
                // retaining the scoped controller on the calling task.
                run_managed_session_turn(
                    runtime,
                    input,
                    cancel,
                    scoped_effect_controller,
                    sink.clone(),
                )
                .await
            }
        };
        drop(sink);
        let _ = event_drain.await;
        // Release the registration only once the sink is closed and its drain
        // has finished, so this turn's own final events are included. This
        // widens the window in which the session counts as "having a running
        // turn" by the drain, compared with the pre-guard code that removed
        // the entry before `drop(sink)`; `start_turn` itself is parked here,
        // and the cancel path is covered by `Drop`.
        lease.complete();
        Box::pin(usage.persist_current_usage_ledger(current, &turn_id)).await?;
        turn
    }
}

/// The initialized child session and its committed first turn returned by
/// [`ManagedSessionCapability::initialize_session_and_run_turn`].
pub(in crate::runtime::session_manager) struct InitializedSessionTurn {
    /// The child the turn ran on — a newly created session or a resident one
    /// a retry adopted.
    pub session_id: SessionId,
    pub turn: AssembledTurn,
}

/// Where [`ManagedSessionCapability::initialize_session_and_run_turn`] stopped.
///
/// The variants partition the operation so the process runner can report the
/// stage faithfully without inspecting error strings.
pub(in crate::runtime::session_manager) enum SessionTurnInitError {
    /// Cancellation was observed before the create commit; this attempt
    /// created nothing. Any input a previous attempt left open under this
    /// turn was durably settled before this error was returned.
    CancelledBeforeCreate,
    /// Cancellation was observed in the window between the create commit and
    /// turn admission (or after the turn settled). The session is committed
    /// and retained; nothing this turn accepted remains claimable.
    CancelledAfterCreate { session_id: SessionId },
    /// Session initialization failed.
    ///
    /// `session_id` is the child's recorded identity when the durable
    /// request fixed it. Creation is multi-stage — the durable catalog row
    /// and the runtime commit can land before a later stage fails — so a
    /// `Some` here means a retained session *may* already exist for that id
    /// even though no runtime was registered. `None` means only that the
    /// request named no id, not that nothing was committed.
    Create {
        session_id: Option<SessionId>,
        source: Box<crate::PluginError>,
    },
    /// The process's execution authority did not validate for the child turn.
    Request {
        session_id: SessionId,
        source: Box<crate::PluginError>,
    },
    /// The first turn itself failed to run to a committed outcome — including
    /// a failed final commit, which can leave the accepted input open for a
    /// later attempt to recover.
    Turn {
        session_id: SessionId,
        source: Box<crate::PluginError>,
    },
    /// Cancellation was observed but reconciling the retained child's durable
    /// input failed, so this turn's accepted input may still be open. The
    /// process must stay recoverable: terminalizing it now would strand a
    /// claimable input inside the retained session.
    Reconcile {
        session_id: Option<SessionId>,
        source: Box<crate::PluginError>,
    },
}

impl SessionTurnInitError {
    /// The child session id the operation could have left durable state for —
    /// either a session it provably retained or, for `Create`/`Reconcile`,
    /// the recorded identity whose catalog row may exist even though the
    /// failure carried no runtime handle. `None` means the request named no
    /// session, not that nothing was committed.
    pub(in crate::runtime::session_manager) fn retained_session_id(&self) -> Option<&SessionId> {
        match self {
            Self::CancelledAfterCreate { session_id }
            | Self::Request { session_id, .. }
            | Self::Turn { session_id, .. } => Some(session_id),
            Self::Create { session_id, .. } | Self::Reconcile { session_id, .. } => {
                session_id.as_ref()
            }
            Self::CancelledBeforeCreate => None,
        }
    }
}

fn child_turn_panicked(
    payload: Box<dyn std::any::Any + Send>,
) -> Result<AssembledTurn, crate::PluginError> {
    let message = crate::panic_containment::payload_message(payload.as_ref());
    let failure = Err(crate::PluginError::Session(format!(
        "child_turn_panicked: {message}"
    )));
    crate::panic_containment::enforce_loudness(payload);
    failure
}

#[cfg(test)]
mod panic_tests {
    #[test]
    fn contained_child_turn_panic_is_loud_in_test_builds() {
        let previous = crate::panic_containment::set_loud(true);
        let panic = std::panic::catch_unwind(|| {
            let _ = super::child_turn_panicked(Box::new("child turn remains loud"));
        });
        crate::panic_containment::set_loud(previous);
        assert!(panic.is_err());
    }
}

type ManagedTurnRegistry = Arc<StdMutex<HashMap<TurnId, ManagedSessionTurn>>>;

/// Process-wide registration nonce source. A nonce identifies one registration
/// attempt, which `(session_id, turn_id)` cannot: an id pair can be registered,
/// released and registered again while an older lease is still alive.
static NEXT_MANAGED_TURN_REGISTRATION: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

/// Ownership of one managed child turn's registration.
///
/// A managed turn owns its active-turn registration (which gates
/// `close_session` and any further turn on that session). It is released by
/// `Drop`, so cancelling the process that drives the turn — which drops the
/// `start_turn` future at whichever await it is parked on — cannot strand it.
///
/// Release runs at most once per lease and is scoped to the exact registration
/// the lease created, identified by its `registration` nonce. A stale lease
/// whose id pair was since re-registered by a successor therefore releases
/// nothing, so no ABA sequence can let one lease evict a live successor's turn.
struct ManagedTurnLease {
    turns: ManagedTurnRegistry,
    session_id: SessionId,
    turn_id: TurnId,
    registration: u64,
    released: bool,
}

/// Admission outcome plus the state it was decided from, carried out of the
/// registry lock so the trace is emitted without holding it.
enum ManagedTurnAdmission {
    Admitted {
        registered_turns: usize,
    },
    TurnIdBusy {
        registered_turns: usize,
        holder_session_id: SessionId,
        holder_registration: u64,
    },
    SessionBusy {
        registered_turns: usize,
        holder_turn_id: TurnId,
        holder_registration: u64,
    },
    AtCapacity {
        registered_turns: usize,
        limit: usize,
    },
}

impl ManagedTurnLease {
    fn register_with_limit(
        turns: &ManagedTurnRegistry,
        session_id: &SessionId,
        turn_id: &TurnId,
        concurrency_limit: std::num::NonZeroUsize,
    ) -> Result<Self, crate::PluginError> {
        let registration =
            NEXT_MANAGED_TURN_REGISTRATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Decide under the lock, then emit the decision basis outside it: no
        // tracing subscriber runs while the registry is held.
        let decision = {
            let mut registered = lock_turns(turns);
            let registered_turns = registered.len();
            // A turn id identifies at most one live managed turn: admitting
            // two concurrent turns under one id would cross their durable
            // turn identities whatever the registry is keyed by.
            if let Some(occupied) = registered.get(turn_id) {
                ManagedTurnAdmission::TurnIdBusy {
                    registered_turns,
                    holder_session_id: occupied.session_id.clone(),
                    holder_registration: occupied.registration,
                }
            } else if let Some((holder_turn_id, holder)) = registered
                .iter()
                .find(|(_, turn)| turn.session_id == session_id)
            {
                ManagedTurnAdmission::SessionBusy {
                    registered_turns,
                    holder_turn_id: holder_turn_id.clone(),
                    holder_registration: holder.registration,
                }
            } else if registered_turns >= concurrency_limit.get() {
                ManagedTurnAdmission::AtCapacity {
                    registered_turns,
                    limit: concurrency_limit.get(),
                }
            } else {
                registered.insert(
                    turn_id.clone(),
                    ManagedSessionTurn {
                        session_id: SessionId::from(session_id.to_string()),
                        registration,
                    },
                );
                ManagedTurnAdmission::Admitted {
                    registered_turns: registered_turns + 1,
                }
            }
        };
        match decision {
            ManagedTurnAdmission::TurnIdBusy {
                registered_turns,
                holder_session_id,
                holder_registration,
            } => {
                tracing::debug!(
                    session_id = session_id.as_str(),
                    turn_id = %turn_id,
                    registered_turns,
                    holder_session_id = %holder_session_id,
                    holder_registration,
                    outcome = "denied",
                    event = "managed_turn.admission",
                    "managed turn denied: turn id already registered"
                );
                return Err(crate::PluginError::Session(format!(
                    "turn `{turn_id}` is already running on session `{holder_session_id}`"
                )));
            }
            ManagedTurnAdmission::SessionBusy {
                registered_turns,
                holder_turn_id,
                holder_registration,
            } => {
                tracing::debug!(
                    session_id = session_id.as_str(),
                    turn_id = %turn_id,
                    registered_turns,
                    holder_turn_id = %holder_turn_id,
                    holder_registration,
                    outcome = "denied",
                    event = "managed_turn.admission",
                    "managed turn denied: session already has a running turn"
                );
                return Err(crate::PluginError::Session(format!(
                    "session `{session_id}` already has a running turn"
                )));
            }
            ManagedTurnAdmission::AtCapacity {
                registered_turns,
                limit,
            } => {
                tracing::debug!(
                    session_id = session_id.as_str(),
                    turn_id = %turn_id,
                    registered_turns,
                    limit,
                    outcome = "denied",
                    event = "managed_turn.admission",
                    "managed turn denied: concurrency limit reached"
                );
                return Err(crate::PluginError::Runtime(crate::RuntimeError::new(
                    crate::RuntimeErrorCode::ManagedTurnConcurrencyLimitExceeded,
                    format!(
                        "managed-turn concurrency limit of {limit} is reached; retry after a running turn finishes"
                    ),
                )));
            }
            ManagedTurnAdmission::Admitted { registered_turns } => tracing::debug!(
                session_id = session_id.as_str(),
                turn_id = %turn_id,
                registration,
                registered_turns,
                outcome = "admitted",
                event = "managed_turn.admission",
                "managed turn admitted"
            ),
        }
        Ok(Self {
            turns: Arc::clone(turns),
            session_id: SessionId::from(session_id.to_string()),
            turn_id: turn_id.clone(),
            registration,
            released: false,
        })
    }

    #[cfg(test)]
    fn register(
        turns: &ManagedTurnRegistry,
        session_id: &SessionId,
        turn_id: &TurnId,
    ) -> Result<Self, crate::PluginError> {
        Self::register_with_limit(
            turns,
            session_id,
            turn_id,
            std::num::NonZeroUsize::new(crate::runtime::DEFAULT_MANAGED_TURN_CONCURRENCY_LIMIT)
                .expect("the managed-turn concurrency default is non-zero"),
        )
    }

    /// Release this registration.
    fn release(&mut self, reason: &'static str) {
        if self.released {
            return;
        }
        self.released = true;
        // Capture the holder that decided the outcome before the entry is
        // dropped, so a superseded release can name what superseded it.
        let (owned, holder) = {
            let mut registered = lock_turns(&self.turns);
            let holder = registered
                .get(&self.turn_id)
                .map(|turn| (turn.session_id.clone(), turn.registration));
            let owned = holder
                .as_ref()
                .is_some_and(|(_, registration)| *registration == self.registration);
            if owned {
                registered.remove(&self.turn_id);
            }
            (owned, holder)
        };
        if !owned {
            // A successor owns this turn id now, or nothing does. Releasing
            // either entry would evict live work, so this lease releases
            // nothing.
            tracing::debug!(
                session_id = %self.session_id,
                turn_id = %self.turn_id,
                registration = self.registration,
                holder_registration = holder.as_ref().map(|(_, registration)| *registration),
                holder_session_id = holder.as_ref().map(|(session_id, _)| session_id.as_str()),
                holder_present = holder.is_some(),
                reason,
                consulted = "managed_turn_registry",
                outcome = "superseded",
                event = "managed_turn.release",
                "managed turn release skipped: registration was superseded"
            );
            return;
        }
        tracing::debug!(
            session_id = %self.session_id,
            turn_id = %self.turn_id,
            registration = self.registration,
            reason,
            outcome = "released",
            event = "managed_turn.release",
            "managed turn registration released"
        );
    }

    fn complete(mut self) {
        self.release("completed")
    }
}

impl Drop for ManagedTurnLease {
    fn drop(&mut self) {
        self.release("dropped");
    }
}

/// Workspace-policy lock acquisition for the active-turn registry. Nothing is
/// ever left half-written under this lock: every critical section is a single
/// map operation.
pub(in crate::runtime::session_manager) fn lock_turns(
    turns: &ManagedTurnRegistry,
) -> std::sync::MutexGuard<'_, HashMap<TurnId, ManagedSessionTurn>> {
    turns.lock_recover()
}

async fn run_managed_session_turn(
    runtime: RuntimeHandle,
    input: crate::TurnInput,
    cancel: CancellationToken,
    scoped_effect_controller: crate::ScopedEffectController<'_>,
    sink: ChannelEventSink,
) -> Result<AssembledTurn, crate::PluginError> {
    // This mutex is the managed runtime's single-writer boundary. Hold it for
    // the complete turn and publish from the guarded post-turn state before
    // releasing it, exactly as the former native path did.
    let mut runtime_guard = runtime.runtime.lock().await;
    let scoped_effect_controller = match scoped_effect_controller.execution_scope() {
        crate::ExecutionScope::Turn { turn_id, .. } => scoped_effect_controller
            .rescope(
                crate::AdmittedScope::unpinned(runtime_guard.state.turn_scope(turn_id.clone()))
                    .map_err(|err| crate::PluginError::Session(err.to_string()))?,
            )
            .map_err(crate::PluginError::Runtime)?,
        crate::ExecutionScope::Process { .. } => scoped_effect_controller,
        scope => {
            return Err(crate::PluginError::Session(format!(
                "managed session turns require a turn or process execution scope, got {scope:?}"
            )));
        }
    };
    let options =
        crate::runtime::TurnOptions::new(cancel, scoped_effect_controller).with_events(&sink);
    let result = runtime_guard
        .stream_turn_with_agent_frames(input, options)
        .await
        .map_err(crate::PluginError::Runtime)
        .and_then(|run| {
            run.into_final_turn().ok_or_else(|| {
                crate::PluginError::Session("agent frame run completed without a turn".to_string())
            })
        });
    runtime.publish_from(&runtime_guard);
    result
}

struct AbortTaskOnDrop {
    handle: tokio::task::AbortHandle,
    armed: bool,
}

impl AbortTaskOnDrop {
    fn new(handle: tokio::task::AbortHandle) -> Self {
        Self {
            handle,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for AbortTaskOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared_turns() -> ManagedTurnRegistry {
        Arc::new(StdMutex::new(HashMap::new()))
    }

    #[test]
    fn dropped_managed_turn_lease_releases_the_registration() {
        let turns = shared_turns();
        let lease =
            ManagedTurnLease::register(&turns, &SessionId::from("session"), &TurnId::from("turn"))
                .expect("register");

        drop(lease);

        assert!(turns.lock_recover().is_empty());
    }

    #[test]
    fn completed_managed_turn_lease_releases_exactly_once() {
        let turns = shared_turns();
        let lease =
            ManagedTurnLease::register(&turns, &SessionId::from("session"), &TurnId::from("turn"))
                .expect("register");

        // `complete` consumes the lease, so its `Drop` runs immediately after
        // the explicit release: the double removal must be a no-op.
        lease.complete();

        assert!(turns.lock_recover().is_empty());
    }

    /// The ABA a `(session_id, turn_id)` check cannot see: the stale lease's own
    /// id pair is registered again by a successor, so identity has to come from
    /// the registration nonce.
    ///
    /// This walks the sequence found in review, step by step:
    /// 1. `A` registers `(S, T)` and keeps running.
    /// 2. A foreign session `X` tries to register `T`. In the original sequence
    ///    this overwrote `A`'s entry; admission now **rejects** it, which is the
    ///    guard that makes the rest of that sequence unreachable — asserted
    ///    here so relaxing it fails this test.
    /// 3. `A`'s registration is vacated without `A` noticing. No in-tree path
    ///    does this today (only the owning lease removes an entry), so the test
    ///    vacates the slot directly: the nonce must protect the successor for
    ///    *any* future removal path, not just the ones that exist now.
    /// 4. The original session re-registers `(S, T)` as `B`.
    /// 5. Stale `A` drops — and must not release `B`'s entry.
    #[test]
    fn stale_managed_turn_lease_does_not_evict_a_same_identity_successor() {
        let turns = shared_turns();
        let stale =
            ManagedTurnLease::register(&turns, &SessionId::from("session"), &TurnId::from("turn"))
                .expect("register");
        // Step 2: the foreign-session overwrite is now denied outright.
        let foreign = match ManagedTurnLease::register(
            &turns,
            &SessionId::from("foreign-session"),
            &TurnId::from("turn"),
        ) {
            Ok(_) => {
                panic!("a foreign session must not take a turn id that is already running")
            }
            Err(err) => err,
        };
        assert!(
            foreign
                .to_string()
                .contains("turn `turn` is already running on session `session`"),
            "unexpected denial: {foreign}"
        );
        assert_eq!(
            lock_turns(&turns).get("turn").map(|turn| turn.registration),
            Some(stale.registration),
            "the denied foreign registration must leave the live entry untouched"
        );
        // Step 3.
        lock_turns(&turns).remove("turn");
        // Step 4.
        let successor =
            ManagedTurnLease::register(&turns, &SessionId::from("session"), &TurnId::from("turn"))
                .expect("re-register");

        // Step 5.
        drop(stale);

        assert_eq!(
            lock_turns(&turns).get("turn").map(|turn| turn.registration),
            Some(successor.registration),
            "a stale lease must not release the successor's registration"
        );

        // The successor still owns the entry and releases it itself.
        successor.complete();
        assert!(lock_turns(&turns).is_empty());
    }

    #[test]
    fn managed_turn_lease_rejects_a_second_turn_on_the_same_turn_id() {
        let turns = shared_turns();
        let _lease =
            ManagedTurnLease::register(&turns, &SessionId::from("session"), &TurnId::from("turn"))
                .expect("register");

        let err = match ManagedTurnLease::register(
            &turns,
            &SessionId::from("other-session"),
            &TurnId::from("turn"),
        ) {
            Ok(_) => panic!("a turn id identifies at most one live managed turn"),
            Err(err) => err,
        };

        assert!(
            err.to_string()
                .contains("turn `turn` is already running on session `session`"),
            "unexpected denial: {err}"
        );
        assert_eq!(
            lock_turns(&turns)
                .get("turn")
                .map(|turn| turn.session_id.clone()),
            Some(SessionId::from("session")),
            "a denied registration must not overwrite the live one"
        );
    }

    #[test]
    fn managed_turn_admission_rejects_beyond_cap_with_typed_retryable_error() {
        let turns = shared_turns();
        let limit = std::num::NonZeroUsize::new(1).expect("test limit is non-zero");
        let lease = ManagedTurnLease::register_with_limit(
            &turns,
            &SessionId::from("first-session"),
            &TurnId::from("first-turn"),
            limit,
        )
        .expect("first turn fits under cap");

        let error = match ManagedTurnLease::register_with_limit(
            &turns,
            &SessionId::from("second-session"),
            &TurnId::from("second-turn"),
            limit,
        ) {
            Ok(_) => panic!("second concurrent turn must be denied at capacity"),
            Err(error) => error,
        };
        let crate::PluginError::Runtime(error) = error else {
            panic!("capacity denial must use the typed runtime error family");
        };
        assert_eq!(
            error.code,
            crate::RuntimeErrorCode::ManagedTurnConcurrencyLimitExceeded
        );
        assert!(error.is_retryable());
        assert_eq!(lock_turns(&turns).len(), 1);

        drop(lease);
        ManagedTurnLease::register_with_limit(
            &turns,
            &SessionId::from("second-session"),
            &TurnId::from("second-turn"),
            limit,
        )
        .expect("released capacity is immediately reusable");
    }

    #[test]
    fn managed_turn_lease_rejects_a_second_turn_on_the_same_session() {
        let turns = shared_turns();
        let _lease =
            ManagedTurnLease::register(&turns, &SessionId::from("session"), &TurnId::from("turn"))
                .expect("register");

        let err = match ManagedTurnLease::register(
            &turns,
            &SessionId::from("session"),
            &TurnId::from("other-turn"),
        ) {
            Ok(_) => panic!("a session runs at most one managed turn"),
            Err(err) => err,
        };

        assert!(err.to_string().contains("already has a running turn"));
    }

    #[test]
    fn session_turn_request_requires_matching_scope_and_sets_trace_turn_id() {
        let controller = crate::NativeRuntimeEffectController::default();
        let scoped_effect_controller = crate::ScopedEffectController::borrowed(
            &controller,
            crate::AdmittedScope::turn("child", "child-turn"),
        )
        .expect("turn scope");
        let request = crate::SessionTurnRequest::new(
            "child",
            "child-turn",
            crate::TurnInput::text("run child"),
            scoped_effect_controller,
        )
        .expect("valid child turn request");

        assert_eq!(request.session_id(), "child");
        assert_eq!(request.turn_id(), "child-turn");
        assert_eq!(request.input().trace_turn_id.as_deref(), Some("child-turn"));
    }

    #[test]
    fn session_turn_request_rejects_mismatched_execution_scope() {
        let controller = crate::NativeRuntimeEffectController::default();
        let scoped_effect_controller = crate::ScopedEffectController::borrowed(
            &controller,
            crate::AdmittedScope::turn("child", "other-turn"),
        )
        .expect("turn scope");
        let err = match crate::SessionTurnRequest::new(
            "child",
            "child-turn",
            crate::TurnInput::text("run child"),
            scoped_effect_controller,
        ) {
            Ok(_) => panic!("mismatched turn scope should fail"),
            Err(err) => err,
        };

        assert!(err.to_string().contains("same id"));
    }

    #[test]
    fn process_backed_session_turn_request_preserves_admitted_scope_and_trace_identity() {
        let controller = crate::NativeRuntimeEffectController::default();
        let process_scope = crate::ExecutionScope::process("process:subagent:call");
        let scoped_effect_controller = crate::ScopedEffectController::borrowed(
            &controller,
            crate::AdmittedScope::process(crate::ProcessRef::new(
                "process:subagent:call",
                crate::ProcessIncarnation::from_registration_sequence(1),
            )),
        )
        .expect("process scope");
        let request = crate::SessionTurnRequest::new_process_backed(
            "session:subagent:call",
            "process:subagent:call",
            crate::TurnInput::text("run child"),
            &crate::ProcessId::from("process:subagent:call"),
            scoped_effect_controller,
        )
        .expect("valid process-backed child turn request");

        assert_eq!(request.session_id(), "session:subagent:call");
        assert_eq!(request.turn_id(), "process:subagent:call");
        assert_eq!(
            request.input().trace_turn_id.as_deref(),
            Some("process:subagent:call")
        );
        let (_, scoped_effect_controller) = request.into_parts();
        assert_eq!(scoped_effect_controller.execution_scope(), &process_scope);
    }
}
