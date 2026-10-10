use super::typescript_types as types;
use super::{
    CellTags, DialectPromptVocabulary, DialectPrompts, DialectRefusal, DialectRefusalKind,
    ExecutionSection, ExecutionSectionRequest,
};

/// The name `lash-dialect-typescript` is installed under.
pub(crate) const LANGUAGE_ID: &str = "typescript";

/// TypeScript's prompt adapter: it spells TypeScript prompts, types and
/// tool paths.
#[derive(Clone, Copy, Debug, Default)]
pub struct TypescriptPrompts;

impl DialectPrompts for TypescriptPrompts {
    /// Being a catalog member is being advertised, and the execution section
    /// advertises the binding's call path as a typed declaration the model
    /// calls verbatim. A path TypeScript resolves to anything but a tool call
    /// — a module segment no cell can write, an ECMA global namespace, a
    /// refused method name — can only be advertised as a callable nothing, so
    /// it is refused (FIG-1444).
    fn tool_call_path(
        &self,
        binding: &lash_vm_runtime::ResolvedToolBinding,
    ) -> Result<String, DialectRefusal> {
        let call_path = binding.call_path();
        types::ensure_tool_call_path_addressable(&call_path).map_err(|error| DialectRefusal {
            kind: DialectRefusalKind::UnaddressableToolPath,
            message: format!("no TypeScript cell can call `{call_path}` as a tool: {error}"),
        })?;
        Ok(call_path)
    }

    fn tool_signature(
        &self,
        call_path: &str,
        input: &lash_sansio::SchemaShape,
        output: &lash_sansio::SchemaShape,
    ) -> String {
        let input = types::render_schema_shape(input);
        let input = if input == "Record<string, never>" {
            "{}"
        } else {
            &input
        };
        let output = types::render_schema_shape(output);
        format!("{call_path}({input}): Promise<{output}>")
    }

    fn schema_type(&self, shape: &lash_sansio::SchemaShape) -> String {
        types::render_schema_shape(shape)
    }

    fn schema_definition(&self, name: &str, shape: &lash_sansio::SchemaShape) -> String {
        format!("type {name} = {}", self.schema_type(shape))
    }

    fn render_tool_example(&self, authored: &str) -> Option<String> {
        Some(render_tool_example(authored))
    }

    fn prompt_vocabulary(&self) -> DialectPromptVocabulary {
        TYPESCRIPT_PROMPT_VOCABULARY
    }

    fn render_execution_section(&self, request: ExecutionSectionRequest<'_>) -> ExecutionSection {
        render_execution_section(request)
    }
}

const TYPESCRIPT_CELL_TAGS: CellTags = CellTags {
    open: "<typescript>",
    close: "</typescript>",
};

fn is_plain_identifier(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
        && !text.starts_with(|character: char| character.is_ascii_digit())
}

const TYPESCRIPT_PROMPT_VOCABULARY: DialectPromptVocabulary = DialectPromptVocabulary {
    language_name: "TypeScript",
    execution_title: "TypeScript execution",
    cell_tags: TYPESCRIPT_CELL_TAGS,
    cell_noun: "cell",
    history_type: "HistoryItem[]",
    history_item_name: "HistoryItem",
    print_call: "console.log",
    print_statement_prefix: "console.log(",
    print_statement_suffix: ")",
    finish_name: lash_kernel_dialect::FINISH_NAME,
    finish_statement: "finish(value)",
    finish_null_statement: "finish(null)",
    continue_as_call: "control.continue_as(...)",
    continue_as_example: "await control.continue_as({ task: \"continue the audit from the summarized findings\", seed: { problem: input.prompt, findings: findings } });",
    // A wrong field name is the one mistake this runtime does not report.
    // Reading a key that was never there yields `undefined`, which flows into
    // arithmetic as `NaN` and into totals as nothing at all: the cell
    // succeeds, the observation looks plausible, and the number is wrong.
    // Every key of every value is written out — in the row itself where the
    // record is small enough, in the `Schema:` block otherwise — so there is
    // never a reason to write one from memory.
    not_carried_repair: "Bind the function to a top-level name of its own so it is kept, or define it again in this cell; await a task and keep its result.",
    unjoined_task_repair: "Await every promise before the cell ends: `await` it, or collect them with `await Promise.all([...])`. A cell does not leave work running behind it.",
    field_miss_rule: "Never write a field name you haven't seen in the key sets below — guessed field names silently produce zeros rather than errors. If a name is not listed, it does not exist on that value.",
};

