//! Execution-site correlation between the VM compiler and the workflow lens.
//!
//! A workflow node's identity is keyed on an AST path, and the VM emits an
//! execution site carrying the same path for the instruction it compiled from
//! that position. Every runtime event a host shows on a graph rides on those
//! two sides agreeing. This file is that proof.
//!
//! It lived in `lash_vm`'s unit tests until the lens moved to this crate
//! (FIG-3033): a `[dev-dependencies]` edge from `lash_vm` back on
//! `lash-typescript` does not reach a unit test, because the lib-test target
//! compiles a second instance of `lash_vm` and its `Program` is then a
//! different type. The witnesses are re-authored over TypeScript, which is the
//! only cell language; where a witness relied on a LashVm-only form, the
//! header comment on the test says what replaced it.

use lash_typescript::parse;
use lash_vm::testing::ast_builders as b;
use lash_vm::testing::harness::{EchoHost, compiled_execution_sites, link_labeled};
use lash_vm::{
    AbilityOp, AbilityOutcome, AstRoot, Declaration, ExecutionHost, ExecutionHostError,
    ExecutionOutcome, LashVmExecutionObservation, Program, State, Value, WorkflowEffect,
    WorkflowNodeKind,
};

/// The language-neutral IR projection.
fn workflow_graph_from_program(program: &lash_vm::Program) -> lash_vm::WorkflowGraph {
    lash_vm::workflow_graph_from_program(program)
}

/// A `(kind, label, site)` triple for every execution site the compiler emitted,
/// ordered by site so the compiler's and the graph's lists are comparable.
fn compiled_site_descriptors(
    compiled: &lash_vm::CompiledProgram,
) -> Vec<(String, String, lash_sansio::WorkflowSiteRef)> {
    let mut sites = compiled_execution_sites(compiled)
        .into_iter()
        .map(|site| (site.kind.to_string(), site.label.clone(), site.site.clone()))
        .collect::<Vec<_>>();
    // By descriptor, then site: independent of instruction order or the
    // projector's kind order.
    sites.sort();
    sites
}

/// The same triples, read off the projected graph instead.
fn graph_site_descriptors(
    program: &Program,
) -> Vec<(String, String, lash_sansio::WorkflowSiteRef)> {
    let graph = workflow_graph_from_program(program);
    let mut sites = graph
        .nodes()
        .flat_map(|node| {
            node.execution_sites.iter().map(|site| {
                (
                    site.kind.to_string(),
                    site.label.clone(),
                    lash_sansio::WorkflowSiteRef::new(node.id.clone(), site.site_path.clone()),
                )
            })
        })
        .collect::<Vec<_>>();
    sites.sort();
    sites
}

fn parse_program(source: &str) -> Program {
    parse(source).expect("fixture parses")
}

#[tokio::test(flavor = "current_thread")]
async fn real_run_observations_use_projected_workflow_node_ids_directly() {
    #[derive(Default)]
    struct ObservationHost {
        node_ids: std::sync::Mutex<Vec<lash_vm::WorkflowNodeId>>,
    }

    impl ExecutionHost for ObservationHost {
        async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
            EchoHost.perform(op).await
        }

        fn observes_lash_vm_execution(&self) -> bool {
            true
        }

        fn observe_lash_vm_execution(&self, observation: LashVmExecutionObservation) {
            self.node_ids
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(observation.call_site.at.site.node_id);
        }
    }

    let source = r#"const first = await tools.echo({ value: "first" });
if (true) {
  await tools.echo({ value: first });
}
for (const item of [first]) {
  await tools.echo({ value: item });
  await tools.echo({ value: "second" });
}
finish(first);
"#;
    let linked = link_labeled(parse_program(source));
    let graph_node_ids = workflow_graph_from_program(linked.artifact.ir())
        .nodes()
        .map(|node| node.id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let compiled = lash_vm::testing::harness::compile_linked_main(&linked);
    let host = ObservationHost::default();

    let outcome = lash_vm::execute(&compiled, &mut State::new(), &host)
        .await
        .expect("workflow invocation should run");
    assert_eq!(
        outcome,
        ExecutionOutcome::Finished(Value::String("first".into()))
    );

    let observed_node_ids = host
        .node_ids
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(!observed_node_ids.is_empty(), "the run must observe nodes");
    assert!(
        observed_node_ids
            .iter()
            .all(|node_id| graph_node_ids.contains(node_id)),
        "every observed node id must already be a projected graph node id; graph={graph_node_ids:?}, observed={observed_node_ids:?}"
    );
}

/// The one process a fixture lifts.
///
/// FIG-2999: a process literal's declaration is named by the linker's lift
/// digest, so a fixture asks the linked module for the process it lifted rather
/// than spelling a name the source no longer carries.
fn only_lifted_process(linked: &lash_vm::LinkedModule) -> String {
    let mut names = linked
        .artifact
        .ir()
        .declarations
        .iter()
        .filter_map(|declaration| match declaration {
            lash_vm::Declaration::Process(process) => Some(process.name.to_string()),
            _ => None,
        });
    let name = names.next().expect("the module lifts one process");
    assert!(
        names.next().is_none(),
        "this fixture lifts exactly one process"
    );
    name
}

#[test]
fn lifted_process_spans_move_to_the_declaration_root() {
    let source = r#"const worker = async () => {
  await tools.echo({ value: "inside" });
  return "done";
};
finish(worker);
"#;
    let linked = link_labeled(parse_program(source));
    let (index, process) = linked
        .artifact
        .ir()
        .declarations
        .iter()
        .enumerate()
        .find_map(|(index, declaration)| {
            let Declaration::Process(process) = declaration else {
                return None;
            };
            Some((index as u32, process))
        })
        .expect("the process literal lifts");
    assert!(
        linked.artifact.ir().spans.is_empty(),
        "the durable artifact is span-free"
    );
    let process_spans = linked
        .spans()
        .iter()
        .filter(|(path, _)| path.root == AstRoot::Declaration(index))
        .collect::<Vec<_>>();

    assert!(
        process_spans.len() > 1,
        "the lifted declaration carries nested source provenance: {process_spans:?}"
    );
    assert!(
        process_spans.iter().any(|(path, span)| {
            path.steps.len() > 1 && source.get(span.start..span.end) == Some("return \"done\";")
        }),
        "the return statement keeps its exact source slice: {process_spans:?}; {process:?}"
    );
}

