//! Acceptance-before-shift laws for direct turns (ADR 0069).
//!
//! Every turn enters through one durable acceptance commit and is then executed,
//! so these belong to the store contract rather than to one backend's tests: a
//! backend that admits a direct turn without recording it, or records it in a
//! shape its own drains cannot recover, has a different ingress from its
//! siblings.
//!
//! A host's turn is a send the engine executes; the in-process entry these laws
//! shift is the one a child session's turn takes inside its parent's
//! execution, through the testing door
//! [`TestTurnExecution::execute_child_session_turn`](crate::testing::TestTurnExecution::execute_child_session_turn).
//!
//! The suites run a real runtime turn over the supplied durable store and read
//! it back only through surfaces every backend already owes:
//! `list_pending_turn_inputs`, `list_turn_input_applications`, and
//! `cancel_pending_turn_input`.

use crate::admit;
use lash_core::testing::TestTurnExecution as _;
use lash_sansio::SessionId;
use lash_sansio::TurnId;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The session every conformance store in this suite is exercised under.
const SESSION_ID: &str = "root";

pub(super) fn text_response(text: &str) -> crate::LlmResponse {
    crate::LlmResponse {
        parts: vec![crate::LlmOutputPart::Text {
            text: text.to_string(),
            response_meta: None,
        }],
        response_metadata: Default::default(),
        ..crate::LlmResponse::default()
    }
}

pub(super) async fn acceptance_runtime(
    store: &Arc<dyn crate::RuntimeStore>,
    backend: &crate::Backend,
    provider: crate::ProviderHandle,
    plugin_factories: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
    lease_owner: crate::LeaseOwnerIdentity,
) -> crate::LashRuntime {
    acceptance_runtime_for_session(
        SESSION_ID,
        store,
        backend,
        provider,
        plugin_factories,
        lease_owner,
    )
    .await
}

