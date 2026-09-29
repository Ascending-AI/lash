//! Shared fixtures for the conformance suites: paired handles opened
//! against the same durable backing store, used by the `*_reopenable`
//! suite variants.

use super::*;
use lash_core::PROCESS_WAKE_DELIVERY_FORMAT_VERSION;

pub(crate) fn assert_fresh_instances<T: ?Sized>(left: &Arc<T>, right: &Arc<T>, suite: &str) {
    assert!(
        !Arc::ptr_eq(left, right),
        "{suite} factory reused one Arc across conformance roles"
    );
}

/// Admit a scope for a host entry point: conformance suites mint scopes
/// directly, so this stands in for the admission authority's answer — a
/// process scope pins the fabricated first-registration incarnation the
/// fixture fabricates for it.
pub(crate) fn admit(scope: crate::ExecutionScope) -> crate::AdmittedScope {
    match &scope {
        crate::ExecutionScope::Process { process_id } => {
            crate::AdmittedScope::process(process_id.clone())
        }
        _ => crate::AdmittedScope::new(scope),
    }
}

/// Record one completed attachment write: acquire the write fence, then stamp
/// the upload evidence. This is the only way a manifest row comes into being,
/// and the stamp is the only thing that makes a digest adoptable.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(crate) async fn record_completed_attachment_write(
    store: &Arc<dyn crate::RuntimeStore>,
    intent: crate::AttachmentIntent,
) {
    let crate::AttachmentWriteFence::Granted(permit) = store
        .begin_attachment_write(intent.clone())
        .await
        .expect("begin attachment write")
    else {
        panic!(
            "expected a granted write fence for `{}`",
            intent.attachment_id
        );
    };
    store
        .complete_attachment_write(&intent, permit)
        .await
        .expect("stamp attachment upload evidence");
}

/// A pair of [`ProcessRegistry`] handles opened against the same durable
/// backing store.
pub struct ReopenableProcessRegistry {
    pub open: Arc<dyn crate::ConformanceProcessRegistry>,
    pub reopen: Arc<dyn crate::ConformanceProcessRegistry>,
}

/// A pair of [`RuntimeStore`] handles opened against the same durable
/// backing store, and the effect host of the same substrate: the owner of the
/// turn-control promises a closure authorization names.
pub struct ReopenableRuntimePersistence {
    pub open: Arc<dyn RuntimeStore>,
    pub reopen: Arc<dyn RuntimeStore>,
    pub effect_host: Arc<dyn crate::EffectHost>,
}

/// A pair of [`AttachmentStore`](crate::AttachmentStore) handles opened against
/// the same durable backing store.
pub struct ReopenableAttachmentStore {
    pub open: Arc<dyn crate::AttachmentStore>,
    pub reopen: Arc<dyn crate::AttachmentStore>,
}

/// A pair of [`TriggerStore`](crate::TriggerStore) handles opened against
/// the same durable backing store.
pub struct ReopenableTriggerStore {
    pub open: Arc<dyn crate::TriggerStore>,
    pub reopen: Arc<dyn crate::TriggerStore>,
}

/// Push an unpersisted event node onto `state`'s active path and make it the
/// resident leaf. Pair with [`commit_conformance_state`] to advance a session's
/// durable head from outside any runtime.
pub(crate) use lash_core::testing::store_fixtures::{
    admit_conformance_session, append_conformance_event_node, commit_conformance_state,
    durable_turn_address, durable_turn_scope,
};

/// `session_id`'s view of `store` (ADR 0112 §3): what a law's runtime is
/// built over, and how a law reads one session's state.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: every law names a valid session id"
)]
pub(crate) fn session_view(
    store: &Arc<dyn RuntimeStore>,
    session_id: impl Into<crate::SessionId>,
) -> crate::store::SessionStore {
    crate::store::SessionStore::new(Arc::clone(store), session_id.into())
        .expect("conformance session ids are valid")
}

/// The live head's window of `session_id`: its current frame, the read every
/// runtime load adopts (ADR 0112 §5).
pub(crate) async fn load_current_window(
    store: &dyn RuntimeStore,
    session_id: &crate::SessionId,
) -> Result<Option<crate::store::SessionWindowRead>, crate::StoreError> {
    store
        .load_session_window(session_id, crate::store::WindowSelector::Current)
        .await
}

/// A runtime store a law holds either behind an `Arc` or borrowed.
pub(crate) trait AsRuntimeStore: Send + Sync {
    fn runtime_store(&self) -> &dyn RuntimeStore;
}

impl AsRuntimeStore for Arc<dyn RuntimeStore> {
    fn runtime_store(&self) -> &dyn RuntimeStore {
        self.as_ref()
    }
}

impl<'a> AsRuntimeStore for dyn RuntimeStore + 'a {
    fn runtime_store(&self) -> &dyn RuntimeStore {
        self
    }
}

