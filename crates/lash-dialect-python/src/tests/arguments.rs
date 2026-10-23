//! FIG-6961: a function's own `def` keeps its missing-argument check only
//! where a call can leave a parameter without a default unfilled
//! (`scope::always_filled`).

use super::machine::{kernel_text, output};

fn checks(source: &str, name: &str) -> usize {
    let document = super::machine::lower(source).unwrap().document;
    let target =
        lash_kernel_dialect::Library::resolve(super::machine::library(), "py.arg_missing").unwrap();
    let mut pending = document
        .units()
        .filter(|(unit, _)| match unit {
            lash_vocabulary::sites::Unit::Library(id) => {
                lash_kernel_doc::FunctionCatalog::header(super::machine::library(), id).is_none()
            }
            lash_vocabulary::sites::Unit::Main => true,
            _ => false,
        })
        .map(|(_, block)| lash_kernel_doc::Node::Block(block))
        .collect::<Vec<_>>();
    let mut nodes = Vec::new();
    while let Some(node) = pending.pop() {
        pending.extend(node.children());
        nodes.push(node);
    }
    nodes.into_iter().filter(|node| match node {
        lash_kernel_doc::Node::Expr(lash_kernel_doc::Expr::Act(action)) => matches!(action.kind, lash_kernel_doc::ActKind::Call(lash_kernel_doc::Callee::Local(function)) if function == target) && matches!(action.args.first(), Some(lash_kernel_doc::Expr::Literal(lash_kernel_doc::Literal::Text(value))) if value == name),
        lash_kernel_doc::Node::Action(lash_kernel_doc::Action::Call { callee: lash_kernel_doc::Callee::Local(function), args }) => *function == target && matches!(args.first(), Some(lash_kernel_doc::Atom::Literal(lash_kernel_doc::Literal::Text(value))) if value == name),
        _ => false,
    }).count()
}

fn ran(source: &str, lines: &[&str]) {
    let expected: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
    assert_eq!(output(source), (expected, "ok".to_string()), "{source}");
}

/// A nested `def` read only as the callee of calls that give each required
/// parameter, by position or by name, recursion included, runs no check.
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
/// a call through the escaped value raises Python's TypeError in the body.
#[test]
fn an_escaping_def_keeps_its_argument_check() {
    for (escape, call) in [
        // stored
        ("holder = [f]\n    return holder[0]()", "outer()"),
        // passed
        (
            "def call(g):\n        return g()\n    return call(f)",
            "outer()",
        ),
        // returned
        ("return f", "outer()()"),
        // captured by a closure that escapes and hands it on
        ("def h():\n        return f\n    return h", "outer()()()"),
    ] {
        let source = format!(
            "def outer():\n    def f(a):\n        return a\n    {escape}\ntry:\n    {call}\nexcept TypeError as error:\n    print(error)\n"
        );
        ran(&source, &["f() missing a required positional argument"]);
        assert_eq!(checks(&source, "f"), 1, "{source}");
    }
}

/// A name bound twice, or named by a `nonlocal`, is not a function bound
/// once: every def it holds keeps its check.
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
/// check, and that call raises in the body as before.
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

/// A call made before the `def` binds the name still meets the unbound
/// name, through the absent slot and `py.name_check`; the def itself, only
/// ever called in full, has no argument check.
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
