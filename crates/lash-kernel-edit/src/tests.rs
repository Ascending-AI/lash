//! FIG-7154: integration laws at the Document/Export editor boundary.
use super::*;
use crate::authoring::{Branch, DataValue, Operation, ValueTarget, Wrapper};
use lash_kernel_check::{Environment, NodeKind};
use lash_kernel_doc::{
    ActExpr, ActKind, Callee, Document, Export, Expr, FunctionDefinition, FunctionExport, Literal,
    Node, NumberPolicy, Rhs, StartConvention, Stmt, parse_definition,
};
use lash_kernel_package::{LinkedContent, Namespace};
use std::collections::{BTreeMap, BTreeSet};
use {lash_kernel_value::names::*, lash_kernel_value::package::*, lash_kernel_value::types::*};

fn site(path: &[u32]) -> Site {
    Site::new(Unit::Main, path)
}
use {lash_kernel_value::sites::Site, lash_kernel_value::sites::Unit};

fn text(value: &str) -> Expr {
    Expr::Literal(Literal::Text(value.into()))
}
fn data(value: &str) -> DataValue {
    DataValue::Literal(Literal::Text(value.into()))
}
fn boolean(value: bool) -> Expr {
    Expr::Literal(Literal::Bool(value))
}
fn perform(slot: u32, args: Vec<Expr>) -> Expr {
    Expr::Act(Box::new(ActExpr {
        kind: ActKind::Perform {
            slot: ImportSlot(slot),
            result: Type::Text,
            then_yield: false,
        },
        args,
    }))
}
fn step(value: &str) -> Stmt {
    Stmt::Do {
        value: perform(0, vec![text(value)]),
    }
}
fn document(body: Vec<Stmt>) -> Document {
    let mut document = Document::new(NumberPolicy::Float, body);
    let definition = parse_definition("function law.read(value: Any) -> Text\nkernel 1\ncharge 1\nimport 0 operation service.read(value: Any) -> Text\nbody { let answer = perform 0(value) as Text return answer }").unwrap();
    document.imports = definition.body().unwrap().imports.clone();
    document
}
fn apply(draft: &mut Draft, edits: Vec<Operation>) -> Applied {
    let functions = draft.document().functions.clone();
    draft
        .apply(
            &Transaction {
                base: draft.identity(),
                edits,
            },
            &Environment::new(&functions),
        )
        .unwrap()
}
fn refuse(draft: &mut Draft, edits: Vec<Operation>) -> EditRefusal {
    let before = draft.document().clone();
    let identity = draft.identity();
    let correspondence = draft.correspondence_since_open().clone();
    let functions = before.functions.clone();
    let refusal = draft
        .apply(
            &Transaction {
                base: identity,
                edits,
            },
            &Environment::new(&functions),
        )
        .unwrap_err();
    assert_eq!(draft.document(), &before, "CANVAS-ATOMIC: refused document");
    assert_eq!(
        draft.identity(),
        identity,
        "CANVAS-ATOMIC: refused identity"
    );
    assert_eq!(draft.correspondence_since_open(), &correspondence);
    refusal
}
fn export(document: &mut Document, name: &str, definition: FunctionDefinition) -> FunctionId {
    let id = definition.identity();
    document
        .imports
        .extend(definition.body().unwrap().imports.clone());
    document.exports.insert(
        Name::new(name),
        Export::Function(FunctionExport {
            function: id,
            signature: definition.signature.clone(),
            convention: StartConvention::Direct,
            asynchronous: false,
            rest: false,
        }),
    );
    document.functions.insert(id, definition);
    id
}

/// CANVAS-CLOSED: only closed data operations with the exact current base are accepted.
#[test]
fn canvas_closed_rejects_source_syntax_and_stale_bases() {
    let mut draft = Draft::open(document(vec![step("before")])).unwrap();
    let base = draft.identity();
    let operation = Operation::SetValue {
        base,
        target: ValueTarget::ToolArgument {
            action: site(&[0, 0]),
            index: 0,
            path: vec![],
        },
        value: data("after"),
    };
    let transaction = Transaction {
        base,
        edits: vec![operation.clone()],
    };
    let mut wire = serde_json::to_value(&transaction).unwrap();
    wire["edits"][0]["set_value"]["source"] = "return false".into();
    assert!(Transaction::from_json(&wire.to_string()).is_err());
    let mut wire = serde_json::to_value(&operation).unwrap();
    wire["set_value"]["value"] = serde_json::json!({"call": {"callee": "hidden"}});
    assert!(serde_json::from_value::<Operation>(wire).is_err());
    apply(&mut draft, vec![operation.clone()]);
    let before = draft.document().clone();
    let refusal = draft
        .apply(&transaction, &Environment::new(&before.functions))
        .unwrap_err();
    assert_eq!(
        refusal.diagnostics[0].kind,
        EditDiagnosticKind::StaleBase {
            base,
            current: draft.identity()
        }
    );
    assert_eq!(draft.document(), &before);
    // The envelope can be fresh while an individual operation still carries an old base.
    let refusal = refuse(&mut draft, vec![operation]);
    assert_eq!(refusal.diagnostics[0].edit, Some(0));
    assert!(matches!(
        refusal.diagnostics[0].kind,
        EditDiagnosticKind::StaleBase { .. }
    ));
}

