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

const SEED: u64 = 0xf6_0002;
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
                serde_json::json!({"process_id": context.process_id()}),
            ))
            .into(),
        )
    }
}

fn turn_parent(turn_id: &str) -> crate::ParentScope {
    crate::ParentScope::turn(SessionId::from(SESSION), crate::TurnId::from(turn_id))
}

fn cancel_child(
    _process_id: &str,
    turn_id: &str,
    env_ref: ProcessExecutionEnvRef,
) -> ProcessRegistration {
    let mut registration =
        engine_registration("immediate-success", env_ref, serde_json::Value::Null);
    registration.provenance.originator =
        crate::ProcessOriginator::session(crate::SessionScope::new(SESSION));
    registration.lifecycle =
        crate::ProcessLifecyclePolicy::new(turn_parent(turn_id), crate::OnParentEnd::Cancel);
    registration
}

/// Commit one turn to the session store the worker's backend hands out,
/// leaving the parent-end ledger row unwritten: exactly the durable state a
/// crash between the two writes produces.
///
/// `head_revision` is the store's revision this commit expects: every turn here
/// commits into the one shared session, so a second commit follows the first.
async fn commit_turn(backend: &crate::Backend, turn_id: &str, head_revision: u64) {
    let factory = backend.session_store_factory();
    let session_id = SessionId::from(SESSION);
    let store = match factory
        .open_existing_store_by_id(&session_id)
        .await
        .expect("look the session up in the backend catalog")
    {
        Some(store) => store,
        None => factory
            .create_store(&crate::SessionStoreCreateRequest {
                session_id: session_id.clone(),
                relation: crate::SessionRelation::Root,
                pending_observer_intents: Vec::new(),
                policy: crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
            })
            .await
            .expect("create the session in the backend catalog"),
    };
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
    let double = kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let (worker, registry, env_ref) =
        worker_on_backend(Arc::new(NeverDrivenEngine), &backend).await;

    let committed = crate::ProcessId::fixture("redrive-committed-child");
    let interrupted = crate::ProcessId::fixture("redrive-interrupted-child");
    let redrive_committed_child_record = registry
        .register_process(cancel_child(
            committed.as_str(),
            "committed-turn",
            env_ref.clone(),
        ))
        .await
        .expect("register the committed turn's child");
    let committed = redrive_committed_child_record.id.clone();
    let redrive_interrupted_child_record = registry
        .register_process(cancel_child(
            interrupted.as_str(),
            "interrupted-turn",
            env_ref,
        ))
        .await
        .expect("register the interrupted turn's child");
    let interrupted = redrive_interrupted_child_record.id.clone();
    commit_turn(&backend, "committed-turn", 0).await;

    worker
        .redrive_missing_opener_parent_end_rows()
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
        .redrive_missing_opener_parent_end_rows()
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
    let double = kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let (worker, registry, env_ref) =
        worker_on_backend(Arc::new(NeverDrivenEngine), &backend).await;

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
    let committed = crate::ProcessId::fixture("zz-committed-child");
    let zz_committed_child_record = registry
        .register_process(cancel_child(
            committed.as_str(),
            "zz-committed-turn",
            env_ref,
        ))
        .await
        .expect("register the committed turn's child");
    let committed = zz_committed_child_record.id.clone();
    commit_turn(&backend, "zz-committed-turn", 0).await;

    worker
        .redrive_missing_opener_parent_end_rows()
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
        .redrive_missing_opener_parent_end_rows()
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
    commit_turn(&backend, "stuck-turn-000", 1).await;
    worker
        .redrive_missing_opener_parent_end_rows()
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
