//! Laws of the edits. Each pins a named rule of
//! `docs/kernel/semantics.md`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_kernel_check::{Environment, RefusalReason};
use lash_kernel_doc::{
    Action, Annotations, Atom, Catch, Datum, Document, EffectName, ErrorDatum, Expr, Function,
    FunctionDefinition, FunctionId, FunctionName, FunctionRegistry, Handle, Integer, Label,
    Literal, Member, Name, Node, NodeAnnotation, NumberPolicy, Param, Rhs, Signature, Site, Stmt,
    Timestamp, Type, Unit, parse_definition, parse_document, validate_annotations,
};

use lash_kernel_vm::{
    Bounds, End, Host, KernelMachine, Machine, Outcome, Program, Request, Start, Step, Target,
};

use crate::apply::Working;
use crate::tree::NodeMut;
use crate::{
    Correspondence, Draft, Edit, EditDiagnostic, EditDiagnosticKind, EditRefusal, Location,
    NodeClass, Position, Transaction,
};

/// The library the laws run against: a native function, the corrected
/// definition that replaces it, and a function the environment lacks.
struct Library {
    catalog: BTreeMap<FunctionId, FunctionDefinition>,
    neg: FunctionId,
    corrected: FunctionId,
    absent: FunctionId,
}

fn library() -> Library {
    let mut catalog = BTreeMap::new();
    let mut register = |text: &str, held: bool| {
        let definition = parse_definition(text).unwrap_or_else(|error| panic!("{error}\n{text}"));
        let function = definition.identity().unwrap();
        if held {
            catalog.insert(function, definition);
        }
        function
    };
    let neg = register(
        "function num.neg(x: Number) -> Number\nkernel 1\ncharge 1\nnative\n",
        true,
    );
    let corrected = register(
        "function num.neg(x: Number) -> Number\nkernel 1\ncharge 2\nnative\n",
        true,
    );
    let absent = register(
        "function num.neg(x: Number) -> Number\nkernel 1\ncharge 3\nnative\n",
        false,
    );
    Library {
        catalog,
        neg,
        corrected,
        absent,
    }
}

fn document(text: &str) -> Document {
    parse_document(text).unwrap_or_else(|error| panic!("{error}\n{text}"))
}

fn name(name: &str) -> Name {
    Name::new(name)
}

fn effect(name: &str) -> EffectName {
    EffectName::new(name).unwrap()
}

fn text_to_any(param: &str) -> Signature {
    Signature {
        params: vec![Param {
            name: name(param),
            ty: Type::Text,
            optional: false,
        }],
        result: Type::Any,
    }
}

/// An environment that provides `fetch(url: Text) -> Any`,
/// `notify(text: Text) -> Any` and the library.
fn environment(library: &Library) -> Environment<'_> {
    let mut environment = Environment::new(&library.catalog);
    environment
        .effects
        .insert(effect("fetch"), text_to_any("url"));
    environment
        .effects
        .insert(effect("notify"), text_to_any("text"));
    environment
}

fn main(path: &[u32]) -> Site {
    Site::new(Unit::Main, path)
}

fn function(function: &str, path: &[u32]) -> Site {
    Site::new(Unit::Function(name(function)), path)
}

fn text(text: &str) -> Expr {
    Expr::Literal(Literal::Text(text.to_string()))
}

fn print(value: &str) -> Stmt {
    Stmt::Print { value: text(value) }
}

fn before(site: Site) -> Position {
    Position::before(site).unwrap()
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
    walk(Node::Block(&document.main), main(&[]), &mut out);
    for (name, function) in &document.functions {
        let site = Site::new(Unit::Function(name.clone()), []);
        walk(Node::Block(&function.body), site, &mut out);
    }
    out.sort();
    out
}

fn open(text: &str) -> Draft {
    Draft::open(document(text), None).unwrap()
}

fn transaction(draft: &Draft, edits: Vec<Edit>) -> Transaction {
    Transaction {
        base: draft.identity(),
        edits,
    }
}

fn refused(draft: &mut Draft, edits: Vec<Edit>, environment: &Environment<'_>) -> EditRefusal {
    let held = (draft.document().clone(), draft.annotations().clone());
    let refusal = draft
        .apply(&transaction(draft, edits), environment)
        .expect_err("the transaction was published");
    // `K-EDIT-001`: a refused transaction changes nothing.
    assert_eq!(
        held,
        (draft.document().clone(), draft.annotations().clone())
    );
    refusal
}