/// CANVAS-VALUE/K-ID: graph read sites address the same values when written, including do arguments.
#[test]
fn canvas_values_use_read_sites_and_reidentify_exports_and_callers() {
    let mut doc = document(vec![]);
    let leaf = parse_definition(
        "function law.leaf() -> Text\nkernel 1\ncharge 1\nbody { return \"old\" }",
    )
    .unwrap();
    let leaf_id = export(&mut doc, "leaf", leaf);
    let mut caller = parse_definition(
        "function law.caller() -> Text\nkernel 1\ncharge 1\nbody { return \"unused\" }",
    )
    .unwrap();
    caller.body_mut().unwrap().block = vec![Stmt::Return {
        value: Expr::Act(Box::new(ActExpr {
            kind: ActKind::Call(Callee::Local(leaf_id)),
            args: vec![],
        })),
    }];
    caller.body_mut().unwrap().functions =
        BTreeMap::from([(leaf_id, doc.functions[&leaf_id].name.clone())]);
    let caller_id = export(&mut doc, "caller", caller);
    doc.main = Some(vec![
        Stmt::Let {
            name: Name::new("value"),
            value: Rhs::Expr(text("initial")),
        },
        Stmt::Do {
            value: perform(0, vec![Expr::List(vec![text("nested"), text("untouched")])]),
        },
    ]);
    let graph = lash_kernel_check::derive(&doc, &doc.functions).unwrap();
    let leaf_site = graph.nodes().iter().find(|node| {
        node.site.unit == Unit::Library(leaf_id) && matches!(&node.kind, NodeKind::Expr(lash_kernel_check::ExprNode::Literal(Literal::Text(value))) if value == "old")
    }).unwrap().site.clone();
    let action = graph
        .nodes()
        .iter()
        .find(|node| node.site.unit == Unit::Main && node.site == site(&[1, 0]))
        .unwrap()
        .site
        .clone();
    let mut draft = Draft::open(doc).unwrap();
    let base = draft.identity();
    let applied = apply(
        &mut draft,
        vec![
            Operation::SetValue {
                base,
                target: ValueTarget::Literal {
                    expression: leaf_site.clone(),
                },
                value: data("new"),
            },
            Operation::SetValue {
                base,
                target: ValueTarget::Initializer {
                    statement: site(&[0]),
                },
                value: data("changed"),
            },
            Operation::SetValue {
                base,
                target: ValueTarget::ToolArgument {
                    action: action.clone(),
                    index: 0,
                    path: vec![0],
                },
                value: data("edited"),
            },
        ],
    );
    let Export::Function(new_leaf) = &draft.document().exports[&Name::new("leaf")] else {
        panic!("function")
    };
    let Export::Function(new_caller) = &draft.document().exports[&Name::new("caller")] else {
        panic!("function")
    };
    assert_ne!(new_leaf.function, leaf_id);
    assert_ne!(new_caller.function, caller_id);
    assert!(!draft.document().functions.contains_key(&leaf_id));
    let body = draft.document().functions[&new_caller.function]
        .body()
        .unwrap();
    assert_eq!(
        Node::Block(&body.block).functions_within(),
        BTreeSet::from([new_leaf.function])
    );
    assert_eq!(
        body.functions.keys().copied().collect::<BTreeSet<_>>(),
        BTreeSet::from([new_leaf.function])
    );
    let successor = applied.correspondence.successor(&leaf_site).unwrap();
    assert_eq!(successor.unit, Unit::Library(new_leaf.function));
    assert!(applied.correspondence.survivor(&leaf_site).unwrap().edited);
    assert_eq!(
        draft.document().main.as_ref().unwrap(),
        &vec![
            Stmt::Let {
                name: Name::new("value"),
                value: Rhs::Expr(text("changed"))
            },
            Stmt::Do {
                value: perform(0, vec![Expr::List(vec![text("edited"), text("untouched")])])
            },
        ]
    );
    assert_eq!(applied.correspondence.successor(&action), Some(&action));
    // A whole computed data argument remains inside the do expression, with no temporary bindings.
    let base = draft.identity();
    apply(
        &mut draft,
        vec![Operation::SetValue {
            base,
            target: ValueTarget::ToolArgument {
                action,
                index: 0,
                path: vec![],
            },
            value: DataValue::List(vec![data("whole")]),
        }],
    );
    assert_eq!(
        draft.document().main.as_ref().unwrap()[1],
        Stmt::Do {
            value: perform(0, vec![Expr::List(vec![text("whole")])])
        }
    );
}

/// CANVAS-STEP: base addresses survive insertion, movement and removal in one batch.
#[test]
fn canvas_steps_preserve_order_and_base_addressed_survivors() {
    let mut draft = Draft::open(document(vec![step("a"), step("b"), step("c")])).unwrap();
    let base = draft.identity();
    let applied = apply(
        &mut draft,
        vec![
            Operation::InsertStep {
                base,
                at: Position::before(site(&[1])).unwrap(),
                slot: ImportSlot(0),
                args: vec![DataValue::List(vec![data("new")])],
                result: Type::Text,
                binding: None,
            },
            Operation::MoveStep {
                base,
                action: site(&[2, 0]),
                to: Position::before(site(&[0])).unwrap(),
            },
            Operation::RemoveStep {
                base,
                action: site(&[1, 0]),
            },
        ],
    );
    assert_eq!(
        draft.document().main.as_ref().unwrap(),
        &vec![
            step("c"),
            step("a"),
            Stmt::Do {
                value: perform(0, vec![Expr::List(vec![text("new")])])
            }
        ]
    );
    assert_eq!(
        applied.correspondence.successor(&site(&[2, 0])),
        Some(&site(&[0, 0]))
    );
    assert_eq!(
        applied.correspondence.successor(&site(&[0, 0])),
        Some(&site(&[1, 0]))
    );
    assert!(applied.correspondence.successor(&site(&[1, 0])).is_none());
    assert!(
        !applied
            .correspondence
            .entries()
            .iter()
            .any(|survivor| survivor.to == site(&[2]))
    );
}

