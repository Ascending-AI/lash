use std::collections::BTreeSet;
use std::sync::Arc;

use lash_core::plugin::{CodeExecutorPlugin, ProtocolSessionContext};
use lash_core::{SessionError, SessionHistoryRecord};
use lash_rlm_types::{RlmGlobalsPatchPluginBody, RlmProtocolEvent};

use crate::dialect::{DialectSession, SessionDialect};
use crate::projection::decode_rlm_protocol_event;

pub(crate) struct RlmRuntimeState {
    dialect: Arc<SessionDialect>,
    frame_claim: tokio::sync::Mutex<Option<lash_core::ReferrerClaim>>,
    execution: tokio::sync::Mutex<DialectSession>,
}

impl RlmRuntimeState {
    pub(crate) fn new(dialect: Arc<SessionDialect>) -> Result<Self, SessionError> {
        Ok(Self {
            frame_claim: tokio::sync::Mutex::new(None),
            execution: tokio::sync::Mutex::new(dialect.create_session()),
            dialect,
        })
    }

    #[cfg(test)]
    pub(crate) fn new_for_tests() -> Result<Self, SessionError> {
        let services = crate::dialect::test_dialect_services();
        Self::new(Arc::new(crate::dialect::SessionDialect::new(
            crate::dialect::CellDialect::typescript(),
            services,
        )))
    }

    /// The facts the session's prompt sections render (ADR 0133): the
    /// current bound-variables view and the packages' code exports.
    ///
    /// The runtime asks only where it composes a call's prompt. An admitted
    /// call records its prompt, so a resend never reads this state again.
    pub(crate) async fn prompt_facts(
        &self,
        recorded: Option<&lash_core::RecordedRender>,
        pages: Option<&lash_core::plugin::SessionPages>,
        packages: &lash_core_store::tool_surface::RecordedPackages,
    ) -> Result<crate::prompt_sections::RlmPromptFacts, SessionError> {
        let renderer = self.dialect.renderer();
        let recorded = lash_core::RecordedRender::require_available(recorded, renderer.0.id())
            .map_err(|code| SessionError::Protocol(code.to_string()))?;
        let params: crate::render::ResolvedRlmRender =
            serde::Deserialize::deserialize(recorded.params.as_ref())
                .map_err(|error| SessionError::Protocol(error.to_string()))?;
        let package_surface = self.dialect.package_surface(packages).await?;
        let mut execution = self.execution.lock().await;
        let (bound_variables, globals, inferred_names) = execution
            .prepare_bound_variables_prompt(params.preview, pages)
            .await?
            .render()
            .map_err(|error| {
                crate::tool_catalog::section_refused(
                    self.dialect.tool_presenter().presenter().id(),
                    error,
                )
            })?;
        drop(execution);
        Ok(crate::prompt_sections::RlmPromptFacts {
            reserved_names: inferred_names,
            bound_variables,
            packages: package_surface,
            globals: Arc::new(globals),
        })
    }

