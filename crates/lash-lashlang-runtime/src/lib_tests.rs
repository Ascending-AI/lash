use super::*;

use crate::process_grammar_tests::run_sleep_process;
use lashlang::testing::ast_builders as b;

const SEED: u64 = 0x1a5_1a9;

/// A storage-backed test backend for paths that do not execute engine effects.
pub(crate) async fn memory_backend() -> lash_core::Backend {
    lash_conformance::recording_backend_over(memory_store_set().await)
}

/// A fresh memory store set's Lashlang artifact store: a storage port a test
/// reaches without an engine.
pub(crate) async fn memory_artifact_store() -> LashlangArtifacts {
    use lash_core_execution::StoreSet;
    LashlangArtifacts::new(memory_store_set().await.module_artifacts())
}

thread_local! {
    /// The store sets the running test opened, held as its backends are.
    static HELD_STORE_SETS: std::cell::RefCell<Vec<Arc<lash_sqlite_store::SqliteStoreSet>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// A fresh SQLite memory store set, storage only (no engine), held for the
/// rest of the running test: the twin of `memory_backend` for a test that
/// reaches only store ports.
pub(crate) async fn memory_store_set() -> Arc<lash_sqlite_store::SqliteStoreSet> {
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open a SQLite memory store set"),
    );
    HELD_STORE_SETS.with(|held| held.borrow_mut().push(Arc::clone(&stores)));
    stores
}

#[test]
fn effect_group_wait_identity_uses_the_durable_group_contract() {
    let invocation = |replay_key: &str| {
        lash_core::RuntimeEffectInvocation::new(
            lash_core::EffectAddress::new(
                lash_core::ExecutionScope::turn("session", "turn"),
                replay_key,
            )
            .expect("valid test effect address"),
            lash_core::RuntimeAttribution::for_session("session"),
            "effect",
        )
    };
    let group = lash_core::RuntimeEffectGroup::try_new(
        invocation("group"),
        "scope:group:batch:1",
        vec![lash_core::RuntimeEffectEnvelope::new(
            invocation("child"),
            lash_core::RuntimeEffectCommand::Sleep {
                spec: lash_core::SleepSpec::For { duration_ms: 1 },
            },
        )],
        lash_core::GroupWakePolicy::FirstSuccess,
        lash_core::LoserPolicy::RunToCompletion,
    )
    .expect("valid durable group");
    let awaited = TraceNodeAwaited::EffectGroup {
        group_key: group.group_key().to_string(),
        position: 0,
        wake: group.wake(),
    };
    assert_eq!(awaited.kind(), TraceNodeWaitKind::EffectGroup);
    assert_eq!(
        serde_json::to_value(awaited).expect("serialize group wait"),
        serde_json::json!({
            "type": "effect_group",
            "group_key": group.group_key(),
            "position": 0,
            "wake": "first_success"
        })
    );
}

/// Register the harness process and answer the id the registrar minted for
/// it: every scope and authority the run needs names that id, so the harness
/// registers before it builds them (ADR 0107).
pub(crate) async fn register_harness_process(
    registry: &Arc<dyn lash_core::ProcessRegistry>,
    registration: &lash_core::ProcessRegistration,
    env_ref: &lash_core::ProcessExecutionEnvRef,
) -> lash_core::ProcessId {
    registry
        .register_process(
            registration
                .clone()
                .with_execution_env_ref(Some(env_ref.clone())),
        )
        .await
        .expect("register the harness process")
        .id
}

/// The session policy the harness's execution env declares: the worker
/// builds its runtime over it, so it must carry model metadata.
/// A fresh host pin's claim: what a test publishes fixtures under
/// (ADR 0113 §2.6).
pub(crate) fn host_claim() -> lash_core::ReferrerClaim {
    lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(
        lash_core::HostArtifactPin::mint(),
    ))
    .expect("a host pin is unguarded")
}

fn harness_session_policy() -> lash_core::SessionPolicy {
    lash_core::SessionPolicy {
        model: lash_core::ModelSpec::builder("mock-model")
            .context_window_tokens(200_000)
            .build()
            .expect("harness model"),
        ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
    }
}

/// A lashlang process fixture over the Restate server double: an admitted
/// process runs inside a real `LashProcessWorkflow` segment, so its effects
/// journal exactly the way a deployed segment's do. The harness owns the
/// double, which must outlive every run it admits.
pub(crate) struct DoubleProcessHarness {
    double: lash_restate_test::RestateTestBackend,
    backend: lash_core::Backend,
    wiring: lash_core::ProcessWorkWiring,
    env_ref: lash_core::ProcessExecutionEnvRef,
}

impl DoubleProcessHarness {
    /// The double's lash backend: its process registry is the process table
    /// the deployed workflow and this fixture share.
    pub(crate) fn backend(&self) -> &lash_core::Backend {
        &self.backend
    }

    /// The registry `admit` registers on and `await_terminal` watches.
    pub(crate) fn registry(&self) -> Arc<dyn lash_core::ProcessRegistry> {
        self.backend.process_registry()
    }

    /// The execution env the harness published: every registration it admits
    /// must carry this reference.
    pub(crate) fn env_ref(&self) -> &lash_core::ProcessExecutionEnvRef {
        &self.env_ref
    }

    /// The artifact store the installed worker loads its modules from: the
    /// backend's own, so a module a test publishes is the module the segment
    /// decodes.
    pub(crate) fn artifact_store(&self) -> LashlangArtifacts {
        LashlangArtifacts::new(lash_core::Backend::module_artifacts(&self.backend))
    }

    /// Serve process segments on `engine`, with `extra_factories` added to
    /// the code-protocol test plugins any process may need.
    pub(crate) fn install_lashlang_worker(
        &self,
        engine: LashlangProcessEngine,
        extra_factories: Vec<Arc<dyn lash_core::facade_support::PluginFactory>>,
    ) {
        let runtime_host = lash_core::facade_support::RuntimeHostConfig::new(
            self.backend.clone(),
            lash_core::CommitBudget::bounded(1024 * 1024, 512),
            lash_core::QueuedWorkBatchingConfig::new(1),
        )
        .with_process_engine_registration(lashlang_process_engine_registration(engine));
        let mut factories = lash_core::testing::test_code_protocol_factories();
        factories.extend(extra_factories);
        let worker = lash_core_worker::DurableProcessWorker::new(
            lash_core_worker::DurableProcessWorkerConfig::new(
                Arc::new(lash_core::facade_support::PluginHost::new(factories)),
                runtime_host,
                self.wiring.clone(),
                Arc::new(lash_core::NoSessionWork::new()),
                lash_core::testing::runtime_lease_owner(),
            )
            .with_session_policy(harness_session_policy()),
        )
        .expect("valid double process worker");
        self.double.install_process_worker(worker);
    }

    /// Register `registration` and admit it: the armed `ProcessStart`
    /// obligation's own-commit attempt delivers the row to the deployment,
    /// whose workflow runs it on the installed worker.
    pub(crate) async fn admit(
        &self,
        registration: lash_core::ProcessRegistration,
    ) -> lash_core::ProcessId {
        let process_id = self
            .registry()
            .register_process(registration)
            .await
            .expect("register the harness process")
            .id;
        self.deliver_start(&process_id).await;
        process_id
    }

    /// Deliver one registered process to the installed worker through the
    /// `ProcessStart` obligation's relay — the one production delivery path.
    pub(crate) async fn deliver_start(&self, process_id: &lash_core::ProcessId) {
        let relay = lash_core::runtime::process_start::ProcessStartRelay::new(
            self.backend
                .obligation_ledger(lash_core::store::ObligationKind::ProcessStart),
            self.registry(),
            Arc::clone(self.wiring.port()),
            self.backend.clock(),
        );
        let verdict = relay
            .deliver_start(process_id)
            .await
            .expect("deliver the harness process");
        assert_eq!(
            verdict,
            lash_core::runtime::drive::relay::RelayVerdict::Delivered,
            "the armed start delivers: {verdict:?}"
        );
    }

    /// Await the process's terminal registry record.
    pub(crate) async fn await_terminal(
        &self,
        process_id: &lash_core::ProcessId,
    ) -> lash_core::ProcessAwaitOutput {
        lash_core::NoProcessWork::for_registry(self.registry())
            .await_terminal(process_id)
            .await
            .expect("the harness process reaches a terminal record")
    }
}