/// What a run did that its host saw.
#[derive(Debug, PartialEq)]
struct Ran {
    /// Each effect performed, with its arguments, in request order.
    effects: Vec<(EffectName, Vec<Datum>)>,
    printed: Vec<Datum>,
    result: Datum,
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

/// Runs `document`'s `main` on the kernel machine to its end, answering
/// every effect with `answer`.
fn run(document: &Document, answer: &Datum) -> Ran {
    let program = Program {
        document: Arc::new(document.clone()),
        library: lash_kernel_vm::PreparedLibrary::new(Arc::new(FunctionRegistry::new())),
    };
    let bounds = Bounds {
        charge: 1_000_000,
        memory: 1 << 20,
        call_depth: 16,
        live_tasks: 4,
        requests_per_park: 4,
        join_members: 4,
    };
    let start = Start {
        target: Target::Main,
        args: Vec::new(),
        bindings: Default::default(),
    };
    let mut machine = KernelMachine::start(program, bounds, start).unwrap();
    let mut host = Printer::default();
    let mut effects = Vec::new();
    loop {
        match machine.run(&mut host, u64::MAX).unwrap() {
            Step::Parked(park) => {
                for request in park.requests {
                    let Request::Effect(request) = request else {
                        panic!("the document sleeps");
                    };
                    effects.push((request.effect, request.args));
                    machine
                        .deliver(request.wait, Outcome::Completed(answer.clone()))
                        .unwrap();
                }
            }
            Step::Slice => {}
            Step::Ended(End::Finished(finished)) => {
                return Ran {
                    effects,
                    printed: host.printed,
                    result: finished.result,
                };
            }
            Step::Ended(end) => panic!("the run ended with {end:?}"),
        }
    }
}

/// A document in the shape a front end emits: every operand of a
/// statement-position form is hoisted to a temporary first (`K-STMT-005`).
const LOWERED: &str = r#"kernel 1
numbers float
effect fetch(url: Text) -> Any
effect notify(text: Text) -> Any

main {
  let t1 = "https://a"
  let page = perform fetch(t1) as Any
  let t2 = page.ok
  if t2 {
    let t3 = "done"
    do perform notify(t3) as Any
  }
  finish page
}
"#;

/// Gate 3 of the design (§11), `K-EDIT-001` to `K-EDIT-003`: a host that
/// links no dialect changes an effect argument, inserts a statement and
/// replaces a condition with typed edits alone, and the document it gets
/// admits, runs on the kernel machine and does what the edits say.
#[test]
fn a_host_with_no_dialect_changes_an_argument_inserts_a_statement_and_replaces_a_condition() {
    let library = library();
    let environment = environment(&library);
    let mut draft = open(LOWERED);
    let quoted = |text: &str| Datum::Text(text.to_string());
    let page = Datum::Record(vec![
        ("ok".to_string(), Datum::Bool(true)),
        ("ready".to_string(), Datum::Bool(false)),
    ]);
    // As lowered: it fetches `a` and, the page being ok, notifies.
    assert_eq!(
        run(draft.document(), &page),
        Ran {
            effects: vec![
                (effect("fetch"), vec![quoted("https://a")]),
                (effect("notify"), vec![quoted("done")]),
            ],
            printed: vec![],
            result: page.clone(),
        }
    );

    let edits = vec![
        Edit::SetArgument {
            action: main(&[1, 0]),
            index: 0,
            argument: Atom::Literal(Literal::Text("https://b".to_string())),
        },
        Edit::InsertStatement {
            at: before(main(&[3])),
            statement: Stmt::Print {
                value: Expr::Variable(name("page")),
            },
        },
        Edit::SetCondition {
            statement: main(&[3]),
            condition: Expr::Member(Box::new(Member::Field {
                target: Expr::Variable(name("page")),
                field: "ready".to_string(),
            })),
        },
    ];
    let applied = draft
        .apply(&transaction(&draft, edits), &environment)
        .unwrap();

    let expected = document(
        r#"kernel 1
numbers float
effect fetch(url: Text) -> Any
effect notify(text: Text) -> Any

main {
  let t1 = "https://a"
  let page = perform fetch("https://b") as Any
  let t2 = page.ok
  print page
  if page.ready {
    let t3 = "done"
    do perform notify(t3) as Any
  }
  finish page
}
"#,
    );
    assert_eq!(draft.document(), &expected);
    assert_eq!(applied.admitted.identity, expected.identity().unwrap());
    assert_eq!(draft.identity(), applied.admitted.identity);
    // As edited: it fetches `b`, prints the page and, the page not being
    // ready, does not notify.
    assert_eq!(
        run(draft.document(), &page),
        Ran {
            effects: vec![(effect("fetch"), vec![quoted("https://b")])],
            printed: vec![page.clone()],
            result: page,
        }
    );
    // The `perform` a parked run would be waiting at is the same node.
    let perform = applied.correspondence.survivor(&main(&[1, 0])).unwrap();
    assert_eq!((&perform.to, perform.edited), (&main(&[1, 0]), true));
    // The `if` moved down by the inserted statement.
    assert_eq!(
        applied.correspondence.successor(&main(&[3])),
        Some(&main(&[4]))
    );
}

/// The document the correspondence law edits: every kind of node an edit
/// names.
fn subject(library: &Library) -> String {
    format!(
        r#"kernel 1
numbers float
effect fetch(url: Text) -> Any
use num.neg = @{neg}
entry start(n: Int) -> Any

fn start(n) {{
  return n
}}

fn helper(n) {{
  return n
}}

main {{
  let a = num.neg(1)
  if a {{
    print "then"
  }}
  let b = [a, 2]
  try {{
    print b
  }} catch e {{
    print e
  }}
  let r = perform fetch("u") as Any
  let k = call start(a)
  finish r
}}
"#,
        neg = library.neg,
    )
}

/// What a case expects of the nodes under a site of the base.
enum Fate {
    /// The node and everything under it is gone.
    Gone(Site),
    /// The node and everything under it is at this site now.
    Moved(Site, Site),
}

fn fate(rules: &[Fate], site: &Site) -> Option<Site> {
    let under = |prefix: &Site| site.unit == prefix.unit && site.path.starts_with(&prefix.path);
    for rule in rules {
        match rule {
            Fate::Gone(prefix) if under(prefix) => return None,
            Fate::Moved(from, to) if under(from) => {
                let mut path = to.path.clone();
                path.extend(&site.path[from.path.len()..]);
                return Some(Site::new(to.unit.clone(), path));
            }
            _ => {}
        }
    }
    Some(site.clone())
}

/// `main`'s statements `first..=last` each move by `by`.
fn shifted(first: u32, last: u32, by: i32) -> Vec<Fate> {
    (first..=last)
        .map(|index| {
            Fate::Moved(
                main(&[index]),
                main(&[index.checked_add_signed(by).unwrap()]),
            )
        })
        .collect()
}

struct Case {
    edit: Edit,
    /// The first rule a site is under decides it; a site under none stays
    /// where it is.
    fates: Vec<Fate>,
    /// The base sites of the nodes an edit wrote.
    edited: Vec<Site>,
}

fn case(edit: Edit, fates: Vec<Fate>, edited: &[Site]) -> Case {
    Case {
        edit,
        fates,
        edited: edited.to_vec(),
    }
}

fn cases(library: &Library) -> Vec<Case> {
    let any = Signature {
        params: vec![Param {
            name: name("n"),
            ty: Type::Any,
            optional: false,
        }],
        result: Type::Any,
    };
    let returns = |value: &str| Function {
        params: vec![name("n")],
        body: vec![Stmt::Return { value: text(value) }],
    };
    let mut removed = vec![Fate::Gone(main(&[1]))];
    removed.extend(shifted(2, 6, -1));
    let mut move_into_try = vec![Fate::Moved(main(&[1]), main(&[2, 0, 1]))];
    move_into_try.extend(shifted(2, 6, -1));
    vec![
        case(
            Edit::InsertStatement {
                at: before(main(&[1])),
                statement: print("new"),
            },
            shifted(1, 6, 1),
            &[],
        ),
        case(
            Edit::RemoveStatement {
                statement: main(&[1]),
            },
            removed,
            &[],
        ),
        // Into a block that the removal itself shifts.
        case(
            Edit::MoveStatement {
                statement: main(&[1]),
                to: Position::end(main(&[3, 0])),
            },
            move_into_try,
            &[],
        ),
        case(
            Edit::CloneStatement {
                statement: main(&[1]),
                to: before(main(&[2])),
            },
            shifted(2, 6, 1),
            &[],
        ),
        case(
            Edit::ReplaceStatement {
                statement: main(&[1]),
                with: print("replaced"),
            },
            vec![
                Fate::Gone(main(&[1, 0])),
                Fate::Gone(main(&[1, 1])),
                Fate::Gone(main(&[1, 2])),
            ],
            &[main(&[1])],
        ),
        case(
            Edit::ReplaceExpression {
                expression: main(&[2, 0]),
                with: Expr::List(vec![text("only")]),
            },
            vec![Fate::Gone(main(&[2, 0, 0])), Fate::Gone(main(&[2, 0, 1]))],
            &[main(&[2, 0])],
        ),
        case(
            Edit::ReplaceAction {
                action: main(&[4, 0]),
                with: Action::Perform {
                    effect: effect("fetch"),
                    args: vec![Atom::Literal(Literal::Text("w".to_string()))],
                    result: Type::Any,
                },
            },
            vec![],
            &[main(&[4, 0])],
        ),
        case(
            Edit::SetArgument {
                action: main(&[4, 0]),
                index: 0,
                argument: Atom::Literal(Literal::Text("v".to_string())),
            },
            vec![],
            &[main(&[4, 0])],
        ),
        case(
            Edit::SetCondition {
                statement: main(&[1]),
                condition: Expr::Literal(Literal::Bool(true)),
            },
            vec![],
            &[main(&[1, 0])],
        ),
        case(
            Edit::SetCatch {
                statement: main(&[3]),
                catch: Some(Catch {
                    binding: name("error"),
                    body: vec![print("caught")],
                }),
            },
            vec![Fate::Gone(main(&[3, 1]))],
            &[main(&[3])],
        ),
        case(
            Edit::SetFinally {
                statement: main(&[3]),
                finally: Some(vec![print("after")]),
            },
            vec![],
            &[main(&[3])],
        ),
        case(
            Edit::RenameVariable {
                declared_at: main(&[2]),
                name: name("b"),
                to: name("c"),
            },
            vec![],
            &[main(&[2]), main(&[3, 0, 0, 0])],
        ),
        case(
            Edit::SetLabel {
                node: main(&[1]),
                label: Some(Label {
                    title: "Check".to_string(),
                    description: None,
                }),
            },
            vec![],
            &[],
        ),
        case(
            Edit::SetData {
                node: main(&[1]),
                key: "layout".to_string(),
                value: Some(serde_json::json!({"x": 1})),
            },
            vec![],
            &[],
        ),
        case(
            Edit::InsertFunction {
                name: name("extra"),
                function: returns("extra"),
            },
            vec![],
            &[],
        ),
        case(
            Edit::RemoveFunction {
                name: name("helper"),
            },
            vec![Fate::Gone(function("helper", &[]))],
            &[],
        ),
        case(
            Edit::ReplaceFunction {
                name: name("helper"),
                function: returns("replaced"),
            },
            vec![Fate::Gone(function("helper", &[0]))],
            &[function("helper", &[])],
        ),
        case(
            Edit::RenameFunction {
                from: name("helper"),
                to: name("assistant"),
            },
            vec![Fate::Moved(
                function("helper", &[]),
                function("assistant", &[]),
            )],
            &[],
        ),
        case(
            Edit::SetPrivateBindings {
                names: BTreeSet::from([name("a")]),
            },
            vec![],
            &[],
        ),
        case(
            Edit::InsertEntry {
                function: name("helper"),
                signature: any.clone(),
            },
            vec![],
            &[],
        ),
        case(
            Edit::RemoveEntry {
                function: name("start"),
            },
            vec![],
            &[],
        ),
        // The call of the entry is written; its body moves to the new unit.
        case(
            Edit::RenameEntry {
                from: name("start"),
                to: name("begin"),
            },
            vec![Fate::Moved(function("start", &[]), function("begin", &[]))],
            &[main(&[5, 0])],
        ),
        case(
            Edit::SetEntrySignature {
                function: name("start"),
                signature: any,
            },
            vec![],
            &[],
        ),
        case(
            Edit::SetEffectSignature {
                effect: effect("fetch"),
                signature: text_to_any("address"),
            },
            vec![],
            &[],
        ),
        case(
            Edit::SetNumberPolicy {
                numbers: NumberPolicy::BySpelling,
            },
            vec![],
            &[],
        ),
        case(
            Edit::ReplaceFunctionIdentity {
                from: library.neg,
                to: library.corrected,
            },
            vec![],
            &[main(&[0, 0])],
        ),
    ]
}

/// The edit's kind. The match is exhaustive, so a new edit kind does not
/// compile until it is named here and given a case.
fn kind(edit: &Edit) -> &'static str {
    match edit {
        Edit::InsertStatement { .. } => "insert_statement",
        Edit::RemoveStatement { .. } => "remove_statement",
        Edit::MoveStatement { .. } => "move_statement",
        Edit::CloneStatement { .. } => "clone_statement",
        Edit::ReplaceStatement { .. } => "replace_statement",
        Edit::ReplaceExpression { .. } => "replace_expression",
        Edit::ReplaceAction { .. } => "replace_action",
        Edit::SetArgument { .. } => "set_argument",
        Edit::SetCondition { .. } => "set_condition",
        Edit::SetCatch { .. } => "set_catch",
        Edit::SetFinally { .. } => "set_finally",
        Edit::RenameVariable { .. } => "rename_variable",
        Edit::SetLabel { .. } => "set_label",
        Edit::SetData { .. } => "set_data",
        Edit::InsertFunction { .. } => "insert_function",
        Edit::RemoveFunction { .. } => "remove_function",
        Edit::ReplaceFunction { .. } => "replace_function",
        Edit::RenameFunction { .. } => "rename_function",
        Edit::SetPrivateBindings { .. } => "set_private_bindings",
        Edit::InsertEntry { .. } => "insert_entry",
        Edit::RemoveEntry { .. } => "remove_entry",
        Edit::RenameEntry { .. } => "rename_entry",
        Edit::SetEntrySignature { .. } => "set_entry_signature",
        Edit::SetEffectSignature { .. } => "set_effect_signature",
        Edit::SetNumberPolicy { .. } => "set_number_policy",
        Edit::ReplaceFunctionIdentity { .. } => "replace_function_identity",
    }
}

