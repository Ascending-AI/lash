use crate::{Diagnostic, DiagnosticCode};
use lash_sansio::{ExtraKeys, ObjectShape, SchemaShape, ShapeKind};

/// The dialect contract lives with the runtime that implements it:
/// `lash_vm` owns the signature rows and derives the VM's normalization
/// arities from their prose, so this crate's lowering, rendering, and
/// receiver-kind answers are projections rather than a second statement.
pub(crate) use lash_vm::{
    INSTANCE_STDLIB_SIGNATURES, LiteralReceivers, STATIC_STDLIB_SIGNATURES, StdlibSignature,
};

/// Keeping this text derived from the same tables that drive lowering makes a
/// newly accepted method impossible to omit from the prompt accidentally.
pub fn render_stdlib_contract() -> String {
    fn render(signatures: &[StdlibSignature]) -> String {
        signatures
            .iter()
            .map(|signature| {
                let arguments = signature.arguments;
                format!("`{}.{}`({arguments})", signature.owner, signature.method)
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    format!(
        "Static calls: {}.\n\nInstance calls: {}.",
        render(STATIC_STDLIB_SIGNATURES),
        render(INSTANCE_STDLIB_SIGNATURES)
    )
}

/// Number of owner-qualified calls in the v1 standard-library contract.
pub fn stdlib_name_count() -> usize {
    STATIC_STDLIB_SIGNATURES.len() + INSTANCE_STDLIB_SIGNATURES.len()
}

/// Spells a schema shape as a TypeScript type.
///
/// The shape is the contract layer's one reading of a JSON Schema; this is
/// TypeScript's spelling of it and reads no schema itself. An object keeps
/// its fields whether or not it is closed, and gains an index signature only
/// when the schema says extra keys are allowed.
pub fn render_schema_shape(shape: &SchemaShape) -> String {
    match &shape.kind {
        ShapeKind::Unknown => "unknown".to_string(),
        ShapeKind::Null => "null".to_string(),
        ShapeKind::Bool => "boolean".to_string(),
        ShapeKind::Int | ShapeKind::Float => "number".to_string(),
        ShapeKind::Str => "string".to_string(),
        ShapeKind::Literals(values) => {
            render_union(values.iter().map(serde_json::Value::to_string))
        }
        ShapeKind::List(item) => format!("Array<{}>", render_schema_shape(item)),
        ShapeKind::Tuple(items) => format!(
            "[{}]",
            items
                .iter()
                .map(render_schema_shape)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        ShapeKind::Object(object) => render_object(object),
        ShapeKind::Union(members) => render_union(members.iter().map(render_schema_shape)),
        ShapeKind::Named(name) => render_type_name(name),
        ShapeKind::Process(None) => "Process".to_string(),
        ShapeKind::Process(Some(signature)) => format!(
            "Process<[{}], {}>",
            signature
                .params
                .iter()
                .map(|param| format!("{}: {}", param.name, render_schema_shape(&param.shape)))
                .collect::<Vec<_>>()
                .join(", "),
            render_schema_shape(&signature.output)
        ),
        // `ShapeKind` is non-exhaustive: a kind this dialect has no spelling
        // for yet is shown as the widest type rather than guessed at.
        _ => "unknown".to_string(),
    }
}

/// Alternatives joined with `|`. `int` and `float` are both `number`, so
/// members that spell the same are shown once.
fn render_union(members: impl Iterator<Item = String>) -> String {
    let mut spelled = Vec::<String>::new();
    for member in members {
        if !spelled.contains(&member) {
            spelled.push(member);
        }
    }
    spelled.join(" | ")
}

/// Confirms a TypeScript cell can address `call_path` verbatim as a tool call.
///
/// The check lowers the call the catalog advertises instead of re-deriving the
/// dialect's name rules, so every reason a path is unreachable — a reserved word
/// no cell can write in expression position, an ECMA global namespace the
/// lowerer resolves itself, a method name the dialect refuses outright — is
/// honored by construction and cannot drift from the lowerer. Registration calls
/// it so a tool whose path the dialect resolves to anything other than a tool
/// call is refused rather than advertised as a callable nothing (FIG-1444).
pub fn ensure_tool_call_path_addressable(call_path: &str) -> Result<(), Diagnostic> {
    let segments = call_path.split('.').collect::<Vec<_>>();
    #[expect(
        clippy::expect_used,
        reason = "`str::split` always yields at least one segment, so the vector is never empty"
    )]
    let (operation, modules) = segments.split_last().expect("split never yields nothing");
    if modules.is_empty() {
        return Err(Diagnostic::new(
            DiagnosticCode::UnknownBinding,
            format!(
                "tool call path `{call_path}` has no module path, so a TypeScript cell has no receiver to call it on"
            ),
            None,
        ));
    }
    // The inner diagnostic answers a different question than the caller asked:
    // `Math.floor` fails the probe as `TS_AWAIT_UNSUPPORTED`, which reads as an
    // instruction to drop the `await` rather than as "this path names an
    // ECMAScript global, so no tool can live under it". Lead with the reason the
    // path is unadvertisable and carry the inner diagnostic as the detail.
    let detail = match crate::parse(&format!("finish(await {call_path}({{}}));")) {
        Ok(program) if addresses_tool(&program.main, modules, operation) => return Ok(()),
        Ok(_) => "the dialect resolves that call itself instead of dispatching a tool".to_string(),
        Err(inner) => format!(
            "that cell is refused as {}: {}",
            inner.code.as_str(),
            inner.message
        ),
    };
    Err(Diagnostic::refusal(
        DiagnosticCode::MethodUnsupported,
        format!(
            "tool call path `{call_path}` does not dispatch a tool in a TypeScript cell, so advertising it would promise a callable no binding provides: writing `await {call_path}(input)` names a word no cell can write in expression position, an ECMAScript global namespace, or a method the dialect claims for itself — {detail}"
        ),
        None,
    ))
}

fn addresses_tool(expr: &lash_vm::Expr, modules: &[&str], operation: &str) -> bool {
    match expr {
        lash_vm::Expr::ReceiverCall {
            receiver,
            operation: called,
            ..
        } if called.as_str() == operation => match receiver.as_ref() {
            lash_vm::Expr::ResourceRef(resource) => resource
                .path
                .iter()
                .map(|segment| segment.as_str())
                .eq(modules.iter().copied()),
            _ => false,
        },
        _ => expr
            .children()
            .any(|child| addresses_tool(child, modules, operation)),
    }
}

fn render_object(object: &ObjectShape) -> String {
    let extra = match &object.extra_keys {
        ExtraKeys::Closed => None,
        // A schema that names its fields and says nothing about other keys
        // is shown as those fields; one that names none accepts anything.
        ExtraKeys::Unstated if object.fields.is_empty() => Some("unknown".to_string()),
        ExtraKeys::Unstated => None,
        ExtraKeys::Open(extra) => Some(render_schema_shape(extra)),
        _ => Some("unknown".to_string()),
    };
    if object.fields.is_empty() {
        return match extra {
            Some(extra) => format!("Record<string, {extra}>"),
            None => "Record<string, never>".to_string(),
        };
    }
    let mut members = object
        .fields
        .iter()
        .map(|field| {
            format!(
                "{}{}: {}",
                render_property_name(&field.name),
                if field.required { "" } else { "?" },
                render_schema_shape(&field.shape)
            )
        })
        .collect::<Vec<_>>();
    if let Some(extra) = extra {
        members.push(format!("[key: string]: {extra}"));
    }
    format!("{{ {} }}", members.join("; "))
}

/// A named host type as an identifier this dialect can write.
///
/// Host data types are dotted names (`cron.Tick`): a valid schema `$ref`,
/// not a valid TypeScript name. The one spelling — dots to underscores —
/// is used in the `type … =` declaration and in every `ShapeKind::Named`
/// reference alike, so the model is never shown a name it cannot resolve
/// against the declaration above it. A name still not writable — a reserved
/// word, or one colliding with the generated-binding namespace — takes the
/// generated encoding instead.
pub fn render_type_name(name: &str) -> String {
    render_identifier(&name.replace('.', "_"))
}

fn render_identifier(name: &str) -> String {
    const GENERATED_PREFIX: &str = "__lash_tool_";
    if is_identifier(name) && !is_reserved_word(name) && !name.starts_with(GENERATED_PREFIX) {
        name.to_string()
    } else {
        let encoded = name
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        format!("{GENERATED_PREFIX}{encoded}")
    }
}

#[expect(
    clippy::expect_used,
    reason = "the value is a `str`, and `serde_json` never fails to encode one"
)]
fn render_property_name(name: &str) -> String {
    if is_identifier(name) && !is_reserved_word(name) {
        name.to_string()
    } else {
        serde_json::to_string(name).expect("strings serialize")
    }
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first == '$' || first.is_alphabetic())
        && chars
            .all(|character| character == '_' || character == '$' || character.is_alphanumeric())
}

