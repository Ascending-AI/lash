use crate::admin::SessionConfigPatch;
#[cfg(feature = "rlm")]
use crate::support::SessionSpec;
use crate::support::SessionWorkEngine;
use crate::support::{
    Arc, CancellationToken, DeploymentStore, EmbedError, LashCore, PluginFactory, ProcessRegistry,
    PromptContribution, PromptLayerSink, PromptSlot, PromptTemplate, ProviderHandle, Result,
    RunActivityCollector, RuntimeSessionState, SessionError, SessionObservationSubscription,
    SessionResume, StaticPluginFactory, StdMutex, ToolProvider, TurnActivity, TurnActivityId,
    TurnActivitySink, TurnEvent, TurnInput, TurnOutcome, TurnReport, async_trait, message_text,
};
use lash_core::ProcessExecutionEnvStore;
#[cfg(feature = "rlm")]
use lash_core::facade_support::RuntimeSessionStateFacadeOps;
use lash_core::facade_support::{
    AgentFrameReasonFacadeOps, SessionGraphFacadeOps, SessionNodeProjection, ToolStateFacadeOps,
};
use lash_core::{ProcessLifecycle as _, ProcessRegistrar as _};
use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt;
use std::collections::VecDeque;
#[cfg(feature = "rlm")]
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::{
    LlmContentBlock, LlmRequest, LlmResponse, LlmRole, LlmStreamEvent, ResponseTextMeta,
};
use lash_core::{LlmOutputPart, SessionProcessEventKind, StoreError, ToolDefinitionBindingExt};
use tokio::sync::{Mutex as TokioMutex, oneshot};

/// Create a session's durable metadata without building a runtime.
///
/// A Durable Session never creates (ADR 0119), so a test that enqueues to a
/// session it has not opened creates it first through the facade's only
/// creating verb — the same move an in-repo host that relied on
/// enqueue-materialisation now makes.
pub(crate) async fn create_catalog_session(core: &LashCore, session_id: &str) -> Result<()> {
    core.session(session_id)
        .create(crate::SessionCreation::default())
        .await?;
    Ok(())
}

/// The crate's one test path to a session that may not exist yet (FIG-4112).
///
/// Only `create` creates, so a test that is not about creation reaches its
/// session through this: [`created`](Self::created) creates the builder's
/// session with the core's config — pinned to the builder's provider, when it
/// names one — unless the catalog already holds it, then hands the builder
/// back for its terminal verb. An existing or deleted id is left as it is,
/// so the verb that follows reports it. Tests about creation call
/// [`SessionBuilder::create`](crate::SessionBuilder::create) themselves.
pub(crate) trait CreatedSession: Sized {
    async fn created(self) -> Self;
}

impl CreatedSession for crate::SessionBuilder {
    async fn created(self) -> Self {
        let mut spec = crate::SessionSpec::default();
        if let Some(provider) = &self.provider {
            spec = spec.provider_id(provider.kind());
        }
        match self
            .core
            .session(self.session_id.clone())
            .create(crate::SessionCreation {
                spec,
                ..Default::default()
            })
            .await
        {
            Ok(_)
            | Err(EmbedError::SessionAlreadyExists { .. })
            | Err(EmbedError::Store(StoreError::SessionDeleted { .. })) => self,
            Err(error) => panic!("create session `{}`: {error:?}", self.session_id),
        }
    }
}

/// Every node of an ancestry from the head, newest first, fetched a page
/// at a time through `page`.
async fn paged_history<F, Fut>(mut page: F) -> Result<Vec<lash_core::store::HistoryNode>>
where
    F: FnMut(lash_core::store::HistoryAnchor, lash_core::store::HistoryBudget) -> Fut,
    Fut: std::future::Future<Output = Result<lash_core::store::HistoryPage>>,
{
    let budget = lash_core::store::HistoryBudget {
        max_nodes: std::num::NonZeroU32::new(64).expect("non-zero page"),
        max_bytes: std::num::NonZeroU64::new(1 << 20).expect("non-zero page"),
    };
    let mut anchor = lash_core::store::HistoryAnchor::Head;
    let mut nodes = Vec::new();
    loop {
        let page = page(anchor, budget).await?;
        nodes.extend(page.nodes);
        match page.next {
            Some(cursor) => anchor = lash_core::store::HistoryAnchor::Cursor(cursor),
            None => return Ok(nodes),
        }
    }
}

