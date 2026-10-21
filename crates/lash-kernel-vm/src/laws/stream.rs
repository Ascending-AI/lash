//! The flat stream (`lash_kernel_exec::Op`): each statement runs as one
//! range of decoded ops. Its expression nodes are charged at their
//! pre-order positions by charge regions, which every layout but the
//! natural one splits at random (`K-CHG-002`). An odd layout runs the
//! general path: no prepared native entry or operator opcode
//! (`K-MACH-008`).

use lash_kernel_doc::Datum;
use lash_kernel_doc::heap::PageId;
use lash_vocabulary::names::Name;

use super::embedder::{ROOMY, Source, World, census, library};
use crate::{
    Bound, Bounds, CellStart, End, Layout, Machine, PreparedLibrary, Program, RunError, Step,
    Target,
};

/// A generated expression over two variables, `a` (7) and `b` (`[1, 2,
/// 3]`), and one that no scope binds.
enum Node {
    Int(u64),
    Text,
    A,
    B,
    Unbound,
    Tuple(Vec<Node>),
    List(Vec<Node>),
    Record(Vec<Node>),
    Map(Vec<Node>),
    /// A record's field, by its position.
    Field(Box<Node>, usize),
    Index(Box<Node>, Box<Node>),
    Add(Box<Node>, Box<Node>),
    Len(Box<Node>),
    Twice(Box<Node>),
}

/// Xorshift: the trees are the same on every run.
struct Seeded(u64);

