use super::*;
use pretty_assertions::assert_eq;

/// FIG-3611, ADR 0106: a start key whose process ran to terminal and was
/// pruned registers a successor under the same key — a new minted id — and
/// the successor's process-owned session stores are fresh: they share no id,
/// no tombstone and no committed state with the pruned lifetime's stores, and
/// the successor runs to its own terminal.
///
/// Red before the registration cutover, where the process's name was its
/// identity: the successor re-derived the pruned process's session ids and
/// every create failed `StoreError::SessionDeleted` on their permanent
/// tombstones.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_same_start_key_successor_after_prune_owns_fresh_session_stores(
    factory: Arc<dyn crate::SessionStoreFactory>,
    registry: Arc<dyn crate::ProcessRegistry>,
    _effect_host: Arc<dyn crate::EffectHost>,
) {
    let key = crate::StartKey::for_host(crate::StartKeyOwner::HOST, "successor-after-prune");
    let start = || {
        process_registry::registration("successor-after-prune").with_start_key(Some(key.clone()))
    };

    // First lifetime: register under the key, commit a marker into each of
    // its process-owned session stores, and run it to terminal.
    let first = registry
        .register_process(start())
        .await
        .expect("register the key's first process");
    let mut first_requests = Vec::new();
    for (index, session_id) in crate::process_runtime_session_ids(&first.id)
        .into_iter()
        .enumerate()
    {
        let request = session_store_request(
            &session_id,
            "successor-after-prune-model",
            crate::SessionRelation::default(),
        );
        let store = factory
            .create_store(&request)
            .await
            .expect("create first-lifetime process-owned session store");
        let mut state = crate::RuntimeSessionState::new(request.policy.clone());
        state.session_id = session_id.clone();
        state.append_active_conversation_messages(&[crate::Message {
            id: format!("first-lifetime-message-{index}"),
            role: crate::MessageRole::User,
            parts: vec![crate::Part::text(
                format!("first-lifetime-message-{index}.p0"),
                "state only the first lifetime may see".to_string(),
                None,
            )]
            .into(),
            origin: None,
        }]);
        store
            .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(&state, &[]))
            .await
            .expect("commit first-lifetime session state");
        first_requests.push(request);
    }
    registry
        .complete_process(
            &first.id,
            crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::json!({"lifetime": "first"}),
            )),
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("run the first process to terminal");
    let report = registry
        .prune_terminal_processes(u64::MAX, None, crate::ProjectionWatermark::NoProjector)
        .await
        .expect("prune the first process");
    assert_eq!(report.pruned_processes, 1);
    for request in &first_requests {
        assert!(
            factory
                .open_existing_store(request)
                .await
                .expect("probe pruned process-owned store")
                .is_none(),
            "process prune left session store {} behind",
            request.session_id
        );
        assert!(
            factory
                .session_was_deleted(&request.session_id)
                .await
                .expect("probe the deleted set for a pruned process session"),
            "process prune must record session {} as deleted",
            request.session_id
        );
        let reuse_error = match factory.create_store(request).await {
            Ok(_) => panic!(
                "a pruned process-owned session id must stay unbindable: {}",
                request.session_id
            ),
            Err(error) => error,
        };
        assert_session_id_was_used_and_deleted(reuse_error, &request.session_id);
    }

    // The key is free again: its next start mints a successor with its own
    // id, and the pruned id refuses rather than resolving to the successor.
    let second = registry
        .register_process_reporting_disposition(start(), &[])
        .await
        .expect("start again under the pruned process's key");
    assert_eq!(
        second.disposition,
        crate::ProcessRegistrationDisposition::Created,
        "a pruned process no longer holds its key"
    );
    assert_ne!(second.record.id, first.id, "a minted id is never reused");
    assert!(
        matches!(
            registry.get_process(&first.id).await,
            Err(crate::PluginError::ProcessNoLongerRetained { .. })
        ),
        "the pruned id refuses; it never resolves to the successor"
    );

    // Every derived session id is the successor's own: never bound, holding
    // none of the pruned lifetime's committed state, and carrying none of its
    // tombstones.
    for (index, session_id) in crate::process_runtime_session_ids(&second.record.id)
        .into_iter()
        .enumerate()
    {
        assert!(
            !first_requests
                .iter()
                .any(|request| request.session_id == session_id),
            "the successor derives session ids of its own: {session_id}"
        );
        let request = session_store_request(
            &session_id,
            "successor-after-prune-model",
            crate::SessionRelation::default(),
        );
        assert!(
            factory
                .open_existing_store(&request)
                .await
                .expect("probe successor session before creation")
                .is_none(),
            "the successor's session id {session_id} was already bound"
        );
        assert!(
            factory
                .read_session(&session_id)
                .await
                .expect("read successor session before creation")
                .is_none(),
            "the successor's session id {session_id} shows first-lifetime state"
        );
        assert!(
            !factory
                .session_was_deleted(&session_id)
                .await
                .expect("probe the deleted set for a successor session"),
            "the successor's session id {session_id} is tombstoned"
        );
        let store = factory
            .create_store(&request)
            .await
            .expect("the successor binds its own session ids");
        let mut state = crate::RuntimeSessionState::new(request.policy.clone());
        state.session_id = session_id.clone();
        state.append_active_conversation_messages(&[crate::Message {
            id: format!("successor-message-{index}"),
            role: crate::MessageRole::User,
            parts: vec![crate::Part::text(
                format!("successor-message-{index}.p0"),
                format!("state written by the successor {index}"),
                None,
            )]
            .into(),
            origin: None,
        }]);
        store
            .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(&state, &[]))
            .await
            .expect("commit successor session state");
        let view = factory
            .read_session(&session_id)
            .await
            .expect("read the successor's session")
            .expect("the successor's committed session has a read view");
        assert_eq!(
            view.messages().len(),
            1,
            "the successor's session holds only its own writes"
        );
        assert_eq!(
            view.messages()[0].id,
            format!("successor-message-{index}"),
            "no first-lifetime message is visible to the successor"
        );
    }

    // The successor runs to its own terminal, and the terminal on record is
    // its own outcome.
    let output = crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
        serde_json::json!({"lifetime": "successor"}),
    ));
    registry
        .complete_process(
            &second.record.id,
            output.clone(),
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("run the successor to terminal");
    let record = registry
        .get_process(&second.record.id)
        .await
        .expect("read the successor")
        .expect("the successor stays retained");
    assert_eq!(record.status, crate::ProcessStatus::Completed);
    assert_eq!(
        record.outcome.as_ref(),
        Some(&output),
        "the terminal on record is the successor's own"
    );
}
