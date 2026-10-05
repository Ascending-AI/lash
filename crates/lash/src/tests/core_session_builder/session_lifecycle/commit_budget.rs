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
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse("commit-budget-surface").expect("nonblank host identity"))
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

async fn core_with_commit_budget(commit_budget: crate::CommitBudget) -> Result<LashCore> {
    core_over_backend_with_commit_budget(double_backend().await, commit_budget)
}

fn core_over_backend_with_commit_budget(
    backend: lash_core::Backend,
    commit_budget: crate::CommitBudget,
) -> Result<LashCore> {
    explicit_ephemeral_facets_with_budget(LashCore::standard_builder(backend), commit_budget)
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())
}

fn pending_park_state(session_id: impl Into<SessionId>, text: &str) -> RuntimeSessionState {
    let policy = lash_core::SessionPolicy {
        model: Some(recorded_llm_profile(mock_llm_profile_spec())),
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
        .list_open_queued_work(&SessionId::fixture(session_id))
        .await
        .expect("read the session's open work");
    assert!(
        open.is_empty(),
        "the over-budget command settled, leaving nothing open: {open:?}"
    );
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
        .session(
            crate::SessionId::parse("append-node-budget-surface").expect("nonblank host identity"),
        )
        .created()
        .await
        .open()
        .await?;

    let error = Box::pin(session.admin().state().append_messages(vec![
        lash_core::PluginMessage::text(lash_core::MessageRole::User, "first appended node"),
        lash_core::PluginMessage::text(
            lash_core::MessageRole::User,
            "second appended node exceeds the one-node budget",
        ),
    ]))
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
    .session(crate::SessionId::parse("park-byte-budget-surface").expect("nonblank host identity"))
    .create(crate::SessionCreation::root(mock_session_spec()))
    .await?;
    let core = core_over_backend_with_commit_budget(
        backend,
        crate::CommitBudget::new(
            crate::CommitBudgetLimit::bounded(CONFIGURED_BYTE_LIMIT),
            crate::CommitBudgetLimit::Unbounded,
        ),
    )?;
    let session = core
        .session(
            crate::SessionId::parse("park-byte-budget-surface").expect("nonblank host identity"),
        )
        .open()
        .await?;
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
        .session(
            crate::SessionId::parse("park-node-budget-surface").expect("nonblank host identity"),
        )
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
