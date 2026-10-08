//! [`DeploymentStore`](crate::DeploymentStore) conformance: create,
//! reopen, delete, and session metadata.

use super::session_store_factory_enumeration::session_store_factory_enumeration_is_read_only_and_keeps_tombstones;
use super::session_store_factory_vacuum::{
    session_store_factory_delete_takes_the_sessions_pins,
    session_store_factory_vacuum_agrees_on_unpin_before_delete,
    session_store_factory_vacuum_is_scoped_to_bound_session,
};
use super::*;
use crate::ActorContext;
use lash_sansio::SessionId;
use lash_sansio::TurnId;
use pretty_assertions::assert_eq;

mod admission;
mod identity_claims;
pub use identity_claims::*;
mod lifecycle_states;
pub use lifecycle_states::a_closing_session_lists_as_closing_never_as_live;
mod owning_process;
pub use owning_process::session_meta_records_the_process_that_owns_it;
mod process_successor;
pub use process_successor::a_same_start_key_successor_after_prune_has_independent_attachment_referrers;
#[path = "session_store_factory_attachment_fence.rs"]
mod attachment_fence;
#[path = "session_store_factory_config_commands.rs"]
mod config_commands;
#[path = "session_store_factory_creation_budget.rs"]
mod creation_budget;
pub use config_commands::{
    cancelled_session_config_settlement_is_typed,
    session_config_settlement_pending_returns_without_wait,
    superseded_config_settlement_adopts_the_newer_head,
};
pub use creation_budget::session_creation_refuses_a_head_no_commit_fits;
mod state_version;

/// `make` must return a fresh, empty factory on each call.
///
/// `make_attached` returns a fresh factory together with the attachment byte
/// store of the same substrate, for the laws that sweep bytes against the
/// factory's roots.
pub async fn session_store_factory<F, A>(make: F, make_attached: A)
where
    F: Fn() -> Arc<dyn crate::store::ConformanceDeployment>,
    A: Fn() -> (
        Arc<dyn crate::store::ConformanceDeployment>,
        Arc<dyn crate::AttachmentStore>,
    ),
{
    let first = make();
    let second = make();
    assert_fresh_instances(&first, &second, "session_store_factory");
    drop((first, second));
    state_version::session_state_version_admission_contract(make()).await;
    super::hostile_input::session_namespace(make()).await;
    admission::session_admission_contract(make()).await;
    session_store_binding_is_catalog_cardinality_independent(&make).await;
    session_store_factory_open_missing_returns_none(make()).await;
    session_store_factory_create_seeds_and_reopens_meta(make()).await;
    session_store_factory_round_trips_every_relation_shape(make()).await;
    session_store_factory_create_is_idempotent(make()).await;
    session_store_factory_enumeration_is_read_only_and_keeps_tombstones(make()).await;
    session_store_factory_admissible_queued_work_peek(make()).await;
    session_store_factory_never_used_delete_is_noop(make()).await;
    session_store_factory_rejects_writes_after_delete(make()).await;
    let (factory, attachments) = make_attached();
    attachment_reference_lifecycle_with_store(factory, attachments).await;
    session_store_factory_pending_write_is_a_root(make()).await;
    attachment_fence::session_store_factory_attachment_gc_fence_state_machine(make()).await;
    let (factory, attachments) = make_attached();
    session_store_factory_fenced_sweep_collects_and_records_reclaimed(factory, attachments).await;
    session_store_factory_rejects_cross_session_graph_parents(make()).await;
    session_store_factory_fork_semantics(make()).await;
    session_store_factory_delete_takes_the_sessions_pins(make()).await;
    session_store_factory_vacuum_is_scoped_to_bound_session(make()).await;
    session_store_factory_vacuum_agrees_on_unpin_before_delete(make()).await;
    session_store_factory_delete_removes_store_and_is_idempotent(make()).await;
    session_store_factory_delete_fences_stale_handles(make()).await;
}

/// Hold a backend to the read-only session-view contract.
///
/// The factory must expose committed history, failure evidence and usage while another
/// handle writes the session. Reading must leave that writer's
/// authority intact, and deleting the session must produce the same absent
/// read disposition as the ordinary live-open surface.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn session_store_factory_read_session(factory: Arc<dyn crate::DeploymentStore>) {
    const SESSION_ID: &str = "read-only-session-view";
    let expected_relation = crate::SessionRelation::Child {
        parent_session_id: SessionId::from("read-only-session-parent"),
        caused_by: Some(crate::CausalRef::Turn {
            session_id: SessionId::from("read-only-session-parent"),
            turn_id: TurnId::from("read-only-session-parent-turn"),
        }),
    };
    let request = session_store_request(
        &SessionId::from(SESSION_ID),
        "read-only-session-model",
        expected_relation.clone(),
    );
    assert!(
        factory
            .read_view(&SessionId::from(SESSION_ID))
            .await
            .expect("read a missing session")
            .is_none(),
        "a missing session has no read view"
    );

    let writer = factory
        .admit_view(&request)
        .await
        .expect("create read-session writer");
    let mut state = crate::RuntimeSessionState {
        session_id: SessionId::fixture(SESSION_ID.to_string()),
        token_usage: crate::TokenUsage {
            input_tokens: 11,
            output_tokens: 7,
            ..Default::default()
        },
        ..crate::RuntimeSessionState::new(request.config.session_policy())
    };
    state.append_active_conversation_messages(&[crate::Message {
        id: "read-only-session-message".to_string(),
        role: crate::MessageRole::User,
        parts: vec![crate::Part::text(
            "read-only-session-message.p0".to_string(),
            "committed before inspection".to_string(),
            None,
        )]
        .into(),
        origin: None,
        reply_marker: None,
    }]);
    let partial_text = "provider-visible prefix before the stream failed";
    let failure_evidence = crate::TurnFailureEvidence {
        partial_output: Some(crate::TurnFailurePartialOutput::Complete {
            text: partial_text.to_string(),
        }),
        billed_usage: crate::llm::types::LlmUsage {
            input_tokens: 17,
            output_tokens: 5,
            ..Default::default()
        },
        refusal: crate::ChargeSafetyRefusalEvidence {
            denial_reason: crate::ChargeSafetyDenialReason::GuaranteeRequired,
            protocol_position: crate::ProtocolPosition::OutputStarted,
            attempt_number: 1,
            attempt_count: 1,
        },
    };
    let mut commit = crate::RuntimeCommit::persisted_state_for_test(&state);
    commit.failure_evidence = vec![failure_evidence.clone()];
    writer
        .commit_runtime_state(commit)
        .await
        .expect("commit readable session state");

    let view = factory
        .read_view(&SessionId::from(SESSION_ID))
        .await
        .expect("read alongside live writer")
        .expect("committed session has a read view");
    assert_eq!(view.session_id(), SESSION_ID);
    assert_eq!(view.durable_relation(), Some(&expected_relation));
    assert_eq!(view.messages().len(), 1, "history is projected");
    // Failure evidence is not part of the view; it is paged (ADR 0112 §8).
    let settlements = crate::conformance::helpers::load_failure_evidence(
        factory.as_ref(),
        &SessionId::from(SESSION_ID),
    )
    .await
    .expect("page the session's failure evidence");
    assert_eq!(
        settlements.len(),
        1,
        "the failed generation's evidence remains readable after factory reopen"
    );
    assert_eq!(
        settlements[0].evidence,
        vec![failure_evidence],
        "the turn settlement preserves typed partial output, billed usage, and refusal facts"
    );
    assert!(
        view.messages().iter().all(|message| message
            .parts
            .iter()
            .all(|part| !part.content().contains(partial_text))),
        "settlement evidence has no graph/message path into prompt assembly"
    );
    assert_eq!(
        view.token_usage(),
        &crate::TokenUsage {
            input_tokens: 11,
            output_tokens: 7,
            ..Default::default()
        },
        "usage is projected"
    );

    factory
        .delete_session(&SessionId::from(SESSION_ID))
        .await
        .expect("delete read-session fixture");
    assert!(
        factory
            .read_view(&SessionId::from(SESSION_ID))
            .await
            .expect("read deleted session disposition")
            .is_none(),
        "a deleted session is absent from both live-open and read-only surfaces"
    );
}

