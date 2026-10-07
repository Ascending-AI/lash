//! Model calls on the durable path (ADR 0132 §4, §7; spec v3 Part C). Owned
//! by L3 (FIG-5172).
//!
//! A model call is `Repeatable` generation. `model.start` pins it before the
//! first byte is sent: the digest of its request, its attempt and its
//! `model_total` deadline, clipped to what remains of the turn's deadline.
//!
//! - **Re-send.** A turn restored in its `Model` phase re-delivers the same
//!   call. It is sent again as the next attempt only when its request has the
//!   pinned digest: the checkpoint the pin committed with re-yields it byte
//!   for byte, and anything else is a broken pin, never a new call.
//! - **The deadline is never refreshed.** A re-send runs under the pinned
//!   deadline, and one found expired settles `TimedOut { ExecutionTotal }` at
//!   once, without sending.
//! - **Only the completed response is durable.** It commits with the next
//!   phase; streamed deltas go to the live replay store, and a re-send
//!   restarts the session's live stream first, so an observer sees a gap and
//!   reloads rather than old partial text joined to new.

use std::time::Duration;

use lash_durable::{DueSource, DurableInstant};

use super::session::{ModelPin, TurnDrive, TurnError};
use crate::{
    ActorContext, EffectId, ExecutionBudgets, ExecutionLimit, FailureCode, LlmCallError,
    LlmRequest, LlmTerminalReason, ProviderFailureKind, TurnFailureCode,
};

/// version_surface = "coexist"
/// version_guard(items(MODEL_REQUEST_PIN_DOMAIN, request_ref))
const MODEL_REQUEST_PIN_DOMAIN: &str = "lash-model-request-pin/v1";

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

/// The pinned reference of `request`: the digest of its encoding.
///
/// # Errors
///
/// [`TurnError::Exec`] when the request does not encode.
pub(super) fn request_ref(request: &LlmRequest) -> Result<String, TurnError> {
    let bytes = serde_json::to_vec(request)
        .map_err(|error| TurnError::Exec(format!("the model request does not encode: {error}")))?;
    Ok(format!(
        "b3:{}",
        lash_core_ids::stable_hash::blake3_hex(MODEL_REQUEST_PIN_DOMAIN, &bytes)
    ))
}

