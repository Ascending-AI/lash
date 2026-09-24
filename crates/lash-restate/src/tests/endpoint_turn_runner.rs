//! An in-process Restate invoker for the turn-driving conformance laws.
//!
//! [`EndpointTurnRunner`] runs each law's turn inside a real handler on a real
//! [`Endpoint`], through the SDK's shared-core VM, and plays the Restate
//! server's part itself: it keeps one journal per turn scope, answers the
//! handler's run proposals, durable-wait-index calls, timers and promise
//! peeks as they are issued, and on the next attempt replays everything the
//! scope's invocation journaled so far.
//!
//! An attempt that ends with the handler failing retryably leaves its
//! invocation open, exactly as the server keeps a failed attempt's journal
//! and retries it: the next run of the same scope is that retry. The probe
//! handler fails an attempt retryably when its turn crashed or aborted
//! without an outcome (a live fault, or a park on a replay divergence), and
//! completes the invocation when the turn settled. A settled scope never runs
//! again.
//!
//! What the server does between attempts (backoff, the pause after the
//! handler's last attempt) is not modelled: a law's next run of a scope is
//! the retry.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bytes::{Bytes, BytesMut};
use http_body_util::BodyExt as _;
use restate_sdk::context::WorkflowContext;
use restate_sdk::errors::{HandlerError, HandlerResult, TerminalError};
use restate_sdk::prelude::Endpoint;
use restate_sdk::serde::Json;

use super::endpoint_protocol::{
    FusedChannelBody, RESTATE_INVOCATION_CONTENT_TYPE, RestateCallFrame, decode_call_frame,
    decode_varint, durable_wait_index_call_response, encode_call_completion, encode_input_command,
    encode_invocation_id_completion, encode_peek_promise_completion, encode_restate_message,
    encode_sleep_completion, encode_start_message, protobuf_len_field, protobuf_varint_field,
    put_len_field, put_varint_field,
};
use super::live_turn_probe::CatchUnwind;

/// How long one attempt may run before the invoker gives up on it.
const ATTEMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// What a scope's next attempt runs.
enum PendingAttempt {
    Job(Option<lash_conformance::ConformanceTurnJob>),
    Attempt {
        attempt: lash_conformance::ConformanceTurnAttempt,
        crashing: bool,
    },
}

type PendingAttempts = Arc<Mutex<HashMap<String, (lash_core::AdmittedScope, PendingAttempt)>>>;

/// The workflow whose handler runs one attempt of a law's turn.
#[restate_sdk::workflow]
pub(super) trait EndpointTurnProbe {
    async fn run(key: Json<String>) -> HandlerResult<Json<bool>>;
}

struct EndpointTurnProbeImpl {
    pending: PendingAttempts,
}

impl EndpointTurnProbe for EndpointTurnProbeImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(key): Json<String>,
    ) -> HandlerResult<Json<bool>> {
        let next = {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending
                .get_mut(&key)
                .and_then(|(admitted, attempt)| match attempt {
                    PendingAttempt::Job(job) => {
                        job.take().map(|job| (admitted.clone(), job, false))
                    }
                    PendingAttempt::Attempt { attempt, crashing } => {
                        let attempt = Arc::clone(attempt);
                        let job: lash_conformance::ConformanceTurnJob =
                            Box::new(move |scoped| attempt(scoped));
                        Some((admitted.clone(), job, *crashing))
                    }
                })
        };
        let Some((admitted, job, crashing)) = next else {
            return Err(TerminalError::new(format!(
                "no attempt of conformance turn `{key}` is pending in this invoker"
            ))
            .into());
        };
        let controller = crate::RestateRuntimeEffectController::new_for_test(ctx);
        let scoped = controller
            .scoped_effect_controller(admitted)
            .map_err(TerminalError::from_error)?;
        match (CatchUnwind { inner: job(scoped) }).await {
            Ok(lash_conformance::ConformanceTurnEnd::Settled) => Ok(Json(true)),
            // The turn aborted without an outcome: the attempt fails
            // retryably, so the invocation keeps its journal for its retry.
            Ok(lash_conformance::ConformanceTurnEnd::Aborted(
                lash_core::TurnFailureCause::Parked,
            )) => Err(crate::parked_turn_failure(format!(
                "conformance turn `{key}`"
            ))),
            Ok(lash_conformance::ConformanceTurnEnd::Aborted(cause)) => Err(HandlerError::from(
                std::io::Error::other(format!("conformance turn `{key}` aborted: {cause:?}")),
            )),
            // The crashing attempt died as the law asked: the handler died
            // mid-turn, and the invocation is retried from its journal.
            Err(()) if crashing => Err(HandlerError::from(std::io::Error::other(format!(
                "conformance turn `{key}` crashed"
            )))),
            Err(()) => Err(TerminalError::new(format!(
                "conformance turn `{key}` panicked inside the probe handler"
            ))
            .into()),
        }
    }
}

