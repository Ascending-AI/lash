use super::*;

/// A typo is not a policy refusal.
///
/// Classifying every compile failure as Policy produced the one thing
/// the typed distinction exists to prevent: `unknown name \`task\`` arrived under "the
/// runtime refused this cell; sending it again unchanged will be refused
/// again. Rewrite it in the form named above" — with no form named above,
/// because a misspelled identifier has no accepted alternative form. The
/// gate is the diagnostic code, not the fact that compilation failed.
#[tokio::test]
pub(super) async fn a_wrong_program_and_a_forbidden_construct_are_classified_apart() {
    let kind = async |source: &str| match lash_vm_client::service::Service::default()
        .request_accounted(lash_vm_client::service::Request::CompileModule {
            source: source.into(),
            environment: Default::default(),
            cell: true,
        })
        .await
        .expect("worker diagnostic")
    {
        lash_vm_client::service::Response::CompileRefused { policy, .. } => {
            if policy {
                lash_core::CellFailureKind::Policy
            } else {
                lash_core::CellFailureKind::Program
            }
        }
        other => panic!("expected a refusal: {other:?}"),
    };
    assert_eq!(
        kind("finish(taks);").await,
        lash_core::CellFailureKind::Program,
        "a misspelled name is the program being wrong"
    );
    assert_eq!(
        kind("class A {}").await,
        lash_core::CellFailureKind::Policy,
        "a construct outside the dialect is a refusal"
    );

    // And the imperative the Policy branch chooses is only honest when the
    // diagnostic really does name a form.
    let forbidden = lash_typescript::parse_with_globals("class A {}", &BTreeSet::new())
        .expect_err("classes are refused");
    assert!(
        !forbidden.suggestions.is_empty(),
        "a Policy classification promises a named form: {forbidden:?}"
    );

    // One code, both families. `TS_METHOD_UNSUPPORTED` is emitted both for
    // the determinism refusals — which the runtime will never run, however
    // the model rewrites them — and for ordinary arity mistakes. Reading
    // the code alone gets one of the two wrong whichever way it is read.
    let nondeterministic = "finish('a'.localeCompare('b'));";
    let miscounted = "finish([1].map());";
    assert_eq!(
        lash_typescript::parse_with_globals(nondeterministic, &BTreeSet::new())
            .expect_err("locale ordering is refused")
            .code
            .as_str(),
        lash_typescript::parse_with_globals(miscounted, &BTreeSet::new())
            .expect_err("map needs a callback")
            .code
            .as_str(),
        "the premise of this check is that one code carries both"
    );
    assert_eq!(
        kind(nondeterministic).await,
        lash_core::CellFailureKind::Policy,
        "the runtime will never run this"
    );
    assert_eq!(
        kind(miscounted).await,
        lash_core::CellFailureKind::Program,
        "the method exists and the call is wrong"
    );
}

/// The executor is where a TypeScript rejection becomes the text a model
/// reads, and for the whole of the dialect's life that conversion was
/// `error.to_string()` — which drops the span the diagnostic carries. The
/// model was told a construct was refused and left to find it.
#[test]
pub(super) fn a_typescript_rejection_reaches_the_model_with_its_own_line_number() {
    let code = "const rows = [1, 2, 3];\nconst total = 0;\nclass Accumulator {}\n";
    let error = lash_typescript::parse_with_globals(code, &BTreeSet::new())
        .expect_err("classes are refused");
    let diagnostic = lash_typescript::format_diagnostic(code, &error);

    assert!(
        diagnostic.starts_with("TS_CLASS_UNSUPPORTED: "),
        "{diagnostic}"
    );
    assert!(diagnostic.contains("--> line 3, column 1"), "{diagnostic}");
    assert!(
        diagnostic.contains("\nclass Accumulator {}\n"),
        "{diagnostic}"
    );
    assert!(diagnostic.contains("\nhint: "), "{diagnostic}");
}

