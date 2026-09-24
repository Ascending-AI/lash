//! The snapshot round-trip law (FIG-3608).
//!
//! For every value type the dialect accepts, a value that is created, stored
//! in a session global, reloaded from the durable snapshot and used in a later
//! cell behaves exactly as if it had never been stored. Each [`Row`] creates
//! one value in one cell and uses it in the next; the law runs the pair three
//! ways and requires the later cell to observe what the same code observes in
//! a single cell, where the value is never stored:
//!
//! * live, with one resident state;
//! * reloaded, restarting through the durable snapshot between the cells;
//! * and the single-cell reference itself must be Node's answer (from
//!   `generated.json`), so "as if never stored" is JavaScript's meaning, not
//!   whatever lash does in one cell.
//!
//! Every heap object kind has a row, or the stated reason no program builds
//! one ([`NOT_A_DIALECT_VALUE`]); `lashlang::testing::heap_object_kinds` names
//! them by an exhaustive match, so a new kind cannot enter the heap without a
//! row here.
//!
//! A type that cannot round-trip must be refused with a named diagnostic,
//! never silently degraded. Where the law fails today, the row is pinned
//! ([`Pin::Fails`]) by the open defect or registered deviation that makes it
//! fail, in the modes it fails in, and the law requires it to keep failing
//! there until the defect is fixed: then the pin is deleted (the ratchet).

use super::super::harness::HarnessMode;
use super::{Observation, run_session};

