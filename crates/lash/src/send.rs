//! One way in: [`LashSession::send`](crate::LashSession::send) accepts an
//! input durably and asks the session's engine to execute it (FIG-3600).
//!
//! The turn no longer runs in the caller's future. The caller holds a
//! [`SendHandle`] and reads what happened from what was recorded: the input's
//! run, then that run's terminal or its park. The engine is used only as a
//! wake barrier and to surface a shift it refused.
//!
//! Polling any one of a handle's [`events`](SendHandle::events),
//! [`outcome`](SendHandle::outcome) or [`output`](SendHandle::output) is
//! enough to follow the engine to completion. The handle remembers its
//! answer, so a later call answers the same retained outcome.

use lash_core::ActorContext;
use lash_protocol_types::core_support::RunIdCoreSupport;
mod batch;
mod cancel;
mod follow;
mod resolve;

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures_util::Stream;
use futures_util::future::BoxFuture;
use lash_core::facade_support::DurableSessionOps;
use lash_core::runtime::{TurnInputAcceptanceReceipt, TurnInputIngress};
use lash_core::{InputId, LiveReplayStore, SessionId, TurnId};
use lash_protocol_types::sync::MutexExt;
use tokio::sync::mpsc;

use crate::durable_session::DurableSession;
use crate::error::{EmbedError, Result, SendError};
use crate::support::{LashSession, TurnActivity, TurnActivitySink, TurnInput, TurnOutcome};
use crate::turn::TurnOutput;
use lash_core::{GenerationOptions, LlmProfileKey, ReasoningSelection, RunSpec};

use lash_core::facade_support::{
    TurnCancelMode, TurnCancelReceipt, TurnCancelUndeliveredInputPolicy,
};
use lash_core::store::StallReason;

pub use batch::{BatchInput, SendBatchBuilder};
use follow::{Subject, Tap};

/// The session a send, a handle or a cancel is bound to.
#[derive(Clone)]
pub(crate) enum SendTarget {
    /// An open session: outcome reads leave its resident runtime alone.
    Live(LashSession),
    /// A Durable Session: no runtime to refresh.
    Durable(DurableSession),
}

/// What a send reads and writes through: the session's store, its queue
/// operations, the effect host terminal reads
/// and cancels go through, and the live replay events come from.
#[derive(Clone)]
pub(crate) struct SendParts {
    pub(crate) observer_pacing: Arc<crate::ObserverPacing>,
    pub(crate) session_id: SessionId,
    pub(crate) store: lash_core::store::SessionStore,
    pub(crate) ops: DurableSessionOps,
    pub(crate) effect_host: ActorContext,
    pub(crate) live_replay_store: Arc<dyn LiveReplayStore>,
    /// The models a spec's model key is judged against before the input is
    /// accepted.
    pub(crate) models: Arc<dyn lash_core::LlmProfiles>,
}

/// The parts an outcome follower needs. Session state has its own read API.
pub(crate) struct SendContext {
    pub(crate) parts: SendParts,
    live: bool,
}

impl SendTarget {
    pub(crate) fn observer_pacing(&self) -> crate::ObserverPacing {
        match self {
            Self::Live(session) => *session.binding.observer_pacing,
            Self::Durable(session) => *session.observer_pacing,
        }
    }

    pub(crate) async fn context(&self) -> Result<SendContext> {
        match self {
            Self::Live(session) => Ok(SendContext {
                parts: session.durable().send_parts().await?,
                live: true,
            }),
            Self::Durable(durable) => Ok(SendContext {
                parts: durable.send_parts().await?,
                live: false,
            }),
        }
    }

    pub(crate) fn session_id(&self) -> SessionId {
        match self {
            Self::Live(session) => session.session_id(),
            Self::Durable(durable) => durable.session_id().clone(),
        }
    }

    /// The session's store whether or not lash has created the session: a
    /// forsake writes through it, and a send or probe of a session not yet
    /// created reads the id's tombstone from it. It reads nothing.
    fn uncreated_store(&self) -> Result<lash_core::store::SessionStore> {
        match self {
            Self::Live(session) => session.durable().uncreated_store(),
            Self::Durable(durable) => durable.uncreated_store(),
        }
    }

    /// The core's trace runtime, which captures the caller's context and
    /// opens a send's producer span.
    pub(crate) fn tracing(&self) -> lash_core::facade_support::TraceRuntime {
        match self {
            Self::Live(session) => session.binding.tracing(),
            Self::Durable(durable) => durable.tracing().clone(),
        }
    }

    /// Open the producer span of a `kind` host call making `call`'s trace
    /// choices, which knows `ids` now. Synchronous: a send opens it before
    /// its first await.
    pub(crate) fn open_producer(
        &self,
        kind: lash_core::ProducerKind,
        call: &lash_core::TraceCall,
        ids: lash_core::TraceIds,
    ) -> lash_core::ProducerSpan {
        self.tracing().open_producer(kind, call, ids)
    }

    /// The live replay position now: a cursor taken before an acceptance
    /// sees everything the acceptance's shift publishes.
    fn current_cursor(&self) -> lash_core::SessionCursor {
        match self {
            Self::Live(session) => {
                let observation = session.runtime.observe();
                session.runtime.live_replay_store.current_cursor(
                    &SessionId::from(observation.session_id()),
                    observation.session_revision(),
                )
            }
            Self::Durable(durable) => durable
                .live_replay_store()
                .current_cursor(durable.session_id(), lash_core::SessionRevision::new(0)),
        }
    }
}

/// Refuse a spec whose model selection cannot run before the input is
/// accepted: a model key this host's models do not register, or reasoning
/// the capability of the model the run would run refuses, judged as the
/// run will judge it against the session's recorded config (FIG-4531).
/// Nothing is recorded here: the run mints the key's binding once, when it
/// records its shape. A spec that names neither keeps the session's recorded
/// selection and is not judged.
async fn refuse_unservable_selection(context: &SendContext, spec: &RunSpec) -> Result<()> {
    let overrides = &spec.overrides;
    if overrides.model.is_none() && overrides.reasoning.is_none() {
        return Ok(());
    }
    let minted = overrides
        .model
        .as_ref()
        .map(|key| context.parts.models.snapshot(key))
        .transpose()
        .map_err(|error| {
            EmbedError::Runtime(lash_core::RuntimeError::new(
                lash_core::RuntimeErrorCode::LlmProfileUnknown,
                format!("send refused: {error}"),
            ))
        })?;
    let selected = match (minted, overrides.reasoning.clone()) {
        (Some(model), Some(reasoning)) => {
            Some(lash_core::LlmProfileConfig::new(model).with_reasoning(reasoning))
        }
        (minted, reasoning) => {
            let recorded = context
                .parts
                .store
                .load_session_head_meta()
                .await?
                .ok_or_else(|| EmbedError::UnknownSession {
                    session_id: context.parts.session_id.clone(),
                })?
                .config
                .model;
            match (minted, recorded) {
                (Some(model), recorded) => Some(lash_core::LlmProfileConfig {
                    model,
                    reasoning: recorded.map(|model| model.reasoning).unwrap_or_default(),
                }),
                (None, Some(recorded)) => Some(match reasoning {
                    Some(reasoning) => recorded.with_reasoning(reasoning),
                    None => recorded,
                }),
                // A session that records no model refuses the run when its
                // shape resolves.
                (None, None) => None,
            }
        }
    };
    let Some(selected) = selected else {
        return Ok(());
    };
    selected.validate_reasoning().map_err(|error| {
        EmbedError::Runtime(lash_core::RuntimeError::new(
            lash_core::RuntimeErrorCode::ReasoningRefused,
            format!("send refused: {error}"),
        ))
    })
}

