use super::{
    CellTags, Dialect, DialectPromptVocabulary, DialectRefusal,
    DialectRefusalKind, ExecutionSectionRequest, ShapeNotation,
};

pub(crate) const LANGUAGE_ID: &str = "typescript";

/// The TypeScript host adapter selects the shipped worker frontend and
/// spells TypeScript prompts and tool paths. A host selects it by naming it
/// (`Arc::new(TypescriptDialect)`) where it constructs the RLM protocol.
#[derive(Clone, Copy, Debug, Default)]
pub struct TypescriptDialect;

impl Dialect for TypescriptDialect {
    fn language_id(&self) -> &'static str {
        LANGUAGE_ID
    }

    fn worker_service(&self) -> lash_vm_client::service::Service {
        lash_vm_client::service::Service::default()
    }

    /// Being a catalog member is being advertised, and the execution section
    /// advertises the binding's call path as a typed declaration the model
    /// calls verbatim. A path TypeScript resolves to anything but a tool call
    /// — a module segment no cell can write, an ECMA global namespace, a
    /// refused method name — can only be advertised as a callable nothing, so
    /// it is refused (FIG-1444).
    fn tool_call_path(
        &self,
        binding: &lash_lashlang_runtime::ResolvedToolBinding,
    ) -> Result<String, DialectRefusal> {
        let call_path = binding.call_path();
        lash_typescript::ensure_tool_call_path_addressable(&call_path).map_err(|error| {
            DialectRefusal {
                kind: DialectRefusalKind::UnaddressableToolPath,
                message: format!("no TypeScript cell can call `{call_path}` as a tool: {error}"),
            }
        })?;
        Ok(call_path)
    }

    fn tool_signature(
        &self,
        call_path: &str,
        input_schema: &serde_json::Value,
        output_schema: &serde_json::Value,
    ) -> String {
        let input = lash_typescript::render_schema_type(input_schema);
        let input = if input == "Record<string, never>" {
            "{}"
        } else {
            &input
        };
        let output = lash_typescript::render_schema_type(output_schema);
        format!("{call_path}({input}): Promise<{output}>")
    }

    fn render_tool_example(&self, authored: &str) -> Option<String> {
        Some(render_tool_example(authored))
    }

    fn prompt_vocabulary(&self) -> DialectPromptVocabulary {
        TYPESCRIPT_PROMPT_VOCABULARY
    }

    fn history_item_definition(&self, images: bool) -> Vec<String> {
        let image_field = if images {
            ", images?: list[HistoryImage]"
        } else {
            ""
        };
        let mut lines = vec![
            "type HistoryItem =".to_string(),
            "  | { kind: \"message\", id: str, role: enum[\"user\", \"system\", \"assistant\", \"event\"], content: str, attachments?: list[HistoryAttachment] }".to_string(),
            format!("  | {{ kind: \"lashlang_step\", id: str, protocol_iteration: int, code: str, output: list[any]{image_field}, error?: str | null, final_output?: any | null }}"),
            "type HistoryAttachment = { id: str, media_type?: str | null, label?: str | null, source: str, reference: str }".to_string(),
        ];
        if images {
            lines.push("type HistoryImage = { id: str, media_type: str, width?: int | null, height?: int | null, bytes: int, label?: str | null }".to_string());
        }
        lines
    }

    fn render_execution_section(&self, request: ExecutionSectionRequest<'_>) -> String {
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
    print_call: "console.log",
    print_statement_prefix: "console.log(",
    print_statement_suffix: ")",
    finish_name: "finish",
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
    field_miss_rule: "Never write a field name you haven't seen in the key sets below — guessed field names silently produce zeros rather than errors. If a name is not listed, it does not exist on that value.",
    shape_notation: TYPESCRIPT_SHAPE_NOTATION,
};

/// The shape notation TypeScript prompts have always shown for inferred
/// values.
const TYPESCRIPT_SHAPE_NOTATION: ShapeNotation = ShapeNotation {
    any: "any",
    null: "null",
    bool: "bool",
    int: "int",
    float: "float",
    str: "str",
    record: "record",
    list_open: "list[",
    list_close: "]",
    union_separator: " | ",
    definition_keyword: "type ",
    definition_assign: " = ",
    record_open: "{",
    field_indent: "  ",
    field_separator: ": ",
    field_terminator: ",",
    record_close: "}",
};

/// Lashlang's type syntax in TypeScript's spelling.
///
/// The host surface is declared once, in Lashlang `TypeExpr`s, and both
/// dialects have to describe it. Rendering `list[str]` or `-> float` to a
/// TypeScript reader would be the same defect ADR 0063 closes everywhere else,
/// so the mapping is explicit rather than a formatted passthrough.
/// A host type's name as TypeScript can spell it.
///
/// Host data types are named with dots (`cron.Tick`), which is a valid
/// *reference* in Lashlang and not a valid TypeScript identifier. The
/// declaration already renders as `type cron_Tick = …`, so every reference to
/// it has to agree — otherwise the model is shown a type it cannot resolve
/// against the declaration immediately above it.
fn typescript_type_name(name: &str) -> String {
    name.replace('.', "_")
}

