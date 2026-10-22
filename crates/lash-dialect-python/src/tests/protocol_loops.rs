//! PROTO-LOOP (`K-ITER-005`): each Python `for` is one kernel loop through
//! `py.for_in`, whose cursor walks the value live and whose check refuses a
//! dict or a set that changed as CPython's iterator does (FIG-6406), and
//! whose `else` is the kernel loop's. Each case runs uninterrupted and
//! reopened from its sealed state after every unit of work, so that a
//! loop is cut at its header, at its check and between its step and its
//! bind, and both runs agree on every line, request and unit of charge.

use std::sync::Arc;

use lash_kernel_vm::Bounds;

use super::machine::{BOUNDS, ending, execution_observation, installed, lower};

/// How `source` ends and what it prints, the same uninterrupted and
/// reopened at every safe point.
fn run(source: &str) -> (String, Vec<String>) {
    let lowered = lower(source).unwrap_or_else(|error| panic!("{error}"));
    let program = crate::tests::linked::program(
        Arc::clone(lowered.document.shared()),
        installed().prepared.clone(),
    );
    let bounds = Bounds {
        charge_limit: 200_000,
        ..BOUNDS
    };
    let straight = execution_observation(program.clone(), bounds, false);
    let reopened = execution_observation(program, bounds, true);
    assert_eq!(reopened, straight, "{source}");
    (ending(straight.end), straight.lines)
}

fn lines(source: &str) -> Vec<String> {
    let (end, lines) = run(source);
    assert_eq!(end, "ok", "{source}");
    lines
}

/// `continue`, `break`, `return` and a raise leave a pass as Python's do,
/// across an await in the body; `else` runs on exhaustion only.
#[test]
fn passes_continue_break_return_raise_and_else() {
    let source = "def first_big(xs):\n    for x in xs:\n        if x > 2:\n            return x\n    return None\n\
        out = []\n\
        for x in [1, 2, 3, 4]:\n    if x == 2:\n        continue\n    out.append(await host.echo(x))\n    if x == 3:\n        break\nelse:\n    print('never')\n\
        for x in (5, 6):\n    out.append(x)\nelse:\n    print('exhausted')\n\
        try:\n    for x in [7]:\n        raise ValueError('inside')\n    else:\n        print('never')\nexcept ValueError as error:\n    print(error)\n\
        n = 0\n\
        while n < 2:\n    n = n + 1\nelse:\n    print('while', n)\n\
        while True:\n    break\nelse:\n    print('never')\n\
        print(out, first_big([1, 3, 5]))\n";
    assert_eq!(
        lines(source),
        ["exhausted", "inside", "while 2", "[1, 3, 5, 6] 3"]
    );
}

/// A str is walked by code point and a dict's items through its view's
/// protocol, each value read live.
#[test]
fn a_str_walks_code_points_and_a_view_reads_the_dict_live() {
    let source = "for c in 'a\u{e9}\u{1f600}':\n    print(await host.echo(c))\n\
        d = {'a': 1, 'b': 2}\n\
        for k, v in d.items():\n    d['b'] = 20\n    print(k, v)\n\
        for v in d.values():\n    print(v)\n\
        for k in d.keys():\n    print(k)\n";
    assert_eq!(
        lines(source),
        [
            "a",
            "\u{e9}",
            "\u{1f600}",
            "a 1",
            "b 20",
            "1",
            "20",
            "a",
            "b"
        ]
    );
}

/// A dict entry deleted before the loop reaches it is never visited,
/// though the loop parks between; but the size it opened with is checked
/// at the next step, so the loop raises there.
#[test]
fn a_dict_delete_before_a_park_is_refused_at_the_next_step() {
    let (end, lines) = run(
        "d = {'a': 1, 'b': 2, 'c': 3}\nfor k in d:\n    print(await host.echo(k))\n    if k == 'a':\n        del d['b']\n",
    );
    assert_eq!(
        end,
        "error RuntimeError: dictionary changed size during iteration"
    );
    assert_eq!(lines, ["a"]);
}

/// A delete during the last pass raises at exhaustion, and `else` does not
/// run; a delete followed by `break` raises nothing.
#[test]
fn a_final_delete_raises_at_exhaustion_and_a_break_after_one_does_not() {
    let (end, lines) = run(
        "d = {'a': 1, 'b': 2}\nfor k in d:\n    print(k)\n    if k == 'b':\n        del d['b']\nelse:\n    print('never')\n",
    );
    assert_eq!(
        end,
        "error RuntimeError: dictionary changed size during iteration"
    );
    assert_eq!(lines, ["a", "b"]);
    assert_eq!(
        self::lines("d = {'a': 1, 'b': 2}\nfor k in d:\n    del d[k]\n    break\nprint(d)\n"),
        ["{'b': 2}"]
    );
    let (end, _) = run("s = {1, 2}\nfor x in s:\n    s.add(3)\n");
    assert_eq!(end, "error RuntimeError: Set changed size during iteration");
}

/// FIG-6406: a delete and an add of the same size keep the size but spend
/// the budget of entries the loop opened with, and raise when it is spent.
#[test]
fn a_same_size_delete_and_add_spends_the_entry_budget() {
    let (end, lines) = run(
        "d = {'a': 1, 'b': 2}\nfor k in d:\n    print(await host.echo(k))\n    del d[k]\n    d[k + 'x'] = 1\n",
    );
    assert_eq!(
        end,
        "error RuntimeError: dictionary keys changed during iteration"
    );
    assert_eq!(lines, ["a", "b"]);
}
