//! Law L12 (FIG-3571): observation is language-agnostic.
//!
//! `Mini` is a test-only front end with no TypeScript anywhere in this
//! crate's dependency graph. It lowers its own statements straight to IR under
//! its own `SourceLanguage`, marking structure with the language-neutral
//! roles: an iteration whose body has several statements and a branch, an
//! attribute assignment, an effect call and a lifted process. Its programs get
//! complete maps (L1), identical sites and events across stored reload and
//! redrive (L2), and working process observation — with nothing in the runtime
//! knowing what a `Mini` program looked like.

use super::*;

/// The second front end's statements.
enum Mini {
    Let(&'static str, lashlang::Expr),
    /// `each <binding> in <over> { <body> }`
    Each {
        binding: &'static str,
        over: lashlang::Expr,
        body: Vec<Mini>,
    },
    /// `when <condition> { <then> } otherwise { <otherwise> }`
    When {
        condition: lashlang::Expr,
        then: Vec<Mini>,
        otherwise: Vec<Mini>,
    },
    /// `<object>.<field> := <value>`
    SetAttr {
        object: &'static str,
        field: &'static str,
        value: lashlang::Expr,
    },
    /// `pause` — a durable sleep effect.
    Pause,
    /// `worker <name> { <body> }` — an inline process, lifted by the linker.
    Worker {
        name: &'static str,
        body: Vec<Mini>,
    },
    Show(lashlang::Expr),
    Done(lashlang::Expr),
}

const MINI_LANGUAGE: &str = "mini";

/// The front end: statement lists lower to blocks, and each construct to the
/// IR form or structural role that says what it does.
struct MiniLowerer {
    temporaries: u32,
}

impl MiniLowerer {
    fn program(statements: Vec<Mini>) -> lashlang::Program {
        let mut lowerer = Self { temporaries: 0 };
        let mut program = b::program(lowerer.statements(statements));
        program.language = lashlang::SourceLanguage::new(MINI_LANGUAGE);
        program
    }

    fn statements(&mut self, statements: Vec<Mini>) -> Vec<lashlang::Expr> {
        statements
            .into_iter()
            .map(|statement| self.statement(statement))
            .collect()
    }

    fn block(&mut self, statements: Vec<Mini>) -> lashlang::Expr {
        b::block(self.statements(statements))
    }

    fn temporary(&mut self, purpose: &str) -> String {
        self.temporaries += 1;
        format!("mini_{purpose}_{}", self.temporaries)
    }

    fn statement(&mut self, statement: Mini) -> lashlang::Expr {
        match statement {
            Mini::Let(name, value) => b::assign(name, value),
            Mini::Each {
                binding,
                over,
                body,
            } => b::for_in(binding, over, self.block(body)),
            Mini::When {
                condition,
                then,
                otherwise,
            } => b::if_else(condition, self.block(then), self.block(otherwise)),
            Mini::SetAttr {
                object,
                field,
                value,
            } => {
                let base = self.temporary("base");
                let result = self.temporary("result");
                b::role(
                    lashlang::StructuralRole::AttributeAssign,
                    b::block(vec![
                        b::assign(&base, b::var(object)),
                        b::assign(&result, value),
                        b::assign_path(&base, vec![b::field_step(field)], b::var(&result)),
                        b::var(&result),
                    ]),
                )
            }
            Mini::Pause => b::sleep_until(b::num(0.0)),
            Mini::Worker { name, body } => {
                let body = self.block(body);
                b::assign(name, b::process_literal(Vec::new(), body))
            }
            Mini::Show(value) => b::print(value),
            Mini::Done(value) => b::finish(value),
        }
    }
}

/// The worker's body: every construct but the lift, so the lifted process is
/// the one that iterates, branches, writes an attribute and sleeps.
fn worker_body() -> Vec<Mini> {
    vec![
        Mini::Let("tally", b::record(vec![("seen", b::num(0.0))])),
        Mini::Each {
            binding: "step",
            over: b::list(vec![b::num(1.0), b::num(2.0)]),
            body: vec![
                Mini::Let(
                    "next",
                    b::binary(b::var("step"), lashlang::BinaryOp::Add, b::num(1.0)),
                ),
                Mini::When {
                    condition: b::binary(b::var("next"), lashlang::BinaryOp::Greater, b::num(2.0)),
                    then: vec![Mini::SetAttr {
                        object: "tally",
                        field: "seen",
                        value: b::var("next"),
                    }],
                    otherwise: vec![Mini::Let("skipped", b::var("next"))],
                },
            ],
        },
        Mini::Pause,
        Mini::Done(b::string("worked")),
    ]
}

fn mini_program() -> lashlang::Program {
    MiniLowerer::program(vec![
        Mini::Let("box", b::record(vec![("value", b::num(0.0))])),
        Mini::Each {
            binding: "item",
            over: b::list(vec![b::num(1.0), b::num(2.0)]),
            body: vec![
                Mini::Show(b::var("item")),
                Mini::When {
                    condition: b::binary(b::var("item"), lashlang::BinaryOp::Greater, b::num(1.0)),
                    then: vec![Mini::SetAttr {
                        object: "box",
                        field: "value",
                        value: b::var("item"),
                    }],
                    otherwise: vec![Mini::Show(b::string("small"))],
                },
            ],
        },
        Mini::Worker {
            name: "worker",
            body: worker_body(),
        },
        Mini::Done(b::var("worker")),
    ])
}

fn mini_environment() -> LashlangHostEnvironment {
    LashlangHostEnvironment::new(
        lashlang::LashlangHostCatalog::new(),
        LashlangAbilities::default().with_sleep(),
    )
}

fn mini_module() -> lashlang::ModuleCompileOutput {
    lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: "mini",
        program: mini_program(),
        environment: &mini_environment(),
    })
    .expect("the mini program links")
}

