//! JSON.stringify's plain-data path (`ts.json.plain`, then the console's
//! native render) against its generic walk (`ts.json.stringify_generic`).
//!
//! Each law runs one document whose entries differ in one call only, on the
//! same generated input, so the generic walk is the oracle for the result,
//! the error and the prints, and the entries' charges isolate each call's.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_kernel_doc::{Datum, ErrorDatum, Float, Handle, Integer, Timestamp};

use lash_kernel_vm::{Bounds, CellStart, End, Host, Machine, Program, RunError, Step, Target};
use lash_vocabulary::Name;

use super::machine;

const BOUNDS: Bounds = Bounds {
    memory_meter: lash_kernel_vm::MemoryMeter::On,
    charge: lash_kernel_vm::Charge::Exact,
    charge_limit: 50_000_000,
    memory: 256 << 20,
    call_depth: 400,
    live_tasks: 10,
    requests_per_park: 10,
    join_members: 10,
};

/// The entries: each builds the input, then makes its one call. `build`
/// makes what start arguments cannot carry: a shared value, cycles and
/// closures.
const ENTRIES: &str = r#"
function build(v: Any, mode: Any) -> Any
kernel 1
metering statements
body {
  let shout = fn(holder, passed) { print "toJSON" return "shout" }
  if eq(mode, 1) { return [v, v] }
  if eq(mode, 2) { let r = {head: v} set r.self = r return r }
  if eq(mode, 3) { let a = [v] set a[1] = a return a }
  if eq(mode, 4) { return {value: v, toJSON: shout} }
  if eq(mode, 5) { return [v, shout] }
  if eq(mode, 6) { return {value: v, f: shout} }
  if eq(mode, 7) { return {a: v, b: [v, {c: v}]} }
  return v
}
function fast(v: Any, mode: Int, tail: List(Any)) -> Any
kernel 1
metering statements
body {
  let x = invoke build(v, mode)
  let args = list.concat([x], tail)
  let out = invoke ts.json.stringify(null, args)
  return out
}
function walk(v: Any, mode: Int, tail: List(Any)) -> Any
kernel 1
metering statements
body {
  let x = invoke build(v, mode)
  let args = list.concat([x], tail)
  let out = invoke ts.json.stringify_generic(null, args)
  return out
}
function admits(v: Any, mode: Int, tail: List(Any)) -> Any
kernel 1
metering statements
body {
  let x = invoke build(v, mode)
  let args = list.concat([x], tail)
  let out = invoke ts.json.plain(x)
  return out
}
function draw(v: Any, mode: Int, tail: List(Any)) -> Any
kernel 1
metering statements
body {
  let x = invoke build(v, mode)
  let args = list.concat([x], tail)
  let out = invoke ts.console.json(x)
  return out
}
function idle(v: Any, mode: Int, tail: List(Any)) -> Any
kernel 1
metering statements
body {
  let x = invoke build(v, mode)
  let args = list.concat([x], tail)
  let out = null
  return out
}
function quick(x: Any) -> Any
kernel 1
metering statements
body { return x }
"#;

