//! FIG-3682: a direct turn redriven after its own commit replays at the head
//! it was admitted on.
//!
//! A worker can die after a turn's final commit landed and before the turn's
//! handler returned. The tier then redelivers the turn, and the redrive
//! replays its journal. By then the live head already holds the turn's own
//! commit. A redrive that took its turn index or its input state from that
//! head would address the turn's effects under the next index and build its
//! model request over history that includes the turn's own answer, so it
//! would issue other effects than the journal holds. The turn's admission
//! records the head it was admitted on and the turn index, and the redrive
//! rebuilds the turn from that record.
//!
//! The law runs two turns on one session, each crashed after its commit and
//! redriven: the first is admitted before the session's first commit, the
//! second on the first turn's committed head. Each redrive finishes with the
//! answer its first execution committed, asks the model nothing, commits
//! nothing again and keeps its admitted turn index.
//!
//! The protocol's code executor carries state into every commit, derived
//! from the state it was last restored with. A redrive opens on the live
//! head, which already holds the turn's own execution state, so it matches
//! its first execution's commit only when adopting the admitted head also
//! restores the executor at that head (FIG-3684).

use crate::admit;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_sansio::{SessionId, TurnId};
use pretty_assertions::assert_eq;

/// Panics as the committed turn's delivery begins: the turn's final commit
/// is durable and its handler has not returned.
struct PanicAfterTurnCommit;

impl lash_core::runtime::RuntimeTurnPhaseProbe for PanicAfterTurnCommit {
    fn begin(&self, phase: lash_core::runtime::RuntimeTurnPhase) {
        if phase == lash_core::runtime::RuntimeTurnPhase::PostCommitDelivery {
            panic!("injected crash after the turn commit and before its delivery");
        }
    }

    fn end(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}

    fn begin_named(&self, _phase: &str) {}
}

/// A protocol whose code executor's state is a counter: restoring an
/// execution state adopts its counter, and every capture commits the counter
/// plus one. A turn's committed execution state therefore names the state its
/// executor was restored at, the way a real executor's heap does.
#[derive(Default)]
struct CountingExecutionProtocol {
    restored: std::sync::atomic::AtomicU64,
}

impl CountingExecutionProtocol {
    fn root(value: u64) -> Arc<[u8]> {
        value.to_string().into_bytes().into()
    }

