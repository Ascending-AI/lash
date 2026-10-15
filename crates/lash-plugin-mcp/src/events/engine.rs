//! The subscription process engine: one durable lash process per
//! subscription, advanced by lash like any engine (ADR 0132 §7).
//!
//! Its loop, from committed state:
//!
//! 1. **Fetch.** A `fetch` step polls once (`events/poll`) or holds one push
//!    window (`events/stream`), from the saved cursor.
//! 2. **Commit.** The transition that reads the batch commits the new cursor
//!    and admits the `dispatch` step of the batch's deliveries, in one
//!    `process.advance` commit: a crash before it re-fetches from the old
//!    cursor; a crash after it dispatches the same deliveries again.
//! 3. **Dispatch.** The `dispatch` step delivers each item under a key
//!    derived from (subscription, generation, item). A keyed send or start
//!    accepts identical content once, so a rerun or a redelivery from the
//!    server starts nothing new.
//! 4. **Wait.** It fetches again at once while the server has more, else it
//!    sleeps until `nextPollMs` (at least the host's floor) has passed. A
//!    push subscription reopens its stream at once.
//!
//! A truncated batch records a typed gap, which is also delivered to a
//! session or isolated reaction as a keyed notice. A failed fetch backs off
//! and retries; one the server refuses for good ends the subscription with
//! the typed refusal; a principal the server forbids, or a server that stays
//! saturated or unreachable, parks it on its resume key. Cancelling the
//! process is the unsubscribe: it lets an in-flight push window close its
//! stream, and admits no dispatch after it.

use std::sync::Arc;
use std::time::Duration;

use lash_core::durable_port::DurableInstant;
use lash_core::{
    EngineAction, EngineEvent, EngineState, EngineStateFormat, EngineStepKind, KeyName,
    ProcessEngine, ProcessInfraError, ProcessOutcome, StepName, StepRequest,
};
use serde::{Deserialize, Serialize};

use super::client::McpEventsCallError;
use super::config::McpDeliveryMode;
use super::spec::{McpReactionTarget, McpSubscriptionSpec};
use super::status::{
    McpDeliveryFailure, McpEventGap, McpParkReason, McpSubscriptionEnd, McpSubscriptionStatus,
    remember,
};
use super::wire::{McpEventOccurrence, McpEventsRefusal, McpEventsWireError};

/// The kind every subscription process is registered under.
pub const MCP_EVENTS_ENGINE: &str = "mcp-events-subscription";
/// The one key a parked subscription waits on, re-pinned per park.
pub const MCP_EVENTS_RESUME_KEY: &str = "mcp.events.resume";

/// The format of a subscription process's committed state: the
/// [`Subscription`] encoding `EngineState.bytes` holds, and the fetch and
/// dispatch step payloads journaled beside it. The bytes carry no stamp of
/// their own: the process's format set carries this version as its engine
/// surface, so only a node whose engine decodes it claims the process
/// (ADR 0106 §1).
///
/// version_guard(
///     roots(Subscription, FetchInput, FetchOutcome, DispatchInput, DispatchOutcome),
/// )
/// version_surface = "drain"
/// format_outside_manifest = "engine-owned process state: the process's format set admits it as its engine surface (ADR 0106 §1); the engine manifest rows of ADR 0104 §2 are not wired"
const STATE_VERSION: u32 = 1;
pub(crate) const FETCH: &str = "fetch";
pub(crate) const DISPATCH: &str = "dispatch";
/// How long an unsubscribe waits for an in-flight push window to close its
/// stream.
const CANCEL_GRACE: Duration = Duration::from_secs(10);

/// What one fetch step answers.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(crate) enum FetchOutcome {
    Batch(Batch),
    Failed {
        error: McpEventsCallError,
        at_ms: i64,
    },
}

/// What one fetch delivered, and where the subscription resumes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Batch {
    pub(crate) events: Vec<McpEventOccurrence>,
    pub(crate) cursor: Option<String>,
    pub(crate) truncated: bool,
    pub(crate) has_more: bool,
    pub(crate) next_poll_ms: Option<u64>,
    /// The push stream's `terminated`, when the server ended it.
    pub(crate) terminated: Option<McpEventsWireError>,
    /// When the fetch ended, on the store's clock.
    pub(crate) at_ms: i64,
}