/// The process operations are leaf tools now (FIG-2999): nothing in the
/// dialect gates them, so their availability is read off the catalogue the
/// host actually rendered rather than off an ability flag.
pub(crate) fn catalogue_has_process_surface(tool_catalog: &lash_core::ToolCatalog) -> bool {
    tool_catalog.tools.iter().any(|tool| {
        lash_vm_runtime::required_tool_executable(&tool.manifest)
            .is_ok_and(|binding| binding.call_path().starts_with("processes."))
    })
}

/// The authoring rules the process tool signatures cannot state themselves.
///
/// `processes.*` renders from the catalogue like any other tool, so this block
/// carries only what a signature cannot: that a process is a literal value,
/// and that its captures are copied by value.
pub(crate) fn typescript_process_prompt(process_surface: bool) -> String {
    if !process_surface {
        return String::new();
    }
    r#"A process is an `async` arrow the cell never calls: `const review = async (request: string) => { ... };`, or one written inline in a process tool's argument. Its name is the `const` it is bound to, and start arguments key by the arrow's parameter names. Returning from it succeeds; throwing fails.
A process runs apart from the cell: its body sees its parameters and nothing of the surrounding cell, and a body that names one of the cell's bindings is refused as a non-liftable capture. Pass such a value through the start's `args`.
A started handle outlives the turn; Stop cancels only the awaited handle; cancel is a request the child sees at its next step or wake."#
        .to_string()
}

