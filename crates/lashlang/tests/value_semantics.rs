//! Value-semantics probes taken verbatim from the FIG-1301 adversarial
//! re-reviews.
//!
//! Every case here is a program a reviewer wrote to break the heap
//! representation: a container that kept a live alias to a value stored
//! elsewhere, or a snapshot the encoder emitted outside the language its own
//! decoder accepts. They now assert ECMA reference semantics — a store
//! aliases, and a mutation is visible through every name that reaches the
//! object (ADR 0096) — and that an emitted snapshot always decodes. The
//! decoding half is what these probes were written to break, and it is
//! unchanged: sharing is exactly the shape the reported failures produced.

use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome, Expr, Program,
    Snapshot, State, Value, execute,
};

// `a::list`/`a::number` build IR nodes; the bare `list`/`number` below build
// the `Value`s the assertions compare against.
use crate::ast_support as a;

/// `push(<list>, <item>)` — the IR's non-mutating append.
fn push(list_expr: Expr, item: Expr) -> Expr {
    a::call("push", vec![list_expr, item])
}

#[derive(Default)]
struct ProbeHost;

impl ExecutionHost for ProbeHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Print(_) => Ok(AbilityResult::Unit),
            AbilityOp::Finish(value) | AbilityOp::Fail(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unsupported probe host ability")),
        }
    }
}

fn finished(outcome: ExecutionOutcome) -> Value {
    match outcome {
        ExecutionOutcome::Finished(value) => value,
        ExecutionOutcome::Continued => panic!("expected `finish`"),
        ExecutionOutcome::Failed(value) => panic!("unexpected failure: {value}"),
    }
}

/// The probes are built from the IR rather than authored: what they pin is the
/// heap's behaviour under aliasing and snapshotting, which is a property of the
/// IR and not of any dialect (ADR 0096). The program each one replaces is kept
/// verbatim as a comment above it.
#[expect(
    clippy::expect_used,
    reason = "the probe cell compiles and executes against the probe host, per each message"
)]
async fn run(state: &mut State, cell: Program) -> Value {
    let compiled = lashlang_compile_program(&cell).expect("probe cell should compile");
    finished(
        execute(&compiled, state, &ProbeHost)
            .await
            .expect("probe cell should execute"),
    )
}

/// Round-trips `state` through the canonical snapshot wire.
///
/// Both directions matter: the encoder must be able to emit the state, and the
/// decoder must accept what it emitted.
#[expect(
    clippy::expect_used,
    reason = "the emitted snapshot bytes decode by construction of the writer, per the message"
)]
fn round_trip(state: &State) -> State {
    let bytes = state
        .snapshot()
        .to_canonical_bytes()
        .expect("state should encode");
    let snapshot = Snapshot::from_canonical_bytes(&bytes).expect("emitted bytes should decode");
    State::from_snapshot(snapshot)
}

fn list(values: Vec<Value>) -> Value {
    Value::List(values.into())
}

fn number(value: f64) -> Value {
    Value::Number(value)
}

/// Sol probe 1: the optimized single-item concat across three cells.
///
/// `acc = acc + [x]` inserts the object `x` names, so a later mutation of `x`
/// is visible through `acc`, and the snapshot carrying both roots still
/// decodes.
#[tokio::test(flavor = "current_thread")]
async fn optimized_concat_insertion_shares_the_appended_binding() {
    let mut state = State::new();
    // x = [1]
    // acc = []
    // finish 0
    run(
        &mut state,
        a::program(vec![
            a::assign("x", a::list(vec![a::number(1.0)])),
            a::assign("acc", a::list(Vec::new())),
            a::finish(a::number(0.0)),
        ]),
    )
    .await;
    let mut state = round_trip(&state);
    // acc = acc + [x]
    // finish 0
    run(
        &mut state,
        a::program(vec![
            a::assign("acc", a::add(a::var("acc"), a::list(vec![a::var("x")]))),
            a::finish(a::number(0.0)),
        ]),
    )
    .await;
    let mut state = round_trip(&state);
    // x = push(x, 2)
    // finish acc
    let value = run(
        &mut state,
        a::program(vec![
            a::assign("x", push(a::var("x"), a::number(2.0))),
            a::finish(a::var("acc")),
        ]),
    )
    .await;

    assert_eq!(value, list(vec![list(vec![number(1.0), number(2.0)])]));
}

