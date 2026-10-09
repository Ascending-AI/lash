//! The total-editability law (FIG-5572): a workflow document holds every
//! construct of the program it projects as typed IR, so projecting a program
//! and reconstructing it is the identity, with no dialect involved.
//!
//! The corpus is direct IR, one program per region and role, and the slot
//! walk decides whether it is complete: every [`Expr`] variant must occur in
//! it, in a statement position and below one.

use std::collections::BTreeSet;

use crate::testing::ast_builders as b;
use crate::testing::ir_variants::{EXPR_VARIANT_NAMES, program_bodies, variants_in};
use crate::{
    AstPath, CoercingBinaryOp, CoercingUnaryOp, Declaration, Expr, ExprSlot, ExprSlotVisitor,
    FunctionExpr, MethodKey, ModuleArtifact, OperandLogicalOp, ProcessDecl, ProcessOrigin,
    ProcessWrapperParts, Program, StructuralRole, TypeExpr, WORKFLOW_IR_VERSION, WorkflowGraph,
    WorkflowGraphDecodeError, WorkflowGraphError, WorkflowNodeId, WorkflowNodeKind,
    WorkflowSlotPath, validate_ast, walk_expr_slots, workflow_graph_from_artifact,
    workflow_graph_from_program, workflow_node_statement, workflow_program_from_graph,
    workflow_slot_value,
};

fn echo(value: Expr) -> Expr {
    b::module_call(&["tools"], "echo", vec![b::record(vec![("value", value)])])
}

fn completion(mut statements: Vec<Expr>, value: Expr) -> Expr {
    statements.push(value);
    b::role(StructuralRole::Completion, b::block(statements))
}

fn scope(statements: Vec<Expr>) -> Expr {
    b::role(StructuralRole::Scope, b::block(statements))
}

fn run_function(params: &[&str], body: Expr) -> FunctionExpr {
    FunctionExpr {
        name: Some("run".into()),
        js_name: Some("run".into()),
        receiver: None,
        params: params.iter().map(|name| (*name).into()).collect(),
        captures: Vec::new(),
        body: Box::new(body),
    }
}

/// A process body inside the failure wrapper, called directly or through a
/// driver builtin.
fn wrapped(params: &[&str], body: Expr, driver: bool) -> Expr {
    ProcessWrapperParts::build(
        run_function(params, body),
        driver.then(|| ("__lash_vm_async".into(), vec![b::bool_lit(true)])),
        params.iter().map(|name| b::var(name)).collect(),
        "caught".into(),
    )
}

fn attribute_assign(key: Option<Expr>, value: Expr) -> Expr {
    let mut items = vec![b::assign("__base", b::var("state"))];
    let step = match key {
        Some(index) => {
            items.push(b::assign("__key", index));
            b::index_step(b::var("__key"))
        }
        None => b::field_step("count"),
    };
    items.push(b::assign("__result", value));
    items.push(b::assign_path("__base", vec![step], b::var("__result")));
    items.push(b::var("__result"));
    b::role(StructuralRole::AttributeAssign, b::block(items))
}

fn declared_process(name: &str, body: Expr) -> Declaration {
    Declaration::Process(ProcessDecl {
        name: name.into(),
        params: vec![b::param("input", TypeExpr::Str)],
        return_ty: Some(TypeExpr::Any),
        label: Some(b::label("Worker", Some("does the work"))),
        origin: ProcessOrigin::Declared,
        body,
    })
}