#[tokio::test]
pub(super) async fn typescript_method_diagnostics_consult_the_link_time_module_catalog() {
    let mut catalog = lashlang::LashlangHostCatalog::new();
    catalog
        .add_module_operation(
            ["text"],
            "TextModule",
            "sha256",
            "tool:text/sha256",
            lashlang::TypeExpr::Any,
            lashlang::TypeExpr::Any,
        )
        .expect("text module operation");
    let environment =
        lashlang::LashlangHostEnvironment::new(catalog, lashlang::LashlangAbilities::default())
            .with_globals(["text"]);

    let diagnostic = async |source: &str| match lash_vm_client::service::Service::default()
        .request_accounted(lash_vm_client::service::Request::CompileModule {
            source: source.into(),
            environment: environment.clone(),
            cell: true,
        })
        .await
        .expect("worker diagnostic")
    {
        lash_vm_client::service::Response::CompileRefused {
            error: lashlang::ModuleCompileError::Parse(diagnostic),
            ..
        } => diagnostic.message,
        other => panic!("expected a parse refusal: {other:?}"),
    };
    let shadowed = diagnostic("text.sha256({});").await;
    assert_eq!(
        shadowed,
        "local binding `text` shadows module `text`; rename the binding or call the module before binding"
    );
    let ordinary = diagnostic("const s = 'a,b'; s.anchor(',');").await;
    assert_eq!(
        ordinary,
        "method `anchor` is not in the TypeScript runtime surface"
    );
}

#[derive(Default)]
pub(super) struct NoopHost;

impl ExecutionHost for NoopHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(operation) => Err(ExecutionHostError::new(format!(
                "unknown module operation: {}",
                operation.operation
            ))),
            AbilityOp::Finish(value) | AbilityOp::Fail(value) => Ok(AbilityOutcome::Value(value)),
            _ => Err(ExecutionHostError::new("unsupported host ability")),
        }
    }
}

pub(super) async fn worker_compile_program(
    program: &lashlang::Program,
) -> Result<lash_vm_client::service::CompiledModule, String> {
    match lash_vm_client::service::Service::default()
        .request_accounted(lash_vm_client::service::Request::CompileAst {
            source: String::new(),
            program: program.clone(),
            environment: Default::default(),
        })
        .await
        .map_err(|e| e.to_string())?
    {
        lash_vm_client::service::Response::Module(module) => Ok(*module),
        other => Err(format!("unexpected worker compile response: {other:?}")),
    }
}
pub(super) trait WorkerFixtureState {
    fn worker_bytes(&self) -> Option<Vec<u8>>;
    async fn install_worker_bytes(&mut self, bytes: Vec<u8>) -> Result<(), String>;
}
impl WorkerFixtureState for lash_vm_client::RemoteState {
    fn worker_bytes(&self) -> Option<Vec<u8>> {
        self.bytes().map(Vec::from)
    }
    async fn install_worker_bytes(&mut self, bytes: Vec<u8>) -> Result<(), String> {
        self.install_bytes(bytes)
            .await
            .map_err(|error| error.to_string())
    }
}
impl WorkerFixtureState for lashlang::State {
    fn worker_bytes(&self) -> Option<Vec<u8>> {
        Some(
            self.snapshot()
                .to_canonical_bytes()
                .expect("fixture state encodes"),
        )
    }
    async fn install_worker_bytes(&mut self, bytes: Vec<u8>) -> Result<(), String> {
        *self = lashlang::State::from_snapshot(
            lashlang::VmInstance::pristine()
                .open_snapshot(&bytes)
                .map_err(|e| e.to_string())?,
        );
        Ok(())
    }
}
pub(super) async fn execute_with_projected(
    module: &lash_vm_client::service::CompiledModule,
    state: &mut impl WorkerFixtureState,
    projected: &ProjectedBindings,
) -> Result<ExecutionOutcome, lashlang::RuntimeError> {
    let service = lash_vm_client::service::Service::default();
    let owner = lash_vm_protocol::VmOwner::new("projection-witness");
    let snapshot = state
        .worker_bytes()
        .map(|bytes| {
            lash_vm_protocol::StartState::Snapshot(lash_vm_protocol::OpaqueVmState::seal(
                lash_vm_protocol::VmStateKind::Snapshot,
                owner.clone(),
                lashlang::vm_contract_versions(),
                bytes,
            ))
        })
        .unwrap_or(lash_vm_protocol::StartState::Fresh);
    // The witness reads projections and parks nowhere, so nothing commits.
    let process_id = lash_sansio::ProcessId::fixture("projection-witness");
    let cx = lash_core::ActorContext::unavailable();
    let snapshots = lash_vm_broker::DurableSnapshotStore::new(
        &cx,
        lash_vm_broker::ExecKey::Process(process_id.clone()),
    );
    let admissions = lash_lashlang_runtime::RunAdmissions {
        opener: lash_core::EffectOpener::process(process_id),
        limit: lash_lashlang_runtime::run_operation_limit(&cx),
        policy: &|_, _| None,
        host_state: &|| Ok(None),
    };
    let run = lash_lashlang_runtime::WorkerRun {
        service: &service,
        host: &NoopHost,
        identities: lash_vm_broker::CodeCallIdentities::process_body(
            lash_sansio::ProcessId::fixture("projection-witness"),
        ),
        owner,
        frame_epoch: lash_vm_protocol::FrameEpoch(0),
        program: lash_vm_protocol::ProgramSource::Artifact {
            module_ref: module.module_ref.to_string(),
            entry: lash_vm_protocol::ProgramEntry::Main,
            artifact: module.artifact.bytes().to_vec(),
        },
        context: lash_vm_client::RunContext::default(),
        projected: projected.clone(),
        bounds: lashlang::ExecutionBounds::new(
            lashlang::ExecutionBound::Unbounded,
            lashlang::ExecutionBound::Unbounded,
        ),
        state: snapshot,
        from: None,
        snapshots: &snapshots,
        admissions: &admissions,
        boundary: &|| false,
        hand_over: None,
        providers: lashlang::testing::projection::test_catalog(),
    }
    .run()
    .await
    .expect("worker run");
    match run {
        lash_vm_broker::BrokeredEnd::Complete { value, checkpoint } => {
            state
                .install_worker_bytes(checkpoint.vm.bytes().to_vec())
                .await
                .expect("install worker state");
            Ok(rmp_serde::from_slice(&value.0).expect("worker outcome"))
        }
        lash_vm_broker::BrokeredEnd::GuestError { error, checkpoint } => {
            if let Some(checkpoint) = checkpoint {
                state
                    .install_worker_bytes(checkpoint.vm.bytes().to_vec())
                    .await
                    .expect("install worker state");
            }
            Err(rmp_serde::from_slice::<lashlang::RuntimeFailure>(&error.0)
                .expect("worker guest failure")
                .error)
        }
        other => panic!("unexpected worker end: {other:?}"),
    }
}

