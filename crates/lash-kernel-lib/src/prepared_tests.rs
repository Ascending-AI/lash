//! Prepared native admission, charging and observation laws (`K-LIB-006`, `K-CHG-003`). A native's
//! prepared entry answers a call only with the value its general call returns,
//! and a run that takes it is the run that takes the general call: the same
//! result or error, charge and memory. The machine's odd layouts take every
//! native's general call, so each law runs one document in both.

use std::sync::Arc;

use lash_kernel_doc::{
    Datum, ErrorDatum, FunctionId, FunctionRegistry, Handle, Integer, Timestamp, parse_document,
};
use lash_kernel_vm::{
    Bounds, End, Host, Layout, Machine, Meters, PreparedLibrary, Program, Step,
    register_machine_functions,
};

use crate::{register_collections, register_numbers};

const BOUNDS: Bounds = Bounds {
    charge: 10_000_000,
    memory: 64 << 20,
    call_depth: 100,
    live_tasks: 100,
    requests_per_park: 100,
    join_members: 100,
};

struct World;

impl Host for World {
    fn clock(&mut self) -> Timestamp {
        Timestamp {
            nanoseconds: Integer::from(0),
        }
    }
    fn random(&mut self) -> u64 {
        0
    }
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
        let registry = Arc::new(registry);
        Self {
            fast: PreparedLibrary::new(Arc::clone(&registry)),
            general: PreparedLibrary::with_layout(Arc::clone(&registry), Layout(1)),
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
        let mut uses = format!("use {name} = @{}\n", self.id(name));
        if name != "error.new" {
            uses.push_str(&format!("use error.new = @{}\n", self.id("error.new")));
        }
        let text = format!(
            "kernel 1\nnumbers by_spelling\n{uses}main {{ {lets}return {name}({}) }}",
            call.join(", ")
        );
        let document = Arc::new(parse_document(&text).unwrap_or_else(|error| panic!("{error}")));
        let run = |library: &PreparedLibrary, layout: Layout| -> (End, Meters) {
            let program = Program {
                release: lash_kernel_doc::heap::PageId([8; 32]),
                document: Arc::clone(&document),
                library: library.clone(),
            };
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

/// K-MACH-008: preparing native entries preserves execution outcomes,
/// including a successful scalar, structural fallback and an admitted error.
#[test]
fn prepared_and_general_layouts_attribute_the_same_work() {
    let libraries = Libraries::new();
    let uses: String = [
        "add",
        "num.add",
        "int.mul",
        "float.add",
        "lt",
        "eq",
        "bool.not",
    ]
    .iter()
    .map(|name| format!("use {name} = @{}\n", libraries.id(name)))
    .collect();
    let document = Arc::new(
        parse_document(&format!(
            "kernel 1\nnumbers by_spelling\n{uses}
fn helper(x) {{ return add(x, 1) }}
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
        ))
        .unwrap(),
    );
    let mut ends = Vec::new();
    let mut charges = Vec::new();
    for layout in [Layout(0), Layout(1)] {
        let program = Program {
            release: lash_kernel_doc::heap::PageId([8; 32]),
            document: Arc::clone(&document),
            library: if layout == Layout(0) {
                libraries.fast.clone()
            } else {
                libraries.general.clone()
            },
        };
        let mut machine = crate::testsupport::start(program, BOUNDS, layout).unwrap();
        ends.push(machine.run(&mut World, u64::MAX).unwrap());
        charges.push(machine.meters().charged);
    }
    assert_eq!(ends[0], ends[1]);
    assert_eq!(charges[0], charges[1]);
    assert!(matches!(ends[0], Step::Ended(End::Finished(_))));
}
