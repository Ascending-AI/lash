//! Python call binding, callable values and resumable iterator laws.

use super::machine::{kernel_text, output};

fn ran(source: &str, lines: &[&str]) {
    let expected: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
    assert_eq!(output(source), (expected, "ok".to_string()), "{source}");
}

fn checks(source: &str, name: &str) -> usize {
    let document = super::machine::lower(source).unwrap().document;
    let target =
        lash_kernel_dialect::Library::resolve(super::machine::library(), "py.bind").unwrap();
    let mut pending = document
        .units()
        .filter(|(unit, _)| match unit {
            lash_kernel_value::sites::Unit::Library(id) => {
                lash_kernel_doc::FunctionCatalog::header(super::machine::library(), id).is_none()
            }
            lash_kernel_value::sites::Unit::Main => true,
            _ => false,
        })
        .map(|(_, block)| lash_kernel_doc::Node::Block(block))
        .collect::<Vec<_>>();
    let mut nodes = Vec::new();
    while let Some(node) = pending.pop() {
        pending.extend(node.children());
        nodes.push(node);
    }
    nodes
        .iter()
        .filter(|node| match node {
            lash_kernel_doc::Node::Expr(lash_kernel_doc::Expr::Act(action)) => {
                matches!(&action.kind, lash_kernel_doc::ActKind::Call(lash_kernel_doc::Callee::Local(function)) if *function == target)
                    && matches!(action.args.first(), Some(lash_kernel_doc::Expr::Literal(lash_kernel_doc::Literal::Text(value))) if value == name)
                    && matches!(action.args.get(4), Some(lash_kernel_doc::Expr::Literal(lash_kernel_doc::Literal::Bool(false))))
            }
            lash_kernel_doc::Node::Action(lash_kernel_doc::Action::Call {
                callee: lash_kernel_doc::Callee::Local(function),
                args,
            }) => {
                *function == target
                    && matches!(args.first(), Some(lash_kernel_doc::Atom::Literal(lash_kernel_doc::Literal::Text(value))) if value == name)
                    && matches!(args.get(4), Some(lash_kernel_doc::Atom::Literal(lash_kernel_doc::Literal::Bool(false))))
            }
            _ => false,
        })
        .count()
}

/// A nested `def` read only as the callee of calls that give every required
/// argument, by position or by name, recursion included, skips that check.
#[test]
fn a_def_every_call_fills_has_no_argument_check() {
    let source = r#"
def outer():
    def add(a, b=2):
        return a + b
    def fact(n):
        return 1 if n <= 1 else n * fact(n - 1)
    def twice(x):
        return [add(x), add(a=x), add(x, b=5)]
    return twice(1) + [fact(5)]
print(outer())
"#;
    ran(source, &["[3, 3, 6, 120]"]);
    for name in ["add", "fact", "twice"] {
        assert_eq!(checks(source, name), 0, "{name}\n{}", kernel_text(source));
    }
}

/// A def whose value leaves the direct callee position keeps its check, and
/// a call through the escaped value raises Python's TypeError in the binder.
#[test]
fn an_escaping_def_keeps_its_argument_check() {
    for (escape, call) in [
        ("holder = [f]\n    return holder[0]()", "outer()"),
        (
            "def call(g):\n        return g()\n    return call(f)",
            "outer()",
        ),
        ("return f", "outer()()"),
        ("def h():\n        return f\n    return h", "outer()()()"),
    ] {
        let source = format!(
            "def outer():\n    def f(a):\n        return a\n    {escape}\ntry:\n    {call}\nexcept TypeError as error:\n    print(error)\n"
        );
        ran(&source, &["f() missing a required positional argument"]);
        assert_eq!(checks(&source, "f"), 1, "{source}");
    }
}

/// A name bound twice, or named by a `nonlocal`, is not one fixed function.
#[test]
fn a_rebound_def_keeps_its_argument_check() {
    let twice = r#"
def outer():
    def f(a):
        return a
    first = f(1)
    def f(a, b):
        return a + b
    return first + f(1, 2)
print(outer())
"#;
    ran(twice, &["4"]);
    assert_eq!(checks(twice, "f"), 2);
    let nonlocal = r#"
def outer():
    def f(a):
        return a
    def swap():
        nonlocal f
        f = lambda a: a * 10
    swap()
    return f(2)
print(outer())
"#;
    ran(nonlocal, &["20"]);
    assert_eq!(checks(nonlocal, "f"), 1);
}

