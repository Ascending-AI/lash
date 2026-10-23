//! A region's operator sites obey PLAN-INDEP and K-CHG-003 (FIG-6965).

use super::*;

/// Regions on: the natural layout.
const ON: u64 = 0;
/// Regions off, and everything else equal: the reference.
const OFF: u64 = Layout(ON).without_regions().0;

/// Python loops whose allocating regions call helpers at their operator
/// sites, each named for the paths and charges its calls reach.
const PROGRAMS: &[(&str, &str)] = &[
    (
        "map reads by constant and variable keys, a list write through py.index, and a held map write",
        "def body():\n    tree = {\"level\": {\"rows\": [[0], [1], [2]], \"counters\": {\"c\": 0}}}\n    for n in range(5):\n        tree[\"level\"][\"rows\"][n % 3] = [n]\n        key = \"c\"\n        tree[\"level\"][\"counters\"][key] = tree[\"level\"][\"counters\"][key] + 1\n    return tree\nawait host.observe(body())\n",
    ),
    (
        "native entries' answers, a method's heap paths and a held map write",
        "def body():\n    counts = {}\n    for n in range(5):\n        key = \"key_\" + str(n % 3)\n        counts[key] = counts.get(key, 0) + n\n    return counts\nawait host.observe(body())\n",
    ),
    (
        "a template's immediate paths, an operator's typed path left to its site, a minimum formula and a constant one",
        "def body():\n    rows = {\"k\": 7, \"b\": None, \"r\": [1]}\n    v = \"\"\n    x = 0.0\n    c = 0\n    for n in range(4):\n        v = str(rows[\"k\"])\n        x = n + 0.5\n        if rows[\"b\"] is None:\n            c = c + 1\n        rows[\"r\"].append(n)\n    return (v, x, c, len(rows[\"r\"]))\nawait host.observe(body())\n",
    ),
    (
        "a heap path's decline that the reference raises, caught",
        "def body():\n    rows = {\"k\": 7, \"r\": [1]}\n    c = 0\n    for n in range(4):\n        c = c + rows[\"k\"]\n        rows[\"r\"].append(n)\n        if n == 3:\n            c = rows[\"missing\"]\n    return c\ntry:\n    await host.observe(body())\nexcept KeyError as e:\n    await host.observe(\"raised\")\n",
    ),
];

/// TypeScript loops whose allocating regions write records by a key a read
/// in the region found, write lists through `ts.set` and test a record's
/// brand, each named for what comes between the read and the write. A
/// callback that adds or deletes keys runs as the reference runs it, the
/// region exiting there; the region's own writes add keys and write over
/// entries; and a write by another key than the read's must not take the
/// read's position. A write of the brand revokes the proof of its
/// absence.
const TYPESCRIPT: &[(&str, &str)] = &[
    (
        "a plain record's keyed writes after a callback adds keys and deletes the key read",
        "function body() {\n  const counts = {};\n  function poke(o, n) {\n    if (n === 3) { o[\"extra\"] = 1; }\n    if (n === 7) { delete o[\"key_1\"]; }\n    return n;\n  }\n  for (let n = 0; n < 9; n++) {\n    const key = \"key_\" + String(n % 3);\n    const before = counts[key] || 0;\n    if (n % 4 === 3) { poke(counts, n); }\n    counts[key] = before + n;\n  }\n  return counts;\n}\nawait host.observe(body());\n",
    ),
    (
        "a plain record's keyed writes after the region adds a key, writes over the entry read and deletes it",
        "function body() {\n  const counts = {};\n  for (let n = 0; n < 9; n++) {\n    const key = \"key_\" + String(n % 3);\n    const before = counts[key] || 0;\n    if (n === 3) { counts[\"extra\"] = 1; }\n    if (n === 5) { counts[key] = 50; }\n    if (n === 6) { delete counts[key]; }\n    counts[key] = before + n;\n  }\n  return counts;\n}\nawait host.observe(body());\n",
    ),
    (
        "a plain record's write by another key than the read's",
        "function body() {\n  const counts = { key_0: 0, key_1: 10, key_2: 20 };\n  for (let n = 0; n < 7; n++) {\n    const key = \"key_\" + String(n % 3);\n    const other = \"key_\" + String((n + 1) % 3);\n    counts[key] = (counts[other] || 0) + n;\n  }\n  return counts;\n}\nawait host.observe(body());\n",
    ),
    (
        "a shared record's ts.set writes and brand tests after the region writes its brand",
        "function body() {\n  const groups = {};\n  for (let item = 0; item < 9; item++) {\n    const key = \"bucket_\" + String(item % 3);\n    if (item === 6) { groups[\"__lash_\" + \"brand\"] = \"x\"; }\n    groups[key] = (groups[key] || 0) + 1;\n  }\n  const lines = [];\n  for (const key of Object.keys(groups)) { lines.push(key); }\n  return { groups: groups, lines: lines.join(\"|\") };\n}\ntry {\n  await host.observe(body());\n} catch (e) {\n  await host.observe(e.name);\n}\n",
    ),
    (
        "a field write after the region adds a field",
        "function body() {\n  const tree = { level: { counters: { c: 0, d: 0 } } };\n  for (let n = 0; n < 6; n++) {\n    if (n === 4) { tree.level.counters.e = 1; }\n    tree.level.counters[\"c\"] = tree.level.counters[\"c\"] + 1;\n  }\n  return tree;\n}\nawait host.observe(body());\n",
    ),
    (
        "ts.set list writes at whole positions below, at and past the length, and at -0",
        "function body(at) {\n  const tree = { level: { rows: [[0], [1], [2]] } };\n  for (let n = 0; n < at.length; n++) {\n    tree.level.rows[at[n]] = [n];\n  }\n  return tree.level.rows;\n}\nawait host.observe(body([0, 2, -0, 1, 3, 6, 5, 1, 0]));\n",
    ),
    (
        "ts.set list writes at positions no element write takes, raised",
        "function body(at) {\n  const tree = { level: { rows: [[0], [1], [2]] } };\n  for (let n = 0; n < at.length; n++) {\n    tree.level.rows[at[n]] = [n];\n  }\n  return tree.level.rows;\n}\nfor (const bad of [1.5, -1, NaN, 4294967295]) {\n  try {\n    await host.observe(body([0, 1, bad, 2]));\n  } catch (e) {\n    await host.observe(e.name);\n  }\n}\n",
    ),
];