/// CANVAS-STEP: pure local argument helpers are separable; nested effects are not.
#[test]
fn canvas_steps_accept_pure_helpers_and_refuse_effectful_arguments() {
    let mut doc = document(vec![]);
    let pure = parse_definition(
        "function law.pure(value: Text) -> Text\nkernel 1\ncharge 1\nbody { return value }",
    )
    .unwrap();
    let pure_id = pure.identity();
    doc.functions.insert(pure_id, pure);
    let effect = parse_definition("function law.effect(value: Text) -> Text\nkernel 1\ncharge 1\nimport 0 operation service.read(value: Text) -> Text\nbody { let answer = perform 0(value) as Text return answer }").unwrap();
    let effect_id = effect.identity();
    doc.imports = effect.body().unwrap().imports.clone();
    doc.functions.insert(effect_id, effect);
    let call = |id| {
        Expr::Act(Box::new(ActExpr {
            kind: ActKind::Call(Callee::Local(id)),
            args: vec![text("arg")],
        }))
    };
    doc.main = Some(vec![
        Stmt::Do {
            value: perform(0, vec![call(pure_id)]),
        },
        Stmt::Do {
            value: perform(0, vec![call(effect_id)]),
        },
    ]);
    let mut draft = Draft::open(doc).unwrap();
    let base = draft.identity();
    let applied = apply(
        &mut draft,
        vec![Operation::MoveStep {
            base,
            action: site(&[0, 0]),
            to: Position::end(site(&[])),
        }],
    );
    assert_eq!(
        applied.correspondence.successor(&site(&[0, 0])),
        Some(&site(&[1, 0]))
    );
    let base = draft.identity();
    let refusal = refuse(
        &mut draft,
        vec![Operation::RemoveStep {
            base,
            action: site(&[0, 0]),
        }],
    );
    assert_eq!(
        refusal.diagnostics[0].kind,
        EditDiagnosticKind::NonSeparableStep {
            enclosing_statement: Some(site(&[0])),
            separable_step: None
        }
    );
    // Direct nested performs are equally inseparable, even without an intervening helper.
    let mut draft = Draft::open(document(vec![Stmt::Do {
        value: perform(0, vec![perform(0, vec![text("nested")])]),
    }]))
    .unwrap();
    let base = draft.identity();
    let refusal = refuse(
        &mut draft,
        vec![Operation::RemoveStep {
            base,
            action: site(&[0, 0]),
        }],
    );
    assert!(matches!(
        refusal.diagnostics[0].kind,
        EditDiagnosticKind::NonSeparableStep { .. }
    ));
}

/// CANVAS-WRAP: supported wrappers retain ordered steps and compose correspondence on unwrap.
#[test]
fn canvas_wrappers_round_trip_steps_and_refuse_discarding_work() {
    for wrapper in [
        Wrapper::If {
            condition: boolean(true),
        },
        Wrapper::For {
            binding: Name::new("item"),
            iterable: Expr::List(vec![]),
        },
        Wrapper::Try {
            catch_binding: Name::new("error"),
        },
    ] {
        let doc = document(vec![step("a"), step("b")]);
        let mut draft = Draft::open(doc.clone()).unwrap();
        let base = draft.identity();
        let branch = if matches!(wrapper, Wrapper::If { .. }) {
            Branch::Then
        } else {
            Branch::Body
        };
        let body_index = if matches!(wrapper, Wrapper::Try { .. }) {
            0
        } else {
            1
        };
        let applied = apply(
            &mut draft,
            vec![Operation::WrapSteps {
                base,
                first: site(&[0, 0]),
                last: site(&[1, 0]),
                wrapper,
            }],
        );
        assert_eq!(draft.document().main.as_ref().unwrap().len(), 1);
        assert_eq!(
            applied.correspondence.successor(&site(&[1, 0])),
            Some(&site(&[0, body_index, 1, 0]))
        );
        let base = draft.identity();
        apply(
            &mut draft,
            vec![Operation::UnwrapSteps {
                base,
                wrapper: site(&[0]),
                branch,
            }],
        );
        assert_eq!(draft.document(), &doc);
        assert_eq!(
            draft.correspondence_since_open().successor(&site(&[1, 0])),
            Some(&site(&[1, 0]))
        );
    }
    let mut draft = Draft::open(document(vec![Stmt::If {
        condition: boolean(true),
        then_block: vec![step("kept")],
        else_block: vec![step("must not discard")],
    }]))
    .unwrap();
    let base = draft.identity();
    let refusal = refuse(
        &mut draft,
        vec![Operation::UnwrapSteps {
            base,
            wrapper: site(&[0]),
            branch: Branch::Then,
        }],
    );
    assert_eq!(
        refusal.diagnostics[0].kind,
        EditDiagnosticKind::NonEmptyDiscardedArm
    );
}