/// `session_id`'s current frame adopted as runtime state, the load every
/// runtime open performs (ADR 0112 §9): the state-version check, a `Current`
/// window read, and adoption. `Ok(None)` means no head row.
pub(crate) async fn load_window_state(
    store: &(impl AsRuntimeStore + ?Sized),
    session_id: &crate::SessionId,
) -> Result<Option<crate::RuntimeSessionState>, crate::StoreError> {
    let store = store.runtime_store();
    store.read_session_state_version(session_id).await?;
    let Some(read) = load_current_window(store, session_id).await? else {
        return Ok(None);
    };
    if read.session_id != *session_id {
        return Err(crate::StoreError::StoredDataCorrupt {
            record_kind: "SessionWindowRead",
            message: format!(
                "a window read for session `{session_id}` names session `{}`",
                read.session_id
            ),
        });
    }
    crate::store::window_state(read, store.fleet_format()).map(|loaded| Some(loaded.state))
}

/// Every stored usage row of `session_id`, in `seq` order, paged through
/// [`SessionHistoryStore::load_usage_ledger_page`](crate::store::SessionHistoryStore::load_usage_ledger_page).
pub(crate) async fn load_usage_ledger(
    store: &dyn RuntimeStore,
    session_id: &crate::SessionId,
) -> Result<Vec<crate::TokenLedgerEntry>, crate::StoreError> {
    const PAGE: std::num::NonZeroU32 = std::num::NonZeroU32::MIN.saturating_add(63);
    let mut entries = Vec::new();
    let mut after = None;
    loop {
        let page = store
            .load_usage_ledger_page(session_id, after.as_ref(), PAGE)
            .await?;
        entries.extend(page.rows.into_iter().map(|row| row.entry));
        match page.next {
            Some(next) => after = Some(next),
            None => return Ok(entries),
        }
    }
}

/// Every failure settlement of `session_id`, in `(committed_at_ms, turn_id)`
/// order, paged through
/// [`SessionHistoryStore::load_failure_evidence_page`](crate::store::SessionHistoryStore::load_failure_evidence_page).
pub(crate) async fn load_failure_evidence(
    store: &dyn RuntimeStore,
    session_id: &crate::SessionId,
) -> Result<Vec<crate::TurnFailureSettlement>, crate::StoreError> {
    const PAGE: std::num::NonZeroU32 = std::num::NonZeroU32::MIN.saturating_add(15);
    let mut settlements = Vec::new();
    let mut after = None;
    loop {
        let page = store
            .load_failure_evidence_page(session_id, after.as_ref(), PAGE)
            .await?;
        settlements.extend(page.settlements);
        match page.next {
            Some(next) => after = Some(next),
            None => return Ok(settlements),
        }
    }
}

/// Whether a view whose session may have been deleted still reads
/// `node_id`. Every history read of a deleted session answers
/// [`crate::StoreError::SessionDeleted`] (ADR 0112 §6), which reads nothing.
pub(crate) async fn node_readable_through_deleted(
    store: &crate::store::SessionStore,
    node_id: &str,
) -> Result<bool, crate::StoreError> {
    match node_readable(store, node_id).await {
        Err(crate::StoreError::SessionDeleted { .. }) => Ok(false),
        other => other,
    }
}

/// One stored node of `session_id`: a one-node page anchored at it
/// (ADR 0112 §6). A node the session cannot read is the typed
/// [`crate::StoreError::HistoryAnchorUnavailable`], never `Ok(None)`.
pub(crate) async fn load_one_node(
    store: &dyn RuntimeStore,
    session_id: &crate::SessionId,
    node_id: &str,
) -> Result<crate::SessionNodeRecord, crate::StoreError> {
    let page = store
        .load_ancestors(
            session_id,
            crate::store::HistoryAnchor::Node(crate::NodeId::from(node_id)),
            crate::store::HistoryBudget {
                max_nodes: std::num::NonZeroU32::MIN,
                max_bytes: std::num::NonZeroU64::MAX,
            },
        )
        .await?;
    let mut nodes = page.nodes.into_iter();
    match (nodes.next(), nodes.next()) {
        (Some(node), None) if node.record.node_id.as_str() == node_id => Ok(node.record),
        (first, _) => Err(crate::StoreError::Backend(format!(
            "a one-node page anchored at `{node_id}` returned {:?}",
            first.map(|node| node.record.node_id)
        ))),
    }
}

/// Whether the view's session can read `node_id`: a one-node page anchored at
/// it (ADR 0112 §6). An unreadable node is the typed
/// [`crate::StoreError::HistoryAnchorUnavailable`]; any other error is returned.
pub(crate) async fn node_readable(
    store: &crate::store::SessionStore,
    node_id: &str,
) -> Result<bool, crate::StoreError> {
    match load_one_node(store.store().as_ref(), store.session_id(), node_id).await {
        Ok(_) => Ok(true),
        Err(crate::StoreError::HistoryAnchorUnavailable { .. }) => Ok(false),
        Err(error) => Err(error),
    }
}