/// What a fetch step is handed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct FetchInput {
    pub(crate) spec: McpSubscriptionSpec,
    pub(crate) cursor: Option<String>,
}

/// One delivery a dispatch makes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "item", rename_all = "snake_case")]
pub(crate) enum DeliveryItem {
    Event { occurrence: McpEventOccurrence },
    Gap { gap: McpEventGap },
}

impl DeliveryItem {
    pub(crate) fn key_item(&self) -> String {
        match self {
            Self::Event { occurrence } => super::spec::event_item(&occurrence.event_id),
            Self::Gap { gap } => super::spec::gap_item(gap.ordinal),
        }
    }
}

/// What a dispatch step is handed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DispatchInput {
    pub(crate) spec: McpSubscriptionSpec,
    pub(crate) items: Vec<DeliveryItem>,
}

/// How one delivery ended.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub(crate) enum DeliveryResult {
    /// Accepted now, or found accepted under its key.
    Delivered,
    /// Refused for good; on record.
    Failed {
        error: super::status::McpDeliveryError,
    },
    /// Not attempted: the subscription was cancelled first.
    Skipped,
}

/// What a dispatch step answers, item by item.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DispatchOutcome {
    pub(crate) results: Vec<(String, DeliveryResult)>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "next", rename_all = "snake_case")]
enum Next {
    Fetch,
    SleepUntil { at_ms: i64 },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
enum Phase {
    Fetching,
    Dispatching {
        items: Vec<DeliveryItem>,
        then: Next,
    },
    Sleeping,
    Parking,
    Parked,
    /// Unsubscribed while a step ran: the terminal follows its settling.
    Cancelling {
        origin: lash_core::CancelOrigin,
    },
}

/// A subscription's committed state.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Subscription {
    spec: McpSubscriptionSpec,
    cursor: Option<String>,
    phase: Phase,
    status: McpSubscriptionStatus,
}

fn infra(message: impl std::fmt::Display) -> ProcessInfraError {
    ProcessInfraError::new(lash_core::PluginError::Session(message.to_string()))
}

fn format() -> EngineStateFormat {
    EngineStateFormat {
        kind: MCP_EVENTS_ENGINE.to_owned(),
        version: STATE_VERSION,
    }
}

fn decode_state(state: &EngineState) -> Result<Option<Subscription>, ProcessInfraError> {
    if state.bytes.is_empty() {
        return Ok(None);
    }
    serde_json::from_slice(&state.bytes)
        .map(Some)
        .map_err(|error| infra(format!("a subscription's state does not decode: {error}")))
}

fn encode_state(subscription: &Subscription) -> Result<EngineState, ProcessInfraError> {
    Ok(EngineState {
        format: format(),
        bytes: serde_json::to_vec(subscription).map_err(infra)?,
        pages: None,
    })
}

fn step(name: &str, input: impl Serialize) -> Result<EngineAction, ProcessInfraError> {
    Ok(EngineAction::Steps {
        steps: vec![StepRequest::Engine {
            step: StepName(name.to_owned()),
            kind: EngineStepKind::new(name),
            input: serde_json::to_value(input).map_err(infra)?,
        }],
        wake: None,
    })
}

fn fetch(subscription: &mut Subscription) -> Result<EngineAction, ProcessInfraError> {
    subscription.phase = Phase::Fetching;
    step(
        FETCH,
        FetchInput {
            spec: subscription.spec.clone(),
            cursor: subscription.cursor.clone(),
        },
    )
}

fn dispatch(
    subscription: &mut Subscription,
    items: Vec<DeliveryItem>,
    then: Next,
) -> Result<EngineAction, ProcessInfraError> {
    let action = step(
        DISPATCH,
        DispatchInput {
            spec: subscription.spec.clone(),
            items: items.clone(),
        },
    )?;
    subscription.phase = Phase::Dispatching { items, then };
    Ok(action)
}

fn terminal_cancelled(origin: lash_core::CancelOrigin) -> EngineAction {
    EngineAction::Terminal(ProcessOutcome::from_tool_output(
        lash_core::ToolCallOutput::cancelled(
            lash_core::ToolCancellation::runtime("the subscription was unsubscribed")
                .with_origin(origin),
        ),
    ))
}

