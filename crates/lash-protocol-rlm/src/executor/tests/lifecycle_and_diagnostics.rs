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
