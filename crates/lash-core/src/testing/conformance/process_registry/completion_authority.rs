use super::*;

fn rerunnable_registration(id: &str) -> ProcessRegistration {
    ProcessRegistration::new(
        id,
        ProcessInput::Engine {
            kind: "test-engine".to_string(),
            payload: serde_json::Value::Null,
        },
        RecoveryDisposition::Rerunnable,
        ProcessProvenance::host(),
    )
    // Engine rows require a captured execution env (see `validate_process_registration`).
    .with_execution_env_ref(Some(ProcessExecutionEnvRef::new(format!(
        "process-env:test:{id}"
    ))))
}

fn success(value: serde_json::Value) -> ProcessAwaitOutput {
    ProcessAwaitOutput::Success {
        value,
        control: None,
    }
}

/// The unleased `complete_process` path validates the completion authority
/// against the row's declared `RecoveryDisposition` inside the completion
/// operation, uniformly across backends, and records that authority on the
/// terminal event as audit evidence (ADR 0027). This pins the full
/// disposition×authority matrix — accepted and rejected — for every backend.
pub(super) async fn completion_authority_validated_against_disposition(
    registry: Arc<dyn ProcessRegistry>,
) {
    // ExternallyOwned: only ExternalOwner and (for an Abandoned terminal)
    // ReconciledAbandon may close it; a WorkflowKey authority is rejected because
    // no substrate ever ran it.
    registry
        .register_process(registration("auth-ext-accept"))
        .await
        .expect("register externally-owned");
    let completed = registry
        .complete_process(
            "auth-ext-accept",
            success(serde_json::json!("ok")),
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("external owner closes an externally-owned row");
    assert_eq!(
        completed.status.terminal_state(),
        Some(ProcessTerminalState::Completed)
    );
    // The authority kind is recorded on the terminal event as audit evidence.
    let terminal_event = registry
        .events_after("auth-ext-accept", 0)
        .await
        .expect("events")
        .into_iter()
        .find(|event| event.event_type == "process.completed")
        .expect("terminal event present");
    assert_eq!(
        terminal_event.payload.get("completion_authority"),
        Some(&serde_json::json!({
            "authority": "external_owner",
        })),
        "the terminal event records the completion authority kind as evidence"
    );

    registry
        .register_process(registration("auth-ext-reject-wf"))
        .await
        .expect("register externally-owned");
    let rejected = registry
        .complete_process(
            "auth-ext-reject-wf",
            success(serde_json::json!("no")),
            ProcessCompletionAuthority::workflow_key("auth-ext-reject-wf"),
        )
        .await;
    assert!(
        rejected.is_err(),
        "workflow-key authority must not close an externally-owned row"
    );
    assert!(
        registry
            .get_process("auth-ext-reject-wf")
            .await
            .is_some_and(|record| !record.is_terminal()),
        "a rejected completion appends no terminal event"
    );

    // ReconciledAbandon closes an externally-owned row only with an Abandoned
    // terminal; a non-Abandoned output under that authority is rejected.
    registry
        .register_process(registration("auth-ext-reconcile"))
        .await
        .expect("register externally-owned");
    assert!(
        registry
            .complete_process(
                "auth-ext-reconcile",
                success(serde_json::json!("not-abandoned")),
                ProcessCompletionAuthority::ReconciledAbandon,
            )
            .await
            .is_err(),
        "reconciled-abandon writes only an Abandoned terminal"
    );
    registry
        .complete_process(
            "auth-ext-reconcile",
            ProcessAwaitOutput::Abandoned {
                evidence: Box::new(AbandonEvidence {
                    writer: AbandonWriter::ReconciledRequest,
                    owner: None,
                    epoch_ms: 7,
                }),
                control: None,
            },
            ProcessCompletionAuthority::ReconciledAbandon,
        )
        .await
        .expect("reconciled abandon closes an externally-owned row");

    // OwnerBound: WorkflowKey (the substrate that ran it) closes it; an
    // ExternalOwner authority is rejected because a lash-executed row has a
    // lease-fenced single writer.
    registry
        .register_process(owner_bound_registration("auth-ownerbound-accept"))
        .await
        .expect("register owner-bound");
    registry
        .complete_process(
            "auth-ownerbound-accept",
            success(serde_json::json!("ran")),
            ProcessCompletionAuthority::workflow_key("auth-ownerbound-accept"),
        )
        .await
        .expect("workflow-key closes an owner-bound row it ran");

    registry
        .register_process(owner_bound_registration("auth-ownerbound-reject"))
        .await
        .expect("register owner-bound");
    assert!(
        registry
            .complete_process(
                "auth-ownerbound-reject",
                success(serde_json::json!("no")),
                ProcessCompletionAuthority::external_owner(),
            )
            .await
            .is_err(),
        "external-owner authority must not close a lash-executed owner-bound row"
    );

    // Rerunnable: same as OwnerBound — WorkflowKey closes it, ExternalOwner is
    // rejected.
    registry
        .register_process(rerunnable_registration("auth-rerun-accept"))
        .await
        .expect("register rerunnable");
    registry
        .complete_process(
            "auth-rerun-accept",
            success(serde_json::json!("ran")),
            ProcessCompletionAuthority::workflow_key("auth-rerun-accept"),
        )
        .await
        .expect("workflow-key closes a rerunnable row it ran");

    registry
        .register_process(rerunnable_registration("auth-rerun-reject"))
        .await
        .expect("register rerunnable");
    assert!(
        registry
            .complete_process(
                "auth-rerun-reject",
                success(serde_json::json!("no")),
                ProcessCompletionAuthority::external_owner(),
            )
            .await
            .is_err(),
        "external-owner authority must not close a lash-executed rerunnable row"
    );
}

/// The unleased `complete_process` validates the authority against the row it
/// actually appends to — not a disposition read in a separate step. Sequential
/// proxy for the crash-race (finding 6): complete a row, prune it, re-register
/// the same id with a *different* disposition, then retry the completion with the
/// authority that was valid for the OLD disposition. Because validation reads the
/// live row inside the completion, the retried completion is rejected and appends
/// no terminal event to the re-registered row.
pub(super) async fn completion_authority_reads_live_disposition_not_stale(
    registry: Arc<dyn ProcessRegistry>,
) {
    // Round 1: an externally-owned row, closed by an external owner.
    registry
        .register_process(registration("auth-reregister"))
        .await
        .expect("register externally-owned");
    registry
        .complete_process(
            "auth-reregister",
            success(serde_json::json!("first")),
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("external owner closes the externally-owned row");

    // Prune the terminal row so the id is free to be re-registered. The cutoff is
    // far-future but still fits in an `i64` (the store backends bind it as one).
    registry
        .prune_terminal_processes(u64::MAX >> 1, None, None)
        .await
        .expect("prune terminal row");
    assert!(
        registry.get_process("auth-reregister").await.is_none(),
        "the terminal row is pruned and the id is free"
    );

    // Round 2: re-register the SAME id with a different disposition (owner-bound),
    // for which an external-owner authority is invalid.
    registry
        .register_process(owner_bound_registration("auth-reregister"))
        .await
        .expect("re-register owner-bound under the reused id");

    // The stale external-owner authority — valid for the pruned externally-owned
    // row — must be rejected against the live owner-bound disposition, and must
    // append no terminal event.
    assert!(
        registry
            .complete_process(
                "auth-reregister",
                success(serde_json::json!("stale")),
                ProcessCompletionAuthority::external_owner(),
            )
            .await
            .is_err(),
        "a completion validated against a stale disposition must be rejected"
    );
    assert!(
        registry
            .get_process("auth-reregister")
            .await
            .is_some_and(|record| !record.is_terminal()),
        "the rejected completion appends no terminal event to the re-registered row"
    );

    // The authority valid for the live disposition still closes the row.
    registry
        .complete_process(
            "auth-reregister",
            success(serde_json::json!("ran")),
            ProcessCompletionAuthority::workflow_key("auth-reregister"),
        )
        .await
        .expect("workflow-key closes the re-registered owner-bound row");
}