    /// Replace live execution from the committed restore view and replay its
    /// ordinary seed/default events. No uncommitted live values survive.
    pub(crate) async fn restore_runtime_session_state(
        &self,
        state: lash_core::plugin::ProtocolSessionRestoreView,
    ) -> Result<(), SessionError> {
        let claim = state
            .pages
            .as_ref()
            .zip(state.current_frame_node_id.as_ref())
            .map(|(pages, frame)| {
                let session = lash_vocabulary::identity::SessionId::parse(pages.actor.id())
                    .map_err(|error| SessionError::Protocol(error.to_string()))?;
                lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::FrameEnvironment(
                    lash_core::FrameEnvironmentId::new(session, frame.clone()),
                ))
                .map_err(|error| SessionError::Protocol(error.to_string()))
            })
            .transpose()?;
        *self.frame_claim.lock().await = claim.clone();
        let mut execution_guard = self.execution.lock().await;
        let execution = &mut *execution_guard;
        let snapshot = state.execution_state.map_err(|error| SessionError::Store {
            context: "failed to hydrate code mode execution-state components".to_string(),
            source: error,
        })?;
        execution.release_replaced_frame().await;
        *execution = self.dialect.create_session();
        execution.note_pages(state.pages.as_ref());
        if let Some(snapshot) = snapshot {
            execution.restore_execution_graph(
                &snapshot,
                lash_core_store::execution_state::ExecutionGraphPurpose::Final,
            )?;
            execution.succeed_saved_functions(claim.as_ref()).await?;
        }
        for event in &state.active_events {
            if let SessionHistoryRecord::Protocol(event) = event
                && let Some(event) = decode_rlm_protocol_event(event).map_err(history_corruption)?
            {
                self.apply_seed_or_globals_event(execution, &event).await?;
            }
        }
        Ok(())
    }

    pub(crate) async fn append_session_nodes(
        &self,
        nodes: &[lash_core::SessionAppendNode],
    ) -> Result<(), SessionError> {
        let mut execution_guard = self.execution.lock().await;
        let execution = &mut *execution_guard;
        for node in nodes {
            if let lash_core::SessionAppendNode::ProtocolEvent { event, .. } = node
                && let Some(event) = decode_rlm_protocol_event(event).map_err(history_corruption)?
            {
                self.apply_seed_or_globals_event(execution, &event).await?;
            }
        }
        Ok(())
    }

    pub(crate) async fn execute_code(
        &self,
        ctx: lash_core::RuntimeExecutionContext<'_>,
        request: lash_core::ExecRequest,
    ) -> Result<lash_core::ExecResponse, SessionError> {
        // The guard is held across the whole cell: a second caller waits for
        // the cell to finish instead of being told the state is busy, and a
        // cell cancelled mid-flight leaves the state where it was.
        let mut guard = self.execution.lock().await;
        // Restore replaces the dialect session and model admission may replay
        // without rendering its prompt. The recorded round is the authority
        // for imports on every execution, including a cold restored cell.
        guard.shows_packages(ctx.recorded_packages().clone());
        Box::pin(guard.execute(ctx, request)).await
    }

    pub(crate) fn execution_state_dirty(&self) -> bool {
        // A contended `try_lock` means a cell is running, and a running cell
        // is dirty by construction.
        self.execution
            .try_lock()
            .map(|execution| execution.execution_state_dirty())
            .unwrap_or(true)
    }

    pub(crate) async fn capture_execution_graph(
        &self,
        purpose: lash_core_store::execution_state::ExecutionGraphPurpose,
    ) -> Result<lash_core_store::execution_state::ExecutionGraphCapture, SessionError> {
        let capture = self
            .execution
            .lock()
            .await
            .capture_execution_graph(purpose)
            .await?;
        Ok(capture)
    }

    pub(crate) async fn execution_candidate(
        &self,
        root: lash_core_store::checkpoint_graph::NodeId,
        commit: lash_core_store::checkpoint_graph::CommitId,
        tx: &mut lash_durable::ActorTx,
    ) -> Result<Option<lash_core::plugin::ExecutionCandidate>, SessionError> {
        self.execution
            .lock()
            .await
            .execution_candidate(root, commit, tx)
            .await
    }

    pub(crate) async fn hydrated_execution_graph(
        &self,
    ) -> Result<Option<lash_core::plugin::HydratedExecutionGraph>, SessionError> {
        self.execution
            .lock()
            .await
            .hydrated_execution_graph()
            .await
            .map(Some)
    }

    pub(crate) async fn acknowledge_execution_state_capture(&self) {
        let _ = self
            .execution
            .lock()
            .await
            .acknowledge_execution_state_capture()
            .await;
    }

    pub(crate) async fn abort_execution_state_capture(&self) {
        let _ = self.execution.lock().await.abort_execution_state_capture();
    }

    pub(crate) async fn settle_code_execution(
        &self,
        outcome: lash_core::plugin::CodeExecutionOutcome,
    ) -> Result<(), SessionError> {
        self.execution.lock().await.settle_code_execution(outcome)
    }

    /// Installs a session's execution state, its descriptor page (ADR 0145
    /// §9): the working state a turn phase owns, or a session revision's,
    /// as `purpose` says, before any model continuation or cell compiles.
    pub(crate) async fn restore_execution_graph(
        &self,
        graph: &lash_core_store::execution_state::HydratedExecutionGraph,
        purpose: lash_core_store::execution_state::ExecutionGraphPurpose,
    ) -> Result<(), SessionError> {
        let claim = self.frame_claim.lock().await.clone();
        let mut execution = self.execution.lock().await;
        execution.restore_execution_graph(graph, purpose)?;
        execution.succeed_saved_functions(claim.as_ref()).await?;
        Ok(())
    }

    async fn apply_seed_or_globals_event(
        &self,
        execution: &mut DialectSession,
        event: &RlmProtocolEvent,
    ) -> Result<(), SessionError> {
        match event {
            RlmProtocolEvent::RlmGlobalsPatch(patch) => execution.patch_globals(patch).await?,
            RlmProtocolEvent::RlmSeed(seed) => {
                execution
                    .patch_globals(&RlmGlobalsPatchPluginBody {
                        set_default: seed.globals.clone(),
                    })
                    .await?
            }
            RlmProtocolEvent::RlmTrajectoryEntry(_) | RlmProtocolEvent::RlmDiagnostic(_) => {}
        }
        Ok(())
    }
}

pub(crate) struct CodeModeCodeExecutor {
    state: Arc<RlmRuntimeState>,
}

impl CodeModeCodeExecutor {
    pub(crate) fn new(state: Arc<RlmRuntimeState>) -> Self {
        Self { state }
    }
}

#[async_trait::async_trait]
impl CodeExecutorPlugin for CodeModeCodeExecutor {
    async fn execute_code(
        &self,
        ctx: lash_core::RuntimeExecutionContext<'_>,
        request: lash_core::ExecRequest,
    ) -> Result<lash_core::ExecResponse, SessionError> {
        Box::pin(self.state.execute_code(ctx, request)).await
    }

