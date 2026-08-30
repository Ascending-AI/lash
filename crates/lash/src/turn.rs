use lash_sansio::sync::MutexExt;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::support::*;
use futures_util::Stream;
use lash_core::facade_support::{
    RuntimeSessionStateFacadeOps, ScopedEffectControllerFacadeOps,
    SelectedQueuedWorkDrainError as CoreSelectedQueuedWorkDrainError, TurnContextFacadeOps,
};

pub use lash_core::{facade_support::AssistantOutput, facade_support::TurnIssue};

pub(crate) mod queued_drain;
mod selected_drain;

use lash_core::QueuedWorkClaimRefusal;
pub(crate) use queued_drain::{EmptyQueuedDrainReason, QueuedTurnDrain};
use selected_drain::{queued_turn_drain, selected_drain_outcome, selected_drain_refusal_cause};

/// How one distinct requested batch ID satisfied a successful selected drain.
///
/// Missing rows are idempotent success. A present row that cannot join the
/// exact composition causes a pre-execution refusal, not a satisfaction value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelectedQueuedWorkBatchSatisfaction {
    /// This invocation claimed and executed the durable row.
    ClaimedNow {
        /// Requested durable batch ID.
        batch_id: String,
    },
    /// No durable row remained, so the idempotent request was already done.
    AlreadySatisfied {
        /// Requested durable batch ID.
        batch_id: String,
    },
}

/// Successful result of an exact, host-selected queued-work drain.
///
/// Every distinct requested ID was either executed now or had no remaining
/// durable row. A present ID that cannot join the exact claim returns
/// [`EmbedError::SelectedQueuedWorkDrainRefused`]
/// before selected execution.
#[derive(Clone, Debug)]
pub struct SelectedQueuedWorkDrainOutcome<T> {
    /// Executed turn, absent only for a fully satisfied drain with no selected turn.
    pub turn: Option<T>,
    /// One entry per distinct requested ID, ordered by first occurrence.
    pub satisfied: Vec<SelectedQueuedWorkBatchSatisfaction>,
}

impl<T> SelectedQueuedWorkDrainOutcome<T> {
    /// Reports whether this successful drain settled every requested ID without
    /// executing a selected turn.
    ///
    /// Refusals are errors, so `true` means every distinct ID was satisfied
    /// without a selected turn (or the selection was empty), never unclaimable.
    pub fn settled_without_selected_turn(&self) -> bool {
        self.turn.is_none()
    }

    /// Reports whether this successful drain executed a newly claimed turn.
    ///
    /// `false` has the same fully-satisfied meaning as
    /// [`Self::settled_without_selected_turn`].
    pub fn executed_selected_turn(&self) -> bool {
        self.turn.is_some()
    }

    /// Returns the turn or panics with `message` when the successful drain was
    /// fully satisfied without one.
    #[track_caller]
    pub fn expect(self, message: &str) -> T {
        self.turn.expect(message)
    }
}

/// The two internal event sinks threaded through the turn-execution helpers.
///
/// `events` is the raw lower-level runtime event stream, reachable from app
/// code only via [`TurnBuilder::advanced`]; `turn_events` is the semantic
/// [`TurnActivity`] stream used by the primary builder API.
/// Bundling them keeps the internal turn fns to a single sink parameter.
#[derive(Clone, Copy, Default)]
pub(crate) struct TurnSinks<'a> {
    events: Option<&'a dyn EventSink>,
    turn_events: Option<&'a dyn TurnActivitySink>,
}

impl<'a> TurnSinks<'a> {
    pub(crate) fn turn(events: &'a dyn TurnActivitySink) -> Self {
        Self {
            events: None,
            turn_events: Some(events),
        }
    }

    pub(crate) fn session(events: &'a dyn EventSink) -> Self {
        Self {
            events: Some(events),
            turn_events: None,
        }
    }

    fn events(&self) -> Option<&'a dyn EventSink> {
        self.events
    }

    fn turn_events(&self) -> Option<&'a dyn TurnActivitySink> {
        self.turn_events
    }
}

/// Cancellation tokens of the turns currently executing through one opened
/// [`LashSession`](crate::LashSession) (shared by its clones).
/// [`LashSession::cancel_running_turns`](crate::LashSession::cancel_running_turns)
/// cancels them without the caller having to thread a token around.
#[derive(Clone, Default)]
pub(crate) struct TurnCancelRegistry {
    inner: Arc<StdMutex<TurnCancelRegistryInner>>,
}

#[derive(Default)]
struct TurnCancelRegistryInner {
    next_id: u64,
    active: BTreeMap<u64, RegisteredTurnCancel>,
}

struct RegisteredTurnCancel {
    token: CancellationToken,
    origin_hint: TurnCancelOriginHint,
}

impl TurnCancelRegistry {
    /// Register the token of a turn that is about to execute. The guard
    /// removes the entry when the turn finishes, however it finishes.
    fn register(
        &self,
        token: CancellationToken,
        origin_hint: TurnCancelOriginHint,
    ) -> TurnCancelGuard {
        let mut inner = self.inner.lock_recover();
        let id = inner.next_id;
        inner.next_id += 1;
        inner
            .active
            .insert(id, RegisteredTurnCancel { token, origin_hint });
        TurnCancelGuard {
            registry: Arc::clone(&self.inner),
            id,
        }
    }

    pub(crate) fn cancel_all(&self, origin: Option<String>) -> usize {
        let inner = self.inner.lock_recover();
        for registered in inner.active.values() {
            registered.origin_hint.set(origin.clone());
            registered.token.cancel();
        }
        inner.active.len()
    }
}

pub(crate) struct TurnCancelGuard {
    registry: Arc<StdMutex<TurnCancelRegistryInner>>,
    id: u64,
}

impl Drop for TurnCancelGuard {
    fn drop(&mut self) {
        self.registry.lock_recover().active.remove(&self.id);
    }
}

/// Builder for configuring turn.
pub struct TurnBuilder {
    pub(crate) runtime: RuntimeHandle,
    pub(crate) effect_host: Arc<dyn EffectHost>,
    pub(crate) active_plugins: Vec<ActivePluginBinding>,
    pub(crate) input: TurnInput,
    pub(crate) cancel: CancellationToken,
    pub(crate) cancels: TurnCancelRegistry,
    pub(crate) protocol_turn_options: Option<ProtocolTurnOptions>,
    pub(crate) provider: Option<ProviderHandle>,
    pub(crate) turn_id: Option<String>,
    pub(crate) cancel_origin_hint: TurnCancelOriginHint,
}

