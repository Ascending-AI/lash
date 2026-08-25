use super::*;

const CENSUS_MARKER_PREFIX: &str = "child-expression-field:";

fn marker(field: &str) -> Expr {
    Expr::Ident(format!("{CENSUS_MARKER_PREFIX}{field}"))
}

fn collect_markers(expr: &Expr, markers: &mut BTreeSet<String>) {
    if let Expr::Ident(name) = expr
        && let Some(field) = name.strip_prefix(CENSUS_MARKER_PREFIX)
    {
        markers.insert(field.to_owned());
    }
    for child in expr.children() {
        collect_markers(child, markers);
    }
}

fn assert_field_census<'a>(
    accessor: &str,
    expected: &[&str],
    roots: impl Iterator<Item = &'a Expr>,
) {
    let expected = expected
        .iter()
        .map(|field| (*field).to_owned())
        .collect::<BTreeSet<_>>();
    let mut actual = BTreeSet::new();
    for root in roots {
        collect_markers(root, &mut actual);
    }

    let missing = expected.difference(&actual).cloned().collect::<Vec<_>>();
    let unexpected = actual.difference(&expected).cloned().collect::<Vec<_>>();
    assert!(
        missing.is_empty() && unexpected.is_empty(),
        "{accessor} field census mismatch\nmissing: {missing:#?}\nunexpected: {unexpected:#?}"
    );
}

fn default_pattern(field: &str) -> Pattern {
    Pattern::Assign {
        target: Box::new(Pattern::Ident("value".into())),
        default: Box::new(marker(field)),
    }
}

fn function(params: Vec<Pattern>, body: FunctionBody) -> Function {
    Function {
        name: None,
        params,
        body,
        is_async: false,
    }
}