/// Every node of `durable`'s ancestry from its head, newest first, paged
/// through the history reader: prior frames stay durable but not resident.
pub(crate) async fn durable_history(
    durable: &crate::DurableSession,
) -> Result<Vec<lash_core::store::HistoryNode>> {
    paged_history(|anchor, budget| durable.history(anchor, budget)).await
}

/// Every node of `store`'s session ancestry from its head, newest first.
pub(crate) async fn store_history(
    store: &lash_core::store::SessionStore,
) -> Result<Vec<lash_core::store::HistoryNode>> {
    paged_history(|anchor, budget| async move { Ok(store.load_ancestors(anchor, budget).await?) })
        .await
}

/// The agent frames `history` records, oldest first: the durable frames a
/// current-frame-only resident state no longer carries.
pub(crate) fn history_frames(
    history: Vec<lash_core::store::HistoryNode>,
    session_id: &SessionId,
) -> Vec<lash_core::AgentFrameRecord> {
    let records = history
        .into_iter()
        .rev()
        .map(|node| node.record)
        .collect::<Vec<_>>();
    let leaf = records.last().map(|record| record.node_id.clone());
    lash_core::SessionGraph::from_nodes(records, leaf)
        .expect("a durable ancestry is a valid graph")
        .agent_frame_records(session_id)
}

/// The operation of a head a test seeds directly: `label` names the seed,
/// so each seeded head is its own commit rather than a replay.
fn seed_operation(label: &str) -> lash_core::OperationId {
    lash_core::OperationId::new(
        lash_core::ExecutionScope::runtime_operation(format!("test-seed:{label}")),
        "commit",
    )
}

/// A memory backend whose catalog holds `state`'s session, its head
/// committed once with `state`'s graph and checkpoint under the config its
/// policy records: the durable head a test reopens. The view is that
/// session on the backend's catalog.
pub(crate) async fn backend_seeded(
    state: RuntimeSessionState,
) -> (lash_core::Backend, lash_core::store::SessionStore) {
    let config = lash_core::PersistedSessionConfig::from(&state.policy);
    backend_seeded_with_config(state, config).await
}

/// [`backend_seeded`] whose head records `config`.
pub(crate) async fn backend_seeded_with_config(
    mut state: RuntimeSessionState,
    config: lash_core::PersistedSessionConfig,
) -> (lash_core::Backend, lash_core::store::SessionStore) {
    let backend = double_backend().await;
    let store = lash_core::runtime::admit_session_view(
        &backend.session_store_factory(),
        &lash_core::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: state.session_id.clone(),
            relation: lash_core::SessionRelation::Root,
            config: state.policy.clone().into(),
            head: lash_core::SessionCreationHead::CommittedByCreator,
        },
    )
    .await
    .expect("admit the seeded session");
    state.ensure_agent_frame_initialized();
    let mut commit = lash_core::RuntimeCommit::persisted_state_with_operation_for_testing(
        &state,
        &[],
        seed_operation("head"),
    );
    commit.config = config;
    store
        .commit_runtime_state(commit)
        .await
        .expect("commit the seeded head");
    (backend, store)
}

/// Commit a new head of `store`'s session whose config records
/// `provider_id`: another runtime moving the durable provider pin.
pub(crate) async fn set_head_provider_id(
    store: &lash_core::store::SessionStore,
    provider_id: impl Into<String>,
) {
    let loaded = lash_core::store::load_session_window_state(
        store,
        lash_core::store::WindowSelector::Current,
    )
    .await
    .expect("load the seeded head")
    .expect("the seeded session has a head");
    let mut commit = lash_core::RuntimeCommit::persisted_state_with_operation_for_testing(
        &loaded.state,
        &[],
        seed_operation("provider"),
    );
    commit.config = loaded.config;
    commit.config.provider_id = provider_id.into();
    store
        .commit_runtime_state(commit)
        .await
        .expect("commit the moved provider pin");
}