/// A fresh double ready to admit a process: the harness execution env is
/// published up front, its reference stamped on every registration `admit`
/// and `register_harness_process` make.
pub(crate) async fn double_process_harness() -> DoubleProcessHarness {
    let double = lash_restate_test::backend(SEED, lash_restate_test::ServerConfig::default())
        .await
        .expect("build the Restate server double");
    let backend = double.lash_backend();
    let env_ref = lash_core::testing::publish_process_execution_env_for_testing(
        backend.process_env_store().as_ref(),
        &crate::lib_tests::host_claim(),
        &lash_core::ProcessExecutionEnvSpec::new(
            lash_core::PluginOptions::default(),
            harness_session_policy(),
        ),
    )
    .await
    .expect("the harness execution env publishes");
    let wiring = backend.process_work();
    DoubleProcessHarness {
        double,
        backend,
        wiring,
        env_ref,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn real_process_sleep_for_emits_wait_and_completion() {
    let (result, graph_store) = run_sleep_process().await;
    assert!(
        matches!(result, lash_core::ProcessAwaitOutput::Settled { ref output } if output.is_success()),
        "the sleep process settles successfully: {result:?}"
    );
    let graph = graph_store
        .graphs()
        .into_iter()
        .next()
        .expect("process graph");
    assert!(graph.nodes.iter().any(|node| matches!(
        node.observation,
        TraceLashlangNodeObservation::Completed { .. }
    )));
    assert!(graph.history.iter().any(|event| matches!(
        &event.event.payload,
        TraceLanguageExecutionPayload::NodeWaiting {
            awaited: TraceNodeAwaited::Sleep { deadline_ms: None },
            ..
        }
    )));
}

#[tokio::test(flavor = "current_thread")]
async fn real_process_signal_wait_names_the_durable_key_and_resolves() {
    struct SignalSink {
        graph: Arc<TraceLashlangGraphStore>,
        waiting: tokio::sync::mpsc::UnboundedSender<()>,
    }

    impl lash_trace::TraceSink for SignalSink {
        fn append(
            &self,
            record: &lash_trace::TraceRecord,
        ) -> Result<(), lash_trace::TraceSinkError> {
            lash_trace::TraceSink::append(&*self.graph, record)?;
            if matches!(
                &record.event,
                lash_trace::TraceEvent::LanguageExecution {
                    event: TraceLanguageExecution {
                        payload: TraceLanguageExecutionPayload::NodeWaiting {
                            awaited: TraceNodeAwaited::Signal { .. },
                            ..
                        },
                        ..
                    },
                    ..
                }
            ) {
                let _ = self.waiting.send(());
            }
            Ok(())
        }
    }

    let harness = crate::lib_tests::double_process_harness().await;
    let store = harness.artifact_store();
    let output = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: "process listen() signals { ready: any } { await wait_signal(ready); finish null }",
        program: b::module(
            vec![b::process_with_signals(
                "listen",
                Vec::new(),
                vec![b::signal("ready", lashlang::TypeExpr::Any)],
                b::block(vec![
                    b::assign("payload", b::wait_signal("ready")),
                    b::finish(b::null()),
                ]),
            )],
            Vec::new(),
        ),
        environment: &LashlangHostEnvironment::new(
            lashlang::LashlangHostCatalog::new(),
            LashlangAbilities::default(),
        ),
    })
    .expect("signal process compiles");
    store
        .publish_module_artifact(&crate::lib_tests::host_claim(), &output.artifact)
        .await
        .expect("signal process artifact publishes");
    let input = LashlangProcessInput {
        module_ref: output.module_ref.clone(),
        process_ref: output
            .artifact
            .process_ref("listen")
            .expect("listen export")
            .clone(),
        host_requirements_ref: output.host_requirements_ref.clone(),
        process_name: "listen".to_string(),
        args: serde_json::Map::new(),
    };
    let registration = lash_core::ProcessRegistration::new(
        input.to_process_input().expect("valid process input"),
        lash_core::ProcessProvenance::host(),
        lash_core::LifetimeDecision::Detached,
    )
    .with_admitted_identity(lash_core::AdmittedProcessIdentity::for_testing(
        input.process_identity(),
    ))
    .with_execution_env_ref(Some(harness.env_ref().clone()));
    let graph = Arc::new(TraceLashlangGraphStore::default());
    let (waiting, mut observed_wait) = tokio::sync::mpsc::unbounded_channel();
    let sink: Arc<dyn lash_trace::TraceSink> = Arc::new(SignalSink {
        graph: Arc::clone(&graph),
        waiting,
    });
    harness.install_lashlang_worker(
        LashlangProcessEngine::new(store, LashlangSurface::default())
            .with_execution_trace(Some(sink), lash_trace::TraceContext::default()),
        Vec::new(),
    );
    let process_id = harness.admit(registration).await;
    let effect_host = harness.backend().effect_host();
    let resolve = async {
        if observed_wait.recv().await.is_none() {
            return false;
        }
        let key = lash_core::AwaitEventResolver::await_event_key(
            effect_host.await_event_resolver(),
            &lash_core::ExecutionScope::process(process_id.clone()),
            lash_core::AwaitEventWaitIdentity::process_signal(&process_id, "ready", 1),
        )
        .await
        .expect("durable signal key");
        assert!(matches!(
            lash_core::AwaitEventResolver::resolve_await_event(
                effect_host.await_event_resolver(),
                &key,
                lash_core::Resolution::Ok(serde_json::json!({ "received": true })),
            )
            .await
            .expect("resolve signal"),
            lash_core::ResolveOutcome::Accepted
        ));
        true
    };
    let (terminal, observed) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let resolve = resolve;
        let observed = resolve.await;
        let terminal = harness.await_terminal(&process_id).await;
        (terminal, observed)
    })
    .await
    .expect("signal process and resolver finish");
    assert!(
        observed,
        "run ended before signal wait: {terminal:?}; graphs: {:?}",
        graph.graphs()
    );
    assert!(
        matches!(terminal, lash_core::ProcessAwaitOutput::Settled { ref output } if output.is_success()),
        "the signal process settles successfully: {terminal:?}"
    );
    let graph = graph.graphs().into_iter().next().expect("signal graph");
    let expected_key = lash_core::facade_support::process_signal_wait_key(&process_id, "ready", 1);
    assert!(graph.history.iter().any(|event| matches!(
        &event.event.payload,
        TraceLanguageExecutionPayload::NodeWaiting {
            awaited: TraceNodeAwaited::Signal { name, key },
            ..
        } if name == "ready" && key == &expected_key
    )));
    assert!(graph.nodes.iter().any(|node| matches!(
        node.observation,
        TraceLashlangNodeObservation::Completed { .. }
    )));
}

#[tokio::test(flavor = "current_thread")]
async fn real_process_tool_batch_wait_uses_the_dispatch_batch_id() {
    /// The fixture echo tool under the `tools` module binding the compiled
    /// program calls it by: the deployment's manifest must carry the
    /// `typescript.tool` binding the import spelled.
    struct BoundFixtureTools(lash_core::testing::FixtureTools);

    #[async_trait::async_trait]
    impl lash_core::ToolProvider for BoundFixtureTools {
        fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
            vec![
                lash_core::testing::fixture_echo_definition()
                    .with_tool_binding(ToolBinding::new(["tools"], "echo"))
                    .manifest(),
            ]
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
            self.0.resolve_contract(name)
        }

        async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
            self.0.execute(call).await
        }
    }

    let tool = lash_core::testing::fixture_echo_definition()
        .with_tool_binding(ToolBinding::new(["tools"], "echo"));
    let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![tool]);
    let resources = lashlang_resources_from_tool_catalog(&catalog).expect("tool catalog imports");
    let echo = |value: &str| {
        b::unwrap(b::receiver_call(
            b::resource(&["tools"]),
            "echo",
            vec![b::record(vec![("value", b::string(value))])],
        ))
    };
    let harness = crate::lib_tests::double_process_harness().await;
    let store = harness.artifact_store();
    let output = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: "process batch() -> null { let values = await [tools.echo({value: 'a'})?, tools.echo({value: 'b'})?]; finish null }",
        program: b::module(
            vec![b::process_returning(
                "batch",
                Vec::new(),
                lashlang::TypeExpr::Null,
                b::block(vec![
                    b::assign("values", b::await_expr(b::list(vec![echo("a"), echo("b")]))),
                    b::finish(b::null()),
                ]),
            )],
            Vec::new(),
        ),
        environment: &LashlangHostEnvironment::new(resources, LashlangAbilities::default()),
    })
    .expect("batch process compiles");
    store
        .publish_module_artifact(&crate::lib_tests::host_claim(), &output.artifact)
        .await
        .expect("batch process artifact publishes");
    let input = LashlangProcessInput {
        module_ref: output.module_ref.clone(),
        process_ref: output
            .artifact
            .process_ref("batch")
            .expect("batch export")
            .clone(),
        host_requirements_ref: output.host_requirements_ref.clone(),
        process_name: "batch".to_string(),
        args: serde_json::Map::new(),
    };
    let registration = lash_core::ProcessRegistration::new(
        input.to_process_input().expect("valid process input"),
        lash_core::ProcessProvenance::host(),
        lash_core::LifetimeDecision::Detached,
    )
    .with_admitted_identity(lash_core::AdmittedProcessIdentity::for_testing(
        input.process_identity(),
    ));
    let graph_store = Arc::new(TraceLashlangGraphStore::default());
    let sink: Arc<dyn lash_trace::TraceSink> = graph_store.clone();
    harness.install_lashlang_worker(
        LashlangProcessEngine::new(store, LashlangSurface::default())
            .with_execution_trace(Some(sink), lash_trace::TraceContext::default()),
        vec![Arc::new(lash_core::plugin::StaticPluginFactory::new(
            "fixture-tools",
            lash_core::facade_support::PluginSpec::new().with_tool_provider(Arc::new(
                BoundFixtureTools(lash_core::testing::FixtureTools::new()),
            )),
        ))],
    );
    let process_id = crate::lib_tests::register_harness_process(
        &harness.registry(),
        &registration,
        harness.env_ref(),
    )
    .await;
    harness.deliver_start(&process_id).await;
    let result = harness.await_terminal(&process_id).await;
    assert!(
        matches!(result, lash_core::ProcessAwaitOutput::Settled { ref output } if output.is_success()),
        "the batch process settles successfully: {result:?}"
    );
    let graph = graph_store
        .graphs()
        .into_iter()
        .next()
        .expect("batch process graph");
    assert!(
        graph.conflicts.is_empty(),
        "batch graph conflicts: {:?}",
        graph.conflicts
    );
    let mut waits = graph
        .history
        .iter()
        .filter_map(|event| match &event.event.payload {
            TraceLanguageExecutionPayload::NodeWaiting {
                node_id,
                occurrence,
                awaited: TraceNodeAwaited::ToolBatch { batch_id, position },
                ..
            } => Some((node_id, *occurrence, batch_id, *position)),
            _ => None,
        })
        .collect::<Vec<_>>();
    waits.sort_by_key(|(_, _, _, position)| *position);
    assert_eq!(waits.len(), 2, "both tool leaves must await one batch");
    assert_eq!(waits[0].3, 0);
    assert_eq!(waits[1].3, 1);
    assert_eq!(waits[0].2, waits[1].2);
    // The batch both leaves await is the aggregate's command (FIG-3586): its
    // issue ordinal under the process body's namespace, never a digest of
    // the leaves.
    assert!(
        waits[0].2.starts_with("lashlang:v2:") && waits[0].2.contains(":lk2:"),
        "the awaited batch is the aggregate's command key: {waits:?}"
    );
}

