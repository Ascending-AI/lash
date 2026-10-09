//! Laws of document admission (FIG-5574). Every program is direct IR, so no
//! law passes through a dialect.

use crate::testing::ast_builders as b;
use crate::testing::harness::test_environment;
use crate::workflow_graph::{
    WorkflowBodyRef, WorkflowBodySlot, WorkflowDeclaration, WorkflowDraft, WorkflowEdit,
    WorkflowEditTransaction, WorkflowGraph, WorkflowNodeId,
};
use crate::{Expr, LinkedModule, Program, TypeExpr};

use super::{
    WorkflowAdmissionDiagnosticKind as Kind, WorkflowAdmissionLocation, admit_workflow_graph,
    workflow_graph_from_artifact,
};

fn echo(value: Expr) -> Expr {
    b::module_call(&["tools"], "echo", vec![b::record(vec![("value", value)])])
}

/// `main` starts an inline process whose body guards `guarded` in a `try`.
fn starting(guarded: Vec<Expr>) -> Program {
    let literal = b::process_literal(
        vec![b::param("input", TypeExpr::Str)],
        b::block(vec![
            b::try_expr(
                b::block(guarded),
                Some(b::catch("error", b::block(vec![b::print(b::var("error"))]))),
                None,
            ),
            b::finish(b::var("input")),
        ]),
    );
    b::program(vec![
        b::assign(
            "handle",
            b::module_call(
                &["processes"],
                "start",
                vec![b::record(vec![("definition", literal)])],
            ),
        ),
        b::finish(b::var("handle")),
    ])
}

fn linked(program: Program) -> LinkedModule {
    LinkedModule::link(program, test_environment()).expect("the fixture links")
}

/// Every node and process container id of `graph`.
fn ids(graph: &WorkflowGraph) -> Vec<WorkflowNodeId> {
    let mut ids = graph
        .nodes()
        .map(|node| node.id.clone())
        .collect::<Vec<_>>();
    for declaration in &graph.declarations {
        if let WorkflowDeclaration::Process(process) = declaration {
            ids.push(process.id.clone());
        }
    }
    ids
}

/// The draft of `graph` with the statement the lifted process's `try` body
/// holds, and that body.
fn guarded(graph: &WorkflowGraph) -> (WorkflowDraft, WorkflowBodyRef, crate::WorkflowDraftHandle) {
    let draft = WorkflowDraft::open(graph).expect("the admitted document opens");
    let [WorkflowDeclaration::Process(process)] = draft.document().declarations.as_slice() else {
        panic!("the literal is one process container");
    };
    let container = draft.handle(&process.id).expect("the container's handle");
    let region = draft
        .body(&WorkflowBodyRef::Process(container))
        .expect("the container's body")[0];
    let body = WorkflowBodyRef::Child {
        node: region,
        slot: WorkflowBodySlot::TryBody,
    };
    let call = draft.body(&body).expect("the try body")[0];
    (draft, body, call)
}

/// The linker derives a lifted process; admission hands it the literal
/// again. An unchanged admitted document is therefore its own artifact, and
/// an edit inside the lifted process admits to the program the edited
/// source links to, with every submitted node named in the admitted
/// document.
#[test]
fn an_admitted_document_readmits_to_its_artifact_and_an_edit_to_the_edited_program() {
    let environment = test_environment();
    let original = linked(starting(vec![echo(b::var("input"))]));
    let document = workflow_graph_from_artifact(&original.artifact);

    let unchanged = admit_workflow_graph(&document, &environment).expect("readmission");
    assert_eq!(
        unchanged.linked.artifact.module_ref(),
        original.artifact.module_ref()
    );
    assert_eq!(unchanged.graph, document);
    for id in ids(&document) {
        assert_eq!(unchanged.nodes.get(&id), Some(&id), "{id:?} is itself");
    }

    let (mut draft, body, call) = guarded(&document);
    draft
        .apply(WorkflowEditTransaction {
            base: draft.revision(),
            edits: vec![WorkflowEdit::InsertNode {
                body,
                before: Some(call),
                statement: b::print(b::string("start")),
            }],
        })
        .expect("the insert applies");
    let edited = admit_workflow_graph(draft.document(), &environment).expect("admission");
    let expected = linked(starting(vec![
        b::print(b::string("start")),
        echo(b::var("input")),
    ]));
    // A lifted name digests the spelling its literal was lifted from, and
    // the two spellings differ (linked against unlinked), so the programs
    // are equal in everything but that name.
    let unnamed = |linked: &LinkedModule| {
        let [crate::Declaration::Process(process)] = linked.artifact.ir().declarations.as_slice()
        else {
            panic!("one lifted process");
        };
        serde_json::to_string(linked.artifact.ir())
            .expect("the program serializes")
            .replace(process.name.as_str(), "lifted")
    };
    assert_eq!(unnamed(&edited.linked), unnamed(&expected));
    assert_ne!(
        edited.linked.artifact.module_ref(),
        original.artifact.module_ref()
    );
    let admitted = ids(&edited.graph);
    let submitted = ids(draft.document());
    assert_eq!(submitted.len(), admitted.len());
    for id in &submitted {
        let to = edited.nodes.get(id).expect("every submitted node is named");
        assert!(admitted.contains(to), "{to:?} is an admitted node");
    }
}

/// A refusal is the linker's, against the environment handed in, at the node
/// and expression that caused it; nothing is admitted.
#[test]
fn a_requirement_the_environment_lacks_is_refused_at_its_node() {
    let environment = test_environment();
    let document =
        workflow_graph_from_artifact(&linked(starting(vec![echo(b::var("input"))])).artifact);
    let (mut draft, _, call) = guarded(&document);
    draft
        .apply(WorkflowEditTransaction {
            base: draft.revision(),
            edits: vec![WorkflowEdit::ReplaceNode {
                node: call,
                statement: b::module_call(&["vault"], "read", vec![b::record(Vec::new())]),
            }],
        })
        .expect("the replacement is a valid program");
    let node = draft.node_id(call).expect("the call's id").clone();

    let refusal =
        admit_workflow_graph(draft.document(), &environment).expect_err("no `vault` module");
    let [diagnostic] = refusal.diagnostics.as_slice() else {
        panic!("one diagnostic: {refusal:?}");
    };
    assert_eq!(diagnostic.kind, Kind::HostRequirement);
    assert_eq!(diagnostic.message, "unknown module `vault`");
    let WorkflowAdmissionLocation::Node { node: at, .. } = &diagnostic.location else {
        panic!("the refusal names a node: {diagnostic:?}");
    };
    assert_eq!(at, &node);

    // The same document admits where the module exists: the environment, not
    // the document, decided.
    let mut resources = crate::LashVmHostCatalog::new();
    resources
        .try_extend(environment.resources.clone())
        .expect("the test catalogue");
    resources
        .add_module_operation(
            ["vault"],
            "Vault",
            "read",
            "read",
            TypeExpr::Any,
            TypeExpr::Any,
        )
        .expect("a new module");
    let with_vault = crate::LashVmHostEnvironment::new(resources);
    let admission = admit_workflow_graph(draft.document(), &with_vault).expect("admission");
    let requirements = admission.linked.artifact.host_requirements();
    assert!(with_vault.satisfies(requirements));
    assert!(
        !environment.satisfies(requirements),
        "the artifact states the requirement the linker derived"
    );
}
