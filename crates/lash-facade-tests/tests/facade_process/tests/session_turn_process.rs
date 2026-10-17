use super::*;

use lash_facade_tests::session_process::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn observation_names_the_child_session_of_an_unnamed_start() -> Result<()> {
    observed_child_session_matches_execution(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn observation_names_the_child_session_of_a_named_start() -> Result<()> {
    observed_child_session_matches_execution(true).await
}

/// A child turn that panics ends its process with a typed failure, never a
/// success and never a hang, and the parent's session lives on: its next
/// turn answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn child_turn_panic_is_typed_and_the_parent_remains_alive() -> Result<()> {
    let core = deploy(
        &sqlite_memory_store_backend().await,
        ChildModel::Panics,
        &[mock_llm_profile_spec()],
        true,
    )
    .await?;
    let started = core
        .processes()
        .start(request("mock-model"), core.effect_host())
        .await?;
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        core.processes().await_output(&started.process_id),
    )
    .await
    .expect("the panicked child's process ends within a minute")?;
    assert!(
        matches!(&output, lash_core::ProcessAwaitOutput::Settled { output }
            if matches!(&output.outcome, lash_core::ToolCallOutcome::Failure(failure)
                if failure.code == "process_session_turn_provider_error")),
        "the panicked child's process ends a typed failure: {output:?}"
    );
    let parent = core
        .session(lash::SessionId::parse(PARENT).expect("nonblank host identity"))
        .open()
        .await?;
    let answer = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        parent.send(lash::TurnInput::text("are you there")).output(),
    )
    .await
    .expect("the parent answers")?;
    assert_eq!(answer.assistant_message(), Some("parent lives"));
    drop(parent);
    core.shutdown().await?;
    Ok(())
}

/// A session-turn process cancelled while its child turn runs ends `Cancelled`, and its
/// child session stays: it reopens with no turn open and nothing left to
/// admit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_turn_process_cancelled_mid_turn_ends_cancelled_and_keeps_its_child() -> Result<()>
{
    let entered = Arc::new(tokio::sync::Notify::new());
    let core = deploy(
        &sqlite_memory_store_backend().await,
        ChildModel::Holds(Arc::clone(&entered)),
        &[mock_llm_profile_spec()],
        true,
    )
    .await?;
    let started = core
        .processes()
        .start(request("mock-model"), core.effect_host())
        .await?;
    tokio::time::timeout(std::time::Duration::from_secs(30), entered.notified())
        .await
        .expect("the child's turn reaches its model");
    core.processes()
        .cancel(&started.process_id, core.effect_host())
        .await?;
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        core.processes().await_output(&started.process_id),
    )
    .await
    .expect("the cancelled process settles")?;
    assert!(
        matches!(&output, lash_core::ProcessAwaitOutput::Settled { output }
            if matches!(output.outcome, lash_core::ToolCallOutcome::Cancelled(_))),
        "the process ends cancelled: {output:?}"
    );
    let child = core
        .session(lash::SessionId::parse(CHILD).expect("nonblank host identity"))
        .open()
        .await?;
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while child.current_turn().await?.is_some() {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        Result::Ok(())
    })
    .await
    .expect("no turn stays open in the kept child")?;
    drop(child);
    core.shutdown().await?;
    Ok(())
}