/// Over a real run, every observed site is on the branch actually taken.
///
/// Was the same test with a label naming each step; the projector's own node
/// names stand in for the titles, and the ordering — and the absence of the
/// skipped branch — is what the assertion turns on.
#[tokio::test(flavor = "current_thread")]
async fn real_runs_correlate_every_execution_site_to_the_selected_workflow_path() {
    #[derive(Default)]
    struct CorrelationHost {
        observations: std::sync::Mutex<Vec<LashVmExecutionObservation>>,
    }

    impl ExecutionHost for CorrelationHost {
        async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
            EchoHost.perform(op).await
        }

        fn observes_lash_vm_execution(&self) -> bool {
            true
        }

        fn observe_lash_vm_execution(&self, observation: LashVmExecutionObservation) {
            self.observations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(observation);
        }
    }

    let source = r#"const first = await tools.echo({ value: "first" });
let selected = null;
if (true) {
  selected = await tools.echo({ value: first });
} else {
  selected = await tools.echo({ value: "skipped" });
}
finish(selected);
"#;
    let linked = link_labeled(parse_program(source));
    let graph = workflow_graph_from_program(linked.artifact.ir());
    let compiled = lash_vm::testing::harness::compile_linked_main(&linked);

    let mut invocation_paths = Vec::new();
    for _ in 0..2 {
        let host = CorrelationHost::default();
        let outcome = lash_vm::execute(&compiled, &mut State::new(), &host)
            .await
            .expect("workflow invocation should run");
        assert_eq!(
            outcome,
            ExecutionOutcome::Finished(Value::String("first".into()))
        );

        let observations = host
            .observations
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let correlated = observations
            .iter()
            .map(|observation| {
                let at = &observation.call_site.at;
                let (node_id, occurrence) = (at.site.node_id.clone(), at.occurrence.get());
                assert!(
                    graph.nodes().any(|node| node.id == node_id),
                    "correlated node id must belong to the projected graph"
                );
                (observation, node_id, occurrence)
            })
            .collect::<Vec<_>>();

        let selected_path = correlated
            .iter()
            .filter(|(observation, _, _)| {
                matches!(
                    observation.fact,
                    lash_vm::LashVmExecutionFact::NodeStarted
                        | lash_vm::LashVmExecutionFact::BranchSelected { .. }
                )
            })
            .map(|(_, node_id, occurrence)| {
                let node = graph
                    .nodes()
                    .find(|node| node.id == *node_id)
                    .expect("correlated graph node");
                (node.display_name().to_string(), *occurrence)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            selected_path,
            vec![
                ("echo".to_string(), 1),
                ("if".to_string(), 1),
                ("update selected".to_string(), 1),
                ("finish".to_string(), 1),
            ],
            "correlated nodes should follow only the executed branch"
        );

        // Both branches write `selected`, so they project to nodes with the
        // same name and differ only in the expression they carry. Naming the
        // skipped one is what makes "only the executed branch" an assertion
        // rather than a coincidence of ordering.
        for (_, node_id, _) in &correlated {
            let node = graph
                .nodes()
                .find(|node| node.id == *node_id)
                .expect("correlated graph node");
            // The `if` container carries both branches, so only leaf nodes are
            // read for the branch they came from.
            if matches!(node.kind, WorkflowNodeKind::Container(_)) {
                continue;
            }
            assert!(
                !format!("{:?}", node.kind).contains("skipped"),
                "a node from the branch that was not taken was correlated: {node:?}"
            );
        }
        assert!(
            graph.nodes().any(|node| {
                !matches!(node.kind, WorkflowNodeKind::Container(_))
                    && format!("{:?}", node.kind).contains("skipped")
            }),
            "the projected graph must contain the branch that was not taken"
        );

        invocation_paths.push(selected_path);
    }

    assert_eq!(
        invocation_paths[0], invocation_paths[1],
        "each invocation must start with an independent correlation sequence"
    );
}

/// A nested assignment stays a plain step on both sides.
///
/// Was `execution_site_nested_assignment_label_remains_a_generic_step_on_both_sides`,
/// where a label on the outer assignment produced a `step` descriptor and the
/// point was that it was not promoted to the inner effect's descriptor. The
/// witness is stated as the same non-promotion without a label: the nested
/// assignment contributes no descriptor of its own, and the effect nested
/// inside it keeps its own descriptor at its own path, identically on both
/// sides.
#[test]
fn execution_site_nested_assignment_emits_no_descriptor_of_its_own() {
    let source = r#"let inner = null;
let outer = (inner = await sleep(1));
finish(outer);
"#;
    let linked = link_labeled(parse_program(source));
    let compiled = lash_vm::testing::harness::compile_linked_main(&linked);
    let compiler = compiled_site_descriptors(&compiled);

    assert!(
        !compiler
            .iter()
            .any(|(kind, _, _)| kind == "step" || kind == "assign"),
        "a nested assignment must not gain a descriptor of its own: {compiler:?}"
    );
    assert_eq!(
        compiler
            .iter()
            .map(|(kind, label, _)| (kind.as_str(), label.as_str()))
            .collect::<Vec<_>>(),
        [("sleep", "sleep for"), ("terminal", "result")],
    );
    assert_eq!(
        compiler,
        graph_site_descriptors(linked.artifact.ir()),
        "the projector must agree with the compiler, descriptor and path"
    );
}

