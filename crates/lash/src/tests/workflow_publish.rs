//! FIG-5574: a host publishes an edited workflow document directly. Lash
//! admits the IR in its VM workers against the host's environment, holds the
//! definition under the host's pin and answers its identity, the admitted
//! document and the correspondence.

use super::*;
use crate::workflow::{
    WorkflowAdmissionDiagnosticKind, WorkflowAdmissionLocation, WorkflowBodyRef,
    WorkflowCorrespondenceEntry, WorkflowDraft, WorkflowEdit, WorkflowEditTransaction,
    WorkflowEntry, WorkflowPublication, WorkflowPublish, WorkflowRead,
};
use lash_vm::testing::ast_builders as b;
use lash_vm_client::service::WorkerPath;

/// A core over SQLite whose VM workers record what each call asked of them.
async fn recording_core() -> (LashCore, crate::vm::WorkerService) {
    let backend = sqlite_memory_store_backend().await;
    let workers = untimed_fixture_workers().with_worker_receipts();
    let factory = rlm_factory(&backend).with_worker_service(workers.clone());
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(backend, factory))
        .build(crate::testing::runtime_lease_owner())
        .expect("the core builds");
    (core, workers)
}

fn environment() -> lash_core::ProcessExecutionEnvSpec {
    lash_core::ProcessExecutionEnvSpec::new(
        lash_core::AdmittedPluginConfig::default(),
        lash_core::SessionPolicy::new(
            crate::TurnBudget::bounded(32),
            crate::MaxToolCalls::new(1024),
            crate::NoProgressBudget::bounded(12),
        ),
        lash_core::SessionToolAccess::ambient(),
    )
}

/// `guarded(name)`: binds `out` inside a `try` and finishes with it. Direct
/// IR: no source text exists for it anywhere.
fn guarded() -> lash_vm::WorkflowGraph {
    lash_vm::workflow_graph_from_program(&b::module(
        vec![b::process(
            "guarded",
            vec![b::param("name", lash_vm::TypeExpr::Str)],
            b::block(vec![
                b::assign("out", b::string("start")),
                b::try_expr(
                    b::block(vec![b::assign("out", b::var("name"))]),
                    Some(b::catch(
                        "error",
                        b::block(vec![b::assign("out", b::string("caught"))]),
                    )),
                    None,
                ),
                b::finish(b::var("out")),
            ]),
        )],
        Vec::new(),
    ))
}

async fn publish(
    core: &LashCore,
    pin: &crate::process::HostArtifactPin,
    draft: &WorkflowDraft,
) -> WorkflowPublish {
    core.host_artifacts()
        .publish_workflow(pin, draft, WorkflowEntry::Sole, &environment())
        .await
        .expect("the publication answers")
}

fn published(publish: WorkflowPublish) -> WorkflowPublication {
    match publish {
        WorkflowPublish::Published(publication) => *publication,
        other => panic!("the workflow publishes: {other:?}"),
    }
}

/// The draft of `graph` with the `try` region of its one process and the
/// statements of that process.
fn opened(
    graph: &lash_vm::WorkflowGraph,
) -> (
    WorkflowDraft,
    WorkflowBodyRef,
    crate::workflow::WorkflowDraftHandle,
) {
    let draft = WorkflowDraft::open(graph).expect("the document opens");
    let process = draft
        .handle(&draft.document().process("guarded").expect("the process").id)
        .expect("the process container's handle");
    let body = WorkflowBodyRef::Process(process);
    let region = draft.body(&body).expect("the process body")[1];
    (draft, body, region)
}

/// Runs `definition` as `guarded("operator")` and answers what it finished
/// with.
async fn run(
    core: &LashCore,
    pin: &crate::process::HostArtifactPin,
    definition: &lash_core::ProcessDefinition,
    key: &str,
) -> (lash_core::ProcessId, serde_json::Value) {
    let env_ref = core
        .host_artifacts()
        .publish_process_env(pin, &environment())
        .await
        .expect("publish the process environment");
    let process_id = start(core, &env_ref, definition, key, ("name", "operator")).await;
    let value = finished(core, &process_id).await;
    (process_id, value)
}

