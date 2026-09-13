//! Recovery closes the window between a turn's commit and its parent-end row.
//!
//! A turn commits to the session store and then writes its parent-end ledger
//! row to the process registry. Those are separate stores, so a crash can land
//! between them and leave `Cancel` children naming a turn that will never end
//! again. The worker's redrive re-derives the missing row — but only for a turn
//! whose commit is durable, because a turn that crashed before its own commit
//! is interrupted rather than ended and its redrive re-registers exactly the
//! children a sweep would have cancelled.

use super::*;

use crate::store::SessionCommitStore as _;

const SESSION: &str = "parent-end-redrive-session";

/// Never actually runs here: these laws drive only the parent-end passes, and
/// an engine is part of a well-formed worker.
struct NeverDrivenEngine;

#[async_trait::async_trait]
impl crate::ProcessEngine for NeverDrivenEngine {
    fn kind(&self) -> &'static str {
        "immediate-success"
    }

    async fn run(
        &self,
        context: crate::ProcessEngineRunContext<'_>,
        _payload: serde_json::Value,
    ) -> Result<crate::ProcessRunOutcome, crate::ProcessInfraError> {
        Ok(
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::json!({"process_id": context.registration().id}),
            ))
            .into(),
        )
    }
}

/// A factory whose sessions survive: the redrive must reach the very store the
/// turn committed to, so a fresh store per open would answer every question
/// "not committed".
#[derive(Default)]
struct SharedInMemorySessionStoreFactory {
    stores: Mutex<std::collections::HashMap<SessionId, Arc<crate::InMemorySessionStore>>>,
}

impl SharedInMemorySessionStoreFactory {
    fn store(&self, session_id: &SessionId) -> Arc<crate::InMemorySessionStore> {
        Arc::clone(
            self.stores
                .lock_recover()
                .entry(session_id.clone())
                .or_default(),
        )
    }
}

#[async_trait::async_trait]
impl crate::AttachmentRootSet for SharedInMemorySessionStoreFactory {
    async fn live_attachment_refs(
        &self,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<std::collections::BTreeSet<crate::AttachmentId>, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "live_attachment_refs",
        })
    }

    async fn has_live_attachment_ref(
        &self,
        _id: &crate::AttachmentId,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "has_live_attachment_ref",
        })
    }
}

#[async_trait::async_trait]
impl SessionStoreFactory for SharedInMemorySessionStoreFactory {
    async fn create_store(
        &self,
        request: &crate::SessionStoreCreateRequest,
    ) -> Result<Arc<dyn crate::RuntimePersistence>, crate::StoreError> {
        Ok(self.store(&request.session_id))
    }

    async fn open_existing_store(
        &self,
        request: &crate::SessionStoreCreateRequest,
    ) -> Result<Option<Arc<dyn crate::RuntimePersistence>>, String> {
        Ok(Some(self.store(&request.session_id)))
    }

    async fn session_was_deleted(&self, _session_id: &SessionId) -> Result<bool, String> {
        Ok(false)
    }

    async fn delete_session(
        &self,
        _session_id: &SessionId,
    ) -> crate::store::MaintenanceResult<crate::store::SessionBlobReclaimReport> {
        Ok(crate::store::SessionBlobReclaimReport::default())
    }
}

fn turn_parent(turn_id: &str) -> crate::ParentScope {
    crate::ParentScope::Turn {
        session_id: SessionId::from(SESSION),
        turn_id: crate::TurnId::from(turn_id),
    }
}

fn cancel_child(
    process_id: &str,
    turn_id: &str,
    env_ref: ProcessExecutionEnvRef,
) -> ProcessRegistration {
    let mut registration = engine_registration(
        process_id,
        "immediate-success",
        env_ref,
        serde_json::Value::Null,
    );
    registration.provenance.originator =
        crate::ProcessOriginator::session(crate::SessionScope::new(SESSION));
    registration.lifecycle =
        crate::ProcessLifecyclePolicy::new(turn_parent(turn_id), crate::OnParentEnd::Cancel);
    registration
}

/// Commit one turn to the session store the worker's factory hands out,
/// leaving the parent-end ledger row unwritten: exactly the durable state a
/// crash between the two writes produces.
///
/// `head_revision` is the store's revision this commit expects: every turn here
/// commits into the one shared session, so a second commit follows the first.
async fn commit_turn(
    factory: &Arc<SharedInMemorySessionStoreFactory>,
    turn_id: &str,
    head_revision: u64,
) {
    let store = factory.store(&SessionId::from(SESSION));
    let state = crate::runtime::RuntimeSessionState {
        session_id: SessionId::from(SESSION),
        head_revision,
        ..crate::runtime::RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
        ))
    };
    let mut commit = crate::RuntimeCommit::persisted_state_for_test(&state, &[]);
    commit.turn_commit =
        crate::RuntimeTurnCommitStamp::new(crate::OperationId::turn(SESSION, turn_id, "final"));
    store
        .commit_runtime_state(commit)
        .await
        .expect("commit the turn whose parent-end row the crash swallowed");
}