/// A child session whose creating pass admitted its row and then failed its
/// first head commit is finished from the config its admission recorded
/// (FIG-4627), even on a redeployment that serves its key under changed
/// metadata: its turn runs to the answer its process ends with, and its
/// committed head keeps the metadata its creation resolved.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_partially_created_child_finishes_from_its_recorded_creation_request() -> Result<()> {
    use lash_core::testing::{Script, StoreOp};
    const REDEPLOYED_WINDOW: usize = 123_456;
    let stores: Arc<dyn lash_core::StoreSet> = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("SQLite memory stores"),
    );
    let script = Script::new();
    let layered = lash_core::testing::runtime_helpers::LayeredStores::over(Arc::clone(&stores))
        .map_session_store_factory(|inner| -> Arc<dyn lash_core::DeploymentStore> {
            script.wrap("catalog", inner)
        })
        .into_store_set();
    let core = deploy(
        &lash_conformance::backend_over(layered),
        ChildModel::Answers,
        &[mock_llm_profile_spec()],
        true,
    )
    .await?;
    // The parent is created; every head commit from here on is the child's
    // first, and this deployment never lands one.
    let commits = script.calls(StoreOp::commit_runtime_state);
    script
        .on(StoreOp::commit_runtime_state)
        .from_nth(commits + 1)
        .before()
        .fail(|| lash_core::StoreError::Contended);
    let started = core
        .processes()
        .start(started("mock-model", true), core.effect_host())
        .await?;
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while script.calls(StoreOp::commit_runtime_state) <= commits {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the child's admission reaches its first head commit");
    core.shutdown().await?;
    drop(core);

    let core = deploy(
        &lash_conformance::backend_over(stores),
        ChildModel::Answers,
        &[llm_profile_spec("mock-model", None, REDEPLOYED_WINDOW)],
        false,
    )
    .await?;
    // Its passes may have spent the activation-loop budget.
    core.processes()
        .redrive(&started.process_id, "operator")
        .await?;
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        core.processes().await_output(&started.process_id),
    )
    .await
    .expect("the partially created child's process settles")?;
    assert!(
        matches!(&output, lash_core::ProcessAwaitOutput::Settled { output }
            if output.is_success()),
        "the finished child's turn answered: {output:?}"
    );
    let child = core
        .session(lash::SessionId::parse(CHILD).expect("nonblank host identity"))
        .open()
        .await?;
    let window = child
        .policy_snapshot()
        .model
        .map(|model| model.model.context_window_tokens());
    assert_eq!(
        window,
        Some(200_000),
        "the child keeps the metadata its creation resolved"
    );
    drop(child);
    core.shutdown().await?;
    Ok(())
}

/// A session-turn process on a profile key this deployment does not serve is never
/// settled by the refusal: the process stays open while its child session
/// parks, and once a deployment serves the key, an operator's redrive of the
/// child runs its turn to its answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_turn_on_an_unserved_profile_is_repaired_by_a_serving_deployment() -> Result<()> {
    const UNSERVED: &str = "unserved-model";
    let backend = sqlite_memory_store_backend().await;
    let core = deploy(
        &backend,
        ChildModel::Answers,
        &[mock_llm_profile_spec()],
        true,
    )
    .await?;
    let started = core
        .processes()
        .start(request(UNSERVED), core.effect_host())
        .await?;
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            core.processes().await_output(&started.process_id),
        )
        .await
        .is_err(),
        "the refusal of an unserved key never settles the process"
    );
    core.shutdown().await?;
    drop(core);

    let core = deploy(
        &backend,
        ChildModel::Answers,
        &[
            mock_llm_profile_spec(),
            llm_profile_spec(UNSERVED, None, 200_000),
        ],
        false,
    )
    .await?;
    // The child parks once its passes exhaust the activation-loop budget,
    // or else this deployment's claim runs it and the redrive is a no-op.
    core.session(lash::SessionId::parse(CHILD).expect("nonblank host identity"))
        .open()
        .await?
        .durable()
        .redrive("operator")
        .await?;
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        core.processes().await_output(&started.process_id),
    )
    .await
    .expect("the serving deployment settles the process")?;
    assert!(
        matches!(&output, lash_core::ProcessAwaitOutput::Settled { output }
            if output.is_success()),
        "the child's turn ran on the serving deployment: {output:?}"
    );
    core.shutdown().await?;
    Ok(())
}

/// FIG-6565: the real child runner projects the committed provider cause,
/// even though it has no observed issue vector to consult.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_turn_provider_cause_reaches_the_parent() -> Result<()> {
    let provider = lash_core::testing::TestProvider::builder()
        .kind("child-provider-cause")
        .complete(|_| async {
            Err(
                lash::provider::LlmTransportError::new("private provider body")
                    .with_kind(lash_core::ProviderFailureKind::Auth)
                    .with_http_status(401),
            )
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .build(lash_facade_tests::runtime_lease_owner())?;
    let started = core
        .processes()
        .start(request("mock-model"), core.effect_host())
        .await?;
    let answer = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        core.processes().await_output(&started.process_id),
    )
    .await
    .expect("child settles")?;
    let lash_core::ProcessAwaitOutput::Settled { output } = answer else {
        panic!("settled child")
    };
    let lash_core::ToolCallOutcome::Failure(failure) = output.outcome else {
        panic!("failed child")
    };
    let raw = failure
        .raw
        .as_ref()
        .map(lash_core::ToolValue::to_json_value)
        .expect("committed cause");
    assert_eq!(raw["cause"], lash::TurnErrorCode::AuthRejected.code());
    assert!(raw.get("retryable").is_none());
    assert!(raw.get("provider_failure_kind").is_none());
    core.shutdown().await?;
    Ok(())
}
