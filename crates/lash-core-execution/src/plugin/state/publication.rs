//! The K10 publication coordinator (binding Q5).
//!
//! A recorded body collects the command batches its tool result and its
//! permitted callbacks propose. When it completes successfully the
//! coordinator reduces them privately, against the published namespace and
//! in proposal order, and the body's outcome carries the resolutions with
//! its result. The engine records that outcome; once it returns it, the
//! coordinator publishes the resolutions under each namespace's frontier.
//! A served replay of the same outcome publishes the same resolutions and
//! runs no reducer.
//!
//! A tool call's resolutions are an admitted member's (ADR 0132 §5): its
//! decision stages them ([`StagedPluginState`]), they ride the member's
//! `x_outcome` record into the transaction that commits it, and the member's
//! lifecycle publishes them from the committed record, only once that
//! commit is acknowledged; a resume publishes them from the same record
//! before the round is presented. Nothing a member reduced is visible
//! before its outcome is durable. A turn's after-turn callbacks stage theirs
//! the same way: they ride the turn's head commit in its `turn.commit`, and
//! publish once that commit is acknowledged (FIG-5283).
//!
//! A namespace has at most one reduced publication the engine has not yet
//! returned. The next reduction of that namespace waits for it, so it never
//! reduces against unrecorded work; a body that proposes nothing, or only
//! for other namespaces, never waits. For two members of one round that
//! change one namespace this is the composition rule: they apply in the
//! order they reduce, and the second reduces only once the first's outcome
//! committed and published, against that committed value. Neither reads the
//! other's uncommitted state, and each committed record is one step of the
//! namespace's frontier.
use super::*;
use crate::plugin::FormatVersion;
use crate::{RuntimeEffectControllerError, RuntimeEffectKind, RuntimeEffectOutcome};
use lash_core_store::store::plugin_writers::PluginCallbackIdentity;
use lash_core_store::tool_run::{
    FrontierStep, ReducerRefusal, StateCommandBatch, StateCommandLimits,
};
use std::future::Future;

/// The resolutions one recorded body published, retained with its result.
///
/// Effect-host implementors retain this with the outcome. Replay installs
/// the resolutions on the original owner before returning the recorded
/// result; it runs no body, callback or reducer.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginStateEffect {
    pub owner: crate::RuntimeOwner,
    pub address: crate::EffectAddress,
    pub resolutions: Vec<StateResolution>,
}

/// What a reducer resolves: one key's current candidate value and the
/// command's input.
#[derive(Clone, Copy, Debug)]
pub struct StateReduction<'a> {
    pub key: &'a str,
    pub current: Option<&'a Value>,
    pub input: &'a Value,
}

