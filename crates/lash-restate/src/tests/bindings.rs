//! `RestateBackend::endpoint_builder` binds every service lash-restate
//! addresses by name — the set `LashService` spells for every caller — and
//! leaves the host's own services to the host.

#![allow(
    deprecated,
    reason = "Restate SDK 0.11 retains the trait service API used by protocol fixtures"
)]

use std::collections::BTreeSet;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use restate_sdk::endpoint::Endpoint;
use restate_sdk::prelude::{HandlerResult, WorkflowContext};

use crate::services::LASH_SERVICES;
use crate::{RestateAuthorityId, RestateBackend, RestateQueuedWork};

/// A host's own turn workflow, bound beside lash's services.
#[restate_sdk::workflow]
trait HostTurnWorkflow {
    async fn run() -> HandlerResult<()>;
}

struct HostTurnWorkflowImpl;

impl HostTurnWorkflow for HostTurnWorkflowImpl {
    async fn run(&self, _ctx: WorkflowContext<'_>) -> HandlerResult<()> {
        Ok(())
    }
}

/// The service names `endpoint` reports in the discovery document it serves
/// the Restate runtime. It asks for manifest v4, as the runtime does: only v4
/// can carry a turn handler's retry policy.
async fn discovered_service_names(endpoint: &Endpoint) -> BTreeSet<String> {
    let request = http::Request::builder()
        .uri("/discover")
        .header(
            http::header::ACCEPT,
            "application/vnd.restate.endpointmanifest.v4+json",
        )
        .body(Full::new(Bytes::new()))
        .expect("build the discovery request");
    let response = endpoint.handle(request);
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("read the discovery response")
        .to_bytes();
    assert!(
        status.is_success(),
        "discovery answered {status}: {}",
        String::from_utf8_lossy(&body)
    );
    let document: serde_json::Value =
        serde_json::from_slice(&body).expect("decode the discovery document");
    document["services"]
        .as_array()
        .expect("the discovery document lists its services")
        .iter()
        .map(|service| {
            service["name"]
                .as_str()
                .expect("a discovered service has a name")
                .to_string()
        })
        .collect()
}

fn lash_service_names() -> BTreeSet<String> {
    LASH_SERVICES
        .iter()
        .map(|service| service.name().to_string())
        .collect()
}

/// A Restate backend over a memory store set, and the process worker of a
/// core built over it: what a host hands `endpoint_builder`.
async fn backend_and_process_worker()
-> (Arc<RestateBackend>, lash_core_worker::DurableProcessWorker) {
    let backend = Arc::new(RestateBackend::new(
        "http://127.0.0.1:9",
        RestateAuthorityId::new("lash-restate-endpoint-builder").expect("valid authority"),
        Arc::new(
            lash_sqlite_store::SqliteStoreSet::memory()
                .await
                .expect("open the memory store set"),
        ) as Arc<dyn lash_core::StoreSet>,
        RestateQueuedWork::Disabled,
    ));
    let core = lash::LashCore::standard_builder(
        Arc::clone(&backend) as Arc<dyn lash_core::Backend>,
        lash::TurnBudget::Unbounded,
    )
    .provider(
        lash_core::testing::TestProvider::builder()
            .kind("endpoint-builder-stub")
            .complete(|_| async { Ok(lash_core::LlmResponse::default()) })
            .build()
            .into_handle(),
    )
    .model(lash_core::ModelSpec::new(
        "endpoint-builder-model",
        std::num::NonZeroUsize::new(1024).expect("non-zero context window"),
    ))
    .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
    .build(lash::persistence::LeaseOwnerIdentity::opaque(
        "lash-restate-endpoint-builder",
        "lash-restate-endpoint-builder-boot",
    ))
    .expect("build the core");
    let worker = lash_core_worker::DurableProcessWorker::new(
        core.durable_process_worker_config()
            .expect("the core configures a process worker"),
    )
    .expect("valid process worker");
    (backend, worker)
}

#[test]
fn every_lash_service_has_its_own_name() {
    assert_eq!(
        lash_service_names().len(),
        LASH_SERVICES.len(),
        "two lash services share a Restate name: {:?}",
        LASH_SERVICES
    );
}

/// The discovery document is what the Restate runtime registers, so this is
/// the set a deployment serves. Each name is the one every lash caller
/// addresses, so a service lash calls is a service the endpoint binds, and a
/// handler macro whose name drifts from its caller's fails here.
#[tokio::test]
async fn the_endpoint_builder_binds_every_lash_service() {
    let (backend, worker) = backend_and_process_worker().await;
    let endpoint = backend.endpoint_builder(worker).build();
    assert_eq!(
        discovered_service_names(&endpoint).await,
        lash_service_names()
    );
}

/// A host binds its own services — here a turn workflow carrying a handler
/// retry policy — on the builder it gets back, beside lash's.
#[tokio::test]
async fn a_host_binds_its_own_services_beside_lash_services() {
    let (backend, worker) = backend_and_process_worker().await;
    let endpoint = backend
        .endpoint_builder(
            crate::RestateProcessServing::new(worker).with_segment_effect_budget_selector(|_| 3),
        )
        .bind(crate::turn_service(HostTurnWorkflowImpl.serve(), "run"))
        .build();
    let mut expected = lash_service_names();
    expected.insert("HostTurnWorkflow".to_string());
    assert_eq!(discovered_service_names(&endpoint).await, expected);
}
