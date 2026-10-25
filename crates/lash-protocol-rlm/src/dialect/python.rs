use lash_tool_presenter_python::render_shape;
use lash_vocabulary::tool_shape::SchemaShape;

use super::{
    CellTags, DialectPromptVocabulary, DialectPrompts, ExecutionSection, ExecutionSectionRequest,
};

/// The name `lash-dialect-python` is installed under.
pub(crate) const LANGUAGE_ID: &str = "python";

/// Python's prompt adapter: it spells Python prompts, value types and tool
/// names. Its types are the Python tool presenter's spelling.
#[derive(Clone, Copy, Debug, Default)]
pub struct PythonPrompts;

const PYTHON_CELL_TAGS: CellTags = CellTags {
    open: "<python>",
    close: "</python>",
};

impl DialectPrompts for PythonPrompts {
    fn spelling(&self) -> lash_kernel_dialect::Spelling {
        lash_kernel_dialect::Spelling::Python
    }

    fn schema_type(
        &self,
        shape: &SchemaShape,
    ) -> Result<String, lash_tool_presenter_api::PresentationRefusal> {
        render_shape(shape)
    }

    fn schema_definition(
        &self,
        name: &str,
        shape: &SchemaShape,
    ) -> Result<String, lash_tool_presenter_api::PresentationRefusal> {
        lash_tool_presenter_python::schema_definition(name, shape)
    }

    fn prompt_vocabulary(&self) -> DialectPromptVocabulary {
        PYTHON_PROMPT_VOCABULARY
    }

    fn render_execution_section(&self, request: ExecutionSectionRequest<'_>) -> ExecutionSection {
        let ExecutionSectionRequest {
            channel,
            tools,
            catalogue,
            ..
        } = request;
        let label = catalogue.map(|catalogue| {
            super::catalogue_label(
                catalogue,
                &format!(
                    "await {}({{ ... }})",
                    super::spelled_effect(
                        lash_kernel_dialect::Spelling::Python,
                        &catalogue.discovery_operation
                    )
                ),
            )
        });
        let tools = [(!tools.is_empty()).then_some(tools), label.as_deref()]
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
                    "`{\"code\": \"<complete program>\"}`. Tool calls, `control.finish` included, run inside ",
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
                r#"Use prose for conversation; use {action} for action or computation. Call tools by their dotted path, as `await package.tool_name({{ ... }})`, with one dict of arguments, only those listed under **Tools**.

{response_shape}
{example}

Lexical and top-level bindings may shadow built-in names; top-level shadows persist across executions. Only an offered control capability ends the turn, never a user binding with the same name. Top-level variables persist across executions as data. A function bound to a top-level name persists as a self-contained copy: its captures are frozen when its cell ends, so later changes to those variables are not seen and changes the function makes to them are not kept. Functions capturing tasks are not carried; functions inside ordinary lists and dict values are carried with their captures. Keep a task's result, not the task. An async function, in this cell or saved from an earlier one, can start a durable process with `await processes.start({{"definition": saved_fn, "args": {{"parameter": value}}}})` when the process tools are offered. Passing a function to a tool slot typed as a function (such as `processes.start`'s `definition`) copies its code and current captures inline; later mutations are invisible to the copy. A function in a slot typed as data raises `not_data`. Defining a function publishes nothing. A published workflow starts with `definition_id` instead. The returned handle carries `definition_id`; list with `await processes.list({{"definition": saved_fn}})` or `await processes.list({{"definition_id": handle["definition_id"]}})`. Functions have no definition id property. Published workflow entries may use static module helpers and constants. Omitted entry arguments select defaults in definition order; explicit None stays a value. Entry annotations admit finite text Literal alternatives and type aliases.

This is a Python subset, not CPython. Advertised packages are available by their paths; `asyncio`, the installed pure `math` and `json` modules, and erased typing scaffolding are supported. Bytes, exact integer bitwise operators, pure value methods and computed format specifications work. There is no general import loader. Ordinary function decorators, assignment expressions, list slice assignment/deletion, and value-based `match` patterns work. Context managers use explicit guest member tables or declared package methods. Classes, generators, class patterns and `except*` remain unsupported. File, network and process access requires the tools.

Calls support `*args`, `**kwargs`, positional-only and keyword-only parameters, including calls through aliases. Safe builtins and exception classes are values. `iter`, `next`, lazy `map` and `filter` keep guest state across waits. `globals()`, `locals()` and `vars()` return copies of guest bindings; `vars(dict)` returns the live guest dictionary. `dir()` lists sorted guest names. None of these expose host state. `asyncio.gather(..., return_exceptions=True)` returns errors in input order alongside successful results.

### Type reference

Tool declarations describe values and calls. `TypedDict`, aliases and the typing names shown there do not install classes or imports in the cell runtime.

### Host API

`print(value)` shows output in the next step. `await control.finish(value)` ends the turn with `value`: nothing after it runs, so await it directly, as the last thing the program does, after every other task has been awaited. A failed tool call raises an exception whose message says why. The top level is already asynchronous: `await` works there. Run calls concurrently with `await asyncio.gather(a(), b())`. Every task the program starts must be awaited before the program ends; a program that ends with a task still running, or failed with an exception nothing awaited, fails with `CELL_TASKS_OUTSTANDING` naming the code that is still running."#
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

    print_call: "print",
    print_statement_prefix: "print(",
    print_statement_suffix: ")",
    finish_call: "await control.finish(value)",
    continue_as_call: "control.continue_as(...)",
    not_carried_repair: "Await the task and keep its result, or define the function using captures that are data or other saved functions.",
    unjoined_task_repair: "Await every task before the cell ends: `await` it, or collect them with `await asyncio.gather(...)`. A cell does not leave work running behind it.",
    field_miss_rule: "Never write a key you haven't seen in the key sets below: reading a key that is not there raises `KeyError`. If a name is not listed, it does not exist on that value.",
};
