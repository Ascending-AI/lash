//! CANVAS and EXTRACT laws for the public boundary.
use crate::apply::Working;
use crate::authoring::{Branch, DataValue, Operation, ValueTarget, Wrapper};
use crate::tree::NodeMut;
use crate::{Draft, EditDiagnosticKind as Cause, EditRefusal, Position, Transaction};
use lash_kernel_check::{Environment, RefusalReason};
use lash_kernel_doc::heap::{HeapFault, PageGrant, PageId, PageRef};
use lash_kernel_doc::{
    ActExpr, ActKind, Datum, Document, DocumentId, EffectName, ErrorDatum, Expr, FunctionRegistry,
    Handle, Integer, InvalidReason, Literal, Name, Node, Param, Signature, Site, Stmt, Timestamp,
    Type, Unit, parse_document, print_document,
};
use lash_kernel_state::paged::{PageAnswer, PageBody, PageSource};
use lash_kernel_vm::{
    Bounds, CellStart, End, Host, KernelMachine, Machine, Outcome, Program, Request, Step, Target,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

fn doc(body: &str) -> Document {
    let mut document = parse_document(&format!(
        "kernel 1\nnumbers float\neffect send(value: Any) -> Any\nmain {{ {body} }}"
    ))
    .unwrap();
    let required = lash_kernel_check::requirements(&document, &FunctionRegistry::new());
    document
        .manifest
        .effects
        .retain(|effect, _| required.effects.contains(effect));
    document
}
fn site(path: &[u32]) -> Site {
    Site::new(Unit::Main, path.to_vec())
}
fn value(text: &str) -> DataValue {
    DataValue::Literal(Literal::Text(text.into()))
}
fn expr(text: &str) -> Expr {
    Expr::Literal(Literal::Text(text.into()))
}
fn env(registry: &FunctionRegistry) -> Environment<'_> {
    let mut env = Environment::new(registry);
    env.effects.insert(
        EffectName::new("send").unwrap(),
        Signature {
            params: vec![Param {
                name: Name::new("value"),
                ty: Type::Any,
                optional: false,
            }],
            result: Type::Any,
        },
    );
    env
}
fn batch(draft: &Draft, edits: Vec<Operation>) -> Transaction {
    Transaction {
        base: draft.identity(),
        edits,
    }
}
fn rejected(
    draft: &mut Draft,
    edits: Vec<Operation>,
    environment: &Environment<'_>,
) -> EditRefusal {
    let held = draft.clone();
    let error = draft.apply(&batch(draft, edits), environment).unwrap_err();
    assert_eq!(draft.document(), held.document());
    assert_eq!(draft.identity(), held.identity());
    assert_eq!(
        draft.correspondence_since_open(),
        held.correspondence_since_open()
    );
    // Transport retains the complete cause, including nested checker fields.
    let json = serde_json::to_string(&error).unwrap();
    assert_eq!(serde_json::from_str::<EditRefusal>(&json).unwrap(), error);
    error
}
fn insert(base: DocumentId, at: Position, binding: &str) -> Operation {
    Operation::InsertStep {
        base,
        at,
        effect: EffectName::new("send").unwrap(),
        args: vec![value("new")],
        result: Type::Any,
        binding: Some(Name::new(binding)),
    }
}

fn relink_library(value: &str) -> (FunctionRegistry, lash_kernel_doc::FunctionId) {
    let mut registry = FunctionRegistry::new();
    let g = registry
        .register(
            lash_kernel_doc::parse_definition(&format!(
                "function helper.g() -> Any\nkernel 1\ncharge 1\nbody {{ return {value} }}"
            ))
            .unwrap(),
            None,
        )
        .unwrap();
    let f = registry.register(lash_kernel_doc::parse_definition(&format!(
        "function helper.f() -> Any\nkernel 1\ncharge 1\nuse helper.g = @{g}\nbody {{ let y = invoke helper.g() return y }}"
    )).unwrap(), None).unwrap();
    (registry, f)
}

fn relink_document(registry: &FunctionRegistry, f: lash_kernel_doc::FunctionId) -> Document {
    let mut document = parse_document(&format!(
        "kernel 1\nnumbers float\nuse helper.f = @{f}\nentry run() -> Any\nfn run() {{ let y = invoke helper.f() return y }}\nmain {{}}"
    )).unwrap();
    document.manifest.functions = lash_kernel_check::requirements(&document, registry).functions;
    document
}

/// RELINK-CLOSURE (FIG-6550): a stored entry -> f -> g is linked in one
/// transaction, including g even though no document node calls it directly.
#[test]
fn relink_transitive_manifest_closure() {
    let (old, old_f) = relink_library("null");
    let original = relink_document(&old, old_f);
    let stored = Document::from_json(&original.to_json().unwrap()).unwrap();
    let mut draft = Draft::open(stored).unwrap();
    let (current, current_f) = relink_library("true");
    let expected = relink_document(&current, current_f);
    assert_ne!(original.manifest.functions, expected.manifest.functions);
    assert_eq!(expected.manifest.functions.len(), 2);

    let correspondence = draft.relink(&current).unwrap();
    assert_eq!(correspondence.base, original.identity());
    assert_eq!(correspondence.result, expected.identity());
    assert!(correspondence.entries().iter().all(|s| s.from == s.to));
    assert!(correspondence.entries().iter().any(|s| s.edited));
    assert_eq!(draft.correspondence_since_open(), &correspondence);

    assert_eq!(draft.document(), &expected);
    assert_eq!(draft.identity(), expected.identity());
    assert_eq!(
        draft.document().manifest.functions,
        lash_kernel_check::requirements(draft.document(), &current).functions
    );
}

