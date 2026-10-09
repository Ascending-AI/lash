//! Laws of typed workflow editing (FIG-5573).
//!
//! Each program is direct IR and each expectation is the program the edited
//! document must spell, so no law passes through a dialect.

use crate::testing::ast_builders as b;
use crate::{
    Declaration, Expr, ExprSlot, FunctionExpr, ProcessDecl, ProcessOrigin, ProcessWrapperParts,
    Program, StructuralRole, TypeExpr, WorkflowBodySlot, WorkflowDeclaration, WorkflowGraph,
    WorkflowNodeId, WorkflowSlotPath, workflow_graph_from_program, workflow_node_statement,
    workflow_program_from_graph,
};

use super::{
    WorkflowBindingRef, WorkflowBodyRef, WorkflowCorrespondence,
    WorkflowCorrespondenceEntry as Entry, WorkflowDraft, WorkflowDraftHandle, WorkflowEdgeDrag,
    WorkflowEdit, WorkflowEditDiagnosticKind as Kind, WorkflowEditLocation, WorkflowEditRefusal,
    WorkflowEditTransaction, WorkflowNodeSource,
};

fn echo(value: Expr) -> Expr {
    b::module_call(&["tools"], "echo", vec![b::record(vec![("value", value)])])
}

/// The slot of the value an `echo` statement passes.
fn echoed() -> WorkflowSlotPath {
    WorkflowSlotPath::structural([
        ExprSlot::Operand,
        ExprSlot::Operand,
        ExprSlot::Arg(0),
        ExprSlot::Entry(0),
    ])
}

fn completion(mut statements: Vec<Expr>) -> Expr {
    statements.push(Expr::Absent);
    b::role(StructuralRole::Completion, b::block(statements))
}

/// A process body inside the failure wrapper, its run function driven by a
/// builtin and passed `params` straight through.
fn wrapped(params: &[&str], captures: &[&str], body: Expr) -> Expr {
    ProcessWrapperParts::build(
        FunctionExpr {
            name: Some("run".into()),
            js_name: Some("run".into()),
            receiver: None,
            params: params.iter().map(|name| (*name).into()).collect(),
            captures: captures.iter().map(|name| (*name).into()).collect(),
            body: Box::new(body),
        },
        Some(("__lash_vm_async".into(), vec![b::bool_lit(true)])),
        params.iter().map(|name| b::var(name)).collect(),
        "caught".into(),
    )
}

fn open(program: &Program) -> WorkflowDraft {
    WorkflowDraft::open(&workflow_graph_from_program(program)).expect("the document opens")
}

fn apply(
    draft: &mut WorkflowDraft,
    edits: Vec<WorkflowEdit>,
) -> Result<WorkflowCorrespondence, WorkflowEditRefusal> {
    draft.apply(WorkflowEditTransaction {
        base: draft.revision(),
        edits,
    })
}

fn main(draft: &WorkflowDraft) -> Vec<WorkflowDraftHandle> {
    draft.body(&WorkflowBodyRef::Main).expect("main is a body")
}

fn child(
    draft: &WorkflowDraft,
    node: WorkflowDraftHandle,
    slot: WorkflowBodySlot,
) -> Vec<WorkflowDraftHandle> {
    draft
        .body(&WorkflowBodyRef::Child { node, slot })
        .expect("the container has the body")
}

fn spelled(draft: &WorkflowDraft) -> Program {
    workflow_program_from_graph(draft.document()).expect("a draft's document spells a program")
}

fn id(draft: &WorkflowDraft, handle: WorkflowDraftHandle) -> WorkflowNodeId {
    draft.node_id(handle).expect("the handle is live").clone()
}

/// The single refusal of a transaction, after checking that the draft kept
/// its document and revision.
fn refused(
    draft: &mut WorkflowDraft,
    edits: Vec<WorkflowEdit>,
) -> (Option<usize>, WorkflowEditLocation, Kind) {
    let before: WorkflowGraph = draft.document().clone();
    let revision = draft.revision();
    let refusal = apply(draft, edits).expect_err("the transaction is refused");
    assert_eq!(draft.document(), &before, "a refusal changes nothing");
    assert_eq!(draft.revision(), revision);
    let [diagnostic] = refusal.diagnostics.as_slice() else {
        panic!("one diagnostic, found {:?}", refusal.diagnostics);
    };
    (
        diagnostic.edit,
        diagnostic.location.clone(),
        diagnostic.kind.clone(),
    )
}