fn program(
    retained: &lash_kernel_package::LinkedContent,
    roots: &BTreeSet<lash_vocabulary::LinkId>,
    library: lash_kernel_vm::PreparedLibrary,
) -> Program {
    let names = [
        "eq",
        "list.concat",
        "ts.json.stringify",
        "ts.json.stringify_generic",
        "ts.json.plain",
        "ts.console.json",
    ];
    let mut text = names
        .iter()
        .map(|name| format!("use {name}\n"))
        .collect::<String>();
    text.push_str("use build\n");
    text.push_str("use fast\n");
    text.push_str("use walk\n");
    text.push_str("use admits\n");
    text.push_str("use draw\n");
    text.push_str("use idle\n");
    text.push_str("use quick\n");
    text.push_str(ENTRIES);
    let document = super::linked_inspection::definitions(
        &text,
        &["fast", "walk", "admits", "draw", "idle"],
        super::library(),
    );
    let (mut content, root) = super::linked::content(&document);
    let mut pending = roots.iter().copied().collect::<Vec<_>>();
    let mut visited = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !visited.insert(id) {
            continue;
        }
        let link = &retained.links[&id];
        content
            .documents
            .insert(link.document, retained.documents[&link.document].clone());
        content.links.insert(id, link.clone());
        pending.extend(link.fills.values().filter_map(|fill| match fill {
            lash_vocabulary::package::Fill::Export { link, .. } => Some(*link),
            _ => None,
        }));
    }
    Program::prepare(
        root,
        content
            .documents
            .into_iter()
            .map(|(id, document)| (id, Arc::new(document)))
            .collect(),
        &content.links,
        super::pages::CLOSURE,
        library,
        lash_kernel_doc::Selection::Fresh,
    )
    .expect("JSON inputs retain their producing Links")
}

#[derive(Default)]
struct Console {
    lines: Vec<String>,
    data: Vec<Datum>,
}

impl Host for Console {
    fn perform(
        &mut self,
        use_: &lash_vocabulary::OperationUse,
        _: &[Datum],
        _: &lash_vocabulary::sites::PositionLabel,
        _: &lash_vocabulary::sites::Site,
    ) -> Result<Result<Datum, lash_kernel_doc::ErrorDatum>, lash_kernel_vm::HostFault> {
        crate::tests::linked_reads::read(use_, 0)
    }

    fn read(&mut self, _handle: &Handle, _request: &Datum) -> Result<Datum, ErrorDatum> {
        Err(ErrorDatum {
            kind: "type_error".to_owned(),
            message: "no projection".to_owned(),
            data: Datum::Null,
        })
    }

    fn print(&mut self, value: &Datum) {
        self.lines.push(format!("{value:?}"));
        self.data.push(value.clone());
    }

    fn cancel_requested(&mut self) -> bool {
        false
    }
}

/// What one entry did: how it ended, what it printed and its charge.
#[derive(Debug)]
struct Ran {
    end: String,
    printed: Vec<String>,
    charged: u64,
}

fn run(program: &Program, entry: &str, case: &Case) -> Ran {
    let start = CellStart {
        target: Target::Entry(Name::new(entry)),
        args: vec![
            case.value.clone(),
            Datum::Int(Integer::from(i64::from(case.mode))),
            Datum::List(case.tail.clone()),
        ],
        session: None,
        owner: super::pages::OWNER,
        modes: Default::default(),
        host: Vec::new(),
    };
    let mut machine = super::pages::start(program.clone(), BOUNDS, start)
        .unwrap_or_else(|error| panic!("{error}"));
    let mut console = Console::default();
    let step = machine
        .run(&mut console, u64::MAX)
        .unwrap_or_else(|error| panic!("{error}"));
    let end = match step {
        Step::Ended(End::Finished(finished)) => format!("ok {:?}", finished.result),
        Step::Ended(End::Error(RunError::Uncaught {
            error: Datum::Error(error),
            ..
        })) => {
            format!("error {}: {}", error.kind, error.message)
        }
        other => format!("{other:?}"),
    };
    Ran {
        end,
        printed: console.lines,
        charged: machine.meters().charged.expect("exact charge"),
    }
}

/// One input: a start value, how `build` shapes it and the arguments after
/// the value (a replacer and a space).
#[derive(Clone, Debug, PartialEq)]
struct Case {
    value: Datum,
    mode: u8,
    tail: Vec<Datum>,
}

