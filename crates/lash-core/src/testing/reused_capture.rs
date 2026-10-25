//! The no-new-pages capture still owns its reader cleanup (FIG-7358).
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use lash_core_execution::runtime::actor::round::graph::CallBody;
use lash_core_store::checkpoint_graph::{
    CommitId, EncodeBudget, ExplicitLimit, GraphStaging, NodeId, NodeKind,
};
use lash_core_store::store::AdmittedInputIds;
use lash_durable::domain::pages::{LeaseFence, PageStore, RootLeaseId, RootLeaseRequest};
use lash_durable::domain::{CellId, ExecKey, ExecRootWrite, RootOwner, TurnWrite};
use lash_durable::{ActorKey, ActorTx, CommitLabel, DomainWrite, FormatSet, MailTx, NodeSpec};
use lash_execution_contracts::backend::Backend;
use lash_vocabulary::identity::{InputId, SessionId, TurnId};

use crate::plugin::{CodeExecutorPlugin, ExecutionCandidate, ProtocolSessionContext};

struct ReusedCapture {
    pages: Arc<dyn PageStore>,
    lease: RootLeaseId,
    root: NodeId,
    gate: Arc<crate::testing::Gate>,
}

#[async_trait::async_trait]
impl CodeExecutorPlugin for ReusedCapture {
    async fn execute_code(
        &self,
        _: crate::RuntimeExecutionContext<'_>,
        _: crate::ExecRequest,
    ) -> Result<crate::ExecResponse, crate::SessionError> {
        unreachable!("the law exercises capture publication, not cell execution")
    }

    async fn frame_switch_carries(
        &self,
        _: ProtocolSessionContext<'_>,
        _: &crate::FrameNodeId,
        _: &[crate::SessionAppendNode],
    ) -> Result<Vec<crate::ArtifactName>, crate::SessionError> {
        Ok(Vec::new())
    }

    async fn execution_candidate(
        &self,
        root: NodeId,
        _: CommitId,
        _: &mut ActorTx,
    ) -> Result<Option<ExecutionCandidate>, crate::SessionError> {
        assert_eq!(root, self.root, "publish the already committed capture");
        self.gate.pass().await;
        Ok(None)
    }

    async fn acknowledge_execution_state_capture(&self) {
        self.pages.release_root_lease(self.lease).await.unwrap();
    }
}

async fn claimed(backend: &Backend, session: &SessionId, run: &TurnId) -> crate::ActorContext {
    super::runtime_helpers::create_runtime_fixture_session(
        backend.session_store_factory().as_ref(),
        session,
        &crate::testing::mock_session_policy(),
    )
    .await
    .unwrap();
    let actor = ActorKey::session(session.as_str()).unwrap();
    let formats = FormatSet::new(lash_durable::domain::SESSION_ACTOR_FORMATS);
    let mut wake = MailTx::new();
    wake.wake(actor.clone());
    backend
        .durable()
        .commit_mail(wake, CommitLabel::new("law.wake"))
        .await
        .unwrap();
    let node = backend
        .durable()
        .register_node(&NodeSpec {
            node: lash_durable::NodeId::new("reused-capture-owner"),
            decodes: vec![formats],
            serves: Vec::new(),
            capabilities: Default::default(),
            ttl_millis: 600_000,
        })
        .await
        .unwrap();
    let claim = backend.durable().claim(&node, 1).await.unwrap().remove(0);
    crate::ActorContext::new(
        backend.clone(),
        actor,
        claim.epoch,
        crate::AdmittedScope::turn(session.clone(), run.clone()),
        tokio_util::sync::CancellationToken::new(),
        Arc::new(lash_durable::NoProbe),
    )
}