#[test]
fn child_expression_accessors_reach_every_expression_and_statement_field() {
    let expression_carriers = vec![
        Expr::Array(vec![
            ArrayElement::Value(marker("Array.value")),
            ArrayElement::Spread(marker("Array.spread")),
        ]),
        Expr::Object(vec![
            ObjectProperty::KeyValue(
                PropertyKey::Computed(Box::new(marker("Object.key_value.key"))),
                marker("Object.key_value.value"),
            ),
            ObjectProperty::Spread(marker("Object.spread")),
        ]),
        Expr::Assign {
            target: AssignTarget::Pattern(Box::new(default_pattern("Assign.target"))),
            op: AssignOp::Assign,
            value: Box::new(marker("Assign.value")),
        },
        Expr::Member {
            object: Box::new(marker("Member.object")),
            property: MemberProperty::Index(Box::new(marker("Member.property"))),
        },
        Expr::Unary {
            op: UnaryOp::Not,
            value: Box::new(marker("Unary.value")),
        },
        Expr::Binary {
            left: Box::new(marker("Binary.left")),
            op: BinaryOp::Add,
            right: Box::new(marker("Binary.right")),
        },
        Expr::Logical {
            left: Box::new(marker("Logical.left")),
            op: LogicalOp::And,
            right: Box::new(marker("Logical.right")),
        },
        Expr::Conditional {
            test: Box::new(marker("Conditional.test")),
            consequent: Box::new(marker("Conditional.consequent")),
            alternate: Box::new(marker("Conditional.alternate")),
        },
        Expr::Template {
            quasis: vec![String::new(), String::new()],
            expressions: vec![marker("Template.expressions")],
        },
        Expr::Function(function(
            vec![default_pattern("Function.params")],
            FunctionBody::Expression(Box::new(marker("Function.expression_body"))),
        )),
        Expr::Function(function(
            Vec::new(),
            FunctionBody::Block(vec![Stmt::Expr(marker("Function.block_body"))]),
        )),
        Expr::Call {
            callee: Box::new(marker("Call.callee")),
            args: vec![
                CallArg::Value(marker("Call.args.value")),
                CallArg::Spread(marker("Call.args.spread")),
            ],
        },
        Expr::New {
            constructor: "Set".into(),
            args: vec![
                CallArg::Value(marker("New.args.value")),
                CallArg::Spread(marker("New.args.spread")),
            ],
        },
        Expr::OptionalChain {
            base: Box::new(marker("OptionalChain.base")),
            operations: vec![
                OptionalOperation::Member {
                    property: MemberProperty::Index(Box::new(marker(
                        "OptionalChain.member.property",
                    ))),
                    optional: true,
                },
                OptionalOperation::Call {
                    args: vec![
                        CallArg::Value(marker("OptionalChain.call.args.value")),
                        CallArg::Spread(marker("OptionalChain.call.args.spread")),
                    ],
                    optional: true,
                },
            ],
        },
        Expr::Await(Box::new(marker("Await.value"))),
        Expr::Update {
            target: AssignTarget::Member {
                object: Box::new(marker("Update.target.object")),
                property: MemberProperty::Index(Box::new(marker("Update.target.property"))),
            },
            delta: 1.0,
            prefix: false,
        },
        Expr::Delete {
            object: Box::new(marker("Delete.object")),
            property: MemberProperty::Index(Box::new(marker("Delete.property"))),
        },
    ];
    assert_field_census(
        "Expr::children",
        &[
            "Array.value",
            "Array.spread",
            "Object.key_value.key",
            "Object.key_value.value",
            "Object.spread",
            "Assign.target",
            "Assign.value",
            "Member.object",
            "Member.property",
            "Unary.value",
            "Binary.left",
            "Binary.right",
            "Logical.left",
            "Logical.right",
            "Conditional.test",
            "Conditional.consequent",
            "Conditional.alternate",
            "Template.expressions",
            "Function.params",
            "Function.expression_body",
            "Function.block_body",
            "Call.callee",
            "Call.args.value",
            "Call.args.spread",
            "New.args.value",
            "New.args.spread",
            "OptionalChain.base",
            "OptionalChain.member.property",
            "OptionalChain.call.args.value",
            "OptionalChain.call.args.spread",
            "Await.value",
            "Update.target.object",
            "Update.target.property",
            "Delete.object",
            "Delete.property",
        ],
        expression_carriers.iter().flat_map(Expr::children),
    );

    let statement_carriers = vec![
        Stmt::Expr(marker("Expr.expression")),
        Stmt::Block(vec![Stmt::Expr(marker("Block.statements"))]),
        Stmt::Var {
            kind: VarKind::Const,
            declarations: vec![Var {
                pattern: default_pattern("Var.declarations.pattern"),
                init: Some(marker("Var.declarations.init")),
            }],
        },
        Stmt::Enum {
            name: "E".into(),
            members: vec![EnumMember {
                name: "M".into(),
                value: marker("Enum.members.value"),
                reverse: false,
            }],
        },
        Stmt::Function {
            name: "f".into(),
            function: function(
                vec![default_pattern("Function.params")],
                FunctionBody::Block(vec![Stmt::Expr(marker("Function.block_body"))]),
            ),
        },
        Stmt::Function {
            name: "f".into(),
            function: function(
                Vec::new(),
                FunctionBody::Expression(Box::new(marker("Function.expression_body"))),
            ),
        },
        Stmt::Return(Some(marker("Return.value"))),
        Stmt::If {
            test: marker("If.test"),
            consequent: Box::new(Stmt::Expr(marker("If.consequent"))),
            alternate: Some(Box::new(Stmt::Expr(marker("If.alternate")))),
        },
        Stmt::While {
            test: marker("While.test"),
            body: Box::new(Stmt::Expr(marker("While.body"))),
        },
        Stmt::DoWhile {
            body: Box::new(Stmt::Expr(marker("DoWhile.body"))),
            test: marker("DoWhile.test"),
        },
        Stmt::For {
            init: Some(Box::new(Stmt::Expr(marker("For.init")))),
            test: Some(marker("For.test")),
            update: Some(marker("For.update")),
            body: Box::new(Stmt::Expr(marker("For.body"))),
        },
        Stmt::ForOf {
            pattern: default_pattern("ForOf.pattern"),
            kind: Some(VarKind::Const),
            iterable: marker("ForOf.iterable"),
            body: Box::new(Stmt::Expr(marker("ForOf.body"))),
        },
        Stmt::ForIn {
            pattern: default_pattern("ForIn.pattern"),
            kind: Some(VarKind::Const),
            object: marker("ForIn.object"),
            body: Box::new(Stmt::Expr(marker("ForIn.body"))),
        },
        Stmt::Switch {
            discriminant: marker("Switch.discriminant"),
            cases: vec![SwitchCase {
                test: Some(marker("Switch.cases.test")),
                consequent: vec![Stmt::Expr(marker("Switch.cases.consequent"))],
            }],
        },
        Stmt::Throw(marker("Throw.expression")),
        Stmt::Try {
            body: vec![Stmt::Expr(marker("Try.body"))],
            catch: Some(Catch {
                binding: Some(default_pattern("Try.catch.binding")),
                body: vec![Stmt::Expr(marker("Try.catch.body"))],
            }),
            finally: Some(vec![Stmt::Expr(marker("Try.finally"))]),
        },
    ];
    assert_field_census(
        "Stmt::child_expressions",
        &[
            "Expr.expression",
            "Block.statements",
            "Var.declarations.pattern",
            "Var.declarations.init",
            "Enum.members.value",
            "Function.params",
            "Function.block_body",
            "Function.expression_body",
            "Return.value",
            "If.test",
            "If.consequent",
            "If.alternate",
            "While.test",
            "While.body",
            "DoWhile.body",
            "DoWhile.test",
            "For.init",
            "For.test",
            "For.update",
            "For.body",
            "ForOf.pattern",
            "ForOf.iterable",
            "ForOf.body",
            "ForIn.pattern",
            "ForIn.object",
            "ForIn.body",
            "Switch.discriminant",
            "Switch.cases.test",
            "Switch.cases.consequent",
            "Throw.expression",
            "Try.body",
            "Try.catch.binding",
            "Try.catch.body",
            "Try.finally",
        ],
        statement_carriers.iter().flat_map(Stmt::child_expressions),
    );

    let pattern_carriers = [
        Pattern::Rest(Box::new(default_pattern("Rest.target"))),
        Pattern::Member {
            object: Box::new(marker("Member.object")),
            property: MemberProperty::Index(Box::new(marker("Member.property"))),
        },
        Pattern::Assign {
            target: Box::new(default_pattern("Assign.target")),
            default: Box::new(marker("Assign.default")),
        },
        Pattern::Array {
            elements: vec![Some(default_pattern("Array.elements"))],
            rest: Some(Box::new(default_pattern("Array.rest"))),
        },
        Pattern::Object {
            properties: vec![ObjectPatternProperty {
                key: PropertyKey::Computed(Box::new(marker("Object.properties.key"))),
                value: default_pattern("Object.properties.value"),
            }],
            rest: Some(Box::new(default_pattern("Object.rest"))),
        },
    ];
    assert_field_census(
        "Pattern::child_expressions",
        &[
            "Rest.target",
            "Member.object",
            "Member.property",
            "Assign.target",
            "Assign.default",
            "Array.elements",
            "Array.rest",
            "Object.properties.key",
            "Object.properties.value",
            "Object.rest",
        ],
        pattern_carriers.iter().flat_map(Pattern::child_expressions),
    );
}
