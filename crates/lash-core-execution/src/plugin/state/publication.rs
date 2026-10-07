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
//! before its outcome is durable.
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
/// behaviour of the plugin's revision, and a recorded resolution never runs
/// it again.
pub type StateReducer =
    Arc<dyn Fn(StateReduction<'_>) -> Result<Option<Value>, HookCause> + Send + Sync>;

/// One command batch proposed inside a recorded body.
pub(crate) struct Proposal {
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
        owner: lash_core_store::store::plugin_writers::PluginRevision,
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
    state: Arc<Mutex<PluginStateRegistry>>,
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
    plugins: &crate::PluginSession,
    proposal: Proposal,
) -> Result<(), PluginStateError> {
    let plugin = proposal.batch.plugin.plugin.clone();
    let mut pending = Some(proposal);
    let _ = SINK.try_with(|sink| {
        let mut sink = sink.lock_recover();
        if Arc::ptr_eq(&sink.state, &plugins.state)
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
    plugins: &crate::PluginSession,
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
    plugins: &crate::PluginSession,
    body: F,
) -> (F::Output, Vec<Proposal>) {
    let sink = Arc::new(Mutex::new(Sink {
        state: Arc::clone(&plugins.state),
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
    plugins: Arc<crate::PluginSession>,
    kind: RuntimeEffectKind,
    address: crate::EffectAddress,
    body: F,
) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError>
where
    F: Future<Output = Result<RuntimeEffectOutcome, RuntimeEffectControllerError>>,
{
    let segment = plugins.state_segment();
    let (result, proposals) = collect_proposals(&plugins, body).await;
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
        return error.into();
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
    plugins: Arc<crate::PluginSession>,
    address: crate::EffectAddress,
    settled: bool,
}

impl EffectPublication {
    /// Begin the publication of the effect at `address` for `plugins`.
    pub fn begin(plugins: Arc<crate::PluginSession>, address: crate::EffectAddress) -> Self {
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
    plugins: Arc<crate::PluginSession>,
    address: crate::EffectAddress,
}

impl Drop for StateReservation {
    fn drop(&mut self) {
        self.plugins.abandon_publication(&self.address);
    }
}

const REDUCER_PANIC: &str = "lash.state_reducer_panicked";

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
            .map(|proposal| proposal.batch.plugin.plugin.clone())
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
                    return Ok(self.resolve(&registry, address, segment, proposals));
                }
            }
            notified.await;
        }
    }

    /// Reduce each batch in order against the published namespace and the
    /// batches before it, privately.
    fn resolve(
        &self,
        registry: &PluginStateRegistry,
        address: &crate::EffectAddress,
        segment: SegmentOrdinal,
        proposals: Vec<Proposal>,
    ) -> Vec<StateResolution> {
        let mut candidates: BTreeMap<String, PluginNamespaceState> = BTreeMap::new();
        proposals
            .into_iter()
            .map(|Proposal { batch, proposer }| {
                let plugin = batch.plugin.plugin.clone();
                let namespace = candidates.entry(plugin.clone()).or_insert_with(|| {
                    registry
                        .data
                        .plugins
                        .get(&plugin)
                        .cloned()
                        .unwrap_or_default()
                });
                namespace.publication.owner_segment = registry.segment;
                let outcome = match self.admit_batch(&batch, proposer.as_ref(), namespace) {
                    Err(refusal) => StateResolutionOutcome::Refused { refusal },
                    Ok(()) => batch.reduce(&namespace.values, &mut |key, name, current, input| {
                        self.run_reducer(
                            &plugin,
                            name,
                            StateReduction {
                                key,
                                current,
                                input,
                            },
                        )
                    }),
                };
                if let StateResolutionOutcome::Applied { changes } = &outcome {
                    for change in changes {
                        change.apply_to(&mut namespace.values);
                    }
                }
                let resolution = StateResolution {
                    publisher: address.clone(),
                    plugin: batch.plugin,
                    origin: batch.origin,
                    segment,
                    ordinal: namespace.publication.next(),
                    predecessor: namespace.publication.applied,
                    outcome,
                };
                namespace.generation = namespace.generation.saturating_add(1);
                namespace.publication.record(&resolution);
                resolution
            })
            .collect()
    }

    /// Whether `batch` may reduce at all: its proposer's authority, its
    /// plugin revision, its namespace's format and its bounds.
    fn admit_batch(
        &self,
        batch: &StateCommandBatch,
        proposer: Option<&PluginCallbackIdentity>,
        namespace: &PluginNamespaceState,
    ) -> Result<(), StateCommandRefusal> {
        match proposer {
            Some(proposer) => batch.check(proposer, StateCommandLimits::PUBLISHED)?,
            None => batch.check_bounds(StateCommandLimits::PUBLISHED)?,
        }
        let declaration = self
            .host
            .factories()
            .iter()
            .map(|factory| factory.plugin_declaration())
            .find(|declaration| declaration.id.as_str() == batch.plugin.plugin)
            .filter(|declaration| declaration.behavior_revision == batch.plugin.behavior_revision)
            .ok_or(StateCommandRefusal::WrongOwner)?;
        if namespace.format_version != declaration.format_version {
            return Err(StateCommandRefusal::IncompatibleWriter(
                crate::FormatRefusal {
                    plugin: batch.plugin.plugin.clone(),
                    namespace: crate::FormatNamespace::State,
                    stored: namespace.format_version,
                    readable: declaration.format_version,
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
            .capabilities
            .get()
            .and_then(|capabilities| capabilities.contributions.state_reducers.get(plugin))
            .and_then(|reducers| reducers.get(name))
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

    fn publish_recorded(
        &self,
        address: &crate::EffectAddress,
        outcome: RuntimeEffectOutcome,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let RuntimeEffectOutcome::PluginState { state, result, .. } = outcome else {
            self.release_publication(address);
            return Ok(outcome);
        };
        if state.owner != self.owner {
            self.abandon_publication(address);
            return Err(PluginStateError::EffectOwnerMismatch.into());
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
                    .entry(resolution.plugin.plugin.clone())
                    .or_default();
                match publish_one(namespace, segment, resolution)? {
                    Published::Applied | Published::AlreadyApplied => Ok(()),
                    Published::Ahead => {
                        let queue = owed.entry(resolution.plugin.plugin.clone()).or_default();
                        if queue
                            .get(&resolution.ordinal.0)
                            .is_some_and(|pending| pending != resolution)
                        {
                            return Err(PluginStateError::Frontier {
                                plugin: resolution.plugin.plugin.clone(),
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
            return Err(error.into());
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

    /// Release what `address` reserved, and wake every waiting reduction: a
    /// publication may also have settled owed resolutions.
    pub(crate) fn release_publication(&self, address: &crate::EffectAddress) {
        let mut registry = self.state.lock_recover();
        registry.reserved.retain(|_, holder| holder != address);
        registry.settled.notify_waiters();
    }

    /// The engine never returned `address`'s outcome: fence what it
    /// reduced, since it may be durable.
    fn abandon_publication(&self, address: &crate::EffectAddress) {
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
            match &resolution.outcome {
                StateResolutionOutcome::Applied { changes } => {
                    for change in changes {
                        change.apply_to(&mut namespace.values);
                    }
                }
                StateResolutionOutcome::Refused { refusal } => {
                    tracing::warn!(
                        event = "plugin_state.batch_refused",
                        plugin_id = %resolution.plugin.plugin,
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
            plugin: resolution.plugin.plugin.clone(),
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
