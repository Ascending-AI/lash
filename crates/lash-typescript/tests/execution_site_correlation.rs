//! Execution-site correlation between the VM compiler and the workflow lens.
//!
//! A workflow node's identity is keyed on an AST path, and the VM emits an
//! execution site carrying the same path for the instruction it compiled from
//! that position. Every runtime event a host shows on a graph rides on those
//! two sides agreeing. This file is that proof.
//!
//! It lived in `lashlang`'s unit tests until the lens moved to this crate
//! (FIG-3033): a `[dev-dependencies]` edge from `lashlang` back on
//! `lash-typescript` does not reach a unit test, because the lib-test target
//! compiles a second instance of `lashlang` and its `Program` is then a
//! different type. The witnesses are re-authored over TypeScript, which is the
//! only cell language; where a witness relied on a Lashlang-only form, the
//! header comment on the test says what replaced it.

use lash_typescript::parse;
use lashlang::testing::ast_builders as b;
use lashlang::testing::harness::{EchoHost, compiled_execution_sites, link_labeled};
use lashlang::{
    AbilityOp, AbilityResult, AstRoot, Declaration, ExecutionHost, ExecutionHostError,
    ExecutionOutcome, LashlangExecutionObservation, Program, State, Value, WorkflowEffectKind,
    WorkflowNodeKind,
};

/// The language-neutral IR projection, with TypeScript opaque-statement text.
fn workflow_graph_from_program(program: &lashlang::Program) -> lashlang::WorkflowGraph {
    lashlang::workflow_graph_from_program(
        program,
        &lash_typescript::workflow_graph::TypeScriptStatementText,
    )
}

/// A `(kind, label, path)` triple for every execution site the compiler emitted,
/// ordered by path so the compiler's and the graph's lists are comparable.
fn compiled_site_descriptors(
    compiled: &lashlang::CompiledProgram,
) -> Vec<(String, String, Vec<u32>)> {
    let mut sites = compiled_execution_sites(compiled)
        .into_iter()
        .map(|site| {
            (
                site.node_kind.to_string(),
                site.label.clone(),
                site.workflow_site.path.clone(),
            )
        })
        .collect::<Vec<_>>();
    // Path first, then the descriptor: sites sharing a path compare as a set,
    // independent of instruction order or the projector's kind order.
    sites.sort_by(|left, right| left.2.cmp(&right.2).then_with(|| left.cmp(right)));
    sites
}

/// The same triples, read off the projected graph instead.
fn graph_site_descriptors(program: &Program) -> Vec<(String, String, Vec<u32>)> {
    let mut sites = workflow_graph_from_program(program)
        .nodes()
        .flat_map(|node| node.execution_sites.iter())
        .map(|site| (site.kind.to_string(), site.label.clone(), site.path.clone()))
        .collect::<Vec<_>>();
    // Path first, then the descriptor: sites sharing a path compare as a set,
    // independent of instruction order or the projector's kind order.
    sites.sort_by(|left, right| left.2.cmp(&right.2).then_with(|| left.cmp(right)));
    sites
}

fn parse_program(source: &str) -> Program {
    parse(source).expect("fixture parses")
}