/// Sol probe 1, single cell: the same concat without a snapshot boundary.
#[tokio::test(flavor = "current_thread")]
async fn optimized_concat_insertion_shares_within_one_cell() {
    // x = [1]
    // acc = []
    // acc = acc + [x]
    // x = push(x, 2)
    // finish acc
    let value = run(
        &mut State::new(),
        a::program(vec![
            a::assign("x", a::list(vec![a::number(1.0)])),
            a::assign("acc", a::list(Vec::new())),
            a::assign("acc", a::add(a::var("acc"), a::list(vec![a::var("x")]))),
            a::assign("x", push(a::var("x"), a::number(2.0))),
            a::finish(a::var("acc")),
        ]),
    )
    .await;

    assert_eq!(value, list(vec![list(vec![number(1.0), number(2.0)])]));
}

/// The general concat form copies the right operand's members too.
#[tokio::test(flavor = "current_thread")]
async fn general_concat_copies_the_right_operand_members() {
    // x = [1]
    // b = [x, x]
    // acc = []
    // acc = acc + b
    // x = push(x, 2)
    // finish acc
    let value = run(
        &mut State::new(),
        a::program(vec![
            a::assign("x", a::list(vec![a::number(1.0)])),
            a::assign("b", a::list(vec![a::var("x"), a::var("x")])),
            a::assign("acc", a::list(Vec::new())),
            a::assign("acc", a::add(a::var("acc"), a::var("b"))),
            a::assign("x", push(a::var("x"), a::number(2.0))),
            a::finish(a::var("acc")),
        ]),
    )
    .await;

    assert_eq!(
        value,
        list(vec![list(vec![number(1.0)]), list(vec![number(1.0)])])
    );
}

/// The same concat where the right operand is a bare variable, which lowers to
/// the fused slot form rather than through the operand stack.
#[tokio::test(flavor = "current_thread")]
async fn slot_concat_copies_the_right_operand_members() {
    // x = [1]
    // b = [x]
    // acc = []
    // acc = acc + b
    // b = push(b, 9)
    // x = push(x, 2)
    // finish acc
    let value = run(
        &mut State::new(),
        a::program(vec![
            a::assign("x", a::list(vec![a::number(1.0)])),
            a::assign("b", a::list(vec![a::var("x")])),
            a::assign("acc", a::list(Vec::new())),
            a::assign("acc", a::add(a::var("acc"), a::var("b"))),
            a::assign("b", push(a::var("b"), a::number(9.0))),
            a::assign("x", push(a::var("x"), a::number(2.0))),
            a::finish(a::var("acc")),
        ]),
    )
    .await;

    assert_eq!(value, list(vec![list(vec![number(1.0)])]));
}

/// Sol probe 2: a root holding a nested container, then aliased.
///
/// The two roots share the nested object, and the snapshot carrying that shape
/// must still decode: the reported failure encoded successfully and then failed
/// its own decoder with "heap roots `alias` and `pair` must not share object
/// 5". Sharing is now ordinary (ADR 0096); the decode is the assertion.
#[tokio::test(flavor = "current_thread")]
async fn aliased_root_with_a_nested_container_round_trips() {
    let mut state = State::new();
    // child = [1]
    // pair = (child,)
    // alias = pair
    // finish 0
    run(
        &mut state,
        a::program(vec![
            a::assign("child", a::list(vec![a::number(1.0)])),
            a::assign("pair", a::tuple(vec![a::var("child")])),
            a::assign("alias", a::var("pair")),
            a::finish(a::number(0.0)),
        ]),
    )
    .await;
    let mut restored = round_trip(&state);
    // child = push(child, 2)
    // finish [pair, alias, child]
    let value = run(
        &mut restored,
        a::program(vec![
            a::assign("child", push(a::var("child"), a::number(2.0))),
            a::finish(a::list(vec![
                a::var("pair"),
                a::var("alias"),
                a::var("child"),
            ])),
        ]),
    )
    .await;

    let pair = Value::Tuple(vec![list(vec![number(1.0), number(2.0)])].into());
    assert_eq!(
        value,
        list(vec![
            pair.clone(),
            pair,
            list(vec![number(1.0), number(2.0)])
        ])
    );
}