// ---------------------------------------------------------------------------
// SendBuilder
// ---------------------------------------------------------------------------

/// Builder for one [`send`](crate::LashSession::send).
///
/// Awaiting it commits the acceptance and asks the engine for a shift; it
/// yields a [`SendHandle`]. [`output`](Self::output) is the one-call form.
///
/// The shape the input runs under is its [`RunSpec`]: the default is the
/// session config as it stands when the input's run starts, after every
/// config command queued ahead of that boundary. [`run`](Self::run) and the
/// one-shot setters shape this input's run only; nothing they set reaches
/// the session config. The run resolves its shape once, and its first
/// phase records it, so a resume or a cancel of the run executes the same
/// shape. Inputs whose specs differ never share a turn.
#[must_use = "a SendBuilder does nothing until awaited"]
pub struct SendBuilder {
    pub(crate) target: SendTarget,
    pub(crate) input: TurnInput,
    pub(crate) id: Option<TurnId>,
    pub(crate) ingress: TurnInputIngress,
    pub(crate) run_spec: RunSpec,
    pub(crate) pin: bool,
    pub(crate) trace: lash_core::TraceCall,
    /// The first run options that did not serialize, refused at acceptance.
    pub(crate) options_error: Option<lash_core::RunOptionsEncodeError>,
}

impl SendBuilder {
    pub(crate) fn new(target: SendTarget, input: TurnInput) -> Self {
        Self {
            target,
            input,
            id: None,
            ingress: TurnInputIngress::NextTurn,
            run_spec: RunSpec::default(),
            pin: false,
            trace: lash_core::TraceCall::default(),
            options_error: None,
        }
    }

    /// Open this send's `lash.send` producer span under `context`, the
    /// trace context of whatever caused it. An explicit context wins over
    /// the caller's ambient one, which is then never consulted.
    ///
    /// The context is telemetry beside the submission, not part of it: the
    /// input retains the producer as its cause, the first acceptance of an
    /// [`id`](Self::id) retains it, and a retry under another context is
    /// the same submission and keeps the first one. The turn the input
    /// heads joins its trace.
    pub fn trace_context(mut self, context: lash_core::TraceCarrier) -> Self {
        self.trace.caller = lash_core::TraceCaller::Explicit(Some(context));
        self
    }

    /// Open this send's producer span with no caller, detached from the
    /// ambient context: a new trace root.
    pub fn trace_context_none(mut self) -> Self {
        self.trace.caller = lash_core::TraceCaller::Explicit(None);
        self
    }

    /// Snapshot the caller's current trace context now, through the core's
    /// telemetry adapter, instead of when the send is first polled. Use it
    /// when the builder is awaited somewhere the caller's context no longer
    /// is, such as a spawned task. With no adapter installed there is
    /// nothing to capture and the producer is a root.
    pub fn capture_trace_context(mut self) -> Self {
        self.trace.caller =
            lash_core::TraceCaller::Explicit(self.target.tracing().identity().ambient());
        self
    }

    /// How this send's producer span relates to its caller: its child (the
    /// default), or, with [`TraceLinkMode::Link`](lash_core::TraceLinkMode::Link),
    /// a new trace root that links it. The work the send admits takes the
    /// producer as its parent either way.
    pub fn trace_link_mode(mut self, mode: lash_core::TraceLinkMode) -> Self {
        self.trace.link_mode = mode;
        self
    }

    /// Pin this input in the transaction that accepts it: the state its
    /// run commits is retained through every collection, from before the
    /// run can start, until it is unpinned or the session is deleted.
    /// [`Target::Input`](lash_core::Target::Input) of
    /// [`SendHandle::input_id`] names it afterwards, for
    /// [`fork_at`](crate::LashCore::fork_at) and
    /// [`unpin`](crate::LashSession::unpin).
    pub fn pin(mut self) -> Self {
        self.pin = true;
        self
    }

    /// The host's id for this input. It is the idempotency key **and** the
    /// run the input starts: it is stored verbatim as the row's source key,
    /// so the input's run is `TurnId(id)`. Under the default drain every
    /// next-turn input is its own run; a drain policy that takes several
    /// inputs into one run executes them under the first one's run.
    ///
    /// A retry validates the original submission digest, including after
    /// settlement. Identical content returns the original acceptance; changed
    /// content is refused.
    ///
    /// This is the recovery for a lost response: a host that does not know
    /// whether a send was accepted sends the same id with the same content
    /// again. That send accepts the input exactly once, however many times it
    /// is repeated, and its handle answers the one run. Reading
    /// [`attach_id`](crate::LashSession::attach_id) instead answers only what
    /// lash holds now: [`SendOutcome::Unknown`] when it holds nothing.
    ///
    /// An id the host [forsook](crate::LashSession::forsake) is refused
    /// before acceptance, whatever the content, including when the host
    /// forsook it before lash created the session.
    ///
    /// Exactly-once acceptance does not imply exactly-once external actions:
    /// uncommitted work can repeat. See the
    /// [crash contract](https://github.com/Ascending-AI/lash/blob/main/README.md#what-survives-a-crash).
    pub fn id(mut self, id: TurnId) -> Self {
        self.id = Some(id);
        self
    }

    /// Where the input applies: [`TurnInputIngress::NextTurn`] (the default)
    /// or an active turn's checkpoint.
    ///
    /// An input steered into a running turn joins that turn's recorded
    /// shape: leave its spec default to inherit it. An explicit spec that
    /// differs from the running turn's is refused before acceptance
    /// ([`RuntimeErrorCode::RunSpecMismatch`](lash_core::RuntimeErrorCode::RunSpecMismatch)).
    pub fn ingress(mut self, ingress: TurnInputIngress) -> Self {
        self.ingress = ingress;
        self
    }

    /// The whole spec this input runs under, replacing anything set before.
    /// The spec is part of the input's submission: a retry under the same
    /// [`id`](Self::id) must carry the same spec.
    pub fn run(mut self, spec: RunSpec) -> Self {
        self.run_spec = spec;
        self
    }