/// CANVAS-CONDITION: change only a pure, boolean header; action sites cannot be added or erased.
#[test]
fn canvas_conditions_preserve_bodies_and_reject_effects_and_wrong_types() {
    for statement in [
        Stmt::If {
            condition: boolean(true),
            then_block: vec![step("body")],
            else_block: vec![step("else")],
        },
        Stmt::While {
            metered: 0,
            condition: boolean(true),
            body: vec![step("body")],
            else_block: vec![step("else")],
        },
    ] {
        let mut draft = Draft::open(document(vec![statement])).unwrap();
        let base = draft.identity();
        let applied = apply(
            &mut draft,
            vec![Operation::ChangeCondition {
                base,
                statement: site(&[0]),
                condition: boolean(false),
            }],
        );
        assert_eq!(
            applied.correspondence.successor(&site(&[0, 1, 0, 0])),
            Some(&site(&[0, 1, 0, 0]))
        );
        assert!(
            applied
                .correspondence
                .survivor(&site(&[0, 0]))
                .unwrap()
                .edited
        );
        match &draft.document().main.as_ref().unwrap()[0] {
            Stmt::If { condition, .. } | Stmt::While { condition, .. } => {
                assert_eq!(condition, &boolean(false))
            }
            _ => panic!("condition-bearing statement retained"),
        }
        let base = draft.identity();
        let refusal = refuse(
            &mut draft,
            vec![Operation::ChangeCondition {
                base,
                statement: site(&[0]),
                condition: perform(0, vec![text("effect")]),
            }],
        );
        assert_eq!(
            refusal.diagnostics[0].kind,
            EditDiagnosticKind::ActionInHeader
        );
        let refusal = refuse(
            &mut draft,
            vec![Operation::ChangeCondition {
                base,
                statement: site(&[0]),
                condition: text("not boolean"),
            }],
        );
        assert!(matches!(
            refusal.diagnostics[0].kind,
            EditDiagnosticKind::Refused(lash_kernel_check::RefusalReason::ArgumentType { .. })
        ));
        // An indirect value edit must obey the same header rule.
        let refusal = refuse(
            &mut draft,
            vec![Operation::SetValue {
                base,
                target: ValueTarget::Literal {
                    expression: site(&[0, 0]),
                },
                value: data("not boolean"),
            }],
        );
        assert!(matches!(
            refusal.diagnostics[0].kind,
            EditDiagnosticKind::Refused(_)
        ));
    }
}

/// CANVAS-ATOMIC: a late targeting or admission fault publishes neither an earlier edit nor its correspondence.
#[test]
fn canvas_batches_roll_back_targeting_and_admission_failures() {
    let mut draft = Draft::open(document(vec![step("original")])).unwrap();
    let base = draft.identity();
    let first = Operation::SetValue {
        base,
        target: ValueTarget::ToolArgument {
            action: site(&[0, 0]),
            index: 0,
            path: vec![],
        },
        value: data("changed"),
    };
    let refusal = refuse(
        &mut draft,
        vec![
            first.clone(),
            Operation::RemoveStep {
                base,
                action: site(&[99, 0]),
            },
        ],
    );
    assert_eq!(refusal.diagnostics[0].edit, Some(1));
    assert_eq!(refusal.diagnostics[0].kind, EditDiagnosticKind::NoSuchNode);
    let refusal = refuse(
        &mut draft,
        vec![
            first,
            Operation::SetValue {
                base,
                target: ValueTarget::ToolArgument {
                    action: site(&[0, 0]),
                    index: 0,
                    path: vec![],
                },
                value: DataValue::Variable(Name::new("unbound")),
            },
        ],
    );
    assert!(matches!(
        refusal.diagnostics[0].kind,
        EditDiagnosticKind::Refused(lash_kernel_check::RefusalReason::UnboundVariable { .. })
    ));
}

/// CANVAS-REFUSAL: actual diagnostics retain the edit, base site and typed cause in the host JSON form.
#[test]
fn canvas_refusal_wire_names_the_exact_failed_operation_and_site() {
    let mut draft = Draft::open(document(vec![Stmt::Return {
        value: text("not a step"),
    }]))
    .unwrap();
    let base = draft.identity();
    let refusal = refuse(
        &mut draft,
        vec![Operation::RemoveStep {
            base,
            action: site(&[0, 0]),
        }],
    );
    let wire = serde_json::to_value(&refusal).unwrap();
    assert_eq!(
        wire,
        serde_json::json!({"diagnostics": [{
            "edit": 0, "location": {"base": {"unit": "main", "path": [0, 0]}},
            "kind": {"non_separable_step": {"enclosing_statement": {"unit": "main", "path": [0]}, "separable_step": null}}
        }]})
    );
    assert_eq!(refusal.diagnostics[0].kind.code(), "non_separable_step");
    assert_eq!(
        serde_json::from_value::<EditRefusal>(wire).unwrap(),
        refusal
    );
}

