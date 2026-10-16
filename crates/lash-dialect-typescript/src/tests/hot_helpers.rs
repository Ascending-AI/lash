//! The hottest internal helpers against their previous definitions: each
//! law calls the previous body as a document function or by its retained
//! pin, alongside the helper as the library now defines it, on the same
//! input. The previous body is the oracle for results, errors and prints.

use std::sync::Arc;

use lash_kernel_doc::{Datum, ErrorDatum, Float, Handle, Integer, Name, Timestamp, parse_document};
use lash_kernel_vm::{Bounds, CellStart, End, Host, Machine, Program, RunError, Step, Target};

use super::machine;

const BOUNDS: Bounds = Bounds {
    charge: 50_000_000,
    memory: 256 << 20,
    call_depth: 400,
    live_tasks: 10,
    requests_per_park: 10,
    join_members: 10,
};

/// The library functions the entries name.
const USES: [&str; 25] = [
    "eq",
    "kind",
    "same",
    "num.lt",
    "num.add",
    "num.sub",
    "error.new",
    "list.len",
    "list.copy_deep",
    "record.copy_deep",
    "map.copy_deep",
    "set.copy_deep",
    "record.values",
    "map.values",
    "text.concat",
    "ts.boundary",
    "num.floor",
    "num.abs",
    "num.is_nan",
    "num.to_int",
    "int.to_text",
    "text.len",
    "ts.number.parts",
    "ts.number.layout",
    "ts.number.to_string",
];

/// `ts.boundary` as it was before `collection.holds_none` let plain data
/// cross uncopied.
const FORMER_BOUNDARY: &str = r#"
fn former_boundary(value) {
  let k = kind(value)
  if same(k, "text") { return value }
  if same(k, "float") { return value }
  if same(k, "tuple") {
    if same(value, ()) { return value }
    if same(value[0], "ts.function") { throw error.new("not_data", "A function or a built-in object is not data", null) }
    if same(value[0], "ts.object") { throw error.new("not_data", "A function or a built-in object is not data", null) }
    return value
  }
  if same(k, "set") { return set.copy_deep(value) }
  let copied = value
  if same(k, "list") { set copied = list.copy_deep(value) } else {
    if same(k, "record") { set copied = record.copy_deep(value) } else {
      if same(k, "map") { set copied = map.copy_deep(value) } else { return value }
    }
  }
  let pending = [copied]
  while num.lt(0, list.len(pending)) {
    let last = num.sub(list.len(pending), 1)
    let item = pending[last]
    remove pending[last]
    let children = item
    if same(kind(item), "list") {
      let index = 0
      while num.lt(index, list.len(item)) {
        let child = item[index]
        if same(child, ()) { set item[index] = null }
        if same(child, absent) { set item[index] = null }
        set index = num.add(index, 1)
      }
    } else {
      if same(kind(item), "record") { set children = record.values(item) } else { set children = map.values(item) }
    }
    for child in children {
      let ck = kind(child)
      if same(ck, "float") {} else { if same(ck, "text") {} else {
        if same(ck, "tuple") {
          if same(child, ()) {} else {
            if same(child[0], "ts.function") { throw error.new("not_data", "A function or a built-in object is not data", null) }
            if same(child[0], "ts.object") { throw error.new("not_data", "A function or a built-in object is not data", null) }
          }
        } else {
          if same(ck, "list") { set pending[list.len(pending)] = child }
          if same(ck, "record") { set pending[list.len(pending)] = child }
          if same(ck, "map") { set pending[list.len(pending)] = child }
        }
      } }
    }
  }
  return copied
}
"#;

