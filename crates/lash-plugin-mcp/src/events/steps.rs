//! The subscription engine's step bodies: `fetch` reads one batch from the
//! server, and `dispatch` makes a batch's keyed deliveries.
//!
//! Both are `Repeatable`. A fetch is a read: a rerun reads from the same
//! committed cursor. A dispatch's every delivery is keyed by its item, so a
//! rerun finds what the first run started and starts nothing new; that is
//! also why a dispatch that met a transient core fault answers a capacity
//! verdict and runs again whole.

use std::time::Duration;

use lash_core::tool_run::{MaterialOwner, MaterialRole};
use lash_core::{
    ArgsMode, CapacityCause, CapacityVerdict, EngineStepAnswer, EngineStepKind, EngineStepRun,
    ExecutionPolicy, Material, ProcessId, ProcessOriginator, ProcessStartRequest,
    SessionTurnOutcome, SettledOutput,
};
use tokio_util::sync::CancellationToken;

use super::config::McpDeliveryMode;
use super::engine::{
    Batch, DISPATCH, DeliveryItem, DeliveryResult, DispatchInput, DispatchOutcome, FETCH,
    FetchInput, FetchOutcome,
};
use super::runtime::{McpEvents, McpEventsCore, McpEventsPortError};
use super::spec::{
    McpReactionTarget, McpSubscriptionSpec, delivery_key, delivery_session, delivery_turn,
    event_input, gap_input,
};
use super::status::McpDeliveryError;

/// The longest one fetch may run: a poll or a push window, with its close.
pub(crate) const FETCH_EXECUTION: Duration = Duration::from_secs(300);
/// The longest one dispatch of a batch may run.
pub(crate) const DISPATCH_EXECUTION: Duration = Duration::from_secs(600);
/// How long a step waits before it runs again when the core is not bound or
/// failed transiently.
const CORE_RETRY: Duration = Duration::from_secs(1);
/// The session-turn definition key of an isolated reaction's child.
pub const MCP_EVENTS_SESSION_TURN: &str = "mcp-events-reaction";

/// The subscription engine's bodies, over the plugin's pool and bound core.
pub(crate) struct SubscriptionSteps {
    pub(crate) events: McpEvents,
}

fn settled(process: &ProcessId, payload: &impl serde::Serialize) -> EngineStepAnswer {
    let text = serde_json::to_string(payload).unwrap_or_else(|error| {
        // Every outcome is plain data; this is unreachable.
        format!("{{\"outcome\":\"failed\",\"error\":\"{error}\"}}")
    });
    SettledOutput::Completed(Material::journal_local(
        MaterialOwner::Process {
            process_id: process.clone(),
        },
        MaterialRole::AttemptOutput,
        text,
    ))
    .into()
}

fn capacity(detail: impl Into<String>) -> EngineStepAnswer {
    EngineStepAnswer::Capacity(CapacityVerdict::new(
        CapacityCause::RetryableInfrastructure,
        CORE_RETRY,
        detail,
    ))
}

/// An input the engine never writes: its outcome does not decode either,
/// so the subscription fails visibly instead of guessing.
fn malformed(run: &EngineStepRun, error: impl std::fmt::Display) -> EngineStepAnswer {
    settled(
        &run.process,
        &serde_json::json!({
            "malformed": format!("step `{}` input does not decode: {error}", run.kind.0),
        }),
    )
}

#[async_trait::async_trait]
impl lash_core::EngineSteps for SubscriptionSteps {
    fn kinds(&self) -> Vec<EngineStepKind> {
        vec![EngineStepKind::new(FETCH), EngineStepKind::new(DISPATCH)]
    }

    fn execution(&self, kind: &EngineStepKind) -> Duration {
        if kind.0 == DISPATCH {
            DISPATCH_EXECUTION
        } else {
            FETCH_EXECUTION
        }
    }

    /// A failed run is a fault of the body, not of the server (the server's
    /// refusals are outcomes): a few quick retries.
    fn retry(&self, _kind: &EngineStepKind) -> ExecutionPolicy {
        ExecutionPolicy::repeatable(std::num::NonZeroU32::MIN.saturating_add(2), 500, 5_000)
    }

    async fn run(&self, run: EngineStepRun, cancel: CancellationToken) -> EngineStepAnswer {
        match run.kind.0.as_str() {
            FETCH => match serde_json::from_value::<FetchInput>(run.input.clone()) {
                Ok(input) => {
                    let outcome = self.fetch(&run, input, cancel).await;
                    settled(&run.process, &outcome)
                }
                Err(error) => malformed(&run, error),
            },
            DISPATCH => match serde_json::from_value::<DispatchInput>(run.input.clone()) {
                Ok(input) => self.dispatch(&run, input, cancel).await,
                Err(error) => malformed(&run, error),
            },
            _ => malformed(&run, "the subscription engine runs no such step"),
        }
    }
}

