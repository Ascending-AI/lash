use lash_core::SessionError;
use lash_lashlang_runtime::LashlangSurface;

use super::{
    CellTags, DialectSession, LashlangDialectServices, RlmDialect, RlmDialectSession, SourceDialect,
};

pub(crate) const LANGUAGE_ID: &str = "typescript";

pub(crate) struct TypescriptDialect {
    surface: LashlangSurface,
    services: LashlangDialectServices,
}

impl TypescriptDialect {
    pub(crate) fn new(surface: LashlangSurface, services: LashlangDialectServices) -> Self {
        Self { surface, services }
    }

    /// A dialect that can render prompts and diagnostics but cannot execute,
    /// mirroring `LashlangDialect::prompt_only`. The protocol driver needs one
    /// per dialect to answer questions about cells without an execution
    /// environment behind it.
    pub(crate) fn prompt_only(surface: LashlangSurface) -> Self {
        Self {
            surface,
            services: LashlangDialectServices {
                projection_resolver: std::sync::Arc::new(
                    crate::projection::ProjectionRegistry::new(),
                ),
                artifact_store: lashlang::global_in_memory_lashlang_artifact_store(),
                deferred_tool_resolver: None,
                execution_trace_config: crate::executor::RlmLashlangExecutionTraceConfig::default(),
                execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
                channel: crate::plugin::RlmChannel::Cell,
            },
        }
    }
}

fn is_plain_identifier(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
        && !text.starts_with(|character: char| character.is_ascii_digit())
}

