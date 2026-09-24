//! Wiring-time binding validation against the endpoint's own discovery
//! document.

#![allow(
    deprecated,
    reason = "Restate SDK 0.11 retains the trait service API used by protocol fixtures"
)]

use crate::durable_wait::{
    LashDurableWaitIndex, LashDurableWaitIndexImpl, LashDurableWaitWorkflow,
    LashDurableWaitWorkflowImpl,
};
use crate::process_attach::{LashProcessAttach, LashProcessAttachImpl};
use crate::{
    RestateBackend, RestateBindingCheckError, RestateEffectGroupServices, RestateProcessDeployment,
    assert_services_bound, bound_service_names,
};
use restate_sdk::endpoint::Endpoint;

#[tokio::test]
async fn bound_service_names_reports_what_the_endpoint_bound() {
    let endpoint = Endpoint::builder()
        .bind(LashDurableWaitWorkflowImpl.serve())
        .bind(LashDurableWaitIndexImpl.serve())
        .bind(LashProcessAttachImpl.serve())
        .build();
    let bound = bound_service_names(&endpoint)
        .await
        .expect("endpoint discovery answers");
    for name in [
        "LashDurableWaitWorkflow",
        "LashDurableWaitIndex",
        "LashProcessAttach",
    ] {
        assert!(bound.contains(name), "discovery must report `{name}`");
    }
    assert!(
        !bound.contains("LashProcessWorkflow"),
        "an unbound service must not appear in discovery"
    );
}

/// A turn handler carries a retry policy, which only discovery manifest v4
/// can express; the binding check must ask for it, as the Restate runtime does,
/// instead of taking the SDK's oldest manifest and a refusal.
#[tokio::test]
async fn bound_service_names_reads_a_service_whose_handler_carries_a_retry_policy() {
    let endpoint = Endpoint::builder()
        .bind(crate::turn_service(
            LashDurableWaitWorkflowImpl.serve(),
            "await_resolution",
        ))
        .build();
    let bound = bound_service_names(&endpoint)
        .await
        .expect("discovery answers for a handler with a retry policy");
    assert!(bound.contains("LashDurableWaitWorkflow"));
}

#[tokio::test]
async fn assert_services_bound_passes_on_a_complete_surface() {
    let endpoint = Endpoint::builder()
        .bind(LashDurableWaitWorkflowImpl.serve())
        .bind(LashDurableWaitIndexImpl.serve())
        .bind(LashProcessAttachImpl.serve())
        .build();
    assert_services_bound(
        &endpoint,
        &[
            "LashDurableWaitWorkflow",
            "LashDurableWaitIndex",
            "LashProcessAttach",
        ],
    )
    .await
    .expect("endpoint binding every named service validates");
}

#[test]
fn restate_backend_requires_the_process_surface() {
    // One Restate backend runs both turns and processes, so its endpoint
    // must bind the durable-wait pair and the process services together.
    assert_eq!(
        RestateBackend::required_service_names(),
        RestateProcessDeployment::required_service_names()
    );
}

#[tokio::test]
async fn assert_services_bound_names_each_missing_service() {
    // Deliberately missing binding: the durable-wait index is absent, and the
    // process deployment's required set adds two more absent names. The check
    // must name all three rather than fail on the first.
    let endpoint = Endpoint::builder()
        .bind(LashDurableWaitWorkflowImpl.serve())
        .build();
    let error = assert_services_bound(
        &endpoint,
        RestateProcessDeployment::required_service_names().as_slice(),
    )
    .await
    .expect_err("an endpoint missing required services must not validate");
    let RestateBindingCheckError::UnboundServices(missing) = error else {
        panic!("a bound endpoint's discovery answer yields UnboundServices, not {error}");
    };
    assert_eq!(
        missing,
        vec![
            "LashDurableWaitIndex".to_string(),
            "LashProcessAttach".to_string(),
            "LashProcessWorkflow".to_string(),
        ]
    );
}

#[tokio::test]
async fn required_service_names_cover_every_lash_surface_family() {
    // The deployment-owned sets are disjoint per family and together equal the
    // full lash-provided surface a production endpoint binds.
    let all: std::collections::BTreeSet<&str> = RestateProcessDeployment::required_service_names()
        .into_iter()
        .chain(RestateEffectGroupServices::required_service_names())
        .collect();
    assert_eq!(
        all,
        [
            "EffectGroupDispatch",
            "EffectGroupIndex",
            "EffectGroupPayload",
            "LashDurableWaitIndex",
            "LashDurableWaitWorkflow",
            "LashProcessAttach",
            "LashProcessWorkflow",
        ]
        .into_iter()
        .collect()
    );
}