/// Queued turn work carrying `text`: one process wake of `process` at
/// `sequence`. A process wake is the one turn-work payload; a frame handoff is
/// the head's pending follow-on, never a queue row (ADR 0101 §3). The source
/// key is the wake's own, so the same `(process, sequence)` names the same row.
pub(crate) fn process_wake_work(
    session_id: &crate::SessionId,
    process: &str,
    sequence: u64,
    text: &str,
    delivery_policy: crate::DeliveryPolicy,
) -> crate::QueuedWorkBatchDraft {
    let process_id = crate::ProcessId::fixture(process);
    let wake = crate::ProcessWakeDelivery {
        version: crate::FleetFormat::current().writer_version(lash_core::surface_format!(
            PROCESS_WAKE_DELIVERY_FORMAT_VERSION
        )),
        wake_id: format!("wake:{session_id}:{process}:{sequence}"),
        target_session_id: session_id.clone(),
        process_id: process_id.clone(),
        sequence,
        event_type: "process.wake".to_string(),
        event_invocation: crate::RuntimeInvocation {
            attribution: crate::RuntimeAttribution::for_session(session_id),
            subject: crate::RuntimeSubject::ProcessEvent {
                process_id: process_id.clone(),
                sequence,
                event_type: "process.wake".to_string(),
            },
            caused_by: None,
            replay: None,
        },
        process_caused_by: None,
        authority: crate::QueuedWorkAuthority::default(),
        input: text.to_string(),
        created_at_ms: 1,
    };
    crate::QueuedWorkBatchDraft::new(
        session_id,
        delivery_policy,
        crate::TurnWorkPayload::process_wake(wake),
    )
    .with_source_key(crate::process_wake_source_key(&process_id, sequence))
    .with_process_wake_source(process_id, sequence)
}

/// `registration` as a runtime start realized under `starter` records it: its
/// ancestry is `[starter]`, and it lives `Until` its starter (FIG-3607 R1,
/// R3).
pub(crate) fn started_until_starter(
    registration: crate::ProcessRegistration,
    starter: crate::ScopeId,
) -> crate::ProcessRegistration {
    started_until(registration, starter.clone(), starter)
}

/// `registration` as a runtime start realized under `starter` records it,
/// living `Until` `scope`, which must be `starter` or the session above it.
pub(crate) fn started_until(
    mut registration: crate::ProcessRegistration,
    starter: crate::ScopeId,
    scope: crate::ScopeId,
) -> crate::ProcessRegistration {
    registration.ancestry = if starter == scope {
        crate::Ancestry::from_scopes([starter])
    } else {
        crate::Ancestry::from_scopes([starter, scope.clone()])
    };
    registration.lifetime = crate::LifetimeDecision::Until {
        scope,
        grant: crate::ScopeGrant::Ancestor,
    };
    registration
}

/// `registration` as a runtime start realized under `starter` records it,
/// `Detached` from every scope.
pub(crate) fn started_detached(
    mut registration: crate::ProcessRegistration,
    starter: crate::ScopeId,
) -> crate::ProcessRegistration {
    registration.ancestry = crate::Ancestry::from_scopes([starter]);
    registration.lifetime = crate::LifetimeDecision::Detached;
    registration
}

/// One due-obligation pass over `stores`' `ParentEnd` ledger, built the way
/// the deployment's reconcile tick wires it (ADR 0109 §1.5): a
/// [`ParentEndRelay`](lash_core::runtime::drive::ParentEndRelay) over the
/// store set's ledger, the registry the claims name rows in, and a native
/// process port — the same `deliver_cancel` the law's native executions read
/// their cancel request from.
///
/// This is the obligation's delivery half: the `ScopeClose` delivery records
/// the missing ledger rows (ADR 0094), and this claims each armed `ParentEnd`
/// row, applies its plan and settles it — or retries it under its backoff when
/// the delivery cannot run.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the store set's ports and the pass bound are established"
)]
pub(crate) async fn deliver_due_parent_end_obligations(
    stores: &Arc<dyn crate::StoreSet>,
) -> lash_core::engine::RelayPass {
    let registry = stores.process_registry();
    let clock = stores.clock();
    let relay = lash_core::runtime::drive::ParentEndRelay::new(
        stores.obligation_ledger(crate::store::ObligationKind::ParentEnd),
        Arc::clone(&registry),
        Arc::new(crate::NoProcessWork::for_registry(Arc::clone(&registry))),
        Arc::clone(&clock),
    );
    lash_core::runtime::drive::relay::relay_due(
        &relay,
        clock.as_ref(),
        std::num::NonZeroUsize::new(256).expect("parent-end page bound is non-zero"),
    )
    .await
    .expect("the parent-end obligation pass runs")
}