/// A memory backend whose catalog is decorated by `decorate`: a test
/// catalog that records or faults the requests it serves.
pub(crate) async fn backend_with_catalog(
    decorate: impl FnOnce(Arc<dyn lash_core::DeploymentStore>) -> Arc<dyn lash_core::DeploymentStore>,
) -> DecoratedBackend {
    DecoratedBackend::over(double_backend().await).session_store_factory(decorate)
}

/// Admits on the catalog it wraps and records the request of every
/// admission that created its session.
struct RecordingAdmissions {
    inner: Arc<dyn lash_core::DeploymentStore>,
    requests: Arc<std::sync::Mutex<Vec<lash_core::SessionStoreCreateRequest>>>,
}

impl RecordingAdmissions {
    /// A recording layer and the requests it will record.
    fn over(
        inner: Arc<dyn lash_core::DeploymentStore>,
    ) -> (
        Arc<dyn lash_core::DeploymentStore>,
        Arc<std::sync::Mutex<Vec<lash_core::SessionStoreCreateRequest>>>,
    ) {
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let layer = Arc::new(Self {
            inner,
            requests: Arc::clone(&requests),
        });
        (layer, requests)
    }
}

#[async_trait]
impl lash_core::store::RuntimeStoreDecorator for RecordingAdmissions {
    type Inner = dyn lash_core::DeploymentStore;

    fn inner(&self) -> &Self::Inner {
        self.inner.as_ref()
    }

    async fn admit_session(
        &self,
        request: &lash_core::SessionStoreCreateRequest,
    ) -> std::result::Result<lash_core::store::SessionAdmission, StoreError> {
        let admission = self.inner.admit_session(request).await?;
        // Only the admission that creates the session records what it was
        // created with; a rebinding admission leaves the row untouched.
        if admission == lash_core::store::SessionAdmission::Created {
            self.requests.lock_recover().push(request.clone());
        }
        Ok(admission)
    }
}

impl lash_core::DeploymentStoreDecorator for RecordingAdmissions {}

/// Serves the catalog it wraps and names every session write it serves: an
/// admission that creates a session, a runtime commit and a session-meta save.
/// A test clears the ledger, acts, and asserts the act wrote nothing — not
/// merely that the values it reads back are equal.
pub(crate) struct CountingWrites {
    inner: Arc<dyn lash_core::DeploymentStore>,
    writes: Arc<std::sync::Mutex<Vec<&'static str>>>,
}

impl CountingWrites {
    /// A counting layer and the ledger it will fill.
    pub(crate) fn over(
        inner: Arc<dyn lash_core::DeploymentStore>,
    ) -> (
        Arc<dyn lash_core::DeploymentStore>,
        Arc<std::sync::Mutex<Vec<&'static str>>>,
    ) {
        let writes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let layer = Arc::new(Self {
            inner,
            writes: Arc::clone(&writes),
        });
        (layer, writes)
    }

    fn record(&self, write: &'static str) {
        self.writes.lock_recover().push(write);
    }
}

#[async_trait]
impl lash_core::store::RuntimeStoreDecorator for CountingWrites {
    type Inner = dyn lash_core::DeploymentStore;

    fn inner(&self) -> &Self::Inner {
        self.inner.as_ref()
    }

    async fn admit_session(
        &self,
        request: &lash_core::SessionStoreCreateRequest,
    ) -> std::result::Result<lash_core::store::SessionAdmission, StoreError> {
        let admission = self.inner.admit_session(request).await?;
        if admission == lash_core::store::SessionAdmission::Created {
            self.record("create");
        }
        Ok(admission)
    }

    async fn commit_runtime_state(
        &self,
        commit: lash_core::RuntimeCommit,
    ) -> std::result::Result<lash_core::store::RuntimeCommitReceipt, StoreError> {
        self.record("commit");
        self.inner.commit_runtime_state(commit).await
    }

    async fn save_session_meta(
        &self,
        meta: lash_core::SessionMeta,
    ) -> std::result::Result<(), StoreError> {
        self.record("session_meta");
        self.inner.save_session_meta(meta).await
    }
}

impl lash_core::DeploymentStoreDecorator for CountingWrites {}

#[derive(Default)]
struct RecordingEvents {
    events: TokioMutex<Vec<TurnActivity>>,
}

impl RecordingEvents {
    async fn snapshot(&self) -> Vec<TurnActivity> {
        self.events.lock().await.clone()
    }
}

