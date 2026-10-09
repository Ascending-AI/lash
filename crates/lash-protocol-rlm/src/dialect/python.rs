use lash_sansio::{ExtraKeys, ObjectShape, SchemaShape, ShapeKind};

use super::{
    CellTags, DialectPromptVocabulary, DialectPrompts, DialectRefusal, DialectRefusalKind,
    ExecutionSection, ExecutionSectionRequest,
};

/// The name `lash-dialect-python` is installed under.
pub(crate) const LANGUAGE_ID: &str = "python";

/// Python's prompt adapter: it spells Python prompts, types and tool names.
#[derive(Clone, Copy, Debug, Default)]
pub struct PythonPrompts;

const PYTHON_CELL_TAGS: CellTags = CellTags {
    open: "<python>",
    close: "</python>",
};

const PYTHON_KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_alphabetic())
        && chars.all(|character| character == '_' || character.is_alphanumeric())
        && !PYTHON_KEYWORDS.contains(&text)
}

impl DialectPrompts for PythonPrompts {
    /// A Python cell calls a tool by one name: the binding's module path
    /// and operation joined with `_` (`web.fetch` is `web_fetch`). A name
    /// that is no Python identifier is refused.
    fn tool_call_path(
        &self,
        binding: &lash_vm_runtime::ResolvedToolBinding,
    ) -> Result<String, DialectRefusal> {
        let name = binding.call_path().replace('.', "_");
        if !is_identifier(&name) {
            return Err(DialectRefusal {
                kind: DialectRefusalKind::UnaddressableToolPath,
                message: format!(
                    "no Python cell can call `{}` as a tool: `{name}` is not a name a cell can write",
                    binding.call_path()
                ),
            });
        }
        Ok(name)
    }

    fn tool_signature(&self, call_path: &str, input: &SchemaShape, output: &SchemaShape) -> String {
        format!(
            "async def {call_path}(input: {}) -> {}",
            render_shape(input),
            render_shape(output)
        )
    }

    fn schema_type(&self, shape: &SchemaShape) -> String {
        render_shape(shape)
    }

    fn schema_definition(&self, name: &str, shape: &SchemaShape) -> String {
        format!("{name} = {}", render_shape(shape))
    }

    /// Authored examples are written in the call shape of dotted paths,
    /// which no Python cell writes: they are left out of the prompt.
    fn render_tool_example(&self, _authored: &str) -> Option<String> {
        None
    }

    fn prompt_vocabulary(&self) -> DialectPromptVocabulary {
        PYTHON_PROMPT_VOCABULARY
    }

    fn render_execution_section(&self, request: ExecutionSectionRequest<'_>) -> ExecutionSection {
        let finish_name = lash_kernel_dialect::FINISH_NAME;
        let ExecutionSectionRequest {
            channel,
            tools,
            discovery_operation,
            ..
        } = request;
        let discovery = discovery_operation.map(|operation| {
            format!(
                "Other tools exist; find them with `await {}({{ ... }})`.",
                operation.replace('.', "_")
            )
        });
        let tools = [discovery.as_deref(), (!tools.is_empty()).then_some(tools)]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        let declarations = if tools.is_empty() {
            String::new()
        } else {
            format!("### Tools\n\n{}", tools.join("\n\n"))
        };
        let example_program = "total = 1 + 2\nprint(total)";
        let (action, response_shape, example) = match channel {
            crate::plugin::RlmChannel::Cell => (
                format!("a paired `{}` block", PYTHON_CELL_TAGS.open),
                super::cell_response_shape(PYTHON_CELL_TAGS),
                format!(
                    "### Example cell\n\n{}\n{example_program}\n{}",
                    PYTHON_CELL_TAGS.open, PYTHON_CELL_TAGS.close
                ),
            ),
            crate::plugin::RlmChannel::NativeTool => (
                "the `execute_code` program".to_string(),
                concat!(
                    "### Tool transport\n\nEach response makes one `execute_code` call with ",
                    "`{\"code\": \"<complete program>\"}`. Tool calls and `finish` run inside ",
                    "the program.\n"
                )
                .to_string(),
                format!(
                    "### Example execute_code call\n\nexecute_code({})",
                    serde_json::json!({"code": example_program})
                ),
            ),
        };
        ExecutionSection {
            prose: format!(
                r#"Use prose for conversation; use {action} for action or computation. Call tools as `await tool_name({{ ... }})` with one dict of arguments, only those listed under **Tools**.

{response_shape}
{example}

Built-in names, including `{finish_name}`, cannot be reused by top-level bindings. Top-level variables persist across executions as data. A function, a class or a task does not outlive the program that created it: define a function again where it is used, and keep a task's result, not the task.

This is a Python subset, not CPython: there are no imports beyond `asyncio`, and no file, network or process access except through the tools.

### Host API

`print(value)` shows output in the next step. A failed tool call raises an exception whose message says why. The top level is already asynchronous: `await` works there. Run calls concurrently with `await asyncio.gather(a(), b())`. Every task the program starts must be awaited before the program ends; a program that ends with a task still running, or failed with an exception nothing awaited, fails with `CELL_TASKS_OUTSTANDING` naming the code that is still running."#
            ),
            declarations,
        }
    }
}

