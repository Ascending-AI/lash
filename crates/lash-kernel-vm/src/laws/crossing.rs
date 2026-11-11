//! Type-directed crossing (`K-EFF-002`, `K-CLO-004`): a function crosses
//! into an effect exactly where the type the effect declares at its path
//! admits one, and raises `not_data` naming that path anywhere else, before
//! anything is requested.

use lash_kernel_doc::Datum;
use lash_kernel_value::content_ids::LinkId;
use lash_kernel_value::names::{CallableId, FunctionId, Name};

use super::embedder::{ROOMY, Setup, Source, World, core_program};
use crate::{CellStart, CoreMachine, End, Machine, Request, RunError, Step};

/// A closure that states how a host starts it, so it can cross.
const STARTABLE: &str = "let f = fn(a) presentation (a: Any) -> Any bare direct { return a }";

/// Performs `take(arg)` on the bytecode core, `take` declared to take one
/// `input` of type `ty`: the arguments it requested, or the kind and
/// message of the error it raised instead, when it requested nothing.
/// The host binds `h` to a function of a Link the run has not loaded.
fn perform(ty: &str, arg: &str) -> Result<Vec<Datum>, (String, String)> {
    let foreign = CallableId {
        link: LinkId::from_bytes([4; 32]),
        function: FunctionId::from_bytes([5; 32]),
    };
    let mut machine = <CoreMachine as Machine>::start(
        core_program(
            &format!(
                "import 4 operation host.take(input: {ty}) -> Any\nmain {{ {STARTABLE} do perform 4({arg}) as Any }}"
            ),
            None,
        ),
        ROOMY,
        CellStart {
            host: vec![(Name::new("h"), Datum::Function(foreign))],
            ..Setup::default().start
        },
        Box::new(Source::default()),
    )
    .unwrap_or_else(|error| panic!("{ty}: the law's document starts: {error:?}"));
    loop {
        match machine.run(&mut World::default(), u64::MAX).unwrap() {
            Step::Slice => {}
            Step::Parked(park) => {
                let [Request::Effect(effect)] = park.requests.as_slice() else {
                    panic!("{ty}: one effect request: {:?}", park.requests);
                };
                return Ok(effect.args.clone());
            }
            Step::Ended(End::Error(RunError::Uncaught {
                error: Datum::Error(error),
                ..
            })) => return Err((error.kind, error.message)),
            Step::Ended(other) => panic!("{ty}: the run ended without a request: {other:?}"),
        }
    }
}

/// The datum at `path` of `datum`: record fields by name, list members by
/// position.
fn at<'d>(datum: &'d Datum, path: &[&str]) -> &'d Datum {
    path.iter().fold(datum, |datum, step| match datum {
        Datum::Record(fields) => {
            &fields
                .iter()
                .find(|(name, _)| name == step)
                .unwrap_or_else(|| panic!("no field `{step}`"))
                .1
        }
        Datum::List(items) | Datum::Tuple(items) => &items[step.parse::<usize>().expect("index")],
        other => panic!("no member `{step}` in {other:?}"),
    })
}

/// CROSS-BY-TYPE: a function crosses as a function value exactly where the
/// declared type at its path admits a function: a function type, a union
/// with one, or `Any`, at the argument itself or nested in it.
#[test]
fn a_function_crosses_where_its_declared_type_admits_one() {
    let function = "Fn(a: Any) -> Any";
    let cases = [
        (function.to_owned(), "f", vec![]),
        (format!("Union(Text, {function})"), "f", vec![]),
        ("Any".to_owned(), "f", vec![]),
        (
            format!("Record{{run: {function}}}"),
            "{run: f}",
            vec!["run"],
        ),
        (format!("List(Union(Int, {function}))"), "[1, f]", vec!["1"]),
    ];
    for (ty, arg, path) in cases {
        let args = perform(&ty, arg).unwrap_or_else(|error| panic!("{ty}: {error:?}"));
        let crossed = at(&args[0], &path);
        assert!(
            matches!(crossed, Datum::FunctionValue(_)),
            "{ty}: {crossed:?}"
        );
    }
}