#[test]
fn renaming_a_binding_rewrites_the_uses_that_resolve_to_it() {
    let program = |total: &str| {
        b::program(vec![
            b::assign(total, b::num(1.0)),
            b::for_in(
                "item",
                b::list(vec![b::num(1.0)]),
                b::block(vec![echo(b::list(vec![b::var(total), b::var("item")]))]),
            ),
            // A closure that captures the variable reads it under the same
            // name; one that takes a parameter of that name does not.
            b::assign("reads", b::closure(None, &[], &[total], b::var(total))),
            b::assign(
                "shadows",
                b::closure(None, &["total"], &[], b::var("total")),
            ),
            echo(b::var(total)),
        ])
    };
    let mut draft = open(&program("total"));
    let reader = main(&draft)[4];
    let rename = |name: &str| WorkflowEdit::RenameBinding {
        binding: WorkflowBindingRef::Variable {
            at: reader,
            name: "total".into(),
        },
        name: name.into(),
    };

    let (_, _, kind) = refused(&mut draft, vec![rename("item")]);
    assert_eq!(
        kind,
        Kind::BindingNameTaken {
            name: "item".into()
        }
    );

    let handles = main(&draft);
    apply(&mut draft, vec![rename("sum")]).expect("the rename applies");
    assert_eq!(spelled(&draft), program("sum"));
    assert_eq!(main(&draft), handles, "a rename keeps every handle");
}

#[test]
fn renaming_a_process_parameter_follows_it_through_the_failure_wrapper() {
    let program = |input: &str| {
        let body = completion(vec![b::print(b::var(input)), b::finish(b::var(input))]);
        b::module(
            vec![Declaration::Process(ProcessDecl {
                name: "worker".into(),
                params: vec![b::param(input, TypeExpr::Str)],
                return_ty: None,
                label: None,
                origin: ProcessOrigin::Declared,
                body: wrapped(&[input], &[], body),
            })],
            vec![b::print(b::string("main"))],
        )
    };
    let mut draft = open(&program("text"));
    let worker = draft
        .handle(&draft.document().process("worker").expect("declared").id)
        .expect("the process has a handle");
    // Named where a statement of the body reads it, the variable is still
    // the process's parameter: the run function only passes it through.
    let reader = draft
        .body(&WorkflowBodyRef::Process(worker))
        .expect("the process has a body")[0];
    apply(
        &mut draft,
        vec![WorkflowEdit::RenameBinding {
            binding: WorkflowBindingRef::Variable {
                at: reader,
                name: "text".into(),
            },
            name: "payload".into(),
        }],
    )
    .expect("the rename applies");
    assert_eq!(spelled(&draft), program("payload"));
}

#[test]
fn removing_a_producer_with_live_uses_is_refused_unless_the_transaction_reconnects_them() {
    let mut draft = open(&b::program(vec![
        b::assign("value", echo(b::string("a"))),
        echo(b::var("value")),
    ]));
    let [producer, consumer] = main(&draft)[..] else {
        panic!("two statements");
    };

    let (edit, location, kind) = refused(
        &mut draft,
        vec![WorkflowEdit::RemoveNode { node: producer }],
    );
    assert_eq!(edit, None, "bindings are checked on the whole result");
    assert_eq!(
        kind,
        Kind::UnresolvedBinding {
            name: "value".into()
        }
    );
    assert_eq!(
        location,
        WorkflowEditLocation::Node {
            node: consumer,
            slot: echoed(),
        },
        "the diagnostic points at the read"
    );
    let statement =
        workflow_node_statement(draft.node(consumer).expect("still there")).expect("a statement");
    assert_eq!(
        statement.at_slots(&echoed().expr_slots().expect("structural")),
        Some(&b::var("value"))
    );

    let stale = draft.revision();
    apply(
        &mut draft,
        vec![
            WorkflowEdit::RemoveNode { node: producer },
            WorkflowEdit::ReplaceExpression {
                node: consumer,
                slot: echoed(),
                expression: b::string("b"),
            },
        ],
    )
    .expect("the same transaction reconnects the use");
    assert_eq!(spelled(&draft), b::program(vec![echo(b::string("b"))]));

    let refusal = draft
        .apply(WorkflowEditTransaction {
            base: stale,
            edits: Vec::new(),
        })
        .expect_err("a transaction names the revision it was written against");
    assert!(matches!(
        refusal.diagnostics[0].kind,
        Kind::StaleRevision { .. }
    ));
}