fn loop_else(while_loop: bool) -> Stmt {
    if while_loop {
        Stmt::While {
            metered: 0,
            condition: boolean(false),
            body: vec![],
            else_block: vec![step("else")],
        }
    } else {
        Stmt::For {
            metered: 0,
            binding: Name::new("item"),
            iterable: Expr::List(vec![]),
            via: None,
            body: vec![],
            else_block: vec![step("else")],
        }
    }
}
/// K-ID-004: removing or moving the last loop-else statement removes the optional block's identity.
#[test]
fn empty_loop_else_has_no_survivor_after_remove_or_move() {
    for while_loop in [false, true] {
        for moving in [false, true] {
            let mut draft =
                Draft::open(document(vec![loop_else(while_loop), step("tail")])).unwrap();
            let base = draft.identity();
            let action = site(&[0, 2, 0, 0]);
            let operation = if moving {
                Operation::MoveStep {
                    base,
                    action: action.clone(),
                    to: Position::end(site(&[])),
                }
            } else {
                Operation::RemoveStep {
                    base,
                    action: action.clone(),
                }
            };
            let applied = apply(&mut draft, vec![operation]);
            assert!(applied.correspondence.successor(&site(&[0, 2])).is_none());
            assert!(
                !draft
                    .document()
                    .site_ordinals()
                    .contains_key(&site(&[0, 2]))
            );
            assert_eq!(
                applied.correspondence.successor(&action),
                moving.then(|| site(&[2, 0])).as_ref()
            );
            assert_eq!(
                draft.document().main.as_ref().unwrap().len(),
                if moving { 3 } else { 2 }
            );
        }
    }
}
/// K-ID-004/CANVAS-WRAP: singleton loop-else blocks remain addressable across in-place move, wrap and unwrap.
#[test]
fn singleton_loop_else_moves_wraps_and_unwraps_in_place() {
    for while_loop in [false, true] {
        let doc = document(vec![loop_else(while_loop)]);
        let mut draft = Draft::open(doc.clone()).unwrap();
        let base = draft.identity();
        let applied = apply(
            &mut draft,
            vec![Operation::MoveStep {
                base,
                action: site(&[0, 2, 0, 0]),
                to: Position::end(site(&[0, 2])),
            }],
        );
        assert_eq!(draft.document(), &doc);
        assert_eq!(
            applied.correspondence.successor(&site(&[0, 2])),
            Some(&site(&[0, 2]))
        );
        let base = draft.identity();
        let applied = apply(
            &mut draft,
            vec![Operation::WrapSteps {
                base,
                first: site(&[0, 2, 0, 0]),
                last: site(&[0, 2, 0, 0]),
                wrapper: Wrapper::If {
                    condition: boolean(true),
                },
            }],
        );
        assert_eq!(
            applied.correspondence.successor(&site(&[0, 2, 0, 0])),
            Some(&site(&[0, 2, 0, 1, 0, 0]))
        );
        let base = draft.identity();
        apply(
            &mut draft,
            vec![Operation::UnwrapSteps {
                base,
                wrapper: site(&[0, 2, 0]),
                branch: Branch::Then,
            }],
        );
        assert_eq!(draft.document(), &doc);
        assert_eq!(
            draft
                .correspondence_since_open()
                .successor(&site(&[0, 2, 0, 0])),
            Some(&site(&[0, 2, 0, 0]))
        );
    }
}

/// EXTRACT-REACHABLE: keep exactly one Export's local/anonymous closure and Imports, never main or siblings.
#[test]
fn extract_retains_only_the_selected_export_and_transitive_closure() {
    let mut doc = document(vec![step("main only")]);
    let (root, locals) = lash_kernel_doc::parse_definition_with_locals("function law.entry(value: Text) -> Text\nkernel 1\ncharge 1\nimport 0 operation service.read(value: Text) -> Text\nbody { let callback = fn() { let answer = perform 0(value) as Text return answer } let answer = apply callback() return answer }").unwrap();
    let root_id = export(&mut doc, "selected", root);
    let local_ids: BTreeSet<_> = locals.iter().map(FunctionDefinition::identity).collect();
    doc.functions
        .extend(locals.into_iter().map(|f| (f.identity(), f)));
    let unrelated = parse_definition("function law.other() -> Text\nkernel 1\ncharge 1\nimport 1 operation service.other() -> Text\nbody { let answer = perform 1() as Text return answer }").unwrap();
    export(&mut doc, "other", unrelated);
    doc.private_bindings.insert(Name::new("private"));
    let before = doc.clone();
    let extracted = extract_entry(
        &doc,
        &Name::new("selected"),
        &Environment::new(&doc.functions),
    )
    .unwrap();
    assert_eq!(doc, before);
    assert!(extracted.document.main.is_none());
    assert!(extracted.document.private_bindings.is_empty());
    assert_eq!(
        extracted
            .document
            .exports
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec![Name::new("selected")]
    );
    assert_eq!(
        extracted
            .document
            .functions
            .keys()
            .copied()
            .collect::<BTreeSet<_>>(),
        local_ids.into_iter().chain([root_id]).collect()
    );
    assert_eq!(extracted.requirements, BTreeSet::from([ImportSlot(0)]));
    assert_eq!(
        extracted
            .document
            .imports
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        vec![ImportSlot(0)]
    );
    let applied = extracted.applied.unwrap();
    for (site, _) in extracted.document.ordinal_nodes() {
        assert_eq!(applied.correspondence.successor(&site), Some(&site));
    }
    assert!(applied.correspondence.successor(&site(&[])).is_none());
    let refusal = extract_entry(
        &doc,
        &Name::new("missing"),
        &Environment::new(&doc.functions),
    )
    .unwrap_err();
    assert_eq!(
        refusal.diagnostics[0].kind,
        EditDiagnosticKind::NoSuchEntry {
            name: Name::new("missing")
        }
    );
    assert_eq!(doc, before);
}