/// Statements that are single nodes: every leaf-like statement kind and the
/// expression variants they carry.
fn statements() -> Program {
    let mut program = b::program(vec![
        b::assign("state", b::record(vec![("count", b::num(0.0))])),
        b::assign(
            "items",
            b::list(vec![b::null(), Expr::Absent, b::bool_lit(true)]),
        ),
        b::assign("text", b::string("value")),
        b::assign("answer", echo(b::var("text"))),
        b::labelled(
            b::label("Greet", Some("says hello")),
            b::assign("greeting", echo(b::string("hello"))),
        ),
        b::labelled(b::label("Bare", None), b::print(b::var("greeting"))),
        echo(b::var("answer")),
        b::await_expr(b::var("answer")),
        b::unwrap(b::sleep_for(b::num(1.0))),
        b::print(b::var("text")),
        b::assign("state", b::record(vec![])),
        b::assign_path(
            "state",
            vec![b::field_step("nested"), b::index_step(b::num(0.0))],
            b::var("text"),
        ),
        attribute_assign(None, b::num(1.0)),
        attribute_assign(Some(b::string("key")), b::num(2.0)),
        attribute_assign(
            None,
            b::binary(
                b::field(b::var("__base"), "count"),
                CoercingBinaryOp::Add,
                b::num(1.0),
            ),
        ),
        attribute_assign(
            Some(b::var("text")),
            b::binary(
                b::index(b::var("__base"), b::var("__key")),
                CoercingBinaryOp::Multiply,
                b::num(2.0),
            ),
        ),
        // A member assignment whose object is no plain variable is no state
        // update; it travels whole.
        b::role(
            StructuralRole::AttributeAssign,
            b::block(vec![
                b::assign("__base", b::field(b::var("state"), "inner")),
                b::assign("__result", b::num(3.0)),
                b::assign_path("__base", vec![b::field_step("count")], b::var("__result")),
                b::var("__result"),
            ]),
        ),
        b::role(
            StructuralRole::CollectionTransform {
                operation: "map".into(),
            },
            b::block(vec![
                b::assign("__receiver", b::var("items")),
                b::assign(
                    "__callback",
                    b::closure(None, &["item"], &[], b::var("item")),
                ),
                b::assign(
                    "__driver",
                    b::closure(None, &[], &["__receiver", "__callback"], b::null()),
                ),
                b::call(b::var("__driver"), vec![]),
            ]),
        ),
        b::role(
            StructuralRole::JsonTraversal,
            b::block(vec![
                b::assign("__input", b::var("state")),
                b::if_else(b::var("__input"), b::num(1.0), b::num(2.0)),
            ]),
        ),
        b::assign("mapped", b::map(b::var("items"), "item", b::var("item"))),
        b::assign(
            "computed",
            b::logical(
                b::unary(CoercingUnaryOp::ToString, b::var("text")),
                OperandLogicalOp::Or,
                b::index(b::var("items"), b::num(0.0)),
            ),
        ),
        b::assign("built", b::builtin("len", vec![b::var("items")])),
        b::assign("declared", b::function_call("double", vec![b::num(2.0)])),
        b::assign(
            "method",
            Expr::MethodCall {
                receiver: Box::new(b::var("state")),
                method: MethodKey::Index(Box::new(b::var("text"))),
                args: vec![b::num(1.0)],
            },
        ),
        Expr::MethodCall {
            receiver: Box::new(b::var("state")),
            method: MethodKey::Field("run".into()),
            args: vec![],
        },
        Expr::ThisCall {
            this: Box::new(b::var("state")),
            function: Box::new(b::var("method")),
            args: vec![b::num(1.0)],
        },
        b::assign(
            "descriptor",
            b::host_descriptor("timer.Schedule", b::record(vec![])),
        ),
        b::assign("child", b::process_ref("worker")),
        b::block(vec![b::print(b::string("a bare block is one statement"))]),
        Expr::Throw(Box::new(b::var("text"))),
        b::fail(b::string("failed")),
        b::finish(b::var("answer")),
    ]);
    program.declarations = vec![
        declared_process(
            "worker",
            wrapped(
                &["input"],
                completion(
                    vec![
                        b::assign("local", echo(b::var("input"))),
                        Expr::FunctionReturn(Box::new(b::var("local"))),
                    ],
                    Expr::Absent,
                ),
                false,
            ),
        ),
        declared_process(
            "driven",
            wrapped(&["input"], b::block(vec![b::print(b::var("input"))]), true),
        ),
        // A body with no wrapper is the whole process body.
        declared_process("plain", b::block(vec![b::finish(b::var("input"))])),
        Declaration::Process(ProcessDecl {
            name: format!("{}lifted", crate::LIFTED_PROCESS_NAME_PREFIX).into(),
            params: vec![b::param("hidden", TypeExpr::Any)],
            return_ty: Some(TypeExpr::Any),
            label: None,
            origin: ProcessOrigin::Lifted {
                site: AstPath::main(vec![0]),
                hidden_params: 1,
                declared_return_ty: Some(TypeExpr::Any),
            },
            body: wrapped(&[], b::block(vec![b::finish(b::var("hidden"))]), false),
        }),
        b::function_decl(
            "double",
            vec![b::function_param("value", TypeExpr::Any)],
            TypeExpr::Any,
            b::binary(b::var("value"), CoercingBinaryOp::Multiply, b::num(2.0)),
        ),
    ];
    program.main = match program.main {
        Expr::Block(mut statements) => {
            statements.push(b::assign(
                "lifted",
                b::process_ref(&format!("{}lifted", crate::LIFTED_PROCESS_NAME_PREFIX)),
            ));
            Expr::Block(statements)
        }
        main => main,
    };
    program.private_bindings = ["__base", "__key", "__result"]
        .into_iter()
        .map(Into::into)
        .collect();
    program
}