impl SubscriptionSteps {
    /// One poll, or one push window, from the committed cursor.
    async fn fetch(
        &self,
        run: &EngineStepRun,
        input: FetchInput,
        cancel: CancellationToken,
    ) -> FetchOutcome {
        let _in_flight = self.events.activity().fetch();
        let FetchInput { spec, cursor } = input;
        let bounds = &spec.bounds;
        let mut params = serde_json::json!({
            "name": spec.event,
            "arguments": spec.arguments,
            "cursor": cursor,
        });
        if let Some(max_age_ms) = bounds.max_age_ms {
            params["maxAgeMs"] = serde_json::json!(max_age_ms);
        }
        let pool = self.events.pool();
        let fetched = match spec.mode {
            McpDeliveryMode::Poll => {
                params["maxEvents"] = serde_json::json!(bounds.max_events);
                pool.poll_events(
                    &spec.server,
                    params,
                    Duration::from_millis(bounds.fetch_timeout_ms),
                )
                .await
                .map(|poll| Batch {
                    events: poll.events,
                    cursor: poll.cursor.or(cursor),
                    truncated: poll.truncated,
                    has_more: poll.has_more,
                    next_poll_ms: poll.next_poll_ms,
                    terminated: None,
                    at_ms: 0,
                })
            }
            McpDeliveryMode::Push => pool
                .stream_events(
                    &spec.server,
                    params,
                    Duration::from_millis(bounds.push_window_ms),
                    usize::try_from(bounds.max_events).unwrap_or(usize::MAX),
                    cancel,
                )
                .await
                .map(|window| Batch {
                    events: window.events,
                    cursor: window.cursor.or(cursor),
                    truncated: window.truncated,
                    has_more: false,
                    next_poll_ms: None,
                    terminated: window.terminated,
                    at_ms: 0,
                }),
        };
        // The fetch ends on the clock `run.now` was read on, never that
        // reading plus wall time: the next poll is scheduled on that clock.
        let at_ms = i64::try_from(run.clock.timestamp_ms()).unwrap_or(i64::MAX);
        match fetched {
            Ok(batch) => FetchOutcome::Batch(Batch { at_ms, ..batch }),
            Err(error) => FetchOutcome::Failed { error, at_ms },
        }
    }

    /// Deliver a batch, item by item, each under its derived key. Once the
    /// subscription is cancelled it starts nothing more.
    async fn dispatch(
        &self,
        run: &EngineStepRun,
        input: DispatchInput,
        cancel: CancellationToken,
    ) -> EngineStepAnswer {
        let core = match self.events.core() {
            Ok(core) => core,
            Err(error) => return capacity(error.to_string()),
        };
        let timeout = Duration::from_millis(input.spec.bounds.dispatch_timeout_ms);
        let mut results = Vec::with_capacity(input.items.len());
        for item in &input.items {
            let key_item = item.key_item();
            if cancel.is_cancelled() {
                results.push((key_item, DeliveryResult::Skipped));
                continue;
            }
            let delivered = tokio::time::timeout(
                timeout,
                deliver(core.as_ref(), &run.process, &input.spec, item),
            )
            .await
            .unwrap_or_else(|_| {
                Err(McpEventsPortError::Unavailable {
                    message: format!(
                        "the delivery did not answer within {} ms",
                        timeout.as_millis()
                    ),
                })
            });
            let result = match delivered {
                Ok(result) => result,
                Err(McpEventsPortError::Conflict { message }) => DeliveryResult::Failed {
                    error: McpDeliveryError::Conflict { message },
                },
                Err(McpEventsPortError::Refused { message }) => DeliveryResult::Failed {
                    error: McpDeliveryError::Refused { message },
                },
                // Every delivery is keyed: the whole batch runs again.
                Err(
                    error @ (McpEventsPortError::Unbound | McpEventsPortError::Unavailable { .. }),
                ) => {
                    return capacity(error.to_string());
                }
            };
            results.push((key_item, result));
        }
        settled(&run.process, &DispatchOutcome { results })
    }
}

/// One keyed delivery.
async fn deliver(
    core: &dyn McpEventsCore,
    subscription: &ProcessId,
    spec: &McpSubscriptionSpec,
    item: &DeliveryItem,
) -> Result<DeliveryResult, McpEventsPortError> {
    let key = delivery_key(subscription, spec.generation, &item.key_item());
    let originator = ProcessOriginator::host_scoped(format!("mcp-events:{subscription}"));
    let input = match item {
        DeliveryItem::Event { occurrence } => event_input(subscription, spec, occurrence),
        DeliveryItem::Gap { gap } => gap_input(subscription, spec, gap),
    };
    match &spec.target {
        McpReactionTarget::Session { session } => {
            core.send(session, delivery_turn(&key), input).await?;
        }
        McpReactionTarget::Isolated { child } => {
            // Each delivery creates a child session of its own, named by its
            // key: a repeat names the same child, never a second one.
            let mut child = child.as_ref().clone();
            child.session_id = Some(delivery_session(&key));
            let request = ProcessStartRequest::session_turn(
                MCP_EVENTS_SESSION_TURN,
                child,
                input,
                SessionTurnOutcome::Turn,
                originator,
                lash_core::LifetimeDecision::Detached,
            )
            .with_host_start_key(key);
            core.start(request).await?;
        }
        McpReactionTarget::Workflow(workflow) => {
            let DeliveryItem::Event { occurrence } = item else {
                return Ok(DeliveryResult::Skipped);
            };
            let args = match workflow.mapping.arguments(occurrence) {
                Ok(args) => args,
                Err(error) => {
                    return Ok(DeliveryResult::Failed {
                        error: McpDeliveryError::Mapping(error),
                    });
                }
            };
            if let Err(refusal) = core
                .check_args(&workflow.definition, &args, ArgsMode::Complete)
                .await?
            {
                return Ok(DeliveryResult::Failed {
                    error: McpDeliveryError::Arguments {
                        code: refusal.code().to_owned(),
                        message: refusal.to_string(),
                    },
                });
            }
            let request = ProcessStartRequest::definition(
                workflow.definition.id.clone(),
                Some(workflow.definition.signature.clone()),
                args,
                workflow.launch.clone(),
                originator,
                lash_core::LifetimeDecision::Detached,
            )
            .with_host_start_key(key);
            core.start(request).await?;
        }
    }
    Ok(DeliveryResult::Delivered)
}