fn terminal_ended(end: &McpSubscriptionEnd) -> EngineAction {
    let mut failure = lash_core::ToolFailure::tool(
        lash_core::ToolFailureClass::External,
        "mcp_event_subscription_ended",
        end.to_string(),
    );
    failure.raw = serde_json::to_value(end)
        .ok()
        .map(lash_core::ToolValue::untrusted_json);
    EngineAction::Terminal(ProcessOutcome::from_tool_output(
        lash_core::ToolCallOutput::failure(failure),
    ))
}

/// The wait after `failures` failed fetches in a row.
fn backoff_ms(spec: &McpSubscriptionSpec, failures: u32) -> i64 {
    let shift = failures.saturating_sub(1).min(32);
    let wait = spec
        .bounds
        .retry_initial_ms
        .saturating_mul(1_u64 << shift)
        .min(spec.bounds.retry_max_ms);
    i64::try_from(wait).unwrap_or(i64::MAX)
}

fn millis(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn sleep_until(subscription: &mut Subscription, at_ms: i64) -> EngineAction {
    subscription.phase = Phase::Sleeping;
    EngineAction::Sleep {
        until: DurableInstant(at_ms),
        site: None,
    }
}

/// Park on the resume key, with `reason` on record until a host resumes.
fn park(subscription: &mut Subscription, reason: McpParkReason) -> EngineAction {
    subscription.status.parked = Some(reason);
    subscription.phase = Phase::Parking;
    EngineAction::PinKey {
        name: KeyName(MCP_EVENTS_RESUME_KEY.to_owned()),
        bound: lash_core::ParkBound::UntilScopeEnd,
    }
}

/// What follows a batch once its deliveries are made: the server's end of
/// the subscription, a park it asked for, or the next fetch.
fn conclude(
    subscription: &mut Subscription,
    then: Next,
) -> Result<EngineAction, ProcessInfraError> {
    if let Some(end) = &subscription.status.ended {
        return Ok(terminal_ended(end));
    }
    if let Some(reason) = subscription.status.parked.clone() {
        return Ok(park(subscription, reason));
    }
    match then {
        Next::Fetch => fetch(subscription),
        Next::SleepUntil { at_ms } => Ok(sleep_until(subscription, at_ms)),
    }
}

/// The transition a fetched batch makes: the new cursor commits with the
/// admission of the batch's dispatch.
fn fetched(
    subscription: &mut Subscription,
    batch: Batch,
) -> Result<EngineAction, ProcessInfraError> {
    let Batch {
        events,
        cursor,
        truncated,
        has_more,
        next_poll_ms,
        terminated,
        at_ms,
    } = batch;
    subscription.status.fetch_failures = 0;
    subscription.status.last_fetch_failure = None;
    let mut items = Vec::new();
    if truncated {
        let gap = McpEventGap {
            ordinal: subscription.status.gaps + 1,
            observed_at_ms: at_ms,
            had_cursor: subscription.cursor.is_some(),
        };
        subscription.status.gaps += 1;
        remember(&mut subscription.status.recent_gaps, gap.clone());
        // A workflow trigger starts once per event; a gap starts nothing
        // there and stays on record.
        if !matches!(subscription.spec.target, McpReactionTarget::Workflow(_)) {
            items.push(DeliveryItem::Gap { gap });
        }
    }
    items.extend(
        events
            .into_iter()
            .map(|occurrence| DeliveryItem::Event { occurrence }),
    );
    subscription.cursor = cursor;
    subscription.status.has_cursor = subscription.cursor.is_some();
    if let Some(error) = terminated {
        // What the stream delivered before its end still reacts; the end
        // follows the dispatch.
        match McpEventsRefusal::of_code(error.code) {
            McpEventsRefusal::Forbidden => {
                subscription.status.parked = Some(McpParkReason::Forbidden { error });
            }
            _ => subscription.status.ended = Some(McpSubscriptionEnd::Terminated { error }),
        }
    }
    let then = match subscription.spec.mode {
        McpDeliveryMode::Push => Next::Fetch,
        McpDeliveryMode::Poll if has_more => Next::Fetch,
        McpDeliveryMode::Poll => {
            let floor = subscription.spec.bounds.poll_floor_ms;
            let wait = next_poll_ms.unwrap_or(floor).max(floor);
            Next::SleepUntil {
                at_ms: at_ms.saturating_add(millis(wait)),
            }
        }
    };
    if items.is_empty() {
        return conclude(subscription, then);
    }
    dispatch(subscription, items, then)
}

/// The transition a failed fetch makes. `at_ms` is when it failed, if the
/// step said; without it the next fetch runs at once.
fn fetch_failed(
    subscription: &mut Subscription,
    error: McpEventsCallError,
    at_ms: Option<i64>,
) -> Result<EngineAction, ProcessInfraError> {
    match &error {
        McpEventsCallError::Refused {
            refusal: McpEventsRefusal::Forbidden,
            error: wire,
            ..
        } => {
            let reason = McpParkReason::Forbidden {
                error: wire.clone(),
            };
            subscription.status.last_fetch_failure = Some(error);
            return Ok(park(subscription, reason));
        }
        McpEventsCallError::Refused {
            refusal:
                McpEventsRefusal::NotFound
                | McpEventsRefusal::InvalidParams
                | McpEventsRefusal::Unsupported
                | McpEventsRefusal::MethodNotFound,
            ..
        }
        | McpEventsCallError::NotAdvertised { .. }
        | McpEventsCallError::Disabled { .. }
        | McpEventsCallError::UnknownServer { .. } => {
            let end = McpSubscriptionEnd::Refused { error };
            let action = terminal_ended(&end);
            subscription.status.ended = Some(end);
            return Ok(action);
        }
        _ => {}
    }
    subscription.status.fetch_failures = subscription.status.fetch_failures.saturating_add(1);
    subscription.status.last_fetch_failure = Some(error.clone());
    let failures = subscription.status.fetch_failures;
    if failures >= subscription.spec.bounds.failures_before_park {
        let reason = match error.refusal() {
            Some(McpEventsRefusal::ResourceExhausted) => McpParkReason::Saturated {
                failures,
                last: error,
            },
            _ => McpParkReason::Unavailable {
                failures,
                last: error,
            },
        };
        return Ok(park(subscription, reason));
    }
    match at_ms {
        Some(at_ms) => {
            let at_ms = at_ms.saturating_add(backoff_ms(&subscription.spec, failures));
            Ok(sleep_until(subscription, at_ms))
        }
        None => fetch(subscription),
    }
}

/// Record what a dispatch delivered and failed.
fn record(subscription: &mut Subscription, outcome: DispatchOutcome) {
    for (item, result) in outcome.results {
        match result {
            DeliveryResult::Delivered => {
                if item.starts_with("event:") {
                    subscription.status.delivered += 1;
                }
            }
            DeliveryResult::Failed { error } => {
                subscription.status.delivery_failures += 1;
                remember(
                    &mut subscription.status.recent_delivery_failures,
                    McpDeliveryFailure { item, error },
                );
            }
            DeliveryResult::Skipped => {}
        }
    }
}

fn completed<T: serde::de::DeserializeOwned>(
    outcome: &lash_core::SettledOutput,
) -> Result<Option<T>, ProcessInfraError> {
    match outcome {
        lash_core::SettledOutput::Completed(material) => serde_json::from_str(material.payload())
            .map(Some)
            .map_err(|error| infra(format!("a step's outcome does not decode: {error}"))),
        _ => Ok(None),
    }
}

fn started(payload: serde_json::Value) -> Result<(Subscription, EngineAction), ProcessInfraError> {
    let spec: McpSubscriptionSpec = serde_json::from_value(payload)
        .map_err(|error| infra(format!("a subscription's payload does not decode: {error}")))?;
    let mut subscription = Subscription {
        status: McpSubscriptionStatus {
            server: spec.server.clone(),
            event: spec.event.clone(),
            mode: spec.mode,
            has_cursor: false,
            delivered: 0,
            gaps: 0,
            recent_gaps: Vec::new(),
            delivery_failures: 0,
            recent_delivery_failures: Vec::new(),
            fetch_failures: 0,
            last_fetch_failure: None,
            parked: None,
            ended: None,
        },
        spec,
        cursor: None,
        phase: Phase::Fetching,
    };
    let action = fetch(&mut subscription)?;
    Ok((subscription, action))
}

fn settled(
    subscription: &mut Subscription,
    step: &str,
    outcome: &lash_core::SettledOutput,
) -> Result<EngineAction, ProcessInfraError> {
    match (subscription.phase.clone(), step) {
        (Phase::Cancelling { origin }, _) => {
            if step == DISPATCH
                && let Some(outcome) = completed::<DispatchOutcome>(outcome)?
            {
                record(subscription, outcome);
            }
            Ok(terminal_cancelled(origin))
        }
        (Phase::Fetching, FETCH) => match completed::<FetchOutcome>(outcome)? {
            Some(FetchOutcome::Batch(batch)) => fetched(subscription, batch),
            Some(FetchOutcome::Failed { error, at_ms }) => {
                fetch_failed(subscription, error, Some(at_ms))
            }
            None => {
                let error = McpEventsCallError::Transport {
                    server: subscription.spec.server.clone(),
                    method: FETCH.to_owned(),
                    message: format!("the fetch step did not complete: {outcome:?}"),
                };
                fetch_failed(subscription, error, None)
            }
        },
        (Phase::Dispatching { items, then }, DISPATCH) => {
            match completed::<DispatchOutcome>(outcome)? {
                Some(outcome) => {
                    record(subscription, outcome);
                    conclude(subscription, then)
                }
                // It never completed within its policy: dispatch the same
                // deliveries again. Their keys make a repeat start nothing.
                None => dispatch(subscription, items, then),
            }
        }
        (phase, step) => Err(infra(format!(
            "step `{step}` settled while the subscription was {phase:?}"
        ))),
    }
}

/// The subscription engine.
pub(crate) struct SubscriptionEngine;

impl SubscriptionEngine {
    fn transition(
        subscription: Option<Subscription>,
        event: EngineEvent,
    ) -> Result<(Subscription, EngineAction), ProcessInfraError> {
        match (subscription, event) {
            (None, EngineEvent::Started { payload }) => started(payload),
            (Some(mut subscription), event) => {
                let action = Self::react(&mut subscription, event)?;
                Ok((subscription, action))
            }
            (None, _) => Err(infra("a subscription advanced before it started")),
        }
    }

    fn react(
        subscription: &mut Subscription,
        event: EngineEvent,
    ) -> Result<EngineAction, ProcessInfraError> {
        match event {
            EngineEvent::Started { .. } => Err(infra("a subscription started twice")),
            EngineEvent::StepSettled { step, outcome, .. } => {
                settled(subscription, &step.0, &outcome)
            }
            EngineEvent::Woke => match subscription.phase {
                Phase::Sleeping => fetch(subscription),
                _ => Ok(EngineAction::Idle),
            },
            EngineEvent::KeyPinned { name, .. } => {
                subscription.phase = Phase::Parked;
                Ok(EngineAction::AwaitExternal { name, site: None })
            }
            EngineEvent::ExternalResolved { .. } => {
                subscription.status.parked = None;
                subscription.status.fetch_failures = 0;
                fetch(subscription)
            }
            EngineEvent::ExternalTimedOut { .. }
            | EngineEvent::ProcessEnded { .. }
            | EngineEvent::ProcessWaitTimedOut { .. } => Ok(EngineAction::Idle),
            EngineEvent::Cancelled { origin, .. } => match subscription.phase {
                // An in-flight fetch sees its token and closes its stream; a
                // dispatch sees it and starts nothing more. Both settle.
                Phase::Fetching | Phase::Dispatching { .. } => {
                    subscription.phase = Phase::Cancelling { origin };
                    Ok(EngineAction::Idle)
                }
                _ => Ok(terminal_cancelled(origin)),
            },
        }
    }
}

#[async_trait::async_trait]
impl ProcessEngine for SubscriptionEngine {
    fn settled_ran(
        &self,
        _kind: &lash_core::EngineStepKind,
        _output: &lash_core::SettledOutput,
    ) -> Vec<(u32, u64)> {
        Vec::new()
    }

    fn trace_class(&self) -> lash_core::ProcessTraceClass {
        lash_core::ProcessTraceClass::Internal
    }

    fn kind(&self) -> &'static str {
        MCP_EVENTS_ENGINE
    }

    fn state_format(&self) -> EngineStateFormat {
        format()
    }

    fn cancel_grace(&self) -> Duration {
        CANCEL_GRACE
    }

    fn program_identity(
        &self,
        _payload: &serde_json::Value,
    ) -> Option<lash_core::ExecutableGeneration> {
        None
    }

    fn check_settings(
        &self,
        _payload: &serde_json::Value,
        settings: Option<&lash_core::EngineSettings>,
    ) -> Result<(), lash_core::PluginError> {
        match settings {
            None => Ok(()),
            Some(_) => Err(lash_core::PluginError::Registration(format!(
                "process engine `{MCP_EVENTS_ENGINE}` takes no settings"
            ))),
        }
    }

    fn default_settings(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<Option<lash_core::EngineSettings>, lash_core::PluginError> {
        Ok(None)
    }

    fn advance(
        &self,
        state: EngineState,
        event: EngineEvent,
    ) -> Result<(EngineState, EngineAction), ProcessInfraError> {
        let (subscription, action) = Self::transition(decode_state(&state)?, event)?;
        Ok((encode_state(&subscription)?, action))
    }

    /// A workflow trigger's definition and environment, held for as long as
    /// the subscription exists (ADR 0113).
    fn start_artifacts(
        &self,
        payload: &serde_json::Value,
    ) -> Result<Vec<lash_core::ArtifactName>, lash_core::PluginError> {
        let spec: McpSubscriptionSpec =
            serde_json::from_value(payload.clone()).map_err(|error| {
                lash_core::PluginError::Session(format!(
                    "a subscription's payload does not decode: {error}"
                ))
            })?;
        Ok(match spec.target {
            McpReactionTarget::Workflow(workflow) => vec![
                lash_core::ArtifactName {
                    store: lash_core::ArtifactStoreId::ProcessDefinition,
                    artifact_ref: workflow.definition.id.to_string(),
                },
                lash_core::ArtifactName {
                    store: lash_core::ArtifactStoreId::ProcessEnv,
                    artifact_ref: workflow.launch.env.as_str().to_owned(),
                },
            ],
            McpReactionTarget::Session { .. } | McpReactionTarget::Isolated { .. } => Vec::new(),
        })
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &lash_core::ResolvedArtifactCleanup,
    ) -> Result<(), lash_core::ArtifactStoreError> {
        Ok(())
    }

    /// Never called: the engine names nothing under a store of its own.
    async fn acquire_engine_artifact(
        &self,
        _claim: &lash_core::ReferrerClaim,
        artifact_ref: &str,
    ) -> Result<(), lash_core::PluginError> {
        Err(lash_core::PluginError::Session(format!(
            "process engine `{MCP_EVENTS_ENGINE}` stores no artifact `{artifact_ref}`"
        )))
    }

    async fn performed_effects(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<lash_core::PerformedEffects, lash_core::ProcessEnvRefusal> {
        Ok(lash_core::PerformedEffects::new())
    }

    async fn check_args(
        &self,
        _signature: &lash_core::ProcessSignature,
        _args: &serde_json::Map<String, serde_json::Value>,
        _mode: lash_core::ArgsMode,
    ) -> Result<(), lash_core::ArgsMismatch> {
        Err(lash_core::ArgsMismatch::UnsupportedSignature {
            engine_kind: MCP_EVENTS_ENGINE.to_owned().into(),
        })
    }

    async fn resolve(
        &self,
        _reference: &lash_core::ProcessDefinitionRef,
    ) -> Result<lash_core::ProcessDefinitionResolution, lash_core::ProcessDefinitionError> {
        Ok(lash_core::ProcessDefinitionResolution::new(
            lash_core::ProcessSignature::Unknown,
        ))
    }
}

/// The status a subscription's state projects.
pub(crate) struct SubscriptionStatusProjection;

impl lash_core::EngineStatusProjection for SubscriptionStatusProjection {
    fn status(&self, state: &EngineState) -> Option<serde_json::Value> {
        let subscription: Subscription = serde_json::from_slice(&state.bytes).ok()?;
        serde_json::to_value(subscription.status).ok()
    }
}

/// The registration of the subscription engine, with its step bodies and
/// status projection.
pub(crate) fn registration(
    steps: Arc<dyn lash_core::EngineSteps>,
) -> lash_core::ProcessEngineRegistration {
    lash_core::ProcessEngineRegistration::accepting(Arc::new(SubscriptionEngine))
        .with_engine_steps(steps)
        .with_status_projection(Arc::new(SubscriptionStatusProjection))
}