/// Regions: every container, every body form, and the statement lists a
/// front end closes with a completion value.
fn regions() -> Program {
    b::program(vec![
        b::assign("total", b::num(0.0)),
        // Branches as statement lists, with and without a completion value.
        b::if_else(
            b::var("total"),
            completion(vec![b::print(b::string("then"))], Expr::Absent),
            completion(vec![], Expr::Absent),
        ),
        // An expression `if`: each branch is its one statement.
        b::assign(
            "picked",
            b::if_else(b::var("total"), b::num(1.0), Expr::Absent),
        ),
        b::if_else(
            b::var("total"),
            b::block(vec![b::print(b::string("block"))]),
            b::block(vec![b::if_else(
                b::var("picked"),
                b::block(vec![b::print(b::string("else if"))]),
                b::block(vec![]),
            )]),
        ),
        b::for_in(
            "item",
            b::list(vec![b::num(1.0)]),
            b::block(vec![b::print(b::var("item")), Expr::Continue]),
        ),
        Expr::For {
            binding: "__element".into(),
            authored_binding: Some("entry".into()),
            iterable: Box::new(b::builtin("snapshot", vec![b::var("picked")])),
            bind: Some(Box::new(b::block(vec![b::assign(
                "entry",
                b::var("__element"),
            )]))),
            body: Box::new(completion(
                vec![b::print(b::var("entry")), Expr::Break],
                Expr::Absent,
            )),
        },
        b::assign(
            "looped",
            b::for_bind(
                "__pair",
                b::var("picked"),
                b::block(vec![
                    b::assign("key", b::index(b::var("__pair"), b::num(0.0))),
                    b::assign("value", b::index(b::var("__pair"), b::num(1.0))),
                ]),
                b::block(vec![b::print(b::var("key"))]),
            ),
        ),
        b::while_loop(
            b::var("total"),
            completion(
                vec![b::assign("total", b::num(1.0)), Expr::Break],
                Expr::Absent,
            ),
        ),
        b::assign(
            "waited",
            b::while_loop(b::bool_lit(false), b::block(vec![])),
        ),
        b::try_expr(
            completion(vec![b::print(b::string("body"))], Expr::Absent),
            Some(b::catch(
                "error",
                b::block(vec![
                    b::print(b::var("error")),
                    Expr::Throw(Box::new(b::var("error"))),
                ]),
            )),
            Some(b::block(vec![b::print(b::string("finally"))])),
        ),
        b::try_expr(
            b::block(vec![b::print(b::string("guarded"))]),
            None,
            Some(b::print(b::string("one statement"))),
        ),
        b::assign(
            "tried",
            b::try_expr(b::num(1.0), Some(b::catch("error", b::var("error"))), None),
        ),
        b::labelled(
            b::label("Guard", None),
            b::try_expr(
                b::block(vec![scope(vec![b::print(b::string("nested scope"))])]),
                Some(b::catch("error", b::block(vec![]))),
                None,
            ),
        ),
        scope(vec![
            b::assign("inner", b::num(1.0)),
            scope(vec![b::print(b::var("inner"))]),
        ]),
        b::assign("scoped", scope(vec![b::print(b::string("value scope"))])),
        // A statement the front end gave a value: one nested completion list
        // per assignment, one holding several statements, one holding another
        // list, and one holding none.
        completion(vec![b::assign("total", b::num(2.0))], b::var("total")),
        b::print(b::string("between")),
        completion(
            vec![
                b::print(b::string("first")),
                completion(vec![b::assign("total", b::num(3.0))], b::var("total")),
                b::print(b::string("last")),
            ],
            b::num(4.0),
        ),
        completion(vec![], b::string("no statement")),
        completion(
            vec![completion(vec![], b::num(5.0))],
            b::string("an empty list in a list"),
        ),
        b::print(b::string("end")),
    ])
}

