use super::*;
use lash::SessionId;

pub(crate) struct StateProjectionReads {
    pub(crate) read_view: lash::persistence::SessionReadView,
    /// The durable handle the projection read through, kept for the caller's
    /// history paging.
    pub(crate) durable: lash::DurableSession,
    pub(crate) has_durable_head: bool,
    pub(crate) cursor: SessionCursor,
    pub(crate) pending_turn_inputs: Vec<lash::PendingTurnInputRead>,
    pub(crate) queued_work: Vec<lash::persistence::QueuedWorkBatch>,
    pub(crate) turn_input_applications: Vec<lash::remote::observations::RemoteTurnInputApplication>,
    pub(crate) turn_failure_settlements: Vec<lash::TurnFailureSettlement>,
}

impl AppState {
    /// Attaches an observation handle without taking the session's execution
    /// lease.
    ///
    /// `/api/observations` is the page's live turn stream: it writes nothing,
    /// and the page reconnects it every 900 ms for as long as it is refused.
    /// [`lash::SessionBuilder::open`] claims the execution lease to admit the
    /// state it loads, which is the one thing a running turn holds for its
    /// whole length — so every reconnect during a turn burnt the bounded retry
    /// budget and answered 503, and the reconnect loop made that a storm: 156
    /// `session.open.contended` and 27 `session.open.retry_exhausted` records
    /// in one ~25 s turn on the judged `rlm-cell-boundary` run.
    ///
    /// The durable head this attach needs is the same one `/api/state` reads
    /// without a lease, so it is read the same way and handed to
    /// [`lash::SessionBuilder::observe_with_state`], which admits nothing,
    /// claims nothing, and is never a runtime the session's shifts run on.
    /// `observe_with_state` is kept because these callers need a live
    /// observation stream, which only a `LashSession` serves; it resolves the
    /// catalog itself, so an unknown id is `UnknownSession` and a tombstone is
    /// `SessionDeleted` without a lookup here. Observing never creates
    /// (FIG-4112): no row is written (FIG-3144, FIG-3151).
    pub(crate) async fn open_session_for_observation(
        &self,
        session_id: &SessionId,
    ) -> Result<lash::LashSession, lash::EmbedError> {
        let request = state_store_request(self, session_id);
        let runtime_store: Arc<dyn lash::persistence::RuntimeStore> =
            self.core.backend().session_store_factory();
        let store = lash::persistence::SessionStore::new(runtime_store, session_id.clone())
            .map_err(lash::EmbedError::Store)?;
        let state = match lash::persistence::load_session_window_state(
            &store,
            lash::persistence::WindowSelector::Current,
        )
        .await
        {
            Ok(loaded) => loaded.map(|loaded| loaded.state),
            Err(lash::persistence::StoreError::SessionNotFound { .. }) => None,
            Err(error) => return Err(lash::EmbedError::Store(error)),
        }
        .unwrap_or_else(|| {
            // A session with no durable head yet: the same empty state the
            // `/api/state` projection falls back to, so an observer that
            // attaches before the first commit sees what the snapshot does.
            let mut state =
                lash::persistence::RuntimeSessionState::new(request.config.session_policy());
            state.session_id = session_id.clone();
            state
        });
        self.session_builder(session_id.clone())
            .observe_with_state(state)
            .await
    }
}

pub(crate) fn state_store_request(
    state: &AppState,
    session_id: &SessionId,
) -> lash::persistence::SessionStoreCreateRequest {
    let mut policy = lash::runtime::SessionPolicy::new(
        lash::TurnBudget::Unbounded,
        lash::MaxToolCalls::new(1024),
    );
    let selection = state.selected_llm_profile();
    policy.model = workbench_recorded_llm_profile(&selection.key())
        .ok()
        .map(|model| lash::LlmProfileConfig::new(model).with_reasoning(selection.reasoning()));
    lash::persistence::SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: session_id.clone(),
        relation: lash::persistence::SessionRelation::Root,
        config: (&policy).into(),
        head: lash::persistence::SessionCreationHead::Config,
    }
}

