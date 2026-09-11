//! Real pinned-SDK replay and handler fixtures for the `WorkbenchCronJob` tick.
//!
//! These fixtures speak the Restate service protocol directly so the handler's
//! journal can be replayed through the same shared-core VM the deployment runs.
//! Pure-unit tests cannot pin two FIG-1071 properties:
//!
//! 1. A pre-FIG-1071 journal whose first `ctx.run` already completed with the
//!    legacy session-only value must replay without shifting any later syscall.
//!    The run command is matched by header equality (name plus completion id),
//!    so an inserted or renamed syscall fails the replay instead of silently
//!    re-executing.
//! 2. A live session whose registration is absent or disabled must cancel the
//!    tick at the handler level with no re-arm command and no delivery.

use super::*;
use crate::restate::WorkbenchCronJob as _;
use bytes::{BufMut, Bytes, BytesMut};
use http::Request;
use http_body_util::channel::Channel;
use http_body_util::{BodyExt as _, Full};
use lash::SessionId;
use restate_sdk::prelude::Endpoint;
use std::convert::Infallible;

const INVOCATION_CONTENT_TYPE: &str = "application/vnd.restate.invocation.v6";

const MSG_START: u16 = 0x0000;
const MSG_RUN_PROPOSAL: u16 = 0x0005;
const MSG_INPUT: u16 = 0x0400;
const MSG_OUTPUT: u16 = 0x0401;
const MSG_GET_LAZY_STATE: u16 = 0x0402;
const MSG_CLEAR_STATE: u16 = 0x0404;
const MSG_SLEEP: u16 = 0x040C;
const MSG_CALL: u16 = 0x040D;
const MSG_ONE_WAY: u16 = 0x040E;
const MSG_RUN: u16 = 0x0411;
const MSG_GET_LAZY_STATE_DONE: u16 = 0x8002;
const MSG_RUN_DONE: u16 = 0x8011;

fn encode_restate_message(message_type: u16, payload: Vec<u8>) -> Bytes {
    let mut encoded = BytesMut::with_capacity(8 + payload.len());
    let header = ((message_type as u64) << 48) | payload.len() as u64;
    encoded.put_u64(header);
    encoded.extend_from_slice(&payload);
    encoded.freeze()
}

fn put_varint(buf: &mut BytesMut, mut value: u64) {
    while value >= 0x80 {
        buf.put_u8(((value as u8) & 0x7f) | 0x80);
        value >>= 7;
    }
    buf.put_u8(value as u8);
}

fn put_field_key(buf: &mut BytesMut, field_number: u32, wire_type: u8) {
    put_varint(buf, ((field_number as u64) << 3) | wire_type as u64);
}

fn put_varint_field(buf: &mut BytesMut, field_number: u32, value: u64) {
    put_field_key(buf, field_number, 0);
    put_varint(buf, value);
}

fn put_len_field(buf: &mut BytesMut, field_number: u32, value: &[u8]) {
    put_field_key(buf, field_number, 2);
    put_varint(buf, value.len() as u64);
    buf.extend_from_slice(value);
}

