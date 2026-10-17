use super::*;
use lash::{SessionId, TurnInput, TurnOutcome};
use lash_facade_tests::controls::*;

const CHAT: &str = "recorded-controls-chat";
const PULSAR: &str = "recorded-controls-pulsar";

/// How a turn ended and how many model calls it made since the last
/// reading of `calls`.
fn shape(output: &lash::TurnOutput, calls: &AtomicUsize) -> (TurnOutcome, usize) {
    (output.outcome.clone(), calls.swap(0, Ordering::SeqCst))
}

fn finished_after_all_tools() -> (TurnOutcome, usize) {
    (
        TurnOutcome::Finished(lash::TurnFinish::AssistantMessage {
            text: "answered".to_string(),
        }),
        TOOL_CALLS_BEFORE_ANSWER + 1,
    )
}

fn stopped_at(turns: usize) -> (TurnOutcome, usize) {
    (TurnOutcome::Stopped(lash::TurnStop::MaxTurns), turns)
}

/// Both sessions keep their recorded budgets when the engine opens them
/// itself: no host runtime is open, so every run below is executed on a
/// runtime the engine opened from the store, under a core whose own default
/// is neither session's budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn budgets_survive_engine_reopen() -> Result<()> {
    let calls = Arc::new(AtomicUsize::new(0));
    let core = core_over(
        sqlite_memory_store_backend().await,
        looping_provider(&calls),
    )?;
    create_with_budget(&core, CHAT, lash::TurnBudget::Unbounded).await?;
    create_with_budget(&core, PULSAR, lash::TurnBudget::bounded(BOUNDED_TURNS)).await?;

    for round in ["first", "second"] {
        assert_eq!(
            shape(
                &engine_executed_turn(&core, CHAT, &format!("{round} chat")).await?,
                &calls
            ),
            finished_after_all_tools(),
            "the engine's {round} open of the unbounded session keeps it unbounded"
        );
        assert_eq!(
            shape(
                &engine_executed_turn(&core, PULSAR, &format!("{round} pulsar")).await?,
                &calls
            ),
            stopped_at(BOUNDED_TURNS),
            "the engine's {round} open of the bounded session keeps its bound"
        );
    }
    Ok(())
}

/// `SetTurnBudget` reaches the next run: a session created unbounded runs
/// to the model's answer, and after the command its next run, on the open
/// runtime and on the engine's own reopen, stops at the commanded budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_commanded_turn_budget_bounds_the_next_run() -> Result<()> {
    const ID: &str = "commanded-turn-budget";
    let calls = Arc::new(AtomicUsize::new(0));
    let core = core_over(
        sqlite_memory_store_backend().await,
        looping_provider(&calls),
    )?;
    create_with_budget(&core, ID, lash::TurnBudget::Unbounded).await?;
    let session = core
        .session(lash::SessionId::parse(ID).expect("nonblank host identity"))
        .open()
        .await?;
    let before = session
        .send(TurnInput::text("look it all up"))
        .output()
        .await?;
    assert_eq!(shape(&before, &calls), finished_after_all_tools());

    let revision = session.admin().config().revision().await?;
    let outcome = apply(
        &session,
        "bound-the-budget",
        revision,
        lash::config::ConfigTransaction::of(lash::config::SetTurnBudget {
            turn_budget: lash::TurnBudget::bounded(BOUNDED_TURNS),
        }),
    )
    .await?;
    assert!(
        matches!(
            outcome,
            lash::config::ConfigTransactionOutcome::Applied { .. }
        ),
        "{outcome:?}"
    );
    let after = session
        .send(TurnInput::text("look it all up again"))
        .output()
        .await?;
    assert_eq!(
        shape(&after, &calls),
        stopped_at(BOUNDED_TURNS),
        "the open runtime's next run executes under the commanded budget"
    );
    drop(session);
    assert_eq!(
        shape(
            &engine_executed_turn(&core, ID, "and once more").await?,
            &calls
        ),
        stopped_at(BOUNDED_TURNS),
        "the engine's reopen runs under the commanded budget"
    );
    Ok(())
}

/// The budget figments records for a session (FIG-4376): its runs stop
/// after exactly this many model calls.
const RECORDED_TURNS: usize = 12;

