//! Unit tests for the `Expr` AST: type-expression wire shapes, validation
//! refusals, and the `children` traversal primitive.

use super::*;

fn param(name: &str, ty: TypeExpr) -> ProcessParam {
    ProcessParam {
        name: name.into(),
        ty,
    }
}

#[test]
fn process_signature_construction_and_wire_shape_are_checked() {
    let process = TypeExpr::Process(ProcessType::known(
        ProcessSignature::try_new(vec![param("message", TypeExpr::Str)], TypeExpr::Bool)
            .expect("valid signature"),
    ));
    assert_eq!(
        serde_json::to_value(&process).unwrap(),
        serde_json::json!({
            "Process": {
                "kind": "known",
                "params": [{"name": "message", "ty": "Str"}],
                "output": "Bool"
            }
        })
    );
    assert_eq!(
        serde_json::from_value::<TypeExpr>(serde_json::to_value(&process).unwrap()).unwrap(),
        process
    );
    assert_eq!(format_type_expr(&process), "Process<(message: str), bool>");

    let unknown = TypeExpr::Process(ProcessType::unknown());
    assert_eq!(
        serde_json::to_value(&unknown).unwrap(),
        serde_json::json!({"Process": {"kind": "unknown"}})
    );
    assert_eq!(format_type_expr(&unknown), "Process");
}

#[test]
fn process_signature_refuses_invalid_names_without_broadening_type_validation() {
    assert!(matches!(
        ProcessSignature::try_new(vec![param("1bad", TypeExpr::Str)], TypeExpr::Bool),
        Err(ProcessSignatureError::InvalidParameterName { .. })
    ));
    assert!(matches!(
        ProcessSignature::try_new(vec![param("if", TypeExpr::Str)], TypeExpr::Bool),
        Err(ProcessSignatureError::InvalidParameterName { .. })
    ));
    assert!(matches!(
        ProcessSignature::try_new(
            vec![param("value", TypeExpr::Str), param("value", TypeExpr::Int)],
            TypeExpr::Bool,
        ),
        Err(ProcessSignatureError::DuplicateParameter { .. })
    ));
    ProcessSignature::try_new(
        vec![param("value", TypeExpr::Enum(Vec::new()))],
        TypeExpr::Union(vec![TypeExpr::Str]),
    )
    .expect("FIG-2879 does not add unrelated TypeExpr restrictions");
}

#[test]
fn process_signature_decode_refuses_missing_duplicate_unknown_and_legacy_fields() {
    for wire in [
        r#"{"Process":{"params":[],"output":"Bool"}}"#,
        r#"{"Process":{"kind":null}}"#,
        r#"{"Process":{"kind":"known","params":null,"output":"Bool"}}"#,
        r#"{"Process":{"kind":"known","params":[],"output":null}}"#,
        r#"{"Process":{"kind":"known","params":[],"params":[],"output":"Bool"}}"#,
        r#"{"Process":{"kind":"known","params":[],"output":"Bool","extra":true}}"#,
        r#"{"Process":{"kind":"known","params":[{"name":"x","ty":"Str","extra":true}],"output":"Bool"}}"#,
        r#"{"Process":{"input":"Str","output":"Bool","input_count":1}}"#,
        r#"{"Process":{"kind":"known","params":[{"name":"x","ty":"Str"},{"name":"x","ty":"Int"}],"output":"Bool"}}"#,
        r#"{"Process":{"kind":"known","params":[{"name":"outer","ty":{"Process":{"kind":"known","params":[{"name":"x","ty":"Str"},{"name":"x","ty":"Int"}],"output":"Bool"}}}],"output":"Bool"}}"#,
    ] {
        assert!(serde_json::from_str::<TypeExpr>(wire).is_err(), "{wire}");
    }
}

#[test]
fn unknown_process_type_is_refused_in_program_ir() {
    let program = Program::block(vec![Expr::TypeLiteral(Box::new(TypeExpr::Process(
        ProcessType::unknown(),
    )))]);
    assert!(matches!(
        validate_ast(&program),
        Err(InvalidAst::UnknownProcessSignature)
    ));
}

#[test]
fn type_expr_formatting_covers_nested_shapes() {
    let ty = TypeExpr::Object(vec![
        TypeField {
            name: "status".into(),
            ty: TypeExpr::Enum(vec!["ok".into(), "err".into()]),
            optional: false,
        },
        TypeField {
            name: "tags".into(),
            ty: TypeExpr::List(Box::new(TypeExpr::Str)),
            optional: true,
        },
        TypeField {
            name: "owner".into(),
            ty: TypeExpr::Ref("User".into()),
            optional: false,
        },
        TypeField {
            name: "value".into(),
            ty: TypeExpr::Union(vec![TypeExpr::Int, TypeExpr::Null]),
            optional: false,
        },
    ]);

    assert_eq!(
        format_type_expr(&ty),
        r#"{ status: enum["ok", "err"], tags: list[str]?, owner: User, value: int | null }"#
    );
    assert_eq!(ty.to_string(), format_type_expr(&ty));
}

fn var(name: &str) -> Expr {
    Expr::Variable(name.into())
}