/// Starts `definition` with one string argument under the host key `key`.
async fn start(
    core: &LashCore,
    env_ref: &lash_core::ProcessExecutionEnvRef,
    definition: &lash_core::ProcessDefinition,
    key: &str,
    (name, value): (&str, &str),
) -> lash_core::ProcessId {
    let mut args = serde_json::Map::new();
    args.insert(name.to_owned(), serde_json::json!(value));
    core.processes()
        .start(
            lash_core::ProcessStartRequest::new(
                lash_core::ProcessStartTarget::Definition {
                    definition_id: definition.id.clone(),
                    signature_claim: Some(definition.signature.clone()),
                    args,
                },
                lash_core::ProcessOriginator::host(),
                lash_core::LifetimeDecision::Detached,
            )
            .with_host_start_key(key)
            .with_env_ref(env_ref.clone()),
            core.effect_host(),
        )
        .await
        .expect("the process starts")
        .process_id
}

/// What the process finished with.
async fn finished(core: &LashCore, process_id: &lash_core::ProcessId) -> serde_json::Value {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        core.processes().await_output(process_id),
    )
    .await
    .expect("the process settles")
    .expect("the output reads");
    let lash_core::ProcessAwaitOutput::Settled { output } = output else {
        panic!("the process settles with an output: {output:?}");
    };
    assert!(output.is_success(), "the process finishes: {output:?}");
    output.value_for_projection()
}

/// Inspect, edit inside a `try` region, publish and run, on typed IR alone.
/// The worker is the only place on these paths that holds a parser, and
/// every call it served is on record: none parsed or printed a dialect.
#[tokio::test]
async fn a_workflow_is_inspected_edited_in_a_try_region_published_and_run_without_a_dialect() {
    let (core, workers) = recording_core().await;
    let artifacts = core.host_artifacts();
    let pin = crate::process::HostArtifactPin::mint();
    let first = published(
        publish(
            &core,
            &pin,
            &WorkflowDraft::open(&guarded()).expect("the IR opens"),
        )
        .await,
    );
    assert_eq!(first.document.entry, "guarded");

    let WorkflowRead::Inspected(inspection) = artifacts
        .definition_graph(&first.definition.id)
        .await
        .expect("the definition reads")
    else {
        panic!("a published workflow has a document");
    };
    assert_eq!(inspection.definition, first.definition);
    assert_eq!(inspection.document, first.document);

    let (mut draft, _, region) = opened(&inspection.document.graph);
    let region_id = draft.node_id(region).expect("the region's id").clone();
    let guarded_body = WorkflowBodyRef::Child {
        node: region,
        slot: lash_vm::WorkflowBodySlot::TryBody,
    };
    draft
        .apply(WorkflowEditTransaction {
            base: draft.revision(),
            edits: vec![WorkflowEdit::InsertNode {
                body: guarded_body.clone(),
                before: None,
                statement: b::assign("out", b::string("edited")),
            }],
        })
        .expect("the edit applies");
    let inserted = *draft
        .body(&guarded_body)
        .expect("the try body")
        .last()
        .expect("the inserted statement");
    let second = published(publish(&core, &pin, &draft).await);
    assert_ne!(second.definition.id, first.definition.id);

    let admitted = second
        .document
        .graph
        .nodes()
        .map(|node| node.id.clone())
        .collect::<Vec<_>>();
    let entries = &second.correspondence.entries;
    assert!(
        entries.iter().any(|entry| matches!(
            entry,
            WorkflowCorrespondenceEntry::Retained { handle, from, to }
                if *handle == region && *from == region_id && admitted.contains(to)
        )),
        "the region is the node it was: {entries:?}"
    );
    assert!(
        entries.iter().any(|entry| matches!(
            entry,
            WorkflowCorrespondenceEntry::Inserted { handle, to, .. }
                if *handle == inserted && admitted.contains(to)
        )),
        "the inserted statement is named in the admitted document: {entries:?}"
    );
    assert!(
        !entries
            .iter()
            .any(|entry| matches!(entry, WorkflowCorrespondenceEntry::Unmatched { .. })),
        "admission names every node: {entries:?}"
    );

    // The first definition is immutable: a process of it still runs the
    // program it was admitted with, beside a process of the edited one.
    let (kept, kept_output) = run(&core, &pin, &first.definition, "publish-first").await;
    let (_, edited_output) = run(&core, &pin, &second.definition, "publish-second").await;
    assert_eq!(kept_output, serde_json::json!("operator"));
    assert_eq!(edited_output, serde_json::json!("edited"));
    assert_eq!(
        core.processes()
            .get(&kept)
            .await
            .expect("read the process")
            .expect("the process is retained")
            .identity
            .definition_id,
        Some(first.definition.id.clone())
    );

    let paths = workers
        .worker_receipts()
        .into_iter()
        .map(|receipt| receipt.path)
        .collect::<Vec<_>>();
    assert_eq!(
        paths
            .iter()
            .filter(|path| **path == WorkerPath::Admit)
            .count(),
        2,
        "each publication is one admission: {paths:?}"
    );
    assert!(
        !paths.iter().any(|path| matches!(
            path,
            WorkerPath::References | WorkerPath::Compile | WorkerPath::CreateDefinition
        )),
        "no call reached the worker's dialect front end: {paths:?}"
    );
    core.shutdown().await.expect("the core shuts down");
}