    /// The model this input's run executes on, by the host's key. The run
    /// mints the key's binding once, when it records its shape, and runs the
    /// session's reasoning unless [`reasoning`](Self::reasoning) is set too.
    /// A key the host's models do not register is refused before the input
    /// is accepted.
    pub fn model(mut self, key: impl Into<LlmProfileKey>) -> Self {
        self.run_spec.overrides.model = Some(key.into());
        self
    }

    /// The reasoning this input's run executes its model with.
    pub fn reasoning(mut self, reasoning: ReasoningSelection) -> Self {
        self.run_spec.overrides.reasoning = Some(reasoning);
        self
    }

    /// The generation options this input's run executes with.
    pub fn generation(mut self, generation: GenerationOptions) -> Self {
        self.run_spec.overrides.generation = Some(generation);
        self
    }

    /// The typed run options this input's run states for one config owner,
    /// protocol or host plugin alike, under the [`RunOptionsKey`](lash_core::RunOptionsKey)
    /// that owner's plugin crate exports (FIG-6021). The owner applies them
    /// over the namespace the run's snapshot records, field by field as it
    /// defines; nothing else merges them. A run states them for this input
    /// only: they never reach the session config, and a later send that
    /// states none runs under the session's own namespace.
    ///
    /// Setting them again for the same owner replaces them. Options are
    /// part of the spec, so they join the input's submission digest, split
    /// it from inputs under other specs, and an input steered into a running
    /// turn must leave its spec default to join that turn. An owner the
    /// run's snapshot records no namespace for, or options its type does not
    /// admit, refuse the run's shape typed; options that do not serialize
    /// refuse the send at acceptance ([`EmbedError::RunOptionsEncode`](crate::EmbedError::RunOptionsEncode)).
    pub fn options<T: serde::Serialize>(
        mut self,
        key: lash_core::RunOptionsKey<T>,
        options: T,
    ) -> Self {
        if let Err(error) = self.run_spec.overrides.owners.set(key, options) {
            self.options_error.get_or_insert(error);
        }
        self
    }

    /// Accept, then wait for the settled turn.
    pub async fn output(self) -> Result<TurnOutput> {
        self.await?.output().await
    }

    /// Accept, forward the run's live activity to `sink`, and answer its
    /// [`SendHandle::outcome_into`].
    pub async fn outcome_into(self, sink: &dyn TurnActivitySink) -> Result<SendOutcome> {
        self.await?.outcome_into(sink).await
    }

    /// Accept, forward the run's live activity to `sink`, then return the
    /// committed outcome ([`SendHandle::output_into`]).
    pub async fn output_into(self, sink: &dyn TurnActivitySink) -> Result<TurnOutcome> {
        self.await?.output_into(sink).await
    }

    async fn accept(self) -> Result<SendHandle> {
        let Self {
            target,
            mut input,
            id,
            ingress,
            run_spec,
            pin,
            trace,
            options_error,
        } = self;
        if let Some(error) = options_error {
            return Err(error.into());
        }
        // The host id names the run; the shift executes the run's turns
        // under it, so the input carries no turn id of its own.
        let host_id = id.or_else(|| input.trace_turn_id.take());
        input.trace_turn_id = None;
        // The producer opens once, here, on the first poll and before the
        // first await, under the caller's context: nothing later changes the
        // edge. The input retains it as its cause. Its ids are what is known
        // before admission: the session, and the run a host named.
        let mut producer = target.open_producer(
            lash_core::ProducerKind::Send,
            &trace,
            lash_core::TraceIds {
                session: Some(target.session_id()),
                turn: host_id.clone(),
                ..Default::default()
            },
        );
        let parent = producer.parent();
        let trace_cause = lash_core::TraceCause::from(parent.clone());
        let accepted = async {
            // An input sent without a host id gets a fresh id, so its row is
            // keyed and its run named like any other.
            let id = host_id.unwrap_or_else(crate::turn::fresh_turn_id);
            let context = send_context(&target, &[id.as_str()]).await?;
            let cursor = target.current_cursor();
            // A retained submission is answered by the store's digest
            // verdict, including a changed-content conflict. Only a new key
            // consults the host's current model selection (FIG-5762).
            if (run_spec.overrides.model.is_some() || run_spec.overrides.reasoning.is_some())
                && context
                    .parts
                    .store
                    .turn_input_submission_digest(id.as_str())
                    .await?
                    .is_none()
            {
                refuse_unservable_selection(&context, &run_spec).await?;
            }
            let enqueued = context
                .parts
                .ops
                .enqueue_turn_inputs(
                    &context.parts.store,
                    vec![(input, Some(id.to_string()))],
                    ingress,
                    run_spec,
                    pin,
                    parent,
                )
                .await?
                .pop()
                .ok_or_else(|| {
                    EmbedError::Runtime(lash_core::RuntimeError::new(
                        lash_core::RuntimeErrorCode::StoreCommitFailed,
                        "a batch of one admitted no pending turn input",
                    ))
                })?;
            Ok::<_, EmbedError>((enqueued, id, cursor))
        }
        .await;
        let (enqueued, id, cursor) = match accepted {
            Ok(accepted) => accepted,
            Err(error) => {
                producer.settle(crate::tracing::refused(&error));
                return Err(error);
            }
        };
        // The unique local carrier is retained only by this acceptance's
        // first SQL writer. A retry returns the original cause instead.
        producer.accepted(enqueued.input_id.clone());
        producer.settle(if enqueued.trace_cause == trace_cause {
            lash_core::ProducerOutcome::Selected
        } else {
            lash_core::ProducerOutcome::Reused
        });
        let receipt = TurnInputAcceptanceReceipt::from(&enqueued);
        Ok(SendHandle {
            target,
            receipt,
            id: Some(id),
            cursor,
            shared: Arc::new(HandleShared::pending()),
        })
    }
}

impl std::future::IntoFuture for SendBuilder {
    type Output = Result<SendHandle>;
    type IntoFuture = BoxFuture<'static, Result<SendHandle>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.accept())
    }
}

// ---------------------------------------------------------------------------
// Outcomes
// ---------------------------------------------------------------------------