/// A function call is projected under the descriptor the compiler assigned it.
#[test]
fn execution_site_function_call_is_projected_with_the_compiler_descriptor() {
    let source = r#"const identity = (value) => value;
finish(identity(1));
"#;
    let linked = link_labeled(parse_program(source));
    let compiled = lash_vm::testing::harness::compile_linked_main(&linked);
    let finish = lash_vm::workflow_node_id("main", &[1]);
    let compiler = compiled_site_descriptors(&compiled);
    assert_eq!(
        compiler
            .iter()
            .map(|(kind, label, site)| (kind.as_str(), label.as_str(), &site.node_id))
            .collect::<Vec<_>>(),
        vec![
            ("call", "function call", &finish),
            ("terminal", "result", &finish)
        ]
    );
    assert_eq!(graph_site_descriptors(linked.artifact.ir()), compiler);
}

/// A wrapped effect takes its graph name from the compiler's descriptor.
#[test]
fn execution_site_wrapped_effect_uses_the_compiler_descriptor_for_its_graph_name() {
    let source = "const slept = await sleep(1);\n";
    let program = parse_program(source);

    let graph = workflow_graph_from_program(&program);
    let node = graph.nodes().next().expect("wrapped sleep graph node");
    assert!(matches!(
        &node.kind,
        WorkflowNodeKind::Effect(WorkflowEffect::SleepFor { .. })
    ));
    assert_eq!(node.name, "sleep for");
    assert_eq!(
        node.execution_sites
            .iter()
            .map(|site| (site.kind.as_str(), site.label.as_str()))
            .collect::<Vec<_>>(),
        [("sleep", "sleep for")]
    );
}

#[test]
fn for_and_while_containers_have_matching_compiler_and_graph_sites() {
    let source = r#"for (const value of [1, 2]) {
  console.log(value);
}
while (false) {
  console.log("never");
}
"#;
    let linked = link_labeled(parse_program(source));
    let compiled = lash_vm::testing::harness::compile_linked_main(&linked);
    let compiler = compiled_site_descriptors(&compiled)
        .into_iter()
        .filter(|(kind, _, _)| kind == "loop")
        .collect::<Vec<_>>();
    let graph = graph_site_descriptors(linked.artifact.ir())
        .into_iter()
        .filter(|(kind, _, _)| kind == "loop")
        .collect::<Vec<_>>();

    assert_eq!(
        compiler
            .iter()
            .map(|(kind, label, site)| (kind.as_str(), label.as_str(), &site.node_id))
            .collect::<Vec<_>>(),
        vec![
            ("loop", "for", &lash_vm::workflow_node_id("main", &[0])),
            ("loop", "while", &lash_vm::workflow_node_id("main", &[1])),
        ]
    );
    assert_eq!(graph, compiler);
}

#[test]
fn reassigned_conditional_resource_sites_are_projected() {
    let source = r#"let selected = 0;
selected = true
  ? await tools.echo({ value: "then" })
  : await tools.err({ value: "else" });
finish(selected);
"#;
    let linked = link_labeled(parse_program(source));
    let compiled = lash_vm::testing::harness::compile_linked_main(&linked);
    let graph = workflow_graph_from_program(linked.artifact.ir());
    let graph_ids = graph
        .nodes()
        .map(|node| node.id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    for site in compiled_execution_sites(&compiled) {
        assert!(
            graph_ids.contains(&site.site.node_id),
            "runtime site must name a projected graph node: {site:?}"
        );
    }

    let resource_labels = graph
        .nodes()
        .flat_map(|node| node.execution_sites.iter())
        .filter(|site| site.kind == lash_sansio::ExecutionNodeKind::ResourceOperation)
        .map(|site| site.label.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        resource_labels,
        std::collections::BTreeSet::from(["echo", "err"]),
        "both conditional arms must be present in the projected graph"
    );
}

#[test]
fn direct_ir_process_sites_are_children_of_the_process_root() {
    let program = b::module(
        vec![b::process(
            "direct",
            Vec::new(),
            b::finish(b::module_call(
                &["tools"],
                "echo",
                vec![b::record(vec![("value", b::string("done"))])],
            )),
        )],
        Vec::new(),
    );
    let linked = link_labeled(program);
    let compiled = lash_vm::testing::harness::compile_linked_process_named(&linked, "direct")
        .expect("direct IR process should compile");
    let graph = workflow_graph_from_program(linked.artifact.ir());
    let process = graph.process("direct").expect("projected process");
    let graph_ids = graph
        .nodes()
        .map(|node| node.id.clone())
        .collect::<std::collections::BTreeSet<_>>();

    for site in compiled_execution_sites(&compiled) {
        assert!(
            graph_ids.contains(&site.site.node_id),
            "runtime site must name a projected graph node: {site:?}"
        );
        assert_ne!(
            site.site.node_id, process.id,
            "the process root is a non-executable container"
        );
    }
}

fn descriptor_pairs(compiled: &lash_vm::CompiledProgram) -> Vec<(String, String)> {
    compiled_execution_sites(compiled)
        .into_iter()
        .map(|site| (site.kind.to_string(), site.label.clone()))
        .collect()
}

/// A resource operation inside a process correlates to its workflow node.
///
/// Was `labeled_process_resource_operation_site_correlates_to_workflow_node`,
/// which put a label on the operation and asserted the graph node carried that
/// title. The witness is stated on the site path instead — which is the
/// correlation the test was really about: the site the VM emits for the
/// operation inside the process's `run` body names the node the projector
/// minted for it, at the path the lowerer's wrapper puts that body at
/// (FIG-3057). The title itself is asserted over a real run by the agent
/// scenarios.
#[test]
fn process_resource_operation_site_correlates_to_workflow_node() {
    let source = r#"const searchTest = async () => {
  /** @label Lookup app state */
  const result = await tools.echo({ value: { ok: true } });
  return result;
};
finish(1);
"#;
    let linked = link_labeled(parse_program(source));
    let compiled = lash_vm::testing::harness::compile_linked_process_named(
        &linked,
        &only_lifted_process(&linked),
    )
    .expect("process should compile");
    let site = compiled_execution_sites(&compiled)
        .into_iter()
        .find(|site| site.kind == lash_sansio::ExecutionNodeKind::ResourceOperation)
        .expect("resource operation execution site");

    let graph = workflow_graph_from_program(linked.artifact.ir());
    let graph_node = graph
        .nodes()
        .find(|node| node.id == site.site.node_id)
        .unwrap_or_else(|| {
            panic!(
                "runtime site {site:?} does not match graph nodes {:?}",
                graph
                    .nodes()
                    .map(|node| (&node.id, &node.name, &node.execution_sites))
                    .collect::<Vec<_>>()
            )
        });

    assert_eq!(graph_node.display_name(), "Lookup app state");
    assert!(graph_node.label.is_some());
    assert!(
        matches!(
            &graph_node.kind,
            WorkflowNodeKind::Call { operation, .. } if operation == "echo"
        ),
        "the correlated node is the resource operation itself: {:?}",
        graph_node.kind
    );
    assert!(
        !compiled_execution_sites(&compiled)
            .into_iter()
            .any(
                |candidate| candidate.kind == lash_sansio::ExecutionNodeKind::Step
                    && candidate.site.node_id == site.site.node_id
            ),
        "a resource operation should not also emit a generic step site at its own path"
    );
}