const EDIT_KINDS: usize = 26;

/// `K-EDIT-004`: after each kind of edit, every node of the base that
/// survives maps to the node it became and to no other, a node that does
/// not survive maps to nothing, and the nodes marked edited are the ones
/// the edit wrote.
#[test]
fn after_each_edit_kind_every_surviving_node_maps_to_its_successor_and_no_other() {
    let library = library();
    let environment = environment(&library);
    let subject = subject(&library);
    let base = document(&subject);
    let cases = cases(&library);
    let kinds: BTreeSet<&str> = cases.iter().map(|case| kind(&case.edit)).collect();
    assert_eq!(kinds.len(), EDIT_KINDS, "an edit kind has no case");

    for case in cases {
        let kind = kind(&case.edit);
        let mut draft = open(&subject);
        let applied = draft
            .apply(&transaction(&draft, vec![case.edit]), &environment)
            .unwrap_or_else(|refusal| panic!("{kind}: {refusal}"));

        let expected: Vec<(Site, Site, bool)> = sites(&base)
            .into_iter()
            .filter_map(|site| {
                let to = fate(&case.fates, &site)?;
                let edited = case.edited.contains(&site);
                Some((site, to, edited))
            })
            .collect();
        let found: Vec<(Site, Site, bool)> = applied
            .correspondence
            .entries()
            .iter()
            .map(|entry| (entry.from.clone(), entry.to.clone(), entry.edited))
            .collect();
        assert_eq!(found, expected, "{kind}");

        // Every successor is a node of the result, and no two nodes share
        // one.
        let result = sites(draft.document());
        let successors: BTreeSet<&Site> = found.iter().map(|(_, to, _)| to).collect();
        assert_eq!(successors.len(), found.len(), "{kind}");
        assert!(successors.iter().all(|to| result.contains(to)), "{kind}");
        assert_eq!(applied.correspondence.base, base.identity().unwrap());
        assert_eq!(applied.correspondence.result, draft.identity(), "{kind}");
    }
}