#[path = "lib_tests/aggregate_child.rs"]
mod aggregate_child;
mod pre_cutover_refusal;
mod second_front_end;

/// `process <name>(<params>) -> <return_ty> { finish <body> }` as a one-process
/// module. ADR 0096 retired the Lashlang front-end, so the fixtures that used
/// to be written as source state their AST instead; the source each one stood
/// for is kept as a comment at the call site.
pub(crate) fn process_module(
    name: &str,
    params: Vec<lashlang::ProcessParam>,
    return_ty: lashlang::TypeExpr,
    body: lashlang::Expr,
) -> lashlang::Program {
    b::module(
        vec![b::process_returning(
            name,
            params,
            return_ty,
            b::finish(body),
        )],
        Vec::new(),
    )
}

/// The labelled workflow witness, whose Lashlang source is spelled out at the
/// call site: labelled statements, an if/else, a `for`, a map and a
/// `while`.
fn labeled_workflow_program() -> lashlang::Program {
    b::program(vec![
        b::labelled(
            b::label("Seed value", None),
            b::assign("value", b::num(1.0)),
        ),
        b::if_else(
            b::bool_lit(true),
            b::block(vec![b::labelled(
                b::label("Selected print", None),
                b::print(b::var("value")),
            )]),
            b::block(vec![b::labelled(
                b::label("Skipped print", None),
                b::print(b::num(0.0)),
            )]),
        ),
        b::for_in(
            "item",
            b::list(vec![b::num(1.0), b::num(2.0)]),
            b::block(vec![b::labelled(
                b::label("For print", None),
                b::print(b::var("item")),
            )]),
        ),
        b::assign(
            "measured",
            b::map(
                b::list(vec![b::num(1.0), b::num(2.0)]),
                "item",
                b::builtin("len", vec![b::list(vec![b::var("item")])]),
            ),
        ),
        b::assign("count", b::num(0.0)),
        b::while_loop(
            b::binary(
                b::var("count"),
                lashlang::JavaScriptBinaryOp::Less,
                b::num(1.0),
            ),
            b::block(vec![
                b::labelled(b::label("Loop print", None), b::print(b::var("count"))),
                b::assign(
                    "count",
                    b::binary(
                        b::var("count"),
                        lashlang::JavaScriptBinaryOp::Add,
                        b::num(1.0),
                    ),
                ),
            ]),
        ),
        b::labelled(b::label("Finish value", None), b::finish(b::var("value"))),
    ])
}

/// The shipped `processes.start` contract, as a linkable catalogue.
///
/// Transcribed rather than imported: `lash-plugin-process-controls`
/// dev-depends on this crate, so depending on it back would be a package
/// cycle. The shape is the declaration's own — a `Process` definition slot, an
/// `args` object, and the one process type as the answer (ADR 0095).
fn process_start_catalog() -> lashlang::LashlangHostCatalog {
    let mut catalog = lashlang::LashlangHostCatalog::new();
    catalog
        .add_module_operation_contract(
            ["processes"],
            "Processes",
            "start",
            "tool:start_process",
            &lashlang::OperationContract::new(
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "definition": { "x-lash": { "kind": "process_unknown" } },
                        "args": { "type": "object" },
                    },
                    "required": ["definition"],
                    "additionalProperties": false
                }),
                serde_json::json!({ "x-lash": { "kind": "process_unknown" } }),
            ),
        )
        .expect("link the process start operation");
    catalog
}

/// `process worker(root: str) -> str { finish root }`
/// `process scan(root: str) -> str {
///    handle = await processes.start({ definition: worker, args: { root: root } })
///    finish root
///  }`
///
/// The admission fixture's module: post-ADR-0095 a process surface is
/// catalogue presence, so a module only *requires* it by authoring a real
/// `processes.start`.
fn scan_module_requiring_the_process_surface() -> lashlang::Program {
    b::module(
        vec![
            b::process_returning(
                "worker",
                vec![b::param("root", lashlang::TypeExpr::Str)],
                lashlang::TypeExpr::Str,
                b::finish(b::var("root")),
            ),
            b::process_returning(
                "scan",
                vec![b::param("root", lashlang::TypeExpr::Str)],
                lashlang::TypeExpr::Str,
                b::block(vec![
                    b::assign("handle", b::start("worker", vec![("root", b::var("root"))])),
                    b::finish(b::var("root")),
                ]),
            ),
        ],
        Vec::new(),
    )
}

/// `process scan(root: str) -> str { finish root }`
fn scan_module() -> lashlang::Program {
    process_module(
        "scan",
        vec![b::param("root", lashlang::TypeExpr::Str)],
        lashlang::TypeExpr::Str,
        b::var("root"),
    )
}

/// `process handler(<first>: <first_ty>, <second>: str) -> bool { finish true }`
fn handler_module(first: &str, first_ty: lashlang::TypeExpr, second: &str) -> lashlang::Program {
    process_module(
        "handler",
        vec![
            b::param(first, first_ty),
            b::param(second, lashlang::TypeExpr::Str),
        ],
        lashlang::TypeExpr::Bool,
        b::bool_lit(true),
    )
}

struct EveryNEffectsController(usize);

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for EveryNEffectsController {
    /// A test double that mints keys under no durable authority.
    fn await_event_authority_binding_id(&self) -> Option<String> {
        None
    }
}

#[async_trait::async_trait]
impl lash_core::RuntimeEffectController for EveryNEffectsController {
    fn wants_segment_boundary(
        &self,
        progress: &lash_core::SegmentProgress,
    ) -> Option<lash_core::BoundaryReason> {
        progress
            .effects_executed
            .is_multiple_of(self.0 as u64)
            .then_some(lash_core::BoundaryReason::JournalBudget)
    }