impl TurnBuilder {
    /// Install a process-local cooperative token.
    ///
    /// This low-level hook remains for provider plumbing, shutdown, and tests.
    /// Host-facing stop controls should use `TurnWorkDriver::request_cancel`
    /// with an exact session/turn address. If this token fires, cancellation
    /// evidence records no origin; call
    /// [`cancel_with_origin`](Self::cancel_with_origin) when the origin is
    /// known.
    pub fn cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self.cancel_origin_hint = TurnCancelOriginHint::default();
        lash_core::facade_support::configure_local_turn_token(&self.cancel_origin_hint, None);
        self
    }

    /// Install a process-local token with an opaque host-defined origin.
    ///
    /// Lash records the value without interpreting it.
    pub fn cancel_with_origin(mut self, cancel: CancellationToken, origin: Option<String>) -> Self {
        self.cancel = cancel;
        self.cancel_origin_hint = TurnCancelOriginHint::default();
        lash_core::facade_support::configure_local_turn_token(&self.cancel_origin_hint, origin);
        self
    }

    /// Configures the protocol turn options and returns the updated builder.
    pub fn protocol_turn_options(mut self, options: ProtocolTurnOptions) -> Self {
        self.protocol_turn_options = Some(options);
        self
    }

    /// Configures the provider and returns the updated builder.
    pub fn provider(mut self, provider: ProviderHandle) -> Self {
        self.provider = Some(provider);
        self
    }

    /// Configures the physical turn id and returns the updated builder.
    ///
    /// Hosts must keep this id unique within the session. Reusing it addresses
    /// the same trace, effects, and cancellation promises as the earlier turn;
    /// Lash does not mint or check uniqueness for host-supplied ids.
    pub fn turn_id(mut self, id: impl Into<String>) -> Self {
        self.turn_id = Some(id.into());
        self
    }

    /// Configures the prompt template and returns the updated builder.
    pub fn prompt_template(mut self, template: PromptTemplate) -> Self {
        self.input.turn_context.set_prompt_template(template);
        self
    }

    /// Configures the prompt contribution and returns the updated builder.
    pub fn prompt_contribution(mut self, contribution: PromptContribution) -> Self {
        self.input
            .turn_context
            .add_prompt_contribution(contribution);
        self
    }

    /// Replaces prompt slot.
    pub fn replace_prompt_slot(
        mut self,
        slot: PromptSlot,
        contributions: impl IntoIterator<Item = PromptContribution>,
    ) -> Self {
        self.input
            .turn_context
            .replace_prompt_slot(slot, contributions);
        self
    }

    /// Clears prompt slot.
    pub fn clear_prompt_slot(mut self, slot: PromptSlot) -> Self {
        self.input.turn_context.clear_prompt_slot(slot);
        self
    }

    /// Configures the prompt layer and returns the updated builder.
    pub fn prompt_layer(mut self, layer: PromptLayer) -> Self {
        self.input.turn_context.set_prompt_layer(layer);
        self
    }

    /// Attach typed per-turn input for an activated plugin binding.
    ///
    /// This is the generic primitive. Plugin crates should usually wrap it in a
    /// domain extension trait such as `.with_tone(tone)` or `.with_board(board)`
    /// so application code stays typed in its own vocabulary.
    pub fn with_plugin_input<P: PluginBinding>(mut self, input: P::Input) -> Self {
        self.input.turn_context.insert_plugin_input(P::ID, input);
        self
    }

    /// Returns the configured effect receiver.
    pub fn effects(self, controller: &dyn RuntimeEffectController) -> ScopedTurnBuilder<'_> {
        ScopedTurnBuilder {
            builder: self,
            controller,
        }
    }

    /// Accept this turn's input durably, drive it, and collect its activity.
    ///
    /// Convenience over [`stream_to`](Self::stream_to), which documents what one
    /// acceptance commit then one drive means for a host.
    pub async fn run(self) -> Result<TurnOutput> {
        let collector = RunActivityCollector::default();
        let result = self.stream_to(&collector).await?;
        Ok(TurnOutput {
            result,
            activities: collector.into_activities(),
        })
    }

    /// Accept this turn's input durably, drive it, and stream its activity.
    ///
    /// One acceptance commit then one drive (ADR 0069). For a host that means:
    ///
    /// * **A persistent session pays one extra store commit per turn** — the
    ///   same Pending Turn Input commit queued ingress has always paid.
    /// * **Dropping the returned future does not stop the turn.** The caller is
    ///   the first driver, not the owner, so a caller that goes away has handed
    ///   the turn to whoever drains next. Abandon a turn by cancelling its
    ///   accepted input, or with
    ///   [`LashSession::request_turn_cancel`](crate::LashSession::request_turn_cancel).
    /// * **Input already queued for this session joins this turn**, because a
    ///   direct turn claims the head of the same pending queue a drain does.
    /// * **A retry after an unacknowledged crash is a new turn.** Lash mints no
    ///   idempotency key for a direct turn; a host needing at-most-once
    ///   submission supplies its own with
    ///   [`EnqueueTurnBuilder::id`](crate::EnqueueTurnBuilder::id).
    pub async fn stream_to(self, events: &dyn TurnActivitySink) -> Result<TurnReport> {
        let effect_host = Arc::clone(&self.effect_host);
        self.stream_to_with_effect_host(events, effect_host.as_ref())
            .await
    }

    /// Starts streaming the configured operation.
    pub fn stream(self) -> Result<TurnStream> {
        let effect_host = Arc::clone(&self.effect_host);
        self.stream_with_effect_host(effect_host.as_ref())
    }

    /// Access lower-level turn execution that bypasses the semantic
    /// [`TurnActivity`] tier.
    pub fn advanced(self) -> AdvancedTurn {
        AdvancedTurn { builder: self }
    }

    fn resolved_turn_id(
        &self,
        scoped_effect_controller: Option<&ScopedEffectController<'_>>,
    ) -> Option<String> {
        self.turn_id.clone().or_else(|| {
            scoped_effect_controller
                .filter(|controller| controller.execution_scope().validates_turn_trace_id())
                .map(|controller| controller.scope_id().to_string())
        })
    }

    fn turn_scope(&self, turn_id: &str) -> lash_core::ExecutionScope {
        let observation = self.runtime.observe();
        observation.persisted_state.turn_scope(turn_id)
    }

    pub(crate) fn prepare(
        mut self,
        turn_id: Option<String>,
    ) -> Result<(RuntimeHandle, TurnInput, CancellationToken, TurnCancelGuard)> {
        if let Some(options) = self.protocol_turn_options {
            self.input.protocol_turn_options = Some(options);
        }
        if let Some(provider) = self.provider {
            self.input.turn_context.set_provider(provider);
        }
        if let Some(turn_id) = turn_id {
            self.input.trace_turn_id = Some(turn_id);
        }
        validate_required_plugin_inputs(&self.active_plugins, &self.input)?;
        self.input
            .turn_context
            .set_local_cancel_origin_hint(self.cancel_origin_hint.clone());
        let cancel_guard = self
            .cancels
            .register(self.cancel.clone(), self.cancel_origin_hint);
        Ok((self.runtime, self.input, self.cancel, cancel_guard))
    }

    async fn stream_to_with_effect_host(
        self,
        events: &dyn TurnActivitySink,
        effect_host: &dyn EffectHost,
    ) -> Result<TurnReport> {
        let turn_id = self.resolved_turn_id(None).unwrap_or_else(fresh_turn_id);
        let scoped_effect_controller = effect_host.scoped(self.turn_scope(&turn_id))?;
        self.stream_to_with_scope(events, scoped_effect_controller, Some(turn_id))
            .await
    }

    async fn stream_to_with_effect_controller(
        self,
        events: &dyn TurnActivitySink,
        controller: &dyn RuntimeEffectController,
    ) -> Result<TurnReport> {
        let turn_id = self.resolved_turn_id(None).unwrap_or_else(fresh_turn_id);
        let scoped_effect_controller =
            ScopedEffectController::borrowed(controller, self.turn_scope(&turn_id))?;
        self.stream_to_with_scope(events, scoped_effect_controller, Some(turn_id))
            .await
    }

    async fn stream_to_with_scope(
        self,
        events: &dyn TurnActivitySink,
        scoped_effect_controller: ScopedEffectController<'_>,
        turn_id: Option<String>,
    ) -> Result<TurnReport> {
        let (runtime, input, cancel, _cancel_guard) = self.prepare(turn_id)?;
        stream_prepared_turn(
            &runtime,
            input,
            TurnSinks::turn(events),
            scoped_effect_controller,
            cancel,
        )
        .await
    }

    fn stream_with_effect_host(self, effect_host: &dyn EffectHost) -> Result<TurnStream> {
        let turn_id = self.resolved_turn_id(None).unwrap_or_else(fresh_turn_id);
        let scoped_effect_controller = effect_host
            .scoped_static(self.turn_scope(&turn_id))?
            .ok_or(EmbedError::StaticTurnStreamRequiresStaticEffectHost)?;
        self.stream_with_scope(scoped_effect_controller, Some(turn_id))
    }

    fn stream_with_scope(
        self,
        scoped_effect_controller: ScopedEffectController<'static>,
        turn_id: Option<String>,
    ) -> Result<TurnStream> {
        let (runtime, input, cancel, cancel_guard) = self.prepare(turn_id)?;
        let (tx, rx) = mpsc::channel(64);
        let sink = ChannelTurnActivitySink { tx };
        let completion = tokio::spawn(async move {
            let _cancel_guard = cancel_guard;
            stream_prepared_turn(
                &runtime,
                input,
                TurnSinks::turn(&sink),
                scoped_effect_controller,
                cancel,
            )
            .await
        });
        Ok(TurnStream {
            activities: rx,
            completion,
        })
    }
}