#[async_trait]
impl TurnActivitySink for RecordingEvents {
    async fn emit(&self, activity: TurnActivity) {
        self.events.lock().await.push(activity);
    }
}

fn test_activity(correlation_id: &str, event: TurnEvent) -> TurnActivity {
    TurnActivity::new(TurnActivityId::new(correlation_id.to_string()), event)
}

fn assistant_prose(events: &[TurnActivity]) -> String {
    events
        .iter()
        .filter_map(|activity| match &activity.event {
            TurnEvent::AssistantProseDelta { text, .. } => Some(text.as_ref()),
            _ => None,
        })
        .collect()
}

struct AppTools;

#[async_trait]
impl ToolProvider for AppTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![app_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "app_lookup").then(|| Arc::new(app_tool_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async { lash_core::ToolOutcome::ok(serde_json::json!({ "ok": true })) })
            .await
            .into()
    }
}

#[cfg(feature = "rlm")]
struct FailingAppTools;

#[cfg(feature = "rlm")]
#[async_trait]
impl ToolProvider for FailingAppTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![app_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "app_lookup").then(|| Arc::new(app_tool_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async { lash_core::ToolOutcome::err_fmt("lookup failed but Lashlang recovered") })
            .await
            .into()
    }
}

struct PendingAppTools {
    key_tx: StdMutex<Option<oneshot::Sender<lash_core::AwaitEventKey>>>,
}

impl PendingAppTools {
    fn new(key_tx: oneshot::Sender<lash_core::AwaitEventKey>) -> Self {
        Self {
            key_tx: StdMutex::new(Some(key_tx)),
        }
    }
}

#[async_trait]
impl ToolProvider for PendingAppTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![app_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "app_lookup").then(|| Arc::new(app_tool_definition().contract()))
    }

    fn attempt_may_defer(&self, tool_id: &lash_core::ToolId) -> bool {
        tool_id == app_tool_definition().id()
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async {
            assert_eq!(call.name(), "app_lookup");
            let key = match call.context.completion_key() {
                Ok(key) => key,
                Err(err) => return lash_core::ToolOutcome::err_fmt(err),
            };
            if let Some(tx) = self.key_tx.lock_recover().take() {
                let _ = tx.send(key);
            }
            lash_core::ToolOutcome::pending(lash_core::PendingCompletion::new())
        })
        .await
        .into()
    }
}

#[cfg(feature = "rlm")]
struct DurableInputTools {
    key_tx:
        StdMutex<Option<oneshot::Sender<std::result::Result<lash_core::AwaitEventKey, String>>>>,
    attempt_count: Arc<AtomicUsize>,
}

#[cfg(feature = "rlm")]
struct RetryingDirectTools;

#[cfg(feature = "rlm")]
#[async_trait]
impl ToolProvider for RetryingDirectTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![retrying_direct_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "retrying_direct").then(|| Arc::new(retrying_direct_tool_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        assert_eq!(call.name(), "retrying_direct");
        let model = match call.context.sessions().model().await {
            Ok(model) => model,
            Err(err) => return lash_core::ToolOutcome::err_fmt(err).into(),
        };
        let completion = match call
            .context
            .direct_completions()
            .complete(
                lash_core::facade_support::DirectRequest::text(
                    model.model,
                    format!(
                        "retrying direct completion attempt {}",
                        call.context.attempt_number()
                    ),
                ),
                "retrying_direct",
            )
            .await
        {
            Ok(completion) => completion,
            Err(err) => return lash_core::ToolOutcome::err_fmt(err).into(),
        };
        if call.context.attempt_number() == 1 {
            return lash_core::ToolOutcome::failure(lash_core::ToolFailure::safe_retry(
                lash_core::ToolFailureClass::Execution,
                "retrying_direct_first_attempt",
                "retry the complete atomic attempt",
                Some(0),
            ))
            .into();
        }
        lash_core::ToolOutcome::ok(serde_json::json!(completion.text)).into()
    }
}