/// A model that never stops calling `app_lookup` until the whole run has
/// made [`RUNAWAY_CALLS`] calls. `calls` counts the calls it served.
fn endless_provider(calls: &Arc<AtomicUsize>) -> ProviderHandle {
    let calls = Arc::clone(calls);
    lash::testing::TestProvider::builder()
        .kind("recorded-controls")
        .complete(move |_request| {
            let calls = Arc::clone(&calls);
            async move {
                let call = calls.fetch_add(1, Ordering::SeqCst) + 1;
                if call >= RUNAWAY_CALLS {
                    return Ok(text_response("runaway"));
                }
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::ToolCall {
                        call_id: format!("lookup-{call}"),
                        tool_name: "app_lookup".into(),
                        input_json: "{}".into(),
                        replay: None,
                    }],
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

/// A session's recorded budget bounds its runs after the engine restarts
/// under a core whose own budget is unbounded (figments' re-pin at
/// 257bef60f3 ran such a run past 500 model calls). One session is created
/// with the budget, the other is given it by `SetTurnBudget`; then the
/// `first` deployment goes away, and `restart` brings up a new one over its
/// stores, which opens both under an unbounded core. Each run, executed on the
/// engine's own reopen and on a host open, stops after exactly
/// [`RECORDED_TURNS`] model calls, typed: the next call is never made.
/// `prefix` names the law's sessions.
async fn a_recorded_budget_bounds_every_run_after_an_engine_restart(
    first: lash_core::Backend,
    restart: impl AsyncFnOnce() -> lash_core::Backend,
    prefix: &str,
) -> Result<LashCore> {
    let created = format!("{prefix}-created-bounded");
    let commanded = format!("{prefix}-commanded-bounded");
    let calls = Arc::new(AtomicUsize::new(0));
    {
        let creator = core_over(first, endless_provider(&calls))?;
        create_with_budget(
            &creator,
            &created,
            lash::TurnBudget::bounded(RECORDED_TURNS),
        )
        .await?;
        create_with_budget(&creator, &commanded, lash::TurnBudget::Unbounded).await?;
        let session = creator
            .session(SessionId::fixture(commanded.clone()))
            .open()
            .await?;
        let revision = session.admin().config().revision().await?;
        let outcome = apply(
            &session,
            "bound-before-restart",
            revision,
            lash::config::ConfigTransaction::of(lash::config::SetTurnBudget {
                turn_budget: lash::TurnBudget::bounded(RECORDED_TURNS),
            }),
        )
        .await?;
        assert!(
            matches!(
                outcome,
                lash::config::ConfigTransactionOutcome::Applied { .. }
            ),
            "{outcome:?}"
        );
        drop(session);
        creator.shutdown().await?;
    }
    let second = restart().await;
    let unbounded = explicit_ephemeral_facets(LashCore::standard_builder(second))
        .serve_test_llm_profile(endless_provider(&calls), mock_llm_profile_spec())
        .tools(lash::tool_provider_key!("tools"), Arc::new(AppTools))
        .build(lash_facade_tests::runtime_lease_owner())?;
    for id in [created.as_str(), commanded.as_str()] {
        assert_eq!(
            shape(
                &engine_executed_turn(&unbounded, id, "look it all up").await?,
                &calls
            ),
            stopped_at(RECORDED_TURNS),
            "{id}: the engine's reopen after the restart runs the recorded budget"
        );
        let opened = unbounded.session(SessionId::fixture(id)).open().await?;
        let output = opened
            .send(TurnInput::text("look it all up again"))
            .output()
            .await?;
        assert_eq!(
            shape(&output, &calls),
            stopped_at(RECORDED_TURNS),
            "{id}: a host open under the unbounded core runs the recorded budget"
        );
    }
    Ok(unbounded)
}

/// The law over `stores`: the restart is a new deployment over them.
async fn after_a_restart_over(stores: Arc<dyn lash_core::StoreSet>) -> Result<()> {
    let unbounded = a_recorded_budget_bounds_every_run_after_an_engine_restart(
        lash_conformance::backend_over(Arc::clone(&stores)),
        async || lash_conformance::backend_over(stores),
        "restart",
    )
    .await?;
    unbounded.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recorded_budget_bounds_every_run_after_an_engine_restart_on_sqlite() -> Result<()> {
    after_a_restart_over(sqlite_memory_store_set().await).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a with-service.sh pg gate"]
async fn a_recorded_budget_bounds_every_run_after_an_engine_restart_on_postgres() -> Result<()> {
    let (stores, _database, _attachments) = postgres_store_parts().await;
    after_a_restart_over(stores).await
}

/// A creation whose spec states no `max_tool_calls` is refused, typed, and
/// nothing is created (FIG-4546, FIG-4594): the limit has no default, no
/// built-in ceiling and no core setting to fall back to.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_creation_without_max_tool_calls_is_refused() -> Result<()> {
    const ID: &str = "creation-without-max-tool-calls";
    let core = core_over(sqlite_memory_store_backend().await, mock_provider())?;
    let mut unstated = mock_session_spec();
    unstated.max_tool_calls = None;
    let error = core
        .session(lash::SessionId::parse(ID).expect("nonblank host identity"))
        .create(lash::SessionCreation::root(unstated))
        .await
        .err()
        .expect("a creation without max_tool_calls must be refused");
    assert!(
        matches!(error, lash::EmbedError::MissingMaxToolCalls),
        "{error:?}"
    );
    assert!(error.is_terminal() && !error.is_retryable(), "{error}");
    assert!(
        matches!(
            core.session(lash::SessionId::parse(ID).expect("nonblank host identity"))
                .open()
                .await,
            Err(lash::EmbedError::UnknownSession { .. })
        ),
        "the refused creation wrote no session"
    );
    Ok(())
}
const RUNAWAY_CALLS: usize = 100;