/// The compiler and the projector emit the same descriptor vocabulary.
///
/// The fixture reaches every descriptor TypeScript can spell, `step` — an
/// `@label` doc-comment title on a statement that bears no descriptor of its
/// own — included (FIG-3047). Two the Lash VM version also covered have no
/// TypeScript form and are therefore not asserted here: `yield`
/// and `sleep`/`sleep until`, neither of which the front end lowers to.
#[test]
fn execution_site_compiler_and_graph_emit_the_complete_descriptor_vocabulary() {
    let source = r#"const worker = async () => {
  const payload = await tools.echo({value:"ready"});
  return payload;
};
/** @label Plain value */
const plain = 1;
const result = await tools.echo({ value: plain });
const run = await processes.start({ definition: worker });
await sleep(1);
if (true) {
} else {
}
const identity = (value) => value;
const called = identity(1);
for (const value of [1]) {
  console.log(value);
}
while (false) {
}
finish(result);
"#;
    let linked = link_labeled(parse_program(source));
    let main = lash_vm::testing::harness::compile_linked_main(&linked);
    let process = lash_vm::testing::harness::compile_linked_process_named(
        &linked,
        &only_lifted_process(&linked),
    )
    .expect("descriptor process compiles");

    let mut compiler = descriptor_pairs(&main);
    compiler.extend(descriptor_pairs(&process));
    compiler.sort();
    compiler.dedup();
    let mut graph = workflow_graph_from_program(linked.artifact.ir())
        .nodes()
        .flat_map(|node| node.execution_sites.iter())
        .map(|site| (site.kind.to_string(), site.label.clone()))
        .collect::<Vec<_>>();
    graph.sort();
    graph.dedup();

    let mut expected = vec![
        ("branch".to_string(), "if".to_string()),
        ("call".to_string(), "function call".to_string()),
        ("loop".to_string(), "for".to_string()),
        ("loop".to_string(), "while".to_string()),
        ("resource_operation".to_string(), "echo".to_string()),
        ("resource_operation".to_string(), "start".to_string()),
        ("sleep".to_string(), "sleep for".to_string()),
        ("step".to_string(), "Plain value".to_string()),
        ("terminal".to_string(), "result".to_string()),
    ];
    expected.sort();
    assert_eq!(
        graph, expected,
        "workflow graph descriptor vocabulary drifted"
    );

    assert_eq!(
        compiler, expected,
        "compiler must emit sites only for the authored process body the graph projects"
    );
}

// ---- Exact sites, per-site occurrences and loop context (FIG-5575) ----

use lash_sansio::{WorkflowLoopFrame, WorkflowLoopPosition, WorkflowSitePath};
use lash_vm::{
    LashVmExecutionCallSite, ResourceOperationBatchLeaf, ResourceOperationBatchOutcome,
    ResourceOperationOutcome, VmExecutionStart, VmInstance, VmRequest, VmResume, VmRunConfig,
    VmStep,
};

/// What one run showed its host.
#[derive(Debug, PartialEq)]
struct SiteRun {
    /// Every observation, in order.
    observations: Vec<LashVmExecutionObservation>,
    /// The call site of every tool call the run issued, each once, with the
    /// `value` it echoed.
    calls: Vec<(Value, LashVmExecutionCallSite)>,
    end: String,
}

