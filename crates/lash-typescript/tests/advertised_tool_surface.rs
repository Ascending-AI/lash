//! The advertised form of a tool operation must be the callable form.
//!
//! A rendered catalog is a promise: the model reads the declaration as ground
//! truth and calls exactly what it says. Mangling a reserved-word path into
//! `__lash_tool_<hex>` broke that promise in the one direction that cannot be
//! recovered from — the advertised identifier had no binding, so calling it
//! rejected with `TS_UNKNOWN_BINDING` for itself while the natural dotted path
//! the catalog never mentioned worked (FIG-1444).

use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome, State, Value,
};
use serde_json::json;

struct ToolCallRecordingHost {
    dispatched: std::sync::Mutex<Vec<(String, String)>>,
}

impl ExecutionHost for ToolCallRecordingHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(call) => {
                let alias = match &call.receiver {
                    Value::Resource(handle) => handle.alias.clone(),
                    other => format!("{other:?}"),
                };
                self.dispatched
                    .lock()
                    .expect("dispatched lock")
                    .push((alias, call.operation));
                Ok(AbilityResult::Value(Value::String("tool-ok".into())))
            }
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            other => Err(ExecutionHostError::new(format!(
                "unexpected ability {other:?}"
            ))),
        }
    }
}

fn input_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": { "id": { "type": "string" } },
        "required": ["id"]
    })
}

fn advertise(call_path: &str) -> String {
    lash_typescript::render_tool_signature(
        call_path,
        &input_schema(),
        Some(&json!({ "type": "string" })),
    )
}

/// Links and runs `await <call_path>({ id: "m1" })` against a host binding
/// registered for `modules`/`operation`, returning what the host was asked to
/// dispatch. A call path that does not reach the binding fails here.
fn dispatch(call_path: &str, modules: &[&str], operation: &str) -> Vec<(String, String)> {
    let mut catalog = lashlang::LashlangHostCatalog::new();
    catalog
        .add_module_operation_binding(
            modules
                .iter()
                .map(|module| module.to_string())
                .collect::<Vec<_>>(),
            "ToolModule",
            operation,
            format!("tool:test/{}", modules.join("_")),
            lashlang::ResourceOperationBinding {
                input_ty: lashlang::TypeExpr::Any,
                output_ty: lashlang::TypeExpr::Any,
                output_from_input: None,
            },
        )
        .expect("operation binding");
    let environment =
        lashlang::LashlangHostEnvironment::new(catalog, lashlang::LashlangAbilities::default());
    let source = format!(r#"finish(await {call_path}({{ id: "m1" }}));"#);
    let linked = lash_typescript::link(&source, &environment)
        .unwrap_or_else(|error| panic!("`{source}` must link: {error:?}"));
    let host = ToolCallRecordingHost {
        dispatched: std::sync::Mutex::new(Vec::new()),
    };
    let outcome = futures::executor::block_on(lashlang::execute(
        &lash_typescript::compile_linked(&linked),
        &mut State::new(),
        &host,
    ))
    .unwrap_or_else(|error| panic!("`{source}` must execute: {error:?}"));
    assert_eq!(
        outcome,
        ExecutionOutcome::Finished(Value::String("tool-ok".into())),
        "`{source}` must finish the dispatched result"
    );
    host.dispatched.lock().expect("dispatched lock").clone()
}

#[test]
fn reserved_word_operation_is_advertised_in_its_callable_form() {
    let declaration = advertise("inbox.delete");
    assert_eq!(
        declaration,
        "declare const inbox: { delete(input: { id: string }): Promise<string> };"
    );
    lash_typescript::ensure_tool_call_path_addressable("inbox.delete")
        .expect("the advertised identifier must be callable");
    assert_eq!(
        dispatch("inbox.delete", &["inbox"], "delete"),
        vec![("inbox".to_string(), "delete".to_string())]
    );
}

#[test]
fn nested_reserved_word_operation_declares_the_tail_as_nested_properties() {
    let declaration = advertise("inbox.alpha.delete");
    assert_eq!(
        declaration,
        "declare const inbox: { alpha: { delete(input: { id: string }): Promise<string> } };"
    );
    lash_typescript::ensure_tool_call_path_addressable("inbox.alpha.delete")
        .expect("the advertised identifier must be callable");
    assert_eq!(
        dispatch("inbox.alpha.delete", &["inbox", "alpha"], "delete"),
        vec![("inbox.alpha".to_string(), "delete".to_string())]
    );
}

/// `type`, `get`, `any`, `string` and friends are reserved only where
/// TypeScript expects a declaration name; a cell writes them as ordinary
/// identifiers. Mangling a module path spelled with one advertised a callable
/// nothing for a path the lowerer accepts as written.
#[test]
fn contextual_keyword_module_paths_are_advertised_as_written() {
    assert_eq!(
        advertise("type.check"),
        "declare const type: { check(input: { id: string }): Promise<string> };"
    );
    assert_eq!(
        dispatch("type.check", &["type"], "check"),
        vec![("type".to_string(), "check".to_string())]
    );
    assert_eq!(
        advertise("get.thing"),
        "declare const get: { thing(input: { id: string }): Promise<string> };"
    );
    assert_eq!(
        dispatch("get.thing", &["get"], "thing"),
        vec![("get".to_string(), "thing".to_string())]
    );
}

/// Only the root of a path is written in expression position, so a reserved word
/// deeper in the module path is callable — `outer.class.op(…)` lowers to the tool
/// — and has to be advertised that way.
#[test]
fn reserved_words_inside_a_module_path_are_advertised_as_properties() {
    assert_eq!(
        advertise("outer.class.op"),
        "declare const outer: { class: { op(input: { id: string }): Promise<string> } };"
    );
    assert_eq!(
        dispatch("outer.class.op", &["outer", "class"], "op"),
        vec![("outer.class".to_string(), "op".to_string())]
    );
}

/// A module segment ECMAScript forbids in expression position cannot be written
/// by any cell, so no advertisement can be honest. It keeps the mangled
/// rendering and the addressability check refuses it — registration turns that
/// refusal into a rejected tool instead of an uncallable catalog entry.
#[test]
fn module_paths_no_cell_can_write_are_refused_rather_than_advertised() {
    for call_path in ["delete.thing", "new.thing", "class.list", "for.each.item"] {
        let declaration = advertise(call_path);
        assert!(
            declaration.contains("__lash_tool_"),
            "{call_path} cannot be advertised as callable: {declaration}"
        );
        let error = lash_typescript::ensure_tool_call_path_addressable(call_path)
            .expect_err("an unwritable module path must be refused");
        assert!(
            matches!(
                error.code,
                lash_typescript::DiagnosticCode::SyntaxError
                    | lash_typescript::DiagnosticCode::MethodUnsupported
            ),
            "{call_path}: {error:?}"
        );
    }
}

/// A single-segment name has no receiver to call it on, so it is refused rather
/// than advertised — the mangled rendering stays for callers that render a
/// non-path name.
#[test]
fn names_without_a_module_path_are_refused() {
    let declaration = advertise("search-docs");
    assert!(declaration.contains("__lash_tool_"), "{declaration}");
    let error = lash_typescript::ensure_tool_call_path_addressable("search-docs")
        .expect_err("a name with no module path is not addressable");
    assert_eq!(
        error.code,
        lash_typescript::DiagnosticCode::UnknownBinding,
        "{error:?}"
    );
}