async fn cancel_origins(
    registry: &Arc<dyn ProcessRegistry>,
    process_id: &ProcessId,
) -> Option<crate::CancelOrigin> {
    registry
        .get_process(process_id)
        .await
        .expect("read the child")
        .expect("the child row survives the sweep")
        .cancel_request
        .map(|request| request.origin)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recovery_re_derives_a_committed_turns_missing_parent_end_row_exactly_once() {
    let factory = Arc::new(SharedInMemorySessionStoreFactory::default());
    let (worker, registry, env_ref, _test_registry) =
        worker_with_session_store_factory(Arc::new(NeverDrivenEngine), factory.clone()).await;

    let committed = ProcessId::from("redrive-committed-child");
    let interrupted = ProcessId::from("redrive-interrupted-child");
    registry
        .register_process(cancel_child(
            committed.as_str(),
            "committed-turn",
            env_ref.clone(),
        ))
        .await
        .expect("register the committed turn's child");
    registry
        .register_process(cancel_child(
            interrupted.as_str(),
            "interrupted-turn",
            env_ref,
        ))
        .await
        .expect("register the interrupted turn's child");
    commit_turn(&factory, "committed-turn", 0).await;

    worker
        .redrive_missing_turn_parent_end_rows()
        .await
        .expect("re-derive the missing ledger rows");

    let plan = registry
        .get_parent_end_plan(&turn_parent("committed-turn"))
        .await
        .expect("read the re-derived ledger row")
        .expect("the committed turn owes a ledger row");
    assert_eq!(plan.parent, turn_parent("committed-turn"));
    assert!(
        registry
            .get_parent_end_plan(&turn_parent("interrupted-turn"))
            .await
            .expect("read the interrupted turn's ledger row")
            .is_none(),
        "a turn interrupted before its commit is not ended, so it owes no row"
    );

    worker
        .drive_pending_parent_end_plans()
        .await
        .expect("settle the re-derived row");
    assert_eq!(
        cancel_origins(&registry, &committed).await,
        Some(crate::CancelOrigin::ParentEnded),
        "the committed turn's Cancel child is cancelled by its ended parent"
    );
    assert_eq!(
        cancel_origins(&registry, &interrupted).await,
        None,
        "the interrupted turn's child is left for its redrive"
    );

    // A second pass is a no-op: the settled row is never re-derived, and the
    // cancelled child keeps the first request's origin.
    worker
        .redrive_missing_turn_parent_end_rows()
        .await
        .expect("second redrive pass");
    worker
        .drive_pending_parent_end_plans()
        .await
        .expect("second sweep pass");
    let settled = registry
        .get_parent_end_plan(&turn_parent("committed-turn"))
        .await
        .expect("read the settled ledger row")
        .expect("the ledger row outlives its sweep");
    assert_eq!(settled.ended_at_ms, plan.ended_at_ms);
    assert!(settled.settled_at_ms.is_some());
    assert_eq!(
        cancel_origins(&registry, &committed).await,
        Some(crate::CancelOrigin::ParentEnded)
    );
}

/// A page full of candidates that can never be recorded must not starve the
/// ones that can.
///
/// Every stuck scope here is a turn that never committed and is never
/// redriven, which is a legitimate durable state, and each keeps its slot in
/// the candidate order forever. With a prefix page the committed turn behind
/// them would never be read at all, so its `Cancel` child would outlive it for
/// good; with the keyset cursor the sweep walks past them and reaches it on
/// the next pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_full_page_of_unrecordable_scopes_does_not_starve_the_committed_one() {
    const STUCK: usize = 300;
    let factory = Arc::new(SharedInMemorySessionStoreFactory::default());
    let (worker, registry, env_ref, _test_registry) =
        worker_with_session_store_factory(Arc::new(NeverDrivenEngine), factory.clone()).await;

    for index in 0..STUCK {
        registry
            .register_process(cancel_child(
                &format!("stuck-child-{index:03}"),
                &format!("stuck-turn-{index:03}"),
                env_ref.clone(),
            ))
            .await
            .expect("register a stuck turn's child");
    }
    // Lexically last, so every stuck scope is read before it.
    let committed = ProcessId::from("zz-committed-child");
    registry
        .register_process(cancel_child(
            committed.as_str(),
            "zz-committed-turn",
            env_ref,
        ))
        .await
        .expect("register the committed turn's child");
    commit_turn(&factory, "zz-committed-turn", 0).await;

    worker
        .redrive_missing_turn_parent_end_rows()
        .await
        .expect("first pass reads a full page of stuck scopes");
    assert!(
        registry
            .get_parent_end_plan(&turn_parent("zz-committed-turn"))
            .await
            .expect("read the ledger row after the first pass")
            .is_none(),
        "the first page is entirely stuck scopes, so the committed turn is not reached yet"
    );

    worker
        .redrive_missing_turn_parent_end_rows()
        .await
        .expect("second pass resumes after the cursor");
    assert!(
        registry
            .get_parent_end_plan(&turn_parent("zz-committed-turn"))
            .await
            .expect("read the ledger row after the second pass")
            .is_some(),
        "the cursor walks past the stuck scopes and the committed turn gets its row"
    );

    worker
        .drive_pending_parent_end_plans()
        .await
        .expect("settle the re-derived row");
    assert_eq!(
        cancel_origins(&registry, &committed).await,
        Some(crate::CancelOrigin::ParentEnded),
        "the committed turn's Cancel child is cancelled once its row exists"
    );

    // The second pass read a short page, so the cursor wrapped to the start of
    // the candidate order. A scope that only becomes recordable afterwards
    // sits lexically first, behind the cursor's high-water mark: it is reached
    // on the next pass only because the wrap happened.
    commit_turn(&factory, "stuck-turn-000", 1).await;
    worker
        .redrive_missing_turn_parent_end_rows()
        .await
        .expect("the pass after a short page starts a new lap");
    assert!(
        registry
            .get_parent_end_plan(&turn_parent("stuck-turn-000"))
            .await
            .expect("read the ledger row of the newly committed scope")
            .is_some(),
        "a short page wraps the cursor, so a scope that becomes recordable later is reconsidered"
    );
}