fn decode_varint(input: &[u8], cursor: &mut usize) -> Option<u64> {
    let mut value = 0_u64;
    for shift in (0..64).step_by(7) {
        let byte = *input.get(*cursor)?;
        *cursor += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

fn protobuf_len_field(input: &[u8], target: u64) -> Option<&[u8]> {
    let mut cursor = 0;
    while cursor < input.len() {
        let key = decode_varint(input, &mut cursor)?;
        let field = key >> 3;
        match key & 7 {
            0 => {
                let _ = decode_varint(input, &mut cursor)?;
            }
            2 => {
                let len = usize::try_from(decode_varint(input, &mut cursor)?).ok()?;
                let end = cursor.checked_add(len)?;
                let value = input.get(cursor..end)?;
                if field == target {
                    return Some(value);
                }
                cursor = end;
            }
            _ => return None,
        }
    }
    None
}

fn protobuf_varint_field(input: &[u8], target: u64) -> Option<u64> {
    let mut cursor = 0;
    while cursor < input.len() {
        let key = decode_varint(input, &mut cursor)?;
        let field = key >> 3;
        match key & 7 {
            0 => {
                let value = decode_varint(input, &mut cursor)?;
                if field == target {
                    return Some(value);
                }
            }
            2 => {
                let len = usize::try_from(decode_varint(input, &mut cursor)?).ok()?;
                cursor = cursor.checked_add(len)?;
                if cursor > input.len() {
                    return None;
                }
            }
            _ => return None,
        }
    }
    None
}

fn encode_start_message(object_key: &str, known_entries: u32) -> Bytes {
    let mut payload = BytesMut::new();
    put_len_field(&mut payload, 1, object_key.as_bytes());
    put_len_field(&mut payload, 2, object_key.as_bytes());
    put_varint_field(&mut payload, 3, u64::from(known_entries));
    // Partial state makes a missing key an unknown (lazy) read rather than an
    // authoritative empty one, so non-replay invocations exercise the state
    // command round trip the driver answers.
    put_varint_field(&mut payload, 5, 1);
    put_len_field(&mut payload, 6, object_key.as_bytes());
    encode_restate_message(MSG_START, payload.to_vec())
}

fn encode_input_command(payload: &[u8]) -> Bytes {
    let mut value = BytesMut::new();
    put_len_field(&mut value, 1, payload);

    let mut command = BytesMut::new();
    put_len_field(&mut command, 14, &value);
    encode_restate_message(MSG_INPUT, command.to_vec())
}

fn encode_get_lazy_state_command(key: &str, completion_id: u32) -> Bytes {
    let mut payload = BytesMut::new();
    put_len_field(&mut payload, 1, key.as_bytes());
    put_varint_field(&mut payload, 11, u64::from(completion_id));
    encode_restate_message(MSG_GET_LAZY_STATE, payload.to_vec())
}

fn encode_run_command(completion_id: u32, name: &str) -> Bytes {
    let mut payload = BytesMut::new();
    put_varint_field(&mut payload, 11, u64::from(completion_id));
    put_len_field(&mut payload, 12, name.as_bytes());
    encode_restate_message(MSG_RUN, payload.to_vec())
}

fn encode_get_lazy_state_completion(completion_id: u32, value: Option<&[u8]>) -> Bytes {
    let mut payload = BytesMut::new();
    put_varint_field(&mut payload, 1, u64::from(completion_id));
    match value {
        Some(value) => {
            let mut nested = BytesMut::new();
            put_len_field(&mut nested, 1, value);
            put_len_field(&mut payload, 5, &nested);
        }
        None => put_len_field(&mut payload, 4, &[]),
    }
    encode_restate_message(MSG_GET_LAZY_STATE_DONE, payload.to_vec())
}

fn encode_run_completion(completion_id: u32, value: &[u8]) -> Bytes {
    let mut nested_value = BytesMut::new();
    put_len_field(&mut nested_value, 1, value);
    let mut notification = BytesMut::new();
    put_varint_field(&mut notification, 1, u64::from(completion_id));
    put_len_field(&mut notification, 5, &nested_value);
    encode_restate_message(MSG_RUN_DONE, notification.to_vec())
}

fn proposed_run_completion(payload: &[u8]) -> Option<(u32, &[u8])> {
    let mut cursor = 0;
    let mut completion_id = None;
    let mut value = None;
    while cursor < payload.len() {
        let key = decode_varint(payload, &mut cursor)?;
        let field = key >> 3;
        match key & 7 {
            0 => {
                let parsed = decode_varint(payload, &mut cursor)?;
                if field == 1 {
                    completion_id = u32::try_from(parsed).ok();
                }
            }
            2 => {
                let len = usize::try_from(decode_varint(payload, &mut cursor)?).ok()?;
                let end = cursor.checked_add(len)?;
                let bytes = payload.get(cursor..end)?;
                if field == 14 {
                    value = Some(bytes);
                }
                cursor = end;
            }
            _ => return None,
        }
    }
    Some((completion_id?, value?))
}

/// Every message frame in `input` with its type, including the 8-byte header.
fn restate_frames(input: &[u8]) -> Vec<(u16, Bytes)> {
    let mut cursor = 0;
    let mut frames = Vec::new();
    while cursor + 8 <= input.len() {
        let header = u64::from_be_bytes(
            input[cursor..cursor + 8]
                .try_into()
                .expect("restate frame header"),
        );
        let message_type = (header >> 48) as u16;
        let payload_len =
            usize::try_from(header & 0x0000_FFFF_FFFF_FFFF).expect("restate frame payload length");
        let frame_end = cursor + 8 + payload_len;
        if frame_end > input.len() {
            break;
        }
        frames.push((
            message_type,
            Bytes::copy_from_slice(&input[cursor..frame_end]),
        ));
        cursor = frame_end;
    }
    frames
}

fn command_frame_types(output: &[u8]) -> Vec<u16> {
    restate_frames(output)
        .into_iter()
        .map(|(message_type, _)| message_type)
        .filter(|message_type| (0x0400..0x0500).contains(message_type))
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
struct RunCommandFrame {
    completion_id: u32,
    name: String,
}

fn run_command_frames(output: &[u8]) -> Vec<RunCommandFrame> {
    restate_frames(output)
        .into_iter()
        .filter(|(message_type, _)| *message_type == MSG_RUN)
        .map(|(_, frame)| {
            let payload = frame.get(8..).expect("run command payload");
            RunCommandFrame {
                completion_id: u32::try_from(
                    protobuf_varint_field(payload, 11).expect("run command completion id"),
                )
                .expect("run completion id fits u32"),
                name: String::from_utf8(
                    protobuf_len_field(payload, 12)
                        .expect("run command name")
                        .to_vec(),
                )
                .expect("run command name must be UTF-8"),
            }
        })
        .collect()
}

fn encode_invocation_body(object_key: &str, input: &[u8]) -> Bytes {
    let start = encode_start_message(object_key, 1);
    let input = encode_input_command(input);
    let mut body = BytesMut::with_capacity(start.len() + input.len());
    body.extend_from_slice(&start);
    body.extend_from_slice(&input);
    body.freeze()
}

fn encode_replay_body(object_key: &str, commands: &[Bytes], notifications: &[Bytes]) -> Bytes {
    let input = serde_json::to_vec(&()).expect("encode unit cron run input");
    let known_entries =
        u32::try_from(1 + commands.len() + notifications.len()).expect("known entry count");
    let mut body = BytesMut::new();
    body.extend_from_slice(&encode_start_message(object_key, known_entries));
    body.extend_from_slice(&encode_input_command(&input));
    for command in commands {
        body.extend_from_slice(command);
    }
    for notification in notifications {
        body.extend_from_slice(notification);
    }
    body.freeze()
}

fn cron_endpoint(state: crate::AppState) -> Endpoint {
    Endpoint::builder()
        .bind(crate::restate::WorkbenchCronJobImpl::new(state).serve())
        .build()
}

async fn invoke_cron_run_body(endpoint: &Endpoint, body: Bytes) -> Result<Bytes, String> {
    let response = endpoint.handle(
        Request::builder()
            .uri("/invoke/WorkbenchCronJob/run")
            .header(http::header::CONTENT_TYPE, INVOCATION_CONTENT_TYPE)
            .body(Full::new(body))
            .expect("cron run invocation request"),
    );
    let status = response.status();
    if !status.is_success() {
        return Err(format!("cron run endpoint returned status {status}"));
    }
    response
        .into_body()
        .collect()
        .await
        .map(|body| body.to_bytes())
        .map_err(|error| format!("cron run endpoint body failed: {error}"))
}

/// Drive a fresh (non-replay) `run` invocation to completion, answering the
/// handler's state read and accepting every run proposal it makes.
async fn invoke_cron_run_driven(
    endpoint: &Endpoint,
    object_key: &str,
    state: &crate::restate::WorkbenchCronState,
) -> Result<Bytes, String> {
    let state_json = serde_json::to_vec(state).map_err(|error| error.to_string())?;
    let body = encode_invocation_body(object_key, &serde_json::to_vec(&()).unwrap());
    let (mut sender, channel_body) = Channel::<Bytes, Infallible>::new(8);
    sender
        .send_data(body)
        .await
        .map_err(|error| format!("cron run input failed: {error}"))?;
    let mut sender = Some(sender);
    let response = endpoint.handle(
        Request::builder()
            .uri("/invoke/WorkbenchCronJob/run")
            .header(http::header::CONTENT_TYPE, INVOCATION_CONTENT_TYPE)
            .body(channel_body)
            .expect("cron run invocation request"),
    );
    let status = response.status();
    if !status.is_success() {
        return Err(format!("cron run endpoint returned status {status}"));
    }
    let mut response = response.into_body();
    let mut output = BytesMut::new();
    let mut decoded = 0usize;
    while let Some(frame) = response.frame().await {
        let frame = frame.map_err(|error| format!("cron run endpoint body failed: {error}"))?;
        let Ok(data) = frame.into_data() else {
            continue;
        };
        output.extend_from_slice(&data);
        while output.len().saturating_sub(decoded) >= 8 {
            let header = u64::from_be_bytes(
                output[decoded..decoded + 8]
                    .try_into()
                    .expect("restate frame header"),
            );
            let message_type = (header >> 48) as u16;
            let payload_len = usize::try_from(header & 0x0000_FFFF_FFFF_FFFF)
                .expect("restate frame payload length");
            let frame_end = decoded + 8 + payload_len;
            if output.len() < frame_end {
                break;
            }
            let payload = &output[decoded + 8..frame_end];
            match message_type {
                MSG_GET_LAZY_STATE => {
                    let completion_id = u32::try_from(
                        protobuf_varint_field(payload, 11).expect("state completion id"),
                    )
                    .expect("state completion id fits u32");
                    sender
                        .as_mut()
                        .expect("cron run input stays open")
                        .send_data(encode_get_lazy_state_completion(
                            completion_id,
                            Some(&state_json),
                        ))
                        .await
                        .map_err(|error| format!("state completion failed: {error}"))?;
                }
                MSG_RUN_PROPOSAL => {
                    let (completion_id, value) = proposed_run_completion(payload)
                        .ok_or("invalid run completion proposal")?;
                    sender
                        .as_mut()
                        .expect("cron run input stays open")
                        .send_data(encode_run_completion(completion_id, value))
                        .await
                        .map_err(|error| format!("run completion failed: {error}"))?;
                }
                MSG_OUTPUT => {
                    drop(sender.take());
                }
                _ => {}
            }
            decoded = frame_end;
        }
    }
    drop(sender);
    Ok(output.freeze())
}

fn replay_cron_state(
    session_id: &SessionId,
    source_key: &str,
) -> crate::restate::WorkbenchCronState {
    crate::restate::WorkbenchCronState {
        request: WorkbenchCronRequest {
            session_id: SessionId::from(session_id.to_string()),
            source_key: source_key.to_string(),
            expr: "*/10 * * * * *".to_string(),
            tz: Some("UTC".to_string()),
            name: Some("FIG-1071 replay".to_string()),
        },
        next_execution_time: "2026-08-08T12:00:10+00:00".to_string(),
        next_execution_id: "invocation-fig1071-replay".to_string(),
        last_fired_at: None,
    }
}

async fn materialize_session(state: &crate::AppState, session_id: &SessionId) {
    drop(
        state
            .core
            .session(session_id)
            .open()
            .await
            .expect("materialize FIG-1071 replay session"),
    );
}

async fn replay_fixture(
    data_dir: &tempfile::TempDir,
    source_key: &str,
) -> (crate::AppState, SessionId, String) {
    let trigger_store = Arc::new(lash::triggers::InMemoryTriggerStore::default());
    let state = crate::tests::recoverable_chat_test_state_with_trigger_store(
        data_dir.path(),
        Arc::clone(&trigger_store) as Arc<dyn lash::triggers::TriggerStore>,
    )
    .await;
    let session_id = state.current_session_id();
    materialize_session(&state, &session_id).await;
    let object_key = crate::restate::cron_job_key(&session_id, source_key);
    (state, session_id, object_key)
}

#[tokio::test]
async fn legacy_live_basis_replay_continues_through_journaled_downstream_commands() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let source_key = "cron-source:fig1071-replay-live";
    let (state, session_id, object_key) = replay_fixture(&data_dir, source_key).await;
    // No registration exists, so a freshly executed basis would cancel. The
    // journaled legacy `live` value must be replayed instead, and the already
    // journaled fired-at run must be replayed too rather than re-emitted.
    let cron_state = replay_cron_state(&session_id, source_key);
    let state_json = serde_json::to_vec(&cron_state).expect("encode cron state");
    let endpoint = cron_endpoint(state);

    let body = encode_replay_body(
        &object_key,
        &[
            encode_get_lazy_state_command(crate::restate::CRON_STATE_KEY, 1),
            encode_run_command(2, "workbench-cron:tick-basis"),
            encode_run_command(3, "workbench-cron:fired-at"),
        ],
        &[
            encode_get_lazy_state_completion(1, Some(&state_json)),
            encode_run_completion(2, b"\"live\""),
            encode_run_completion(3, b"\"2026-08-08T12:00:10+00:00\""),
        ],
    );
    let output = invoke_cron_run_body(&endpoint, body)
        .await
        .expect("legacy live replay must continue");

    let runs = run_command_frames(&output);
    assert!(
        runs.iter().all(|run| run.name != "workbench-cron:fired-at"),
        "an already-journaled downstream run must not be re-emitted: {runs:?}"
    );
    let types = command_frame_types(&output);
    assert!(
        types
            .iter()
            .any(|message_type| [MSG_SLEEP, MSG_CALL, MSG_ONE_WAY].contains(message_type)),
        "a replayed live tick must continue to its re-arm command: {types:?}"
    );
}

#[tokio::test]
async fn legacy_retired_and_unknown_basis_replay_enter_the_cancel_path() {
    for legacy in ["retired", "unknown"] {
        let data_dir = tempfile::tempdir().expect("tempdir");
        let source_key = "cron-source:fig1071-replay-cancel";
        let (state, session_id, object_key) = replay_fixture(&data_dir, source_key).await;
        let cron_state = replay_cron_state(&session_id, source_key);
        let state_json = serde_json::to_vec(&cron_state).expect("encode cron state");
        let endpoint = cron_endpoint(state);

        let body = encode_replay_body(
            &object_key,
            &[
                encode_get_lazy_state_command(crate::restate::CRON_STATE_KEY, 1),
                encode_run_command(2, "workbench-cron:tick-basis"),
            ],
            &[
                encode_get_lazy_state_completion(1, Some(&state_json)),
                encode_run_completion(2, format!("\"{legacy}\"").as_bytes()),
            ],
        );
        let output = invoke_cron_run_body(&endpoint, body)
            .await
            .unwrap_or_else(|error| panic!("legacy `{legacy}` replay must continue: {error}"));

        let runs = run_command_frames(&output);
        assert_eq!(
            runs.first(),
            Some(&RunCommandFrame {
                completion_id: 3,
                name: "workbench-cron:trace-cancelled".to_string(),
            }),
            "a legacy `{legacy}` value must reach the cancel trace run at completion id 2"
        );
    }
}

#[tokio::test]
async fn unfinished_basis_run_replay_reissues_the_same_run_identity() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let source_key = "cron-source:fig1071-replay-unfinished";
    let (state, session_id, object_key) = replay_fixture(&data_dir, source_key).await;
    let cron_state = replay_cron_state(&session_id, source_key);
    let state_json = serde_json::to_vec(&cron_state).expect("encode cron state");
    let endpoint = cron_endpoint(state);

    // The first run is journaled but never completed, so replay must re-open the
    // same run identity at the same completion id and suspend there.
    let body = encode_replay_body(
        &object_key,
        &[
            encode_get_lazy_state_command(crate::restate::CRON_STATE_KEY, 1),
            encode_run_command(2, "workbench-cron:tick-basis"),
        ],
        &[encode_get_lazy_state_completion(1, Some(&state_json))],
    );
    let output = invoke_cron_run_body(&endpoint, body)
        .await
        .expect("unfinished basis replay must suspend at the run");

    // The unfinished run already owns its journal entry, so the handler does not
    // re-emit a RunCommand; it re-executes the closure and proposes on the same
    // completion id. A shifted syscall or renamed run would instead mismatch.
    let frames = restate_frames(&output);
    let proposal_id = frames
        .iter()
        .find(|(message_type, _)| *message_type == MSG_RUN_PROPOSAL)
        .and_then(|(_, frame)| proposed_run_completion(frame.get(8..)?).map(|(id, _)| id));
    assert_eq!(
        proposal_id,
        Some(2),
        "the unfinished run must propose on its own completion id"
    );
    assert!(
        run_command_frames(&output).is_empty(),
        "an already-journaled run must not be re-emitted"
    );
}

async fn register_then_disable(
    trigger_store: &lash::triggers::InMemoryTriggerStore,
    session_id: &SessionId,
    source_key: &str,
) {
    let subscription_key = format!("cron-test:fig1071-handler:{source_key}");
    let outcome = lash::triggers::TriggerStore::execute_command(
        trigger_store,
        &format!("register:fig1071-handler:{source_key}"),
        lash::triggers::TriggerCommand::Register {
            owner_scope: lash::triggers::TriggerOwnerScope::session(session_id),
            actor: lash::process::ProcessOriginator::session(lash::process::SessionScope::new(
                session_id,
            )),
            draft: lash::triggers::TriggerSubscriptionDraft::for_process(
                subscription_key,
                lash::process::ProcessExecutionEnvRef::new(format!("process-env:{source_key}")),
                crate::CRON_SCHEDULE_SOURCE_TYPE,
                source_key,
                lash::process::ProcessInput::Engine {
                    kind: "cron-test-engine".to_string(),
                    payload: serde_json::json!({}),
                },
                lash::process::ProcessIdentity::new("cron-test-engine"),
            )
            .with_payload_schema(lash::triggers::LashSchema::any()),
        },
    )
    .await
    .expect("register FIG-1071 handler replay trigger")
    .expect("FIG-1071 handler replay trigger mutation");
    let lash::triggers::TriggerCommandOutcome::Mutation { receipt } = outcome else {
        panic!("register must return a mutation receipt");
    };
    let record = receipt.record_snapshot;
    lash::triggers::TriggerStore::execute_command(
        trigger_store,
        &format!("disable:fig1071-handler:{source_key}"),
        lash::triggers::TriggerCommand::Disable {
            owner_scope: record.owner_scope.clone(),
            actor: record.registrant.clone(),
            subscription_key: record.subscription_key.clone(),
            expected_revision: record.revision,
        },
    )
    .await
    .expect("disable FIG-1071 handler replay trigger")
    .expect("FIG-1071 handler replay disable mutation");
}

async fn handler_cancels_without_rearming_or_emitting(register_and_disable: bool) {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let trigger_store = Arc::new(lash::triggers::InMemoryTriggerStore::default());
    let state = crate::tests::recoverable_chat_test_state_with_trigger_store(
        data_dir.path(),
        Arc::clone(&trigger_store) as Arc<dyn lash::triggers::TriggerStore>,
    )
    .await;
    let session_id = state.current_session_id();
    materialize_session(&state, &session_id).await;
    let source_key = "cron-source:fig1071-handler-cancel";
    if register_and_disable {
        register_then_disable(trigger_store.as_ref(), &session_id, source_key).await;
    }
    assert!(
        state.restate_cron_job_keys.lock_recover().is_empty(),
        "the test must start with empty process-local cron bookkeeping"
    );

    let cron_state = replay_cron_state(&session_id, source_key);
    let endpoint = cron_endpoint(state.clone());
    let output = invoke_cron_run_driven(
        &endpoint,
        &crate::restate::cron_job_key(&session_id, source_key),
        &cron_state,
    )
    .await
    .expect("handler-level cancel must complete");

    let types = command_frame_types(&output);
    assert!(
        !types.contains(&MSG_SLEEP) && !types.contains(&MSG_CALL) && !types.contains(&MSG_ONE_WAY),
        "a cancelled tick must not re-arm; command types were {types:?}"
    );
    assert!(
        types.contains(&MSG_CLEAR_STATE),
        "a cancelled tick must clear its cron state; command types were {types:?}"
    );

    let occurrences = lash::triggers::TriggerStore::list_occurrences(
        trigger_store.as_ref(),
        lash::triggers::TriggerOccurrenceFilter::default(),
    )
    .await
    .expect("list cancelled tick outcomes");
    let expected_reason = if register_and_disable {
        "registration_disabled"
    } else {
        "registration_absent"
    };
    assert_eq!(
        occurrences.len(),
        1,
        "a cancelled tick records exactly one outcome; records={occurrences:?}"
    );
    assert_eq!(
        occurrences[0].outcome,
        lash::triggers::TriggerOccurrenceOutcome::Dropped {
            reason: expected_reason.to_string(),
        },
        "a cancelled tick must not emit a fired occurrence"
    );
    assert!(
        lash::triggers::TriggerStore::list_deliveries(trigger_store.as_ref())
            .await
            .expect("list cancelled tick deliveries")
            .is_empty(),
        "a cancelled tick must emit no delivery"
    );
    assert!(
        state.restate_cron_job_keys.lock_recover().is_empty(),
        "the cancellation must not depend on process-local bookkeeping"
    );
}

#[tokio::test]
async fn handler_cancels_an_absent_registration_without_rearming_or_emitting() {
    handler_cancels_without_rearming_or_emitting(false).await;
}

#[tokio::test]
async fn handler_cancels_a_disabled_registration_without_rearming_or_emitting() {
    handler_cancels_without_rearming_or_emitting(true).await;
}