/// A plugin's pure read-modify-write for [`StateCommand::Apply`]: the key's
/// next value (`None` removes it) or a typed refusal, which rejects the
/// whole batch. It sees nothing but its [`StateReduction`]; it is a
/// behaviour of the registered plugin, and a recorded resolution never runs
/// it again.
pub type StateReducer =
    Arc<dyn Fn(StateReduction<'_>) -> Result<Option<Value>, HookCause> + Send + Sync>;

/// One command batch proposed inside a recorded body.
pub struct Proposal {
    pub(crate) batch: StateCommandBatch,
    /// The callback that proposed it, whose slot must hold state authority;
    /// `None` for a tool body's own result.
    pub(crate) proposer: Option<PluginCallbackIdentity>,
}

impl Proposal {
    /// A callback's commands, attributed to the callback.
    pub(crate) fn for_callback(
        callback: &PluginCallbackIdentity,
        origin: StateCommandOrigin,
        commands: StateCommands,
    ) -> Self {
        Self {
            batch: StateCommandBatch {
                plugin: callback.owner.clone(),
                origin,
                commands: commands.into_commands(),
            },
            proposer: Some(callback.clone()),
        }
    }

    /// A tool body's commands against its owning plugin's namespace.
    pub(crate) fn for_tool(
        owner: String,
        origin: StateCommandOrigin,
        commands: StateCommands,
    ) -> Self {
        Self {
            batch: StateCommandBatch {
                plugin: owner,
                origin,
                commands: commands.into_commands(),
            },
            proposer: None,
        }
    }
}

struct Sink {
    key: usize,
    proposals: Vec<Proposal>,
}

tokio::task_local! {
    static SINK: Arc<Mutex<Sink>>;
}

/// Hand `proposal` to the recorded body running on this task for `plugins`.
///
/// # Errors
///
/// [`PluginStateError::Unrecorded`] when no recorded body of this session
/// runs here: nothing could carry the resolution.
pub(crate) fn propose(
    plugins: &dyn super::super::ExecutionState,
    proposal: Proposal,
) -> Result<(), PluginStateError> {
    let plugin = proposal.batch.plugin.clone();
    let mut pending = Some(proposal);
    let _ = SINK.try_with(|sink| {
        let mut sink = sink.lock_recover();
        if sink.key == plugins.proposal_key()
            && let Some(proposal) = pending.take()
        {
            sink.proposals.push(proposal);
        }
    });
    match pending {
        None => Ok(()),
        Some(_) => Err(PluginStateError::Unrecorded { plugin }),
    }
}

/// [`propose`] every proposal, in order.
pub(crate) fn propose_all(
    plugins: &dyn super::super::ExecutionState,
    proposals: Vec<Proposal>,
) -> Result<(), PluginStateError> {
    proposals
        .into_iter()
        .try_for_each(|proposal| propose(plugins, proposal))
}

/// Run `body`, collecting what it proposes for `plugins` instead of handing
/// it to an enclosing recorded body: for a callback whose decision is
/// recorded by a step of its own.
pub(crate) async fn collect_proposals<F: Future>(
    plugins: &dyn super::super::ExecutionState,
    body: F,
) -> (F::Output, Vec<Proposal>) {
    let sink = Arc::new(Mutex::new(Sink {
        key: plugins.proposal_key(),
        proposals: Vec::new(),
    }));
    let output = SINK.scope(Arc::clone(&sink), body).await;
    let proposals = std::mem::take(&mut sink.lock_recover().proposals);
    (output, proposals)
}

/// Run a recorded body and attach the resolutions of what it proposed.
///
/// An attempt fault or a recorded failure publishes nothing: eligibility is
/// the body's final result (Q5 §8).
pub(crate) async fn record_effect<F>(
    plugins: Arc<dyn super::super::ExecutionState>,
    kind: RuntimeEffectKind,
    address: crate::EffectAddress,
    body: F,
) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError>
where
    F: Future<Output = Result<RuntimeEffectOutcome, RuntimeEffectControllerError>>,
{
    let segment = plugins.state_segment();
    let (result, proposals) = collect_proposals(plugins.as_ref(), body).await;
    if proposals.is_empty() || result.is_err() {
        return result;
    }
    let resolutions = Box::pin(plugins.reduce_proposals(&address, segment, proposals))
        .await
        .map_err(fenced_fault)?;
    Ok(RuntimeEffectOutcome::PluginState {
        kind,
        state: Box::new(PluginStateEffect {
            owner: plugins.owner().clone(),
            address,
            resolutions,
        }),
        result: Box::new(result),
    })
}

/// A reduction a fenced namespace refused: never recorded, so the body's
/// result is not replaced, and the invocation's recovery rebuilds the
/// namespace from durable state.
fn fenced_fault(error: PluginStateError) -> RuntimeEffectControllerError {
    let PluginStateError::PublicationFenced { plugin } = &error else {
        return lash_execution_contracts::plugin_error::PluginError::State(error).into();
    };
    let mut fault = RuntimeEffectControllerError::new(
        crate::RuntimeErrorCode::PluginSessionManager,
        error.to_string(),
    );
    fault.cause = Some(crate::RuntimeErrorCause::PluginStatePublicationFenced {
        plugin: plugin.clone(),
    });
    fault.retryable_uncommitted_derivation()
}

/// The publication of one recorded effect's outcome, from the frame that
/// awaits the engine's answer. Dropping it unsettled fences whatever the
/// effect reduced: the engine may have recorded it.
///
/// The engine's seam for a frame that executes an effect through a
/// controller directly; `core_internal` re-exports it.
pub struct EffectPublication {
    plugins: Arc<dyn super::super::ExecutionState>,
    address: crate::EffectAddress,
    settled: bool,
}

impl EffectPublication {
    /// Begin the publication of the effect at `address` for `plugins`.
    pub fn begin(
        plugins: Arc<dyn super::super::ExecutionState>,
        address: crate::EffectAddress,
    ) -> Self {
        Self {
            plugins,
            address,
            settled: false,
        }
    }

    /// Stage `resolutions`, what a tool call's decision reduced, to commit
    /// with the call's outcome: its namespaces stay reserved for the call
    /// until that outcome's committed record publishes them.
    pub(crate) fn stage(mut self, resolutions: Vec<StateResolution>) -> StagedPluginState {
        self.settled = true;
        StagedPluginState {
            resolutions,
            reservation: Some(Arc::new(StateReservation {
                plugins: Arc::clone(&self.plugins),
                address: self.address.clone(),
            })),
        }
    }

    /// Publish the resolutions `outcome` carries and return its result.
    ///
    /// # Errors
    ///
    /// The body's own recorded error, an owner mismatch, or a resolution
    /// its namespace's frontier refuses.
    pub fn publish(
        mut self,
        outcome: RuntimeEffectOutcome,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        self.settled = true;
        self.plugins.publish_recorded(&self.address, outcome)
    }
}

impl Drop for EffectPublication {
    fn drop(&mut self) {
        if !self.settled {
            self.plugins.abandon_publication(&self.address);
        }
    }
}

/// A tool call's plugin-state resolutions, reduced by its decision and
/// staged to commit with its outcome (ADR 0132 §5): the store-local effect
/// [`StoreLocalEffect::PluginState`](crate::runtime::actor::round::StoreLocalEffect::PluginState).
///
/// Its namespaces stay reserved for the call until the outcome's committed
/// record publishes the resolutions. Dropped otherwise, it fences them: the
/// outcome may be durable. A call that ends without carrying it to an
/// outcome [`discard`](Self::discard)s it, which publishes nothing.
#[derive(Clone)]
pub struct StagedPluginState {
    resolutions: Vec<StateResolution>,
    reservation: Option<Arc<StateReservation>>,
}

impl StagedPluginState {
    /// The resolutions, as the outcome's record carries them.
    #[must_use]
    pub fn resolutions(&self) -> &[StateResolution] {
        &self.resolutions
    }

    /// Release the reservation without publishing: the call ended without
    /// an outcome that carries these resolutions, so none is durable.
    pub fn discard(self) {
        if let Some(reservation) = &self.reservation {
            reservation
                .plugins
                .release_publication(&reservation.address);
        }
    }
}

impl std::fmt::Debug for StagedPluginState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StagedPluginState")
            .field("resolutions", &self.resolutions)
            .finish_non_exhaustive()
    }
}