/// One short call anywhere in the scope, nested ones included, keeps the
/// check, and that call raises before its body runs.
#[test]
fn a_short_call_keeps_the_argument_check() {
    let source = r#"
def outer():
    def f(a, b):
        return a + b
    def inner():
        return f(1)
    try:
        inner()
    except TypeError as error:
        print(error)
    return f(1, 2)
print(outer())
"#;
    ran(source, &["f() missing a required positional argument", "3"]);
    assert_eq!(checks(source, "f"), 1);
}

/// A call before the def binds the name still meets the unbound name check;
/// after binding, the fully supplied call skips argument validation.
#[test]
fn a_forward_capture_keeps_its_name_check() {
    let source = r#"
def outer():
    def early():
        return late(2)
    try:
        early()
    except NameError:
        print('unbound')
    def late(x):
        return x * 10
    return early()
print(outer())
"#;
    ran(source, &["unbound", "20"]);
    let text = kernel_text(source);
    assert!(
        super::machine::calls_helper(source, "py.name_check"),
        "{text}"
    );
    assert_eq!(checks(source, "late"), 0, "{text}");
}

/// Call-shape distinctions absent from the static proof stay on the runtime
/// binder, even when every visible call supplies each required argument.
#[test]
fn special_parameter_kinds_keep_runtime_argument_checks() {
    let source = r#"
def outer():
    def positional_only(value, /):
        return value
    def keyword_only(*, value):
        return value
    def with_rest(value, *rest):
        return value
    def with_extra(value, **extra):
        return value
    return [positional_only(1), keyword_only(value=2), with_rest(3), with_extra(4)]
print(outer())
"#;
    ran(source, &["[1, 2, 3, 4]"]);
    for name in ["positional_only", "keyword_only", "with_rest", "with_extra"] {
        assert_eq!(checks(source, name), 1, "{name}\n{}", kernel_text(source));
    }
}

