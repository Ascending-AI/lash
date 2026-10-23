//! Model calls on the durable path (ADR 0132 §4, §7; spec v3 Part C). Owned
//! by L3 (FIG-5172).
//!
//! A model call is `Repeatable` generation. `model.start` pins it before the
//! first byte is sent: the digest of its request, its attempt and its
//! `model_total` deadline, clipped to what remains of the turn's deadline.
//!
//! - **Re-send.** A turn restored in its `Model` phase at `model.start`
//!   re-delivers the same unanswered call, and sends the request template
//!   its admission stored, its attachment slots filled afresh
//!   (WIRE-SLOTS), reading the response under the response context that
//!   admission recorded. It is sent again as the next attempt only when its
//!   request has the pinned reference: the checkpoint the pin committed
//!   with re-yields it, and anything else is a broken pin, never a new call. The reference is the
//!   request as the checkpoint names it (FIG-5207), so pinning and checking
//!   it reuse the digest the checkpoint computed once.
//! - **The deadline is never refreshed.** A re-send runs under the pinned
//!   deadline, and one found expired settles `TimedOut { ExecutionTotal }` at
//!   once, without sending.
//! - **The provider's outcome is durable once its receipt commits.**
//!   `model.answer` commits the call's [`ModelAnswer`] and is acknowledged
//!   before any stream hook is finished, any response hook runs, or any
//!   admission, cell or publication follows from it. A restore at
//!   `model.answer` derives from the receipt and never sends the call
//!   again; a call restored at `model.start`, whose receipt never committed,
//!   is unanswered and is re-sent. The provider may have executed and
//!   charged that unanswered attempt: a crash between the provider's
//!   execution and the receipt's commit, or a receipt write that fails, is
//!   ambiguous, and only a provider-side idempotency or resume contract
//!   could close it (FIG-1275). Streamed deltas go to the live replay store,
//!   and a re-send restarts the session's live stream first: the stream
//!   retracts what the earlier attempts streamed and the re-sent attempt
//!   streams under ids of its own, so an observer follows on without old
//!   partial text joined to new. The pin holds where the live replay stood
//!   before the first attempt streamed: the retraction is read back from
//!   there, or the stream restarts with a gap (FIG-5399). A derivation from
//!   a receipt restarts nothing: it is the attempt that answered.
//! - **Slots are held through the call.** Before a call is admitted, every
//!   attachment its template's slots name is acquired under the call's
//!   owner (`owned_call::hold_slots`), so a ref only the call names survives a
//!   takeover and is reclaimable once that owner settles (ADR 0135 §7).

use std::time::Duration;

use lash_durable::{DueSource, DurableInstant};

use super::session::{ModelPin, TurnDrive, TurnError};
use crate::ActorContext;
use crate::ExecutionBudgets;
use crate::ExecutionLimit;
use crate::FailureCode;
use crate::HostTurnProtocol;
use crate::LlmCallError;
use crate::LlmTerminalReason;
use crate::ProviderFailureKind;
use crate::TurnFailureCode;
use lash_sansio::EffectId;
use lash_sansio::TurnCheckpoint;

/// version_surface = "coexist"
/// version_guard(items(MODEL_REQUEST_PIN_DOMAIN, request_ref))
const MODEL_REQUEST_PIN_DOMAIN: &str = "lash-model-request-pin/v1";

/// A model call's receipt: the provider's normalized outcome, committed
/// beside its call's model phase under `model.answer` (one placed text the
/// turn's row names, [`ModelReceipt`]), under the owner fence, and
/// acknowledged before anything derives from it. It holds everything a
/// derivation reads: the response (parts, correlations, opaque replay
/// metadata and usage) or the call's typed unsuccessful end, the attempt
/// that answered and the call's stable identity, whether the response streamed and which reasoning it
/// published, the response callbacks the call was admitted under and the
/// stream capsule its stream hooks are finished from. A restore that finds
/// it derives from it on any worker and never sends the call again: a
/// durably recorded response is never purchased again. Only the receipt's
/// own commit can be lost after the provider executed (see
/// [`send`]); the commit that consumes the receipt (`model.done`, a cell's
/// first save, the next `model.start` or `turn.commit`) leaves the model
/// phase and drops it.
///
/// [`ModelReceipt`]: lash_core_execution::runtime::actor::model_receipt::ModelReceipt
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelAnswer {
    /// The attempt that answered.
    pub(crate) attempt: u32,
    /// The provider's outcome: the raw completion, or the call's typed
    /// unsuccessful end.
    pub(crate) result: Result<crate::llm::types::LlmResponse, LlmCallError>,
    pub(crate) text_streamed: bool,
    /// The call's stable identity. Its sealed record is retained separately;
    /// a cell accepted mid-stream resolves it after tail settlement.
    pub(crate) call_id: crate::LlmCallId,
    /// The hooks' plan, the stream capsule and the published reasoning.
    pub(crate) stream: crate::runtime::LlmStreamRecord,
}

