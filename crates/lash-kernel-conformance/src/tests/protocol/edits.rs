//! K-EDIT: transactions checked against small kernel documents.

use std::collections::BTreeSet;
use std::sync::Arc;

use lash_kernel_check::{Environment, RefusalReason};
use lash_kernel_doc::{
    Action, Annotations, Expr, Label, Literal, Name, NodeAnnotation, Stmt, parse_definition,
    parse_document,
};
use lash_kernel_edit::{
    Applied, Correspondence, Draft, Edit, EditDiagnostic, EditDiagnosticKind, Location, Position,
    Transaction,
};

use super::{Case, main, registry};

fn apply(draft: &mut Draft, edits: Vec<Edit>, env: &Environment<'_>) -> Applied {
    draft
        .apply(
            &Transaction {
                base: draft.identity(),
                edits,
            },
            env,
        )
        .expect("publish transaction")
}

fn remove(index: u32) -> Edit {
    Edit::RemoveStatement {
        statement: main(&[index]),
    }
}
fn print(n: i64) -> Stmt {
    parse_document(&format!(
        "kernel 1\nnumbers by_spelling\nmain {{ print {n} }}"
    ))
    .expect("print payload")
    .main
    .remove(0)
}

pub(super) fn check(rule: &str, case: &Case) {
    let registry = registry();
    let mut env = Environment::new(registry.as_ref());
    let document = parse_document(&case.document).expect("base document");
    env.effects = document.manifest.effects.clone();
    let mut draft = Draft::open(document.clone(), None).expect("open draft");
    match rule {
        "K-EDIT-001" => {
            let original = draft.clone();
            let refusal = draft
                .apply(
                    &Transaction {
                        base: draft.identity(),
                        edits: vec![remove(0), remove(0)],
                    },
                    &env,
                )
                .expect_err("second removal refuses the whole transaction");
            assert_eq!(
                refusal.diagnostics,
                vec![EditDiagnostic {
                    edit: Some(1),
                    location: Some(Location::Base(main(&[0]))),
                    kind: EditDiagnosticKind::NoSuchNode,
                }]
            );
            assert_eq!(draft.document(), original.document());
            assert_eq!(draft.annotations(), original.annotations());
            assert_eq!(
                draft.correspondence_since_open(),
                original.correspondence_since_open()
            );
            apply(&mut draft, vec![remove(0)], &env);
            assert_eq!(draft.document().main, document.main[1..]);
        }
        "K-EDIT-002" => {
            let base = draft.identity();
            apply(
                &mut draft,
                vec![
                    remove(0),
                    Edit::SetCondition {
                        statement: main(&[1]),
                        condition: Expr::Literal(Literal::Bool(false)),
                    },
                ],
                &env,
            );
            let expected = parse_document(
                "kernel 1\nnumbers by_spelling\nmain { if false { print 2 } finish null }",
            )
            .unwrap();
            assert_eq!(draft.document(), &expected);
            let held = draft.document().clone();
            let refusal = draft
                .apply(
                    &Transaction {
                        base,
                        edits: vec![remove(0)],
                    },
                    &env,
                )
                .unwrap_err();
            assert!(matches!(
                refusal.diagnostics[0].kind,
                EditDiagnosticKind::StaleBase { .. }
            ));
            let refusal = draft
                .apply(
                    &Transaction {
                        base: draft.identity(),
                        edits: vec![Edit::MoveStatement {
                            statement: main(&[0]),
                            to: Position::end(main(&[0, 1])),
                        }],
                    },
                    &env,
                )
                .unwrap_err();
            assert_eq!(refusal.diagnostics[0].kind, EditDiagnosticKind::IntoItself);
            assert_eq!(draft.document(), &held);
        }
        "K-EDIT-003" => {
            let refusal = draft
                .apply(
                    &Transaction {
                        base: draft.identity(),
                        edits: vec![remove(0)],
                    },
                    &env,
                )
                .unwrap_err();
            assert_eq!(
                refusal.diagnostics,
                vec![EditDiagnostic {
                    edit: None,
                    location: Some(Location::Edited(main(&[0, 0]))),
                    kind: EditDiagnosticKind::Refused(RefusalReason::UnboundVariable {
                        name: Name::new("tmp")
                    }),
                }]
            );
            assert_eq!(draft.document(), &document);
        }
        "K-EDIT-004" => {
            let first = apply(
                &mut draft,
                vec![
                    Edit::MoveStatement {
                        statement: main(&[0]),
                        to: Position::end(main(&[])),
                    },
                    Edit::CloneStatement {
                        statement: main(&[1]),
                        to: Position::end(main(&[])),
                    },
                    Edit::ReplaceStatement {
                        statement: main(&[1]),
                        with: print(3),
                    },
                ],
                &env,
            )
            .correspondence;
            assert_eq!(first.successor(&main(&[0])), Some(&main(&[2])));
            assert_eq!(first.successor(&main(&[0, 0, 0])), Some(&main(&[2, 0, 0])));
            assert!(!first.survivor(&main(&[0])).unwrap().edited);
            assert!(first.survivor(&main(&[1])).unwrap().edited);
            assert_eq!(first.successor(&main(&[1, 0])), None);
            assert_eq!(first.predecessor(&main(&[3])), None);
            assert_eq!(first.predecessor(&main(&[3, 0])), None);
            let second = apply(&mut draft, vec![remove(2)], &env).correspondence;
            assert_eq!(
                draft.correspondence_since_open(),
                &first.then(&second).unwrap()
            );
            assert_eq!(
                draft.correspondence_since_open().successor(&main(&[0])),
                None
            );
            let entries = first.entries();
            assert_eq!(
                entries.iter().map(|e| &e.to).collect::<BTreeSet<_>>().len(),
                entries.len()
            );
        }
        "K-EDIT-005" => {
            env.effects.insert(
                lash_kernel_doc::EffectName::new("new").unwrap(),
                document.manifest.effects.values().next().unwrap().clone(),
            );
            apply(
                &mut draft,
                vec![Edit::ReplaceAction {
                    action: main(&[0, 0]),
                    with: Action::Perform {
                        effect: lash_kernel_doc::EffectName::new("new").unwrap(),
                        args: vec![],
                        result: lash_kernel_doc::Type::Any,
                    },
                }],
                &env,
            );
            assert_eq!(
                draft
                    .document()
                    .manifest
                    .effects
                    .keys()
                    .map(|n| n.as_str())
                    .collect::<Vec<_>>(),
                ["new"]
            );
            assert_eq!(draft.document().manifest.kernel, document.manifest.kernel);
            apply(&mut draft, vec![remove(0)], &env);
            assert!(draft.document().manifest.effects.is_empty());
        }
        "K-EDIT-006" => {
            let label = Label {
                title: "first".into(),
                description: None,
            };
            let mut annotations = Annotations::new(draft.identity());
            annotations.dialect = Some("typescript".into());
            annotations.source = Some("authored source".into());
            annotations.nodes = vec![NodeAnnotation {
                site: main(&[0]),
                label: Some(label.clone()),
                data: Default::default(),
            }];
            draft = Draft::open(document.clone(), Some(annotations)).unwrap();
            let first = apply(
                &mut draft,
                vec![Edit::SetLabel {
                    node: main(&[1]),
                    label: Some(label.clone()),
                }],
                &env,
            );
            assert_eq!(first.correspondence.base, first.correspondence.result);
            assert_eq!(
                draft.annotations().source.as_deref(),
                Some("authored source")
            );
            apply(
                &mut draft,
                vec![
                    Edit::CloneStatement {
                        statement: main(&[0]),
                        to: Position::end(main(&[])),
                    },
                    Edit::MoveStatement {
                        statement: main(&[0]),
                        to: Position::end(main(&[])),
                    },
                    remove(1),
                ],
                &env,
            );
            assert_eq!(
                draft
                    .annotations()
                    .nodes
                    .iter()
                    .map(|n| (&n.site, &n.label))
                    .collect::<Vec<_>>(),
                vec![
                    (&main(&[1]), &Some(label.clone())),
                    (&main(&[2]), &Some(label))
                ]
            );
            assert_eq!(draft.annotations().document, draft.identity());
            assert_eq!(draft.annotations().dialect.as_deref(), Some("typescript"));
            assert_eq!(draft.annotations().source, None);
        }
        "K-EDIT-007" => {
            apply(
                &mut draft,
                vec![Edit::RenameVariable {
                    declared_at: main(&[0]),
                    name: Name::new("x"),
                    to: Name::new("sum"),
                }],
                &env,
            );
            let expected = parse_document("kernel 1\nnumbers by_spelling\nprivate sum\nmain { let sum = 1 if true { let x = 2 print x } print sum finish null }").unwrap();
            assert_eq!(draft.document(), &expected);
            let mut capture = Draft::open(parse_document("kernel 1\nnumbers by_spelling\nmain { let x = 1 if true { let y = 2 print x } }").unwrap(), None).unwrap();
            let refusal = capture
                .apply(
                    &Transaction {
                        base: capture.identity(),
                        edits: vec![Edit::RenameVariable {
                            declared_at: main(&[0]),
                            name: Name::new("x"),
                            to: Name::new("y"),
                        }],
                    },
                    &env,
                )
                .unwrap_err();
            assert!(matches!(
                refusal.diagnostics[0].kind,
                EditDiagnosticKind::RenameCollides { .. }
            ));
        }
        "K-EDIT-008" => {
            apply(
                &mut draft,
                vec![Edit::RenameEntry {
                    from: Name::new("f"),
                    to: Name::new("g"),
                }],
                &env,
            );
            let expected = parse_document("kernel 1\nnumbers by_spelling\nfn g() { return 3 }\nentry g() -> Int\nmain { let r = call g() let h = spawn call g() let j = join h finish &g }").unwrap();
            assert_eq!(draft.document(), &expected);
            apply(
                &mut draft,
                vec![Edit::RemoveEntry {
                    function: Name::new("g"),
                }],
                &env,
            );
            assert!(draft.document().entries.is_empty());
            assert!(draft.document().functions.contains_key(&Name::new("g")));
            let signature = expected.entries[&Name::new("g")].clone();
            apply(
                &mut draft,
                vec![Edit::InsertEntry {
                    function: Name::new("g"),
                    signature,
                }],
                &env,
            );
            let mut empty = Draft::open(parse_document("kernel 1\nnumbers by_spelling\nfn f() { return 3 }\nentry f() -> Int\nmain { finish null }").unwrap(), None).unwrap();
            apply(
                &mut empty,
                vec![Edit::RemoveFunction {
                    name: Name::new("f"),
                }],
                &env,
            );
            assert!(empty.document().entries.is_empty());
            assert!(empty.document().functions.is_empty());
        }
        "K-EDIT-009" => {
            let mut library = (*registry).clone();
            let definition = parse_definition(
                "function corrected(x: Any) -> Any\nkernel 1\ncharge 1\nbody { return x }",
            )
            .unwrap();
            let old = library.register(definition.clone(), None).unwrap();
            let mut corrected = definition.clone();
            corrected.charge = lash_kernel_doc::Formula::Constant(2);
            let new = library.register(corrected, None).unwrap();
            let absent = {
                let mut d = definition;
                d.charge = lash_kernel_doc::Formula::Constant(3);
                d.identity().unwrap()
            };
            let env = Environment::new(&library);
            let written = parse_document(&case.document).unwrap();
            let mut draft = Draft::open(written, None).unwrap();
            let held = draft.document().clone();
            let refusal = draft
                .apply(
                    &Transaction {
                        base: draft.identity(),
                        edits: vec![Edit::ReplaceFunctionIdentity {
                            from: old,
                            to: absent,
                        }],
                    },
                    &env,
                )
                .unwrap_err();
            assert!(
                matches!(refusal.diagnostics[0].kind, EditDiagnosticKind::Refused(RefusalReason::MissingFunction { function, .. }) if function == absent)
            );
            assert_eq!(draft.document(), &held);
            apply(
                &mut draft,
                vec![Edit::ReplaceFunctionIdentity { from: old, to: new }],
                &env,
            );
            assert_eq!(
                draft
                    .document()
                    .manifest
                    .functions
                    .keys()
                    .copied()
                    .collect::<Vec<_>>(),
                [new]
            );
            let expected = parse_document(&format!("kernel 1\nnumbers by_spelling\nuse corrected = @{new}\nfn f() {{ let y = invoke corrected(2) return y }}\nmain {{ let x = invoke corrected(1) finish x }}")).unwrap();
            assert_eq!(draft.document(), &expected);
            let mut runner = crate::MachineRunner::<lash_kernel_vm::KernelMachine>::new(Arc::new(
                library.clone(),
            ));
            crate::check_case(&mut runner, case).unwrap();
            let refusal = draft
                .apply(
                    &Transaction {
                        base: draft.identity(),
                        edits: vec![Edit::ReplaceFunctionIdentity { from: old, to: new }],
                    },
                    &env,
                )
                .unwrap_err();
            assert_eq!(
                refusal.diagnostics[0].kind,
                EditDiagnosticKind::FunctionNotCalled { function: old }
            );
        }
        "K-EDIT-010" => {
            let transaction = Transaction {
                base: draft.identity(),
                edits: vec![Edit::SetFinally {
                    statement: main(&[0]),
                    finally: None,
                }],
            };
            let json = transaction.to_json().unwrap();
            assert_eq!(Transaction::from_json(&json).unwrap(), transaction);
            assert!(!json.contains("finally\":null"));
            assert_eq!(
                serde_json::to_value(&transaction.edits[0]).unwrap(),
                serde_json::json!({"set_finally":{"statement":{"unit":"main","path":[0]}}})
            );
            let mut unknown = serde_json::to_value(&transaction).unwrap();
            unknown["force"] = true.into();
            assert!(Transaction::from_json(&unknown.to_string()).is_err());
            let applied = draft.apply(&transaction, &env).unwrap();
            let json = serde_json::to_value(&applied.correspondence).unwrap();
            assert_eq!(
                serde_json::from_value::<Correspondence>(json.clone()).unwrap(),
                applied.correspondence
            );
            let mut unknown = json;
            unknown["force"] = true.into();
            assert!(serde_json::from_value::<Correspondence>(unknown).is_err());
            assert!(serde_json::from_value::<Edit>(serde_json::json!({"set_finally":{"statement":{"unit":"main","path":[0]},"force":true}})).is_err());
        }
        _ => panic!("no edit driver for {rule}"),
    }
    if rule == "K-EDIT-009" {
        return;
    }
    let mut runner =
        crate::MachineRunner::<lash_kernel_vm::KernelMachine>::new(Arc::clone(&registry));
    crate::check_case(&mut runner, case).expect("base document observations");
}
