//! Store-factory decorator that observes real runtime-checkpoint commits, so a
//! harness can render durable-write lines from facts the backend accepted.

use crate::SessionId;
use lash_sansio::sync::MutexExt;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::store::{
    RuntimeCommit, RuntimeCommitReceipt, RuntimeStore, RuntimeStoreDecorator, StoreError,
    WindowSelector,
};
use crate::{AttachmentId, BlobRef, DeploymentStore};
use lash_core_execution::DeploymentStoreDecorator;
use serde::{Deserialize, Serialize};

/// Schema tag carried on every observed commit.
///
/// The value keeps its historical `lash.sim.` prefix because generated
/// simulation trace artifacts embed it; the observer itself is no longer
/// simulator-specific.
pub const CHECKPOINT_WRITE_EVENT_SCHEMA: &str = "lash.sim.checkpoint-write-event.v4";

/// One checkpoint component observed at the successful store-commit seam.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CheckpointComponentWrite {
    pub component: CheckpointComponent,
    pub kind: CheckpointComponentWriteKind,
}

/// Closed vocabulary of runtime-checkpoint components.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointComponent {
    TurnState,
    ToolState,
    PluginState,
    ExecutionState,
}

impl CheckpointComponent {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TurnState => "turn_state",
            Self::ToolState => "tool_state",
            Self::PluginState => "plugin_state",
            Self::ExecutionState => "execution_state",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CheckpointComponentWriteKind {
    /// Decoded plugin state, including the host-owned generations.
    PluginState {
        state: crate::PluginState,
    },
    /// Body present at the commit seam. `logical_bytes` is the size of the
    /// encoding-independent JSON projection used only for human comparison;
    /// it is not a backend's MessagePack/compressed byte count.
    Stored {
        #[serde(default, alias = "bytes", skip_serializing_if = "Option::is_none")]
        logical_bytes: Option<usize>,
    },
    UnchangedRef,
}

/// A successful runtime-state commit as observed by the simulator's store
/// wrapper. Component bodies are inspected before delegation, while the
/// resulting head revision is recorded only after the backend accepts them.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CheckpointWriteEvent {
    pub schema: String,
    /// The real session id passed to the store commit.
    pub session_id: SessionId,
    /// Optional generated-trace attribution for a separately executed contract
    /// proof. Ordinary generated runtime commits use `session_id` directly.
    /// Flattened so the wire keys stay `attributed_session_id` and
    /// `cause_boundary_id`.
    #[serde(flatten)]
    pub attribution: Option<CheckpointAttribution>,
    pub commit_index: usize,
    pub turn_index: usize,
    pub revision_before: u64,
    pub revision_after: u64,
    pub components: Vec<CheckpointComponentWrite>,
    /// Submitted rows plus the accepted raw/read projections observed after the
    /// commit. Simulation checkers fold these values without calling store or
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<CheckpointStateWrite>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CheckpointStateWrite {
    pub submitted_graph_append: serde_json::Value,
    pub submitted_turn_state: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_raw_rows: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_read_model: Option<serde_json::Value>,
}

/// Generated-trace attribution for a separately executed contract proof: the
/// session the proof belongs to and the boundary that caused it. Runtime-turn
/// writes are linked by session plus turn instead.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CheckpointAttribution {
    #[serde(rename = "attributed_session_id")]
    pub session_id: SessionId,
    pub cause_boundary_id: String,
}

impl CheckpointWriteEvent {
    pub fn has_unchanged_ref(&self) -> bool {
        self.components
            .iter()
            .any(|component| component.kind == CheckpointComponentWriteKind::UnchangedRef)
    }

    pub fn attributed_session(&self) -> &str {
        self.attribution
            .as_ref()
            .map(|attribution| attribution.session_id.as_str())
            .unwrap_or(&self.session_id)
    }
}

/// Shared sink used by every store handle created during one generated run.
///
/// This observes commits made through decorated `DeploymentStore` handles.
/// `DurableProcessWorker` task bodies run storeless reconstruction runtimes
/// that commit no session state, so they are outside this collector's
/// coverage; transcript consumers are warned at their emitter boundary too.
#[derive(Clone, Debug, Default)]
pub struct CheckpointWriteCollector {
    state: Arc<Mutex<CheckpointWriteCollectorState>>,
    ref_only_mutation: Option<RefOnlyCommitMutation>,
}