fn is_reserved_word(name: &str) -> bool {
    RESERVED_WORDS.contains(&name)
}

/// Every word the renderer declines to emit in identifier position.
///
/// Exposed through [`reserved_words`] so the advertisement sweep can be driven
/// by this table instead of a hand-copied one: a word added here is swept for
/// callability without anybody remembering to extend the test.
const RESERVED_WORDS: &[&str] = &[
    "abstract",
    "accessor",
    "any",
    "as",
    "asserts",
    "async",
    "await",
    "bigint",
    "boolean",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "constructor",
    "continue",
    "debugger",
    "declare",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "from",
    "function",
    "get",
    "global",
    "if",
    "implements",
    "import",
    "in",
    "infer",
    "instanceof",
    "interface",
    "is",
    "keyof",
    "let",
    "module",
    "namespace",
    "never",
    "new",
    "null",
    "number",
    "object",
    "of",
    "override",
    "package",
    "private",
    "protected",
    "public",
    "readonly",
    "require",
    "return",
    "satisfies",
    "set",
    "static",
    "string",
    "super",
    "switch",
    "symbol",
    "this",
    "throw",
    "true",
    "try",
    "type",
    "typeof",
    "undefined",
    "unique",
    "unknown",
    "using",
    "var",
    "void",
    "while",
    "with",
    "yield",
];