/// The recorded answer to a send. Each variant carries only its own data.
///
/// A run's terminal refusal is a variant, never an `Err`: a handle's
/// `outcome()` answers `Err` when it could not read the answer, which asking
/// again retries, or while the session carries a fault an operator must
/// clear (ADR 0109 §9).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum SendOutcome {
    /// An explicitly completed host task, read from its operation receipt.
    #[non_exhaustive]
    OperationSettled {
        run: crate::RunId,
        outcome: Box<lash_core::runtime::PluginOperationCommandOutcome>,
        gaps: Vec<lash_core::facade_support::LiveReplayGap>,
    },
    #[non_exhaustive]
    Settled {
        run: crate::RunId,
        output: Box<TurnOutput>,
        gaps: Vec<lash_core::facade_support::LiveReplayGap>,
    },
    #[non_exhaustive]
    Parked {
        parked: ParkedTurn,
        gaps: Vec<lash_core::facade_support::LiveReplayGap>,
    },
    #[non_exhaustive]
    Stalled {
        stalled: StalledDelivery,
        gaps: Vec<lash_core::facade_support::LiveReplayGap>,
    },
    /// The run ended with a typed refusal no retry could change, and no turn
    /// of it committed (ADR 0069 §6): the run's recorded terminal answer. A
    /// shift the engine refused before any run took the input answers its
    /// refusal the same way, with no `run`.
    #[non_exhaustive]
    Refused {
        run: Option<crate::RunId>,
        refusal: Box<lash_core::RuntimeError>,
        gaps: Vec<lash_core::facade_support::LiveReplayGap>,
    },
    /// The input was withdrawn before any run took it. Its withdrawal stays
    /// on record, so a send under the same id answers this withdrawal and
    /// runs nothing.
    #[non_exhaustive]
    Withdrawn {
        gaps: Vec<lash_core::facade_support::LiveReplayGap>,
    },
    /// Lash holds no record of the input. This is provisional: the input
    /// may arrive later, so a host must keep it pending and probe again or
    /// re-send the same id and content. It is never a final refusal.
    /// Its session is absent or deleted,
    /// it was never accepted, or it was withdrawn and
    /// [`vacuum`](lash_core::store::StoreMaintenance::vacuum)
    /// has since reclaimed the withdrawal. A send under the same id is
    /// accepted as new when the session is live. A host that lost a send's
    /// response re-sends the same id with the same content rather than reading this answer
    /// ([`SendBuilder::id`]).
    ///
    /// A host that must end the wait without a timer forsakes the id
    /// ([`LashSession::forsake`](crate::LashSession::forsake)), even before
    /// lash has created the session: that answers the input if a send of it
    /// was accepted after all, and otherwise records the id's tombstone,
    /// after which no send of the id is ever accepted and this probe answers
    /// [`Forsaken`](Self::Forsaken).
    #[non_exhaustive]
    Unknown {
        gaps: Vec<lash_core::facade_support::LiveReplayGap>,
    },
    /// The host forsook the id before lash accepted any input under it
    /// ([`LashSession::forsake`](crate::LashSession::forsake)). This is final:
    /// the tombstone stays on record until the session is deleted, and a
    /// send under the id is refused
    /// ([`RuntimeErrorCode::TurnInputForsaken`](lash_core::RuntimeErrorCode::TurnInputForsaken))
    /// and runs nothing.
    #[non_exhaustive]
    Forsaken {
        gaps: Vec<lash_core::facade_support::LiveReplayGap>,
    },
}

/// How an input's run stands once it stopped moving.
///
/// Parked is not terminal: the run holds its work until an operator
/// redrives its session ([`DurableSession::redrive`](crate::DurableSession::redrive))
/// or cancels it, and a host re-awaits it through
/// [`LashSession::run`](crate::LashSession::run).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub enum TurnStatus {
    Answered,
    Failed {
        /// Cause recorded in the terminal. Other failure categories and
        /// admission refusals retain their own typed evidence.
        cause: Option<crate::TurnErrorCause>,
    },
    Cancelled,
    Parked(ParkedTurn),
    /// The input was accepted, but its delivery to the engine stalled (ADR
    /// 0109 §3): no run took it, and none will until an operator re-arms
    /// its obligation. Not terminal: the input stays durable, and a host
    /// re-awaits it once re-armed.
    Stalled(StalledDelivery),
    /// No input is recorded yet. This is provisional, not a final refusal:
    /// see [`SendOutcome::Unknown`].
    Unknown,
    /// The host forsook the id before any input was accepted under it, and
    /// none ever will be: see [`SendOutcome::Forsaken`].
    Forsaken,
}

/// An accepted input the engine was never handed: its ingress obligation
/// stalled.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StalledDelivery {
    pub session_id: SessionId,
    pub input_id: InputId,
    pub reason: StallReason,
    /// Delivery attempts made before it stalled.
    pub attempts: u32,
    /// The last delivery failure, as the relay recorded it: its typed code
    /// beside its message.
    pub last_error: Option<lash_core::store::DeliveryError>,
    /// When it stalled, in milliseconds since the Unix epoch.
    pub stalled_at_ms: u64,
}

/// A run whose session parked with the run open (FIG-5367): durable and
/// non-terminal. The run resumes once an operator repairs the cause and
/// redrives the session ([`DurableSession::redrive`](crate::DurableSession::redrive)).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ParkedTurn {
    pub session_id: SessionId,
    pub run: crate::RunId,
    /// Why the session parked, as its actor recorded it: a recorded model
    /// no deployment serves names its key
    /// ([`SessionParkReason::profile_key`](crate::SessionParkReason::profile_key)).
    pub reason: crate::SessionParkReason,
}

impl SendOutcome {
    pub fn status(&self) -> TurnStatus {
        match self {
            Self::OperationSettled { outcome, .. } => match outcome.as_ref() {
                lash_core::runtime::PluginOperationCommandOutcome::Completed { .. } => {
                    TurnStatus::Answered
                }
                lash_core::runtime::PluginOperationCommandOutcome::Cancelled => {
                    TurnStatus::Cancelled
                }
                lash_core::runtime::PluginOperationCommandOutcome::Failed { .. }
                | lash_core::runtime::PluginOperationCommandOutcome::Refused { .. } => {
                    TurnStatus::Failed { cause: None }
                }
            },
            Self::Settled { output, .. } => output.status(),
            Self::Parked { parked, .. } => TurnStatus::Parked(parked.clone()),
            Self::Stalled { stalled, .. } => TurnStatus::Stalled(stalled.clone()),
            Self::Refused { .. } => TurnStatus::Failed { cause: None },
            Self::Withdrawn { .. } => TurnStatus::Cancelled,
            Self::Unknown { .. } => TurnStatus::Unknown,
            Self::Forsaken { .. } => TurnStatus::Forsaken,
        }
    }

    pub fn run(&self) -> Option<&crate::RunId> {
        match self {
            Self::Settled { run, .. } | Self::OperationSettled { run, .. } => Some(run),
            Self::Parked { parked, .. } => Some(&parked.run),
            Self::Refused { run, .. } => run.as_ref(),
            Self::Stalled { .. }
            | Self::Withdrawn { .. }
            | Self::Unknown { .. }
            | Self::Forsaken { .. } => None,
        }
    }

