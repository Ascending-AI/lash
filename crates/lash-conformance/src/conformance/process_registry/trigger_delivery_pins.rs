//! Trigger delivery pins (ADR 0021, FIG-4203): the pin a trigger delivery's
//! registration writes on its process until the delivery's bind commits.

use super::*;
use pretty_assertions::assert_eq;

/// A pinned row is never pruned, however long ago it retired, and its start
/// key keeps leading to it. Releasing the pin is idempotent, and a released
/// row is pruned as any other. The listing names exactly the pinned rows
/// with the delivery each awaits.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn a_trigger_delivery_pin_holds_its_row_until_released(
    registry: Arc<dyn ProcessRegistry>,
) {
    let pin = lash_core::TriggerDeliveryPin {
        occurrence_id: "pinned-occurrence".to_string(),
        subscription_id: "pinned-subscription".to_string(),
    };
    let start_key = lash_core::StartKey::for_host("trigger-delivery-pin-start");
    let pinned = registry
        .register_process(
            ProcessRegistration::new(
                ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            )
            .with_start_key(Some(start_key.clone()))
            .with_trigger_delivery_pin(Some(pin.clone())),
        )
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
        registry
            .complete_process(
                process_id,
                ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                    serde_json::Value::Null,
                )),
                crate::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete the process");
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