/// Sol probe 3: self insertion now builds a cycle, because a store aliases
/// (ADR 0096). A cycle is representable on the heap, so the failure is at the
/// host boundary — a value handed out must be finite — and it is typed, not a
/// hang or a stack overflow.
#[tokio::test(flavor = "current_thread")]
async fn self_insertion_builds_a_cycle_the_host_boundary_refuses() {
    // a = []
    // a = push(a, a)
    // finish a
    let compiled = lashlang_compile_program(&a::program(vec![
        a::assign("a", a::list(Vec::new())),
        a::assign("a", push(a::var("a"), a::var("a"))),
        a::finish(a::var("a")),
    ]))
    .expect("probe cell should compile");
    let mut state = State::new();
    let error = execute(&compiled, &mut state, &ProbeHost)
        .await
        .expect_err("a cyclic value cannot cross the host boundary");
    assert!(error.to_string().contains("contains a cycle"), "{error}");
    round_trip(&state);
}

/// Opus N1: an ordinary accumulate-then-alias program whose snapshot could not
/// decode.
#[tokio::test(flavor = "current_thread")]
async fn accumulated_rows_aliased_to_a_second_root_round_trip() {
    let mut state = State::new();
    // acc = []
    // for i in range(0, 3) { acc = push(acc, [i, [i]]) }
    // b = acc
    // finish 0
    run(
        &mut state,
        a::program(vec![
            a::assign("acc", a::list(Vec::new())),
            a::for_range(
                "i",
                3.0,
                vec![a::assign(
                    "acc",
                    push(
                        a::var("acc"),
                        a::list(vec![a::var("i"), a::list(vec![a::var("i")])]),
                    ),
                )],
            ),
            a::assign("b", a::var("acc")),
            a::finish(a::number(0.0)),
        ]),
    )
    .await;
    let mut restored = round_trip(&state);
    let value = run(
        &mut restored,
        a::program(vec![a::finish(a::list(vec![a::var("acc"), a::var("b")]))]),
    )
    .await;

    let rows = list(vec![
        list(vec![number(0.0), list(vec![number(0.0)])]),
        list(vec![number(1.0), list(vec![number(1.0)])]),
        list(vec![number(2.0), list(vec![number(2.0)])]),
    ]);
    assert_eq!(value, list(vec![rows.clone(), rows]));
}

/// Opus N1, mutation form: the aliased root names the same list, so it
/// observes later appends (ADR 0096), and the snapshot still decodes.
#[tokio::test(flavor = "current_thread")]
async fn aliased_accumulator_observes_later_appends() {
    // acc = []
    // for i in range(0, 2) { acc = push(acc, [i]) }
    // b = acc
    // acc = push(acc, [9])
    // finish b
    let value = run(
        &mut State::new(),
        a::program(vec![
            a::assign("acc", a::list(Vec::new())),
            a::for_range(
                "i",
                2.0,
                vec![a::assign(
                    "acc",
                    push(a::var("acc"), a::list(vec![a::var("i")])),
                )],
            ),
            a::assign("b", a::var("acc")),
            a::assign("acc", push(a::var("acc"), a::list(vec![a::number(9.0)]))),
            a::finish(a::var("b")),
        ]),
    )
    .await;

    assert_eq!(
        value,
        list(vec![
            list(vec![number(0.0)]),
            list(vec![number(1.0)]),
            list(vec![number(9.0)])
        ])
    );
}

/// A descendant reached through a path read, stored elsewhere, then mutated in
/// place through the original binding: every name reaches the same object, so
/// the mutation is visible everywhere (ADR 0096) and the state still decodes.
#[tokio::test(flavor = "current_thread")]
async fn descendant_read_into_a_new_binding_shares_the_descendant() {
    let mut state = State::new();
    // tree = { rows: [[1], [2]] }
    // first = tree.rows[0]
    // first = push(first, 99)
    // copy = tree
    // copy.rows[1] = [7]
    // finish [tree, first, copy]
    let value = run(
        &mut state,
        a::program(vec![
            a::assign(
                "tree",
                a::record(vec![(
                    "rows",
                    a::list(vec![
                        a::list(vec![a::number(1.0)]),
                        a::list(vec![a::number(2.0)]),
                    ]),
                )]),
            ),
            a::assign(
                "first",
                a::index(a::field(a::var("tree"), "rows"), a::number(0.0)),
            ),
            a::assign("first", push(a::var("first"), a::number(99.0))),
            a::assign("copy", a::var("tree")),
            a::assign_path(
                "copy",
                vec![a::field_step("rows"), a::index_step(a::number(1.0))],
                a::list(vec![a::number(7.0)]),
            ),
            a::finish(a::list(vec![
                a::var("tree"),
                a::var("first"),
                a::var("copy"),
            ])),
        ]),
    )
    .await;

    assert_eq!(
        value,
        list(vec![
            Value::Record(std::sync::Arc::new(
                [(
                    "rows".to_string(),
                    list(vec![
                        list(vec![number(1.0), number(99.0)]),
                        list(vec![number(7.0)])
                    ])
                )]
                .into_iter()
                .collect()
            )),
            list(vec![number(1.0), number(99.0)]),
            Value::Record(std::sync::Arc::new(
                [(
                    "rows".to_string(),
                    list(vec![
                        list(vec![number(1.0), number(99.0)]),
                        list(vec![number(7.0)])
                    ])
                )]
                .into_iter()
                .collect()
            )),
        ])
    );
    round_trip(&state);
}