/// Assert that the first admission of a session into a fresh catalog creates
/// the session's durable metadata, and that admitting it again rebinds.
///
/// `make` must return a fresh, empty catalog; the law names the session it
/// admits (ADR 0112 §1.1).
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn fresh_session_admission_returns_created<F>(make: F)
where
    F: FnOnce(&str) -> Arc<dyn crate::RuntimeStore>,
{
    let request = session_store_request(
        &SessionId::from("fresh-admission-created"),
        "fresh-admission-model",
        crate::SessionRelation::Child {
            parent_session_id: SessionId::from("fresh-admission-parent"),
            caused_by: None,
        },
    );
    let store = make(&request.session_id);

    assert_eq!(
        store
            .admit_session(&request)
            .await
            .expect("admit a fresh session"),
        crate::SessionAdmission::Created
    );
    assert_eq!(
        store
            .admit_session(&request)
            .await
            .expect("admit the same session again"),
        crate::SessionAdmission::Rebound,
        "a second admission with the same lineage rebinds and writes nothing"
    );
}

/// The notification fast path reads durable admissible work without creating a
/// session or hydrating runtime state. Future-only and empty queues are idle.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn session_store_factory_admissible_queued_work_peek(
    factory: Arc<dyn crate::DeploymentStore>,
) {
    let request = session_store_request(
        &SessionId::from("admissible-queued-work-peek"),
        "admissible-queued-work-model",
        crate::SessionRelation::Root,
    );
    assert!(
        !factory
            .has_admissible_queued_work(&request.session_id)
            .await
            .expect("peek a missing session"),
        "a missing session must not report admissible queued work"
    );
    let store = factory
        .admit_view(&request)
        .await
        .expect("create peek conformance store");
    assert!(
        !factory
            .has_admissible_queued_work(&request.session_id)
            .await
            .expect("peek an empty queue"),
        "an empty queue must not report admissible queued work"
    );

    let ready = store
        .enqueue_queued_work(crate::QueuedWorkBatchDraft::new(
            &request.session_id,
            crate::DeliveryPolicy::EarliestSafeBoundary,
            crate::SessionCommand::RefreshToolCatalog {
                reason: "ready".to_string(),
            },
        ))
        .await
        .expect("enqueue queued work");
    assert!(
        factory
            .has_admissible_queued_work(&request.session_id)
            .await
            .expect("peek a populated queue"),
        "queued work must be visible through the factory peek"
    );
    store
        .cancel_queued_work_batch(&ready.batch_id)
        .await
        .expect("cancel queued work")
        .expect("batch remains cancellable");
    assert!(
        !factory
            .has_admissible_queued_work(&request.session_id)
            .await
            .expect("peek after cancelling the only batch"),
        "the queue must report empty again after its batch is removed"
    );

    store
        .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
            &request.session_id,
            crate::TurnInputIngress::NextTurn,
            crate::TurnInput::text("admissible next-turn input"),
        ))
        .await
        .expect("enqueue admissible next-turn input");
    assert!(
        factory
            .has_admissible_queued_work(&request.session_id)
            .await
            .expect("peek an admissible next-turn input"),
        "deferred next-turn input must be visible through the factory peek"
    );
}