    async fn execute_effect(
        &self,
        _envelope: lash_core::RuntimeEffectEnvelope,
        _local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError> {
        unreachable!("predicate test does not execute effects")
    }

    async fn open_effect_group(
        &self,
        _group: lash_core::RuntimeEffectGroup,
    ) -> Result<lash_core::EffectGroupHandle, lash_core::RuntimeEffectControllerError> {
        Err(lash_core::effect_groups_unsupported(
            "EveryNEffectsController",
        ))
    }

    async fn await_next_settlement(
        &self,
        _handle: &mut lash_core::EffectGroupHandle,
        _cancel: lash_core::TurnCancelWait,
    ) -> Result<lash_core::GroupSettlement, lash_core::RuntimeEffectControllerError> {
        Err(lash_core::effect_groups_unsupported(
            "EveryNEffectsController",
        ))
    }

    async fn close_effect_group(
        &self,
        _handle: lash_core::EffectGroupHandle,
        _disposition: lash_core::LoserPolicy,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        Err(lash_core::effect_groups_unsupported(
            "EveryNEffectsController",
        ))
    }
}

#[test]
fn every_n_controller_requests_boundaries_and_the_trait_default_does_not() {
    let progress = lash_core::SegmentProgress {
        effects_executed: 2,
        journaled_bytes_estimate: None,
    };
    assert_eq!(
        lash_core::RuntimeEffectController::wants_segment_boundary(
            &EveryNEffectsController(2),
            &progress,
        ),
        Some(lash_core::BoundaryReason::JournalBudget)
    );
    assert_eq!(
        lash_core::RuntimeEffectController::wants_segment_boundary(
            &lash_core::testing::UnavailableEffectController,
            &progress,
        ),
        None
    );
}

#[tokio::test(flavor = "current_thread")]
async fn foreground_trace_skeleton_is_derived_from_the_workflow_graph() {
    let source = r#"
        @label(title: "Seed value")
        value = 1
        if true {
          @label(title: "Selected print")
          print value
        } else {
          @label(title: "Skipped print")
          print 0
        }
        for item in [1, 2] {
          @label(title: "For print")
          print item
        }
        measured = [len([item]) for item in [1, 2]]
        count = 0
        while count < 1 {
          @label(title: "Loop print")
          print count
          count = count + 1
        }
        @label(title: "Finish value")
        finish value
    "#;
    let environment = LashlangHostEnvironment::new(
        lashlang::LashlangHostCatalog::new(),
        LashlangAbilities::all(),
    )
    .with_language_features(lashlang::LashlangLanguageFeatures::default().with_label_annotations());
    let program = labeled_workflow_program();
    let output = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source,
        program: program.clone(),
        environment: &environment,
    })
    .expect("labeled workflow compiles");
    // The projection is language-neutral and lives beside the IR (ADR 0100
    // R8): this witness is direct IR — `@label` and a list comprehension have
    // no TypeScript form — and projects with no dialect in the graph.
    let graph = lashlang::workflow_graph_from_program(&program, &lashlang::NoStatementText);
    let trace_graph =
        lashlang::workflow_graph_from_artifact(&output.artifact, &lashlang::NoStatementText);
    let trace_map = trace_lashlang_main_map(&output.artifact);
    assert_eq!(
        Some(output.artifact.source_identity()),
        trace_graph.source_identity,
        "the trace integration must retain the projector's source identity"
    );

    let container_kinds = graph
        .nodes()
        .filter_map(|node| match &node.kind {
            lashlang::WorkflowNodeKind::Container(lashlang::WorkflowContainer::If { .. }) => {
                Some("if")
            }
            lashlang::WorkflowNodeKind::Container(lashlang::WorkflowContainer::For { .. }) => {
                Some("for")
            }
            lashlang::WorkflowNodeKind::Container(lashlang::WorkflowContainer::While {
                ..
            }) => Some("while"),
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        container_kinds,
        std::collections::BTreeSet::from(["for", "if", "while"]),
        "the equality probe must cover every workflow container kind"
    );

    let expected_nodes = graph
        .nodes()
        .filter(|node| !node.execution_sites.is_empty())
        .map(|node| node.id.to_string())
        .collect::<std::collections::BTreeSet<_>>();
    let actual_nodes = trace_map
        .nodes
        .iter()
        .map(|node| node.id.clone())
        .collect::<std::collections::BTreeSet<_>>();

    assert!(!expected_nodes.is_empty());
    assert_eq!(actual_nodes, expected_nodes);
    assert!(
        trace_map
            .nodes
            .iter()
            .any(|node| node.label == "Selected print")
    );
    assert!(
        trace_map
            .nodes
            .iter()
            .any(|node| node.label == "Loop print")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn process_trace_map_is_obtainable_without_an_execution_started_event() {
    let environment = LashlangHostEnvironment::new(
        lashlang::LashlangHostCatalog::new(),
        LashlangAbilities::default(),
    );
    let output = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: r#"process scan(root: str) -> str { finish root }"#,
        program: scan_module(),
        environment: &environment,
    })
    .expect("process module compiles");
    let store = crate::lib_tests::memory_artifact_store().await;
    store
        .publish_module_artifact(&crate::lib_tests::host_claim(), &output.artifact)
        .await
        .expect("artifact publishes");
    let input = LashlangProcessInput {
        module_ref: output.module_ref.clone(),
        process_ref: output
            .artifact
            .process_ref("scan")
            .expect("scan export")
            .clone(),
        host_requirements_ref: output.host_requirements_ref.clone(),
        process_name: "scan".to_string(),
        args: serde_json::Map::new(),
    };

    let direct = trace_lashlang_process_map(&output.artifact, "scan").expect("direct map");
    let snapshot = trace_lashlang_process_map_snapshot(&store, &input)
        .await
        .expect("stored map snapshot");
    assert_eq!(snapshot, direct);
    assert!(!snapshot.nodes.is_empty());

    let mut missing_process = input.clone();
    missing_process.process_name = "missing".to_string();
    assert!(matches!(
        trace_lashlang_process_map_snapshot(&store, &missing_process).await,
        Err(TraceLanguageExecutionMapError::ProcessMissing { process_name, .. })
            if process_name == "missing"
    ));

    let missing_hash = lashlang::ContentHash::new("missing-trace-map-artifact");
    let mut missing_artifact = input;
    missing_artifact.module_ref = lashlang::ModuleRef::new(&missing_hash);
    assert!(matches!(
        trace_lashlang_process_map_snapshot(&store, &missing_artifact).await,
        Err(TraceLanguageExecutionMapError::ArtifactMissing(_))
    ));
}

#[test]
fn process_input_serializes_as_generic_engine_payload() {
    let hash = lashlang::ContentHash::new("abc123");
    let input = LashlangProcessInput {
        module_ref: lashlang::ModuleRef::new(&hash),
        process_ref: lashlang::ProcessRef::new(hash.clone(), 7),
        host_requirements_ref: lashlang::HostRequirementsRef::new(&hash),
        process_name: "main".to_string(),
        args: serde_json::Map::from_iter([("prompt".to_string(), serde_json::json!("go"))]),
    };

    let process_input = input
        .clone()
        .into_process_input()
        .expect("lashlang process input serializes");

    let lash_core::ProcessInput::Engine { kind, payload } = process_input else {
        panic!("lashlang runtime must use the generic engine process input");
    };
    assert_eq!(kind, LASHLANG_ENGINE_KIND);
    assert_eq!(
        LashlangProcessInput::from_payload(payload)
            .expect("engine payload decodes")
            .process_name,
        input.process_name
    );
}

#[test]
fn process_input_remote_helpers_use_generic_engine_and_identity() {
    let hash = lashlang::ContentHash::new("abc123");
    let input = LashlangProcessInput {
        module_ref: lashlang::ModuleRef::new(&hash),
        process_ref: lashlang::ProcessRef::new(hash.clone(), 7),
        host_requirements_ref: lashlang::HostRequirementsRef::new(&hash),
        process_name: "main".to_string(),
        args: serde_json::Map::from_iter([("prompt".to_string(), serde_json::json!("go"))]),
    };

    let remote_input: lash_remote_protocol::RemoteProcessInput = input
        .clone()
        .try_into()
        .expect("lashlang process input serializes remotely");
    let lash_remote_protocol::RemoteProcessInput::Engine { kind, payload } = remote_input else {
        panic!("lashlang runtime must use the generic remote engine process input");
    };
    assert_eq!(kind, LASHLANG_ENGINE_KIND);
    assert_eq!(
        LashlangProcessInput::from_payload(payload)
            .expect("remote payload decodes")
            .process_name,
        "main"
    );

    let identity = input.process_identity();
    assert_eq!(identity.kind, LASHLANG_ENGINE_KIND);
    assert_eq!(identity.label.as_deref(), Some("main"));
    assert_eq!(input.remote_identity().label.as_deref(), Some("main"));

    let draft = input
        .remote_trigger_subscription_draft(
            "button-main",
            "process-env:v6:blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .parse()
                .expect("canonical env ref"),
            "ui.button.pressed",
            "source-key",
        )
        .expect("remote trigger draft");
    draft.validate().expect("draft validates");
    assert_eq!(draft.target_label.as_deref(), Some("main"));
    assert_eq!(draft.target_identity.label.as_deref(), Some("main"));
}

#[test]
fn missing_tool_binding_is_not_fabricated() {
    let tool = lash_core::ToolDefinition::raw(
        "tool:test/read_file",
        "read_file",
        "read a file",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::Value::Null,
    );

    let err = required_tool_typescript_executable(&tool.manifest)
        .expect_err("missing explicit binding should fail");

    assert!(matches!(
        err,
        ToolBindingError::MissingBinding {
            tool,
            binding_key: TYPESCRIPT_TOOL_BINDING_KEY,
        } if tool == "read_file"
    ));
}

