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
const FIRST_TURN: &str = "first-turn";
const NEXT_TURN: &str = "next-turn";

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
        Self::head_moves_under_model_call(0).await
    }

    /// The fixture whose provider moves the head while it answers model
    /// call `moving` (0-based), as another writer would.
    async fn head_moves_under_model_call(moving: usize) -> Self {
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
                        if provider_calls.fetch_add(1, Ordering::SeqCst) == moving {
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

    /// The invocations still open once the engine has settled: a drive a
    /// late ask started (a relay's, or the one queued behind the running
    /// drive) is given until a deadline to finish, so only work that never
    /// finishes is left.
    async fn open_after_settling(&self) -> Vec<lash_restate_test::InvocationView> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            self.double.server().settle().await;
            let open = self
                .double
                .server()
                .invocations()
                .into_iter()
                .filter(|view| view.status != "completed")
                .collect::<Vec<_>>();
            if open.is_empty() || tokio::time::Instant::now() >= deadline {
                return open;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
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

/// FIG-4018: a root that ends with a typed refusal is not left the session's
/// unfinished root. Its end is written to the store, so the session's next
/// send is admitted under a new root and completes, rather than re-admitting
/// the refused root and answering its old refusal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_send_after_a_refused_root_drives_a_new_root() -> Result<()> {
    // The head moves under the second turn, so the first commits the head
    // the session's later turns run on.
    let fixture = Fixture::head_moves_under_model_call(1).await;
    let session = fixture.core.session(SESSION).open().await?;
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        session
            .send(TurnInput::text("the session's first turn"))
            .id(FIRST_TURN)
            .output(),
    )
    .await
    .expect("the first turn settles")?;

    let refused = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        session
            .send(TurnInput::text("the head moves under this turn's commit"))
            .id(TURN)
            .output(),
    )
    .await
    .expect("the refused turn settles")
    .expect_err("the superseded commit ends the turn with its refusal");
    assert!(
        matches!(&refused, EmbedError::Runtime(error)
            if error.code == lash_core::RuntimeErrorCode::StoreCommitSuperseded),
        "the first root ends with its typed refusal: {refused:?}"
    );

    let next = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        session
            .send(TurnInput::text("the session's next send"))
            .id(NEXT_TURN)
            .output(),
    )
    .await;
    let runs = fixture.turn_runs(NEXT_TURN);
    let next = next
        .unwrap_or_else(|_| panic!("the next send settles; its runs: {runs:#?}"))
        .unwrap_or_else(|error| {
            panic!("the next send completes under a new root: {error:?}; runs: {runs:#?}")
        });
    assert_eq!(
        next.assistant_message(),
        Some("answered"),
        "the next send is answered by its own turn"
    );
    let [_] = runs.as_slice() else {
        panic!("the next send ran as its own root: {runs:#?}");
    };

    let open = fixture.open_after_settling().await;
    assert!(
        open.is_empty(),
        "the engine drains: no invocation is left running, retrying or paused: {open:#?}"
    );
    Ok(())
}

/// FIG-4018's crash window: the refused root's run dies after its end is
/// written to the store and before the engine records its outcome. The
/// replay retraces the run's journal to the same refusal, writes nothing
/// more and records the outcome, so the root has one terminal, the refusal,
/// and nothing is left paused. The refused send answers the refusal, and
/// the session's next send is admitted under a new root and completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_root_crashed_before_its_outcome_converges_on_one_terminal() -> Result<()> {
    let fixture = Fixture::head_moves_under_model_call(1).await;
    let session = fixture.core.session(SESSION).open().await?;
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        session
            .send(TurnInput::text("the session's first turn"))
            .id(FIRST_TURN)
            .output(),
    )
    .await
    .expect("the first turn settles")?;
    // The run's only state write is its recorded outcome.
    fixture.double.server().crash_on(
        lash_restate_test::CrashRule::new(lash_restate_test::CrashPoint::BeforeFrame {
            ty: lash_restate_test::protocol::MessageType::SetStateCommand,
        })
        .service(lash_restate_test::TURN_DRIVER_SERVICE)
        .key(lash_restate::turn_workflow_key(
            &lash_core::SessionId::from(SESSION),
            &lash_core::TurnId::from(TURN),
        ))
        .within_attempts(1),
    );

    let refused = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        session
            .send(TurnInput::text("the head moves under this turn's commit"))
            .id(TURN)
            .output(),
    )
    .await;
    let runs = fixture.turn_runs(TURN);
    let refused = refused
        .unwrap_or_else(|_| panic!("the refused turn settles; its runs: {runs:#?}"))
        .expect_err("the superseded commit ends the turn with its refusal");
    assert!(
        matches!(&refused, EmbedError::Runtime(error)
            if error.code == lash_core::RuntimeErrorCode::StoreCommitSuperseded),
        "the refused root answers its typed refusal: {refused:?}; runs: {runs:#?}"
    );

    let next = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        session
            .send(TurnInput::text("the session's next send"))
            .id(NEXT_TURN)
            .output(),
    )
    .await
    .expect("the next send settles")
    .unwrap_or_else(|error| panic!("the next send completes under a new root: {error:?}"));
    assert_eq!(next.assistant_message(), Some("answered"));

    fixture.double.server().settle().await;
    let runs = fixture.turn_runs(TURN);
    let [run] = runs.as_slice() else {
        panic!("the refused root ran in one invocation: {runs:#?}");
    };
    assert_eq!(run.status, "completed", "the replay completed: {run:?}");
    assert_eq!(
        run.attempts, 2,
        "the crash cut the first attempt and the replay finished: {run:?}"
    );
    let terminal = fixture
        .core
        .store_factory
        .root_terminal(
            &lash_core::SessionId::from(SESSION),
            &lash_core::TurnId::from(TURN),
        )
        .await?
        .expect("the refused root has terminal evidence");
    assert!(
        matches!(
            &terminal.cause,
            lash_core::store::RootTerminalCause::Refused { code, .. }
                if *code == lash_core::RuntimeErrorCode::StoreCommitSuperseded
        ),
        "the root's one terminal is its refusal: {terminal:?}"
    );
    assert_eq!(
        fixture.provider_calls.load(Ordering::SeqCst),
        3,
        "the replay called no model: the first turn, the refused turn and the next send did"
    );
    let open = fixture.open_after_settling().await;
    assert!(
        open.is_empty(),
        "the engine drains: no invocation is left running, retrying or paused: {open:#?}"
    );
    Ok(())
}
