//! PROTO-LOOP (`K-ITER-005`): each `for...of` is one kernel loop through
//! `ts.for_of`, whose cursor walks an array, a Map, a Set or a string live
//! and whose driver steps any other iterator by the `next` it read once.
//! Each case runs uninterrupted and reopened from its sealed state after
//! every unit of work, so that a loop is cut at its header, inside its
//! `next` (`Pull`) and between its step and its bind, and both runs agree
//! on the result and every unit of charge.

use lash_kernel_doc::{Datum, Float};

use super::machine::plain_record_resume;

fn finished(source: &str) -> Datum {
    let plain = plain_record_resume(source, u64::MAX, false);
    let reopened = plain_record_resume(source, 1, true);
    assert_eq!((&reopened.0, reopened.1), (&plain.0, plain.1), "{source}");
    plain.0
}

/// `continue`, `break`, `return` and `throw` leave a pass as JavaScript's
/// do, across an await in the body, and a hole is `undefined`. (The dialect
/// refuses a labelled `continue`.)
#[test]
fn passes_continue_break_return_and_throw() {
    let source = "function firstBig(xs) { for (const x of xs) { if (x > 2) { return x; } } return -1; }\n\
        const out = [];\n\
        for (const a of [1, 2]) {\n\
          for (const x of [1, 2, 3, 4]) {\n\
            if (x === 2 || x === 4) { continue; }\n\
            out.push(await echo(x * a));\n\
            if (a === 2 && x === 3) { break; }\n\
          }\n\
        }\n\
        try { for (const x of [7]) { throw new Error(\"inside\"); } } catch (e) { out.push(e.message); }\n\
        for (const x of [1, , 3]) { out.push(x === undefined ? \"hole\" : x); }\n\
        try { for (const x of 5) { out.push(x); } } catch (e) { out.push(\"refused\"); }\n\
        out.push(firstBig([1, 3, 5]));\n\
        await finish(out.join(\",\"));";
    assert_eq!(
        finished(source),
        Datum::Text("1,3,2,6,inside,1,hole,3,refused,3".into())
    );
}

/// A string is walked by code point; a Map and a Set live, an entry
/// deleted before the loop reaches it skipped, though the loop parks
/// between, and one added reached; an iterator through its `next`.
#[test]
fn collections_walk_live_and_iterators_step_through_next() {
    let source = "const out = [];\n\
        for (const c of \"a\u{e9}\u{1f600}\") { out.push(await echo(c)); }\n\
        const m = new Map([[\"a\", 1], [\"b\", 2], [\"c\", 3]]);\n\
        for (const [key, v] of m) {\n\
          if (key === \"a\") { m.delete(\"b\"); m.set(\"d\", 4); }\n\
          await echo(key);\n\
          out.push(key + v);\n\
        }\n\
        const s = new Set([1, 2]);\n\
        for (const x of s) { if (x === 1) { s.add(3); } out.push(x); }\n\
        for (const x of [10, 20].values()) { out.push(await echo(x)); }\n\
        for (const [i, x] of [\"p\", \"q\"].entries()) { out.push(i + x); }\n\
        await finish(out.join(\",\"));";
    assert_eq!(
        finished(source),
        Datum::Text("a,\u{e9},\u{1f600},a1,c3,d4,1,2,3,10,20,0p,1q".into())
    );
}

/// A value declared an array is walked as what it holds, as JavaScript
/// walks it: the protocol has no typed copy of the loop.
#[test]
fn a_declared_array_is_walked_as_what_it_holds() {
    let source = "const xs: number[] = 'ab' as any;\n\
        let n = 0;\n\
        for (const x of xs) { n = n + 1; }\n\
        await finish(n);";
    assert_eq!(finished(source), Datum::Float(Float::new(2.0)));
}