#[test]
fn explicit_tool_binding_attaches_exactly_one_manifest_key() {
    let tool = lash_core::ToolDefinition::raw(
        "tool:test/read_file",
        "read_file",
        "read a file",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::Value::Null,
    )
    .with_tool_binding(
        ToolBinding::new(["fs"], "read")
            .with_authority_type("Filesystem")
            .with_aliases(["cat"]),
    );

    let binding =
        required_tool_typescript_executable(&tool.manifest).expect("explicit binding resolves");

    assert_eq!(binding.module_path, vec!["fs"]);
    assert_eq!(binding.operation, "read");
    assert_eq!(binding.authority_type, "Filesystem");
    assert_eq!(binding.aliases, vec!["cat"]);
    assert_eq!(
        tool.manifest.bindings.keys().collect::<Vec<_>>(),
        vec![TYPESCRIPT_TOOL_BINDING_KEY],
        "one tool binding lives under one manifest key"
    );
}

#[test]
fn legacy_two_key_manifest_still_reads_and_rewrites_to_one_key() {
    // Manifests written before the keys were unified carried the same payload
    // under both `lashlang.tool` and `typescript.tool`. The reader follows the
    // canonical key; rewriting collapses the map to it.
    let legacy_bindings = serde_json::json!({
        "lashlang.tool": {
            "module_path": ["workspace", "files"],
            "operation": "write",
            "authority_type": "Filesystem",
            "aliases": ["write_text"]
        },
        "typescript.tool": {
            "module_path": ["workspace", "files"],
            "operation": "write",
            "authority_type": "Filesystem",
            "aliases": ["write_text"]
        }
    });
    let mut legacy_manifest = lash_core::ToolDefinition::raw(
        "tool:test/write_file",
        "write_file",
        "write a file",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::Value::Null,
    )
    .manifest;
    legacy_manifest.bindings =
        serde_json::from_value(legacy_bindings.clone()).expect("legacy bindings decode");

    let binding = legacy_manifest
        .tool_binding()
        .expect("legacy binding payload decodes")
        .expect("legacy binding is present");
    let rewritten = lash_core::ToolDefinition::raw(
        "tool:test/write_file",
        "write_file",
        "write a file",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::Value::Null,
    )
    .with_tool_binding(binding);

    assert_eq!(
        serde_json::to_value(&rewritten.manifest.bindings).expect("rewritten bindings encode"),
        serde_json::json!({
            "typescript.tool": legacy_bindings["typescript.tool"]
        })
    );
}

#[test]
fn tool_catalog_imports_declared_static_schema_types() {
    let tool = lash_core::ToolDefinition::raw(
        "tool:test/read_file",
        "read_file",
        "read a file",
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "retries": { "type": "integer" }
            },
            "required": ["path"],
            "additionalProperties": false
        }),
        serde_json::json!({
            "type": "array",
            "items": { "type": ["string", "null"] }
        }),
    )
    .with_tool_binding(ToolBinding::new(["fs"], "read").with_authority_type("Filesystem"));
    let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![tool]);

    let resources = lashlang_resources_from_tool_catalog(&catalog).expect("tool schemas import");
    let operation = resources
        .resolve_operation("Filesystem", "read")
        .expect("operation is registered");

    assert_eq!(
        operation.input_ty,
        lashlang::TypeExpr::Object(vec![
            lashlang::TypeField {
                name: "path".into(),
                ty: lashlang::TypeExpr::Str,
                optional: false,
            },
            lashlang::TypeField {
                name: "retries".into(),
                ty: lashlang::TypeExpr::Int,
                optional: true,
            },
        ])
    );
    assert_eq!(
        operation.output_ty,
        lashlang::TypeExpr::List(Box::new(lashlang::TypeExpr::union(vec![
            lashlang::TypeExpr::Str,
            lashlang::TypeExpr::Null,
        ])))
    );
}

/// A tool contract can say `process` and `handle`, and must say them right.
///
/// Before `x-lash` a tool could not describe a process at all: both sides of
/// the boundary erased it. Now that it can, a contract that says it wrong is
/// refused outright rather than widened to `Any`, because the keyword is only
/// ever written on purpose.
#[test]
fn tool_contracts_carry_lash_types_and_refuse_malformed_ones() {
    let tool = lash_core::ToolDefinition::raw(
        "tool:test/spawn",
        "spawn",
        "spawn a process",
        serde_json::json!({
            "type": "object",
            "properties": { "target": { "x-lash": { "kind": "process_unknown" } } },
            "required": ["target"],
            "additionalProperties": false
        }),
        serde_json::json!({ "x-lash": { "kind": "handle", "payload": { "type": "string" } } }),
    )
    .with_tool_binding(ToolBinding::new(["spawner"], "spawn").with_authority_type("Spawner"));
    let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![tool]);
    let resources = lashlang_resources_from_tool_catalog(&catalog).expect("tool schemas import");
    let operation = resources
        .resolve_operation("Spawner", "spawn")
        .expect("operation is registered");
    assert_eq!(
        operation.input_ty,
        lashlang::TypeExpr::Object(vec![lashlang::TypeField {
            name: "target".into(),
            ty: lashlang::TypeExpr::Process(lashlang::ProcessType::unknown()),
            optional: false,
        }])
    );
    assert_eq!(
        operation.output_ty,
        lashlang::TypeExpr::TriggerHandle(Box::new(lashlang::TypeExpr::Str))
    );

    let malformed = lash_core::ToolDefinition::raw(
        "tool:test/broken",
        "broken",
        "a contract that says a lash type wrong",
        serde_json::json!({
            "type": "object",
            "properties": { "target": { "x-lash": { "kind": "process" } } },
            "required": ["target"],
            "additionalProperties": false
        }),
        serde_json::json!({}),
    )
    .with_tool_binding(ToolBinding::new(["broken"], "run").with_authority_type("Broken"));
    let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![malformed]);
    let error = lashlang_resources_from_tool_catalog(&catalog)
        .expect_err("a malformed lash type refuses the whole contract");
    assert!(
        error.to_string().contains("x-lash"),
        "the diagnostic must name the keyword that is wrong: {error}"
    );
}

#[test]
fn from_input_schema_tool_imports_contract_marker_and_default() {
    let tool = lash_core::ToolDefinition::raw(
        "tool:test/generate",
        "generate",
        "generate typed output",
        serde_json::json!({
            "type": "object",
            "properties": { "schema": {} },
            "required": ["schema"],
            "additionalProperties": false
        }),
        serde_json::json!({ "type": "string" }),
    )
    .with_output_from_input_schema("schema", Some(serde_json::json!({ "type": "string" })))
    .with_tool_binding(ToolBinding::new(["generate"], "run").with_authority_type("Generator"));
    let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![tool]);

    let resources = lashlang_resources_from_tool_catalog(&catalog).expect("tool schemas import");
    let operation = resources
        .resolve_operation("Generator", "run")
        .expect("operation is registered");

    assert_eq!(
        operation.input_ty,
        lashlang::TypeExpr::Object(vec![lashlang::TypeField {
            name: "schema".into(),
            ty: lashlang::TypeExpr::Any,
            optional: false,
        }])
    );
    assert_eq!(operation.output_ty, lashlang::TypeExpr::Any);
    assert_eq!(
        operation.output_from_input,
        Some(lashlang::OutputFromInputBinding {
            input_field: "schema".to_string(),
            default_schema: Some(lashlang::TypeExpr::Str),
        })
    );
}

#[test]
fn representable_type_schema_subset_round_trips() {
    let types = [
        lashlang::TypeExpr::Any,
        lashlang::TypeExpr::Str,
        lashlang::TypeExpr::Int,
        lashlang::TypeExpr::Float,
        lashlang::TypeExpr::Bool,
        lashlang::TypeExpr::Null,
        lashlang::TypeExpr::Enum(vec!["fast".into(), "safe".into()]),
        lashlang::TypeExpr::List(Box::new(lashlang::TypeExpr::Str)),
        lashlang::TypeExpr::union(vec![lashlang::TypeExpr::Str, lashlang::TypeExpr::Null]),
    ];

    for expected in types {
        let schema = lashlang_type_expr_schema(&expected);
        assert_eq!(
            lashlang::json_schema_to_type_expr(&schema).expect("an exported schema imports"),
            expected
        );
    }
}

#[test]
fn dotted_operation_names_are_rejected() {
    let tool = lash_core::ToolDefinition::raw(
        "tool:test/update_plan",
        "update_plan",
        "update a plan",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::Value::Null,
    )
    .with_tool_binding(ToolBinding::new(["tools"], "update.plan"));

    let err = required_tool_typescript_executable(&tool.manifest)
        .expect_err("dotted operation cannot compile as one Lashlang operation");

    assert!(matches!(
        err,
        ToolBindingError::InvalidIdentifier {
            tool,
            part: "operation name",
            value,
        } if tool == "update_plan" && value == "update.plan"
    ));
}