/// Builder for configuring scoped turn.
pub struct ScopedTurnBuilder<'run> {
    builder: TurnBuilder,
    controller: &'run dyn RuntimeEffectController,
}

impl<'run> ScopedTurnBuilder<'run> {
    /// Install a low-level process-local cancellation token.
    pub fn cancel(mut self, cancel: CancellationToken) -> Self {
        self.builder = self.builder.cancel(cancel);
        self
    }

    /// Installs a cancellation token with an optional host-defined origin.
    pub fn cancel_with_origin(mut self, cancel: CancellationToken, origin: Option<String>) -> Self {
        self.builder = self.builder.cancel_with_origin(cancel, origin);
        self
    }

    /// Configures the protocol turn options and returns the updated builder.
    pub fn protocol_turn_options(mut self, options: ProtocolTurnOptions) -> Self {
        self.builder = self.builder.protocol_turn_options(options);
        self
    }

    /// Configures the provider and returns the updated builder.
    pub fn provider(mut self, provider: ProviderHandle) -> Self {
        self.builder = self.builder.provider(provider);
        self
    }

    /// Configures the turn id and returns the updated builder.
    pub fn turn_id(mut self, id: impl Into<String>) -> Self {
        self.builder = self.builder.turn_id(id);
        self
    }

    /// Configures the prompt template and returns the updated builder.
    pub fn prompt_template(mut self, template: PromptTemplate) -> Self {
        self.builder = self.builder.prompt_template(template);
        self
    }

    /// Configures the prompt contribution and returns the updated builder.
    pub fn prompt_contribution(mut self, contribution: PromptContribution) -> Self {
        self.builder = self.builder.prompt_contribution(contribution);
        self
    }

    /// Replaces prompt slot.
    pub fn replace_prompt_slot(
        mut self,
        slot: PromptSlot,
        contributions: impl IntoIterator<Item = PromptContribution>,
    ) -> Self {
        self.builder = self.builder.replace_prompt_slot(slot, contributions);
        self
    }

    /// Clears prompt slot.
    pub fn clear_prompt_slot(mut self, slot: PromptSlot) -> Self {
        self.builder = self.builder.clear_prompt_slot(slot);
        self
    }

    /// Configures the prompt layer and returns the updated builder.
    pub fn prompt_layer(mut self, layer: PromptLayer) -> Self {
        self.builder = self.builder.prompt_layer(layer);
        self
    }

    /// Supplies typed turn input for a bound plugin.
    pub fn with_plugin_input<P: PluginBinding>(mut self, input: P::Input) -> Self {
        self.builder = self.builder.with_plugin_input::<P>(input);
        self
    }

    /// Runs the turn and collects its activity and final report.
    pub async fn run(self) -> Result<TurnOutput> {
        let collector = RunActivityCollector::default();
        let result = self.stream_to(&collector).await?;
        Ok(TurnOutput {
            result,
            activities: collector.into_activities(),
        })
    }

    /// Runs the turn while sending semantic activity to the supplied sink.
    pub async fn stream_to(self, events: &dyn TurnActivitySink) -> Result<TurnReport> {
        self.builder
            .stream_to_with_effect_controller(events, self.controller)
            .await
    }
}

/// Lower-level turn execution that exposes the raw runtime event stream.
///
/// Reachable via [`TurnBuilder::advanced`]. Most applications should use
/// [`TurnBuilder::stream_to`] for semantic turn activity; benchmarks and
/// diagnostics use this when they need the same low-level event stream as the
/// runtime trace.
pub struct AdvancedTurn {
    builder: TurnBuilder,
}