fn context(root: &str) -> PublicationContext {
    PublicationContext {
        domain: PublicationDomain("law".into()),
        scope: PublicationScope::Host,
        publisher: PublisherInstance(root.into()),
        root: Segment::new(root).unwrap(),
    }
}
fn service(doc: &Document, contract: &str) -> PackageAdvertisement {
    let ImportKind::Operation { signature } = &doc.imports[&ImportSlot(0)].kind else {
        panic!("operation")
    };
    PackageAdvertisement::Enumerated(Package {
        path: PackagePath::parse("service").unwrap(),
        members: BTreeMap::from([(
            Segment::new("read").unwrap(),
            Member {
                visibility: Visibility::Public,
                kind: MemberKind::Operation(OperationOffer {
                    requirement: lash_kernel_value::operations::OperationRequirement {
                        signature: signature.clone(),
                        contract: lash_kernel_value::operations::ContractId::new(contract),
                    },
                    intents: BTreeSet::new(),
                    controls: lash_kernel_value::TurnControls::none(),
                    mode: OperationMode::Waited,
                }),
            },
        )]),
    })
}
/// L-ALIAS/L-ADMIT-CONTRACT at the Draft seam: rebind slots, preserve exact code and old Links, refuse atomically.
#[test]
fn relink_rebinds_each_import_without_rewriting_document_or_function_ids() {
    let mut doc = document(vec![step("root")]);
    // Two slots for one operation must each be fulfilled, including one reached in a local function.
    doc.imports
        .insert(ImportSlot(7), doc.imports[&ImportSlot(0)].clone());
    let function = parse_definition("function law.nested(value: Text) -> Text\nkernel 1\ncharge 1\nimport 7 operation service.read(value: Text) -> Text\nbody { let answer = perform 7(value) as Text return answer }").unwrap();
    export(&mut doc, "nested", function);
    let draft = Draft::open(doc.clone()).unwrap();
    let mut content = LinkedContent::default();
    content
        .insert_document(doc.clone(), &doc.functions)
        .unwrap();
    let owner = context("service");
    let consumer = context("app");
    let a = service(&doc, "old");
    let b = service(&doc, "new");
    let old = draft
        .relink(
            &content,
            &BTreeMap::<lash_kernel_value::FunctionId, lash_kernel_doc::FunctionDefinition>::new(),
            &Namespace {
                advertisements: vec![(&owner, &a)],
            },
            &consumer,
            None,
        )
        .unwrap();
    let new = draft
        .relink(
            &content,
            &BTreeMap::<lash_kernel_value::FunctionId, lash_kernel_doc::FunctionDefinition>::new(),
            &Namespace {
                advertisements: vec![(&owner, &b)],
            },
            &consumer,
            None,
        )
        .unwrap();
    assert_ne!(old.identity(), new.identity());
    assert_eq!(old.document, doc.identity());
    assert_eq!(new.document, doc.identity());
    assert_eq!(
        new.fills.keys().copied().collect::<Vec<_>>(),
        vec![ImportSlot(0), ImportSlot(7)]
    );
    for slot in [ImportSlot(0), ImportSlot(7)] {
        let Fill::Operation(before) = &old.fills[&slot] else {
            panic!("operation")
        };
        let Fill::Operation(after) = &new.fills[&slot] else {
            panic!("operation")
        };
        assert_eq!(
            before.contract,
            lash_kernel_value::operations::ContractId::new("old")
        );
        assert_eq!(
            after.contract,
            lash_kernel_value::operations::ContractId::new("new")
        );
    }
    let before = draft.correspondence_since_open().clone();
    assert!(matches!(
        draft.relink(
            &content,
            &BTreeMap::<lash_kernel_value::FunctionId, lash_kernel_doc::FunctionDefinition>::new(),
            &Namespace {
                advertisements: vec![]
            },
            &consumer,
            None
        ),
        Err(lash_kernel_package::LinkRefusal::MissingImport {
            slot: ImportSlot(0),
            ..
        })
    ));
    assert_eq!(draft.document(), &doc);
    assert_eq!(draft.identity(), doc.identity());
    assert_eq!(draft.correspondence_since_open(), &before);
    let mut incompatible = service(&doc, "incompatible");
    let PackageAdvertisement::Enumerated(package) = &mut incompatible else {
        panic!("enumerated")
    };
    let MemberKind::Operation(offer) = &mut package
        .members
        .get_mut(&Segment::new("read").unwrap())
        .unwrap()
        .kind
    else {
        panic!("operation")
    };
    offer.requirement.signature.params[0].ty = Type::Bool;
    assert!(matches!(
        draft.relink(
            &content,
            &BTreeMap::<lash_kernel_value::FunctionId, lash_kernel_doc::FunctionDefinition>::new(),
            &Namespace {
                advertisements: vec![(&owner, &incompatible)]
            },
            &consumer,
            None
        ),
        Err(lash_kernel_package::LinkRefusal::SignatureMismatch {
            slot: ImportSlot(0),
            ..
        })
    ));
    assert_eq!(draft.document(), &doc);
    assert_eq!(draft.correspondence_since_open(), &before);
}