#[test]
fn empty_operation_names_render_as_empty_invalid_identifiers() {
    let tool = lash_core::ToolDefinition::raw(
        "tool:test/empty_operation",
        "empty_operation",
        "an operation with an empty name",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::Value::Null,
    )
    .with_tool_binding(ToolBinding::new(["tools"], ""));

    let err = required_tool_typescript_executable(&tool.manifest)
        .expect_err("an empty operation name cannot compile as a Lashlang operation");

    assert_eq!(
        err.to_string(),
        "tool `empty_operation` has invalid tool-binding operation name `<empty>`"
    );
}

#[test]
fn manifest_tool_binding_accessor_reports_absent_valid_and_malformed() {
    let mut manifest = lash_core::ToolDefinition::raw(
        "tool:test/read_file",
        "read_file",
        "read a file",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::Value::Null,
    )
    .manifest;
    assert_eq!(manifest.tool_binding().expect("absent binding"), None);

    manifest.bindings.insert(
        TYPESCRIPT_TOOL_BINDING_KEY.to_string(),
        serde_json::json!({
            "module_path": ["fs"],
            "operation": "read"
        }),
    );
    let binding = manifest
        .tool_binding()
        .expect("valid binding")
        .expect("present binding");
    assert_eq!(binding.module_path, vec!["fs"]);
    assert_eq!(binding.operation.as_deref(), Some("read"));

    manifest.bindings.insert(
        TYPESCRIPT_TOOL_BINDING_KEY.to_string(),
        serde_json::json!({ "module_path": "fs" }),
    );
    assert!(manifest.tool_binding().is_err());
}

#[test]
fn remote_grant_tool_binding_accessor_reports_absent_valid_and_malformed() {
    let grant = remote_tool_grant("read_file");
    assert_eq!(grant.tool_binding().expect("absent binding"), None);

    let grant = grant.with_tool_binding(ToolBinding::new(["fs"], "read"));
    let binding = grant
        .tool_binding()
        .expect("valid binding")
        .expect("present binding");
    assert_eq!(binding.module_path, vec!["fs"]);
    assert_eq!(binding.operation.as_deref(), Some("read"));

    let mut malformed = grant;
    malformed.bindings.insert(
        TYPESCRIPT_TOOL_BINDING_KEY.to_string(),
        serde_json::json!({ "module_path": "fs" }),
    );
    assert!(malformed.tool_binding().is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn prepared_start_replays_same_start_key_without_duplicate_child_identity() {
    let store = crate::lib_tests::memory_artifact_store().await;
    let environment = LashlangHostEnvironment::new(
        lashlang::LashlangHostCatalog::new(),
        LashlangAbilities::default(),
    );
    let output = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: r#"process scan(root: str) -> str { finish root }"#,
        program: scan_module(),
        environment: &environment,
    })
    .expect("module compiles");
    store
        .publish_module_artifact(&crate::lib_tests::host_claim(), &output.artifact)
        .await
        .expect("module publishes");
    let artifact_store: LashlangArtifacts = store;
    let site = test_start_site("child_process:scan", 1);

    let first = prepare_lashlang_process_start(
        artifact_store.clone(),
        Some(lash_core::StartKey::for_host(
            lash_core::StartKeyOwner::HOST,
            "parent:root",
        )),
        test_process_start(&output, site.clone(), "."),
        lash_core::ProcessOriginator::host(),
        lash_core::LifetimeDecision::Detached,
    )
    .await
    .expect("first start prepares");
    let replayed = prepare_lashlang_process_start(
        artifact_store.clone(),
        Some(lash_core::StartKey::for_host(
            lash_core::StartKeyOwner::HOST,
            "parent:root",
        )),
        test_process_start(&output, site.clone(), "."),
        lash_core::ProcessOriginator::host(),
        lash_core::LifetimeDecision::Detached,
    )
    .await
    .expect("replayed start prepares");
    let sibling = prepare_lashlang_process_start(
        artifact_store.clone(),
        Some(lash_core::StartKey::for_host(
            lash_core::StartKeyOwner::HOST,
            "parent:root:2",
        )),
        test_process_start(&output, test_start_site("child_process:scan", 2), "."),
        lash_core::ProcessOriginator::host(),
        lash_core::LifetimeDecision::Detached,
    )
    .await
    .expect("sibling start prepares");

    assert_eq!(first.request.start_key, replayed.request.start_key);
    assert_eq!(first.request.identity, replayed.request.identity);
    assert_ne!(first.request.start_key, sibling.request.start_key);
}