pub(super) fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}

pub(super) fn hydrate_snapshot(
    snapshot: lash_core::plugin::ExecutionStateCapture,
) -> lash_core::plugin::HydratedExecutionState {
    let lash_core::plugin::ExecutionStateCapture::Replace { root, leaves } = snapshot else {
        panic!("expected replacement capture");
    };
    lash_core::plugin::HydratedExecutionState {
        root,
        components: leaves
            .into_iter()
            .map(|(key, component)| match component {
                lash_core::plugin::LeafChange::Changed(body) => (key, body),
                lash_core::plugin::LeafChange::Unchanged => {
                    panic!("fresh test snapshot unexpectedly reused `{key}`")
                }
            })
            .collect(),
    }
}

pub(super) struct TestProjectedValue(Vec<FlowValue>);

#[derive(Default)]
pub(super) struct SnapshotProjectedToolText {
    pub(super) materialize_count: AtomicUsize,
    pub(super) render_count: AtomicUsize,
}

impl lashlang::testing::projection::TestView for SnapshotProjectedToolText {
    fn type_name(&self) -> &str {
        "string"
    }

    fn read_one(&self, request: ProjectedReadRequest) -> Option<ProjectedReadResponse> {
        match request {
            ProjectedReadRequest::Render => {
                self.render_count.fetch_add(1, Ordering::SeqCst);
                Some(ProjectedReadResponse::Text(
                    "rendered tool text".to_string(),
                ))
            }
            ProjectedReadRequest::Materialize => {
                self.materialize_count.fetch_add(1, Ordering::SeqCst);
                Some(ProjectedReadResponse::Value(FlowValue::String(
                    "materialized tool text".into(),
                )))
            }
            _ => None,
        }
    }
}

impl lashlang::testing::projection::TestView for TestProjectedValue {
    fn type_name(&self) -> &str {
        "list"
    }

    fn read_one(&self, request: ProjectedReadRequest) -> Option<ProjectedReadResponse> {
        let ProjectedReadRequest::Index(index) = request else {
            return match request {
                ProjectedReadRequest::Len => Some(ProjectedReadResponse::Len(self.0.len())),
                ProjectedReadRequest::Materialize => Some(ProjectedReadResponse::Value(
                    FlowValue::List(self.0.clone().into()),
                )),
                _ => None,
            };
        };
        let Ok(Some(index)) = projected_index(&index, self.0.len()) else {
            return None;
        };
        self.0.get(index).cloned().map(ProjectedReadResponse::Value)
    }
}

pub(super) fn projected_history(values: Vec<FlowValue>) -> ProjectedBindings {
    let mut projected = ProjectedBindings::new();
    projected.insert(
        "history",
        lashlang::testing::projection::test_view("history", Arc::new(TestProjectedValue(values))),
    );
    projected
}

