//! Runtime turn-machine fixtures shared by `lash-core`'s own unit tests and by
//! the relocated `runtime::tests` integration binaries.
//!
//! These were `crate::runtime::tests::helpers` until the runtime suites were
//! promoted out of the library's unit-test target; they live behind the
//! `testing` feature, the crate's existing seam for test-only surface.

use crate::llm::transport::LlmTransportError;
use crate::llm::types::LlmStreamEvent;
use crate::plugin::PluginSessionRequest;
use crate::plugin::StaticPluginFactory;
use crate::runtime::*;
use crate::testing::TestProvider;
use crate::*;
use lash_sansio::sync::MutexExt;
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub use super::layered_backend::{LayeredBackend, LayeredStores};
pub use super::recording_store::{EndRefusedRootHook, RecordingDeploymentStore, RecordingStore};

pub struct FixedAttachmentRoots(pub std::collections::BTreeSet<crate::AttachmentId>);

#[async_trait::async_trait]
#[diagnostic::do_not_recommend]
impl crate::AttachmentRootSet for FixedAttachmentRoots {
    async fn attachment_root_page(
        &self,
        source: crate::attachments::AttachmentRootSource,
        after: Option<&crate::AttachmentId>,
    ) -> Result<crate::attachments::AttachmentRootPage, crate::StoreError> {
        use crate::attachments::{AttachmentRootPage, AttachmentRootSource};
        let roots =
            if source == AttachmentRootSource::Referrer(crate::ArtifactReferrerKind::Session) {
                self.0
                    .iter()
                    .filter(|id| after.is_none_or(|after| *id > after))
                    .take(AttachmentRootPage::QUERY_LIMIT)
                    .cloned()
                    .collect()
            } else {
                Vec::new()
            };
        AttachmentRootPage::from_rows(roots)
    }

    async fn has_live_attachment_ref(
        &self,
        id: &crate::AttachmentId,
    ) -> Result<bool, crate::StoreError> {
        Ok(self.0.contains(id))
    }
}

pub fn default_state() -> RuntimeSessionState {
    let mut state = RuntimeSessionState::new(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ));
    state.ensure_agent_frame_initialized();
    state
}

/// Admits `admitted` on a runtime host's own effect host.
///
/// A turn-driving test must bind its scope to the host the turn runs on: the
/// driver publishes the live opener to that host's `ToolChildHost` registry,
/// the execution context publishes recorded envs to that host's env store,
/// and group children resolve both through the executors registered on the
/// admitted controller — a scope minted on a foreign controller leaves every
/// child unroutable (ADR 0099 §2, §3).
/// `scoped_static` returns `None` for hosts that lend no `'static` controller
/// (a Restate `ctx`-bound one); there the caller's own bound controller is the
/// scope and this helper does not apply.
pub fn host_admitted_scope(
    config: &crate::RuntimeHostConfig,
    admitted: crate::AdmittedScope,
) -> crate::ScopedEffectController<'static> {
    config
        .control
        .effect_host
        .scoped_static(admitted)
        .expect("effect host scoped_static")
        .expect("effect host lends a 'static controller for this scope")
}

/// Admits `admitted` on `backend`'s effect host: the host a runtime built
/// over `backend` runs on, so its tool-child routing and env store are the
/// runtime's own.
pub fn backend_admitted_scope(
    backend: &crate::Backend,
    admitted: crate::AdmittedScope,
) -> crate::ScopedEffectController<'static> {
    backend
        .effect_host()
        .scoped_static(admitted)
        .expect("effect host scoped_static")
        .expect("effect host lends a 'static controller for this scope")
}

/// `backend_admitted_scope` for a turn scope.
pub fn backend_turn_scope(
    backend: &crate::Backend,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> crate::ScopedEffectController<'static> {
    backend_admitted_scope(backend, crate::AdmittedScope::turn(session_id, turn_id))
}

/// `backend_admitted_scope` for a process scope, standing in for the
/// worker's admission step.
pub fn backend_process_scope(
    backend: &crate::Backend,
    process_id: &ProcessId,
) -> crate::ScopedEffectController<'static> {
    backend_admitted_scope(backend, crate::AdmittedScope::process(process_id.clone()))
}

/// `host_admitted_scope` for a turn scope.
pub fn host_turn_scope(
    config: &crate::RuntimeHostConfig,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> crate::ScopedEffectController<'static> {
    host_admitted_scope(config, crate::AdmittedScope::turn(session_id, turn_id))
}

/// `host_admitted_scope` for a process scope, standing in for the worker's
/// admission step.
pub fn host_process_scope(
    config: &crate::RuntimeHostConfig,
    process_id: &ProcessId,
) -> crate::ScopedEffectController<'static> {
    host_admitted_scope(config, crate::AdmittedScope::process(process_id.clone()))
}

pub trait ReadModelState {
    fn read_model(&self) -> crate::session_graph::SessionReadModel;
}

impl ReadModelState for SessionSnapshot {
    fn read_model(&self) -> crate::session_graph::SessionReadModel {
        self.read_model()
    }
}

impl ReadModelState for RuntimeSessionState {
    fn read_model(&self) -> crate::session_graph::SessionReadModel {
        self.read_model()
    }
}

pub trait ReadModelStateMut: ReadModelState {
    fn append_message(&mut self, message: Message);
}

impl ReadModelStateMut for SessionSnapshot {
    fn append_message(&mut self, message: Message) {
        self.session_graph.append_message(message);
    }
}

impl ReadModelStateMut for RuntimeSessionState {
    fn append_message(&mut self, message: Message) {
        self.ensure_agent_frame_initialized();
        self.session_graph.append_message(message);
    }
}