pub(crate) const TYPESCRIPT_PROMPT_VOCABULARY: crate::dialect::DialectPromptVocabulary =
    crate::dialect::DialectPromptVocabulary {
        language_name: "TypeScript",
        cell_open_tag: "<typescript>",
        cell_noun: "cell",
        print_call: "console.log",
        print_statement_prefix: "console.log(",
        print_statement_suffix: ")",
        finish_statement: "finish(value)",
        finish_null_statement: "finish(null)",
        continue_as_call: "control.continue_as(...)",
        continue_as_example: "await control.continue_as({ task: \"continue the audit from the summarized findings\", seed: { problem: input.prompt, findings: findings } });",
        // A TypeScript session has no type-literal form: the lowerer never
        // builds `Expr::TypeLiteral`, so its only nested-shape route would be
        // hand-writing the runtime's reserved `$lash_type` wrapper. Silence is
        // the honest rendering; flat string descriptors are what it can write.
        type_literal_hint: "",
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
        lashlang::TypeExpr::Process { input, output, .. } => format!(
            "Process<{}, {}>",
            typescript_type(input),
            typescript_type(output)
        ),
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

impl TypescriptDialect {
    fn render_host_surface_section(
        &self,
        tool_catalog: &lash_core::ToolCatalog,
    ) -> Result<String, SessionError> {
        let host_environment = self
            .surface
            .host_environment(tool_catalog)
            .map_err(|error| {
                SessionError::Protocol(format!("invalid host tool surface: {error}"))
            })?;
        let inventory = crate::protocol::prompt::host_surface_inventory(&host_environment);
        // Catalog tools already have a fully typed declaration under **Tools**,
        // rendered from the same contract; repeating them here would be a
        // second, weaker copy of the same signature.
        let documented_tools = tool_catalog
            .tools
            .iter()
            .filter_map(|tool| {
                lash_lashlang_runtime::required_tool_typescript_executable(&tool.manifest)
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
            return Ok(String::new());
        }
        let mut section = String::from("\n\n### Host Surface");
        if !operations.is_empty() {
            let lines = operations
                .iter()
                .map(|operation| {
                    format!(
                        "{}_{}(input: {}): Promise<{}>; // await {}.{}(input)\n{}",
                        operation.alias,
                        operation.operation,
                        typescript_type(operation.input).replace("Record<string, never>", "{}"),
                        typescript_type(operation.output),
                        operation.alias,
                        operation.operation,
                        crate::protocol::prompt::host_operation_description(
                            &operation.alias,
                            &operation.operation
                        )
                        .unwrap_or("")
                    )
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
                        "- `{source_ty}` can be passed to `registerTrigger` as its `source` and emits `{}`",
                        typescript_type_name(event)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            section.push_str(&format!("\n\nTrigger source protocol metadata:\n\n{lines}"));
        }
        Ok(section)
    }
}

pub(crate) fn typescript_process_prompt(abilities: &lashlang::LashlangAbilities) -> String {
    let mut lines = Vec::new();
    if abilities.processes {
        lines.push(r#"interface Process<Input = unknown, Output = unknown> { readonly name: string }
defineProcess(c: {name: string; run: Function; signals?: Record<string, null>}): Process;
start(p: Process, args?: Record<string, unknown>): Promise<unknown> & {id: string};
wake(value: unknown): void;
Use top-level const, literal name, async run; start keys match run parameter names. Return succeeds after finally; throw fails."#);
        if abilities.process_signals {
            lines.push(
                r#"waitSignal(name: string): Promise<unknown>;
wake(handle: {id: string}, signal: string, payload: unknown): void;
Signals: {go: null}; waitSignal is run-only."#,
            );
        }
        if abilities.triggers {
            lines.push(r#"registerTrigger(c: {source: unknown; target: Process; inputs: Record<string, unknown>; name?: string}): Promise<unknown>;
Literal target; inputs match run parameters."#);
        }
    }
    if abilities.sleep {
        lines.push("`await sleep(ms)` pauses the program.");
    }
    let prompt = lines.join("\n");
    if abilities.process_signals {
        prompt
    } else {
        prompt.replace("; signals?: Record<string, null>", "")
    }
}

impl RlmDialect for TypescriptDialect {
    fn language_id(&self) -> &'static str {
        LANGUAGE_ID
    }

    fn renders_tool_catalogue_inline(&self) -> bool {
        true
    }

    fn prompt_vocabulary(&self) -> crate::dialect::DialectPromptVocabulary {
        TYPESCRIPT_PROMPT_VOCABULARY
    }

    fn tool_call_path(&self, manifest: &lash_core::ToolManifest) -> Result<String, SessionError> {
        Ok(
            lash_lashlang_runtime::required_tool_typescript_executable(manifest)
                .map_err(|error| SessionError::Protocol(error.to_string()))?
                .call_path(),
        )
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
    fn render_tool_example(&self, example: &str) -> String {
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

    fn snapshot_engine_id(&self) -> &'static str {
        LANGUAGE_ID
    }

    fn cell_tags(&self) -> CellTags {
        CellTags {
            open: "<typescript>",
            close: "</typescript>",
        }
    }

    fn create_session(&self) -> Result<Box<dyn RlmDialectSession>, SessionError> {
        Ok(Box::new(DialectSession::new(
            SourceDialect::Typescript,
            LANGUAGE_ID,
            self.surface.clone(),
            self.services.clone(),
        )))
    }

    fn render_execution_section(
        &self,
        features: crate::protocol::RlmPromptFeatures,
        tool_catalog: &lash_core::ToolCatalog,
    ) -> Result<String, SessionError> {
        let tools = crate::tool_catalog::rlm_prompt_tool_docs(tool_catalog, self, features);
        let tools = if tools.is_empty() {
            String::new()
        } else {
            format!("\n\n### Tools\n\n{tools}")
        };
        let host_surface = self.render_host_surface_section(tool_catalog)?;
        let allowed_sections = if host_surface.is_empty() {
            "**Tools**"
        } else {
            "**Tools** or **Host Surface**"
        };
        let response_shape = super::cell_response_shape(self.cell_tags(), self.prompt_vocabulary());
        let environment = self
            .surface
            .host_environment(tool_catalog)
            .map_err(|error| SessionError::Protocol(error.to_string()))?;
        let mut process_abilities = environment.abilities;
        process_abilities.sleep = false;
        let durable = typescript_process_prompt(&process_abilities);
        let durable = if durable.is_empty() {
            durable
        } else {
            format!("\n\n### Processes\n\n{durable}")
        };
        let sleep = if environment.abilities.sleep {
            "\n\n`await sleep(ms)` pauses the program."
        } else {
            ""
        };
        let host_api = format!(
            r#"Top-level bindings persist across executions. Return exactly the value and type the task asks for with `finish(value)`; do not finish an unexamined whole tool result.

`Math`, `Date` (UTC), `String`, `Array`, `Object`, `JSON`, `Map`/`Set`, `RegExp` and `URL` are available; this is not Node or a browser, and classes, generators and `Promise.race` are not supported.

### Host API

`console.log(value)` shows output in the next step; `finish(value)` ends the turn. A failed tool call throws an `Error` whose `cause` is `{{ code, details }}`.{sleep}{durable}"#
        );
        let example =
            "### Example cell\n\n<typescript>\nconst total = 1 + 2;\nfinish(total);\n</typescript>";
        Ok(format!(
            "Use prose for conversation; use a paired `<typescript>` block for action or computation. Call tools as `await module.operation({{ ... }})`, only those listed under {allowed_sections}.\n\n{response_shape}\n{example}\n\n{host_api}\n\n{tools}{host_surface}"
        ))
    }

    fn finalization_copy(&self, termination: &lash_rlm_types::RlmTermination) -> String {
        match termination {
            lash_rlm_types::RlmTermination::FinishRequired { schema } => {
                self.finish_required_finalization(schema.is_some())
            }
            lash_rlm_types::RlmTermination::Natural => {
                "Natural termination: prose alone ends this turn as the final answer, so write prose only when no work remains; otherwise perform the next step in a block, and call `finish(value)` inside the program to return a computed value.".to_string()
            }
        }
    }

    fn cell_error_message(&self, error: crate::protocol::CellExtractionError) -> String {
        match error {
            crate::protocol::CellExtractionError::UnclosedCell => {
                "Model response started a `<typescript>` block but did not close it. Retry with one complete paired block. A line whose trimmed content is exactly `</typescript>` closes the cell.".to_string()
            }
        }
    }

    fn turn_limit_final_copy(&self, max_turns: usize) -> String {
        format!(
            "Turn limit reached ({max_turns}). Reply in plain prose with accomplishments, remaining work, and next steps; do not emit a TypeScript block."
        )
    }

    fn finish_required_copy(&self, requires_schema: bool) -> String {
        if requires_schema {
            "Call `finish(value)` inside a paired `<typescript>...</typescript>` block when the task is complete, with a value matching the required output schema.".to_string()
        } else {
            "Call `finish(value)` inside a paired `<typescript>...</typescript>` block when the task is complete. Use `finish(null)` only when null is intentional.".to_string()
        }
    }

    fn finish_schema_mismatch_copy(&self) -> String {
        "The `finish` value did not match the required output schema. Correct it and call `finish(value)` again.".to_string()
    }

    fn invalid_cell_retry_copy(&self, error_text: &str) -> String {
        format!(
            "{error_text}\n\nReply again using exactly one paired `<typescript>...</typescript>` block."
        )
    }

    fn output_limit_cell_copy(&self, output_token_cap: Option<usize>) -> String {
        let cap = output_token_cap
            .map(|cap| format!(" The request cap was {cap} tokens."))
            .unwrap_or_default();
        format!(
            "Model output truncated the `<typescript>` block before `</typescript>`.{cap} Retry with a shorter block."
        )
    }

    fn code_stream_kind(&self) -> &'static str {
        "typescript_code"
    }

    fn execution_diagnostic_name(&self) -> &'static str {
        "execute_typescript"
    }

    fn stream_cell_start_event_name(&self) -> &'static str {
        "rlm_typescript_cell_start"
    }

    fn stream_cell_end_event_name(&self) -> &'static str {
        "rlm_typescript_cell_end"
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::projection::RlmProjectedBindings;
    use lash_core::ExecRequest;
    use lash_core::plugin::ToolCatalogContext;
    use lash_lashlang_runtime::{ToolBinding, ToolDefinitionBindingExt};

    /// Both shipped dialects, the way a session registers them.
    fn test_dialect_registry() -> crate::dialect::RlmDialectRegistry {
        crate::dialect::RlmDialectRegistry::new([
            std::sync::Arc::new(crate::dialect::lashlang_test_dialect())
                as std::sync::Arc<dyn crate::dialect::RlmDialect>,
            std::sync::Arc::new(crate::dialect::typescript_test_dialect()),
        ])
    }

    #[test]
    fn identity_and_cell_tags_are_typescript() {
        let dialect = TypescriptDialect::new(
            LashlangSurface::default(),
            LashlangDialectServices {
                projection_resolver: Arc::new(crate::projection::ProjectionRegistry::new()),
                artifact_store: lashlang::global_in_memory_lashlang_artifact_store(),
                deferred_tool_resolver: None,
                execution_trace_config: crate::executor::RlmLashlangExecutionTraceConfig::default(),
                execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
                channel: crate::plugin::RlmChannel::Cell,
            },
        );
        assert_eq!(dialect.language_id(), "typescript");
        assert_eq!(dialect.snapshot_engine_id(), "typescript");
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
        let dialect = TypescriptDialect::new(
            lash_lashlang_runtime::LashlangSurface {
                abilities: lashlang::LashlangAbilities::all(),
                language_features: Default::default(),
                resources,
            },
            LashlangDialectServices {
                projection_resolver: Arc::new(crate::projection::ProjectionRegistry::new()),
                artifact_store: lashlang::global_in_memory_lashlang_artifact_store(),
                deferred_tool_resolver: None,
                execution_trace_config: crate::executor::RlmLashlangExecutionTraceConfig::default(),
                execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
                channel: crate::plugin::RlmChannel::Cell,
            },
        );
        let section = dialect
            .render_execution_section(
                crate::protocol::RlmPromptFeatures::default(),
                &lash_core::ToolCatalog::from_tool_definitions(vec![]),
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
            section.contains("`cron.Schedule` can be passed to `registerTrigger`"),
            "the reader's own primitive name, not `trigger.register`: {section}"
        );
        assert!(section.contains("triggers.list"), "{section}");
        assert!(
            section.contains("interface Process<Input = unknown, Output = unknown>"),
            "{section}"
        );
        assert!(section.contains("Process<"), "{section}");
        assert!(!section.contains("ProcessDefinition"), "{section}");
        // And none of it may arrive in Lashlang's type syntax (ADR 0063).
        for leak in ["list[", "-> str", ": str`", "float`", "trigger.register"] {
            assert!(!section.contains(leak), "`{leak}` leaked: {section}");
        }
    }

    #[test]
    fn execution_section_renders_promise_tool_signatures_and_agent_contract() {
        let dialect = TypescriptDialect::new(
            LashlangSurface::default(),
            LashlangDialectServices {
                projection_resolver: Arc::new(crate::projection::ProjectionRegistry::new()),
                artifact_store: lashlang::global_in_memory_lashlang_artifact_store(),
                deferred_tool_resolver: None,
                execution_trace_config: crate::executor::RlmLashlangExecutionTraceConfig::default(),
                execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
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
            .render_execution_section(crate::protocol::RlmPromptFeatures::default(), &catalog)
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

    #[test]
    fn process_handle_interface_advertises_id_member() {
        let dialect = TypescriptDialect::new(
            LashlangSurface {
                abilities: lashlang::LashlangAbilities::all(),
                ..LashlangSurface::default()
            },
            LashlangDialectServices {
                projection_resolver: Arc::new(crate::projection::ProjectionRegistry::new()),
                artifact_store: lashlang::global_in_memory_lashlang_artifact_store(),
                deferred_tool_resolver: None,
                execution_trace_config: crate::executor::RlmLashlangExecutionTraceConfig::default(),
                execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
                channel: crate::plugin::RlmChannel::Cell,
            },
        );
        let prompt = dialect
            .render_execution_section(
                crate::protocol::RlmPromptFeatures::default(),
                &lash_core::ToolCatalog::from_tool_definitions(Vec::new()),
            )
            .expect("render execution section");
        assert!(
            prompt.contains("id: string"),
            "the ProcessHandle interface must advertise its `id` member: {prompt}"
        );
    }

    /// The prompt's `start` declaration must teach the calling convention the
    /// lowerer actually implements.
    ///
    /// `lower_start` passes the object's keys through verbatim as the process's
    /// parameter names, so the second argument is a named-parameter record, not
    /// an options bag with an `input` field. Declaring `{ input?: Input }` is
    /// true only when the run parameter happens to be called `input`, and false
    /// for the example this repo ships — a model that names its parameter for
    /// its domain, which is the natural thing to do, gets a link error the
    /// prompt gives it no way to read. This pins the rule to the behaviour
    /// rather than to the sentence, so the sentence cannot drift back.
    #[test]
    fn the_prompt_teaches_the_real_process_argument_convention() {
        let dialect = TypescriptDialect::new(
            LashlangSurface {
                abilities: lashlang::LashlangAbilities::all(),
                ..LashlangSurface::default()
            },
            LashlangDialectServices {
                projection_resolver: Arc::new(crate::projection::ProjectionRegistry::new()),
                artifact_store: lashlang::global_in_memory_lashlang_artifact_store(),
                deferred_tool_resolver: None,
                execution_trace_config: crate::executor::RlmLashlangExecutionTraceConfig::default(),
                execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
                channel: crate::plugin::RlmChannel::Cell,
            },
        );
        let prompt = dialect
            .render_execution_section(
                crate::protocol::RlmPromptFeatures::default(),
                &lash_core::ToolCatalog::from_tool_definitions(Vec::new()),
            )
            .expect("render execution section");
        assert!(
            !prompt.contains("args?: { input?: Input }"),
            "the declaration promises an options bag the lowerer does not accept: {prompt}"
        );
        assert!(
            prompt.contains("parameter name"),
            "the prompt must state that the keys are the run function's parameter names: {prompt}"
        );

        // The behaviour the sentence describes, asserted directly.
        let program = |key: &str| {
            format!(
                "const approval = defineProcess({{ name: \"approval\", signals: {{}},                  run: async (request: unknown) => {{ return request; }} }});                  const handle = start(approval, {{ {key}: 1 }}); finish(1);"
            )
        };
        let host = lashlang::LashlangHostEnvironment::new(
            lashlang::LashlangHostCatalog::default(),
            lashlang::LashlangAbilities::all(),
        );
        lash_typescript::link(&program("request"), &host)
            .expect("a key matching the run parameter links");
        let error = lash_typescript::link(&program("input"), &host)
            .expect_err("a key that is not a parameter name must reject");
        assert!(
            error
                .to_string()
                .contains("does not accept argument `input`"),
            "the rejection names the offending key: {error}"
        );
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
        let dialect = TypescriptDialect::new(
            LashlangSurface::default(),
            LashlangDialectServices {
                projection_resolver: Arc::new(crate::projection::ProjectionRegistry::new()),
                artifact_store: lashlang::global_in_memory_lashlang_artifact_store(),
                deferred_tool_resolver: None,
                execution_trace_config: crate::executor::RlmLashlangExecutionTraceConfig::default(),
                execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
                channel: crate::plugin::RlmChannel::Cell,
            },
        );
        let prompt = dialect
            .render_execution_section(
                crate::protocol::RlmPromptFeatures::default(),
                &lash_core::ToolCatalog::from_tool_definitions(Vec::new()),
            )
            .expect("render execution section");
        let real = lash_typescript::DiagnosticCode::ALL
            .iter()
            .map(|code| code.as_str())
            .collect::<std::collections::BTreeSet<_>>();

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

        let phantom = named
            .iter()
            .filter(|token| !real.contains(**token))
            .collect::<Vec<_>>();
        assert!(
            phantom.is_empty(),
            "the prompt names {phantom:?}, which the dialect cannot emit"
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
        let host = lashlang::LashlangHostEnvironment::new(
            lashlang::LashlangHostCatalog::default(),
            lashlang::LashlangAbilities::all(),
        );
        // Identifiers that exist only in Lashlang's surface. A model reading
        // the TypeScript prompt has never seen any of them.
        let lashlang_only = [
            "wait_signal",
            "signal_run",
            "define_process",
            "register_trigger",
            "__typescript_runtime",
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
                "defineProcess not at top level",
                "function f(): unknown { return defineProcess({ name: \"p\", signals: {}, run: async (a: unknown) => { return a; } }); } finish(f());",
            ),
            (
                "a non-literal process name",
                "const n = \"p\"; const p = defineProcess({ name: n, signals: {}, run: async (a: unknown) => { return a; } }); finish(1);",
            ),
            (
                "a start key that is not a run parameter",
                "const p = defineProcess({ name: \"p\", signals: {}, run: async (request: unknown) => { return request; } }); finish(start(p, { input: 1 }));",
            ),
            (
                "registerTrigger with a non-literal target",
                "const p = defineProcess({ name: \"p\", signals: {}, run: async (a: unknown) => { return a; } }); const t = p; finish(await registerTrigger({ source: 1, target: t, inputs: {} }));",
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

        let prompt = typescript_process_prompt(&lashlang::LashlangAbilities::all());
        assert!(prompt.contains("waitSignal is run-only"));
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
                let dialect = TypescriptDialect::new(
                    LashlangSurface::default(),
                    LashlangDialectServices {
                        projection_resolver: Arc::new(crate::projection::ProjectionRegistry::new()),
                        artifact_store: lashlang::global_in_memory_lashlang_artifact_store(),
                        deferred_tool_resolver: None,
                        execution_trace_config:
                            crate::executor::RlmLashlangExecutionTraceConfig::default(),
                        execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
                        channel: crate::plugin::RlmChannel::Cell,
                    },
                );
                let mut session = dialect.create_session().expect("typescript session");
                let response = session
                    .execute(
                        lash_core::testing::code_execution_context(),
                        ExecRequest {
                            language: "typescript".to_string(),
                            code: "const answer: number = 40 + 2; finish(answer);".to_string(),
                        },
                        RlmProjectedBindings::new(),
                    )
                    .await
                    .expect("execute typescript");

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

        let dialects = test_dialect_registry();
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
                    session_id: "session".to_string(),
                    tools: vec![tool.manifest()],
                    resolve_contract: None,
                    tool_access: lash_core::SessionToolAccess::default(),
                    subagent: None,
                    extensions: Default::default(),
                },
                &dialects,
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
        for expected in [
            "delete.op",
            "new.op",
            "Math.op",
            "probe.then",
            "probe.catch",
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
        let section = TypescriptDialect::prompt_only(LashlangSurface::default())
            .render_execution_section(crate::protocol::RlmPromptFeatures::default(), &catalog)
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

    /// Read each method signature from the actual catalogue text.
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
            ) -> Result<lashlang::AbilityResult, lashlang::ExecutionHostError> {
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
                        Ok(lashlang::AbilityResult::Value(lashlang::Value::String(
                            "tool-ok".into(),
                        )))
                    }
                    lashlang::AbilityOp::Finish(value) => Ok(lashlang::AbilityResult::Value(value)),
                    other => Err(lashlang::ExecutionHostError::new(format!(
                        "unexpected ability {other:?}"
                    ))),
                }
            }
        }

        let mut catalog = lashlang::LashlangHostCatalog::new();
        catalog
            .add_module_operation_binding(
                modules.to_vec(),
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
        let host = RecordingHost {
            dispatched: std::sync::Mutex::new(Vec::new()),
        };
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(lashlang::execute(
                &lash_typescript::compile_linked(&linked),
                &mut lashlang::State::new(),
                &host,
            ))
            .unwrap_or_else(|error| panic!("`{source}` must execute: {error:?}"));
        host.dispatched.lock().expect("dispatched lock").clone()
    }
}