/// A multi-root program leaves behind bindings that share objects, and the
/// state it persists must still decode — including after a later append that
/// every alias observes (ADR 0096).
#[tokio::test(flavor = "current_thread")]
async fn multi_root_program_state_always_decodes() {
    let mut state = State::new();
    // base = [[1], [2]]
    // alias = base
    // pair = (base, alias)
    // record = { left: base, right: pair }
    // rows = [item for item in base]
    // joined = base + alias
    // appended = []
    // appended = appended + [record]
    // finish 0
    run(
        &mut state,
        a::program(vec![
            a::assign(
                "base",
                a::list(vec![
                    a::list(vec![a::number(1.0)]),
                    a::list(vec![a::number(2.0)]),
                ]),
            ),
            a::assign("alias", a::var("base")),
            a::assign("pair", a::tuple(vec![a::var("base"), a::var("alias")])),
            a::assign(
                "record",
                a::record(vec![("left", a::var("base")), ("right", a::var("pair"))]),
            ),
            a::assign(
                "rows",
                a::comprehension(a::var("item"), "item", a::var("base")),
            ),
            a::assign("joined", a::add(a::var("base"), a::var("alias"))),
            a::assign("appended", a::list(Vec::new())),
            a::assign(
                "appended",
                a::add(a::var("appended"), a::list(vec![a::var("record")])),
            ),
            a::finish(a::number(0.0)),
        ]),
    )
    .await;
    let mut restored = round_trip(&state);
    // base = push(base, [3])
    // finish 0
    run(
        &mut restored,
        a::program(vec![
            a::assign("base", push(a::var("base"), a::list(vec![a::number(3.0)]))),
            a::finish(a::number(0.0)),
        ]),
    )
    .await;
    let restored = round_trip(&restored);

    assert_eq!(
        restored.globals().get("alias").cloned(),
        Some(list(vec![
            list(vec![number(1.0)]),
            list(vec![number(2.0)]),
            list(vec![number(3.0)])
        ]))
    );
}

/// `decode(encode(state)) == state` for a program that allocated, discarded and
/// re-allocated, which leaves the heap holding vacant storage slots and a free
/// list.
///
/// Snapshot equality is the oracle the round-trip tests lean on, so it has to
/// compare what the wire actually carries — live objects under their IDs, the
/// roots that name them, and the meters — rather than the private storage
/// layout, which a round trip legitimately compacts.
#[tokio::test(flavor = "current_thread")]
async fn snapshot_equality_survives_a_round_trip_after_temporaries() {
    let mut state = State::new();
    // kept = [[1], [2]]
    // for n in range(0, 40) {
    //   scratch = [{ n: n }, { n: n + 1 }]
    // }
    // kept = push(kept, [3])
    // finish 0
    run(
        &mut state,
        a::program(vec![
            a::assign(
                "kept",
                a::list(vec![
                    a::list(vec![a::number(1.0)]),
                    a::list(vec![a::number(2.0)]),
                ]),
            ),
            a::for_range(
                "n",
                40.0,
                vec![a::assign(
                    "scratch",
                    a::list(vec![
                        a::record(vec![("n", a::var("n"))]),
                        a::record(vec![("n", a::add(a::var("n"), a::number(1.0)))]),
                    ]),
                )],
            ),
            a::assign("kept", push(a::var("kept"), a::list(vec![a::number(3.0)]))),
            a::finish(a::number(0.0)),
        ]),
    )
    .await;

    let snapshot = state.snapshot();
    let bytes = snapshot.to_canonical_bytes().expect("state should encode");
    let decoded = Snapshot::from_canonical_bytes(&bytes).expect("state should decode");

    assert_eq!(
        decoded, snapshot,
        "a decoded snapshot must equal the snapshot it came from"
    );
    assert_eq!(
        decoded
            .to_canonical_bytes()
            .expect("decoded snapshot should re-encode"),
        bytes,
        "accepted bytes are a fixed point"
    );

    // And the equality is not vacuous: a state with different heap contents
    // compares unequal.
    let mut other = State::from_snapshot(decoded);
    // kept = push(kept, [4])
    // finish 0
    run(
        &mut other,
        a::program(vec![
            a::assign("kept", push(a::var("kept"), a::list(vec![a::number(4.0)]))),
            a::finish(a::number(0.0)),
        ]),
    )
    .await;
    assert_ne!(other.snapshot(), snapshot);
}

