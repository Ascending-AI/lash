use lashlang::{TypeExpr, TypeField, json_schema_to_type_expr};
use serde_json::Value;

/// Renders one tool's JSON schemas as a TypeScript declaration using the
/// shared Lash type engine's schema importer.
pub fn render_tool_signature(
    name: &str,
    input_schema: &Value,
    output_schema: Option<&Value>,
) -> String {
    let input = json_schema_to_type_expr(input_schema);
    let output = output_schema.map_or(TypeExpr::Any, json_schema_to_type_expr);
    let segments = name.split('.').collect::<Vec<_>>();
    if segments.len() > 1
        && segments
            .iter()
            .all(|segment| is_identifier(segment) && !is_reserved_word(segment))
    {
        let (operation, modules) = segments.split_last().expect("non-empty call path");
        let mut declaration = format!(
            "function {operation}(input: {}): Promise<{}>;",
            render_type(&input),
            render_type(&output)
        );
        for module in modules.iter().rev() {
            declaration = format!("declare namespace {module} {{ {declaration} }}");
        }
        declaration
    } else {
        format!(
            "declare function {}(input: {}): Promise<{}>;",
            render_identifier(name),
            render_type(&input),
            render_type(&output)
        )
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
        TypeExpr::Enum(values) => values
            .iter()
            .map(|value| serde_json::to_string(value.as_str()).expect("strings serialize"))
            .collect::<Vec<_>>()
            .join(" | "),
        TypeExpr::List(item) => format!("Array<{}>", render_type(item)),
        TypeExpr::Object(fields) => render_object(fields),
        TypeExpr::Ref(name) => render_identifier(name),
        TypeExpr::Process { input, output, .. } => {
            format!("Process<{}, {}>", render_type(input), render_type(output))
        }
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
    matches!(
        name,
        "abstract"
            | "accessor"
            | "any"
            | "as"
            | "asserts"
            | "async"
            | "await"
            | "bigint"
            | "boolean"
            | "break"
            | "case"
            | "catch"
            | "class"
            | "const"
            | "constructor"
            | "continue"
            | "debugger"
            | "declare"
            | "default"
            | "delete"
            | "do"
            | "else"
            | "enum"
            | "export"
            | "extends"
            | "false"
            | "finally"
            | "for"
            | "from"
            | "function"
            | "get"
            | "global"
            | "if"
            | "implements"
            | "import"
            | "in"
            | "infer"
            | "instanceof"
            | "interface"
            | "is"
            | "keyof"
            | "let"
            | "module"
            | "namespace"
            | "never"
            | "new"
            | "null"
            | "number"
            | "object"
            | "of"
            | "override"
            | "package"
            | "private"
            | "protected"
            | "public"
            | "readonly"
            | "require"
            | "return"
            | "satisfies"
            | "set"
            | "static"
            | "string"
            | "super"
            | "switch"
            | "symbol"
            | "this"
            | "throw"
            | "true"
            | "try"
            | "type"
            | "typeof"
            | "undefined"
            | "unique"
            | "unknown"
            | "using"
            | "var"
            | "void"
            | "while"
            | "with"
            | "yield"
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn renders_schema_through_shared_type_engine() {
        let signature = render_tool_signature(
            "search-docs",
            &json!({
                "type": "object",
                "additionalProperties": false,
                "properties": { "query": { "type": "string" }, "limit": { "type": "integer" } },
                "required": ["query"]
            }),
            Some(&json!({ "type": "array", "items": { "type": "string" } })),
        );
        assert_eq!(
            signature,
            "declare function __lash_tool_7365617263682d646f6373(input: { limit?: number; query: string }): Promise<Array<string>>;"
        );
    }
}
