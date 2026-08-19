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
        self
    }

    /// Install a process-local token with an opaque host-defined origin.
    ///
    /// Lash records the value without interpreting it.
    pub fn cancel_with_origin(mut self, cancel: CancellationToken, origin: Option<String>) -> Self {
        self.cancel = cancel;
        self.cancel_origin_hint = TurnCancelOriginHint::default();
        self.cancel_origin_hint.set(origin);
        self
    }

    pub fn protocol_turn_options(mut self, options: ProtocolTurnOptions) -> Self {
        self.protocol_turn_options = Some(options);
        self
    }

    pub fn provider(mut self, provider: ProviderHandle) -> Self {
        self.provider = Some(provider);
        self
    }

    pub fn turn_id(mut self, id: impl Into<String>) -> Self {
        self.turn_id = Some(id.into());
        self
    }

    pub fn prompt_template(mut self, template: PromptTemplate) -> Self {
        self.input.turn_context.set_prompt_template(template);
        self
    }

    pub fn prompt_contribution(mut self, contribution: PromptContribution) -> Self {
        self.input
            .turn_context
            .add_prompt_contribution(contribution);
        self
    }

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

    pub fn clear_prompt_slot(mut self, slot: PromptSlot) -> Self {
        self.input.turn_context.clear_prompt_slot(slot);
        self
    }

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

    pub fn effects(self, controller: &dyn RuntimeEffectController) -> ScopedTurnBuilder<'_> {
        ScopedTurnBuilder {
            builder: self,
            controller,
        }
    }

    pub async fn run(self) -> Result<TurnOutput> {
        let collector = RunActivityCollector::default();
        let result = self.stream_to(&collector).await?;
        Ok(TurnOutput {
            result,
            activities: collector.into_activities(),
        })
    }

    pub async fn stream_to(self, events: &dyn TurnActivitySink) -> Result<TurnResult> {
        let effect_host = Arc::clone(&self.effect_host);
        reject_controller_owned_replay_host(effect_host.as_ref(), "turn")?;
        self.stream_to_with_effect_host(events, effect_host.as_ref())
            .await
    }

    pub fn stream(self) -> Result<TurnStream> {
        let effect_host = Arc::clone(&self.effect_host);
        reject_controller_owned_replay_host(effect_host.as_ref(), "turn stream")?;
        self.stream_with_effect_host(effect_host.as_ref())
    }

    /// Access lower-level turn execution that bypasses the semantic
    /// [`TurnActivity`] tier.
    pub fn advanced(self) -> AdvancedTurn {
        AdvancedTurn { builder: self }
    }

    fn resolved_turn_id(&self) -> String {
        self.turn_id
            .clone()
            .or_else(|| self.input.trace_turn_id.clone())
            .unwrap_or_else(fresh_turn_id)
    }

    fn turn_scope(&self, turn_id: &str) -> lash_core::ExecutionScope {
        let observation = self.runtime.observe();
        observation.persisted_state.turn_scope(turn_id)
    }

    pub(crate) fn prepare(
        mut self,
        trace_turn_id: Option<String>,
    ) -> Result<(RuntimeHandle, TurnInput, CancellationToken, TurnCancelGuard)> {
        if let Some(options) = self.protocol_turn_options {
            self.input.protocol_turn_options = Some(options);
        }
        if let Some(provider) = self.provider {
            self.input.turn_context.set_provider(provider);
        }
        if let Some(trace_turn_id) = trace_turn_id {
            self.input.trace_turn_id = Some(trace_turn_id);
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
    ) -> Result<TurnResult> {
        let turn_id = self.resolved_turn_id();
        let scoped_effect_controller = effect_host.scoped(self.turn_scope(&turn_id))?;
        self.stream_to_with_scope(events, scoped_effect_controller, Some(turn_id))
            .await
    }

    async fn stream_to_with_effect_controller(
        self,
        events: &dyn TurnActivitySink,
        controller: &dyn RuntimeEffectController,
    ) -> Result<TurnResult> {
        let turn_id = self.resolved_turn_id();
        let scoped_effect_controller =
            ScopedEffectController::borrowed(controller, self.turn_scope(&turn_id))?;
        self.stream_to_with_scope(events, scoped_effect_controller, Some(turn_id))
            .await
    }

    async fn stream_to_with_scope(
        self,
        events: &dyn TurnActivitySink,
        scoped_effect_controller: ScopedEffectController<'_>,
        trace_turn_id: Option<String>,
    ) -> Result<TurnResult> {
        let (runtime, input, cancel, _cancel_guard) = self.prepare(trace_turn_id)?;
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
        let turn_id = self.resolved_turn_id();
        let scoped_effect_controller = effect_host
            .scoped_static(self.turn_scope(&turn_id))?
            .ok_or(EmbedError::StaticTurnStreamRequiresStaticEffectHost)?;
        self.stream_with_scope(scoped_effect_controller, Some(turn_id))
    }

    fn stream_with_scope(
        self,
        scoped_effect_controller: ScopedEffectController<'static>,
        trace_turn_id: Option<String>,
    ) -> Result<TurnStream> {
        let (runtime, input, cancel, cancel_guard) = self.prepare(trace_turn_id)?;
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

    pub fn cancel_with_origin(mut self, cancel: CancellationToken, origin: Option<String>) -> Self {
        self.builder = self.builder.cancel_with_origin(cancel, origin);
        self
    }

    pub fn protocol_turn_options(mut self, options: ProtocolTurnOptions) -> Self {
        self.builder = self.builder.protocol_turn_options(options);
        self
    }

    pub fn provider(mut self, provider: ProviderHandle) -> Self {
        self.builder = self.builder.provider(provider);
        self
    }

    pub fn turn_id(mut self, id: impl Into<String>) -> Self {
        self.builder = self.builder.turn_id(id);
        self
    }

    pub fn prompt_template(mut self, template: PromptTemplate) -> Self {
        self.builder = self.builder.prompt_template(template);
        self
    }

    pub fn prompt_contribution(mut self, contribution: PromptContribution) -> Self {
        self.builder = self.builder.prompt_contribution(contribution);
        self
    }

    pub fn replace_prompt_slot(
        mut self,
        slot: PromptSlot,
        contributions: impl IntoIterator<Item = PromptContribution>,
    ) -> Self {
        self.builder = self.builder.replace_prompt_slot(slot, contributions);
        self
    }

    pub fn clear_prompt_slot(mut self, slot: PromptSlot) -> Self {
        self.builder = self.builder.clear_prompt_slot(slot);
        self
    }

    pub fn prompt_layer(mut self, layer: PromptLayer) -> Self {
        self.builder = self.builder.prompt_layer(layer);
        self
    }

    pub fn with_plugin_input<P: PluginBinding>(mut self, input: P::Input) -> Self {
        self.builder = self.builder.with_plugin_input::<P>(input);
        self
    }

    pub async fn run(self) -> Result<TurnOutput> {
        let collector = RunActivityCollector::default();
        let result = self.stream_to(&collector).await?;
        Ok(TurnOutput {
            result,
            activities: collector.into_activities(),
        })
    }

    pub async fn stream_to(self, events: &dyn TurnActivitySink) -> Result<TurnResult> {
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

    pub async fn stream_to_with_scope(
        self,
        events: &dyn TurnActivitySink,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<TurnResult> {
        let trace_turn_id = trace_turn_id_for_scope(&self.builder, &scoped_effect_controller);
        self.builder
            .stream_to_with_scope(events, scoped_effect_controller, trace_turn_id)
            .await
    }

    pub fn stream_with_scope(
        self,
        scoped_effect_controller: ScopedEffectController<'static>,
    ) -> Result<TurnStream> {
        let trace_turn_id = trace_turn_id_for_scope(&self.builder, &scoped_effect_controller);
        self.builder
            .stream_with_scope(scoped_effect_controller, trace_turn_id)
    }

    /// Run the turn while sending raw lower-level runtime events to `events`.
    pub async fn collect_session_events_with_scope(
        self,
        events: &dyn EventSink,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<TurnResult> {
        let trace_turn_id = trace_turn_id_for_scope(&self.builder, &scoped_effect_controller);
        let (runtime, input, cancel, _cancel_guard) = self.builder.prepare(trace_turn_id)?;
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

pub struct TurnStream {
    activities: mpsc::Receiver<Result<TurnActivity>>,
    completion: JoinHandle<Result<TurnResult>>,
}

impl TurnStream {
    pub async fn next_activity(&mut self) -> Option<Result<TurnActivity>> {
        self.activities.recv().await
    }

    pub async fn finish(self) -> Result<TurnResult> {
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

pub struct QueuedTurnBuilder {
    pub(crate) runtime: RuntimeHandle,
    pub(crate) effect_host: Arc<dyn EffectHost>,
    pub(crate) cancel: CancellationToken,
    pub(crate) cancel_origin_hint: TurnCancelOriginHint,
    pub(crate) cancels: TurnCancelRegistry,
    pub(crate) drain_id: Option<String>,
}

impl QueuedTurnBuilder {
    /// Install a process-local token whose origin is unknown.
    pub fn cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self.cancel_origin_hint = TurnCancelOriginHint::default();
        self
    }

    /// Install a process-local token with an opaque host-defined origin.
    ///
    /// Lash records the value without interpreting it.
    pub fn cancel_with_origin(mut self, cancel: CancellationToken, origin: Option<String>) -> Self {
        self.cancel = cancel;
        self.cancel_origin_hint = TurnCancelOriginHint::default();
        self.cancel_origin_hint.set(origin);
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

    pub fn drain_id(mut self, drain_id: impl Into<String>) -> Self {
        self.drain_id = Some(drain_id.into());
        self
    }

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

    pub async fn stream_to(
        self,
        events: &dyn TurnActivitySink,
    ) -> Result<QueuedTurnDrain<TurnResult>> {
        let effect_host = Arc::clone(&self.effect_host);
        reject_controller_owned_replay_host(effect_host.as_ref(), "queued turn")?;
        self.stream_to_with_effect_host(events, effect_host.as_ref())
            .await
    }

    pub fn advanced(self) -> AdvancedQueuedTurn {
        AdvancedQueuedTurn { builder: self }
    }

    fn resolved_drain_id(&self) -> String {
        self.drain_id.clone().unwrap_or_else(fresh_queue_drain_id)
    }

    async fn stream_to_with_effect_host(
        self,
        events: &dyn TurnActivitySink,
        effect_host: &dyn EffectHost,
    ) -> Result<QueuedTurnDrain<TurnResult>> {
        let drain_id = self.resolved_drain_id();
        let scope = self
            .runtime
            .observe()
            .persisted_state
            .queue_drain_scope(drain_id);
        let scoped_effect_controller = effect_host.scoped(scope)?;
        self.stream_to_with_scope(events, scoped_effect_controller)
            .await
    }

    async fn stream_to_with_effect_controller(
        self,
        events: &dyn TurnActivitySink,
        controller: &dyn RuntimeEffectController,
    ) -> Result<QueuedTurnDrain<TurnResult>> {
        let drain_id = self.resolved_drain_id();
        let scope = self
            .runtime
            .observe()
            .persisted_state
            .queue_drain_scope(drain_id);
        let scoped_effect_controller = ScopedEffectController::borrowed(controller, scope)?;
        self.stream_to_with_scope(events, scoped_effect_controller)
            .await
    }

    async fn stream_to_with_scope(
        self,
        events: &dyn TurnActivitySink,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<QueuedTurnDrain<TurnResult>> {
        let Self {
            runtime,
            effect_host: _,
            cancel,
            cancel_origin_hint,
            cancels,
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

    /// Sets the effect-scope identity for any selected turn. By default Lash
    /// uses the first batch ID, or a fresh identity for an empty selection.
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
    ) -> Result<SelectedQueuedWorkDrainOutcome<TurnResult>> {
        let effect_host = Arc::clone(&self.builder.effect_host);
        reject_controller_owned_replay_host(effect_host.as_ref(), "selected queued turn")?;
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
    ) -> Result<SelectedQueuedWorkDrainOutcome<TurnResult>> {
        let drain_id = self.resolved_drain_id();
        let scope = self
            .builder
            .runtime
            .observe()
            .persisted_state
            .queue_drain_scope(drain_id);
        let scoped_effect_controller = effect_host.scoped(scope)?;
        self.stream_to_with_scope(events, scoped_effect_controller)
            .await
    }

    async fn stream_to_with_effect_controller(
        self,
        events: &dyn TurnActivitySink,
        controller: &dyn RuntimeEffectController,
    ) -> Result<SelectedQueuedWorkDrainOutcome<TurnResult>> {
        let drain_id = self.resolved_drain_id();
        let scope = self
            .builder
            .runtime
            .observe()
            .persisted_state
            .queue_drain_scope(drain_id);
        let scoped_effect_controller = ScopedEffectController::borrowed(controller, scope)?;
        self.stream_to_with_scope(events, scoped_effect_controller)
            .await
    }

    async fn stream_to_with_scope(
        self,
        events: &dyn TurnActivitySink,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<SelectedQueuedWorkDrainOutcome<TurnResult>> {
        let Self { builder, batch_ids } = self;
        let QueuedTurnBuilder {
            runtime,
            effect_host: _,
            cancel,
            cancel_origin_hint,
            cancels,
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

pub struct ScopedQueuedTurnBuilder<'run> {
    builder: QueuedTurnBuilder,
    controller: &'run dyn RuntimeEffectController,
}

impl<'run> ScopedQueuedTurnBuilder<'run> {
    pub fn cancel(mut self, cancel: CancellationToken) -> Self {
        self.builder = self.builder.cancel(cancel);
        self
    }

    pub fn cancel_with_origin(mut self, cancel: CancellationToken, origin: Option<String>) -> Self {
        self.builder = self.builder.cancel_with_origin(cancel, origin);
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

    pub fn drain_id(mut self, drain_id: impl Into<String>) -> Self {
        self.builder = self.builder.drain_id(drain_id);
        self
    }

    pub async fn run(self) -> Result<QueuedTurnDrain<TurnOutput>> {
        let collector = RunActivityCollector::default();
        Ok(self.stream_to(&collector).await?.map(|result| TurnOutput {
            result,
            activities: collector.into_activities(),
        }))
    }

    pub async fn stream_to(
        self,
        events: &dyn TurnActivitySink,
    ) -> Result<QueuedTurnDrain<TurnResult>> {
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

    /// Sets the durable effect-scope identity for any selected turn.
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
    ) -> Result<SelectedQueuedWorkDrainOutcome<TurnResult>> {
        self.builder
            .stream_to_with_effect_controller(events, self.controller)
            .await
    }
}

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
    ) -> Result<SelectedQueuedWorkDrainOutcome<TurnResult>> {
        self.builder
            .stream_to_with_scope(events, scoped_effect_controller)
            .await
    }
}

impl AdvancedQueuedTurn {
    pub async fn stream_to_with_scope(
        self,
        events: &dyn TurnActivitySink,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<QueuedTurnDrain<TurnResult>> {
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

fn trace_turn_id_for_scope(
    builder: &TurnBuilder,
    scoped_effect_controller: &ScopedEffectController<'_>,
) -> Option<String> {
    if scoped_effect_controller
        .execution_scope()
        .validates_turn_trace_id()
    {
        Some(
            builder
                .turn_id
                .clone()
                .unwrap_or_else(|| scoped_effect_controller.scope_id().to_string()),
        )
    } else {
        builder
            .turn_id
            .clone()
            .or_else(|| builder.input.trace_turn_id.clone())
    }
}

fn reject_controller_owned_replay_host(
    effect_host: &dyn EffectHost,
    operation: &'static str,
) -> Result<()> {
    if effect_host.replay_ownership() == EffectReplayOwnership::Controller {
        return Err(EmbedError::DurableEffectHostRequiresHandlerContext { operation });
    }
    Ok(())
}

pub(crate) async fn stream_next_queued_prepared_turn(
    runtime: &RuntimeHandle,
    sinks: TurnSinks<'_>,
    scoped_effect_controller: ScopedEffectController<'_>,
    cancel: CancellationToken,
    cancel_origin_hint: TurnCancelOriginHint,
) -> Result<QueuedTurnDrain<TurnResult>> {
    let drain = Box::pin(stream_next_queued_prepared_assembled(
        runtime,
        sinks,
        scoped_effect_controller,
        cancel,
        cancel_origin_hint,
    ))
    .await?;
    Ok(drain.map(TurnResult::from_assembled))
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
) -> Result<SelectedQueuedWorkDrainOutcome<TurnResult>> {
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
        turn: outcome.turn.map(TurnResult::from_assembled),
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
) -> Result<TurnResult> {
    let turn = Box::pin(stream_prepared_assembled(
        runtime,
        input,
        sinks,
        scoped_effect_controller,
        cancel,
    ))
    .await?;
    Ok(TurnResult::from_assembled(turn))
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
pub struct TurnResult {
    pub state: SessionSnapshot,
    pub outcome: TurnOutcome,
    /// Durable request evidence, present exactly when `outcome` is cancelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancellation: Option<lash_core::facade_support::TurnCancellationEvidence>,
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
    pub tool_calls: Vec<ToolCallRecord>,
    pub execution: ExecutionSummary,
    pub errors: Vec<TurnIssue>,
}

impl TurnResult {
    fn from_assembled(turn: lash_core::facade_support::AssembledTurn) -> Self {
        Self {
            state: turn.state,
            outcome: turn.outcome,
            cancellation: turn.cancellation,
            assistant_output: turn.assistant_output,
            usage: turn.token_usage,
            children_usage: turn.children_usage,
            llm_calls: turn.llm_calls,
            tool_calls: turn.tool_calls,
            execution: turn.execution,
            errors: turn.errors,
        }
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
    /// clock. Backed by [`ExecutionSummary::started_at_ms`] on
    /// [`execution`](Self::execution).
    pub fn started_at(&self) -> std::time::SystemTime {
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(self.execution.started_at_ms)
    }

    /// Whole-turn duration — claim through final commit and post-persist
    /// hooks — measured on the runtime clock's monotonic source. Backed by
    /// [`ExecutionSummary::duration_ms`] on [`execution`](Self::execution).
    pub fn duration(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.execution.duration_ms)
    }

    pub fn assistant_message(&self) -> Option<&str> {
        match &self.outcome {
            TurnOutcome::Finished(lash_core::facade_support::TurnFinish::AssistantMessage {
                text,
            }) => Some(text),
            _ => None,
        }
    }

    pub fn final_value(&self) -> Option<&serde_json::Value> {
        match &self.outcome {
            TurnOutcome::Finished(lash_core::facade_support::TurnFinish::FinalValue { value }) => {
                Some(value)
            }
            _ => None,
        }
    }

    pub fn tool_value(&self) -> Option<(&str, &serde_json::Value)> {
        match &self.outcome {
            TurnOutcome::Finished(lash_core::facade_support::TurnFinish::ToolValue {
                tool_name,
                value,
            }) => Some((tool_name.as_str(), value)),
            _ => None,
        }
    }

    pub fn is_success(&self) -> bool {
        matches!(
            self.outcome,
            TurnOutcome::Finished(_) | TurnOutcome::AgentFrameSwitch { .. }
        )
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TurnOutput {
    pub result: TurnResult,
    pub activities: Vec<TurnActivity>,
}

impl TurnOutput {
    pub fn assistant_message(&self) -> Option<&str> {
        self.result.assistant_message()
    }

    pub fn final_value(&self) -> Option<&serde_json::Value> {
        self.result.final_value()
    }

    pub fn tool_value(&self) -> Option<(&str, &serde_json::Value)> {
        self.result.tool_value()
    }

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

pub struct TurnActivityFanout {
    sinks: Vec<Arc<dyn TurnActivitySink>>,
}

impl TurnActivityFanout {
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

pub fn message_text(message: &Message) -> String {
    message
        .parts
        .iter()
        .map(|part| part.content.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn message_role(message: &Message) -> &'static str {
    match message.role {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::System => "system",
        MessageRole::Event => "event",
    }
}
