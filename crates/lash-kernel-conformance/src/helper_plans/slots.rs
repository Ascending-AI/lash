//! FIG-6978 / PLAN-INDEP: TypeScript array callbacks whose helpers' slots
//! walks the region's machine steps natively.

use super::*;

/// FIG-6978: the TypeScript program of [`SLOTS_WALKS`] named `name`,
/// lowered.
pub(crate) fn slots_walk_program(name: &str) -> (runner::Case, Program) {
    let (_, source) = SLOTS_WALKS
        .iter()
        .find(|(named, _)| *named == name)
        .unwrap_or_else(|| panic!("no slots walk program {name}"));
    let [(case, program)]: [_; 1] = lowered_islands(&[("typescript", name, source)])
        .try_into()
        .unwrap_or_else(|_| panic!("{name} lowers once"));
    (case, program)
}

/// The slots walk programs: callbacks that shorten, grow and empty the
/// list they walk, write ahead of the walk and visit holes, with filter
/// answers no bool that are coerced; callbacks that raise midway, from
/// their islands and from the reference; and a map and a filter small
/// enough that the sweep bounds them at every one of their events; and
/// walks whose callbacks pin, leave their islands or answer no bool on one
/// element only, the others stepped without opening their statements
/// (FIG-7003).
const SLOTS_WALKS: [(&str, &str); 4] = [
    (
        "slots walks: mutating callbacks",
        "function run() {\n  const shrink = [1, 2, 3, 4, 5];\n  const halved = shrink.map((x, i, a) => { if (i === 1) { a.pop(); a.pop(); } return x * 2; });\n  const grow = [1, 2, 3];\n  const grown = grow.map((x, i, a) => { a.push(x + 10); return x + i; });\n  const empty = [4, 5, 6, 7];\n  const emptied = empty.filter((x, i, a) => { while (a.length > 0) { a.pop(); } return x > 0; });\n  const ahead = [1, 2, 3, 4];\n  const kept = ahead.filter((x, i, a) => { a[i + 1] = x * 10; return x % 2 === 0 || x > 5; });\n  const holes = [1, , 3, , 5];\n  const tripled = holes.map((x) => x * 3);\n  const answers = [0, 1, \"\", \"a\", null, undefined, NaN, {}, [], -0, 2].filter((x) => x);\n  const remainders = [1, 2, 3, 4, 5, 6].filter((x) => x % 3);\n  let seen = 0;\n  [3, , 5].forEach((x) => { seen += x; });\n  return [halved, shrink, grown, grow, emptied, empty, kept, ahead, tripled, 1 in tripled, answers.length, remainders, seen];\n}\nconst result = run();\n",
    ),
    (
        "slots walks: callbacks that raise midway",
        "function run() {\n  let calls = 0;\n  let mapped = \"none\";\n  try {\n    [1, 2, 3, 4].map((x) => { calls++; if (x === 3) { throw new Error(\"map \" + x); } return x * 2; });\n  } catch (e) { mapped = e.message; }\n  let read = \"none\";\n  try { [5, 6, 7, 8].filter((x) => x > 6 ? x.y.z : true); } catch (e) { read = \"caught\"; }\n  let thrown = \"none\";\n  try { [1, 2, 3].filter((x) => { if (x === 2) { throw 7; } return true; }); } catch (e) { thrown = String(e); }\n  return [calls, mapped, read, thrown];\n}\nconst result = run();\n",
    ),
    (
        "slots walks: every bound of a small walk",
        "function run() {\n  const xs = [1, , 3, 4];\n  const ys = xs.map((x) => x * 2);\n  return ys.filter((y) => (y % 3 === 0 ? true : y % 4));\n}\nconst result = run();\n",
    ),
    (
        "slots walks: one element pins or leaves its island",
        "function run() {\n  const xs = [1, 2, 3, 4, 5];\n  const boxed = xs.map((x, i) => (i === 2 ? { x } : x * 2));\n  const log = [];\n  const logged = xs.map((x, i) => { if (i === 3) { log.push(x); } return x + 1; });\n  const named = xs.map((x, i) => (i === 1 ? \"n\" + x : x));\n  const odd = xs.filter((x, i) => (i === 1 ? [x] : i === 3 ? 0 : x % 2 === 1));\n  const records = [{ a: 1 }, { a: 2 }, { a: 3 }].filter((o) => o.a !== 2);\n  return [boxed, logged, log, named, odd, records.map((o) => o.a)];\n}\nconst result = run();\n",
    ),
];
