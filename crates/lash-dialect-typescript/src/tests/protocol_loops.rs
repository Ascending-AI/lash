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
            out.push(await host.echo(x * a));\n\
            if (a === 2 && x === 3) { break; }\n\
          }\n\
        }\n\
        try { for (const x of [7]) { throw new Error(\"inside\"); } } catch (e) { out.push(e.message); }\n\
        for (const x of [1, , 3]) { out.push(x === undefined ? \"hole\" : x); }\n\
        try { for (const x of 5) { out.push(x); } } catch (e) { out.push(\"refused\"); }\n\
        out.push(firstBig([1, 3, 5]));\n\
        await control.finish(out.join(\",\"));";
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
        for (const c of \"a\u{e9}\u{1f600}\") { out.push(await host.echo(c)); }\n\
        const m = new Map([[\"a\", 1], [\"b\", 2], [\"c\", 3]]);\n\
        for (const [key, v] of m) {\n\
          if (key === \"a\") { m.delete(\"b\"); m.set(\"d\", 4); }\n\
          await host.echo(key);\n\
          out.push(key + v);\n\
        }\n\
        const s = new Set([1, 2]);\n\
        for (const x of s) { if (x === 1) { s.add(3); } out.push(x); }\n\
        for (const x of [10, 20].values()) { out.push(await host.echo(x)); }\n\
        for (const [i, x] of [\"p\", \"q\"].entries()) { out.push(i + x); }\n\
        await control.finish(out.join(\",\"));";
    assert_eq!(
        finished(source),
        Datum::Text("a,\u{e9},\u{1f600},a1,c3,d4,1,2,3,10,20,0p,1q".into())
    );
}

/// CONT-ONCE (spec-rev2 §3.5) and PROTO-LOOP on the cutover lowering: a
/// guest `valueOf` that ran before a nested await is not run again when
/// its expression resumes, and an array spliced and a set deleted from
/// while their loops park are walked live, once per element.
#[test]
fn guest_conversions_run_once_and_mutated_collections_walk_live_across_parks() {
    let source = "let calls = 0;\n\
        const n = { valueOf() { calls = calls + 1; return 2; } };\n\
        const out = [];\n\
        out.push(n * 3 + await host.echo(1));\n\
        const a = [1, 2, 3, 4];\n\
        for (const x of a) { out.push(await host.echo(x)); if (x === 1) { a.splice(1, 1); } }\n\
        const s = new Set([1, 2, 3]);\n\
        for (const x of s) { if (x === 1) { s.delete(2); } out.push(await host.echo(x)); }\n\
        await control.finish(out.join(\",\") + \":\" + calls);";
    assert_eq!(finished(source), Datum::Text("7,1,3,4,1,3:1".into()));
}

/// A value declared an array is walked as what it holds, as JavaScript
/// walks it: the protocol has no typed copy of the loop.
#[test]
fn a_declared_array_is_walked_as_what_it_holds() {
    let source = "const xs: number[] = 'ab' as any;\n\
        let n = 0;\n\
        for (const x of xs) { n = n + 1; }\n\
        await control.finish(n);";
    assert_eq!(finished(source), Datum::Float(Float::new(2.0)));
}

/// K-CHG-007 and PLAN-INDEP: binding a list element charges only its own
/// size and immutable key material. Growing a graph behind its identities
/// cannot turn an item handoff into a deep graph walk; text key work grows
/// by its bytes instead. The operation's declared formula pins this rule.
#[test]
fn list_item_charge_does_not_walk_graphs_behind_the_element() {
    let added_charge = |bindings: &str| {
        let base = format!("{bindings}\nawait control.finish(0);");
        let walked =
            format!("{bindings}\nfor (const item of [raw]) {{}}\nawait control.finish(0);");
        let plain = plain_record_resume(&base, u64::MAX, false);
        let looped = plain_record_resume(&walked, u64::MAX, false);
        assert_eq!(plain.0, looped.0);
        looped.1.checked_sub(plain.1).unwrap()
    };
    let small = added_charge("const inner = {value: 'x'.repeat(1)}; const raw = [inner, inner];");
    let large =
        added_charge("const inner = {value: 'x'.repeat(4096)}; const raw = [inner, inner];");
    assert_eq!(large, small, "only the element's own list size is measured");
    let short_key = added_charge("const raw = 'x'.repeat(16);");
    let long_key = added_charge("const raw = 'x'.repeat(32);");
    assert_eq!(
        long_key - short_key,
        16,
        "immutable key bytes bound key work"
    );
}

/// FIG-6976: a for-loop updates once after its body (including finally),
/// uses the head's bindings at continue sites, and keeps each captured let.
/// Awaiting tests and updates resume with the same result and exact charge.
#[test]
fn for_loops_keep_update_order_bindings_and_parks() {
    let lowered = super::lower_with_effects("for (; await host.echo(false);) {}")
        .expect("an awaiting test lowers");
    assert!(
        lowered.document.main.iter().any(|stmt| matches!(
            stmt,
            lash_kernel_doc::Stmt::While {
                condition: lash_kernel_doc::Expr::Literal(lash_kernel_doc::Literal::Bool(true)),
                ..
            }
        )),
        "a test that can park stays in the loop body"
    );
    let source = r#"
        const out = [];
        for (let i = 0; i < 3; i = await host.echo(i + 1)) {
            if (i === 0) { let i = 99; continue; }
            switch (i) { case 1: continue; }
            out.push(i);
        }
        for (var v = 0; await host.echo(v < 2); v++) { out.push("v" + v); }
        out.push(v);
        let missing = 0;
        for (;;) { missing++; if (missing === 2) break; continue; }
        for (; missing < 4;) { missing++; continue; }
        out.push(missing);
        const fs = [];
        for (let captured = 0; captured < 3; captured++) {
            fs.push(() => captured);
            if (captured === 1) continue;
        }
        out.push(fs[0](), fs[1](), fs[2]());
        for (let f = 0; f < 2; f = await host.echo(f + 1)) {
            try { continue; } finally { out.push("f" + f); }
        }
        function bad() { out.push("error"); throw new Error("stop"); }
        try { for (let t = 0; bad(); t++) { out.push("unreachable"); } }
        catch (e) { out.push("test"); }
        try { for (let u = 0; u < 2; bad()) { out.push("body"); } }
        catch (e) { out.push("update"); }
        try {
            for (let c = 0; c < 2; bad()) {
                try { continue; } catch (e) { out.push("wrong catch"); break; }
            }
        } catch (e) { out.push("outer catch"); }
        await control.finish(out.join(","));
    "#;
    assert_eq!(
        finished(source),
        Datum::Text(
            "2,v0,v1,2,4,0,1,2,f0,f1,error,test,body,error,update,error,outer catch".into()
        )
    );
    // Labels remain a dialect refusal; shortening a loop cannot admit one.
    let refused = super::lower("outer: for (let i = 0; i < 2; i++) { continue outer; }")
        .expect_err("labels are unsupported");
    assert_eq!(refused.code, crate::DiagnosticCode::LabelUnsupported);
}
