//! Value-semantics probes taken verbatim from the FIG-1301 adversarial
//! re-reviews.
//!
//! Every case here is a program a reviewer wrote to break the heap
//! representation: a container that kept a live alias to a value stored
//! elsewhere, or a snapshot the encoder emitted outside the language its own
//! decoder accepts. They assert the pre-heap tree language's value semantics —
//! a store copies — and that an emitted snapshot always decodes.

use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome, Snapshot, State,
    Value, compile, execute,
};

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

/// Runs `code` against `state` and returns the value it finishes with.
async fn run(state: &mut State, code: &str) -> Value {
    let compiled = compile(code).expect("probe cell should compile");
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
/// `acc = acc + [x]` inserts a copy, so mutating `x` afterwards leaves `acc`
/// alone. The reported failure produced `[[1, 2]]`.
#[tokio::test(flavor = "current_thread")]
async fn optimized_concat_insertion_copies_the_appended_binding() {
    let mut state = State::new();
    run(&mut state, "x = [1]\nacc = []\nfinish 0").await;
    let mut state = round_trip(&state);
    run(&mut state, "acc = acc + [x]\nfinish 0").await;
    let mut state = round_trip(&state);
    let value = run(&mut state, "x = push(x, 2)\nfinish acc").await;

    assert_eq!(value, list(vec![list(vec![number(1.0)])]));
}

/// Sol probe 1, single cell: the same concat without a snapshot boundary.
#[tokio::test(flavor = "current_thread")]
async fn optimized_concat_insertion_copies_within_one_cell() {
    let value = run(
        &mut State::new(),
        "x = [1]\nacc = []\nacc = acc + [x]\nx = push(x, 2)\nfinish acc",
    )
    .await;

    assert_eq!(value, list(vec![list(vec![number(1.0)])]));
}

/// The general concat form copies the right operand's members too.
#[tokio::test(flavor = "current_thread")]
async fn general_concat_copies_the_right_operand_members() {
    let value = run(
        &mut State::new(),
        "x = [1]\nb = [x, x]\nacc = []\nacc = acc + b\nx = push(x, 2)\nfinish acc",
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
    let value = run(
        &mut State::new(),
        "x = [1]\nb = [x]\nacc = []\nacc = acc + b\nb = push(b, 9)\nx = push(x, 2)\nfinish acc",
    )
    .await;

    assert_eq!(value, list(vec![list(vec![number(1.0)])]));
}

/// Sol probe 2: a root holding a nested container, then aliased.
///
/// The two roots must not share the nested object. The reported failure encoded
/// successfully and then failed its own decoder with "heap roots `alias` and
/// `pair` must not share object 5".
#[tokio::test(flavor = "current_thread")]
async fn aliased_root_with_a_nested_container_round_trips() {
    let mut state = State::new();
    run(
        &mut state,
        "child = [1]\npair = (child,)\nalias = pair\nfinish 0",
    )
    .await;
    let mut restored = round_trip(&state);
    let value = run(
        &mut restored,
        "child = push(child, 2)\nfinish [pair, alias, child]",
    )
    .await;

    let pair = Value::Tuple(vec![list(vec![number(1.0)])].into());
    assert_eq!(
        value,
        list(vec![
            pair.clone(),
            pair,
            list(vec![number(1.0), number(2.0)])
        ])
    );
}

/// Sol probe 3: self insertion stays a copy rather than a cycle.
#[tokio::test(flavor = "current_thread")]
async fn self_insertion_stores_a_copy() {
    let mut state = State::new();
    let value = run(&mut state, "a = []\na = push(a, a)\nfinish a").await;

    assert_eq!(value, list(vec![list(Vec::new())]));
    round_trip(&state);
}

/// Opus N1: an ordinary accumulate-then-alias program whose snapshot could not
/// decode.
#[tokio::test(flavor = "current_thread")]
async fn accumulated_rows_aliased_to_a_second_root_round_trip() {
    let mut state = State::new();
    run(
        &mut state,
        "acc = []\nfor i in range(0, 3) { acc = push(acc, [i, [i]]) }\nb = acc\nfinish 0",
    )
    .await;
    let mut restored = round_trip(&state);
    let value = run(&mut restored, "finish [acc, b]").await;

    let rows = list(vec![
        list(vec![number(0.0), list(vec![number(0.0)])]),
        list(vec![number(1.0), list(vec![number(1.0)])]),
        list(vec![number(2.0), list(vec![number(2.0)])]),
    ]);
    assert_eq!(value, list(vec![rows.clone(), rows]));
}

/// Opus N1, mutation form: the aliased root must not observe later appends.
#[tokio::test(flavor = "current_thread")]
async fn aliased_accumulator_does_not_observe_later_appends() {
    let value = run(
        &mut State::new(),
        "acc = []\nfor i in range(0, 2) { acc = push(acc, [i]) }\nb = acc\nacc = push(acc, [9])\nfinish b",
    )
    .await;

    assert_eq!(
        value,
        list(vec![list(vec![number(0.0)]), list(vec![number(1.0)])])
    );
}

/// A descendant reached through a path read, stored elsewhere, then mutated in
/// place through the original binding.
#[tokio::test(flavor = "current_thread")]
async fn descendant_read_into_a_new_binding_is_isolated() {
    let mut state = State::new();
    let value = run(
        &mut state,
        r#"
        tree = { rows: [[1], [2]] }
        first = tree.rows[0]
        first = push(first, 99)
        copy = tree
        copy.rows[1] = [7]
        finish [tree, first, copy]
        "#,
    )
    .await;

    assert_eq!(
        value,
        list(vec![
            Value::Record(std::sync::Arc::new(
                [(
                    "rows".to_string(),
                    list(vec![list(vec![number(1.0)]), list(vec![number(2.0)])])
                )]
                .into_iter()
                .collect()
            )),
            list(vec![number(1.0), number(99.0)]),
            Value::Record(std::sync::Arc::new(
                [(
                    "rows".to_string(),
                    list(vec![list(vec![number(1.0)]), list(vec![number(7.0)])])
                )]
                .into_iter()
                .collect()
            )),
        ])
    );
    round_trip(&state);
}

/// Every binding a multi-root program leaves behind is independently owned, so
/// the state it persists always decodes.
#[tokio::test(flavor = "current_thread")]
async fn multi_root_program_state_always_decodes() {
    let mut state = State::new();
    run(
        &mut state,
        r#"
        base = [[1], [2]]
        alias = base
        pair = (base, alias)
        record = { left: base, right: pair }
        rows = [item for item in base]
        joined = base + alias
        appended = []
        appended = appended + [record]
        finish 0
        "#,
    )
    .await;
    let mut restored = round_trip(&state);
    run(&mut restored, "base = push(base, [3])\nfinish 0").await;
    let restored = round_trip(&restored);

    assert_eq!(
        restored.globals().get("alias").cloned(),
        Some(list(vec![list(vec![number(1.0)]), list(vec![number(2.0)])]))
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
    run(
        &mut state,
        r#"
        kept = [[1], [2]]
        for n in range(0, 40) {
          scratch = [{ n: n }, { n: n + 1 }]
        }
        kept = push(kept, [3])
        finish 0
        "#,
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
    run(&mut other, "kept = push(kept, [4])\nfinish 0").await;
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
    let value = run(
        &mut state,
        r#"
        xs = [1, 2]
        rec = { a: 1 }
        tup = (1, 2)
        built = []
        for n in range(0, 3) { built = push(built, n) }
        finish [
          format("{0}", xs),
          format("{0}", rec),
          format("{0}", tup),
          format("{0}", built),
          format("list is {0} and record is {1}", xs, rec)
        ]
        "#,
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
    let compiled =
        compile("xs = [1, 2]\nfinish format(\"{0}\", xs + 1)").expect("program should compile");
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