impl ModelAnswer {
    /// A scripted, non-streaming provider outcome with no callbacks. The
    /// drive holds it until the phase runner acknowledges `model.answer`.
    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub fn for_test(
        attempt: u32,
        result: Result<crate::llm::types::LlmResponse, LlmCallError>,
    ) -> Self {
        Self {
            attempt,
            result,
            text_streamed: false,
            call_id: crate::LlmCallId(format!("test-attempt:{attempt}")),
            stream: crate::runtime::LlmStreamRecord {
                attempt: Some(1),
                reasoning_published: Vec::new(),
                capsule: crate::runtime::AssistantStreamCapsule::new(Vec::new(), Vec::new()),
                response_plan: Default::default(),
            },
        }
    }

    /// Derive a scripted receipt with no callbacks into the machine's
    /// response, after the phase runner acknowledged it.
    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub fn into_response_for_test(self, id: EffectId) -> crate::facade_support::Response {
        crate::facade_support::Response::LlmComplete {
            id,
            result: self.result,
            text_streamed: self.text_streamed,
        }
    }

    /// The callbacks the receipt owes until it is derived (S7): exactly
    /// those its derivation invokes
    /// ([`LlmStreamRecord::derivation_callbacks`]), recorded in the receipt,
    /// so a node that cannot resolve one never claims the turn to derive it.
    ///
    /// [`LlmStreamRecord::derivation_callbacks`]: crate::runtime::LlmStreamRecord::derivation_callbacks
    pub(crate) fn owed_callbacks(&self) -> Vec<crate::plugin::PluginCallbackIdentity> {
        self.stream.derivation_callbacks(self.result.is_ok())
    }
}

/// How a model call starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ModelStart {
    /// Send attempt `pin.attempt` under `limit`. `resent` when the call was
    /// already pinned: an earlier attempt may have streamed.
    Send {
        /// The pin `model.start` commits.
        pin: ModelPin,
        /// The live bound, expiring at the pinned deadline.
        limit: ExecutionLimit,
        /// Whether an earlier attempt of this call was started.
        resent: bool,
    },
    /// The pinned deadline has passed: the call settles timed out, unsent.
    Expired {
        /// The pin as it stands.
        pin: ModelPin,
    },
}

/// The pinned reference of the model request `checkpoint` waits on: the
/// digest of how the checkpoint names it, which is the window it pins, how
/// many of the request's leading messages are that window's render, and the
/// content digests of the request's frame and of the chain of its other
/// messages. The window at a pin is immutable and the checkpoint already
/// hashed the rest, so the reference costs no pass over the prompt.
///
/// # Errors
///
/// [`TurnError::Exec`] when the checkpoint waits on no model call.
pub(super) fn request_ref(
    checkpoint: &TurnCheckpoint<HostTurnProtocol>,
) -> Result<String, TurnError> {
    let (request, messages, rendered_from_window) =
        checkpoint.pending_request().ok_or_else(|| {
            TurnError::Exec("the turn's checkpoint waits on no model call".to_owned())
        })?;
    let named = serde_json::to_vec(&(
        checkpoint.window_pin(),
        rendered_from_window,
        request,
        messages,
    ))
    .map_err(|error| TurnError::Exec(format!("the model request does not encode: {error}")))?;
    Ok(format!(
        "b3:{}",
        lash_core_ids::stable_hash::blake3_hex(MODEL_REQUEST_PIN_DOMAIN, &named)
    ))
}