/// L20: metadata follows escaped functions; expansions run once and binding
/// failures occur after evaluating arguments, before entering the body.
#[test]
fn dynamic_keyword_binding_preserves_python_argument_order() {
    ran(
        r#"
events = []
def mark(name, value):
    events.append(name)
    return value
def f(a, /, b=2, *tail, c, **extra):
    events.append('body')
    return [a, b, tail, c, extra]
g = [f][0]
print(g(*mark('args', [1, 3, 4]), c=mark('c', 5), **mark('kw', {'x': 6})))
print(events)
try:
    g(1, **{'c': 2}, **{'c': 3})
except TypeError:
    print('duplicate')
first, *middle, last = [1, 2, 3, 4]
print([first, middle, last])
print([0, *[1, 2], 3], (*[1], 2), {*[1, 2], 2}, {'x': 0, **{'x': 1}})
try:
    g()
except TypeError:
    print('missing')
def reraiser():
    raise
alias = reraiser
try:
    try:
        raise ValueError('active')
    except ValueError:
        alias()
except ValueError:
    print('reraised')
"#,
        &[
            "[1, 3, (4,), 5, {'x': 6}]",
            "['args', 'c', 'kw', 'body']",
            "duplicate",
            "[1, [2, 3], 4]",
            "[0, 1, 2, 3] (1, 2) {1, 2} {'x': 1}",
            "missing",
            "reraised",
        ],
    );
    let described =
        super::machine::lower("def f(a, /, b=2, *tail, c, **extra):\n    return a\n").unwrap();
    assert!(
        described
            .written
            .values()
            .any(|w| w.signature == "(a, /, b=2, *tail, c, **extra)"),
        "{:?}",
        described.written
    );
    // Typed crossing freezes the token data and the packed callable's
    // captured body; a second callable must never hide in token data.
    use {
        lash_kernel_value::EffectName, lash_kernel_value::Name, lash_kernel_value::Param,
        lash_kernel_value::Signature, lash_kernel_value::Type,
    };
    let signature = Signature {
        params: vec![Param {
            name: Name::new("value"),
            ty: Type::Int,
            optional: false,
        }],
        result: Type::Int,
    };
    let effects = std::collections::BTreeMap::from([(
        EffectName::new("host.accept").unwrap(),
        Signature {
            params: vec![Param {
                name: Name::new("callback"),
                ty: Type::Function(Box::new(signature)),
                optional: false,
            }],
            result: Type::Null,
        },
    )]);
    let mut crossed = false;
    let (_, end) = super::machine::drive(
        "def callback(*, value: int) -> int:\n    return value + 1\nawait host.accept(callback)\n",
        effects,
        Default::default(),
        |_, park, text| {
            park.requests
                .into_iter()
                .map(|request| {
                    let lash_kernel_vm::Request::Effect(effect) = request;
                    let lash_kernel_doc::Datum::FunctionValue(value) = &effect.args[0] else {
                        panic!("typed function crossing")
                    };
                    assert_eq!(
                        value.convention,
                        lash_kernel_doc::StartConvention::PyArgsList { positional: 0 }
                    );
                    value
                        .check(
                            super::machine::library(),
                            lash_kernel_doc::FunctionValueLimits::standard(),
                            lash_kernel_value::data_limits::KernelDataLimits::standard(),
                        )
                        .unwrap();
                    assert_eq!(
                        super::machine::callable_result(
                            text,
                            (**value).clone(),
                            vec![lash_kernel_doc::Datum::Int(41.into())]
                        ),
                        lash_kernel_doc::Datum::Int(42.into())
                    );
                    crossed = true;
                    (
                        effect.wait,
                        lash_kernel_vm::Outcome::Completed(lash_kernel_doc::Datum::Null),
                    )
                })
                .collect()
        },
    );
    assert!(crossed, "{end:?}");
    assert!(matches!(end, lash_kernel_vm::End::Finished(_)), "{end:?}");
}

/// L24: aliases retain each builtin's argument kinds; map and filter stay
/// lazy, and type/exception aliases participate in runtime dispatch.
#[test]
fn builtin_values_and_direct_calls_bind_arguments_identically() {
    ran(
        r#"
f = len
print(f([1, 2]), callable(f), callable(3))
convert = int
print(convert('ff', base=16), type(1) is int, isinstance(True, (str, int)))
kind = ValueError
classes = (TypeError, kind)
try:
    raise kind('bad')
except classes as error:
    print(isinstance(error, kind), error)
seen = []
def twice(value):
    seen.append(value)
    return value * 2
items = map(twice, [1, 2, 3])
print(seen)
print(next(items), seen, list(items))
print(list(filter(None, [0, 1, '', 2])))
print(format(12, '04d'), pow(2, 10, 17), pow(3, -1, 11))
print(hex(-16), oct(8), bin(3), ascii('é'))
print(sorted(['aaa', 'b'], key=len))
iterator = iter([0, 1, 2, 3])
print(any(iterator), next(iterator), tuple(iterator))
iterator = iter([1, 2, 3])
print(2 in iterator, next(iterator))
try:
    len(iter([]))
except TypeError:
    print('not a sequence')
try:
    try:
        raise ValueError('invalid handler')
    except (ValueError, 1):
        print('must not match')
except TypeError:
    print('invalid handler')
try:
    format(1, 2)
except TypeError:
    print('format type')
print(callable(('py.function', 0, ())), callable(('py.function', 0, (), 'fake', None)))
try:
    try:
        raise ValueError('nested')
    except ((ValueError,),):
        print('must not match')
except TypeError:
    print('nested handler')
"#,
        &[
            "2 True False",
            "255 True True",
            "True bad",
            "[]",
            "2 [1, 2, 3] [4, 6]",
            "[1, 2]",
            "0012 4 4",
            "-0x10 0o10 0b11 '\\xe9'",
            "['b', 'aaa']",
            "True 2 (3,)",
            "True 3",
            "not a sequence",
            "invalid handler",
            "format type",
            "False False",
            "nested handler",
        ],
    );
    ran(
        r#"
import math
import json
root = math.sqrt
encode = json.dumps
print(root(x=9), encode({'b': 2, 'a': 1}, sort_keys=True, separators=(',', ':')))
rows = []
push = rows.append
push(*[1])
rows.extend(*[[2]])
print(rows)
render = '{name}:{0:04d}'.format
print(render(*[7], **{'name': 'item'}))
split = 'a b c'.split
print(split(' ', 1), split(maxsplit=1))
get = getattr
print(get('abc', 'upper')())
minimum = min
print(minimum([], default=9), minimum(['aa', 'b'], key=len))
zipper = zip
print(list(zipper([1], [2], strict=True)))
show = print
show('a', 'b', sep=':', end='!', flush=True)
format_value = format
print(format_value(12.5, '08.2f'))
"#,
        &[
            "3.0 {\"a\":1,\"b\":2}",
            "[1, 2]",
            "item:0007",
            "['a', 'b c'] ['a', 'b c']",
            "ABC",
            "9 b",
            "[(1, 2)]",
            "a:b!",
            "00012.50",
        ],
    );
}

