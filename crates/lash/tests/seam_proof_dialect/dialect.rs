//! A seam proof, not a language: the smallest code-mode dialect that shows a
//! second front end plugs into the host through the public `Dialect` seam
//! alone. It lives only in this test target.
//!
//! One statement per line, two forms:
//!
//! ```text
//! take NAME from MODULE.OPERATION WITH {json object}
//! give VALUE        where VALUE is NAME, NAME.FIELD or a JSON literal
//! ```

use lash::rlm::lang::{AssignTarget, Expr, Program, ResourceRefExpr, Span};
use lash::rlm::{
    CellTags, Dialect, DialectDiagnostic, DialectPromptVocabulary, DialectRefusal,
    DialectRefusalKind, ExecutionSectionRequest, LashlangHostEnvironment, ResolvedToolBinding,
    RlmChannel, ShapeNotation,
};

pub const LANGUAGE_ID: &str = "seam-proof";

pub const CELL_TAGS: CellTags = CellTags {
    open: "<seam>",
    close: "</seam>",
};

pub struct SeamProofDialect;

impl Dialect for SeamProofDialect {
    fn language_id(&self) -> &'static str {
        LANGUAGE_ID
    }

    fn parse(&self, source: &str) -> Result<Program, DialectDiagnostic> {
        let mut statements = Vec::new();
        for line in source
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
        {
            statements.push(statement(line).ok_or_else(|| refusal(source, line))?);
        }
        Ok(Program::block(statements))
    }

    fn parse_cell(
        &self,
        source: &str,
        _host: &LashlangHostEnvironment,
    ) -> Result<Program, DialectDiagnostic> {
        self.parse(source)
    }

    fn tool_call_path(&self, binding: &ResolvedToolBinding) -> Result<String, DialectRefusal> {
        let path = binding.call_path();
        if path.split('.').all(is_name) {
            Ok(path)
        } else {
            Err(DialectRefusal {
                kind: DialectRefusalKind::UnaddressableToolPath,
                message: format!("`{path}` is not a seam-proof call path"),
            })
        }
    }

    fn tool_signature(
        &self,
        call_path: &str,
        input_schema: &serde_json::Value,
        output_schema: &serde_json::Value,
    ) -> String {
        format!(
            "{call_path} WITH {} GIVES {}",
            schema_notation(input_schema),
            schema_notation(output_schema)
        )
    }

    fn render_tool_example(&self, _authored: &str) -> Option<String> {
        None
    }

    fn prompt_vocabulary(&self) -> DialectPromptVocabulary {
        DialectPromptVocabulary {
            language_name: "Seam proof",
            execution_title: "Seam proof execution",
            cell_tags: CELL_TAGS,
            cell_noun: "cell",
            history_type: "Log",
            print_call: "give",
            print_statement_prefix: "give ",
            print_statement_suffix: "",
            finish_name: "give",
            finish_statement: "give VALUE",
            finish_null_statement: "give null",
            continue_as_call: "take r from control.continue_as WITH {...}",
            continue_as_example: r#"take r from control.continue_as WITH {"task": "go on"}"#,
            field_miss_rule: "Use only the field names listed below.",
            shape_notation: NOTATION,
        }
    }

    fn history_item_definition(&self, _images: bool) -> Vec<String> {
        vec!["shape Log is seq of Rec".to_string()]
    }

    fn render_execution_section(&self, request: ExecutionSectionRequest<'_>) -> String {
        let transport = match request.channel {
            RlmChannel::Cell => {
                "Write the program between standalone `<seam>` and `</seam>` lines."
            }
            RlmChannel::NativeTool => "Send the program as the `code` of one `execute_code` call.",
        };
        format!(
            "{transport} One statement per line. `take NAME from MODULE.OPERATION WITH {{json}}` calls a tool and names its result; `give VALUE` ends the turn with VALUE, a name, `name.field` or a JSON literal.\n\n### Tools\n\n{}",
            request.tools
        )
    }
}

const NOTATION: ShapeNotation = ShapeNotation {
    any: "Anything",
    null: "Nothing",
    bool: "Flag",
    int: "Whole",
    float: "Real",
    str: "Text",
    record: "Rec",
    list_open: "seq of ",
    list_close: "",
    union_separator: " or ",
    definition_keyword: "shape ",
    definition_assign: " is ",
    record_open: "(",
    field_indent: "  ",
    field_separator: " -> ",
    field_terminator: ";",
    record_close: ")",
};

fn statement(line: &str) -> Option<Expr> {
    if let Some(value) = line.strip_prefix("give ") {
        return Some(Expr::Finish(Box::new(value_expr(value.trim())?)));
    }
    let rest = line.strip_prefix("take ")?;
    let (name, rest) = rest.split_once(" from ")?;
    let (path, arguments) = rest.split_once(" WITH ")?;
    let (module, operation) = path.trim().rsplit_once('.')?;
    let name = name.trim();
    if !is_name(name) || !module.split('.').all(is_name) || !is_name(operation) {
        return None;
    }
    let call = Expr::ReceiverCall {
        receiver: Box::new(Expr::ResourceRef(ResourceRefExpr::unresolved(
            module.split('.').map(Into::into).collect(),
        ))),
        operation: operation.into(),
        args: vec![json_expr(&serde_json::from_str(arguments.trim()).ok()?)],
    };
    Some(Expr::Assign {
        target: AssignTarget::variable(name.into()),
        expr: Box::new(Expr::Await(Box::new(Expr::ResultUnwrap(Box::new(call))))),
    })
}

fn value_expr(text: &str) -> Option<Expr> {
    if let Ok(value) = serde_json::from_str(text) {
        return Some(json_expr(&value));
    }
    match text.split_once('.') {
        Some((name, field)) if is_name(name) && is_name(field) => Some(Expr::Field {
            target: Box::new(Expr::Variable(name.into())),
            field: field.into(),
        }),
        None if is_name(text) => Some(Expr::Variable(text.into())),
        _ => None,
    }
}

fn json_expr(value: &serde_json::Value) -> Expr {
    match value {
        serde_json::Value::Null => Expr::Null,
        serde_json::Value::Bool(value) => Expr::Bool(*value),
        serde_json::Value::Number(value) => Expr::Number(value.as_f64().unwrap_or(f64::NAN)),
        serde_json::Value::String(value) => Expr::String(value.as_str().into()),
        serde_json::Value::Array(items) => Expr::List(items.iter().map(json_expr).collect()),
        serde_json::Value::Object(fields) => Expr::Record(
            fields
                .iter()
                .map(|(key, value)| (key.as_str().into(), json_expr(value)))
                .collect(),
        ),
    }
}

fn is_name(text: &str) -> bool {
    text.chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && text
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn schema_notation(schema: &serde_json::Value) -> &'static str {
    match schema.get("type").and_then(serde_json::Value::as_str) {
        Some("string") => NOTATION.str,
        Some("integer") => NOTATION.int,
        Some("number") => NOTATION.float,
        Some("boolean") => NOTATION.bool,
        Some("object") => NOTATION.record,
        Some("null") => NOTATION.null,
        _ => NOTATION.any,
    }
}

fn refusal(source: &str, line: &str) -> DialectDiagnostic {
    let start = source.find(line).unwrap_or(0);
    DialectDiagnostic {
        kind: lash_core::CellFailureKind::Program,
        message: format!("not a seam-proof statement: `{line}`"),
        span: Some(Span {
            start,
            end: start + line.len(),
        }),
        rendered: format!("not a seam-proof statement: `{line}`"),
    }
}