/// [`acceptance_runtime`] over an explicit session id.
pub(super) async fn acceptance_runtime_for_session(
    session_id: &str,
    store: &Arc<dyn crate::RuntimeStore>,
    backend: &crate::Backend,
    provider: crate::ProviderHandle,
    plugin_factories: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
    lease_owner: crate::LeaseOwnerIdentity,
) -> crate::LashRuntime {
    acceptance_runtime_with_batching(
        session_id,
        store,
        backend,
        provider,
        plugin_factories,
        lease_owner,
        crate::QueuedWorkBatchingConfig::new(1),
    )
    .await
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn acceptance_runtime_with_batching(
    session_id: &str,
    store: &Arc<dyn crate::RuntimeStore>,
    backend: &crate::Backend,
    provider: crate::ProviderHandle,
    plugin_factories: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
    lease_owner: crate::LeaseOwnerIdentity,
    batching: crate::QueuedWorkBatchingConfig,
) -> crate::LashRuntime {
    let mut host = crate::RuntimeHostConfig::new(
        backend.clone(),
        crate::CommitBudget::bounded(1024 * 1024, 512),
        batching.clone(),
    );
    host.providers.models = crate::testing::standard_test_llm_profiles(provider);
    let policy = crate::testing::mock_session_policy();
    let state = crate::RuntimeSessionState {
        session_id: SessionId::fixture(session_id.to_string()),
        policy: policy.clone(),
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    Box::pin(
        crate::LashRuntime::builder(host, lease_owner)
            .with_session_id(SessionId::fixture(session_id))
            .with_policy(policy)
            .with_initial_state(state)
            .with_plugin_factories(
                crate::testing::test_standard_protocol_factories()
                    .into_iter()
                    .chain(plugin_factories)
                    .collect(),
            )
            .with_store(crate::conformance::helpers::session_view(
                store,
                SessionId::fixture(session_id.to_string()),
            ))
            .build(),
    )
    .await
    .expect("build the direct-turn acceptance conformance runtime")
}

pub(super) fn direct_input(turn_id: &TurnId, text: &str) -> crate::TurnInput {
    let mut input = crate::TurnInput::text(text);
    input.trace_turn_id = Some(turn_id.clone());
    input
}

/// A direct turn commits its input as admission evidence *before* it executes,
/// and settles that exact row when it commits.
///
/// The settlement is what a backend can be held to: the acceptance identity the
/// handle reports names a row that settles as this turn's canonical input, the
/// committed conversation attributes the model-visible message to that row, and
/// nothing is left pending. A backend that drove the caller's copy of the words
/// instead of the accepted row settles no application for it.
///
/// Mid-shift the session offers *no* open input, because the accepted row is
/// bound to this turn's own run. The ordinary pending listing still returns
/// that row with the factual `Admitted{run}` status naming the run.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn direct_turn_accepts_before_executing(
    prefix: &str,
    backend: crate::Backend,
    store: Arc<dyn crate::RuntimeStore>,
) {
    let turn_id = TurnId::fixture(format!("{prefix}-accept-before-shift"));
    let probe = Arc::new(std::sync::Mutex::new(None));
    let provider = {
        let store = Arc::clone(&store);
        let probe = Arc::clone(&probe);
        crate::testing::TestProvider::builder()
            .kind("stub")
            .complete(move |_| {
                let store = Arc::clone(&store);
                let probe = Arc::clone(&probe);
                async move {
                    // The turn is executing right now, so whatever this reads
                    // was already true before the shift began.
                    let pending = store
                        .list_pending_turn_inputs(&SessionId::from(SESSION_ID))
                        .await
                        .expect("read the session's pending inputs mid-shift");
                    *probe.lock().expect("probe lock") = Some(pending);
                    Ok(text_response("accepted"))
                }
            })
            .build()
            .into_handle()
    };
    let effect_host: Arc<dyn crate::EffectHost> = backend.effect_host();
    let mut runtime = acceptance_runtime(
        &store,
        &backend,
        provider,
        Vec::new(),
        crate::testing::runtime_lease_owner(),
    )
    .await;
    let scope = effect_host
        .scoped(admit(crate::ExecutionScope::turn(SESSION_ID, &turn_id)))
        .expect("scope the direct acceptance turn");
    let turn = runtime
        .execute_child_session_turn(
            direct_input(&turn_id, "direct turn under durable acceptance"),
            crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
        )
        .await
        .expect("run the direct acceptance conformance turn");

    let pending = probe
        .lock()
        .expect("probe lock")
        .clone()
        .expect("the provider must have run");
    assert_eq!(pending.len(), 1, "the admitted input must remain visible");
    let held = &pending[0];
    assert_eq!(
        held.status,
        crate::PendingTurnInputReadStatus::Admitted {
            run: turn_id.clone()
        },
        "the admitted marker must name the run executing it"
    );
    assert_eq!(
        held.input.state,
        crate::TurnInputState::DeferredNextTurn,
        "admitted is a read status, not a persisted TurnInputState"
    );

    let acceptance = turn
        .turn_input_acceptance
        .as_ref()
        .expect("a store-backed direct turn exposes its acceptance identity");
    let input_id = acceptance.input_id.clone();
    assert_eq!(
        held.input.input_id, input_id,
        "the held projection must name the acceptance this turn is executing"
    );
    assert_eq!(acceptance.session_id, SESSION_ID);
    assert_eq!(
        acceptance.source_key.as_deref(),
        Some(turn_id.as_str()),
        "direct ingress names its row by the turn id, the run the shift runs it under"
    );
    assert_eq!(acceptance.ingress, crate::TurnInputIngress::next_turn());

    let application = store
        .list_turn_input_applications(&SessionId::from(SESSION_ID))
        .await
        .expect("read settled applications")
        .into_iter()
        .find(|application| application.input_id == input_id)
        .expect("the accepted row settles as canonical conversation input");
    assert_eq!(application.turn_id.as_str(), turn_id);
    assert!(
        store
            .list_pending_turn_inputs(&SessionId::from(SESSION_ID))
            .await
            .expect("read pending inputs")
            .iter()
            .all(|pending| pending.input.input_id != input_id),
        "a committed turn leaves no pending acceptance behind"
    );

    // The model-visible message the turn ran on is attributed to the accepted
    // row, so the shift consumed the acceptance rather than the caller's copy
    // of the same words.
    assert!(
        turn.state
            .read_view()
            .messages()
            .iter()
            .any(|message| matches!(
                &message.origin,
                Some(crate::MessageOrigin::TurnInput { input_id: Some(id), .. }) if *id == input_id
            )),
        "the committed conversation must attribute its user input to the accepted row"
    );
}

/// Direct ingress identity is the turn id: two direct turns carrying the same
/// content under two turn ids are two admissions, each keyed by its own turn
/// id (the run the shift runs it under), never by its content.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn direct_turn_acceptance_mints_no_idempotency_key(
    prefix: &str,
    backend: crate::Backend,
    store: Arc<dyn crate::RuntimeStore>,
) {
    let seen = Arc::new(AtomicUsize::new(0));
    let provider = {
        let seen = Arc::clone(&seen);
        crate::testing::TestProvider::builder()
            .kind("stub")
            .complete(move |_| {
                let seen = Arc::clone(&seen);
                async move {
                    seen.fetch_add(1, Ordering::SeqCst);
                    Ok(text_response("ok"))
                }
            })
            .build()
            .into_handle()
    };
    let effect_host: Arc<dyn crate::EffectHost> = backend.effect_host();
    let mut runtime = acceptance_runtime(
        &store,
        &backend,
        provider,
        Vec::new(),
        crate::testing::runtime_lease_owner(),
    )
    .await;
    let mut acceptances = Vec::new();
    for round in 0..2 {
        let turn_id = TurnId::fixture(format!("{prefix}-resubmit-{round}"));
        let scope = effect_host
            .scoped(admit(crate::ExecutionScope::turn(SESSION_ID, &turn_id)))
            .expect("scope a resubmitted direct turn");
        let turn = runtime
            .execute_child_session_turn(
                direct_input(&turn_id, "the very same words"),
                crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
            )
            .await
            .expect("run a resubmitted direct turn");
        acceptances.push(
            turn.turn_input_acceptance
                .expect("a store-backed direct turn exposes its acceptance identity"),
        );
    }
    assert_eq!(seen.load(Ordering::SeqCst), 2, "both submissions execute");
    assert_ne!(
        acceptances[0].input_id, acceptances[1].input_id,
        "identical content is two admissions, not one deduplicated retry"
    );
    assert_eq!(
        acceptances
            .iter()
            .map(|acceptance| acceptance.source_key.clone())
            .collect::<Vec<_>>(),
        (0..2)
            .map(|round| Some(format!("{prefix}-resubmit-{round}")))
            .collect::<Vec<_>>(),
        "direct ingress keys each row by its turn id and by nothing else"
    );
}