fn typescript_type(ty: &lashlang::TypeExpr) -> String {
    match ty {
        lashlang::TypeExpr::Any | lashlang::TypeExpr::Dict => "unknown".to_string(),
        lashlang::TypeExpr::Str => "string".to_string(),
        lashlang::TypeExpr::Int | lashlang::TypeExpr::Float => "number".to_string(),
        lashlang::TypeExpr::Bool => "boolean".to_string(),
        lashlang::TypeExpr::Null => "null".to_string(),
        lashlang::TypeExpr::Enum(values) => values
            .iter()
            .map(|value| format!("\"{value}\""))
            .collect::<Vec<_>>()
            .join(" | "),
        lashlang::TypeExpr::List(item) => format!("Array<{}>", typescript_type(item)),
        lashlang::TypeExpr::Object(fields) => {
            if fields.is_empty() {
                return "Record<string, never>".to_string();
            }
            let fields = fields
                .iter()
                .map(|field| {
                    let optional = if field.optional { "?" } else { "" };
                    format!("{}{optional}: {}", field.name, typescript_type(&field.ty))
                })
                .collect::<Vec<_>>()
                .join("; ");
            format!("{{ {fields} }}")
        }
        lashlang::TypeExpr::Ref(name) => typescript_type_name(name),
        lashlang::TypeExpr::Process(process) => match process.as_signature() {
            Some(signature) => format!(
                "Process<[{}], {}>",
                signature
                    .params()
                    .iter()
                    .map(|param| format!("{}: {}", param.name, typescript_type(&param.ty)))
                    .collect::<Vec<_>>()
                    .join(", "),
                typescript_type(signature.output())
            ),
            None => "Process".to_string(),
        },
        lashlang::TypeExpr::TriggerHandle(event) => {
            format!("TriggerHandle<{}>", typescript_type(event))
        }
        other => lashlang::format_type_expr(other),
    }
}

/// The host surface, in this dialect's spelling.
///
/// A TypeScript session used to receive no inventory at all: the section
/// rendered tool signatures and stopped, so the trigger sources, their
/// event types and the `triggers.*` operations were invisible — while the
/// host's own prompt told the model to use them. A judged row watched a
/// model search for `cron.Schedule`, find nothing, and conclude the trigger
/// APIs did not exist.
/// `TriggerSource<cron.Tick>` → `TriggerSource<cron_Tick>`.
///
/// The inventory resolves a constructor's output to a nominal label built from
/// the host type's own dotted name; only the payload inside the angle brackets
/// needs this dialect's spelling.
fn typescript_nominal_output(output: &str) -> String {
    match output.split_once('<') {
        Some((head, tail)) => {
            format!(
                "{head}<{}",
                typescript_type_name(tail.trim_end_matches('>'))
            ) + ">"
        }
        None => typescript_type_name(output),
    }
}