/// RELINK-ATOMIC: missing names, ambiguous names and invalid replacements
/// refuse without publishing any part of a re-link.
#[test]
fn relink_refusals_leave_the_draft_unchanged() {
    let (old, old_f) = relink_library("null");
    let original = relink_document(&old, old_f);
    let mut draft = Draft::open(original.clone()).unwrap();
    let correspondence = draft.correspondence_since_open().clone();
    let missing = FunctionRegistry::new();
    let (mut ambiguous, _) = relink_library("true");
    for (_, registered) in old.iter().filter(|(id, _)| **id != old_f) {
        ambiguous
            .register(registered.definition().as_ref().clone(), None)
            .unwrap();
    }
    ambiguous
        .register(old.get(&old_f).unwrap().definition().as_ref().clone(), None)
        .unwrap();
    let mut invalid = FunctionRegistry::new();
    invalid
        .register(
            lash_kernel_doc::parse_definition(
                "function helper.f(x: Any) -> Any\nkernel 1\ncharge 1\nbody { return x }",
            )
            .unwrap(),
            None,
        )
        .unwrap();
    for (registry, expected) in [
        (&missing, "missing"),
        (&ambiguous, "ambiguous"),
        (&invalid, "invalid"),
    ] {
        let error = draft.relink(registry).unwrap_err();
        match (expected, error) {
            ("missing", crate::RelinkRefusal::MissingFunction { function, name }) => {
                assert_eq!(function, old_f);
                assert_eq!(name.as_str(), "helper.f");
            }
            ("ambiguous", crate::RelinkRefusal::AmbiguousFunction { name, functions }) => {
                assert_eq!(name.as_str(), "helper.f");
                assert_eq!(functions.len(), 2);
            }
            ("invalid", crate::RelinkRefusal::Refused(refusal)) => {
                assert!(refusal.diagnostics.iter().any(|diagnostic| matches!(
                    diagnostic.kind,
                    Cause::Refused(RefusalReason::Invalid(InvalidReason::CallArity { .. }))
                )));
            }
            (_, error) => panic!("unexpected {expected} refusal: {error}"),
        }
        assert_eq!(draft.document(), &original);
        assert_eq!(draft.identity(), original.identity());
        assert_eq!(draft.correspondence_since_open(), &correspondence);
    }
}

