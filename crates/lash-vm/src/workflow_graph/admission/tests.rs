//! Laws of document admission (FIG-5574). Every program is direct IR, so no
//! law passes through a dialect.

use crate::testing::ast_builders as b;
use crate::testing::harness::test_environment;
use crate::workflow_graph::{
    WorkflowBindingRef, WorkflowBodyRef, WorkflowBodySlot, WorkflowDeclaration, WorkflowDraft,
    WorkflowDraftHandle, WorkflowEdit, WorkflowEditDiagnosticKind, WorkflowEditTransaction,
    WorkflowGraph, WorkflowNodeId,
};
use crate::{Declaration, Expr, LinkedModule, ProcessDecl, ProcessOrigin, Program, TypeExpr};

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

/// The linker derives a lifted process again from its declaration. An
/// unchanged admitted document is therefore its own artifact, and an edit
/// inside the lifted process admits to the module the edited source links
/// to, with every submitted node named in the admitted document.
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
    // A lifted name digests what the linker derived, so the edited document
    // is the module its source links to, names included.
    assert_eq!(
        edited.linked.artifact.module_ref(),
        expected.artifact.module_ref()
    );
    assert_eq!(edited.linked.artifact.ir(), expected.artifact.ir());
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

fn start(definition: Expr) -> Expr {
    b::module_call(
        &["processes"],
        "start",
        vec![b::record(vec![("definition", definition)])],
    )
}

/// `main` binds two inline processes and starts the second: `a` finishes
/// `"alpha"`, and `b` takes `x`, starts `a` and finishes `x`.
fn two_processes() -> Program {
    b::program(vec![
        b::assign(
            "a",
            b::process_literal(Vec::new(), b::block(vec![b::finish(b::string("alpha"))])),
        ),
        b::assign(
            "b",
            b::process_literal(
                vec![b::param("x", TypeExpr::Any)],
                b::block(vec![
                    b::assign("child", start(b::var("a"))),
                    echo(b::string("beta")),
                    b::finish(b::var("x")),
                ]),
            ),
        ),
        b::assign("handle", start(b::var("b"))),
        b::finish(b::var("handle")),
    ])
}

/// The lifted process of `linked` whose own body holds the string `marker`.
fn lifted<'a>(linked: &'a LinkedModule, marker: &str) -> &'a ProcessDecl {
    let marker = format!("\"{marker}\"");
    let mut found = linked
        .artifact
        .ir()
        .declarations
        .iter()
        .filter_map(|declaration| match declaration {
            Declaration::Process(process) if process.origin.is_lifted() => Some(process),
            _ => None,
        })
        .filter(|process| {
            serde_json::to_string(&process.body)
                .expect("the body serializes")
                .contains(&marker)
        });
    let process = found.next().expect("a lifted process holds the marker");
    assert!(found.next().is_none(), "one lifted process holds {marker}");
    process
}

/// Every process `expression` references, in walk order.
fn references(expression: &Expr) -> Vec<String> {
    fn walk(expression: &Expr, names: &mut Vec<String>) {
        if let Expr::ProcessRef { process } = expression {
            names.push(process.to_string());
        }
        for child in expression.children() {
            walk(child, names);
        }
    }
    let mut names = Vec::new();
    walk(expression, &mut names);
    names
}

/// The handle of the process container named `name`.
fn container(draft: &WorkflowDraft, name: &str) -> WorkflowDraftHandle {
    let process = draft.document().process(name).expect("the container");
    draft.handle(&process.id).expect("the container's handle")
}

fn apply(draft: &mut WorkflowDraft, edits: Vec<WorkflowEdit>) {
    draft
        .apply(WorkflowEditTransaction {
            base: draft.revision(),
            edits,
        })
        .expect("the edit commits");
}

/// The entries of `linked`, each run once, with what it asked its host for.
fn effects(linked: &LinkedModule) -> std::collections::BTreeMap<String, String> {
    futures_executor::block_on(crate::testing::differential::run_artifact(&linked.artifact))
        .entries
        .into_iter()
        .map(|entry| {
            assert!(
                entry.outcome.is_ok(),
                "{} runs: {:?}",
                entry.entry,
                entry.outcome
            );
            let effects = serde_json::to_string(&entry.effects).expect("effects serialize");
            (entry.entry, effects)
        })
        .collect()
}