impl AdvancedTurn {
    /// Runs the turn with an explicit replay-aware effect scope.
    pub async fn run_with_scope(
        self,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<TurnOutput> {
        let collector = RunActivityCollector::default();
        let result = self
            .stream_to_with_scope(&collector, scoped_effect_controller)
            .await?;
        Ok(TurnOutput {
            result,
            activities: collector.into_activities(),
        })
    }

    /// Collects the scoped turn stream into its final output.
    pub async fn collect_with_scope(
        self,
        events: &dyn TurnActivitySink,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<TurnOutput> {
        let collector = RunActivityCollector::default();
        let fanout = BorrowedTurnActivityFanout {
            live: events,
            collector: &collector,
        };
        let result = self
            .stream_to_with_scope(&fanout, scoped_effect_controller)
            .await?;
        Ok(TurnOutput {
            result,
            activities: collector.into_activities(),
        })
    }

    /// Runs the turn in an explicit effect scope while streaming semantic activity.
    pub async fn stream_to_with_scope(
        self,
        events: &dyn TurnActivitySink,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<TurnReport> {
        let turn_id = self
            .builder
            .resolved_turn_id(Some(&scoped_effect_controller));
        self.builder
            .stream_to_with_scope(events, scoped_effect_controller, turn_id)
            .await
    }

    /// Starts a raw runtime event stream in an explicit effect scope.
    pub fn stream_with_scope(
        self,
        scoped_effect_controller: ScopedEffectController<'static>,
    ) -> Result<TurnStream> {
        let turn_id = self
            .builder
            .resolved_turn_id(Some(&scoped_effect_controller));
        self.builder
            .stream_with_scope(scoped_effect_controller, turn_id)
    }

    /// Run the turn while sending raw lower-level runtime events to `events`.
    pub async fn collect_session_events_with_scope(
        self,
        events: &dyn EventSink,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<TurnReport> {
        let turn_id = self
            .builder
            .resolved_turn_id(Some(&scoped_effect_controller));
        let (runtime, input, cancel, _cancel_guard) = self.builder.prepare(turn_id)?;
        stream_prepared_turn(
            &runtime,
            input,
            TurnSinks::session(events),
            scoped_effect_controller,
            cancel,
        )
        .await
    }
}

/// Stream of turn activity.
pub struct TurnStream {
    activities: mpsc::Receiver<Result<TurnActivity>>,
    completion: JoinHandle<Result<TurnReport>>,
}

impl TurnStream {
    /// Waits for and returns the next turn activity.
    pub async fn next_activity(&mut self) -> Option<Result<TurnActivity>> {
        self.activities.recv().await
    }

    /// Consumes the stream and waits for its final turn output.
    pub async fn finish(self) -> Result<TurnReport> {
        match self.completion.await {
            Ok(result) => result,
            Err(err) if err.is_panic() => {
                let payload = err.into_panic();
                let message = if let Some(message) = payload.downcast_ref::<&str>() {
                    (*message).to_string()
                } else if let Some(message) = payload.downcast_ref::<String>() {
                    message.clone()
                } else {
                    "non-string panic payload".to_string()
                };
                let failure = EmbedError::Runtime(lash_core::RuntimeError::new(
                    RuntimeErrorCode::TurnStreamJoin,
                    format!("turn_panicked: {message}"),
                ));
                if lash_core::panic_containment::is_loud() {
                    std::panic::resume_unwind(payload);
                }
                Err(failure)
            }
            Err(err) => Err(EmbedError::Runtime(lash_core::RuntimeError::new(
                RuntimeErrorCode::TurnStreamJoin,
                format!("turn stream task failed: {err}"),
            ))),
        }
    }
}

impl Stream for TurnStream {
    type Item = Result<TurnActivity>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.activities.poll_recv(cx)
    }
}

/// Builder for configuring queued turn.
pub struct QueuedTurnBuilder {
    pub(crate) runtime: RuntimeHandle,
    pub(crate) effect_host: Arc<dyn EffectHost>,
    pub(crate) cancel: CancellationToken,
    pub(crate) cancel_origin_hint: TurnCancelOriginHint,
    pub(crate) cancels: TurnCancelRegistry,
    pub(crate) turn_id: Option<String>,
    pub(crate) drain_id: Option<String>,
}

impl QueuedTurnBuilder {
    /// Install a process-local token whose origin is unknown.
    pub fn cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self.cancel_origin_hint = TurnCancelOriginHint::default();
        lash_core::facade_support::configure_local_turn_token(&self.cancel_origin_hint, None);
        self
    }

    /// Install a process-local token with an opaque host-defined origin.
    ///
    /// Lash records the value without interpreting it.
    pub fn cancel_with_origin(mut self, cancel: CancellationToken, origin: Option<String>) -> Self {
        self.cancel = cancel;
        self.cancel_origin_hint = TurnCancelOriginHint::default();
        lash_core::facade_support::configure_local_turn_token(&self.cancel_origin_hint, origin);
        self
    }

    /// Configures the physical turn id and returns the updated builder.
    ///
    /// Hosts must keep this id unique within the session. Reusing it addresses
    /// the same trace, effects, and cancellation promises as the earlier turn;
    /// Lash does not mint or check uniqueness for host-supplied ids.
    /// Do not combine this with [`Self::drain_id`]: keep `turn_id` for a
    /// host-minted physical turn identity, or use `drain_id` as the durable
    /// idempotency key for retried drains.
    pub fn turn_id(mut self, id: impl Into<String>) -> Self {
        self.turn_id = Some(id.into());
        self
    }

    /// Replaces automatic queue selection with one exact, idempotent batch-ID set.
    ///
    /// Duplicate IDs are coalesced by first occurrence. Missing durable rows
    /// count as already satisfied; all still-present rows must be claimable as
    /// one composition or Lash refuses the drain before executing a turn.
    pub fn batch_ids(
        self,
        batch_ids: impl IntoIterator<Item = impl Into<String>>,
    ) -> SelectedQueuedTurnBuilder {
        SelectedQueuedTurnBuilder {
            builder: self,
            batch_ids: batch_ids.into_iter().map(Into::into).collect(),
        }
    }

    /// Sets the durable idempotency key for a retried queued-work drain.
    ///
    /// Do not combine this with [`Self::turn_id`]: keep `drain_id` for retried
    /// drains, or use `turn_id` for a host-minted physical turn identity.
    pub fn drain_id(mut self, drain_id: impl Into<String>) -> Self {
        self.drain_id = Some(drain_id.into());
        self
    }

    /// Binds the queued turn to a borrowed effect controller.
    pub fn effects(self, controller: &dyn RuntimeEffectController) -> ScopedQueuedTurnBuilder<'_> {
        ScopedQueuedTurnBuilder {
            builder: self,
            controller,
        }
    }

    /// Drains the next claimable queued work and runs it as one turn.
    ///
    /// A drain that runs no turn returns [`QueuedTurnDrain::Empty`] carrying the
    /// reason, which the host must interpret rather than guess at.
    pub async fn run(self) -> Result<QueuedTurnDrain<TurnOutput>> {
        let collector = RunActivityCollector::default();
        Ok(self.stream_to(&collector).await?.map(|result| TurnOutput {
            result,
            activities: collector.into_activities(),
        }))
    }

    /// Drains queued work while sending semantic activity to the supplied sink.
    pub async fn stream_to(
        self,
        events: &dyn TurnActivitySink,
    ) -> Result<QueuedTurnDrain<TurnReport>> {
        let effect_host = Arc::clone(&self.effect_host);
        self.stream_to_with_effect_host(events, effect_host.as_ref())
            .await
    }

    /// Converts this builder into its advanced queued-turn facade.
    pub fn advanced(self) -> AdvancedQueuedTurn {
        AdvancedQueuedTurn { builder: self }
    }

    fn resolved_drain_id(&self) -> String {
        self.drain_id.clone().unwrap_or_else(fresh_queue_drain_id)
    }

    fn validate_scope_identity_configuration(&self) -> Result<()> {
        if self.drain_id.is_some() && self.turn_id.is_some() {
            return Err(EmbedError::Runtime(lash_core::RuntimeError::new(
                RuntimeErrorCode::ExecutionScopeTurnIdMismatch,
                "`drain_id(...)` and `turn_id(...)` are mutually exclusive; keep `drain_id(...)` as the durable idempotency key for retried drains, or keep `turn_id(...)` as the host-minted physical turn identity",
            )));
        }
        Ok(())
    }

    fn resolved_turn_id(
        &self,
        scoped_effect_controller: Option<&ScopedEffectController<'_>>,
    ) -> Option<String> {
        self.turn_id.clone().or_else(|| {
            scoped_effect_controller
                .filter(|controller| controller.execution_scope().validates_turn_trace_id())
                .map(|controller| controller.scope_id().to_string())
        })
    }

    fn turn_scope(&self, turn_id: &str) -> lash_core::ExecutionScope {
        let observation = self.runtime.observe();
        observation.persisted_state.turn_scope(turn_id)
    }

    fn execution_scope(&self, drain_id: String) -> Result<lash_core::ExecutionScope> {
        self.validate_scope_identity_configuration()?;
        Ok(self.resolved_turn_id(None).map_or_else(
            || {
                lash_core::facade_support::RuntimeSessionStateFacadeOps::queue_drain_scope(
                    &self.runtime.observe().persisted_state,
                    drain_id,
                )
            },
            |turn_id| self.turn_scope(&turn_id),
        ))
    }

    async fn stream_to_with_effect_host(
        self,
        events: &dyn TurnActivitySink,
        effect_host: &dyn EffectHost,
    ) -> Result<QueuedTurnDrain<TurnReport>> {
        let drain_id = self.resolved_drain_id();
        let scope = self.execution_scope(drain_id)?;
        let scoped_effect_controller = effect_host.scoped(scope)?;
        self.stream_to_with_scope(events, scoped_effect_controller)
            .await
    }

    async fn stream_to_with_effect_controller(
        self,
        events: &dyn TurnActivitySink,
        controller: &dyn RuntimeEffectController,
    ) -> Result<QueuedTurnDrain<TurnReport>> {
        let drain_id = self.resolved_drain_id();
        let scope = self.execution_scope(drain_id)?;
        let scoped_effect_controller = ScopedEffectController::borrowed(controller, scope)?;
        self.stream_to_with_scope(events, scoped_effect_controller)
            .await
    }

    async fn stream_to_with_scope(
        self,
        events: &dyn TurnActivitySink,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<QueuedTurnDrain<TurnReport>> {
        self.validate_scope_identity_configuration()?;
        let turn_id = self.resolved_turn_id(Some(&scoped_effect_controller));
        if let Some(turn_id) = turn_id.as_deref()
            && !scoped_effect_controller
                .execution_scope()
                .validates_turn_trace_id()
        {
            let scoped_turn_controller = ScopedEffectController::borrowed(
                scoped_effect_controller.controller(),
                self.turn_scope(turn_id),
            )?;
            return self
                .stream_to_with_resolved_scope(events, scoped_turn_controller)
                .await;
        }
        if let Some(turn_id) = turn_id.as_deref()
            && turn_id != scoped_effect_controller.scope_id()
        {
            return Err(EmbedError::Runtime(lash_core::RuntimeError::new(
                RuntimeErrorCode::ExecutionScopeTurnIdMismatch,
                format!(
                    "input trace_turn_id `{turn_id}` does not match execution scope id `{}`",
                    scoped_effect_controller.scope_id()
                ),
            )));
        }
        self.stream_to_with_resolved_scope(events, scoped_effect_controller)
            .await
    }

    async fn stream_to_with_resolved_scope(
        self,
        events: &dyn TurnActivitySink,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<QueuedTurnDrain<TurnReport>> {
        let Self {
            runtime,
            effect_host: _,
            cancel,
            cancel_origin_hint,
            cancels,
            turn_id: _,
            drain_id: _,
        } = self;
        let _cancel_guard = cancels.register(cancel.clone(), cancel_origin_hint.clone());
        stream_next_queued_prepared_turn(
            &runtime,
            TurnSinks::turn(events),
            scoped_effect_controller,
            cancel,
            cancel_origin_hint,
        )
        .await
    }
}

/// Builder for one exact, idempotent queued-work drain.
///
/// Repeated IDs are coalesced by first occurrence. Missing rows are satisfied;
/// present rows must form one claim or Lash refuses before selected execution.
pub struct SelectedQueuedTurnBuilder {
    builder: QueuedTurnBuilder,
    batch_ids: Vec<String>,
}

impl SelectedQueuedTurnBuilder {
    /// Installs a process-local cancellation token for any selected turn.
    /// A fully satisfied drain starts no turn for the token to cancel.
    pub fn cancel(mut self, cancel: CancellationToken) -> Self {
        self.builder = self.builder.cancel(cancel);
        self
    }

    /// Installs a cancellation token and opaque host origin for any selected
    /// turn. Lash does not interpret the origin.
    pub fn cancel_with_origin(mut self, cancel: CancellationToken, origin: Option<String>) -> Self {
        self.builder = self.builder.cancel_with_origin(cancel, origin);
        self
    }

    /// Configures the physical id of any selected turn.
    ///
    /// Mutually exclusive with [`Self::drain_id`]. See
    /// [`QueuedTurnBuilder::turn_id`] for the identity contracts.
    pub fn turn_id(mut self, id: impl Into<String>) -> Self {
        self.builder = self.builder.turn_id(id);
        self
    }

    /// Sets the effect-scope identity for any selected turn. By default Lash
    /// uses the first batch ID, or a fresh identity for an empty selection.
    ///
    /// Mutually exclusive with [`Self::turn_id`]. See
    /// [`QueuedTurnBuilder::drain_id`] for the identity contracts.
    pub fn drain_id(mut self, drain_id: impl Into<String>) -> Self {
        self.builder = self.builder.drain_id(drain_id);
        self
    }

    /// Uses a borrowed controller when replay authority belongs to the host
    /// handler rather than the session's configured effect host.
    pub fn effects(
        self,
        controller: &dyn RuntimeEffectController,
    ) -> ScopedSelectedQueuedTurnBuilder<'_> {
        ScopedSelectedQueuedTurnBuilder {
            builder: self,
            controller,
        }
    }

    /// Drains exactly and collects any turn activity. Fully satisfied means
    /// [`SelectedQueuedWorkDrainOutcome::settled_without_selected_turn`] is `true`; unclaimable means
    /// a typed refusal before provider or tool execution.
    pub async fn run(self) -> Result<SelectedQueuedWorkDrainOutcome<TurnOutput>> {
        let collector = RunActivityCollector::default();
        let outcome = self.stream_to(&collector).await?;
        Ok(SelectedQueuedWorkDrainOutcome {
            turn: outcome.turn.map(|result| TurnOutput {
                result,
                activities: collector.into_activities(),
            }),
            satisfied: outcome.satisfied,
        })
    }

    /// Drains exactly and sends any selected turn activity to `events`.
    /// Missing IDs succeed; present IDs must be claimable together.
    pub async fn stream_to(
        self,
        events: &dyn TurnActivitySink,
    ) -> Result<SelectedQueuedWorkDrainOutcome<TurnReport>> {
        let effect_host = Arc::clone(&self.builder.effect_host);
        self.stream_to_with_effect_host(events, effect_host.as_ref())
            .await
    }

    /// Exposes the scoped-effect entry point for hosts that already own a
    /// [`ScopedEffectController`].
    pub fn advanced(self) -> AdvancedSelectedQueuedTurn {
        AdvancedSelectedQueuedTurn { builder: self }
    }

    fn resolved_drain_id(&self) -> String {
        self.builder
            .drain_id
            .clone()
            .or_else(|| self.batch_ids.first().cloned())
            .unwrap_or_else(fresh_queue_drain_id)
    }

    async fn stream_to_with_effect_host(
        self,
        events: &dyn TurnActivitySink,
        effect_host: &dyn EffectHost,
    ) -> Result<SelectedQueuedWorkDrainOutcome<TurnReport>> {
        let drain_id = self.resolved_drain_id();
        let scope = self.builder.execution_scope(drain_id)?;
        let scoped_effect_controller = effect_host.scoped(scope)?;
        self.stream_to_with_scope(events, scoped_effect_controller)
            .await
    }

    async fn stream_to_with_effect_controller(
        self,
        events: &dyn TurnActivitySink,
        controller: &dyn RuntimeEffectController,
    ) -> Result<SelectedQueuedWorkDrainOutcome<TurnReport>> {
        let drain_id = self.resolved_drain_id();
        let scope = self.builder.execution_scope(drain_id)?;
        let scoped_effect_controller = ScopedEffectController::borrowed(controller, scope)?;
        self.stream_to_with_scope(events, scoped_effect_controller)
            .await
    }

    async fn stream_to_with_scope(
        self,
        events: &dyn TurnActivitySink,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<SelectedQueuedWorkDrainOutcome<TurnReport>> {
        self.builder.validate_scope_identity_configuration()?;
        let turn_id = self
            .builder
            .resolved_turn_id(Some(&scoped_effect_controller));
        if let Some(turn_id) = turn_id.as_deref()
            && !scoped_effect_controller
                .execution_scope()
                .validates_turn_trace_id()
        {
            let scoped_turn_controller = ScopedEffectController::borrowed(
                scoped_effect_controller.controller(),
                self.builder.turn_scope(turn_id),
            )?;
            return self
                .stream_to_with_resolved_scope(events, scoped_turn_controller)
                .await;
        }
        if let Some(turn_id) = turn_id.as_deref()
            && turn_id != scoped_effect_controller.scope_id()
        {
            return Err(EmbedError::Runtime(lash_core::RuntimeError::new(
                RuntimeErrorCode::ExecutionScopeTurnIdMismatch,
                format!(
                    "input trace_turn_id `{turn_id}` does not match execution scope id `{}`",
                    scoped_effect_controller.scope_id()
                ),
            )));
        }
        self.stream_to_with_resolved_scope(events, scoped_effect_controller)
            .await
    }

    async fn stream_to_with_resolved_scope(
        self,
        events: &dyn TurnActivitySink,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<SelectedQueuedWorkDrainOutcome<TurnReport>> {
        let Self { builder, batch_ids } = self;
        let QueuedTurnBuilder {
            runtime,
            effect_host: _,
            cancel,
            cancel_origin_hint,
            cancels,
            turn_id: _,
            drain_id: _,
        } = builder;
        let _cancel_guard = cancels.register(cancel.clone(), cancel_origin_hint.clone());
        if batch_ids.is_empty() {
            return Ok(SelectedQueuedWorkDrainOutcome {
                turn: None,
                satisfied: Vec::new(),
            });
        }
        stream_selected_queued_prepared_turn(
            &runtime,
            TurnSinks::turn(events),
            scoped_effect_controller,
            cancel,
            cancel_origin_hint,
            &batch_ids,
        )
        .await
    }
}

/// Builder for configuring scoped queued turn.
pub struct ScopedQueuedTurnBuilder<'run> {
    builder: QueuedTurnBuilder,
    controller: &'run dyn RuntimeEffectController,
}

impl<'run> ScopedQueuedTurnBuilder<'run> {
    /// Installs the cancellation token for this scoped queued turn.
    pub fn cancel(mut self, cancel: CancellationToken) -> Self {
        self.builder = self.builder.cancel(cancel);
        self
    }

    /// Installs a cancellation token with an optional host-defined origin.
    pub fn cancel_with_origin(mut self, cancel: CancellationToken, origin: Option<String>) -> Self {
        self.builder = self.builder.cancel_with_origin(cancel, origin);
        self
    }

    /// Configures the physical id of the queued turn.
    ///
    /// Mutually exclusive with [`Self::drain_id`]. See
    /// [`QueuedTurnBuilder::turn_id`] for the identity contracts.
    pub fn turn_id(mut self, id: impl Into<String>) -> Self {
        self.builder = self.builder.turn_id(id);
        self
    }

    /// Replaces automatic queue selection with one exact, idempotent batch-ID set.
    ///
    /// The returned builder keeps this borrowed effect controller. Missing
    /// rows are already satisfied, while present rows must be claimable
    /// together or the drain is refused before execution.
    pub fn batch_ids(
        self,
        batch_ids: impl IntoIterator<Item = impl Into<String>>,
    ) -> ScopedSelectedQueuedTurnBuilder<'run> {
        ScopedSelectedQueuedTurnBuilder {
            builder: self.builder.batch_ids(batch_ids),
            controller: self.controller,
        }
    }

    /// Sets the queued-work drain's durable idempotency key.
    ///
    /// Mutually exclusive with [`Self::turn_id`]. See
    /// [`QueuedTurnBuilder::drain_id`] for the identity contracts.
    pub fn drain_id(mut self, drain_id: impl Into<String>) -> Self {
        self.builder = self.builder.drain_id(drain_id);
        self
    }

    /// Drains queued work and collects its activity and final report.
    pub async fn run(self) -> Result<QueuedTurnDrain<TurnOutput>> {
        let collector = RunActivityCollector::default();
        Ok(self.stream_to(&collector).await?.map(|result| TurnOutput {
            result,
            activities: collector.into_activities(),
        }))
    }

    /// Drains queued work while sending semantic activity to the supplied sink.
    pub async fn stream_to(
        self,
        events: &dyn TurnActivitySink,
    ) -> Result<QueuedTurnDrain<TurnReport>> {
        self.builder
            .stream_to_with_effect_controller(events, self.controller)
            .await
    }
}

/// Exact selected-drain builder bound to a borrowed effect controller.
///
/// It preserves [`SelectedQueuedTurnBuilder`]'s satisfied-or-refused contract;
/// binding changes only where selected effects run.
pub struct ScopedSelectedQueuedTurnBuilder<'run> {
    builder: SelectedQueuedTurnBuilder,
    controller: &'run dyn RuntimeEffectController,
}

impl<'run> ScopedSelectedQueuedTurnBuilder<'run> {
    /// Replaces the process-local cancellation token for any selected turn.
    pub fn cancel(mut self, cancel: CancellationToken) -> Self {
        self.builder = self.builder.cancel(cancel);
        self
    }

