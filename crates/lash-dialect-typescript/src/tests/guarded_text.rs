//! Guarded wrappers preserve the generic helpers' results, typed failures and
//! coercion order. The oracle is the unchanged generic body, on identical inputs.

use std::sync::{Arc, OnceLock};

use lash_kernel_doc::{Datum, ErrorDatum, Float, Handle, Integer};

use lash_kernel_vm::{Bounds, CellStart, End, Host, Machine, Program, RunError, Step, Target};
use lash_vocabulary::Name;

use super::machine;

#[derive(Default)]
struct Console(Vec<Datum>);

impl Host for Console {
    fn read(&mut self, _handle: &Handle, _request: &Datum) -> Result<Datum, ErrorDatum> {
        panic!("these laws read no projection")
    }
    fn print(&mut self, value: &Datum) {
        self.0.push(value.clone());
    }
    fn cancel_requested(&mut self) -> bool {
        false
    }
}

fn program() -> &'static Program {
    static PROGRAM: OnceLock<Program> = OnceLock::new();
    PROGRAM.get_or_init(|| {
        let methods = METHODS;
        let registry = machine::registry();
        let mut source = ["eq", "num.lt", "list.len", "error.new"].into_iter().map(|name| format!("use {name}\n")).collect::<String>();
        for name in methods.iter().flat_map(|name| [name.to_string(), format!("{name}_generic")]) { source.push_str(&format!("use {name}\n")); }
        for entry in ["fast", "walk"] {
            source.push_str(&format!("function {entry}(v: Any, args: List(Any), method: Int, mode: Int) -> Any\nkernel 1\nmetering statements\nbody {{\n"));
            source.push_str(r#"
  if eq(mode, 1) { set v = {toString: fn(holder, passed) { print "receiver" return "a😀beta" }} }
  if eq(mode, 2) { set args[0] = {toString: fn(holder, passed) { print "needle" return "beta" }} }
  if eq(mode, 3) { set args[1] = {valueOf: fn(holder, passed) { print "position" return 2.9 }} }
  if eq(mode, 4) { set args[0] = {__lash_brand: ("regex.ecma", "reg_exp"), __lash_built: b"", pattern: "a", flags: "", lastIndex: 0} }
  if eq(mode, 6) { set v = [[1], (), 3] }
  if eq(mode, 5) {
    set v = {toString: fn(holder, passed) { print "receiver" throw error.new("type_error", "receiver first", null) }}
    set args[0] = {toString: fn(holder, passed) { print "needle" throw error.new("type_error", "needle first", null) }}
    set args[1] = {valueOf: fn(holder, passed) { print "position" throw error.new("type_error", "position first", null) }}
  }
"#);
            for (index, name) in methods.iter().enumerate() {
                let suffix = if entry == "walk" { "_generic" } else { "" };
                source.push_str(&format!("if eq(method, {index}) {{ let out = invoke {name}{suffix}(v, args) if eq(mode, 6) {{ if num.lt(0, list.len(out)) {{ set out[0][0] = 9 }} return (v, out) }} return out }}\n"));
            }
            source.push_str("return null\n}\n");
        }
        let document = super::linked::definitions(&source, &["fast", "walk"], super::library());
        crate::tests::linked::program(Arc::new(document),machine::prepared().clone())
    })
}

const METHODS: &[&str] = &[
    "ts.string.includes",
    "ts.string.indexOf",
    "ts.string.startsWith",
    "ts.string.endsWith",
    "ts.array.slice",
    "ts.string.slice",
    "ts.string.substring",
    "ts.string.at",
    "ts.string.split",
];