#[cfg(feature = "rlm")]
fn retrying_direct_tool_definition() -> lash_core::ToolDefinition {
    test_tool_definition_with_tool_binding(
        lash_core::ToolDefinition::raw(
            "tool:retrying_direct",
            "retrying_direct",
            "Call a direct completion and retry the complete attempt once.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "string" }),
        )
        .with_retry_policy(lash_core::ToolRetryPolicy::safe(2, 0, 0)),
        "retrying_direct",
    )
}

#[cfg(feature = "rlm")]
impl DurableInputTools {
    fn new(key_tx: oneshot::Sender<std::result::Result<lash_core::AwaitEventKey, String>>) -> Self {
        Self {
            key_tx: StdMutex::new(Some(key_tx)),
            attempt_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn attempt_count(&self) -> usize {
        self.attempt_count.load(Ordering::SeqCst)
    }

    fn send_key_result(&self, result: std::result::Result<lash_core::AwaitEventKey, String>) {
        if let Some(tx) = self.key_tx.lock_recover().take() {
            let _ = tx.send(result);
        }
    }
}

#[cfg(feature = "rlm")]
#[async_trait]
impl ToolProvider for DurableInputTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![durable_input_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "mock_input_request").then(|| Arc::new(durable_input_tool_definition().contract()))
    }

    fn attempt_may_defer(&self, tool_id: &lash_core::ToolId) -> bool {
        tool_id == durable_input_tool_definition().id()
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async {
            assert_eq!(call.name(), "mock_input_request");
            let question = call
                .args
                .get("question")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("answer")
                .to_string();
            let key = match call.context.completion_key() {
                Ok(key) => key,
                Err(err) => {
                    self.send_key_result(Err(err.to_string()));
                    return lash_core::ToolOutcome::err_fmt(err);
                }
            };
            self.attempt_count.fetch_add(1, Ordering::SeqCst);
            // The attempt body cannot append process events. It declares the
            // announcement instead, and the runtime appends it when the call parks.
            let announcement = lash_core::PendingAnnouncement::new(
                "process.yield",
                serde_json::json!({
                    "type": "work.input_request.opened",
                    "request_id": "request-1",
                    "question": question,
                    "await_key_id": key.key_id,
                }),
                "mock-input-request:request-1",
            );
            self.send_key_result(Ok(key));
            lash_core::ToolOutcome::pending(
                lash_core::PendingCompletion::new().announcing(announcement),
            )
        })
        .await
        .into()
    }
}

#[cfg(feature = "rlm")]
fn durable_input_tool_definition() -> lash_core::ToolDefinition {
    test_tool_definition_with_tool_binding(
        lash_core::ToolDefinition::raw(
            "tool:mock_input_request",
            "mock_input_request",
            "Open a durable input request and wait for the answer.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "question": { "type": "string" }
                },
                "required": ["question"],
                "additionalProperties": false
            }),
            serde_json::json!({
                "type": "object",
                "properties": {
                    "request_id": { "type": "string" },
                    "answer": {}
                },
                "required": ["request_id", "answer"],
                "additionalProperties": true
            }),
        ),
        "mock_input_request",
    )
}

struct AgentFrameSwitchTools;

#[async_trait]
impl ToolProvider for AgentFrameSwitchTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![agent_frame_switch_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "switch_frame").then(|| Arc::new(agent_frame_switch_tool_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async {
            assert_eq!(call.name(), "switch_frame");
            let task = call
                .args
                .get("task")
                .and_then(serde_json::Value::as_str)
                .expect("task arg")
                .to_string();
            lash_core::ToolOutcome::ok(serde_json::json!({ "ok": true })).with_control(
                lash_core::ToolControl::SwitchAgentFrame {
                    frame_key: lash_core::FrameKey::from_caller_material("durable-follow-frame")
                        .expect("non-empty caller material"),
                    initial_nodes: Vec::new(),
                    task: Some(task),
                },
            )
        })
        .await
        .into()
    }
}

fn agent_frame_switch_tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:switch_frame",
        "switch_frame",
        "Switch to a fresh agent frame.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "task": { "type": "string" }
            },
            "required": ["task"],
            "additionalProperties": false
        }),
        serde_json::json!({ "type": "object" }),
    )
}

fn app_tool_definition() -> lash_core::ToolDefinition {
    test_tool_definition_with_tool_binding(
        lash_core::ToolDefinition::raw(
            "tool:app_lookup",
            "app_lookup",
            "Look up app state.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "object" }),
        ),
        "app_lookup",
    )
}