    fn execution_state_dirty(&self) -> bool {
        self.state.execution_state_dirty()
    }

    fn carries_held_response(&self) -> bool {
        true
    }

    fn snapshot_tool_calls(
        &self,
        cell: &lash_core::plugin::StoredCell,
    ) -> Result<
        (
            Vec<lash_core::ToolCallRecord>,
            lash_vm_broker::round_graph::ReportView,
        ),
        SessionError,
    > {
        crate::executor::snapshot_tool_calls(cell).map_err(SessionError::Protocol)
    }

    async fn check_cell_snapshot(
        &self,
        cell: &lash_core::plugin::StoredCell,
    ) -> Result<Result<(), String>, lash_core::RuntimeError> {
        Ok(crate::executor::check_cell_snapshot(cell))
    }

    async fn carried_cell_snapshot(
        &self,
        cell: &lash_core::plugin::StoredCell,
    ) -> Result<Result<(), String>, lash_core::RuntimeError> {
        self.state
            .dialect
            .kernel()
            .cell_snapshot(cell)
            .map(|()| Ok(()))
            .map_err(|refusal| {
                crate::executor::cell_carry::Error::Migration(refusal)
                    .into_runtime()
                    .into_runtime_error()
            })
    }

    fn history_attachments(
        &self,
        record: &lash_core::ProtocolEvent,
    ) -> Result<Vec<lash_core::AttachmentId>, SessionError> {
        let decoded = decode_rlm_protocol_event(record).map_err(history_corruption)?;
        let Some(RlmProtocolEvent::RlmTrajectoryEntry(cell)) = decoded.as_deref() else {
            return Ok(Vec::new());
        };
        Ok(cell
            .prints_retained
            .iter()
            .map(|retained| &retained.output.reference)
            .chain(&cell.images)
            .chain(
                cell.result
                    .finish()
                    .and_then(lash_core::OutputValue::retained)
                    .map(|retained| &retained.output.reference),
            )
            .map(|reference| reference.id.clone())
            .collect())
    }

    async fn frame_switch_carries(
        &self,
        _ctx: ProtocolSessionContext<'_>,
        _successor: &lash_core::FrameNodeId,
        initial_nodes: &[lash_core::SessionAppendNode],
    ) -> Result<Vec<lash_core::ArtifactName>, SessionError> {
        frame_switch_carries(initial_nodes)
    }

    fn executable_generation(&self) -> Option<lash_core::ExecutableGeneration> {
        Some(crate::executor::cell_generation())
    }

    async fn execution_candidate(
        &self,
        root: lash_core_store::checkpoint_graph::NodeId,
        commit: lash_core_store::checkpoint_graph::CommitId,
        tx: &mut lash_durable::ActorTx,
    ) -> Result<Option<lash_core::plugin::ExecutionCandidate>, SessionError> {
        self.state.execution_candidate(root, commit, tx).await
    }

    async fn hydrated_execution_graph(
        &self,
        _ctx: ProtocolSessionContext<'_>,
    ) -> Result<Option<lash_core::plugin::HydratedExecutionGraph>, SessionError> {
        self.state.hydrated_execution_graph().await
    }

    async fn capture_execution_graph(
        &self,
        _ctx: ProtocolSessionContext<'_>,
        purpose: lash_core_store::execution_state::ExecutionGraphPurpose,
    ) -> Result<lash_core_store::execution_state::ExecutionGraphCapture, SessionError> {
        self.state.capture_execution_graph(purpose).await
    }

    async fn restore_execution_graph(
        &self,
        _ctx: ProtocolSessionContext<'_>,
        graph: &lash_core_store::execution_state::HydratedExecutionGraph,
        purpose: lash_core_store::execution_state::ExecutionGraphPurpose,
    ) -> Result<(), SessionError> {
        self.state.restore_execution_graph(graph, purpose).await
    }

    async fn acknowledge_execution_state_capture(&self) {
        self.state.acknowledge_execution_state_capture().await;
    }

    async fn abort_execution_state_capture(&self) {
        self.state.abort_execution_state_capture().await;
    }

    async fn settle_code_execution(
        &self,
        outcome: lash_core::plugin::CodeExecutionOutcome,
    ) -> Result<(), SessionError> {
        self.state.settle_code_execution(outcome).await
    }
}

/// The definitions a frame switch carries (ADR 0113 §3.1): every process
/// definition a value in the switch's seed and globals events names, by its
/// tagged id. A cell's definitions are held by its frame; only these survive
/// into the successor frame, with the document each one runs.
fn frame_switch_carries(
    nodes: &[lash_core::SessionAppendNode],
) -> Result<Vec<lash_core::ArtifactName>, SessionError> {
    let mut definitions = BTreeSet::new();
    for node in nodes {
        let lash_core::SessionAppendNode::ProtocolEvent { event, .. } = node else {
            continue;
        };
        let values = match decode_rlm_protocol_event(event)
            .map_err(history_corruption)?
            .as_deref()
        {
            Some(RlmProtocolEvent::RlmSeed(seed)) => serde_json::to_value(seed),
            Some(RlmProtocolEvent::RlmGlobalsPatch(patch)) => serde_json::to_value(patch),
            _ => continue,
        };
        // Both bodies are JSON maps, so encoding them cannot fail; a body
        // that did would carry nothing.
        if let Ok(values) = values {
            collect_definition_ids(&values, &mut definitions);
        }
    }
    Ok(definitions
        .into_iter()
        .map(|id| lash_core::ArtifactName {
            store: lash_core::ArtifactStoreId::ProcessDefinition,
            artifact_ref: id.to_string(),
        })
        .collect())
}

