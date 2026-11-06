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

pub(crate) fn forms() -> Vec<(String, String)> {
    let mut forms = Vec::new();
    for annotation in ["any", "number", "string"] {
        for operator in crate::constructs::operators::BINARY {
            forms.push((
                format!("{annotation} {}", operator.token),
                format!(
                    "export function demo(x: {annotation}): any {{ return x {} x; }}",
                    operator.token
                ),
            ));
        }
        for operator in crate::constructs::operators::UNARY {
            forms.push((
                format!("{annotation} {}", operator.token),
                format!(
                    "export function demo(x: {annotation}): any {{ return {} x; }}",
                    operator.token
                ),
            ));
        }
    }
    for (class, _) in lash_ext_ts_helpers::builtins::table().constructors.iter() {
        forms.push((
            format!("new {class}"),
            format!("export function demo(): any {{ return new {class}(); }}"),
        ));
    }
    for (class, _) in lash_ext_ts_helpers::builtins::table().instance_tests.iter() {
        forms.push((
            format!("instanceof {class}"),
            format!("export function demo(x: any): any {{ return x instanceof {class}; }}"),
        ));
    }
    for (name, body) in [
        ("holes", "return [0, , ...x, ,];"),
        ("computed object", "return {[x]: 1, a: 2, ...x};"),
        ("optional field", "return x?.a;"),
        ("optional index", "return x?.[1];"),
        ("optional call", "return x?.(1);"),
        ("optional delete", "return delete x?.a;"),
        ("update", "return x++;"),
        ("arguments", "return arguments;"),
        ("object default", "const {a = 1, ...rest} = x; return rest;"),
        ("array default", "const [a = 1, ...rest] = x; return rest;"),
        ("dynamic constructor", "return new x(1);"),
        ("spread method", "return x.f(...[1, 2]);"),
        (
            "for of",
            "for (const item of x) { if (item) return item; } return null;",
        ),
        (
            "for in",
            "for (const key in x) { if (key) return key; } return null;",
        ),
        (
            "exception",
            "try { throw x; } catch (error) { return error; }",
        ),
        ("template escape", "return `a\\n${x}\\`${x}`;"),
    ] {
        forms.push((
            name.into(),
            format!("export function demo(x: any): any {{ {body} }}"),
        ));
    }
    #[derive(serde::Deserialize)]
    struct Case {
        name: String,
        source: String,
        #[serde(default)]
        cell: bool,
    }
    let corpus: Vec<Case> = serde_json::from_str(include_str!("../readable/corpus.json")).unwrap();
    forms.extend(
        corpus
            .into_iter()
            .filter(|case| case.name.starts_with("FIG-7664") && !case.cell)
            .map(|case| (case.name, case.source)),
    );
    // Pin the complete front end's admission boundary, including its AST check.
    let namespaces = Default::default();
    let environment = lash_kernel_dialect::Environment::new(crate::tests::library(), &namespaces);
    for (name, pattern) in [
        ("nested comparisons", "x === x ? x : ($)"),
        ("nested calls", "Number($)"),
        ("nested records", "{value: $}"),
        ("nested logical reads", "x.a || ($)"),
        ("nested numeric expressions", "x + ($)"),
        ("nested unary expressions", "-($)"),
        ("nested member reads", "($).a"),
    ] {
        let mut expression = "x".to_owned();
        let mut last = None;
        loop {
            let source = format!("export function demo(x: number): any {{ return {expression}; }}");
            if let Err(error) =
                crate::lower(&source, &environment, lash_kernel_dialect::Mode::Module)
            {
                assert_eq!(
                    error.code(),
                    crate::DiagnosticCode::SourceNestingLimit,
                    "{name}: {error}"
                );
                break;
            }
            last = Some(source);
            expression = pattern.replace('$', &expression);
        }
        forms.push((name.into(), last.unwrap()));
    }
    forms
}
