//! Commit budgets reach the host typed, terminal and actionable: a turn's
//! commit, a park's, a host append's, whose command settles failed with the
//! budget refusal (FIG-4202), and a creation whose head no commit fits
//! (FIG-4393).

use super::*;

#[tokio::test]
async fn commit_byte_budget_failure_reaches_the_host_as_terminal_and_actionable() -> Result<()> {
    const CONFIGURED_BYTE_LIMIT: usize = 4_096;
    let oversized_text = "x".repeat(CONFIGURED_BYTE_LIMIT * 2);
    let provider = crate::testing::TestProvider::builder()
        .kind("oversized-commit")
        .complete(move |_request| {
            let oversized_text = oversized_text.clone();
            async move { Ok(text_response(&oversized_text)) }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets_with_budget(
        LashCore::standard_builder(double_backend().await),
        crate::CommitBudget::new(
            crate::CommitBudgetLimit::bounded(CONFIGURED_BYTE_LIMIT),
            crate::CommitBudgetLimit::Unbounded,
        ),
    )
    .serve_test_model(provider, mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session("commit-budget-surface")
        .created()
        .await
        .open()
        .await?;

    let error = match session
        .send(TurnInput::text("produce an oversized turn"))
        .output()
        .await
    {
        Ok(_) => panic!("the oversized turn must fail at the production surface"),
        Err(error) => error,
    };

    let EmbedError::Runtime(runtime_error) = &error else {
        panic!("expected a host-visible runtime error, got {error}");
    };
    assert_eq!(
        runtime_error.code,
        lash_core::RuntimeErrorCode::StoreCommitByteBudgetExceeded
    );
    assert!(
        runtime_error.message.contains(&format!(
            "exceeding the {}-byte transaction budget",
            CONFIGURED_BYTE_LIMIT
        )),
        "{}",
        runtime_error.message
    );
    assert!(error.is_terminal(), "{error}");
    assert!(!error.is_retryable(), "{error}");
    Ok(())
}

#[tokio::test]
async fn commit_node_budget_failure_reaches_the_host_as_terminal_and_actionable() -> Result<()> {
    const CONFIGURED_NODE_LIMIT: usize = 1;
    let provider = crate::testing::TestProvider::builder()
        .kind("oversized-node-commit")
        .complete(|_request| async move { Ok(text_response("assistant response")) })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets_with_budget(
        LashCore::standard_builder(double_backend().await),
        crate::CommitBudget::new(
            crate::CommitBudgetLimit::Unbounded,
            crate::CommitBudgetLimit::bounded(CONFIGURED_NODE_LIMIT),
        ),
    )
    .serve_test_model(provider, mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session("commit-node-budget-surface")
        .created()
        .await
        .open()
        .await?;

    let error = match session
        .send(TurnInput::text("produce a turn"))
        .output()
        .await
    {
        Ok(_) => panic!("the over-limit node commit must fail at the production surface"),
        Err(error) => error,
    };

    let EmbedError::Runtime(runtime_error) = &error else {
        panic!("expected a host-visible runtime error, got {error}");
    };
    assert_eq!(
        runtime_error.code,
        lash_core::RuntimeErrorCode::StoreCommitNodeBudgetExceeded
    );
    assert!(
        runtime_error.message.contains(&format!(
            "exceeding the configured {CONFIGURED_NODE_LIMIT}-row node budget"
        )),
        "{}",
        runtime_error.message
    );
    assert!(error.is_terminal(), "{error}");
    assert!(!error.is_retryable(), "{error}");
    Ok(())
}

async fn core_with_commit_budget(commit_budget: crate::CommitBudget) -> Result<LashCore> {
    core_over_backend_with_commit_budget(double_backend().await, commit_budget)
}

fn core_over_backend_with_commit_budget(
    backend: lash_core::Backend,
    commit_budget: crate::CommitBudget,
) -> Result<LashCore> {
    explicit_ephemeral_facets_with_budget(LashCore::standard_builder(backend), commit_budget)
        .serve_test_model(mock_provider(), mock_model_spec())
        .build(crate::testing::runtime_lease_owner())
}

fn pending_park_state(session_id: impl Into<SessionId>, text: &str) -> RuntimeSessionState {
    let policy = lash_core::SessionPolicy {
        model: Some(recorded_model(mock_model_spec())),
        ..lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        )
    };
    let mut state = RuntimeSessionState::new(policy);
    state.session_id = session_id.into();
    state.ensure_agent_frame_initialized();
    state.append_active_conversation_messages(&[text_message(lash_core::MessageRole::User, text)]);
    state
}

#[cfg(feature = "testing")]
#[tokio::test]
async fn testing_set_persisted_replaces_resident_state_for_park_fixture() -> Result<()> {
    let core = core_with_commit_budget(crate::CommitBudget::new(
        crate::CommitBudgetLimit::Unbounded,
        crate::CommitBudgetLimit::Unbounded,
    ))
    .await?;
    let session = core
        .session("testing-set-persisted-park")
        .created()
        .await
        .open()
        .await?;
    let fixture = pending_park_state("testing-set-persisted-park", "park fixture via testing");
    let node_ids = |nodes: &[std::sync::Arc<lash_core::SessionNodeRecord>]| {
        nodes.iter().map(|n| n.node_id.clone()).collect::<Vec<_>>()
    };
    let fixture_nodes = node_ids(&fixture.session_graph.nodes);
    let fresh = node_ids(&session.admin().state().export().await.session_graph.nodes);
    assert_ne!(fresh, fixture_nodes);
    session.admin().state().set_persisted(fixture).await?;
    let resident = session.admin().state().export().await;
    assert_eq!(node_ids(&resident.session_graph.nodes), fixture_nodes);
    assert_eq!(
        Box::pin(session.park()).await?.session_id(),
        "testing-set-persisted-park"
    );
    Ok(())
}

fn assert_byte_budget_session_error(error: &EmbedError, configured_limit: usize) {
    assert!(
        matches!(
            error,
            EmbedError::Session(SessionError::Store {
                source: lash_core::StoreError::CommitByteBudgetExceeded { max_bytes, .. },
                ..
            }) if *max_bytes == configured_limit
        ),
        "expected typed byte-budget rejection, got {error}"
    );
    assert!(
        error.to_string().contains(&format!(
            "exceeding the {configured_limit}-byte transaction budget"
        )),
        "{error}"
    );
    assert!(error.is_terminal(), "{error}");
    assert!(!error.is_retryable(), "{error}");
}

fn assert_node_budget_session_error(error: &EmbedError, configured_limit: usize) {
    assert!(
        matches!(
            error,
            EmbedError::Session(SessionError::Store {
                source: lash_core::StoreError::CommitNodeBudgetExceeded { max_nodes, .. },
                ..
            }) if *max_nodes == configured_limit
        ),
        "expected typed node-budget rejection, got {error}"
    );
    assert!(
        error.to_string().contains(&format!(
            "exceeding the configured {configured_limit}-row node budget"
        )),
        "{error}"
    );
    assert!(error.is_terminal(), "{error}");
    assert!(!error.is_retryable(), "{error}");
}

/// A host append is a session command (FIG-4202): a commit over the budget
/// settles the command failed with the budget's typed code, so no redrive
/// retries it and the lane holds nothing open.
async fn assert_budget_command_error(
    factory: &Arc<dyn DeploymentStore>,
    session_id: &str,
    error: &EmbedError,
    code: lash_core::RuntimeErrorCode,
    expected_message: &str,
) {
    assert!(
        matches!(error, EmbedError::Runtime(runtime) if runtime.code == code),
        "expected a command settled failed with {code:?}, got {error}"
    );
    assert!(error.to_string().contains(expected_message), "{error}");
    assert!(error.is_terminal(), "{error}");
    assert!(!error.is_retryable(), "{error}");
    let open = factory
        .list_open_queued_work(&SessionId::from(session_id))
        .await
        .expect("read the session's open work");
    assert!(
        open.is_empty(),
        "the over-budget command settled, leaving nothing open: {open:?}"
    );
}

#[tokio::test]
async fn public_append_byte_budget_failure_is_typed_terminal_and_actionable() -> Result<()> {
    // Room for the command's failed settlement, the bare head's commit (a
    // little over 2 KB now that the head's standard namespace records the
    // configured render, FIG-4527; its refusal receipt is not charged,
    // FIG-4471), not for the append it refuses (four times the limit).
    const CONFIGURED_BYTE_LIMIT: usize = 3072;
    let backend = double_backend().await;
    let factory = backend.session_store_factory();
    let core = core_over_backend_with_commit_budget(
        backend,
        crate::CommitBudget::new(
            crate::CommitBudgetLimit::bounded(CONFIGURED_BYTE_LIMIT),
            crate::CommitBudgetLimit::Unbounded,
        ),
    )?;
    let session = core
        .session("append-byte-budget-surface")
        .created()
        .await
        .open()
        .await?;

    let error =
        Box::pin(
            session
                .admin()
                .state()
                .append_messages(vec![lash_core::PluginMessage::text(
                    lash_core::MessageRole::User,
                    "x".repeat(CONFIGURED_BYTE_LIMIT * 4),
                )]),
        )
        .await
        .expect_err("the public append must reject its over-limit commit");

    let append_bytes = error
        .to_string()
        .split("runtime commit carries ")
        .nth(1)
        .and_then(|rest| rest.split_once(' '))
        .and_then(|(bytes, _)| bytes.parse::<usize>().ok())
        .unwrap_or_else(|| panic!("the refusal names the append's bytes: {error}"));
    assert!(
        append_bytes > CONFIGURED_BYTE_LIMIT,
        "the refused append outgrows the budget: {error}"
    );
    assert_budget_command_error(
        &factory,
        "append-byte-budget-surface",
        &error,
        lash_core::RuntimeErrorCode::StoreCommitByteBudgetExceeded,
        &format!("exceeding the {CONFIGURED_BYTE_LIMIT}-byte transaction budget"),
    )
    .await;
    Ok(())
}

#[tokio::test]
async fn public_append_node_budget_failure_is_typed_terminal_and_actionable() -> Result<()> {
    const CONFIGURED_NODE_LIMIT: usize = 1;
    let backend = double_backend().await;
    let factory = backend.session_store_factory();
    let core = core_over_backend_with_commit_budget(
        backend,
        crate::CommitBudget::new(
            crate::CommitBudgetLimit::Unbounded,
            crate::CommitBudgetLimit::bounded(CONFIGURED_NODE_LIMIT),
        ),
    )?;
    let session = core
        .session("append-node-budget-surface")
        .created()
        .await
        .open()
        .await?;

    let error =
        Box::pin(
            session
                .admin()
                .state()
                .append_messages(vec![lash_core::PluginMessage::text(
                    lash_core::MessageRole::User,
                    "one appended message plus the initial frame exceeds one node",
                )]),
        )
        .await
        .expect_err("the public append must reject its over-limit commit");

    assert_budget_command_error(
        &factory,
        "append-node-budget-surface",
        &error,
        lash_core::RuntimeErrorCode::StoreCommitNodeBudgetExceeded,
        &format!("exceeding the configured {CONFIGURED_NODE_LIMIT}-row node budget"),
    )
    .await;
    Ok(())
}

/// Creation measures the head it creates (FIG-4393): a config whose
/// created head no commit fits under the core's budget is refused typed,
/// terminal and actionable, and nothing is written.
#[tokio::test]
async fn create_byte_budget_failure_is_typed_terminal_and_writes_nothing() -> Result<()> {
    const CONFIGURED_BYTE_LIMIT: usize = 256;
    let backend = double_backend().await;
    let factory = backend.session_store_factory();
    let core = core_over_backend_with_commit_budget(
        backend,
        crate::CommitBudget::new(
            crate::CommitBudgetLimit::bounded(CONFIGURED_BYTE_LIMIT),
            crate::CommitBudgetLimit::Unbounded,
        ),
    )?;

    let error = match core
        .session("create-byte-budget-surface")
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await
    {
        Ok(_) => panic!("creation must refuse a head no commit fits under the budget"),
        Err(error) => error,
    };

    assert!(
        matches!(
            &error,
            EmbedError::Store(lash_core::StoreError::CommitByteBudgetExceeded { max_bytes, .. })
                if *max_bytes == CONFIGURED_BYTE_LIMIT
        ),
        "expected typed byte-budget rejection, got {error}"
    );
    assert!(error.is_terminal(), "{error}");
    assert!(!error.is_retryable(), "{error}");
    assert!(
        matches!(
            factory
                .lookup_session(&SessionId::from("create-byte-budget-surface"))
                .await?,
            lash_core::SessionLookup::Absent
        ),
        "a refused creation writes nothing"
    );
    Ok(())
}

#[tokio::test]
async fn park_byte_budget_failure_is_typed_terminal_and_actionable() -> Result<()> {
    const CONFIGURED_BYTE_LIMIT: usize = 256;
    let backend = double_backend().await;
    // The session was created under a budget its head fits; the park's
    // host runs under a lower one.
    core_over_backend_with_commit_budget(
        backend.clone(),
        crate::CommitBudget::new(
            crate::CommitBudgetLimit::Unbounded,
            crate::CommitBudgetLimit::Unbounded,
        ),
    )?
    .session("park-byte-budget-surface")
    .create(crate::SessionCreation::root(mock_session_spec()))
    .await?;
    let core = core_over_backend_with_commit_budget(
        backend,
        crate::CommitBudget::new(
            crate::CommitBudgetLimit::bounded(CONFIGURED_BYTE_LIMIT),
            crate::CommitBudgetLimit::Unbounded,
        ),
    )?;
    let session = core.session("park-byte-budget-surface").open().await?;
    session
        .admin()
        .state()
        .set_persisted(pending_park_state(
            "park-byte-budget-surface",
            &"x".repeat(CONFIGURED_BYTE_LIMIT * 4),
        ))
        .await?;

    let error = match Box::pin(session.park()).await {
        Ok(_) => panic!("park must reject its over-limit commit"),
        Err(refused) => EmbedError::from(refused),
    };

    assert_byte_budget_session_error(&error, CONFIGURED_BYTE_LIMIT);
    Ok(())
}

#[tokio::test]
async fn park_node_budget_failure_is_typed_terminal_and_actionable() -> Result<()> {
    const CONFIGURED_NODE_LIMIT: usize = 1;
    let core = core_with_commit_budget(crate::CommitBudget::new(
        crate::CommitBudgetLimit::Unbounded,
        crate::CommitBudgetLimit::bounded(CONFIGURED_NODE_LIMIT),
    ))
    .await?;
    let session = core
        .session("park-node-budget-surface")
        .created()
        .await
        .open()
        .await?;
    session
        .admin()
        .state()
        .set_persisted(pending_park_state(
            "park-node-budget-surface",
            "one pending message plus the initial frame exceeds one node",
        ))
        .await?;

    let error = match Box::pin(session.park()).await {
        Ok(_) => panic!("park must reject its over-limit commit"),
        Err(refused) => EmbedError::from(refused),
    };

    assert_node_budget_session_error(&error, CONFIGURED_NODE_LIMIT);
    Ok(())
}