impl PartialEq for StagedPluginState {
    fn eq(&self, other: &Self) -> bool {
        self.resolutions == other.resolutions
    }
}

impl Eq for StagedPluginState {}

/// The namespaces one call's staged resolutions reserved. Dropped while
/// they are still reserved, it fences them.
struct StateReservation {
    plugins: Arc<dyn super::super::ExecutionState>,
    address: crate::EffectAddress,
}

impl Drop for StateReservation {
    fn drop(&mut self) {
        self.plugins.abandon_publication(&self.address);
    }
}

const REDUCER_PANIC: &str = "lash.state_reducer_panicked";

/// What admits and reduces a state owner's batches: the plugins a session
/// installed, or the components a process's environment names.
pub(in crate::plugin) trait StateAuthority: Send + Sync {
    /// The state format `plugin` declares here.
    fn declared(&self, plugin: &str) -> Option<FormatVersion>;
    /// `plugin`'s reducer `name`.
    fn reducer(&self, plugin: &str, name: &str) -> Option<StateReducer>;
    /// Whether `plugin` may publish state at all.
    fn keeps_state(&self, _plugin: &str) -> bool {
        true
    }
}

/// One owner's published state, as its publication coordinator reads and
/// writes it: a session's plugin namespaces or a process's component
/// namespaces.
pub(in crate::plugin) struct StateDesk<'a> {
    pub(in crate::plugin) state: &'a Arc<std::sync::Mutex<PluginStateRegistry>>,
    pub(in crate::plugin) owner: &'a crate::RuntimeOwner,
    pub(in crate::plugin) authority: &'a dyn StateAuthority,
    /// The deployment's bounds on one batch of state commands.
    pub(in crate::plugin) limits: StateCommandLimits,
}

impl crate::PluginSession {
    /// Hand a callback's commands to the recorded body running it.
    pub(crate) fn propose_callback_state(
        &self,
        callback: &PluginCallbackIdentity,
        origin: StateCommandOrigin,
        commands: StateCommands,
    ) -> Result<(), PluginStateError> {
        if commands.is_empty() {
            return Ok(());
        }
        propose(self, Proposal::for_callback(callback, origin, commands))
    }