pub fn active_conversation_messages(state: &impl ReadModelState) -> Vec<Message> {
    state.read_model().messages.to_vec()
}

pub fn append_message(state: &mut impl ReadModelStateMut, message: Message) {
    state.append_message(message);
}

/// Apply a host head write as a session's drive does (FIG-4202), for a test
/// that runs no engine: enqueue `command` on the session's command lane in
/// `store` under `idempotency_key`, seal a fresh admission on `store` as a
/// command root's seal does, drain the command lane under that fence on
/// `runtime`, and answer the typed outcome the command settled with.
///
/// # Errors
///
/// The submission, the seal, the drain or the settlement read failed, or the
/// command did not settle applied.
pub async fn apply_host_command(
    runtime: &mut LashRuntime,
    store: &dyn crate::RuntimeStore,
    command: SessionCommand,
    idempotency_key: &str,
) -> Result<SessionCommandOutcome, RuntimeError> {
    let session_id = SessionId::from(runtime.session_id());
    let (receipt, fence) =
        submit_host_command(store, &session_id, command, idempotency_key).await?;
    let applied_here = drain_host_commands(runtime, &fence, None)
        .await?
        .contains(&receipt.batch_id);
    settle_host_command(runtime, receipt, applied_here).await
}

/// Submit a host's session command as its submission records it (FIG-4202),
/// for a test that runs no engine: enqueue `command` on session
/// `session_id`'s command lane in `store` under `idempotency_key`, and seal
/// a fresh admission as a command root's seal does. Answers the command's
/// receipt and the sealed fence its drain presents.
///
/// # Errors
///
/// The submission or the seal failed.
pub async fn submit_host_command(
    store: &dyn crate::RuntimeStore,
    session_id: &SessionId,
    command: SessionCommand,
    idempotency_key: &str,
) -> Result<(crate::SessionCommandReceipt, crate::store::DriveFence), RuntimeError> {
    // Enqueued through the store: a test that runs no engine has no ingress
    // relay to deliver it.
    let source_key = command.source_key(idempotency_key);
    let batch = store
        .enqueue_queued_work(
            crate::QueuedWorkBatchDraft::new(
                session_id.clone(),
                crate::DeliveryPolicy::AfterCurrentTurnCommit,
                command,
            )
            .with_source_key(source_key.clone()),
        )
        .await
        .map_err(|error| {
            RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, error.to_string())
        })?;
    let receipt = crate::SessionCommandReceipt {
        session_id: session_id.clone(),
        batch_id: batch.batch_id,
        source_key,
    };
    let observed = store.drive_epoch(session_id).await.map_err(|error| {
        RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, error.to_string())
    })?;
    let admission = crate::store::AdmissionId::new(format!(
        "host-command:{idempotency_key}:{}",
        uuid::Uuid::new_v4()
    ));
    match store
        .seal_drive_epoch(
            session_id,
            &admission,
            observed.epoch,
            &crate::store::RootStartNonce::new(uuid::Uuid::new_v4().to_string()),
        )
        .await
        .map_err(|error| {
            RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, error.to_string())
        })? {
        crate::store::DriveEpochSeal::Sealed(fence) => Ok((receipt, fence)),
        other => Err(RuntimeError::new(
            RuntimeErrorCode::StoreCommitSuperseded,
            format!("the host command's seal did not land: {other:?}"),
        )),
    }
}

/// Drain the session's command lane on `runtime` under `fence` until it is
/// empty, as a command root does, on `controller` when one is given: the
/// controller a tier's handler lends, for an effect host that runs effects
/// only in a handler. A replay of that handler reads back the runs its
/// journal recorded. Answers the batches each drain applied.
///
/// # Errors
///
/// A drain failed.
pub async fn drain_host_commands(
    runtime: &mut LashRuntime,
    fence: &crate::store::DriveFence,
    controller: Option<&crate::ScopedEffectController<'_>>,
) -> Result<Vec<crate::BatchId>, RuntimeError> {
    let mut drained = Vec::new();
    loop {
        let next = match controller {
            Some(controller) => {
                runtime
                    .drain_next_session_command_with_cancellation(
                        fence,
                        tokio_util::sync::CancellationToken::new(),
                        controller,
                    )
                    .await?
            }
            None => runtime.drain_next_session_command(fence).await?,
        };
        let Some(next) = next else {
            return Ok(drained);
        };
        drained.push(next.batch_id);
    }
}

/// Read the typed outcome the command `receipt` names settled with. A
/// command another runtime applied (`applied_here` false) leaves `runtime`
/// to adopt the head that runtime committed.
///
/// # Errors
///
/// The settlement read failed, or the command did not settle applied.
pub async fn settle_host_command(
    runtime: &mut LashRuntime,
    receipt: crate::SessionCommandReceipt,
    applied_here: bool,
) -> Result<SessionCommandOutcome, RuntimeError> {
    match runtime.settle_session_command(receipt).await? {
        SessionCommandSettlement::Applied { outcome, .. } => {
            if !applied_here {
                runtime
                    .refresh_session_graph_from_store()
                    .await
                    .map_err(|error| {
                        RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, error.to_string())
                    })?;
            }
            Ok(outcome)
        }
        other => Err(RuntimeError::new(
            RuntimeErrorCode::SessionCommandRun,
            format!("the host command did not settle applied: {other:?}"),
        )),
    }
}