/// The definition a run names when it starts the process `name` of `linked`.
fn definition_id(linked: &LinkedModule, name: &str) -> String {
    crate::ProcessDefinitionIdentity::from_artifact_export(&linked.artifact, name)
        .expect("the process is exported")
        .draft()
        .expect("the definition's descriptor")
        .id()
        .to_string()
}

/// The admitted document of [`two_processes`], open for editing, with the
/// names it gives `a` and `b`.
fn two_admitted_processes() -> (WorkflowDraft, String, String) {
    let original = linked(two_processes());
    let document = workflow_graph_from_artifact(&original.artifact);
    let draft = WorkflowDraft::open(&document).expect("the admitted document opens");
    (
        draft,
        lifted(&original, "alpha").name.to_string(),
        lifted(&original, "beta").name.to_string(),
    )
}

/// A reference to a lifted process names that process, whatever the
/// variables around it are called (FIG-5640). `b` starts `a` by reference.
/// Renaming `b`'s parameter to `a`, or binding a local `a` ahead of the
/// start, changes no reference: the published `b` still starts the process
/// the document names, and running it asks the host to start that one.
#[test]
fn a_binding_named_like_a_literals_alias_does_not_capture_its_reference() {
    let environment = test_environment();
    let shadows: [fn(&WorkflowDraft, WorkflowDraftHandle) -> WorkflowEdit; 2] = [
        |_, process| WorkflowEdit::RenameBinding {
            binding: WorkflowBindingRef::Variable {
                at: process,
                name: "x".into(),
            },
            name: "a".into(),
        },
        |draft, process| {
            let body = WorkflowBodyRef::Process(process);
            let first = draft.body(&body).expect("the body of `b`")[0];
            WorkflowEdit::InsertNode {
                body,
                before: Some(first),
                statement: b::assign("a", b::string("shadow")),
            }
        },
    ];
    for shadow in shadows {
        let (mut draft, _, second) = two_admitted_processes();
        let process = container(&draft, &second);
        let edit = shadow(&draft, process);
        apply(&mut draft, vec![edit]);

        let admission = admit_workflow_graph(draft.document(), &environment).expect("admission");
        let first = lifted(&admission.linked, "alpha");
        let second = lifted(&admission.linked, "beta");
        assert_eq!(
            references(&second.body),
            [first.name.to_string()],
            "`b` starts the process the document names"
        );
        let effects = effects(&admission.linked);
        assert!(
            effects[second.name.as_str()].contains(&definition_id(&admission.linked, &first.name)),
            "running `b` starts `a`: {}",
            effects[second.name.as_str()]
        );
    }
}