/// A requirement the host's environment does not provide refuses the whole
/// publication at the node that needs it, before anything reaches a store;
/// and an unchanged admitted document is the definition it was read from.
#[tokio::test]
async fn an_unmet_host_requirement_refuses_atomically_and_an_identical_republish_is_one_definition()
{
    let (core, workers) = recording_core().await;
    let artifacts = core.host_artifacts();
    let pin = crate::process::HostArtifactPin::mint();
    let first = published(
        publish(
            &core,
            &pin,
            &WorkflowDraft::open(&guarded()).expect("the IR opens"),
        )
        .await,
    );

    let (unchanged, _, _) = opened(&first.document.graph);
    let again =
        published(publish(&core, &crate::process::HostArtifactPin::mint(), &unchanged).await);
    assert_eq!(again.definition, first.definition);
    assert_eq!(again.document, first.document);

    let (mut draft, body, region) = opened(&first.document.graph);
    draft
        .apply(WorkflowEditTransaction {
            base: draft.revision(),
            edits: vec![WorkflowEdit::InsertNode {
                body: body.clone(),
                before: Some(region),
                statement: b::module_call(&["vault"], "read", vec![b::record(Vec::new())]),
            }],
        })
        .expect("the edit is a valid program");
    let needy = draft.body(&body).expect("the process body")[1];
    let needy = draft.node_id(needy).expect("the call's id").clone();

    let served = workers.worker_receipts().len();
    let refused_pin = crate::process::HostArtifactPin::mint();
    let WorkflowPublish::Refused(refusal) = publish(&core, &refused_pin, &draft).await else {
        panic!("the host has no `vault` module");
    };
    let [diagnostic] = refusal.diagnostics.as_slice() else {
        panic!("one diagnostic: {refusal:?}");
    };
    assert_eq!(
        diagnostic.kind,
        WorkflowAdmissionDiagnosticKind::HostRequirement
    );
    assert!(
        matches!(&diagnostic.location, WorkflowAdmissionLocation::Node { node, .. } if *node == needy),
        "the refusal names the call: {diagnostic:?}"
    );
    // The refusal was the publication's only worker call: the definition's
    // descriptor, which is checked against the stored module in a worker,
    // was never offered to a store.
    let after = workers
        .worker_receipts()
        .into_iter()
        .skip(served)
        .map(|receipt| receipt.path)
        .collect::<Vec<_>>();
    assert_eq!(after, [WorkerPath::Admit]);
    artifacts
        .release(refused_pin)
        .await
        .expect("a pin that holds nothing releases");
    assert_eq!(
        artifacts
            .get_definition(&first.definition.id)
            .await
            .expect("the definition reads"),
        Some(first.definition.clone())
    );
    core.shutdown().await.expect("the core shuts down");
}

const GREETER: &str = "const greet = async (name: string) => {\n  return name;\n};\n";