/// [`apply_host_command`] for an append: the append's typed outcome.
///
/// # Errors
///
/// As [`apply_host_command`], or the append failed to apply.
pub async fn apply_host_append(
    runtime: &mut LashRuntime,
    store: &dyn crate::RuntimeStore,
    request: AppendSessionNodesRequest,
) -> Result<AppendSessionNodesOutcome, RuntimeError> {
    let key = request.operation_id.clone();
    append_outcome(
        apply_host_command(
            runtime,
            store,
            SessionCommand::AppendSessionNodes {
                request: Box::new(request),
            },
            &key,
        )
        .await?,
    )
}

/// The append outcome a host append's command settled with.
///
/// # Errors
///
/// The append failed to apply, or the command settled as another command.
pub fn append_outcome(
    outcome: SessionCommandOutcome,
) -> Result<AppendSessionNodesOutcome, RuntimeError> {
    match outcome {
        SessionCommandOutcome::AppendSessionNodes { outcome } => Ok(outcome),
        SessionCommandOutcome::Failed { code, message } => Err(RuntimeError::new(code, message)),
        other => Err(RuntimeError::new(
            RuntimeErrorCode::SessionCommandRun,
            format!("an append settled with another command's outcome: {other:?}"),
        )),
    }
}

#[derive(Clone, Default)]
pub struct RecordingSink {
    pub events: Arc<Mutex<Vec<SessionStreamEvent>>>,
}

#[async_trait::async_trait]
impl EventSink for RecordingSink {
    async fn emit(&self, event: SessionStreamEvent) {
        self.events.lock_recover().push(event);
    }
}

impl RecordingSink {
    pub fn snapshot(&self) -> Vec<SessionStreamEvent> {
        self.events.lock_recover().clone()
    }
}

#[derive(Clone, Default)]
pub struct RecordingTurnEvents {
    pub events: Arc<Mutex<Vec<TurnActivity>>>,
}

#[async_trait::async_trait]
impl TurnActivitySink for RecordingTurnEvents {
    async fn emit(&self, activity: TurnActivity) {
        self.events.lock_recover().push(activity);
    }
}

impl RecordingTurnEvents {
    pub fn snapshot(&self) -> Vec<TurnActivity> {
        self.events.lock_recover().clone()
    }
}

#[derive(Debug)]
pub struct MockCall {
    pub stream_events: Vec<LlmStreamEvent>,
    pub response: Result<LlmResponse, LlmTransportError>,
}

pub fn mock_provider(calls: Vec<MockCall>) -> TestProvider {
    mock_provider_with_kind("mock", calls)
}

pub fn mock_openai_compatible_provider(calls: Vec<MockCall>) -> TestProvider {
    mock_provider_with_kind("openai-compatible", calls)
}

fn mock_provider_with_kind(kind: &'static str, calls: Vec<MockCall>) -> TestProvider {
    let calls = Arc::new(Mutex::new(calls));
    TestProvider::builder()
        .kind(kind)
        .requires_streaming(true)
        .complete(move |req| {
            let calls = Arc::clone(&calls);
            async move {
                let call = calls.lock_recover().remove(0);
                if let Some(tx) = req.stream_events.as_ref() {
                    for event in &call.stream_events {
                        tx.send(event.clone());
                    }
                }
                // A real provider returns the usage its stream reported on
                // the completed response too; the attempt's record, and so
                // its accounting fact, is built from that response.
                let streamed_usage =
                    call.stream_events
                        .iter()
                        .rev()
                        .find_map(|event| match event {
                            LlmStreamEvent::Usage(usage) => Some(usage.clone()),
                            _ => None,
                        });
                call.response.map(|mut response| {
                    if response.usage == crate::llm::types::LlmUsage::default()
                        && let Some(usage) = streamed_usage
                    {
                        response.usage = usage;
                    }
                    response
                })
            }
        })
        .build()
}

/// Serve the runtime session's recorded model with `provider`: the host's
/// models become a one-model registry holding exactly that binding. A
/// session with no model records the standard test model first.
pub fn set_runtime_provider(runtime: &mut LashRuntime, provider: crate::ProviderHandle) {
    let config = runtime
        .state()
        .policy
        .model
        .clone()
        .unwrap_or_else(standard_test_model_config);
    runtime.host.core.providers.models = crate::testing::single_model_registry(
        config.key().clone(),
        config.metadata().clone(),
        provider,
    );
    runtime.edit_resident_state_for_test(|state| {
        if state.policy.model.is_none() {
            state.policy.model = Some(config);
        }
    });
}

/// The model selection [`standard_test_policy`] records.
pub fn standard_test_model_config() -> crate::ModelConfig {
    crate::testing::test_model_config(
        "mock-model",
        crate::testing::test_model_metadata("mock-model"),
    )
}

/// A catalog serving `extra` first and everything `base` serves after it.
struct LayeredModels {
    extra: crate::ModelRegistry,
    base: Arc<dyn crate::RuntimeModels>,
}

impl crate::RuntimeModels for LayeredModels {
    fn snapshot(
        &self,
        key: &crate::ModelKey,
    ) -> Result<crate::RecordedModel, crate::ModelUnavailable> {
        self.extra
            .snapshot(key)
            .or_else(|_| self.base.snapshot(key))
    }

    fn bind(
        &self,
        recorded: &crate::RecordedModel,
    ) -> Result<crate::ProviderHandle, crate::ModelUnavailable> {
        match self.extra.bind(recorded) {
            Ok(provider) => Ok(provider),
            Err(crate::ModelUnavailable {
                reason: crate::ModelUnavailableReason::UnknownKey,
                ..
            }) => self.base.bind(recorded),
            Err(other) => Err(other),
        }
    }
}

