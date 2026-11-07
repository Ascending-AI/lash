//! The lowerer's emitted helper inventory, captured at its only resolution point.
use lash_kernel_value::FunctionId;
use std::cell::RefCell;
use std::collections::BTreeMap;

type Inventory = BTreeMap<String, FunctionId>;
thread_local! { static EMITTED: RefCell<Option<Inventory>> = const { RefCell::new(None) }; }

pub(crate) fn record(name: &str, id: FunctionId) {
    EMITTED.with_borrow_mut(|inventory| {
        if let Some(inventory) = inventory {
            inventory.insert(name.into(), id);
        }
    });
}

pub(crate) fn collect<T>(run: impl FnOnce() -> T) -> (T, Inventory) {
    struct Restore(Option<Inventory>);
    impl Drop for Restore {
        fn drop(&mut self) {
            EMITTED.with_borrow_mut(|current| *current = self.0.take());
        }
    }
    let restore = Restore(EMITTED.replace(Some(BTreeMap::new())));
    let result = run();
    let inventory = EMITTED.take().expect("an active lowering inventory");
    drop(restore);
    (result, inventory)
}

/// Representative inputs to syntax lowering. Helper membership is recorded by
/// `Lowerer::function`, never copied into this input corpus.
pub(crate) fn forms() -> Vec<(String, String)> {
    let mut forms: Vec<(String, String)> = [
        ("fstring", "return f'hello {x}!'"),
        ("fstring repr", "return f'{x!r}'"),
        ("fstring ascii", "return f'{x!a}'"),
        ("fstring format", "return f'{x:04d}'"),
        ("fstring nested format", "return f'{x:{x}}'"),
        ("list comprehension", "return [item for item in x]"),
        (
            "filtered comprehension",
            "return [item for item in x if item]",
        ),
        ("set comprehension", "return {item for item in x}"),
        ("dict comprehension", "return {item: item for item in x}"),
        ("slice", "return x[1:3:2]"),
        ("slice omitted", "return x[::]"),
        ("list spread", "return [1, *x, 2]"),
        ("dict spread", "return {'a': 1, **x}"),
        ("tuple", "return (1, x)"),
        ("ellipsis", "return ..."),
        ("unpack", "a, *rest = x\n    return rest"),
        ("delete", "del x[0]\n    return x"),
        ("delete slice", "del x[1:]\n    return x"),
        ("set slice", "x[1:] = [2]\n    return x"),
        ("walrus", "return (y := x)"),
    ]
    .into_iter()
    .map(|(name, body)| {
        (
            name.into(),
            format!("def demo(x: Any) -> Any:\n    {body}\n"),
        )
    })
    .collect();
    for (name, body) in [
        ("assert", "assert x, 'no'\n    return x"),
        ("raise", "raise ValueError(x)"),
        ("raise cause", "raise ValueError(x) from None"),
        ("with", "with x as item:\n        return item"),
        (
            "match sequence",
            "match x:\n        case [first, *rest]:\n            return rest\n    return None",
        ),
        (
            "match mapping",
            "match x:\n        case {'a': a, **rest}:\n            return rest\n    return None",
        ),
        (
            "range",
            "for item in range(x):\n        if item:\n            return item\n    return None",
        ),
        (
            "dict loop",
            "for key, value in x.items():\n        return value\n    return None",
        ),
        ("starred tuple", "return (1, *x)"),
        ("starred set", "return {1, *x}"),
        (
            "nested comprehension",
            "return [(a, b) for a in x for b in x if b]",
        ),
        ("bytes", "return b'a\\x00\\xff'"),
        ("call spread", "return x(*[1, 2], **{'a': 1})"),
    ] {
        forms.push((
            name.into(),
            format!("def demo(x: Any) -> Any:\n    {body}\n"),
        ));
    }
    for annotation in ["Any", "int", "float", "str", "bool"] {
        for expression in ["f'{x}'", "f'value {x}!'", "str(x)", "repr(x)", "ascii(x)"] {
            forms.push((
                format!("{annotation} {expression}"),
                format!("def demo(x: {annotation}) -> Any:\n    return {expression}\n"),
            ));
        }
        for operator in crate::constructs::operators::BINARY
            .iter()
            .filter_map(|row| row.token())
            .collect::<std::collections::BTreeSet<_>>()
        {
            for right in ["Any", "int", "float", "str", "bool"] {
                forms.push((
                    format!("{annotation} {operator} {right}"),
                    format!(
                        "def demo(x: {annotation}, y: {right}) -> Any:\n    return x {operator} y\n"
                    ),
                ));
            }
        }
        for operator in crate::constructs::operators::UNARY {
            forms.push((
                format!("{annotation} {}", operator.token),
                format!(
                    "def demo(x: {annotation}) -> Any:\n    return {} x\n",
                    if operator.token == "neg" {
                        "-"
                    } else {
                        operator.token
                    }
                ),
            ));
        }
        for comparison in crate::constructs::operators::COMPARISONS {
            forms.push((
                format!("{annotation} {}", comparison.inverse),
                format!(
                    "def demo(x: {annotation}) -> Any:\n    return x {} x\n",
                    comparison.inverse
                ),
            ));
        }
    }
    forms
}