    pub(in crate::plugin) fn state_desk(&self) -> StateDesk<'_> {
        StateDesk {
            state: &self.state,
            owner: &self.owner,
            authority: self,
            limits: self.host.data_limits().plugin_state,
        }
    }

    pub(crate) fn state_segment(&self) -> SegmentOrdinal {
        self.state_desk().state_segment()
    }

    pub(crate) async fn reduce_proposals(
        &self,
        address: &crate::EffectAddress,
        segment: SegmentOrdinal,
        proposals: Vec<Proposal>,
    ) -> Result<Vec<StateResolution>, PluginStateError> {
        self.state_desk()
            .reduce_proposals(address, segment, proposals)
            .await
    }

    /// Publish the resolutions a recorded outcome carries and return the
    /// body's result. Each resolution applies at most once, in its
    /// namespace's order; the whole outcome validates before any namespace
    /// changes.
    pub fn publish_effect_state(
        &self,
        outcome: RuntimeEffectOutcome,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        self.state_desk().publish_effect_state(outcome)
    }

    pub(crate) fn publish_recorded(
        &self,
        address: &crate::EffectAddress,
        outcome: RuntimeEffectOutcome,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        self.state_desk().publish_recorded(address, outcome)
    }

    /// Publish `resolutions`, what committed outcome records carry, each
    /// under the effect that reduced it, and release what that effect
    /// reserved.
    ///
    /// # Errors
    ///
    /// A resolution its namespace's frontier refuses.
    pub fn publish_committed_state(
        &self,
        resolutions: &[StateResolution],
    ) -> Result<(), RuntimeEffectControllerError> {
        self.state_desk().publish_committed_state(resolutions)
    }

    #[cfg(test)]
    pub(crate) fn publish_run_resolutions(
        &self,
        address: &crate::EffectAddress,
        resolutions: Vec<StateResolution>,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.state_desk()
            .publish_run_resolutions(address, resolutions)
    }

    pub(crate) fn state_with(
        &self,
        staged: &[StateResolution],
    ) -> Result<PluginState, PluginStateError> {
        self.state_desk().state_with(staged)
    }

    pub(crate) fn release_publication(&self, address: &crate::EffectAddress) {
        self.state_desk().release_publication(address);
    }

    pub(crate) fn abandon_publication(&self, address: &crate::EffectAddress) {
        self.state_desk().abandon_publication(address);
    }

    /// The segment that owns publication from now on: a recorded resolution
    /// of an earlier segment no longer applies (K6, Q5 §6).
    pub fn adopt_state_segment(&self, segment: SegmentOrdinal) {
        self.state_desk().adopt_state_segment(segment);
    }
}

impl StateAuthority for crate::PluginSession {
    fn declared(&self, plugin: &str) -> Option<FormatVersion> {
        self.host
            .factories()
            .iter()
            .map(|factory| factory.plugin_declaration())
            .find(|declaration| declaration.id.as_str() == plugin)
            .map(|declaration| declaration.format_version)
    }

    fn reducer(&self, plugin: &str, name: &str) -> Option<StateReducer> {
        self.capabilities
            .get()
            .and_then(|capabilities| capabilities.contributions.state_reducers.get(plugin))
            .and_then(|reducers| reducers.get(name))
            .cloned()
    }

    fn keeps_state(&self, plugin: &str) -> bool {
        self.host.keeps_state(plugin)
    }
}