/// Serve `metadata` under `key` beside everything the runtime serves now,
/// through the transport that serves the session's recorded model, and
/// return the key a config command selects it by.
#[expect(
    clippy::expect_used,
    reason = "test helper: the session's own model always binds in a fixture"
)]
pub fn serve_model_beside(
    runtime: &mut LashRuntime,
    key: &str,
    metadata: crate::ModelMetadata,
) -> crate::ModelKey {
    let current = runtime
        .state()
        .policy
        .model
        .clone()
        .unwrap_or_else(standard_test_model_config);
    let base = Arc::clone(&runtime.host.core.providers.models);
    let provider = base
        .bind(&current.model)
        .expect("the session's recorded model binds");
    let extra = crate::ModelRegistry::new()
        .register(key, crate::RegisteredModel::new(metadata, provider))
        .expect("a non-empty key registers");
    runtime.host.core.providers.models = Arc::new(LayeredModels { extra, base });
    crate::ModelKey::new(key)
}

/// Register `models` beside the runtime session's recorded model, so a
/// config command can move the session to any of them. The session's own
/// model keeps the transport it is served by now.
#[expect(
    clippy::expect_used,
    reason = "test helper: a duplicate model key is a fixture defect"
)]
pub fn serve_runtime_models(
    runtime: &mut LashRuntime,
    models: impl IntoIterator<Item = (crate::ModelKey, crate::RegisteredModel)>,
) {
    let mut registry = crate::ModelRegistry::new();
    let mut served = std::collections::BTreeSet::new();
    for (key, entry) in models {
        served.insert(key.clone());
        registry = registry
            .register(key, entry)
            .expect("each served model has its own key");
    }
    if let Some(current) = runtime.state().policy.model.clone()
        && !served.contains(current.key())
        && let Ok(provider) = runtime.host.core.providers.models.bind(&current.model)
    {
        registry = registry
            .register(
                current.key().clone(),
                crate::RegisteredModel::new(current.metadata().clone(), provider),
            )
            .expect("the session's model registers once");
    }
    runtime.host.core.providers.models = Arc::new(registry);
}

pub use crate::testing::standard_test_policy;

/// A host over `backend` whose models serve the standard test model with an
/// empty mock provider.
pub fn test_host_config(backend: &crate::Backend) -> EmbeddedRuntimeHost {
    EmbeddedRuntimeHost::new(test_runtime_host_config_with_provider(
        backend,
        mock_provider(Vec::new()).into_handle(),
    ))
}

pub fn test_host_config_with_trace_path(
    backend: &crate::Backend,
    path: PathBuf,
) -> EmbeddedRuntimeHost {
    let mut config = test_runtime_host_config(backend);
    config.tracing.trace_sink = Some(Arc::new(lash_trace::JsonlTraceSink::new(path)));
    EmbeddedRuntimeHost::new(config)
}

pub fn test_host_config_with_trace_path_and_stream_events(
    backend: &crate::Backend,
    path: PathBuf,
) -> EmbeddedRuntimeHost {
    let mut config = test_runtime_host_config(backend);
    config.tracing.trace_sink = Some(Arc::new(lash_trace::JsonlTraceSink::new(path)));
    config.tracing.trace_level = lash_trace::TraceLevel::Extended;
    EmbeddedRuntimeHost::new(config)
}

pub fn plugin_session_with_tools(
    session_id: &SessionId,
    tools: Arc<dyn crate::ToolProvider>,
) -> Arc<crate::PluginSession> {
    let tool_factory = StaticPluginFactory::new(
        crate::plugin::PluginDeclaration::initial("test_tools"),
        crate::PluginSpec::new().with_tool_provider(Arc::clone(&tools)),
    );
    let mut factories = crate::testing::test_standard_protocol_factories();
    factories.push(Arc::new(tool_factory));
    crate::PluginHost::new(factories)
        .build_session(PluginSessionRequest::creation(
            session_id,
            Default::default(),
        ))
        .expect("plugins")
}

pub struct EmptyTools;

/// Advance `store`'s durable head the way another writer would: load the
/// persisted session (or start from an empty one when the bound session has no
/// head yet), apply `change`, and commit it as the next revision.
/// Returns the head that commit wrote.
pub async fn advance_session_head(
    store: &RecordingStore,
    change: impl FnOnce(&mut RuntimeSessionState),
) -> crate::SessionHeadMeta {
    advance_session_head_fenced(store, change, true).await
}

/// [`advance_session_head`], except the commit presents no drive fence: the
/// way a writer outside every drive moves the head. The head-ownership check
/// admits a lane-less write only onto a head no commit has published over yet
/// (FIG-4202), so this is a first commit racing a bound root; the published
/// head is not `published_by_drive`, and a resumed root's head inspection
/// meets it as another writer's, `Overtaken` (FIG-4200).
pub async fn advance_session_head_unfenced(
    store: &RecordingStore,
    change: impl FnOnce(&mut RuntimeSessionState),
) -> crate::SessionHeadMeta {
    advance_session_head_fenced(store, change, false).await
}