/// CANVAS-CLOSED: raw syntax and the old passthrough cannot decode.
#[test]
fn canvas_closed() {
    let base = doc("").identity();
    let raw = serde_json::json!({"base": base, "edits": [{"replace_statement": {"statement": {"unit":"main","path":[0]}, "with":{"finish":{"value":{"literal":"null"}}}}}]});
    assert!(
        Transaction::from_json(&raw.to_string()).is_err(),
        "raw statement replacement reached the public edit boundary"
    );
    let node = serde_json::json!({"unit": "main", "path": [0]});
    for retired in [
        serde_json::json!({"set_label": {"node": node.clone(), "label": {"title": "host"}}}),
        serde_json::json!({"set_data": {"node": node.clone(), "key": "layout", "value": {"x": 1}}}),
    ] {
        assert!(serde_json::from_value::<Operation>(retired.clone()).is_err());
        assert!(
            Transaction::from_json(
                &serde_json::json!({"base": base, "edits": [retired]}).to_string()
            )
            .is_err()
        );
    }
    for name in [
        "replace_statement",
        "replace_expression",
        "rename_variable",
        "rename_function",
        "rename_entry",
        "set_entry_signature",
        "set_effect_signature",
        "replace_function_identity",
        "insert_statement",
        "set_catch",
        "set_finally",
        "clone_statement",
        "edit",
        "insert_effect",
    ] {
        let raw = serde_json::json!({name: {"base":base,"statement":{"unit":"main","path":[0]},"with":{"finish":{"value":{"literal":"null"}}}}});
        assert!(
            serde_json::from_value::<Operation>(raw.clone()).is_err(),
            "{name}"
        );
        assert!(
            Transaction::from_json(&serde_json::json!({"base":base,"edits":[raw]}).to_string())
                .is_err()
        );
    }
    for executable in [
        serde_json::json!({"clock":{}}),
        serde_json::json!({"act": {"kind":"sleep", "args":[]}}),
        serde_json::json!({"closure":{"params":[],"body":[]}}),
        serde_json::json!({"call":{"function":"bad","args":[]}}),
    ] {
        assert!(serde_json::from_value::<DataValue>(executable).is_err());
    }
    let schema = serde_json::to_value(schemars::schema_for!(Operation)).unwrap();
    let kinds: Vec<_> = schema["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|s| s["properties"].as_object().unwrap().keys().cloned())
        .collect();
    assert_eq!(
        kinds,
        [
            "set_value",
            "insert_step",
            "remove_step",
            "move_step",
            "wrap_steps",
            "unwrap_steps",
            "change_condition"
        ]
    );
    let operation = insert(base, Position::end(site(&[])), "sent");
    let mut json = serde_json::to_value(operation).unwrap();
    json["insert_step"]["arbitrary"] = true.into();
    assert!(serde_json::from_value::<Operation>(json).is_err());
}

/// CANVAS-VALUE: only data enters value slots; surviving actions keep order and identity.
#[test]
fn canvas_value() {
    let registry = FunctionRegistry::new();
    let env = env(&registry);
    let mut draft = Draft::open(doc(r#"let input = "old" let sent = perform send(input) as Any then yield let outer = perform send({a: perform send("child") as Any, b: "before"}) as Any finish sent"#)).unwrap();
    let base = draft.identity();
    let applied = draft
        .apply(
            &batch(
                &draft,
                vec![
                    Operation::SetValue {
                        base,
                        target: ValueTarget::Initializer {
                            statement: site(&[0]),
                        },
                        value: value("new"),
                    },
                    Operation::SetValue {
                        base,
                        target: ValueTarget::ToolArgument {
                            action: site(&[1, 0]),
                            index: 0,
                            path: vec![],
                        },
                        value: DataValue::Variable(Name::new("input")),
                    },
                    Operation::SetValue {
                        base,
                        target: ValueTarget::ToolArgument {
                            action: site(&[2, 0]),
                            index: 0,
                            path: vec![1],
                        },
                        value: DataValue::List(vec![value("after")]),
                    },
                    Operation::SetValue {
                        base,
                        target: ValueTarget::Literal {
                            expression: site(&[2, 0, 0, 0, 0]),
                        },
                        value: value("changed child"),
                    },
                ],
            ),
            &env,
        )
        .unwrap();
    for path in [&[1, 0][..], &[2, 0], &[2, 0, 0, 0]] {
        assert_eq!(
            applied.correspondence.successor(&site(path)),
            Some(&site(path))
        );
    }
    for value in [
        DataValue::List(vec![DataValue::Variable(Name::new("input"))]),
        DataValue::Variable(Name::new("input")),
    ] {
        let base = draft.identity();
        let applied = draft
            .apply(
                &batch(
                    &draft,
                    vec![Operation::SetValue {
                        base,
                        target: ValueTarget::ToolArgument {
                            action: site(&[1, 0]),
                            index: 0,
                            path: vec![],
                        },
                        value,
                    }],
                ),
                &env,
            )
            .unwrap();
        assert_eq!(
            applied.correspondence.successor(&site(&[1, 0])),
            Some(&site(&[1, 0]))
        );
        // Argument normalization preserves the action's scheduling semantics.
        match draft.document().node(&site(&[1, 0])).unwrap() {
            Node::Action(lash_kernel_doc::Action::Perform { then_yield, .. }) => {
                assert!(*then_yield)
            }
            Node::Expr(Expr::Act(act)) => assert!(matches!(
                act.kind,
                ActKind::Perform {
                    then_yield: true,
                    ..
                }
            )),
            _ => panic!("the perform survives value edits"),
        }
        assert_eq!(
            parse_document(&print_document(draft.document())).unwrap(),
            *draft.document()
        );
    }
    let current = draft.identity();
    for target in [
        ValueTarget::Initializer {
            statement: site(&[1]),
        },
        ValueTarget::Literal {
            expression: site(&[2, 0]),
        },
        ValueTarget::ToolArgument {
            action: site(&[2, 0]),
            index: 0,
            path: vec![],
        },
    ] {
        let error = rejected(
            &mut draft,
            vec![Operation::SetValue {
                base: current,
                target,
                value: value("no"),
            }],
            &env,
        );
        assert_eq!(error.diagnostics[0].kind, Cause::NotValueTarget);
    }
    let error = rejected(
        &mut draft,
        vec![Operation::SetValue {
            base: current,
            target: ValueTarget::Initializer {
                statement: site(&[0]),
            },
            value: DataValue::Variable(Name::new("missing")),
        }],
        &env,
    );
    assert!(matches!(
        error.diagnostics[0].kind,
        Cause::Refused(RefusalReason::UnboundVariable { .. })
    ));
    let error = rejected(
        &mut draft,
        vec![Operation::SetValue {
            base,
            target: ValueTarget::Initializer {
                statement: site(&[0]),
            },
            value: value("stale"),
        }],
        &env,
    );
    assert!(matches!(error.diagnostics[0].kind, Cause::StaleBase { .. }));
    let error = rejected(
        &mut draft,
        vec![Operation::SetValue {
            base: current,
            target: ValueTarget::Initializer {
                statement: site(&[0]),
            },
            value: DataValue::Literal(Literal::Function(Name::new("code"))),
        }],
        &env,
    );
    assert_eq!(error.diagnostics[0].kind, Cause::ValueOutsideCanvasSet);
}

/// CANVAS-STEP: exact separable effects move as subtrees, without repairing scope or outputs.
#[test]
fn canvas_step() {
    let registry = FunctionRegistry::new();
    let env = env(&registry);
    let mut draft = Draft::open(doc(
        r#"let a = perform send("a") as Any let b = perform send("b") as Any finish a"#,
    ))
    .unwrap();
    let base = draft.identity();
    let transaction = batch(
        &draft,
        vec![
            insert(base, Position::before(site(&[2])).unwrap(), "c"),
            Operation::MoveStep {
                base,
                action: site(&[1, 0]),
                to: Position::before(site(&[0])).unwrap(),
            },
        ],
    );
    let decoded = Transaction::from_json(&transaction.to_json().unwrap()).unwrap();
    assert_eq!(decoded, transaction);
    let applied = draft.apply(&decoded, &env).unwrap();
    assert_eq!(
        applied.correspondence.successor(&site(&[1, 0])),
        Some(&site(&[0, 0]))
    );
    assert_eq!(
        applied.correspondence.successor(&site(&[0, 0])),
        Some(&site(&[1, 0]))
    );
    assert_eq!(applied.correspondence.predecessor(&site(&[2])), None);
    let base = draft.identity();
    let error = rejected(
        &mut draft,
        vec![Operation::RemoveStep {
            base,
            action: site(&[1, 0]),
        }],
        &env,
    );
    assert!(matches!(
        error.diagnostics[0].kind,
        Cause::Refused(RefusalReason::UnboundVariable { .. })
    ));
    draft
        .apply(
            &batch(
                &draft,
                vec![Operation::RemoveStep {
                    base,
                    action: site(&[0, 0]),
                }],
            ),
            &env,
        )
        .unwrap();
    for source in [
        r#"let got = [perform send("nested") as Any] finish got"#,
        r#"let got = perform send(perform send("nested") as Any) as Any finish got"#,
        "let got = sleep 1 finish got",
    ] {
        let mut draft = Draft::open(doc(source)).unwrap();
        let base = draft.identity();
        let error = rejected(
            &mut draft,
            vec![Operation::RemoveStep {
                base,
                action: site(&[0, 0, 0]),
            }],
            &env,
        );
        assert!(matches!(
            error.diagnostics[0].kind,
            Cause::NonSeparableStep | Cause::NoSuchNode
        ));
    }
    let mut receiver_source =
        doc(r#"let target = {} set target.field = perform send("rhs") as Any"#);
    let Stmt::Assign {
        place: lash_kernel_doc::Place::Member(lash_kernel_doc::Member::Field { target, .. }),
        ..
    } = &mut receiver_source.main[1]
    else {
        unreachable!()
    };
    *target = Expr::Act(Box::new(ActExpr {
        kind: ActKind::Perform {
            effect: EffectName::new("send").unwrap(),
            result: Type::Any,
            then_yield: false,
        },
        args: vec![expr("receiver")],
    }));
    let mut receiver = Draft::open(receiver_source).unwrap();
    let base = receiver.identity();
    let error = rejected(
        &mut receiver,
        vec![Operation::RemoveStep {
            base,
            action: site(&[1, 1]),
        }],
        &env,
    );
    assert_eq!(error.diagnostics[0].kind, Cause::NonSeparableStep);
    let mut scoped = Draft::open(doc(
        r#"if true { let x = "inner" let sent = perform send(x) as Any } finish null"#,
    ))
    .unwrap();
    let base = scoped.identity();
    let error = rejected(
        &mut scoped,
        vec![Operation::MoveStep {
            base,
            action: site(&[0, 1, 1, 0]),
            to: Position::end(site(&[])),
        }],
        &env,
    );
    assert!(matches!(
        error.diagnostics[0].kind,
        Cause::Refused(RefusalReason::UnboundVariable { .. })
    ));
    let mut shadowed = Draft::open(doc(r#"let x = "outer" let sent = perform send(x) as Any if true { let x = "inner" } finish null"#)).unwrap();
    let base = shadowed.identity();
    let error = rejected(
        &mut shadowed,
        vec![Operation::MoveStep {
            base,
            action: site(&[1, 0]),
            to: Position::end(site(&[2, 1])),
        }],
        &env,
    );
    assert_eq!(error.diagnostics[0].kind, Cause::EscapingBinding);
    let mut nested = Draft::open(doc(r#"if true { do perform send("x") as Any }"#)).unwrap();
    let base = nested.identity();
    let error = rejected(
        &mut nested,
        vec![Operation::MoveStep {
            base,
            action: site(&[0, 0]),
            to: Position::end(site(&[0, 1])),
        }],
        &env,
    );
    assert_eq!(error.diagnostics[0].kind, Cause::IntoItself);
}

/// CANVAS-WRAP: wrappers preserve children and discard only empty arms.
#[test]
fn canvas_wrap() {
    let registry = FunctionRegistry::new();
    let env = env(&registry);
    for wrapper in [
        Wrapper::If {
            condition: Expr::Literal(Literal::Bool(true)),
        },
        Wrapper::For {
            binding: Name::new("item"),
            iterable: Expr::List(vec![expr("x")]),
        },
        Wrapper::Try {
            catch_binding: Name::new("error"),
        },
    ] {
        let original =
            doc(r#"do perform send("a") as Any do perform send("b") as Any finish null"#);
        let mut draft = Draft::open(original.clone()).unwrap();
        let base = draft.identity();
        let body = if matches!(wrapper, Wrapper::Try { .. }) {
            0
        } else {
            1
        };
        let branch = if matches!(wrapper, Wrapper::If { .. }) {
            Branch::Then
        } else {
            Branch::Body
        };
        let applied = draft
            .apply(
                &batch(
                    &draft,
                    vec![Operation::WrapSteps {
                        base,
                        first: site(&[0, 0]),
                        last: site(&[1, 0]),
                        wrapper,
                    }],
                ),
                &env,
            )
            .unwrap();
        assert_eq!(
            applied.correspondence.successor(&site(&[0, 0])),
            Some(&site(&[0, body, 0, 0]))
        );
        assert_eq!(
            applied.correspondence.successor(&site(&[1, 0])),
            Some(&site(&[0, body, 1, 0]))
        );
        let base = draft.identity();
        draft
            .apply(
                &batch(
                    &draft,
                    vec![Operation::UnwrapSteps {
                        base,
                        wrapper: site(&[0]),
                        branch,
                    }],
                ),
                &env,
            )
            .unwrap();
        assert_eq!(draft.document(), &original);
        assert_eq!(
            draft.correspondence_since_open().successor(&site(&[0, 0])),
            Some(&site(&[0, 0]))
        );
    }
    for (source, branch, cause) in [
        (
            r#"if true { do perform send("keep") as Any } else { print 1 }"#,
            Branch::Then,
            Cause::NonEmptyDiscardedArm,
        ),
        (
            r#"try { do perform send("keep") as Any } catch error { print error }"#,
            Branch::Body,
            Cause::NonEmptyDiscardedArm,
        ),
        (
            r#"try { } finally { print 1 }"#,
            Branch::Body,
            Cause::NonEmptyDiscardedArm,
        ),
        (
            r#"for item in [1] { do perform send(item) as Any }"#,
            Branch::Body,
            Cause::EscapingBinding,
        ),
        (
            "for item in [1] { print 1 } else { print 2 }",
            Branch::Body,
            Cause::NonEmptyDiscardedArm,
        ),
        (
            "for item in [1] { break }",
            Branch::Body,
            Cause::ControlTargetLost,
        ),
        (
            "for item in [1] { if true { continue } }",
            Branch::Body,
            Cause::ControlTargetLost,
        ),
        ("while true {}", Branch::Body, Cause::UnsupportedCanvasEdit),
        (
            r#"for item in perform send(1) as Any {}"#,
            Branch::Body,
            Cause::ActionInHeader,
        ),
    ] {
        let mut draft = Draft::open(doc(source)).unwrap();
        let base = draft.identity();
        let error = rejected(
            &mut draft,
            vec![Operation::UnwrapSteps {
                base,
                wrapper: site(&[0]),
                branch,
            }],
            &env,
        );
        assert_eq!(error.diagnostics[0].kind, cause);
    }
    let mut local = Draft::open(doc(
        "for item in [1] { let item = 2 print item for nested in [1] { break } }",
    ))
    .unwrap();
    let base = local.identity();
    local
        .apply(
            &batch(
                &local,
                vec![Operation::UnwrapSteps {
                    base,
                    wrapper: site(&[0]),
                    branch: Branch::Body,
                }],
            ),
            &env,
        )
        .unwrap();
    for wrapper in [
        Wrapper::If {
            condition: expr("wrong"),
        },
        Wrapper::For {
            binding: Name::new("item"),
            iterable: Expr::Literal(Literal::Bool(true)),
        },
    ] {
        let mut draft = Draft::open(doc(r#"do perform send("a") as Any"#)).unwrap();
        let base = draft.identity();
        let error = rejected(
            &mut draft,
            vec![Operation::WrapSteps {
                base,
                first: site(&[0, 0]),
                last: site(&[0, 0]),
                wrapper,
            }],
            &env,
        );
        assert!(matches!(
            error.diagnostics[0].kind,
            Cause::Refused(RefusalReason::ArgumentType { .. })
        ));
    }
    let mut draft = Draft::open(doc(
        r#"do perform send("a") as Any print 1 do perform send("b") as Any"#,
    ))
    .unwrap();
    let base = draft.identity();
    let error = rejected(
        &mut draft,
        vec![Operation::WrapSteps {
            base,
            first: site(&[0, 0]),
            last: site(&[2, 0]),
            wrapper: Wrapper::Try {
                catch_binding: Name::new("error"),
            },
        }],
        &env,
    );
    assert_eq!(error.diagnostics[0].kind, Cause::NotContiguousSteps);
    let mut draft = Draft::open(doc(
        r#"do perform send("a") as Any if true { do perform send("b") as Any }"#,
    ))
    .unwrap();
    let base = draft.identity();
    let error = rejected(
        &mut draft,
        vec![Operation::WrapSteps {
            base,
            first: site(&[0, 0]),
            last: site(&[1, 1, 0, 0]),
            wrapper: Wrapper::Try {
                catch_binding: Name::new("error"),
            },
        }],
        &env,
    );
    assert_eq!(error.diagnostics[0].kind, Cause::NotContiguousSteps);
}

/// CANVAS-CONDITION: if/while headers cannot add or erase action sites.
#[test]
fn canvas_condition() {
    let registry = FunctionRegistry::new();
    let env = env(&registry);
    let mut draft =
        Draft::open(doc("let flag = true if flag {} while false {} finish null")).unwrap();
    let base = draft.identity();
    draft
        .apply(
            &batch(
                &draft,
                vec![
                    Operation::ChangeCondition {
                        base,
                        statement: site(&[1]),
                        condition: Expr::Literal(Literal::Bool(false)),
                    },
                    Operation::ChangeCondition {
                        base,
                        statement: site(&[2]),
                        condition: Expr::Variable(Name::new("flag")),
                    },
                ],
            ),
            &env,
        )
        .unwrap();
    let base = draft.identity();
    let error = rejected(
        &mut draft,
        vec![Operation::ChangeCondition {
            base,
            statement: site(&[1]),
            condition: Expr::Variable(Name::new("missing")),
        }],
        &env,
    );
    assert!(matches!(
        error.diagnostics[0].kind,
        Cause::Refused(RefusalReason::UnboundVariable { .. })
    ));
    let error = rejected(
        &mut draft,
        vec![Operation::ChangeCondition {
            base,
            statement: site(&[1]),
            condition: expr("wrong"),
        }],
        &env,
    );
    assert!(matches!(
        error.diagnostics[0].kind,
        Cause::Refused(RefusalReason::ArgumentType { .. })
    ));
    // A literal slot or its binding cannot bypass condition admission.
    for expression in [site(&[1, 0]), site(&[0, 0])] {
        let error = rejected(
            &mut draft,
            vec![Operation::SetValue {
                base,
                target: ValueTarget::Literal { expression },
                value: value("not a boolean"),
            }],
            &env,
        );
        assert!(matches!(
            error.diagnostics[0].kind,
            Cause::Refused(RefusalReason::ArgumentType { .. })
        ));
    }
    let mut action_header = Draft::open(doc(r#"if perform send("test") as Bool {}"#)).unwrap();
    let base = action_header.identity();
    let error = rejected(
        &mut action_header,
        vec![Operation::ChangeCondition {
            base,
            statement: site(&[0]),
            condition: Expr::Literal(Literal::Bool(true)),
        }],
        &env,
    );
    assert_eq!(error.diagnostics[0].kind, Cause::ActionInHeader);
    let condition = Expr::Act(Box::new(ActExpr {
        kind: ActKind::Perform {
            effect: EffectName::new("send").unwrap(),
            result: Type::Bool,
            then_yield: false,
        },
        args: vec![expr("test")],
    }));
    let base = draft.identity();
    let error = rejected(
        &mut draft,
        vec![Operation::ChangeCondition {
            base,
            statement: site(&[1]),
            condition,
        }],
        &env,
    );
    assert_eq!(error.diagnostics[0].kind, Cause::ActionInHeader);
}

#[derive(Default)]
struct Printer {
    printed: Vec<Datum>,
}

impl Host for Printer {
    fn clock(&mut self) -> Timestamp {
        Timestamp {
            nanoseconds: Integer::from(0),
        }
    }

    fn random(&mut self) -> u64 {
        0
    }

    fn read(&mut self, _: &Handle, request: &Datum) -> Result<Datum, ErrorDatum> {
        Ok(request.clone())
    }

    fn print(&mut self, value: &Datum) {
        self.printed.push(value.clone());
    }

    fn cancel_requested(&mut self) -> bool {
        false
    }
}

/// The pages a law's run seals, kept in memory.
#[derive(Default)]
struct Pages(BTreeMap<PageId, PageAnswer>);

impl PageSource for Pages {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
        Ok(pages
            .iter()
            .map(|page| {
                self.0
                    .get(&page.id)
                    .cloned()
                    .ok_or(HeapFault::Missing { page: page.id })
            })
            .collect())
    }

    fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
        let mut tokens = body.kind.0.to_be_bytes().to_vec();
        tokens.extend_from_slice(&body.tokens);
        let page = PageRef {
            id: PageId(lash_kernel_doc::heap_values::key_digest(&tokens)),
            grant: PageGrant::NONE,
        };
        self.0.insert(
            page.id,
            PageAnswer {
                page,
                kind: body.kind,
                tokens: body.tokens.into(),
                children: body.children,
            },
        );
        Ok(page)
    }
}

/// CANVAS-ATOMIC: refusals leave the draft intact; editing never retargets a parked run.
#[test]
fn canvas_atomic() {
    let registry = FunctionRegistry::new();
    let environment = env(&registry);
    let original = doc(r#"let sent = perform send("wait") as Any finish "old""#);
    let mut draft = Draft::open(original.clone()).unwrap();
    let base = draft.identity();
    let error = rejected(
        &mut draft,
        vec![
            insert(base, Position::before(site(&[1])).unwrap(), "added"),
            Operation::RemoveStep {
                base,
                action: site(&[88]),
            },
        ],
        &environment,
    );
    assert_eq!(error.diagnostics[0].edit, Some(1));
    let bounds = Bounds {
        memory_limit: lash_kernel_vm::MemoryLimit::Recount,
        charge: lash_kernel_vm::Charge::Exact,
        charge_limit: 1_000_000,
        memory: 1 << 20,
        call_depth: 16,
        live_tasks: 4,
        requests_per_park: 4,
        join_members: 4,
    };
    let program = Program {
        release: PageId([8; 32]),
        document: Arc::new(original.clone()),
        library: lash_kernel_vm::PreparedLibrary::new(Arc::new(registry)),
    };
    let mut machine = KernelMachine::start(
        program,
        bounds,
        CellStart {
            owner: PageId([9; 32]),
            target: Target::Main,
            args: vec![],
            session: None,
            modes: Default::default(),
            host: vec![],
        },
        Box::new(Pages::default()),
    )
    .unwrap();
    let mut host = Printer::default();
    let Step::Parked(park) = machine.run(&mut host, u64::MAX).unwrap() else {
        panic!("the effect parks")
    };
    let registry = FunctionRegistry::new();
    let environment = env(&registry);
    draft
        .apply(
            &batch(
                &draft,
                vec![Operation::SetValue {
                    base,
                    target: ValueTarget::Literal {
                        expression: site(&[1, 0]),
                    },
                    value: value("new"),
                }],
            ),
            &environment,
        )
        .unwrap();
    assert_ne!(draft.identity(), base);
    for request in park.requests {
        let Request::Effect(request) = request else {
            panic!("the effect waits")
        };
        machine
            .deliver(request.wait, Outcome::Completed(Datum::Null))
            .unwrap();
    }
    let Step::Ended(End::Finished(finished)) = machine.run(&mut host, u64::MAX).unwrap() else {
        panic!("the pinned run finishes")
    };
    assert_eq!(finished.result, Datum::Text("old".into()));
}

/// EXTRACT-REACHABLE: the whole-cell entry slice retains its reachable code,
/// drops unrelated main/functions/private bindings and reports exactly its effects.
#[test]
fn extract_reachable() {
    let source = parse_document(
        r#"kernel 1
numbers float
effect send(value: Any) -> Any
effect unused(value: Any) -> Any
entry hello(to: Any) -> Any
entry other(to: Any) -> Any
private secret
fn hello(to) {
  let callback = fn() { return &indirect }
  let sent = call helper(to)
  let task = spawn call helper(to)
  do cancel task
  return callback
}
fn helper(to) { let sent = perform send(to) as Any return sent }
fn indirect() { return &helper }
fn other(to) { let ignored = perform unused(to) as Any return ignored }
main { let secret = "cell" let made = perform unused(&hello) as Any finish made }
"#,
    )
    .unwrap();
    let registry = FunctionRegistry::new();
    let environment = env(&registry);
    let extraction = crate::extract_entry(&source, &Name::new("hello"), &environment).unwrap();
    assert!(extraction.document.main.is_empty());
    assert!(extraction.document.private_bindings.is_empty());
    assert_eq!(
        extraction
            .document
            .entries
            .keys()
            .map(Name::as_str)
            .collect::<Vec<_>>(),
        ["hello"]
    );
    assert_eq!(
        extraction
            .document
            .functions
            .keys()
            .map(Name::as_str)
            .collect::<Vec<_>>(),
        ["hello", "helper", "indirect"]
    );
    assert_eq!(
        extraction.requirements,
        BTreeSet::from([EffectName::new("send").unwrap()])
    );
    assert_eq!(
        extraction
            .document
            .manifest
            .effects
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>(),
        extraction.requirements
    );
    let applied = extraction.applied.as_ref().unwrap();
    assert_eq!(applied.correspondence.successor(&site(&[0])), None);
    let hello = Site::new(Unit::Function(Name::new("hello")), [0]);
    assert_eq!(applied.correspondence.successor(&hello), Some(&hello));
    assert_eq!(source.main.len(), 3);
    let json = serde_json::to_value(&extraction).unwrap();
    assert!(json.get("applied").is_none());
    let decoded: crate::Extraction = serde_json::from_value(json).unwrap();
    assert_eq!(decoded.document, extraction.document);
    assert_eq!(decoded.requirements, extraction.requirements);
    assert!(decoded.applied.is_none());
    for (entry, environment) in [
        ("missing", env(&registry)),
        ("hello", Environment::new(&registry)),
    ] {
        let error = crate::extract_entry(&source, &Name::new(entry), &environment).unwrap_err();
        assert_eq!(
            serde_json::from_str::<EditRefusal>(&serde_json::to_string(&error).unwrap()).unwrap(),
            error
        );
        if entry == "missing" {
            assert!(matches!(
                error.diagnostics[0].kind,
                Cause::NoSuchEntry { .. }
            ));
        } else {
            assert!(matches!(
                error.diagnostics[0].kind,
                Cause::Refused(RefusalReason::MissingEffect { .. })
            ));
        }
    }
    let mut broken = source.clone();
    broken.functions.get_mut(&Name::new("helper")).unwrap().body = vec![Stmt::Return {
        value: Expr::Literal(Literal::Function(Name::new("absent"))),
    }];
    let error = crate::extract_entry(&broken, &Name::new("hello"), &environment).unwrap_err();
    assert!(matches!(
        error.diagnostics[0].kind,
        Cause::Refused(RefusalReason::Invalid(
            InvalidReason::FunctionNotDeclared { .. }
        ))
    ));
    assert_eq!(
        serde_json::from_str::<EditRefusal>(&serde_json::to_string(&error).unwrap()).unwrap(),
        error
    );
}

/// The refusal wire is an owned typed sum, including checker causes.
#[test]
fn canvas_refusal_wire() {
    let base = doc("").identity();
    let causes = vec![
        Cause::UnsupportedCanvasEdit,
        Cause::NotValueTarget,
        Cause::ValueOutsideCanvasSet,
        Cause::NonSeparableStep,
        Cause::NotContiguousSteps,
        Cause::ActionInHeader,
        Cause::NonEmptyDiscardedArm,
        Cause::EscapingBinding,
        Cause::ControlTargetLost,
        Cause::StaleBase {
            base,
            current: base,
        },
        Cause::NoSuchNode,
        Cause::WrongNode {
            expected: crate::NodeClass::Action,
            found: crate::NodeClass::Expression,
        },
        Cause::NotInBlock,
        Cause::IntoItself,
        Cause::NoCondition,
        Cause::NoSuchArgument {
            index: 3,
            arguments: 1,
        },
        Cause::NoSuchFunction {
            name: Name::new("missing"),
        },
        Cause::NoSuchEntry {
            name: Name::new("missing"),
        },
        Cause::InvalidCorrespondence,
        Cause::Invalid(InvalidReason::LoopControlOutsideLoop {
            keyword: "break".into(),
        }),
        Cause::Refused(RefusalReason::UnboundVariable {
            name: Name::new("missing"),
        }),
    ];
    let registry = FunctionRegistry::new();
    let signature = env(&registry).effects.values().next().unwrap().clone();
    let function = lash_kernel_doc::parse_definition(
        "function sample.fn(value: Any) -> Any\nkernel 1\ncharge 1\nbody { return value }",
    )
    .unwrap()
    .identity();
    let name = lash_kernel_doc::FunctionName::new("sample.fn").unwrap();
    let effect = EffectName::new("send").unwrap();
    let checker_causes = vec![
        RefusalReason::Invalid(InvalidReason::InvalidType {
            problem: "invalid type".into(),
        }),
        RefusalReason::MissingEffect {
            effect: effect.clone(),
        },
        RefusalReason::EffectSignature {
            effect: effect.clone(),
            expected: signature.clone(),
            provided: signature,
            mismatch: Box::new(lash_kernel_check::SignatureMismatch {
                side: lash_kernel_check::SignatureSide::Output,
                path: "/output".into(),
                expected: Type::Text,
                provided: Type::Int,
            }),
        },
        RefusalReason::MissingFunction {
            function,
            name: name.clone(),
        },
        RefusalReason::UnboundVariable {
            name: Name::new("missing"),
        },
        RefusalReason::ArgumentType {
            callee: "send".into(),
            param: Name::new("value"),
            expected: Type::Text,
            found: Type::Int,
        },
        RefusalReason::PerformResult {
            effect: effect.clone(),
            declared: Type::Int,
            stated: Type::Text,
        },
        RefusalReason::EffectNotPerformed { effect },
        RefusalReason::FunctionNotReached {
            function,
            name: name.clone(),
        },
        RefusalReason::FunctionNotListed {
            function,
            name: name.clone(),
            through: function,
        },
        RefusalReason::FunctionName {
            function,
            listed: name.clone(),
            defined: name,
        },
    ];
    let causes = causes
        .into_iter()
        .chain(checker_causes.into_iter().map(Cause::Refused))
        .chain([Cause::Invalid(InvalidReason::PlaceFirstAssignment {
            problem: "writes a variable, so nothing of its place is evaluated".into(),
        })]);
    for kind in causes {
        let error = EditRefusal {
            diagnostics: vec![crate::EditDiagnostic {
                edit: Some(0),
                location: Some(crate::Location::Base(site(&[0]))),
                kind,
            }],
        };
        assert_eq!(
            serde_json::from_str::<EditRefusal>(&serde_json::to_string(&error).unwrap()).unwrap(),
            error
        );
    }
}

/// Every site of `document`, read from the document alone.
fn sites(document: &Document) -> Vec<Site> {
    fn walk(node: Node<'_>, site: Site, out: &mut Vec<Site>) {
        for (index, child) in (0u32..).zip(node.children()) {
            walk(child, site.child(index), out);
        }
        out.push(site);
    }
    let mut out = Vec::new();
    walk(Node::Block(&document.main), site(&[]), &mut out);
    for (name, function) in &document.functions {
        let site = Site::new(Unit::Function(name.clone()), []);
        walk(Node::Block(&function.body), site, &mut out);
    }
    out.sort();
    out
}

/// `K-ID-004`: a site addresses the same node for writing as it does for
/// reading, for every form.
#[test]
fn a_site_names_the_same_node_for_writing_as_for_reading() {
    let source = r#"kernel 1
numbers float
effect fetch(url: Text) -> Any

fn start(count) {
  let page = perform fetch("u") as Any
  do sleep 10
  do yield
  return page
}

main {
  let xs = [1, 2.5]
  let t = (xs, "a", b"00", null, absent, true)
  let m = map{"k": 1}
  let s = set{1, 2}
  let r = {name: "n"}
  set r.name = m["k"]
  set xs[0] = 1
  remove m["k"]
  remove r.name
  if true {
    let inner = 1
  } else {
    while false {
      break
    }
  }
  for x in xs {
    if s[x] {
      continue
    }
  }
  try {
    throw r
  } catch e {
    print e
  } finally {
    print "done"
  }
  let f = fn(a) {
    return a
  }
  let roll = random
  let now = clock
  let got = read(t, {path: ["a", 0]})
  let h = spawn call start(1)
  let hs = [h]
  let first = join h
  let every = join all hs
  do cancel h
  let applied = apply f(&start)
  if roll {
    fail "no"
  }
  finish got
}
"#;
    let mut document = parse_document(source).unwrap();
    let reading = document.clone();
    let all = sites(&reading);
    assert_eq!(Working::open(&reading).sites(), all);
    for site in all {
        let body = match &site.unit {
            Unit::Main => &mut document.main,
            Unit::Function(name) => &mut document.functions.get_mut(name).unwrap().body,
            Unit::Library(_) => unreachable!(),
        };
        let written = NodeMut::Block(body).descend(&site.path).unwrap();
        assert_eq!(
            format!("{:?}", written.as_node()),
            format!("{:?}", reading.node(&site).unwrap()),
            "{site}"
        );
    }
}

/// FIG-6471 / K-EDIT-004: every discarded action accepts expression arguments.
#[test]
fn set_argument_on_do_keeps_computed_values_in_the_expression() {
    let registry = FunctionRegistry::new();
    let environment = env(&registry);
    for (original, computed) in [
        (
            "perform send(null) as Any",
            "perform send({text: \"x\"}) as Any",
        ),
        ("join h", "join(let(task = h, task))"),
        ("join all hs", "join all(let(tasks = hs, tasks))"),
        ("cancel h", "cancel(let(task = h, task))"),
    ] {
        // The declared callee spelling also keeps the setup independent of edits.
        let source = format!(
            "kernel 1\nnumbers float\neffect send(value: Any) -> Any\nfn child() {{ return null }}\nmain {{ let h = spawn call child() let hs = [h] do {original} finish null }}"
        );
        let document = parse_document(&source).unwrap();
        let mut working = Working::open(&document);
        let expected =
            parse_document(&source.replace(&format!("do {original}"), &format!("do {computed}")))
                .unwrap();
        let Stmt::Do {
            value: Expr::Act(act),
        } = &expected.main[2]
        else {
            panic!("do action")
        };
        assert!(
            working
                .edit(&crate::Edit::SetArgument {
                    action: site(&[2, 0]),
                    index: 0,
                    argument: act.args[0].clone(),
                })
                .is_ok(),
            "computed {original}"
        );
        let published = working.publish(document.identity(), &environment).unwrap();
        let mut expected = expected;
        expected.manifest.effects = published.document.manifest.effects.clone();
        assert_eq!(published.document, expected);
        assert_eq!(
            parse_document(&print_document(&published.document)).unwrap(),
            expected
        );
    }
}
