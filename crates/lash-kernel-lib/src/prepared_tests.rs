//! Prepared native admission, charging and observation laws (`K-LIB-006`, `K-CHG-003`). A native's
//! prepared entry answers a call only with the value its general call returns,
//! and a run that takes it is the run that takes the general call: the same
//! result or error, charge and memory. The machine's odd layouts take every
//! native's general call, so each law runs one document in both.

use std::sync::Arc;

use lash_kernel_doc::{
    Datum, ErrorDatum, FunctionRegistry, Handle, Integer, Timestamp, parse_document,
};

use lash_kernel_vm::kernel_internals::{Layout, prepare_with_layout};
use lash_kernel_vm::{
    Bounds, End, Host, Machine, Meters, PreparedLibrary, Program, Step, register_machine_functions,
};
use lash_vocabulary::FunctionId;

use crate::{register_collections, register_numbers, register_text_json};

const BOUNDS: Bounds = Bounds {
    memory_meter: lash_kernel_vm::MemoryMeter::On,
    charge: lash_kernel_vm::Charge::Exact,
    charge_limit: 10_000_000,
    memory: 64 << 20,
    call_depth: 100,
    live_tasks: 100,
    requests_per_park: 100,
    join_members: 100,
};

struct World;

impl Host for World {
    fn read(&mut self, _: &Handle, _: &Datum) -> Result<Datum, ErrorDatum> {
        Ok(Datum::Null)
    }
    fn print(&mut self, _: &Datum) {}
    fn cancel_requested(&mut self) -> bool {
        false
    }
}

/// The kernel library prepared twice: in the natural layout, which takes
/// each native's fast path, and in one that takes every general call.
struct Libraries {
    registry: Arc<FunctionRegistry>,
    fast: PreparedLibrary,
    general: PreparedLibrary,
}

impl Libraries {
    fn new() -> Self {
        let mut registry = FunctionRegistry::new();
        register_machine_functions(&mut registry).unwrap();
        register_numbers(&mut registry).unwrap();
        register_collections(&mut registry).unwrap();
        register_text_json(&mut registry).unwrap();
        let registry = Arc::new(registry);
        Self {
            fast: PreparedLibrary::new(Arc::clone(&registry)),
            general: prepare_with_layout(Arc::clone(&registry), Layout(1)),
            registry,
        }
    }

    fn id(&self, name: &str) -> FunctionId {
        self.registry
            .iter()
            .find(|(_, function)| function.definition().name.as_str() == name)
            .map(|(id, _)| *id)
            .unwrap_or_else(|| panic!("no function `{name}`"))
    }

    /// Runs `main`, which binds `args` and returns `name` called on them,
    /// in both libraries, and asserts the two runs alike.
    fn agree(&self, name: &str, args: &[&str]) {
        let lets: String = args
            .iter()
            .enumerate()
            .map(|(index, arg)| format!("let a{index} = {arg} "))
            .collect();
        let call: Vec<String> = (0..args.len()).map(|index| format!("a{index}")).collect();
        let text = format!(
            "kernel 1\nnumbers by_spelling\nmain {{ {lets}return {name}({}) }}",
            call.join(", ")
        );
        let document = Arc::new(crate::testsupport::document(&text, &self.registry));
        let run = |library: &PreparedLibrary, layout: Layout| -> (End, Meters) {
            let program = crate::testsupport::program(Arc::clone(&document), library.clone());
            let mut machine = crate::testsupport::start(program, BOUNDS, layout).unwrap();
            match machine.run(&mut World, u64::MAX).unwrap() {
                Step::Ended(end) => (end, machine.meters()),
                other => panic!("{text}: {other:?}"),
            }
        };
        let fast = run(&self.fast, Layout(0));
        let general = run(&self.general, Layout(1));
        assert_eq!(fast, general, "{text}");
    }

    /// Runs `name` on every tuple of `corpus` of its arity in both.
    fn agree_on_all(&self, name: &str, arity: usize, corpus: &[&str]) {
        let mut tuple = vec![0; arity];
        loop {
            let args: Vec<&str> = tuple.iter().map(|index| corpus[*index]).collect();
            self.agree(name, &args);
            let Some(position) = tuple.iter().rposition(|index| index + 1 < corpus.len()) else {
                return;
            };
            tuple[position] += 1;
            tuple[position + 1..].fill(0);
        }
    }
}

/// K-CHG-001/003, K-MACH-008: the prepared and general entry give the same
/// result, error, charge and memory on all numeric identities and domains.
#[test]
fn charge_invariance() {
    let libraries = Libraries::new();
    let corpus = [
        "0",
        "1",
        "-3",
        "9223372036854775807",
        "999999999999999999999",
        "-0.0",
        "2.5",
        "nan",
        "inf",
        "\"12\"",
        "(1, 2)",
        "[1, 2]",
    ];
    for (definition, _) in crate::numbers() {
        let name = definition.name.as_str();
        let arity = definition.signature.params.len();
        // Three-argument math has the same admission as its binary siblings.
        let corpus = if arity > 2 { &corpus[..5] } else { &corpus[..] };
        libraries.agree_on_all(name, arity, corpus);
    }
}

