use lash_core::ToolCallOutcome;
use lashlang::{ExecutionHostError, Value as LashlangValue};
use tokio_util::sync::CancellationToken;

use crate::LashlangHostError;

/// One execution's own cancellation scope.
///
/// `ExecutionHost::is_cancelled` is documented as a host terminal that bypasses
/// guest exception handlers, and `LashlangProcessHost` wires it precisely so a
/// cancelled process "terminates as an uncatchable host terminal instead of
/// running to completion inside a guest handler". A cancelled *tool call* is the
/// same kind of fact — not a value, and not a failure the guest may retry — so
/// the tool bridges trip this scope instead of handing the guest something to
/// catch, and the host reports it through `is_cancelled` from then on.
///
/// A scope built with [`ExecutionCancellation::child_of`] is also cancelled when
/// its parent is, so one field carries both the engine's cancellation and the
/// run's own.
#[derive(Clone, Debug, Default)]
pub struct ExecutionCancellation(CancellationToken);

impl ExecutionCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    /// A scope of this run's own, cancelled when `parent` is cancelled.
    pub fn child_of(parent: &CancellationToken) -> Self {
        Self(parent.child_token())
    }

    pub fn cancel(&self) {
        self.0.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }

    /// Resolves when this scope is cancelled, for a caller racing it against
    /// execution.
    pub fn cancelled(&self) -> tokio_util::sync::WaitForCancellationFuture<'_> {
        self.0.cancelled()
    }
}

pub fn lashlang_value_to_json(
    value: &LashlangValue,
) -> Result<serde_json::Value, ExecutionHostError> {
    serde_json::to_value(value)
        .map_err(|source| LashlangHostError::SerializeValue { source }.into())
}

/// Projects a completed tool call for the guest, owning the reply.
///
/// The outcome has three arms and each means something different to a guest, so
/// all three are named here: a success is a value, a failure is a catchable
/// effect error carrying the tool's own classification, and a cancellation is a
/// host terminal (see [`ExecutionCancellation`]). Writing it as
/// "failure, else success, else whatever is left" is what let a cancellation
/// reach the guest as a retryable rejection whose message was a serialized JSON
/// object.
pub fn protocol_tool_reply_to_lashlang_value(
    reply: lash_core::facade_support::ToolInvocationReply,
    replay_key: &str,
    cancellation: &ExecutionCancellation,
) -> Result<LashlangValue, ExecutionHostError> {
    let output = reply.output;
    match &output.outcome {
        ToolCallOutcome::Failure(failure) => {
            return Err(ExecutionHostError::from_tool_failure(failure, replay_key));
        }
        ToolCallOutcome::Cancelled(cancelled) => {
            return Err(cancelled_tool_terminal(cancelled, cancellation));
        }
        ToolCallOutcome::Success(_) => {}
    }
    Ok(lashlang::from_json(output.into_value_for_projection()))
}

/// The borrowed twin of [`protocol_tool_reply_to_lashlang_value`], for callers
/// that keep the output (the RLM executor keeps it for its call ledger).
pub fn protocol_tool_output_to_lashlang_value(
    output: &lash_core::ToolCallOutput,
    replay_key: &str,
    cancellation: &ExecutionCancellation,
) -> Result<LashlangValue, ExecutionHostError> {
    match &output.outcome {
        ToolCallOutcome::Success(_) => Ok(lashlang::from_json(output.value_for_projection())),
        ToolCallOutcome::Failure(failure) => {
            Err(ExecutionHostError::from_tool_failure(failure, replay_key))
        }
        ToolCallOutcome::Cancelled(cancelled) => {
            Err(cancelled_tool_terminal(cancelled, cancellation))
        }
    }
}

/// Ends the execution and names the cancellation, in that order: the error is
/// what the trace records, and the cancelled scope is what actually stops the
/// guest — `is_cancelled` refuses the next effect whatever a handler does with
/// the error.
fn cancelled_tool_terminal(
    cancelled: &lash_core::ToolCancellation,
    cancellation: &ExecutionCancellation,
) -> ExecutionHostError {
    cancellation.cancel();
    LashlangHostError::ToolCancelled {
        message: cancelled.message.clone(),
    }
    .into()
}

pub fn process_event_payload(
    value: &LashlangValue,
) -> Result<serde_json::Value, ExecutionHostError> {
    Ok(serde_json::json!({
        "value": lashlang_value_to_json(value)?,
        "text": value.to_string(),
    }))
}