impl Seeded {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// A number-valued node, with its value.
fn number(random: &mut Seeded, depth: u32) -> (Node, u64) {
    let leaf = depth == 0 || random.below(3) == 0;
    match if leaf {
        random.below(3)
    } else {
        3 + random.below(5)
    } {
        0 => {
            let value = random.below(9);
            (Node::Int(value), value)
        }
        1 => (Node::A, 7),
        2 => {
            let at = random.below(3);
            (
                Node::Index(Box::new(Node::B), Box::new(Node::Int(at))),
                at + 1,
            )
        }
        3 => {
            let (x, a) = number(random, depth - 1);
            let (y, b) = number(random, depth - 1);
            (Node::Add(Box::new(x), Box::new(y)), a + b)
        }
        4 => {
            let items: Vec<Node> = (0..random.below(4))
                .map(|_| any(random, depth - 1))
                .collect();
            let len = items.len() as u64;
            (Node::Len(Box::new(Node::List(items))), len)
        }
        5 => {
            let (x, value) = number(random, depth - 1);
            (Node::Twice(Box::new(x)), value * 2)
        }
        6 => {
            let len = 1 + random.below(3);
            let at = random.below(len);
            let mut value = 0;
            let items = (0..len)
                .map(|index| {
                    let (item, item_value) = number(random, depth - 1);
                    if index == at {
                        value = item_value;
                    }
                    item
                })
                .collect();
            (
                Node::Index(Box::new(Node::List(items)), Box::new(Node::Int(at))),
                value,
            )
        }
        _ => {
            let len = 1 + random.below(2);
            let at = random.below(len) as usize;
            let mut value = 0;
            let fields = (0..len as usize)
                .map(|index| {
                    if index == at {
                        let (field, field_value) = number(random, depth - 1);
                        value = field_value;
                        field
                    } else {
                        any(random, depth - 1)
                    }
                })
                .collect();
            (Node::Field(Box::new(Node::Record(fields)), at), value)
        }
    }
}

/// A node of any value, or one that raises.
fn any(random: &mut Seeded, depth: u32) -> Node {
    let leaf = depth == 0 || random.below(3) == 0;
    match if leaf {
        random.below(4)
    } else {
        4 + random.below(5)
    } {
        0 => Node::Text,
        1 => Node::B,
        2 if random.below(4) == 0 => Node::Unbound,
        2 | 3 => number(random, 0).0,
        4 => number(random, depth).0,
        5 => Node::Tuple(
            (0..2 + random.below(2))
                .map(|_| any(random, depth - 1))
                .collect(),
        ),
        6 => Node::List(
            (0..random.below(4))
                .map(|_| any(random, depth - 1))
                .collect(),
        ),
        7 => Node::Record(
            (0..1 + random.below(2))
                .map(|_| any(random, depth - 1))
                .collect(),
        ),
        _ => Node::Map(
            (0..1 + random.below(2))
                .map(|_| any(random, depth - 1))
                .collect(),
        ),
    }
}

fn text(node: &Node) -> String {
    let list = |items: &[Node]| items.iter().map(text).collect::<Vec<_>>().join(", ");
    match node {
        Node::Int(value) => value.to_string(),
        Node::Text => "\"t\"".to_owned(),
        Node::A => "a".to_owned(),
        Node::B => "b".to_owned(),
        Node::Unbound => "nope".to_owned(),
        Node::Tuple(items) => format!("({})", list(items)),
        Node::List(items) => format!("[{}]", list(items)),
        Node::Record(fields) => format!(
            "{{{}}}",
            fields
                .iter()
                .enumerate()
                .map(|(at, field)| format!("f{at}: {}", text(field)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Node::Map(values) => format!(
            "map{{{}}}",
            values
                .iter()
                .enumerate()
                .map(|(at, value)| format!("\"k{at}\": {}", text(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Node::Field(record, at) => format!("{}.f{at}", text(record)),
        Node::Index(target, index) => format!("{}[{}]", text(target), text(index)),
        Node::Add(x, y) => format!("num.add({}, {})", text(x), text(y)),
        Node::Len(items) => format!("list.len({})", text(items)),
        Node::Twice(x) => format!("pair.twice({})", text(x)),
    }
}

/// A value's size (`K-CHG-004`) for the integers the trees make.
fn size(value: u64) -> u64 {
    if value == 0 { 1 } else { 2 }
}

/// The charge events evaluating `node` makes, one at a time, in the
/// order `K-CHG-002` states: a node is charged before what is under it,
/// a construction its members after them, a call its formula after its
/// arguments. `false`: it raised after the last event.
fn events(node: &Node, out: &mut Vec<u64>) -> bool {
    out.push(1);
    let members = |items: &[Node], keyed: bool, out: &mut Vec<u64>| {
        for item in items {
            if keyed {
                out.push(1);
            }
            if !events(item, out) {
                return false;
            }
        }
        if !items.is_empty() {
            out.push(items.len() as u64);
        }
        true
    };
    match node {
        Node::Int(_) | Node::Text | Node::A | Node::B => true,
        Node::Unbound => false,
        Node::Tuple(items) | Node::List(items) | Node::Record(items) => members(items, false, out),
        Node::Map(values) => members(values, true, out),
        Node::Field(record, _) => events(record, out),
        Node::Index(target, index) => events(target, out) && events(index, out),
        Node::Add(x, y) => {
            let ran = events(x, out) && events(y, out);
            if ran {
                out.push(1);
            }
            ran
        }
        Node::Len(items) => {
            let ran = events(items, out);
            if ran {
                out.push(1);
            }
            ran
        }
        Node::Twice(x) => {
            let ran = events(x, out);
            if ran {
                out.push(5 + size(value(x)));
            }
            ran
        }
    }
}

/// A number node's value.
fn value(node: &Node) -> u64 {
    match node {
        Node::Int(value) => *value,
        Node::A => 7,
        Node::Index(target, index) => match target.as_ref() {
            Node::B => value(index) + 1,
            Node::List(items) => value(&items[value(index) as usize]),
            _ => unreachable!("an index reads a list"),
        },
        Node::Field(record, at) => match record.as_ref() {
            Node::Record(fields) => value(&fields[*at]),
            _ => unreachable!("a field reads a record"),
        },
        Node::Add(x, y) => value(x) + value(y),
        Node::Len(items) => match items.as_ref() {
            Node::List(items) => items.len() as u64,
            _ => unreachable!("a length reads a list"),
        },
        Node::Twice(x) => value(x) * 2,
        _ => unreachable!("a number node"),
    }
}

/// Each way the laws run a tree: the stream as the natural layout runs
/// it, in whole charge regions with operator opcodes; an odd layout's
/// general path; and an even shuffled layout with `pair.twice` run as its
/// kernel body. Both others split their regions at random.
const RUNS: [(Layout, bool); 3] = [(Layout(0), true), (Layout(1), true), (Layout(2), false)];

fn program(expression: &str, layout: Layout, native_twice: bool) -> Program {
    let library = library(native_twice);
    let text = format!(
        "numbers by_spelling\n{}definition \"function go() -> Any\\nkernel 1\\nmetering statements\\nbody {{  let a = 7 let b = [1, 2, 3] let x = {expression_json} return 0  }}\"\n\nmain {{}}\n",
        library.header,
        expression_json = super::embedder::fragment(expression)
    );
    let document = super::embedder::document(&text, &library.registry);
    crate::laws::embedder::program(
        std::sync::Arc::new(document),
        PreparedLibrary::with_layout(library.registry, layout),
    )
}

/// How the run of `program` under a charge bound of `charge` ends, and
/// what it charged.
fn ends(program: &Program, layout: Layout, charge: u64) -> (End, u64) {
    let start = CellStart {
        target: Target::Entry(Name::new("go")),
        args: Vec::new(),
        session: None,
        owner: PageId([9; 32]),
        modes: crate::EffectModes::new(),
        host: Vec::new(),
    };
    let bounds = Bounds {
        charge_limit: charge,
        ..ROOMY
    };
    let mut machine = crate::KernelMachine::start_with_census(
        program.clone(),
        bounds,
        start,
        layout,
        Box::new(Source::default()),
        census(),
    )
    .unwrap_or_else(|error| panic!("{error}"));
    match machine.run(&mut World::default(), u64::MAX) {
        Ok(Step::Ended(end)) => (end, machine.meters().charged.expect("exact charge")),
        other => panic!("the run waits on nothing: {other:?}"),
    }
}

fn halted(end: &End) -> bool {
    matches!(end, End::Error(RunError::Bound(exceeded)) if exceeded.bound == Bound::Charge)
}

/// `K-CHG-002`, `K-MACH-008`: on generated expression trees, a run bound
/// at every charge from 0 to its whole charge ends the same way in each
/// layout, and the same as charging each event one at a time in the
/// order the cost table states: each node before what is under it. A run
/// that crosses its bound inside a charge region is charged exactly to the
/// event that crosses, one that raises inside a region is charged the
/// events before the raise, and one that would raise under a node the
/// bound ends at halts instead.
#[test]
fn every_charge_bound_ends_where_charging_one_event_at_a_time_ends() {
    let mut random = Seeded(0x5eed_cafe);
    let (mut trees, mut raising, mut bounds) = (0, 0, 0);
    // The statements before the expression's: `let a = 7` and
    // `let b = [1, 2, 3]`, then the `let x` statement itself.
    let before = [1, 1, 1, 1, 1, 1, 1, 3, 1];
    for _ in 0..256 {
        let tree = any(&mut random, 3);
        let expression = text(&tree);
        let mut charges = before.to_vec();
        let completes = events(&tree, &mut charges);
        raising += usize::from(!completes);
        let programs: Vec<(Program, Layout)> = RUNS
            .iter()
            .map(|(layout, native_twice)| (program(&expression, *layout, *native_twice), *layout))
            .collect();
        let (whole, total) = ends(&programs[0].0, programs[0].1, ROOMY.charge_limit);
        match (&whole, completes) {
            (End::Finished(_), true) => {}
            (
                End::Error(RunError::Uncaught {
                    error: Datum::Error(error),
                    ..
                }),
                false,
            ) => {
                assert_eq!(error.kind, "unbound_variable", "{expression}");
            }
            other => panic!("{expression}: {other:?}"),
        }
        let modelled: u64 = charges.iter().sum();
        assert!(total >= modelled, "{expression}: {total} < {modelled}");
        for charge in 0..=total {
            let runs: Vec<(End, u64)> = programs
                .iter()
                .map(|(program, layout)| ends(program, *layout, charge))
                .collect();
            for (run, (layout, native_twice)) in runs.iter().zip(RUNS).skip(1) {
                assert_eq!(
                    run, &runs[0],
                    "{expression} at {charge}: layout {layout:?}, native twice {native_twice}"
                );
            }
            // One event at a time: the first whose charge passes the bound
            // ends the run, charged with it.
            let mut charged = 0;
            let crossing = charges.iter().find_map(|units| {
                charged += units;
                (charged > charge).then_some(charged)
            });
            if let Some(expected) = crossing {
                let (end, actual) = &runs[0];
                assert!(halted(end), "{expression} at {charge}: {end:?}");
                assert_eq!(*actual, expected, "{expression} at {charge}");
            }
            bounds += 1;
        }
        assert!(halted(&ends(&programs[0].0, programs[0].1, total - 1).0));
        trees += 1;
    }
    assert!(raising > 0 && raising < trees, "{raising} of {trees} raise");
    println!("charge bounds: trees={trees} raising={raising} bounds={bounds}");
}

/// K-EVAL-001/002, K-CHG-002 and K-MACH-008: only a selected branch
/// runs, bindings evaluate once, and jumps preserve that contract in every layout.
#[test]
fn conditional_regions_are_lazy_and_expression_bindings_evaluate_once_in_every_layout() {
    use super::embedder::{Embedder, Setup};
    let mut charge = None;
    for layout in [Layout(0), Layout(1), Layout(0x5eed)] {
        for skipped in [
            "null.missing",
            "(read(h, \"skipped\"), read(h, \"skipped\"), read(h, \"skipped\"))",
        ] {
            let program = format!(
                r#"
definition "function go(h: Handle(\"table\")) -> Any\nkernel 1\nmetering statements\nbody {{ \n  finish (if(read(h, true), read(h, \"then\"), {skipped_json}), if(read(h, false), {skipped_json}, read(h, \"else\")), let(x = read(h, \"bound\"), (x, x)))\n }}"

main {{ finish null }}
"#,
                skipped_json = crate::laws::embedder::fragment(skipped)
            );
            let mut embedder = Embedder::with(
                &program,
                Setup {
                    layout,
                    start: CellStart {
                        target: Target::Entry(Name::new("go")),
                        args: vec![Datum::Handle(lash_kernel_doc::Handle {
                            kind: "table".into(),
                            id: "one".into(),
                        })],
                        session: None,
                        owner: PageId([9; 32]),
                        modes: crate::EffectModes::new(),
                        host: Vec::new(),
                    },
                    ..Setup::default()
                },
            );
            embedder.machine.verify_stream().unwrap();
            let end = embedder.run_to_end(&[]);
            assert_eq!(
                super::embedder::result(end),
                Datum::Tuple(vec![
                    Datum::Text("then".into()),
                    Datum::Text("else".into()),
                    Datum::Tuple(vec![Datum::Text("bound".into()); 2])
                ])
            );
            assert_eq!(
                embedder.world.reads,
                vec![
                    Datum::Bool(true),
                    Datum::Text("then".into()),
                    Datum::Bool(false),
                    Datum::Text("else".into()),
                    Datum::Text("bound".into())
                ]
            );
            let charged = embedder.machine.meters().charged.expect("exact charge");
            assert_eq!(*charge.get_or_insert(charged), charged);
        }
    }
}