/// What `ts.json.plain` should say, written independently of it: the value
/// `build` makes holds only lists, records, null, bools, text and finite
/// numbers; no record has a `toJSON` or `__lash_brand` field or a key that begins
/// with a digit; no container is more than eight deep. The number of values
/// is unbounded, counting each place a shared value is held.
fn plain(case: &Case) -> bool {
    fn walk(value: &Datum, depth: usize) -> bool {
        let members: Vec<&Datum> = match value {
            Datum::Null | Datum::Bool(_) | Datum::Text(_) | Datum::Int(_) => return true,
            Datum::Float(float) => return float.get().is_finite(),
            Datum::List(items) => items.iter().collect(),
            Datum::Record(fields) => {
                if fields.iter().any(|(key, _)| {
                    key == "toJSON"
                        || key == "__lash_brand"
                        || key.starts_with(|c: char| c.is_ascii_digit())
                }) {
                    return false;
                }
                fields.iter().map(|(_, value)| value).collect()
            }
            _ => return false,
        };
        depth <= 8 && members.iter().all(|member| walk(member, depth + 1))
    }
    let value = match case.mode {
        0 => case.value.clone(),
        1 => Datum::List(vec![case.value.clone(), case.value.clone()]),
        7 => Datum::Record(vec![
            ("a".to_owned(), case.value.clone()),
            (
                "b".to_owned(),
                Datum::List(vec![
                    case.value.clone(),
                    Datum::Record(vec![("c".to_owned(), case.value.clone())]),
                ]),
            ),
        ]),
        // A cycle, a toJSON hook or a closure.
        _ => return false,
    };
    walk(&value, 1)
}

/// A deterministic xorshift stream.
struct Bits(u64);

