use crate::{Diagnostic, DiagnosticCode};
use lashlang::{TypeExpr, TypeField, json_schema_to_type_expr};
use serde_json::Value;

/// The dialect contract lives with the runtime that implements it:
/// `lashlang` owns the signature rows and derives the VM's normalization
/// arities from their prose, so this crate's lowering, rendering, and
/// receiver-kind answers are projections rather than a second statement.
pub(crate) use lashlang::{
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

/// Spells a JSON schema as a TypeScript type using the shared type engine.
///
/// A schema the importer refuses is rendered as the widest type rather than
/// failing: this is prompt-facing documentation, and the same schema is
/// refused with a typed diagnostic where it actually enters the catalog.
pub fn render_schema_type(schema: &Value) -> String {
    render_type(&json_schema_to_type_expr(schema).unwrap_or(lashlang::TypeExpr::Any))
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

fn addresses_tool(expr: &lashlang::Expr, modules: &[&str], operation: &str) -> bool {
    match expr {
        lashlang::Expr::ReceiverCall {
            receiver,
            operation: called,
            ..
        } if called.as_str() == operation => match receiver.as_ref() {
            lashlang::Expr::ResourceRef(resource) => resource
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

fn render_type(ty: &TypeExpr) -> String {
    match ty {
        TypeExpr::Any => "unknown".to_string(),
        TypeExpr::Str => "string".to_string(),
        TypeExpr::Int | TypeExpr::Float => "number".to_string(),
        TypeExpr::Bool => "boolean".to_string(),
        TypeExpr::Dict => "Record<string, unknown>".to_string(),
        TypeExpr::Null => "null".to_string(),
        #[expect(
            clippy::expect_used,
            reason = "the values are `str`s, and `serde_json` never fails to encode one"
        )]
        TypeExpr::Enum(values) => values
            .iter()
            .map(|value| serde_json::to_string(value.as_str()).expect("strings serialize"))
            .collect::<Vec<_>>()
            .join(" | "),
        TypeExpr::List(item) => format!("Array<{}>", render_type(item)),
        TypeExpr::Object(fields) => render_object(fields),
        TypeExpr::Ref(name) => render_identifier(name),
        TypeExpr::Process(process) => match process.as_signature() {
            Some(signature) => format!(
                "Process<[{}], {}>",
                signature
                    .params()
                    .iter()
                    .map(|param| format!("{}: {}", param.name, render_type(&param.ty)))
                    .collect::<Vec<_>>()
                    .join(", "),
                render_type(signature.output())
            ),
            None => "Process".to_string(),
        },
        TypeExpr::TriggerHandle(event) => format!("TriggerHandle<{}>", render_type(event)),
        TypeExpr::Union(items) => items
            .iter()
            .map(render_type)
            .collect::<Vec<_>>()
            .join(" | "),
    }
}

fn render_object(fields: &[TypeField]) -> String {
    if fields.is_empty() {
        return "Record<string, never>".to_string();
    }
    format!(
        "{{ {} }}",
        fields
            .iter()
            .map(|field| format!(
                "{}{}: {}",
                render_property_name(field.name.as_str()),
                if field.optional { "?" } else { "" },
                render_type(&field.ty)
            ))
            .collect::<Vec<_>>()
            .join("; ")
    )
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

    /// Shorthand for the receiver-kind column of
    /// [`INSTANCE_STDLIB_SIGNATURES`], so a signature assertion stays readable
    /// at one line.
    use super::LiteralReceivers as On;

    use super::*;

    #[test]
    fn renders_schema_through_shared_type_engine() {
        let ty = render_schema_type(&json!({
            "type": "object", "additionalProperties": false,
            "properties": { "query": { "type": "string" }, "limit": { "type": "integer" } },
            "required": ["query"]
        }));
        assert_eq!(ty, "{ limit?: number; query: string }");
    }

    #[test]
    fn renders_named_and_unknown_process_types_without_fabricating_a_signature() {
        let process = |params: Vec<lashlang::ProcessParam>| {
            TypeExpr::Process(lashlang::ProcessType::known(
                lashlang::ProcessSignature::try_new(params, TypeExpr::Bool).unwrap(),
            ))
        };
        let param = |name: &str, ty| lashlang::ProcessParam {
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
            "Process<[payload: { value: string }], boolean>"
        );
        assert_eq!(
            render_type(&process(vec![
                param("left", TypeExpr::Str),
                param("right", TypeExpr::Int),
            ])),
            "Process<[left: string, right: number], boolean>"
        );
        assert_eq!(
            render_type(&TypeExpr::Process(lashlang::ProcessType::unknown())),
            "Process"
        );
    }

    /// The literal-receiver matrix the hand-written arms in the lowerer used to
    /// carry, transcribed once. The size of each column is the whole claim: the
    /// arms held 28 string, 33 array, 5 number and 3 remaining-literal names,
    /// and this table restates them with array `valueOf` added, which is the
    /// one divergence FIG-1718 resolves. A method widened onto a shape it did
    /// not carry moves a count and fails here rather than quietly enlarging the
    /// accepted surface.
    #[test]
    fn literal_receiver_column_restates_the_matrix_it_replaced() {
        let carrying = |kind: LiteralReceivers| {
            INSTANCE_STDLIB_SIGNATURES
                .iter()
                .filter(|signature| signature.receivers.contains(kind))
                .count()
        };
        assert_eq!(carrying(On::STRING), 28, "string literal receivers");
        assert_eq!(carrying(On::ARRAY), 35, "array literal receivers");
        assert_eq!(carrying(On::NUMBER), 5, "number literal receivers");
        assert_eq!(carrying(On::OTHER), 3, "remaining literal receivers");

        // Cardinality alone lets a compensating swap through — one method moved
        // off arrays and another moved on keeps the count. The array column is
        // the one this ticket changed, so pin its membership outright; the other
        // three are small enough that a swap inside them is caught by the string
        // and number columns disagreeing about the same name.
        let carried_by = |kind: LiteralReceivers| {
            let mut names = INSTANCE_STDLIB_SIGNATURES
                .iter()
                .filter(|signature| signature.receivers.contains(kind))
                .map(|signature| signature.method)
                .collect::<Vec<_>>();
            names.sort_unstable();
            names
        };
        assert_eq!(
            carried_by(On::ARRAY),
            [
                "at",
                "concat",
                "copyWithin",
                "every",
                "fill",
                "filter",
                "find",
                "findIndex",
                "findLast",
                "findLastIndex",
                "flat",
                "flatMap",
                "forEach",
                "includes",
                "indexOf",
                "join",
                "lastIndexOf",
                "map",
                "pop",
                "push",
                "reduce",
                "reduceRight",
                "reverse",
                "shift",
                "slice",
                "some",
                "sort",
                "splice",
                "toReversed",
                "toSorted",
                "toSpliced",
                "toString",
                "unshift",
                "valueOf",
                "with",
            ]
        );
        assert_eq!(
            carried_by(On::NUMBER),
            [
                "toExponential",
                "toFixed",
                "toPrecision",
                "toString",
                "valueOf"
            ]
        );
        assert_eq!(
            carried_by(On::OTHER),
            ["hasOwnProperty", "toString", "valueOf"]
        );
    }

    /// The receiver-kind column is keyed by method name, so a second row for a
    /// name already in the table would answer the lookup only by accident of
    /// ordering.
    #[test]
    fn instance_method_names_are_unique() {
        let mut names = INSTANCE_STDLIB_SIGNATURES
            .iter()
            .map(|signature| signature.method)
            .collect::<Vec<_>>();
        names.sort_unstable();
        let total = names.len();
        names.dedup();
        assert_eq!(names.len(), total, "duplicate instance method name");
    }

    /// A static call names an owner namespace rather than a receiver, so its
    /// rows must not claim a literal receiver shape.
    #[test]
    fn static_signatures_carry_no_receiver_kind() {
        assert!(
            STATIC_STDLIB_SIGNATURES
                .iter()
                .all(|signature| signature.receivers == LiteralReceivers::NONE)
        );
    }
}