/// A host setup step that can fail before the program runs.
#[derive(Clone, Copy, Debug)]
enum HostSetupFailureSite {
    HostEnvironment,
    ArtifactStore,
    ResolveProjectedBindings,
    CancelledSetup,
}

struct FailingArtifactStore;

#[async_trait::async_trait]
impl lash_core::ModuleArtifactStore for FailingArtifactStore {
    async fn publish_module_artifact(
        &self,
        _claim: &lash_core::ReferrerClaim,
        _module_ref: &str,
        _bytes: &[u8],
    ) -> Result<(), lash_core::ArtifactStoreError> {
        Err(lash_core::ArtifactStoreError::Backend(
            "injected artifact store failure".to_string(),
        ))
    }

    async fn acquire_module_artifact(
        &self,
        _claim: &lash_core::ReferrerClaim,
        _module_ref: &str,
    ) -> Result<(), lash_core::ArtifactStoreError> {
        Err(lash_core::ArtifactStoreError::Backend(
            "injected artifact store failure".to_string(),
        ))
    }

    async fn end_module_referrer(
        &self,
        _cleanup: &lash_core::ResolvedArtifactCleanup,
    ) -> Result<(), lash_core::ArtifactStoreError> {
        Err(lash_core::ArtifactStoreError::Backend(
            "injected artifact store failure".to_string(),
        ))
    }

    async fn get_module_artifact(
        &self,
        _module_ref: &str,
    ) -> Result<Option<Vec<u8>>, lash_core::ArtifactStoreError> {
        Ok(None)
    }
}

fn colliding_host_catalog() -> lash_core::ToolCatalog {
    let definition = |id, name| {
        lash_core::ToolDefinition::raw(
            id,
            name,
            "colliding test binding",
            lash_core::ToolDefinition::default_input_schema(),
            serde_json::json!({ "type": "boolean" }),
        )
        .expect("valid declared tool schemas")
        .with_tool_binding(lash_lashlang_runtime::ToolBinding::new(
            ["test"],
            "collision",
        ))
    };
    lash_core::ToolCatalog::from_tool_definitions(vec![
        definition("tool:collision_a", "collision_a"),
        definition("tool:collision_b", "collision_b"),
    ])
}

/// Pauses after the first journaled cell checkpoint has observed no stop.
/// The VM reaches this effect only after running 2^20 instructions in the

async fn inject_host_setup_failure(site: HostSetupFailureSite) -> ExecResponse {
    let mut state = RlmExecutionState::new();
    let handler = crate::testing::DurableHost::open(crate::testing::default_cell_scope()).await;
    let mut context = None;
    let mut request = ExecRequest {
        code: "finish(1);".to_string(),
    };
    let mut artifact_store: lashlang::LashlangArtifacts = handler.artifacts();
    let mut surface = LashlangSurface::default();
    let mut projected_bindings = RlmProjectedBindings::default();

    match site {
        HostSetupFailureSite::HostEnvironment => {
            context = Some(
                lash_core::testing::code_execution_context_with_tool_catalog(
                    handler.ports(),
                    colliding_host_catalog(),
                ),
            );
        }
        HostSetupFailureSite::ArtifactStore => {
            request.code = r#"const worker = async () => { return null; };
            finish(null);"#
                .to_string();
            artifact_store = lashlang::LashlangArtifacts::new(Arc::new(FailingArtifactStore));
            surface = LashlangSurface::new(
                lashlang::LashlangAbilities::default(),
                lashlang::LashlangLanguageFeatures::default(),
                lashlang::LashlangHostCatalog::new(),
            );
        }
        HostSetupFailureSite::ResolveProjectedBindings => {
            // `history` is the reserved built-in: binding it through the
            // session collides when the executor assembles projected
            // bindings.
            projected_bindings = RlmProjectedBindings::new()
                .bind_json("history", serde_json::json!("injected"))
                .expect("bind injected projection");
        }
        HostSetupFailureSite::CancelledSetup => {
            context = Some(lash_core::testing::cancelled_code_execution_context(
                handler.ports(),
            ));
            request.code = "missing =".to_string();
        }
    }

    let context =
        context.unwrap_or_else(|| lash_core::testing::code_execution_context(handler.ports()));
    execute_code_with_test_render(
        &mut state,
        context,
        request,
        artifact_store,
        surface,
        None,
        projected_bindings,
        None,
        lashlang::ExecutionBounds::unbounded(),
        crate::plugin::RlmChannel::Cell,
    )
    .await
}

