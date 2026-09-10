//! Resident-session continuation state and the reload gate that governs it.
//!
//! One runtime handle keeps five facts about how far its in-memory session has
//! travelled with the durable one: whether resident plugin/protocol state is
//! still valid, whether this handle has ever loaded the graph itself, whether
//! a borrowed nested commit moved the durable head out from under it, which
//! lease generation its last commit ran under, and which turn produced the
//! revision an observation should attribute. They are one type here because
//! their rules only make sense together: invalidation clears four of them at
//! once, and a full durable adoption settles the same four.

use super::*;
use crate::TurnId;

/// Validity state of in-memory resident session/plugin state on a [`LashRuntime`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResidentSessionState {
    /// In-memory session and plugin state are valid and match durable expectations.
    Valid,
    /// Resident state was invalidated and requires durable reload before further execution.
    Invalidated { decision_id: String },
}

/// The resident head revision the reload gate compared from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::runtime) struct ResidentHeadRevision(pub(in crate::runtime) u64);

/// The durable head revision the reload gate compared against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::runtime) struct DurableHeadRevision(pub(in crate::runtime) u64);

/// Where the reload gate read the durable head from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::runtime) enum ResidentReloadDurableSource {
    /// The gate returned before consulting any durable source.
    NotConsulted,
    /// The session has a history store and the gate read through it.
    HistoryStore,
    /// The session is store-less, so the resident snapshot is the only source.
    ResidentSnapshot,
}

impl ResidentReloadDurableSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::NotConsulted => "not_consulted",
            Self::HistoryStore => "history_store",
            Self::ResidentSnapshot => "resident_snapshot",
        }
    }
}

/// What the gate established about the durable head it compared against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::runtime) enum ResidentReloadHeadFreshness {
    /// No reload was required, so the resident head is the current one.
    CurrentResidentState,
    /// A store-backed refresh was owed and had not run yet.
    RefreshPending,
    /// The session has no store, so no durable head exists to refresh from.
    StoreUnavailable,
    /// The durable head was refreshed from the store.
    ReloadedFromStore,
    /// The durable refresh was attempted and failed.
    RefreshFailed,
}

impl ResidentReloadHeadFreshness {
    fn as_str(self) -> &'static str {
        match self {
            Self::CurrentResidentState => "current_resident_state",
            Self::RefreshPending => "refresh_pending",
            Self::StoreUnavailable => "store_unavailable",
            Self::ReloadedFromStore => "reloaded_from_store",
            Self::RefreshFailed => "refresh_failed",
        }
    }
}

/// The restore stage a denied reload failed in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::runtime) enum ResidentReloadStage {
    /// No stage failed.
    None,
    DurableHeadRefresh,
    SessionAvailability,
    ToolStateRestore,
    ToolCatalogRefresh,
    PluginStateRestore,
    ProtocolSessionRestore,
    SessionRestoredHook,
}

impl ResidentReloadStage {
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::DurableHeadRefresh => "durable_head_refresh",
            Self::SessionAvailability => "session_availability",
            Self::ToolStateRestore => "tool_state_restore",
            Self::ToolCatalogRefresh => "tool_catalog_refresh",
            Self::PluginStateRestore => "plugin_state_restore",
            Self::ProtocolSessionRestore => "protocol_session_restore",
            Self::SessionRestoredHook => "session_restored_hook",
        }
    }
}

/// The reload gate's verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::runtime) enum ResidentReloadOutcome {
    /// Resident state was already valid; nothing was reloaded.
    NotRequired,
    /// Invalidated resident state was reloaded from durable facts.
    Restored,
    /// The reload failed and resident state stays invalidated.
    Denied,
}

impl ResidentReloadOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::NotRequired => "not_required",
            Self::Restored => "restored",
            Self::Denied => "denied",
        }
    }
}