/// L-ALIAS/L-PIN: relinking one alias changes its slot alone; reached code keeps each defining Link's fills.
#[test]
fn relink_closure_keeps_alias_slots_and_transitive_link_pins() {
    let mut leaf = document(vec![]);
    leaf.main = None;
    let definition = parse_definition("function law.read(value: Text) -> Text\nkernel 1\ncharge 1\nimport 0 operation service.read(value: Text) -> Text\nbody { let answer = perform 0(value) as Text return answer }").unwrap();
    let function = export(&mut leaf, "read", definition);
    let signature = leaf.functions[&function].signature.clone();
    let mut content = LinkedContent::default();
    content
        .insert_document(leaf.clone(), &leaf.functions)
        .unwrap();
    let leaf_draft = Draft::open(leaf.clone()).unwrap();
    let owner = context("service");
    let consumer = context("app");
    let mut links = vec![];
    for contract in ["old", "new"] {
        let ad = service(&leaf, contract);
        let link = leaf_draft
            .relink(
                &content,
            &BTreeMap::<lash_kernel_value::FunctionId, lash_kernel_doc::FunctionDefinition>::new(),
                &Namespace {
                    advertisements: vec![(&owner, &ad)],
                },
                &consumer,
                None,
            )
            .unwrap();
        links.push(content.insert_link(link).unwrap());
    }
    let mut root = Document::new(NumberPolicy::Float, vec![]);
    for (slot, name) in [(1, "first"), (2, "second")] {
        root.imports.insert(
            ImportSlot(slot),
            Import {
                name: QualifiedName {
                    package: PackagePath::parse("code").unwrap(),
                    member: Segment::new(name).unwrap(),
                    type_member: None,
                },
                kind: ImportKind::Code {
                    signature: signature.clone(),
                    convention: Convention::Direct,
                },
            },
        );
        root.main.as_mut().unwrap().push(Stmt::Do {
            value: Expr::Act(Box::new(ActExpr {
                kind: ActKind::Call(Callee::Import(ImportSlot(slot))),
                args: vec![text(name)],
            })),
        });
    }
    content
        .insert_document(root.clone(), &root.functions)
        .unwrap();
    let draft = Draft::open(root.clone()).unwrap();
    let code_owner = context("code");
    let mut roots = vec![];
    for first in [links[0], links[1]] {
        let code = PackageAdvertisement::Enumerated(Package {
            path: PackagePath::parse("code").unwrap(),
            members: [("first", first), ("second", links[0])]
                .into_iter()
                .map(|(name, link)| {
                    (
                        Segment::new(name).unwrap(),
                        Member {
                            visibility: Visibility::Public,
                            kind: MemberKind::Export {
                                link,
                                export: ExportName {
                                    name: Name::new("read"),
                                    member: None,
                                },
                            },
                        },
                    )
                })
                .collect(),
        });
        let link = draft
            .relink(
                &content,
            &BTreeMap::<lash_kernel_value::FunctionId, lash_kernel_doc::FunctionDefinition>::new(),
                &Namespace {
                    advertisements: vec![(&code_owner, &code)],
                },
                &consumer,
                None,
            )
            .unwrap();
        assert_eq!(
            link.fills[&ImportSlot(1)],
            Fill::Export {
                link: first,
                export: ExportName {
                    name: Name::new("read"),
                    member: None
                }
            }
        );
        assert_eq!(
            link.fills[&ImportSlot(2)],
            Fill::Export {
                link: links[0],
                export: ExportName {
                    name: Name::new("read"),
                    member: None
                }
            }
        );
        roots.push(content.insert_link(link).unwrap());
    }
    let reached = content
        .inspect(
            roots[1],
            &lash_kernel_package::ReachSelection::Start(StartSelector::Main),
            &BTreeMap::new(),
        )
        .unwrap();
    assert_eq!(reached.operations.len(), 2);
    for (link, contract) in [(links[0], "old"), (links[1], "new")] {
        assert_eq!(reached.functions[&link], BTreeSet::from([function]));
        let lash_kernel_package::OperationNeed::Fixed { requirement, .. } = &reached.operations
            [&OperationUse {
                link,
                slot: (ImportSlot(0)).into(),
            }]
        else {
            panic!("fixed operation")
        };
        assert_eq!(
            requirement.contract,
            lash_kernel_value::operations::ContractId::new(contract)
        );
    }
    assert_eq!(
        content.links[&roots[0]].fills[&ImportSlot(1)],
        content.links[&roots[0]].fills[&ImportSlot(2)]
    );
    assert_eq!(draft.document(), &root);
}