/// Every tagged process definition id in `value`: a definition as a cell
/// hands it out holds its id, and a cell can hand out the id alone.
fn collect_definition_ids(
    value: &serde_json::Value,
    ids: &mut BTreeSet<lash_core::ProcessDefinitionId>,
) {
    match value {
        serde_json::Value::Object(fields) => {
            match lash_core::ProcessDefinitionId::from_tagged_json(value) {
                Ok(id) => {
                    ids.insert(id);
                }
                Err(_) => {
                    for field in fields.values() {
                        collect_definition_ids(field, ids);
                    }
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_definition_ids(item, ids);
            }
        }
        _ => {}
    }
}

fn history_corruption(error: lash_core::StoredDataCorruption) -> SessionError {
    SessionError::Plugin(
        lash_execution_contracts::plugin_error::PluginError::StoredDataCorrupt {
            record_kind: error.record_kind,
            message: error.message,
        },
    )
}

#[cfg(test)]
#[allow(
    clippy::large_futures,
    reason = "the tests poll the one 16KB execute-code future from plain sync harnesses; boxing each call site would be noise without changing what is measured"
)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::{Context, Poll, Waker};

    /// Runs `request` on `state` as one cell, under a durable host of its
    /// own.
    async fn execute_cell(
        state: &RlmRuntimeState,
        host: &crate::testing::DurableHost,
        request: lash_core::ExecRequest,
    ) -> Result<lash_core::ExecResponse, SessionError> {
        // Each cell is an execution of its own in the turn.
        static CELLS: AtomicUsize = AtomicUsize::new(0);
        let key: &'static str = Box::leak(
            format!("exec-code:{}", CELLS.fetch_add(1, Ordering::SeqCst)).into_boxed_str(),
        );
        Box::pin(
            state.execute_code(
                crate::testing::recorded_cell_context(
                    host,
                    "test-session",
                    "test-turn",
                    key,
                    Arc::new(crate::testing::NoTools),
                )
                .await,
                request,
            ),
        )
        .await
    }

    /// The scope [`admitted_context`] claims.
    fn admitted_scope() -> lash_core::AdmittedScope {
        lash_core::AdmittedScope::turn(
            lash_vocabulary::identity::SessionId::from("runtime-state-session"),
            lash_vocabulary::identity::TurnId::from("runtime-state-turn"),
        )
    }

    /// A cell that references an unresolved module call-path, so the run's
    /// deferred provider is consulted before anything is linked or run.
    const PARKING_CELL: &str = "await web.fetch({});";

    /// A deferred provider that parks a cell inside `resolve` until it is
    /// released.
    ///
    /// This is the only suspension point a unit test can plant in the middle of
    /// a cell without a live host, and it is what makes the two properties
    /// under test observable at all: what a *second* caller sees while a cell
    /// is running, and what the session looks like after a cell is cancelled
    /// while running.
    #[derive(Default)]
    struct ParkingResolver {
        entered: AtomicUsize,
        released: AtomicBool,
    }

    impl ParkingResolver {
        fn entered(&self) -> usize {
            self.entered.load(Ordering::SeqCst)
        }

        fn release(&self) {
            self.released.store(true, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl lash_core::plugin::packages::DeferredPackageProvider for ParkingResolver {
        fn validate_grant(
            &self,
            grant: &lash_vocabulary::DeferredGrant,
            registry: lash_vocabulary::claims::RegistryKind,
        ) -> Result<(), lash_execution_contracts::process::linking::AdmissionRefusal> {
            if registry == lash_vocabulary::claims::RegistryKind::Session
                && grant.provider.owner == "parking"
                && grant.provider.key == "web"
                && grant.path
                    == lash_vocabulary::PackagePath::new(
                        lash_vocabulary::Segment::new("web").unwrap(),
                    )
            {
                Ok(())
            } else {
                Err(lash_execution_contracts::process::linking::AdmissionRefusal::ProviderNotHostable {
                    provider: grant.provider.clone(), registry,
                })
            }
        }
        async fn resolve(
            &self,
            _cx: &lash_core::DeferredResolveContext<'_>,
            _grant: &lash_vocabulary::DeferredGrant,
            _requested: &lash_vocabulary::QualifiedName,
        ) -> Result<
            lash_core::plugin::packages::DeferredAnswer,
            lash_core::plugin::packages::ResolverFault,
        > {
            self.entered.fetch_add(1, Ordering::SeqCst);
            std::future::poll_fn(|cx| {
                if self.released.load(Ordering::SeqCst) {
                    Poll::Ready(())
                } else {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            })
            .await;
            Ok(lash_core::plugin::packages::DeferredAnswer::Unavailable { note: None })
        }
    }
    #[async_trait::async_trait]
    impl lash_core::plugin::packages::PackageProvider for ParkingResolver {
        fn roots(&self, _: &lash_core::OwnerConfigView) -> BTreeSet<lash_vocabulary::Segment> {
            [lash_vocabulary::Segment::new("web").unwrap()].into()
        }
        fn advertise(
            &self,
            _config: &lash_core::OwnerConfigView,
            context: &lash_vocabulary::PublicationContext,
        ) -> Result<
            Vec<lash_core::plugin::packages::PackageContribution>,
            lash_execution_contracts::PluginError,
        > {
            use lash_vocabulary::*;
            let advertisement = PackageAdvertisement::Deferred(Box::new(DeferredPackage {
                path: PackagePath::new(Segment::new("web").unwrap()),
                visibility: Visibility::Public,
                provider: ProviderName {
                    owner: "parking".into(),
                    key: "web".into(),
                },
                config: ProviderConfig {
                    component: None,
                    format: providers::FormatVersion::ONE,
                    value: serde_json::Value::Null,
                    payload: None,
                },
                resolution_policy: Default::default(),
            }));
            let publication =
                lash_core::plugin::packages::operation_publication(context, &advertisement)?;
            Ok(vec![lash_core::plugin::packages::PackageContribution {
                publication,
                advertisement,
                routes: vec![],
            }])
        }
        async fn hold(
            &self,
            _claim: &lash_core::ReferrerClaim,
            _advertisements: &[&lash_vocabulary::PackageAdvertisement],
        ) -> Result<(), lash_execution_contracts::process::linking::EnginePortError> {
            Ok(())
        }
    }

    fn parked_session() -> (Arc<ParkingResolver>, RlmRuntimeState) {
        let resolver = Arc::new(ParkingResolver::default());
        let state = RlmRuntimeState::new_for_tests().expect("runtime state");
        (resolver, state)
    }

    fn cell(code: &str) -> lash_core::ExecRequest {
        lash_core::ExecRequest {
            code: code.to_string(),
        }
    }

    /// A cell context under `cell_id`'s invocation, under the claimed
    /// context `handler` lends for [`admitted_scope`].
    async fn admitted_context(
        handler: &crate::testing::DurableHost,
        cell_id: &str,
        resolver: &Arc<ParkingResolver>,
    ) -> lash_core::RuntimeExecutionContext<'static> {
        let replay_key = format!("exec-code:{cell_id}");
        let (provider, catalog) = crate::testing::with_finish(Arc::new(crate::testing::NoTools));
        let mut factories = lash_core::testing::test_code_protocol_factories();
        factories.push(Arc::new(lash_core::plugin::StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial("parking"),
            lash_core::plugin::PluginSpec::new()
                .with_package_provider(lash_core::tool_provider_key!("web"), resolver.clone())
                .with_deferred_package_provider(
                    lash_core::tool_provider_key!("web"),
                    resolver.clone(),
                ),
        )));
        crate::testing::recorded_context(
            lash_core::testing::TestExecutionContextBuilder::new(handler.ports())
                .provider_registration(provider)
                .tool_catalog(catalog)
                .plugin_factories(factories)
                .runtime_parent_invocation(lash_core::testing::exec_code_invocation(
                    "runtime-state-session",
                    "runtime-state-turn",
                    0,
                    0,
                    replay_key.clone(),
                    replay_key,
                )),
        )
        .await
    }

    /// Shift `future` until it is parked inside the resolver, i.e. suspended in
    /// the middle of a cell with the execution state in hand.
    ///
    /// Each poll runs on the task's own waker and the loop yields between
    /// polls, so reaching the resolver takes real turns of the runtime rather
    /// than a fixed number of polls.
    async fn poll_until_parked<F: Future>(future: &mut Pin<Box<F>>, resolver: &ParkingResolver) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while resolver.entered() == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the cell never reached the deferred resolver"
            );
            let polled = std::future::poll_fn(|cx| {
                std::task::Poll::Ready(future.as_mut().poll(cx).is_pending())
            })
            .await;
            assert!(polled, "a cell parked in the resolver cannot complete");
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    }

    /// A restore view for `frame` carrying `snapshot` and `nodes` as its
    /// active history.
    fn restore_view(
        frame: &str,
        snapshot: Option<lash_core::plugin::HydratedExecutionGraph>,
        nodes: Vec<lash_core::SessionAppendNode>,
    ) -> lash_core::plugin::ProtocolSessionRestoreView {
        lash_core::plugin::ProtocolSessionRestoreView {
            current_frame_node_id: Some(
                lash_core::FrameNodeId::new(frame).expect("test frame identity is non-empty"),
            ),
            execution_state: Ok(snapshot),
            active_events: nodes
                .into_iter()
                .map(|node| match node {
                    lash_core::SessionAppendNode::ProtocolEvent { event, .. } => {
                        SessionHistoryRecord::Protocol(event)
                    }
                    other => panic!("seed nodes are protocol events: {other:?}"),
                })
                .collect(),
            pages: None,
        }
    }

    fn baton_seed_nodes(label: &str) -> Vec<lash_core::SessionAppendNode> {
        let seed =
            crate::projection::RlmSeed::from_seed_value(&serde_json::json!({ "baton": label }))
                .expect("seed value");
        crate::projection::rlm_seed_initial_nodes(seed, lash_core::FleetFormat::current())
    }

    /// The value `finish baton` yields on the state's live execution.
    async fn live_baton(
        state: &RlmRuntimeState,
        host: &crate::testing::DurableHost,
    ) -> serde_json::Value {
        let response = execute_cell(state, host, cell("await control.finish(baton);"))
            .await
            .expect("the baton cell runs");
        state
            .settle_code_execution(lash_core::plugin::CodeExecutionOutcome::Accepted)
            .await
            .expect("settle the baton cell");
        crate::testing::commit_phase(state, host).await;
        assert_eq!(response.error(), None);
        response
            .finish_value()
            .cloned()
            .expect("the baton cell finishes")
    }

    /// The regression test for the defect FIG-1729 fixes.
    ///
    /// The old code moved the execution state out of its holder for the
    /// duration of the cell, so a future dropped mid-cell dropped the state
    /// with it and left `None` behind for good: every later call on that
    /// session failed with the busy protocol error, permanently. The state is
    /// now only borrowed, so a cancelled cell leaves it exactly where it was
    /// and the next cell runs.
    #[test]
    fn a_cancelled_cell_leaves_the_session_usable() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let (resolver, state) = parked_session();

                // Execute a cell until it is suspended mid-flight, then drop it:
                // a cancellation with the execution state in the cell's hands.
                let cancelled_handler = crate::testing::DurableHost::open(admitted_scope()).await;
                {
                    let mut cancelled = Box::pin(state.execute_code(
                        admitted_context(&cancelled_handler, "cancelled", &resolver).await,
                        cell(PARKING_CELL),
                    ));
                    poll_until_parked(&mut cancelled, &resolver).await;
                }

                // The state was borrowed, never moved out, so the session is
                // still whole and the next cell runs normally.
                let handler = crate::testing::DurableHost::open(admitted_scope()).await;
                let next = state
                    .execute_code(
                        admitted_context(&handler, "survivor", &resolver).await,
                        cell("let survivor = 1;\nawait control.finish(survivor);"),
                    )
                    .await
                    .expect("the session survives a cell cancelled mid-flight");
                assert_eq!(next.error(), None);
                assert_eq!(next.finish_value().cloned(), Some(serde_json::json!(1)));
            });
    }

    #[test]
    fn a_second_concurrent_cell_waits_and_then_runs() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let (resolver, state) = parked_session();
                let mut cx = Context::from_waker(Waker::noop());

                // One cell is running: parked mid-flight, holding the state.
                let running_handler = crate::testing::DurableHost::open(admitted_scope()).await;
                let mut running = Box::pin(state.execute_code(
                    admitted_context(&running_handler, "running", &resolver).await,
                    cell(PARKING_CELL),
                ));
                poll_until_parked(&mut running, &resolver).await;

                // A second cell arrives while the first is still running. It
                // makes no progress whatsoever: it is queued behind the running
                // cell rather than answered — with a result or with an error.
                let waiting_handler = &running_handler;
                {
                    let mut waiting = Box::pin(state.execute_code(
                        admitted_context(waiting_handler, "waiting", &resolver).await,
                        cell("let second_cell = 2;"),
                    ));
                    for _ in 0..16 {
                        assert!(
                            waiting.as_mut().poll(&mut cx).is_pending(),
                            "a cell arriving mid-cell must wait for the running cell"
                        );
                    }
                    assert_eq!(
                        resolver.entered(),
                        1,
                        "the waiting cell never began executing beside the running one"
                    );
                }

                // The running cell finishes and hands the state back, live.
                resolver.release();
                let first = running.await.expect("the parked cell completes");
                assert!(
                    first.error().is_some(),
                    "`web.fetch` resolves to nothing, so the parked cell ends in a link error"
                );
                state
                    .settle_code_execution(lash_core::plugin::CodeExecutionOutcome::Accepted)
                    .await
                    .expect("settle the first returned cell");

                // The waiting cell, redriven, now runs — on that same state.
                let handler = &running_handler;
                let second = state
                    .execute_code(
                        admitted_context(handler, "waiting", &resolver).await,
                        cell("let second_cell = 2;"),
                    )
                    .await
                    .expect("the cell that waited now runs");
                assert_eq!(second.error(), None);
                state
                    .settle_code_execution(lash_core::plugin::CodeExecutionOutcome::Accepted)
                    .await
                    .expect("settle the second returned cell");

                let handler = &running_handler;
                let total = state
                    .execute_code(
                        admitted_context(handler, "total", &resolver).await,
                        cell("await control.finish(second_cell);"),
                    )
                    .await
                    .expect("execute code");
                assert_eq!(total.error(), None);
                assert_eq!(total.finish_value().cloned(), Some(serde_json::json!(2)));
            });
    }

    #[test]
    fn a_returned_cell_must_be_settled_before_another_cell_can_start() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let state = RlmRuntimeState::new_for_tests().expect("runtime state");
                let host =
                    crate::testing::DurableHost::open(crate::testing::default_cell_scope()).await;
                execute_cell(&state, &host, cell("let first = 1;"))
                    .await
                    .expect("first cell");

                let overlapping = execute_cell(&state, &host, cell("let second = 2;"))
                    .await
                    .expect_err("an unsettled response fences the next cell");
                assert!(matches!(
                    overlapping,
                    SessionError::Protocol(message)
                        if message == "the previous code execution response has not been settled"
                ));

                state
                    .settle_code_execution(lash_core::plugin::CodeExecutionOutcome::Accepted)
                    .await
                    .expect("settle first response");
                execute_cell(&state, &host, cell("let second = 2;"))
                    .await
                    .expect("settlement releases the next cell");
            });
    }

    /// FIG-5772: the bindings inventory lists a saved function with its
    /// signature and the cell whose end froze what it reads.
    #[test]
    fn a_saved_function_is_listed_with_its_signature_and_the_cell_that_froze_it() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let state = RlmRuntimeState::new_for_tests().expect("runtime state");
                let host = crate::testing::DurableHost::open(crate::testing::default_cell_scope()).await;
                let render = crate::testing::recorded_test_render();
                execute_cell(
                    &state,
                    &host,
                    cell("const rate = 2;\nfunction scale(value: number): number { return value * rate; }"),
                )
                .await
                .expect("execute code");

                let prompt = state
                    .prompt_facts(Some(&render), None, &Default::default())
                    .await
                    .expect("prompt facts")
                    .bound_variables;
                assert!(
                    prompt.contains("`scale`")
                        && prompt.contains(
                            "function (value: number) => number; captures frozen at cell 1"
                        ),
                    "{prompt}"
                );
            });
    }

    #[test]
    fn cancelled_settlement_restores_the_bound_variables_render() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let state = RlmRuntimeState::new_for_tests().expect("runtime state");
                let host =
                    crate::testing::DurableHost::open(crate::testing::default_cell_scope()).await;
                let render = crate::testing::recorded_test_render();

                execute_cell(&state, &host, cell("let survives = 7;"))
                    .await
                    .expect("execute accepted cell");
                state
                    .settle_code_execution(lash_core::plugin::CodeExecutionOutcome::Accepted)
                    .await
                    .expect("accept first cell");
                execute_cell(&state, &host, cell("let cancelled_tail = 1;"))
                    .await
                    .expect("execute cell before late cancellation");
                let rendered = state
                    .prompt_facts(Some(&render), None, &Default::default())
                    .await
                    .expect("prompt facts")
                    .bound_variables;
                assert!(rendered.contains("cancelled_tail"));

                state
                    .settle_code_execution(lash_core::plugin::CodeExecutionOutcome::Cancelled)
                    .await
                    .expect("cancel second cell");
                let rendered = state
                    .prompt_facts(Some(&render), None, &Default::default())
                    .await
                    .expect("prompt facts")
                    .bound_variables;
                assert!(rendered.contains("survives"));
                assert!(!rendered.contains("cancelled_tail"));
            });
    }

    /// A restore view for `frame` whose active history carries one RLM seed

    #[test]
    fn same_frame_restore_without_a_snapshot_discards_uncommitted_execution() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let state = RlmRuntimeState::new_for_tests().expect("state");
                let host =
                    crate::testing::DurableHost::open(crate::testing::default_cell_scope()).await;
                state
                    .restore_runtime_session_state(restore_view(
                        "frame-1",
                        None,
                        baton_seed_nodes("committed"),
                    ))
                    .await
                    .expect("restore seeds the committed baton");
                assert_eq!(
                    Box::pin(live_baton(&state, &host)).await,
                    serde_json::json!("committed")
                );

                let mutated = execute_cell(
                    &state,
                    &host,
                    cell("let baton = \"uncommitted\";\nawait control.finish(baton);"),
                )
                .await
                .expect("the mutating cell runs");
                assert_eq!(
                    mutated.finish_value().cloned(),
                    Some(serde_json::json!("uncommitted")),
                    "{:?}",
                    mutated.error()
                );
                state
                    .settle_code_execution(lash_core::plugin::CodeExecutionOutcome::Accepted)
                    .await
                    .expect("settle the mutating cell");

                state
                    .restore_runtime_session_state(restore_view(
                        "frame-1",
                        None,
                        baton_seed_nodes("committed"),
                    ))
                    .await
                    .expect("a same-frame restore without a snapshot succeeds");
                assert_eq!(
                    Box::pin(live_baton(&state, &host)).await,
                    serde_json::json!("committed"),
                    "the restore must rebuild the execution from the view, not keep the \
                     uncommitted assignment"
                );
            });
    }

    /// FIG-2521: a same-frame restore whose view carries an execution snapshot
    /// replaces the live execution with that snapshot. This is the view the
    /// runtime builds for an append rollback (the pre-append capture) and for a
    /// reload whose durable head carries an execution root.
    #[test]
    fn same_frame_restore_with_a_snapshot_replaces_the_live_execution() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let state = RlmRuntimeState::new_for_tests().expect("state");
                let host =
                    crate::testing::DurableHost::open(crate::testing::default_cell_scope()).await;
                state
                    .restore_runtime_session_state(restore_view(
                        "frame-1",
                        None,
                        baton_seed_nodes("committed"),
                    ))
                    .await
                    .expect("restore seeds the committed baton");
                crate::testing::commit_phase(&state, &host).await;
                let committed = state
                    .hydrated_execution_graph()
                    .await
                    .expect("capture")
                    .expect("the code mode executor always holds a snapshotable state");

                let mutated = execute_cell(
                    &state,
                    &host,
                    cell("let baton = \"uncommitted\";\nawait control.finish(baton);"),
                )
                .await
                .expect("the mutating cell runs");
                assert_eq!(
                    mutated.finish_value().cloned(),
                    Some(serde_json::json!("uncommitted")),
                    "{:?}",
                    mutated.error()
                );
                state
                    .settle_code_execution(lash_core::plugin::CodeExecutionOutcome::Accepted)
                    .await
                    .expect("settle the mutating cell");

                state
                    .restore_runtime_session_state(restore_view(
                        "frame-1",
                        Some(committed.clone()),
                        baton_seed_nodes("committed"),
                    ))
                    .await
                    .expect("a same-frame restore with a snapshot succeeds");
                // Compared before any further cell runs: executing a cell pins
                // this execution's child attempt bound into the root, which is
                // a write the restore itself must not make.
                assert_eq!(
                    state
                        .hydrated_execution_graph()
                        .await
                        .expect("capture")
                        .expect("state"),
                    committed,
                    "the restored execution is exactly the view's snapshot"
                );
                assert_eq!(
                    Box::pin(live_baton(&state, &host)).await,
                    serde_json::json!("committed")
                );
            });
    }
}