#[derive(Debug, Default)]
struct CheckpointWriteCollectorState {
    events: Vec<CheckpointWriteEvent>,
    next_commit_by_session: BTreeMap<String, usize>,
    commit_budgets: BTreeMap<(SessionId, u64), crate::testing::RuntimeCommitBudgetMeasurement>,
    committed_attachment_ids: BTreeMap<(SessionId, u64), Vec<AttachmentId>>,
    latest_components_by_session:
        BTreeMap<SessionId, BTreeMap<String, crate::CheckpointComponentDescriptor>>,
}

#[derive(Clone, Debug)]
struct RefOnlyCommitMutation {
    session_id: SessionId,
    revision_before: u64,
}

impl CheckpointWriteCollector {
    /// Configure the regression-test mutation that reproduces the missing-body
    /// defect: when an updated component also carries its prior ref, drop the
    /// body before the backend sees the commit.
    ///
    /// This is the injected defect that proves a durable-write transcript can
    /// still discriminate a missing component body (ADR 0044's mutation rule).
    pub fn with_ref_only_mutation(session_id: impl Into<SessionId>, revision_before: u64) -> Self {
        Self {
            state: Arc::default(),
            ref_only_mutation: Some(RefOnlyCommitMutation {
                session_id: session_id.into(),
                revision_before,
            }),
        }
    }

    pub fn events(&self) -> Vec<CheckpointWriteEvent> {
        let mut events = self.state.lock_recover().events.clone();
        events.sort_by(|left, right| {
            (
                left.attributed_session(),
                left.revision_before,
                left.revision_after,
                left.commit_index,
            )
                .cmp(&(
                    right.attributed_session(),
                    right.revision_before,
                    right.revision_after,
                    right.commit_index,
                ))
        });
        events
    }

    /// Return the exact pre-transaction budget measurement for an observed,
    /// successfully committed runtime write.
    pub fn runtime_commit_budget(
        &self,
        session_id: &SessionId,
        revision_before: u64,
    ) -> Option<crate::testing::RuntimeCommitBudgetMeasurement> {
        self.state
            .lock_recover()
            .commit_budgets
            .get(&(session_id.clone(), revision_before))
            .copied()
    }

    /// Return the attachment roots submitted by one observed runtime commit.
    pub fn committed_attachment_ids(
        &self,
        session_id: &SessionId,
        revision_before: u64,
    ) -> Option<Vec<AttachmentId>> {
        self.state
            .lock_recover()
            .committed_attachment_ids
            .get(&(session_id.clone(), revision_before))
            .cloned()
    }

    /// Record one observed commit, assigning its per-session commit index.
    ///
    /// Harnesses call this when re-attributing a commit that a separately
    /// executed proof produced; the decorator calls it for every commit it sees.
    pub fn push(&self, mut event: CheckpointWriteEvent) {
        let mut state = self.state.lock_recover();
        let session_id = event.attributed_session().to_string();
        let next = state.next_commit_by_session.entry(session_id).or_insert(0);
        *next += 1;
        event.commit_index = *next;
        state.events.push(event);
    }

    fn push_runtime_commit(
        &self,
        event: CheckpointWriteEvent,
        budget: crate::testing::RuntimeCommitBudgetMeasurement,
        committed_attachment_ids: Vec<AttachmentId>,
    ) {
        let key = (event.session_id.clone(), event.revision_before);
        self.push(event);
        let mut state = self.state.lock_recover();
        state.commit_budgets.insert(key.clone(), budget);
        state
            .committed_attachment_ids
            .insert(key, committed_attachment_ids);
    }