// ---------------------------------------------------------------------------
// Journaled initial shift set (ADR 0069 §6, FIG-3532)
// ---------------------------------------------------------------------------

/// A journal-owning layer over the in-process effect host: the first
/// execution of an effect runs through the host and records its outcome under
/// the effect's replay key, and every later execution of the same key returns
/// the recorded outcome without running anything — what a durable engine does
/// on replay.
///
/// `crash_at` simulates a worker dying at an effect: the next effect of that
/// kind fails before it runs and is never journaled, so the redrive executes
/// it for real.
#[derive(Default)]
pub(super) struct JournalLayer {
    outcomes: std::sync::Mutex<std::collections::HashMap<String, crate::RuntimeEffectOutcome>>,
    lose_outcome_at: std::sync::Mutex<Option<crate::RuntimeEffectKind>>,
}

impl JournalLayer {
    /// The next effect of `kind` runs to completion, and then the worker dies
    /// before its outcome is recorded, so the redrive runs its body again.
    #[expect(clippy::expect_used, reason = "conformance fixture lock")]
    pub(super) fn lose_outcome_at_next(&self, kind: crate::RuntimeEffectKind) {
        *self.lose_outcome_at.lock().expect("lose-outcome lock") = Some(kind);
    }
}

#[async_trait::async_trait]
impl crate::testing::EffectLayer for JournalLayer {
    #[expect(clippy::expect_used, reason = "conformance fixture lock")]
    async fn execute_effect(
        &self,
        inner: &dyn crate::RuntimeEffectController,
        envelope: crate::RuntimeEffectEnvelope,
        local_executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        // Keyed by replay key, the address a durable engine journals under:
        // effect ids alone repeat across turns.
        let effect_id = envelope.invocation.effect_replay_key().to_string();
        if let Some(outcome) = self.outcomes.lock().expect("journal lock").get(&effect_id) {
            return Ok(outcome.clone());
        }
        let kind = envelope.command.kind();

        let outcome = inner.execute_effect(envelope, local_executor).await?;
        {
            let mut lose_outcome_at = self.lose_outcome_at.lock().expect("lose-outcome lock");
            if *lose_outcome_at == Some(kind) {
                *lose_outcome_at = None;
                return Err(crate::RuntimeEffectControllerError::foreign(
                    "conformance_worker_crash",
                    crate::TurnFailureCause::LiveFault,
                    format!(
                        "the worker died after the {} effect ran, before its outcome was recorded",
                        kind.as_str()
                    ),
                ));
            }
        }
        self.outcomes
            .lock()
            .expect("journal lock")
            .insert(effect_id, outcome.clone());
        Ok(outcome)
    }
}