/// One decision of the resident-state reload gate, as it is traced.
///
/// Every axis is its own type, so two axes cannot be transposed at a call site
/// without a compile error. The nine bare arguments this replaced included
/// three adjacent `&str` axes and two adjacent `u64` revisions, where a swap
/// compiled clean and silently wrote a corrupt trace record.
pub(in crate::runtime) struct ResidentSessionReloadDecision<'a> {
    pub(in crate::runtime) decision_id: &'a str,
    pub(in crate::runtime) consulted_validity: bool,
    pub(in crate::runtime) durable_source: ResidentReloadDurableSource,
    pub(in crate::runtime) resident_head_revision: ResidentHeadRevision,
    pub(in crate::runtime) durable_head_freshness: ResidentReloadHeadFreshness,
    pub(in crate::runtime) durable_head_revision: DurableHeadRevision,
    pub(in crate::runtime) failing_restore_stage: ResidentReloadStage,
    pub(in crate::runtime) outcome: ResidentReloadOutcome,
    /// Absent for every decision that is not a denial.
    pub(in crate::runtime) error_classification: Option<&'a RuntimeErrorCode>,
}

/// The continuation facts one runtime handle holds about its resident session.
///
/// Constructed valid, cleared as a set by [`Self::invalidate`], and settled as
/// a set by [`Self::mark_adopted`]. `graph_head_stale` is shared with the
/// session services this handle hands out, so a borrowed nested commit can
/// mark the resident graph stale from another owner.
pub(in crate::runtime) struct ResidentSessionContinuity {
    validity: ResidentSessionState,
    /// Set only after this handle itself has attempted a durable graph load.
    graph_loaded_from_store: bool,
    /// Set by a successful borrowed nested commit. The lane remains continuous,
    /// but the durable head may have advanced outside this runtime's resident
    /// state, so the next physical turn must reload deliberately before planning.
    graph_head_stale: Arc<AtomicBool>,
    /// Lease-guard identity retained across a successful physical-turn commit.
    /// A match proves no release/reacquisition boundary occurred before the
    /// next physical turn on this handle.
    last_committed_lease_continuity: Option<SessionExecutionLeaseContinuity>,
    /// Most recent physical turn committed by this runtime, paired with the
    /// resulting session revision for observation-envelope attribution.
    last_committed_observation_turn: Option<(u64, String)>,
}

impl ResidentSessionContinuity {
    /// A handle that has loaded nothing and committed nothing yet.
    pub(in crate::runtime) fn fresh() -> Self {
        Self {
            validity: ResidentSessionState::Valid,
            graph_loaded_from_store: false,
            graph_head_stale: Arc::new(AtomicBool::new(false)),
            last_committed_lease_continuity: None,
            last_committed_observation_turn: None,
        }
    }

    pub(in crate::runtime) fn validity(&self) -> &ResidentSessionState {
        &self.validity
    }

    pub(in crate::runtime) fn is_valid(&self) -> bool {
        matches!(self.validity, ResidentSessionState::Valid)
    }

    /// Drop every continuation fact that a durable reload must re-establish.
    ///
    /// The incident decision identity is minted once and retained across
    /// repeated invalidations, so the reload that finally clears it names the
    /// incident that opened it.
    pub(in crate::runtime) fn invalidate(&mut self, session_id: &str) {
        if matches!(self.validity, ResidentSessionState::Valid) {
            self.validity = ResidentSessionState::Invalidated {
                decision_id: format!(
                    "resident-session-reload:{}:{}",
                    session_id,
                    uuid::Uuid::new_v4()
                ),
            };
        }
        self.graph_loaded_from_store = false;
        self.last_committed_lease_continuity = None;
        self.last_committed_observation_turn = None;
    }

    /// Settle every resident-freshness fact after a full durable adoption
    /// (FIG-1875): the resident state is valid, the graph is the one loaded
    /// from the store, and no cross-process staleness is pending.
    ///
    /// `lease_continuity` is the continuity of the session-execution lease
    /// held across the adoption, when the caller holds one: while that lease
    /// stays live no other executor can advance the durable head, so the
    /// freshly adopted resident graph is current under it and the turn loop
    /// issues no second durable probe.
    pub(in crate::runtime) fn mark_adopted(
        &mut self,
        lease_continuity: Option<SessionExecutionLeaseContinuity>,
    ) {
        self.validity = ResidentSessionState::Valid;
        self.graph_loaded_from_store = true;
        self.graph_head_stale.store(false, Ordering::Release);
        self.last_committed_lease_continuity = lease_continuity;
    }