/// Resolves a guest sleep into a durable intent without sampling the clock.
///
/// `until` keeps its absolute deadline; the effect seam derives the wait from
/// the substrate clock, so the journaled envelope is replay-stable even though
/// the remaining duration shrinks between attempts (FIG-2968).
pub fn process_sleep(
    kind: lashlang::SleepKind,
    value: &LashlangValue,
) -> Result<lash_core::SleepSpec, ExecutionHostError> {
    match kind {
        lashlang::SleepKind::For => Ok(lash_core::SleepSpec::For {
            duration_ms: duration_value_ms(value)?,
        }),
        lashlang::SleepKind::Until => Ok(lash_core::SleepSpec::Until {
            deadline_ms: deadline_value_ms(value)?,
        }),
    }
}

fn duration_value_ms(value: &LashlangValue) -> Result<u64, ExecutionHostError> {
    match value {
        LashlangValue::Number(value) if value.is_finite() && *value >= 0.0 => {
            Ok(value.round() as u64)
        }
        LashlangValue::String(value) => parse_duration_ms(value),
        other => Err(LashlangHostError::InvalidSleepDuration {
            actual: other.to_string(),
        }
        .into()),
    }
}

fn deadline_value_ms(value: &LashlangValue) -> Result<u64, ExecutionHostError> {
    match value {
        LashlangValue::Number(value) if value.is_finite() && *value >= 0.0 => {
            Ok(value.round() as u64)
        }
        LashlangValue::String(value) => chrono::DateTime::parse_from_rfc3339(value)
            .map(|deadline| deadline.timestamp_millis().max(0) as u64)
            .map_err(|source| LashlangHostError::InvalidSleepDeadline { source }.into()),
        other => Err(LashlangHostError::InvalidSleepDeadlineValue {
            actual: other.to_string(),
        }
        .into()),
    }
}