struct LongTextTools;

#[async_trait]
impl ToolProvider for LongTextTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![long_text_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "app_lookup").then(|| Arc::new(long_text_tool_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async {
            lash_core::ToolOutcome::ok(serde_json::json!("abcdefghijklmnopqrstuvwxyz0123456789"))
        })
        .await
        .into()
    }
}

fn long_text_tool_definition() -> lash_core::ToolDefinition {
    test_tool_definition_with_tool_binding(
        lash_core::ToolDefinition::raw(
            "tool:app_lookup",
            "app_lookup",
            "Look up verbose app state.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "string" }),
        ),
        "app_lookup",
    )
}

fn test_tool_definition_with_tool_binding(
    definition: lash_core::ToolDefinition,
    name: impl Into<String>,
) -> lash_core::ToolDefinition {
    definition.with_tool_binding(lash_core::ToolBinding::new(["tools"], name))
}

struct SurfacePluginFactory;

impl lash_core::facade_support::PluginFactory for SurfacePluginFactory {
    fn id(&self) -> &'static str {
        "surface_test"
    }

    fn build(
        &self,
        _ctx: &lash_core::facade_support::PluginSessionContext,
    ) -> std::result::Result<
        Arc<dyn lash_core::facade_support::SessionPlugin>,
        lash_core::PluginError,
    > {
        Ok(Arc::new(SurfacePlugin))
    }
}

struct SurfacePlugin;

impl lash_core::facade_support::SessionPlugin for SurfacePlugin {
    fn id(&self) -> &'static str {
        "surface_test"
    }

    fn register(
        &self,
        reg: &mut lash_core::facade_support::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        reg.output().response(Arc::new(|ctx| {
            Box::pin(async move {
                Ok(lash_core::facade_support::AssistantResponseTransform {
                    response: ctx.response,
                    events: vec![lash_core::PluginRuntimeEvent::Status {
                        key: "surface".to_string(),
                        label: "working".to_string(),
                        detail: Some("details".to_string()),
                    }],
                })
            })
        }));
        Ok(())
    }
}

fn mock_provider() -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("embed-test")
        .requires_streaming(true)
        .complete(|request| async move {
            let user_text = last_user_text(&request);
            let reply = format!("echo: {user_text}");
            if let Some(events) = request.stream_events.as_ref() {
                events.send(LlmStreamEvent::Delta {
                    block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                    text: reply.clone(),
                });
            }
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: reply,
                    response_meta: None,
                }],
                usage: lash_core::llm::types::LlmUsage {
                    input_tokens: user_text.split_whitespace().count() as i64,
                    output_tokens: 2,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    reasoning_output_tokens: 0,
                },
                response_metadata: Default::default(),
                ..LlmResponse::default()
            })
        })
        .build()
        .into_handle()
}

/// A second provider whose kind differs from [`mock_provider`], for pinning
/// tests that must name a provider the session did not record.
fn other_kind_provider() -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("other-embed-test")
        .complete(|_request| async move {
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "other".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            })
        })
        .build()
        .into_handle()
}

fn tool_roundtrip_provider() -> ProviderHandle {
    let responses = Arc::new(TokioMutex::new(VecDeque::from([
        LlmResponse {
            parts: vec![LlmOutputPart::ToolCall {
                call_id: "call-1".to_string(),
                tool_name: "app_lookup".to_string(),
                input_json: "{}".to_string(),
                replay: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        },
        LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "done".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        },
    ])));
    crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(move |_request| {
            let responses = Arc::clone(&responses);
            async move { Ok(responses.lock().await.pop_front().expect("queued response")) }
        })
        .build()
        .into_handle()
}

fn agent_frame_switch_provider() -> ProviderHandle {
    let responses = Arc::new(TokioMutex::new(VecDeque::from([
        LlmResponse {
            parts: vec![LlmOutputPart::ToolCall {
                call_id: "switch-call".to_string(),
                tool_name: "switch_frame".to_string(),
                input_json: serde_json::json!({
                    "task": "finish in the next frame"
                })
                .to_string(),
                replay: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        },
        text_response("done after frame switch"),
    ])));
    crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(move |_request| {
            let responses = Arc::clone(&responses);
            async move { Ok(responses.lock().await.pop_front().expect("queued response")) }
        })
        .build()
        .into_handle()
}

fn text_response(text: &str) -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text: text.to_string(),
            response_meta: None,
        }],
        response_metadata: Default::default(),
        ..LlmResponse::default()
    }
}