/// TypeScript is a lens on either side of the same pipeline: imported
/// source is a document a draft opens and publishes to the definition its
/// source links to, and an inspected document answers its canonical source
/// with a span per node when asked.
#[tokio::test]
async fn typescript_imports_into_the_same_publication_and_exports_as_a_source_view() {
    let (core, _) = recording_core().await;
    let artifacts = core.host_artifacts();
    let pin = crate::process::HostArtifactPin::mint();
    let imported = lash_typescript::workflow_graph::workflow_graph_from_source(GREETER)
        .expect("the greeter lowers");
    let publication = published(
        publish(
            &core,
            &pin,
            &WorkflowDraft::open(&imported).expect("the import opens"),
        )
        .await,
    );

    let surface = lash_vm_runtime::LashVmSurface::default()
        .host_environment(&lash_core::ToolCatalog::default())
        .expect("the default surface has an environment");
    let linked = lash_typescript::link(GREETER, &surface)
        .expect("the greeter links")
        .artifact;
    assert_eq!(
        publication.document.graph.source_identity,
        Some(linked.source_identity()),
        "the import is the program its source links to"
    );

    let WorkflowRead::Inspected(inspection) = artifacts
        .definition_graph(&publication.definition.id)
        .await
        .expect("the definition reads")
    else {
        panic!("a published workflow has a document");
    };
    let view = inspection
        .source_view()
        .expect("the greeter has a spelling");
    assert_eq!(
        view.source,
        lash_typescript::workflow_graph::typescript_program_source(linked.ir())
            .expect("the greeter prints")
    );
    assert_eq!(view.source_identity, Some(linked.source_identity()));
    assert!(
        inspection
            .document
            .graph
            .nodes()
            .all(|node| view.spans.contains_key(&node.id)),
        "every node has a span: {view:?}"
    );
    core.shutdown().await.expect("the core shuts down");
}

/// A workflow that defines a process inline and starts it by value.
const SUPERVISOR: &str = r#"const supervise = async (stage: string) => {
  const audit = async (stage: string) => {
    return `audited ${stage}`;
  };
  const started = await processes.start({ definition: audit, args: { stage } });
  return await started;
};
"#;

/// FIG-5621: one publication stores a definition for every process the
/// admitted module exports, and a started process holds the definitions its
/// module can start (ADR 0113 §3.6). The host publishes the workflow once,
/// starts it and releases its pin; the run starts its inline process and
/// finishes. Once the pin's edges are severed, the definition and its
/// sibling are still startable through the first process's record alone.
#[tokio::test]
async fn a_published_workflow_starts_its_inline_process_after_the_host_pin_is_released() {
    let backend = sqlite_memory_store_backend().await;
    let factory = rlm_factory(&backend);
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(backend, factory))
        .plugin(Arc::new(
            lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(
                lash_core::lifetime::session_or_starter,
            ),
        ))
        .build(crate::testing::runtime_lease_owner())
        .expect("the core builds");
    let artifacts = core.host_artifacts();
    let pin = crate::process::HostArtifactPin::mint();
    let imported = lash_typescript::workflow_graph::workflow_graph_from_source(SUPERVISOR)
        .expect("the supervisor lowers");
    let draft = WorkflowDraft::open(&imported).expect("the import opens");
    // The workflow is the process the source's main body defines; its
    // inline process is the one lifted out of it.
    let entry = draft
        .document()
        .declarations
        .iter()
        .find_map(|declaration| match declaration {
            lash_vm::WorkflowDeclaration::Process(process)
                if matches!(
                    &process.origin,
                    lash_vm::ProcessOrigin::Lifted { site, .. } if site.root == lash_vm::AstRoot::Main
                ) =>
            {
                Some(process.id.clone())
            }
            _ => None,
        })
        .expect("the workflow's process");
    let supervise = published(
        artifacts
            .publish_workflow(&pin, &draft, WorkflowEntry::Process(entry), &environment())
            .await
            .expect("the publication answers"),
    )
    .definition;
    // Only the pin holds this one: it is gone once the pin's end is applied.
    let witness = published(
        publish(
            &core,
            &pin,
            &WorkflowDraft::open(&guarded()).expect("the IR opens"),
        )
        .await,
    )
    .definition;
    let env_ref = artifacts
        .publish_process_env(&pin, &environment())
        .await
        .expect("publish the process environment");

    let first = start(&core, &env_ref, &supervise, "supervise-1", ("stage", "one")).await;
    artifacts.release(pin).await.expect("the pin releases");
    assert_eq!(
        finished(&core, &first).await,
        serde_json::json!("audited one"),
        "the run starts its inline process with the pin released"
    );

    // Observe the descriptor's reclamation directly. Resolving an unpinned
    // definition can race cleanup of its module before its descriptor.
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while core
            .backend()
            .definition_store()
            .get_process_definition(&witness.id)
            .await
            .expect("the witness descriptor reads")
            .is_some()
        {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the released pin's edges are severed");
    let second = start(&core, &env_ref, &supervise, "supervise-2", ("stage", "two")).await;
    assert_eq!(
        finished(&core, &second).await,
        serde_json::json!("audited two"),
        "the first process's record holds the workflow and its inline process"
    );
    core.shutdown().await.expect("the core shuts down");
}

