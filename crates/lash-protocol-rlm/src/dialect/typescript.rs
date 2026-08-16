use std::collections::BTreeSet;
use std::sync::Arc;

use lash_core::{ExecRequest, ExecResponse, RuntimeExecutionContext, SessionError};
use lash_lashlang_runtime::LashlangSurface;
use lash_rlm_types::RlmGlobalsPatchPluginBody;

use super::{
    BoundVariablesPromptRender, CellTags, LashlangDialectServices, RlmDialect, RlmDialectSession,
};
use crate::executor::{RlmExecutionState, execute_typescript_code_with_bounds};
use crate::projection::RlmProjectedBindings;
use crate::rlm_support::{BoundVariableRenderCache, render_bound_variables};

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
        continue_as_call: "control.continue_as(...)",
        continue_as_example: "await control.continue_as({ task: \"continue the audit from the summarized findings\", seed: { problem: input.prompt, findings: findings } });",
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
            "ProcessDefinition<{}, {}>",
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
        let mut section = String::from("\n\n### Host surface");
        if !operations.is_empty() {
            let lines = operations
                .iter()
                .map(|operation| {
                    format!(
                        "declare function {}_{}(input: {}): Promise<{}>; // await {}.{}(input)",
                        operation.alias,
                        operation.operation,
                        typescript_type(operation.input),
                        typescript_type(operation.output),
                        operation.alias,
                        operation.operation
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            section.push_str(&format!(
                "\n\nAwaited runtime operations, called as `await <module>.<operation>(input)`:\n\n```typescript\n{lines}\n```"
            ));
        }
        if !inventory.data_types.is_empty() {
            let lines = inventory
                .data_types
                .iter()
                .map(|(name, ty)| {
                    format!(
                        "// {name}\ntype {} = {};",
                        name.replace('.', "_"),
                        typescript_type(ty)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            section.push_str(&format!(
                "\n\nNamed host data types:\n\n```typescript\n{lines}\n```"
            ));
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
                .join("\n");
            section.push_str(&format!(
                "\n\nPure value constructors. Never `await` these; use them wherever an expression is allowed:\n\n```typescript\n{lines}\n```"
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

impl RlmDialect for TypescriptDialect {
    fn language_id(&self) -> &'static str {
        LANGUAGE_ID
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
        Ok(Box::new(TypescriptDialectSession {
            state: Some(RlmExecutionState::for_engine(LANGUAGE_ID)?),
            surface: self.surface.clone(),
            services: self.services.clone(),
            bound_variable_render_cache: Arc::new(std::sync::Mutex::new(
                BoundVariableRenderCache::default(),
            )),
        }))
    }

    fn render_execution_section(
        &self,
        _features: crate::protocol::RlmPromptFeatures,
        tool_catalog: &lash_core::ToolCatalog,
    ) -> Result<String, SessionError> {
        let tools = tool_catalog
            .tools
            .iter()
            .filter_map(|tool| {
                let binding =
                    lash_lashlang_runtime::required_tool_typescript_executable(&tool.manifest)
                        .ok()?;
                let contract = tool_catalog.resolve_contract(&tool.manifest.name)?;
                Some(lash_typescript::render_tool_signature(
                    &binding.call_path(),
                    contract.input_schema.canonical(),
                    Some(contract.output_schema.canonical()),
                ))
            })
            .collect::<Vec<_>>()
            .join("\n");
        let tools = if tools.is_empty() {
            "\n\nNo host tools are available in this turn.".to_string()
        } else {
            format!(
                "\n\n### Tools\n\nEvery call requires `await` and returns the declared `Promise<T>`:\n\n```typescript\n{tools}\n```"
            )
        };
        let host_surface = self.render_host_surface_section(tool_catalog)?;
        let host_api = r#"## TypeScript execution

Write one script inside standalone `<typescript>` and `</typescript>` lines. Top-level bindings persist across cells. `console.log(value)` inspects and continues; `finish(value)` is cell-only and ends the turn with a computed value. Never finish a raw tool dump: inspect it, then finish a concise result.

### Host API

```typescript
interface ProcessDefinition<Input, Output> { readonly name: string }
interface ProcessHandle<Output> extends PromiseLike<Output> {}
declare const console: {
  log(...values: unknown[]): void;
  warn(...values: unknown[]): void;
  error(...values: unknown[]): void;
  info(...values: unknown[]): void;
  debug(...values: unknown[]): void;
};
declare function print(value: unknown): void;
declare function finish(value: unknown): never;
declare function sleep(milliseconds: number): Promise<void>;
declare function waitSignal(name: string): Promise<unknown>; // inside a defineProcess run body only
declare function defineProcess<Input, Output>(config: { name: string; signals: Record<string, null>; run: (input: Input) => Promise<Output> }): ProcessDefinition<Input, Output>;
declare function start<Input, Output>(process: ProcessDefinition<Input, Output>, args?: Record<string, unknown>): ProcessHandle<Output>;
declare function wake(progress: unknown): void;
declare function wake(handle: ProcessHandle<unknown>, signal: string, payload: unknown): void;
declare function registerTrigger(config: { source: unknown; target: ProcessDefinition<unknown, unknown>; inputs: Record<string, unknown>; name?: string }): Promise<unknown>;
```

Declare durable work only as a top-level `const p = defineProcess({ name: "literal", signals: { signal: null }, run: async (...) => { ... } })`. The keys of `start`'s second argument are the `run` function's own parameter names, not a fixed `input` field — `run: async (request: unknown)` is started as `start(p, { request: value })`, and any other key rejects; `registerTrigger`'s `inputs` keys work the same way. `await start(...)` waits for its result; an un-awaited handle can be signalled. In `run`, `wake(value)` emits progress, `await waitSignal("literal")` and `await sleep(ms)` suspend durably, `return` succeeds after enclosing `finally` blocks, and an uncaught `throw` fails. `waitSignal` is the only primitive above that is scoped to a process body: outside one it is refused as "`waitSignal` can only be used inside a process body", while `await sleep(ms)` is also valid in a cell. `await registerTrigger(...)` requires a literal process target. `Promise.all`/`Promise.allSettled` accept top-level tool promises and resolved values; `Promise.all` reports the first-settled rejection (v1 waits for every leaf before reporting).

### v1 guardrails

Use ordinary modern TypeScript control flow and expression syntax: destructuring (including defaults/rest), optional chaining, spread, compound/update operators, `switch`, `do...while`, `for...in`, `for...of`, parameter defaults/rest, `var`, runtime enums, and const enums are supported. Async functions and arrows are supported wherever every awaited value is a tool call, `sleep`, or a process handle; fan out with `await Promise.all(items.map(async (item) => ...))` or its `Promise.allSettled` form, which runs the callbacks sequentially and durably. Error-family constructors, `new Map`/`Set`/`Date`/`RegExp`, and `new URL(input, base?)` / `new URLSearchParams(init?)` are supported; `instanceof` accepts exactly those built-ins plus `Array` and `Object`. A `URL`'s `searchParams` is one live object, so mutating it updates `href`. RegExp literals and `new RegExp(pattern?, flags?)` accept `gimsuy`; use `exec`/`test` or string `match`/`search`/`replace`/`replaceAll`/`split`, and consume `matchAll` directly with `for...of`, spread, or `Array.from`. Date math is UTC-only; use `getUTC*` and `toISOString()`. Bare conversions and number parsers are available; other iterators must be consumed directly by `for...of`, spread, `Array.from`, `new Map|Set`, or `Object.fromEntries`. `globalThis.name` and top-level bindings address the durable session state.

Classes (`TS_CLASS_UNSUPPORTED`), generators (`TS_GENERATOR_UNSUPPORTED`), namespaces (`TS_NAMESPACE_UNSUPPORTED`), decorators (`TS_DECORATOR_UNSUPPORTED`), `for await` (`TS_FOR_OF_UNSUPPORTED`), labels (`TS_LABEL_UNSUPPORTED`), arbitrary `new` (`TS_NEW_UNSUPPORTED`), and arbitrary `instanceof` (`TS_INSTANCEOF_UNSUPPORTED`) reject with a replacement in the diagnostic. A rejection names what it refused on its first line, points at the offending line and column, and — whenever the dialect has an accepted alternative — names it on a following `hint:` line. When a `hint:` line is present it is the rewrite; when it is absent the diagnostic itself is the whole answer. RegExp flags `d`/`v` reject as `TS_REGEX_INDICES_FLAG_UNSUPPORTED`/`TS_REGEX_UNICODE_SETS_FLAG_UNSUPPORTED`; remove `d` and use `match.index` plus capture lengths, or replace `v` with `u` and ordinary Unicode classes. A retained `matchAll` iterator rejects as `TS_REGEX_ITERATOR_POSITION`; spread it immediately. Assigning to a captured `let` rejects as `TS_MUTABLE_CAPTURE_UNSUPPORTED`; mutate a captured object's field instead. `for...of` snapshots its input, so a body that aliases or mutates that input rejects as `TS_FOR_OF_UNSUPPORTED`. Promise chaining and `Promise.resolve`/`reject` reject: use direct `await` and `try/catch`. `Promise.race`/`any` reject pending FIG-1416; use `Promise.all`, `Promise.allSettled`, or durable `sleep`. Unsupported methods reject as `TS_METHOD_UNSUPPORTED`. `localeCompare` and locale formatting reject; use `(a < b ? -1 : a > b ? 1 : 0)` and `toFixed(digits)`. `Date.now()` and argless `new Date()` use the same journaled clock effect; non-ISO parsing, local-time Date methods, and implicit Date string coercion reject with UTC/ISO repairs. `Math.random()` is journaled.

### Deterministic standard library"#;
        let stdlib = lash_typescript::render_stdlib_contract();
        Ok(format!("{host_api}\n\n{stdlib}{tools}{host_surface}"))
    }

    fn finalization_copy(&self, termination: &lash_rlm_types::RlmTermination) -> &'static str {
        match termination {
            lash_rlm_types::RlmTermination::FinishRequired { .. } => {
                "This turn requires a final value. Reply with one paired `<typescript>...</typescript>` block that calls `finish(value)`."
            }
            lash_rlm_types::RlmTermination::Natural => {
                "Continue with one paired `<typescript>...</typescript>` block, or finish with prose and no block. A call to `finish(value)` returns a computed final value."
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
            "Call `finish(value)` from one paired `<typescript>...</typescript>` block with a value matching the required output schema.".to_string()
        } else {
            "Call `finish(value)` from one paired `<typescript>...</typescript>` block.".to_string()
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

struct TypescriptDialectSession {
    state: Option<RlmExecutionState>,
    surface: LashlangSurface,
    services: LashlangDialectServices,
    bound_variable_render_cache: Arc<std::sync::Mutex<BoundVariableRenderCache>>,
}

impl TypescriptDialectSession {
    fn state(&self) -> Result<&RlmExecutionState, SessionError> {
        self.state
            .as_ref()
            .ok_or_else(|| SessionError::Protocol("RLM execution state is busy".to_string()))
    }

    fn state_mut(&mut self) -> Result<&mut RlmExecutionState, SessionError> {
        self.state
            .as_mut()
            .ok_or_else(|| SessionError::Protocol("RLM execution state is busy".to_string()))
    }
}

#[async_trait::async_trait]
impl RlmDialectSession for TypescriptDialectSession {
    async fn execute(
        &mut self,
        ctx: RuntimeExecutionContext<'_>,
        request: ExecRequest,
        session_projected_bindings: RlmProjectedBindings,
    ) -> Result<ExecResponse, SessionError> {
        if self.state.is_none() {
            return Err(SessionError::Protocol(
                "RLM execution state is busy".to_string(),
            ));
        }
        let reset_state = RlmExecutionState::for_engine(LANGUAGE_ID)?;
        let state = self
            .state
            .take()
            .ok_or_else(|| SessionError::Protocol("RLM execution state is busy".to_string()))?;
        let result = execute_typescript_code_with_bounds(
            state,
            ctx,
            request,
            Arc::clone(&self.services.artifact_store),
            self.surface.clone(),
            self.services.deferred_tool_resolver.clone(),
            session_projected_bindings,
            Arc::clone(&self.services.projection_resolver),
            self.services.execution_trace_config.clone(),
            self.services.execution_bounds.into_engine(),
        )
        .await;
        match result {
            Ok((state, response)) => {
                self.state = Some(state);
                Ok(response)
            }
            Err(error) => {
                self.state = Some(reset_state);
                Err(error)
            }
        }
    }

    fn execution_state_dirty(&self) -> bool {
        self.state
            .as_ref()
            .map(RlmExecutionState::execution_state_dirty)
            .unwrap_or(true)
    }

    fn snapshot_execution_state(
        &mut self,
    ) -> Result<lash_core::plugin::ExecutionStateSnapshot, SessionError> {
        self.state_mut()?.snapshot_execution_state()
    }

    fn probe_execution_state_capture(&mut self) -> Result<(), SessionError> {
        self.state_mut()?.probe_execution_state_capture()
    }

    fn hydrated_execution_state(
        &self,
    ) -> Result<lash_core::plugin::HydratedExecutionState, SessionError> {
        self.state()?.hydrated_execution_state()
    }

    fn acknowledge_execution_state_capture(&mut self) -> Result<(), SessionError> {
        self.state_mut()?.acknowledge_execution_state_capture();
        Ok(())
    }

    fn abort_execution_state_capture(&mut self) -> Result<(), SessionError> {
        self.state_mut()?.abort_execution_state_capture();
        Ok(())
    }

    fn restore_execution_state(
        &mut self,
        state: &lash_core::plugin::HydratedExecutionState,
    ) -> Result<(), SessionError> {
        self.state_mut()?
            .restore_execution_state(state)
            .map_err(|error| SessionError::Protocol(error.to_string()))
    }

    fn prune_protected_globals(
        &mut self,
        protected_names: &BTreeSet<String>,
    ) -> Result<(), SessionError> {
        self.state_mut()?.prune_protected_globals(protected_names);
        Ok(())
    }

    fn patch_globals(
        &mut self,
        patch: &RlmGlobalsPatchPluginBody,
        protected_names: &BTreeSet<String>,
    ) -> Result<(), SessionError> {
        self.state_mut()?.patch_globals(patch, protected_names)
    }

    fn prepare_bound_variables_prompt(
        &self,
        exclude: &BTreeSet<String>,
    ) -> Result<BoundVariablesPromptRender, SessionError> {
        let mut globals = self.state()?.bound_variable_values(exclude);
        // A block-scoped binding that shadows an outer name is lowered to a
        // generated slot. It is the author's value under a name the author
        // never wrote, and it is dead by the time any turn boundary renders,
        // so it is never a bound variable the model should see.
        globals.retain(|(name, _)| !name.starts_with(lash_typescript::GENERATED_BINDING_PREFIX));
        let cache = Arc::clone(&self.bound_variable_render_cache);
        Ok(BoundVariablesPromptRender::new(move || {
            let mut cache = cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            render_bound_variables(&mut cache, &globals, TYPESCRIPT_PROMPT_VOCABULARY)
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_lashlang_runtime::{LashlangToolBinding, ToolDefinitionLashlangExt};

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
    /// declared `cron.Schedule` and the `triggers.*` operations advertised them
    /// in its own prompt copy while the substrate told the model nothing. A
    /// judged row watched a model search for `cron.Schedule`, find nothing, and
    /// conclude the trigger APIs did not exist — a VOID row produced by a
    /// prompt that denied a capability the session actually had.
    #[test]
    fn the_execution_section_declares_the_hosts_trigger_surface() {
        let mut resources = lashlang::LashlangHostCatalog::new();
        lashlang::add_trigger_resource_operations(&mut resources);
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
            },
        );
        let section = dialect
            .render_execution_section(
                crate::protocol::RlmPromptFeatures::default(),
                &lash_core::ToolCatalog::from_tool_definitions(vec![]),
            )
            .expect("render execution section");

        assert!(section.contains("### Host surface"), "{section}");
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
        .with_lashlang_binding(LashlangToolBinding::new(["web"], "fetch"));
        let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![tool]);
        let section = dialect
            .render_execution_section(crate::protocol::RlmPromptFeatures::default(), &catalog)
            .expect("render execution section");
        assert!(
            section.contains(
                "declare namespace web { function fetch(input: { url: string }): Promise<string>; }"
            ),
            "{section}"
        );
        assert!(section.contains("defineProcess"), "{section}");
        assert!(section.contains("Promise.allSettled"), "{section}");
        assert!(
            section.contains("TS_MUTABLE_CAPTURE_UNSUPPORTED"),
            "{section}"
        );
        assert!(section.contains("TS_FOR_OF_UNSUPPORTED"), "{section}");
        assert!(
            section.contains(&lash_typescript::render_stdlib_contract()),
            "the prompt stdlib inventory must come from the lowering signature table"
        );
        insta::assert_snapshot!("typescript_execution_section", section);
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
            LashlangSurface::default(),
            LashlangDialectServices {
                projection_resolver: Arc::new(crate::projection::ProjectionRegistry::new()),
                artifact_store: lashlang::global_in_memory_lashlang_artifact_store(),
                deferred_tool_resolver: None,
                execution_trace_config: crate::executor::RlmLashlangExecutionTraceConfig::default(),
                execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
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
            named.len() >= 7,
            "the walker found only {named:?}; the prompt names more than that"
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

        // The prompt now states the scope rule the reject enforces, and states
        // that its neighbour is *not* scoped that way. Both halves are pinned
        // here, because a scope annotation that is wrong in the permissive
        // direction is worse than none.
        //
        // FIG-1398: assert the rendered prompt contains the exact refusal message
        // the linker emits rather than checking a hardcoded literal against `refusal`.
        // A prompt-sentence reword or diagnostic change turns this test red
        // immediately without relying on an insta snapshot.
        let refusal = lash_typescript::link("await waitSignal(\"go\");", &host)
            .expect_err("waitSignal outside a process body must reject");
        let prompt =
            TypescriptDialect::prompt_only(lash_lashlang_runtime::LashlangSurface::default())
                .render_execution_section(
                    crate::protocol::RlmPromptFeatures::default(),
                    &lash_core::ToolCatalog::default(),
                )
                .expect("typescript execution section");
        let expected_quote = format!("\"{}\"", refusal.message);
        assert!(
            prompt.contains(&expected_quote),
            "the rendered prompt must quote the linker refusal verbatim: expected {expected_quote} in prompt"
        );
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
                    },
                );
                let mut session = dialect.create_session().expect("typescript session");
                let response = session
                    .execute(
                        lash_core::testing::code_execution_context(),
                        ExecRequest {
                            language: "typescript".to_string(),
                            code: "const answer: number = 40 + 2; finish(answer);".to_string(),
                            accept_finish: true,
                        },
                        RlmProjectedBindings::new(),
                    )
                    .await
                    .expect("execute typescript");

                assert_eq!(response.error, None);
                assert_eq!(response.terminal_finish, Some(serde_json::json!(42)));
            });
    }
}
