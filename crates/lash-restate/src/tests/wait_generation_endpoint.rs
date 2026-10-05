//! Shared endpoints for durable wait generation laws.
use super::*;
/// The builds run no process here.
struct IdleRunner;

#[async_trait::async_trait]
impl RestateProcessRunner for IdleRunner {
    fn executable_generation(
        &self,
        _registration: &ProcessRegistration,
    ) -> Option<lash_core::ExecutableGeneration> {
        None
    }

    async fn run_process_segment(
        &self,
        _started: &crate::SegmentStarted,
        _process_id: ProcessId,
        _registration: ProcessRegistration,
        _execution_context: ProcessExecutionContext,
        _scoped_effect_controller: ScopedEffectController<'_>,
        _handover: Option<lash_core::SegmentHandover>,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        Err(PluginError::Session(
            "no process runs in the L4 law".to_string(),
        ))
    }
}

fn generation(build: &'static str) -> lash_core::engine::BuildGeneration {
    lash_core::engine::BuildGeneration::for_test(build)
}

/// One build of every lash service over the shared stores, with its own
/// effect host (the group resolver runs children on it).
pub(super) async fn build_endpoint(
    connection: &RestateConnection,
    stores: &dyn lash_core::StoreSet,
    build: &'static str,
) -> (Arc<RestateEffectHost>, Endpoint) {
    build_endpoint_reading(connection, stores, build, crate::RESTATE_WIRE).await
}

/// [`build_endpoint`] for a build that reads the wire versions `reads`, as a
/// build of another release does.
pub(super) async fn build_endpoint_reading(
    connection: &RestateConnection,
    stores: &dyn lash_core::StoreSet,
    build: &'static str,
    reads: crate::VersionRange,
) -> (Arc<RestateEffectHost>, Endpoint) {
    let (host, builder) = build_endpoint_builder(connection, stores, build, reads).await;
    (host, builder.build())
}

async fn build_endpoint_builder(
    connection: &RestateConnection,
    stores: &dyn lash_core::StoreSet,
    build: &'static str,
    reads: crate::VersionRange,
) -> (Arc<RestateEffectHost>, restate_sdk::endpoint::Builder) {
    let host = Arc::new(RestateEffectHost::in_namespace(
        connection.clone(),
        test_restate_authority_id(),
        crate::RestateNamespace::default(),
    ));
    let registry = stores.process_registry();
    let ingress = RestateIngressClient::new(connection.clone());
    let endpoint = crate::services::bind_lash_services_reading(
        Endpoint::builder(),
        crate::services::LashServiceParts {
            tool_realizer: Arc::new(crate::tests::NoIntentsRealizer),
            effect_host: &host,
            admin: crate::RestateAdminClient::new(connection.clone()),
            materials: stores.tool_material_store(),
            attachments: stores.attachment_referrers(),
            process_workflow: LashProcessWorkflowImpl::new(
                Arc::new(IdleRunner),
                registry,
                stores.process_continuations(),
                ingress,
                Arc::new(lash_core::attachments::NoopAttachmentReferrers),
                test_restate_authority_id(),
                generation(build),
                &crate::services::DEFAULT_NAMESPACE,
            ),
            session_shifts: crate::RestateSessionShiftsSlot::new(),
            build_generation: generation(build),
            namespace: crate::RestateNamespace::default(),
            fleet: crate::object_state::FleetView::default(),
        },
        reads,
    );
    (host, endpoint)
}