impl StateDesk<'_> {
    /// The segment that owns publication now.
    pub(crate) fn state_segment(&self) -> SegmentOrdinal {
        self.state.lock_recover().segment
    }

    /// Reduce `proposals` once every namespace they address has no
    /// unreturned publication of another effect, and reserve those
    /// namespaces for `address` until its outcome is published.
    pub(crate) async fn reduce_proposals(
        &self,
        address: &crate::EffectAddress,
        segment: SegmentOrdinal,
        proposals: Vec<Proposal>,
    ) -> Result<Vec<StateResolution>, PluginStateError> {
        let namespaces: BTreeSet<String> = proposals
            .iter()
            .map(|proposal| proposal.batch.plugin.clone())
            .collect();
        let settled = Arc::clone(&self.state.lock_recover().settled);
        loop {
            let notified = settled.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut registry = self.state.lock_recover();
                if let Some(plugin) = namespaces
                    .iter()
                    .find(|namespace| registry.fenced.contains(*namespace))
                {
                    return Err(PluginStateError::PublicationFenced {
                        plugin: plugin.clone(),
                    });
                }
                let busy = namespaces.iter().any(|namespace| {
                    registry.owed.contains_key(namespace)
                        || registry
                            .reserved
                            .get(namespace)
                            .is_some_and(|holder| holder != address)
                });
                if !busy {
                    for namespace in &namespaces {
                        registry.reserved.insert(namespace.clone(), address.clone());
                    }
                    return Ok(self.resolve(&mut registry, address, segment, proposals));
                }
            }
            notified.await;
        }
    }

    /// Reduce each batch in order against the published namespace and the
    /// batches before it, privately.
    fn resolve(
        &self,
        registry: &mut PluginStateRegistry,
        address: &crate::EffectAddress,
        segment: SegmentOrdinal,
        proposals: Vec<Proposal>,
    ) -> Vec<StateResolution> {
        let mut candidates: BTreeMap<String, PluginNamespaceState> = BTreeMap::new();
        proposals
            .into_iter()
            .map(|Proposal { batch, proposer }| {
                let plugin = batch.plugin.clone();
                let namespace = candidates.entry(plugin.clone()).or_insert_with(|| {
                    registry
                        .data
                        .plugins
                        .get(&plugin)
                        .cloned()
                        .unwrap_or_default()
                });
                namespace.publication.owner_segment = registry.segment;
                let mut outcome = match self.admit_batch(&batch, proposer.as_ref(), namespace) {
                    Err(refusal) => StateResolutionOutcome::Refused { refusal },
                    Ok(()) => batch.reduce(
                        &namespace.values,
                        &mut |key, name, current, input| {
                            self.run_reducer(
                                &plugin,
                                name,
                                StateReduction {
                                    key,
                                    current,
                                    input,
                                },
                            )
                        },
                        self.limits,
                    ),
                };
                let reduced = match &outcome {
                    StateResolutionOutcome::Applied { changes } => {
                        let mut values = (*namespace.values).clone();
                        for change in changes {
                            change.apply_to(&mut values);
                        }
                        Some(values)
                    }
                    StateResolutionOutcome::Refused { .. } => None,
                };
                let reduced = reduced.and_then(|values| {
                    match session_budget(registry, &candidates, &plugin, &values) {
                        Ok(()) => Some(values),
                        Err(refusal) => {
                            outcome = StateResolutionOutcome::Refused { refusal };
                            None
                        }
                    }
                });
                let namespace = candidates.entry(plugin.clone()).or_default();
                if let Some(values) = reduced {
                    namespace.replace_values(Arc::new(values));
                }
                let resolution = StateResolution {
                    publisher: address.clone(),
                    plugin: batch.plugin,
                    format_version: namespace.format_version,
                    origin: batch.origin,
                    segment,
                    ordinal: namespace.publication.next(),
                    predecessor: namespace.publication.applied(),
                    outcome,
                };
                namespace.generation = namespace.generation.saturating_add(1);
                namespace.publication.record(&resolution);
                resolution
            })
            .collect()
    }

    /// Whether `batch` may reduce at all: its proposer's authority, its
    /// plugin id, its namespace's format and its bounds.
    fn admit_batch(
        &self,
        batch: &StateCommandBatch,
        proposer: Option<&PluginCallbackIdentity>,
        namespace: &PluginNamespaceState,
    ) -> Result<(), StateCommandRefusal> {
        match proposer {
            Some(proposer) => batch.check(proposer, self.limits)?,
            None => batch.check_bounds(self.limits)?,
        }
        if !self.authority.keeps_state(&batch.plugin) {
            return Err(StateCommandRefusal::DecisionOnly);
        }
        let format_version = self
            .authority
            .declared(&batch.plugin)
            .ok_or(StateCommandRefusal::WrongOwner)?;
        if namespace.format_version != format_version {
            return Err(StateCommandRefusal::IncompatibleWriter(
                crate::FormatRefusal {
                    plugin: batch.plugin.clone(),
                    namespace: crate::FormatNamespace::State,
                    stored: namespace.format_version,
                    readable: format_version,
                },
            ));
        }
        Ok(())
    }

    fn run_reducer(
        &self,
        plugin: &str,
        name: &str,
        reduction: StateReduction<'_>,
    ) -> Result<Option<Value>, ReducerRefusal> {
        let reducer = self
            .authority
            .reducer(plugin, name)
            .ok_or(ReducerRefusal::Unknown)?;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| reducer(reduction))) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(cause)) => Err(ReducerRefusal::Refused(cause)),
            Err(_) => Err(ReducerRefusal::Refused(HookCause {
                error_type: REDUCER_PANIC.into(),
                error_version: std::num::NonZeroU32::MIN,
                payload: Value::String(format!("reducer `{name}` of plugin `{plugin}` panicked")),
            })),
        }
    }

    /// Publish the resolutions a recorded outcome carries and return the
    /// body's result. Each resolution applies at most once, in its
    /// namespace's order; the whole outcome validates before any namespace
    /// changes.
    pub fn publish_effect_state(
        &self,
        outcome: RuntimeEffectOutcome,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let address = match &outcome {
            RuntimeEffectOutcome::PluginState { state, .. } => state.address.clone(),
            _ => return Ok(outcome),
        };
        self.publish_recorded(&address, outcome)
    }

    pub(crate) fn publish_recorded(
        &self,
        address: &crate::EffectAddress,
        outcome: RuntimeEffectOutcome,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let RuntimeEffectOutcome::PluginState { state, result, .. } = outcome else {
            self.release_publication(address);
            return Ok(outcome);
        };
        if state.owner != *self.owner {
            self.abandon_publication(address);
            return Err(lash_execution_contracts::plugin_error::PluginError::State(
                PluginStateError::EffectOwnerMismatch,
            )
            .into());
        }
        self.publish_run_resolutions(address, state.resolutions)?;
        *result
    }

    /// Publish `resolutions`, what committed outcome records carry, each
    /// under the effect that reduced it, and release what that effect
    /// reserved. A resolution the namespace already holds applies once.
    ///
    /// # Errors
    ///
    /// A resolution its namespace's frontier refuses: the namespace is
    /// fenced, since the resident state no longer follows the durable one.
    pub fn publish_committed_state(
        &self,
        resolutions: &[StateResolution],
    ) -> Result<(), RuntimeEffectControllerError> {
        let mut by_publisher: Vec<(&crate::EffectAddress, Vec<StateResolution>)> = Vec::new();
        for resolution in resolutions {
            match by_publisher
                .iter_mut()
                .find(|(publisher, _)| **publisher == resolution.publisher)
            {
                Some((_, batch)) => batch.push(resolution.clone()),
                None => by_publisher.push((&resolution.publisher, vec![resolution.clone()])),
            }
        }
        by_publisher
            .into_iter()
            .try_for_each(|(publisher, batch)| self.publish_run_resolutions(publisher, batch))
    }

    pub(crate) fn publish_run_resolutions(
        &self,
        address: &crate::EffectAddress,
        resolutions: Vec<StateResolution>,
    ) -> Result<(), RuntimeEffectControllerError> {
        let mut registry = self.state.lock_recover();
        let mut candidate = registry.data.clone();
        let mut owed = registry.owed.clone();
        let segment = registry.segment;
        let published = resolutions
            .iter()
            .try_for_each(|resolution| {
                let namespace = candidate
                    .plugins
                    .entry(resolution.plugin.clone())
                    .or_default();
                match publish_one(namespace, segment, resolution)? {
                    Published::Applied | Published::AlreadyApplied => Ok(()),
                    Published::Ahead => {
                        let queue = owed.entry(resolution.plugin.clone()).or_default();
                        if queue
                            .get(&resolution.ordinal.0)
                            .is_some_and(|pending| pending != resolution)
                        {
                            return Err(PluginStateError::Frontier {
                                plugin: resolution.plugin.clone(),
                                refusal: FrontierRefusal::ReceiptMismatch {
                                    found: resolution.ordinal.0,
                                },
                            });
                        }
                        queue.insert(resolution.ordinal.0, resolution.clone());
                        Ok(())
                    }
                }
            })
            .and_then(|()| settle_owed(&mut candidate, &mut owed, segment));
        if let Err(error) = published {
            tracing::warn!(event = "plugin_state.frontier_refused", ?address, owner_segment = segment.0, %error, "recorded plugin-state publication refused");
            drop(registry);
            self.abandon_publication(address);
            return Err(lash_execution_contracts::plugin_error::PluginError::State(error).into());
        }
        if candidate != registry.data {
            registry.data = candidate;
            registry.source = None;
        }
        registry.owed = owed;
        drop(registry);
        self.release_publication(address);
        Ok(())
    }

    /// The published state with `staged` applied over it, each resolution in
    /// its namespace's order: what a commit carrying `staged` holds. Nothing
    /// resident changes.
    ///
    /// # Errors
    ///
    /// A resolution its namespace's frontier refuses.
    pub(crate) fn state_with(
        &self,
        staged: &[StateResolution],
    ) -> Result<PluginState, PluginStateError> {
        let registry = self.state.lock_recover();
        let mut data = registry.data.clone();
        for resolution in staged {
            let namespace = data.plugins.entry(resolution.plugin.clone()).or_default();
            match publish_one(namespace, registry.segment, resolution)? {
                Published::Applied | Published::AlreadyApplied => {}
                // A staged resolution was reduced against the published
                // namespace, so it follows it at once.
                Published::Ahead => {
                    return Err(PluginStateError::Frontier {
                        plugin: resolution.plugin.clone(),
                        refusal: FrontierRefusal::OutOfOrder {
                            found: resolution.ordinal.0,
                        },
                    });
                }
            }
        }
        Ok(data)
    }

    /// Release what `address` reserved, and wake every waiting reduction: a
    /// publication may also have settled owed resolutions.
    pub(crate) fn release_publication(&self, address: &crate::EffectAddress) {
        let mut registry = self.state.lock_recover();
        registry.reserved.retain(|_, holder| holder != address);
        registry.settled.notify_waiters();
    }

    /// The engine never returned `address`'s outcome: fence what it
    /// reduced, since it may be durable.
    pub(crate) fn abandon_publication(&self, address: &crate::EffectAddress) {
        let mut registry = self.state.lock_recover();
        let abandoned: Vec<String> = registry
            .reserved
            .iter()
            .filter(|(_, holder)| *holder == address)
            .map(|(namespace, _)| namespace.clone())
            .collect();
        if abandoned.is_empty() {
            return;
        }
        for namespace in abandoned {
            registry.reserved.remove(&namespace);
            tracing::warn!(
                event = "plugin_state.publication_fenced",
                plugin_id = %namespace,
                "a reduced plugin-state publication was abandoned before the engine returned it"
            );
            registry.fenced.insert(namespace);
        }
        registry.settled.notify_waiters();
    }

    /// The segment that owns publication from now on: a recorded resolution
    /// of an earlier segment no longer applies (K6, Q5 §6).
    pub fn adopt_state_segment(&self, segment: SegmentOrdinal) {
        let mut registry = self.state.lock_recover();
        registry.segment = registry.segment.max(segment);
        let segment = registry.segment;
        for namespace in registry.data.plugins.values_mut() {
            namespace.publication.owner_segment = segment;
        }
        registry.source = None;
    }
}