/// DRAFT-ATOMIC: resolved renames preserve shadowed uses, and replacement refuses whole.
#[test]
fn draft_atomic_resolves_names_and_refuses_replacement() {
    let mut draft = Draft::open(document(vec![
        Stmt::Let {
            name: Name::new("value"),
            value: Rhs::Expr(text("outer")),
        },
        Stmt::If {
            condition: boolean(true),
            then_block: vec![
                Stmt::Let {
                    name: Name::new("value"),
                    value: Rhs::Expr(text("inner")),
                },
                Stmt::Print {
                    value: Expr::Variable(Name::new("value")),
                },
            ],
            else_block: vec![],
        },
        Stmt::Print {
            value: Expr::Variable(Name::new("value")),
        },
    ]))
    .unwrap();
    let base = draft.identity();
    let renamed = apply(
        &mut draft,
        vec![Operation::RenameBinding {
            base,
            declaration: site(&[0]),
            name: Name::new("renamed"),
        }],
    );
    assert!(
        matches!(&draft.document().main.as_ref().unwrap()[2], Stmt::Print { value: Expr::Variable(n) } if n == &Name::new("renamed"))
    );
    assert!(
        matches!(&draft.document().main.as_ref().unwrap()[1], Stmt::If { then_block, .. } if matches!(&then_block[1], Stmt::Print { value: Expr::Variable(n) } if n == &Name::new("value")))
    );
    let base = draft.identity();
    let before = draft.document().clone();
    let invalid = document(vec![Stmt::Print {
        value: Expr::Variable(Name::new("missing")),
    }]);
    // Main's session reads are legal, so an invalid header is the admission refusal.
    let invalid = Document {
        main: Some(vec![Stmt::If {
            condition: text("wrong"),
            then_block: vec![],
            else_block: vec![],
        }]),
        ..invalid
    };
    refuse(
        &mut draft,
        vec![Operation::ReplaceDocument {
            base,
            document: invalid,
        }],
    );
    assert_eq!(draft.document(), &before);
    let collision = document(vec![
        Stmt::Let {
            name: Name::new("a"),
            value: Rhs::Expr(text("a")),
        },
        Stmt::Let {
            name: Name::new("b"),
            value: Rhs::Expr(text("b")),
        },
        Stmt::Print {
            value: Expr::Variable(Name::new("a")),
        },
    ]);
    let mut draft = Draft::open(collision).unwrap();
    let base = draft.identity();
    assert!(renamed.correspondence.survivor(&site(&[0])).unwrap().edited);
    assert!(
        renamed
            .correspondence
            .survivor(&site(&[2, 0]))
            .unwrap()
            .edited
    );
    assert!(matches!(
        refuse(
            &mut draft,
            vec![Operation::RenameBinding {
                base,
                declaration: site(&[0]),
                name: Name::new("b")
            }]
        )
        .diagnostics[0]
            .kind,
        EditDiagnosticKind::NameCollision { .. }
    ));
    refuse(
        &mut draft,
        vec![Operation::RemoveBinding {
            base,
            declaration: site(&[0]),
        }],
    );
    let replacement = document(vec![Stmt::Print {
        value: text("replacement"),
    }]);
    assert_eq!(
        refuse(
            &mut draft,
            vec![
                Operation::ReplaceDocument {
                    base,
                    document: replacement.clone()
                },
                Operation::RemoveBinding {
                    base,
                    declaration: site(&[0])
                }
            ]
        )
        .diagnostics[0]
            .kind,
        EditDiagnosticKind::ReplacementMustBeSole
    );
    let applied = apply(
        &mut draft,
        vec![Operation::ReplaceDocument {
            base,
            document: replacement.clone(),
        }],
    );
    assert!(applied.correspondence.entries().is_empty());
    assert_eq!(draft.into_document(), replacement);
    let mut repaired = Draft::open(document(vec![
        Stmt::Let {
            name: Name::new("a"),
            value: Rhs::Expr(text("old")),
        },
        Stmt::Let {
            name: Name::new("b"),
            value: Rhs::Expr(Expr::Variable(Name::new("a"))),
        },
    ]))
    .unwrap();
    let base = repaired.identity();
    apply(
        &mut repaired,
        vec![
            Operation::RemoveBinding {
                base,
                declaration: site(&[0]),
            },
            Operation::SetValue {
                base,
                target: ValueTarget::Initializer {
                    statement: site(&[1]),
                },
                value: data("repaired"),
            },
        ],
    );
}

/// CANVAS-VALUE: exact Error/Timestamp constructors remain pure data operations.
#[test]
fn typed_data_constructors_use_installed_helpers() {
    let mut functions = lash_kernel_doc::FunctionRegistry::declarations();
    let timestamp = functions.register(parse_definition("function timestamp.from_nanoseconds(value: Int) -> Timestamp\nkernel 1\ncharge 1\nnative").unwrap(), None).unwrap();
    let error = functions.register(parse_definition("function error.new(kind: Text, message: Text, data: Any) -> Error\nkernel 1\ncharge 1\nnative").unwrap(), None).unwrap();
    let mut draft = Draft::open(Document::new(
        NumberPolicy::Float,
        vec![Stmt::Let {
            name: Name::new("value"),
            value: Rhs::Expr(text("initial")),
        }],
    ))
    .unwrap();
    let base = draft.identity();
    draft
        .apply(
            &Transaction {
                base,
                edits: vec![Operation::SetValue {
                    base,
                    target: ValueTarget::Initializer {
                        statement: site(&[0]),
                    },
                    value: DataValue::Error {
                        constructor: error,
                        kind: "kind".into(),
                        message: "message".into(),
                        data: Box::new(DataValue::Timestamp {
                            constructor: timestamp,
                            nanoseconds: 42.into(),
                        }),
                    },
                }],
            },
            &Environment::new(&functions),
        )
        .unwrap();
    assert!(draft.document().library.contains(&timestamp));
    assert!(draft.document().library.contains(&error));
    let before = draft.document().clone();
    let base = draft.identity();
    assert!(
        draft
            .apply(
                &Transaction {
                    base,
                    edits: vec![Operation::SetValue {
                        base,
                        target: ValueTarget::Initializer {
                            statement: site(&[0])
                        },
                        value: DataValue::Timestamp {
                            constructor: error,
                            nanoseconds: 42.into()
                        }
                    }]
                },
                &Environment::new(&functions)
            )
            .is_err()
    );
    assert_eq!(draft.document(), &before);
}
