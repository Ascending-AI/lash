//! Shared fixtures for the conformance suites: paired handles opened
//! against the same durable backing store, used by the `*_reopenable`
//! suite variants.

use super::*;
use crate::ActorContext;

pub(crate) fn assert_fresh_instances<T: ?Sized>(left: &Arc<T>, right: &Arc<T>, suite: &str) {
    assert!(
        !Arc::ptr_eq(left, right),
        "{suite} factory reused one Arc across conformance roles"
    );
}

/// The newest revision `session_id` still retains that published `leaf`: the
/// state a law forks or pins when it holds a leaf rather than a revision.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(crate) async fn revision_at<S>(catalog: &S, session_id: &SessionId, leaf: &str) -> u64
where
    S: crate::SessionCatalogStore + ?Sized,
{
    catalog
        .revisions(session_id)
        .await
        .expect("list the session's retained revisions")
        .into_iter()
        .rev()
        .find(|revision| revision.leaf_node_id.as_deref() == Some(leaf))
        .unwrap_or_else(|| panic!("`{session_id}` retains no revision at leaf `{leaf}`"))
        .head_revision
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
    intent: crate::AttachmentWrite,
) {
    let crate::AttachmentWriteFence::Granted(permit) = store
        .begin_attachment_write(&intent)
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

/// The second published environment used by reference-count and rebinding
/// laws: distinct bytes and therefore a distinct content-addressed reference.
#[expect(clippy::expect_used, reason = "conformance environment fixture")]
pub fn process_registry_alternate_environment_ref() -> crate::ProcessExecutionEnvRef {
    alternate_process_environment()
        .stable_ref()
        .expect("alternate environment encodes")
}

fn alternate_process_environment() -> crate::ProcessExecutionEnvSpec {
    crate::ProcessExecutionEnvSpec::new(
        crate::AdmittedPluginConfig::default(),
        crate::SessionPolicy::new(crate::TurnBudget::Unbounded, crate::MaxToolCalls::new(1)),
    )
}

/// Publish the two environments registry fixtures register under. Admission
/// requires stored bytes even when a law concerns only the registry's rows.
#[expect(clippy::expect_used, reason = "conformance environment fixture")]
pub async fn publish_process_registry_fixture_environments(
    store: &dyn crate::ProcessExecutionEnvStore,
) {
    lash_core::testing::process_execution_env_fixture(store).await;
    crate::publish_process_execution_env(
        store,
        &lash_core::testing::host_pin_claim_for_testing(),
        &alternate_process_environment(),
    )
    .await
    .expect("alternate environment publishes");
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
pub struct ReopenableRuntimeStore {
    pub open: Arc<dyn RuntimeStore>,
    pub reopen: Arc<dyn RuntimeStore>,
    pub effect_host: ActorContext,
}

/// A pair of [`AttachmentStore`](crate::AttachmentStore) handles opened against
/// the same durable backing store.
pub struct ReopenableAttachmentStore {
    pub open: Arc<dyn crate::AttachmentStore>,
    pub reopen: Arc<dyn crate::AttachmentStore>,
}

/// Push an unpersisted event node onto `state`'s active path and make it the
/// resident leaf. Pair with [`commit_conformance_state`] to advance a session's
/// durable head from outside any runtime.
pub(crate) use lash_core::testing::store_fixtures::{
    admit_conformance_session, append_conformance_event_node, commit_conformance_state,
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
            crate::store::HistoryAnchor::Node(crate::NodeId::fixture(node_id)),
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

/// A lifecycle wait fact with stable replay identity for store laws.
pub(crate) fn call_wait_event(
    process: &crate::ProcessId,
    label: &str,
    replay: &str,
    payload: serde_json::Value,
) -> crate::ProcessEventAppendRequest {
    let wait = crate::WaitState {
        since_ms: 1,
        kind: crate::WaitKind::Call {
            call_id: lash_core::ToolCallId::fixture(&format!("{label}:{payload}")),
            tool_id: lash_core::ToolId::new(label),
        },
    };
    crate::ProcessEventAppendRequest::wait_entered(process, &wait)
        .with_replay_key(format!("law:{label}:{replay}"))
}