/// Decide how the call whose request has `reference` ([`request_ref`])
/// starts at the store's `now`.
///
/// `pinned` is the turn row's pin when it names this call, which is then
/// resent under its own identity; otherwise the call is new and its pin
/// takes `call`, the next ordinal of the turn's model calls.
/// `turn_deadline` is the turn's own deadline, which a fresh call's deadline
/// never outlives. `stream_from` is where the session's live replay stands
/// now, which a fresh call pins and a resend keeps from its first attempt.
///
/// # Errors
///
/// [`TurnError::ModelPinBroken`] when a pinned call re-delivers a request
/// with another reference.
pub(super) fn start(
    budgets: &ExecutionBudgets,
    now: DurableInstant,
    turn_deadline: Option<DurableInstant>,
    pinned: Option<ModelPin>,
    call: u32,
    reference: String,
    stream_from: String,
) -> Result<ModelStart, TurnError> {
    let now_ms = millis(now);
    match pinned {
        Some(pin) => {
            if pin.request_ref != reference {
                return Err(TurnError::ModelPinBroken {
                    pinned: pin.request_ref,
                    redelivered: reference,
                });
            }
            if now >= pin.deadline {
                return Ok(ModelStart::Expired { pin });
            }
            let limit = ExecutionLimit::starting_at(
                now_ms,
                Duration::from_millis(millis(pin.deadline).saturating_sub(now_ms)),
                budgets.provider().per_request(),
            );
            Ok(ModelStart::Send {
                pin: ModelPin {
                    attempt: pin.attempt.saturating_add(1),
                    ..pin
                },
                limit,
                resent: true,
            })
        }
        None => {
            let enclosing = turn_deadline.map(|deadline| {
                ExecutionLimit::starting_at(
                    now_ms,
                    Duration::from_millis(millis(deadline).saturating_sub(now_ms)),
                    budgets.provider().per_request(),
                )
            });
            let limit = budgets.model_call_limit(now_ms, enclosing.as_ref());
            let pin = ModelPin {
                call,
                attempt: 1,
                request_ref: reference,
                deadline: DurableInstant(i64::try_from(limit.expires_at).unwrap_or(i64::MAX)),
                stream_from,
            };
            if now >= pin.deadline {
                return Ok(ModelStart::Expired { pin });
            }
            Ok(ModelStart::Send {
                pin,
                limit,
                resent: false,
            })
        }
    }
}

/// Send one attempt of a pinned call and answer the machine, bounded live by
/// its pinned deadline: `remaining` is `deadline - now` on the store's clock,
/// counted down on the node's. A re-sent call restarts the session's live
/// stream first; an expired one settles timed out, unsent. An immediate cancel
/// cooperatively settles the live call before returning `false`; `true` means
/// the call answered or timed out.
///
/// # Errors
///
/// [`TurnError`] when the live stream cannot restart, or the call aborts
/// the turn.
pub(super) async fn send(
    cx: &ActorContext,
    drive: &mut dyn TurnDrive,
    id: EffectId,
    request: std::sync::Arc<lash_protocol_types::llm::types::RetainedLlmRequest>,
    admitted: &lash_protocol_types::llm::types::AdmittedSend,
    start: &ModelStart,
) -> Result<bool, TurnError> {
    let (pin, limit, resent) = match start {
        ModelStart::Expired { pin } => {
            drive.model_unsent(cx, id, timed_out(pin)).await?;
            return Ok(true);
        }
        ModelStart::Send { pin, limit, resent } => (pin, *limit, *resent),
    };
    if resent {
        drive.restart_live_stream(cx, id, pin).await?;
    }
    cx.note_due(DueSource::ModelDeadline, pin.deadline);
    let now = cx.durable_now().await?;
    let remaining = Duration::from_millis(millis(pin.deadline).saturating_sub(millis(now)));
    {
        let cancel = tokio_util::sync::CancellationToken::new();
        let stop_error = std::sync::Arc::new(std::sync::Mutex::new(None));
        let call = drive.model_call(
            cx,
            id,
            request,
            admitted,
            super::session::ModelCallAttempt {
                ordinal: pin.attempt,
                limit,
                cancel: cancel.clone(),
                stop_error: std::sync::Arc::clone(&stop_error),
            },
        );
        tokio::pin!(call);
        let expiry = cx.clock().sleep(remaining);
        tokio::pin!(expiry);
        loop {
            tokio::select! {
                biased;
                answered = &mut call => {
                    answered?;
                    break;
                }
                () = &mut expiry => {
                    // Stop the dispatched call with its typed deadline cause,
                    // retaining its sealed attempts before settling the turn.
                    *stop_error.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
                        Some(timed_out(pin));
                    cancel.cancel();
                    call.await?;
                    break;
                }
                () = cx.wait_for_mail() => {
                    if super::turn_cancel::immediate(cx).await? {
                        cancel.cancel();
                        // A live cancellation settles the provider call before
                        // the turn terminal; dropping it discards sealed attempts.
                        call.await?;
                        cx.clear_due(DueSource::ModelDeadline);
                        return Ok(false);
                    }
                }
            }
        }
    };
    cx.clear_due(DueSource::ModelDeadline);
    Ok(true)
}