/// CROSS-PATH: a function where the declared type is concrete data (a typed
/// record field, a text, a closed record, `Json` data and everything in
/// it) raises `not_data` naming the argument path, and nothing is requested.
/// So does a function of a Link the run has not loaded, where a function
/// may cross: it has no defining document to cross against.
#[test]
fn a_function_in_a_concretely_typed_slot_raises_not_data_naming_its_path() {
    let cases = [
        (
            "Record{plan: Record{steps: List(Record{map: Text})}}",
            "{plan: {steps: [{map: \"a\"}, {map: \"b\"}, {map: f}]}}",
            "input.plan.steps[2].map",
        ),
        ("Text", "f", "input"),
        ("Record{x: Record{}}", "{x: f}", "input.x"),
        ("Json", "[1, f]", "input[1]"),
        (
            "Record{definition: Any, args: Json}",
            "{definition: f, args: {g: f}}",
            "input.args.g",
        ),
        (
            "Record{definition: Any, args: Json}",
            "{definition: f, args: map{\"g\": f}}",
            "input.args.g",
        ),
        // A union admits a function only through a member that does.
        ("Union(Text, List(Int))", "[f]", "input[0]"),
        ("Any", "h", "input"),
        ("Record{run: Any}", "{run: h}", "input.run"),
    ];
    for (ty, arg, path) in cases {
        let (kind, message) = perform(ty, arg).expect_err(ty);
        assert_eq!(kind, "not_data", "{ty}: {message}");
        assert!(
            message.contains(&format!("`{path}`")),
            "{ty}: {message} names {path}"
        );
    }
}

/// `K-CLO-004` (cutover contract §2, cells): a crossing freezes an
/// independent graph. The function crosses with its captures as they are
/// at the crossing, so a live write after it is in a later crossing and
/// not in the earlier one, and each start of a crossed function begins
/// from its frozen captures: over `[1, 2]`, `f(3)` then `f(4)` give
/// `[1, 2, 3]` and then `[1, 2, 4]`.
#[test]
fn a_crossed_function_starts_from_its_frozen_captures_each_time() {
    use super::embedder::{int, wait_of};
    use crate::Outcome;
    let text = "import 4 operation host.take(input: Any) -> Any\nmain {\n  let xs = [1, 2]\n  let f = fn(a) presentation (a: Any) -> Any bare direct { set xs[list.len(xs)] = a return xs }\n  do perform 4(f) as Any\n  set xs[0] = 9\n  do perform 4(f) as Any\n  return 0\n}";
    let start = |target, args| {
        let mut machine = <CoreMachine as Machine>::start(
            core_program(text, None),
            ROOMY,
            CellStart {
                target,
                args,
                ..Setup::default().start
            },
            Box::new(Source::default()),
        )
        .unwrap_or_else(|error| panic!("{error:?}"));
        let mut crossed = Vec::new();
        loop {
            match machine.run(&mut World::default(), u64::MAX).unwrap() {
                Step::Slice => {}
                Step::Parked(park) => {
                    for request in &park.requests {
                        let Request::Effect(effect) = request;
                        crossed.push(effect.args[0].clone());
                        machine
                            .deliver(wait_of(request), Outcome::Completed(Datum::Null))
                            .unwrap();
                    }
                }
                Step::Ended(End::Finished(finished)) => return (finished.result, crossed),
                Step::Ended(other) => panic!("the run did not finish: {other:?}"),
            }
        }
    };
    let (_, crossed) = start(crate::Target::Main, Vec::new());
    let [Datum::FunctionValue(before), Datum::FunctionValue(after)] = crossed.as_slice() else {
        panic!("two crossed functions: {crossed:?}");
    };
    let call = |function: &lash_kernel_doc::FunctionValue, arg| {
        start(
            crate::Target::Callable(Box::new(function.clone())),
            vec![int(arg)],
        )
        .0
    };
    let list = |items: &[i64]| Datum::List(items.iter().map(|n| int(*n)).collect());
    assert_eq!(call(before, 3), list(&[1, 2, 3]));
    assert_eq!(call(before, 4), list(&[1, 2, 4]));
    assert_eq!(call(after, 3), list(&[9, 2, 3]));
}

/// `K-EFF-002`: a task is no data. A task among an effect's arguments
/// raises `not_data` before anything is requested, however deep it sits.
#[test]
fn a_task_in_an_effect_argument_raises_not_data_before_the_request() {
    let text = "definition \"function work() -> Any\\nkernel 1\\nmetering statements\\nbody { return 1 }\"\nimport 4 operation host.take(input: Any) -> Any\nmain {\n  let t = spawn call work()\n  do perform 4({plan: [1, t]}) as Any\n  return 0\n}";
    let mut machine = <CoreMachine as Machine>::start(
        core_program(text, None),
        ROOMY,
        Setup::default().start,
        Box::new(Source::default()),
    )
    .unwrap_or_else(|error| panic!("{error:?}"));
    match machine.run(&mut World::default(), u64::MAX).unwrap() {
        Step::Ended(End::Error(RunError::Uncaught {
            error: Datum::Error(error),
            ..
        })) => assert_eq!(error.kind, "not_data", "{}", error.message),
        other => panic!("the task crossed or a request was made: {other:?}"),
    }
}
