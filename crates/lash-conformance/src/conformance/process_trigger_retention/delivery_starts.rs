//! A trigger delivery's start against its delivery (ADR 0021, ADR 0107 §5):
//! the pin its registration writes on its process until the bind commits
//! (FIG-4203), and the delivery row its registration reads once its start key
//! finds nothing (FIG-4369).

use super::*;
use pretty_assertions::assert_eq;

/// Reserve one delivery of `{name}`'s occurrence to `{name}`'s subscription.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn reserve_delivery(
    handles: &ProcessTriggerRetentionHandles,
    name: &str,
) -> crate::TriggerDeliveryReservation {
    let source = format!("{name}-source");
    register_trigger(
        &handles.triggers,
        &SessionId::fixture(format!("{name}-session")),
        &format!("{name}-key"),
        &source,
        &format!("{name}-register"),
    )
    .await;
    let mut ingress = handles
        .triggers
        .ingest_occurrence(crate::TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            source,
            serde_json::json!({ "button": "Blue" }),
            format!("{name}-occurrence"),
        ))
        .await
        .expect("ingest the occurrence");
    assert_eq!(ingress.reservations.len(), 1, "one subscription matches");
    ingress.reservations.remove(0)
}

fn delivery_pin(reservation: &crate::TriggerDeliveryReservation) -> crate::TriggerDeliveryPin {
    crate::TriggerDeliveryPin {
        occurrence_id: reservation.occurrence.occurrence_id.clone(),
        subscription_id: reservation.subscription.subscription_id.clone(),
    }
}

/// A registration pinned to `pin`'s delivery under `start_key`.
fn pinned_registration(
    start_key: &crate::StartKey,
    pin: &crate::TriggerDeliveryPin,
) -> ProcessRegistration {
    ProcessRegistration::new(
        ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        ProcessProvenance::host(),
        lash_core::Lifetime::Detached,
    )
    .with_admitted_identity(lash_core::AdmittedProcessIdentity::for_testing(
        ProcessIdentity::new("test"),
    ))
    .with_start_key(Some(start_key.clone()))
    .with_trigger_delivery_pin(Some(pin.clone()))
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn complete(handles: &ProcessTriggerRetentionHandles, process_id: &ProcessId) {
    handles
        .registry
        .complete_process(
            process_id,
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::Value::Null,
            )),
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete the process");
}

/// A pinned row is never pruned, however long ago it retired, and its start
/// key keeps leading to it. Releasing the pin is idempotent, and a released
/// row is pruned as any other. The listing names exactly the pinned rows
/// with the delivery each awaits.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn a_trigger_delivery_pin_holds_its_row_until_released(
    handles: ProcessTriggerRetentionHandles,
) {
    let registry = &handles.registry;
    let reservation = reserve_delivery(&handles, "delivery-pin").await;
    let pin = delivery_pin(&reservation);
    let start_key = crate::StartKey::for_host("trigger-delivery-pin-start");
    let pinned = registry
        .register_process(pinned_registration(&start_key, &pin))
        .await
        .expect("register a pinned process")
        .id;
    let unpinned = registry
        .register_process(ProcessRegistration::new(
            ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        ))
        .await
        .expect("register an unpinned process")
        .id;
    assert_eq!(
        registry
            .list_trigger_delivery_pins()
            .await
            .expect("list pins"),
        vec![lash_core::PinnedTriggerDelivery {
            process_id: pinned.clone(),
            pin,
        }],
        "the registration wrote the pin with the row"
    );
    for process_id in [&pinned, &unpinned] {
        complete(&handles, process_id).await;
    }

    let report = registry
        .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::NoProjector)
        .await
        .expect("prune");
    assert_eq!(
        report.pruned_processes, 1,
        "only the unpinned row is pruned"
    );
    assert_eq!(
        registry
            .get_process_by_start_key(&start_key)
            .await
            .expect("read the start key")
            .map(|record| record.id),
        Some(pinned.clone()),
        "the pinned row's start key still leads to it"
    );

    registry
        .release_trigger_delivery_pin(&pinned)
        .await
        .expect("release the pin");
    registry
        .release_trigger_delivery_pin(&pinned)
        .await
        .expect("release the pin again");
    registry
        .release_trigger_delivery_pin(&unpinned)
        .await
        .expect("releasing a pruned row's absent pin changes nothing");
    assert_eq!(
        registry
            .list_trigger_delivery_pins()
            .await
            .expect("list pins"),
        Vec::new(),
        "the release cleared the pin"
    );
    let report = registry
        .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::NoProjector)
        .await
        .expect("prune");
    assert_eq!(report.pruned_processes, 1, "the released row is pruned");
    assert_eq!(
        registry
            .get_process_by_start_key(&start_key)
            .await
            .expect("read the start key"),
        None,
        "past the prune, the start key finds nothing (ADR 0107)"
    );
}

