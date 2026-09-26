//! The process cancel race on the SDK's own endpoint (FIG-3673).
//!
//! Each process-drive wait that observes no turn journals its guarded command
//! and then a `GetPromise` of the segment's `process_cancel_requested`
//! promise, and the VM's first-completed await decides the winner. These laws
//! run the real controller context over the SDK endpoint double: a first
//! attempt records both commands and suspends, then a redrive completes both
//! in each notification order and must take the branch the first completion
//! names. The losing side is disposed of: a lost event wait is released
//! `Cancelled`, a lost process await's call is cancelled.

use super::endpoint_protocol::{
    encode_call_completion, encode_get_promise_completion, encode_input_command,
    encode_invocation_id_completion, encode_start_message, protobuf_varint_field,
};
use super::*;
use crate::controller::context::{ProcessCancelRace, RestateControllerContext};
use crate::durable_wait::{
    RestateDurableWaitAwaitRequest, RestateDurableWaitResolveResponse, RestateTurnCancelRaceOutcome,
};
use bytes::BytesMut;

const PROBE: &str = "P16RaceProbe";
const CALL_COMMAND: u16 = 0x040D;
const GET_PROMISE_COMMAND: u16 = 0x0409;
const SEND_SIGNAL_COMMAND: u16 = 0x0410;

#[derive(Debug, Serialize, serde::Deserialize)]
struct RaceProbeInput {
    wait: String,
}

#[restate_sdk::workflow]
trait P16RaceProbe {
    async fn run(input: Json<RaceProbeInput>) -> HandlerResult<Json<String>>;
}

struct P16RaceProbeImpl;

fn race_label<T>(outcome: RestateTurnCancelRaceOutcome<T>) -> String {
    match outcome {
        RestateTurnCancelRaceOutcome::Completed(_) => "completed",
        RestateTurnCancelRaceOutcome::ProcessCancelled => "process_cancelled",
        RestateTurnCancelRaceOutcome::TurnCancelled => "turn_cancelled",
        RestateTurnCancelRaceOutcome::SessionRevoked { .. } => "session_revoked",
    }
    .to_string()
}

fn probe_wait_request() -> Result<RestateDurableWaitAwaitRequest, TerminalError> {
    let key = restate_await_event_key_for_authority(
        &test_restate_authority_id(),
        &ExecutionScope::process(ProcessId::fixture("p16-race-probe")),
        AwaitEventWaitIdentity::Custom {
            key: "probe".to_string(),
        },
    )
    .map_err(TerminalError::from_error)?;
    Ok(RestateDurableWaitAwaitRequest {
        key,
        deadline: None,
    })
}

impl P16RaceProbe for P16RaceProbeImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(input): Json<RaceProbeInput>,
    ) -> HandlerResult<Json<String>> {
        let label = match input.wait.as_str() {
            "event" => race_label(
                RestateControllerContext::await_event_or_turn_cancel(
                    &ctx,
                    probe_wait_request()?,
                    "probe".to_string(),
                    None,
                    ProcessCancelRace::Raced,
                )
                .await?,
            ),
            "rank" => race_label(
                RestateControllerContext::await_effect_group_wait(
                    &ctx,
                    probe_wait_request()?,
                    "probe".to_string(),
                    None,
                    ProcessCancelRace::Raced,
                )
                .await?,
            ),
            "process" => race_label(
                RestateControllerContext::await_process_terminal_or_turn_cancel(
                    &ctx,
                    ProcessId::fixture("p16-race-probe-child"),
                    None,
                    ProcessCancelRace::Raced,
                )
                .await?,
            ),
            other => return Err(TerminalError::new(format!("unknown probe wait `{other}`")).into()),
        };
        Ok(Json(label))
    }
}

#[derive(Clone, Copy, Debug)]
enum FirstCompletion {
    Guarded,
    Promise,
}

/// The value that completes the guarded call of `wait`.
fn guarded_completion(wait: &str) -> serde_json::Value {
    match wait {
        "process" => serde_json::to_value(process_success(serde_json::json!("child done")))
            .expect("encode the child's terminal"),
        _ => serde_json::to_value(Resolution::Ok(serde_json::json!("resolved")))
            .expect("encode the wait's resolution"),
    }
}

/// The payload of a promise that holds an accepted cancel request.
fn cancel_promise_value() -> Vec<u8> {
    serde_json::to_vec(
        &serde_json::to_string(&crate::process::RestateProcessCancelSignal::CancelRequested)
            .expect("encode the cancel signal"),
    )
    .expect("encode the promise payload")
}