#[test]
pub(super) fn every_host_setup_failure_is_classified_as_host() {
    block_on(async {
        // The `LinkedProgramCacheError` catch-all among the `Host`
        // classifications in `executor/mod.rs` intentionally has no row: the
        // cache currently constructs only `Parse` and `Link`, which are
        // classified by the preceding arms, so no host-classified variant can
        // be injected through its public API.
        let cases = [
            (
                HostSetupFailureSite::HostEnvironment,
                "invalid Lashlang host tool surface",
            ),
            (
                HostSetupFailureSite::ArtifactStore,
                "injected artifact store failure",
            ),
            (
                HostSetupFailureSite::ResolveProjectedBindings,
                "`history` is already bound as an RLM projected binding",
            ),
            (
                HostSetupFailureSite::CancelledSetup,
                "foreground execution stopped during setup",
            ),
        ];

        for (site, expected_message) in cases {
            let error = Box::pin(inject_host_setup_failure(site))
                .await
                .error
                .unwrap_or_else(|| panic!("{site:?}: injected setup failure must be observed"));
            assert!(
                error.message.contains(expected_message),
                "{site:?}: wrong setup path reached: {error:?}"
            );
            assert_eq!(
                error.kind,
                lash_core::CellFailureKind::Host,
                "{site:?}: host setup failures must not blame the program"
            );
        }
    });
}

/// The context of a cell the host stops through `stop`.
fn stopped_on(
    handler: &crate::testing::DurableHost,
    stop: lash_core::CancellationToken,
) -> RuntimeExecutionContext<'static> {
    lash_core::testing::code_execution_context(handler.ports()).with_cancellation_token(stop)
}

/// A cell spinning in a loop observes the host's stop at its next
/// checkpoint: it fails as a host stop, its unfinished bindings are
/// discarded, and the session's earlier bindings survive a cold restore.
#[test]
pub(super) fn spinning_code_observes_a_mid_execution_host_stop() {
    block_on(async {
        let mut state = RlmExecutionState::for_engine("typescript");
        let handler = crate::testing::DurableHost::open(crate::testing::default_cell_scope()).await;
        let first = execute_code_with_test_render(
            &mut state,
            lash_core::testing::code_execution_context(handler.ports()),
            ExecRequest {
                code: "let survives: number = 7;".to_string(),
            },
            handler.artifacts(),
            LashlangSurface::default(),
            None,
            RlmProjectedBindings::default(),
            None,
            lashlang::ExecutionBounds::unbounded(),
            crate::plugin::RlmChannel::Cell,
        )
        .await;
        assert_eq!(first.error, None);

        let stop = lash_core::CancellationToken::new();
        let execution = execute_code_with_test_render(
            &mut state,
            stopped_on(&handler, stop.clone()),
            ExecRequest {
                code: "let cancelledTail: number = 1; while (true) {}".to_string(),
            },
            handler.artifacts(),
            LashlangSurface::default(),
            None,
            RlmProjectedBindings::default(),
            None,
            lashlang::ExecutionBounds::unbounded(),
            crate::plugin::RlmChannel::Cell,
        );
        let response = Box::pin(tokio::time::timeout(
            std::time::Duration::from_secs(5),
            async {
                let (response, ()) = tokio::join!(execution, async {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    stop.cancel();
                });
                response
            },
        ))
        .await
        .expect("the running loop observes the mid-spin stop");

        assert_eq!(
            response
                .error
                .expect("the stopped cell reports an error")
                .kind,
            lash_core::CellFailureKind::Host
        );
        assert!(state.vm.state().globals().get("cancelledTail").is_none());
        assert!(state.vm.state().globals().get("survives").is_some());
        let snapshot = hydrate_snapshot(
            state
                .snapshot_execution_state(lash_core::FleetFormat::current())
                .await
                .expect("snapshot after mid-spin cancellation"),
        );
        let mut restored = RlmExecutionState::for_engine("typescript");
        restored
            .restore_execution_state(&snapshot, lash_core::FleetFormat::current())
            .await
            .expect("cold restore after mid-spin cancellation");
        assert!(restored.vm.state().globals().get("cancelledTail").is_none());
        assert!(restored.vm.state().globals().get("survives").is_some());
    });
}