    /// Replaces the cancellation token and opaque origin for any selected turn.
    pub fn cancel_with_origin(mut self, cancel: CancellationToken, origin: Option<String>) -> Self {
        self.builder = self.builder.cancel_with_origin(cancel, origin);
        self
    }

    /// Configures the physical id of any selected turn.
    ///
    /// Mutually exclusive with [`Self::drain_id`]. See
    /// [`QueuedTurnBuilder::turn_id`] for the identity contracts.
    pub fn turn_id(mut self, id: impl Into<String>) -> Self {
        self.builder = self.builder.turn_id(id);
        self
    }

    /// Sets the durable idempotency key for a retried selected drain.
    ///
    /// Mutually exclusive with [`Self::turn_id`]. See
    /// [`QueuedTurnBuilder::drain_id`] for the identity contracts.
    pub fn drain_id(mut self, drain_id: impl Into<String>) -> Self {
        self.builder = self.builder.drain_id(drain_id);
        self
    }

    /// Drains the exact selection and collects activity for any selected turn.
    ///
    /// Successful `None` is fully satisfied; busy or partial claims are errors.
    pub async fn run(self) -> Result<SelectedQueuedWorkDrainOutcome<TurnOutput>> {
        let collector = RunActivityCollector::default();
        let outcome = self.stream_to(&collector).await?;
        Ok(SelectedQueuedWorkDrainOutcome {
            turn: outcome.turn.map(|result| TurnOutput {
                result,
                activities: collector.into_activities(),
            }),
            satisfied: outcome.satisfied,
        })
    }