#[tokio::test(flavor = "current_thread")]
async fn process_admission_four_shape_table_preserves_codes_and_prepare_omission() {
    let store = crate::lib_tests::memory_artifact_store().await;
    let required_environment =
        LashlangHostEnvironment::new(process_start_catalog(), LashlangAbilities::default());
    let output = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: r#"process worker(root: str) -> str { finish root }
process scan(root: str) -> str {
  handle = await processes.start({ definition: worker, args: { root: root } })
  finish root
}"#,
        program: scan_module_requiring_the_process_surface(),
        environment: &required_environment,
    })
    .expect("module compiles");
    store
        .publish_module_artifact(&crate::lib_tests::host_claim(), &output.artifact)
        .await
        .expect("module publishes");
    let start = test_process_start(&output, test_start_site("child_process:scan", 1), ".");
    let input = LashlangProcessInput {
        module_ref: start.module_ref.clone(),
        process_ref: start.process_ref.clone(),
        host_requirements_ref: start.host_requirements_ref.clone(),
        process_name: start.process_name.clone(),
        args: serde_json::Map::new(),
    };

    let mut requirements_mismatch = input.clone();
    requirements_mismatch.host_requirements_ref =
        lashlang::HostRequirementsRef::new(&lashlang::ContentHash::new("mismatch"));
    let mut process_mismatch = input.clone();
    process_mismatch.process_ref =
        lashlang::ProcessRef::new(lashlang::ContentHash::new("wrong-process"), 0);
    for (mut bad_start, expected_code, expected_message) in [
        (
            start.clone(),
            LashlangProcessFailureCode::ProcessHostRequirementsMismatch,
            "requested surface",
        ),
        (
            start.clone(),
            LashlangProcessFailureCode::ProcessRefMismatch,
            "does not export process",
        ),
    ] {
        if expected_code == LashlangProcessFailureCode::ProcessHostRequirementsMismatch {
            bad_start.host_requirements_ref = requirements_mismatch.host_requirements_ref.clone();
        } else {
            bad_start.process_ref = process_mismatch.process_ref.clone();
        }
        let error = prepare_lashlang_process_start(
            store.clone(),
            Some(lash_core::StartKey::for_host(
                lash_core::StartKeyOwner::HOST,
                "parent:four-shape",
            )),
            bad_start,
            lash_core::ProcessOriginator::host(),
            lash_core::LifetimeDecision::Detached,
        )
        .await
        .expect_err("the real prepare entry point must reject immutable mismatches");
        let LashlangRuntimeError::ProcessAdmission(refusal) = error else {
            panic!("prepare must preserve the typed admission refusal: {error:?}")
        };
        assert_eq!(refusal.failure_code(), expected_code);
        assert!(refusal.to_string().contains(expected_message), "{refusal}");
    }

    let mut malformed_tool = lash_core::ToolDefinition::raw(
        "four-shape-invalid-host",
        "four_shape_invalid_host",
        "malformed Lashlang binding fixture",
        serde_json::json!({"type": "object"}),
        serde_json::Value::Null,
    );
    malformed_tool.manifest.bindings.insert(
        TYPESCRIPT_TOOL_BINDING_KEY.to_string(),
        serde_json::json!({"not": "a tool binding"}),
    );
    let invalid_host_catalog = Arc::new(lash_core::ToolCatalog::from_tool_definitions(vec![
        malformed_tool,
    ]));
    assert!(
        LashlangSurface::default()
            .for_process_registry(true)
            .host_environment(&invalid_host_catalog)
            .is_err(),
        "the invalid-host fixture must genuinely fail catalog conversion"
    );
    let incompatible_host_catalog = Arc::new(lash_core::ToolCatalog::default());
    let incompatible_environment = LashlangSurface::default()
        .for_process_registry(false)
        .host_environment(&incompatible_host_catalog)
        .expect("the incompatible-host fixture must itself be valid");
    assert!(
        lashlang_host_environment_satisfies_requirements(
            output.artifact.host_requirements(),
            &incompatible_environment,
        )
        .is_err(),
        "the valid fixture must genuinely lack the artifact's required process surface"
    );

    prepare_lashlang_process_start(
        store.clone(),
        Some(lash_core::StartKey::for_host(
            lash_core::StartKeyOwner::HOST,
            "parent:four-shape",
        )),
        start,
        lash_core::ProcessOriginator::host(),
        lash_core::LifetimeDecision::Detached,
    )
    .await
    .expect("the real prepare entry point explicitly omits both live-host fixtures");

    let artifact_store: LashlangArtifacts = store;
    let cases = [
        (
            requirements_mismatch,
            Arc::new(lash_core::ToolCatalog::default()),
            false,
            LashlangProcessFailureCode::ProcessHostRequirementsMismatch,
            "requested surface",
        ),
        (
            process_mismatch,
            Arc::new(lash_core::ToolCatalog::default()),
            false,
            LashlangProcessFailureCode::ProcessRefMismatch,
            "does not export process",
        ),
        (
            input.clone(),
            invalid_host_catalog,
            true,
            LashlangProcessFailureCode::ProcessHostEnvironmentInvalid,
            "missing an explicit tool-binding module path",
        ),
        (
            input,
            incompatible_host_catalog,
            false,
            LashlangProcessFailureCode::ProcessHostEnvironmentIncompatible,
            "incompatible with this host surface",
        ),
    ];
    for (input, catalog, registry_available, expected_code, expected_message) in cases {
        let double = lash_restate_test::backend(SEED, lash_restate_test::ServerConfig::default())
            .await
            .expect("build the Restate server double");
        let payload = serde_json::to_value(&input).expect("valid process payload");
        let registration = lash_core::ProcessRegistration::new(
            input.to_process_input().expect("valid engine input"),
            lash_core::ProcessProvenance::host(),
            lash_core::LifetimeDecision::Detached,
        )
        .with_admitted_identity(lash_core::AdmittedProcessIdentity::for_testing(
            input.process_identity(),
        ));
        let context = lash_core::testing::process_engine_run_context_for_validation(
            &double.lash_backend(),
            registration,
            catalog,
            registry_available,
        );
        let run_outcome = Box::pin(crate::process::run_lashlang_process(
            LashlangProcessEngine::new(artifact_store.clone(), LashlangSurface::default()),
            context,
            payload,
        ))
        .await
        .expect("admission mismatches are durable process outcomes, not infra errors");
        let run_output = run_outcome
            .terminal_output()
            .expect("admission refusal must be terminal");
        let lash_core::ProcessAwaitOutput::Settled { output } = run_output else {
            panic!("admission refusal must be a settled durable process failure")
        };
        let lash_core::ToolCallOutcome::Failure(failure) = &output.outcome else {
            panic!("admission refusal must map to a durable failure")
        };
        assert_eq!(failure.code, expected_code.as_str());
        assert!(failure.message.contains(expected_message), "{failure:?}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn prepared_start_checks_indirect_process_identity_against_named_signature() {
    let store = crate::lib_tests::memory_artifact_store().await;
    let environment = LashlangHostEnvironment::new(
        lashlang::LashlangHostCatalog::new(),
        LashlangAbilities::default(),
    );
    let matching = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: "process handler(event: str, other: str) -> bool { finish true }",
        program: handler_module("event", lashlang::TypeExpr::Str, "other"),
        environment: &environment,
    })
    .expect("matching handler compiles");
    let mismatching = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: "process handler(payload: str, other: str) -> bool { finish true }",
        program: handler_module("payload", lashlang::TypeExpr::Str, "other"),
        environment: &environment,
    })
    .expect("mismatching handler compiles");
    let wrong_type = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: "process handler(event: int, other: str) -> bool { finish true }",
        program: handler_module("event", lashlang::TypeExpr::Int, "other"),
        environment: &environment,
    })
    .expect("wrong-type handler compiles");
    let wrong_order = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: "process handler(other: str, event: str) -> bool { finish true }",
        program: handler_module("other", lashlang::TypeExpr::Str, "event"),
        environment: &environment,
    })
    .expect("wrong-order handler compiles");
    let receiver = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: "process install(envelope: { handler: Process<(event: str, other: str), bool> }) -> bool { finish true }",
        program: b::module(
            vec![b::process_returning(
                "install",
                vec![b::param(
                    "envelope",
                    lashlang::TypeExpr::Object(vec![b::type_field(
                        "handler",
                        b::process_type(
                            vec![
                                b::param("event", lashlang::TypeExpr::Str),
                                b::param("other", lashlang::TypeExpr::Str),
                            ],
                            lashlang::TypeExpr::Bool,
                        ),
                        false,
                    )]),
                )],
                lashlang::TypeExpr::Bool,
                b::finish(b::bool_lit(true)),
            )],
            Vec::new(),
        ),
        environment: &environment,
    })
    .expect("receiver compiles");
    let owner = crate::lib_tests::host_claim();
    for artifact in [
        &matching.artifact,
        &mismatching.artifact,
        &wrong_type.artifact,
        &wrong_order.artifact,
        &receiver.artifact,
    ] {
        store
            .publish_module_artifact(&owner, artifact)
            .await
            .expect("module publishes");
    }
    let artifact_store: LashlangArtifacts = store.clone();

    let start_with = |definition: lashlang::ProcessDefinitionIdentity| {
        let mut envelope = lashlang::Record::new();
        envelope.insert(
            "handler".to_string(),
            lashlang::from_json(definition.to_process_value()),
        );
        let mut args = lashlang::Record::new();
        args.insert(
            "envelope".to_string(),
            lashlang::Value::Record(Arc::new(envelope)),
        );
        lashlang::ProcessStart {
            module_ref: receiver.module_ref.clone(),
            process_ref: receiver.artifact.process_ref("install").unwrap().clone(),
            host_requirements_ref: receiver.host_requirements_ref.clone(),
            start_site: test_start_site("child_process:install", 1),
            process_name: "install".to_string(),
            args,
        }
    };

    prepare_lashlang_process_start(
        artifact_store.clone(),
        Some(lash_core::StartKey::for_host(
            lash_core::StartKeyOwner::HOST,
            "parent:root",
        )),
        start_with(
            lashlang::ProcessDefinitionIdentity::from_artifact_export(
                &matching.artifact,
                "handler",
            )
            .unwrap(),
        ),
        lash_core::ProcessOriginator::host(),
        lash_core::LifetimeDecision::Detached,
    )
    .await
    .expect("matching immutable signature passes");

    let error = prepare_lashlang_process_start(
        artifact_store.clone(),
        Some(lash_core::StartKey::for_host(
            lash_core::StartKeyOwner::HOST,
            "parent:root",
        )),
        start_with(
            lashlang::ProcessDefinitionIdentity::from_artifact_export(
                &mismatching.artifact,
                "handler",
            )
            .unwrap(),
        ),
        lash_core::ProcessOriginator::host(),
        lash_core::LifetimeDecision::Detached,
    )
    .await
    .expect_err("different outer parameter name must fail before registration");
    assert!(matches!(
        error,
        LashlangRuntimeError::InvalidProcessArgument { ref path, .. }
            if path == "envelope.handler"
    ));
    assert!(error.to_string().contains("payload"), "{error}");

    for (description, definition) in [
        (
            "different parameter type",
            lashlang::ProcessDefinitionIdentity::from_artifact_export(
                &wrong_type.artifact,
                "handler",
            )
            .unwrap(),
        ),
        (
            "different parameter order at the same arity",
            lashlang::ProcessDefinitionIdentity::from_artifact_export(
                &wrong_order.artifact,
                "handler",
            )
            .unwrap(),
        ),
    ] {
        let error = prepare_lashlang_process_start(
            artifact_store.clone(),
            Some(lash_core::StartKey::for_host(
                lash_core::StartKeyOwner::HOST,
                "parent:root",
            )),
            start_with(definition),
            lash_core::ProcessOriginator::host(),
            lash_core::LifetimeDecision::Detached,
        )
        .await
        .expect_err(description);
        assert!(matches!(
            error,
            LashlangRuntimeError::InvalidProcessArgument { ref path, .. }
                if path == "envelope.handler"
        ));
    }

    let valid =
        lashlang::ProcessDefinitionIdentity::from_artifact_export(&matching.artifact, "handler")
            .unwrap();
    let wrong_ref = lashlang::ProcessDefinitionIdentity::new(
        valid.module_ref,
        valid.host_requirements_ref,
        lashlang::ProcessRef::new(lashlang::ContentHash::new("wrong-process"), 0),
        valid.process_name,
    );
    let error = prepare_lashlang_process_start(
        artifact_store.clone(),
        Some(lash_core::StartKey::for_host(
            lash_core::StartKeyOwner::HOST,
            "parent:root",
        )),
        start_with(wrong_ref),
        lash_core::ProcessOriginator::host(),
        lash_core::LifetimeDecision::Detached,
    )
    .await
    .expect_err("identity with a different process ref must fail");
    assert!(matches!(
        error,
        LashlangRuntimeError::InvalidProcessArgument { ref path, .. }
            if path == "envelope.handler"
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn process_signature_union_accepts_a_later_matching_nonprocess_arm() {
    let store = crate::lib_tests::memory_artifact_store().await;
    let environment = LashlangHostEnvironment::new(
        lashlang::LashlangHostCatalog::new(),
        LashlangAbilities::default(),
    );
    let receiver = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: "process install(handler: Process<(event: str), bool> | str) -> bool { finish true }",
        program: process_module(
            "install",
            vec![b::param(
                "handler",
                lashlang::TypeExpr::union(vec![
                    b::process_type(
                        vec![b::param("event", lashlang::TypeExpr::Str)],
                        lashlang::TypeExpr::Bool,
                    ),
                    lashlang::TypeExpr::Str,
                ]),
            )],
            lashlang::TypeExpr::Bool,
            b::bool_lit(true),
        ),
        environment: &environment,
    })
    .expect("union receiver compiles");
    store
        .publish_module_artifact(&crate::lib_tests::host_claim(), &receiver.artifact)
        .await
        .expect("module publishes");
    let mut args = lashlang::Record::new();
    args.insert(
        "handler".to_string(),
        lashlang::Value::String("fallback".into()),
    );
    let start = lashlang::ProcessStart {
        module_ref: receiver.module_ref.clone(),
        process_ref: receiver.artifact.process_ref("install").unwrap().clone(),
        host_requirements_ref: receiver.host_requirements_ref.clone(),
        start_site: test_start_site("child_process:install", 1),
        process_name: "install".to_string(),
        args,
    };
    let artifact_store: LashlangArtifacts = store;

    prepare_lashlang_process_start(
        artifact_store,
        Some(lash_core::StartKey::for_host(
            lash_core::StartKeyOwner::HOST,
            "parent:root",
        )),
        start,
        lash_core::ProcessOriginator::host(),
        lash_core::LifetimeDecision::Detached,
    )
    .await
    .expect("later string union arm accepts the value");
}