#[test]
pub(super) fn late_cancellation_preserves_staged_and_acknowledged_large_leaf_bookkeeping() {
    block_on(async {
        {
            let (language, first_code, tail_code, tail_binding) = (
                "typescript",
                format!("let survives: string = \"{}\";", "x".repeat(1024)),
                "let cancelledTail: number = 1;",
                "cancelledTail",
            );
            for acknowledge_first_capture in [false, true] {
                let mut state = RlmExecutionState::for_engine(language);
                let handler =
                    crate::testing::DurableHost::open(crate::testing::default_cell_scope()).await;
                let first = execute_code_with_test_render(
                    &mut state,
                    lash_core::testing::code_execution_context(handler.ports()),
                    ExecRequest {
                        code: first_code.clone(),
                    },
                    handler.artifacts(),
                    LashlangSurface::default(),
                    None,
                    RlmProjectedBindings::default(),
                    None,
                    lashlang::ExecutionBounds::unbounded(),
                    crate::plugin::RlmChannel::Cell,
                )
                .await;
                assert_eq!(first.error, None, "{language}: large first cell");
                let first_snapshot = state
                    .snapshot_execution_state(lash_core::FleetFormat::current())
                    .await
                    .expect("large first-cell snapshot");
                let first_hydration = hydrate_snapshot(first_snapshot);
                if acknowledge_first_capture {
                    state.acknowledge_execution_state_capture();
                }

                let handler =
                    crate::testing::DurableHost::open(crate::testing::default_cell_scope()).await;
                let tail = execute_code_with_test_render(
                    &mut state,
                    lash_core::testing::code_execution_context(handler.ports()),
                    ExecRequest {
                        code: tail_code.to_string(),
                    },
                    handler.artifacts(),
                    LashlangSurface::default(),
                    None,
                    RlmProjectedBindings::default(),
                    None,
                    lashlang::ExecutionBounds::unbounded(),
                    crate::plugin::RlmChannel::Cell,
                )
                .await;
                assert_eq!(tail.error, None, "{language}: tail cell");
                state.terminate_code_execution();

                let final_snapshot = state
                    .snapshot_execution_state(lash_core::FleetFormat::current())
                    .await
                    .expect("snapshot after late cancellation");
                let final_hydration = if acknowledge_first_capture {
                    hydrate_snapshot_against(final_snapshot, &first_hydration)
                } else {
                    hydrate_snapshot(final_snapshot)
                };
                let mut restored = RlmExecutionState::for_engine(language);
                restored
                    .restore_execution_state(&final_hydration, lash_core::FleetFormat::current())
                    .await
                    .expect("cold restore after late cancellation");
                assert!(restored.vm.state().globals().get("survives").is_some());
                assert!(restored.vm.state().globals().get(tail_binding).is_none());
            }
        }
    });
}

/// A typo is not a policy refusal.
///
/// Classifying every compile failure as Policy produced the one thing
/// the typed distinction exists to prevent: `unknown name \`task\`` arrived under "the
/// runtime refused this cell; sending it again unchanged will be refused
/// again. Rewrite it in the form named above" — with no form named above,
/// because a misspelled identifier has no accepted alternative form. The

pub(super) fn hydrate_snapshot_against(
    snapshot: lash_core::plugin::ExecutionStateCapture,
    prior: &lash_core::plugin::HydratedExecutionState,
) -> lash_core::plugin::HydratedExecutionState {
    let lash_core::plugin::ExecutionStateCapture::Replace { root, leaves } = snapshot else {
        panic!("expected replacement capture");
    };
    lash_core::plugin::HydratedExecutionState {
        root,
        components: leaves
            .into_iter()
            .map(|(key, component)| match component {
                lash_core::plugin::LeafChange::Changed(body) => (key, body),
                lash_core::plugin::LeafChange::Unchanged => {
                    let body = prior
                        .components
                        .get(&key)
                        .unwrap_or_else(|| panic!("durable prior is missing leaf `{key}`"))
                        .clone();
                    (key, body)
                }
            })
            .collect(),
    }
}

#[derive(Default)]
pub(super) struct NoopTraceSink;

impl lash_core::facade_support::TraceSink for NoopTraceSink {
    fn append(
        &self,
        _record: &lash_core::facade_support::TraceRecord,
    ) -> Result<(), lash_core::facade_support::TraceSinkError> {
        Ok(())
    }
}