/// Runs `compiled` stepwise as an echoing host, parking on the tool calls
/// and batch leaves `park` names by their position among those the run issues and
/// reopening every parked run from its bytes on a pristine instance.
fn run_sites(compiled: lash_vm::CompiledProgram, park: impl Fn(usize) -> bool) -> (SiteRun, usize) {
    let program = std::sync::Arc::new(compiled);
    let mut config = VmRunConfig::new(
        lash_vm::ExecutionMode::Foreground,
        lash_vm::ExecutionBounds::new(
            lash_vm::ExecutionBound::Unbounded,
            lash_vm::ExecutionBound::Unbounded,
        ),
    );
    config.observe_execution = true;
    let mut instance = VmInstance::pristine();
    let mut observations = Vec::new();
    let mut calls = Vec::new();
    let mut parks = 0;
    let mut held = None::<Value>;
    let mut held_batch = None::<Vec<LashVmExecutionCallSite>>;
    let mut step = instance
        .start(program.clone(), VmExecutionStart::Session, config.clone())
        .expect("the run starts");
    let end = loop {
        step = match step {
            VmStep::Suspended(suspended) => {
                observations.extend(suspended.observations);
                let resume = match suspended.request {
                    VmRequest::Effect(AbilityOp::ResourceOperation(operation)) => {
                        let value = operation
                            .args
                            .first()
                            .and_then(Value::as_record)
                            .and_then(|record| record.get("value"))
                            .cloned()
                            .unwrap_or(Value::Null);
                        match held.take() {
                            Some(held) => VmResume::Effect(Ok(AbilityOutcome::Value(held))),
                            None => {
                                let position = calls.len();
                                calls.push((
                                    value.clone(),
                                    *operation.call_site.expect("a tool call names its site"),
                                ));
                                if park(position) {
                                    held = Some(value);
                                    VmResume::Park
                                } else {
                                    VmResume::Effect(Ok(AbilityOutcome::Value(value)))
                                }
                            }
                        }
                    }
                    // A batch parks when `park` names any of its leaves, and
                    // the batch issued again names every leaf's site as the
                    // parked one did.
                    VmRequest::Effect(AbilityOp::ResourceOperationBatch(batch)) => {
                        let leaves = batch
                            .leaves
                            .into_iter()
                            .map(|leaf| match leaf {
                                ResourceOperationBatchLeaf::Operation(operation) => (
                                    operation
                                        .args
                                        .first()
                                        .and_then(Value::as_record)
                                        .and_then(|record| record.get("value"))
                                        .cloned()
                                        .unwrap_or(Value::Null),
                                    *operation.call_site.expect("a tool leaf names its site"),
                                ),
                                ResourceOperationBatchLeaf::Timer(sleep) => {
                                    panic!("unexpected timer leaf {sleep:?}")
                                }
                            })
                            .collect::<Vec<_>>();
                        let results = || {
                            AbilityOutcome::ResourceOperationBatch(
                                ResourceOperationBatchOutcome::AllResults(
                                    leaves
                                        .iter()
                                        .map(|(value, _)| {
                                            ResourceOperationOutcome::Value(value.clone())
                                        })
                                        .collect(),
                                ),
                            )
                        };
                        match held_batch.take() {
                            Some(parked) => {
                                assert_eq!(
                                    leaves.iter().map(|(_, site)| site).collect::<Vec<_>>(),
                                    parked.iter().collect::<Vec<_>>(),
                                    "a batch issued again names the sites it parked with"
                                );
                                VmResume::Effect(Ok(results()))
                            }
                            None => {
                                let first = calls.len();
                                calls.extend(leaves.iter().cloned());
                                if (first..calls.len()).any(&park) {
                                    held_batch =
                                        Some(leaves.into_iter().map(|(_, site)| site).collect());
                                    VmResume::Park
                                } else {
                                    VmResume::Effect(Ok(results()))
                                }
                            }
                        }
                    }
                    VmRequest::Effect(AbilityOp::Finish(value)) => {
                        VmResume::Effect(Ok(AbilityOutcome::Value(value)))
                    }
                    VmRequest::Effect(op) => panic!("unexpected effect {op:?}"),
                    VmRequest::CancelCheckpoint(_) => {
                        VmResume::CancelCheckpoint { cancelled: false }
                    }
                    VmRequest::Boundary => VmResume::Continue,
                    VmRequest::ParkDeclined(error) => panic!("park declined: {error}"),
                };
                instance
                    .resume(resume)
                    .expect("the resume answers its request")
            }
            VmStep::Parked(parked) => {
                observations.extend(parked.observations);
                parks += 1;
                let bytes = parked
                    .continuation
                    .to_bytes()
                    .expect("a parked continuation encodes");
                instance = VmInstance::pristine();
                let continuation = instance
                    .open_continuation(&bytes)
                    .expect("a parked continuation reopens on a fresh instance");
                instance
                    .start(
                        program.clone(),
                        VmExecutionStart::Continuation(Box::new(continuation)),
                        config.clone(),
                    )
                    .expect("the continuation resumes")
            }
            VmStep::Complete(complete) => {
                observations.extend(complete.observations);
                break format!("{:?}", complete.outcome);
            }
            VmStep::GuestError(error) => {
                observations.extend(error.observations);
                break format!("guest error {:?}", error.failure.error);
            }
        };
    };
    (
        SiteRun {
            observations,
            calls,
            end,
        },
        parks,
    )
}

fn compile_main(source: &str) -> (lash_vm::WorkflowGraph, lash_vm::CompiledProgram) {
    let linked = link_labeled(parse_program(source));
    (
        workflow_graph_from_program(linked.artifact.ir()),
        lash_vm::testing::harness::compile_linked_main(&linked),
    )
}

/// The loop context of a call as `(activation, position)` per enclosing
/// loop, outermost first.
fn loop_positions(call: &LashVmExecutionCallSite) -> Vec<(u64, WorkflowLoopPosition)> {
    call.at
        .loops
        .iter()
        .map(|frame| (frame.activation, frame.position))
        .collect()
}

/// Every site the compiler emits and every site the document lists carries
/// the exact expression it stands for, and the two lists are one set.
fn assert_sites_resolve_in_the_document(
    graph: &lash_vm::WorkflowGraph,
    compiled: &lash_vm::CompiledProgram,
) {
    let mut graph_sites = Vec::new();
    for node in graph.nodes() {
        let statement = lash_vm::workflow_node_statement(node);
        for site in &node.execution_sites {
            let expression = statement
                .at_slots(site.site_path.slots.slots())
                .unwrap_or_else(|| panic!("site {site:?} resolves in its node's statement"));
            if site.site_path.role.is_none() {
                let (kind, label) = lash_vm::execution_site_descriptor(expression)
                    .unwrap_or_else(|| panic!("site {site:?} names an executable expression"));
                assert_eq!((kind, label.as_ref()), (site.kind, site.label.as_str()));
            }
            graph_sites.push(lash_vm::LashVmExecutionSite {
                site: lash_sansio::WorkflowSiteRef::new(node.id.clone(), site.site_path.clone()),
                kind: site.kind,
                label: site.label.clone(),
            });
        }
    }
    let unique = graph_sites
        .iter()
        .map(|site| &site.site)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        unique.len(),
        graph_sites.len(),
        "a site address names one site"
    );
    for site in compiled_execution_sites(compiled) {
        assert!(
            graph_sites.contains(site),
            "compiled site {site:?} is a site of the document"
        );
    }
}

