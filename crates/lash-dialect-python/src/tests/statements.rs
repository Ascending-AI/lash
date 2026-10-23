//! Python statement binding and cleanup laws (FIG-7289).

use super::machine;

fn lines(source: &str, expected: &[&str]) {
    let result = machine::run(source, &[]);
    assert_eq!(result.end, "ok", "{result:?}");
    assert_eq!(result.lines(), expected);
}

#[test]
fn function_decorators_apply_bottom_up() {
    lines(
        r#"
trace = []
def decorator(n):
    trace.append(n)
    def wrap(f):
        trace.append(n + 10)
        def wrapped(x=3):
            return f(x) + n
        return wrapped
    return wrap
@decorator(1)
@decorator(2)
def f(x=3):
    return x
print(trace, f(), f(4))
try:
    @7
    def invalid():
        pass
except TypeError:
    print('not callable')
"#,
        &["[1, 2, 12, 11] 6 7", "not callable"],
    );
}

#[test]
fn walrus_binds_outside_the_comprehension() {
    lines(
        r#"
x = 99
def f():
    values = [(last := x * 2) for x in [1, 2, 3] if (keep := x > 1)]
    print(values, last, keep)
    print((once := 7), once)
f()
print(x)
f = lambda: (local := 8)
print(f())
"#,
        &["[4, 6] 6 True", "7 7", "99", "8"],
    );
}

#[test]
fn erased_type_syntax_has_no_runtime_binding() {
    lines(
        r#"
type Numbers = list[int]
def first[T](xs: Numbers) -> T:
    return xs[0]
print(first([4, 5]))
"#,
        &["4"],
    );
    let effects = machine::effects();
    let namespaces = super::linked::namespaces(&effects, machine::controls()).unwrap();
    let lowered = crate::lower(
        "type Pair[T] = tuple[T, T]\ndef run(x: Pair[int]) -> list[int]:\n    return [x[0]]\n",
        &lash_kernel_dialect::Environment::new(machine::library(), &namespaces),
        lash_kernel_dialect::Mode::Module,
    )
    .unwrap();
    let lash_kernel_doc::Export::Function(export) =
        lowered.document.exports.values().next().unwrap()
    else {
        panic!("function export")
    };
    assert_eq!(
        export.signature.params[0].ty,
        lash_vocabulary::Type::Tuple(vec![lash_vocabulary::Type::Int; 2])
    );
    let error = machine::lower("type Number = int\nprint(Number)\n").unwrap_err();
    assert_eq!(error.code, crate::Code::UnknownName.as_str());
}

#[test]
fn globals_keep_the_module_binding_identity() {
    lines(
        r#"
x = 1
def outer():
    x = 2
    def inner():
        global x
        print(x)
        x = 3
    inner()
    print(x)
outer()
print(x)
"#,
        &["1", "2", "3"],
    );
}

#[test]
fn slice_assignment_and_delete_preserve_aliases() {
    lines(
        r#"
xs = [0, 1, 2, 3, 4]
alias = xs
trace = []
def rhs():
    trace.append('value')
    return [8, 9]
def target():
    trace.append('target')
    return xs
def bound():
    trace.append('bound')
    return 1
target()[bound():3] = rhs()
print(trace, alias)
xs[::-2] = [5, 6, 7]
print(alias)
try:
    xs[::2] = [1]
except ValueError:
    print('cardinality', alias)
xs[1:3] = xs
print(alias)
del xs[::-2]
print(alias)
a = 1
b = 2
del (a, b), xs[:1]
try:
    print(a)
except NameError:
    print('unbound', alias)
def local():
    x = 1
    del x
    try:
        print(x)
    except UnboundLocalError:
        print('local unbound')
local()
trace.clear()
target()[bound():2] += rhs()
print(trace, alias)
"#,
        &[
            "['value', 'target', 'bound'] [0, 8, 9, 3, 4]",
            "[7, 8, 6, 3, 5]",
            "cardinality [7, 8, 6, 3, 5]",
            "[7, 7, 8, 6, 3, 5, 3, 5]",
            "[7, 8, 3, 3]",
            "unbound [8, 3, 3]",
            "local unbound",
            "['target', 'bound', 'value'] [8, 3, 8, 9, 3]",
        ],
    );
}

#[test]
fn match_guards_observe_only_successful_pattern_bindings() {
    lines(
        r#"
trace = []
x = 'old'
def subject():
    trace.append('subject')
    return {'a': [1, 2, 3], 'b': 4}
def guard(v):
    trace.append(v)
    return False
match subject():
    case {'a': [x, 99]}:
        print('wrong')
    case {'a': [x, *tail], **rest} if guard(x):
        print('wrong')
    case {'a': ([1, *tail] | [2, *tail]) as whole, **rest}:
        print(x, tail, whole, rest)
print(trace)
match [7, 8]:
    case [x, 0]:
        pass
print(x)
match None:
    case True:
        print('wrong')
    case None:
        print('none')
"#,
        &["1 [2, 3] [1, 2, 3] {'b': 4}", "['subject', 1]", "1", "none"],
    );
}

#[test]
fn context_manager_exit_sees_the_active_exception() {
    lines(
        r#"
trace = []
def enter():
    trace.append('enter')
    return 7
def leave(kind, error, traceback):
    assert kind is (ValueError if error is not None else None)
    trace.append(error.args if error is not None else 'normal')
    return True
manager = {'__enter__': enter, '__exit__': leave}
with manager as value:
    print(value)
    raise ValueError('bad')
with manager:
    trace.append('body')
def early():
    with manager:
        return 9
print(early(), trace)
async def aenter():
    return await host.echo(8)
async def aexit(kind, error, traceback):
    assert kind is ValueError
    print(error.args)
    await host.echo('exit')
    return True
async with {'__aenter__': aenter, '__aexit__': aexit} as value:
    print(value)
    raise ValueError('async')
def propagate(kind, error, traceback):
    print('propagating', error.args)
    return False
try:
    with {'__enter__': enter, '__exit__': propagate}:
        raise ValueError('kept')
except ValueError:
    print('caught')
def failed_enter():
    raise ValueError('enter failed')
try:
    with {'__enter__': failed_enter, '__exit__': propagate}:
        print('wrong')
except ValueError:
    print('enter failed without exit')
"#,
        &[
            "7",
            "9 ['enter', ('bad',), 'enter', 'body', 'normal', 'enter', 'normal']",
            "8",
            "('async',)",
            "propagating ('kept',)",
            "caught",
            "enter failed without exit",
        ],
    );
}