static EXECUTION_BOUND_EXHAUSTION_MODE: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(super) async fn execute_continue_as_with_trace_sink(
    trace_sink: Option<Arc<dyn lash_core::facade_support::TraceSink>>,
) -> lash_core::ToolCallRecord {
    let definition = crate::continue_as_tool_definition(&crate::dialect::TypescriptDialect);
    let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![definition]);
    let invocation = lash_core::testing::exec_code_invocation(
        "test-session",
        "turn-7",
        7,
        2,
        "exec-code-3",
        "exec-code:3",
    );
    let handler = crate::testing::DurableHost::open(lash_core::AdmittedScope::turn(
        lash_core::SessionId::from("test-session"),
        lash_core::TurnId::from("turn-7"),
    ))
    .await;
    let context =
        lash_core::testing::code_execution_context_with_tool_provider_catalog_and_invocation(
            handler.ports(),
            Arc::new(crate::control_tools::RlmControlToolsProvider {
                vocabulary: crate::dialect::Dialect::prompt_vocabulary(
                    &crate::dialect::TypescriptDialect,
                ),
            }),
            catalog,
            invocation,
        );
    let response = execute_code_with_test_render(
        &mut RlmExecutionState::new(),
        context,
        ExecRequest {
            code: r#"await control.continue_as({ task: "continue deterministically" });"#
                .to_string(),
        },
        handler.artifacts(),
        LashlangSurface::default(),
        None,
        RlmProjectedBindings::default(),
        trace_sink.map(test_trace),
        lashlang::ExecutionBounds::unbounded(),
        crate::plugin::RlmChannel::Cell,
    )
    .await;
    assert_eq!(response.error, None);
    assert_eq!(response.calls.len(), 1);
    response
        .calls
        .into_iter()
        .next()
        .and_then(|call| call.host_record)
        .expect("one continue_as host record")
}

#[test]
#[ignore = "blocked: L4 (FIG-5174): a cell's tool call fails, `the final has no hydrated admission` (ProductionToolHandlers::prepared is never filled); repro executor::tests::typescript_cells::code_mode_receives_the_structured_tool_value_and_ignores_its_view"]
pub(super) fn resource_call_identity_is_trace_sink_independent() {
    block_on(async {
        let without_trace = Box::pin(execute_continue_as_with_trace_sink(None)).await;
        let with_trace = Box::pin(execute_continue_as_with_trace_sink(Some(Arc::new(
            NoopTraceSink,
        ))))
        .await;

        // Semantic hash v8 deliberately rekeys the module-rooted execution
        // site and the frame key derived from its call ID. Keep both literal
        // while proving trace configuration is absent from their inputs.
        // Re-pinned by the single-language cutover (ADR 0096): the instruction
        // set lost the deep-copy instructions the retired surface compiled to,
        // so this program's canonical IR — and the digest keyed off it — is a
        // different constant. Re-pinned again by FIG-3071, which moved
        // `LASHLANG_SEMANTIC_HASH_VERSION` to v10 because a declared process
        // parameter type now reaches module identity. Re-pinned again by
        // FIG-2996 part 1, which moved `LASHLANG_SEMANTIC_HASH_VERSION` to v11
        // for the one handle kind. Re-pinned again by FIG-3088, which moved the
        // constant to v12 after the hash-writer rewrite. Re-pinned again by
        // FIG-2997, which moved the constant to v13 for the process-literal
        // lift. Re-pinned again by FIG-2999, which moved the constant to v14
        // after the process special forms left the dialect and the ability set
        // they were gated by left `host_requirements`. Re-pinned again by FIG-3120,
        // which moved the constant to v15 after `canonical_program_ir` started
        // alpha-normalizing local binder names so equal module refs carry equal
        // bytes. Re-pinned again by FIG-3394, which tags the opener kind: a
        // turn scope is a free-form string that could spell a process opener
        // exactly, so `turn:` is part of the scope rather than decoration.
        // Re-pinned once more under FIG-3394 when the fixture began naming the
        // cell it executes: the opener is now the admitted turn address
        // (`test-session:turn-7`) rather than a fixture-spelled effect key.
        // Re-pinned once more under FIG-3394 when the opener's identity
        // encoding became canonical: every component is length-prefixed
        // (`turn:12:test-session:6:turn-7`) so delimiter-bearing ids cannot
        // collide, which changes the call id and the frame key derived from
        // it. FIG-3571 deleted `canonical_program_ir` and its normalizer: an
        // artifact carries the linked program verbatim as `ir`, so the module
        // ref hashes binder names again. The call id below names no module
        // ref, so it does not move.
        // Re-pinned by FIG-3586: a call's id is its issue ordinal under the
        // cell's scope, so neither the call site's node id nor the tool's
        // operation appears in it — which also moves the frame key derived
        // from it.
        // What the pair asserts is unchanged: the two sides are still equal,
        // which is the trace-sink independence this test exists for; only the
        // derivation both sides share moved.
        // Re-pinned by FIG-4080: the call id is the `ToolCallId` the cell's
        // admission derives for issue ordinal 0 (ADR 0117 §2).
        assert_eq!(
            without_trace.call_id.as_str(),
            "tc_714c3372ac3c8a6cf222ffeacd7f00c7a853cde7f72ae3a392bc558043b7f34d"
        );
        assert_eq!(with_trace.call_id, without_trace.call_id);

        let without_trace_key = match without_trace.output.control {
            Some(lash_core::ToolControl::SwitchAgentFrame { frame_key, .. }) => frame_key,
            other => panic!("expected frame switch, got {other:?}"),
        };
        let with_trace_key = match with_trace.output.control {
            Some(lash_core::ToolControl::SwitchAgentFrame { frame_key, .. }) => frame_key,
            other => panic!("expected frame switch, got {other:?}"),
        };
        assert_eq!(
            without_trace_key.as_str(),
            "frame-key/v2/f091a7415a8f61330c53a4d67429e6eb35dfafa71901d387dfb676850ae02b10"
        );
        assert_eq!(
            with_trace_key.as_str(),
            "frame-key/v2/f091a7415a8f61330c53a4d67429e6eb35dfafa71901d387dfb676850ae02b10"
        );
    });
}