/// K-LIB-006: `text.concat`'s prepared entry answers two short texts whose
/// concatenation a short text holds, with the text the general call
/// answers: the prepared and general runs give the same result, error,
/// charge and memory up to the short text's bound, across it and past it,
/// with multi-byte scalars and with an operand that is no text.
#[test]
fn short_concatenation_is_the_general_calls() {
    let libraries = Libraries::new();
    let corpus = [
        "\"\"",
        "\"key_\"",
        "\"63\"",
        "\"\u{1F600}x\"",
        "\"seven 7\"",
        "\"fourteen bytes\"",
        "\"fifteen bytes!!\"",
        "7",
    ];
    libraries.agree_on_all("text.concat", 2, &corpus);
}

/// K-LIB-006: a word's digits that a short text holds are answered by the
/// prepared entry, charged or not, spelled as the big integer spells them
/// in every radix; past the short text's bound it declines to the general
/// call, which reserves them.
#[test]
fn short_integer_digits_are_the_integers_own_spelling() {
    use lash_kernel_doc::Value;
    use lash_kernel_doc::arena::ShortText;
    use num_bigint::BigInt;

    let mut registry = FunctionRegistry::new();
    register_numbers(&mut registry).unwrap();
    let prepared = |name: &str| {
        registry
            .iter()
            .find(|(_, function)| function.definition().name.as_str() == name)
            .and_then(|(_, function)| function.native()?.prepare())
            .unwrap_or_else(|| panic!("`{name}` has a prepared entry"))
    };
    let (int_text, num_text) = (prepared("int.to_text"), prepared("num.to_text"));
    let spelled = |value: Option<Value>| {
        value.map(|value| match value {
            Value::Text(text) => text.as_str().to_owned(),
            other => panic!("digits are a short text, not {other:?}"),
        })
    };
    let short = |digits: String| Some(digits).filter(|digits| digits.len() <= ShortText::MAX);
    for word in [
        0,
        1,
        -1,
        35,
        -36,
        99_999_999_999_999,
        -9_999_999_999_999,
        100_000_000_000_000,
        -99_999_999_999_999,
        i64::MAX,
        i64::MIN,
    ] {
        for radix in 2..=36u32 {
            let args = [Value::Int(word), Value::Int(radix.into())];
            let expected = short(BigInt::from(word).to_str_radix(radix));
            for charging in [false, true] {
                assert_eq!(
                    spelled(int_text.answer(&args, charging).map(|answer| answer.value)),
                    expected,
                    "{word} in radix {radix}"
                );
            }
        }
        let expected = short(BigInt::from(word).to_string());
        assert_eq!(
            spelled((num_text.entry)(&[Value::Int(word)])),
            expected,
            "{word}"
        );
    }
}

/// K-MACH-008: preparing native entries preserves execution outcomes,
/// including a successful scalar, structural fallback and an admitted error.
#[test]
fn prepared_and_general_layouts_attribute_the_same_work() {
    let libraries = Libraries::new();
    let document = Arc::new(
        crate::testsupport::document(&format!(
            "kernel 1\nnumbers by_spelling\n
definition \"function helper(x: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {{ return add(x, 1) }}\"
main {{
    let a = call helper(0)
    let b = num.add(-0.0, 2.5)
    let c = int.mul(3, 7)
    let d = float.add(-0.0, 0.0)
    let ordered = lt(a, c)
    let structural = eq([1, 2], [1, 2])
    let negated = bool.not(ordered)
    try {{ let bad = num.add(\"text\", a) }} catch e {{ print e.kind }}
    return (a, b, c, d, ordered, structural, negated)
}}"
        ), &libraries.registry),
    );
    let mut ends = Vec::new();
    let mut charges = Vec::new();
    for layout in [Layout(0), Layout(1)] {
        let program = crate::testsupport::program(
            Arc::clone(&document),
            if layout == Layout(0) {
                libraries.fast.clone()
            } else {
                libraries.general.clone()
            },
        );
        let mut machine = crate::testsupport::start(program, BOUNDS, layout).unwrap();
        ends.push(machine.run(&mut World, u64::MAX).unwrap());
        charges.push(machine.meters().charged.expect("exact charge"));
    }
    assert_eq!(ends[0], ends[1]);
    assert_eq!(charges[0], charges[1]);
    assert!(matches!(ends[0], Step::Ended(End::Finished(_))));
}