#[cfg(feature = "rlm")]
fn typescript_block(source: &str) -> String {
    format!("<typescript>\n{}\n</typescript>", source.trim())
}

#[cfg(feature = "rlm")]
fn queued_text_provider(texts: Vec<impl Into<String>>) -> ProviderHandle {
    let responses = Arc::new(TokioMutex::new(VecDeque::from(
        texts
            .into_iter()
            .map(|text| {
                let text = text.into();
                LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text,
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                }
            })
            .collect::<Vec<_>>(),
    )));
    crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(move |_request| {
            let responses = Arc::clone(&responses);
            async move { Ok(responses.lock().await.pop_front().expect("queued response")) }
        })
        .build()
        .into_handle()
}

fn semantic_group_provider() -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(|_request| async move {
            Ok(LlmResponse {
                parts: vec![
                    LlmOutputPart::Text {
                        text: "first".to_string(),
                        response_meta: Some(ResponseTextMeta {
                            id: Some("assistant:first".to_string()),
                            status: None,
                            phase: None,
                            ..ResponseTextMeta::default()
                        }),
                    },
                    LlmOutputPart::Text {
                        text: "second".to_string(),
                        response_meta: Some(ResponseTextMeta {
                            id: Some("assistant:second".to_string()),
                            status: None,
                            phase: None,
                            ..ResponseTextMeta::default()
                        }),
                    },
                ],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            })
        })
        .build()
        .into_handle()
}

fn text_provider(kind: &'static str, _model: &'static str, text: &'static str) -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind(kind)
        .complete(move |_request| async move {
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: text.to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            })
        })
        .build()
        .into_handle()
}

type SeenModels = Arc<std::sync::Mutex<Vec<(String, lash_core::ReasoningSelection)>>>;