/// The settlement of a call whose pinned deadline passed:
/// `TimedOut { ExecutionTotal }`.
#[must_use]
pub(super) fn timed_out(pin: &ModelPin) -> LlmCallError {
    LlmCallError {
        message: format!(
            "the model call reached its total limit (expired at {} ms, attempt {})",
            pin.deadline.0, pin.attempt
        ),
        retryable: false,
        kind: ProviderFailureKind::Timeout,
        raw: None,
        code: Some(FailureCode::lash(TurnFailureCode::ModelTotalExceeded)),
        terminal_reason: LlmTerminalReason::ProviderError,
        request_body: None,
        partial_response: None,
    }
}

fn millis(instant: DurableInstant) -> u64 {
    u64::try_from(instant.0).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request's reference, as [`request_ref`] names one.
    fn reference(text: &str) -> String {
        format!("b3:{text}")
    }

    fn budgets() -> ExecutionBudgets {
        ExecutionBudgets::recommended()
    }

    #[test]
    fn a_fresh_call_pins_its_digest_and_a_deadline_clipped_to_the_turn() {
        let now = DurableInstant(10_000);
        let ModelStart::Send { pin, resent, .. } = start(
            &budgets(),
            now,
            None,
            None,
            1,
            reference("hi"),
            "before".to_owned(),
        )
        .expect("starts") else {
            panic!("a fresh call is sent");
        };
        let total = i64::try_from(budgets().model_total().as_millis()).expect("millis");
        assert_eq!(pin.call, 1);
        assert_eq!(pin.attempt, 1);
        assert_eq!(pin.deadline, DurableInstant(10_000 + total));
        assert_eq!(pin.request_ref, reference("hi"));
        assert_eq!(pin.stream_from, "before");
        assert!(!resent);

        let turn_ends = DurableInstant(10_500);
        let ModelStart::Send { pin, limit, .. } = start(
            &budgets(),
            now,
            Some(turn_ends),
            None,
            1,
            reference("hi"),
            "before".to_owned(),
        )
        .expect("starts") else {
            panic!("a fresh call inside its turn is sent");
        };
        assert_eq!(pin.deadline, turn_ends, "nested in the turn, clipped to it");
        assert_eq!(limit.expires_at, 10_500);
    }

    /// L-C1: a resumed call keeps its pinned deadline and the live replay
    /// boundary its first attempt streamed after, and an expired one settles
    /// at once.
    #[test]
    fn a_resumed_call_keeps_its_deadline_and_an_expired_one_settles_unsent() {
        let pinned = ModelPin {
            call: 2,
            attempt: 1,
            request_ref: reference("hi"),
            deadline: DurableInstant(20_000),
            stream_from: "before".to_owned(),
        };
        let ModelStart::Send { pin, limit, resent } = start(
            &budgets(),
            DurableInstant(15_000),
            None,
            Some(pinned.clone()),
            3,
            reference("hi"),
            "after the first attempt".to_owned(),
        )
        .expect("starts") else {
            panic!("a pinned call inside its deadline is re-sent");
        };
        assert_eq!(pin.attempt, 2);
        assert_eq!(pin.call, 2, "a resend is the same call");
        assert_eq!(
            pin.deadline, pinned.deadline,
            "the deadline is never refreshed"
        );
        assert_eq!(
            pin.stream_from, pinned.stream_from,
            "the live replay boundary is never refreshed"
        );
        assert_eq!(limit.expires_at, 20_000);
        assert!(resent);

        let expired = start(
            &budgets(),
            DurableInstant(20_000),
            None,
            Some(pinned.clone()),
            3,
            reference("hi"),
            "after the first attempt".to_owned(),
        )
        .expect("starts");
        assert_eq!(expired, ModelStart::Expired { pin: pinned });
    }

    #[test]
    fn a_pinned_call_that_redelivers_another_request_is_a_broken_pin() {
        let pinned = ModelPin {
            call: 2,
            attempt: 1,
            request_ref: reference("hi"),
            deadline: DurableInstant(20_000),
            stream_from: "before".to_owned(),
        };
        assert!(matches!(
            start(
                &budgets(),
                DurableInstant(15_000),
                None,
                Some(pinned),
                3,
                reference("something else"),
                "after the first attempt".to_owned(),
            ),
            Err(TurnError::ModelPinBroken { .. })
        ));
    }
}