async fn advance_session_head_fenced(
    store: &RecordingStore,
    change: impl FnOnce(&mut RuntimeSessionState),
    fenced: bool,
) -> crate::SessionHeadMeta {
    let session_id = store
        .session_id()
        .expect("recording store has a session id");
    let persisted = crate::SessionHistoryStore::load_session_window(
        store,
        &session_id,
        crate::store::WindowSelector::Current,
    )
    .await
    .expect("load the persisted session window");
    let mut state = match persisted {
        Some(read) => {
            crate::store::window_state(read, store.fleet_format())
                .expect("adopt the persisted session window")
                .state
        }
        // A session with no committed head: the other writer commits its
        // first one.
        None => {
            let meta = crate::SessionCommitStore::load_session_meta(store, &session_id)
                .await
                .expect("load the session binding")
                .expect("the store is bound to a session");
            let mut state = RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
            ));
            state.session_id = meta.session_id;
            state
        }
    };
    change(&mut state);
    // The bound turn owns the head (FIG-4202): a writer that moves it once
    // a commit has published over the created head must present the root's
    // own drive fence, as a second execution of that root would. Over the
    // created head a lane-less write is still admitted, which is what an
    // unfenced advance exercises. Before the first seal nothing owns it.
    let mut commit = crate::RuntimeCommit::persisted_state_for_test(&state);
    commit.drive_fence = if fenced {
        crate::store::current_drive_fence(store, &session_id)
            .await
            .expect("read the session's drive fence")
            .map(Box::new)
    } else {
        None
    };
    crate::SessionCommitStore::commit_runtime_state(store, commit)
        .await
        .expect("commit the advanced head");
    crate::SessionCommitStore::load_session_head_meta(store, &session_id)
        .await
        .expect("read the advanced head")
        .expect("the advanced head exists")
}

/// Create a runtime fixture's missing session explicitly; an existing fixture
/// is reopened through lookup and a tombstone is never reused.
pub async fn create_runtime_fixture_session(
    store: &dyn crate::RuntimeStore,
    session_id: &SessionId,
    policy: &crate::SessionPolicy,
) -> Result<(), crate::StoreError> {
    create_runtime_fixture_session_with_config(
        store,
        session_id,
        crate::PersistedSessionConfig::from(policy),
    )
    .await
}

/// [`create_runtime_fixture_session`], except the created head records
/// `config`: a fixture whose runtime installs config-owning plugins records
/// the namespaces they resolve, the way a creator does (FIG-4379, FIG-4553).
pub async fn create_runtime_fixture_session_with_config(
    store: &dyn crate::RuntimeStore,
    session_id: &SessionId,
    config: crate::PersistedSessionConfig,
) -> Result<(), crate::StoreError> {
    match store.lookup_session(session_id).await? {
        crate::store::SessionLookup::Live(_) => return Ok(()),
        crate::store::SessionLookup::Deleted => {
            return Err(crate::StoreError::SessionDeleted {
                session_id: session_id.clone(),
            });
        }
        crate::store::SessionLookup::Absent => {}
    }
    assert_eq!(
        store
            .admit_session(&crate::SessionStoreCreateRequest {
                session_id: session_id.clone(),
                relation: crate::SessionRelation::Root,
                pending_observer_intents: Vec::new(),
                config,
                head: crate::SessionCreationHead::Config,
                owning_process_id: None,
            })
            .await?,
        crate::store::SessionAdmission::Created,
        "runtime fixture already exists",
    );
    Ok(())
}

/// Create a fresh fixture session explicitly, refusing accidental fixture reuse.
pub async fn create_session_store(
    factory: &Arc<dyn crate::DeploymentStore>,
    request: &crate::SessionStoreCreateRequest,
) -> Result<crate::store::SessionStore, crate::StoreError> {
    assert_eq!(
        factory.admit_session(request).await?,
        crate::store::SessionAdmission::Created,
        "fixture session already exists",
    );
    let runtime: Arc<dyn crate::RuntimeStore> = factory.clone();
    crate::store::SessionStore::new(runtime, request.session_id.clone())
}

/// A fresh root session store for `session_id` from `backend`'s catalog,
/// under a [`RecordingStore`].
pub async fn recording_session_store(
    backend: &crate::Backend,
    session_id: impl Into<SessionId>,
) -> Arc<RecordingStore> {
    let session_id = session_id.into();
    let store = backend.session_store_factory();
    store
        .admit_session(&crate::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: session_id.clone(),
            relation: crate::SessionRelation::Root,
            config: crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
            )
            .into(),
            head: crate::SessionCreationHead::Config,
        })
        .await
        .expect("create a session store from the backend catalog");
    Arc::new(RecordingStore::over_session(store, session_id))
}

pub fn test_commit_budget() -> crate::CommitBudget {
    crate::CommitBudget::bounded(1024 * 1024, 512)
}

/// A host config over `backend` with the test commit budget and a batching
/// bound of one.
pub fn test_runtime_host_config(backend: &crate::Backend) -> RuntimeHostConfig {
    RuntimeHostConfig::new(
        backend.clone(),
        test_commit_budget(),
        crate::QueuedWorkBatchingConfig::new(1),
    )
}

pub fn test_runtime_host_config_with_provider(
    backend: &crate::Backend,
    provider: crate::ProviderHandle,
) -> RuntimeHostConfig {
    let mut config = test_runtime_host_config(backend);
    let model = standard_test_model_config();
    config.providers.models = crate::testing::single_model_registry(
        model.key().clone(),
        model.metadata().clone(),
        provider,
    );
    config
}

#[async_trait::async_trait]
impl crate::ToolProvider for EmptyTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        Vec::new()
    }

    fn resolve_contract(&self, _name: &str) -> Option<Arc<crate::ToolContract>> {
        None
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        crate::ToolOutcome::err(serde_json::json!("Unknown tool")).into()
    }
}

pub struct TestRuntime {
    attachment_acceptance: Arc<crate::provider::AttachmentCapabilitySnapshot>,
    plugins: Vec<Arc<dyn crate::PluginFactory>>,
    tools: Arc<dyn crate::ToolProvider>,
    transport: TestProvider,
    host: EmbeddedRuntimeHost,
    store: Option<Arc<dyn crate::RuntimeStore>>,
    process_registry: Option<Arc<dyn crate::ProcessRegistry>>,
    session_id: Option<SessionId>,
}

