use bytes::{BufMut, Bytes, BytesMut};
use http_body_util::{BodyExt, Full, channel::Channel};
use restate_sdk::errors::TerminalError;
use restate_sdk::prelude::Endpoint;
use std::convert::Infallible;

const RESTATE_INVOCATION_CONTENT_TYPE: &str = "application/vnd.restate.invocation.v6";

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

fn encode_start_message(workflow_key: &str, known_entries: u32) -> Bytes {
    let mut payload = BytesMut::new();
    put_len_field(&mut payload, 1, workflow_key.as_bytes());
    put_len_field(&mut payload, 2, workflow_key.as_bytes());
    put_varint_field(&mut payload, 3, u64::from(known_entries));
    put_len_field(&mut payload, 6, workflow_key.as_bytes());
    encode_restate_message(0x0000, payload.to_vec())
}

fn encode_input_command(payload: &[u8]) -> Bytes {
    let mut value = BytesMut::new();
    put_len_field(&mut value, 1, payload);

    let mut command = BytesMut::new();
    put_len_field(&mut command, 14, &value);
    encode_restate_message(0x0400, command.to_vec())
}

fn encode_invocation_body<T: serde::Serialize>(
    workflow_key: &str,
    input: &T,
) -> Result<Bytes, TerminalError> {
    let input = serde_json::to_vec(input).map_err(TerminalError::from_error)?;
    let start = encode_start_message(workflow_key, 1);
    let input = encode_input_command(&input);
    let mut body = BytesMut::with_capacity(start.len() + input.len());
    body.extend_from_slice(&start);
    body.extend_from_slice(&input);
    Ok(body.freeze())
}