    /// Drains exactly through the bound controller, sending activity to `events`.
    pub async fn stream_to(
        self,
        events: &dyn TurnActivitySink,
    ) -> Result<SelectedQueuedWorkDrainOutcome<TurnReport>> {
        self.builder
            .stream_to_with_effect_controller(events, self.controller)
            .await
    }
}

/// Exposes advanced execution controls for a queued turn.
pub struct AdvancedQueuedTurn {
    builder: QueuedTurnBuilder,
}

/// Advanced selected-drain entry point for an already scoped effect controller.
///
/// The caller supplies replay scope; exact satisfaction/refusal is unchanged.
pub struct AdvancedSelectedQueuedTurn {
    builder: SelectedQueuedTurnBuilder,
}

impl AdvancedSelectedQueuedTurn {
    /// Drains exactly, using `scoped_effect_controller` for any selected turn.
    ///
    /// A successful outcome without a turn means the selection was fully
    /// satisfied; a present but unclaimable selection is returned as an error.
    pub async fn stream_to_with_scope(
        self,
        events: &dyn TurnActivitySink,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<SelectedQueuedWorkDrainOutcome<TurnReport>> {
        self.builder
            .stream_to_with_scope(events, scoped_effect_controller)
            .await
    }
}

impl AdvancedQueuedTurn {
    /// Drains queued work in an explicit effect scope while streaming activity.
    pub async fn stream_to_with_scope(
        self,
        events: &dyn TurnActivitySink,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<QueuedTurnDrain<TurnReport>> {
        self.builder
            .stream_to_with_scope(events, scoped_effect_controller)
            .await
    }
}

fn fresh_turn_id() -> String {
    lash_core::TurnActivityId::new(uuid::Uuid::new_v4().to_string())
        .0
        .to_string()
}

fn fresh_queue_drain_id() -> String {
    format!("queue-drain:{}", fresh_turn_id())
}

pub(crate) async fn stream_next_queued_prepared_turn(
    runtime: &RuntimeHandle,
    sinks: TurnSinks<'_>,
    scoped_effect_controller: ScopedEffectController<'_>,
    cancel: CancellationToken,
    cancel_origin_hint: TurnCancelOriginHint,
) -> Result<QueuedTurnDrain<TurnReport>> {
    let drain = Box::pin(stream_next_queued_prepared_assembled(
        runtime,
        sinks,
        scoped_effect_controller,
        cancel,
        cancel_origin_hint,
    ))
    .await?;
    Ok(drain.map(TurnReport::from_assembled))
}

pub(crate) async fn stream_next_queued_prepared_assembled(
    runtime: &RuntimeHandle,
    sinks: TurnSinks<'_>,
    scoped_effect_controller: ScopedEffectController<'_>,
    cancel: CancellationToken,
    cancel_origin_hint: TurnCancelOriginHint,
) -> Result<QueuedTurnDrain<AssembledTurn>> {
    let writer_handle = runtime.writer();
    let mut writer = writer_handle.lock().await;
    let observation_sink = SessionObservationTurnActivitySink {
        runtime: runtime.clone(),
        live: sinks.turn_events(),
    };
    let opts = turn_options(
        sinks.events(),
        &observation_sink,
        scoped_effect_controller,
        cancel,
    )
    .with_local_cancel_origin_hint(cancel_origin_hint);
    let drain = writer.stream_next_queued_work(opts).await?;
    runtime.publish_from(&writer);
    Ok(queued_turn_drain(drain))
}

pub(crate) async fn stream_selected_queued_prepared_turn(
    runtime: &RuntimeHandle,
    sinks: TurnSinks<'_>,
    scoped_effect_controller: ScopedEffectController<'_>,
    cancel: CancellationToken,
    cancel_origin_hint: TurnCancelOriginHint,
    batch_ids: &[String],
) -> Result<SelectedQueuedWorkDrainOutcome<TurnReport>> {
    let outcome = Box::pin(stream_selected_queued_prepared_assembled(
        runtime,
        sinks,
        scoped_effect_controller,
        cancel,
        cancel_origin_hint,
        batch_ids,
    ))
    .await?;
    Ok(SelectedQueuedWorkDrainOutcome {
        turn: outcome.turn.map(TurnReport::from_assembled),
        satisfied: outcome.satisfied,
    })
}

pub(crate) async fn stream_selected_queued_prepared_assembled(
    runtime: &RuntimeHandle,
    sinks: TurnSinks<'_>,
    scoped_effect_controller: ScopedEffectController<'_>,
    cancel: CancellationToken,
    cancel_origin_hint: TurnCancelOriginHint,
    batch_ids: &[String],
) -> Result<SelectedQueuedWorkDrainOutcome<AssembledTurn>> {
    let writer_handle = runtime.writer();
    let mut writer = writer_handle.lock().await;
    let observation_sink = SessionObservationTurnActivitySink {
        runtime: runtime.clone(),
        live: sinks.turn_events(),
    };
    let opts = turn_options(
        sinks.events(),
        &observation_sink,
        scoped_effect_controller,
        cancel,
    )
    .with_local_cancel_origin_hint(cancel_origin_hint);
    let outcome = match writer.stream_selected_queued_work(opts, batch_ids).await {
        Ok(outcome) => outcome,
        Err(CoreSelectedQueuedWorkDrainError::Runtime(error)) => {
            return Err(error.into());
        }
        Err(CoreSelectedQueuedWorkDrainError::Refused { cause }) => {
            return Err(EmbedError::SelectedQueuedWorkDrainRefused {
                cause: selected_drain_refusal_cause(cause),
            });
        }
    };
    runtime.publish_from(&writer);
    Ok(selected_drain_outcome(outcome))
}

fn turn_options<'a>(
    events: Option<&'a dyn EventSink>,
    turn_events: &'a dyn TurnActivitySink,
    scoped_effect_controller: ScopedEffectController<'a>,
    cancel: CancellationToken,
) -> lash_core::facade_support::TurnOptions<'a> {
    let mut opts = lash_core::facade_support::TurnOptions::new(cancel, scoped_effect_controller);
    if let Some(events) = events {
        opts = opts.with_events(events);
    }
    opts.with_turn_events(turn_events)
}

struct SessionObservationTurnActivitySink<'a> {
    runtime: RuntimeHandle,
    live: Option<&'a dyn TurnActivitySink>,
}

