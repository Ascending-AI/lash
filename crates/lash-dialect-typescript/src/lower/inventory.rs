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
    for annotation in ["any", "number", "string", "bigint", "boolean"] {
        for operator in crate::constructs::operators::BINARY {
            for right in ["any", "number", "string", "bigint", "boolean"] {
                forms.push((
                    format!("{annotation} {} {right}", operator.token),
                    format!(
                        "export function demo(x: {annotation}, y: {right}): any {{ return x {} y; }}",
                        operator.token
                    ),
                ));
            }
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
    for (path, declaration) in lash_ext_ts_helpers::builtins::table().positional.iter() {
        for annotation in ["any", "number", "string", "bigint", "boolean"] {
            for expression in ["x", "x + 1", "1"] {
                let args = vec![expression; declaration.arity].join(", ");
                forms.push((
                    format!("{path}({args}), {annotation}"),
                    format!(
                        "export function demo(x: {annotation}): any {{ return {path}({args}); }}"
                    ),
                ));
            }
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
        ("do while", "do { x = x - 1; } while (x > 0); return x;"),
        (
            "do while exits",
            "do { if (x.a) break; if (x.b) continue; x = x.c; } while (x); return x;",
        ),
        (
            "do while true",
            "do { x = x.next; if (!x) break; } while (true); return x;",
        ),
        (
            "counted for",
            "let s = 0; for (let i = 0; i < x; i++) { s += i; } return s;",
        ),
        ("for without parts", "for (;;) { if (x) break; } return x;"),
        ("while", "while (x) { x = x.next; } return x;"),
        (
            "labeled loops",
            "outer: for (const a of x) { for (const b of a) { if (b) continue outer; break outer; } } return x;",
        ),
        (
            "switch",
            "switch (x) { case 1: return 'a'; case 'b': break; default: x = 2; } return x;",
        ),
        (
            "regular expression",
            "return x.match(/W\\s*[123]\\s*:\\s*\\d+/gi);",
        ),
        ("closure", "const f = (y: any) => x + y; return f(1);"),
        ("conditional", "return x ? x.a : x.b;"),
        ("nullish", "return x ?? 1;"),
        ("logical", "return x && x.a || x.b;"),
        ("typeof", "return typeof x;"),
        ("in", "return 'a' in x;"),
        ("delete", "delete x.a; return x;"),
        ("compound", "x += 1; x ??= 2; x ||= 3; x &&= 4; return x;"),
        ("throw", "if (!x) throw new Error('e'); return x;"),
        ("try finally", "try { return x.a; } finally { x = 1; }"),
        ("destructuring", "const {a, b: [c]} = x; return a + c;"),
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
            .filter(|case| {
                (case.name.starts_with("FIG-7664") || case.name.starts_with("FIG-7666"))
                    && !case.cell
            })
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

/// Every member the built-in table gives, on a value of no known kind, on
/// each receiver an annotation states, and on a map and a set the body
/// builds: a new row reaches the printer through this table alone. The
/// front end refuses some rows on some receivers (`eval`); those lower
/// nothing to print.
pub(crate) fn members() -> Vec<(String, String)> {
    let mut forms = Vec::new();
    let table = lash_ext_ts_helpers::builtins::table();
    use lash_ext_ts_helpers::builtins::Receiver;
    let receivers = [
        (None, "any", "x"),
        (Some(Receiver::Text), "string", "x"),
        (Some(Receiver::Number), "number", "x"),
        (Some(Receiver::Bool), "boolean", "x"),
        (Some(Receiver::BigInt), "bigint", "x"),
        (Some(Receiver::List), "any[]", "x"),
        (Some(Receiver::Map), "any", "new Map([[x, x]])"),
        (Some(Receiver::Set), "any", "new Set([x])"),
    ];
    for (kind, annotation, receiver) in receivers {
        let answers = |rows: &lash_ext_ts_helpers::builtins::Rows| {
            kind.is_none_or(|kind| rows.iter().any(|(row, _)| *row == kind))
        };
        let bind = format!("const r = {receiver};");
        for (name, _) in table.methods.iter().filter(|(_, rows)| answers(rows)) {
            forms.push((
                format!("{receiver}: {annotation} .{name}()"),
                format!(
                    "export function demo(x: {annotation}, y: any): any {{ {bind} return r.{name}(y); }}"
                ),
            ));
        }
        for (name, _) in table.properties.iter().filter(|(_, rows)| answers(rows)) {
            forms.push((
                format!("{receiver}: {annotation} .{name}"),
                format!("export function demo(x: {annotation}): any {{ {bind} return r.{name}; }}"),
            ));
        }
    }
    // A prototype's method is its receiver's member above, not a global call.
    for (path, _) in table
        .functions
        .iter()
        .filter(|(path, _)| !path.contains(".prototype."))
    {
        forms.push((
            format!("{path}(x)"),
            format!("export function demo(x: any): any {{ return {path}(x); }}"),
        ));
    }
    for (path, _) in table.values.iter() {
        forms.push((
            path.to_string(),
            format!("export function demo(): any {{ return {path}; }}"),
        ));
    }
    forms
}