/// A draft: a process literal still inline, with another inside its body.
fn draft() -> Program {
    let inner = b::process_literal(
        vec![b::param("depth", TypeExpr::Any)],
        wrapped(
            &["depth"],
            completion(vec![b::finish(b::var("depth"))], Expr::Absent),
            false,
        ),
    );
    let outer = Expr::ProcessLiteral(Box::new(crate::ProcessLiteralExpr {
        params: vec![b::param("input", TypeExpr::Str)],
        hidden_args: vec![b::param("captured", TypeExpr::Any)],
        return_ty: Some(TypeExpr::Any),
        body: Box::new(wrapped(
            &["input"],
            completion(
                vec![
                    b::assign("nested", inner),
                    b::try_expr(
                        b::block(vec![b::print(b::var("input"))]),
                        Some(b::catch("error", b::block(vec![b::fail(b::var("error"))]))),
                        None,
                    ),
                    b::finish(b::var("captured")),
                ],
                Expr::Absent,
            ),
            true,
        )),
    }));
    b::program(vec![
        b::assign("captured", b::num(1.0)),
        b::labelled(b::label("Child", None), b::assign("child", outer)),
    ])
}

fn corpus() -> Vec<(&'static str, Program)> {
    vec![
        ("statements", statements()),
        ("regions", regions()),
        ("draft", draft()),
    ]
}

#[test]
fn projecting_then_reconstructing_is_the_identity_for_every_ir_variant() {
    let mut seen = BTreeSet::new();
    for (name, program) in corpus() {
        validate_ast(&program).unwrap_or_else(|error| panic!("{name} is valid IR: {error}"));
        seen.extend(variants_in(&program));
        let graph = workflow_graph_from_program(&program);
        let rebuilt = workflow_program_from_graph(&graph)
            .unwrap_or_else(|error| panic!("{name} reconstructs: {error}"));
        assert_eq!(
            rebuilt, program,
            "{name} reconstructs to the program it projects"
        );
    }
    assert_eq!(
        seen,
        EXPR_VARIANT_NAMES.into_iter().collect(),
        "the corpus holds every IR variant"
    );
}

#[test]
fn an_admitted_document_reconstructs_its_source_identity() {
    for program in [statements(), regions()] {
        let artifact = ModuleArtifact::from_program(program).expect("the program admits");
        let graph = workflow_graph_from_artifact(&artifact);
        let rebuilt = workflow_program_from_graph(&graph).expect("the document reconstructs");
        let readmitted = ModuleArtifact::from_program(rebuilt).expect("the program admits again");
        assert_eq!(
            Some(readmitted.source_identity()),
            graph.source_identity,
            "reconstruction preserves the definition identity"
        );
        assert_eq!(readmitted.module_ref(), artifact.module_ref());
    }
}

#[test]
fn derived_views_are_recomputed_and_never_read() {
    fn scramble(graph: &mut crate::WorkflowSubgraph, counter: &mut u32) {
        graph.edges.clear();
        for node in &mut graph.nodes {
            *counter += 1;
            node.id = WorkflowNodeId::new(format!("node:host-{counter}"));
            node.available_variables = vec!["stale".to_string()];
            node.outputs.clear();
            node.execution_sites.clear();
            node.source_span = None;
            if node.name_source == crate::WorkflowNodeNameSource::Derived {
                node.name = "renamed by a host".to_string();
            }
            if let WorkflowNodeKind::Container(container) = &mut node.kind {
                for (_, child) in container.child_subgraphs_mut() {
                    scramble(child, counter);
                }
            }
        }
    }
    for (name, program) in corpus() {
        let projected = workflow_graph_from_program(&program);
        let mut edited = projected.clone();
        let mut counter = 0;
        scramble(&mut edited.main, &mut counter);
        for declaration in &mut edited.declarations {
            if let crate::WorkflowDeclaration::Process(process) = declaration {
                counter += 1;
                process.id = WorkflowNodeId::new(format!("node:host-{counter}"));
                scramble(&mut process.body, &mut counter);
            }
        }
        edited.source_identity = Some("a stale claim".to_string());
        assert_eq!(
            workflow_program_from_graph(&edited).expect("the edited document reconstructs"),
            program,
            "{name}: no derived field reaches the program"
        );
        assert_eq!(
            edited.rederive().expect("the edited document rederives"),
            projected,
            "{name}: rederiving restores every derived view and drops the identity claim"
        );
    }
}

