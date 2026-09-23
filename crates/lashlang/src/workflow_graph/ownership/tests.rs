//! The structural walk over direct IR and over each structural role
//! (FIG-3571): no program here passed through a front end.

use crate::testing::ast_builders as b;
use crate::testing::harness::link_labeled;
use crate::{
    AstPath, Expr, InvalidAst, NoStatementText, Program, StructuralRole, WorkflowNodeKind,
    validate_ast, workflow_graph_from_program,
};

use super::WorkflowProjection;

fn echo(value: &str) -> Expr {
    b::unwrap(b::await_expr(b::module_call(
        &["tools"],
        "echo",
        vec![b::record(vec![("value", b::string(value))])],
    )))
}

fn completion(mut statements: Vec<Expr>) -> Expr {
    statements.push(Expr::Undefined);
    b::role(StructuralRole::Completion, b::block(statements))
}

fn main_statement_paths(program: &Program) -> Vec<Vec<u32>> {
    fn collect(body: &super::WorkflowBody<'_>, out: &mut Vec<Vec<u32>>) {
        for statement in &body.statements {
            out.push(statement.node_path.indices().to_vec());
            for child in &statement.bodies {
                collect(child, out);
            }
        }
    }
    let projection = WorkflowProjection::for_main(program);
    let mut out = Vec::new();
    collect(projection.body(), &mut out);
    out
}

#[test]
fn a_direct_ir_loop_with_a_multi_statement_body_is_per_statement() {
    let program = b::program(vec![b::for_in(
        "item",
        b::list(vec![b::num(1.0)]),
        b::block(vec![echo("first"), echo("second")]),
    )]);
    assert_eq!(
        main_statement_paths(&program),
        vec![vec![0], vec![0, 1, 0], vec![0, 1, 1]]
    );
}

#[test]
fn an_iteration_bind_belongs_to_the_loop_and_its_body_is_per_statement() {
    let program = b::program(vec![b::for_bind(
        "element",
        b::list(vec![b::num(1.0)]),
        b::block(vec![b::assign("item", b::var("element"))]),
        completion(vec![echo("first"), echo("second")]),
    )]);
    assert_eq!(
        main_statement_paths(&program),
        vec![vec![0], vec![0, 2, 0, 0], vec![0, 2, 0, 1]],
        "the body sits after the bind, and its completion value is no statement"
    );
    let projection = WorkflowProjection::for_main(&program);
    assert_eq!(
        projection
            .ownership_map()
            .path_for_ast(&AstPath::main(vec![0, 1, 0]))
            .map(|path| path.indices().to_vec()),
        Some(vec![0]),
        "the bind's statements are owned by the loop node"
    );
}

#[test]
fn a_completion_wrapped_statement_is_the_statement_it_wraps() {
    let program = b::program(vec![
        b::assign("total", b::num(0.0)),
        b::role(
            StructuralRole::Completion,
            b::block(vec![b::assign("total", b::num(1.0)), b::var("total")]),
        ),
    ]);
    let graph = workflow_graph_from_program(&program, &NoStatementText);
    assert!(
        matches!(
            graph.main.nodes[1].kind,
            WorkflowNodeKind::StateUpdate { .. }
        ),
        "the wrapped assignment projects as the state update it is: {:?}",
        graph.main.nodes[1].kind
    );
}

#[test]
fn an_attribute_assignment_projects_its_authored_target() {
    let program = b::program(vec![
        b::assign("state", b::record(vec![("count", b::num(0.0))])),
        b::role(
            StructuralRole::AttributeAssign,
            b::block(vec![
                b::assign("base", b::var("state")),
                b::assign("result", b::num(1.0)),
                b::assign_path("base", vec![b::field_step("count")], b::var("result")),
                b::var("result"),
            ]),
        ),
    ]);
    validate_ast(&program).expect("the attribute assignment has its role's shape");
    let graph = workflow_graph_from_program(&program, &NoStatementText);
    let WorkflowNodeKind::StateUpdate { target, .. } = &graph.main.nodes[1].kind else {
        panic!("expected a state update: {:?}", graph.main.nodes[1].kind);
    };
    assert_eq!(target.root.as_str(), "state");
}

#[test]
fn a_direct_ir_process_body_is_projected_from_the_first_statement() {
    let linked = link_labeled(b::module(
        vec![b::process(
            "worker",
            Vec::new(),
            b::block(vec![echo("first"), echo("second"), b::finish(b::null())]),
        )],
        Vec::new(),
    ));
    let program = &linked.artifact.ir();
    let crate::Declaration::Process(process) = &program.declarations[0] else {
        panic!("one process")
    };
    let projection =
        WorkflowProjection::for_process(&process.body, AstPath::declaration(0, Vec::new()));
    let paths = projection
        .body()
        .statements
        .iter()
        .map(|statement| statement.node_path.indices().to_vec())
        .collect::<Vec<_>>();
    assert_eq!(paths, vec![vec![0, 0], vec![0, 1], vec![0, 2]]);
}

#[test]
fn malformed_roles_are_refused() {
    let cases = [
        (StructuralRole::Scope, b::num(1.0)),
        (StructuralRole::Completion, b::num(1.0)),
        (StructuralRole::Completion, b::block(vec![echo("impure")])),
        (
            StructuralRole::AttributeAssign,
            b::block(vec![b::assign("base", b::var("state"))]),
        ),
        (
            StructuralRole::CollectionTransform {
                operation: "map".into(),
            },
            b::block(vec![b::num(1.0)]),
        ),
        // Receiver and callback bound, but no driver capturing them: the
        // shape the one-line check accepted before FIG-3571 tightened it.
        (
            StructuralRole::CollectionTransform {
                operation: "map".into(),
            },
            b::block(vec![
                b::assign("receiver", b::list(vec![])),
                b::assign("callback", b::closure(None, &["item"], &[], b::var("item"))),
                b::num(1.0),
            ]),
        ),
        // An attribute update whose value reads the pinned base other than as
        // the current attribute.
        (
            StructuralRole::AttributeAssign,
            b::block(vec![
                b::assign("base", b::var("state")),
                b::assign("result", b::var("base")),
                b::assign_path("base", vec![b::field_step("field")], b::var("result")),
                b::var("result"),
            ]),
        ),
        (StructuralRole::ProcessWrapper, b::finish(b::null())),
    ];
    for (role, expr) in cases {
        let name = role.name();
        let program = b::program(vec![b::role(role, expr)]);
        assert!(
            matches!(
                validate_ast(&program),
                Err(InvalidAst::MalformedRole { role, .. }) if role == name
            ),
            "a malformed `{name}` role is refused"
        );
    }
}