/// `K-EDIT-002`: a statement moved later in its own block lands ahead of
/// the statement the position named, counted after it is taken out; a
/// statement cannot move into itself.
#[test]
fn a_move_lands_where_its_position_says_and_never_inside_itself() {
    let library = library();
    let environment = environment(&library);
    let mut draft = open(&subject(&library));
    let applied = draft
        .apply(
            &transaction(
                &draft,
                vec![Edit::MoveStatement {
                    statement: main(&[1]),
                    to: before(main(&[4])),
                }],
            ),
            &environment,
        )
        .unwrap();
    let successor = |index: u32| applied.correspondence.successor(&main(&[index])).cloned();
    assert_eq!(
        [1, 2, 3, 4].map(successor),
        [3, 1, 2, 4].map(|index| Some(main(&[index])))
    );

    let refusal = refused(
        &mut draft,
        vec![Edit::MoveStatement {
            statement: main(&[2]),
            to: Position::end(main(&[2, 0])),
        }],
        &environment,
    );
    assert_eq!(
        refusal.diagnostics,
        vec![EditDiagnostic {
            edit: Some(0),
            location: Some(Location::Base(main(&[2]))),
            kind: EditDiagnosticKind::IntoItself,
        }]
    );
}

/// `K-EDIT-010`: a transaction that holds every kind of edit, and the
/// correspondence it publishes, are the same values after a trip through
/// JSON; an unknown member is refused.
#[test]
fn a_transaction_and_its_correspondence_round_trip_through_json() {
    let library = library();
    let environment = environment(&library);
    let draft = open(&subject(&library));
    let transaction = transaction(
        &draft,
        cases(&library).into_iter().map(|case| case.edit).collect(),
    );
    let json = transaction.to_json().unwrap();
    assert_eq!(Transaction::from_json(&json).unwrap(), transaction);
    // Clearing a clause is written by leaving it out.
    let cleared = Edit::SetCatch {
        statement: main(&[3]),
        catch: None,
    };
    let json = serde_json::to_string(&cleared).unwrap();
    assert_eq!(
        json,
        r#"{"set_catch":{"statement":{"unit":"main","path":[3]}}}"#
    );
    assert_eq!(serde_json::from_str::<Edit>(&json).unwrap(), cleared);

    let unknown = r#"{"remove_statement":{"statement":{"unit":"main","path":[3]},"force":true}}"#;
    assert!(serde_json::from_str::<Edit>(unknown).is_err());
    let unknown = format!(r#"{{"base":"{}","edits":[],"note":1}}"#, draft.identity());
    assert!(Transaction::from_json(&unknown).is_err());

    let mut edited = draft.clone();
    let applied = edited
        .apply(
            &Transaction {
                base: draft.identity(),
                edits: vec![Edit::RemoveStatement {
                    statement: main(&[1]),
                }],
            },
            &environment,
        )
        .unwrap();
    let json = serde_json::to_string(&applied.correspondence).unwrap();
    assert_eq!(
        serde_json::from_str::<Correspondence>(&json).unwrap(),
        applied.correspondence
    );
}

/// `K-EDIT-003`: a hoisted temporary is an ordinary variable. An edit that
/// leaves one used before it is bound is refused by the scope rule, naming
/// the use, in `main` and in a declared function alike.
#[test]
fn an_edit_that_leaves_a_hoisted_temporary_used_before_it_is_bound_is_refused() {
    let library = library();
    let environment = environment(&library);
    let unbound = |site: Site, variable: &str| EditDiagnostic {
        edit: None,
        location: Some(Location::Edited(site)),
        kind: EditDiagnosticKind::Refused(RefusalReason::UnboundVariable {
            name: name(variable),
        }),
    };

    // Removing the temporary's `let`.
    let mut draft = open(LOWERED);
    let refusal = refused(
        &mut draft,
        vec![Edit::RemoveStatement {
            statement: main(&[0]),
        }],
        &environment,
    );
    assert_eq!(refusal.diagnostics, vec![unbound(main(&[0, 0]), "t1")]);

    // Moving the use ahead of the `let`.
    let refusal = refused(
        &mut draft,
        vec![Edit::MoveStatement {
            statement: main(&[1]),
            to: before(main(&[0])),
        }],
        &environment,
    );
    assert_eq!(refusal.diagnostics, vec![unbound(main(&[0, 0]), "t1")]);

    // Moving a statement out of the block that binds what it uses.
    let refusal = refused(
        &mut draft,
        vec![Edit::MoveStatement {
            statement: main(&[3, 1, 1]),
            to: Position::end(main(&[])),
        }],
        &environment,
    );
    assert_eq!(refusal.diagnostics, vec![unbound(main(&[5, 0]), "t3")]);

    let mut draft = open(
        r#"kernel 1
numbers float
effect fetch(url: Text) -> Any

fn load(base) {
  let t1 = [base, "/x"]
  let t2 = t1[0]
  let page = perform fetch(t2) as Any
  return page
}

main {
  let got = call load("u")
  finish got
}
"#,
    );
    let refusal = refused(
        &mut draft,
        vec![Edit::RemoveStatement {
            statement: function("load", &[1]),
        }],
        &environment,
    );
    assert_eq!(
        refusal.diagnostics,
        vec![unbound(function("load", &[1, 0]), "t2")]
    );
}

/// `K-EDIT-001`, `K-EDIT-002`: a transaction applies whole or not at all,
/// a fault names its edit and the site the edit named, a later edit names a
/// node by its site in the base wherever an earlier edit put it, and a
/// transaction written against another document is refused.
#[test]
fn a_transaction_applies_whole_against_its_base_or_not_at_all() {
    let library = library();
    let environment = environment(&library);
    let mut draft = open(&subject(&library));
    let opened = draft.identity();

    let remove = |index: u32| Edit::RemoveStatement {
        statement: main(&[index]),
    };
    let fault = |edit: u32, site: Site, kind: EditDiagnosticKind| {
        vec![EditDiagnostic {
            edit: Some(edit),
            location: Some(Location::Base(site)),
            kind,
        }]
    };
    // The second edit names a node the first removed.
    let refusal = refused(
        &mut draft,
        vec![
            remove(1),
            Edit::SetCondition {
                statement: main(&[1]),
                condition: text("x"),
            },
        ],
        &environment,
    );
    assert_eq!(
        refusal.diagnostics,
        fault(1, main(&[1]), EditDiagnosticKind::NoSuchNode)
    );
    let refusal = refused(
        &mut draft,
        vec![Edit::ReplaceExpression {
            expression: main(&[1]),
            with: text("x"),
        }],
        &environment,
    );
    assert_eq!(
        refusal.diagnostics,
        fault(
            0,
            main(&[1]),
            EditDiagnosticKind::WrongNode {
                expected: NodeClass::Expression,
                found: NodeClass::Statement,
            }
        )
    );
    let refusal = refused(
        &mut draft,
        vec![Edit::SetArgument {
            action: main(&[4, 0]),
            index: 1,
            argument: Atom::Literal(Literal::Null),
        }],
        &environment,
    );
    assert_eq!(
        refusal.diagnostics,
        fault(
            0,
            main(&[4, 0]),
            EditDiagnosticKind::NoSuchArgument {
                index: 1,
                arguments: 1,
            }
        )
    );

    // Both edits name base sites: the second finds the `try` where the
    // first left it.
    let applied = draft
        .apply(
            &transaction(
                &draft,
                vec![
                    remove(1),
                    Edit::SetFinally {
                        statement: main(&[3]),
                        finally: Some(vec![print("after")]),
                    },
                ],
            ),
            &environment,
        )
        .unwrap();
    assert!(matches!(
        draft.document().main[2],
        Stmt::Try(ref scope) if scope.finally.is_some()
    ));

    let stale = Transaction {
        base: opened,
        edits: vec![remove(0)],
    };
    let held = draft.document().clone();
    let refusal = draft.apply(&stale, &environment).unwrap_err();
    assert_eq!(
        refusal.diagnostics,
        vec![EditDiagnostic {
            edit: None,
            location: None,
            kind: EditDiagnosticKind::StaleBase {
                base: opened,
                current: applied.admitted.identity,
            },
        }]
    );
    assert_eq!(draft.document(), &held);

    // The draft's own correspondence composes the transactions since it
    // opened.
    let second = draft
        .apply(&transaction(&draft, vec![remove(4)]), &environment)
        .unwrap();
    let since_open = draft.correspondence_since_open();
    assert_eq!(
        (since_open.base, since_open.result),
        (opened, draft.identity())
    );
    assert_eq!(
        Some(since_open),
        applied.correspondence.then(&second.correspondence).as_ref()
    );
    assert_eq!(since_open.successor(&main(&[1])), None);
    assert_eq!(since_open.successor(&main(&[5])), None);
    let finish = since_open.survivor(&main(&[6])).unwrap();
    assert_eq!((&finish.to, finish.edited), (&main(&[4]), false));
    assert_eq!(since_open.predecessor(&main(&[2])), Some(&main(&[3])));
    assert!(since_open.survivor(&main(&[3])).unwrap().edited);
}

/// `K-EDIT-006`: annotations move with their node, a copy carries them, a
/// removed node's are dropped, the layer is keyed to the new identity, and
/// authored source is kept only while the document behaves the same.
#[test]
fn annotations_move_with_their_node() {
    let library = library();
    let environment = environment(&library);
    let base = document(&subject(&library));
    let label = |title: &str| {
        Some(Label {
            title: title.to_string(),
            description: None,
        })
    };
    let annotation = |site: Site, title: &str| NodeAnnotation {
        site,
        label: label(title),
        data: BTreeMap::new(),
    };
    let annotations = Annotations {
        document: base.identity().unwrap(),
        dialect: Some("typescript".to_string()),
        source: Some("authored".to_string()),
        nodes: vec![
            annotation(main(&[1]), "Check"),
            annotation(main(&[1, 1, 0]), "Say"),
            annotation(main(&[5]), "Call"),
        ],
    };
    let mut draft = Draft::open(base, Some(annotations)).unwrap();

    // Annotation edits leave behaviour, and so the authored source, alone.
    let relabel = Edit::SetLabel {
        node: main(&[5]),
        label: label("Start"),
    };
    let applied = draft
        .apply(&transaction(&draft, vec![relabel]), &environment)
        .unwrap();
    assert_eq!(applied.correspondence.base, applied.correspondence.result);
    assert_eq!(draft.annotations().source.as_deref(), Some("authored"));

    let edits = vec![
        Edit::CloneStatement {
            statement: main(&[1]),
            to: Position::end(main(&[3, 0])),
        },
        Edit::MoveStatement {
            statement: main(&[1]),
            to: before(main(&[3])),
        },
        Edit::RemoveStatement {
            statement: main(&[5]),
        },
        Edit::SetData {
            node: main(&[6]),
            key: "layout".to_string(),
            value: Some(serde_json::json!([1, 2])),
        },
    ];
    draft
        .apply(&transaction(&draft, edits), &environment)
        .unwrap();
    let found = draft.annotations();
    assert_eq!(
        found.nodes,
        vec![
            annotation(main(&[2]), "Check"),
            annotation(main(&[2, 1, 0]), "Say"),
            annotation(main(&[3, 0, 1]), "Check"),
            annotation(main(&[3, 0, 1, 1, 0]), "Say"),
            NodeAnnotation {
                site: main(&[5]),
                label: None,
                data: BTreeMap::from([("layout".to_string(), serde_json::json!([1, 2]))]),
            },
        ]
    );
    assert_eq!(found.document, draft.identity());
    assert_eq!(found.dialect.as_deref(), Some("typescript"));
    assert_eq!(found.source, None);
    validate_annotations(found, draft.document()).unwrap();
}

/// `K-EDIT-005`, `K-EDIT-009`: the manifest a draft publishes is the one the edited code
/// derives. An effect newly performed is listed with the environment's
/// signature, one no longer performed is dropped, an adopted function is
/// listed in place of the one it corrects, and a function the environment
/// lacks is refused by name.
#[test]
fn the_published_manifest_is_the_one_the_edited_code_derives() {
    let library = library();
    let environment = environment(&library);
    let mut draft = open(&subject(&library));
    let notify = Stmt::Do {
        action: Action::Perform {
            effect: effect("notify"),
            args: vec![Atom::Literal(Literal::Text("hi".to_string()))],
            result: Type::Any,
        },
    };
    let edits = vec![
        Edit::InsertStatement {
            at: before(main(&[6])),
            statement: notify,
        },
        Edit::ReplaceStatement {
            statement: main(&[4]),
            with: Stmt::Let {
                name: name("r"),
                value: Rhs::Expr(text("none")),
            },
        },
        Edit::ReplaceFunctionIdentity {
            from: library.neg,
            to: library.corrected,
        },
    ];
    draft
        .apply(&transaction(&draft, edits), &environment)
        .unwrap();
    let manifest = &draft.document().manifest;
    assert_eq!(
        manifest.effects,
        BTreeMap::from([(effect("notify"), text_to_any("text"))])
    );
    assert_eq!(
        manifest.functions.keys().collect::<Vec<_>>(),
        vec![&library.corrected]
    );

    let refusal = refused(
        &mut draft,
        vec![Edit::ReplaceFunctionIdentity {
            from: library.corrected,
            to: library.absent,
        }],
        &environment,
    );
    assert_eq!(
        refusal.diagnostics,
        vec![EditDiagnostic {
            edit: None,
            location: None,
            kind: EditDiagnosticKind::Refused(RefusalReason::MissingFunction {
                function: library.absent,
                name: FunctionName::new("num.neg").unwrap(),
            }),
        }]
    );
    let refusal = refused(
        &mut draft,
        vec![Edit::ReplaceFunctionIdentity {
            from: library.neg,
            to: library.corrected,
        }],
        &environment,
    );
    assert_eq!(
        refusal.diagnostics,
        vec![EditDiagnostic {
            edit: Some(0),
            location: None,
            kind: EditDiagnosticKind::FunctionNotCalled {
                function: library.neg,
            },
        }]
    );
}

/// `K-EDIT-007`: a rename reaches the declaration and every use that
/// resolves to it and no other variable of that name, keeps a private
/// binding private, and is refused when the new name would change what
/// some name refers to.
#[test]
fn a_rename_follows_one_declaration_and_refuses_capture() {
    let library = library();
    let environment = environment(&library);
    let source = r#"kernel 1
numbers float
private total

fn scale(n) {
  let twice = [n, n]
  return twice
}

main {
  let total = 1
  let other = 2
  let f = fn(total) {
    return total
  }
  if other {
    let inner = 3
    print [inner, total]
  }
  let h = spawn apply f(total)
  let got = join h
  finish [total, other]
}
"#;
    let mut draft = open(source);
    let edits = vec![
        Edit::RenameVariable {
            declared_at: main(&[0]),
            name: name("total"),
            to: name("sum"),
        },
        Edit::RenameVariable {
            declared_at: function("scale", &[]),
            name: name("n"),
            to: name("factor"),
        },
        Edit::RenameVariable {
            declared_at: main(&[2, 0]),
            name: name("total"),
            to: name("x"),
        },
    ];
    draft
        .apply(&transaction(&draft, edits), &environment)
        .unwrap();
    let expected = document(
        r#"kernel 1
numbers float
private sum

fn scale(factor) {
  let twice = [factor, factor]
  return twice
}

main {
  let sum = 1
  let other = 2
  let f = fn(x) {
    return x
  }
  if other {
    let inner = 3
    print [inner, sum]
  }
  let h = spawn apply f(sum)
  let got = join h
  finish [sum, other]
}
"#,
    );
    assert_eq!(draft.document(), &expected);

    let collides = |declared_at: Site, from: &str, to: &str| {
        vec![EditDiagnostic {
            edit: Some(0),
            location: Some(Location::Base(declared_at)),
            kind: EditDiagnosticKind::RenameCollides {
                from: name(from),
                to: name(to),
            },
        }]
    };
    // `other` would come to mean the renamed variable where it is read.
    let rename = |declared_at: Site, from: &str, to: &str| Edit::RenameVariable {
        declared_at,
        name: name(from),
        to: name(to),
    };
    let refusal = refused(
        &mut draft,
        vec![rename(main(&[1]), "other", "sum")],
        &environment,
    );
    assert_eq!(refusal.diagnostics, collides(main(&[1]), "other", "sum"));
    // The inner `inner` would hide the renamed variable from its own use.
    let refusal = refused(
        &mut draft,
        vec![rename(main(&[0]), "sum", "inner")],
        &environment,
    );
    assert_eq!(refusal.diagnostics, collides(main(&[0]), "sum", "inner"));
    let refusal = refused(
        &mut draft,
        vec![rename(main(&[1]), "missing", "y")],
        &environment,
    );
    assert_eq!(
        refusal.diagnostics,
        vec![EditDiagnostic {
            edit: Some(0),
            location: Some(Location::Base(main(&[1]))),
            kind: EditDiagnosticKind::NoSuchBinding {
                name: name("missing"),
            },
        }]
    );
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
    let mut document = document(source);
    let reading = document.clone();
    let all = sites(&reading);
    assert_eq!(
        Working::open(&reading, &Annotations::new(reading.identity().unwrap())).sites(),
        all
    );
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