#[test]
fn a_move_across_scopes_reruns_dominance_and_capture_checks() {
    let mut draft = open(&b::module(
        vec![b::process(
            "worker",
            vec![b::param("count", TypeExpr::Any)],
            b::block(vec![b::finish(b::var("count"))]),
        )],
        vec![
            b::assign("count", b::num(1.0)),
            b::for_in(
                "item",
                b::list(vec![b::num(1.0)]),
                b::block(vec![echo(b::var("item"))]),
            ),
            b::print(b::var("count")),
        ],
    ));
    let [_, each, print] = main(&draft)[..] else {
        panic!("three statements");
    };
    let [body] = child(&draft, each, WorkflowBodySlot::LoopBody)[..] else {
        panic!("one statement in the loop");
    };

    // Out of the loop, nothing binds the element the statement reads.
    let (_, location, kind) = refused(
        &mut draft,
        vec![WorkflowEdit::MoveNode {
            node: body,
            body: WorkflowBodyRef::Main,
            before: None,
        }],
    );
    assert_eq!(
        kind,
        Kind::UnresolvedBinding {
            name: "item".into()
        }
    );
    assert_eq!(
        location,
        WorkflowEditLocation::Node {
            node: body,
            slot: echoed(),
        }
    );

    // In the process, `count` is the process's parameter, another variable.
    let worker = draft
        .handle(&draft.document().process("worker").expect("declared").id)
        .expect("the process has a handle");
    let (_, location, kind) = refused(
        &mut draft,
        vec![WorkflowEdit::MoveNode {
            node: print,
            body: WorkflowBodyRef::Process(worker),
            before: None,
        }],
    );
    assert_eq!(
        kind,
        Kind::BindingCaptured {
            name: "count".into()
        }
    );
    assert_eq!(
        location,
        WorkflowEditLocation::Node {
            node: print,
            slot: WorkflowSlotPath::default(),
        }
    );
}

#[test]
fn a_try_region_is_edited_in_place() {
    let mut draft = open(&b::program(vec![b::try_expr(
        b::block(vec![echo(b::string("a"))]),
        Some(b::catch("error", b::block(vec![b::print(b::var("error"))]))),
        Some(b::block(vec![b::print(b::string("done"))])),
    )]));
    let [region] = main(&draft)[..] else {
        panic!("one statement");
    };
    let [call] = child(&draft, region, WorkflowBodySlot::TryBody)[..] else {
        panic!("one statement in the body");
    };
    let [handler] = child(&draft, region, WorkflowBodySlot::Catch)[..] else {
        panic!("one statement in the catch");
    };
    let [cleanup] = child(&draft, region, WorkflowBodySlot::Finally)[..] else {
        panic!("one statement in the finally");
    };
    let before = [region, call, handler].map(|handle| id(&draft, handle));

    let correspondence = apply(
        &mut draft,
        vec![
            WorkflowEdit::SetCatch {
                node: region,
                binding: Some("problem".into()),
            },
            WorkflowEdit::ReplaceExpression {
                node: handler,
                slot: WorkflowSlotPath::structural([ExprSlot::Operand]),
                expression: b::var("problem"),
            },
            WorkflowEdit::InsertNode {
                body: WorkflowBodyRef::Child {
                    node: region,
                    slot: WorkflowBodySlot::TryBody,
                },
                before: Some(call),
                statement: b::print(b::string("start")),
            },
            WorkflowEdit::SetFinally {
                node: region,
                present: false,
            },
        ],
    )
    .expect("the region's edits apply");

    assert_eq!(
        spelled(&draft),
        b::program(vec![b::try_expr(
            b::block(vec![b::print(b::string("start")), echo(b::string("a"))]),
            Some(b::catch(
                "problem",
                b::block(vec![b::print(b::var("problem"))]),
            )),
            None,
        )])
    );
    let [started, kept] = child(&draft, region, WorkflowBodySlot::TryBody)[..] else {
        panic!("two statements in the body");
    };
    assert_eq!(kept, call);
    let [region_id, call_id, handler_id] = before;
    assert!(correspondence.entries.contains(&Entry::Retained {
        handle: region,
        from: region_id.clone(),
        to: region_id,
    }));
    assert!(correspondence.entries.contains(&Entry::Retained {
        handle: call,
        from: call_id.clone(),
        to: id(&draft, call),
    }));
    assert_ne!(call_id, id(&draft, call), "an id follows its position");
    assert!(correspondence.entries.contains(&Entry::Retained {
        handle: handler,
        from: handler_id.clone(),
        to: handler_id,
    }));
    assert!(correspondence.entries.contains(&Entry::Inserted {
        handle: started,
        to: id(&draft, started),
        source: WorkflowNodeSource::Authored,
    }));
    assert!(
        correspondence
            .entries
            .iter()
            .any(|entry| matches!(entry, Entry::Deleted { handle, .. } if *handle == cleanup))
    );
}