/// Decide how the call for `request` starts at the store's `now`.
///
/// `pinned` is the turn row's pin when it names this call; `turn_deadline`
/// is the turn's own deadline, which a fresh call's deadline never outlives.
///
/// # Errors
///
/// [`TurnError::ModelPinBroken`] when a pinned call re-delivers a request
/// with another digest.
pub(super) fn start(
    budgets: &ExecutionBudgets,
    now: DurableInstant,
    turn_deadline: Option<DurableInstant>,
    pinned: Option<ModelPin>,
    request: &LlmRequest,
) -> Result<ModelStart, TurnError> {
    let reference = request_ref(request)?;
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
                attempt: 1,
                request_ref: reference,
                deadline: DurableInstant(i64::try_from(limit.expires_at).unwrap_or(i64::MAX)),
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
/// stream first; an expired one settles timed out, unsent.
///
/// # Errors
///
/// [`TurnError`] when the live stream cannot restart, or the call aborts
/// the turn.
pub(super) async fn send(
    cx: &ActorContext,
    drive: &mut dyn TurnDrive,
    id: EffectId,
    request: std::sync::Arc<LlmRequest>,
    start: &ModelStart,
) -> Result<(), TurnError> {
    let (pin, limit, resent) = match start {
        ModelStart::Expired { pin } => {
            super::phases::settle_unsent(drive, id, timed_out(pin));
            return Ok(());
        }
        ModelStart::Send { pin, limit, resent } => (pin, *limit, *resent),
    };
    if resent {
        drive.restart_live_stream(cx).await?;
    }
    cx.note_due(DueSource::ModelDeadline, pin.deadline);
    let now = cx.durable_now().await?;
    let remaining = Duration::from_millis(millis(pin.deadline).saturating_sub(millis(now)));
    let expired = tokio::select! {
        answered = drive.model_call(cx, id, request, pin.attempt, limit) => {
            answered?;
            false
        }
        () = cx.clock().sleep(remaining) => true,
    };
    cx.clear_due(DueSource::ModelDeadline);
    if expired {
        super::phases::settle_unsent(drive, id, timed_out(pin));
    }
    Ok(())
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

    fn request(text: &str) -> LlmRequest {
        use lash_sansio::llm::types::{LlmMessage, LlmRequestScope, LlmRole, LlmToolChoice};
        use lash_sansio::llm_profile::{
            LlmProfileConfig, LlmProfileKey, LlmProfileMetadata, RecordedLlmProfile,
        };
        LlmRequest {
            instructions: None,
            model: LlmProfileConfig::new(RecordedLlmProfile::mint(
                LlmProfileKey::new("model-call-fixture"),
                LlmProfileMetadata::builder("model-a".to_string())
                    .context_window_tokens(128_000)
                    .capability(Default::default())
                    .extra_body(Default::default())
                    .request_defaults(Default::default())
                    .build()
                    .expect("valid profile"),
            )),
            messages: vec![LlmMessage::text(LlmRole::User, text)],
            resolved_stored: Default::default(),
            tools: std::sync::Arc::new(Vec::new()),
            tool_choice: LlmToolChoice::Auto,
            attachment_acceptance: Default::default(),
            generation: Default::default(),
            scope: LlmRequestScope::new("session", "frame", "request"),
            output_spec: None,
            stream_events: None,
            provider_trace: None,
        }
    }

    fn budgets() -> ExecutionBudgets {
        ExecutionBudgets::default()
    }

    #[test]
    fn a_fresh_call_pins_its_digest_and_a_deadline_clipped_to_the_turn() {
        let now = DurableInstant(10_000);
        let ModelStart::Send { pin, resent, .. } =
            start(&budgets(), now, None, None, &request("hi")).expect("starts")
        else {
            panic!("a fresh call is sent");
        };
        let total = i64::try_from(budgets().model_total().as_millis()).expect("millis");
        assert_eq!(pin.attempt, 1);
        assert_eq!(pin.deadline, DurableInstant(10_000 + total));
        assert_eq!(pin.request_ref, request_ref(&request("hi")).expect("ref"));
        assert!(!resent);

        let turn_ends = DurableInstant(10_500);
        let ModelStart::Send { pin, limit, .. } =
            start(&budgets(), now, Some(turn_ends), None, &request("hi")).expect("starts")
        else {
            panic!("a fresh call inside its turn is sent");
        };
        assert_eq!(pin.deadline, turn_ends, "nested in the turn, clipped to it");
        assert_eq!(limit.expires_at, 10_500);
    }

    /// L-C1: a resumed call keeps its pinned deadline, and an expired one
    /// settles at once.
    #[test]
    fn a_resumed_call_keeps_its_deadline_and_an_expired_one_settles_unsent() {
        let pinned = ModelPin {
            attempt: 1,
            request_ref: request_ref(&request("hi")).expect("ref"),
            deadline: DurableInstant(20_000),
        };
        let ModelStart::Send { pin, limit, resent } = start(
            &budgets(),
            DurableInstant(15_000),
            None,
            Some(pinned.clone()),
            &request("hi"),
        )
        .expect("starts") else {
            panic!("a pinned call inside its deadline is re-sent");
        };
        assert_eq!(pin.attempt, 2);
        assert_eq!(
            pin.deadline, pinned.deadline,
            "the deadline is never refreshed"
        );
        assert_eq!(limit.expires_at, 20_000);
        assert!(resent);

        let expired = start(
            &budgets(),
            DurableInstant(20_000),
            None,
            Some(pinned.clone()),
            &request("hi"),
        )
        .expect("starts");
        assert_eq!(expired, ModelStart::Expired { pin: pinned });
    }

    #[test]
    fn a_pinned_call_that_redelivers_another_request_is_a_broken_pin() {
        let pinned = ModelPin {
            attempt: 1,
            request_ref: request_ref(&request("hi")).expect("ref"),
            deadline: DurableInstant(20_000),
        };
        assert!(matches!(
            start(
                &budgets(),
                DurableInstant(15_000),
                None,
                Some(pinned),
                &request("something else"),
            ),
            Err(TurnError::ModelPinBroken { .. })
        ));
    }
}