/// `ts.number.to_string` as it was before integers were asked first.
const FORMER_NUMBER_TO_STRING: &str = r#"
fn former_number_to_string(value) {
  if num.is_nan(value) { return "NaN" }
  if eq(value, inf) { return "Infinity" }
  if eq(value, -inf) { return "-Infinity" }
  if eq(value, 0) { return "0" }
  if eq(num.floor(value), value) {
    if num.lt(num.abs(value), 9007199254740992.0) { return int.to_text(num.to_int(value), 10) }
  }
  let parts = invoke ts.number.parts(value)
  let digits = parts[0]
  let point = num.add(text.len(digits), parts[1])
  let scientific = num.lt(point, -5)
  if num.lt(21, point) { set scientific = true }
  let output = invoke ts.number.layout(digits, point, scientific)
  if num.lt(value, 0) { return text.concat("-", output) }
  return output
}
"#;

/// The entries: `build` shapes the input, then one entry calls the helper
/// and the other the former body. `build` makes what start arguments
/// cannot carry: shared values, cycles, function tokens, closures, holes
/// in place and nesting past the start's depth.
const ENTRIES: &str = r#"
entry boundary(v: Any, mode: Int) -> Any
entry former_boundary_entry(v: Any, mode: Int) -> Any
entry boundary_cost(v: Any, mode: Int) -> Any
entry former_boundary_cost(v: Any, mode: Int) -> Any
entry idle(v: Any, mode: Int) -> Any
entry number_text(v: Any, mode: Int) -> Any
entry former_number_text(v: Any, mode: Int) -> Any
fn nest(v, n) {
  let d = v
  let i = 0
  while num.lt(i, n) { set d = [d] set i = num.add(i, 1) }
  return d
}
fn build(v, mode) {
  let shout = fn(this, args) { print "called" return "shout" }
  if eq(mode, 1) { return [v, v] }
  if eq(mode, 2) { let r = {head: v} set r.self = r return r }
  if eq(mode, 3) { let a = [v] set a[1] = a return a }
  if eq(mode, 4) { return [v, ("ts.function", "", null, null, shout)] }
  if eq(mode, 5) { return {value: v, f: ("ts.object", "Math", "Math")} }
  if eq(mode, 6) { return {value: v, f: shout} }
  if eq(mode, 7) { return {a: v, b: [v, {c: v}]} }
  if eq(mode, 8) { return [v, (), absent] }
  if eq(mode, 9) { return map{"k": [v, ()], "j": {x: [absent]}} }
  if eq(mode, 10) { return {e: error.new("k", "m", [(), v])} }
  if eq(mode, 11) { return [{x: [v]}, set{1, (1, ())}] }
  if eq(mode, 12) { return {brand: v, state: "pending"} }
  if eq(mode, 13) { return ("ts.function", v, null, null, shout) }
  if eq(mode, 14) { return ("ts.object", v, v) }
  if eq(mode, 15) { return shout }
  if eq(mode, 16) { return &nest }
  if eq(mode, 17) { return (v, "ts.function") }
  if num.lt(mode, 200) {} else {
    if num.lt(mode, 300) {
      let n = num.sub(mode, 200)
      let d = call nest(v, n)
      return {a: d, b: [[d]]}
    }
    let n = num.sub(mode, 300)
    let d = call nest(v, n)
    return {b: [[d]], a: d}
  }
  if num.lt(mode, 100) {} else {
    let n = num.sub(mode, 100)
    let d = call nest(v, n)
    return d
  }
  return v
}
fn report(out, x, mode) {
  if num.lt(99, mode) { return (kind(out), eq(out, x)) }
  if eq(mode, 6) { return (out.value, x.value, kind(out.f)) }
  print out
  return (out, x)
}
fn boundary(v, mode) {
  let x = call build(v, mode)
  let out = invoke ts.boundary(x)
  let reported = call report(out, x, mode)
  return reported
}
fn former_boundary_entry(v, mode) {
  let x = call build(v, mode)
  let out = call former_boundary(x)
  let reported = call report(out, x, mode)
  return reported
}
fn boundary_cost(v, mode) {
  let x = call build(v, mode)
  let out = invoke ts.boundary(x)
  return null
}
fn former_boundary_cost(v, mode) {
  let x = call build(v, mode)
  let out = call former_boundary(x)
  return null
}
fn number_text(v, mode) {
  let out = invoke ts.number.to_string(v)
  return out
}
fn former_number_text(v, mode) {
  let out = call former_number_to_string(v)
  return out
}
fn idle(v, mode) {
  let x = call build(v, mode)
  return null
}
main { }
"#;