fn child_vars(expr: &Expr) -> Vec<String> {
    expr.children()
        .map(|child| match child {
            Expr::Variable(name) => name.to_string(),
            other => format!("{other:?}"),
        })
        .collect()
}

#[test]
fn children_yields_leaves_as_empty() {
    for leaf in [
        Expr::Null,
        Expr::Bool(true),
        Expr::Number(1.0),
        Expr::String("s".into()),
        var("x"),
        Expr::Break,
        Expr::Continue,
        Expr::WaitSignal {
            name: "ready".into(),
        },
        Expr::TypeLiteral(Box::new(TypeExpr::Str)),
    ] {
        let children: Vec<_> = leaf.children().collect();
        assert!(children.is_empty(), "{leaf:?} should have no children");
    }
}

#[test]
fn children_yields_composite_subexpressions_in_order() {
    let block = Expr::Block(vec![var("a"), var("b"), var("c")]);
    assert_eq!(child_vars(&block), ["a", "b", "c"]);

    let record = Expr::Record(vec![("k1".into(), var("v1")), ("k2".into(), var("v2"))]);
    assert_eq!(child_vars(&record), ["v1", "v2"]);

    let if_expr = Expr::If {
        condition: Box::new(var("cond")),
        then_block: Box::new(var("then")),
        else_block: Box::new(var("else")),
    };
    assert_eq!(child_vars(&if_expr), ["cond", "then", "else"]);

    let while_expr = Expr::While {
        condition: Box::new(var("cond")),
        body: Box::new(var("body")),
    };
    assert_eq!(child_vars(&while_expr), ["cond", "body"]);

    let receiver = Expr::ReceiverCall {
        receiver: Box::new(var("recv")),
        operation: "op".into(),
        args: vec![var("arg0"), var("arg1")],
    };
    assert_eq!(child_vars(&receiver), ["recv", "arg0", "arg1"]);

    let binary = Expr::Binary {
        left: Box::new(var("left")),
        op: BinaryOp::Add,
        right: Box::new(var("right")),
    };
    assert_eq!(child_vars(&binary), ["left", "right"]);
}

#[test]
fn children_yields_assign_index_steps_before_value() {
    let assign = Expr::Assign {
        target: AssignTarget {
            root: "root".into(),
            steps: vec![
                AssignPathStep::Field("field".into()),
                AssignPathStep::Index(var("idx")),
            ],
        },
        expr: Box::new(var("value")),
    };
    // Field steps contribute no child expressions; the dynamic index is
    // yielded before the assigned value.
    assert_eq!(child_vars(&assign), ["idx", "value"]);
}

#[test]
fn children_handles_finish() {
    assert_eq!(child_vars(&Expr::Finish(Box::new(var("done")))), ["done"]);
}

#[test]
fn children_size_hint_is_exact() {
    let block = Expr::Block(vec![var("a"), var("b"), var("c"), var("d")]);
    let iter = block.children();
    assert_eq!(iter.len(), 4);
    assert_eq!(iter.size_hint(), (4, Some(4)));
}

#[test]
fn visitor_walks_descendants_through_single_child_boundary() {
    struct VariableCollector(Vec<String>);

    impl ExprVisitor for VariableCollector {
        fn visit_expr(&mut self, expr: &Expr) {
            if let Expr::Variable(name) = expr {
                self.0.push(name.to_string());
            }
            walk_expr(self, expr);
        }
    }

    let expr = Expr::While {
        condition: Box::new(var("ready")),
        body: Box::new(Expr::Block(vec![
            Expr::Assign {
                target: AssignTarget {
                    root: "items".into(),
                    steps: vec![AssignPathStep::Index(var("idx"))],
                },
                expr: Box::new(var("value")),
            },
            Expr::Finish(Box::new(var("done"))),
        ])),
    };

    let mut collector = VariableCollector(Vec::new());
    collector.visit_expr(&expr);

    assert_eq!(collector.0, ["ready", "idx", "value", "done"]);
}

#[test]
fn folder_reconstructs_owned_expr_trees() {
    struct RenameVariables;

    impl ExprFolder for RenameVariables {
        fn fold_expr(&mut self, expr: Expr) -> Expr {
            match expr {
                Expr::Variable(name) => Expr::Variable(format!("renamed_{name}").into()),
                other => fold_expr_children(self, other),
            }
        }
    }

    let expr = Expr::Assign {
        target: AssignTarget {
            root: "items".into(),
            steps: vec![AssignPathStep::Index(var("idx"))],
        },
        expr: Box::new(Expr::List(vec![var("first"), var("second")])),
    };

    let mut folder = RenameVariables;
    let folded = folder.fold_expr(expr);

    let Expr::Assign { target, expr } = folded else {
        panic!("expected assign");
    };
    assert!(matches!(
        target.steps.as_slice(),
        [AssignPathStep::Index(Expr::Variable(name))] if name.as_str() == "renamed_idx"
    ));
    let Expr::List(items) = *expr else {
        panic!("expected list");
    };
    assert_eq!(items, vec![var("renamed_first"), var("renamed_second")]);
}