impl Bits {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn float(value: f64) -> Datum {
    Datum::Float(Float::new(value))
}

fn text(value: &str) -> Datum {
    Datum::Text(value.to_owned())
}

/// Leaves JSON writes plainly, and the traps of its spelling: signed zero,
/// exponent thresholds, escapes, control characters, line separators and
/// supplementary characters.
fn leaf(bits: &mut Bits) -> Datum {
    const NUMBERS: [f64; 14] = [
        0.0,
        -0.0,
        1.0,
        -7.0,
        0.1,
        0.30000000000000004,
        1e21,
        1e-7,
        123456789012.5,
        -1e-7,
        5e-324,
        f64::MAX,
        4294967295.0,
        1.5e300,
    ];
    const TEXTS: [&str; 10] = [
        "",
        "alpha",
        "quote\"back\\slash",
        "tab\tline\nfeed",
        "\u{0}\u{1f}\u{7f}",
        "λ ünï",
        "\u{2028}\u{2029}",
        "😀 astral",
        "</script>",
        "0",
    ];
    match bits.below(5) {
        0 => Datum::Null,
        1 => Datum::Bool(bits.below(2) == 0),
        2 => float(NUMBERS[bits.below(NUMBERS.len() as u64) as usize]),
        _ => text(TEXTS[bits.below(TEXTS.len() as u64) as usize]),
    }
}

/// What the plain path must leave to the generic walk.
fn trap(bits: &mut Bits) -> Datum {
    match bits.below(16) {
        0 => Datum::Absent,
        1 => Datum::Tuple(Vec::new()),
        2 => float(f64::NAN),
        3 => float(if bits.below(2) == 0 {
            f64::INFINITY
        } else {
            f64::NEG_INFINITY
        }),
        4 => Datum::Record(vec![("toJSON".to_owned(), text("not callable"))]),
        5 => Datum::Record(vec![("__lash_brand".to_owned(), text("date.invalid"))]),
        6 => Datum::Record(vec![("__lash_brand".to_owned(), float(1.0))]),
        7 => Datum::Record(vec![
            ("b".to_owned(), float(1.0)),
            ("10".to_owned(), float(2.0)),
            ("2".to_owned(), float(3.0)),
        ]),
        8 => Datum::Record(vec![
            ("01".to_owned(), float(1.0)),
            ("1e0".to_owned(), float(2.0)),
        ]),
        9 => Datum::Map(vec![(text("k"), float(1.0))]),
        10 => Datum::Set(vec![text("k")]),
        11 => Datum::Timestamp(Timestamp {
            nanoseconds: Integer::from(1_000_000_000i64),
        }),
        12 => Datum::Int(Integer::from(-12i64)),
        13 => Datum::Tuple(vec![text("ts.object"), text("Math"), text("Math")]),
        14 => Datum::Error(Box::new(ErrorDatum {
            kind: "boom".to_owned(),
            message: "m".to_owned(),
            data: Datum::Null,
        })),
        _ => Datum::Bytes(lash_kernel_doc::Bytes::new(vec![1u8, 2])),
    }
}

/// A random graph of at most `budget` values and `depth` levels; one
/// value in `trap_odds` is a trap.
fn value(bits: &mut Bits, depth: u32, budget: &mut u32, trap_odds: u64) -> Datum {
    *budget = budget.saturating_sub(1);
    if trap_odds != 0 && bits.below(trap_odds) == 0 {
        return trap(bits);
    }
    if depth == 0 || *budget == 0 || bits.below(3) == 0 {
        return leaf(bits);
    }
    let width = bits.below(7);
    if bits.below(2) == 0 {
        Datum::List(
            (0..width)
                .map(|_| value(bits, depth - 1, budget, trap_odds))
                .collect(),
        )
    } else {
        const KEYS: [&str; 12] = [
            "id",
            "name",
            "",
            "-0",
            "a b",
            "λ",
            "\"quoted\"",
            "x9",
            "_",
            "$ref",
            "key\n",
            ":",
        ];
        let mut fields: BTreeMap<&str, Datum> = BTreeMap::new();
        let mut order = Vec::new();
        for _ in 0..width {
            let key = KEYS[bits.below(KEYS.len() as u64) as usize];
            if !fields.contains_key(key) {
                order.push(key);
            }
            fields.insert(key, value(bits, depth - 1, budget, trap_odds));
        }
        Datum::Record(
            order
                .into_iter()
                .map(|key| (key.to_owned(), fields[key].clone()))
                .collect(),
        )
    }
}

/// `depth` lists, one inside the other, around `inner`.
fn nested(depth: usize, inner: Datum) -> Datum {
    (0..depth).fold(inner, |value, _| Datum::List(vec![value]))
}

/// The guard's depth boundary, values beyond the former 128-node cap,
/// and shared and cyclic graphs.
fn edges() -> Vec<Datum> {
    let flat = |count: usize| Datum::List((0..count).map(|n| float(n as f64)).collect());
    vec![
        nested(8, text("deep")),
        nested(9, text("deep")),
        nested(70, Datum::Null),
        flat(127),
        flat(128),
        Datum::Record((0..127).map(|n| (format!("k{n}"), Datum::Null)).collect()),
        Datum::Record((0..128).map(|n| (format!("k{n}"), Datum::Null)).collect()),
        Datum::List(vec![flat(63), flat(63)]),
        Datum::List(vec![flat(63), flat(64)]),
        Datum::List(Vec::new()),
        Datum::Record(Vec::new()),
        text("top-level text"),
        float(-0.0),
        Datum::Null,
        Datum::Absent,
    ]
}

/// Retain the exact producing code only when a graph contains callables.
fn callable_links(value: &Datum) -> BTreeSet<lash_vocabulary::LinkId> {
    let mut links = BTreeSet::new();
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            Datum::Function(callable) => {
                links.insert(callable.link);
            }
            Datum::List(values) | Datum::Tuple(values) | Datum::Set(values) => {
                pending.extend(values)
            }
            Datum::Record(fields) => pending.extend(fields.iter().map(|(_, value)| value)),
            Datum::Map(entries) => {
                pending.extend(entries.iter().flat_map(|(key, value)| [key, value]))
            }
            Datum::Error(error) => pending.push(&error.data),
            _ => {}
        }
    }
    links
}

