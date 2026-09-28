//! A turn whose commit is superseded mid-turn, on the Restate engine
//! (FIG-4010).
//!
//! Another writer moves the session head while a root's turn runs, so the
//! turn's commit is refused as superseded. The root's `LashTurn` retry would
//! replay the admission base its journal recorded and meet the same moved
//! head on every attempt, so the root ends in the attempt that met the
//! refusal, with `StoreCommitSuperseded` as its typed refusal. It is never
//! retried into a replay that re-decides at a recorded position (a journal
//! mismatch, Restate `RT0016`, or a park) and paused: the engine drains.
//!
//! The law runs on lash-restate's engine over the Restate server double, with
//! the session's store decorated so the forcing write lands through the store
//! the engine drives.

use super::*;
use lash_core::testing::runtime_helpers::{LayeredStores, RecordingSessionStoreFactory};

const SESSION: &str = "commit-superseded";
const TURN: &str = "superseded-turn";

/// A core over the double's engine, and the double it runs on.
struct Fixture {
    core: LashCore,
    double: lash_restate_test::RestateTestBackend,
    provider_calls: Arc<AtomicUsize>,
}

impl Fixture {
    /// The fixture whose provider, answering the first model call, commits
    /// the session head's next revision the way another writer would: the
    /// running turn's commit is then superseded.
    async fn head_moves_under_the_first_turn() -> Self {
        let catalog = Arc::new(std::sync::OnceLock::<Arc<RecordingSessionStoreFactory>>::new());
        let installed = Arc::clone(&catalog);
        let backend =
            double_backend_over(lash_restate_test::ServerConfig::default(), move |stores| {
                LayeredStores::over(stores)
                    .map_session_store_factory(|inner| {
                        let recording = Arc::new(RecordingSessionStoreFactory::over(inner));
                        let _ = installed.set(Arc::clone(&recording));
                        recording
                    })
                    .into_store_set()
            })
            .await;
        let double = latest_double().expect("the double the backend runs on");
        let provider_calls = Arc::new(AtomicUsize::new(0));
        let provider = {
            let provider_calls = Arc::clone(&provider_calls);
            crate::testing::TestProvider::builder()
                .kind("commit-superseded")
                .complete(move |_request| {
                    let provider_calls = Arc::clone(&provider_calls);
                    let catalog = Arc::clone(&catalog);
                    async move {
                        if provider_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                            let store = catalog
                                .get()
                                .and_then(|catalog| catalog.store_for(&SessionId::from(SESSION)))
                                .expect("the engine opened the session's store");
                            lash_core::testing::runtime_helpers::advance_session_head(
                                store.as_ref(),
                                &[],
                                |_| {},
                            )
                            .await;
                        }
                        Ok(text_response("answered"))
                    }
                })
                .build()
                .into_handle()
        };
        let core = explicit_ephemeral_facets(LashCore::standard_builder(
            backend,
            crate::TurnBudget::Unbounded,
        ))
        .provider(provider)
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())
        .expect("build the core");
        Self {
            core,
            double,
            provider_calls,
        }
    }

    /// Every `LashTurn` run of `turn` the double saw.
    fn turn_runs(&self, turn: &str) -> Vec<lash_restate_test::InvocationView> {
        let key = lash_restate::turn_workflow_key(
            &lash_core::SessionId::from(SESSION),
            &lash_core::TurnId::from(turn),
        );
        self.double
            .server()
            .invocations()
            .into_iter()
            .filter(|view| {
                view.target.starts_with("LashTurn") && view.target.ends_with(&format!("/{key}/run"))
            })
            .collect()
    }
}

/// The law: the superseded commit ends its root with the typed refusal in the
/// one attempt that met it. The root's run completes on its first attempt,
/// the send answers `StoreCommitSuperseded`, and every invocation the drive
/// made completes: nothing is left retrying or paused on the engine.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_superseded_turn_commit_ends_its_root_typed_and_never_pauses() -> Result<()> {
    let fixture = Fixture::head_moves_under_the_first_turn().await;
    let session = fixture.core.session(SESSION).open().await?;

    let superseded = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        session
            .send(TurnInput::text("the head moves under this turn's commit"))
            .id(TURN)
            .output(),
    )
    .await;
    let runs = fixture.turn_runs(TURN);
    let superseded = superseded
        .unwrap_or_else(|_| panic!("the superseded turn settles; its runs: {runs:#?}"))
        .expect_err("the superseded commit ends the turn with its refusal");
    let EmbedError::Runtime(refusal) = &superseded else {
        panic!("the refusal is the typed runtime error: {superseded:?}; runs: {runs:#?}");
    };
    assert_eq!(
        refusal.code,
        lash_core::RuntimeErrorCode::StoreCommitSuperseded,
        "the turn ends with the superseded commit: {refusal:?}; runs: {runs:#?}"
    );
    let [run] = runs.as_slice() else {
        panic!("the root ran in one invocation: {runs:#?}");
    };
    assert_eq!(run.status, "completed", "the root's run completed: {run:?}");
    assert_eq!(
        run.attempts, 1,
        "the superseded commit is never retried: {run:?}"
    );
    assert_eq!(
        fixture.provider_calls.load(Ordering::SeqCst),
        1,
        "the model is called once"
    );

    fixture.double.server().settle().await;
    let open = fixture
        .double
        .server()
        .invocations()
        .into_iter()
        .filter(|view| view.status != "completed")
        .collect::<Vec<_>>();
    assert!(
        open.is_empty(),
        "the engine drains: no invocation is left running, retrying or paused: {open:#?}"
    );
    Ok(())
}
