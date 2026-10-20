//! A region's operator sites obey PLAN-INDEP and K-CHG-003 (FIG-6965).

use std::sync::Arc;

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
        "def body():\n    tree = {\"level\": {\"rows\": [[0], [1], [2]], \"counters\": {\"c\": 0}}}\n    for n in range(5):\n        tree[\"level\"][\"rows\"][n % 3] = [n]\n        key = \"c\"\n        tree[\"level\"][\"counters\"][key] = tree[\"level\"][\"counters\"][key] + 1\n    return tree\nawait observe(body())\n",
    ),
    (
        "native entries' answers, a method's heap paths and a held map write",
        "def body():\n    counts = {}\n    for n in range(5):\n        key = \"key_\" + str(n % 3)\n        counts[key] = counts.get(key, 0) + n\n    return counts\nawait observe(body())\n",
    ),
    (
        "a template's immediate paths, an operator's typed path left to its site, a minimum formula and a constant one",
        "def body():\n    rows = {\"k\": 7, \"b\": None, \"r\": [1]}\n    v = \"\"\n    x = 0.0\n    c = 0\n    for n in range(4):\n        v = str(rows[\"k\"])\n        x = n + 0.5\n        if rows[\"b\"] is None:\n            c = c + 1\n        rows[\"r\"].append(n)\n    return (v, x, c, len(rows[\"r\"]))\nawait observe(body())\n",
    ),
    (
        "a heap path's decline that the reference raises, caught",
        "def body():\n    rows = {\"k\": 7, \"r\": [1]}\n    c = 0\n    for n in range(4):\n        c = c + rows[\"k\"]\n        rows[\"r\"].append(n)\n        if n == 3:\n            c = rows[\"missing\"]\n    return c\ntry:\n    await observe(body())\nexcept KeyError as e:\n    await observe(\"raised\")\n",
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
    let runner = runner::Runner::new("python").unwrap();
    let mut libraries = None;
    for (name, source) in PROGRAMS {
        let case = runner::Case {
            id: (*name).into(),
            dialect: "python".into(),
            source: (*source).into(),
            bits: 0,
            stride: 0,
            nanoseconds: 0,
        };
        let runner::Lowered::Program(program) = runner.lower(&case).unwrap() else {
            panic!("{name}: the program lowers")
        };
        let (on, off) = libraries.get_or_insert_with(|| {
            let registry = program.library.registry();
            (
                prepare_with_layout(Arc::clone(registry), Layout(ON)),
                prepare_with_layout(Arc::clone(registry), Layout(OFF)),
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
        for charge in 0..=expected.charged {
            assert_eq!(
                observed::<ON>(&case, &regions, charge).0,
                observed::<OFF>(&case, &reference, charge).0,
                "{name}: bound {charge}"
            );
        }
    }
}