/// The inputs: generated plain graphs, graphs with traps, the edges, each
/// shaped by every `build` mode, and some with a replacer or a space.
fn cases() -> Vec<(Case, Arc<Program>)> {
    let mut bits = Bits(0x5873_0000_0000_0001);
    let mut values = edges();
    let (generated, retained) = smith_values();
    values.extend(generated);
    for round in 0..160 {
        let mut budget = 40;
        let odds = if round % 2 == 0 { 0 } else { 12 };
        values.push(value(&mut bits, 4, &mut budget, odds));
    }
    let mut programs = BTreeMap::new();
    let library = machine::prepared().clone();
    let mut cases = Vec::new();
    for value in values {
        let roots = callable_links(&value);
        let program = programs
            .entry(roots.clone())
            .or_insert_with(|| Arc::new(program(&retained, &roots, library.clone())))
            .clone();
        for mode in 0..8 {
            cases.push((
                Case {
                    value: value.clone(),
                    mode,
                    tail: Vec::new(),
                },
                Arc::clone(&program),
            ));
        }
        let tail = match bits.below(4) {
            0 => vec![Datum::Absent, Datum::Absent],
            1 => vec![Datum::Null],
            2 => vec![Datum::Absent, float(2.0)],
            _ => vec![Datum::List(vec![text("id"), text("name")]), text("--")],
        };
        cases.push((
            Case {
                value,
                mode: 0,
                tail,
            },
            program,
        ));
    }
    // Repeated prints may produce identical graphs, but their independently
    // drawn tails can differ. Deduplicate only complete, exactly equal cases.
    let mut unique = Vec::new();
    for (case, program) in cases {
        if !unique.iter().any(|(seen, _)| seen == &case) {
            unique.push((case, program));
        }
    }
    println!("JSON classifier distinct cases: {}", unique.len());
    unique
}

/// The plain path writes what the generic walk writes, raises what it
/// raises and prints what it prints, for every generated value and trap.
/// An admitted call costs its guard and the native render and nothing of
/// the walk; a refused one its guard and the whole walk. Each holds up to
/// a constant that depends only on the path, which of the replacer and the
/// space are absent, and whether the call raised.
#[test]
fn plain_json_matches_the_generic_walk_and_costs_its_guard_and_one_path() {
    for (source, code) in [
        (
            r"JSON.stringify('\ud800')",
            crate::DiagnosticCode::LoneSurrogateLiteralUnsupported,
        ),
        (
            "JSON.stringify({get value() { return 1; }})",
            crate::DiagnosticCode::AccessorUnsupported,
        ),
        (
            "JSON.stringify({__proto__: {toJSON() { return 1; }}, value: 2})",
            crate::DiagnosticCode::PrototypeMutationUnsupported,
        ),
        (
            "Object.prototype.toJSON = function() { return 1; }; JSON.stringify({value: 2})",
            crate::DiagnosticCode::PrototypeMutationUnsupported,
        ),
    ] {
        assert_eq!(super::lower(source).unwrap_err().code, code, "{source}");
    }
    let mut admitted_costs: BTreeMap<(Vec<bool>, bool), u64> = BTreeMap::new();
    let mut refused_costs: BTreeMap<(Vec<bool>, bool), u64> = BTreeMap::new();
    let (mut admitted, mut refused) = (0, 0);
    for (case, program) in cases() {
        let fast = run(&program, "fast", &case);
        let generic = run(&program, "walk", &case);
        assert_eq!(
            (&fast.end, &fast.printed),
            (&generic.end, &generic.printed),
            "{case:?}"
        );
        let guard = run(&program, "admits", &case);
        let none = run(&program, "idle", &case);
        let replacer_or_space = case.tail.iter().any(|arg| *arg != Datum::Absent);
        let expected = !replacer_or_space && plain(&case);
        if !replacer_or_space {
            assert_eq!(
                guard.end,
                format!("ok {:?}", Datum::Bool(plain(&case))),
                "{case:?}"
            );
        }
        let guard_cost = guard.charged - none.charged;
        let (path, costs) = if expected {
            admitted += 1;
            let render = run(&program, "draw", &case);
            assert_eq!(fast.end, render.end, "{case:?}");
            (render.charged - none.charged, &mut admitted_costs)
        } else {
            refused += 1;
            let guarded = if replacer_or_space { 0 } else { guard_cost };
            (guarded + generic.charged - none.charged, &mut refused_costs)
        };
        let overhead = (fast.charged - none.charged)
            .checked_sub(path + if expected { guard_cost } else { 0 })
            .unwrap_or_else(|| panic!("{case:?}: {fast:?} {path} {guard_cost}"));
        let first = *costs
            .entry((
                case.tail.iter().map(|arg| *arg == Datum::Absent).collect(),
                fast.end.starts_with("ok"),
            ))
            .or_insert(overhead);
        assert_eq!(overhead, first, "{case:?}");
        if expected {
            assert!(fast.charged < generic.charged, "{case:?}");
        }
    }
    println!(
        "plain json differential: admitted={admitted} refused={refused} admitted_overhead={admitted_costs:?} refused_overhead={refused_costs:?}"
    );
    assert!(admitted > 400 && refused > 400, "{admitted} {refused}");
}