    /// Record that this handle has now attempted a durable graph load.
    pub(in crate::runtime) fn mark_graph_loaded(&mut self) {
        self.graph_loaded_from_store = true;
    }

    /// Record that the resident graph matches the durable head again.
    pub(in crate::runtime) fn mark_graph_head_current(&self) {
        self.graph_head_stale.store(false, Ordering::Release);
    }

    /// The shared staleness flag handed to session services, which mark it from
    /// a borrowed nested commit this handle never observes directly.
    pub(in crate::runtime) fn graph_head_stale_flag(&self) -> &Arc<AtomicBool> {
        &self.graph_head_stale
    }

    /// Whether the resident graph can be planned against without a fresh
    /// durable probe.
    ///
    /// Only continuous lease custody proves it: this handle must have loaded
    /// the graph itself, no nested commit may have marked it stale, and the
    /// lease generation must be the same one the last commit ran under. Any
    /// release/reacquisition boundary in between lets another executor advance
    /// the durable head.
    pub(in crate::runtime) fn graph_is_current_under(
        &self,
        lease_continuity: Option<SessionExecutionLeaseContinuity>,
    ) -> bool {
        self.graph_loaded_from_store
            && !self.graph_head_stale.load(Ordering::Acquire)
            && lease_continuity.is_some()
            && lease_continuity == self.last_committed_lease_continuity
    }

    /// Retain (or drop) the lease identity a just-committed turn ran under.
    pub(in crate::runtime) fn retain_committed_lease_continuity(
        &mut self,
        lease_continuity: Option<SessionExecutionLeaseContinuity>,
    ) {
        self.last_committed_lease_continuity = lease_continuity;
    }

    /// Name the turn that produced `revision`, for observation attribution.
    pub(in crate::runtime) fn record_committed_observation_turn(
        &mut self,
        revision: u64,
        turn_id: &TurnId,
    ) {
        self.last_committed_observation_turn = Some((revision, turn_id.to_string()));
    }

    /// The turn id this handle committed at `revision`, when it is the most
    /// recent one it committed.
    pub(in crate::runtime) fn last_committed_turn_id_for_revision(
        &self,
        revision: u64,
    ) -> Option<&str> {
        self.last_committed_observation_turn
            .as_ref()
            .filter(|(committed_revision, _)| *committed_revision == revision)
            .map(|(_, turn_id)| turn_id.as_str())
    }

    #[cfg(test)]
    pub(in crate::runtime) fn mark_graph_head_stale(&self) {
        self.graph_head_stale.store(true, Ordering::Release);
    }

    #[cfg(test)]
    pub(in crate::runtime) fn graph_loaded_from_store(&self) -> bool {
        self.graph_loaded_from_store
    }

    #[cfg(test)]
    pub(in crate::runtime) fn graph_head_is_stale(&self) -> bool {
        self.graph_head_stale.load(Ordering::Acquire)
    }
}

impl LashRuntime {
    pub(in crate::runtime) fn invalidate_resident_session_state(&mut self) {
        self.resident_session.invalidate(&self.state.session_id);
        if let Some(session) = self.session.as_ref() {
            session.invalidate_runtime_caches();
        }
    }

    pub(in crate::runtime) fn trace_synchronous_resident_state_refusal(
        &self,
        decision_id: &str,
        consumer: &'static str,
    ) {
        tracing::info!(
            event = "resident_session_state.sync_refusal",
            decision_id,
            session_id = %self.state.session_id,
            consumer,
            consulted_validity = false,
            outcome = "refused",
            error_classification = RuntimeErrorCode::ResidentSessionReloadFailed.as_str(),
            "synchronous resident-state consumer refused invalidated state"
        );
    }