async fn seeded(
    cx: &crate::ActorContext,
    session: &SessionId,
    run: &TurnId,
) -> (NodeId, NodeId, NodeId) {
    let mut graph = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    let body = CallBody {
        record: crate::ToolCallRecord {
            call_id: crate::ToolCallId::fixture("retired-lookup"),
            provider_call_id: None,
            tool: "tools.app_lookup".into(),
            args: serde_json::json!({}),
            output: crate::ToolCallOutput::success(serde_json::json!({"found": true})),
        },
    }
    .stage(&mut graph)
    .unwrap();
    // The publication seam treats executor-owned descriptor bodies as opaque;
    // only the old descriptor's edge to the archival call matters here.
    let old = graph.encode_value(NodeKind::EXEC_ROOT, &body).unwrap();
    let reused = graph.encode_value(NodeKind::EXEC_ROOT, &42_u64).unwrap();
    let exec = ExecKey::Cell(
        session.clone(),
        run.clone(),
        CellId::new("committed-capture"),
    );
    let mut tx = cx.begin().await.unwrap();
    tx.stage(graph.into_nodes().into_values());
    tx.write(DomainWrite::Turn(TurnWrite::Admit {
        session: session.clone(),
        run: run.clone(),
        admission: crate::store::RunAdmissionRecord::Turn {
            took: crate::store::AdmittedTurnRows::Inputs {
                ids: AdmittedInputIds::one(InputId::fixture("reused-capture-input")),
            },
            trace: None,
        },
        turn_deadline: None,
    }));
    tx.write(DomainWrite::Turn(TurnWrite::Working {
        session: session.clone(),
        run: run.clone(),
        expected: None,
        working: Some(old),
    }));
    tx.write(DomainWrite::ExecRoot(ExecRootWrite::Put {
        exec,
        expected: None,
        expected_root: None,
        root: reused,
        commit_id: CommitId::mint(),
    }));
    cx.commit(tx, CommitLabel::new("law.seed-capture"))
        .await
        .unwrap();
    (body, old, reused)
}

/// Force publication of an already stored capture before deleting its session.
/// Both built-in stores mount this law; the callback is an executor's cleanup
/// obligation, represented by a lease on the old cell's archival body.
pub async fn deleted_session_reclaims_reused_capture(backend: Backend) {
    let session = SessionId::fixture("retire-reused-capture");
    let run = TurnId::fixture("retire-reused-capture-run");
    let cx = claimed(&backend, &session, &run).await;
    let pages = cx.pages().unwrap();
    let (body, old, reused) = seeded(&cx, &session, &run).await;
    let lease = pages
        .acquire_root_lease(RootLeaseRequest {
            owner: RootOwner::WorkingExecution {
                session: session.clone(),
                run: run.clone(),
            },
            expected: old,
            fence: LeaseFence::Actor {
                actor: cx.actor().clone(),
                epoch: cx.epoch(),
            },
            ttl_millis: 600_000,
        })
        .await
        .unwrap();
    let gate = Arc::new(crate::testing::Gate::new("capture has no fresh pages"));
    let executor: Arc<dyn CodeExecutorPlugin> = Arc::new(ReusedCapture {
        pages: pages.clone(),
        lease: lease.id,
        root: reused,
        gate: gate.clone(),
    });
    let mut tx = cx.begin().await.unwrap();
    tx.require(reused);
    tx.write(DomainWrite::Turn(TurnWrite::Working {
        session: session.clone(),
        run: run.clone(),
        expected: Some(old),
        working: Some(reused),
    }));
    let mut publishing = Box::pin(crate::runtime::durable::session_publication::commit(
        &cx,
        Some(&executor),
        &session,
        tx,
        CommitLabel::new("law.reused-capture"),
        None,
    ));
    gate.reached_by(&mut publishing, 1).await;
    backend
        .session_store_factory()
        .gc_unreachable()
        .await
        .unwrap();
    pages.read_pages(&lease, &[body]).await.unwrap();
    gate.open_all();
    publishing.await.unwrap();
    // Close ends the run before physical storage deletion and releases the
    // activation. Keep that ordering, including retirement of its working root.
    let mut tx = cx.begin().await.unwrap();
    tx.write(DomainWrite::Turn(TurnWrite::Terminal {
        session: session.clone(),
        run,
        cause: Box::new(crate::store::RunTerminalCause::SessionDeleted {
            intent: crate::store::ControlIntentId::from_sequence(1),
        }),
        head_revision: None,
        transcript: None,
    }));
    tx.give_up(lash_durable::Release::Idle);
    cx.commit(tx, CommitLabel::new("law.release"))
        .await
        .unwrap();
    backend
        .session_store_factory()
        .delete_session(&session)
        .await
        .unwrap();
    let collected = backend
        .session_store_factory()
        .gc_unreachable()
        .await
        .unwrap();
    assert_eq!(
        collected.retained_blob_count, 0,
        "a deleted session retained its archival call through an unacknowledged reused capture"
    );
}