/// Rewrites an authored Lash VM example into this dialect.
///
/// Deliberately a small, total rewriter over the shapes the authored corpus
/// actually uses rather than a translator: every example is a sequence of
/// statement lines that are either an awaited call, an assignment, or a
/// `finish`. Anything it does not recognize still loses the try-operator
/// and gains a terminator, which is the difference between "reads like
/// TypeScript" and "is a syntax error".
///
/// It rewrites line by line, so an example whose *string literal* spans a
/// real newline would have a terminator inserted inside the literal. No
/// authored example does that (they escape it as `\n`), and the walker
/// parses every rendered example, so the day one does the check fails
/// rather than the model reading a syntax error.
fn render_tool_example(example: &str) -> String {
    example
        .lines()
        .map(|line| {
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                return String::new();
            }
            let indent_len = trimmed.len() - trimmed.trim_start().len();
            let (indent, body) = trimmed.split_at(indent_len);
            // `expr?` — the Lash VM try-operator. TypeScript propagates a
            // rejection from `await` itself, so the operator has no twin.
            let body = body.strip_suffix('?').unwrap_or(body);
            let body = match body.strip_prefix("finish ") {
                Some(value) => format!("finish({value})"),
                None => match body.split_once(" = ") {
                    Some((name, value)) if is_plain_identifier(name) => {
                        format!("const {name} = {value}")
                    }
                    _ => body.to_string(),
                },
            };
            let body = if body.ends_with(';') || body.ends_with('{') || body.ends_with(',') {
                body
            } else {
                format!("{body};")
            };
            format!("{indent}{body}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_execution_section(request: ExecutionSectionRequest<'_>) -> ExecutionSection {
    let finish_name = lash_kernel_dialect::FINISH_NAME;
    let ExecutionSectionRequest {
        channel,
        tools,
        tool_catalog,
        discovery_operation,
    } = request;
    // A host with a discovery tool says so where the model reads which
    // tools it may call: under **Tools**, which renders whether or not the
    // session keeps the built-in prose.
    let discovery = discovery_operation.map(|operation| {
        format!("Other tools exist; find them with `await {operation}({{ ... }})`.")
    });
    let tools = [discovery.as_deref(), (!tools.is_empty()).then_some(tools)]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    let tools = if tools.is_empty() {
        String::new()
    } else {
        format!("### Tools\n\n{}", tools.join("\n\n"))
    };
    let allowed_sections = "**Tools**";
    // Transport prose is authored per channel, side by side, rather than
    // derived from the cell wording by string replacement (FIG-2881).
    let action = match channel {
        crate::plugin::RlmChannel::Cell => {
            format!("a paired `{}` block", TYPESCRIPT_CELL_TAGS.open)
        }
        crate::plugin::RlmChannel::NativeTool => "the `execute_code` program".to_string(),
    };
    let response_shape = match channel {
        crate::plugin::RlmChannel::Cell => super::cell_response_shape(TYPESCRIPT_CELL_TAGS),
        crate::plugin::RlmChannel::NativeTool => concat!(
            "### Tool transport\n\nEach response makes one `execute_code` call with ",
            "`{\"code\": \"<complete program>\"}`. Tool calls and `finish` run inside ",
            "the program; prose before the call is commentary.\n"
        )
        .to_string(),
    };
    let durable = typescript_process_prompt(catalogue_has_process_surface(tool_catalog));
    let durable = if durable.is_empty() {
        durable
    } else {
        format!("\n\n### Processes\n\n{durable}")
    };
    let sleep = "\n\n`await sleep(ms)` pauses the program. For a timeout, race a call against a timer — `await Promise.race([call, sleep(ms)])` is `undefined` when the timer wins, and the losing call is cancelled.";
    let host_api = format!(
        r#"Built-in names, including `{finish_name}`, cannot be reused by top-level bindings. Top-level bindings persist across executions as data. A function bound to a top-level name persists too, as a copy: what it reads from outside itself is frozen when its cell ends, so a later change to a top-level variable is not seen by it, and a change it makes to one is not kept. A pending promise does not outlive the cell that created it, nor does a function that holds one: a later cell that uses such a binding fails with `SESSION_BINDING_NOT_CARRIED`, so keep a promise's awaited result, not the promise. Return exactly the value and type the task asks for with `finish(value)`; do not finish an unexamined whole tool result. Putting an object into a string — with `+`, `` `${{...}}` `` or `String(...)` — gives the placeholder `[object Object]`, never its contents; read the value with `console.log(value)` or serialize it with `JSON.stringify(value)`.

`Math`, `Date` (UTC), `String`, `Array`, `Object`, `JSON`, `Map`/`Set`, `RegExp` and `URL` are available; this is not Node or a browser, and classes, generators and `new Promise(...)` are not supported.

Type annotations are trusted and enforced where they are used: arithmetic, comparison, `!`, a condition, a template or an index on a value declared `number`, `string`, `boolean` or an array raises `type_error` when the value is not that type, and an index outside a declared array raises `index_out_of_range`, where JavaScript would convert or give `undefined`. Annotate only what is true, or leave the annotation off to keep JavaScript's conversions.

### Concurrency

An `async` function starts running when it is called, and async callbacks run concurrently: `items.map(async (item) => ...)` starts every call at once, and `await Promise.all(...)` collects them. Await every promise before the cell ends. A cell that ends while work it started is still running, or after a promise rejected with nothing awaiting it, fails with `CELL_TASKS_OUTSTANDING` and names the async code that is still running; `Promise.race([])` rejects instead of waiting forever.

### Host API

`console.log(value)` shows output in the next step; `finish(value)` ends the turn. A failed tool call throws; wrap it in `try`/`catch` to carry on.{sleep}{durable}"#
    );
    // One worked program, rendered in each channel's own call shape.
    let example_program = "const total = 1 + 2;\nfinish(total);";
    let example = match channel {
        crate::plugin::RlmChannel::Cell => format!(
            "### Example cell\n\n{open}\n{example_program}\n{close}",
            open = TYPESCRIPT_CELL_TAGS.open,
            close = TYPESCRIPT_CELL_TAGS.close,
        ),
        crate::plugin::RlmChannel::NativeTool => format!(
            "### Example execute_code call\n\nexecute_code({})",
            serde_json::json!({"code": example_program})
        ),
    };
    let declarations = tools.trim().to_string();
    ExecutionSection {
        prose: format!(
            "Use prose for conversation; use {action} for action or computation. Call tools as `await module.operation({{ ... }})`, only those listed under {allowed_sections}.\n\n{response_shape}\n{example}\n\n{host_api}"
        ),
        declarations,
    }
}

#[cfg(test)]
mod tests {
    use crate::dialect::{RlmDialectServices, SessionDialect};
    use lash_vm_runtime::{ToolBinding, ToolDefinitionBindingExt};

    #[test]
    fn the_process_section_follows_the_catalogue_and_teaches_the_argument_convention() {
        let dialect = SessionDialect::new(
            crate::dialect::CellDialect::typescript(),
            RlmDialectServices {
                kernel: crate::executor::KernelCarry::default(),
                presentation: crate::RlmPresentationConfig::standard(),
                workers: lash_vm_client::service::Service::default(),
                deferred_tool_resolver: None,
                execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
                code_renderer: Default::default(),
                channel: crate::plugin::RlmChannel::Cell,
            },
        );
        let render = |catalog: &lash_core::ToolCatalog| {
            dialect
                .render_execution_section(
                    crate::protocol::RlmPromptFeatures::default(),
                    catalog,
                    crate::plugin::RlmChannel::Cell,
                    None,
                )
                .expect("render execution section")
        };

        let without = render(&lash_core::ToolCatalog::from_tool_definitions(Vec::new()));
        assert!(!without.contains("### Processes"), "{without}");

        let start = lash_core::ToolDefinition::raw(
            "tool:process-controls/start",
            "processes_start",
            "Start a process",
            serde_json::json!({
                "type": "object",
                "properties": { "definition": { "type": "object" } },
                "required": ["definition"],
                "additionalProperties": false
            }),
            serde_json::json!({
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"],
                "additionalProperties": false
            }),
        )
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120))
        .with_tool_binding(ToolBinding::new(["processes"], "start"));
        let with = render(&lash_core::ToolCatalog::from_tool_definitions(vec![start]));
        assert!(with.contains("### Processes"), "{with}");
        // The handle shape is the tool's own return contract now, not prose.
        assert!(with.contains("processes.start"), "{with}");
        assert!(with.contains("id: string"), "{with}");
        assert!(
            with.contains("parameter names"),
            "the keys are the run arrow's parameter names: {with}"
        );
        assert!(
            with.contains("non-liftable capture"),
            "what a process body may name is prompt-only knowledge: {with}"
        );
        // None of the deleted special forms may come back as prose.
        for retired in [
            "defineProcess",
            "registerTrigger",
            "wake(",
            "start(p:Process",
        ] {
            assert!(!with.contains(retired), "`{retired}` survived: {with}");
        }
    }

    /// Being a catalog member is being advertised under a path a cell
    /// writes verbatim (FIG-1444): a path with no module, with a word no
    /// cell can write in expression position, or under a namespace the
    /// language resolves itself is refused at registration.
    #[test]
    fn a_tool_path_no_cell_can_write_is_refused() {
        use super::types::{ensure_tool_call_path_addressable, reserved_words};
        ensure_tool_call_path_addressable("web.fetch").expect("an ordinary path is addressable");
        ensure_tool_call_path_addressable("a.b.c").expect("a nested module path is addressable");
        // A member name may be a reserved word; only the receiver is written
        // as an identifier.
        ensure_tool_call_path_addressable("processes.await")
            .expect("a reserved word is a member name a cell can write");
        assert!(ensure_tool_call_path_addressable("fetch").is_err());
        assert!(ensure_tool_call_path_addressable("Math.floor").is_err());
        assert!(ensure_tool_call_path_addressable("web.has-dash").is_err());
        for word in reserved_words() {
            assert!(
                ensure_tool_call_path_addressable(&format!("{word}.run")).is_err(),
                "`{word}` cannot root a tool path"
            );
        }
    }
}