/// Deleting a session must erase readable state, fence handles that were
/// opened before the delete, and surface a stale runtime commit as a typed
/// terminal host outcome.
///
/// A store handle held by an in-flight turn outlives `delete_session`. If that
/// stale handle can still read retained state or write, backend behavior
/// depends on object lifetime and the delete can be undone after the host
/// retired the id.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn session_store_factory_delete_fences_stale_handles(
    factory: Arc<dyn crate::store::ConformanceDeployment>,
) {
    let request = session_store_request(
        &SessionId::from("delete-fence-stale-handle"),
        "delete-fence-model",
        crate::SessionRelation::Root,
    );
    let stale = factory
        .admit_view(&request)
        .await
        .expect("admit the session behind the view that will go stale");
    let stale_meta = stale
        .load_session_meta()
        .await
        .expect("load stale handle metadata")
        .expect("stale handle metadata");
    let mut state = crate::RuntimeSessionState {
        session_id: request.session_id.clone(),
        ..crate::RuntimeSessionState::new(request.config.session_policy())
    };
    state.ensure_agent_frame_initialized();
    stale
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(&state))
        .await
        .expect("seed a checkpoint on the handle that will go stale");
    stale
        .enqueue_pending_turn_input(
            crate::PendingTurnInputDraft::new(
                &request.session_id,
                crate::TurnInputIngress::NextTurn,
                crate::TurnInput::text("pending input on the handle that will go stale"),
            )
            .with_source_key("delete-fence-stale-handle:pending-input"),
        )
        .await
        .expect("seed a pending turn input on the handle that will go stale");
    stale
        .enqueue_queued_work(crate::QueuedWorkBatchDraft::new(
            &request.session_id,
            crate::DeliveryPolicy::EarliestSafeBoundary,
            crate::SessionCommand::RefreshToolCatalog {
                reason: "queued work on the handle that will go stale".to_string(),
            },
        ))
        .await
        .expect("seed queued work on the handle that will go stale");
    factory
        .delete_session(&request.session_id)
        .await
        .expect("delete the session out from under the stale handle");

    assert!(
        stale
            .load_session_meta()
            .await
            .expect("read metadata through the stale handle")
            .is_none(),
        "a stale handle must observe deleted session metadata as absent"
    );
    // Every history read of a deleted session answers `SessionDeleted`
    // (ADR 0112 §5), never an empty window.
    let window_error = stale
        .load_session_window(crate::store::WindowSelector::Current)
        .await
        .expect_err("a stale view must not read the deleted session's window");
    assert!(
        matches!(
            window_error,
            crate::StoreError::SessionDeleted { ref session_id }
                if session_id == request.session_id
        ),
        "a stale view must observe the deleted session as deleted, got: {window_error}"
    );
    assert!(
        stale
            .list_pending_turn_inputs()
            .await
            .expect("list pending inputs through the stale handle")
            .is_empty(),
        "a stale handle must observe deleted pending turn inputs as absent"
    );
    assert!(
        stale
            .list_queued_work()
            .await
            .expect("list queued work through the stale handle")
            .is_empty(),
        "a stale handle must observe deleted queued work as absent"
    );
    let ensure_error = stale
        .store()
        .admit_session(&request)
        .await
        .expect_err("a stale view's store must not reinsert deleted session metadata");
    assert!(
        matches!(
            ensure_error,
            crate::StoreError::SessionDeleted { ref session_id }
                if session_id == request.session_id
        ),
        "stale session binding must be fenced as deleted, got: {ensure_error}"
    );
    let save_error = stale
        .settle_observer_intents(stale_meta.pending_observer_intents)
        .await
        .expect_err("a stale handle must not restore deleted session metadata");
    assert!(
        matches!(
            save_error,
            crate::StoreError::SessionDeleted { ref session_id }
                if session_id == request.session_id
        ),
        "stale metadata writes must be fenced as deleted, got: {save_error}"
    );

    let error = stale
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(&state))
        .await
        .expect_err("a stale handle must not resurrect a deleted session");
    assert!(
        matches!(
            error,
            crate::StoreError::SessionDeleted { ref session_id }
                if session_id == request.session_id
        ),
        "a stale commit into a deleted session must be fenced as deleted, got: {error}"
    );
    let runtime_error =
        lash_core::testing::conformance_support::runtime_error_from_store_commit(error);
    assert_eq!(
        runtime_error.code,
        crate::RuntimeErrorCode::SessionDeleted,
        "a commit against a deleted session must reach the host as a typed runtime outcome"
    );
    assert_eq!(
        runtime_error.deleted_session_id(),
        Some(&request.session_id),
        "the typed runtime outcome must retain the deleted session identity"
    );
    assert!(
        runtime_error.is_terminal(),
        "a deleted session cannot be repaired by retrying the commit"
    );
    assert!(
        !runtime_error.is_retryable(),
        "a deleted session must never invite an unchanged retry"
    );
    assert!(
        factory
            .live_view_for(&request)
            .await
            .expect("open after the fenced commit")
            .is_none(),
        "a fenced commit must leave the session deleted"
    );

    let recreate_error = match factory.admit_view(&request).await {
        Ok(_) => panic!("explicit create must not lift a retired session id's fence"),
        Err(error) => error,
    };
    assert_session_id_was_used_and_deleted(recreate_error, &request.session_id);

    let stale_error = stale
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(&state))
        .await
        .expect_err("the pre-delete handle must remain fenced after refused recreation");
    assert!(
        matches!(
            stale_error,
            crate::StoreError::SessionDeleted { ref session_id }
                if session_id == request.session_id
        ),
        "a pre-delete handle must remain fenced after refused recreation, got: {stale_error}"
    );
}

/// Process-retention conformance: a process-scoped cancellation closure first
/// wins the prune race and retains the terminal process. Once the exact owner
/// consumes that authorization, pruning releases attachment intents and
/// removes both durable session stores before the process row disappears.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn ended_process_record_has_no_attachment_edges(
    factory: Arc<dyn crate::DeploymentStore>,
    registry: Arc<dyn crate::ProcessRegistry>,
    effect_host: ActorContext,
) {
    let _ = (registry, effect_host);
    let store: Arc<dyn crate::RuntimeStore> = factory.clone();
    let referrer =
        crate::ArtifactReferrer::ProcessRecord(crate::ProcessId::fixture("pruned-record"));
    let id = crate::AttachmentId::parse("a1".repeat(32)).expect("id");
    let write = crate::AttachmentWrite {
        attachment_id: id.clone(),
        claim: crate::ReferrerClaim::unguarded(referrer.clone()).expect("claim"),
    };
    crate::conformance::helpers::record_completed_attachment_write(&store, write).await;
    store
        .end_attachment_referrer(&referrer)
        .await
        .expect("end record");
    assert!(
        store
            .attachment_referrers(&id)
            .await
            .expect("refs")
            .is_empty()
    );
}

/// Exercise the shared-bytes attachment contract: identical bytes across
/// sessions dedup to one blob, reads resolve across session boundaries, and
/// mark-and-sweep GC collects a blob only once no retained root references it.
pub async fn attachment_reference_lifecycle_with_store(
    factory: Arc<dyn crate::DeploymentStore>,
    backend: Arc<dyn crate::AttachmentStore>,
) {
    crate::conformance::attachment_adoption::cross_session_attachment_adoption_conformance(
        factory,
        Arc::new(move || backend.clone()),
    )
    .await;
}

fn assert_meta_matches_request(meta: &SessionMeta, request: &crate::SessionStoreCreateRequest) {
    assert_eq!(meta.session_id, request.session_id);
    assert_eq!(meta.relation, request.relation);
}