    fn trace_resident_session_reload_decision(&self, decision: ResidentSessionReloadDecision<'_>) {
        tracing::info!(
            event = "resident_session_state.reload_decision",
            decision_id = decision.decision_id,
            session_id = %self.state.session_id,
            consulted_validity = decision.consulted_validity,
            durable_source = decision.durable_source.as_str(),
            resident_head_revision = decision.resident_head_revision.0,
            durable_head_freshness = decision.durable_head_freshness.as_str(),
            durable_head_revision = decision.durable_head_revision.0,
            failing_restore_stage = decision.failing_restore_stage.as_str(),
            outcome = decision.outcome.as_str(),
            error_classification = decision
                .error_classification
                .map_or("none", RuntimeErrorCode::as_str),
            "resident-state reload gate decided"
        );
    }

    pub(in crate::runtime) async fn reload_invalidated_resident_session_state(
        &mut self,
    ) -> Result<(), RuntimeError> {
        self.reload_invalidated_resident_session_state_under_lease(None)
            .await
    }

    pub(super) async fn reload_invalidated_resident_session_state_under_lease(
        &mut self,
        session_execution_lease: Option<&SessionExecutionLeaseGuard>,
    ) -> Result<(), RuntimeError> {
        let decision_id = match self.resident_session.validity() {
            ResidentSessionState::Valid => {
                self.trace_resident_session_reload_decision(ResidentSessionReloadDecision {
                    decision_id: "resident-session-reload:not-required",
                    consulted_validity: true,
                    durable_source: ResidentReloadDurableSource::NotConsulted,
                    resident_head_revision: ResidentHeadRevision(self.state.head_revision),
                    durable_head_freshness: ResidentReloadHeadFreshness::CurrentResidentState,
                    durable_head_revision: DurableHeadRevision(self.state.head_revision),
                    failing_restore_stage: ResidentReloadStage::None,
                    outcome: ResidentReloadOutcome::NotRequired,
                    error_classification: None,
                });
                return Ok(());
            }
            ResidentSessionState::Invalidated { decision_id } => decision_id.clone(),
        };
        let resident_head_revision = self.state.head_revision;
        let store = self
            .session
            .as_ref()
            .and_then(|session| session.history_store());
        let durable_source = if store.is_some() {
            ResidentReloadDurableSource::HistoryStore
        } else {
            ResidentReloadDurableSource::ResidentSnapshot
        };
        let mut durable_head_freshness = if store.is_some() {
            ResidentReloadHeadFreshness::RefreshPending
        } else {
            ResidentReloadHeadFreshness::StoreUnavailable
        };
        let mut durable_state = self.state.clone();
        let mut durable_head_revision = durable_state.head_revision;
        let reload_result: Result<(), (ResidentReloadStage, RuntimeError)> = async {
            if let Some(store) = store.as_ref() {
                crate::store::refresh_persisted_session_state(store.as_ref(), &mut durable_state)
                    .await
                    .map_err(|err| {
                        (
                            ResidentReloadStage::DurableHeadRefresh,
                            RuntimeError::new(
                                RuntimeErrorCode::ResidentSessionReloadFailed,
                                format!(
                                    "failed to reload invalidated resident session state: {err}"
                                ),
                            ),
                        )
                    })?;
                durable_head_freshness = ResidentReloadHeadFreshness::ReloadedFromStore;
                durable_head_revision = durable_state.head_revision;
            }

            let session = self.session.as_mut().ok_or_else(|| {
                (
                    ResidentReloadStage::SessionAvailability,
                    RuntimeError::new(
                        RuntimeErrorCode::ResidentSessionReloadFailed,
                        "runtime session is unavailable while reloading invalidated resident state",
                    ),
                )
            })?;
            session.invalidate_runtime_caches();
            if let Some(tool_state) = durable_state.tool_state_snapshot().cloned() {
                session
                    .plugins()
                    .tool_registry()
                    .restore_state(tool_state)
                    .map_err(|err| {
                        (
                            ResidentReloadStage::ToolStateRestore,
                            RuntimeError::new(
                                RuntimeErrorCode::ResidentSessionReloadFailed,
                                err.to_string(),
                            ),
                        )
                    })?;
            }
            session.refresh_tool_catalog().await.map_err(|err| {
                (
                    ResidentReloadStage::ToolCatalogRefresh,
                    RuntimeError::new(
                        RuntimeErrorCode::ResidentSessionReloadFailed,
                        err.to_string(),
                    ),
                )
            })?;
            if let Some(snapshot) = durable_state.plugin_state() {
                session.plugins().hydrate_state(snapshot).map_err(|err| {
                    (
                        ResidentReloadStage::PluginStateRestore,
                        RuntimeError::new(
                            RuntimeErrorCode::ResidentSessionReloadFailed,
                            err.to_string(),
                        ),
                    )
                })?;
            }
            let protocol_session = Arc::clone(session.plugins().protocol_session());
            let session_id = durable_state.session_id.clone();
            protocol_session
                .restore_session(
                    crate::plugin::ProtocolSessionContext::new(session, &session_id),
                    crate::plugin::ProtocolSessionRestoreView::new(&durable_state),
                )
                .await
                .map_err(|err| {
                    (
                        ResidentReloadStage::ProtocolSessionRestore,
                        RuntimeError::new(
                            RuntimeErrorCode::ResidentSessionReloadFailed,
                            err.to_string(),
                        ),
                    )
                })?;

            if store.is_some() {
                durable_state.discard_runtime_snapshots();
            } else {
                durable_state.discard_runtime_snapshots_retaining_accepted_execution();
            }
            session
                .plugins()
                .emit_runtime_event(crate::PluginLifecycleEvent::SessionRestored(
                    crate::SessionReadView::from_persisted_state(&durable_state),
                ))
                .await
                .map_err(|err| {
                    (
                        ResidentReloadStage::SessionRestoredHook,
                        RuntimeError::new(
                            RuntimeErrorCode::ResidentSessionReloadFailed,
                            err.to_string(),
                        ),
                    )
                })?;
            self.state = durable_state;
            // A successful reload is a full durable adoption: settle the
            // freshness facts so the turn loop does not issue a second
            // durable probe right after this reload (FIG-1875).
            self.resident_session.mark_adopted(
                session_execution_lease.and_then(SessionExecutionLeaseGuard::continuity),
            );
            Ok(())
        }
        .await;

        match reload_result {
            Ok(()) => {
                self.trace_resident_session_reload_decision(ResidentSessionReloadDecision {
                    decision_id: &decision_id,
                    consulted_validity: false,
                    durable_source,
                    resident_head_revision: ResidentHeadRevision(resident_head_revision),
                    durable_head_freshness,
                    durable_head_revision: DurableHeadRevision(durable_head_revision),
                    failing_restore_stage: ResidentReloadStage::None,
                    outcome: ResidentReloadOutcome::Restored,
                    error_classification: None,
                });
                Ok(())
            }
            Err((failing_restore_stage, err)) => {
                if failing_restore_stage == ResidentReloadStage::DurableHeadRefresh {
                    durable_head_freshness = ResidentReloadHeadFreshness::RefreshFailed;
                }
                self.trace_resident_session_reload_decision(ResidentSessionReloadDecision {
                    decision_id: &decision_id,
                    consulted_validity: false,
                    durable_source,
                    resident_head_revision: ResidentHeadRevision(resident_head_revision),
                    durable_head_freshness,
                    durable_head_revision: DurableHeadRevision(durable_head_revision),
                    failing_restore_stage,
                    outcome: ResidentReloadOutcome::Denied,
                    error_classification: Some(&err.code),
                });
                Err(err)
            }
        }
    }

    pub(in crate::runtime) async fn reload_invalidated_resident_session_state_for_session(
        &mut self,
    ) -> Result<(), SessionError> {
        self.reload_invalidated_resident_session_state()
            .await
            .map_err(|err| SessionError::Protocol(err.to_string()))
    }
}
