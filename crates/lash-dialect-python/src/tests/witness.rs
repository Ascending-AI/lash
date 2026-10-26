//! The dialect against CPython, on cells.
//!
//! The arc's capture script, which `captured.rs` reads, records values. It
//! does not record what a program prints, a sleep, a wait a cancelled
//! task abandons, or which park a tool call was issued in, and its
//! programs are one function, not a cell. `witness/record.py` records
//! those: `witness/recorded.json` holds what CPython did with each cell of
//! `witness/cases/`, as the lines it printed, the tool calls and sleeps it
//! asked for between deliveries, and how it ended. Each law here lowers
//! one case, runs it on the kernel machine under the same deliveries and
//! requires the same record.

use serde::Deserialize;

use super::machine::{self, Epoch, Recorded};

#[derive(Deserialize)]
struct Case {
    name: String,
    source: String,
    deliveries: Vec<Vec<String>>,
    epochs: Vec<Epoch>,
    end: String,
}

#[derive(Deserialize)]
struct Witness {
    cases: Vec<Case>,
}

fn case(name: &str) -> Case {
    let witness: Witness = serde_json::from_str(include_str!("../../witness/recorded.json"))
        .expect("recorded.json is what record.py writes");
    witness
        .cases
        .into_iter()
        .find(|case| case.name == name)
        .unwrap_or_else(|| panic!("no recorded case `{name}`"))
}

/// The case runs on the kernel as CPython ran it.
fn agrees(name: &str, source: &str) {
    let case = case(name);
    assert_eq!(
        case.source, source,
        "stale witness source `{name}`; run record.py"
    );
    let cpython = Recorded {
        epochs: case.epochs,
        end: case.end,
    };
    let kernel = machine::run(&case.source, &case.deliveries);
    assert_eq!(
        kernel,
        cpython,
        "`{name}` on the kernel (left) is not what CPython recorded (right)\n{}",
        machine::kernel_text(&case.source)
    );
}

macro_rules! witness {
    ($($name:ident),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                agrees(stringify!($name), include_str!(concat!("../../witness/cases/", stringify!($name), ".py")));
            }
        )*
    };
}

witness!(
    number_text,
    unicode_split,
    negative_round,
    numeric_text_grammar,
    repr_escapes,
    cancel_cleanup,
    tasks,
    control,
    collections,
    exceptions,
    order,
    methods,
    gather_cancel,
    comprehension_scope,
    comprehension_size,
    list_pop,
    await_value,
    handler_tuple,
    bare_raise,
    cancel_result,
    raise_cause,
);