/// Every word this dialect declines to render in identifier position.
pub fn reserved_words() -> &'static [&'static str] {
    RESERVED_WORDS
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use lash_vm::{TypeExpr, TypeField};
    use serde_json::Value;

    fn render_schema(schema: &Value) -> String {
        render_schema_shape(&SchemaShape::from_json_schema(schema))
    }

    fn render_type(ty: &TypeExpr) -> String {
        render_schema(&lash_vm::type_expr_to_json_schema(ty))
    }

    #[test]
    fn inferred_process_signature_matches_schema_typescript_and_artifact() {
        let linked = crate::link(
            "const worker = async (query: string, retries: number): Promise<boolean> => { return true; }; finish(worker);",
            &lash_vm::LashVmHostEnvironment::default(),
        ).expect("link typed process");
        let process = linked
            .artifact
            .ir()
            .declarations
            .iter()
            .find_map(|declaration| {
                if let lash_vm::Declaration::Process(process) = declaration {
                    Some(process)
                } else {
                    None
                }
            })
            .expect("compiled process declaration");
        let expected = TypeExpr::Process(lash_vm::ProcessType::known(
            lash_vm::ProcessSignature::try_new(
                vec![
                    lash_vm::ProcessParam {
                        name: "query".into(),
                        ty: TypeExpr::Str,
                    },
                    lash_vm::ProcessParam {
                        name: "retries".into(),
                        ty: TypeExpr::Float,
                    },
                ],
                TypeExpr::Bool,
            )
            .expect("expected named signature"),
        ));
        let inferred = linked
            .artifact
            .process_type(&process.name)
            .expect("inferred output");
        assert_eq!(inferred, expected);
        let schema = lash_vm::type_expr_to_json_schema(&inferred);
        assert_eq!(
            lash_vm::json_schema_to_type_expr(&schema).expect("schema signature"),
            expected
        );
        assert_eq!(
            render_schema(&schema),
            "Process<[query: string, retries: number], boolean>"
        );
        let retained = lash_vm::ModuleArtifact::from_store_bytes(
            &linked.artifact.to_store_bytes().expect("encode artifact"),
        )
        .expect("decode retained artifact");
        assert_eq!(retained.process_type(&process.name), Some(expected));
        assert_eq!(
            retained.ir().declarations,
            linked.artifact.ir().declarations
        );
    }

    fn assert_async_output(source: &str, output: TypeExpr, schema: Value) {
        assert_async_output_in_environment(
            source,
            output,
            schema,
            &lash_vm::LashVmHostEnvironment::default(),
        );
    }

    fn assert_async_output_in_environment(
        source: &str,
        output: TypeExpr,
        schema: Value,
        environment: &lash_vm::LashVmHostEnvironment,
    ) {
        let linked = crate::link(source, environment).expect("link async process");
        let process = linked
            .artifact
            .ir()
            .declarations
            .iter()
            .find_map(|declaration| match declaration {
                lash_vm::Declaration::Process(process) => Some(process),
                _ => None,
            })
            .expect("lifted process");
        let expected = TypeExpr::Process(lash_vm::ProcessType::known(
            lash_vm::ProcessSignature::try_new(vec![], output.clone()).expect("signature"),
        ));
        let inferred = linked
            .artifact
            .process_type(&process.name)
            .expect("process type");
        assert_eq!(inferred, expected);
        assert_eq!(process.return_ty, Some(output.clone()));
        let process_schema = lash_vm::type_expr_to_json_schema(&inferred);
        assert_eq!(process_schema["x-lash"]["signature"]["output"], schema);
        let retained = lash_vm::ModuleArtifact::from_store_bytes(
            &linked.artifact.to_store_bytes().expect("encode artifact"),
        )
        .expect("decode artifact");
        assert_eq!(retained.process_type(&process.name), Some(expected.clone()));
        assert_eq!(
            retained.ir().declarations,
            linked.artifact.ir().declarations
        );
    }

    #[test]
    fn async_process_promise_number_signature_and_schema() {
        assert_async_output(
            "const worker = async (): Promise<number> => { return 42; }; finish(worker);",
            TypeExpr::Float,
            json!({"type": "number"}),
        );
    }

    #[test]
    fn async_process_promise_string_signature_and_schema() {
        assert_async_output(
            "const worker = async (): Promise<string> => { return 'ready'; }; finish(worker);",
            TypeExpr::Str,
            json!({"type": "string"}),
        );
    }

    #[test]
    fn async_process_unannotated_boolean_signature_and_schema() {
        assert_async_output(
            "const worker = async () => { return true; }; finish(worker);",
            TypeExpr::Bool,
            json!({"type": "boolean"}),
        );
    }

    #[test]
    fn async_process_output_annotation_survives_workflow_rendering() {
        for (source, output) in [
            (
                "const worker = async () => { return 42; }; finish(worker);",
                TypeExpr::Int,
            ),
            (
                "const worker = async (): boolean => true; finish(worker);",
                TypeExpr::Bool,
            ),
            (
                "const worker = async (): Promise<boolean | string> => { return true; }; finish(worker);",
                TypeExpr::union(vec![TypeExpr::Bool, TypeExpr::Str]),
            ),
            (
                "const worker = async (): Promise<{ ready: boolean }> => { return { ready: true }; }; finish(worker);",
                TypeExpr::Object(vec![TypeField {
                    name: "ready".into(),
                    ty: TypeExpr::Bool,
                    optional: false,
                }]),
            ),
        ] {
            let environment = lash_vm::LashVmHostEnvironment::default();
            let linked = crate::link(source, &environment).expect("link process");
            for graph in [
                crate::workflow_graph::workflow_graph_from_source(source).expect("source graph"),
                lash_vm::workflow_graph_from_artifact(&linked.artifact),
            ] {
                let rendered =
                    crate::workflow_graph::workflow_graph_to_source(&graph).expect("render graph");
                let relinked =
                    crate::link(&rendered, &environment).expect("relink rendered process");
                let process = relinked
                    .artifact
                    .ir()
                    .declarations
                    .iter()
                    .find_map(|declaration| match declaration {
                        lash_vm::Declaration::Process(process) => Some(process),
                        _ => None,
                    })
                    .expect("rendered process");
                assert_eq!(process.return_ty, Some(output.clone()), "{rendered}");
            }
        }
    }

    #[test]
    fn async_process_awaited_tool_signature_and_schema() {
        let mut catalog = lash_vm::LashVmHostCatalog::new();
        catalog
            .add_module_operation(
                ["tools"],
                "Tools",
                "check",
                "tool:check",
                TypeExpr::Any,
                TypeExpr::Bool,
            )
            .expect("tool catalogue");
        let environment = lash_vm::LashVmHostEnvironment::new(catalog);
        assert_async_output_in_environment(
            "const worker = async () => { return await tools.check({}); }; finish(worker);",
            TypeExpr::Bool,
            json!({"type": "boolean"}),
            &environment,
        );
    }

    #[test]
    fn async_process_awaited_helper_signature_and_schema() {
        assert_async_output(
            "const worker = async () => { return await (async (value = true) => { return true; })(); }; finish(worker);",
            TypeExpr::Bool,
            json!({"type": "boolean"}),
        );
    }

    #[test]
    fn async_process_catch_return_signature_and_schema() {
        assert_async_output(
            "const worker = async () => { try { throw 'failed'; } catch (error) { return true; } }; finish(worker);",
            TypeExpr::Bool,
            json!({"type": "boolean"}),
        );
    }

    #[test]
    fn async_process_finally_overrides_return_signature_and_schema() {
        assert_async_output(
            "const worker = async () => { try { return 'ignored'; } finally { return true; } }; finish(worker);",
            TypeExpr::Bool,
            json!({"type": "boolean"}),
        );
    }

    #[test]
    fn async_process_nested_returns_do_not_escape_signature_and_schema() {
        assert_async_output(
            "const worker = async () => { function nested() { return 'ignored'; } return true; }; finish(worker);",
            TypeExpr::Bool,
            json!({"type": "boolean"}),
        );
    }

    #[test]
    fn async_process_fallthrough_signature_and_schema() {
        assert_async_output(
            "const worker = async () => { if (true) { return true; } }; finish(worker);",
            TypeExpr::union(vec![TypeExpr::Bool, TypeExpr::Null]),
            json!({"anyOf": [{"type": "boolean"}, {"type": "null"}]}),
        );
    }

    #[test]
    fn async_process_rejects_incompatible_return_annotation() {
        let error = crate::link(
            "const worker = async (): Promise<boolean> => { return 42; }; finish(worker);",
            &lash_vm::LashVmHostEnvironment::default(),
        )
        .expect_err("incompatible output");
        assert!(error.message.contains("bool"), "{error:?}");
        assert!(error.message.contains("int"), "{error:?}");
    }

    #[test]
    fn async_process_rejects_non_durable_return_annotation() {
        let error = crate::parse(
            "const worker = async (): Promise<() => boolean> => { return true; }; finish(worker);",
        )
        .expect_err("non-durable output");
        assert_eq!(error.code.as_str(), "TS_PROCESS_RETURN_TYPE_UNSUPPORTED");
    }

    #[test]
    fn a_closed_object_renders_its_fields_required_first() {
        let ty = render_schema(&json!({
            "type": "object", "additionalProperties": false,
            "properties": { "query": { "type": "string" }, "limit": { "type": "integer" } },
            "required": ["query"]
        }));
        assert_eq!(ty, "{ query: string; limit?: number }");
    }

    /// FIG-4544: an MCP schema almost never sets `additionalProperties:
    /// false`. Its fields are still its fields, and an index signature
    /// appears only where the schema says extra keys are allowed.
    #[test]
    fn an_open_object_keeps_every_field_name_and_type() {
        assert_eq!(
            render_schema(&json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "minLength": 1 },
                    "filter": {
                        "type": "object",
                        "properties": {
                            "tags": { "type": "array", "items": { "type": "string" } },
                            "mode": { "enum": ["any", "all"] }
                        }
                    },
                    "page": { "type": ["integer", "null"] }
                },
                "required": ["query"]
            })),
            r#"{ query: string; filter?: { mode?: "any" | "all"; tags?: Array<string> }; page?: number | null }"#
        );
        assert_eq!(
            render_schema(&json!({
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "additionalProperties": { "type": "integer" }
            })),
            "{ name?: string; [key: string]: number }"
        );
        assert_eq!(
            render_schema(&json!({ "type": "object" })),
            "Record<string, unknown>"
        );
        assert_eq!(
            render_schema(&json!({ "type": "object", "additionalProperties": false })),
            "Record<string, never>"
        );
        assert_eq!(
            render_schema(&json!({ "anyOf": [{ "type": "integer" }, { "type": "number" }] })),
            "number"
        );
        assert_eq!(
            render_schema(&json!({ "type": "array", "prefixItems": [{ "type": "string" }, {}] })),
            "[string, unknown]"
        );
    }

    #[test]
    fn renders_named_and_unknown_process_types_without_fabricating_a_signature() {
        let process = |params: Vec<lash_vm::ProcessParam>| {
            TypeExpr::Process(lash_vm::ProcessType::known(
                lash_vm::ProcessSignature::try_new(params, TypeExpr::Bool).unwrap(),
            ))
        };
        let param = |name: &str, ty| lash_vm::ProcessParam {
            name: name.into(),
            ty,
        };

        assert_eq!(render_type(&process(vec![])), "Process<[], boolean>");
        assert_eq!(
            render_type(&process(vec![param("message", TypeExpr::Str)])),
            "Process<[message: string], boolean>"
        );
        assert_eq!(
            render_type(&process(vec![param(
                "payload",
                TypeExpr::Object(vec![TypeField {
                    name: "value".into(),
                    ty: TypeExpr::Str,
                    optional: false,
                }]),
            )])),
            "Process<[payload: { value: string; [key: string]: unknown }], boolean>"
        );
        assert_eq!(
            render_type(&process(vec![
                param("left", TypeExpr::Str),
                param("right", TypeExpr::Int),
            ])),
            "Process<[left: string, right: number], boolean>"
        );
        assert_eq!(
            render_type(&TypeExpr::Process(lash_vm::ProcessType::unknown())),
            "Process"
        );
    }
}