#[test]
fn surface_merges_plugin_extensions() {
    let contribution = LashlangSurfaceContribution::new(
        LashlangAbilities::default(),
        LashlangLanguageFeatures::default().with_label_annotations(),
        LashlangHostCatalog::tool_default(["lookup"]),
    );
    let extensions = lash_core::PluginExtensions::from_contributions([
        lash_core::facade_support::PluginExtensionContribution::new(
            LASHLANG_SURFACE_EXTENSION_ID,
            contribution,
        )
        .expect("extension payload serializes"),
    ]);

    let surface = LashlangSurface::default()
        .with_plugin_extensions(&extensions)
        .expect("lashlang surface extension merges");
    let environment = surface
        .host_environment(&lash_core::ToolCatalog::default())
        .expect("empty tool catalog has no Lashlang bindings to validate");

    assert!(environment.abilities.sleep);
    assert!(environment.language_features.label_annotations);
    assert!(
        environment
            .resources
            .resolve_module_operation("Tools", "tools", "lookup")
            .is_some()
    );
}

/// Masking a path over the catalog's memoized import builds exactly the
/// environment of the catalog and surface without the masked members — down
/// to the serialized bytes — and leaves the unmasked environment intact.
#[test]
fn masked_host_environment_is_the_environment_without_the_masked_members() {
    let tool = |id: &str, module: &str, operation: &str, authority: &str| {
        lash_core::ToolDefinition::raw(
            format!("tool:test/{id}"),
            id,
            format!("{id} fixture"),
            serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
            serde_json::json!({ "type": "string" }),
        )
        .with_tool_binding(ToolBinding::new([module], operation).with_authority_type(authority))
    };
    let mut internal = tool("internal_read", "fs", "internal", "Filesystem");
    internal.manifest.activation = lash_core::ToolActivation::Internal;
    let unmasked_tools = vec![
        tool("fs_write", "fs", "write", "Filesystem"),
        tool("mirror_read", "mirror", "read", "Filesystem"),
        internal,
    ];
    let masked_tools = [
        tool("fs_read", "fs", "read", "Filesystem"),
        tool("web_fetch", "web", "fetch", "Web"),
    ];
    let catalog = lash_core::ToolCatalog::from_tool_definitions(
        masked_tools
            .into_iter()
            .chain(unmasked_tools.clone())
            .collect(),
    );
    let masked = [
        "fs.read",
        "web.fetch",
        "tools.lookup",
        "absent.path",
        "undotted",
    ]
    .map(String::from)
    .into_iter()
    .collect::<BTreeSet<_>>();
    let surface = |operations: &[&str]| {
        LashlangSurface::default()
            .with_resources(LashlangHostCatalog::tool_default(
                operations.iter().copied(),
            ))
            .expect("surface resources are unique")
    };

    let environment = surface(&["lookup", "list"])
        .host_environment_masking(&catalog, &masked)
        .expect("masked environment builds");
    let expected = surface(&["list"])
        .host_environment(&lash_core::ToolCatalog::from_tool_definitions(
            unmasked_tools,
        ))
        .expect("environment without the masked members builds");
    assert_eq!(environment, expected);
    assert_eq!(
        serde_json::to_string(&environment).expect("environment serializes"),
        serde_json::to_string(&expected).expect("environment serializes"),
    );

    let unmasked = surface(&["lookup", "list"])
        .host_environment(&catalog)
        .expect("unmasked environment builds");
    assert!(unmasked.resources.provides_module_operation("fs", "read"));
    assert_eq!(
        unmasked,
        surface(&["lookup", "list"])
            .host_environment(&catalog.filtered(|_| true))
            .expect("a fresh import of the same members builds"),
        "masking never edits the catalog's memoized import"
    );
}

#[test]
fn surface_resources_return_typed_catalog_conflicts() {
    let surface = LashlangSurface::default()
        .with_resources(LashlangHostCatalog::tool_default(["lookup"]))
        .expect("first resource contribution is unique");

    assert!(matches!(
        surface.with_resources(LashlangHostCatalog::tool_default(["lookup"])),
        Err(LashlangRuntimeError::HostCatalog {
            source: lashlang::LashlangHostCatalogError::ConflictingModuleOperation {
                module,
                operation,
                ..
            }
        }) if module == "tools" && operation == "lookup"
    ));
}

#[test]
fn plugin_extensions_return_typed_catalog_conflicts() {
    let contributions = ["first", "second"].map(|_| {
        lash_core::facade_support::PluginExtensionContribution::new(
            LASHLANG_SURFACE_EXTENSION_ID,
            LashlangSurfaceContribution::new(
                LashlangAbilities::default(),
                LashlangLanguageFeatures::default(),
                LashlangHostCatalog::tool_default(["lookup"]),
            ),
        )
        .expect("extension payload serializes")
    });
    let extensions = lash_core::PluginExtensions::from_contributions(contributions);

    assert!(matches!(
        LashlangSurface::default().with_plugin_extensions(&extensions),
        Err(LashlangRuntimeError::HostCatalog {
            source: lashlang::LashlangHostCatalogError::ConflictingModuleOperation {
                module,
                operation,
                ..
            }
        }) if module == "tools" && operation == "lookup"
    ));
}

fn remote_tool_grant(name: &str) -> lash_remote_protocol::RemoteToolGrant {
    lash_remote_protocol::RemoteToolGrant {
        id: format!("remote-tool:{name}"),
        name: name.to_string(),
        description: String::new(),
        input_schema: lash_remote_protocol::RemoteSchemaContract {
            canonical: lash_core::ToolDefinition::default_input_schema(),
            projection: lash_remote_protocol::RemoteSchemaProjectionPolicy::default(),
        },
        output_schema: lash_remote_protocol::RemoteSchemaContract::default(),
        output_contract: lash_remote_protocol::RemoteToolOutputContract::Static,
        examples: Vec::new(),
        activation: None,
        argument_projection: None,
        retry_policy: None,
        bindings: Default::default(),
    }
}

fn test_start_site(node_id: &str, occurrence: u64) -> lashlang::LashlangExecutionCallSite {
    lashlang::LashlangExecutionCallSite {
        site: lashlang::LashlangExecutionSite {
            node_id: node_id.to_string(),
            node_kind: lash_sansio::ExecutionNodeKind::Call,
            label: "start scan".to_string(),
            branch: None,
            workflow_site: lashlang::WorkflowExecutionSite::new(
                "process:scan",
                [],
                lash_sansio::ExecutionNodeKind::Call,
                "start scan",
            ),
        },
        occurrence,
    }
}

fn test_process_start(
    output: &lashlang::ModuleCompileOutput,
    start_site: lashlang::LashlangExecutionCallSite,
    root: &str,
) -> lashlang::ProcessStart {
    let mut args = lashlang::Record::new();
    args.insert("root".to_string(), lashlang::Value::String(root.into()));
    lashlang::ProcessStart {
        module_ref: output.module_ref.clone(),
        process_ref: output
            .artifact
            .process_ref("scan")
            .expect("scan process export")
            .clone(),
        host_requirements_ref: output.host_requirements_ref.clone(),
        start_site,
        process_name: "scan".to_string(),
        args,
    }
}