impl TestRuntime {
    /// A runtime over `backend`: its host, and its process registry unless
    /// [`Self::without_process_registry`] drops it.
    pub fn new(backend: &crate::Backend, transport: TestProvider) -> Self {
        Self {
            attachment_acceptance: Default::default(),
            plugins: crate::testing::test_standard_protocol_factories(),
            tools: Arc::new(EmptyTools),
            transport,
            host: test_host_config(backend),
            store: None,
            process_registry: Some(backend.process_registry()),
            session_id: None,
        }
    }

    pub fn attachment_acceptance(
        mut self,
        snapshot: Arc<crate::provider::AttachmentCapabilitySnapshot>,
    ) -> Self {
        self.attachment_acceptance = snapshot;
        self
    }

    pub fn plugins(mut self, plugins: Vec<Arc<dyn crate::PluginFactory>>) -> Self {
        self.plugins = plugins;
        self
    }

    pub fn tools(mut self, tools: Arc<dyn crate::ToolProvider>) -> Self {
        self.tools = tools;
        self
    }

    /// Replace the host. Its backend's process registry replaces the
    /// runtime's unless the runtime runs without one.
    pub fn host(mut self, host: EmbeddedRuntimeHost) -> Self {
        if self.process_registry.is_some() {
            self.process_registry = Some(host.core.backend().process_registry());
        }
        self.host = host;
        self
    }

    pub fn store(mut self, store: Arc<dyn crate::RuntimeStore>) -> Self {
        self.store = Some(store);
        self
    }

    pub fn with_session_id(mut self, session_id: impl Into<SessionId>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn process_registry(mut self, process_registry: Arc<dyn crate::ProcessRegistry>) -> Self {
        self.process_registry = Some(process_registry);
        self
    }

    pub fn without_process_registry(mut self) -> Self {
        self.process_registry = None;
        self
    }

    pub async fn build(self) -> LashRuntime {
        // `lash-core`'s own `cfg(test)` binary used to get the in-tree protocol
        // fake injected by `builtin_plugin_factories`. The relocated runtime
        // suites link the library without `cfg(test)`, so reproduce that
        // injection here, with the same id-override rule `PluginHost::new`
        // applies, rather than widening the builtin set for every crate that
        // turns on the `testing` feature.
        let mut factories = self.plugins;
        let tools = Arc::clone(&self.tools);
        factories.push(Arc::new(StaticPluginFactory::new(
            crate::plugin::PluginDeclaration::initial("test_tools"),
            crate::PluginSpec::new().with_tool_provider(Arc::clone(&tools)),
        )));
        let plugin_host = crate::testing::test_plugin_host(factories);
        let mut initial_state = RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ));
        // The fixture session records what a creator records: each installed
        // owner's namespace from its defaults (FIG-4379).
        initial_state.authority.plugin_config = plugin_host
            .resolve_creation_plugin_config(None, &crate::PluginOptions::default(), None, true)
            .unwrap_or_else(|refusal| {
                panic!("the installed owners refuse their defaults: {refusal}")
            });
        let plugin_session = plugin_host
            .build_session(PluginSessionRequest::creation(
                "root",
                crate::plugin::SessionAuthorityContext {
                    plugin_config: initial_state.admitted_plugin_config(),
                    ..Default::default()
                },
            ))
            .expect("plugins");
        if let Some(session_id) = self.session_id {
            initial_state.session_id = session_id.clone();
            initial_state.policy.session_id = Some(session_id);
        }
        let mut policy = standard_test_policy();
        policy.attachment_acceptance = self.attachment_acceptance.clone();
        initial_state.policy.attachment_acceptance = self.attachment_acceptance;
        let attachment_store = Arc::clone(&self.host.core.durability.attachment_store);
        let process_env_store = Arc::clone(&self.host.core.durability.process_env_store);
        if let Some(store) = self.store.as_ref() {
            // The created head records the same plugin config the runtime's
            // initial state carries, so a reopen adopts it (FIG-4553).
            let mut config = crate::PersistedSessionConfig::from(&policy);
            config.plugin_config = initial_state.authority.plugin_config.clone();
            create_runtime_fixture_session_with_config(
                store.as_ref(),
                &initial_state.session_id,
                config,
            )
            .await
            .expect("create the runtime fixture session");
        }
        let store = self.store.map(|store| {
            crate::store::SessionStore::new(store, initial_state.session_id.clone())
                .expect("test session id is valid")
        });
        let runtime = match (store, self.process_registry) {
            (Some(store), None) => LashRuntime::from_persistent_embedded_state(
                policy,
                self.host,
                crate::PersistentRuntimeServices::new(
                    plugin_session,
                    store,
                    attachment_store,
                    process_env_store,
                ),
                initial_state.clone(),
                crate::testing::runtime_lease_owner(),
            )
            .await
            .expect("runtime"),
            (None, None) => LashRuntime::from_embedded_state(
                policy,
                self.host,
                crate::RuntimeServices::new(plugin_session, attachment_store, process_env_store),
                initial_state.clone(),
                crate::testing::runtime_lease_owner(),
            )
            .await
            .expect("runtime"),
            (Some(store), Some(registry)) => {
                let host = crate::ProcessRuntimeHost::with_ports(
                    self.host,
                    crate::testing::process_work_wiring_for_registry(registry),
                    Arc::new(crate::NoSessionWork::new()),
                );
                LashRuntime::from_persistent_background_state(
                    policy,
                    host,
                    crate::PersistentRuntimeServices::new(
                        plugin_session,
                        store,
                        attachment_store,
                        process_env_store,
                    ),
                    initial_state.clone(),
                    crate::testing::runtime_lease_owner(),
                )
                .await
                .expect("runtime")
            }
            (None, Some(registry)) => {
                let host = crate::ProcessRuntimeHost::with_ports(
                    self.host,
                    crate::testing::process_work_wiring_for_registry(registry),
                    Arc::new(crate::NoSessionWork::new()),
                );
                LashRuntime::from_background_state(
                    policy,
                    host,
                    crate::RuntimeServices::new(
                        plugin_session,
                        attachment_store,
                        process_env_store,
                    ),
                    initial_state,
                    crate::testing::runtime_lease_owner(),
                )
                .await
                .expect("runtime")
            }
        };
        let mut runtime = runtime;
        set_runtime_provider(&mut runtime, self.transport.into_handle());
        runtime
    }
}