/// What delivering one recorded resolution did.
enum Published {
    Applied,
    AlreadyApplied,
    /// It follows a publication not yet delivered: it applies once that one
    /// has.
    Ahead,
}

fn publish_one(
    namespace: &mut PluginNamespaceState,
    segment: SegmentOrdinal,
    resolution: &StateResolution,
) -> Result<Published, PluginStateError> {
    namespace.publication.owner_segment = namespace.publication.owner_segment.max(segment);
    match namespace.publication.step(resolution) {
        Ok(FrontierStep::AlreadyApplied) => Ok(Published::AlreadyApplied),
        Ok(FrontierStep::Apply) => {
            if namespace.publication.applied().is_none() {
                namespace.format_version = resolution.format_version;
            } else if namespace.format_version != resolution.format_version {
                return Err(PluginStateError::Decode {
                    key: resolution.plugin.clone(),
                    message: format!(
                        "publication format {} differs from namespace format {}",
                        resolution.format_version, namespace.format_version
                    ),
                });
            }
            match &resolution.outcome {
                StateResolutionOutcome::Applied { changes } => {
                    for change in changes {
                        change.apply_to(namespace.values_mut());
                    }
                }
                StateResolutionOutcome::Refused { refusal } => {
                    tracing::warn!(
                        event = "plugin_state.batch_refused",
                        plugin_id = %resolution.plugin,
                        ordinal = resolution.ordinal.0,
                        %refusal,
                        "a plugin's state command batch was refused and published nothing"
                    );
                }
            }
            namespace.generation = namespace.generation.saturating_add(1);
            namespace.publication.record(resolution);
            Ok(Published::Applied)
        }
        Err(lash_core_store::tool_run::FrontierRefusal::OutOfOrder { .. })
            if resolution.ordinal > namespace.publication.next() =>
        {
            Ok(Published::Ahead)
        }
        Err(refusal) => Err(PluginStateError::Frontier {
            plugin: resolution.plugin.clone(),
            refusal,
        }),
    }
}