/// A replaying worker resumes from the invocation's pre-commit resident state
/// while the store may already hold the first execution's commit, so the
/// redrive runtime sees no persisted session. It also counts every read a shift
/// could make of pending rows, so a replay can prove it made none.
struct RedriveStore {
    inner: Arc<dyn crate::RuntimeStore>,
    pending_row_reads: Arc<AtomicUsize>,
}

impl RedriveStore {
    fn wrap(inner: &Arc<dyn crate::RuntimeStore>) -> (Arc<Self>, Arc<AtomicUsize>) {
        let reads = Arc::new(AtomicUsize::new(0));
        (
            Arc::new(Self {
                inner: Arc::clone(inner),
                pending_row_reads: Arc::clone(&reads),
            }),
            reads,
        )
    }
}

#[async_trait::async_trait]
impl crate::store::RuntimeStoreDecorator for RedriveStore {
    type Inner = dyn crate::RuntimeStore;

    fn inner(&self) -> &Self::Inner {
        self.inner.as_ref()
    }

    async fn admit_run(
        &self,
        request: &crate::store::AdmitRunRequest,
    ) -> Result<Option<crate::store::RunAdmission>, crate::StoreError> {
        self.pending_row_reads.fetch_add(1, Ordering::SeqCst);
        self.inner.admit_run(request).await
    }

    async fn load_session_window(
        &self,
        session_id: &SessionId,
        selector: crate::store::WindowSelector,
    ) -> Result<Option<crate::store::SessionWindowRead>, crate::StoreError> {
        match selector {
            crate::store::WindowSelector::Current => Ok(None),
            admitted @ crate::store::WindowSelector::Admitted(_) => {
                self.inner.load_session_window(session_id, admitted).await
            }
        }
    }

    async fn list_pending_turn_inputs(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<crate::PendingTurnInputRead>, crate::StoreError> {
        self.pending_row_reads.fetch_add(1, Ordering::SeqCst);
        self.inner.list_pending_turn_inputs(session_id).await
    }

    async fn pending_turn_input(
        &self,
        session_id: &SessionId,
        input_id: &crate::InputId,
    ) -> Result<Option<crate::PendingTurnInputRead>, crate::StoreError> {
        self.pending_row_reads.fetch_add(1, Ordering::SeqCst);
        self.inner.pending_turn_input(session_id, input_id).await
    }
}

/// One journal and one effect host shared by a first execution and its
/// redrive, the way a durable engine's handler keeps its journal across
/// worker incarnations.
pub(super) struct Journal {
    pub(super) controller: Arc<JournalLayer>,
    pub(super) effect_host: Arc<dyn crate::EffectHost>,
    /// `backend` with [`Self::effect_host`] in place of its own host.
    pub(super) backend: crate::Backend,
    batching: crate::QueuedWorkBatchingConfig,
}

impl Journal {
    /// The journal layered over `backend`'s effect host.
    pub(super) fn new(backend: &crate::Backend) -> Self {
        let controller = Arc::new(JournalLayer::default());
        let effect_host: Arc<dyn crate::EffectHost> =
            Arc::new(crate::testing::LayeredEffectHost::new(
                backend.effect_host(),
                Arc::clone(&controller) as Arc<dyn crate::testing::EffectLayer>,
            ));
        let law_backend = crate::LawBackend::over(backend)
            .with_effect_host(Arc::clone(&effect_host))
            .into_backend();
        Self {
            controller,
            effect_host,
            backend: law_backend,
            batching: crate::QueuedWorkBatchingConfig::new(1),
        }
    }