#[async_trait]
impl TurnActivitySink for SessionObservationTurnActivitySink<'_> {
    fn is_noop(&self) -> bool {
        false
    }

    async fn emit(&self, activity: TurnActivity) {
        self.runtime.record_turn_activity(None, activity.clone());
        if let Some(live) = self.live {
            live.emit(activity).await;
        }
    }

    async fn emit_for_turn(&self, turn_id: &str, activity: TurnActivity) {
        self.runtime
            .record_turn_activity(Some(turn_id), activity.clone());
        if let Some(live) = self.live {
            live.emit_for_turn(turn_id, activity).await;
        }
    }
}

struct ChannelTurnActivitySink {
    tx: mpsc::Sender<Result<TurnActivity>>,
}

#[async_trait]
impl TurnActivitySink for ChannelTurnActivitySink {
    async fn emit(&self, activity: TurnActivity) {
        let _ = self.tx.send(Ok(activity)).await;
    }
}
fn validate_required_plugin_inputs(
    active_plugins: &[ActivePluginBinding],
    input: &TurnInput,
) -> Result<()> {
    for plugin in active_plugins {
        if plugin.requires_turn_input && !input.turn_context.has_plugin_input(plugin.id) {
            return Err(EmbedError::MissingPluginTurnInput {
                plugin_id: plugin.id,
            });
        }
    }
    Ok(())
}

pub(crate) async fn stream_prepared_turn(
    runtime: &RuntimeHandle,
    input: TurnInput,
    sinks: TurnSinks<'_>,
    scoped_effect_controller: ScopedEffectController<'_>,
    cancel: CancellationToken,
) -> Result<TurnReport> {
    let turn = Box::pin(stream_prepared_assembled(
        runtime,
        input,
        sinks,
        scoped_effect_controller,
        cancel,
    ))
    .await?;
    Ok(TurnReport::from_assembled(turn))
}

pub(crate) async fn stream_prepared_assembled(
    runtime: &RuntimeHandle,
    input: TurnInput,
    sinks: TurnSinks<'_>,
    scoped_effect_controller: ScopedEffectController<'_>,
    cancel: CancellationToken,
) -> Result<AssembledTurn> {
    let turn = Box::pin(stream_prepared_agent_frame_run(
        runtime,
        input,
        sinks,
        scoped_effect_controller,
        cancel,
    ))
    .await?;
    turn.into_final_turn().ok_or_else(|| {
        EmbedError::Runtime(lash_core::RuntimeError::new(
            RuntimeErrorCode::EmptyAgentFrameRun,
            "runtime completed without an assembled turn",
        ))
    })
}

pub(crate) async fn stream_prepared_agent_frame_run(
    runtime: &RuntimeHandle,
    input: TurnInput,
    sinks: TurnSinks<'_>,
    scoped_effect_controller: ScopedEffectController<'_>,
    cancel: CancellationToken,
) -> Result<lash_core::facade_support::AgentFrameRun> {
    let writer_handle = runtime.writer();
    let mut writer = writer_handle.lock().await;
    if let Some(extension) = input.protocol_extension.as_ref() {
        writer
            .validate_protocol_turn_extension(extension)
            .await
            .map_err(EmbedError::Session)?;
    }
    let observation_sink = SessionObservationTurnActivitySink {
        runtime: runtime.clone(),
        live: sinks.turn_events(),
    };
    let turn = Box::pin(writer.stream_turn_with_agent_frames(
        input,
        turn_options(
            sinks.events(),
            &observation_sink,
            scoped_effect_controller,
            cancel,
        ),
    ))
    .await?;
    runtime.publish_from(&writer);
    Ok(turn)
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
/// Report produced by turn.
pub struct TurnReport {
    /// Final runtime state for the turn.
    pub state: SessionSnapshot,
    /// Cancellation evidence, when the turn was cancelled, rides this outcome
    /// — read it with [`TurnReport::cancellation`].
    pub outcome: TurnOutcome,
    /// Assistant output committed by the turn.
    pub assistant_output: AssistantOutput,
    /// Parent's own LLM tokens for this turn. Does **not** include child
    /// sessions; see [`children_usage`](Self::children_usage) and
    /// [`total_usage`](Self::total_usage).
    pub usage: TokenUsage,
    /// Per-`(session, source, model)` ledger entries for child sessions whose
    /// LLM calls completed during this turn (subagents, compaction, observers,
    /// etc.). Two child sessions using the same source and model remain two
    /// rows. Empty unless the turn spawned children.
    #[serde(default)]
    pub children_usage: Vec<TokenLedgerEntry>,
    /// Provider calls made by the parent session during this turn, in protocol
    /// order. Child-session calls remain on each child's result. This is the
    /// complete lash-side model attribution surface: a turn has no single
    /// producing model, so lash exposes the per-call ledger and the host
    /// composes any higher-level view from it (ADR 0033).
    #[serde(default)]
    pub llm_calls: Vec<LlmCallRecord>,
    /// Bounded, non-transcript evidence from charge-safety-refused
    /// generations in this turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failure_evidence: Vec<lash_core::TurnFailureEvidence>,
    /// Tool calls issued by the turn in protocol order.
    pub tool_calls: Vec<ToolCallRecord>,
    /// Execution metadata collected for the turn.
    pub execution: TurnExecutionMetrics,
    /// Errors captured while settling the turn.
    pub errors: Vec<TurnIssue>,
    /// Durable acceptance identity of the input this turn was driven from.
    ///
    /// Every turn enters through one acceptance commit before anything executes
    /// (ADR 0069), and this is the same receipt
    /// [`EnqueueTurnBuilder::send`](crate::EnqueueTurnBuilder::send) returns: its
    /// `input_id` addresses the pending row and matches the settled application.
    /// Absent only for a store-less session, which cannot record an acceptance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance: Option<lash_core::runtime::TurnInputAcceptanceReceipt>,
    /// Undelivered inputs affected by this turn's cancellation policy.
    #[serde(
        default,
        skip_serializing_if = "lash_core::TurnCancelInputOutcome::is_empty"
    )]
    pub cancel_input_outcome: lash_core::TurnCancelInputOutcome,
}