pub async fn standard_runtime_with_transport(
    backend: &crate::Backend,
    transport: TestProvider,
) -> LashRuntime {
    TestRuntime::new(backend, transport).build().await
}
pub type RuntimeTestPluginBuilder = dyn Fn(&crate::PluginSessionContext) -> Result<Arc<dyn crate::SessionPlugin>, crate::PluginError>
    + Send
    + Sync;
pub type RuntimeExternalRegistrar =
    dyn Fn(&mut crate::PluginRegistrar) -> Result<(), crate::PluginError> + Send + Sync;

pub struct RuntimeTestPluginFactory {
    pub build: Arc<RuntimeTestPluginBuilder>,
}

impl crate::PluginFactory for RuntimeTestPluginFactory {
    fn id(&self) -> &'static str {
        "runtime-test"
    }

    fn declaration(&self) -> crate::plugin::PluginDeclaration {
        crate::plugin::PluginDeclaration::initial(self.id())
    }

    fn build(
        &self,
        ctx: &crate::PluginSessionContext,
    ) -> Result<Arc<dyn crate::SessionPlugin>, crate::PluginError> {
        (self.build)(ctx)
    }
}

pub struct RuntimeTestPlugin {
    pub before_turn: Option<crate::plugin::BeforeTurnHook>,
    pub checkpoint: Option<crate::plugin::CheckpointHook>,
    pub presentation_steps: Vec<crate::plugin::ToolPresentationStep>,
    pub runtime_event: Option<crate::plugin::PluginLifecycleEventHook>,
    pub external_registrar: Option<Arc<RuntimeExternalRegistrar>>,
}

impl crate::SessionPlugin for RuntimeTestPlugin {
    fn id(&self) -> &'static str {
        "runtime-test"
    }

    fn register(&self, reg: &mut crate::PluginRegistrar) -> Result<(), crate::PluginError> {
        if let Some(hook) = &self.before_turn {
            reg.turn().before(Arc::clone(hook));
        }
        if let Some(hook) = &self.checkpoint {
            reg.turn().checkpoint(Arc::clone(hook));
        }
        for step in &self.presentation_steps {
            reg.tool_results().presentation_step(Arc::clone(step));
        }
        if let Some(hook) = &self.runtime_event {
            reg.session().on_event(Arc::clone(hook));
        }
        if let Some(register) = &self.external_registrar {
            register(reg)?;
        }
        Ok(())
    }
}

pub async fn runtime_with_plugins(
    backend: &crate::Backend,
    plugins: Vec<Arc<dyn crate::PluginFactory>>,
    transport: TestProvider,
) -> LashRuntime {
    TestRuntime::new(backend, transport)
        .plugins(plugins)
        .build()
        .await
}

pub async fn runtime_with_plugins_and_tools(
    backend: &crate::Backend,
    plugins: Vec<Arc<dyn crate::PluginFactory>>,
    tools: Arc<dyn crate::ToolProvider>,
    transport: TestProvider,
) -> LashRuntime {
    TestRuntime::new(backend, transport)
        .plugins(plugins)
        .tools(tools)
        .build()
        .await
}

pub async fn runtime_with_plugins_and_tools_and_host(
    plugins: Vec<Arc<dyn crate::PluginFactory>>,
    tools: Arc<dyn crate::ToolProvider>,
    transport: TestProvider,
    host: EmbeddedRuntimeHost,
) -> LashRuntime {
    let backend = host.core.backend().clone();
    TestRuntime::new(&backend, transport)
        .plugins(plugins)
        .tools(tools)
        .host(host)
        .build()
        .await
}

pub async fn runtime_with_plugins_and_tools_and_host_and_store(
    plugins: Vec<Arc<dyn crate::PluginFactory>>,
    tools: Arc<dyn crate::ToolProvider>,
    transport: TestProvider,
    host: EmbeddedRuntimeHost,
    store: Arc<dyn crate::RuntimeStore>,
) -> LashRuntime {
    let backend = host.core.backend().clone();
    TestRuntime::new(&backend, transport)
        .plugins(plugins)
        .tools(tools)
        .host(host)
        .store(store)
        .build()
        .await
}

pub struct EchoTool;

fn echo_tool_definition() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        "tool:echo_tool",
        "echo_tool",
        "Return a tool payload",
        serde_json::json!({
            "type": "object",
            "properties": { "value": { "type": "string" } },
            "required": ["value"],
            "additionalProperties": false
        }),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
}