fn run(
    entry: &str,
    value: &Datum,
    args: &[Datum],
    method: usize,
    mode: u8,
) -> (String, Vec<Datum>) {
    let mut machine = super::pages::start(
        program().clone(),
        Bounds {
            memory_meter: lash_kernel_vm::MemoryMeter::On,
            charge: lash_kernel_vm::Charge::Exact,
            charge_limit: 1_000_000,
            memory: 16 << 20,
            call_depth: 200,
            live_tasks: 10,
            requests_per_park: 10,
            join_members: 10,
        },
        CellStart {
            target: Target::Entry(Name::new(entry)),
            args: vec![
                value.clone(),
                Datum::List(args.to_vec()),
                Datum::Int(Integer::from(method as i64)),
                Datum::Int(Integer::from(i64::from(mode))),
            ],
            session: None,
            owner: super::pages::OWNER,
            modes: Default::default(),
            host: Vec::new(),
        },
    )
    .unwrap();
    let mut console = Console::default();
    let end = match machine.run(&mut console, u64::MAX).unwrap() {
        Step::Ended(End::Finished(result)) => format!("ok {:?}", result.result),
        Step::Ended(End::Error(RunError::Uncaught {
            error: Datum::Error(error),
            ..
        })) => {
            format!("error {}: {}", error.kind, error.message)
        }
        other => panic!("unexpected end: {other:?}"),
    };
    (end, console.0)
}

fn float(n: f64) -> Datum {
    Datum::Float(Float::new(n))
}
fn text(s: &str) -> Datum {
    Datum::Text(s.to_owned())
}

/// K-DIALECT-001: admission removes only coercion/padding, never UTF-16
/// positions, numeric normalization, RegExp rejection or coercion ordering.
#[test]
fn primitive_search_matches_generic_results_errors_and_coercion_order() {
    let values = [
        "",
        "alpha/beta/gamma",
        "a😀beta",
        "éλ中",
        "😀😀",
        "aaaa",
        "a\0b",
    ];
    let needles = ["", "a", "beta", "😀", "λ", "absent", "aaa"];
    let positions = [
        Datum::Absent,
        float(-0.0),
        float(f64::NAN),
        float(f64::INFINITY),
        float(f64::NEG_INFINITY),
        float(-2.9),
        float(2.9),
        float(1e100),
        float(1.0),
        Datum::Int(Integer::from(2)),
        Datum::Null,
        text("2"),
        Datum::Bool(true),
    ];
    let mut pairs = 0;
    for method in 0..4 {
        for value in values {
            for needle in needles {
                for args in [
                    vec![],
                    vec![text(needle)],
                    vec![text(needle), Datum::Absent, text("ignored")],
                ]
                .into_iter()
                .chain(
                    positions
                        .iter()
                        .map(|position| vec![text(needle), position.clone()]),
                ) {
                    assert_eq!(
                        run("fast", &text(value), &args, method, 0),
                        run("walk", &text(value), &args, method, 0),
                        "{method} {value:?} {args:?}"
                    );
                    pairs += 1;
                }
            }
        }
        for value in [
            Datum::Null,
            Datum::Absent,
            float(12.0),
            Datum::Bool(true),
            text("a😀beta"),
        ] {
            for mode in 0..6 {
                let args = [text("beta"), float(2.0)];
                assert_eq!(
                    run("fast", &value, &args, method, mode),
                    run("walk", &value, &args, method, mode),
                    "{method} {value:?} mode={mode}"
                );
                pairs += 1;
            }
        }
    }
    // Generated texts include scalar widths, empty needles and repeated prefixes.
    let mut bits = 0x5884_0000_0000_0001u64;
    let alphabet = ['a', 'b', 'λ', '😀', '\0'];
    for _ in 0..128 {
        let mut value = String::new();
        for _ in 0..12 {
            bits ^= bits << 13;
            bits ^= bits >> 7;
            bits ^= bits << 17;
            value.push(alphabet[(bits % alphabet.len() as u64) as usize]);
        }
        let needle: String = value
            .chars()
            .skip((bits % 8) as usize)
            .take((bits % 4) as usize)
            .collect();
        let args = [text(&needle), float((bits % 30) as f64 - 12.5)];
        for method in 0..4 {
            assert_eq!(
                run("fast", &text(&value), &args, method, 0),
                run("walk", &text(&value), &args, method, 0)
            );
            pairs += 1;
        }
    }
    println!("primitive search differential pairs={pairs}");
}