/// Why a row's law fails today, or how the dialect refuses the type.
#[derive(Clone, Copy, Debug)]
pub(super) enum Pin {
    /// The law holds.
    Holds,
    /// The law fails in `modes`, by the open defect or registered deviation
    /// `name` (in the crate README); `authority` is the ticket that fixes it
    /// or the ruling that registered it.
    Fails {
        name: &'static str,
        authority: &'static str,
        modes: &'static [HarnessMode],
    },
    /// The dialect refuses to create the value, by this diagnostic.
    Refused(&'static str),
}

const BOTH: &[HarnessMode] = &[HarnessMode::Resident, HarnessMode::RestartBetweenCells];

/// One value type: a cell that binds `value`, and a cell that uses it.
#[derive(Clone, Copy, Debug)]
pub(super) struct Row {
    /// The heap object kind (`lashlang::testing::heap_object_kinds`) or the
    /// primitive the row holds to the law.
    pub(super) kind: &'static str,
    /// A name unique among the rows.
    pub(super) name: &'static str,
    pub(super) create: &'static str,
    pub(super) uses: &'static str,
    pub(super) pin: Pin,
    /// A registered deviation by which the single-cell answer differs from
    /// Node's, as the crate README names it.
    pub(super) node_deviation: Option<&'static str>,
}

/// The heap kinds no program of the dialect builds, and why.
pub(super) const NOT_A_DIALECT_VALUE: &[(&str, &str)] = &[(
    "tuple",
    "an IR tuple, which the TypeScript lowering never builds and the printer refuses to spell; a TypeScript array is a list",
)];

const fn row(
    kind: &'static str,
    name: &'static str,
    create: &'static str,
    uses: &'static str,
) -> Row {
    Row {
        kind,
        name,
        create,
        uses,
        pin: Pin::Holds,
        node_deviation: None,
    }
}

const fn pinned(mut row: Row, pin: Pin) -> Row {
    row.pin = pin;
    row
}

pub(super) const ROWS: &[Row] = &[
    row(
        "number",
        "negative-zero",
        "const value = -0;",
        "console.log(Object.is(value, -0), 1 / value, value + 1);",
    ),
    row(
        "number",
        "non-finite",
        "const value = [NaN, Infinity, -Infinity, 2 ** 53, 0.1 + 0.2];",
        "console.log(Number.isNaN(value[0]), value[1] > 1e308, value[2] < 0, value[3] + 1, value[4] * 10);",
    ),
    row(
        "string",
        "string",
        "const value = 'h\u{e9}llo \u{1F600} line\\nbreak';",
        "console.log(value.length, [...value].length, value.at(-1), value.split('\\n')[1], JSON.stringify(value));",
    ),
    row(
        "boolean",
        "boolean",
        "const value = [true, false];",
        "console.log(value[0] && !value[1], typeof value[1]);",
    ),
    row(
        "null",
        "null",
        "const value = null;",
        "console.log(value === null, typeof value, value ?? 'fallback');",
    ),
    row(
        "undefined",
        "undefined",
        "const value = [undefined, { gone: undefined, kept: 1 }];",
        "console.log(value[0] === undefined, value.length, 'gone' in value[1], Object.keys(value[1]));",
    ),
    row(
        "list",
        "array",
        "const value = [1, 'two', [3, [4]], { five: 5 }, null];",
        "value.push(6);\nconsole.log(value.length, value[2][1][0], value[3].five, value.at(-1), Array.isArray(value), JSON.stringify(value));",
    ),
    row(
        "record",
        "object",
        "const value = { a: 1, b: [true, null], c: { d: 'x' } };",
        "value.a = 2;\nvalue.b.push(false);\nconsole.log(Object.keys(value), value.c.d, JSON.stringify(value));",
    ),
    row(
        "record",
        "object-insertion-order",
        "const value = { zeta: 1, alpha: { two: 2, one: 1 }, 10: 'ten', 2: 'two' };",
        "value.zeta = 3;\nconsole.log(Object.keys(value), JSON.stringify(value));\nfor (const key in value.alpha) {\n  console.log(key);\n}",
    ),
    row(
        "record",
        "shared-object",
        "const shared = { count: 1 };\nconst value = { left: shared, right: shared };",
        "value.left.count = 2;\nconsole.log(value.right.count, shared.count);",
    ),
    row(
        "record",
        "nested-plain",
        "const value = { a: [{ b: 1 }, { b: 2 }], c: { d: [[1], [2, 3]] } };",
        "value.a[1].b += 10;\nconsole.log(value.a.map((item) => item.b), value.c.d.flat(), JSON.stringify(value));",
    ),
    row(
        "Map",
        "map",
        "const value = new Map([['b', 2], ['a', { n: 1 }]]);",
        "value.set('c', 3);\nconsole.log(value.size, value.get('a').n, [...value.keys()], value instanceof Map);",
    ),
    row(
        "Set",
        "set",
        "const value = new Set([3, 1, 3, 2]);",
        "value.add(0);\nconsole.log(value.size, value.has(1), [...value], value instanceof Set);",
    ),
    row(
        "Date",
        "date",
        "const value = new Date(Date.UTC(2024, 1, 29, 12, 30));",
        "console.log(value.toISOString(), value.getUTCDay(), value.getTime(), value instanceof Date);",
    ),
    row(
        "RegExp",
        "regexp",
        "const value = /a(b+)/gi;\nvalue.test('xab');",
        "console.log(value.lastIndex, value.source, value.flags, value.exec('xabbb ABB')?.[1], value.lastIndex, String(value));",
    ),
    row(
        "RegExp match array",
        "regexp-match",
        "const value = /(?<word>[a-z]+)(\\d)?/.exec('  lash9');",
        "console.log(value.index, value.input, value.groups.word, value[2], value.length, Array.isArray(value));",
    ),
    row(
        "Error",
        "error",
        "const value = new RangeError('out of range', { cause: 'why' });",
        "console.log(value.name, value.message, value.cause, String(value), value instanceof RangeError, value instanceof Error);",
    ),
    row(
        "URL",
        "url",
        "const value = new URL('https://user@example.com:8080/a/b?x=1#frag');",
        "value.pathname = '/c';\nconsole.log(value.href, value.port, value.searchParams.get('x'), value instanceof URL);",
    ),
    row(
        "URLSearchParams",
        "url-search-params",
        "const value = new URLSearchParams('b=2&a=1&b=3');",
        "value.append('c', 'x y');\nconsole.log(value.toString(), value.getAll('b'), value.size, value instanceof URLSearchParams);",
    ),
    row(
        "record",
        "object-holding-a-map",
        "const value = { tags: new Map([['k', 1]]), n: 1 };",
        "console.log(value.tags.get('k'), value.n);",
    ),
    row(
        "list",
        "array-of-dates",
        "const value = [new Date(0), new Date(86400000)];",
        "console.log(value.map((date) => date.toISOString()));",
    ),
    pinned(
        row(
            "function",
            "closure",
            "const base = 10;\nconst value = (n) => n + base;",
            "console.log(value(1));",
        ),
        Pin::Fails {
            name: "closure-boundary",
            authority: "ADR 0062 register entry 17",
            modes: BOTH,
        },
    ),
    Row {
        kind: "record",
        name: "process-value",
        create: "const value = async () => {\n  return 1;\n};",
        uses: "console.log(typeof value);",
        pin: Pin::Holds,
        node_deviation: Some("process-literal-is-a-process-value"),
    },
    pinned(
        row(
            "class instance",
            "class-instance",
            "class Point {\n  constructor(x) {\n    this.x = x;\n  }\n}\nconst value = new Point(1);",
            "console.log(value.x);",
        ),
        Pin::Refused("TS_CLASS_UNSUPPORTED"),
    ),
];

/// What the law compares: how a cell ended and what it printed.
fn behaviour(observation: &Observation) -> Observation {
    Observation {
        probes: std::collections::BTreeMap::new(),
        ..observation.comparable()
    }
}

/// The value used in the cell that created it: the reference.
pub(super) fn never_stored(row: &Row) -> Observation {
    let source = format!("{}\n{}\n", row.create, row.uses);
    run_session(HarnessMode::Resident, &[], &[source])
        .pop()
        .expect("one cell ran")
        .observation
}

/// The value stored by one cell and used by the next, in `mode`: the
/// creating cell's observation and the using cell's.
pub(super) fn stored(row: &Row, mode: HarnessMode) -> (Observation, Observation) {
    let sources = [format!("{}\n", row.create), format!("{}\n", row.uses)];
    let mut cells = run_session(mode, &[], &sources).into_iter();
    let create = cells.next().expect("the creating cell ran").observation;
    let uses = cells.next().expect("the using cell ran").observation;
    (create, uses)
}

/// Checks one row against the law and Node's answer for its two cells;
/// returns every failure.
pub(super) fn check(row: &Row, node: &[Observation]) -> Vec<String> {
    let mut failures = Vec::new();
    let context = format!("round-trip row `{}` ({})", row.name, row.kind);
    if let Pin::Refused(code) = row.pin {
        for mode in HarnessMode::ALL {
            let (create, _) = stored(row, *mode);
            if create.outcome != "rejected" || create.diagnostic.as_deref() != Some(code) {
                failures.push(format!(
                    "{context} {mode:?}: the value must be refused by `{code}`, and the creating cell observed {create:?}"
                ));
            }
        }
        return failures;
    }
    let reference = never_stored(row);
    // Node's single-cell answer: the creating cell's lines, then the using
    // cell's, ending as the using cell ends.
    let [node_create, node_uses] = node else {
        failures.push(format!(
            "{context}: Node's answer has {} cells, not 2",
            node.len()
        ));
        return failures;
    };
    let mut node_reference = node_uses.clone();
    node_reference.prints = node_create
        .prints
        .iter()
        .chain(&node_uses.prints)
        .cloned()
        .collect();
    let agrees = behaviour(&reference) == behaviour(&node_reference);
    match (row.node_deviation, agrees) {
        (None, false) => failures.push(format!(
            "{context}: the value used where it was created is not Node's answer\n  lash: {reference:?}\n  node: {node_reference:?}"
        )),
        (Some(deviation), true) => failures.push(format!(
            "{context}: `{deviation}` no longer separates lash from Node here; delete it"
        )),
        _ => {}
    }
    for mode in HarnessMode::ALL {
        let (create, uses) = stored(row, *mode);
        if create.outcome != "normal" {
            failures.push(format!(
                "{context} {mode:?}: the creating cell must run, and observed {create:?}"
            ));
            continue;
        }
        // The creating cell prints nothing of its own in any row, so the
        // using cell answers for the whole reference.
        let mut observed = uses.clone();
        observed.prints = create.prints.iter().chain(&uses.prints).cloned().collect();
        let holds = behaviour(&observed) == behaviour(&reference);
        let pinned = match row.pin {
            Pin::Fails {
                name,
                authority,
                modes,
            } if modes.contains(mode) => Some((name, authority)),
            _ => None,
        };
        match (pinned, holds) {
            (None, false) => failures.push(format!(
                "{context} {mode:?}: the stored value does not behave as if never stored\n  stored:       {observed:?}\n  never stored: {reference:?}"
            )),
            (Some((name, authority)), true) => failures.push(format!(
                "{context} {mode:?}: the law now holds; `{name}` ({authority}) is fixed here, so delete the pin"
            )),
            _ => {}
        }
    }
    failures
}