    pub fn output(&self) -> Option<&TurnOutput> {
        match self {
            Self::Settled { output, .. } => Some(output),
            Self::OperationSettled { .. }
            | Self::Parked { .. }
            | Self::Stalled { .. }
            | Self::Refused { .. }
            | Self::Withdrawn { .. }
            | Self::Unknown { .. }
            | Self::Forsaken { .. } => None,
        }
    }

    /// The typed refusal a [`Refused`](Self::Refused) run ended with.
    pub fn refusal(&self) -> Option<&lash_core::RuntimeError> {
        match self {
            Self::Refused { refusal, .. } => Some(refusal),
            Self::OperationSettled { .. }
            | Self::Settled { .. }
            | Self::Parked { .. }
            | Self::Stalled { .. }
            | Self::Withdrawn { .. }
            | Self::Unknown { .. }
            | Self::Forsaken { .. } => None,
        }
    }

    pub fn into_output(self) -> Option<TurnOutput> {
        match self {
            Self::Settled { output, .. } => Some(*output),
            Self::OperationSettled { .. }
            | Self::Parked { .. }
            | Self::Stalled { .. }
            | Self::Refused { .. }
            | Self::Withdrawn { .. }
            | Self::Unknown { .. }
            | Self::Forsaken { .. } => None,
        }
    }

    /// Gaps in the follower's live activity. The settled report remains
    /// authoritative even when the follower missed activities.
    pub fn gaps(&self) -> &[lash_core::facade_support::LiveReplayGap] {
        match self {
            Self::OperationSettled { gaps, .. }
            | Self::Settled { gaps, .. }
            | Self::Parked { gaps, .. }
            | Self::Stalled { gaps, .. }
            | Self::Refused { gaps, .. }
            | Self::Withdrawn { gaps }
            | Self::Unknown { gaps }
            | Self::Forsaken { gaps } => gaps,
        }
    }

    pub(crate) fn gaps_mut(&mut self) -> &mut Vec<lash_core::facade_support::LiveReplayGap> {
        match self {
            Self::OperationSettled { gaps, .. }
            | Self::Settled { gaps, .. }
            | Self::Parked { gaps, .. }
            | Self::Stalled { gaps, .. }
            | Self::Refused { gaps, .. }
            | Self::Withdrawn { gaps }
            | Self::Unknown { gaps }
            | Self::Forsaken { gaps } => gaps,
        }
    }
}

impl TurnStatus {
    /// Classify a committed outcome, using the same rule as [`TurnStatus::of_outcome`].
    /// A frame switch answers its turn; a cancelled stop is cancelled and
    /// every other stop is failed.
    pub fn of_outcome(outcome: &TurnOutcome) -> Self {
        match outcome {
            TurnOutcome::Finished(_) | TurnOutcome::AgentFrameSwitch { .. } => TurnStatus::Answered,
            TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled { .. }) => {
                TurnStatus::Cancelled
            }
            TurnOutcome::Stopped(_) => TurnStatus::Failed {
                cause: outcome.error_cause().cloned(),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// SendHandle and RunHandle
// ---------------------------------------------------------------------------

/// What every call on one handle shares: the answer once it is known.
struct HandleShared {
    settled: Mutex<Option<SendOutcome>>,
}

impl HandleShared {
    fn pending() -> Self {
        Self {
            settled: Mutex::new(None),
        }
    }

    fn answer(&self) -> Option<SendOutcome> {
        self.settled.lock_recover().clone()
    }

    fn remember(&self, outcome: &SendOutcome) {
        // Absence is only this read's observation: a later send may still
        // admit the input, including while this handle stays alive.
        if matches!(outcome, SendOutcome::Unknown { .. }) {
            return;
        }
        let mut settled = self.settled.lock_recover();
        if settled.is_none() {
            *settled = Some(outcome.clone());
        }
    }
}

/// Follow `subject` for a handle: answer from the handle's memory when it
/// already knows, otherwise follow and remember.
async fn settle(
    target: &SendTarget,
    subject: &Subject,
    cursor: &lash_core::SessionCursor,
    shared: &HandleShared,
    mut tap: Tap<'_>,
) -> Result<SendOutcome> {
    if let Some(outcome) = shared.answer() {
        if let Some(output) = outcome.output() {
            for activity in &output.activities {
                tap.activity(activity).await;
            }
        }
        return Ok(outcome);
    }
    let outcome = match follow_subject(target, subject, cursor, &mut tap).await {
        Ok(outcome) => outcome,
        // An input has no retained acceptance once its session is gone, and
        // none before lash creates it, though a forsake may already have
        // closed its id. Store failures and standing faults remain failed
        // reads.
        Err(error) if absent_input_session(subject, &error) => match (subject, &error) {
            (Subject::Input(receipt), EmbedError::UnknownSession { .. })
                if target
                    .uncreated_store()?
                    .input_status(&receipt.input_id)
                    .await?
                    .forsaken =>
            {
                SendOutcome::Forsaken { gaps: Vec::new() }
            }
            _ => SendOutcome::Unknown { gaps: Vec::new() },
        },
        Err(error) => return Err(error),
    };
    shared.remember(&outcome);
    Ok(outcome)
}

fn absent_input_session(subject: &Subject, error: &EmbedError) -> bool {
    matches!(subject, Subject::Input(_))
        && matches!(
            error,
            EmbedError::UnknownSession { .. }
                | EmbedError::Store(
                    lash_core::StoreError::SessionDeleted { .. }
                        | lash_core::StoreError::SessionNotFound { .. }
                )
        )
}

async fn follow_subject(
    target: &SendTarget,
    subject: &Subject,
    cursor: &lash_core::SessionCursor,
    tap: &mut Tap<'_>,
) -> Result<SendOutcome> {
    let context = target.context().await?;
    let mut from = follow::Position::at(cursor.clone());
    let mut gaps = Vec::new();
    // A follow without a window answers; a pending one would go on from its
    // position.
    let outcome = loop {
        match Box::pin(follow::follow(&context, subject, from, tap, None)).await? {
            follow::Followed::Answered(mut outcome) => {
                gaps.append(outcome.gaps_mut());
                *outcome.gaps_mut() = gaps;
                break *outcome;
            }
            follow::Followed::Pending {
                position,
                gaps: mut met,
            } => {
                from = position;
                gaps.append(&mut met);
            }
        }
    };
    Ok(outcome)
}

/// An events stream fed by a follower on its own task.
fn spawn_events(
    target: SendTarget,
    subject: Subject,
    cursor: lash_core::SessionCursor,
    shared: Arc<HandleShared>,
) -> TurnEvents {
    let (tx, rx) = mpsc::channel(target.observer_pacing().send_channel.get());
    let task_tx = tx.clone();
    tokio::spawn(async move {
        if let Err(error) = settle(&target, &subject, &cursor, &shared, Tap::Channel(task_tx)).await
        {
            let _ = tx.send(Err(error)).await;
        }
    });
    TurnEvents {
        inner: Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        })),
    }
}