/// One scope's invocation, as the server holds it between attempts.
#[derive(Default)]
struct Invocation {
    /// Every command the handler journaled, in journal order.
    commands: Vec<Bytes>,
    /// Every notification the invoker answered with, in the order sent.
    notifications: Vec<Bytes>,
    /// The invocation completed: its handler returned.
    completed: bool,
}

/// How one attempt ended.
#[derive(Debug, PartialEq, Eq)]
enum AttemptEnd {
    /// The handler returned; the invocation is complete.
    Completed,
    /// The handler failed the attempt retryably; the invocation stays open.
    Failed(String),
}

/// Runs each conformance turn as an attempt of its scope's invocation on an
/// in-process endpoint.
pub(super) struct EndpointTurnRunner {
    endpoint: Endpoint,
    pending: PendingAttempts,
    invocations: tokio::sync::Mutex<HashMap<String, Invocation>>,
    waits: DurableWaits,
}

impl EndpointTurnRunner {
    pub(super) fn shared() -> Arc<dyn lash_conformance::ConformanceTurnRunner> {
        let pending = PendingAttempts::default();
        let endpoint = Endpoint::builder()
            .bind(
                EndpointTurnProbeImpl {
                    pending: Arc::clone(&pending),
                }
                .serve(),
            )
            .build();
        Arc::new(Self {
            endpoint,
            pending,
            invocations: tokio::sync::Mutex::default(),
            waits: DurableWaits::default(),
        })
    }