fn render_host_surface_section(
    tool_catalog: &lash_core::ToolCatalog,
    host_environment: &lashlang::LashlangHostEnvironment,
) -> String {
    let mut inventory = crate::protocol::prompt::host_surface_inventory(host_environment);
    // FIG-2999: the trigger operations are no longer gated by an ability,
    // so the prompt gates them on there being something to register. With
    // no declared trigger source a cell cannot build a `source` value, and
    // the whole `triggers.*` block — with the registration row type it
    // returns — is prose the model can never act on.
    if inventory.trigger_sources.is_empty() {
        inventory
            .operations
            .retain(|operation| operation.alias != lashlang::TRIGGER_MODULE_ALIAS);
        inventory
            .data_types
            .retain(|(name, _)| name != lashlang::TRIGGER_REGISTRATION_TYPE_NAME);
    }
    // Catalog tools already have a fully typed declaration under **Tools**,
    // rendered from the same contract; repeating them here would be a
    // second, weaker copy of the same signature.
    let documented_tools = tool_catalog
        .tools
        .iter()
        .filter_map(|tool| {
            lash_lashlang_runtime::required_tool_executable(&tool.manifest)
                .ok()
                .map(|binding| binding.call_path())
        })
        .collect::<std::collections::BTreeSet<_>>();
    let operations = inventory
        .operations
        .iter()
        .filter(|operation| {
            !documented_tools.contains(&format!("{}.{}", operation.alias, operation.operation))
        })
        .collect::<Vec<_>>();
    if operations.is_empty()
        && inventory.data_types.is_empty()
        && inventory.constructors.is_empty()
        && inventory.trigger_sources.is_empty()
    {
        return String::new();
    }
    let mut section = String::from("\n\n### Host Surface");
    if !operations.is_empty() {
        let lines = operations
            .iter()
            .map(|operation| {
                let signature = format!(
                    "{}.{}(input: {}): Promise<{}>; // lashlang `{}_{}`",
                    operation.alias,
                    operation.operation,
                    typescript_type(operation.input).replace("Record<string, never>", "{}"),
                    typescript_type(operation.output),
                    operation.alias,
                    operation.operation,
                );
                match crate::protocol::prompt::host_operation_description(
                    &operation.alias,
                    &operation.operation,
                ) {
                    Some(description) => format!("{signature}\n{description}"),
                    None => signature,
                }
            })
            .collect::<Vec<_>>()
            .join("\n    ");
        section.push_str(&format!(
                "\n\nAwaited runtime operations, called as `await <module>.<operation>(input)`:\n\n    {lines}"
            ));
    }
    if !inventory.data_types.is_empty() {
        let lines = inventory
            .data_types
            .iter()
            .map(|(name, ty)| {
                format!(
                    "// {name}\n    type {} = {};",
                    name.replace('.', "_"),
                    typescript_type(ty)
                )
            })
            .collect::<Vec<_>>()
            .join("\n    ");
        section.push_str(&format!("\n\nNamed host data types:\n\n    {lines}"));
    }
    if !inventory.constructors.is_empty() {
        let lines = inventory
            .constructors
            .iter()
            .map(|constructor| {
                format!(
                    "{}(input: {}): {}",
                    constructor.path,
                    typescript_type(constructor.input),
                    typescript_nominal_output(&constructor.output)
                )
            })
            .collect::<Vec<_>>()
            .join("\n    ");
        section.push_str(&format!(
                "\n\nPure value constructors. Never `await` these; use them wherever an expression is allowed:\n\n    {lines}"
            ));
    }
    if !inventory.trigger_sources.is_empty() {
        let lines = inventory
            .trigger_sources
            .iter()
            .map(|(source_ty, event)| {
                format!(
                    "- `{source_ty}` is a `triggers.register` `source` and emits `{}`",
                    typescript_type_name(event)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        section.push_str(&format!("\n\nTrigger source protocol metadata:\n\n{lines}"));
    }
    section
}

/// The process operations are leaf tools now (FIG-2999): nothing in the
/// dialect gates them, so their availability is read off the catalogue the
/// host actually rendered rather than off an ability flag.
pub(crate) fn catalogue_has_process_surface(tool_catalog: &lash_core::ToolCatalog) -> bool {
    tool_catalog.tools.iter().any(|tool| {
        lash_lashlang_runtime::required_tool_executable(&tool.manifest)
            .is_ok_and(|binding| binding.call_path().starts_with("processes."))
    })
}

/// The authoring rules the process tool signatures cannot state themselves.
///
/// `processes.*` renders from the catalogue like any other tool, so this block
/// carries only what a signature cannot: that a process is a literal value,
/// that its captures are copied by value, and that the signal a `run` body
/// waits for is typed where it is awaited.
pub(crate) fn typescript_process_prompt(process_surface: bool) -> String {
    if !process_surface {
        return String::new();
    }
    r#"A process is an `async` arrow the cell never calls: `const review = async (request: string) => { ... };`, or one written inline in a process tool's argument. Its name is the `const` it is bound to, and start arguments key by the arrow's parameter names. Returning from it succeeds; throwing fails.
Captures are by value: a name the body reads from the surrounding cell is copied when the process starts, so a later assignment is not seen, and a name that is not a durable `const` value is refused as a non-liftable capture.
`waitSignal(name: string): Promise<unknown>` is run-only: it suspends the process until that signal arrives. The signal set is inferred from the literal names waited for, and the payload is typed at the await site: `const go = (await waitSignal("go")) as { at: string };`.
A started handle outlives the turn; Stop cancels only the awaited handle; cancel is a request the child sees at its next step or wake."#
        .to_string()
}

/// Rewrites an authored Lashlang example into this dialect.
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
            // `expr?` — the Lashlang try-operator. TypeScript propagates a
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

fn render_execution_section(request: ExecutionSectionRequest<'_>) -> String {
    let ExecutionSectionRequest {
        channel,
        tools,
        tool_catalog,
        host_environment: environment,
        discovery_operation,
    } = request;
    let tools = if tools.is_empty() {
        String::new()
    } else {
        format!("\n\n### Tools\n\n{tools}")
    };
    let host_surface = render_host_surface_section(tool_catalog, environment);
    let allowed_sections = if host_surface.is_empty() {
        "**Tools**"
    } else {
        "**Tools** or **Host Surface**"
    };
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
    let sleep = if environment.abilities.sleep {
        "\n\n`await sleep(ms)` pauses the program. For a timeout, race a call against a timer — `await Promise.race([call, sleep(ms)])` is `undefined` when the timer wins, and the losing call keeps running until the turn ends."
    } else {
        ""
    };
    let host_api = format!(
        r#"Top-level bindings persist across executions. Return exactly the value and type the task asks for with `finish(value)`; do not finish an unexamined whole tool result. Putting an object into a string — with `+`, `` `${{...}}` `` or `String(...)` — gives the placeholder `[object Object]`, never its contents; read the value with `console.log(value)` or serialize it with `JSON.stringify(value)`.

`Math`, `Date` (UTC), `String`, `Array`, `Object`, `JSON`, `Map`/`Set`, `RegExp` and `URL` are available; this is not Node or a browser, and classes and generators are not supported.

### Host API

`console.log(value)` shows output in the next step; `print(value)` shows a structured value, summarised field by field rather than cut off when it is large; `finish(value)` ends the turn. A failed tool call throws an `Error` whose `cause` is `{{ code, details }}`.{sleep}{durable}"#
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
    // `tools` and `host_surface` either carry their own leading `\n\n` or
    // are empty, so they append directly — an unconditional separator here
    // leaves stray blank lines where a skipped block would have gone.
    // A host with a discovery tool says so at the end of the first
    // paragraph, where the model reads which tools it may call.
    let discovery = discovery_operation
        .map(|operation| {
            format!(" Other tools exist; find them with `await {operation}({{ ... }})`.")
        })
        .unwrap_or_default();
    format!(
        "Use prose for conversation; use {action} for action or computation. Call tools as `await module.operation({{ ... }})`, only those listed under {allowed_sections}.{discovery}\n\n{response_shape}\n{example}\n\n{host_api}{tools}{host_surface}"
    )
}

#[cfg(test)]
mod tests {
    use lash_sansio::SessionId;

    use super::*;
    use crate::dialect::{RlmDialectServices, SessionDialect};
    use crate::projection::RlmProjectedBindings;
    use lash_core::ExecRequest;
    use lash_core::plugin::ToolCatalogContext;
    use lash_lashlang_runtime::LashlangSurface;

    const SEED: u64 = 0x5_2c04;
    use lash_lashlang_runtime::{ToolBinding, ToolDefinitionBindingExt};

    #[test]
    fn identity_and_cell_tags_are_typescript() {
        let dialect = SessionDialect::new(
            std::sync::Arc::new(crate::dialect::TypescriptDialect),
            LashlangSurface::default(),
            RlmDialectServices {
                workers: lash_vm_client::service::Service::default(),
                artifact_store: crate::testing::memory_artifact_store_blocking(),
                deferred_tool_resolver: None,
                deferred_trigger_resolver: None,
                execution_trace_config: crate::executor::RlmLashlangExecutionTraceConfig::default(),
                execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
                code_renderer: Default::default(),
                channel: crate::plugin::RlmChannel::Cell,
            },
        );
        assert_eq!(dialect.language_id(), "typescript");
        assert_eq!(dialect.cell_tags().open, "<typescript>");
        assert_eq!(dialect.cell_tags().close, "</typescript>");
    }

    /// A TypeScript session must be told what it may register a trigger on.
    ///
    /// The section used to render tool signatures and stop, so a host that
    /// declared `cron.Schedule` left the `triggers.*` operations out of the
    /// substrate's prompt while the host prompt copy advertised them. A judged
    /// row watched a model search for `cron.Schedule`, find nothing, and
    /// conclude the trigger APIs did not exist — a VOID row produced by a
    /// prompt that denied a capability the session actually had.
    #[test]
    fn the_execution_section_declares_the_hosts_trigger_surface() {
        let mut resources = lashlang::LashlangHostCatalog::new();
        resources
            .add_trigger_source_constructor(
                ["cron", "Schedule"],
                lashlang::TypeExpr::Object(vec![
                    lashlang::TypeField {
                        name: "expr".into(),
                        ty: lashlang::TypeExpr::Str,
                        optional: false,
                    },
                    lashlang::TypeField {
                        name: "tz".into(),
                        ty: lashlang::TypeExpr::Str,
                        optional: true,
                    },
                ]),
                lashlang::NamedDataType::object(
                    "cron.Tick",
                    vec![lashlang::TypeField {
                        name: "fired_at".into(),
                        ty: lashlang::TypeExpr::Str,
                        optional: false,
                    }],
                )
                .expect("valid tick type"),
            )
            .expect("cron trigger source");
        let dialect = SessionDialect::new(
            std::sync::Arc::new(crate::dialect::TypescriptDialect),
            lash_lashlang_runtime::LashlangSurface {
                abilities: lashlang::LashlangAbilities::all(),
                language_features: Default::default(),
                resources,
            },
            RlmDialectServices {
                workers: lash_vm_client::service::Service::default(),
                artifact_store: crate::testing::memory_artifact_store_blocking(),
                deferred_tool_resolver: None,
                deferred_trigger_resolver: None,
                execution_trace_config: crate::executor::RlmLashlangExecutionTraceConfig::default(),
                execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
                code_renderer: Default::default(),
                channel: crate::plugin::RlmChannel::Cell,
            },
        );
        let section = dialect
            .render_execution_section(
                crate::protocol::RlmPromptFeatures::default(),
                &lash_core::ToolCatalog::from_tool_definitions(vec![]),
                crate::plugin::RlmChannel::Cell,
                None,
            )
            .expect("render execution section");

        assert!(section.contains("### Host Surface"), "{section}");
        assert!(
            section.contains(
                "cron.Schedule(input: { expr: string; tz?: string }): TriggerSource<cron_Tick>"
            ),
            "the constructor must be declared in TypeScript's own type spelling: {section}"
        );
        // The reference and the declaration must agree: a dotted name is a
        // valid Lashlang reference and not a TypeScript identifier, so the
        // model would otherwise be shown a type it cannot resolve against the
        // declaration directly above it.
        assert!(section.contains("type cron_Tick ="), "{section}");
        // Every *reference* agrees with the declaration. The host's real dotted
        // name survives only in the comment above each declaration, which is
        // the bridge to the name the host's own errors and docs use.
        let code_lines = section
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !code_lines.contains("cron.Tick"),
            "no reference may keep the dotted spelling: {section}"
        );
        assert!(
            section.contains("`cron.Schedule` is a `triggers.register` `source`"),
            "the trigger source names the tool that consumes it: {section}"
        );
        // Host Surface operations spell the call the model actually types —
        // `module.operation(input): Promise<…>`, the same form the **Tools**
        // section uses — with the lashlang identifier demoted to a comment.
        assert!(
            section.contains("triggers.register(input:"),
            "the callable signature leads: {section}"
        );
        assert!(section.contains("triggers.list(input:"), "{section}");
        assert!(
            !code_lines.contains("triggers_register("),
            "the internal `triggers_register` name must not be the signature: {section}"
        );
        // Skipped blocks join out cleanly: no run of blank lines where an
        // absent `### Processes` (or empty Tools/Host Surface) would leave a
        // gap.
        assert!(
            !section.contains("\n\n\n"),
            "a skipped block must not leave blank residue: {section}"
        );
        // The process surface is the catalogue now (FIG-2999): an empty
        // catalogue renders no process vocabulary at all.
        assert!(!section.contains("defineProcess"), "{section}");
        assert!(!section.contains("### Processes"), "{section}");
        assert!(!section.contains("ProcessDefinition"), "{section}");
        // And none of it may arrive in Lashlang's type syntax (ADR 0063).
        for leak in ["list[", "-> str", ": str`", "float`", "trigger.register("] {
            assert!(!section.contains(leak), "`{leak}` leaked: {section}");
        }
    }

    #[test]
    fn execution_section_renders_promise_tool_signatures_and_agent_contract() {
        let dialect = SessionDialect::new(
            std::sync::Arc::new(crate::dialect::TypescriptDialect),
            LashlangSurface::default(),
            RlmDialectServices {
                workers: lash_vm_client::service::Service::default(),
                artifact_store: crate::testing::memory_artifact_store_blocking(),
                deferred_tool_resolver: None,
                deferred_trigger_resolver: None,
                execution_trace_config: crate::executor::RlmLashlangExecutionTraceConfig::default(),
                execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
                code_renderer: Default::default(),
                channel: crate::plugin::RlmChannel::Cell,
            },
        );
        let tool = lash_core::ToolDefinition::raw(
            "tool:test/web_fetch",
            "web_fetch",
            "Fetch a URL",
            serde_json::json!({
                "type": "object",
                "properties": { "url": { "type": "string" } },
                "required": ["url"],
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "string" }),
        )
        .with_tool_binding(ToolBinding::new(["web"], "fetch"));
        let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![tool]);
        let section = dialect
            .render_execution_section(
                crate::protocol::RlmPromptFeatures::default(),
                &catalog,
                crate::plugin::RlmChannel::Cell,
                None,
            )
            .expect("render execution section");
        assert!(
            section.contains("web.fetch({ url: string }): Promise<string>"),
            "{section}"
        );
        assert!(
            !section.contains("defineProcess"),
            "disabled processes stay hidden"
        );
        assert!(
            !section.contains("Promise.allSettled"),
            "fan-out needs no teaching: {section}"
        );
        assert!(!section.contains("### v1 guardrails"));
        assert!(!section.contains("### Deterministic standard library"));
        assert!(section.contains("`Date` (UTC)"));
    }

    /// The process section is gated by the catalogue, not by an ability.
    ///
    /// FIG-2999 deleted `LashlangAbilities.{processes, process_signals,
    /// triggers}`: whether a session can run processes is whether the host
    /// rendered the `processes.*` tools, so the authoring block appears with
    /// them and disappears without them. It carries only what a tool signature
    /// cannot say — that the body is an uncalled `async` arrow whose parameter
    /// names are the start argument keys, that captures are copied by value,
    /// and where the awaited signal payload is typed.
    #[test]
    fn the_process_section_follows_the_catalogue_and_teaches_the_argument_convention() {
        let dialect = SessionDialect::new(
            std::sync::Arc::new(crate::dialect::TypescriptDialect),
            LashlangSurface::default(),
            RlmDialectServices {
                workers: lash_vm_client::service::Service::default(),
                artifact_store: crate::testing::memory_artifact_store_blocking(),
                deferred_tool_resolver: None,
                deferred_trigger_resolver: None,
                execution_trace_config: crate::executor::RlmLashlangExecutionTraceConfig::default(),
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
            with.contains("Captures are by value"),
            "capture-by-value is prompt-only knowledge: {with}"
        );
        assert!(
            with.contains("typed at the await site"),
            "signal typing belongs at the await site: {with}"
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

    /// Every `TS_` token the prompt names must be a code the dialect can
    /// actually emit.
    ///
    /// The prompt is prose, so a code name in it is unchecked by the compiler.
    /// This layer shipped `TS_FOR_OF_ITERATOR_UNSUPPORTED` — a code that has
    /// never existed — into the production prompt, into the assertion above
    /// (which pinned the falsehood rather than catching it), and into a runbook
    /// gate that could therefore never fire. Telling the model to expect a
    /// string it will never see degrades exactly the error recovery the prompt
    /// exists to support, so the whole class is closed here rather than the one
    /// instance.
    #[test]
    fn every_diagnostic_code_named_in_the_prompt_exists() {
        let dialect = SessionDialect::new(
            std::sync::Arc::new(crate::dialect::TypescriptDialect),
            LashlangSurface::default(),
            RlmDialectServices {
                workers: lash_vm_client::service::Service::default(),
                artifact_store: crate::testing::memory_artifact_store_blocking(),
                deferred_tool_resolver: None,
                deferred_trigger_resolver: None,
                execution_trace_config: crate::executor::RlmLashlangExecutionTraceConfig::default(),
                execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
                code_renderer: Default::default(),
                channel: crate::plugin::RlmChannel::Cell,
            },
        );
        let prompt = dialect
            .render_execution_section(
                crate::protocol::RlmPromptFeatures::default(),
                &lash_core::ToolCatalog::from_tool_definitions(Vec::new()),
                crate::plugin::RlmChannel::Cell,
                None,
            )
            .expect("render execution section");
        let mut named = std::collections::BTreeSet::new();
        let mut rest = prompt.as_str();
        while let Some(start) = rest.find("TS_") {
            rest = &rest[start..];
            let end = rest
                .find(|character: char| !character.is_ascii_uppercase() && character != '_')
                .unwrap_or(rest.len());
            named.insert(&rest[..end]);
            rest = &rest[end..];
        }
        assert!(
            named.is_empty(),
            "FIG-2750 removes diagnostic inventory: {named:?}"
        );
    }

    /// The second, structural check on prompt honesty: the diagnostics the
    /// prompt's own primitives can emit must be spelled in this dialect.
    ///
    /// `every_diagnostic_code_named_in_the_prompt_exists` walks `TS_` tokens,
    /// so it can only see *codes*. It cannot see an identifier leak, and one
    /// shipped: misusing `waitSignal` rejected with ``` `wait_signal` can only
    /// be used inside a process body ``` — a Lashlang identifier that appears
    /// nowhere in the TypeScript prompt, handed to a model that has no way to
    /// map it back. This walks the other direction: every primitive the prompt
    /// declares is misused on purpose, and the resulting model-facing message
    /// must not name a Lashlang-only spelling.
    #[test]
    fn no_diagnostic_from_a_prompt_primitive_names_a_lashlang_identifier() {
        // The catalog carries a real trigger source so the registration misuse
        // below reaches the target: with no declared source, every trigger
        // registration fails on its `source` first and the fixture proves
        // nothing about dynamic targets.
        let mut resources = lashlang::LashlangHostCatalog::default();
        resources
            .add_trigger_source_constructor(
                ["timer", "Schedule"],
                lashlang::TypeExpr::Object(vec![lashlang::TypeField {
                    name: "expr".into(),
                    ty: lashlang::TypeExpr::Str,
                    optional: false,
                }]),
                lashlang::NamedDataType::object(
                    "timer.Tick",
                    vec![lashlang::TypeField {
                        name: "fired_at".into(),
                        ty: lashlang::TypeExpr::Str,
                        optional: false,
                    }],
                )
                .expect("valid timer tick type"),
            )
            .expect("valid timer trigger source");
        // ... and the trigger operations themselves. Without them `triggers`
        // is an unknown module, every registration misuse below rejects with
        // `TS_LINK_ERROR: unknown module 'triggers'`, and the fixture proves
        // nothing about the shapes it names.
        lashlang::add_trigger_resource_operations(&mut resources)
            .expect("valid trigger operations");
        lashlang::add_trigger_register_tool_binding(&mut resources)
            .expect("trigger register tool binding is unique");
        let host =
            lashlang::LashlangHostEnvironment::new(resources, lashlang::LashlangAbilities::all());
        // Identifiers that exist only in Lashlang's surface. A model reading
        // the TypeScript prompt has never seen any of them.
        let lashlang_only = [
            "wait_signal",
            "signal_run",
            "define_process",
            "register_trigger",
            lashlang::LANGUAGE_RUNTIME_MODULE_PATH,
        ];
        // Misuse shapes for the primitives the Host API block declares. Each
        // must reject, and reject in TypeScript's own vocabulary.
        let misuses = [
            ("waitSignal at top level", "await waitSignal(\"go\");"),
            (
                "waitSignal inside a plain function",
                "function f(): unknown { return waitSignal(\"go\"); } finish(f());",
            ),
            (
                "a process body capturing a mutable binding",
                "let counter = 1; const p = async (a: unknown) => { return counter; }; finish(1);",
            ),
            (
                "a trigger registration whose `inputs` is not the erased arrow",
                "const p = async (a: unknown) => { return a; }; finish(await triggers.register({ source: timer.Schedule({ expr: \"0 8 * * *\" }), target: { definition: p }, inputs: { a: 1 } }));",
            ),
            (
                "an unknown binding",
                "finish(await nowhere.fetch({ url: \"x\" }));",
            ),
        ];

        let mut leaks = Vec::new();
        for (label, source) in misuses {
            let message = match lash_typescript::link(source, &host) {
                Ok(_) => {
                    leaks.push(format!("{label}: linked, so it is not a misuse at all"));
                    continue;
                }
                Err(error) => error.to_string(),
            };
            for identifier in lashlang_only {
                if message.contains(identifier) {
                    leaks.push(format!("{label}: names `{identifier}` — {message}"));
                }
            }
        }
        assert!(
            leaks.is_empty(),
            "model-facing TypeScript diagnostics leak Lashlang identifiers: {leaks:#?}"
        );

        // The process authoring block is catalogue-gated, so the prompt text
        // itself is asserted directly rather than through a rendered section.
        let prompt = typescript_process_prompt(true);
        assert!(prompt.contains("`waitSignal(name: string): Promise<unknown>` is run-only"));
        lash_typescript::link("await sleep(1); finish(1);", &host)
            .expect("the prompt says sleep is also valid in a cell");
    }

    #[test]
    fn session_executes_a_typescript_request_end_to_end() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let dialect = SessionDialect::new(
                    std::sync::Arc::new(crate::dialect::TypescriptDialect),
                    LashlangSurface::default(),
                    RlmDialectServices {
                        workers: lash_vm_client::service::Service::default(),
                        artifact_store: crate::testing::memory_artifact_store().await,
                        deferred_tool_resolver: None,
                        deferred_trigger_resolver: None,
                        execution_trace_config:
                            crate::executor::RlmLashlangExecutionTraceConfig::default(),
                        execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
                        code_renderer: Default::default(),
                        channel: crate::plugin::RlmChannel::Cell,
                    },
                );
                let mut session = dialect.create_session();
                let double =
                    crate::testing::kernel_double(SEED, lash_restate_test::ServerConfig::default())
                        .await;
                let handler = double
                    .open_handler(crate::testing::default_cell_scope())
                    .await
                    .expect("open the cell's handler");
                let response = session
                    .execute(
                        lash_core::testing::code_execution_context(crate::testing::double_ports(
                            &double, &handler,
                        )),
                        ExecRequest {
                            code: "const answer: number = 40 + 2; finish(answer);".to_string(),
                        },
                        RlmProjectedBindings::new(),
                    )
                    .await
                    .expect("execute typescript");
                handler.close().await.expect("close the cell's handler");

                assert_eq!(response.error, None);
                assert_eq!(response.terminal_finish, Some(serde_json::json!(42)));
            });
    }

    /// Every identifier the rendered catalog advertises must link to a binding.
    ///
    /// The instance defect — a reserved-word operation advertised only as
    /// `__lash_tool_<hex>`, which rejects with `TS_UNKNOWN_BINDING` for itself —
    /// is one member of a class: any name the renderer spells differently from
    /// the way a cell must call it is a promise the catalog cannot keep. This
    /// sweep holds both halves of the contract over every hazardous name in
    /// every path position: registration refuses the paths no cell can address,
    /// and every declaration rendered for the paths it admits is callable
    /// exactly as advertised (FIG-1444).
    #[test]
    fn every_advertised_catalog_identifier_is_callable_verbatim() {
        let mut hazards = lash_typescript::reserved_words().to_vec();
        // Names the lowerer resolves itself rather than dispatching: the promise
        // chaining refusal (`then`/`catch`/`finally`) and the instance stdlib
        // collision matrix FIG-1443 fixed.
        hazards.extend(["then", "catch", "finally"]);
        hazards.extend(lash_typescript::accepted_instance_methods());
        // Strict-mode-illegal *binding* names that are still legal member
        // roots: `eval.op`/`arguments.op` lower and dispatch like any other
        // tool path (the literal `undefined` does too, via RESERVED_WORDS).
        // FIG-1483 records the decision to admit them — the catalog advertises
        // the call path, which is exactly what the cell writes — so the sweep
        // holds their admission rather than assuming a refusal.
        hazards.extend(["eval", "arguments"]);
        // Roots the lowerer treats as ECMA global namespaces, so a tool module
        // can never be addressed under them.
        hazards.extend([
            "Math",
            "Date",
            "Promise",
            "String",
            "Object",
            "Symbol",
            "globalThis",
            "Intl",
            "Error",
            "Set",
            "URL",
            "RegExp",
            "JSON",
            "Number",
            "Array",
            "Map",
            "console",
            "crypto",
        ]);
        hazards.sort_unstable();
        hazards.dedup();

        let candidates = hazards
            .iter()
            .flat_map(|word| {
                [
                    (vec![word.to_string()], "op".to_string()),
                    (
                        vec!["outer".to_string(), word.to_string()],
                        "op".to_string(),
                    ),
                    (vec!["probe".to_string()], word.to_string()),
                    (
                        vec!["probe".to_string(), "inner".to_string()],
                        word.to_string(),
                    ),
                ]
            })
            .collect::<Vec<_>>();

        let dialect = crate::dialect::typescript_test_dialect();
        let mut admitted = Vec::new();
        let mut refused = Vec::new();
        for (modules, operation) in &candidates {
            let call_path = format!("{}.{operation}", modules.join("."));
            let name = format!("t_{}", call_path.replace('.', "_"));
            let tool = lash_core::ToolDefinition::raw(
                format!("tool:test/{name}"),
                name.clone(),
                "Probe",
                serde_json::json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": { "id": { "type": "string" } },
                    "required": ["id"]
                }),
                serde_json::json!({ "type": "string" }),
            )
            .with_tool_binding(ToolBinding::new(modules.clone(), operation.as_str()));
            let registration = crate::tool_catalog::rlm_tool_catalog(
                ToolCatalogContext {
                    owner: lash_core::RuntimeOwner::Session(SessionId::from("session")),
                    tools: vec![tool.manifest()],
                    resolve_contract: None,
                    tool_access: lash_core::SessionToolAccess::default(),
                    subagent: None,
                    extensions: Default::default(),
                },
                &dialect,
            );
            match registration {
                Ok(_) => admitted.push((tool, modules.clone(), operation.clone(), call_path)),
                Err(error) => {
                    assert!(
                        error.to_string().contains("no TypeScript cell can call"),
                        "{call_path} was refused for an unrelated reason: {error}"
                    );
                    refused.push(call_path);
                }
            }
        }

        // The refusals are the paths a cell cannot write or the lowerer claims
        // for itself; every one of them used to be advertised as a callable.
        // `undefined`, `eval` and `arguments` joined them in FIG-3656: each
        // now names a real global value, so `X.op` is that value's member
        // call — never a tool path.
        for expected in [
            "delete.op",
            "new.op",
            "Math.op",
            "probe.then",
            "probe.catch",
            "undefined.op",
            "eval.op",
            "arguments.op",
        ] {
            assert!(
                refused.iter().any(|path| path == expected),
                "registration must refuse `{expected}`: {refused:?}"
            );
        }
        assert!(
            admitted.len() > 200,
            "the sweep must admit the bulk of the matrix, not just a handful: {}",
            admitted.len()
        );

        let catalog = lash_core::ToolCatalog::from_tool_definitions(
            admitted.iter().map(|(tool, ..)| tool.clone()).collect(),
        );
        let section = SessionDialect::prompt_only(
            std::sync::Arc::new(crate::dialect::TypescriptDialect),
            LashlangSurface::default(),
        )
        .render_execution_section(
            crate::protocol::RlmPromptFeatures::default(),
            &catalog,
            crate::plugin::RlmChannel::Cell,
            None,
        )
        .expect("render execution section");
        let declarations = tool_declarations(&section);
        assert_eq!(
            declarations.len(),
            admitted.len(),
            "every admitted tool must be advertised once"
        );

        let advertised = declarations
            .iter()
            .map(|declaration| advertised_call_path(declaration))
            .collect::<std::collections::BTreeSet<_>>();
        for (_, modules, operation, call_path) in &admitted {
            assert!(
                advertised.contains(call_path),
                "`{call_path}` is in the catalog but is not advertised under its call path: {declarations:?}"
            );
            lash_typescript::ensure_tool_call_path_addressable(call_path)
                .expect("an admitted path is addressable");
            assert_eq!(
                dispatch_through(call_path, modules, operation),
                vec![(modules.join("."), operation.clone())],
                "`{call_path}` must dispatch the binding it advertises"
            );
        }
    }

    fn tool_declarations(section: &str) -> Vec<String> {
        section
            .split_once("### Tools")
            .expect("Tools section")
            .1
            .split("\n### ")
            .next()
            .unwrap()
            .lines()
            .filter_map(|line| {
                line.strip_prefix('`')
                    .and_then(|line| line.strip_suffix('`'))
            })
            .filter(|line| line.contains("): Promise<"))
            .map(str::to_string)
            .collect()
    }

    fn advertised_call_path(signature: &str) -> String {
        signature
            .split_once('(')
            .expect("method signature")
            .0
            .to_string()
    }

    /// Links and runs the advertised call against a host binding for
    /// `modules`/`operation`, returning what the host was asked to dispatch.
    fn dispatch_through(
        call_path: &str,
        modules: &[String],
        operation: &str,
    ) -> Vec<(String, String)> {
        struct RecordingHost {
            dispatched: std::sync::Mutex<Vec<(String, String)>>,
        }
        impl lashlang::ExecutionHost for RecordingHost {
            async fn perform(
                &self,
                op: lashlang::AbilityOp,
            ) -> Result<lashlang::AbilityOutcome, lashlang::ExecutionHostError> {
                match op {
                    lashlang::AbilityOp::ResourceOperation(call) => {
                        let alias = match &call.receiver {
                            lashlang::Value::Resource(handle) => handle.alias.clone(),
                            other => format!("{other:?}"),
                        };
                        self.dispatched
                            .lock()
                            .expect("dispatched lock")
                            .push((alias, call.operation));
                        Ok(lashlang::AbilityOutcome::Value(lashlang::Value::String(
                            "tool-ok".into(),
                        )))
                    }
                    lashlang::AbilityOp::Finish(value) => {
                        Ok(lashlang::AbilityOutcome::Value(value))
                    }
                    other => Err(lashlang::ExecutionHostError::new(format!(
                        "unexpected ability {other:?}"
                    ))),
                }
            }
        }

        let mut catalog = lashlang::LashlangHostCatalog::new();
        catalog
            .add_module_operation_contract(
                modules.to_vec(),
                "ToolModule",
                operation,
                format!("tool:test/{}", modules.join("_")),
                &lashlang::OperationContract::new(serde_json::json!({}), serde_json::json!({})),
            )
            .expect("operation binding");
        let environment =
            lashlang::LashlangHostEnvironment::new(catalog, lashlang::LashlangAbilities::default());
        let source = format!(r#"finish(await {call_path}({{ id: "m1" }}));"#);
        let linked = lash_typescript::link(&source, &environment)
            .unwrap_or_else(|error| panic!("`{source}` must link: {error:?}"));
        let host = RecordingHost {
            dispatched: std::sync::Mutex::new(Vec::new()),
        };
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(lashlang::execute(
                &lashlang::testing::harness::compile_linked_main(&linked),
                &mut lashlang::State::new(),
                &host,
            ))
            .unwrap_or_else(|error| panic!("`{source}` must execute: {error:?}"));
        host.dispatched.lock().expect("dispatched lock").clone()
    }
}