#[tokio::test(flavor = "current_thread")]
async fn real_run_observations_use_projected_workflow_node_ids_directly() {
    #[derive(Default)]
    struct ObservationHost {
        node_ids: std::sync::Mutex<Vec<String>>,
    }

    impl ExecutionHost for ObservationHost {
        async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
            EchoHost.perform(op).await
        }

        fn observe_lashlang_execution(&self, observation: LashlangExecutionObservation) {
            let site = match observation {
                LashlangExecutionObservation::NodeStarted { site, .. }
                | LashlangExecutionObservation::ChildProcessWaiting { site, .. }
                | LashlangExecutionObservation::NodeResumed { site, .. }
                | LashlangExecutionObservation::NodeCompleted { site, .. }
                | LashlangExecutionObservation::NodeFailed { site, .. }
                | LashlangExecutionObservation::BranchSelected { site, .. }
                | LashlangExecutionObservation::ChildStarted { site, .. } => site,
            };
            self.node_ids
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(site.node_id);
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
        .map(|node| node.id.to_string())
        .collect::<std::collections::BTreeSet<_>>();
    let compiled = lashlang::testing::harness::compile_linked_main(&linked);
    let host = ObservationHost::default();

    let outcome = lashlang::execute(&compiled, &mut State::new(), &host)
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
fn only_lifted_process(linked: &lashlang::LinkedModule) -> String {
    let mut names = linked
        .artifact
        .ir()
        .declarations
        .iter()
        .filter_map(|declaration| match declaration {
            lashlang::Declaration::Process(process) => Some(process.name.to_string()),
            _ => None,
        });
    let name = names.next().expect("the module lifts one process");
    assert!(
        names.next().is_none(),
        "this fixture lifts exactly one process"
    );
    name
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
  await processes.emit({ value: result });
  return result;
};
finish(1);
"#;
    let linked = link_labeled(parse_program(source));
    let compiled = lashlang::testing::harness::compile_linked_process_named(
        &linked,
        &only_lifted_process(&linked),
    )
    .expect("process should compile");
    let site = compiled_execution_sites(&compiled)
        .into_iter()
        .find(|site| site.node_kind == lash_sansio::ExecutionNodeKind::ResourceOperation)
        .expect("resource operation execution site");

    let graph = workflow_graph_from_program(linked.artifact.ir());
    let graph_node = graph
        .nodes()
        .find(|node| node.id.as_str() == site.node_id)
        .unwrap_or_else(|| {
            panic!(
                "runtime site {site:?} does not match graph nodes {:?}",
                graph
                    .nodes()
                    .map(|node| (&node.id, &node.name, &node.execution_sites))
                    .collect::<Vec<_>>()
            )
        });

    assert_eq!(graph_node.name, "Lookup app state");
    assert_eq!(
        graph_node.name_source,
        lashlang::WorkflowNodeNameSource::Label
    );
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
                |candidate| candidate.node_kind == lash_sansio::ExecutionNodeKind::Step
                    && candidate.workflow_site.path == site.workflow_site.path
            ),
        "a resource operation should not also emit a generic step site at its own path"
    );
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
        observations: std::sync::Mutex<Vec<LashlangExecutionObservation>>,
    }

    impl ExecutionHost for CorrelationHost {
        async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
            EchoHost.perform(op).await
        }

        fn observe_lashlang_execution(&self, observation: LashlangExecutionObservation) {
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
    let compiled = lashlang::testing::harness::compile_linked_main(&linked);

    let mut invocation_paths = Vec::new();
    for _ in 0..2 {
        let host = CorrelationHost::default();
        let outcome = lashlang::execute(&compiled, &mut State::new(), &host)
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
                let (site, occurrence) = match observation {
                    LashlangExecutionObservation::NodeStarted { site, occurrence }
                    | LashlangExecutionObservation::ChildProcessWaiting {
                        site, occurrence, ..
                    }
                    | LashlangExecutionObservation::NodeResumed { site, occurrence }
                    | LashlangExecutionObservation::NodeCompleted { site, occurrence }
                    | LashlangExecutionObservation::NodeFailed {
                        site, occurrence, ..
                    }
                    | LashlangExecutionObservation::BranchSelected {
                        site, occurrence, ..
                    }
                    | LashlangExecutionObservation::ChildStarted {
                        site, occurrence, ..
                    } => (site, *occurrence),
                };
                let node_id = lashlang::WorkflowNodeId::new(site.node_id.clone());
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
                    observation,
                    LashlangExecutionObservation::NodeStarted { .. }
                        | LashlangExecutionObservation::BranchSelected { .. }
                )
            })
            .map(|(_, node_id, occurrence)| {
                let node = graph
                    .nodes()
                    .find(|node| node.id == *node_id)
                    .expect("correlated graph node");
                (node.name.clone(), *occurrence)
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
    let compiled = lashlang::testing::harness::compile_linked_main(&linked);
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
    let compiled = lashlang::testing::harness::compile_linked_main(&linked);
    let expected = vec![
        ("call".to_string(), "function call".to_string(), vec![1]),
        ("terminal".to_string(), "result".to_string(), vec![1]),
    ];
    assert_eq!(compiled_site_descriptors(&compiled), expected);
    assert_eq!(graph_site_descriptors(linked.artifact.ir()), expected);
}

/// The compiler and the projector emit the same descriptor vocabulary.
///
/// The fixture reaches every descriptor TypeScript can spell, `step` — an
/// `@label` doc-comment title on a statement that bears no descriptor of its
/// own — included (FIG-3047). Two the Lashlang version also covered have no
/// TypeScript form and are therefore not asserted here: `process_event`/`yield`
/// and `sleep`/`sleep until`, neither of which the front end lowers to.
#[test]
fn execution_site_compiler_and_graph_emit_the_complete_descriptor_vocabulary() {
    let source = r#"const worker = async () => {
  const payload = await waitSignal("ready");
  await processes.emit({ value: payload });
  return payload;
};
/** @label Plain value */
const plain = 1;
const result = await tools.echo({ value: plain });
const run = await processes.start({ definition: worker });
await sleep(1);
await processes.signal({ handle: run, name: "ready", payload: null });
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
    let main = lashlang::testing::harness::compile_linked_main(&linked);
    let process = lashlang::testing::harness::compile_linked_process_named(
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
        // FIG-2999: starting, signalling and yielding are leaf tools now, so
        // they carry the one `resource_operation` descriptor kind instead of
        // the `child_process`, `signal` and `process_event` kinds the deleted
        // special forms had.
        ("resource_operation".to_string(), "echo".to_string()),
        ("resource_operation".to_string(), "emit".to_string()),
        ("resource_operation".to_string(), "signal".to_string()),
        ("resource_operation".to_string(), "start".to_string()),
        ("sleep".to_string(), "sleep for".to_string()),
        ("step".to_string(), "Plain value".to_string()),
        ("terminal".to_string(), "result".to_string()),
        ("wait".to_string(), "wait_signal".to_string()),
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

    let branch = compiled_execution_sites(&main)
        .into_iter()
        .find(|site| site.node_kind == lash_sansio::ExecutionNodeKind::Branch)
        .expect("compiled branch site");
    assert!(
        branch.branch.is_some(),
        "branch descriptor must use branch_site"
    );
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
        WorkflowNodeKind::Effect {
            effect: WorkflowEffectKind::SleepFor,
            ..
        }
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
    let compiled = lashlang::testing::harness::compile_linked_main(&linked);
    let compiler = compiled_site_descriptors(&compiled)
        .into_iter()
        .filter(|(kind, _, _)| kind == "loop")
        .collect::<Vec<_>>();
    let graph = graph_site_descriptors(linked.artifact.ir())
        .into_iter()
        .filter(|(kind, _, _)| kind == "loop")
        .collect::<Vec<_>>();

    assert_eq!(
        compiler,
        vec![
            ("loop".to_string(), "for".to_string(), vec![0]),
            ("loop".to_string(), "while".to_string(), vec![1]),
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
    let compiled = lashlang::testing::harness::compile_linked_main(&linked);
    let graph = workflow_graph_from_program(linked.artifact.ir());
    let graph_ids = graph
        .nodes()
        .map(|node| node.id.to_string())
        .collect::<std::collections::BTreeSet<_>>();
    for site in compiled_execution_sites(&compiled) {
        assert!(
            graph_ids.contains(&site.node_id),
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
    let compiled = lashlang::testing::harness::compile_linked_process_named(&linked, "direct")
        .expect("direct IR process should compile");
    let graph = workflow_graph_from_program(linked.artifact.ir());
    let process = graph.process("direct").expect("projected process");
    let graph_ids = graph
        .nodes()
        .map(|node| node.id.to_string())
        .collect::<std::collections::BTreeSet<_>>();

    for site in compiled_execution_sites(&compiled) {
        assert!(
            graph_ids.contains(&site.node_id),
            "runtime site must name a projected graph node: {site:?}"
        );
        assert_ne!(
            site.node_id,
            process.id.as_str(),
            "the process root is a non-executable container"
        );
        assert!(
            !site.workflow_site.path.is_empty(),
            "an executable process site must have a child path"
        );
    }
}

fn descriptor_pairs(compiled: &lashlang::CompiledProgram) -> Vec<(String, String)> {
    compiled_execution_sites(compiled)
        .into_iter()
        .map(|site| (site.node_kind.to_string(), site.label.clone()))
        .collect()
}