    fn apply_mutation(&self, commit: &mut RuntimeCommit) {
        let Some(mutation) = &self.ref_only_mutation else {
            return;
        };
        if commit.session_id != mutation.session_id
            || commit.expected_head_revision != mutation.revision_before
        {
            return;
        }
        let prior_components = self
            .state
            .lock_recover()
            .latest_components_by_session
            .get(&commit.session_id)
            .cloned()
            .unwrap_or_default();
        for (key, component) in &mut commit.checkpoint.components {
            if component.body().is_some()
                && let Some(descriptor) = prior_components.get(key).cloned()
            {
                *component = crate::HydratedCheckpointComponent::Unchanged { descriptor };
            }
        }
    }

    fn record_manifest(&self, session_id: &SessionId, manifest: &crate::SessionCheckpoint) {
        self.state
            .lock_recover()
            .latest_components_by_session
            .insert(session_id.clone(), manifest.components.clone());
    }
}

/// It preserves the backend contract exactly and adds observation only after a real commit
/// succeeds, which is what makes the resulting durable-write transcript lines real facts
/// rather than harness-constructed ones.
pub struct ObservedDeploymentStore {
    inner: Arc<dyn DeploymentStore>,
    collector: CheckpointWriteCollector,
}

impl ObservedDeploymentStore {
    pub fn new(inner: Arc<dyn DeploymentStore>, collector: CheckpointWriteCollector) -> Self {
        Self { inner, collector }
    }
}

/// Give conformance roles distinct outer handles over one in-memory substrate
/// without adding `Clone` or shared-field semantics to the production store.
#[cfg(any(test, feature = "testing"))]
pub fn fresh_runtime_persistence_handle(inner: Arc<dyn RuntimeStore>) -> Arc<dyn RuntimeStore> {
    Arc::new(ObservedRuntimeStore {
        inner,
        collector: CheckpointWriteCollector::default(),
    })
}

#[async_trait::async_trait]
impl RuntimeStoreDecorator for ObservedDeploymentStore {
    type Inner = dyn DeploymentStore;

    fn inner(&self) -> &Self::Inner {
        self.inner.as_ref()
    }

    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitReceipt, StoreError> {
        observe_commit(self.inner.as_ref(), &self.collector, commit).await
    }
}

impl DeploymentStoreDecorator for ObservedDeploymentStore {}

struct ObservedRuntimeStore {
    inner: Arc<dyn RuntimeStore>,
    collector: CheckpointWriteCollector,
}

#[async_trait::async_trait]
impl RuntimeStoreDecorator for ObservedRuntimeStore {
    type Inner = dyn RuntimeStore;

    fn inner(&self) -> &Self::Inner {
        self.inner.as_ref()
    }

    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitReceipt, StoreError> {
        observe_commit(self.inner.as_ref(), &self.collector, commit).await
    }
}

async fn observe_commit(
    inner: &dyn RuntimeStore,
    collector: &CheckpointWriteCollector,
    mut commit: RuntimeCommit,
) -> Result<RuntimeCommitReceipt, StoreError> {
    collector.apply_mutation(&mut commit);
    let mut event = checkpoint_write_event(&commit);
    let budget = crate::testing::measure_runtime_commit_budget(&commit)?;
    let committed_attachment_ids = commit.committed_attachment_ids.clone();
    let result = inner.commit_runtime_state(commit).await?;
    collector.record_manifest(&event.session_id, &result.manifest);
    if let Some(state) = event.state.as_mut()
        && let Some(accepted) = inner
            .load_session_window(&event.session_id, WindowSelector::Current)
            .await?
    {
        let read_model = accepted.window.read_model();
        state.accepted_raw_rows = Some(serde_json::json!({
            "graph_nodes": accepted.window.nodes,
            "graph_leaf_node_id": accepted.window.leaf_node_id,
            "turn_state": accepted.checkpoint.as_ref().map(|checkpoint| &checkpoint.turn_state),
        }));
        state.accepted_read_model = Some(serde_json::json!({
            "graph_node_count": accepted.window.nodes.len(),
            "messages": read_model.messages.as_ref(),
            "token_usage": accepted.checkpoint.as_ref().map(|checkpoint| &checkpoint.turn_state.token_usage),
        }));
    }
    collector.push_runtime_commit(
        CheckpointWriteEvent {
            revision_after: result.head_revision,
            ..event
        },
        budget,
        committed_attachment_ids,
    );
    Ok(result)
}

