use super::*;
use pretty_assertions::assert_eq;

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn long_cancellation_requester_replay_is_backend_safe(
    registry: Arc<dyn ProcessRegistry>,
) {
    let record = registry
        .register_process(registration("long-cancellation-requester-replay"))
        .await
        .expect("register long-cancellation process");
    let process_id = record.id.clone();
    let requester = (0..800)
        .map(|index| format!("{index:08x}"))
        .collect::<String>();
    let cancel = || {
        registry.request_process_cancel(
            &process_id,
            lash_core::CancelOrigin::OperatorRequested,
            format!("actor:{requester}"),
            None,
        )
    };
    let first = cancel()
        .await
        .expect("request cancellation with long requester");
    let replay = cancel()
        .await
        .expect("repeat cancellation with long requester");
    assert_eq!(
        replay.last_event_sequence, first.last_event_sequence,
        "long cancellation requester retries must remain idempotent on every backend"
    );
}

/// A cancelled terminal is stored with the standing cancel request's origin,
/// so a request that carries none is that same fact when it is presented
/// again: it coalesces on the recorded event, whether the event's payload is
/// retained or released (FIG-5509).
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn an_unstamped_cancelled_terminal_replay_coalesces(
    registry: Arc<dyn ProcessRegistry>,
) {
    for released in [false, true] {
        let process_id = registry
            .register_process(executed_registration(&format!(
                "unstamped-cancelled-terminal-replay:{released}"
            )))
            .await
            .expect("register the process a cancel ends")
            .id;
        let authority = start_runner(registry.as_ref(), &process_id).await;
        registry
            .request_process_cancel(
                &process_id,
                lash_core::CancelOrigin::OperatorRequested,
                "actor:operator".to_string(),
                None,
            )
            .await
            .expect("record the standing cancel request");
        let request = crate::terminal_append_request(
            &process_id,
            &ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::cancelled(
                crate::ToolCancellation::runtime("runner stopped"),
            )),
            None,
        );
        let first = registry
            .append_event_with_authority(&process_id, request.clone(), &authority)
            .await
            .expect("append the unstamped cancelled terminal");
        let Some(crate::ProcessTerminal::Settled { output }) = first.event.terminal() else {
            panic!("the terminal append records a settled outcome");
        };
        let crate::ToolCallOutcome::Cancelled(cancellation) = &output.outcome else {
            panic!("the terminal append records a cancellation");
        };
        assert_eq!(
            cancellation.origin,
            Some(lash_core::CancelOrigin::OperatorRequested),
            "the stored terminal carries the standing request's origin"
        );
        if released {
            registry
                .release_process_events(&process_id, first.event.sequence)
                .await
                .expect("release the terminal's payload");
        }
        let replay = registry
            .append_event_with_authority(&process_id, request, &authority)
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "the same unstamped terminal is the same fact (released: {released}): {error}"
                )
            });
        assert_eq!(replay.event.sequence, first.event.sequence);
        assert_eq!(replay.realization, crate::StoreRealization::Coalesced);
        assert_eq!(replay.event.fact, first.event.fact);
    }
}
