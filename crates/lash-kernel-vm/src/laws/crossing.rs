//! Type-directed crossing (`K-EFF-002`, `K-CLO-004`): a function crosses
//! into an effect exactly where the type the effect declares at its path
//! admits one, and raises `not_data` naming that path anywhere else, before
//! anything is requested.

use lash_kernel_doc::Datum;

use super::embedder::Embedder;
use crate::{End, Request, RunError, Step};

/// A closure that states how a host starts it, so it can cross.
const STARTABLE: &str = "let f = fn(a) start (a: Any) -> Any bare direct { return a }";

/// Performs `take(arg)`, `take` declared to take one `input` of type `ty`:
/// the arguments it requested, or the kind and message of the error it
/// raised instead, when it requested nothing.
fn perform(ty: &str, arg: &str) -> Result<Vec<Datum>, (String, String)> {
    let mut embedder = Embedder::new(&format!(
        "effect take(input: {ty}) -> Any\nmain {{ {STARTABLE} do perform take({arg}) as Any }}"
    ));
    loop {
        match embedder.run() {
            Step::Slice => {}
            Step::Parked(_) => {
                let [Request::Effect(effect)] = embedder.requests.as_slice() else {
                    panic!("{ty}: one effect request: {:?}", embedder.requests);
                };
                return Ok(effect.args.clone());
            }
            Step::Ended(End::Error(RunError::Uncaught {
                error: Datum::Error(error),
                ..
            })) => {
                assert!(embedder.requests.is_empty(), "{ty}: nothing was requested");
                return Err((error.kind, error.message));
            }
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
