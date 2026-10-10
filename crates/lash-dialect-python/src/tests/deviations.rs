//! Where the dialect is deliberately not Python.
//!
//! One law per entry of `deviations.md`, named as the entry is. Each runs
//! a program whose answer differs from CPython's and pins the dialect's.

use super::machine::output;

fn lines(source: &str) -> Vec<String> {
    let (lines, end) = output(source);
    assert_eq!(end, "ok", "{source}");
    lines
}

/// CPython: `True True`, `2`, `1`.
#[test]
fn bool_is_not_an_integer() {
    let source = "\
print(True == 1, False == 0)
try:
    print(True + 1)
except TypeError as error:
    print(error)
print(len({True: 'a', 1: 'b'}))
";
    assert_eq!(
        lines(source),
        [
            "False False",
            "unsupported operand type(s) for +: 'bool' and 'int'",
            "2"
        ]
    );
}

/// CPython ignores annotations and prints `ab`.
#[test]
fn annotations_are_trusted() {
    let source = "\
def join(a: int, b: int):
    return a + b
print(join(1, 2))
try:
    print(join('a', 'b'))
except TypeError:
    print('the annotation was believed')
";
    assert_eq!(lines(source), ["3", "the annotation was believed"]);
}

/// CPython: `[] generator`; the elements are computed when asked for.
#[test]
fn generator_expression_is_an_eager_list() {
    let source = "\
log = []
def note(value):
    log.append(value)
    return value
pending = (note(x) for x in range(3))
print(log, type(pending).__name__)
";
    assert_eq!(lines(source), ["[0, 1, 2] list"]);
}

/// CPython: `False False False`; each is a distinct object.
#[test]
fn is_compares_immutable_values_by_value() {
    let source = "\
a = (1, 2)
b = tuple([1, 2])
print(a is b, 'ab' + str(1) is 'ab1', 10 ** 20 is 10 ** 20, [] is [])
";
    assert_eq!(lines(source), ["True True True False"]);
}

/// CPython: `range(0, 3) range`.
#[test]
fn range_is_a_list() {
    assert_eq!(
        lines("print(range(3), type(range(3)).__name__)\n"),
        ["[0, 1, 2] list"]
    );
}

/// CPython: `dict_keys(['a', 'b']) dict_items`; a view follows its dict.
#[test]
fn dict_views_are_lists() {
    let source = "\
d = {'a': 1}
keys = d.keys()
d['b'] = 2
print(keys, type(d.items()).__name__)
";
    assert_eq!(lines(source), ["['a'] list"]);
}

/// CPython orders a set by hash: `{0, 1, 2, 3} [4, 5]`.
#[test]
fn a_set_keeps_insertion_order() {
    let source = "\
s = {3, 1, 2}
s.add(0)
print(s, list({5, 4}))
";
    assert_eq!(lines(source), ["{3, 1, 2, 0} [5, 4]"]);
}

/// CPython changes the object: `{1, 2}` and `[1, 2, 1, 2]` for the aliases.
#[test]
fn only_list_plus_equals_mutates_in_place() {
    let source = "\
a = {1}
b = a
a |= {2}
print(a, b)
c = [1]
d = c
c += [2]
print(d)
c *= 2
print(c, d)
";
    assert_eq!(
        lines(source),
        ["{1, 2} {1}", "[1, 2]", "[1, 2, 1, 2] [1, 2]"]
    );
}

/// CPython makes a coroutine object and runs nothing until it is awaited.
#[test]
fn a_coroutine_call_the_front_end_cannot_see_runs_at_once() {
    let source = "\
async def work():
    print('ran')
    return 1
functions = [work]
made = functions[0]()
print(made)
print(await functions[0]())
";
    assert_eq!(lines(source), ["ran", "1", "ran", "1"]);
}

/// CPython raises RecursionError, which a program can catch.
#[test]
fn unbounded_recursion_ends_the_run() {
    let source = "\
def deep(n):
    return deep(n + 1)
try:
    deep(0)
except RecursionError:
    print('caught')
";
    let (lines, end) = output(source);
    assert!(lines.is_empty(), "{lines:?}");
    assert_eq!(end, "run error: the run passed its CallDepth bound of 200");
}

/// CPython: `True`; it takes an element for equal to itself.
#[test]
fn nan_in_a_list_is_not_equal_to_itself() {
    let source = "\
nan = float('nan')
xs = [nan]
print(xs == xs, nan in xs)
";
    assert_eq!(lines(source), ["False False"]);
}

/// CPython: `inf`.
#[test]
fn a_float_literal_too_large_does_not_parse() {
    let source = "\
try:
    print(float('1e400'))
except ValueError as error:
    print(error)
";
    assert_eq!(
        lines(source),
        ["could not convert string to float: '1e400'"]
    );
}

/// K-LFMT-003 supplies neutral parts; Python retains its sign, plain
/// fraction and at-least-two-digit signed exponent layout.
#[test]
fn decimal_parts_keep_python_float_repr_layout() {
    assert_eq!(
        lines(
            "print(repr(0.0), repr(-0.0), repr(float('nan')), repr(float('inf')), repr(float('-inf')))\nprint(repr(1e-4), repr(1e-5), repr(1e15), repr(1e16))\nprint(repr(5e-324), repr(2.2250738585072014e-308), repr(1.7976931348623157e308))\n"
        ),
        [
            "0.0 -0.0 nan inf -inf",
            "0.0001 1e-05 1000000000000000.0 1e+16",
            "5e-324 2.2250738585072014e-308 1.7976931348623157e+308"
        ]
    );
}
