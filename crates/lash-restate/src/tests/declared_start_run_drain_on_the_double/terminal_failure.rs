//! L02/L08: a real child Run's typed failure reaches its awaiting parent.

use super::*;
use crate::durable_wait::{LashDurableWaitRegistry as _, LashDurableWaitWorkflow as _};
use crate::process::{LashProcessWorkflow as _, RestateProcessRunner, SegmentStarted};
use lash_core::{ProcessExecutionContext, ProcessQuery as _, ScopedEffectController};
use restate_sdk::prelude::Endpoint;

fn failure() -> lash_core::ProcessAwaitOutput {
    super::super::process_failure(
        lash_core::ToolFailureClass::Execution,
        "child_failed",
        "the child body failed",
        Some(serde_json::json!({"detail": 7})),
    )
}

struct Runner {
    parent: ProcessId,
    starter: Arc<Starter>,
    observed: Arc<Mutex<Option<lash_core::ProcessAwaitOutput>>>,
}

#[async_trait::async_trait]
impl RestateProcessRunner for Runner {
    fn executable_generation(
        &self,
        _: &lash_core::ProcessRegistration,
    ) -> Option<lash_core::ExecutableGeneration> {
        None
    }

    async fn run_process_segment(
        &self,
        started: &SegmentStarted,
        process_id: ProcessId,
        _: lash_core::ProcessRegistration,
        _: ProcessExecutionContext,
        scoped: ScopedEffectController<'_>,
        _: Option<lash_core::SegmentHandover>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, lash_core::PluginError> {
        let mut call = call("terminal-child", ExternalCancelPolicy::Ignore);
        call.owner = EffectOpener::process(process_id.clone());
        call.segment = started.write_authority().segment().unwrap();
        let body = Starter::new(
            Arc::clone(&self.starter.registry),
            SingletonBodyOutcome::Failed {
                output: "the child body failed".into(),
                suggested_delay_ms: None,
            },
            CancelAt::Never,
        );
        body.materials
            .set(Arc::clone(self.starter.materials.get().unwrap()))
            .ok();
        let mut run = lash_core::tool_dispatch::RunCoordinator::open(
            &scoped,
            call.owner.clone(),
            call.segment,
            call.available.clone(),
        );
        if process_id != self.parent {
            crate::tests::decide_round(
                &mut run,
                std::slice::from_ref(&call),
                std::sync::Arc::clone(&body)
                    as std::sync::Arc<dyn lash_core::tool_dispatch::SingletonToolHandlers>,
                Default::default(),
            )
            .await
            .unwrap();
            let (_, terminal) = run.drain().await.unwrap().pop().unwrap();
            assert!(matches!(
                terminal,
                SingletonTerminal::Final {
                    capture: SingletonCapture::Failed { ref output, ..}, ..
                } if output == "the child body failed"
            ));
            run.close().await.unwrap();
            return Ok(failure().into());
        }
        call.declaration =
            ToolDeclaration::deferring().with_intents([ToolIntentKind::StartProcess]);
        crate::tests::decide_round(
            &mut run,
            std::slice::from_ref(&call),
            std::sync::Arc::clone(&self.starter)
                as std::sync::Arc<dyn lash_core::tool_dispatch::SingletonToolHandlers>,
            Default::default(),
        )
        .await
        .unwrap();
        run.await_deferred().await.unwrap();
        let (_, terminal) = run.drain().await.unwrap().pop().unwrap();
        let SingletonTerminal::Final { capture, .. } = terminal else {
            panic!("the child's terminal final reaches the parent");
        };
        let output: lash_core::ProcessAwaitOutput =
            serde_json::from_str(capture.output().unwrap()).unwrap();
        assert_eq!(output, failure());
        *self.observed.lock().unwrap() = Some(output);
        run.close().await.unwrap();
        Ok(super::super::process_success(serde_json::json!("parent observed failure")).into())
    }
}

#[tokio::test]
async fn l02_l08_a_child_run_failure_reaches_its_awaiting_parent_as_typed_output() {
    let stores = SqliteStoreSet::memory().await.unwrap();
    let registry = stores.process_registry();
    let registration = super::super::executed_registration();
    let parent = registry
        .register_process(registration.clone())
        .await
        .unwrap()
        .id;
    let SingletonBodyOutcome::Done {
        start: Some(start), ..
    } = declaring(Some(start_key("terminal-failure")))
    else {
        unreachable!()
    };
    let mut starter = Starter::new(
        registry.clone(),
        SingletonBodyOutcome::DeferredStart { start },
        CancelAt::Never,
    );
    Arc::get_mut(&mut starter).unwrap().launch_workflow = true;
    starter.materials.set(stores.process_env_store()).ok();
    let observed = Arc::new(Mutex::new(None));
    let runner = Arc::new(Runner {
        parent: parent.clone(),
        starter: Arc::clone(&starter),
        observed: Arc::clone(&observed),
    });
    let server = lash_restate_test::RestateTestServer::new(ServerConfig::default()).unwrap();
    let connection =
        crate::RestateConnection::with_transport(server.ingress_url(), server.transport());
    let ingress = crate::RestateIngressClient::new(connection.clone());
    starter.ingress.set(ingress.clone()).ok();
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
                .bind(crate::durable_wait::LashDurableWaitWorkflowImpl::default().serve())
                .bind(
                    crate::durable_wait::LashDurableWaitRegistryImpl::new(
                        Default::default(),
                        Default::default(),
                        crate::RestateAdminClient::new(connection),
                    )
                    .with_materials(stores.process_env_store())
                    .serve(),
                )
                .build(),
        )
        .await
        .unwrap();
    let input = crate::RestateProcessWorkflowPayload::from(crate::RestateProcessWorkflowInput {
        process_id: parent.clone(),
        registration,
        execution_context: Default::default(),
        segment_ordinal: 0,
        sender_generation: super::super::test_build_generation(),
    });
    let _: crate::RestateProcessWorkflowOutput = tokio::time::timeout(
        Duration::from_secs(30),
        ingress.call_lash_workflow("LashProcessWorkflow", parent.as_str(), "run", &input),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(*observed.lock().unwrap(), Some(failure()));
    let child = starter.launches().pop().unwrap();
    assert_eq!(
        registry
            .get_process(&child)
            .await
            .unwrap()
            .unwrap()
            .status(),
        lash_core::ProcessStatus::Failed
    );
    assert_eq!(starter.discharges(), vec![(child, false)]);
}