    /// Bound every admission this journal's runtimes take to `max_inputs` rows.
    fn with_turn_input_admission(mut self, max_inputs: usize) -> Self {
        self.batching = self.batching.with_max_turn_input_admission(max_inputs);
        self
    }

    /// Take every eligible input, up to the admission bound, into one run
    /// (`DrainMode::All`): the default drain gives each input its own run
    /// (FIG-4457).
    fn composing(mut self) -> Self {
        self.batching = self.batching.with_drain_mode(crate::DrainMode::All);
        self
    }

    /// Run the direct turn `turn_id` against `store` on a fresh runtime.
    pub(super) async fn run(
        &self,
        store: &Arc<dyn crate::RuntimeStore>,
        provider: crate::ProviderHandle,
        turn_id: &TurnId,
        text: &str,
    ) -> Result<crate::AssembledTurn, crate::RuntimeError> {
        self.run_with_plugins(store, provider, Vec::new(), turn_id, text)
            .await
    }

    /// [`Self::run`] with extra plugins on the runtime.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    pub(super) async fn run_with_plugins(
        &self,
        store: &Arc<dyn crate::RuntimeStore>,
        provider: crate::ProviderHandle,
        plugin_factories: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
        turn_id: &TurnId,
        text: &str,
    ) -> Result<crate::AssembledTurn, crate::RuntimeError> {
        let mut runtime = acceptance_runtime_with_batching(
            SESSION_ID,
            store,
            &self.backend,
            provider,
            plugin_factories,
            crate::testing::runtime_lease_owner(),
            self.batching.clone(),
        )
        .await;
        let scope = self
            .effect_host
            .scoped(admit(crate::ExecutionScope::turn(SESSION_ID, turn_id)))
            .expect("scope the journaled direct turn");
        runtime
            .execute_child_session_turn(
                direct_input(turn_id, text),
                crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
            )
            .await
    }
}