/// A session handle's identity is explicit and independent of how many other
/// sessions exist in the durable catalog.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn session_store_binding_is_catalog_cardinality_independent<F>(make: &F)
where
    F: Fn() -> Arc<dyn crate::store::ConformanceDeployment>,
{
    let empty_factory = make();
    let missing = session_store_request(
        &SessionId::from("binding-cardinality-b"),
        "binding-cardinality-model",
        crate::SessionRelation::Root,
    );
    assert!(
        empty_factory
            .live_view_for(&missing)
            .await
            .expect("query an empty session catalog")
            .is_none(),
        "zero-session catalogs must not invent a session identity"
    );

    for earlier_session_count in 0..=1 {
        let factory = make();
        if earlier_session_count == 1 {
            let earlier = session_store_request(
                &SessionId::from("binding-cardinality-a"),
                "binding-cardinality-model",
                crate::SessionRelation::Root,
            );
            factory
                .admit_view(&earlier)
                .await
                .expect("seed the lexicographically earlier session");
        }

        let target = session_store_request(
            &SessionId::from("binding-cardinality-b"),
            "binding-cardinality-model",
            crate::SessionRelation::Root,
        );
        let store = factory
            .admit_view(&target)
            .await
            .expect("create the explicitly bound target store");
        assert_eq!(
            factory
                .admit_session(&target)
                .await
                .expect("readmit the target session"),
            crate::SessionAdmission::Rebound
        );
        let loaded = store
            .load_session_meta()
            .await
            .expect("read through the same explicitly bound handle")
            .expect("write-bound target metadata exists");
        assert_eq!(
            loaded.session_id,
            "binding-cardinality-b",
            "a write-bound B handle must read B from a catalog containing {} session(s)",
            earlier_session_count + 1
        );
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn session_store_factory_never_used_delete_is_noop(factory: Arc<dyn crate::DeploymentStore>) {
    let request = session_store_request(
        &SessionId::from("never-used-delete"),
        "never-used-model",
        crate::SessionRelation::Root,
    );
    factory
        .delete_session(&request.session_id)
        .await
        .expect("delete never-used id is a no-op");
    factory
        .admit_view(&request)
        .await
        .expect("never-used id remains admissible after no-op delete");
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn session_store_factory_rejects_writes_after_delete(
    factory: Arc<dyn crate::DeploymentStore>,
) {
    let request = session_store_request(
        &SessionId::from("write-after-delete"),
        "write-after-delete-model",
        crate::SessionRelation::Root,
    );
    let stale = factory
        .admit_view(&request)
        .await
        .expect("create write-after-delete fixture");
    factory
        .delete_session(&request.session_id)
        .await
        .expect("delete write-after-delete fixture");

    assert_deleted_write(
        stale
            .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
                &request.session_id,
                crate::TurnInputIngress::NextTurn,
                crate::TurnInput::text("must not persist"),
            ))
            .await,
        &request.session_id,
        "pending turn input",
    );
    assert_deleted_write(
        stale
            .enqueue_queued_work(crate::QueuedWorkBatchDraft::new(
                &request.session_id,
                crate::DeliveryPolicy::EarliestSafeBoundary,
                crate::SessionCommand::RefreshToolCatalog {
                    reason: "must not persist".to_string(),
                },
            ))
            .await,
        &request.session_id,
        "queued work",
    );
    let mut state = crate::RuntimeSessionState {
        session_id: request.session_id.clone(),
        ..crate::RuntimeSessionState::new(request.config.session_policy())
    };
    state.ensure_agent_frame_initialized();
    assert_deleted_write(
        stale
            .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(&state))
            .await,
        &request.session_id,
        "runtime commit",
    );

    factory
        .delete_session(&request.session_id)
        .await
        .expect("re-delete cleans any historical post-delete orphans");
}