/// One probe run: the first attempt records the guarded call and the promise
/// and suspends; the redrive completes both, `first` first, and returns the
/// winner and the redrive's frames.
async fn race_with(wait: &str, first: FirstCompletion) -> (String, Bytes) {
    let endpoint = Endpoint::builder().bind(P16RaceProbeImpl.serve()).build();
    let key = format!("p16-race-{wait}");
    let input = RaceProbeInput {
        wait: wait.to_string(),
    };
    let recorded = invoke_endpoint(&endpoint, PROBE, "run", &key, &input)
        .await
        .expect("the first attempt records its race and suspends");
    let commands = restate_recorded_commands(&recorded).expect("decode the recorded race");
    assert_eq!(
        commands
            .iter()
            .map(|command| command.message_type)
            .collect::<Vec<_>>(),
        vec![CALL_COMMAND, GET_PROMISE_COMMAND],
        "the guarded command, then the promise"
    );
    let call = &commands[0];
    let promise = &commands[1];
    let mut body = BytesMut::new();
    body.extend_from_slice(&encode_start_message(&key, 1 + commands.len() as u32));
    body.extend_from_slice(&encode_input_command(
        &serde_json::to_vec(&input).expect("encode the probe input"),
    ));
    for command in &commands {
        body.extend_from_slice(&command.frame);
    }
    if wait == "process" {
        let invocation_id_index = u32::try_from(
            protobuf_varint_field(call.frame.get(8..).expect("call payload"), 10)
                .expect("the call's invocation-id index"),
        )
        .expect("invocation-id index fits u32");
        body.extend_from_slice(&encode_invocation_id_completion(
            invocation_id_index,
            "inv_p16_race_probe_child",
        ));
    }
    let guarded = encode_call_completion(
        call.completion_id.expect("the call's completion id"),
        &serde_json::to_vec(&guarded_completion(wait)).expect("encode the guarded completion"),
    );
    let cancelled = encode_get_promise_completion(
        promise.completion_id.expect("the promise's completion id"),
        &cancel_promise_value(),
    );
    match first {
        FirstCompletion::Guarded => {
            body.extend_from_slice(&guarded);
            body.extend_from_slice(&cancelled);
        }
        FirstCompletion::Promise => {
            body.extend_from_slice(&cancelled);
            body.extend_from_slice(&guarded);
        }
    }
    let release = serde_json::to_value(RestateDurableWaitResolveResponse::Outcome(
        lash_core::ResolveOutcome::Accepted,
    ))
    .expect("encode the release answer");
    let output = invoke_endpoint_body_with_json_call_responses(
        &endpoint,
        PROBE,
        "run",
        body.freeze(),
        vec![release],
    )
    .await
    .expect("the redrive settles its race");
    let label = restate_output_json::<String>(&output).unwrap_or_else(|| {
        panic!(
            "the probe returns its winner: {:?}",
            restate_error_message(&output)
        )
    });
    (label, output)
}

async fn assert_both_orders(wait: &str) {
    let (guarded_first, guarded_output) = race_with(wait, FirstCompletion::Guarded).await;
    assert_eq!(
        guarded_first, "completed",
        "{wait}: the guarded wait completed first, so it wins"
    );
    assert!(
        restate_call_frames(&guarded_output)
            .expect("decode the redrive's calls")
            .is_empty(),
        "{wait}: a guarded win disposes of nothing"
    );
    let (promise_first, promise_output) = race_with(wait, FirstCompletion::Promise).await;
    assert_eq!(
        promise_first, "process_cancelled",
        "{wait}: the cancel promise completed first, so it wins"
    );
    let calls = restate_call_frames(&promise_output).expect("decode the redrive's calls");
    match wait {
        "event" => assert_eq!(
            calls
                .iter()
                .map(|call| (call.service.as_str(), call.handler.as_str()))
                .collect::<Vec<_>>(),
            vec![("LashDurableWaitIndex", "resolve")],
            "event: the lost event wait is released"
        ),
        "process" => {
            assert!(calls.is_empty(), "process: no new call");
            assert!(
                restate_message_types(&promise_output)
                    .expect("decode the redrive's frames")
                    .contains(&SEND_SIGNAL_COMMAND),
                "process: the lost await's call is cancelled"
            );
        }
        _ => assert!(calls.is_empty(), "{wait}: nothing to release"),
    }
}

#[tokio::test]
pub(super) async fn a_process_event_wait_race_takes_the_first_recorded_completion() {
    assert_both_orders("event").await;
}

#[tokio::test]
pub(super) async fn a_process_rank_wait_race_takes_the_first_recorded_completion() {
    assert_both_orders("rank").await;
}

#[tokio::test]
pub(super) async fn a_process_await_race_takes_the_first_recorded_completion() {
    assert_both_orders("process").await;
}