#[async_trait::async_trait]
impl crate::ToolProvider for EchoTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![echo_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "echo_tool").then(|| Arc::new(echo_tool_definition().contract()))
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        assert_eq!(call.name(), "echo_tool");
        let value = call
            .args
            .get("value")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        crate::ToolOutcome::ok(serde_json::json!({
            "payload": format!("raw:{value}")
        }))
        .into()
    }
}

pub struct TerminalControlTool {
    pub controls: Vec<crate::ToolControl>,
}

#[async_trait::async_trait]
impl crate::ToolProvider for TerminalControlTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        (0..self.controls.len())
            .map(|index| terminal_tool_definition(index).manifest())
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        name.strip_prefix("terminal_tool_")
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|index| *index < self.controls.len())
            .map(|index| Arc::new(terminal_tool_definition(index).contract()))
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.result_for(call.name()).into()
    }
}

impl TerminalControlTool {
    fn result_for(&self, name: &str) -> crate::ToolOutcome {
        let index = name
            .strip_prefix("terminal_tool_")
            .and_then(|value| value.parse::<usize>().ok())
            .expect("known terminal test tool");
        crate::ToolOutcome::ok(serde_json::json!({ "tool": name }))
            .with_control(self.controls[index].clone())
    }
}

fn terminal_tool_definition(index: usize) -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        format!("tool:terminal_tool_{index}"),
        format!("terminal_tool_{index}"),
        "Return a terminal control result",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
}

/// Tool that sleeps for 10 seconds unless its future is aborted or the
/// execution-context cancellation token fires. Used to verify that turn
/// cancellation unwinds in-flight tool tasks promptly.
pub struct SlowTool {
    pub observed_cancel: Arc<AtomicBool>,
    /// Notified (via `notify_one`, so an early notify is stored) as the tool
    /// begins executing, before it waits; lets a test synchronise on the tool
    /// being in flight rather than on wall-clock delay.
    pub started: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl crate::ToolProvider for SlowTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![slow_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "slow_tool").then(|| Arc::new(slow_tool_definition().contract()))
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        let observed = Arc::clone(&self.observed_cancel);
        self.started.notify_one();
        if let Some(token) = call.context.cancellation_token() {
            let token = token.clone();
            tokio::select! {
                _ = token.cancelled() => {
                    observed.store(true, Ordering::SeqCst);
                    crate::ToolOutcome::cancelled("cancelled").into()
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {
                    crate::ToolOutcome::ok(serde_json::json!({"status": "completed"})).into()
                }
            }
        } else {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            crate::ToolOutcome::ok(serde_json::json!({"status": "completed"})).into()
        }
    }
}

fn slow_tool_definition() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        "tool:slow_tool",
        "slow_tool",
        "Sleep for a long time; respects cancellation.",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
}

pub struct MemoryProbeTool;

#[async_trait::async_trait]
impl crate::ToolProvider for MemoryProbeTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![memory_probe_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "memory_probe").then(|| Arc::new(memory_probe_tool_definition().contract()))
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        crate::ToolOutcome::ok(json!("ok")).into()
    }
}

fn memory_probe_tool_definition() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        "tool:memory_probe",
        "memory_probe",
        "probe",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "string" }),
    )
}

pub async fn standard_runtime_with_transport_and_host(
    transport: TestProvider,
    host: EmbeddedRuntimeHost,
) -> LashRuntime {
    let backend = host.core.backend().clone();
    TestRuntime::new(&backend, transport)
        .host(host)
        .build()
        .await
}

/// Reopen a session that session initialisation committed, through the
/// ordinary resumed-open path — the same open any embedder performs on
/// durable state. The returned runtime is an ordinary session owned by the
/// caller; nothing registers it anywhere (FIG-3378).
pub fn reopen_session_runtime<'a>(
    parent: &'a LashRuntime,
    session_id: &'a SessionId,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = LashRuntime> + Send + 'a>> {
    Box::pin(async move {
        let deployment = parent.host.core.session_store_factory();
        let store = lash_core_execution::live_session_view(&deployment, session_id)
            .await
            .expect("open child session store")
            .expect("child session store exists");
        let state =
            crate::store::load_session_window_state(&store, crate::store::WindowSelector::Current)
                .await
                .expect("load child session state")
                .expect("persisted child session state")
                .state;
        let policy = state.effective_policy().clone();
        let plugin_host = parent
            .session
            .as_ref()
            .expect("parent session")
            .plugins()
            .host()
            .clone();
        let mut env = crate::RuntimeEnvironment::builder(parent.host.core.clone())
            .with_plugin_host(Arc::new(plugin_host))
            .build();
        env.work = parent.host.work.clone();
        LashRuntime::from_environment(
            &env,
            policy,
            state,
            Some(store),
            parent.runtime_lease_owner.clone(),
        )
        .await
        .expect("reopen child session runtime")
    })
}

/// Record on a fresh `state` the plugin configuration a creator records
/// (FIG-4379): every owner installed on `host` resolves its namespace from
/// what `state` states, and `protocol_plugin_id` names the protocol owner. A
/// test that builds a session's state by hand calls this where a creator
/// would, before the session's first open.
pub fn record_creation_plugin_config(
    host: &crate::plugin::PluginHost,
    protocol_plugin_id: &str,
    state: &mut RuntimeSessionState,
) {
    let requested = crate::PluginOptions {
        plugins: state
            .authority
            .plugin_config
            .iter()
            .map(|(plugin_id, value)| (plugin_id.clone(), value.clone()))
            .collect(),
    };
    state.authority.plugin_config = host
        .resolve_creation_plugin_config(Some(protocol_plugin_id), &requested, None, true)
        .unwrap_or_else(|refusal| {
            panic!("the installed owners refuse the stated config: {refusal}")
        });
}