#[test]
#[should_panic(expected = "confidence execution exhausted a required Lashlang bound")]
pub(super) fn confidence_execution_fails_loudly_on_bound_exhaustion() {
    let _mode = EXECUTION_BOUND_EXHAUSTION_MODE.lock_recover();
    block_on(async {
        let handler = crate::testing::DurableHost::open(crate::testing::default_cell_scope()).await;
        let _ = execute_code_with_test_render(
            &mut RlmExecutionState::new(),
            lash_core::testing::code_execution_context(handler.ports()),
            ExecRequest {
                code: "let i = 0;\nwhile (i < 5000) { i = i + 1; }\nfinish(i);".to_string(),
            },
            handler.artifacts(),
            LashlangSurface::default(),
            None,
            RlmProjectedBindings::default(),
            None,
            lashlang::ExecutionBounds::new(
                lashlang::ExecutionBound::instructions(1),
                lashlang::ExecutionBound::Unbounded,
            ),
            crate::plugin::RlmChannel::Cell,
        )
        .await;
    });
}

#[test]
fn typed_worker_size_limits_are_recorded_cell_failures_across_the_plugin_boundary() {
    let _mode = EXECUTION_BOUND_EXHAUSTION_MODE.lock_recover();
    struct RestoreLoudness(bool);
    impl Drop for RestoreLoudness {
        fn drop(&mut self) {
            set_execution_bound_exhaustion_loud(self.0);
        }
    }
    let _restore = RestoreLoudness(set_execution_bound_exhaustion_loud(false));
    block_on(async {
        for effect_limit in [true, false] {
            let handler =
                crate::testing::DurableHost::open(crate::testing::default_cell_scope()).await;
            let mut state = RlmExecutionState::new();
            let mut config = state.vm.state().service().config().clone();
            if effect_limit {
                config.protocol.max_effect_value_bytes = 1;
            } else {
                config.protocol.max_vm_state_bytes = 1;
            }
            state
                .vm
                .state_mut()
                .replace_service(lash_vm_client::service::Service::new(config));
            let result = execute_code_with_test_render(
                &mut state,
                lash_core::testing::code_execution_context(handler.ports()),
                ExecRequest {
                    code: "finish(42);".to_owned(),
                },
                handler.artifacts(),
                LashlangSurface::default(),
                None,
                RlmProjectedBindings::default(),
                None,
                lashlang::ExecutionBounds::unbounded(),
                crate::plugin::RlmChannel::Cell,
            )
            .await;
            let failure = result.error.expect("recorded run limit");
            assert_eq!(
                failure.kind,
                lash_core::CellFailureKind::Program,
                "{failure:?}"
            );
            let limit = failure.worker_limit.expect("typed cause");
            match (effect_limit, limit) {
                (true, lash_vm_protocol::WorkerLimit::EffectValue { size, bound: 1 })
                | (false, lash_vm_protocol::WorkerLimit::VmState { size, bound: 1 }) => {
                    assert!(size > 1)
                }
                other => panic!("the configured run limit: {other:?}"),
            }
            let encoded = serde_json::to_vec(&failure).expect("plugin result");
            let decoded: lash_core::CellFailure =
                serde_json::from_slice(&encoded).expect("host result");
            assert_eq!(decoded.worker_limit, Some(limit));
        }
    });
}