fn checkpoint_write_event(commit: &RuntimeCommit) -> CheckpointWriteEvent {
    let checkpoint = &commit.checkpoint;
    let mut components = vec![CheckpointComponentWrite {
        component: CheckpointComponent::TurnState,
        kind: CheckpointComponentWriteKind::Stored {
            logical_bytes: checkpoint_encoded_len(&checkpoint.turn_state).ok(),
        },
    }];
    record_component(
        &mut components,
        CheckpointComponent::ToolState,
        checkpoint.component_ref(crate::store::TOOL_STATE_CHECKPOINT_COMPONENT),
        checkpoint
            .component_body(crate::store::TOOL_STATE_CHECKPOINT_COMPONENT)
            .map(|body| Some(body.len())),
    );
    if let Some(body) = checkpoint.component_body(crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT) {
        let state = rmp_serde::from_slice::<crate::PluginState>(body)
            .expect("runtime plugin state encodes");
        components.push(CheckpointComponentWrite {
            component: CheckpointComponent::PluginState,
            kind: CheckpointComponentWriteKind::PluginState { state },
        });
    } else if checkpoint
        .component_ref(crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT)
        .is_some()
    {
        components.push(CheckpointComponentWrite {
            component: CheckpointComponent::PluginState,
            kind: CheckpointComponentWriteKind::UnchangedRef,
        });
    }
    // Execution state is an opaque `Vec<u8>` the engine owns, so it is recorded
    // as written without a size. Measuring it the way typed components are
    // measured would serialize the bytes as a JSON decimal array, which reports
    // roughly 3.5x the real length and — because digits per byte depend on the
    // byte's value — shifts whenever an embedded identifier changes. That made
    // transcripts flaky on cosmetic churn while staying blind to real payload
    // differences, since genuinely different execution states round to the same
    // rendered size. `lash-sim`'s contract support omits it for the same reason.
    record_component(
        &mut components,
        CheckpointComponent::ExecutionState,
        checkpoint.component_ref(crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT),
        checkpoint
            .component_body(crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT)
            .map(|_| None),
    );
    CheckpointWriteEvent {
        schema: CHECKPOINT_WRITE_EVENT_SCHEMA.to_string(),
        session_id: commit.session_id.clone(),
        attribution: None,
        commit_index: 0,
        turn_index: commit.checkpoint.turn_state.turn_index,
        revision_before: commit.expected_head_revision,
        revision_after: 0,
        components,
        state: Some(CheckpointStateWrite {
            // The observation keeps the pre-enum `GraphAppend` wire shape:
            // checkers fold appended node rows plus the leaf the commit
            submitted_graph_append: serde_json::json!({
                "nodes": commit.graph.nodes(),
                "leaf_node_id": commit
                    .graph
                    .leaf_node_id()
                    .or(commit.graph_base_leaf_node_id.as_ref()),
            }),
            submitted_turn_state: serde_json::to_value(&checkpoint.turn_state)
                .expect("runtime turn state is serializable"),
            accepted_raw_rows: None,
            accepted_read_model: None,
        }),
    }
}

fn checkpoint_encoded_len(value: &impl Serialize) -> Result<usize, rmp_serde::encode::Error> {
    rmp_serde::to_vec_named(value).map(|bytes| bytes.len())
}

/// `logical_bytes` presence marks the component as written this commit; the
/// inner value is its rendered size, which callers omit for opaque blobs.
fn record_component(
    components: &mut Vec<CheckpointComponentWrite>,
    component: CheckpointComponent,
    component_ref: Option<&BlobRef>,
    logical_bytes: Option<Option<usize>>,
) {
    let kind = if let Some(logical_bytes) = logical_bytes {
        Some(CheckpointComponentWriteKind::Stored { logical_bytes })
    } else if component_ref.is_some() {
        Some(CheckpointComponentWriteKind::UnchangedRef)
    } else {
        None
    };
    if let Some(kind) = kind {
        components.push(CheckpointComponentWrite { component, kind });
    }
}
