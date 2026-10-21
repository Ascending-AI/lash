//! TS-TEXT-BUILD: `String(x)` and the other positional built-ins, text plus
//! a float, and a two-part template lower to direct text operations, with
//! no argument list, no `ts.add` and no join (FIG-6818). Each answers what
//! the general form answers, in the same order.

use super::main_text;

/// What a cell finishes with, as JSON text.
fn finished(source: &str) -> String {
    match super::machine::end(source) {
        super::machine::Ended::Finished(lash_kernel_doc::Datum::Text(text)) => text,
        other => panic!("{source}: {other:?}"),
    }
}

/// A key built by `String`, by `+` and by a template is the same two
/// operations: a float's spelling and one concatenation.
#[test]
fn a_float_key_lowers_to_its_spelling_and_one_concatenation() {
    for key in [
        "\"key_\" + String(n % 64)",
        "\"key_\" + (n % 64)",
        "`key_${n % 64}`",
    ] {
        let text = main_text(&format!(
            "function f(n: number) {{ const key = {key}; return key; }}"
        ));
        assert!(
            text.contains("text.concat(\"key_\", ts.number.format_value(num.rem_trunc("),
            "{key}:\n{text}"
        );
        assert!(
            super::linked::helper_slot(
                &super::lower(&format!(
                    "function f(n: number) {{ const key = {key}; return key; }}"
                ))
                .unwrap()
                .document,
                "ts.string.create",
            )
            .is_none(),
            "{key} keeps the general string helper:\n{text}"
        );
        for general in ["ts.add(", "text.join(", "ts.to_string("] {
            assert!(!text.contains(general), "{key} keeps {general}:\n{text}");
        }
    }
    let swapped = main_text("function f(n: number) { return (n % 64) + \"_key\"; }");
    assert!(
        swapped.contains("text.concat(ts.number.format_value(num.rem_trunc("),
        "{swapped}"
    );
}

/// Only a float proven by construction is spelled directly. A number an
/// annotation names may be any value at run time, and a value of unknown
/// type may run its own `toString`, so both keep the general forms.
#[test]
fn an_unproven_operand_keeps_the_general_form() {
    let annotated = main_text("function f(n: number) { return \"k\" + n; }");
    assert!(annotated.contains("ts.add(\"k\", n)"), "{annotated}");
    let unknown = main_text("function f(x) { return String(x) + `k${x}`; }");
    assert!(unknown.contains("ts.to_string(x)"), "{unknown}");
    assert!(
        super::linked::helper_slot(
            &super::lower("function f(x) { return String(x) + `k${x}`; }")
                .unwrap()
                .document,
            "ts.string.create",
        )
        .is_none(),
        "{unknown}"
    );
    // A spread has no fixed position: it keeps the argument list.
    let spread = main_text("function f(xs) { return String(...xs); }");
    assert!(
        super::lower("function f(xs) { return String(...xs); }")
            .unwrap()
            .document
            .imports
            .values()
            .any(|import| import.name
                == lash_kernel_dialect::authoring::helper_name(
                    &lash_vocabulary::FunctionName::new("ts.string.create").unwrap()
                )),
        "{spread}"
    );
    // Three or more parts are one join.
    let three = main_text("function f(x) { return `a${x}b${x}`; }");
    assert!(three.contains("text.join("), "{three}");
}

/// `String`, `Number`, `Boolean` and `Math.pow` called directly answer
/// what their list helpers answer through `.call`, for every value kind,
/// a missing argument, no argument and extra arguments. Extra arguments
/// evaluate before the conversion, which may run a value's own method.
#[test]
fn positional_built_ins_agree_with_their_list_helpers() {
    let source = r#"
      const log = [];
      const tick = (tag) => { log.push(tag); return tag; };
      const o = {
        toString() { log.push("toString"); return "o"; },
        valueOf() { log.push("valueOf"); return 7; },
      };
      const values = [undefined, null, true, false, 0, -0, 1.5, NaN, Infinity,
        -Infinity, 1e21, 1e-7, "", "12", " 3 ", "x", [], [1, 2], {}, o];
      const mismatches = [];
      const same = (what, direct, general) => {
        if (!Object.is(direct, general)) { mismatches.push([what, String(direct), String(general)]); }
      };
      for (const v of values) {
        same("String", String(v), String.call(undefined, v));
        same("String float", String(v * 1), String.call(undefined, v * 1));
        same("Number", Number(v), Number.call(undefined, v));
        same("Boolean", Boolean(v), Boolean.call(undefined, v));
        same("Math.pow", Math.pow(v, 2), Math.pow.call(undefined, v, 2));
        same("Math.pow base", Math.pow(2, v), Math.pow.call(undefined, 2, v));
        same("Math.pow one", Math.pow(v), Math.pow.call(undefined, v));
      }
      same("String()", String(), String.call(undefined));
      same("Number()", Number(), Number.call(undefined));
      same("Boolean()", Boolean(), Boolean.call(undefined));
      same("Math.pow()", Math.pow(), Math.pow.call(undefined));
      log.length = 0;
      const extra = [String(o, tick("b")), Number(o, tick("c")), Math.pow(o, 2, tick("d"))];
      const direct = log.splice(0);
      const general = [String.call(undefined, o, tick("b")), Number.call(undefined, o, tick("c")),
        Math.pow.call(undefined, o, 2, tick("d"))];
      same("extra", JSON.stringify([extra, direct]), JSON.stringify([general, log]));
      await control.finish(JSON.stringify({
        mismatches,
        sample: [String(-0), String(1e21 * 1), String(), Number(), Number(" 3 "), Boolean(),
          Math.pow(2), Math.pow(2, 10)],
        order: direct,
      }));
    "#;
    assert_eq!(
        finished(source),
        r#"{"mismatches":[],"sample":["0","1e+21","",0,3,false,null,1024],"order":["b","toString","c","valueOf","d","valueOf"]}"#
    );
}

/// Text plus a float, either way round, and a two-part template answer
/// what `ts.add` and the join answer, for every float spelling.
#[test]
fn text_plus_a_float_and_two_part_templates_agree_with_the_general_forms() {
    let source = r#"
      const floats = [0, -0, 1.5, -2.25, NaN, Infinity, -Infinity, 1e21, 1e-7, 0.1 + 0.2,
        123, -5, 2 ** 53, 5e-324];
      const mismatches = [];
      const same = (what, direct, general) => {
        if (direct !== general) { mismatches.push([what, direct, general]); }
      };
      for (const f of floats) {
        const x = f * 1;
        same("text + float", "k" + x, ("k" as any) + (f as any));
        same("float + text", x + "k", (f as any) + ("k" as any));
        let s = "k";
        s += x;
        same("+=", s, ("k" as any) + (f as any));
        same("template", `k${x}`, `k${x}${""}`);
        same("template right", `${x}k`, `${x}k${""}`);
        same("template any", `k${f}`, `k${f}${""}`);
      }
      const log = [];
      const o = {
        toString() { log.push("toString"); return "o"; },
        valueOf() { log.push("valueOf"); return 7; },
      };
      same("template object", `k${o}`, `k${o}${""}`);
      same("template both", `${o}${o}`, `${o}${o}${""}`);
      await control.finish(JSON.stringify({
        mismatches,
        sample: ["k" + (-0 * 1), (1e21 * 1) + "k", `k${0.5 * 1}`],
        log,
      }));
    "#;
    assert_eq!(
        finished(source),
        r#"{"mismatches":[],"sample":["k0","1e+21k","k0.5"],"log":["toString","toString","toString","toString","toString","toString"]}"#
    );
}