/// Apply every owed resolution whose predecessor has now applied.
fn settle_owed(
    candidate: &mut PluginState,
    owed: &mut BTreeMap<String, BTreeMap<u64, StateResolution>>,
    segment: SegmentOrdinal,
) -> Result<(), PluginStateError> {
    for (plugin, queue) in owed.iter_mut() {
        let namespace = candidate.plugins.entry(plugin.clone()).or_default();
        while let Some(entry) = queue.first_entry() {
            match publish_one(namespace, segment, entry.get())? {
                Published::Ahead => break,
                Published::Applied | Published::AlreadyApplied => {
                    entry.remove();
                }
            }
        }
    }
    owed.retain(|_, queue| !queue.is_empty());
    Ok(())
}

#[cfg(test)]
mod tests;

/// Whether the session's plugin state, with `plugin`'s namespace holding
/// `values` and every other namespace as `candidates` or the registry hold
/// it, stays within its total budget: every namespace's values and its
/// publication frontier, encoded. Past the warn tier the publication is
/// reported; past the limit it is refused.
fn session_budget(
    registry: &mut PluginStateRegistry,
    candidates: &BTreeMap<String, PluginNamespaceState>,
    plugin: &str,
    values: &BTreeMap<String, Value>,
) -> Result<(), lash_core_store::tool_run::StateCommandRefusal> {
    let limit = registry.limits.max_session_bytes;
    let encoded = |values: &BTreeMap<String, Value>| {
        serde_json::to_vec(values).map_or(usize::MAX, |bytes| bytes.len())
    };
    let frontier = |namespace: &PluginNamespaceState| {
        serde_json::to_vec(&namespace.publication).map_or(usize::MAX, |bytes| bytes.len())
    };
    let mut bytes = encoded(values);
    let namespaces = registry
        .data
        .plugins
        .iter()
        .chain(candidates.iter())
        .map(|(id, namespace)| {
            let current = candidates.get(id).unwrap_or(namespace);
            (id.clone(), (Arc::clone(&current.values), frontier(current)))
        })
        .collect::<BTreeMap<_, _>>();
    for (id, (values, frontier)) in namespaces {
        bytes = bytes.saturating_add(frontier);
        if id == plugin {
            continue;
        }
        let measured = match registry.sizes.get(&id) {
            Some((from, size)) if std::ptr::eq(from.as_ptr(), Arc::as_ptr(&values)) => *size,
            _ => {
                let size = encoded(&values);
                registry
                    .sizes
                    .insert(id.clone(), (Arc::downgrade(&values), size));
                size
            }
        };
        bytes = bytes.saturating_add(measured);
    }
    if bytes > limit {
        tracing::debug!(
            event = "plugin_state.session_budget_refused",
            plugin_id = %plugin,
            bytes,
            limit = limit,
            "a plugin-state publication would take the session past its total budget and is refused"
        );
        return Err(
            lash_core_store::tool_run::StateCommandRefusal::SessionTooLarge { bytes, limit },
        );
    }
    if let Some(warn) = registry.diagnostics.plugin_state_warn_bytes()
        && u64::try_from(bytes).unwrap_or(u64::MAX) > warn.get()
    {
        tracing::debug!(
            event = "plugin_state.session_budget_warn",
            plugin_id = %plugin,
            bytes,
            warn = warn.get(),
            limit = limit,
            "a plugin-state publication takes the session past its warn tier"
        );
    }
    Ok(())
}