const PYTHON_PROMPT_VOCABULARY: DialectPromptVocabulary = DialectPromptVocabulary {
    language_name: "Python",
    execution_title: "Python execution",
    cell_tags: PYTHON_CELL_TAGS,
    cell_noun: "cell",
    history_type: "list[HistoryItem]",
    history_item_name: "HistoryItem",
    print_call: "print",
    print_statement_prefix: "print(",
    print_statement_suffix: ")",
    finish_name: lash_kernel_dialect::FINISH_NAME,
    finish_statement: "finish(value)",
    finish_null_statement: "finish(None)",
    continue_as_call: "control_continue_as(...)",
    continue_as_example: "await control_continue_as({\"task\": \"continue the audit from the summarized findings\", \"seed\": {\"problem\": input[\"prompt\"], \"findings\": findings}})",
    not_carried_repair: "Define the function again in this cell, or await the task and keep its result; keep data, not functions or pending work, in session variables.",
    unjoined_task_repair: "Await every task before the cell ends: `await` it, or collect them with `await asyncio.gather(...)`. A cell does not leave work running behind it.",
    field_miss_rule: "Never write a key you haven't seen in the key sets below: reading a key that is not there raises `KeyError`. If a name is not listed, it does not exist on that value.",
};

/// Spells a schema shape as a Python type.
fn render_shape(shape: &SchemaShape) -> String {
    match &shape.kind {
        ShapeKind::Unknown => "Any".to_string(),
        ShapeKind::Null => "None".to_string(),
        ShapeKind::Bool => "bool".to_string(),
        ShapeKind::Int => "int".to_string(),
        ShapeKind::Float => "float".to_string(),
        ShapeKind::Str => "str".to_string(),
        ShapeKind::Literals(values) => format!(
            "Literal[{}]",
            values
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        ShapeKind::List(item) => format!("list[{}]", render_shape(item)),
        ShapeKind::Tuple(items) => format!(
            "tuple[{}]",
            items
                .iter()
                .map(render_shape)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        ShapeKind::Object(object) => render_object(object),
        ShapeKind::Union(members) => {
            let mut spelled = Vec::<String>::new();
            for member in members.iter().map(render_shape) {
                if !spelled.contains(&member) {
                    spelled.push(member);
                }
            }
            spelled.join(" | ")
        }
        ShapeKind::Named(name) => name.replace('.', "_"),
        // `ShapeKind` is non-exhaustive: a kind this dialect has no spelling
        // for is shown as the widest type rather than guessed at.
        _ => "Any".to_string(),
    }
}

/// A record as the dict it is in a cell: its keys with their types, a key
/// that may be absent marked `?`.
fn render_object(object: &ObjectShape) -> String {
    let extra = match &object.extra_keys {
        ExtraKeys::Closed => None,
        ExtraKeys::Unstated if object.fields.is_empty() => Some("Any".to_string()),
        ExtraKeys::Unstated => None,
        ExtraKeys::Open(extra) => Some(render_shape(extra)),
        _ => Some("Any".to_string()),
    };
    if object.fields.is_empty() {
        return format!(
            "dict[str, {}]",
            extra.unwrap_or_else(|| "Never".to_string())
        );
    }
    let mut members = object
        .fields
        .iter()
        .map(|field| {
            format!(
                "{}{}: {}",
                serde_json::Value::String(field.name.clone()),
                if field.required { "" } else { "?" },
                render_shape(&field.shape)
            )
        })
        .collect::<Vec<_>>();
    if let Some(extra) = extra {
        members.push(format!("str: {extra}"));
    }
    format!("{{{}}}", members.join(", "))
}