/// A reference to a lifted process is a process value wherever the document
/// puts it, and admission takes every document a draft commits (FIG-5640):
/// a second reference from a cloned start, a reference whose literal's
/// binding was cleared, and a reference in a list, a record or a
/// conditional all publish, to one declaration per process, and run.
#[test]
fn every_committed_place_of_a_lifted_reference_publishes_and_runs() {
    let environment = test_environment();

    // A cloned start of an inline process: two references, no binding.
    let original = linked(starting(vec![echo(b::var("input"))]));
    let document = workflow_graph_from_artifact(&original.artifact);
    let mut draft = WorkflowDraft::open(&document).expect("the admitted document opens");
    let main = draft.body(&WorkflowBodyRef::Main).expect("main");
    apply(
        &mut draft,
        vec![WorkflowEdit::CloneNode {
            node: main[0],
            body: WorkflowBodyRef::Main,
            before: Some(main[1]),
        }],
    );
    let cloned = admit_workflow_graph(draft.document(), &environment).expect("a cloned start");
    let [Declaration::Process(process)] = cloned.linked.artifact.ir().declarations.as_slice()
    else {
        panic!("both starts reference one process");
    };
    assert_eq!(
        references(&cloned.linked.artifact.ir().main),
        [process.name.to_string(), process.name.to_string()]
    );
    assert_eq!(
        process.name,
        original_process(&original).name,
        "the process is the one the document held"
    );
    let ran = effects(&cloned.linked);
    let started = definition_id(&cloned.linked, &process.name);
    assert_eq!(ran["main"].matches(&started).count(), 2, "{}", ran["main"]);

    // The binding another process read the literal through is cleared.
    let (mut draft, first, _) = two_admitted_processes();
    let main = draft.body(&WorkflowBodyRef::Main).expect("main");
    apply(
        &mut draft,
        vec![WorkflowEdit::SetBinding {
            node: main[0],
            binding: None,
        }],
    );
    let unbound = admit_workflow_graph(draft.document(), &environment).expect("no binding");
    let (alpha, beta) = (
        lifted(&unbound.linked, "alpha"),
        lifted(&unbound.linked, "beta"),
    );
    assert_ne!(
        alpha.name.as_str(),
        first,
        "its site moved with its statement"
    );
    assert_eq!(references(&beta.body), [alpha.name.to_string()]);
    let ran = effects(&unbound.linked);
    assert!(ran[beta.name.as_str()].contains(&definition_id(&unbound.linked, &alpha.name)));

    // A reference in a list, a record and a conditional, ahead of every
    // other reference to the process.
    let places: [fn(Expr) -> Expr; 3] = [
        |reference| b::list(vec![reference]),
        |reference| b::record(vec![("process", reference)]),
        |reference| {
            b::if_else(
                b::bool_lit(true),
                b::block(vec![reference.clone()]),
                b::block(vec![reference]),
            )
        },
    ];
    for place in places {
        let (mut draft, first, _) = two_admitted_processes();
        let main = draft.body(&WorkflowBodyRef::Main).expect("main");
        apply(
            &mut draft,
            vec![WorkflowEdit::InsertNode {
                body: WorkflowBodyRef::Main,
                before: Some(main[0]),
                statement: b::assign("held", place(b::process_ref(&first))),
            }],
        );
        let placed = admit_workflow_graph(draft.document(), &environment).expect("a placed value");
        let first = lifted(&placed.linked, "alpha");
        let processes = placed.linked.artifact.ir().declarations.len();
        assert_eq!(processes, 2, "each process is declared once");
        assert!(
            references(&placed.linked.artifact.ir().main)
                .iter()
                .all(|name| *name == first.name.as_str()
                    || *name == lifted(&placed.linked, "beta").name.as_str())
        );
        assert_eq!(
            references(&lifted(&placed.linked, "beta").body),
            [first.name.to_string()]
        );
        effects(&placed.linked);
    }
}

fn original_process(linked: &LinkedModule) -> &ProcessDecl {
    let [Declaration::Process(process)] = linked.artifact.ir().declarations.as_slice() else {
        panic!("one lifted process");
    };
    process
}

/// A lifted process's name is a function of its content (FIG-5640): a
/// document edited where its literal sits publishes the module the edited
/// source links to, and a lifted process that reaches itself has no name
/// and is refused.
#[test]
fn a_lifted_process_is_named_by_what_it_derives_to() {
    let environment = test_environment();
    let original = linked(starting(vec![echo(b::var("input"))]));
    let document = workflow_graph_from_artifact(&original.artifact);
    let mut draft = WorkflowDraft::open(&document).expect("the admitted document opens");
    let main = draft.body(&WorkflowBodyRef::Main).expect("main");
    apply(
        &mut draft,
        vec![WorkflowEdit::InsertNode {
            body: WorkflowBodyRef::Main,
            before: Some(main[0]),
            statement: b::print(b::string("first")),
        }],
    );
    let moved = admit_workflow_graph(draft.document(), &environment).expect("admission");
    let mut source = starting(vec![echo(b::var("input"))]);
    let Expr::Block(statements) = &mut source.main else {
        panic!("main is a block");
    };
    statements.insert(0, b::print(b::string("first")));
    let source = linked(source);
    assert_eq!(moved.linked.artifact.ir(), source.artifact.ir());
    assert_eq!(
        moved.linked.artifact.module_ref(),
        source.artifact.module_ref()
    );
    let name = original_process(&source).name.as_str();
    assert_eq!(name, crate::lifted_process_name(original_process(&source)));
    assert_ne!(name, original_process(&original).name.as_str());

    // A process that starts itself has no content to name: the draft
    // refuses the edit at the process, and admission refuses a document
    // that states it anyway.
    let (mut draft, first, _) = two_admitted_processes();
    let process = container(&draft, &first);
    let body = WorkflowBodyRef::Process(process);
    let finish = draft.body(&body).expect("the body of `a`")[0];
    let refusal = draft
        .apply(WorkflowEditTransaction {
            base: draft.revision(),
            edits: vec![WorkflowEdit::InsertNode {
                body,
                before: Some(finish),
                statement: b::assign("again", start(b::process_ref(&first))),
            }],
        })
        .expect_err("a lifted process that starts itself");
    assert!(matches!(
        refusal.diagnostics.as_slice(),
        [diagnostic] if matches!(diagnostic.kind, WorkflowEditDiagnosticKind::DerivedProcess)
            && diagnostic.location == crate::WorkflowEditLocation::Process { process }
    ));
    let mut stated = linked(two_processes()).artifact.ir().clone();
    for declaration in &mut stated.declarations {
        if let Declaration::Process(process) = declaration
            && process.name.as_str() == first
        {
            process.body = b::block(vec![
                b::assign("again", start(b::process_ref(&first))),
                b::finish(b::string("alpha")),
            ]);
        }
    }
    let refusal = admit_workflow_graph(&crate::workflow_graph_from_program(&stated), &environment)
        .expect_err("a lifted process that starts itself");
    let [diagnostic] = refusal.diagnostics.as_slice() else {
        panic!("one diagnostic: {refusal:?}");
    };
    assert_eq!(diagnostic.kind, Kind::InvalidProgram);
}