/// Reads everything `/api/state` projects, without ever taking the session's
/// execution lease.
///
/// This is a probe: the page polls it about twice a second and writes nothing.
/// Opening the session to read it claimed the execution lease, so the poll
/// raced the running turn for the one thing a turn must hold — 242 contended
/// claims and 37 `retry_exhausted` refusals over nine turns in the judged
/// `workbench-continue-as` run, each exhaustion surfacing to the operator as a
/// 503 red banner. The Durable Session answers the same question from the
/// same records and contends with nothing (FIG-3144, ADR 0119).
pub(crate) async fn read_state_projection(
    state: &AppState,
    session_id: &SessionId,
) -> Result<StateProjectionReads, AppError> {
    let durable = state
        .session_builder(session_id.clone())
        .durable()
        .await
        .map_err(AppError::internal)?;
    // `exists` answers whether the catalog holds live metadata: an absent or
    // deleted id reads as an empty session, the shape this projection has
    // always handed the page for a session with nothing committed.
    let session_present = durable.exists().await.map_err(AppError::internal)?;
    let (read_view, has_durable_head, revision) =
        match durable.read().await.map_err(AppError::internal)? {
            Some(view) => {
                // The revision the snapshot's cursor names is the head's: the
                // head revision once a checkpoint exists, the turn index before
                // the first one — the same projection `observation_revision`
                // makes of a loaded runtime state, read here off the retained
                // revision record.
                let head = durable
                    .revisions()
                    .await
                    .map_err(AppError::internal)?
                    .into_iter()
                    .find(|revision| revision.head);
                let revision = head.map_or(view.turn_index() as u64, |head| {
                    if head.checkpoint_ref.is_some() {
                        head.head_revision
                    } else {
                        view.turn_index() as u64
                    }
                });
                (view, true, revision)
            }
            None => {
                // A session with no durable head yet, or one the catalog does not
                // hold: the same empty state `open_session_for_observation`
                // falls back to, so a page that polls before the first commit
                // sees what an observer would.
                let request = state_store_request(state, session_id);
                let mut persisted =
                    lash::persistence::RuntimeSessionState::new(request.config.session_policy());
                persisted.session_id = session_id.clone();
                (
                    lash::persistence::SessionReadView::from_persisted_state(&persisted),
                    false,
                    0,
                )
            }
        };
    // The cursor handed back with this snapshot has to name the replay
    // incarnation that will actually serve the attach. A synthesized
    // `workbench-durable` token names none, so every attach was fenced into
    // `replay_gap(unavailable)`, the page recovered from state, and the fresh
    // snapshot handed it another unservable cursor — a re-snapshot loop every
    // few seconds per open tab, on a perfectly healthy shell (FIG-3162).
    // `observation_cursor` reads the live-replay store and claims nothing, so
    // it pairs with the lease-free durable read above.
    let cursor = state
        .core
        .observation_cursor(session_id, lash::observe::SessionRevision(revision));
    let pending_turn_inputs = if session_present {
        durable
            .pending_turn_inputs()
            .await
            .map_err(AppError::internal)?
    } else {
        Vec::new()
    };
    let queued_work = if session_present {
        durable.queued_work().await.map_err(AppError::internal)?
    } else {
        Vec::new()
    };
    let turn_input_applications = if session_present {
        durable
            .remote_turn_input_applications()
            .await
            .map_err(AppError::internal)?
    } else {
        Vec::new()
    };
    let mut turn_failure_settlements = Vec::new();
    if session_present {
        let mut after = None;
        loop {
            let page = durable
                .failure_evidence(
                    after.as_ref(),
                    std::num::NonZeroU32::MIN.saturating_add(128 - 1),
                )
                .await
                .map_err(AppError::internal)?;
            turn_failure_settlements.extend(page.settlements);
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
    }
    Ok(StateProjectionReads {
        read_view,
        durable,
        has_durable_head,
        cursor,
        pending_turn_inputs,
        queued_work,
        turn_input_applications,
        turn_failure_settlements,
    })
}