#[test]
fn a_lifted_process_is_edited_through_its_container_and_keeps_its_handles() {
    let program = |seed: &str, statements: Vec<Expr>| {
        let mut body = statements;
        body.push(b::finish(b::var(seed)));
        b::program(vec![
            b::assign(seed, b::num(1.0)),
            b::assign(
                "child",
                Expr::ProcessLiteral(Box::new(crate::ProcessLiteralExpr {
                    params: vec![b::param("input", TypeExpr::Str)],
                    hidden_args: vec![b::param(seed, TypeExpr::Any)],
                    return_ty: None,
                    body: Box::new(wrapped(&["input"], &[seed], completion(body))),
                })),
            ),
        ])
    };
    let lifted = |draft: &WorkflowDraft| {
        let [WorkflowDeclaration::Process(process)] = draft.document().declarations.as_slice()
        else {
            panic!("the literal projects as one process container");
        };
        assert!(process.origin.is_lifted());
        process.id.clone()
    };
    let mut draft = open(&program("captured", vec![b::print(b::var("input"))]));
    let container_id = lifted(&draft);
    let container = draft
        .handle(&container_id)
        .expect("the container has a handle");
    let [print, finish] = draft
        .body(&WorkflowBodyRef::Process(container))
        .expect("the container has a body")[..]
    else {
        panic!("two statements in the process");
    };
    let finish_id = id(&draft, finish);

    let correspondence = apply(
        &mut draft,
        vec![
            WorkflowEdit::ReplaceExpression {
                node: print,
                slot: WorkflowSlotPath::structural([ExprSlot::Operand]),
                expression: b::string("started"),
            },
            WorkflowEdit::InsertNode {
                body: WorkflowBodyRef::Process(container),
                before: Some(finish),
                statement: echo(b::var("input")),
            },
        ],
    )
    .expect("the edits inside the lifted process apply");

    // The literal carries the edited body; the container was renamed by the
    // digest of that body and is still the same container to the host.
    assert_eq!(
        spelled(&draft),
        program(
            "captured",
            vec![b::print(b::string("started")), echo(b::var("input"))],
        )
    );
    assert_ne!(lifted(&draft), container_id);
    assert_eq!(draft.handle(&lifted(&draft)), Some(container));
    assert!(correspondence.entries.contains(&Entry::Retained {
        handle: container,
        from: container_id,
        to: lifted(&draft),
    }));
    assert!(correspondence.entries.contains(&Entry::Retained {
        handle: finish,
        from: finish_id,
        to: id(&draft, finish),
    }));

    // A rename in `main` follows the variable into the literal that takes it
    // as a hidden argument.
    let seed = main(&draft)[0];
    apply(
        &mut draft,
        vec![WorkflowEdit::RenameBinding {
            binding: WorkflowBindingRef::Variable {
                at: seed,
                name: "captured".into(),
            },
            name: "seed".into(),
        }],
    )
    .expect("the rename applies");
    assert_eq!(
        spelled(&draft),
        program(
            "seed",
            vec![b::print(b::string("started")), echo(b::var("input"))],
        )
    );
    assert_eq!(draft.handle(&lifted(&draft)), Some(container));
}

