use super::{
    CellTags, DialectPromptVocabulary, DialectPrompts, ExecutionSection, ExecutionSectionRequest,
};

/// The name `lash-dialect-typescript` is installed under.
pub(crate) const LANGUAGE_ID: &str = "typescript";

/// TypeScript's prompt adapter: it spells TypeScript prompts, value types
/// and tool paths. Its types are the TypeScript tool presenter's spelling.
#[derive(Clone, Copy, Debug, Default)]
pub struct TypescriptPrompts;

impl DialectPrompts for TypescriptPrompts {
    fn spelling(&self) -> lash_kernel_dialect::Spelling {
        lash_kernel_dialect::Spelling::TypeScript
    }

    fn schema_type(
        &self,
        shape: &lash_vocabulary::tool_shape::SchemaShape,
    ) -> Result<String, lash_tool_presenter_api::PresentationRefusal> {
        lash_tool_presenter_typescript::render_schema_shape(shape).map_err(Into::into)
    }

    fn schema_definition(
        &self,
        name: &str,
        shape: &lash_vocabulary::tool_shape::SchemaShape,
    ) -> Result<String, lash_tool_presenter_api::PresentationRefusal> {
        lash_tool_presenter_typescript::schema_definition(name, shape).map_err(Into::into)
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

const TYPESCRIPT_PROMPT_VOCABULARY: DialectPromptVocabulary = DialectPromptVocabulary {
    language_name: "TypeScript",
    execution_title: "TypeScript execution",
    cell_tags: TYPESCRIPT_CELL_TAGS,
    cell_noun: "cell",

    history_item_name: "HistoryItem",
    print_call: "console.log",
    print_statement_prefix: "console.log(",
    print_statement_suffix: ")",
    finish_call: "await control.finish(value)",
    continue_as_call: "control.continueAs(...)",
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
    tool_catalog
        .entries()
        .iter()
        .any(|tool| tool.manifest.operation.package.as_slice()[0].as_str() == "processes")
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
    r#"Async functions are ordinary cell code: they may capture cell bindings and be called locally. Passing a function to a tool slot typed as a function (such as `processes.start`'s `definition`) copies its code and current captures inline. A later mutation is invisible to that copy; tasks, handles and cyclic captures cannot cross. A function in a slot typed as data throws `not_data`. Workflow entries may use static module helpers and constants, optional defaults, and exact finite boundary annotations (tuples, structural intersections, generic aliases and admitted package types). Omission selects a default; explicit null stays a value.
Start one with `await processes.start({ definition: review, args: { request: "x" } })`, or pass `definition_id` for a published workflow. Start arguments key by parameter name. Returning succeeds; throwing fails. Defining a function publishes nothing.
The returned handle includes `definition_id`. List runs with `processes.list({ definition: review })` or `processes.list({ definition_id: handle.definition_id })`; the function itself has no definition id property.
A started handle outlives the turn; Stop cancels only the awaited handle; cancel is a request the child sees at its next step or wake."#
        .to_string()
}

fn render_execution_section(request: ExecutionSectionRequest<'_>) -> ExecutionSection {
    let ExecutionSectionRequest {
        channel,
        tools,
        tool_catalog,
        catalogue,
    } = request;
    // A host with a discovery tool labels the listing where the model reads
    // which tools it may call: under **Tools**, which renders whether or not
    // the session keeps the built-in prose.
    let label = catalogue.map(|catalogue| {
        super::catalogue_label(
            catalogue,
            &format!(
                "await {}({{ ... }})",
                super::spelled_effect(
                    lash_kernel_dialect::Spelling::TypeScript,
                    &catalogue.discovery_operation
                )
            ),
        )
    });
    let tools = [(!tools.is_empty()).then_some(tools), label.as_deref()]
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
            "`{\"code\": \"<complete program>\"}`. Tool calls, `control.finish` included, run inside ",
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
        r#"Built-in names cannot be reused by top-level bindings. Top-level bindings persist across executions as data. A function bound to a top-level name persists too, as a copy: what it reads from outside itself is frozen when its cell ends, so a later change to a top-level variable is not seen by it, and a change it makes to one is not kept. A pending promise does not outlive the cell that created it, nor does a function that holds one: a later cell that uses such a binding fails with `SESSION_BINDING_NOT_CARRIED`, so keep a promise's awaited result, not the promise. Return exactly the value and type the task asks for with `await control.finish(value)`; do not finish with an unexamined whole tool result. Putting an object into a string — with `+`, `` `${{...}}` `` or `String(...)` — gives the placeholder `[object Object]`, never its contents; read the value with `console.log(value)` or serialize it with `JSON.stringify(value)`.

`Math`, `Date` (UTC), `String`, `Array`, `Object`, `JSON`, `Map`/`Set`, `RegExp` and `URL` are available; RegExp supports `d` capture indices in UTF-16 units, and Set algebra accepts set-like objects with `size`, `has` and `keys`; this is not Node or a browser, and classes, generators and `new Promise(...)` are not supported.
Date local-named readers and date/time strings use UTC, with zero timezone offset. Date parsing accepts ISO and its own whole-second toString/toUTCString formats. Locale date/time formatting uses fixed en-US and UTC; locale case methods use pinned root/en-US Unicode mappings. Omit the locale or use "en-US", with no formatting options; other locales and localeCompare are refused. Dates are immutable; construct a new Date instead of using setters.

Type annotations are trusted and enforced where they are used: arithmetic, comparison, `!`, a condition, a template or an index on a value declared `number`, `string`, `boolean` or an array raises `type_error` when the value is not that type, and an index outside a declared array raises `index_out_of_range`, where JavaScript would convert or give `undefined`. Annotate only what is true, or leave the annotation off to keep JavaScript's conversions.

### Concurrency

An `async` function starts running when it is called, and async callbacks run concurrently: `items.map(async (item) => ...)` starts every call at once, and `await Promise.all(...)` collects them. Await every promise before the cell ends. A cell that ends while work it started is still running, or after a promise rejected with nothing awaiting it, fails with `CELL_TASKS_OUTSTANDING` and names the async code that is still running; `Promise.race([])` rejects instead of waiting forever.

### Host API

`console.log(value)` shows output in the next step. `await control.finish(value)` ends the turn with `value`: nothing after it runs, so await it directly, as the last thing the program does, after every other promise has been awaited. A failed tool call throws; wrap it in `try`/`catch` to carry on.{sleep}{durable}"#
    );
    // One worked program, rendered in each channel's own call shape.
    let example_program = "const total = 1 + 2;\nawait control.finish(total);";
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