fn parse_duration_ms(value: &str) -> Result<u64, ExecutionHostError> {
    let value = value.trim();
    let (number, multiplier) = if let Some(number) = value.strip_suffix("ms") {
        (number, 1.0)
    } else if let Some(number) = value.strip_suffix('s') {
        (number, 1_000.0)
    } else if let Some(number) = value.strip_suffix('m') {
        (number, 60_000.0)
    } else if let Some(number) = value.strip_suffix('h') {
        (number, 3_600_000.0)
    } else {
        (value, 1.0)
    };
    let parsed = number.trim().parse::<f64>().map_err(|source| {
        let error: ExecutionHostError = LashlangHostError::InvalidDurationNumber {
            value: value.to_string(),
            source,
        }
        .into();
        error
    })?;
    if !parsed.is_finite() || parsed < 0.0 {
        return Err(LashlangHostError::InvalidDurationValue {
            value: value.to_string(),
        }
        .into());
    }
    Ok((parsed * multiplier).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core::{
        AttachmentSource, MediaType, ToolCallOutput, ToolFailure, ToolFailureClass,
        ToolFailureSource, ToolRetryStatus, ToolValue, facade_support::ToolInvocationReply,
    };
    use std::collections::BTreeMap;

    fn policy_failure(retry: ToolRetryStatus) -> ToolFailure {
        ToolFailure {
            class: ToolFailureClass::PermissionDenied,
            code: "approval_denied".to_string(),
            message: "approval was denied".to_string(),
            source: ToolFailureSource::Policy,
            retry,
            raw: None,
        }
    }

    #[test]
    fn both_tool_bridges_preserve_structured_failure_fields() {
        let cancellation = ExecutionCancellation::new();
        let borrowed = ToolCallOutput::failure(policy_failure(ToolRetryStatus::Safe {
            after_ms: Some(1_250),
        }));
        let borrowed_error =
            protocol_tool_output_to_lashlang_value(&borrowed, "borrowed-key", &cancellation)
                .expect_err("a failed output must remain an execution-host error");
        assert_eq!(
            serde_json::to_value(borrowed_error).expect("execution-host error serializes"),
            serde_json::json!({
                "message": "approval was denied",
                "tool_failure": {
                    "class": "permission_denied",
                    "code": "approval_denied",
                    "source": "policy",
                    "retry": { "type": "safe", "after_ms": 1_250 }
                    ,"replay_key": "borrowed-key"
                }
            })
        );

        let reply = ToolInvocationReply::from_output(ToolCallOutput::failure(policy_failure(
            ToolRetryStatus::Exhausted { attempts: 3 },
        )));
        let owned_error = protocol_tool_reply_to_lashlang_value(reply, "owned-key", &cancellation)
            .expect_err("a failed reply must remain an execution-host error");
        assert_eq!(
            serde_json::to_value(owned_error).expect("execution-host error serializes"),
            serde_json::json!({
                "message": "approval was denied",
                "tool_failure": {
                    "class": "permission_denied",
                    "code": "approval_denied",
                    "source": "policy",
                    "retry": { "type": "exhausted", "attempts": 3 }
                    ,"replay_key": "owned-key"
                }
            })
        );

        assert!(
            !cancellation.is_cancelled(),
            "a failed tool call is catchable: it must not end the execution"
        );
    }

    #[test]
    fn observed_failure_keeps_recorded_retry_and_replay_key_before_projection() {
        let output =
            ToolCallOutput::failure(policy_failure(ToolRetryStatus::Exhausted { attempts: 3 }));
        let observed = protocol_tool_output_to_lashlang_value(
            &output,
            "stable-effect-key",
            &ExecutionCancellation::new(),
        )
        .expect_err("failed tool output has typed provenance")
        .tool_failure()
        .expect("typed host error has effect provenance");
        assert_eq!(observed.class, ToolFailureClass::PermissionDenied);
        assert_eq!(observed.code, "approval_denied");
        assert_eq!(observed.message, "approval was denied");
        assert_eq!(observed.replay_key, "stable-effect-key");
        assert_eq!(observed.source, ToolFailureSource::Policy);
        assert_eq!(observed.retry, ToolRetryStatus::Exhausted { attempts: 3 });
        assert_eq!(
            protocol_tool_output_to_lashlang_value(
                &ToolCallOutput::success(serde_json::json!("ok")),
                "stable-effect-key",
                &ExecutionCancellation::new(),
            )
            .expect("successful output projects"),
            LashlangValue::String("ok".into()),
        );
    }

    #[test]
    fn successful_tool_bridges_keep_scalar_record_and_attachment_projections() {
        let cancellation = ExecutionCancellation::new();
        let owned = protocol_tool_reply_to_lashlang_value(
            ToolInvocationReply::success(serde_json::json!("ok")),
            "success-key",
            &cancellation,
        )
        .expect("a successful scalar reply projects");
        assert_eq!(owned, LashlangValue::String("ok".into()));

        let mut record = BTreeMap::new();
        record.insert(
            "count".to_string(),
            ToolValue::Number(serde_json::Number::from(2)),
        );
        record.insert(
            "attachment".to_string(),
            ToolValue::Attachment(AttachmentSource::external_url(
                MediaType::parse("image/png").unwrap(),
                "https://example.test/image.png",
            )),
        );
        let output = ToolCallOutput::success_tool_value(ToolValue::Object(record));
        let expected = output.value_for_projection();
        let borrowed =
            protocol_tool_output_to_lashlang_value(&output, "success-key", &cancellation)
                .expect("a successful record reply projects");
        assert_eq!(
            lashlang_value_to_json(&borrowed).expect("projected Lashlang value serializes"),
            expected
        );
    }

    #[test]
    fn sleep_for_resolves_to_a_relative_duration() {
        let value = LashlangValue::Number(1_500.0);
        assert_eq!(
            process_sleep(lashlang::SleepKind::For, &value).expect("a numeric duration resolves"),
            lash_core::SleepSpec::For { duration_ms: 1_500 }
        );
    }

    #[test]
    fn sleep_until_keeps_the_absolute_deadline_and_never_samples_the_clock() {
        // A deadline at a fixed absolute instant. `process_sleep` must return
        // the deadline itself: the rejected implementation subtracted
        // `Utc::now()` here, so two calls a moment apart produced different
        // durations and the journaled envelope failed its own replay fence
        // (FIG-2968).
        let deadline_ms = 1_800_000_000_000_u64;
        let value = LashlangValue::Number(deadline_ms as f64);
        let first = process_sleep(lashlang::SleepKind::Until, &value).expect("deadline resolves");
        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = process_sleep(lashlang::SleepKind::Until, &value).expect("deadline resolves");
        assert_eq!(first, second);
        assert_eq!(first, lash_core::SleepSpec::Until { deadline_ms });
    }

    #[test]
    fn sleep_until_parses_an_absolute_rfc3339_deadline() {
        let value = LashlangValue::String("2030-01-01T00:00:00Z".into());
        match process_sleep(lashlang::SleepKind::Until, &value).expect("rfc3339 deadline resolves")
        {
            lash_core::SleepSpec::Until { deadline_ms } => {
                assert_eq!(deadline_ms, 1_893_456_000_000);
            }
            other => panic!("expected an absolute deadline, got {other:?}"),
        }
    }

    /// A cancelled tool call is the third arm, and it is a host terminal.
    ///
    /// Before this, the bridges decoded a three-variant outcome with an
    /// early-returned `Failure`, an `is_success()` boolean and an implicit
    /// `else`, so a cancellation arrived as `ToolRejected` — a name that asserts
    /// something untrue about it — carrying a serialized JSON object where its
    /// message belonged, and catchable by a guest handler that means to retry a
    /// rejected tool. `LashlangProcessHost` takes trouble to make a cancelled
    /// process an uncatchable terminal; the bridge was the one path converting a
    /// cancellation into the catchable family instead.
    #[test]
    fn both_tool_bridges_end_the_execution_on_a_cancelled_call() {
        let cancelled = || {
            ToolCallOutput::cancelled(lash_core::ToolCancellation::runtime(
                "approval window closed",
            ))
        };

        let borrowed_scope = ExecutionCancellation::new();
        let borrowed_error =
            protocol_tool_output_to_lashlang_value(&cancelled(), "cancel-key", &borrowed_scope)
                .expect_err("a cancelled output is neither a value nor a retryable failure");
        assert_eq!(
            borrowed_error.message(),
            "approval window closed",
            "the cancellation's own message, not a serialized object"
        );
        assert_eq!(
            borrowed_error.tool_failure_class(),
            None,
            "a cancellation carries no tool failure classification"
        );
        assert!(
            borrowed_scope.is_cancelled(),
            "the host reports cancelled from here on, so the VM refuses the next effect"
        );

        let owned_scope = ExecutionCancellation::new();
        let owned_error = protocol_tool_reply_to_lashlang_value(
            ToolInvocationReply::from_output(cancelled()),
            "cancel-key",
            &owned_scope,
        )
        .expect_err("a cancelled reply is neither a value nor a retryable failure");
        assert_eq!(owned_error.message(), "approval window closed");
        assert!(owned_scope.is_cancelled());
    }

    /// The run's scope carries the engine's cancellation too, so one field
    /// answers `is_cancelled` for both.
    #[test]
    fn a_run_scope_is_cancelled_by_its_parent() {
        let engine = CancellationToken::new();
        let run = ExecutionCancellation::child_of(&engine);
        assert!(!run.is_cancelled());
        engine.cancel();
        assert!(run.is_cancelled());
    }

    /// The end-to-end pin for the seam the bridge owns: a guest that catches
    /// the cancelled call's error still terminates, and the catch's own next
    /// effect is refused — the scope the bridge trips is the flag
    /// `is_cancelled` reports, so no handler can settle the run back into a
    /// normal finish.
    struct CancelledToolHost {
        cancellation: ExecutionCancellation,
        performs: std::sync::atomic::AtomicUsize,
    }

    impl lashlang::ExecutionHost for CancelledToolHost {
        async fn perform(
            &self,
            op: lashlang::AbilityOp,
        ) -> Result<lashlang::AbilityResult, ExecutionHostError> {
            self.performs
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match op {
                lashlang::AbilityOp::ResourceOperation(_) => {
                    protocol_tool_output_to_lashlang_value(
                        &ToolCallOutput::cancelled(lash_core::ToolCancellation::runtime(
                            "approval window closed",
                        )),
                        "cancel-key",
                        &self.cancellation,
                    )
                    .map(lashlang::AbilityResult::Value)
                }
                lashlang::AbilityOp::Finish(value) | lashlang::AbilityOp::Fail(value) => {
                    Ok(lashlang::AbilityResult::Value(value))
                }
                _ => Err(ExecutionHostError::new(
                    "the cancelled-tool host performs resource operations only",
                )),
            }
        }

        fn is_cancelled(&self) -> bool {
            self.cancellation.is_cancelled()
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_cancelled_tool_call_is_terminal_past_a_guest_catch() {
        use lashlang::testing::ast_builders as b;

        let call = || {
            b::module_call(
                &["tools"],
                "echo",
                vec![b::record(vec![("value", b::string("caught"))])],
            )
        };
        let program = b::program(vec![b::finish(b::try_expr(
            call(),
            Some(b::catch("error", call())),
            None,
        ))]);
        let compiled = lashlang::testing::harness::compile_labeled_program(program);
        let host = CancelledToolHost {
            cancellation: ExecutionCancellation::new(),
            performs: std::sync::atomic::AtomicUsize::new(0),
        };
        let mut state = lashlang::State::new();
        let outcome =
            lashlang::testing::harness::execute_compiled(&compiled, &mut state, &host).await;
        assert!(
            matches!(outcome, Err(lashlang::RuntimeError::HostCancelled)),
            "a guest catch cannot settle a cancelled run back into a finish: {outcome:?}"
        );
        assert_eq!(
            host.performs.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the effect inside the catch was refused before it reached the host"
        );
    }
}