/// An accepted input: follow its live activity, and read how its run
/// answered.
///
/// Dropping a handle stops nothing.
pub struct SendHandle {
    target: SendTarget,
    receipt: TurnInputAcceptanceReceipt,
    id: Option<TurnId>,
    cursor: lash_core::SessionCursor,
    shared: Arc<HandleShared>,
}

impl SendHandle {
    pub fn input_id(&self) -> &InputId {
        &self.receipt.input_id
    }

    /// The receipt of the input's durable acceptance.
    pub fn receipt(&self) -> &TurnInputAcceptanceReceipt {
        &self.receipt
    }

    /// The host id, when the send set [`SendBuilder::id`].
    pub fn id(&self) -> Option<&TurnId> {
        self.id.as_ref()
    }

    /// The live-replay cursor taken before this handle's acceptance: the
    /// position its [`events`](Self::events) and [`outcome`](Self::outcome)
    /// follow from. A reader that journaled the acceptance replays from it
    /// deterministically rather than minting a fresh cursor on replay.
    pub fn cursor(&self) -> &lash_core::SessionCursor {
        &self.cursor
    }

    /// Live activity of the run that applies this input, from the cursor
    /// taken before acceptance. Each call subscribes afresh from that cursor.
    /// It ends once the input's run settles or parks; draining it alone
    /// executes the turn to its answer, which the handle then remembers.
    pub fn events(&self) -> TurnEvents {
        spawn_events(
            self.target.clone(),
            Subject::Input(self.receipt.clone()),
            self.cursor.clone(),
            Arc::clone(&self.shared),
        )
    }

    /// The input's run answer: resolves on a terminal, a park, or
    /// [`Unknown`](SendOutcome::Unknown) when no input is recorded yet.
    /// Unknown is provisional; observing it never caches a final answer.
    pub async fn outcome(self) -> Result<SendOutcome> {
        settle(
            &self.target,
            &Subject::Input(self.receipt.clone()),
            &self.cursor,
            &self.shared,
            Tap::Quiet,
        )
        .await
    }

    /// [`outcome`](Self::outcome) narrowed to a settled turn. A refused run
    /// answers its typed refusal as [`EmbedError::Runtime`]; a parked run, an
    /// input withdrawn before it ran, or one lash never accepted answers
    /// [`SendError::NotSettled`].
    pub async fn output(self) -> Result<TurnOutput> {
        let input_id = self.receipt.input_id.clone();
        collected_output(input_id, self.outcome().await?)
    }

    /// Forward the run's live activity to `sink` as it arrives, and answer
    /// [`outcome`](Self::outcome) with that activity collected in its
    /// output. The outcome's [`gaps`](SendOutcome::gaps) say where the
    /// forwarded activity is incomplete.
    pub async fn outcome_into(self, sink: &dyn TurnActivitySink) -> Result<SendOutcome> {
        settle(
            &self.target,
            &Subject::Input(self.receipt.clone()),
            &self.cursor,
            &self.shared,
            Tap::Sink(sink),
        )
        .await
    }

    /// [`outcome_into`](Self::outcome_into) narrowed to a settled turn's
    /// outcome, as [`output`](Self::output) narrows [`outcome`](Self::outcome).
    pub async fn output_into(self, sink: &dyn TurnActivitySink) -> Result<TurnOutcome> {
        let input_id = self.receipt.input_id.clone();
        settled_output(input_id, self.outcome_into(sink).await?)
    }

    /// Withdraw the input if it is still queued, or cooperatively cancel its
    /// running run.
    pub fn cancel(&self) -> CancelBuilder {
        CancelBuilder::for_input(self.target.clone(), self.receipt.clone())
    }

    /// Pin this input: the state the run that applies it commits is
    /// retained through every collection. It can be called before the run
    /// starts, while it runs or after it ended, any number of times; it
    /// never touches the turn. A merged or re-deferred input pins the run
    /// that actually applies it.
    pub async fn pin(&self) -> Result<()> {
        let context = self.target.context().await?;
        context
            .parts
            .store
            .pin(&lash_core::Target::Input(self.receipt.input_id.clone()))
            .await
            .map_err(EmbedError::Store)
    }

    /// The run this input is bound to, once a run has taken it: the run
    /// that executes it, which under a merging drain or a steer is not the
    /// input's own. `None` until then, and for a withdrawn input.
    pub async fn run(&self) -> Result<Option<crate::RunId>> {
        let context = self.target.context().await?;
        context
            .parts
            .store
            .run_binding(&self.receipt.input_id)
            .await
            .map(|run| run.map(crate::RunId::from_turn_id))
            .map_err(EmbedError::Store)
    }
}

/// A logical run, re-awaited by id: after a restart, a park verb, or from a
/// handle that only knows the host id.
pub struct RunHandle<Output = serde_json::Value, Error = lash_core::plugin::PluginOperationFailure>
{
    target: SendTarget,
    run: crate::RunId,
    cursor: lash_core::SessionCursor,
    shared: Arc<HandleShared>,
    operation_name: Option<&'static str>,
    decode_error: fn(
        lash_core::plugin::PluginOperationFailure,
    )
        -> std::result::Result<Error, Box<lash_core::plugin::PluginOperationFailure>>,
    output_type: std::marker::PhantomData<fn() -> Output>,
}

impl<Output, Error> Clone for RunHandle<Output, Error> {
    fn clone(&self) -> Self {
        Self {
            target: self.target.clone(),
            run: self.run.clone(),
            cursor: self.cursor.clone(),
            shared: self.shared.clone(),
            operation_name: self.operation_name,
            decode_error: self.decode_error,
            output_type: std::marker::PhantomData,
        }
    }
}

impl<Output, Error> RunHandle<Output, Error> {
    /// The logical Run identity, including for operation Runs.
    pub fn run(&self) -> &crate::RunId {
        &self.run
    }

    /// Request cancellation of this logical owner. Dropping a follower never cancels it.
    pub fn cancel(&self) -> CancelBuilder {
        CancelBuilder::new(self.target.clone(), CancelTarget::Run(self.run.clone()))
    }

    /// Live activity of the run from the moment this handle was made; no
    /// earlier activity is replayed.
    pub fn events(&self) -> TurnEvents {
        spawn_events(
            self.target.clone(),
            Subject::Run(self.run.turn_id().clone()),
            self.cursor.clone(),
            Arc::clone(&self.shared),
        )
    }

    /// How the run answers. A run still parked answers Parked again.
    pub async fn outcome(self) -> Result<SendOutcome> {
        settle(
            &self.target,
            &Subject::Run(self.run.turn_id().clone()),
            &self.cursor,
            &self.shared,
            Tap::Quiet,
        )
        .await
    }