#[test]
fn correspondence_names_what_insert_move_clone_and_delete_did_to_each_node() {
    let mut draft = open(&b::program(vec![
        b::assign("a", b::num(1.0)),
        b::assign("b", b::num(2.0)),
        echo(b::var("a")),
        echo(b::var("b")),
    ]));
    let opened = main(&draft);
    let [first, second, third, fourth] = opened[..] else {
        panic!("four statements");
    };
    let opened_ids = opened
        .iter()
        .map(|handle| id(&draft, *handle))
        .collect::<Vec<_>>();

    // Insert before: every later statement keeps its handle and changes id.
    let inserted = apply(
        &mut draft,
        vec![WorkflowEdit::InsertNode {
            body: WorkflowBodyRef::Main,
            before: Some(first),
            statement: b::assign("z", b::num(0.0)),
        }],
    )
    .expect("the insert applies");
    let new = main(&draft)[0];
    assert_eq!(main(&draft), [new, first, second, third, fourth]);
    let mut expected = opened
        .iter()
        .zip(&opened_ids)
        .map(|(handle, from)| Entry::Retained {
            handle: *handle,
            from: from.clone(),
            to: id(&draft, *handle),
        })
        .collect::<Vec<_>>();
    expected.push(Entry::Inserted {
        handle: new,
        to: id(&draft, new),
        source: WorkflowNodeSource::Authored,
    });
    assert_eq!(inserted.entries, expected);
    assert!(
        opened_ids
            .iter()
            .all(|from| inserted.successor(from) != Some(from))
    );

    // Move.
    let before = id(&draft, fourth);
    let moved = apply(
        &mut draft,
        vec![WorkflowEdit::MoveNode {
            node: fourth,
            body: WorkflowBodyRef::Main,
            before: Some(third),
        }],
    )
    .expect("the move applies");
    assert_eq!(main(&draft), [new, first, second, fourth, third]);
    assert!(moved.entries.contains(&Entry::Moved {
        handle: fourth,
        from: before,
        to: id(&draft, fourth),
    }));

    // Clone: a distinct identity, with its source named.
    let cloned = apply(
        &mut draft,
        vec![WorkflowEdit::CloneNode {
            node: third,
            body: WorkflowBodyRef::Main,
            before: None,
        }],
    )
    .expect("the clone applies");
    let copy = main(&draft)[5];
    assert_ne!(copy, third);
    assert!(cloned.entries.contains(&Entry::Inserted {
        handle: copy,
        to: id(&draft, copy),
        source: WorkflowNodeSource::Clone { of: third },
    }));

    // Delete.
    let before = id(&draft, third);
    let deleted = apply(&mut draft, vec![WorkflowEdit::RemoveNode { node: third }])
        .expect("the removal applies");
    assert!(deleted.entries.contains(&Entry::Deleted {
        handle: third,
        from: before,
    }));

    // From the opened document to now, through all four transactions.
    let [first_id, second_id, third_id, fourth_id] = &opened_ids[..] else {
        panic!("four ids");
    };
    assert_eq!(
        draft.correspondence_since_open().entries,
        vec![
            Entry::Retained {
                handle: first,
                from: first_id.clone(),
                to: id(&draft, first),
            },
            Entry::Retained {
                handle: second,
                from: second_id.clone(),
                to: id(&draft, second),
            },
            Entry::Deleted {
                handle: third,
                from: third_id.clone(),
            },
            Entry::Moved {
                handle: fourth,
                from: fourth_id.clone(),
                to: id(&draft, fourth),
            },
            Entry::Inserted {
                handle: new,
                to: id(&draft, new),
                source: WorkflowNodeSource::Authored,
            },
            Entry::Inserted {
                handle: copy,
                to: id(&draft, copy),
                source: WorkflowNodeSource::Clone { of: third },
            },
        ]
    );
    assert_eq!(draft.opened_handle(fourth_id), Some(fourth));
    assert_eq!(draft.opened_handle(third_id), None);
}

#[test]
fn correspondence_follows_a_node_through_normalization() {
    let mut draft = open(&b::program(vec![
        b::print(b::string("one")),
        b::print(b::string("two")),
    ]));
    let [listed, branched] = main(&draft)[..] else {
        panic!("two statements");
    };
    let listed_id = id(&draft, listed);
    let correspondence = apply(
        &mut draft,
        vec![
            // A statement that is itself a statement list is listed as its
            // statements.
            WorkflowEdit::ReplaceNode {
                node: listed,
                statement: completion(vec![b::print(b::string("a")), b::print(b::string("b"))]),
            },
            // A statement that owns a body becomes a container of it.
            WorkflowEdit::ReplaceNode {
                node: branched,
                statement: b::if_else(
                    b::bool_lit(true),
                    b::block(vec![b::print(b::string("then"))]),
                    b::block(Vec::new()),
                ),
            },
        ],
    )
    .expect("the replacements apply");
    let [a, second, container] = main(&draft)[..] else {
        panic!("the list became two statements beside the container");
    };
    assert_eq!(container, branched);
    assert!(correspondence.entries.contains(&Entry::Split {
        handle: listed,
        from: listed_id,
        into: vec![(a, id(&draft, a)), (second, id(&draft, second))],
    }));
    let [then] = child(&draft, branched, WorkflowBodySlot::Then)[..] else {
        panic!("one statement in the branch");
    };
    assert!(correspondence.entries.contains(&Entry::Inserted {
        handle: then,
        to: id(&draft, then),
        source: WorkflowNodeSource::Derived { from: branched },
    }));
}