#[test]
fn every_expression_of_a_node_has_a_typed_slot_address() {
    struct Paths(Vec<Vec<ExprSlot>>);
    impl ExprSlotVisitor for Paths {
        fn visit_slot(&mut self, path: &[ExprSlot], _expr: &Expr) {
            self.0.push(path.to_vec());
        }
    }
    for (name, program) in corpus() {
        for body in program_bodies(&program) {
            let mut paths = Paths(Vec::new());
            walk_expr_slots(&mut paths, body);
            for path in paths.0 {
                let steps = body
                    .child_steps(&path)
                    .expect("a slot path spells child steps");
                assert_eq!(
                    body.slot_path(&steps),
                    Some(path.clone()),
                    "{name}: {path:?}"
                );
                let mut by_children = body;
                for step in &steps {
                    by_children = by_children
                        .children()
                        .nth(*step as usize)
                        .expect("child steps address a child");
                }
                assert!(
                    std::ptr::eq(
                        by_children,
                        body.at_slots(&path).expect("the path resolves")
                    ),
                    "{name}: slots and children agree at {path:?}"
                );
            }
        }
        let graph = workflow_graph_from_program(&program);
        for node in graph.nodes() {
            let statement = workflow_node_statement(node).expect("a projected node is a statement");
            let mut paths = Paths(vec![Vec::new()]);
            walk_expr_slots(&mut paths, &statement);
            for path in paths.0 {
                let address = WorkflowSlotPath::structural(path.iter().copied());
                assert!(
                    workflow_slot_value(&statement, &address).is_some(),
                    "{name}: {address} resolves in node `{}`",
                    node.name
                );
            }
        }
    }
}

#[test]
fn a_document_survives_its_wire_encoding() {
    for (name, program) in corpus() {
        let graph = workflow_graph_from_program(&program);
        let json = serde_json::to_string(&graph).expect("the document serializes");
        assert_eq!(
            WorkflowGraph::decode_json(&json).expect("the document decodes"),
            graph,
            "{name}"
        );
    }
}

#[test]
fn an_unsupported_ir_interpretation_is_refused_at_open() {
    let graph = workflow_graph_from_program(&regions());
    assert_eq!(graph.ir_version, WORKFLOW_IR_VERSION);
    let mut document = serde_json::to_value(&graph).expect("the document serializes");
    document["ir_version"] = serde_json::json!(WORKFLOW_IR_VERSION + 1);
    assert!(matches!(
        WorkflowGraph::decode_json_value(document.clone()),
        Err(WorkflowGraphDecodeError::UnsupportedIrVersion(refusal))
            if refusal.found == WORKFLOW_IR_VERSION + 1 && refusal.supported == WORKFLOW_IR_VERSION
    ));
    document
        .as_object_mut()
        .expect("a document is an object")
        .remove("ir_version");
    assert!(matches!(
        WorkflowGraph::decode_json_value(document),
        Err(WorkflowGraphDecodeError::MissingIrVersion)
    ));
    let mut built = graph;
    built.ir_version += 1;
    assert!(matches!(
        workflow_program_from_graph(&built),
        Err(WorkflowGraphError::UnsupportedIrVersion(_))
    ));
}

#[test]
fn an_edit_inside_a_lifted_container_reaches_its_literal() {
    let program = draft();
    let mut graph = workflow_graph_from_program(&program);
    let outer = graph
        .declarations
        .iter_mut()
        .find_map(|declaration| match declaration {
            crate::WorkflowDeclaration::Process(process)
                if process.params.len() == 1 && process.params[0].name.as_str() == "input" =>
            {
                Some(process)
            }
            _ => None,
        })
        .expect("the outer literal projects as a process container");
    let removed = outer.body.nodes.remove(1);
    assert!(matches!(
        removed.kind,
        WorkflowNodeKind::Container(crate::WorkflowContainer::Try { .. })
    ));
    let rebuilt = workflow_program_from_graph(&graph).expect("the edited draft reconstructs");
    let mut expected = program;
    let Expr::Block(statements) = &mut expected.main else {
        panic!("main is a block");
    };
    let literal = statements[1]
        .at_slots_mut(&[ExprSlot::Inner, ExprSlot::Value])
        .expect("the labelled assignment holds the literal");
    let run_body = literal
        .at_slots_mut(&[
            ExprSlot::Body,
            ExprSlot::Inner,
            ExprSlot::Body,
            ExprSlot::Operand,
            ExprSlot::Callee,
            ExprSlot::Arg(0),
            ExprSlot::Body,
            ExprSlot::Inner,
        ])
        .expect("the driven wrapper holds the run body");
    let Expr::Block(run_statements) = run_body else {
        panic!("the run body is a completion list");
    };
    run_statements.remove(1);
    assert_eq!(
        rebuilt, expected,
        "the container's body is the literal's body"
    );

    // A container nothing carries is refused, never dropped.
    let Expr::Block(statements) = &mut expected.main else {
        panic!("main is a block");
    };
    statements.pop();
    let mut orphaned = graph;
    orphaned.main.nodes.pop();
    assert!(matches!(
        workflow_program_from_graph(&orphaned),
        Err(WorkflowGraphError::ProcessOriginMismatch { .. })
    ));
}