    pub async fn output(self) -> Result<TurnOutput> {
        let run = self.run.clone();
        let outcome = self.outcome().await?;
        collected_output(InputId::from(&run), outcome)
    }

    async fn raw_result(
        self,
    ) -> Result<lash_core::facade_support::PluginOperationReceipt<serde_json::Value>> {
        let run = self.run.clone();
        let answered = self.outcome().await?;
        let status = answered.status();
        match answered {
            SendOutcome::OperationSettled { outcome, .. } => match *outcome {
                lash_core::runtime::PluginOperationCommandOutcome::Completed {
                    plugin_id,
                    output,
                    events,
                    pending_turn_inputs,
                } => Ok(lash_core::facade_support::PluginOperationReceipt {
                    output,
                    events: events
                        .into_iter()
                        .map(
                            |value| lash_execution_contracts::session_types::PluginOwned {
                                plugin_id: plugin_id.clone(),
                                value,
                            },
                        )
                        .collect(),
                    pending_turn_inputs,
                }),
                lash_core::runtime::PluginOperationCommandOutcome::Failed { failure } => {
                    Err(EmbedError::Control(
                        lash_core::facade_support::PluginOperationInvokeError::Failed(failure),
                    ))
                }
                lash_core::runtime::PluginOperationCommandOutcome::Refused { refusal } => {
                    Err(EmbedError::Runtime(refusal.into()))
                }
                lash_core::runtime::PluginOperationCommandOutcome::Cancelled => {
                    Err(EmbedError::from(SendError::NotSettled {
                        input_id: InputId::from(&run),
                        status,
                    }))
                }
            },
            SendOutcome::Refused { refusal, .. } => Err(EmbedError::Runtime(*refusal)),
            _ => Err(EmbedError::from(SendError::NotSettled {
                input_id: InputId::from(&run),
                status,
            })),
        }
    }
}

impl<Output: serde::de::DeserializeOwned, Error> RunHandle<Output, Error> {
    /// Read a task's receipt using the output and error codecs selected by
    /// `start_task::<Op>`. Unknown error envelopes remain intact. A raw task
    /// handle returns JSON and the original failure envelope.
    pub async fn result(
        self,
    ) -> std::result::Result<
        lash_core::facade_support::PluginOperationReceipt<Output>,
        crate::admin::PluginTaskResultError<Error>,
    > {
        let name = self.operation_name.unwrap_or("raw task");
        let decode_error = self.decode_error;
        let receipt = self.raw_result().await.map_err(|error| {
            use crate::admin::PluginTaskResultError;
            match error {
                EmbedError::Control(
                    lash_core::facade_support::PluginOperationInvokeError::Failed(failure),
                ) => match decode_error(*failure.clone()) {
                    Ok(error) => PluginTaskResultError::Failed { error, failure },
                    Err(failure) => PluginTaskResultError::Host(Box::new(EmbedError::Control(
                        lash_core::facade_support::PluginOperationInvokeError::Failed(failure),
                    ))),
                },
                error => PluginTaskResultError::Host(Box::new(error)),
            }
        })?;
        let output = serde_json::from_value(receipt.output).map_err(|error| {
            crate::admin::PluginTaskResultError::Host(Box::new(EmbedError::Plugin(
                lash_execution_contracts::plugin_error::PluginError::Invoke(format!(
                    "invalid {name} output: {error}"
                )),
            )))
        })?;
        Ok(lash_core::facade_support::PluginOperationReceipt {
            output,
            events: receipt.events,
            pending_turn_inputs: receipt.pending_turn_inputs,
        })
    }
}

impl RunHandle {
    pub(crate) fn typed<Op: lash_core::facade_support::PluginTask>(
        self,
    ) -> RunHandle<Op::Output, Op::Error> {
        RunHandle {
            target: self.target,
            run: self.run,
            cursor: self.cursor,
            shared: self.shared,
            operation_name: Some(Op::NAME),
            decode_error: Op::decode_error,
            output_type: std::marker::PhantomData,
        }
    }
}

/// A handle on `input_id`, accepted earlier; its cursor is the observation's
/// current position.
pub(crate) fn attach(target: SendTarget, input_id: InputId) -> SendHandle {
    let session_id = target.session_id();
    let cursor = target.current_cursor();
    SendHandle {
        target,
        receipt: TurnInputAcceptanceReceipt {
            input_id,
            session_id,
            source_key: None,
            ingress: TurnInputIngress::NextTurn,
        },
        id: None,
        cursor,
        shared: Arc::new(HandleShared::pending()),
    }
}

/// A handle on the input a send accepted under host id `id`: a keyed input's
/// id is derived from its session and key, so no read finds it. An id lash
/// holds no record of answers [`SendOutcome::Unknown`].
pub(crate) fn attach_id(target: SendTarget, id: TurnId) -> SendHandle {
    let input_id =
        lash_core::PendingTurnInputDraft::keyed_input_id(&target.session_id(), id.as_str());
    let mut handle = attach(target, input_id);
    handle.receipt.source_key = Some(id.to_string());
    handle.id = Some(id);
    handle
}

/// What a forsake of a host id found: the id's tombstone, or the input a
/// send accepted under it first.
#[non_exhaustive]
pub enum Forsaking {
    /// The id's tombstone is recorded, now or by an earlier forsake, whether
    /// or not lash has created the session yet: no send of the id is ever
    /// accepted, and a probe of it answers [`SendOutcome::Forsaken`].
    Forsaken,
    /// A send of the id was accepted before the forsake: nothing was
    /// written, and the handle follows that input, as
    /// [`LashSession::attach_id`](crate::LashSession::attach_id) does.
    Accepted(Box<SendHandle>),
}

/// Forsake host id `id` in `target`'s session, created or not: the store
/// decides under the session's write authority, which every admission of
/// the id takes too, whether an input holds the id or its tombstone is
/// written.
pub(crate) async fn forsake(target: SendTarget, id: TurnId) -> Result<Forsaking> {
    let store = target.uncreated_store()?;
    match store.forsake_turn_input(id.as_str()).await? {
        lash_core::store::TurnInputForsaking::Held { .. } => {
            Ok(Forsaking::Accepted(Box::new(attach_id(target, id))))
        }
        lash_core::store::TurnInputForsaking::Forsaken { .. } => Ok(Forsaking::Forsaken),
    }
}

/// The context a send of host ids `ids` accepts through. A session lash has
/// not created refuses the send as unknown, unless the host forsook one of
/// `ids` before the session existed: that send is refused as forsaken, as an
/// admission would refuse it. Either way nothing is created.
async fn send_context(target: &SendTarget, ids: &[&str]) -> Result<SendContext> {
    let session_id = match target.context().await {
        Err(EmbedError::UnknownSession { session_id }) => session_id,
        context => return context,
    };
    let store = target.uncreated_store()?;
    for id in ids {
        let input_id = lash_core::PendingTurnInputDraft::keyed_input_id(&session_id, id);
        if store.input_status(&input_id).await?.forsaken {
            return Err(EmbedError::Runtime(
                lash_core::StoreError::TurnInputForsaken {
                    session_id,
                    source_key: (*id).to_owned(),
                    input_id,
                }
                .runtime_error(),
            ));
        }
    }
    Err(EmbedError::UnknownSession { session_id })
}