/// FIG-5575: two calls in one statement are two sites, in the document and
/// in what the run reports, even when the calls are identical. Before, both
/// named their node's path and collapsed to one site.
#[test]
fn two_calls_in_one_statement_are_two_sites_in_the_document_and_in_events() {
    let (graph, compiled) = compile_main(
        r#"finish([await tools.echo({ value: "a" }), await tools.echo({ value: "a" })]);
"#,
    );
    assert_sites_resolve_in_the_document(&graph, &compiled);
    let finish = graph
        .nodes()
        .find(|node| matches!(node.kind, WorkflowNodeKind::Terminal { .. }))
        .expect("finish node");
    let operations = finish
        .execution_sites
        .iter()
        .filter(|site| site.kind == lash_vm::RESOURCE_OPERATION_EXECUTION_SITE_KIND)
        .collect::<Vec<_>>();
    assert_eq!(operations.len(), 2, "{:?}", finish.execution_sites);
    assert_ne!(operations[0].site_path, operations[1].site_path);

    let (run, _) = run_sites(compiled, |_| false);
    let [(_, first), (_, second)] = run.calls.as_slice() else {
        panic!("two calls: {:?}", run.calls);
    };
    assert_eq!(first.at.site.node_id, finish.id);
    assert_eq!(second.at.site.node_id, finish.id);
    assert_eq!(
        [&first.at.site.site_path, &second.at.site.site_path],
        [&operations[0].site_path, &operations[1].site_path],
        "each call reports the document's site for it, in evaluation order"
    );
    assert_eq!(
        (first.at.occurrence.get(), second.at.occurrence.get()),
        (1, 1),
        "occurrences count per site, not per node"
    );
    let started = run
        .observations
        .iter()
        .filter(|observation| {
            observation.fact == lash_vm::LashVmExecutionFact::NodeStarted
                && observation.call_site.kind == lash_vm::RESOURCE_OPERATION_EXECUTION_SITE_KIND
        })
        .map(|observation| {
            let at = &observation.call_site.at;
            (at.site.site_path.clone(), at.occurrence.get())
        })
        .collect::<Vec<_>>();
    assert_eq!(
        started,
        vec![
            (operations[0].site_path.clone(), 1),
            (operations[1].site_path.clone(), 1)
        ]
    );
}

const NESTED_LOOPS: &str = r#"for (const a of [1, 2]) {
  for (const b of [1, 2]) {
    await tools.echo({ value: b });
  }
}
finish("done");
"#;

/// A loop reentered by an outer iteration is a new activation, a call's
/// occurrence counts across all of them, and each call carries the stack of
/// loops it ran in.
#[test]
fn nested_loops_give_each_occurrence_its_activation_and_iteration() {
    use WorkflowLoopPosition::Body;
    let (graph, compiled) = compile_main(NESTED_LOOPS);
    assert_sites_resolve_in_the_document(&graph, &compiled);
    let (run, _) = run_sites(compiled, |_| false);
    assert_eq!(
        run.calls
            .iter()
            .map(|(_, call)| (call.at.occurrence.get(), loop_positions(call)))
            .collect::<Vec<_>>(),
        vec![
            (1, vec![(1, Body(1)), (2, Body(1))]),
            (2, vec![(1, Body(1)), (2, Body(2))]),
            (3, vec![(1, Body(2)), (3, Body(1))]),
            (4, vec![(1, Body(2)), (3, Body(2))]),
        ]
    );
    let loops = graph
        .nodes()
        .filter(|node| {
            node.execution_sites
                .iter()
                .any(|site| site.kind == lash_sansio::ExecutionNodeKind::Loop)
        })
        .map(|node| node.id.clone())
        .collect::<Vec<_>>();
    let [outer, inner] = loops.as_slice() else {
        panic!("two loop nodes: {loops:?}");
    };
    for (_, call) in &run.calls {
        let frames: Vec<&WorkflowLoopFrame> = call.at.loops.iter().collect();
        assert_eq!(
            [&frames[0].site.node_id, &frames[1].site.node_id],
            [outer, inner],
            "a frame names its loop's own site"
        );
        assert_eq!(frames[0].site.site_path, WorkflowSitePath::default());
    }
}

/// A run that parks on a call inside the inner loop and resumes from its
/// bytes reports exactly what a run that never parked reports: counters and
/// loop context ride the continuation.
#[test]
fn parking_inside_the_inner_loop_keeps_occurrence_activation_and_iteration() {
    let (_, compiled) = compile_main(NESTED_LOOPS);
    let (straight, straight_parks) = run_sites(compiled.clone(), |_| false);
    assert_eq!(straight_parks, 0);
    assert_eq!(straight.calls.len(), 4);
    for parked_at in 0..4 {
        let (parked, parks) = run_sites(compiled.clone(), |position| position == parked_at);
        assert_eq!(parks, 1, "the run parks on call {parked_at}");
        assert_eq!(parked, straight, "parked on call {parked_at}");
    }
    let (always, parks) = run_sites(compiled, |_| true);
    assert_eq!(parks, 4);
    assert_eq!(always, straight);
}

