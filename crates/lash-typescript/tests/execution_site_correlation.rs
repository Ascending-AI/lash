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
use lash_typescript::workflow_graph::workflow_graph_from_program;
use lashlang::testing::harness::{
    EchoHost, compile_labeled_program, compiled_execution_sites, link_labeled,
};
use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome,
    LashlangExecutionObservation, Program, State, Value, WorkflowEffectKind, WorkflowNodeKind,
    node_id_for_execution_site,
};

/// A `(kind, label, path)` triple for every execution site the compiler emitted,
/// ordered by path so the compiler's and the graph's lists are comparable.
fn compiled_site_descriptors(
    compiled: &lashlang::CompiledProgram,
) -> Vec<(String, String, Vec<u32>)> {
    let mut sites = compiled_execution_sites(compiled)
        .into_iter()
        .map(|site| {
            (
                site.node_kind.clone(),
                site.label.clone(),
                site.workflow_site.path.clone(),
            )
        })
        .collect::<Vec<_>>();
    sites.sort_by(|left, right| left.2.cmp(&right.2));
    sites
}

/// The same triples, read off the projected graph instead.
fn graph_site_descriptors(program: &Program) -> Vec<(String, String, Vec<u32>)> {
    let mut sites = workflow_graph_from_program(program)
        .nodes()
        .flat_map(|node| node.execution_sites.iter())
        .map(|site| (site.kind.clone(), site.label.clone(), site.path.clone()))
        .collect::<Vec<_>>();
    sites.sort_by(|left, right| left.2.cmp(&right.2));
    sites
}

fn parse_program(source: &str) -> Program {
    parse(source).expect("fixture parses")
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
    let source = r#"const searchTest = defineProcess({
  name: "search_test",
  signals: {},
  run: async () => {
    const result = await tools.echo({ value: { ok: true } });
    wake(result);
    return result;
  }
});
finish(1);
"#;
    let linked = link_labeled(parse_program(source));
    let compiled =
        lashlang::compile_linked_process(&linked, "search_test").expect("process should compile");
    let site = compiled_execution_sites(&compiled)
        .into_iter()
        .find(|site| site.node_kind == "resource_operation")
        .expect("resource operation execution site");

    let graph = workflow_graph_from_program(linked.program());
    let graph_node_id = node_id_for_execution_site(&graph, site)
        .expect("runtime site should correlate to a workflow node");
    let graph_node = graph
        .nodes()
        .find(|node| node.id == graph_node_id)
        .expect("correlated graph node");

    assert_eq!(graph_node.name, "echo");
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
            .any(|candidate| candidate.node_kind == "step"
                && candidate.workflow_site.path == site.workflow_site.path),
        "a resource operation should not also emit a generic step site at its own path"
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
    let graph = workflow_graph_from_program(linked.program());
    let compiled = compile_labeled_program(linked.program().clone());

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
                let node_id = node_id_for_execution_site(&graph, site)
                    .expect("every observed runtime site should resolve to the workflow graph");
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
    let compiled = compile_labeled_program(linked.program().clone());
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
        graph_site_descriptors(linked.program()),
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
    let compiled = compile_labeled_program(linked.program().clone());
    let expected = vec![
        ("terminal".to_string(), "result".to_string(), vec![1]),
        ("call".to_string(), "function call".to_string(), vec![1, 0]),
    ];
    assert_eq!(compiled_site_descriptors(&compiled), expected);
    assert_eq!(graph_site_descriptors(linked.program()), expected);
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
    let source = r#"const worker = defineProcess({
  name: "worker",
  signals: { ready: null },
  run: async () => {
    const payload = await waitSignal("ready");
    wake(payload);
    return payload;
  }
});
/** @label Plain value */
const plain = 1;
const result = await tools.echo({ value: plain });
const run = start(worker, {});
await sleep(1);
wake(run, "ready", null);
if (true) {
} else {
}
const identity = (value) => value;
const called = identity(1);
finish(result);
"#;
    let linked = link_labeled(parse_program(source));
    let main = compile_labeled_program(linked.program().clone());
    let process =
        lashlang::compile_linked_process(&linked, "worker").expect("descriptor process compiles");

    let mut compiler = descriptor_pairs(&main);
    compiler.extend(descriptor_pairs(&process));
    compiler.sort();
    compiler.dedup();
    let mut graph = workflow_graph_from_program(linked.program())
        .nodes()
        .flat_map(|node| node.execution_sites.iter())
        .map(|site| (site.kind.clone(), site.label.clone()))
        .collect::<Vec<_>>();
    graph.sort();
    graph.dedup();

    let mut expected = vec![
        ("branch".to_string(), "if".to_string()),
        ("call".to_string(), "function call".to_string()),
        ("child_process".to_string(), "start worker".to_string()),
        ("process_event".to_string(), "wake".to_string()),
        ("resource_operation".to_string(), "echo".to_string()),
        ("signal".to_string(), "signal_run".to_string()),
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

    // The compiler emits one descriptor the projector cannot: the
    // `defineProcess` wrapper's generated catch, which fails the process on an
    // uncaught error. It has no authored spelling, so the lens projects the
    // inner `run` body and never mints a node for it (FIG-3057).
    let mut expected_compiler = expected.clone();
    expected_compiler.push(("terminal".to_string(), "failure".to_string()));
    expected_compiler.sort();
    assert_eq!(
        compiler, expected_compiler,
        "compiler descriptor vocabulary drifted"
    );

    let branch = compiled_execution_sites(&main)
        .into_iter()
        .find(|site| site.node_kind == "branch")
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
            effect: WorkflowEffectKind::Sleep,
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

fn descriptor_pairs(compiled: &lashlang::CompiledProgram) -> Vec<(String, String)> {
    compiled_execution_sites(compiled)
        .into_iter()
        .map(|site| (site.node_kind.clone(), site.label.clone()))
        .collect()
}