/// FIG-5647: declaration slots reach worker admission and durable execution.
#[tokio::test]
async fn function_and_wrapper_slot_edits_publish_and_run_on_the_durable_engine() {
    use crate::workflow::{WorkflowEditLocation, WorkflowExpressionRef};
    use lash_vm::{ExprSlot, FunctionExpr, ProcessWrapperParts, TypeExpr, WorkflowSlotPath};

    let program = b::module(
        vec![
            b::function_decl(
                "decorate",
                vec![b::function_param("value", TypeExpr::Str)],
                TypeExpr::Str,
                b::binary(
                    b::var("value"),
                    lash_vm::CoercingBinaryOp::Add,
                    b::string("_before"),
                ),
            ),
            b::process(
                "guarded",
                vec![b::param("name", TypeExpr::Str)],
                ProcessWrapperParts::build(
                    FunctionExpr {
                        name: Some("run".into()),
                        js_name: None,
                        receiver: None,
                        params: vec!["input".into()],
                        captures: Vec::new(),
                        body: Box::new(b::function_call(
                            "decorate",
                            vec![b::field(b::var("input"), "value")],
                        )),
                    },
                    None,
                    vec![b::record(vec![("value", b::string("initial"))])],
                    "caught".into(),
                ),
            ),
        ],
        Vec::new(),
    );
    let graph = lash_vm::testing::differential::through_wire(
        &lash_vm::workflow_graph_from_program(&program),
    )
    .expect("document wire round trip");
    let mut draft = WorkflowDraft::open(&graph).expect("draft");
    let process = draft
        .handle(&draft.document().process("guarded").expect("guarded").id)
        .expect("process handle");
    let function_slot = WorkflowSlotPath::structural([ExprSlot::Right]);
    draft
        .apply(WorkflowEditTransaction {
            base: draft.revision(),
            edits: vec![WorkflowEdit::ReplaceExpression {
                target: WorkflowExpressionRef::Function("decorate".into()),
                slot: function_slot.clone(),
                expression: b::string("_edited"),
            }],
        })
        .expect("function slot edit");
    let (core, _) = recording_core().await;
    let pin = crate::process::HostArtifactPin::mint();
    let function_publication = published(publish(&core, &pin, &draft).await);
    assert_eq!(
        function_publication.correspondence.expression_edits,
        vec![WorkflowEditLocation::Function {
            name: "decorate".into(),
            slot: function_slot
        }]
    );
    let (_, output) = run(
        &core,
        &pin,
        &function_publication.definition,
        "function-slot",
    )
    .await;
    assert_eq!(output, serde_json::json!("initial_edited"));

    let wrapper_slot = WorkflowSlotPath::structural([ExprSlot::Arg(0), ExprSlot::Entry(0)]);
    draft
        .apply(WorkflowEditTransaction {
            base: draft.revision(),
            edits: vec![WorkflowEdit::ReplaceExpression {
                target: WorkflowExpressionRef::ProcessWrapper(process),
                slot: wrapper_slot.clone(),
                expression: b::string("wrapper"),
            }],
        })
        .expect("wrapper argument slot edit");
    let wrapper_publication = published(publish(&core, &pin, &draft).await);
    assert_eq!(
        wrapper_publication.correspondence.expression_edits.last(),
        Some(&WorkflowEditLocation::ProcessWrapper {
            process,
            slot: wrapper_slot
        })
    );
    let (_, output) = run(&core, &pin, &wrapper_publication.definition, "wrapper-slot").await;
    assert_eq!(output, serde_json::json!("wrapper_edited"));
    core.host_artifacts()
        .release(pin)
        .await
        .expect("release pin");
    core.shutdown().await.expect("shutdown");
}
