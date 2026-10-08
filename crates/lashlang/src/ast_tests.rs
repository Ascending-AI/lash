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

    let schema = serde_json::to_value(schemars::schema_for!(TypeExpr)).unwrap();
    let validator = jsonschema::validator_for(&schema).expect("type schema compiles");
    for value in [
        serde_json::to_value(&process).unwrap(),
        serde_json::to_value(&unknown).unwrap(),
    ] {
        assert!(validator.is_valid(&value), "schema rejected {value}");
        serde_json::from_value::<TypeExpr>(value).expect("schema-valid process decodes");
    }
    for value in [
        serde_json::json!({"Process": {"kind": "known", "params": []}}),
        serde_json::json!({"Process": {"kind": "unknown", "extra": true}}),
        serde_json::json!({"Process": {
            "kind": "known",
            "params": [{"name": "message", "ty": "Str", "extra": true}],
            "output": "Bool"
        }}),
    ] {
        assert!(!validator.is_valid(&value), "schema accepted {value}");
        assert!(
            serde_json::from_value::<TypeExpr>(value.clone()).is_err(),
            "decoder accepted {value}"
        );
    }
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
        TypeExpr::union(vec![TypeExpr::Str, TypeExpr::Int]),
    )
    .expect("FIG-2879 does not add unrelated TypeExpr restrictions");
}

#[test]
fn union_members_hold_at_least_two_variants() {
    // FIG-3269: the two-member floor is structural. `UnionMembers::new`
    // refuses degenerate member lists and `TypeExpr::union` collapses
    // them instead.
    assert!(UnionMembers::new(Vec::new()).is_none());
    assert!(UnionMembers::new(vec![TypeExpr::Str]).is_none());
    assert_eq!(
        TypeExpr::union(vec![TypeExpr::Str]),
        TypeExpr::Str,
        "a one-member union collapses to the member"
    );
    assert_eq!(
        TypeExpr::union(vec![TypeExpr::Str, TypeExpr::Str]),
        TypeExpr::Str,
        "duplicate members deduplicate before the floor is checked"
    );

    // The wire keeps the bare member sequence `Union(Vec)` wrote, and
    // decoding a degenerate sequence refuses.
    let union = TypeExpr::union(vec![TypeExpr::Str, TypeExpr::Null]);
    let wire = serde_json::to_value(&union).unwrap();
    assert_eq!(wire, serde_json::json!({"Union": ["Str", "Null"]}));
    assert_eq!(serde_json::from_value::<TypeExpr>(wire).unwrap(), union);
    for degenerate in [
        serde_json::json!({"Union": []}),
        serde_json::json!({"Union": ["Str"]}),
    ] {
        let label = degenerate.to_string();
        assert!(
            serde_json::from_value::<TypeExpr>(degenerate).is_err(),
            "degenerate union wire must refuse: {label}"
        );
    }

    let schema = serde_json::to_value(schemars::schema_for!(TypeExpr)).unwrap();
    let validator = jsonschema::validator_for(&schema).expect("type schema compiles");
    for (members, accepted) in [
        (serde_json::json!([]), false),
        (serde_json::json!(["Str"]), false),
        (serde_json::json!(["Str", "Null"]), true),
    ] {
        let value = serde_json::json!({"Union": members});
        assert_eq!(validator.is_valid(&value), accepted, "{value}");
        assert_eq!(serde_json::from_value::<TypeExpr>(value).is_ok(), accepted);
    }
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
fn a_started_handle_may_be_the_inferred_output_of_a_process() {
    // Since ADR 0095 `processes.start` answers the one process type of unknown
    // signature, so a bound start handle can be finished and the linker infers
    // it as the declaration's output. The refusal stays on authored signature
    // positions: the same type as a parameter is still refused.
    let unknown = TypeExpr::Process(ProcessType::unknown());
    let mut finishes_a_handle = Program::block(Vec::new());
    finishes_a_handle.declarations = vec![Declaration::Process(ProcessDecl {
        name: "main".into(),
        params: Vec::new(),

        return_ty: Some(TypeExpr::Object(vec![TypeField {
            name: "joined".into(),
            ty: unknown.clone(),
            optional: false,
        }])),
        label: None,
        origin: Default::default(),
        body: Expr::Null,
    })];
    assert!(validate_ast(&finishes_a_handle).is_ok());

    let mut declares_a_handle_param = Program::block(Vec::new());
    declares_a_handle_param.declarations = vec![Declaration::Process(ProcessDecl {
        name: "main".into(),
        params: vec![param("handle", unknown)],

        return_ty: None,
        label: None,
        origin: Default::default(),
        body: Expr::Null,
    })];
    assert!(matches!(
        validate_ast(&declares_a_handle_param),
        Err(InvalidAst::UnknownProcessSignature)
    ));
}

fn var(name: &str) -> Expr {
    Expr::Variable(name.into())
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