/// A `while` condition's evaluations are checks, apart from the body
/// iterations they admit: the last check, which reads false, has no body of
/// its number.
#[test]
fn a_while_condition_check_is_distinct_from_a_body_iteration() {
    use WorkflowLoopPosition::{Body, Check};
    let (graph, compiled) = compile_main(
        r#"let n = 0;
while (await tools.echo({ value: n < 2 })) {
  n = n + 1;
  await tools.echo({ value: "body" });
}
finish(n);
"#,
    );
    assert_sites_resolve_in_the_document(&graph, &compiled);
    let (run, _) = run_sites(compiled.clone(), |_| false);
    let positions = run
        .calls
        .iter()
        .map(|(value, call)| {
            (
                matches!(value, Value::Bool(_)),
                call.at.occurrence.get(),
                loop_positions(call),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        positions,
        vec![
            (true, 1, vec![(1, Check(1))]),
            (false, 1, vec![(1, Body(1))]),
            (true, 2, vec![(1, Check(2))]),
            (false, 2, vec![(1, Body(2))]),
            (true, 3, vec![(1, Check(3))]),
        ]
    );
    let (parked, parks) = run_sites(compiled, |_| true);
    assert_eq!(parks, 5);
    assert_eq!(parked, run, "the final false check parks and resumes too");
}

/// `break`, a caught throw and a return out of a loop each leave the loop:
/// what runs next is in the loops that still enclose it, and a loop entered
/// again is a new activation.
#[test]
fn leaving_a_loop_abruptly_leaves_its_frame() {
    use WorkflowLoopPosition::Body;
    let (_, compiled) = compile_main(
        r#"const first = (items) => {
  for (const item of items) {
    return item;
  }
  return null;
};
for (const a of [1, 2]) {
  try {
    for (const b of [1, 2]) {
      await tools.echo({ value: "inner" });
      throw "stop";
    }
  } catch (error) {
    await tools.echo({ value: "caught" });
  }
  for (const c of [1, 2]) {
    break;
  }
  await tools.echo({ value: first(["after"]) });
}
finish("done");
"#,
    );
    let (run, _) = run_sites(compiled.clone(), |_| false);
    let seen = run
        .calls
        .iter()
        .map(|(value, call)| (value.to_string(), loop_positions(call)))
        .collect::<Vec<_>>();
    // Activations in entry order: outer 1; then per outer iteration the
    // throwing loop, the breaking loop and the returning loop.
    assert_eq!(
        seen,
        vec![
            ("inner".to_string(), vec![(1, Body(1)), (2, Body(1))]),
            ("caught".to_string(), vec![(1, Body(1))]),
            ("after".to_string(), vec![(1, Body(1))]),
            ("inner".to_string(), vec![(1, Body(2)), (5, Body(1))]),
            ("caught".to_string(), vec![(1, Body(2))]),
            ("after".to_string(), vec![(1, Body(2))]),
        ]
    );
    let (parked, _) = run_sites(compiled, |_| true);
    assert_eq!(parked, run);
}

// ---- Deferred handles keep the context they were minted in (FIG-5670) ----

const HANDLES_COLLECTED_IN_NESTED_LOOPS: &str = r#"const hs = [];
for (const a of [1, 2]) {
  for (const b of [1, 2]) {
    hs.push(tools.echo({ value: a * 10 + b }));
  }
}
await tools.echo({ value: "gate" });
finish(await Promise.all(hs));
"#;

/// A loop context as `(activation, position)` per enclosing loop, outermost
/// first.
type LoopPositions = Vec<(u64, WorkflowLoopPosition)>;

/// The occurrence and loop context of every start and completion of a tool
/// call the run reported, in order.
fn tool_transitions(run: &SiteRun) -> Vec<(&'static str, u64, LoopPositions)> {
    run.observations
        .iter()
        .filter_map(|observation| {
            let name = match observation.fact {
                lash_vm::LashVmExecutionFact::NodeStarted => "started",
                lash_vm::LashVmExecutionFact::NodeCompleted => "completed",
                _ => return None,
            };
            let call = &observation.call_site;
            (call.kind == lash_vm::RESOURCE_OPERATION_EXECUTION_SITE_KIND).then(|| {
                (
                    name,
                    call.at.occurrence.get(),
                    call.at
                        .loops
                        .iter()
                        .map(|frame| (frame.activation, frame.position))
                        .collect(),
                )
            })
        })
        .collect()
}

/// A tool handle minted in a loop and awaited after it reports the
/// occurrence, activation and iteration it was minted in: at dispatch, and
/// at the start and completion of its node.
#[test]
fn handles_awaited_after_their_loops_report_the_iteration_that_minted_them() {
    use WorkflowLoopPosition::Body;
    let (_, compiled) = compile_main(HANDLES_COLLECTED_IN_NESTED_LOOPS);
    let (run, _) = run_sites(compiled, |_| false);
    assert!(run.end.contains("Finished"), "{}", run.end);
    let minted = vec![
        (1, vec![(1, Body(1)), (2, Body(1))]),
        (2, vec![(1, Body(1)), (2, Body(2))]),
        (3, vec![(1, Body(2)), (3, Body(1))]),
        (4, vec![(1, Body(2)), (3, Body(2))]),
    ];
    let [(_, gate), leaves @ ..] = run.calls.as_slice() else {
        panic!("the gate call and the batch: {:?}", run.calls);
    };
    assert!(
        gate.at.loops.is_empty(),
        "a call after the loops is in none"
    );
    assert_eq!(
        leaves
            .iter()
            .map(|(_, call)| (call.at.occurrence.get(), loop_positions(call)))
            .collect::<Vec<_>>(),
        minted,
        "each leaf of the batch, in handle order"
    );
    let mut transitions = vec![("started", 1, vec![]), ("completed", 1, vec![])];
    for name in ["started", "completed"] {
        transitions.extend(
            minted
                .iter()
                .map(|(occurrence, loops)| (name, *occurrence, loops.clone())),
        );
    }
    assert_eq!(
        tool_transitions(&run),
        transitions,
        "the gate, then every leaf starts and every leaf completes"
    );
}

/// A run that parks between minting the handles and awaiting them, or on the
/// batch that awaits them, and resumes from its bytes reports exactly what a
/// run that never parked reports: each handle's occurrence and loop context
/// ride the continuation, and the batch issued again starts no node twice.
#[test]
fn parked_handles_keep_the_iteration_that_minted_them_across_a_continuation() {
    let (_, compiled) = compile_main(HANDLES_COLLECTED_IN_NESTED_LOOPS);
    let (straight, straight_parks) = run_sites(compiled.clone(), |_| false);
    assert_eq!(straight_parks, 0);
    assert_eq!(straight.calls.len(), 5, "the gate and four leaves");
    let (gate_parked, parks) = run_sites(compiled.clone(), |position| position == 0);
    assert_eq!(parks, 1, "the run parks on the gate, holding four handles");
    assert_eq!(gate_parked, straight);
    let (batch_parked, parks) = run_sites(compiled.clone(), |position| position == 3);
    assert_eq!(parks, 1, "the run parks on the batch");
    assert_eq!(batch_parked, straight);
    let (always, parks) = run_sites(compiled, |_| true);
    assert_eq!(parks, 2);
    assert_eq!(always, straight);
}

/// A handle awaited inside a loop other than the one that minted it keeps
/// the context it was minted in, and so does one awaited in the same loop a
/// later iteration: the awaiting loop names only what is issued in it.
#[test]
fn a_handle_awaited_in_another_loop_keeps_the_loop_that_minted_it() {
    use WorkflowLoopPosition::Body;
    let (_, compiled) = compile_main(
        r#"const hs = [];
for (const a of [1, 2]) {
  hs.push(tools.echo({ value: a }));
}
for (const h of hs) {
  await h;
  await tools.echo({ value: "inside" });
}
finish("done");
"#,
    );
    let (run, _) = run_sites(compiled.clone(), |_| false);
    assert!(run.end.contains("Finished"), "{}", run.end);
    let seen = run
        .calls
        .iter()
        .map(|(value, call)| {
            (
                value.to_string(),
                call.at.occurrence.get(),
                loop_positions(call),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        seen,
        vec![
            ("1".to_string(), 1, vec![(1, Body(1))]),
            ("inside".to_string(), 1, vec![(2, Body(1))]),
            ("2".to_string(), 2, vec![(1, Body(2))]),
            ("inside".to_string(), 2, vec![(2, Body(2))]),
        ]
    );
    let (parked, parks) = run_sites(compiled, |_| true);
    assert_eq!(parks, 4);
    assert_eq!(parked, run);
}

/// A handle's occurrence is resume state the next segment trusts: a
/// continuation whose pending operation names an occurrence its site never
/// counted, or a loop the run never began, is refused when it resumes.
#[test]
fn a_continuation_refuses_a_handle_its_run_cannot_have_minted() {
    let (_, compiled) = compile_main(HANDLES_COLLECTED_IN_NESTED_LOOPS);
    let program = std::sync::Arc::new(compiled);
    let config = VmRunConfig::new(
        lash_vm::ExecutionMode::Foreground,
        lash_vm::ExecutionBounds::new(
            lash_vm::ExecutionBound::Unbounded,
            lash_vm::ExecutionBound::Unbounded,
        ),
    );
    let mut instance = VmInstance::pristine();
    let mut step = instance
        .start(program.clone(), VmExecutionStart::Session, config.clone())
        .expect("the run starts");
    let parked = loop {
        step = match step {
            VmStep::Suspended(suspended) => {
                let resume = match suspended.request {
                    VmRequest::Effect(AbilityOp::ResourceOperation(_)) => VmResume::Park,
                    VmRequest::CancelCheckpoint(_) => {
                        VmResume::CancelCheckpoint { cancelled: false }
                    }
                    VmRequest::Boundary => VmResume::Continue,
                    other => panic!("unexpected request {other:?}"),
                };
                instance.resume(resume).expect("the resume answers")
            }
            VmStep::Parked(parked) => break parked,
            _ => panic!("the run parks on the gate"),
        };
    };
    let wire: serde_json::Value = serde_json::from_slice(
        &parked
            .continuation
            .to_bytes()
            .expect("a parked continuation encodes"),
    )
    .expect("continuation json");
    let handle = wire["pending_tools"]
        .as_object()
        .expect("pending operations")
        .keys()
        .next()
        .expect("a live handle")
        .clone();
    let refused = |edit: &dyn Fn(&mut serde_json::Value)| {
        let mut wire = wire.clone();
        edit(&mut wire["pending_tools"][handle.as_str()]["occurrence"]);
        let bytes = serde_json::to_vec(&wire).expect("continuation bytes");
        let mut instance = VmInstance::pristine();
        let continuation = instance
            .open_continuation(&bytes)
            .expect("the continuation decodes");
        match instance.start(
            program.clone(),
            VmExecutionStart::Continuation(Box::new(continuation)),
            config.clone(),
        ) {
            Ok(step) => panic!("the continuation resumed: {step:?}"),
            Err(error) => error.to_string(),
        }
    };
    for (edit, reason) in [
        (
            &(|occurrence: &mut serde_json::Value| occurrence["occurrence"] = 5.into())
                as &dyn Fn(&mut serde_json::Value),
            "its site never counted that occurrence",
        ),
        (
            &|occurrence: &mut serde_json::Value| occurrence["occurrence"] = 2.into(),
            "another pending operation holds it",
        ),
        (
            &|occurrence: &mut serde_json::Value| {
                occurrence["loops"][1]["activation"] = 9.into();
            },
            "a loop it names was never begun inside the loop enclosing it",
        ),
        (
            &|occurrence: &mut serde_json::Value| *occurrence = serde_json::Value::Null,
            "its instruction's execution site has no occurrence",
        ),
    ] {
        let error = refused(edit);
        assert!(error.contains(reason), "{reason}: {error}");
    }
}