fn assert_deleted_write<T>(
    result: Result<T, crate::StoreError>,
    session_id: &SessionId,
    surface: &str,
) {
    let error = match result {
        Ok(_) => panic!("{surface} write unexpectedly succeeded"),
        Err(error) => error,
    };
    assert!(
        matches!(
            error,
            crate::StoreError::SessionDeleted {
                session_id: ref deleted
            } if deleted == session_id
        ),
        "{surface} write must fail with SessionDeleted, got {error:?}"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn session_store_factory_open_missing_returns_none(factory: Arc<dyn crate::DeploymentStore>) {
    let request = session_store_request(
        &SessionId::from("missing-session"),
        "missing-model",
        crate::SessionRelation::Root,
    );
    let opened = factory
        .live_view_for(&request)
        .await
        .expect("open missing session");
    assert!(
        opened.is_none(),
        "lookup_session must answer Absent for unknown sessions"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn session_store_factory_create_seeds_and_reopens_meta(
    factory: Arc<dyn crate::DeploymentStore>,
) {
    let relation = crate::SessionRelation::Child {
        parent_session_id: SessionId::from("parent-session"),
        caused_by: None,
    };
    let request = session_store_request(&SessionId::from("session-a"), "model-a", relation);

    let created = factory
        .admit_view(&request)
        .await
        .expect("create session store");
    let created_meta = created
        .load_session_meta()
        .await
        .expect("load created session meta")
        .expect("created session meta");
    assert_meta_matches_request(&created_meta, &request);

    let reopened = factory
        .live_view_for(&request)
        .await
        .expect("open existing session store")
        .expect("existing session store");
    let reopened_meta = reopened
        .load_session_meta()
        .await
        .expect("load reopened session meta")
        .expect("reopened session meta");
    assert_meta_matches_request(&reopened_meta, &request);
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn session_store_factory_round_trips_every_relation_shape(
    factory: Arc<dyn crate::DeploymentStore>,
) {
    let child = |caused_by| crate::SessionRelation::Child {
        parent_session_id: SessionId::from("roundtrip-parent"),
        caused_by,
    };
    let relations = vec![
        ("root", crate::SessionRelation::Root),
        ("child-none", child(None)),
        (
            "child-turn",
            child(Some(crate::CausalRef::Turn {
                session_id: SessionId::from("cause-session"),
                turn_id: TurnId::from("cause-turn"),
            })),
        ),
        (
            "child-effect-runtime-operation",
            child(Some(crate::CausalRef::Effect {
                address: EffectAddress::new(
                    ExecutionScope::runtime_operation("cause-operation"),
                    "cause-effect",
                )
                .expect("valid operation effect cause"),
            })),
        ),
        (
            "child-effect-turn",
            child(Some(crate::CausalRef::Effect {
                address: EffectAddress::new(
                    ExecutionScope::turn("cause-session", "cause-turn"),
                    "cause-effect",
                )
                .expect("valid turn effect cause"),
            })),
        ),
        (
            "child-effect-process",
            child(Some(crate::CausalRef::Effect {
                address: EffectAddress::new(
                    ExecutionScope::process(crate::ProcessId::fixture("cause-process")),
                    "cause-effect",
                )
                .expect("valid process effect cause"),
            })),
        ),
        (
            "child-effect-session-operation",
            child(Some(crate::CausalRef::Effect {
                address: EffectAddress::new(
                    ExecutionScope::session_operation("cause-session", "cause-drain"),
                    "cause-effect",
                )
                .expect("valid session-operation effect cause"),
            })),
        ),
        (
            "child-effect-session-delete",
            child(Some(crate::CausalRef::Effect {
                address: EffectAddress::new(
                    ExecutionScope::session_delete("cause-session"),
                    "cause-effect",
                )
                .expect("valid session-delete effect cause"),
            })),
        ),
        (
            "child-tool-call",
            child(Some(crate::CausalRef::ToolCall {
                session_id: SessionId::from("cause-session"),
                call_id: lash_core::ToolCallId::fixture("cause-call"),
            })),
        ),
        (
            "child-process",
            child(Some(crate::CausalRef::Process {
                process_id: crate::ProcessId::fixture("cause-process"),
            })),
        ),
        (
            "child-process-event",
            child(Some(crate::CausalRef::ProcessEvent {
                process_id: crate::ProcessId::fixture("cause-process"),
                sequence: u64::MAX,
            })),
        ),
        (
            "child-session-node",
            child(Some(crate::CausalRef::SessionNode {
                session_id: SessionId::from("cause-session"),
                node_id: "cause-node".to_string(),
            })),
        ),
        (
            "fork-empty",
            crate::SessionRelation::Fork {
                source_session_id: SessionId::from("declared-missing-session"),
                source_node_id: Some("declared-missing-node".into()),
            },
        ),
        (
            "fork-leafless",
            crate::SessionRelation::Fork {
                source_session_id: SessionId::from("declared-missing-session"),
                source_node_id: None,
            },
        ),
    ];

    for (label, relation) in relations {
        let session_id = SessionId::fixture(format!("session-meta-roundtrip-{label}"));
        // The relation is created with the store: `settle_observer_intents` may not
        // move a recorded lineage (FIG-3045), so the round trip declares it at
        // admission and then rewrites only the rest of the record.
        let request = session_store_request(
            &session_id,
            "session-meta-roundtrip-model",
            relation.clone(),
        );
        let store = factory
            .admit_view(&request)
            .await
            .unwrap_or_else(|error| panic!("create {label} relation store: {error}"));
        let expected = SessionMeta {
            owning_process_id: None,
            pending_observer_intents: vec![
                crate::SessionObserverIntent::host_requested(crate::ProcessId::fixture(
                    "observer-a",
                )),
                crate::SessionObserverIntent::host_requested(crate::ProcessId::fixture(
                    "observer-b",
                )),
            ],
            session_id: session_id.clone(),
            relation,
        };
        store
            .settle_observer_intents((expected.clone()).pending_observer_intents)
            .await
            .unwrap_or_else(|error| panic!("save {label} relation: {error}"));
        let loaded = store
            .load_session_meta()
            .await
            .unwrap_or_else(|error| panic!("load {label} relation: {error}"))
            .unwrap_or_else(|| panic!("{label} relation metadata exists"));
        assert_eq!(loaded, expected, "{label} relation must round-trip");

        let reopened = factory
            .live_view_for(&request)
            .await
            .unwrap_or_else(|error| panic!("reopen {label} relation store: {error}"))
            .unwrap_or_else(|| panic!("{label} relation store exists"));
        assert_eq!(
            reopened
                .load_session_meta()
                .await
                .unwrap_or_else(|error| panic!("reload {label} relation: {error}")),
            Some(expected),
            "{label} relation must survive reopen"
        );
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn session_store_factory_create_is_idempotent(factory: Arc<dyn crate::DeploymentStore>) {
    // The relation is declared at creation: a later `settle_observer_intents` may
    // not move a recorded lineage (FIG-3045).
    let initial = session_store_request(
        &SessionId::from("stable-session"),
        "initial-model",
        crate::SessionRelation::Child {
            parent_session_id: SessionId::from("custom-parent"),
            caused_by: None,
        },
    );
    let _created = factory
        .admit_view(&initial)
        .await
        .expect("create stable session");

    let changed = session_store_request(
        &SessionId::from("stable-session"),
        "changed-model",
        crate::SessionRelation::Root,
    );
    let recreated = factory
        .admit_view(&changed)
        .await
        .expect("recreate stable session");
    let meta = recreated
        .load_session_meta()
        .await
        .expect("load recreated meta")
        .expect("recreated meta");
    assert_eq!(
        meta.parent_session_id().map(SessionId::as_str),
        Some("custom-parent"),
        "admit_session must preserve the original relation"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn session_store_factory_rejects_cross_session_graph_parents(
    factory: Arc<dyn crate::DeploymentStore>,
) {
    let first_request = session_store_request(
        &SessionId::from("graph-parent-owner"),
        "graph-parent-model",
        crate::SessionRelation::Root,
    );
    let second_request = session_store_request(
        &SessionId::from("graph-parent-intruder"),
        "graph-parent-model",
        crate::SessionRelation::Root,
    );
    let first = factory
        .admit_view(&first_request)
        .await
        .expect("create graph parent owner");
    let second = factory
        .admit_view(&second_request)
        .await
        .expect("create graph parent intruder");
    let mut first_state = crate::RuntimeSessionState {
        session_id: first_request.session_id.clone(),
        ..crate::RuntimeSessionState::new(first_request.config.session_policy())
    };
    first_state.ensure_agent_frame_initialized();
    first
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(&first_state))
        .await
        .expect("commit graph parent owner frame");
    let foreign_parent = first_state
        .current_frame_node_id
        .clone()
        .expect("owner frame node id");
    let mut second_state = crate::RuntimeSessionState {
        session_id: second_request.session_id.clone(),
        ..crate::RuntimeSessionState::new(second_request.config.session_policy())
    };
    second_state.ensure_agent_frame_initialized();
    let second_result = second
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(
            &second_state,
        ))
        .await
        .expect("commit intruder's own frame");
    second_state.apply_persisted_commit_result(second_result);
    assert!(
        !crate::conformance::helpers::node_readable(&second, &foreign_parent)
            .await
            .expect("probe unrelated history"),
        "a bound store must not expose an unrelated session's node"
    );
    let child = crate::SessionNodeRecord {
        node_id: "cross-session-child".into(),
        parent_node_id: Some(crate::NodeId::fixture(foreign_parent.to_string())),
        timestamp: "2026-07-27T00:00:00.000000000Z"
            .parse()
            .expect("canonical node timestamp"),
        payload: crate::SessionNodePayload::Event {
            event: crate::SessionHistoryRecord::Protocol(
                crate::ProtocolEvent::typed("cross-session", serde_json::Value::Null)
                    .expect("protocol event"),
            ),
        },
    };
    let state = crate::RuntimeSessionState {
        head_revision: second_state.head_revision,
        persisted_node_ids: second_state.persisted_node_ids,
        session_id: second_state.session_id,
        current_frame_node_id: Some(foreign_parent),
        ..crate::RuntimeSessionState::new(second_request.config.session_policy())
    };
    let commit = crate::RuntimeCommit::persisted_state_with_graph_commit(
        &state,
        crate::GraphAppend::Extend { nodes: vec![child] },
    );
    let child_node_id = commit.graph.nodes()[0].node_id.clone();
    let error = second
        .commit_runtime_state(commit)
        .await
        .expect_err("a graph parent must belong to the committing session");
    assert!(match &error {
        crate::StoreError::InvalidGraphParent { node_id, .. } => node_id == child_node_id,
        crate::StoreError::MissingFrameOpenAncestor { leaf_node_id } => {
            leaf_node_id == child_node_id
        }
        _ => false,
    });
    let intruder_after_rejection = second
        .load_session_window(crate::store::WindowSelector::Current)
        .await
        .expect("load intruder after rejection")
        .expect("intruder head survives rejection");
    assert_eq!(
        intruder_after_rejection.window.nodes.len(),
        1,
        "cross-session parent rejection must be atomic"
    );
}

/// First-class fork contract shared by SQLite and PostgreSQL: a fork names a
/// retained revision of its source, every published revision is retained until
/// the host collects, pins are idempotent names, forks write no graph nodes,
/// and deleting either sibling cannot reclaim the prefix still reachable from
/// the other.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn session_store_factory_fork_semantics(factory: Arc<dyn crate::DeploymentStore>) {
    let source_request = session_store_request(
        &SessionId::from("fork-source"),
        "fork-model",
        crate::SessionRelation::Root,
    );
    let source = factory
        .admit_view(&source_request)
        .await
        .expect("create fork source");
    let mut state = crate::RuntimeSessionState {
        session_id: source_request.session_id.clone(),
        ..crate::RuntimeSessionState::new(source_request.config.session_policy())
    };
    state.set_execution_state_snapshot(Some(vec![0xFA, 0xCE].into()));
    state.ensure_agent_frame_initialized();
    let root_ids = state
        .session_graph
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .collect::<Vec<_>>();
    let root_node_id = state
        .session_graph
        .leaf_node_id
        .clone()
        .expect("source root leaf");
    let first = source
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(&state))
        .await
        .expect("commit fork root");
    state.apply_persisted_commit_result(first);
    state.mark_node_ids_persisted(root_ids);

    let source_id = source_request.session_id.clone();
    let root_revision = state.head_revision;
    let root_target = crate::Target::Revision(root_revision);
    factory
        .pin(&source_id, &root_target)
        .await
        .expect("pin fork root");
    let pinned = factory
        .resolve_target(&source_id, &root_target)
        .await
        .expect("resolve pinned root");
    assert_eq!(pinned.leaf_node_id.as_ref(), Some(&root_node_id));
    assert_eq!(pinned.session_id, source_id);
    assert_eq!(pinned.config.model, state.policy.model);
    assert_eq!(pinned.pinned_by, vec![root_target.clone()]);
    assert!(
        pinned.head,
        "the committed root is the head until it advances"
    );

    append_conformance_event_node(&mut state, "source-child", "source child");
    commit_conformance_state(source.store(), &mut state)
        .await
        .expect("advance source past pinned root");
    let unpinned_past_node_id = state
        .session_graph
        .leaf_node_id
        .clone()
        .expect("source child leaf");
    let unpinned_past_revision = state.head_revision;
    append_conformance_event_node(&mut state, "source-tip", "source tip");
    commit_conformance_state(source.store(), &mut state)
        .await
        .expect("advance source past unpinned child");
    let source_tip_node_id = state
        .session_graph
        .leaf_node_id
        .clone()
        .expect("source tip leaf");
    let tip_revision = state.head_revision;
    let tip_target = crate::Target::Revision(tip_revision);
    let (first_pin, second_pin) = tokio::join!(
        factory.pin(&source_id, &tip_target),
        factory.pin(&source_id, &tip_target)
    );
    first_pin.expect("first concurrent pin succeeds");
    second_pin.expect("second concurrent pin is idempotent");
    let revisions = factory
        .revisions(&source_id)
        .await
        .expect("enumerate the source's retained revisions");
    assert_eq!(
        revisions
            .iter()
            .map(|revision| revision.head_revision)
            .collect::<Vec<_>>(),
        (0..=tip_revision).collect::<Vec<_>>(),
        "every published revision is retained until the host collects"
    );
    let tip = revisions.last().expect("the tip is retained");
    assert!(tip.head);
    assert_eq!(
        tip.pinned_by,
        vec![tip_target.clone()],
        "a pin written twice is one pin"
    );
    factory
        .unpin(&source_id, &tip_target)
        .await
        .expect("remove concurrent pin");
    factory
        .unpin(&source_id, &tip_target)
        .await
        .expect("releasing a pin that is not there changes nothing");

    let delete_first_request = crate::ForkSessionRequest {
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from("aaa-fork-delete-first"),
        source_session_id: source_id.clone(),
        head_revision: tip_revision,
        relation: crate::SessionRelation::Root,
        config: crate::PersistedSessionConfig::from_policy(
            &source_request.config.session_policy(),
            crate::SessionToolAccess::ambient(),
        ),
    };
    factory
        .fork_session(&delete_first_request)
        .await
        .expect("fork live source tip");
    factory
        .delete_session(&delete_first_request.session_id)
        .await
        .expect("delete branch before source");
    assert!(
        crate::conformance::helpers::node_readable(&source, &source_tip_node_id)
            .await
            .expect("load source tip after branch-first delete"),
        "deleting a branch first must not reclaim its live source sibling"
    );

    // A past turn nobody pinned is still a retained revision until the host
    // collects, so it forks.
    let past_fork = factory
        .fork_session(&crate::ForkSessionRequest {
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from("fork-unpinned-past"),
            source_session_id: source_id.clone(),
            head_revision: unpinned_past_revision,
            relation: crate::SessionRelation::Root,
            config: crate::PersistedSessionConfig::from_policy(
                &source_request.config.session_policy(),
                crate::SessionToolAccess::ambient(),
            ),
        })
        .await
        .expect("an unpinned past turn forks before any collection");
    assert_eq!(
        past_fork.leaf_node_id.as_ref(),
        Some(&unpinned_past_node_id)
    );
    factory
        .delete_session(&past_fork.session_id)
        .await
        .expect("remove the past-turn fork");

    // A revision the source has not published refuses; the head is never
    // forked in its place.
    let unpublished = factory
        .fork_session(&crate::ForkSessionRequest {
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from("fork-unpublished"),
            source_session_id: source_id.clone(),
            head_revision: tip_revision + 1,
            relation: crate::SessionRelation::Root,
            config: crate::PersistedSessionConfig::from_policy(
                &source_request.config.session_policy(),
                crate::SessionToolAccess::ambient(),
            ),
        })
        .await
        .expect_err("a revision the source never published must not fork");
    assert!(matches!(
        unpublished,
        crate::StoreError::ForkTargetPruned { session_id, target }
            if session_id == source_id
                && target == crate::Target::Revision(tip_revision + 1)
    ));
    assert!(matches!(
        factory
            .resolve_target(&source_id, &crate::Target::Revision(tip_revision + 1))
            .await,
        Err(crate::StoreError::ForkTargetPending { .. })
    ));

    let fork_request = crate::ForkSessionRequest {
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from("fork-branch"),
        source_session_id: source_id.clone(),
        head_revision: root_revision,
        relation: crate::SessionRelation::Root,
        config: crate::PersistedSessionConfig::from_policy(
            &source_request.config.session_policy(),
            crate::SessionToolAccess::ambient(),
        ),
    };
    let forked = factory
        .fork_session(&fork_request)
        .await
        .expect("fork pinned root");
    assert_eq!(forked.leaf_node_id.as_ref(), Some(&root_node_id));
    assert_eq!(forked.head_revision, root_revision);

    // A `Fork` relation is host-declared lineage, not a store-validated
    // argument: forks are addressed by revision, and repeated rewinds
    // legitimately name superseded intermediate sessions (FIG-1174).
    let lineage_relation_fork = factory
        .fork_session(&crate::ForkSessionRequest {
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from("fork-relation-lineage"),
            source_session_id: source_id.clone(),
            head_revision: root_revision,
            relation: crate::SessionRelation::Fork {
                source_session_id: SessionId::from("no-such-session"),
                source_node_id: Some("no-such-node".into()),
            },
            config: crate::PersistedSessionConfig::from_policy(
                &source_request.config.session_policy(),
                crate::SessionToolAccess::ambient(),
            ),
        })
        .await
        .expect("fork relation lineage must not gate a retained revision");
    assert_eq!(
        lineage_relation_fork.source_session_id, source_id,
        "fork result reports the forked session, never the relation's declared lineage"
    );
    factory
        .delete_session(&SessionId::from("fork-relation-lineage"))
        .await
        .expect("remove lineage fork");
    let branch = factory
        .live_view_for(&crate::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: fork_request.session_id.clone(),
            relation: fork_request.relation.clone(),
            config: fork_request.config.clone(),
            head: crate::SessionCreationHead::Config,
        })
        .await
        .expect("open fork")
        .expect("fork exists");
    let branch_read = branch
        .load_session_window(crate::store::WindowSelector::Current)
        .await
        .expect("load fork")
        .expect("fork head");
    assert_eq!(branch_read.head_revision, 0);
    assert_eq!(branch_read.window.nodes.len(), 1, "fork writes zero nodes");
    assert_eq!(
        branch_read.window.leaf_node_id.as_deref(),
        Some(root_node_id.as_str())
    );
    assert_eq!(
        branch_read.checkpoint.as_ref().and_then(|checkpoint| {
            checkpoint.component_body(crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT)
        }),
        Some(&[0xFA, 0xCE][..]),
        "fork inherits the retained continuation checkpoint"
    );
    let mut branch_state =
        crate::conformance::helpers::load_window_state(branch.store(), branch.session_id())
            .await
            .expect("load fork state")
            .expect("fork state exists");
    append_conformance_event_node(&mut branch_state, "branch-child", "branch child");
    commit_conformance_state(branch.store(), &mut branch_state)
        .await
        .expect("advance fork independently");
    let branch_leaf = branch_state
        .session_graph
        .leaf_node_id
        .clone()
        .expect("branch leaf");
    assert_ne!(
        branch_leaf,
        state
            .session_graph
            .leaf_node_id
            .clone()
            .expect("source leaf"),
        "siblings must navigate independently"
    );

    // Composed rewind: the host forked the target, then deletes the
    // superseded source. The fork remains a valid, independently writable
    // session and the shared prefix survives. The source's pins and retained
    // revisions go with it.
    factory
        .delete_session(&source_request.session_id)
        .await
        .expect("delete superseded source");
    assert!(matches!(
        factory.resolve_target(&source_id, &root_target).await,
        Err(crate::StoreError::SessionDeleted { session_id }) if session_id == source_id
    ));
    assert!(
        crate::conformance::helpers::node_readable(&branch, &root_node_id)
            .await
            .expect("load shared prefix after source delete"),
        "deleting one branch must stop at the first still-referenced node"
    );
    let branch_origin = factory
        .resolve_target(&fork_request.session_id, &crate::Target::Revision(0))
        .await
        .expect("the fork's own first revision is retained");
    assert_eq!(branch_origin.leaf_node_id.as_ref(), Some(&root_node_id));

    let recreate_error = match factory.admit_view(&source_request).await {
        Ok(_) => panic!("a deleted source session id must never be reused"),
        Err(error) => error,
    };
    assert_session_id_was_used_and_deleted(recreate_error, &source_request.session_id);

    let fork_reuse_error = factory
        .fork_session(&crate::ForkSessionRequest {
            pending_observer_intents: Vec::new(),
            session_id: source_request.session_id.clone(),
            source_session_id: fork_request.session_id.clone(),
            head_revision: 0,
            relation: crate::SessionRelation::Root,
            config: crate::PersistedSessionConfig::from_policy(
                &source_request.config.session_policy(),
                crate::SessionToolAccess::ambient(),
            ),
        })
        .await
        .expect_err("forking must reject a previously deleted target session id");
    assert_session_id_was_used_and_deleted(fork_reuse_error, &source_request.session_id);
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn session_store_factory_delete_removes_store_and_is_idempotent(
    factory: Arc<dyn crate::DeploymentStore>,
) {
    let request = session_store_request(
        &SessionId::from("delete-session"),
        "delete-model",
        crate::SessionRelation::Root,
    );
    let created = factory
        .admit_view(&request)
        .await
        .expect("create deleted session");
    let mut state = crate::RuntimeSessionState {
        session_id: request.session_id.clone(),
        ..crate::RuntimeSessionState::new(request.config.session_policy())
    };
    state.ensure_agent_frame_initialized();
    let frame = state
        .session_graph
        .nodes
        .first()
        .map(|node| node.as_ref().clone())
        .expect("initial frame node");
    let frame_node_id = frame.node_id.clone();
    let child_node = |node_id: &str| crate::SessionNodeRecord {
        node_id: crate::NodeId::fixture(node_id.to_string()),
        parent_node_id: Some(frame_node_id.clone()),
        timestamp: "2026-07-27T00:00:00.000000000Z"
            .parse()
            .expect("canonical node timestamp"),
        payload: crate::SessionNodePayload::Event {
            event: crate::SessionHistoryRecord::Protocol(
                crate::ProtocolEvent::typed(node_id, serde_json::Value::Null)
                    .expect("protocol event"),
            ),
        },
    };
    let live_leaf = child_node("delete-live-leaf");
    state.session_graph = crate::SessionGraph::from_nodes(
        vec![frame, live_leaf.clone()],
        Some(live_leaf.node_id.clone()),
    )
    .expect("delete-session fixture graph is valid");
    created
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(&state))
        .await
        .expect("commit graph chain before delete");
    created
        .enqueue_pending_turn_input(
            crate::PendingTurnInputDraft::new(
                &request.session_id,
                crate::TurnInputIngress::NextTurn,
                crate::TurnInput::text("pending input before delete"),
            )
            .with_source_key("delete-session:pending-input"),
        )
        .await
        .expect("enqueue pending turn input before delete");
    assert_eq!(
        created
            .list_pending_turn_inputs()
            .await
            .expect("list pending input before delete")
            .len(),
        1
    );
    assert!(
        factory
            .live_view_for(&request)
            .await
            .expect("open before delete")
            .is_some(),
        "session must exist before delete"
    );

    factory
        .delete_session(&request.session_id)
        .await
        .expect("delete session");
    for node_id in [&frame_node_id, &live_leaf.node_id] {
        assert!(
            !crate::conformance::helpers::node_readable_through_deleted(&created, node_id)
                .await
                .expect("load reclaimed node through stale handle"),
            "delete_session must physically reclaim graph node {node_id}"
        );
    }
    assert!(
        factory
            .live_view_for(&request)
            .await
            .expect("open after delete")
            .is_none(),
        "delete_session must remove the session store"
    );
    factory
        .delete_session(&request.session_id)
        .await
        .expect("second delete must be idempotent");

    let recreate_error = match factory.admit_view(&request).await {
        Ok(_) => panic!("a deleted session id must not be reusable"),
        Err(error) => error,
    };
    assert_session_id_was_used_and_deleted(recreate_error, &request.session_id);

    created
        .vacuum()
        .await
        .expect("vacuum after deletion must preserve the permanent id tombstone");
    let after_vacuum_error = match factory.admit_view(&request).await {
        Ok(_) => panic!("vacuum must not make a deleted session id reusable"),
        Err(error) => error,
    };
    assert_session_id_was_used_and_deleted(after_vacuum_error, &request.session_id);
}

fn assert_session_id_was_used_and_deleted(error: crate::StoreError, session_id: &SessionId) {
    assert!(
        matches!(
            &error,
            crate::StoreError::SessionDeleted {
                session_id: deleted
            } if deleted == session_id
        ),
        "reuse must fail with StoreError::SessionDeleted, got {error:?}"
    );
    assert!(
        error.to_string().contains("was used and deleted"),
        "reuse error must explain that the id was used and deleted: {error}"
    );
}

/// A fenced sweep still collects real garbage, reports itself fenced, records
/// the terminal reclaimed fact, and — the fence's whole claim — deletes nothing
/// that a root existed for.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn session_store_factory_fenced_sweep_collects_and_records_reclaimed(
    factory: Arc<dyn crate::store::ConformanceDeployment>,
    backend: Arc<dyn crate::AttachmentStore>,
) {
    let request = session_store_request(
        &SessionId::from("attachment-gc-fenced-sweep"),
        "attachment-gc-fenced-sweep-model",
        crate::SessionRelation::Root,
    );
    let store = factory
        .admit_view(&request)
        .await
        .expect("create session store");
    let orphan = crate::AttachmentStore::put(
        backend.as_ref(),
        b"conformance-fenced-orphan".to_vec(),
        lash_sansio::AttachmentCreateMeta::new(
            lash_sansio::MediaType::parse("application/octet-stream").expect("media type"),
            None,
            Some("orphan".to_string()),
        ),
    )
    .await
    .expect("put orphan blob");

    let report = crate::attachments::reclaim_unreferenced_attachments(
        &*factory,
        backend.as_ref(),
        crate::AttachmentReclamationPolicy {
            grace_period_ms: 0,
            empty_root_set: crate::EmptyRootSetPolicy::AuthorizeDeleteAll,
        },
    )
    .await
    .expect("sweep");

    assert_eq!(report.reclaimed_count, 1, "the orphan must be collected");
    assert!(
        report.condemn_deferred_ids.is_empty(),
        "an uncontended sweep defers nothing: {:?}",
        report.condemn_deferred_ids
    );
    // Hard failure, not documentation: a fenced authority cannot delete a blob a
    // root exists for, so a non-empty list means the CAS is not atomic with
    // intent recording.
    assert!(
        report.deleted_while_referenced.is_empty(),
        "a {:?} sweep must never delete a referenced blob: {:?}",
        report.fence,
        report.deleted_while_referenced
    );
    assert!(matches!(
        crate::AttachmentStore::get(backend.as_ref(), &orphan.id, 32 * 1024 * 1024).await,
        Err(crate::AttachmentStoreError::NotFound(_))
    ));
    if crate::AttachmentRootSet::fence(&*factory) == crate::AttachmentGcFence::BestEffort {
        return;
    }
    assert_eq!(report.fence, crate::AttachmentGcFence::Fenced);
    // A completed delete retires the condemnation row outright, so the next
    // writer is granted immediately.
    assert!(matches!(
        crate::AttachmentReferrers::begin_attachment_write(
            store.store().as_ref(),
            &(crate::AttachmentWrite {
                attachment_id: orphan.id.clone(),
                claim: crate::conformance::attachment_referrers::claim(
                    crate::ArtifactReferrer::Session(request.session_id.clone())
                )
            })
        )
        .await
        .expect("write after a completed sweep"),
        crate::AttachmentWriteFence::Granted(_)
    ));
}

/// A pending write remains a root until abort or referrer end.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn session_store_factory_pending_write_is_a_root(factory: Arc<dyn crate::DeploymentStore>) {
    let store: Arc<dyn crate::RuntimeStore> = factory.clone();
    let id = crate::AttachmentId::parse("b2".repeat(32)).expect("id");
    let write = crate::AttachmentWrite {
        attachment_id: id.clone(),
        claim: crate::ReferrerClaim::unguarded(crate::ArtifactReferrer::ProcessRecord(
            crate::ProcessId::fixture("pending-root"),
        ))
        .expect("claim"),
    };
    let crate::AttachmentWriteFence::Granted(permit) =
        store.begin_attachment_write(&write).await.expect("begin")
    else {
        panic!("granted")
    };
    let pass = factory.begin_attachment_sweep().await.expect("sweep");
    assert_eq!(
        factory
            .condemn_attachment(&id, &pass)
            .await
            .expect("condemn"),
        crate::AttachmentCondemnation::RootPresent
    );
    store
        .abort_attachment_write(&write, permit)
        .await
        .expect("abort");
    assert_eq!(
        factory
            .condemn_attachment(&id, &pass)
            .await
            .expect("condemn"),
        crate::AttachmentCondemnation::Condemned
    );
}

pub(crate) use lash_core::testing::store_fixtures::session_store_request;

#[expect(
    clippy::expect_used,
    reason = "conformance fixtures establish each result"
)]
pub async fn concurrent_session_admissions_preserve_one_relation(
    factory: Arc<dyn crate::store::ConformanceDeployment>,
) {
    let id = SessionId::from("concurrent-relation");
    let first = session_store_request(
        &id,
        "first-model",
        crate::SessionRelation::Child {
            parent_session_id: SessionId::from("first-parent"),
            caused_by: None,
        },
    );
    let second = session_store_request(
        &id,
        "second-model",
        crate::SessionRelation::Child {
            parent_session_id: SessionId::from("second-parent"),
            caused_by: None,
        },
    );
    let (left, right) = tokio::join!(
        factory.admit_session(&first),
        factory.admit_session(&second)
    );
    let winner = match (&left, &right) {
        (
            Ok(crate::SessionAdmission::Created),
            Err(crate::StoreError::SessionRelationMismatch { .. }),
        ) => &first,
        (
            Err(crate::StoreError::SessionRelationMismatch { .. }),
            Ok(crate::SessionAdmission::Created),
        ) => &second,
        _ => panic!("one creator and one refused relation: {left:?}, {right:?}"),
    };
    let view = factory
        .live_view(&id)
        .await
        .expect("open winner")
        .expect("winner exists");
    let meta = view
        .load_session_meta()
        .await
        .expect("read winner metadata")
        .expect("metadata exists");
    assert_eq!(meta.relation, winner.relation);
    assert_eq!(
        factory.admit_session(winner).await.expect("winner rebinds"),
        crate::SessionAdmission::Rebound
    );
    assert_eq!(
        view.load_session_meta()
            .await
            .expect("read rebound metadata")
            .expect("metadata exists"),
        meta
    );
}