/// K-DIALECT-001: slicing keeps shallow identity, holes, UTF-16 refusals,
/// relative vs swapped bounds, and coercion order on generic receivers.
#[test]
fn primitive_slices_match_generic_bounds_holes_identity_and_utf16_failures() {
    let positions = [
        Datum::Absent,
        float(-0.0),
        float(f64::NAN),
        float(f64::INFINITY),
        float(f64::NEG_INFINITY),
        float(-2.9),
        float(2.9),
        float(1e100),
        float(1.0),
        Datum::Null,
        text("2"),
        Datum::Bool(true),
    ];
    let values = [
        text(""),
        text("a😀bc"),
        text("éλ中"),
        Datum::List(vec![]),
        Datum::List(vec![
            float(1.0),
            Datum::Tuple(vec![]),
            Datum::List(vec![float(3.0)]),
            Datum::Absent,
        ]),
        Datum::Null,
        Datum::Absent,
        Datum::Record(vec![
            ("length".to_owned(), float(3.0)),
            ("0".to_owned(), text("first")),
            ("2".to_owned(), text("last")),
        ]),
    ];
    let mut pairs = 0;
    for method in 4..8 {
        for value in &values {
            for lower in &positions {
                for upper in &positions {
                    let args = [lower.clone(), upper.clone()];
                    assert_eq!(
                        run("fast", value, &args, method, 0),
                        run("walk", value, &args, method, 0),
                        "{method} {value:?} {args:?}"
                    );
                    pairs += 1;
                }
            }
            for mode in 0..6 {
                let args = [float(0.0), float(2.0)];
                assert_eq!(
                    run("fast", value, &args, method, mode),
                    run("walk", value, &args, method, mode)
                );
                pairs += 1;
            }
            for args in [vec![], vec![float(-1.0)]] {
                assert_eq!(
                    run("fast", value, &args, method, 0),
                    run("walk", value, &args, method, 0)
                );
                pairs += 1;
            }
        }
    }
    assert_eq!(
        run("fast", &Datum::Null, &[float(0.0), float(2.0)], 4, 6),
        run("walk", &Datum::Null, &[float(0.0), float(2.0)], 4, 6)
    );
    // Generated scalar strings hit both boundaries around surrogate pairs.
    for width in 0..16 {
        let value = text(&"a😀λ".repeat(width));
        for bound in -4..20 {
            for method in 5..8 {
                let args = [float(f64::from(bound)), float(f64::from(bound + 3))];
                assert_eq!(
                    run("fast", &value, &args, method, 0),
                    run("walk", &value, &args, method, 0)
                );
                pairs += 1;
            }
        }
    }
    println!("primitive slice differential pairs={pairs}");
}

/// K-DIALECT-001: plain split is limit-aware and keeps adjacent/trailing
/// empty pieces; modulo limits, empty separators and regex stay generic.
#[test]
fn plain_split_matches_generic_limits_pieces_errors_and_coercion_order() {
    let limits = [
        Datum::Absent,
        float(-0.0),
        float(0.0),
        float(1.0),
        float(2.0),
        float(4294967295.0),
        float(4294967296.0),
        float(-1.0),
        float(-2.9),
        float(2.9),
        float(f64::NAN),
        float(f64::INFINITY),
        Datum::Null,
        text("2"),
    ];
    let mut pairs = 0;
    for width in 0..16 {
        let value = text(&"a😀,,λ,".repeat(width));
        for separator in ["", ",", "😀", "λ", "a😀", "absent", ",,"] {
            for limit in &limits {
                let args = [text(separator), limit.clone()];
                assert_eq!(
                    run("fast", &value, &args, 8, 0),
                    run("walk", &value, &args, 8, 0),
                    "{value:?} {args:?}"
                );
                pairs += 1;
            }
            for args in [vec![], vec![Datum::Absent], vec![text(separator)]] {
                assert_eq!(
                    run("fast", &value, &args, 8, 0),
                    run("walk", &value, &args, 8, 0)
                );
                pairs += 1;
            }
        }
    }
    for value in [text("a😀beta"), Datum::Null, Datum::Absent, float(12.0)] {
        for mode in 0..6 {
            let args = [text("beta"), float(2.0)];
            assert_eq!(
                run("fast", &value, &args, 8, mode),
                run("walk", &value, &args, 8, mode)
            );
            pairs += 1;
        }
    }
    println!("plain split differential pairs={pairs}");
}