/// A lifted process is derived, and its edits say so (FIG-5640):
/// `RemoveProcess` refuses its container, whether the document holds its
/// declaration or its literal, and changes nothing; `SetProcessSignature`
/// sets the parameters the literal authored and keeps its captures after
/// them, so the published process still takes each capture as one.
#[test]
fn a_lifted_container_refuses_removal_and_keeps_its_captures() {
    let environment = test_environment();
    let captures = || {
        let mut literal = b::process_literal(
            vec![b::param("tick", TypeExpr::Any)],
            b::block(vec![b::finish(b::var("budget"))]),
        );
        if let Expr::ProcessLiteral(literal) = &mut literal {
            literal.hidden_args.push(b::param("budget", TypeExpr::Any));
        }
        b::program(vec![
            b::assign("budget", b::num(3.0)),
            b::assign("handler", literal),
            b::assign("handle", start(b::var("handler"))),
            b::finish(b::var("handle")),
        ])
    };
    let original = linked(captures());
    let admitted = workflow_graph_from_artifact(&original.artifact);
    for document in [
        admitted.clone(),
        crate::workflow_graph_from_program(&captures()),
    ] {
        let mut draft = WorkflowDraft::open(&document).expect("the document opens");
        let before = draft.document().clone();
        let [WorkflowDeclaration::Process(process)] = draft.document().declarations.as_slice()
        else {
            panic!("one process container");
        };
        let process = draft.handle(&process.id).expect("the container's handle");
        let refusal = draft
            .apply(WorkflowEditTransaction {
                base: draft.revision(),
                edits: vec![WorkflowEdit::RemoveProcess { process }],
            })
            .expect_err("a lifted container is not removed");
        assert!(matches!(
            refusal.diagnostics.as_slice(),
            [diagnostic] if matches!(diagnostic.kind, WorkflowEditDiagnosticKind::DerivedProcess)
        ));
        assert_eq!(draft.document(), &before);
    }

    let mut draft = WorkflowDraft::open(&admitted).expect("the admitted document opens");
    let process = container(&draft, original_process(&original).name.as_str());
    apply(
        &mut draft,
        vec![WorkflowEdit::SetProcessSignature {
            process,
            params: vec![
                b::param("when", TypeExpr::Str),
                b::param("often", TypeExpr::Bool),
            ],
            return_ty: None,
        }],
    );
    let names = |process: &[crate::ProcessParam]| {
        process
            .iter()
            .map(|param| param.name.to_string())
            .collect::<Vec<_>>()
    };
    let edited = draft.process(process).expect("the container");
    assert_eq!(names(&edited.params), ["when", "often", "budget"]);
    assert!(matches!(
        edited.origin,
        ProcessOrigin::Lifted {
            hidden_params: 1,
            ..
        }
    ));
    let admission = admit_workflow_graph(draft.document(), &environment).expect("admission");
    let published = original_process(&admission.linked);
    assert_eq!(names(&published.params), ["when", "often", "budget"]);
    assert_eq!(published.params[2], original_process(&original).params[1]);
    assert!(matches!(
        published.origin,
        ProcessOrigin::Lifted {
            hidden_params: 1,
            ..
        }
    ));
}