/// Formatting a container variable must not take down the process.
///
/// `format("{0}", xs)` lowers to the fused slot-format opcode for any bare
/// variable argument, and that opcode reads the slot directly. When the slot
/// held a heap reference the reference reached the stringifier, which treated
/// the case as impossible.
#[tokio::test(flavor = "current_thread")]
async fn formatting_a_container_binding_renders_it() {
    let mut state = State::new();
    // xs = [1, 2]
    // rec = { a: 1 }
    // tup = (1, 2)
    // built = []
    // for n in range(0, 3) { built = push(built, n) }
    // finish [
    //   format("{0}", xs),
    //   format("{0}", rec),
    //   format("{0}", tup),
    //   format("{0}", built),
    //   format("list is {0} and record is {1}", xs, rec)
    // ]
    let value = run(
        &mut state,
        a::program(vec![
            a::assign("xs", a::list(vec![a::number(1.0), a::number(2.0)])),
            a::assign("rec", a::record(vec![("a", a::number(1.0))])),
            a::assign("tup", a::tuple(vec![a::number(1.0), a::number(2.0)])),
            a::assign("built", a::list(Vec::new())),
            a::for_range(
                "n",
                3.0,
                vec![a::assign("built", push(a::var("built"), a::var("n")))],
            ),
            a::finish(a::list(vec![
                a::call("format", vec![a::string("{0}"), a::var("xs")]),
                a::call("format", vec![a::string("{0}"), a::var("rec")]),
                a::call("format", vec![a::string("{0}"), a::var("tup")]),
                a::call("format", vec![a::string("{0}"), a::var("built")]),
                a::call(
                    "format",
                    vec![
                        a::string("list is {0} and record is {1}"),
                        a::var("xs"),
                        a::var("rec"),
                    ],
                ),
            ])),
        ]),
    )
    .await;

    let Value::List(rendered) = value else {
        panic!("expected a list of rendered strings")
    };
    let rendered = rendered
        .iter()
        .map(|value| match value {
            Value::String(text) => text.to_string(),
            other => panic!("expected a string, got {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(rendered[0], "[1,2]");
    assert_eq!(rendered[1], "{\"a\":1}");
    assert_eq!(rendered[2], "(1, 2)");
    assert_eq!(rendered[3], "[0,1,2]");
    assert_eq!(rendered[4], "list is [1,2] and record is {\"a\":1}");
}

/// A type error against a container binding names the container's type, not the
/// internal representation it happens to be stored in.
#[tokio::test(flavor = "current_thread")]
async fn arithmetic_on_a_container_binding_names_the_container_type() {
    let mut state = State::new();
    // xs = [1, 2]
    // finish format("{0}", xs + 1)
    let compiled = lashlang_compile_program(&a::program(vec![
        a::assign("xs", a::list(vec![a::number(1.0), a::number(2.0)])),
        a::finish(a::call(
            "format",
            vec![a::string("{0}"), a::add(a::var("xs"), a::number(1.0))],
        )),
    ]))
    .expect("program should compile");
    let error = execute(&compiled, &mut state, &ProbeHost)
        .await
        .expect_err("adding a number to a list should fail");

    let message = error.to_string();
    assert!(
        message.contains("list"),
        "error should name the list type: {message}"
    );
    assert!(
        !message.contains("heap_ref"),
        "error must not leak the heap representation: {message}"
    );
}

/// Compiles an IR program as the main entry of the raw module artifact it
/// forms, through the one public compile entry.
fn lashlang_compile_program(
    program: &lashlang::Program,
) -> Result<lashlang::CompiledProgram, Box<dyn std::error::Error>> {
    let artifact = lashlang::ModuleArtifact::from_program(program.clone())?;
    Ok(lashlang::compile(
        &artifact,
        lashlang::Entry::Main,
        Some(&program.spans),
    )?)
}
