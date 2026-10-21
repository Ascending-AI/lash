//! PROTO-LOOP, in kernel text (`K-ITER-005`, `K-FORM-013`): a `for … via`
//! opens, checks, steps and binds through its protocol's hooks, and an
//! `else` runs when a loop ends but by `break`. Each case runs
//! uninterrupted, sliced and reopened under alternating layouts at every
//! slice, as CONT-EXACT runs, and its reopens see the loop stand in the
//! header phases named.

use std::collections::BTreeMap;

use lash_kernel_doc::{Datum, FunctionRegistry, parse_definition};
use lash_kernel_state::paged::HeaderRecord;
use lash_vocabulary::legacy_names::FunctionId;

use super::{Drive, bodies, exact, int, list, text, tuple};

/// The hooks the cases' protocols name, each with only a kernel body:
/// `proto.open` gives its value, `proto.next` applies the driver's `step`,
/// `proto.item` wraps an item in a list, and `proto.check` spends one of
/// the remaining items per step, throwing `"budget"` when none is left.
fn hooks(
    registry: &mut FunctionRegistry,
    ids: &BTreeMap<&'static str, FunctionId>,
) -> Vec<(&'static str, FunctionId)> {
    let mut out = bodies(registry, ids);
    let mut register = |name: &'static str, params: &str, body: &str| {
        let definition = parse_definition(&format!(
            "function {name}({params}) -> Any\nkernel 1\ncharge 1\nuse num.add = @{}\n\
             use num.lt = @{}\nbody {{ {body} }}\n",
            ids["num.add"], ids["num.lt"]
        ))
        .unwrap();
        out.push((name, registry.register(definition, None).unwrap()));
    };
    register("proto.open", "x: Any", "return x");
    register(
        "proto.next",
        "d: Any",
        "let step = d.step let r = apply step(d) return r",
    );
    register("proto.item", "s: Any, raw: Any", "return [raw]");
    register(
        "proto.check",
        "s: Any, size: Any, remaining: Any, more: Bool",
        "if more { if num.lt(remaining, 1) { throw \"budget\" } return num.add(remaining, -1) } \
         return remaining",
    );
    out
}

fn run(text: &str, order: &[&str], expected: &Datum, printed: &[Datum], want: &[HeaderRecord]) {
    let base = Drive {
        probes: hooks,
        ..Drive::new(u64::MAX, false)
    };
    let headers = exact(base, text, order, expected, printed).headers;
    for header in want {
        assert!(
            headers.contains(header),
            "the reopens saw {headers:?}: {text}"
        );
    }
}

/// A driver whose `step` waits on an effect for each item: the loop stands
/// at `Pull` with the step's frames above its own, binds each item through
/// `item`, and runs its `else` when `next` gives the empty tuple.
#[test]
fn a_driver_steps_through_next_and_binds_through_item() {
    const SOURCE: &str = "main { let d = {at: 0, step: fn(d) {\nif num.lt(d.at, 3) { let v = perform 0(d.at) as Any set d.at = num.add(d.at, 1) return (v,) }\nreturn () }}\nlet out = []\nfor x in d via (open proto.open, next proto.next, item proto.item) {\nset out[list.len(out)] = x\n} else { print \"exhausted\" }\nreturn out }\n";
    run(
        SOURCE,
        &[],
        &list(&[list(&[int(0)]), list(&[int(1)]), list(&[int(2)])]),
        &[text("exhausted")],
        &[
            HeaderRecord::Open,
            HeaderRecord::Pull,
            HeaderRecord::Item,
            HeaderRecord::Body,
        ],
    );
}

/// A map walked live through a protocol that checks: an entry added during
/// the loop is reached, and the budget fixed at open, spent once per item,
/// throws when it reaches the added entry. A park in the body keeps the
/// guard.
#[test]
fn a_checked_map_spends_its_budget_across_parks() {
    const SOURCE: &str = "main { let m = map{\"a\": 1, \"b\": 2}\nlet seen = []\ntry {\nfor k in m via (open proto.open, check proto.check) {\nlet echoed = perform 0(k) as Any\nset seen[list.len(seen)] = echoed\nset m[text.concat(k, \"!\")] = 0\n}\n} catch e { return (seen, e) }\nreturn seen }\n";
    run(
        SOURCE,
        &[],
        &tuple(&[list(&[text("a"), text("b")]), text("budget")]),
        &[],
        &[HeaderRecord::Check, HeaderRecord::Body],
    );
}

/// A checked loop that runs out checks once more at exhaustion, then runs
/// its `else`; a `break` skips both.
#[test]
fn exhaustion_checks_then_runs_else_and_break_skips_it() {
    const SOURCE: &str = "main { let s = set{1, 2}\n\
        let out = []\n\
        for x in s via (open proto.open, check proto.check) {\n\
          set out[list.len(out)] = x\n\
        } else { print \"exhausted\" }\n\
        for x in s via (open proto.open, check proto.check) {\n\
          set out[list.len(out)] = x\n\
          break\n\
        } else { print \"never\" }\n\
        return out }\n";
    run(
        SOURCE,
        &[],
        &list(&[int(1), int(2), int(1)]),
        &[text("exhausted")],
        &[HeaderRecord::Check, HeaderRecord::Exhausted],
    );
}

/// A paged text is walked by code point across cold reopens; `continue`
/// reaches the header, `break`
/// leaves past the `else`, and a `while` runs its own `else` when its
/// condition fails.
#[test]
fn a_text_walks_code_points_and_loops_take_continue_break_and_else() {
    const SOURCE: &str = "main { let out = []\n\
        let i = 0\n\
        for c in \"a\u{e9}\u{1f600}zqxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\" via (open proto.open) {\n\
          set i = num.add(i, 1)\n\
          if num.lt(i, 2) { continue }\n\
          let echoed = perform 0(c) as Any\n\
          set out[list.len(out)] = echoed\n\
          if num.lt(3, i) { break }\n\
        } else { print \"never\" }\n\
        let j = 0\n\
        while num.lt(j, 2) { set j = num.add(j, 1) } else { print j }\n\
        for x in [1, 2] { set out[list.len(out)] = x } else { print \"listed\" }\n\
        return out }\n";
    run(
        SOURCE,
        &[],
        &list(&[text("\u{e9}"), text("\u{1f600}"), text("z"), int(1), int(2)]),
        &[int(2), text("listed")],
        &[HeaderRecord::Body],
    );
}

/// A value the protocol cannot step raises `type_error` at the header: an
/// opened value that is no collection, with no `next`.
#[test]
fn an_opened_value_with_no_next_raises() {
    const SOURCE: &str = "main { try {\n\
          for x in {a: 1} via (open proto.open) { print x }\n\
        } catch e { return \"refused\" }\n\
        return \"walked\" }\n";
    run(SOURCE, &[], &text("refused"), &[], &[]);
}