    fn adopt(&self, state: Option<&lash_core::plugin::HydratedExecutionState>) {
        let restored = state.map_or(0, |state| {
            std::str::from_utf8(&state.root)
                .ok()
                .and_then(|root| root.parse().ok())
                .unwrap_or_else(|| panic!("unexpected execution-state root {:?}", state.root))
        });
        self.restored.store(restored, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl lash_core::plugin::ProtocolSessionPlugin for CountingExecutionProtocol {
    async fn restore_session(
        &self,
        _ctx: lash_core::plugin::ProtocolSessionContext<'_>,
        state: lash_core::plugin::ProtocolSessionRestoreView,
    ) -> Result<(), crate::SessionError> {
        let state = state
            .execution_state
            .map_err(|source| crate::SessionError::Store {
                context: "hydrate the counting executor's state".to_string(),
                source,
            })?;
        self.adopt(state.as_ref());
        Ok(())
    }
}

#[async_trait::async_trait]
impl lash_core::plugin::CodeExecutorPlugin for CountingExecutionProtocol {
    async fn execute_code(
        &self,
        _ctx: crate::RuntimeExecutionContext<'_>,
        _request: crate::ExecRequest,
    ) -> Result<crate::ExecResponse, crate::SessionError> {
        Err(crate::SessionError::Protocol(
            "the admitted-head law's turns run no code".to_string(),
        ))
    }

    fn execution_state_dirty(&self) -> bool {
        true
    }

    async fn snapshot_execution_state(
        &self,
        _ctx: lash_core::plugin::ProtocolSessionContext<'_>,
    ) -> Result<lash_core::plugin::ExecutionStateSnapshot, crate::SessionError> {
        Ok(lash_core::plugin::ExecutionStateSnapshot::from_root(Some(
            Self::root(self.restored.load(Ordering::SeqCst) + 1),
        )))
    }

    async fn hydrated_execution_state(
        &self,
        _ctx: lash_core::plugin::ProtocolSessionContext<'_>,
    ) -> Result<Option<lash_core::plugin::HydratedExecutionState>, crate::SessionError> {
        Ok(Some(lash_core::plugin::HydratedExecutionState {
            root: Self::root(self.restored.load(Ordering::SeqCst)),
            components: std::collections::BTreeMap::new(),
        }))
    }

    async fn restore_execution_state(
        &self,
        _ctx: lash_core::plugin::ProtocolSessionContext<'_>,
        state: &lash_core::plugin::HydratedExecutionState,
    ) -> Result<(), crate::SessionError> {
        self.adopt(Some(state));
        Ok(())
    }
}

/// Everything a runtime for this law is built from, shared by every attempt
/// so each is the same session on the same store.
#[derive(Clone)]
struct RedriveParts {
    session_id: SessionId,
    host: crate::RuntimeHostConfig,
    store: Arc<dyn crate::RuntimePersistence>,
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn build_runtime(parts: RedriveParts) -> crate::LashRuntime {
    let mut policy = crate::testing::mock_session_policy();
    policy.session_id = Some(parts.session_id.clone());
    let state = crate::RuntimeSessionState {
        session_id: parts.session_id.clone(),
        policy: policy.clone(),
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    // Each attempt is a fresh process: its executor starts empty.
    let protocol = Arc::new(CountingExecutionProtocol::default());
    Box::pin(
        crate::LashRuntime::builder(parts.host, crate::testing::runtime_lease_owner())
            .with_session_id(&parts.session_id)
            .with_policy(policy)
            .with_initial_state(state)
            .with_plugin_factories(vec![
                crate::testing::test_standard_protocol_factory_with_runtime_state(
                    Arc::clone(&protocol) as Arc<dyn lash_core::plugin::ProtocolSessionPlugin>,
                    Some(protocol as Arc<dyn lash_core::plugin::CodeExecutorPlugin>),
                ),
            ])
            .with_store(parts.store)
            .with_queued_work(Arc::new(crate::NoSessionWork::new()))
            .build(),
    )
    .await
    .expect("build the admitted-head redrive conformance runtime")
}

fn redrive_input(turn_id: &TurnId, text: &str) -> crate::TurnInput {
    let mut input = crate::TurnInput::text(text);
    input.trace_turn_id = Some(turn_id.clone());
    input
}

type TurnResultTx =
    tokio::sync::mpsc::UnboundedSender<Result<crate::AssembledTurn, crate::RuntimeError>>;

/// One attempt at `turn_id`: the crashing one panics after its commit; the
/// redrive sends back what its turn returned.
fn attempt(
    parts: &RedriveParts,
    turn_id: &TurnId,
    text: &'static str,
    result_tx: Option<TurnResultTx>,
) -> crate::ConformanceTurnAttempt {
    let parts = parts.clone();
    let turn_id = turn_id.clone();
    Arc::new(move |scope| {
        let parts = parts.clone();
        let turn_id = turn_id.clone();
        let result_tx = result_tx.clone();
        Box::pin(async move {
            let mut runtime = build_runtime(parts).await;
            if result_tx.is_none() {
                runtime.set_turn_phase_probe(Arc::new(PanicAfterTurnCommit));
            }
            let turn = runtime
                .stream_turn(
                    redrive_input(&turn_id, text),
                    crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
                )
                .await;
            let Some(result_tx) = result_tx else {
                panic!("the crash probe did not fire after the turn commit: {turn:?}");
            };
            let end = crate::ConformanceTurnEnd::of(&turn);
            let _ = result_tx.send(turn);
            end
        })
    })
}

/// A direct turn crashed after its commit and redriven replays at the head it
/// was admitted on: the same answer under the same turn index, with no model
/// call and no second commit.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_turn_redriven_after_its_commit_replays_at_its_admitted_head(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let session_id = SessionId::from(format!("{prefix}-admitted-head-session"));
    let calls = Arc::new(AtomicUsize::new(0));
    let model = crate::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let calls = Arc::clone(&calls);
            move |_request| {
                let index = calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    Ok(crate::LlmResponse {
                        parts: vec![crate::LlmOutputPart::Text {
                            text: format!("answer {}", index + 1),
                            response_meta: None,
                        }],
                        ..crate::LlmResponse::default()
                    })
                }
            }
        })
        .build();
    let mut host = crate::LawBackend::over_stores(Arc::clone(&stores), Arc::clone(&effect_host))
        .host_config(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        );
    host.providers.provider_resolver =
        Arc::new(crate::SingleProviderResolver::new(model.into_handle()));
    let store = crate::conformance::law_session_store(stores.as_ref(), &session_id).await;
    let parts = RedriveParts {
        session_id: session_id.clone(),
        host,
        store: Arc::clone(&store),
    };
    let (result_tx, mut result_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut revision = store
        .load_session_head_meta()
        .await
        .expect("read the session head")
        .map_or(0, |head| head.head_revision);

    // The first turn is admitted before the session's first commit; the
    // second on the first turn's committed head.
    for (ordinal, text) in [(1_usize, "first question"), (2, "second question")] {
        let turn_id = TurnId::from(format!("{prefix}-admitted-head-turn-{ordinal}"));
        runner
            .run_crashed_then_redriven_turn(
                admit(crate::ExecutionScope::turn(&session_id, &turn_id)),
                attempt(&parts, &turn_id, text, None),
                attempt(&parts, &turn_id, text, Some(result_tx.clone())),
            )
            .await;
        let turn = result_rx
            .recv()
            .await
            .expect("the tier's runner ran the redriven turn")
            .unwrap_or_else(|error| {
                panic!("turn {ordinal}: the redrive after the commit replays the turn: {error:?}")
            });
        assert!(
            matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
            "turn {ordinal}: redriven outcome: {:?}; errors: {:?}",
            turn.outcome,
            turn.errors
        );
        assert_eq!(
            turn.assistant_output.safe_text,
            format!("answer {ordinal}"),
            "turn {ordinal}: the redrive answers with what its first execution committed"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            ordinal,
            "turn {ordinal}: the redrive reads the model's answer back instead of asking again"
        );
        assert_eq!(
            turn.state.turn_index, ordinal,
            "turn {ordinal}: the redrive keeps the turn index its admission recorded"
        );
        let committed = crate::load_persisted_session_state(store.as_ref())
            .await
            .expect("read the committed head")
            .expect("the turn's commit is durable");
        assert_eq!(
            committed.turn_index, ordinal,
            "turn {ordinal}: the head holds the turn once, at its admitted index"
        );
        assert_eq!(
            committed.head_revision,
            revision + 1,
            "turn {ordinal}: the turn committed once and the redrive commits nothing again"
        );
        let execution_state = committed
            .execution_state_hydration()
            .expect("hydrate the committed execution state")
            .expect("every turn commits the executor's state");
        assert_eq!(
            &*execution_state.root,
            ordinal.to_string().as_bytes(),
            "turn {ordinal}: the executor ran the turn over its admitted head's state"
        );
        revision = committed.head_revision;
    }
}