fn restate_message_frame(input: &[u8], expected_type: u16) -> Option<&[u8]> {
    let mut cursor = 0;
    while cursor < input.len() {
        let header = u64::from_be_bytes(input.get(cursor..cursor + 8)?.try_into().ok()?);
        let message_type = (header >> 48) as u16;
        let payload_len = usize::try_from(header & 0x0000_FFFF_FFFF_FFFF).ok()?;
        let frame_end = cursor.checked_add(8 + payload_len)?;
        let frame = input.get(cursor..frame_end)?;
        if message_type == expected_type {
            return Some(frame);
        }
        cursor = frame_end;
    }
    None
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RestateCallFrame {
    pub frame: Bytes,
    pub service: String,
    pub handler: String,
    pub key: String,
    pub result_completion_id: u32,
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

fn decode_call_frame(frame: &[u8]) -> Option<RestateCallFrame> {
    let payload = frame.get(8..)?;
    Some(RestateCallFrame {
        frame: Bytes::copy_from_slice(frame),
        service: String::from_utf8(protobuf_len_field(payload, 1)?.to_vec()).ok()?,
        handler: String::from_utf8(protobuf_len_field(payload, 2)?.to_vec()).ok()?,
        key: String::from_utf8(protobuf_len_field(payload, 5).unwrap_or_default().to_vec()).ok()?,
        result_completion_id: u32::try_from(protobuf_varint_field(payload, 11)?).ok()?,
    })
}

pub(super) fn restate_call_frames(input: &[u8]) -> Option<Vec<RestateCallFrame>> {
    let mut cursor = 0;
    let mut calls = Vec::new();
    while cursor < input.len() {
        let header = u64::from_be_bytes(input.get(cursor..cursor + 8)?.try_into().ok()?);
        let message_type = (header >> 48) as u16;
        let payload_len = usize::try_from(header & 0x0000_FFFF_FFFF_FFFF).ok()?;
        let frame_end = cursor.checked_add(8 + payload_len)?;
        let frame = input.get(cursor..frame_end)?;
        if message_type == 0x040D {
            calls.push(decode_call_frame(frame)?);
        }
        cursor = frame_end;
    }
    Some(calls)
}

fn encode_call_completion(completion_id: u32, value: &[u8]) -> Bytes {
    let mut nested_value = BytesMut::new();
    put_len_field(&mut nested_value, 1, value);
    let mut notification = BytesMut::new();
    put_varint_field(&mut notification, 1, u64::from(completion_id));
    put_len_field(&mut notification, 5, &nested_value);
    encode_restate_message(0x800D, notification.to_vec())
}

fn encode_signal_value(signal_id: u32, value: &[u8]) -> Bytes {
    let mut nested_value = BytesMut::new();
    put_len_field(&mut nested_value, 1, value);
    let mut notification = BytesMut::new();
    put_varint_field(&mut notification, 2, u64::from(signal_id));
    put_len_field(&mut notification, 5, &nested_value);
    encode_restate_message(0xFBFF, notification.to_vec())
}

/// FIG-790: replay exact call-command frames captured from a prior attempt,
/// optionally completing each call and resolving the handler's first
/// awakeable signal.
pub(super) fn encode_call_replay<T: serde::Serialize>(
    workflow_key: &str,
    input: &T,
    calls: &[(RestateCallFrame, Option<serde_json::Value>)],
    signal: Option<(u32, serde_json::Value)>,
) -> Result<Bytes, TerminalError> {
    let input = serde_json::to_vec(input).map_err(TerminalError::from_error)?;
    let known_entries = u32::try_from(1 + calls.len())
        .map_err(|_| TerminalError::new("too many call commands in replay fixture"))?;
    let mut body = BytesMut::new();
    body.extend_from_slice(&encode_start_message(workflow_key, known_entries));
    body.extend_from_slice(&encode_input_command(&input));
    for (call, _) in calls {
        body.extend_from_slice(&call.frame);
    }
    for (call, completion) in calls {
        if let Some(completion) = completion {
            let completion = serde_json::to_vec(completion).map_err(TerminalError::from_error)?;
            body.extend_from_slice(&encode_call_completion(
                call.result_completion_id,
                &completion,
            ));
        }
    }
    if let Some((signal_id, resolution)) = signal {
        let resolution = serde_json::to_vec(&resolution).map_err(TerminalError::from_error)?;
        body.extend_from_slice(&encode_signal_value(signal_id, &resolution));
    }
    Ok(body.freeze())
}

pub(super) fn restate_output_json<T: serde::de::DeserializeOwned>(input: &[u8]) -> Option<T> {
    let frame = restate_message_frame(input, 0x0401)?;
    let value = protobuf_len_field(frame.get(8..)?, 14)?;
    let json = protobuf_len_field(value, 1)?;
    serde_json::from_slice(json).ok()
}

pub(super) fn restate_output_failure_message(input: &[u8]) -> Option<String> {
    let frame = restate_message_frame(input, 0x0401)?;
    let failure = protobuf_len_field(frame.get(8..)?, 15)?;
    String::from_utf8(protobuf_len_field(failure, 2)?.to_vec()).ok()
}

/// FIG-779: redrive an invocation whose journal already contains the exact
/// `SleepCommand` emitted by its suspended attempt, but no timer completion.
pub(super) fn encode_pending_sleep_replay<T: serde::Serialize>(
    workflow_key: &str,
    input: &T,
    suspended_output: &[u8],
) -> Result<Bytes, TerminalError> {
    let input = serde_json::to_vec(input).map_err(TerminalError::from_error)?;
    let sleep_command = restate_message_frame(suspended_output, 0x040C)
        .ok_or_else(|| TerminalError::new("suspended attempt omitted its SleepCommand"))?;
    let mut body = BytesMut::new();
    body.extend_from_slice(&encode_start_message(workflow_key, 2));
    body.extend_from_slice(&encode_input_command(&input));
    body.extend_from_slice(sleep_command);
    Ok(body.freeze())
}

/// FIG-779: `SleepCommand` (0x040C) carrying only `wake_up_time` and its
/// completion id, as the SDK writes it for `ctx.sleep()`.
fn encode_sleep_command(completion_id: u32) -> Bytes {
    let mut payload = BytesMut::new();
    put_varint_field(&mut payload, 1, 1);
    put_varint_field(&mut payload, 11, u64::from(completion_id));
    encode_restate_message(0x040C, payload.to_vec())
}

/// FIG-779: `SleepCompletionNotification` (0x800C) with a void result, i.e. the
/// timer already fired and its completion is in the replayed journal.
fn encode_sleep_completion(completion_id: u32) -> Bytes {
    let mut payload = BytesMut::new();
    put_varint_field(&mut payload, 1, u64::from(completion_id));
    put_len_field(&mut payload, 4, &[]);
    encode_restate_message(0x800C, payload.to_vec())
}

/// FIG-779: an invocation body whose journal already contains a completed
/// durable timer, so the handler replays the sleep straight to `Ready`.
pub(super) fn encode_completed_sleep_replay<T: serde::Serialize>(
    workflow_key: &str,
    input: &T,
) -> Result<Bytes, TerminalError> {
    let input = serde_json::to_vec(input).map_err(TerminalError::from_error)?;
    let mut body = BytesMut::new();
    body.extend_from_slice(&encode_start_message(workflow_key, 3));
    body.extend_from_slice(&encode_input_command(&input));
    body.extend_from_slice(&encode_sleep_command(1));
    body.extend_from_slice(&encode_sleep_completion(1));
    Ok(body.freeze())
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

pub(super) fn restate_message_types(input: &[u8]) -> Option<Vec<u16>> {
    let mut cursor = 0;
    let mut message_types = Vec::new();
    while cursor < input.len() {
        let header = u64::from_be_bytes(input.get(cursor..cursor + 8)?.try_into().ok()?);
        let message_type = (header >> 48) as u16;
        let payload_len = usize::try_from(header & 0x0000_FFFF_FFFF_FFFF).ok()?;
        cursor = cursor.checked_add(8 + payload_len)?;
        if cursor > input.len() {
            return None;
        }
        message_types.push(message_type);
    }
    Some(message_types)
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

fn encode_run_completion(completion_id: u32, value: &[u8]) -> Bytes {
    let mut nested_value = BytesMut::new();
    put_len_field(&mut nested_value, 1, value);
    let mut notification = BytesMut::new();
    put_varint_field(&mut notification, 1, u64::from(completion_id));
    put_len_field(&mut notification, 5, &nested_value);
    encode_restate_message(0x8011, notification.to_vec())
}

const ENDPOINT_TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

pub(super) async fn invoke_process_workflow_endpoint<T: serde::Serialize>(
    endpoint: &Endpoint,
    handler: &str,
    workflow_key: &str,
    input: &T,
    complete_runs: bool,
) -> Result<Bytes, TerminalError> {
    tokio::time::timeout(
        ENDPOINT_TEST_TIMEOUT,
        invoke_process_workflow_endpoint_unbounded(
            endpoint,
            handler,
            workflow_key,
            input,
            complete_runs,
        ),
    )
    .await
    .map_err(|_| TerminalError::new("workflow endpoint test timed out"))?
}

async fn invoke_process_workflow_endpoint_unbounded<T: serde::Serialize>(
    endpoint: &Endpoint,
    handler: &str,
    workflow_key: &str,
    input: &T,
    complete_runs: bool,
) -> Result<Bytes, TerminalError> {
    if !complete_runs {
        let response = endpoint.handle(
            http::Request::builder()
                .uri(format!("/invoke/LashProcessWorkflow/{handler}"))
                .header(http::header::CONTENT_TYPE, RESTATE_INVOCATION_CONTENT_TYPE)
                .body(Full::new(encode_invocation_body(workflow_key, input)?))
                .expect("workflow invocation request"),
        );
        let status = response.status();
        if !status.is_success() {
            return Err(TerminalError::new_with_code(
                status.as_u16(),
                format!("workflow endpoint invocation returned status {status}"),
            ));
        }
        return response
            .into_body()
            .collect()
            .await
            .map(|body| body.to_bytes())
            .map_err(|err| TerminalError::new(format!("workflow endpoint body failed: {err}")));
    }

    let (mut input_sender, body) = Channel::<Bytes, Infallible>::new(4);
    input_sender
        .send_data(encode_invocation_body(workflow_key, input)?)
        .await
        .map_err(|err| TerminalError::new(format!("workflow endpoint input failed: {err}")))?;
    let mut input_sender = Some(input_sender);
    let response = endpoint.handle(
        http::Request::builder()
            .uri(format!("/invoke/LashProcessWorkflow/{handler}"))
            .header(http::header::CONTENT_TYPE, RESTATE_INVOCATION_CONTENT_TYPE)
            .body(body)
            .expect("workflow invocation request"),
    );
    let status = response.status();
    if !status.is_success() {
        return Err(TerminalError::new_with_code(
            status.as_u16(),
            format!("workflow endpoint invocation returned status {status}"),
        ));
    }
    let mut response = response.into_body();
    let mut output = BytesMut::new();
    let mut decoded = 0;
    while let Some(frame) = response.frame().await {
        let frame = frame
            .map_err(|err| TerminalError::new(format!("workflow endpoint body failed: {err}")))?;
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
            if message_type == 0x0005 {
                let payload = &output[decoded + 8..frame_end];
                let (completion_id, value) = proposed_run_completion(payload).ok_or_else(|| {
                    TerminalError::new("workflow endpoint returned an invalid run completion")
                })?;
                input_sender
                    .as_mut()
                    .expect("workflow input remains open until the end message")
                    .send_data(encode_run_completion(completion_id, value))
                    .await
                    .map_err(|err| {
                        TerminalError::new(format!("workflow run completion failed: {err}"))
                    })?;
            }
            if message_type == 0x0003 {
                drop(input_sender.take());
            }
            decoded = frame_end;
        }
    }
    drop(input_sender);
    Ok(output.freeze())
}

/// Invoke a bound handler with a *complete* (already-closed) request body, the
/// shape Restate uses when it has no further frames to send and expects the SDK
/// to either finish or suspend.
pub(super) async fn invoke_endpoint<T: serde::Serialize>(
    endpoint: &Endpoint,
    service: &str,
    handler: &str,
    key: &str,
    input: &T,
) -> Result<Bytes, TerminalError> {
    invoke_endpoint_body(
        endpoint,
        service,
        handler,
        encode_invocation_body(key, input)?,
    )
    .await
}

pub(super) async fn invoke_endpoint_body(
    endpoint: &Endpoint,
    service: &str,
    handler: &str,
    body: Bytes,
) -> Result<Bytes, TerminalError> {
    tokio::time::timeout(
        ENDPOINT_TEST_TIMEOUT,
        invoke_endpoint_body_unbounded(endpoint, service, handler, body),
    )
    .await
    .map_err(|_| TerminalError::new("endpoint test timed out"))?
}

pub(super) async fn invoke_endpoint_open<T: serde::Serialize>(
    endpoint: &Endpoint,
    service: &str,
    handler: &str,
    key: &str,
    input: &T,
) -> Result<Bytes, TerminalError> {
    invoke_endpoint_body_open(
        endpoint,
        service,
        handler,
        encode_invocation_body(key, input)?,
    )
    .await
}

pub(super) async fn invoke_endpoint_body_open(
    endpoint: &Endpoint,
    service: &str,
    handler: &str,
    body: Bytes,
) -> Result<Bytes, TerminalError> {
    tokio::time::timeout(
        ENDPOINT_TEST_TIMEOUT,
        invoke_endpoint_body_open_unbounded(endpoint, service, handler, body),
    )
    .await
    .map_err(|_| TerminalError::new("open-input endpoint test timed out"))?
}

pub(super) async fn invoke_endpoint_body_with_json_call_responses(
    endpoint: &Endpoint,
    service: &str,
    handler: &str,
    body: Bytes,
    responses: Vec<serde_json::Value>,
) -> Result<Bytes, TerminalError> {
    tokio::time::timeout(
        ENDPOINT_TEST_TIMEOUT,
        invoke_endpoint_body_with_json_call_responses_unbounded(
            endpoint, service, handler, body, responses,
        ),
    )
    .await
    .map_err(|_| TerminalError::new("scripted-call endpoint test timed out"))?
}

async fn invoke_endpoint_body_with_json_call_responses_unbounded(
    endpoint: &Endpoint,
    service: &str,
    handler: &str,
    invocation_body: Bytes,
    responses: Vec<serde_json::Value>,
) -> Result<Bytes, TerminalError> {
    let (mut input_sender, body) = Channel::<Bytes, Infallible>::new(8);
    input_sender
        .send_data(invocation_body)
        .await
        .map_err(|err| TerminalError::new(format!("endpoint input failed: {err}")))?;
    let mut input_sender = Some(input_sender);
    let mut responses = responses.into_iter();
    let response = endpoint.handle(
        http::Request::builder()
            .uri(format!("/invoke/{service}/{handler}"))
            .header(http::header::CONTENT_TYPE, RESTATE_INVOCATION_CONTENT_TYPE)
            .body(body)
            .expect("endpoint invocation request"),
    );
    let status = response.status();
    if !status.is_success() {
        return Err(TerminalError::new_with_code(
            status.as_u16(),
            format!("endpoint invocation returned status {status}"),
        ));
    }
    let mut response = response.into_body();
    let mut output = BytesMut::new();
    let mut decoded = 0;
    while let Some(frame) = response.frame().await {
        let frame =
            frame.map_err(|err| TerminalError::new(format!("endpoint body failed: {err}")))?;
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
            if message_type == 0x040D {
                let call = decode_call_frame(&output[decoded..frame_end])
                    .ok_or_else(|| TerminalError::new("invalid call command frame"))?;
                if let Some(response) = responses.next() {
                    let response =
                        serde_json::to_vec(&response).map_err(TerminalError::from_error)?;
                    input_sender
                        .as_mut()
                        .expect("endpoint input remains open for scripted calls")
                        .send_data(encode_call_completion(call.result_completion_id, &response))
                        .await
                        .map_err(|err| {
                            TerminalError::new(format!("call completion input failed: {err}"))
                        })?;
                } else {
                    drop(input_sender.take());
                }
            }
            if matches!(message_type, 0x0001..=0x0003) {
                drop(input_sender.take());
            }
            decoded = frame_end;
        }
    }
    drop(input_sender);
    Ok(output.freeze())
}

async fn invoke_endpoint_body_open_unbounded(
    endpoint: &Endpoint,
    service: &str,
    handler: &str,
    invocation_body: Bytes,
) -> Result<Bytes, TerminalError> {
    let (mut input_sender, body) = Channel::<Bytes, Infallible>::new(4);
    input_sender
        .send_data(invocation_body)
        .await
        .map_err(|err| TerminalError::new(format!("endpoint input failed: {err}")))?;
    let mut input_sender = Some(input_sender);
    let response = endpoint.handle(
        http::Request::builder()
            .uri(format!("/invoke/{service}/{handler}"))
            .header(http::header::CONTENT_TYPE, RESTATE_INVOCATION_CONTENT_TYPE)
            .body(body)
            .expect("endpoint invocation request"),
    );
    let status = response.status();
    if !status.is_success() {
        return Err(TerminalError::new_with_code(
            status.as_u16(),
            format!("endpoint invocation returned status {status}"),
        ));
    }
    let mut response = response.into_body();
    let mut output = BytesMut::new();
    let mut decoded = 0;
    while let Some(frame) = response.frame().await {
        let frame =
            frame.map_err(|err| TerminalError::new(format!("endpoint body failed: {err}")))?;
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
            if matches!(message_type, 0x0002 | 0x0003) {
                drop(input_sender.take());
            }
            decoded = frame_end;
        }
    }
    drop(input_sender);
    Ok(output.freeze())
}

async fn invoke_endpoint_body_unbounded(
    endpoint: &Endpoint,
    service: &str,
    handler: &str,
    body: Bytes,
) -> Result<Bytes, TerminalError> {
    let response = endpoint.handle(
        http::Request::builder()
            .uri(format!("/invoke/{service}/{handler}"))
            .header(http::header::CONTENT_TYPE, RESTATE_INVOCATION_CONTENT_TYPE)
            .body(Full::new(body))
            .expect("endpoint invocation request"),
    );
    let status = response.status();
    if !status.is_success() {
        return Err(TerminalError::new_with_code(
            status.as_u16(),
            format!("endpoint invocation returned status {status}"),
        ));
    }
    response
        .into_body()
        .collect()
        .await
        .map(|body| body.to_bytes())
        .map_err(|err| TerminalError::new(format!("endpoint body failed: {err}")))
}