fn recording_text_provider(
    kind: &'static str,
    _model: &'static str,
    _variant: Option<&'static str>,
    text: &'static str,
    seen: SeenModels,
) -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind(kind)
        .complete(move |request| {
            let seen = Arc::clone(&seen);
            async move {
                seen.lock_recover()
                    .push((request.model, request.model_variant));
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: text.to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

fn last_user_text(request: &LlmRequest) -> String {
    request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == LlmRole::User)
        .map(|message| {
            message
                .blocks
                .iter()
                .filter_map(|block| match block {
                    LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn system_text(request: &LlmRequest) -> String {
    request
        .instructions
        .as_deref()
        .unwrap_or_default()
        .to_owned()
}

fn request_text(request: &LlmRequest) -> String {
    request
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn recording_prompt_provider(seen: Arc<std::sync::Mutex<Vec<String>>>) -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("prompt-test")
        .complete(move |request| {
            let seen = Arc::clone(&seen);
            async move {
                seen.lock_recover().push(system_text(&request));
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "ok".to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

#[cfg(feature = "rlm")]
fn recording_request_provider(seen: Arc<std::sync::Mutex<Vec<String>>>) -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("request-test")
        .complete(move |request| {
            let seen = Arc::clone(&seen);
            async move {
                seen.lock_recover().push(request_text(&request));
                Ok(text_response(&typescript_block("finish(\"ok\");")))
            }
        })
        .build()
        .into_handle()
}

fn retry_once_provider() -> ProviderHandle {
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    crate::testing::TestProvider::builder()
        .kind("retry-test")
        .requires_streaming(true)
        .options(lash_core::facade_support::ProviderOptions {
            reliability: lash_core::provider::ProviderReliability::default()
                .max_attempts(2)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..lash_core::facade_support::ProviderOptions::default()
        })
        .complete(move |_request| {
            let attempts = Arc::clone(&attempts);
            async move {
                if attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    return Err(LlmTransportError::new("retry me").with_retry_verdict(
                        lash_core::llm::transport::TransportRetryVerdict::RetryableTransient,
                    ));
                }
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "retried".to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

fn checkpoint_gated_provider(
    entered_tx: oneshot::Sender<()>,
    release_rx: oneshot::Receiver<()>,
) -> ProviderHandle {
    let entered_tx = Arc::new(std::sync::Mutex::new(Some(entered_tx)));
    let release_rx = Arc::new(TokioMutex::new(Some(release_rx)));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    crate::testing::TestProvider::builder()
        .kind("checkpoint-gated")
        .complete(move |request| {
            let entered_tx = Arc::clone(&entered_tx);
            let release_rx = Arc::clone(&release_rx);
            let calls = Arc::clone(&calls);
            async move {
                let call = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if call == 0 {
                    if let Some(tx) = entered_tx.lock_recover().take() {
                        let _ = tx.send(());
                    }
                    if let Some(rx) = release_rx.lock().await.take() {
                        let _ = rx.await;
                    }
                    Ok(text_response("first"))
                } else {
                    Ok(text_response(&format!(
                        "after {}",
                        last_user_text(&request)
                    )))
                }
            }
        })
        .build()
        .into_handle()
}

pub(crate) async fn standard_core() -> LashCore {
    standard_core_over(double_backend().await)
}

/// A standard core over `backend`.
pub(crate) fn standard_core_over(backend: lash_core::Backend) -> LashCore {
    explicit_ephemeral_facets(LashCore::standard_builder(
        backend,
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())
    .expect("standard core")
}

/// Default RLM protocol factory for tests, over `backend`, the substrate its
/// Lashlang artifacts live in.
#[cfg(feature = "rlm")]
fn rlm_factory(backend: &lash_core::Backend) -> lash_protocol_rlm::RlmProtocolPluginFactory {
    lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        backend,
    )
}

/// A [`LashCoreBuilder`] pre-seeded with the default RLM factory.
#[cfg(feature = "rlm")]
async fn rlm_core_builder() -> crate::core::LashCoreBuilder {
    rlm_core_builder_over(double_backend().await)
}

/// [`rlm_core_builder`] over `backend`: the core and its RLM factory share
/// the one backend.
#[cfg(feature = "rlm")]
fn rlm_core_builder_over(backend: lash_core::Backend) -> crate::core::LashCoreBuilder {
    let factory = rlm_factory(&backend);
    LashCore::rlm_builder(backend, crate::TurnBudget::Unbounded, factory)
}

mod scope_support;
use scope_support::{
    delete_bound_session, delete_bound_session_outcome, host_scope, runtime_operation_scope,
    text_message,
};
mod control_admin;
mod core_session_builder;
mod deployment_and_testing_facade;
mod durable_session;
mod harness;
pub(crate) use harness::{
    AcceptedSend as _, DecoratedBackend, core_now_ms, double_backend,
    double_backend_explicit_reconcile, double_backend_over, double_backend_over_explicit_reconcile,
    explicit_ephemeral_facets, explicit_ephemeral_facets_with_budget, held_double, latest_double,
    memory_store_backend, memory_store_set, mock_model_spec, model_spec, output_into_cancelled_by,
    redeploy, restate_double, retry_when_claim_frees, run_async_test_on_stack_budget,
    serve_processes, settle_session_drive, store_backend_with_clock, turn_input_states,
};
#[cfg(feature = "rlm")]
mod adr_claims;
mod agent_scenarios;
#[cfg(feature = "rlm")]
mod aggregate_await_comprehension;
#[cfg(feature = "rlm")]
mod aggregate_oracle;
mod commit_superseded;
#[cfg(feature = "rlm")]
mod discovery_execution;
mod failure_settlement;
mod finalize_fault;
mod obligation_relays;
mod plugin_stack;
#[cfg(feature = "rlm")]
mod processes_endstate;
#[cfg(feature = "rlm")]
mod redrive_residue;
#[cfg(feature = "rlm")]
mod rlm_restore_idempotence;
mod send_handle;
mod session_control;
mod session_drive;
#[cfg(feature = "rlm")]
mod stack_budget;
mod standard_compaction_persistence;
mod tool_intent_ingress;
mod tool_restore_report;
mod turn_streaming;
#[cfg(feature = "rlm")]
mod usage_durability;
#[cfg(feature = "rlm")]
mod withheld_follow_on;