/// A handle on `run`; its cursor is the observation's current position.
pub(crate) fn run(target: SendTarget, run: crate::RunId) -> RunHandle {
    let cursor = target.current_cursor();
    RunHandle {
        target,
        run,
        cursor,
        shared: Arc::new(HandleShared::pending()),
        operation_name: None,
        decode_error: Ok,
        output_type: std::marker::PhantomData,
    }
}

fn settled_output(input_id: InputId, outcome: SendOutcome) -> Result<TurnOutcome> {
    Ok(collected_output(input_id, outcome)?.outcome)
}

pub(crate) fn collected_output(input_id: InputId, outcome: SendOutcome) -> Result<TurnOutput> {
    match outcome {
        SendOutcome::Settled { output, .. } => Ok(*output),
        SendOutcome::Refused { refusal, .. } => Err(EmbedError::Runtime(*refusal)),
        outcome => Err(EmbedError::from(SendError::NotSettled {
            input_id,
            status: outcome.status(),
        })),
    }
}

// ---------------------------------------------------------------------------
// TurnEvents
// ---------------------------------------------------------------------------

/// The live activity of one run, as it is published on the session's
/// observation.
pub struct TurnEvents {
    pub(crate) inner: Pin<Box<dyn Stream<Item = Result<TurnActivity>> + Send>>,
}

impl TurnEvents {
    pub async fn next_activity(&mut self) -> Option<Result<TurnActivity>> {
        futures_util::StreamExt::next(self).await
    }
}

impl Stream for TurnEvents {
    type Item = Result<TurnActivity>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

// ---------------------------------------------------------------------------
// Cancel
// ---------------------------------------------------------------------------

/// What a [`cancel`](crate::LashSession::cancel) addresses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CancelTarget {
    /// A logical run, including the run a queued input will open. The same
    /// atomic request withdraws that input or cancels its admitted run.
    Run(crate::RunId),
}

/// Builder for one cancel (ADR 0039).
#[must_use = "a CancelBuilder does nothing until awaited"]
pub struct CancelBuilder {
    target: SendTarget,
    subject: Subject,
    request: cancel::CancelRequestSpec,
}

impl CancelBuilder {
    pub(crate) fn new(target: SendTarget, cancel: CancelTarget) -> Self {
        let CancelTarget::Run(run) = cancel;
        Self {
            target,
            subject: Subject::Run(run.turn_id().clone()),
            request: cancel::CancelRequestSpec::default(),
        }
    }

    fn for_input(target: SendTarget, receipt: TurnInputAcceptanceReceipt) -> Self {
        Self {
            target,
            subject: Subject::Input(receipt),
            request: cancel::CancelRequestSpec::default(),
        }
    }

    /// The cancel request's id; defaults to `cancel:run:{id}`.
    pub fn request_id(mut self, id: impl Into<String>) -> Self {
        self.request.request_id = Some(id.into());
        self
    }

    /// Opaque host data Lash records without interpreting it.
    pub fn origin(mut self, origin: impl Into<String>) -> Self {
        self.request.origin = Some(origin.into());
        self
    }

    pub fn reason(mut self, reason: impl Into<String>) -> Self {
        self.request.reason = Some(reason.into());
        self
    }

    pub fn mode(mut self, mode: TurnCancelMode) -> Self {
        self.request.mode = mode;
        self
    }

    pub fn undelivered(mut self, policy: TurnCancelUndeliveredInputPolicy) -> Self {
        self.request.undelivered = policy;
        self
    }

    async fn apply(self) -> Result<CancelReceipt> {
        let context = self.target.context().await?;
        cancel::apply(&context.parts, &self.subject, self.request).await
    }
}

impl std::future::IntoFuture for CancelBuilder {
    type Output = Result<CancelReceipt>;
    type IntoFuture = BoxFuture<'static, Result<CancelReceipt>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.apply())
    }
}

/// What a cancel did.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub enum CancelReceipt {
    /// The atomic request withdrew queued work before a run applied it.
    /// An input's send handle answers [`SendOutcome::Withdrawn`]. A plugin
    /// task's command is withdrawn only while no run entered it; the cancel
    /// of an entered task is [`Cancelled`](Self::Cancelled) (FIG-5343).
    Withdrawn {
        run: crate::RunId,
        /// The withdrawn input, or `None` for an operation task command.
        input: Option<InputId>,
    },
    /// The request addressed an open run's cancellation gate. The detail
    /// preserves the accepted request, repeats, timing escalations and policy
    /// conflicts. This records cancellation intent, not proof execution has
    /// stopped; await the run's outcome for its terminal.
    Cancelled {
        run: crate::RunId,
        receipt: Box<TurnCancelReceipt>,
    },
    /// No queued input or open run accepts this request: the id is unknown,
    /// revoked, already withdrawn, or its completion won the race.
    UnknownOrRevoked,
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core::facade_support::{TurnCancellationEvidence, TurnFinish, TurnStop};

    #[test]
    fn settled_outcome_status_uses_the_reports_existing_classification() {
        for (outcome, expected) in [
            (
                TurnOutcome::Finished(TurnFinish::AssistantMessage {
                    text: "answer".into(),
                }),
                TurnStatus::Answered,
            ),
            (
                TurnOutcome::AgentFrameSwitch {
                    frame_key: lash_protocol_types::FrameKey::from_caller_material("frame")
                        .expect("frame key"),
                    task: "next task".into(),
                    initial_nodes: Vec::new(),
                    next_run: lash_core::RunSpec::default()
                        .next_run()
                        .expect("the default spec encodes"),
                },
                TurnStatus::Answered,
            ),
            (
                TurnOutcome::Stopped(TurnStop::Cancelled {
                    evidence: TurnCancellationEvidence::internal("test"),
                }),
                TurnStatus::Cancelled,
            ),
            (
                TurnOutcome::Stopped(TurnStop::RuntimeError {
                    cause: crate::TurnErrorCause::new(crate::TurnErrorCode::RuntimeRefused),
                }),
                TurnStatus::Failed {
                    cause: Some(crate::TurnErrorCause::new(
                        crate::TurnErrorCode::RuntimeRefused,
                    )),
                },
            ),
            (
                TurnOutcome::Stopped(TurnStop::AgentFrameSwitchLimit),
                TurnStatus::Failed { cause: None },
            ),
        ] {
            assert_eq!(TurnStatus::of_outcome(&outcome), expected);
        }
    }
}