impl TurnReport {
    fn from_assembled(turn: lash_core::facade_support::AssembledTurn) -> Self {
        // Keep this exhaustive so adding a core turn-result field forces the
        // facade projection to be reviewed alongside the remote projection.
        let lash_core::facade_support::AssembledTurn {
            state,
            turn_input_acceptance,
            turn_cancel_input_outcome,
            outcome,
            assistant_output,
            execution,
            token_usage,
            children_usage,
            llm_calls,
            tool_calls,
            failure_evidence,
            errors,
        } = turn;
        Self {
            state,
            outcome,
            assistant_output,
            usage: token_usage,
            children_usage,
            llm_calls,
            failure_evidence,
            tool_calls,
            execution,
            errors,
            acceptance: turn_input_acceptance,
            cancel_input_outcome: turn_cancel_input_outcome,
        }
    }

    /// Durable cancellation evidence, present exactly when this turn was
    /// cancelled. Cancellation evidence has no home other than the outcome.
    pub fn cancellation(&self) -> Option<&lash_core::facade_support::TurnCancellationEvidence> {
        self.outcome.cancellation()
    }

    /// Sum of parent's own LLM tokens and every child session's LLM tokens
    /// for this turn.
    pub fn total_usage(&self) -> TokenUsage {
        let mut total = self.usage.clone();
        for entry in &self.children_usage {
            total.input_tokens = total.input_tokens.saturating_add(entry.usage.input_tokens);
            total.output_tokens = total
                .output_tokens
                .saturating_add(entry.usage.output_tokens);
            total.cache_read_input_tokens = total
                .cache_read_input_tokens
                .saturating_add(entry.usage.cache_read_input_tokens);
            total.cache_write_input_tokens = total
                .cache_write_input_tokens
                .saturating_add(entry.usage.cache_write_input_tokens);
            total.reasoning_output_tokens = total
                .reasoning_output_tokens
                .saturating_add(entry.usage.reasoning_output_tokens);
        }
        total
    }

    /// Wall-clock instant the runtime started this turn (claim of the
    /// session-execution lease / queued-work claim), read from the runtime
    /// clock. Backed by [`TurnExecutionMetrics::started_at_ms`] on
    /// [`execution`](Self::execution).
    pub fn started_at(&self) -> std::time::SystemTime {
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(self.execution.started_at_ms)
    }

    /// Whole-turn duration — claim through final commit and post-persist
    /// hooks — measured on the runtime clock's monotonic source. Backed by
    /// [`TurnExecutionMetrics::duration_ms`] on [`execution`](Self::execution).
    pub fn duration(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.execution.duration_ms)
    }

    /// Returns the final assistant message when one was produced.
    pub fn assistant_message(&self) -> Option<&str> {
        match &self.outcome {
            TurnOutcome::Finished(lash_core::facade_support::TurnFinish::AssistantMessage {
                text,
            }) => Some(text),
            _ => None,
        }
    }

    /// Deserializes and returns the final structured value.
    pub fn final_value(&self) -> Option<&serde_json::Value> {
        match &self.outcome {
            TurnOutcome::Finished(lash_core::facade_support::TurnFinish::FinalValue { value }) => {
                Some(value)
            }
            _ => None,
        }
    }

    /// Deserializes and returns a tool result by call identifier.
    pub fn tool_value(&self) -> Option<(&str, &serde_json::Value)> {
        match &self.outcome {
            TurnOutcome::Finished(lash_core::facade_support::TurnFinish::ToolValue {
                tool_name,
                value,
            }) => Some((tool_name.as_str(), value)),
            _ => None,
        }
    }

    /// Returns whether the turn reached a successful terminal outcome.
    pub fn is_success(&self) -> bool {
        matches!(
            self.outcome,
            TurnOutcome::Finished(_) | TurnOutcome::AgentFrameSwitch { .. }
        )
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
/// Output produced by turn.
pub struct TurnOutput {
    /// Final settled report for the turn.
    pub result: TurnReport,
    /// Ordered activities observed while executing the turn.
    pub activities: Vec<TurnActivity>,
}

impl TurnOutput {
    /// Returns the final assistant message when one was produced.
    pub fn assistant_message(&self) -> Option<&str> {
        self.result.assistant_message()
    }

    /// Deserializes and returns the final structured value.
    pub fn final_value(&self) -> Option<&serde_json::Value> {
        self.result.final_value()
    }

    /// Deserializes and returns a tool result by call identifier.
    pub fn tool_value(&self) -> Option<(&str, &serde_json::Value)> {
        self.result.tool_value()
    }

    /// Returns whether the underlying turn reached a successful terminal outcome.
    pub fn is_success(&self) -> bool {
        self.result.is_success()
    }
}

struct BorrowedTurnActivityFanout<'a> {
    live: &'a dyn TurnActivitySink,
    collector: &'a RunActivityCollector,
}

#[async_trait]
impl TurnActivitySink for BorrowedTurnActivityFanout<'_> {
    async fn emit(&self, activity: TurnActivity) {
        self.live.emit(activity.clone()).await;
        self.collector.emit(activity).await;
    }
}

#[derive(Default)]
pub(crate) struct RunActivityCollector {
    activities: Arc<StdMutex<Vec<TurnActivity>>>,
}

impl RunActivityCollector {
    fn into_activities(self) -> Vec<TurnActivity> {
        self.activities.lock_recover().clone()
    }

    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> Vec<TurnActivity> {
        self.activities.lock_recover().clone()
    }
}

#[async_trait]
impl TurnActivitySink for RunActivityCollector {
    async fn emit(&self, activity: TurnActivity) {
        self.activities.lock_recover().push(activity);
    }
}

/// Fans a turn's activity stream out to multiple consumers.
pub struct TurnActivityFanout {
    sinks: Vec<Arc<dyn TurnActivitySink>>,
}

impl TurnActivityFanout {
    /// Creates a fanout that forwards each activity to every supplied sink.
    pub fn new(sinks: impl IntoIterator<Item = Arc<dyn TurnActivitySink>>) -> Self {
        Self {
            sinks: sinks.into_iter().collect(),
        }
    }
}

#[async_trait]
impl TurnActivitySink for TurnActivityFanout {
    async fn emit(&self, activity: TurnActivity) {
        for sink in &self.sinks {
            sink.emit(activity.clone()).await;
        }
    }
}

/// Returns the textual content of a message when it contains text.
pub fn message_text(message: &Message) -> String {
    message
        .parts
        .iter()
        .map(|part| part.content.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Returns the protocol role associated with a message.
pub fn message_role(message: &Message) -> &'static str {
    match message.role {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::System => "system",
        MessageRole::Event => "event",
    }
}