#[test]
fn an_edit_shifts_the_completion_groups_around_it() {
    // The second statement is a list closed by its own completion value: a
    // group the body's form names by position.
    let valued = |statements: Vec<Expr>| {
        let mut items = statements;
        items.push(b::string("value"));
        b::role(StructuralRole::Completion, b::block(items))
    };
    let mut draft = open(&b::program(vec![
        b::print(b::string("first")),
        valued(vec![b::print(b::string("grouped"))]),
        b::print(b::string("last")),
    ]));
    let [first, grouped, _] = main(&draft)[..] else {
        panic!("three statements");
    };
    apply(
        &mut draft,
        vec![
            WorkflowEdit::InsertNode {
                body: WorkflowBodyRef::Main,
                before: Some(first),
                statement: b::print(b::string("new")),
            },
            WorkflowEdit::RemoveNode { node: first },
        ],
    )
    .expect("the edits around the group apply");
    assert_eq!(
        spelled(&draft),
        b::program(vec![
            b::print(b::string("new")),
            valued(vec![b::print(b::string("grouped"))]),
            b::print(b::string("last")),
        ])
    );

    // Removing the grouped statement keeps the value its list closes with.
    apply(&mut draft, vec![WorkflowEdit::RemoveNode { node: grouped }])
        .expect("the removal applies");
    assert_eq!(
        spelled(&draft),
        b::program(vec![
            b::print(b::string("new")),
            valued(Vec::new()),
            b::print(b::string("last")),
        ])
    );
}

#[test]
fn an_edge_drag_is_a_typed_edit_and_no_order_is_a_cycle() {
    let mut draft = open(&b::program(vec![
        b::assign("a", echo(b::string("x"))),
        b::for_in(
            "item",
            b::list(vec![b::num(1.0)]),
            b::block(vec![b::print(b::var("item"))]),
        ),
        echo(b::string("y")),
    ]));
    let [producer, each, consumer] = main(&draft)[..] else {
        panic!("three statements");
    };

    // A data drag is a use of the producer's binding in the dragged slot.
    let connect = draft
        .edit_for_edge_drag(&WorkflowEdgeDrag::Data {
            from: producer,
            to: consumer,
            slot: echoed(),
        })
        .expect("the producer binds its value");
    assert_eq!(
        connect,
        WorkflowEdit::ReplaceExpression {
            node: consumer,
            slot: echoed(),
            expression: b::var("a"),
        }
    );
    apply(&mut draft, vec![connect]).expect("the use applies");
    assert!(
        draft
            .document()
            .main
            .edges
            .iter()
            .any(|edge| { edge.from == id(&draft, producer) && edge.to == id(&draft, consumer) })
    );

    // A sequence drag is a move, checked like any other: the producer cannot
    // be ordered after its consumer.
    let reorder = draft
        .edit_for_edge_drag(&WorkflowEdgeDrag::Sequence {
            from: consumer,
            to: producer,
        })
        .expect("both nodes are in the draft");
    assert_eq!(
        reorder,
        WorkflowEdit::MoveNode {
            node: producer,
            body: WorkflowBodyRef::Main,
            before: None,
        }
    );
    let (_, _, kind) = refused(&mut draft, vec![reorder]);
    assert_eq!(kind, Kind::UnresolvedBinding { name: "a".into() });

    // A container after its own statement would be a cycle; repetition is the
    // loop it already is.
    let (edit, _, kind) = refused(
        &mut draft,
        vec![WorkflowEdit::MoveNode {
            node: each,
            body: WorkflowBodyRef::Child {
                node: each,
                slot: WorkflowBodySlot::LoopBody,
            },
            before: None,
        }],
    );
    assert_eq!(edit, Some(0));
    assert_eq!(kind, Kind::MoveIntoOwnSubtree);
}