/// The run of `program` prepared for layout `L` under `charge`, and the
/// loop iterations its regions ran.
fn observed<const L: u64>(case: &runner::Case, program: &Program, charge: u64) -> (Observed, u64) {
    let mut projector = Projector {
        layout: Layout(L),
        ..Projector::default()
    };
    let (observed, work) = run_with_work::<L>(
        case,
        program,
        &mut projector,
        charge,
        200,
        lash_kernel_vm::MemoryMeter::On,
    );
    (observed, work.iterations)
}

/// PLAN-INDEP / K-CHG-003 (FIG-6965): every helper call a region makes at
/// an operator site answers, declines, raises and charges as the reference
/// call does, so that a run under every charge bound, each landing at or
/// between its calls' charges, ends where the reference ends, at the same
/// call, with the same charge and the same charge program.
#[test]
fn a_bound_landing_on_a_helper_call_stops_where_the_reference_stops() {
    agree("python", PROGRAMS, None);
}

/// PLAN-INDEP / K-CHG-003 (FIG-6979): a region's record write by a key a
/// read in the region found, its element write for `ts.set` and its
/// record's brand test write and answer as the reference does after any
/// write a callback interposes between the read and the write: one that
/// adds or deletes keys revokes the read's position, and one that adds
/// the brand revokes the proof of its absence, so that a run under every
/// charge bound ends where the reference ends, with the same charge and
/// the same charge program.
#[test]
fn keyed_and_element_writes_find_their_keys_again_after_an_interposed_write() {
    agree("typescript", TYPESCRIPT, Some(SPREAD));
}

/// The most charge bounds a TypeScript program's sweep runs: its first and
/// last and an even spread between.
const SPREAD: u64 = 300;

/// Every program of `dialect` in `programs` runs as the reference runs,
/// unbounded and under every charge bound up to its charge, or under
/// `spread` of them, its first and last and an even spread between.
fn agree(dialect: &str, programs: &[(&str, &str)], spread: Option<u64>) {
    let runner = runner::Runner::new(dialect).unwrap();
    let mut libraries = None;
    for (name, source) in programs {
        let case = runner::Case {
            id: (*name).into(),
            dialect: dialect.into(),
            source: (*source).into(),
            bits: 0,
            stride: 0,
            nanoseconds: 0,
        };
        let runner::Lowered::Program(program) = runner.lower(&case).unwrap() else {
            panic!("{name}: the program lowers")
        };
        let (on, off) = libraries.get_or_insert_with(|| {
            (
                in_layout(&program.library, Layout(ON)),
                in_layout(&program.library, Layout(OFF)),
            )
        });
        let (mut regions, mut reference) = (program.clone(), program);
        regions.library = on.clone();
        reference.library = off.clone();
        let (expected, unformed) = observed::<OFF>(&case, &reference, u64::MAX);
        let (actual, iterations) = observed::<ON>(&case, &regions, u64::MAX);
        assert_eq!(unformed, 0, "{name}: the reference runs no region");
        assert!(iterations > 0, "{name}: its loop runs as a region");
        assert_eq!(actual, expected, "{name}: unbounded");
        let step = spread.map_or(1, |spread| expected.charged.div_ceil(spread).max(1));
        let bounds = (0..=expected.charged)
            .step_by(usize::try_from(step).unwrap())
            .chain([expected.charged]);
        for charge in bounds {
            assert_eq!(
                observed::<ON>(&case, &regions, charge).0,
                observed::<OFF>(&case, &reference, charge).0,
                "{name}: bound {charge}"
            );
        }
    }
}