/// A delivery's start whose key finds nothing registers nothing once the
/// delivery is bound or gone (FIG-4369).
///
/// The start key finds the delivery's process only while that process is
/// retained. After the delivery is bound and its process pruned, the key
/// finds nothing while the delivery stays bound, and a retention pass then
/// removes the delivery. A start that ingested the delivery before its bind
/// reaches registration in either state. The registrar reads the delivery's
/// row in the transaction that checked the key, and refuses: as bound,
/// naming the pruned process, and then as retired. Neither refusal registers
/// or pins anything.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn a_delivery_start_registers_nothing_once_its_process_was_pruned(
    handles: ProcessTriggerRetentionHandles,
) {
    let registry = &handles.registry;
    let reservation = reserve_delivery(&handles, "delivery-start-admission").await;
    let pin = delivery_pin(&reservation);
    let start_key = crate::DERIVED_START_KEYS.for_trigger_delivery(
        &reservation.occurrence.occurrence_id,
        &reservation.subscription.subscription_id,
        &reservation.subscription.incarnation,
        reservation.subscription.revision,
    );

    // The reserved, unbound delivery's start registers its process.
    let bound = registry
        .register_process(pinned_registration(&start_key, &pin))
        .await
        .expect("the unbound delivery's start registers")
        .id;
    handles
        .triggers
        .bind_delivery_process(&pin.occurrence_id, &pin.subscription_id, &bound)
        .await
        .expect("bind the delivery");
    registry
        .release_trigger_delivery_pin(&bound)
        .await
        .expect("release the delivery's pin");
    complete(&handles, &bound).await;
    let report = registry
        .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::NoProjector)
        .await
        .expect("prune");
    assert_eq!(report.pruned_processes, 1, "the bound process is pruned");

    // Bound, its process pruned: the key finds nothing, and the start is
    // refused naming the bound process.
    match registry
        .register_process(pinned_registration(&start_key, &pin))
        .await
    {
        Err(crate::PluginError::TriggerDeliveryBound {
            occurrence_id,
            subscription_id,
            process_id,
        }) => assert_eq!(
            (occurrence_id, subscription_id, process_id),
            (
                pin.occurrence_id.clone(),
                pin.subscription_id.clone(),
                bound.clone()
            ),
            "the refusal names the delivery and its bound process"
        ),
        other => panic!("a bound delivery's start must refuse as bound: {other:?}"),
    }
    assert_nothing_registered(&handles, &start_key).await;

    // A retention pass removes the delivery: the start is refused as retired.
    crate::reconcile_pruned_trigger_deliveries(
        registry.as_ref(),
        handles.triggers.as_ref(),
        Some(handles.sessions.as_ref()),
    )
    .await
    .expect("reconcile pruned trigger deliveries");
    assert_eq!(
        handles
            .triggers
            .list_deliveries_by_occurrence_id(&pin.occurrence_id)
            .await
            .expect("list the delivery"),
        Vec::new(),
        "retention removed the delivery of the pruned process"
    );
    match registry
        .register_process(pinned_registration(&start_key, &pin))
        .await
    {
        Err(crate::PluginError::TriggerDeliveryRetired {
            occurrence_id,
            subscription_id,
        }) => assert_eq!(
            (occurrence_id, subscription_id),
            (pin.occurrence_id.clone(), pin.subscription_id.clone()),
            "the refusal names the delivery"
        ),
        other => panic!("a removed delivery's start must refuse as retired: {other:?}"),
    }
    assert_nothing_registered(&handles, &start_key).await;
}

/// No process holds `start_key`, and nothing is pinned.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn assert_nothing_registered(
    handles: &ProcessTriggerRetentionHandles,
    start_key: &crate::StartKey,
) {
    assert_eq!(
        handles
            .registry
            .get_process_by_start_key(start_key)
            .await
            .expect("read the start key"),
        None,
        "the refused start registered nothing"
    );
    assert_eq!(
        handles
            .registry
            .list_trigger_delivery_pins()
            .await
            .expect("list pins"),
        Vec::new(),
        "the refused start pinned nothing"
    );
}