fn lifted_worker(artifact: &lashlang::ModuleArtifact) -> String {
    let lifted = artifact
        .ir
        .declarations
        .iter()
        .filter_map(|declaration| match declaration {
            lashlang::Declaration::Process(process) if process.origin.is_lifted() => {
                Some(process.name.to_string())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let [name] = lifted.as_slice() else {
        panic!("one lifted worker, got {lifted:?}");
    };
    name.clone()
}

type SiteKey = (
    String,
    lash_sansio::ExecutionNodeKind,
    lash_sansio::WorkflowExecutionSite,
);

fn compiled_sites(
    artifact: &lashlang::ModuleArtifact,
    entry: lashlang::Entry<'_>,
) -> BTreeSet<SiteKey> {
    let compiled = lashlang::compile(artifact, entry, None).expect("the mini entry compiles");
    lashlang::testing::harness::compiled_execution_sites(&compiled)
        .into_iter()
        .map(|site| {
            (
                site.node_id.clone(),
                site.node_kind,
                site.workflow_site.clone(),
            )
        })
        .collect()
}

fn map_sites(map: &lash_trace::TraceLanguageExecutionMap) -> BTreeSet<SiteKey> {
    map.nodes
        .iter()
        .map(|node| (node.id.clone(), node.kind, node.site.clone()))
        .collect()
}

#[test]
fn a_second_front_end_gets_complete_maps_for_main_and_its_lifted_process() {
    let output = mini_module();
    assert_eq!(output.artifact.ir.language.as_str(), MINI_LANGUAGE);
    let worker = lifted_worker(&output.artifact);
    let worker_ref = output
        .artifact
        .process_ref(&worker)
        .expect("the lifted worker is exported")
        .clone();

    let main_sites = compiled_sites(&output.artifact, lashlang::Entry::Main);
    let main_map = map_sites(&trace_lashlang_main_map(&output.artifact));
    assert!(!main_sites.is_empty());
    assert_eq!(
        main_sites.difference(&main_map).collect::<Vec<_>>(),
        Vec::<&SiteKey>::new(),
        "every main site is in the main map with its kind and owner path"
    );

    let worker_sites = compiled_sites(&output.artifact, lashlang::Entry::Process(&worker_ref));
    let worker_map =
        map_sites(&trace_lashlang_process_map(&output.artifact, &worker).expect("worker map"));
    let kinds = worker_sites
        .iter()
        .map(|(_, kind, _)| *kind)
        .collect::<BTreeSet<_>>();
    for kind in [
        lash_sansio::ExecutionNodeKind::Loop,
        lash_sansio::ExecutionNodeKind::Branch,
        lash_sansio::ExecutionNodeKind::Sleep,
        lash_sansio::ExecutionNodeKind::Terminal,
    ] {
        assert!(
            kinds.contains(&kind),
            "the worker's sites cover {kind:?}, got {kinds:?}"
        );
    }
    assert_eq!(
        worker_sites.difference(&worker_map).collect::<Vec<_>>(),
        Vec::<&SiteKey>::new(),
        "every lifted-process site is in the process map with its kind and owner path"
    );
}

#[test]
fn a_second_front_end_keeps_its_sites_across_relink_and_stored_reload() {
    let first = mini_module();
    let relinked = mini_module();
    assert_eq!(first.module_ref, relinked.module_ref);

    let stored = serde_json::to_vec(&first.artifact).expect("the artifact serializes");
    let reloaded: lashlang::ModuleArtifact =
        serde_json::from_slice(&stored).expect("the stored artifact reloads");
    assert_eq!(reloaded.source_identity(), first.artifact.source_identity());

    let worker = lifted_worker(&first.artifact);
    assert_eq!(lifted_worker(&reloaded), worker);
    let worker_ref = first.artifact.process_ref(&worker).expect("worker").clone();
    assert_eq!(
        compiled_sites(&reloaded, lashlang::Entry::Main),
        compiled_sites(&first.artifact, lashlang::Entry::Main)
    );
    assert_eq!(
        compiled_sites(&reloaded, lashlang::Entry::Process(&worker_ref)),
        compiled_sites(&first.artifact, lashlang::Entry::Process(&worker_ref))
    );
    assert_eq!(
        trace_lashlang_main_map(&reloaded),
        trace_lashlang_main_map(&first.artifact)
    );
}

/// Runs the lifted worker once through the process engine, from a freshly
/// published store, and returns its observed graph.
async fn run_worker(run: &str) -> (lashlang::ModuleCompileOutput, TraceLashlangGraph) {
    let output = mini_module();
    let worker = lifted_worker(&output.artifact);
    let store = Arc::new(InMemoryLashlangArtifactStore::new());
    store
        .publish_module_artifact(&lash_core::ArtifactOwner::host("mini"), &output.artifact)
        .await
        .expect("the mini artifact publishes");
    let input = LashlangProcessInput {
        module_ref: output.module_ref.clone(),
        process_ref: output
            .artifact
            .process_ref(&worker)
            .expect("worker export")
            .clone(),
        host_requirements_ref: output.host_requirements_ref.clone(),
        process_name: worker,
        args: serde_json::Map::new(),
    };
    let process_id = lash_core::ProcessId::from("mini-worker");
    let registration = lash_core::ProcessRegistration::new(
        process_id.clone(),
        input.to_process_input().expect("valid process input"),
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessProvenance::host(),
        lash_core::ProcessLifecyclePolicy::new(
            lash_core::ParentScope::Host,
            lash_core::OnParentEnd::Abandon,
        ),
    )
    .with_admitted_identity(lash_core::AdmittedProcessIdentity::for_testing(
        input.process_identity(),
    ));
    let incarnation = lash_core::ProcessIncarnation::from_registration_sequence(1);
    let effect_host = lash_core::facade_support::NativeEffectHost::default();
    let scoped = lash_core::EffectHost::scoped_static(
        &effect_host,
        lash_core::AdmittedScope::process(lash_core::ProcessRef::new(
            process_id.clone(),
            incarnation,
        )),
    )
    .expect("valid process scope")
    .expect("native controller");
    let parent = lash_core::RuntimeInvocation::effect(
        lash_core::EffectAddress::new(
            lash_core::ExecutionScope::process(process_id.clone()),
            "process-body",
        )
        .expect("valid process effect address"),
        lash_core::RuntimeAttribution::none(),
        "process-body",
    );
    let built = lash_core::testing::TestExecutionContextBuilder::new()
        .borrowed_effect_controller(scoped.clone())
        .runtime_parent_invocation(parent)
        .build();
    let plugins = Arc::clone(&built.dispatch.plugins);
    let catalog = Arc::clone(&built.dispatch.tool_catalog);
    let registry: Arc<dyn lash_core::ProcessRegistry> =
        Arc::new(lash_core::TestLocalProcessRegistry::default());
    let authority =
        lash_core::ProcessExecutionWriteAuthority::invocation(process_id, run).bind_attempt(1);
    let process_events = durable_process_events(&registry, &registration, &authority).await;
    let execution_registration = registration.clone();
    let context = lash_core::ProcessEngineRunContext::new(
        registration,
        incarnation,
        lash_core::ProcessExecutionContext::default().with_execution_write_authority(authority),
        lash_core::testing::process_work_wiring_for_registry(registry),
        lash_core::SessionId::from("mini-session"),
        plugins,
        catalog,
        None,
        None,
        Arc::new(lash_core::NoQueuedWork::new()),
        lash_core::DeliveryPolicy::EarliestSafeBoundary,
        Arc::new(lash_core::facade_support::SystemClock),
        true,
        lash_core::CancellationToken::new(),
        None,
        scoped,
        None,
        Box::new(move |_catalog| {
            Ok(
                lash_core_execution::runtime::ProcessEngineRuntimeContext::new(
                    built
                        .into_runtime()
                        .with_process_execution(&execution_registration, process_events),
                    lash_core_execution::runtime::ProcessEngineRunGuard::new(|_| {
                        Box::pin(async { Ok(()) })
                    }),
                ),
            )
        }),
    );
    let graph_store = Arc::new(TraceLashlangGraphStore::default());
    let sink: Arc<dyn lash_trace::TraceSink> = graph_store.clone();
    let result = Box::pin(crate::process::run_lashlang_process(
        LashlangProcessEngine::new(store, LashlangSurface::default())
            .with_execution_trace(Some(sink), lash_trace::TraceContext::default()),
        context,
        serde_json::to_value(input).expect("process input serializes"),
    ))
    .await
    .expect("the worker runs");
    assert!(
        result.is_terminal() && !format!("{result:?}").contains("Failure"),
        "the worker finishes: {result:?}"
    );
    let graph = graph_store
        .graphs()
        .into_iter()
        .next()
        .expect("the worker's observed graph");
    (output, graph)
}

/// The node events a run emitted, in order.
fn emitted(graph: &TraceLashlangGraph) -> Vec<(String, lash_sansio::ExecutionNodeKind, String)> {
    graph
        .history
        .iter()
        .filter_map(|event| match &event.event.payload {
            TraceLanguageExecutionPayload::NodeStarted {
                node_id, node_kind, ..
            } => Some((node_id.clone(), *node_kind, "started".to_string())),
            TraceLanguageExecutionPayload::NodeCompleted {
                node_id, node_kind, ..
            } => Some((node_id.clone(), *node_kind, "completed".to_string())),
            TraceLanguageExecutionPayload::NodeWaiting {
                node_id, node_kind, ..
            } => Some((node_id.clone(), *node_kind, "waiting".to_string())),
            TraceLanguageExecutionPayload::NodeFailed {
                node_id, node_kind, ..
            } => Some((node_id.clone(), *node_kind, "failed".to_string())),
            TraceLanguageExecutionPayload::BranchSelected {
                node_id, selected, ..
            } => Some((
                node_id.clone(),
                lash_sansio::ExecutionNodeKind::Branch,
                format!("selected {selected:?}"),
            )),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn a_second_front_end_lifted_process_is_observed_and_redrives_identically() {
    let (output, graph) = run_worker("mini-first").await;
    let worker = lifted_worker(&output.artifact);
    assert_eq!(graph.source_identity, output.artifact.source_identity());

    let map = trace_lashlang_process_map(&output.artifact, &worker).expect("worker map");
    let mapped = map
        .nodes
        .iter()
        .map(|node| (node.id.clone(), node.kind))
        .collect::<BTreeSet<_>>();
    let first = emitted(&graph);
    assert!(!first.is_empty(), "the worker's run is observed");
    for (node_id, kind, status) in &first {
        assert!(
            mapped.contains(&(node_id.clone(), *kind)),
            "the worker emitted {status} for `{node_id}` ({kind:?}), which its map lacks"
        );
    }
    assert!(
        graph.nodes.iter().any(|node| matches!(
            node.observation,
            TraceLashlangNodeObservation::Completed { .. }
        )),
        "the worker's graph folds completed nodes"
    );
    assert!(
        first.iter().any(|(_, _, status)| status == "waiting"),
        "the worker's sleep effect is observed waiting: {first:?}"
    );

    let (_, redriven) = run_worker("mini-redrive").await;
    assert_eq!(
        emitted(&redriven),
        first,
        "a fresh engine redriving the worker emits the same sites in the same order"
    );
}