#[cfg(test)]
mod seed_laws {
    use super::*;

    /// RLM-SEED-WRITABLE: a seed is ordinary data, its variable writable and
    /// retained by a cold session restore. Former wrapper-shaped data has no
    /// special authority or read-only meaning.
    #[tokio::test]
    async fn a_seed_entry_is_writable_after_cold_reopen() {
        let mut host =
            crate::testing::DurableHost::open(crate::testing::default_cell_scope()).await;
        let state = RlmRuntimeState::new_for_tests().expect("state");
        let seed = crate::RlmSeed::from_seed_value(&serde_json::json!({
            "value": {"__projected__":{"kind":"materialized", "value":41}}
        }))
        .expect("seed");
        state
            .append_session_nodes(&crate::rlm_seed_initial_nodes(
                seed,
                lash_core::FleetFormat::current(),
            ))
            .await
            .expect("install frame seed");
        let run = |key, code: &str, state: Arc<RlmRuntimeState>, ctx| {
            let code = code.to_owned();
            async move {
                let _ = key;
                let response = state
                    .execute_code(ctx, lash_core::ExecRequest { code })
                    .await
                    .expect("execute");
                assert!(response.error().is_none(), "{:?}", response.error());
                state
                    .settle_code_execution(lash_core::plugin::CodeExecutionOutcome::Accepted)
                    .await
                    .expect("accept");
                response
            }
        };
        let state = Arc::new(state);
        let first = run(
            "seed-first",
            "value = 42; await control.finish(value);",
            state.clone(),
            crate::testing::recorded_cell_context(
                &host,
                "test-session",
                "test-turn",
                "seed-first",
                Arc::new(crate::testing::NoTools),
            )
            .await,
        )
        .await;
        assert_eq!(first.finish_value(), Some(&serde_json::json!(42)));
        crate::testing::commit_phase(&state, &host).await;
        let graph = state
            .hydrated_execution_graph()
            .await
            .expect("graph")
            .expect("state graph");
        drop(state);
        host.kill_and_resume().await;
        let reopened = Arc::new(RlmRuntimeState::new_for_tests().expect("cold session"));
        reopened
            .restore_execution_graph(&graph, lash_core::plugin::ExecutionGraphPurpose::Working)
            .await
            .expect("cold restore");
        let last = run(
            "seed-second",
            "value = value + 1; await control.finish(value);",
            reopened,
            crate::testing::recorded_cell_context(
                &host,
                "test-session",
                "test-turn",
                "seed-second",
                Arc::new(crate::testing::NoTools),
            )
            .await,
        )
        .await;
        assert_eq!(last.finish_value(), Some(&serde_json::json!(43)));
    }
}