/// Replay a whole recorded namespace view without constructing capabilities.
pub(in crate::plugin) fn replay_state(
    mut state: PluginState,
    resolutions: &[StateResolution],
) -> Result<PluginState, PluginStateError> {
    let mut owed = BTreeMap::<String, BTreeMap<u64, StateResolution>>::new();
    for resolution in resolutions {
        let namespace = state.plugins.entry(resolution.plugin.clone()).or_default();
        match publish_one(namespace, resolution.segment, resolution)? {
            Published::Ahead => {
                if let Some(previous) = owed
                    .entry(resolution.plugin.clone())
                    .or_default()
                    .insert(resolution.ordinal.0, resolution.clone())
                    && previous != *resolution
                {
                    return Err(PluginStateError::Frontier {
                        plugin: resolution.plugin.clone(),
                        refusal: FrontierRefusal::ReceiptMismatch {
                            found: resolution.ordinal.0,
                        },
                    });
                }
            }
            Published::Applied | Published::AlreadyApplied => {}
        }
        settle_owed(&mut state, &mut owed, resolution.segment)?;
    }
    if let Some((plugin, pending)) = owed.first_key_value() {
        return Err(PluginStateError::Frontier {
            plugin: plugin.clone(),
            refusal: FrontierRefusal::OutOfOrder {
                found: *pending
                    .first_key_value()
                    .map(|(ordinal, _)| ordinal)
                    .unwrap_or(&0),
            },
        });
    }
    Ok(state)
}