fn program() -> &'static Program {
    static PROGRAM: std::sync::OnceLock<Program> = std::sync::OnceLock::new();
    PROGRAM.get_or_init(|| {
        let registry = machine::registry();
        let mut text = String::from("kernel 1\nnumbers float\n");
        for name in USES {
            let (id, _) = registry
                .iter()
                .find(|(_, function)| function.definition().name.as_str() == name)
                .unwrap_or_else(|| panic!("{name} is registered"));
            text.push_str(&format!("use {name} = @{id}\n"));
        }
        text.push_str(ENTRIES);
        text.push_str(FORMER_BOUNDARY);
        text.push_str(FORMER_NUMBER_TO_STRING);
        let mut document = parse_document(&text).unwrap_or_else(|error| panic!("{error}"));
        document.manifest.functions =
            lash_kernel_check::requirements(&document, registry.as_ref()).functions;
        Program {
            release: super::pages::RELEASE,
            document: Arc::new(document),
            library: machine::prepared().clone(),
        }
    })
}

#[derive(Default)]
struct Console {
    lines: Vec<String>,
}

impl Host for Console {
    fn clock(&mut self) -> Timestamp {
        Timestamp {
            nanoseconds: Integer::from(0),
        }
    }

    fn random(&mut self) -> u64 {
        0
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

fn run(entry: &str, value: &Datum, mode: u16) -> Ran {
    let start = CellStart {
        target: Target::Entry(Name::new(entry)),
        args: vec![value.clone(), Datum::Int(Integer::from(i64::from(mode)))],
        session: None,
        owner: super::pages::OWNER,
        modes: Default::default(),
        host: Vec::new(),
    };
    let mut machine = super::pages::start(program().clone(), BOUNDS, start)
        .unwrap_or_else(|error| panic!("{error}"));
    let mut console = Console::default();
    let step = machine
        .run(&mut console, u64::MAX)
        .unwrap_or_else(|error| panic!("{error}"));
    let end = match step {
        Step::Ended(End::Finished(finished)) => format!("ok {:?}", finished.result),
        Step::Ended(End::Error(RunError::Uncaught(Datum::Error(error)))) => {
            format!("error {}: {}", error.kind, error.message)
        }
        other => format!("{other:?}"),
    };
    Ran {
        end,
        printed: console.lines,
        charged: machine.meters().charged,
    }
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

/// Numbers and their spelling's traps: signed zero, NaN, the infinities,
/// the 2^53 edge, exponent thresholds, the smallest subnormal and the
/// largest float.
const NUMBERS: [f64; 22] = [
    0.0,
    -0.0,
    1.0,
    -7.0,
    0.5,
    0.1,
    0.30000000000000004,
    1e21,
    1e-7,
    1e-6,
    123456789012.5,
    -1e-7,
    5e-324,
    f64::MAX,
    4294967295.0,
    9007199254740991.0,
    9007199254740992.0,
    -9007199254740993.0,
    1.5e300,
    f64::NAN,
    f64::INFINITY,
    f64::NEG_INFINITY,
];

/// A leaf of any kind a start value can carry.
fn leaf(bits: &mut Bits) -> Datum {
    const TEXTS: [&str; 9] = [
        "",
        "alpha",
        "brand:x",
        "ts.function",
        "ts.object",
        "Promise",
        "Math",
        "λ 😀",
        "0",
    ];
    match bits.below(12) {
        0 => Datum::Null,
        1 => Datum::Bool(bits.below(2) == 0),
        2 | 3 => float(NUMBERS[bits.below(NUMBERS.len() as u64) as usize]),
        4 | 5 => text(TEXTS[bits.below(TEXTS.len() as u64) as usize]),
        6 => Datum::Absent,
        7 => Datum::Tuple(Vec::new()),
        8 => Datum::Int(Integer::from(-12i64)),
        9 => match bits.below(4) {
            0 => Datum::Tuple(vec![text("ts.object"), text("Math"), text("Math")]),
            1 => Datum::Tuple(vec![text("ts.function"), text("parseInt")]),
            2 => Datum::Tuple(vec![text("pair"), float(1.0)]),
            _ => Datum::Tuple(vec![Datum::Absent]),
        },
        10 => Datum::Timestamp(Timestamp {
            nanoseconds: Integer::from(1_000_000_000i64),
        }),
        _ => match bits.below(3) {
            0 => Datum::Error(Box::new(ErrorDatum {
                kind: "boom".to_owned(),
                message: "m".to_owned(),
                data: Datum::List(vec![Datum::Tuple(Vec::new())]),
            })),
            1 => Datum::Bytes(lash_kernel_doc::Bytes::new(vec![1u8, 2])),
            _ => Datum::Set(vec![text("k"), float(2.0)]),
        },
    }
}

/// A random tree of at most `budget` values and `depth` levels, its leaves
/// plain unless `traps` is set.
fn value(bits: &mut Bits, depth: u32, budget: &mut u32, traps: bool) -> Datum {
    *budget = budget.saturating_sub(1);
    if depth == 0 || *budget == 0 || bits.below(3) == 0 {
        let leaf = leaf(bits);
        let plain = matches!(
            leaf,
            Datum::Null | Datum::Bool(_) | Datum::Float(_) | Datum::Text(_) | Datum::Int(_)
        );
        return if traps || plain { leaf } else { Datum::Null };
    }
    let width = bits.below(6);
    let shape = bits.below(5);
    let members: Vec<Datum> = (0..width)
        .map(|_| value(bits, depth - 1, budget, traps))
        .collect();
    match shape {
        0 | 1 => Datum::List(members),
        2 => Datum::Map(
            members
                .into_iter()
                .enumerate()
                .map(|(index, member)| (text(&format!("k{index}")), member))
                .collect(),
        ),
        _ => Datum::Record(
            members
                .into_iter()
                .enumerate()
                .map(|(index, member)| (format!("f{index}"), member))
                .collect(),
        ),
    }
}

/// The start values: every leaf kind, generated plain trees and trees
/// with traps.
fn values(seed: u64, rounds: u32) -> Vec<Datum> {
    let mut bits = Bits(seed);
    let mut values: Vec<Datum> = NUMBERS.iter().map(|&number| float(number)).collect();
    values.extend((0..48).map(|_| leaf(&mut bits)));
    for round in 0..rounds {
        let mut budget = 30;
        values.push(value(&mut bits, 4, &mut budget, round % 2 == 1));
    }
    values
}

/// `ts.boundary` answers what its former body answered, raises what it
/// raised and prints what it printed, and leaves the guest's value as it
/// left it: for generated plain values and traps (holes, absent slots,
/// function and built-in tokens, raw closures, other tuples, errors, maps,
/// sets, shared values, cycles), and for nesting at and past the deep
/// copy's 128 levels, also where the order a shared value is first reached
/// decides its depth. A plain container costs less than before.
#[test]
fn the_boundary_matches_its_former_copy_and_walk() {
    let mut modes: Vec<u16> = (0..12).collect();
    modes.extend([125, 126, 127, 128, 129, 224, 225, 226, 324, 325, 326]);
    let mut cases = 0;
    for value in values(0x5885_0000_0000_0001, 120) {
        for &mode in &modes {
            let now = run("boundary", &value, mode);
            let before = run("former_boundary_entry", &value, mode);
            assert_eq!(
                (&now.end, &now.printed),
                (&before.end, &before.printed),
                "{value:?} mode {mode}"
            );
            cases += 1;
        }
    }
    let mut bits = Bits(0x5885_0000_0000_0002);
    let mut containers = 0;
    while containers < 60 {
        let mut budget = 30;
        let value = value(&mut bits, 4, &mut budget, false);
        if !matches!(value, Datum::List(_) | Datum::Map(_) | Datum::Record(_)) {
            continue;
        }
        containers += 1;
        for mode in [0, 1, 7] {
            let idle = run("idle", &value, mode).charged;
            let now = run("boundary_cost", &value, mode).charged - idle;
            let before = run("former_boundary_cost", &value, mode).charged - idle;
            assert!(now < before, "{now} {before} {value:?} mode {mode}");
        }
    }
    println!("boundary differential: cases={cases} plain containers={containers}");
}

/// `ts.number.to_string` spells every number as its former body did: the
/// spelling traps (signed zero, NaN, the infinities, the 2^53 edge,
/// exponent thresholds, subnormals, the largest float), kernel integers
/// small and past 2^53, and generated floats of every exponent, integral
/// ones up to 2^64 among them.
#[test]
fn numbers_are_spelled_as_their_former_body_spelled_them() {
    let mut bits = Bits(0x5885_0000_0000_0004);
    let mut numbers: Vec<Datum> = NUMBERS.iter().map(|&number| float(number)).collect();
    for integer in [0i64, 1, -1, 42, 9_007_199_254_740_993, -(1i64 << 62)] {
        numbers.push(Datum::Int(Integer::from(integer)));
    }
    for _ in 0..1500 {
        let number = match bits.below(3) {
            0 => f64::from_bits(bits.next()),
            1 => {
                (bits.next() >> bits.below(64)) as f64 * if bits.below(2) == 0 { 1.0 } else { -1.0 }
            }
            _ => (bits.below(2_000_000) as f64 - 1_000_000.0) / 8.0,
        };
        numbers.push(float(number));
    }
    for number in &numbers {
        let now = run("number_text", number, 0);
        let before = run("former_number_text", number, 0);
        assert_eq!(
            (&now.end, &now.printed),
            (&before.end, &before.printed),
            "{number:?}"
        );
    }
    println!("number spelling differential: cases={}", numbers.len());
}

// The retained definition is addressed by its original identity, never by name.
const RETAINED_MAP_KEY: &str = r#"
use kind
use same
use eq
use ref
function ts.map.key(value: Any) -> Any
kernel 1
errors "type_error", "RangeError"
charge sum(8, deep(result))
body {
  let k = kind(value)
  if same(k, "absent") { return ("undefined", "key") }
  for object in ["list", "map", "set", "record", "closure", "task"] {
    if same(k, object) { return ref(value) }
  }
  if same(k, "error") { return ("error", ref(value.data), value.kind, value.message) }
  if same(k, "tuple") {
    if same(value, ()) {} else {
      # A function's token holds the closure that calls it, which is keyed
      # as the object it is, or the reference to the declared function a
      # saved one is, which is a key itself; the deleted record of one the
      # program made is keyed as the object it is.
      if same(value[0], "ts.function") {
        let callable = value[4]
        if same(kind(callable), "function") {} else { set callable = ref(callable) }
        if same(value[1], "") { return ("ts.function", "", value[2], value[3], callable, ref(value[5])) }
        return ("ts.function", value[1], value[2], value[3], callable)
      }
    }
  }
  if same(k, "float") { if eq(value, 0) { return 0.0 } }
  return value
}
"#;

/// FIG-6105 and K-CHG-007: numeric admission allocates no key list and
/// charges its executed body. K-MACH-007/008: both the new body and a
/// retained old pin resume with their own results and exact charges.
#[test]
fn numeric_map_keys_charge_only_their_body_and_retained_pins_resume() {
    use lash_kernel_dialect::{Library, NamedLibrary, define_functions};
    use lash_kernel_vm::PreparedLibrary;

    let mut registry = machine::registry().as_ref().clone();
    let current = NamedLibrary::from_registry(&registry)
        .unwrap()
        .resolve("ts.map.key")
        .unwrap();
    let mut retained_names = NamedLibrary::from_registry(&super::kernel_registry()).unwrap();
    let retained = define_functions(RETAINED_MAP_KEY, &mut retained_names)
        .unwrap()
        .pop()
        .unwrap();
    let old = retained.identity();
    if old != current {
        registry.register(retained, None).unwrap();
    }
    let library = PreparedLibrary::new(Arc::new(registry));
    let make_program = |id| {
        let document = parse_document(&format!(
            "kernel 1\nnumbers by_spelling\nuse key = @{id}\nentry key(v: Any) -> Any\nfn key(v) {{ let out = invoke key(v) return out }}\nmain {{}}"
        ))
        .unwrap();
        Program {
            release: super::pages::RELEASE,
            document: Arc::new(document),
            library: library.clone(),
        }
    };
    let run = |program: Program, value: Datum, relay: bool| {
        let source = super::pages::Source::default();
        let mut machine = super::pages::with_source(
            program.clone(),
            BOUNDS,
            CellStart {
                target: Target::Entry(Name::new("key")),
                args: vec![value],
                session: None,
                owner: super::pages::OWNER,
                modes: Default::default(),
                host: Vec::new(),
            },
            lash_kernel_vm::Layout::default(),
            &source,
        )
        .unwrap();
        if !relay {
            machine.count_work();
        }
        let mut host = Console::default();
        loop {
            match machine
                .run(&mut host, if relay { 0 } else { u64::MAX })
                .unwrap()
            {
                Step::Slice => {
                    super::pages::reopen(
                        &mut machine,
                        &program,
                        BOUNDS,
                        &source,
                        lash_kernel_vm::Layout::default(),
                    );
                }
                Step::Ended(end) => return (end, machine.meters().charged, machine.work()),
                other => panic!("a key never parks: {other:?}"),
            }
        }
    };
    let fast = make_program(current);
    let retained = make_program(old);
    for value in [
        Datum::Int(0.into()),
        Datum::Int(7.into()),
        Datum::Int(Integer::parse("9223372036854775808").unwrap()),
        Datum::Float(Float::new(-0.0)),
        Datum::Float(Float::new(0.0)),
        Datum::Float(Float::new(7.5)),
        Datum::Float(Float::new(f64::NAN)),
        Datum::Float(Float::new(f64::INFINITY)),
    ] {
        let (end, charge, work) = run(fast.clone(), value.clone(), false);
        let work = work.unwrap();
        // Entry call/return: 4 + deep(result). The helper's return,
        // call expression, three arguments and two tuple writes cost 9;
        // atomic key.identity costs 1 + deep(result).
        let deep = match &value {
            Datum::Int(n) => 1 + n.bits().div_ceil(64),
            Datum::Float(_) => 1,
            _ => unreachable!(),
        };
        let expected_charge = 4 + deep + 9 + 1 + deep;
        assert_eq!(charge, expected_charge, "K-CHG-007: {value:?}");
        assert_eq!(
            work.heap_insertions, 0,
            "numeric key admission needs no list"
        );
        let (relayed, relayed_charge, _) = run(fast.clone(), value.clone(), true);
        assert_eq!((relayed, relayed_charge), (end.clone(), charge));
        let (old_end, old_charge, _) = run(retained.clone(), value.clone(), false);
        assert_eq!(old_end, end);
        assert!(old_charge > charge, "less work costs less");
        let (relayed_old, relayed_old_charge, _) = run(retained.clone(), value, true);
        assert_eq!((relayed_old, relayed_old_charge), (old_end, old_charge));
    }
}
