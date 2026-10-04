//! L08/L09: an unseated process final carries its launch and admitted hold.

use super::*;
use crate::durable_wait::LashDurableWaitRegistry as _;
use crate::process::LashProcessWorkflow as _;
use crate::process::{
    RestateProcessRunner, RestateProcessWorkflowInput, RestateProcessWorkflowPayload,
    SegmentStarted,
};
use lash_core::store::ToolMaterialStore as _;
use lash_core::tool_dispatch::RunCoordinator;
use lash_core::tool_run::{MaterialHolder, RunLifecycle, RunTransfer};
use lash_core::{
    ProcessExecutionContext, ProcessQuery as _, ProcessRegistration, ScopedEffectController,
};
use restate_sdk::prelude::Endpoint;

struct Runner {
    starter: Arc<Starter>,
    call: SingletonToolCall,
}

#[async_trait::async_trait]
impl RestateProcessRunner for Runner {
    fn executable_generation(
        &self,
        _: &ProcessRegistration,
    ) -> Option<lash_core::ExecutableGeneration> {
        None
    }

    async fn run_process_segment(
        &self,
        started: &SegmentStarted,
        process_id: ProcessId,
        _: ProcessRegistration,
        _: ProcessExecutionContext,
        scoped: ScopedEffectController<'_>,
        handover: Option<lash_core::SegmentHandover>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, lash_core::PluginError> {
        let owner = EffectOpener::process(process_id);
        let segment = started.write_authority().segment().unwrap();
        let material = self.starter.materials.get().unwrap();
        if segment == SegmentOrdinal(0) {
            let mut call = self.call.clone();
            call.owner = owner.clone();
            let mut run = RunCoordinator::open(&scoped, owner, segment, call.available.clone());
            run.decide(&call, self.starter.as_ref()).await.unwrap();
            run.request_cut(lash_core::BoundaryReason::JournalBudget);
            let mut transfer = run.quiesce().await.unwrap();
            run.retain_cut(&mut transfer, material.as_ref())
                .await
                .unwrap();
            assert_eq!(transfer.owed_starts, vec![start_key("process-transfer")]);
            assert_eq!(transfer.environment, call.environment);
            assert_eq!(transfer.reserved_calls, 1);
            assert!(self.starter.launches().is_empty());
            return Ok(lash_core::ProcessRunOutcome::SegmentBoundary(
                lash_core::SegmentHandover {
                    reason: lash_core::BoundaryReason::JournalBudget,
                    program_hash: "start-transfer".to_owned(),
                    engine_state: serde_json::to_vec(&transfer).unwrap(),
                },
            ));
        }
        let transfer: RunTransfer =
            serde_json::from_slice(&handover.unwrap().engine_state).unwrap();
        let successor = MaterialHolder::Segment {
            opener: owner.clone(),
            segment,
        };
        for bundle in &transfer.material {
            material.acquire_material(&successor, bundle).await.unwrap();
        }
        material.release_material(&transfer.holder()).await.unwrap();
        // The final was protected before cancellation. Its launch, original
        // environment and recorded cancel hold still drain on the successor.
        self.starter.cancel.store(true, Ordering::SeqCst);
        let mut run = RunCoordinator::adopt(
            &scoped,
            owner,
            segment,
            self.call.available.clone(),
            transfer,
            Arc::clone(&self.starter) as Arc<dyn SingletonToolHandlers>,
            &lash_core::facade_support::SystemClock,
        )
        .await
        .unwrap();
        run.close().await.unwrap();
        assert_eq!(run.lifecycle(), RunLifecycle::Settled);
        material.release_material(&successor).await.unwrap();
        Ok(super::super::process_success(serde_json::json!("start transferred")).into())
    }
}

#[tokio::test]
async fn l08_process_cut_carries_start_environment_and_cancel_hold_without_body_replay() {
    let stores = SqliteStoreSet::memory().await.unwrap();
    let key = start_key("process-transfer");
    let starter = Starter::new(
        stores.process_registry(),
        declaring(Some(key.clone())),
        CancelAt::Never,
    );
    assert!(starter.materials.set(stores.process_env_store()).is_ok());
    let runner = Arc::new(Runner {
        starter: Arc::clone(&starter),
        call: call("process-start", ExternalCancelPolicy::CancelExternalWork),
    });
    let server = lash_restate_test::RestateTestServer::new(ServerConfig::default()).unwrap();
    let connection =
        crate::RestateConnection::with_transport(server.ingress_url(), server.transport());
    let ingress = crate::RestateIngressClient::new(connection.clone());
    let registry = stores.process_registry();
    server
        .register(
            Endpoint::builder()
                .bind(
                    crate::LashProcessWorkflowImpl::new(
                        runner,
                        registry.clone(),
                        registry.clone(),
                        ingress.clone(),
                        Arc::new(lash_core::attachments::NoopAttachmentReferrers),
                        super::super::test_restate_authority_id(),
                        super::super::test_build_generation(),
                        &crate::services::DEFAULT_NAMESPACE,
                    )
                    .serve(),
                )
                .bind(
                    crate::durable_wait::LashDurableWaitRegistryImpl::new(
                        Default::default(),
                        Default::default(),
                        crate::RestateAdminClient::new(connection),
                    )
                    .serve(),
                )
                .build(),
        )
        .await
        .unwrap();
    let mut registration = super::super::executed_registration();
    registration.env_ref = Some(ProcessExecutionEnvRef::new(ENVIRONMENT));
    let process_id = registry
        .register_process(registration.clone())
        .await
        .unwrap()
        .id;
    let input = RestateProcessWorkflowPayload::from(RestateProcessWorkflowInput {
        process_id: process_id.clone(),
        registration,
        execution_context: Default::default(),
        segment_ordinal: 0,
        sender_generation: super::super::test_build_generation(),
    });
    let _: crate::process::RestateProcessWorkflowOutput = ingress
        .call_lash_workflow("LashProcessWorkflow", process_id.as_str(), "run", &input)
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let record = registry.get_process(&process_id).await.unwrap().unwrap();
        if record.status().is_terminal() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "start successor did not finish: {:#?}",
            server.invocations()
        );
        tokio::task::yield_now().await;
    }
    assert_eq!(starter.executions.load(Ordering::SeqCst), 1);
    assert_eq!(starter.launches().len(), 1);
    assert_eq!(
        starter.discharges(),
        vec![(starter.launches()[0].clone(), true)]
    );
    let rows: Vec<_> = Stores {
        tier: Tier::Memory,
        set: stores,
        dir: None,
    }
    .rows()
    .await
    .into_iter()
    .filter(|row| row.start_key.as_deref() == Some(key.as_str()))
    .collect();
    assert_eq!(rows, vec![Row::drained(&starter.launches()[0], &key, true)]);
}