/// L28: the same cursor and lazy callback state survive each checkpoint;
/// exhaustion is sticky and default=None is distinct from no default.
#[test]
fn python_iterators_resume_with_the_same_remaining_elements() {
    let source = r#"
values = [1, 2]
iterator = iter(values)
print(next(iterator), iter(iterator) is iterator)
await host.echo(0)
values.append(3)
print(next(iterator), list(iterator), next(iterator, None))
values.append(4)
try:
    next(iterator)
except StopIteration:
    print('stopped')
s = slice(None, None, -1)
print([1, 2, 3][s], s.start, s.stop, s.step)
source = [1, 2, 0, 3]
print(list(iter(lambda: source.pop(0), 0)), source)
"#;
    let lowered = super::machine::lower(source).unwrap();
    let program = super::linked::program(
        std::sync::Arc::clone(lowered.document.shared()),
        super::machine::installed().prepared.clone(),
    );
    let uninterrupted =
        super::machine::execution_observation(program.clone(), super::machine::BOUNDS, false);
    let restored = super::machine::execution_observation(program, super::machine::BOUNDS, true);
    assert_eq!(uninterrupted, restored);
    assert_eq!(
        restored.lines,
        [
            "1 True",
            "2 [3] None",
            "stopped",
            "[3, 2, 1] None None -1",
            "[1, 2] [3]"
        ]
    );
}

/// L26: multiple expansions preserve input order; return_exceptions keeps
/// failures as values while siblings complete normally.
#[test]
fn gather_return_exceptions_keeps_input_order() {
    ran(
        r#"
import asyncio
async def good(value):
    await asyncio.sleep(0)
    return value
async def bad():
    raise ValueError('bad')
a = [asyncio.create_task(good(1))]
b = [asyncio.create_task(bad())]
values = await asyncio.gather(*a, *b, good(3), **{'return_exceptions': True})
print(values[0], isinstance(values[1], ValueError), values[2])
try:
    await asyncio.gather(good(4), bad(), return_exceptions=False)
except ValueError:
    print('raised')
await asyncio.sleep(0)
"#,
        &["1 True 3", "raised"],
    );
}

/// L29: name inventories contain only bound guest names. Snapshots isolate
/// namespace writes; object dictionaries remain live.
#[test]
fn guest_locals_never_expose_host_bindings() {
    ran(
        r#"
x = 1
snapshot = globals()
x = 2
print(snapshot['x'], globals()['x'])
def inspect(arg):
    local = 3
    reader = locals
    names = reader()
    names['local'] = 9
    print(names['arg'], local, 'host' in names, 'local' in dir())
    print(vars()['local'])
inspect(7)
d = {'guest': 1}
vars(d)['guest'] = 2
print(d, dir(d))
import math
print('upper' in dir('x'), 'sqrt' in dir(math), '__py_module' in dir(math))
"#,
        &[
            "1 2",
            "7 3 False True",
            "3",
            "{'guest': 2} ['guest']",
            "True True False",
        ],
    );
}