    /// Runs one attempt of `admitted`'s invocation with `attempt` pending.
    async fn run_attempt(
        &self,
        admitted: lash_core::AdmittedScope,
        attempt: PendingAttempt,
    ) -> AttemptEnd {
        let key = format!("{:?}", admitted.scope());
        // Taken out for the attempt, so concurrent turns of other scopes run.
        let mut invocation = self
            .invocations
            .lock()
            .await
            .remove(&key)
            .unwrap_or_default();
        assert!(
            !invocation.completed,
            "conformance turn `{key}` already completed; a settled invocation never runs again"
        );
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key.clone(), (admitted, attempt));
        let end = tokio::time::timeout(ATTEMPT_TIMEOUT, self.attempt(&key, &mut invocation))
            .await
            .unwrap_or_else(|_| panic!("conformance turn `{key}` attempt timed out"));
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&key);
        invocation.completed = end == AttemptEnd::Completed;
        self.invocations.lock().await.insert(key, invocation);
        end
    }

    /// One attempt: replay the invocation's journal, then serve the handler
    /// live until it returns, fails or suspends.
    async fn attempt(&self, key: &str, invocation: &mut Invocation) -> AttemptEnd {
        let input = serde_json::to_vec(key).expect("encode the probe key");
        let known_entries = u32::try_from(1 + invocation.commands.len())
            .expect("conformance journal length fits u32");
        let mut replay = BytesMut::new();
        replay.extend_from_slice(&encode_start_message(key, known_entries));
        replay.extend_from_slice(&encode_input_command(&input));
        for command in &invocation.commands {
            replay.extend_from_slice(command);
        }
        for notification in &invocation.notifications {
            replay.extend_from_slice(notification);
        }

        let (input_tx, receiver) = tokio::sync::mpsc::channel(64);
        input_tx
            .send(replay.freeze())
            .await
            .expect("send the replayed journal");
        let mut input_tx = Some(input_tx);
        let response = self.endpoint.handle(
            http::Request::builder()
                .uri("/invoke/EndpointTurnProbe/run")
                .header(http::header::CONTENT_TYPE, RESTATE_INVOCATION_CONTENT_TYPE)
                .body(FusedChannelBody { receiver })
                .expect("endpoint invocation request"),
        );
        assert!(
            response.status().is_success(),
            "the probe endpoint refused the invocation: {}",
            response.status()
        );
        let mut body = response.into_body();
        let mut output = BytesMut::new();
        let mut decoded = 0;
        let mut next_invocation_id = 0_u32;
        let mut end = None;
        while let Some(frame) = body.frame().await {
            let Ok(data) = frame.expect("endpoint body frame").into_data() else {
                continue;
            };
            output.extend_from_slice(&data);
            while let Some((message_type, frame_end)) = next_frame(&output, decoded) {
                let frame = Bytes::copy_from_slice(&output[decoded..frame_end]);
                let payload = &frame[8..];
                let answer = match message_type {
                    // Suspension: the handler awaits a completion this invoker
                    // does not serve.
                    0x0001 => panic!(
                        "conformance turn `{key}` suspended on a completion the invoker does \
                         not serve; commands so far: {:?}",
                        invocation
                            .commands
                            .iter()
                            .map(|command| message_type_of(command.as_ref()))
                            .collect::<Vec<_>>()
                    ),
                    0x0002 => {
                        end = Some(AttemptEnd::Failed(error_message(payload)));
                        None
                    }
                    0x0003 => {
                        end.get_or_insert(AttemptEnd::Completed);
                        None
                    }
                    0x0005 => Some(run_completion(payload)),
                    0x0400..=0x04FF => {
                        invocation.commands.push(frame.clone());
                        match message_type {
                            // A promise peek reads the unresolved promise.
                            0x040A => Some(encode_peek_promise_completion(
                                completion_id(payload, 11),
                                None,
                            )),
                            // Timers fire at once: time is the invoker's.
                            0x040C => Some(encode_sleep_completion(completion_id(payload, 11))),
                            0x040D => {
                                let call = decode_call_frame(&frame).expect("decode call command");
                                let parameter = protobuf_len_field(payload, 3).unwrap_or_default();
                                let value =
                                    self.waits.answer(&call, parameter).unwrap_or_else(|| {
                                        panic!(
                                            "conformance turn `{key}` called {}/{}, which the \
                                             invoker does not serve",
                                            call.service, call.handler
                                        )
                                    });
                                Some(encode_call_completion(
                                    call.result_completion_id,
                                    &serde_json::to_vec(&value).expect("encode call response"),
                                ))
                            }
                            0x040E => {
                                next_invocation_id += 1;
                                Some(encode_invocation_id_completion(
                                    completion_id(payload, 10),
                                    &format!("inv-{key}-{next_invocation_id}"),
                                ))
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                };
                if let Some(answer) = answer {
                    invocation.notifications.push(answer.clone());
                    if let Some(input_tx) = input_tx.as_ref() {
                        input_tx.send(answer).await.expect("answer the handler");
                    }
                }
                if matches!(message_type, 0x0001..=0x0003) {
                    input_tx = None;
                }
                decoded = frame_end;
            }
        }
        drop(input_tx);
        end.unwrap_or_else(|| panic!("conformance turn `{key}` closed without an end message"))
    }
}

#[async_trait::async_trait]
impl lash_conformance::ConformanceTurnRunner for EndpointTurnRunner {
    async fn run_turn(
        &self,
        admitted: lash_core::AdmittedScope,
        job: lash_conformance::ConformanceTurnJob,
    ) {
        self.run_attempt(admitted, PendingAttempt::Job(Some(job)))
            .await;
    }

    async fn run_crashed_then_redriven_turn(
        &self,
        admitted: lash_core::AdmittedScope,
        crashing: lash_conformance::ConformanceTurnAttempt,
        redrive: lash_conformance::ConformanceTurnAttempt,
    ) {
        let crashed = self
            .run_attempt(
                admitted.clone(),
                PendingAttempt::Attempt {
                    attempt: crashing,
                    crashing: true,
                },
            )
            .await;
        assert!(
            matches!(crashed, AttemptEnd::Failed(_)),
            "the crashing attempt must fail its invocation's attempt: {crashed:?}"
        );
        self.run_attempt(
            admitted,
            PendingAttempt::Attempt {
                attempt: redrive,
                crashing: false,
            },
        )
        .await;
    }
}

/// The durable waits the invoker's server side holds: each resolved wait's
/// terminal, by its `LashDurableWaitWorkflow` key. A wait resolves once; its
/// first terminal wins.
#[derive(Default)]
struct DurableWaits {
    terminals: Mutex<HashMap<String, lash_core::Resolution>>,
}

impl DurableWaits {
    /// The answer to a call the turn's controller makes: the durable wait
    /// index admits every effect and reports the session live, a resolve
    /// settles its wait first-writer-wins, and a peek reads it.
    fn answer(&self, call: &RestateCallFrame, parameter: &[u8]) -> Option<serde_json::Value> {
        let mut terminals = self
            .terminals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match (call.service.as_str(), call.handler.as_str()) {
            ("LashDurableWaitIndex", "is_revoked") => Some(serde_json::Value::Bool(false)),
            ("LashDurableWaitIndex", "resolve") => {
                let request: crate::durable_wait::RestateDurableWaitResolveRequest =
                    serde_json::from_slice(parameter).expect("decode the resolve request");
                let workflow_key =
                    crate::durable_wait::RestateDurableWaitAddress::for_key(&request.key)
                        .workflow_key;
                let outcome = match terminals.get(&workflow_key) {
                    Some(terminal) => lash_core::ResolveOutcome::AlreadyResolved {
                        terminal: terminal.clone(),
                    },
                    None => {
                        terminals.insert(workflow_key, request.resolution);
                        lash_core::ResolveOutcome::Accepted
                    }
                };
                Some(
                    serde_json::to_value(
                        crate::durable_wait::RestateDurableWaitResolveResponse::Outcome(outcome),
                    )
                    .expect("encode the resolve response"),
                )
            }
            ("LashDurableWaitWorkflow", "peek") => Some(
                serde_json::to_value(terminals.get(&call.key)).expect("encode the peek response"),
            ),
            (service, handler) => durable_wait_index_call_response(service, handler),
        }
    }
}

/// The type and end offset of the whole frame starting at `cursor`, once it
/// has fully arrived.
fn next_frame(output: &[u8], cursor: usize) -> Option<(u16, usize)> {
    let header = u64::from_be_bytes(output.get(cursor..cursor + 8)?.try_into().ok()?);
    let payload_len = usize::try_from(header & 0x0000_FFFF_FFFF_FFFF).ok()?;
    let frame_end = cursor.checked_add(8 + payload_len)?;
    (output.len() >= frame_end).then_some(((header >> 48) as u16, frame_end))
}

fn message_type_of(frame: &[u8]) -> u16 {
    frame
        .get(..8)
        .and_then(|header| header.try_into().ok())
        .map(|header: [u8; 8]| (u64::from_be_bytes(header) >> 48) as u16)
        .unwrap_or_default()
}

fn completion_id(payload: &[u8], field: u64) -> u32 {
    protobuf_varint_field(payload, field)
        .and_then(|id| u32::try_from(id).ok())
        .expect("command carries its completion id")
}

fn error_message(payload: &[u8]) -> String {
    protobuf_len_field(payload, 2)
        .map(|message| String::from_utf8_lossy(message).into_owned())
        .unwrap_or_default()
}

/// The `RunCompletionNotification` that acknowledges a proposed run result:
/// the runtime stores what the handler proposed, a value or a failure.
fn run_completion(proposal: &[u8]) -> Bytes {
    let mut cursor = 0;
    let mut completion_id = None;
    let mut value = None;
    let mut failure = None;
    while cursor < proposal.len() {
        let key = decode_varint(proposal, &mut cursor).expect("proposal field key");
        let field = key >> 3;
        match key & 7 {
            0 => {
                let parsed = decode_varint(proposal, &mut cursor).expect("proposal varint");
                if field == 1 {
                    completion_id = u32::try_from(parsed).ok();
                }
            }
            2 => {
                let len = usize::try_from(decode_varint(proposal, &mut cursor).expect("length"))
                    .expect("proposal length");
                let bytes = &proposal[cursor..cursor + len];
                match field {
                    14 => value = Some(bytes),
                    15 => failure = Some(bytes),
                    _ => {}
                }
                cursor += len;
            }
            wire => panic!("unexpected wire type {wire} in a run proposal"),
        }
    }
    let mut notification = BytesMut::new();
    put_varint_field(
        &mut notification,
        1,
        u64::from(completion_id.expect("proposal names its completion")),
    );
    match (value, failure) {
        (Some(value), _) => {
            let mut nested = BytesMut::new();
            put_len_field(&mut nested, 1, value);
            put_len_field(&mut notification, 5, &nested);
        }
        (None, Some(failure)) => put_len_field(&mut notification, 6, failure),
        // An empty value is the protobuf default, so it arrives as no field.
        (None, None) => put_len_field(&mut notification, 5, &[]),
    }
    encode_restate_message(0x8011, notification.to_vec())
}
