//! FIG-6948 / PLAN-INDEP: TypeScript comparator and default sorts, whose
//! merge passes the region's machine steps natively.

use super::*;

/// FIG-6948: the TypeScript sorts of [`COMPARATOR_SORTS`] named `name`,
/// lowered, and whether it sorts by a comparator, whose merge passes the
/// region's machine steps natively.
pub(crate) fn comparator_sort_program(name: &str) -> (runner::Case, Program, bool) {
    let (_, source, compared) = COMPARATOR_SORTS
        .iter()
        .find(|(named, ..)| *named == name)
        .unwrap_or_else(|| panic!("no sort program {name}"));
    let [(case, program)]: [_; 1] = lowered_islands(&[("typescript", name, source)])
        .try_into()
        .unwrap_or_else(|_| panic!("{name} lowers once"));
    (case, program, *compared)
}

/// The comparator sort programs: stable ties; NaN, text and object
/// answers, the last coerced through its `valueOf`; undefined elements and
/// holes and `toSorted`; a comparator that throws and one that grows the
/// array it sorts and calls methods, so its calls leave their islands; a
/// sort small enough that the sweep bounds it at every one of its events;
/// and default sorts of objects, whose strings are taken per comparison,
/// and of primitives, whose strings are taken once.
const COMPARATOR_SORTS: [(&str, &str, bool); 5] = [
    (
        "comparator sorts: ties and answers",
        "function run() {\n  const xs = [{ id: 0, key: 2 }, { id: 1, key: 1 }, { id: 2, key: 2 }, { id: 3, key: 0 }];\n  xs.sort((p, q) => p.key - q.key);\n  const nan = [3, 1, 2].sort((a, b) => (a === 2 ? NaN : a - b));\n  const texts = [3, 1, 2].sort((a, b) => String(a - b));\n  const objects = [2, 3, 1].sort((a, b) => ({ valueOf() { return a - b; } }));\n  return [xs.map((r) => r.id), nan, texts, objects];\n}\nconst result = run();\n",
        true,
    ),
    (
        "comparator sorts: undefined, holes and toSorted",
        "function run() {\n  const a = [3, undefined, , 1, 2];\n  a.sort((x, y) => x - y);\n  const b = [5, , 4, undefined];\n  const c = b.toSorted((x, y) => y - x);\n  return [a, [3 in a, 4 in a, a.length], c, 1 in b, 3 in c];\n}\nconst result = run();\n",
        true,
    ),
    (
        "comparator sorts: throwing and mutating comparators",
        "function run() {\n  const xs = [4, 3, 2, 1];\n  let calls = 0;\n  let caught = \"none\";\n  try {\n    xs.sort((a, b) => { calls++; if (calls === 3) { throw new Error(\"stop\"); } return a - b; });\n  } catch (e) { caught = e.message; }\n  const ys = [3, 1, 4, 2];\n  const seen = [];\n  ys.sort((a, b) => { ys.push(a); seen.push(a + \":\" + b); return b - a; });\n  return [xs, calls, caught, ys, seen];\n}\nconst result = run();\n",
        true,
    ),
    (
        "comparator sorts: every bound of a small sort",
        "function run() {\n  const xs = [3, 1, 4, 1, 5, 2];\n  xs.sort((a, b) => a - b);\n  return xs;\n}\nconst result = run();\n",
        true,
    ),
    (
        "comparator sorts: default sorts",
        "function run() {\n  let made = 0;\n  const spell = (v) => ({ toString() { made++; return String(v); } });\n  const xs = [spell(10), spell(9), spell(100)];\n  xs.sort();\n  [spell(7)].sort();\n  const taken = made;\n  const mixed = [10, \"b\", 9, true, null, \"\\uE000\", \"\\uD83D\\uDE00\"].sort();\n  return [xs.map(String), taken, mixed];\n}\nconst result = run();\n",
        false,
    ),
];