/// A provider that records the text of every request it answers.
pub(super) fn recording_provider(
    answer: &str,
) -> (crate::ProviderHandle, Arc<std::sync::Mutex<Vec<String>>>) {
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let answer = answer.to_string();
    let provider = {
        let requests = Arc::clone(&requests);
        crate::testing::TestProvider::builder()
            .kind("stub")
            .complete(move |request| {
                let requests = Arc::clone(&requests);
                let answer = answer.clone();
                async move {
                    let text = request
                        .messages
                        .iter()
                        .flat_map(|message| message.blocks.iter())
                        .filter_map(|block| match block {
                            crate::llm::types::LlmContentBlock::Text { text, .. } => {
                                Some(text.clone())
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    #[expect(clippy::expect_used, reason = "conformance fixture lock")]
                    requests.lock().expect("request lock").push(text);
                    Ok(text_response(&answer))
                }
            })
            .build()
            .into_handle()
    };
    (provider, requests)
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn vacuum(store: &Arc<dyn crate::RuntimeStore>, session_id: &SessionId) {
    crate::store::StoreMaintenance::vacuum(store.as_ref(), session_id)
        .await
        .expect("vacuum the session's terminal rows");
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn pending_input_ids(store: &Arc<dyn crate::RuntimeStore>) -> Vec<crate::InputId> {
    store
        .list_pending_turn_inputs(&SessionId::from(SESSION_ID))
        .await
        .expect("read pending inputs")
        .into_iter()
        .map(|read| read.input.input_id)
        .collect()
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn applications(
    store: &Arc<dyn crate::RuntimeStore>,
) -> Vec<crate::TurnInputApplication> {
    store
        .list_turn_input_applications(&SessionId::from(SESSION_ID))
        .await
        .expect("read settled applications")
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn enqueue_next_turn(
    store: &Arc<dyn crate::RuntimeStore>,
    text: &str,
) -> crate::PendingTurnInput {
    store
        .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
            SESSION_ID,
            crate::TurnInputIngress::next_turn(),
            crate::TurnInput::text(text),
        ))
        .await
        .expect("enqueue a next-turn input")
}

/// A drain after a replayed commit finds nothing: the redrive left no row open
/// for a second turn to answer.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn assert_nothing_left_to_answer(
    prefix: &str,
    store: &Arc<dyn crate::RuntimeStore>,
    journal: &Journal,
) {
    let answered = Arc::new(AtomicUsize::new(0));
    let provider = {
        let answered = Arc::clone(&answered);
        crate::testing::TestProvider::builder()
            .kind("stub")
            .complete(move |_| {
                let answered = Arc::clone(&answered);
                async move {
                    answered.fetch_add(1, Ordering::SeqCst);
                    Ok(text_response("a second answer"))
                }
            })
            .build()
            .into_handle()
    };
    let mut drainer = acceptance_runtime(
        store,
        &journal.backend,
        provider,
        Vec::new(),
        crate::testing::runtime_lease_owner(),
    )
    .await;
    let drain_id = format!("{prefix}-after-redrive-drain");
    let scope = journal
        .effect_host
        .scoped(admit(crate::ExecutionScope::turn(
            SESSION_ID,
            TurnId::fixture(&drain_id),
        )))
        .expect("scope the post-redrive drain");
    let drain = drainer
        .execute_one_admitted_queued_run(crate::TurnOptions::new(
            tokio_util::sync::CancellationToken::new(),
            scope,
        ))
        .await
        .expect("the post-redrive drain runs");
    assert!(
        matches!(drain, crate::QueuedTurnDrain::Empty(_)),
        "a replayed commit must leave nothing for a second turn to answer"
    );
    assert_eq!(answered.load(Ordering::SeqCst), 0);
}

/// A committed direct turn whose handler died before it was acknowledged is
/// redriven after `vacuum()` pruned its completed row. The redrive executes the
/// journaled shift set, finds the first commit's receipt, and replays it: no
/// row is re-admitted and nothing is answered twice.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn vacuum_then_redrive_replays_receipt_single_row(
    prefix: &str,
    backend: crate::Backend,
    store: Arc<dyn crate::RuntimeStore>,
) {
    let turn_id = TurnId::fixture(format!("{prefix}-vacuum-redrive-single"));
    let journal = Journal::new(&backend);
    let (provider, requests) = recording_provider("deployed staging");
    let first = journal
        .run(&store, provider.clone(), &turn_id, "deploy staging")
        .await
        .expect("the first execution commits");
    let acceptance = first
        .turn_input_acceptance
        .clone()
        .expect("a store-backed direct turn exposes its acceptance");
    let committed = applications(&store).await;
    assert_eq!(committed.len(), 1);

    vacuum(&store, &SessionId::from(SESSION_ID)).await;
    let (redrive_store, _) = RedriveStore::wrap(&store);
    let redrive_store: Arc<dyn crate::RuntimeStore> = redrive_store;
    let replayed = journal
        .run(&redrive_store, provider, &turn_id, "deploy staging")
        .await
        .expect("the redrive replays the original commit's receipt");

    assert_eq!(
        replayed.turn_input_acceptance.as_ref(),
        Some(&acceptance),
        "the redrive keeps the journaled acceptance identity"
    );
    assert_eq!(
        requests.lock().expect("request lock").len(),
        1,
        "the provider answered the input once"
    );
    assert!(
        pending_input_ids(&store).await.is_empty(),
        "the redrive re-admitted nothing"
    );
    assert_eq!(
        applications(&store).await,
        committed,
        "the receipt replay writes no second application"
    );
    Box::pin(assert_nothing_left_to_answer(prefix, &store, &journal)).await;
}

/// A direct turn whose accepted input sits behind earlier admissions is executed
/// after them (FIG-3600): the shift admits each earlier run first, in arrival
/// order, every run's admission takes the admissible prefix up to the admission bound,
/// and the call returns the execution of the run that drove its input. Every input
/// is answered once, nothing is dropped, and nothing waits for a later drain.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn direct_turn_behind_earlier_admissions_runs_after_them(
    prefix: &str,
    backend: crate::Backend,
    store: Arc<dyn crate::RuntimeStore>,
) {
    let turn_id = TurnId::fixture(format!("{prefix}-queued-direct-turn"));
    let first = enqueue_next_turn(&store, "earliest admission").await;
    let second = enqueue_next_turn(&store, "second admission").await;
    let journal = Journal::new(&backend)
        .with_turn_input_admission(2)
        .composing();
    let (provider, requests) = recording_provider("answered in order");

    let turn = journal
        .run(&store, provider.clone(), &turn_id, "the direct input")
        .await
        .expect("a direct turn behind earlier admissions runs after them");
    assert!(
        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
        "the call answers with its own turn: {:?}",
        turn.outcome
    );
    let input_id = turn
        .turn_input_acceptance
        .as_ref()
        .expect("the call reports its acceptance")
        .input_id
        .clone();

    let answered = applications(&store).await;
    assert_eq!(
        answered
            .iter()
            .map(|application| application.input_id.clone())
            .collect::<Vec<_>>(),
        vec![first.input_id, second.input_id, input_id.clone()],
        "the shift answers every input once, in arrival order"
    );
    let direct_answer = answered
        .iter()
        .find(|application| application.input_id == input_id)
        .expect("the direct input is answered");
    assert_eq!(
        direct_answer.turn_id.as_str(),
        turn_id.as_str(),
        "the direct input is answered by its own run"
    );
    assert!(pending_input_ids(&store).await.is_empty());
    let requests = requests.lock().expect("request lock").clone();
    assert_eq!(
        requests.len(),
        2,
        "one run for the two earlier inputs under the admission bound, then the direct one"
    );
    assert!(
        requests
            .last()
            .is_some_and(|last| last.contains("the direct input")),
        "the direct input is answered last: {requests:?}"
    );
}

/// An acceptance whose body ran, committed its row, and then lost its outcome
/// (the worker died before the journal recorded it) is re-run by the redrive.
/// The re-run names the same provisioned input id, so the store adopts the row
/// the first run wrote: one admission, one pending row, and one copy of the
/// words in the committed turn.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn accept_turn_input_redrive_after_store_commit_admits_one_row(
    prefix: &str,
    backend: crate::Backend,
    store: Arc<dyn crate::RuntimeStore>,
) {
    let turn_id = TurnId::fixture(format!("{prefix}-acceptance-lost-outcome"));
    let journal = Journal::new(&backend);
    journal
        .controller
        .lose_outcome_at_next(crate::RuntimeEffectKind::AcceptTurnInput);
    let (provider, requests) = recording_provider("answered once");
    journal
        .run(&store, provider.clone(), &turn_id, "deploy staging once")
        .await
        .expect_err("the worker dies before the acceptance outcome is recorded");
    let admitted = pending_input_ids(&store).await;
    assert_eq!(admitted.len(), 1, "the first run committed its row");

    // The lost-outcome error returns with the crashed worker's lane still
    // held: only the dropped guard's spawned best-effort release frees it, so
    // the redrive's admission can observe the abandoned lease and refuse
    // with `SessionExecutionLaneBusy`. Expire it first.

    let redriven = journal
        .run(&store, provider, &turn_id, "deploy staging once")
        .await
        .expect("the redrive re-runs the acceptance body and commits the turn");
    let acceptance = redriven
        .turn_input_acceptance
        .clone()
        .expect("a store-backed direct turn exposes its acceptance");
    assert_eq!(
        acceptance.input_id, admitted[0],
        "the re-run returns the input id the first run admitted"
    );
    assert!(
        pending_input_ids(&store).await.is_empty(),
        "no second row was admitted"
    );
    let applied = applications(&store).await;
    assert_eq!(
        applied
            .iter()
            .map(|application| application.input_id.clone())
            .collect::<Vec<_>>(),
        vec![acceptance.input_id.clone()],
        "the turn applied exactly one admission"
    );
    let copies = redriven
        .state
        .read_view()
        .messages()
        .iter()
        .filter(|message| {
            matches!(message.role, crate::MessageRole::User)
                && message.parts.iter().any(|part| {
                    matches!(part, crate::Part::Text { content, .. } if content.contains("deploy staging once"))
                })
        })
        .count();
    assert_eq!(copies, 1, "the committed turn carries the words once");
    let requests = requests.lock().expect("request lock").clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].matches("deploy staging once").count(),
        1,
        "the model saw the words once: {requests:?}"
    );
}