/// Use SC8's real program generator and collect the data it prints, including
/// mutated lists/records, sets, maps and effect results, rather than replacing
/// that grammar with a second random-value generator.
fn smith_values() -> (Vec<Datum>, lash_kernel_package::LinkedContent) {
    use arbitrary::{Arbitrary, Unstructured};
    use lash_kernel_conformance::smith::Smith;
    use lash_kernel_vm::{Outcome, Request};

    let mut values = Vec::new();
    let mut retained = lash_kernel_package::LinkedContent::default();
    for seed in 0..64u64 {
        let mut bits = Bits(seed + 1);
        let bytes: Vec<u8> = (0..512).map(|_| bits.next() as u8).collect();
        let smith = Smith::arbitrary(&mut Unstructured::new(&bytes)).unwrap();
        let session = super::pages::Session::seeded([(
            Name::new("projection"),
            Datum::Handle(Handle {
                kind: "smith".into(),
                id: "projection".into(),
            }),
        )]);
        let start = CellStart {
            target: Target::Main,
            args: Vec::new(),
            owner: super::pages::OWNER,
            session: session.root,
            modes: Default::default(),
            host: Vec::new(),
        };
        // Printed functions retain their original callable identities and captures.
        // Keep the producing code alongside the values for the consumer's decoder.
        let (content, _) = super::linked::content(&smith.document);
        retained.documents.extend(content.documents);
        retained.links.extend(content.links);
        let program =
            crate::tests::linked::program(Arc::new(smith.document), machine::prepared().clone());
        let mut machine =
            super::pages::with_source(program, BOUNDS, start, &session.source).unwrap();
        let mut console = Console::default();
        loop {
            match machine.run(&mut console, u64::MAX).unwrap() {
                Step::Parked(park) => {
                    for request in park.requests {
                        let (wait, outcome) = match request {
                            Request::Effect(effect) => {
                                let answer = if super::linked::operation(&effect.use_).as_str()
                                    == "typescript.sleep"
                                {
                                    Datum::Null
                                } else {
                                    effect.args.first().cloned().expect("host.echo's argument")
                                };
                                (effect.wait, Outcome::Completed(answer))
                            }
                        };
                        machine.deliver(wait, outcome).unwrap();
                    }
                }
                Step::Ended(_) => break,
                Step::Slice => unreachable!(),
            }
        }
        values.extend(console.data);
    }
    assert!(values.len() > 64);
    println!(
        "SC8 classifier inputs: {} prints from 64 generated programs",
        values.len()
    );
    (values, retained)
}
