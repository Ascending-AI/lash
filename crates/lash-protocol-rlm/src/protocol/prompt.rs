pub const LASHLANG_TYPE_LITERALS_SECTION: &str = r#"### Type literals

`Type { field: shape, ... }` describes a record shape. Field separators are commas (trailing comma OK).

- Scalars: `str`, `int`, `float`, `bool`, `dict`, `any`, `null`.
- Collections: `list[shape]`, `enum["a", "b"]`, nested `{ ... }` (or the equivalent explicit `Type { ... }`).
- **Optional field** — put `?` after the type: `email: str?` means the field may be absent from the record. If the field IS present, its value must be a string; `null` is **not** allowed.
- **Nullable field** — use a union with `null`: `email: str | null` means the field is required and its value is either a string or null.
- **Unions** — `a | b | c`, e.g. `status: str | int`, `value: str | null`.

    <lashlang>
    profile = validate(record, Type { name: str, email: str?, tags: list[str] })
    </lashlang>
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RlmPromptFeatures {
    pub images: bool,
    pub type_literals: bool,
    pub decomposition: bool,
}

impl Default for RlmPromptFeatures {
    fn default() -> Self {
        Self {
            images: true,
            type_literals: true,
            decomposition: true,
        }
    }
}

pub fn rlm_execution_section_for_host_environment(
    features: RlmPromptFeatures,
    surface: &lashlang::LashlangHostEnvironment,
) -> String {
    let has_operations = surface.resources.has_operations();
    let mut sections = Vec::new();
    sections.push(render_execution_intro(has_operations));
    sections.push(render_language_section(
        features.images,
        has_operations,
        &surface.abilities,
        &surface.language_features,
    ));
    if let Some(section) = render_host_environment_section(surface) {
        sections.push(section);
    }
    sections.push(render_builtins_section(features.images));
    if features.type_literals {
        sections.push(LASHLANG_TYPE_LITERALS_SECTION.to_string());
    }
    if features.decomposition {
        sections.push(render_decomposition_section(
            has_operations,
            surface.abilities.processes,
        ));
    }
    sections.join("\n\n")
}

/// Modules the substrate binds for its own lowering, never for a reader.
///
/// The leading double underscore is the repository's reserved-namespace marker
/// (the TypeScript lowerer's generated bindings share it), so this is a rule
/// about a namespace rather than a list of names to keep in sync. It exists
/// because `lashlang_host_environment_from_tool_catalog` binds
/// `__typescript_runtime` — how the TypeScript lowerer reaches journaled
/// `Date.now()`/`Math.random()` — into *every* Lashlang host, and this section
/// advertised it: a Lashlang reader was handed
/// `await __typescript_runtime.now(any)? -> float`, an internal name in another
/// dialect's vocabulary.
///
/// Hiding rather than renaming is deliberate. The module path is a durable
/// link-time identifier: it is embedded in every lowered TypeScript program,
/// including the persisted bodies of durable processes that must still resolve
/// when a worker wakes them after a restart. ADR 0063 records the rule and the
/// carve-out list.
fn module_is_runtime_internal(path: &[String]) -> bool {
    path.first()
        .is_some_and(|segment| segment.starts_with("__"))
}

/// The host surface a dialect has to describe, walked once.
///
/// Both dialects advertise the same inventory and spell it differently, so the
/// walk lives here and each dialect formats the rows. A TypeScript session used
/// to receive no inventory at all: its execution section rendered tool
/// signatures and nothing else, so the trigger sources, their event types and
/// the `triggers.*` operations were invisible — while the host prompt told the
/// model to use them. A judged row watched a model search for `cron.Schedule`,
/// find nothing, and conclude the trigger APIs did not exist.
pub(crate) struct HostSurfaceInventory<'a> {
    pub(crate) operations: Vec<HostSurfaceOperation<'a>>,
    pub(crate) data_types: Vec<(String, &'a lashlang::TypeExpr)>,
    pub(crate) constructors: Vec<HostSurfaceConstructor<'a>>,
    /// `(trigger source type, event type name)`.
    pub(crate) trigger_sources: Vec<(String, String)>,
}

pub(crate) struct HostSurfaceOperation<'a> {
    pub(crate) alias: String,
    pub(crate) operation: String,
    pub(crate) input: &'a lashlang::TypeExpr,
    pub(crate) output: &'a lashlang::TypeExpr,
}

pub(crate) struct HostSurfaceConstructor<'a> {
    pub(crate) path: String,
    pub(crate) input: &'a lashlang::TypeExpr,
    /// Already resolved to a nominal name (`TriggerSource<cron.Tick>`), which
    /// is spelled the same in both dialects.
    pub(crate) output: String,
}

impl HostSurfaceInventory<'_> {
    pub(crate) fn is_empty(&self) -> bool {
        self.operations.is_empty()
            && self.data_types.is_empty()
            && self.constructors.is_empty()
            && self.trigger_sources.is_empty()
    }
}

pub(crate) fn host_surface_inventory(
    surface: &lashlang::LashlangHostEnvironment,
) -> HostSurfaceInventory<'_> {
    // Operations with real Lashlang types (trigger and other host primitives)
    // are listed here. Tool-catalog operations are bridged with placeholder
    // `any` types and documented in full under **Tools**, so they are skipped to
    // avoid an uninformative `any -> any` duplicate of that section.
    let mut operations = Vec::new();
    for (_, module) in surface.resources.module_instances() {
        if module_is_runtime_internal(&module.path) {
            continue;
        }
        if let Some(resource_type) =
            surface
                .resources
                .resolve_alias(&lashlang::ResourceRefExpr::resolved(
                    module
                        .path
                        .iter()
                        .map(|segment| segment.as_str().into())
                        .collect(),
                    module.resource_type.clone(),
                    module.alias.clone(),
                ))
        {
            for (operation, binding) in &resource_type.operations {
                if matches!(binding.input_ty, lashlang::TypeExpr::Any)
                    && matches!(binding.output_ty, lashlang::TypeExpr::Any)
                {
                    continue;
                }
                operations.push(HostSurfaceOperation {
                    alias: module.alias.clone(),
                    operation: operation.clone(),
                    input: &binding.input_ty,
                    output: &binding.output_ty,
                });
            }
        }
    }
    let data_types = surface
        .resources
        .named_data_types()
        .map(|(_, data_type)| (data_type.name().to_string(), data_type.ty()))
        .collect();
    let constructors = surface
        .resources
        .value_constructors()
        .map(|(_, constructor)| {
            let output = match &constructor.output_ty {
                lashlang::TypeExpr::Ref(name) => surface
                    .resources
                    .resolve_trigger_source(name.as_str())
                    .map(|binding| format!("TriggerSource<{}>", binding.event_type_name()))
                    .unwrap_or_else(|| lashlang::format_type_expr(&constructor.output_ty)),
                other => lashlang::format_type_expr(other),
            };
            HostSurfaceConstructor {
                path: constructor.path.join("."),
                input: &constructor.input_ty,
                output,
            }
        })
        .collect();
    let trigger_sources = surface
        .resources
        .trigger_sources()
        .map(|(source_ty, binding)| (source_ty.to_string(), binding.event_type_name().to_string()))
        .collect();
    HostSurfaceInventory {
        operations,
        data_types,
        constructors,
        trigger_sources,
    }
}

fn render_host_environment_section(surface: &lashlang::LashlangHostEnvironment) -> Option<String> {
    let inventory = host_surface_inventory(surface);
    if inventory.is_empty() {
        return None;
    }
    let operation_lines = inventory
        .operations
        .iter()
        .map(|operation| {
            format!(
                "- `await {}.{}({})? -> {}`",
                operation.alias,
                operation.operation,
                lashlang::format_type_expr(operation.input),
                lashlang::format_type_expr(operation.output)
            )
        })
        .collect::<Vec<_>>();
    let data_type_lines = inventory
        .data_types
        .iter()
        .map(|(name, ty)| format!("- `type {} = {}`", name, lashlang::format_type_expr(ty)))
        .collect::<Vec<_>>();
    let constructor_lines = inventory
        .constructors
        .iter()
        .map(|constructor| {
            format!(
                "- `{}({}) -> {}`",
                constructor.path,
                lashlang::format_type_expr(constructor.input),
                constructor.output
            )
        })
        .collect::<Vec<_>>();
    let trigger_register = lashlang::TriggerHostOperation::Register.host_operation();
    let protocol_lines = inventory
        .trigger_sources
        .iter()
        .map(|(source_ty, event)| {
            format!("- `{source_ty}` can be passed to `{trigger_register}` and emits `{event}`")
        })
        .collect::<Vec<_>>();
    let mut section = String::from("### Host Surface");
    if !operation_lines.is_empty() {
        section.push_str("\n\nAwaited runtime operations:\n\n");
        section.push_str(&operation_lines.join("\n"));
    }
    if !data_type_lines.is_empty() {
        section.push_str("\n\nNamed host data types:\n\n");
        section.push_str(&data_type_lines.join("\n"));
    }
    if !constructor_lines.is_empty() {
        section.push_str("\n\nPure value constructors. Do not `await` these; use them wherever expressions are allowed:\n\n");
        section.push_str(&constructor_lines.join("\n"));
    }
    if !protocol_lines.is_empty() {
        section.push_str("\n\nTrigger source protocol metadata:\n\n");
        section.push_str(&protocol_lines.join("\n"));
    }
    Some(section)
}

fn render_execution_intro(has_operations: bool) -> String {
    let mut section = String::from("### Operating contract\n\n");
    if has_operations {
        section.push_str("Use plain prose only for direct conversational replies that need no action or computation. Use Lashlang when you need to call an available operation, inspect variables, compute values, edit, validate, or return structured/computed results. Invoke documented operations with module syntax — `await module.operation({ ... })?` — from inside a paired `<lashlang>` block, using the real names and signatures listed under **Tools**. If a discovery tool is available, use it to find operations beyond those listed.");
    } else {
        section.push_str("Use plain prose only for direct conversational replies that need no computation. Use Lashlang to compute values, inspect current variables, validate data, or return structured/computed results. No module operations are available in this turn, so do not invent tool calls.");
    }
    section.push_str(
        r#"

### `print` vs `finish`

- `print <expr>` — inspect a value and keep going; output appears on the next step. Print the part you need to decide the next step: a whole value when it is small and all of it is useful (e.g. state you consult each turn), otherwise selected fields, samples, or slices. Avoid dumping a large value when only part of it is relevant.
- `finish <expr>` — final answer; ends the turn. Strings pass through as the reply. Non-string values render as pretty JSON for machine consumers; for user-facing turns, follow the current final-answer format guidance. Keep the final value concise; do not include large variables such as diffs, full logs, raw command output, or other bulky dumps unless the user explicitly asks for them.

Never `finish` a raw tool-result dump. If you need to look at something, `print` it, then `finish` a summary on a later step.

Do not finish with final results that depend on operations, files, generated patches, or other current-state artifacts without inspecting first. Only `finish` once you have observed and verified the relevant results.

### Response shape

Executable code must be inside paired `<lashlang>` and `</lashlang>` tags. The start and close tag lines must be standalone after trimming. A standalone `</lashlang>` line terminates the cell even inside a multiline string, so construct such string content without that standalone delimiter line. When action is needed, place the Lashlang block after any visible prose or omit prose. Any turn-ending rules for prose-only responses versus `finish` are listed in the current **FINALIZATION** section.
"#,
    );
    section
}

fn render_language_section(
    images: bool,
    has_operations: bool,
    abilities: &lashlang::LashlangAbilities,
    language_features: &lashlang::LashlangLanguageFeatures,
) -> String {
    let mut bullets = Vec::new();
    push_value_language_bullets(&mut bullets, images);
    bullets.push(strings_language_bullet());
    bullets.push(operator_language_bullet());
    bullets.push(assignment_language_bullet());
    bullets.push(list_comprehension_language_bullet());
    if has_operations {
        bullets.push(module_operations_language_bullet());
    }
    if abilities.sleep {
        bullets.push(sleep_language_bullet());
    }
    if abilities.processes {
        push_process_language_bullets(&mut bullets, abilities);
    }
    if language_features.label_annotations {
        bullets.push(label_annotations_language_bullet());
    }
    if abilities.triggers && abilities.processes {
        bullets.push(trigger_registry_language_bullet());
    }
    if has_operations {
        bullets.push(operation_scheduling_language_bullet(abilities.processes));
    }
    bullets.extend(base_tail_language_bullets());
    format!("### Language\n\n{}", bullets.join("\n"))
}

fn push_value_language_bullets(bullets: &mut Vec<String>, images: bool) {
    if images {
        bullets.push("- Values: null, booleans, numbers, strings, lists, records, and immutable `Image` handles. Literals: `[a, b]`, `{ a: 1, b: 2 }`.".to_string());
        bullets.push("- Images: image-producing tools may return an `Image` value. Read metadata with `.id`, `.label`, `.size`, `.width`, `.height`; fields are read-only. `print(image)` or `print` on a list/record containing images sends both descriptor text and the actual image attachment to the next model call. `finish image`, `to_string(image)`, and JSON-like serialization emit only `{ \"type\": \"image\", \"id\": ..., \"label\": ..., \"size\": ..., \"width\": ..., \"height\": ... }`. `len(image)` is invalid; use `.size`.".to_string());
    } else {
        bullets.push("- Values: null, booleans, numbers, strings, lists, and records. Literals: `[a, b]`, `{ a: 1, b: 2 }`.".to_string());
    }
}

fn strings_language_bullet() -> String {
    "- Strings: `\"...\"` and `'...'` support `\\n`, `\\r`, `\\t`, escaped matching quotes, and `\\\\`; `\"\"\"...\"\"\"` and `'''...'''` are multiline with the same escapes. Raw strings `r\"...\"`, `r'...'`, `r\"\"\"...\"\"\"`, and `r'''...'''` preserve content exactly. Use raw strings for JSON, Markdown, patches, shell scripts, and other payloads with braces, backslashes, quotes, heredocs, or `@@` hunk markers. Use `format(...)` for interpolation; there are no f-strings.".to_string()
}

fn operator_language_bullet() -> String {
    "- Operators, tightest to loosest: postfix calls/fields/indexing/result `?`; unary `-`/`!`/`not`; `*`/`/`/`%`; `+`/`-`; comparisons `== != < <= > >= in`; `and`/`&&`; `or`/`||`; ternary `? :`. `x in y` tests list/tuple membership, record keys, and string substrings like Python; negate with `!(x in y)` — there is no `not in`. When an `any`-typed operand resolves to an unsupported membership pair at runtime, Lashlang reports a typed error (except `null` haystacks, which return false).".to_string()
}

fn assignment_language_bullet() -> String {
    "- Assign with `name = expr`. Variables persist across `<lashlang>` blocks within the turn. You can also update mutable collection paths rooted at a variable: `record.field = value`, `record[key] = value`, `list[i] = value`, and nested forms like `state.groups[g].count = count + 1`. Record field/index assignment inserts or replaces fields; list assignment replaces an existing integer index only. Record indexing reads dynamic string-coerced keys and returns `null` when missing, so histogram code can use `counts[g] = counts[g] + 1`.".to_string()
}

fn list_comprehension_language_bullet() -> String {
    "- List comprehensions: `[expr for name in iterable]` and `[expr for name in iterable if condition]`; multiple `for`/`if` clauses run left-to-right like Python. Comprehension bindings are local and do not overwrite outer variables. Use explicit loops when you need mutation, `break`, or `continue`.".to_string()
}

fn module_operations_language_bullet() -> String {
    "- Module operations: call host capabilities through documented lowercase module paths, e.g. `await module.operation({ field: value })?` — the real names and signatures are under **Tools**. Host operations are awaited effects; pure UpperCamel value constructors shown in the Host Surface are ordinary expressions and must not be awaited. Bare calls are builtins only, not tools — do not assume a tool exists from generic syntax; call only operations documented under **Tools**. `?` aborts the block with sanitized operation metadata if the operation fails.".to_string()
}

fn sleep_language_bullet() -> String {
    "- Sleep: pause foreground code or process code with `sleep for \"5s\"` or `sleep until deadline`. Durations accept milliseconds, `ms`, `s`, `m`, or `h`; deadlines accept RFC3339 text or Unix epoch milliseconds.".to_string()
}

fn push_process_language_bullets(
    bullets: &mut Vec<String>,
    abilities: &lashlang::LashlangAbilities,
) {
    let mut forms = vec![
        "`yield value`",
        "`wake value`",
        "`finish value`",
        "`fail value`",
    ];
    if abilities.process_signals {
        forms.push("`payload = wait_signal(\"name\")`");
    }
    let trigger_process_note = if abilities.triggers {
        " matching trigger occurrences can also create process runs through registered triggers."
    } else {
        ""
    };
    let signal_declaration_note = if abilities.process_signals {
        " Add typed inbound signals with `signals { name: TYPE }` when the process receives external messages, e.g. `process worker() signals { approve: { ok: bool } } { payload = wait_signal(\"approve\") finish payload }`."
    } else {
        ""
    };
    bullets.push(format!(
        "- Background processes: `process name(param: TYPE) {{ ... }}` declares a reusable process definition.{signal_declaration_note} `handle = start name(param: value)` creates one process run from that definition and returns its run handle;{trigger_process_note} For account-parametric work, pass typed module authorities explicitly, e.g. `process notify(mail: Gmail) {{ await mail.send({{ body: body }})? finish true }}` and `start notify(mail: gmail.work)`. For one-off concrete automations, a process body may reference concrete host paths such as `agents`, `web`, or `gmail.work` directly; params and locals shadow those captures. Inside a process use {}. `wake value` emits a `process.wake` event that notifies the agent/session with `value`; use it when process progress, trigger output, or other background work should re-enter the model as context. `finish value` completes the run and stores `value` as the process success value. `fail value` completes it as failed; falling off the end is `finish null`. `print` is foreground-only and invalid inside processes. Parallelism comes from starting all independent process handles before waiting for any of them; join a list or record of handles with `results = await handles`. `await handle` waits and returns a result wrapper like `{{ ok: true, value: ... }}`; when you need fields from the `finish` value, use `result = (await handle)?` and then read `result.field`. Cancel a live run with `cancel handle` (best-effort). If the Host Surface includes `processes.list`, use `await processes.list({{}})?` for running runs, `await processes.list({{ definition: name }})?` for runs of a definition, and `await processes.list({{ status: \"any\" }})?` for visible run history.",
        join_words(&forms),
    ));
    if abilities.process_signals {
        bullets.push("- Signalling processes: `signal_run(handle, \"name\", payload)` sends a typed `signal.name` event to a running process and may be used from the foreground turn as well as inside a process body, like `await handle` and `cancel handle`. The receiving side, `payload = wait_signal(\"name\")`, parks a process until that named signal arrives and is only valid inside a process body.".to_string());
    }
}

fn label_annotations_language_bullet() -> String {
    "- Execution labels: `@label(title: \"Label\")` or `@label(title: \"Label\", description: \"Details\")` names important Lashlang phases and graph steps. It is a prefix annotation, not a standalone statement; it must appear immediately before the one statement or process declaration it labels, e.g. `@label(title: \"Read files\")\\npaths = await files.glob({ pattern: \"**/*.rs\" })?`. Do not emit `@label(...)` by itself or stack multiple labels before one statement. At top level, label meaningful setup, resource calls, submissions, branches, loops, or process declarations. Inside a `process` body, label durable steps such as awaited module calls, `start`, `sleep`, `wait_signal`, `signal_run`, `wake`, `yield`, `finish`, `fail`, `if`, loops, and setup statements that explain the process. Titles/descriptions must be string literals; do not use variables, interpolation, icons, colors, layout hints, or extra keys.".to_string()
}

fn trigger_registry_language_bullet() -> String {
    let trigger_register = lashlang::TriggerHostOperation::Register.host_operation();
    let trigger_list = lashlang::TriggerHostOperation::List.host_operation();
    let trigger_disable = lashlang::TriggerHostOperation::Disable.host_operation();
    let trigger_enable = lashlang::TriggerHostOperation::Enable.host_operation();
    let trigger_delete = lashlang::TriggerHostOperation::Delete.host_operation();
    format!(
        "- Trigger registry: a trigger registration connects a typed source value to a process definition plus explicit inputs. Register with `receipt = await {trigger_register}({{ source: source, target: daily_digest, inputs: {{ tick: trigger.event }}, name: \"daily_digest\", subscription_key: \"daily-digest\" }})?`. `subscription_key` is a stable reference key within the caller's owner scope; supply one explicitly or let the linker derive the default. Registering a different definition at an existing key conflicts instead of updating it. Constructors build source values; the host/plugin that owns the source lists stored subscriptions by source type/key and emits trigger occurrences when source-specific events happen. `target` is a process definition value. `inputs` is required and maps every process param exactly once. `trigger.event` is the direct whole-event value inside `inputs`; fixed inputs can pass concrete authorities like `gmail.work` or `agents` for account-parametric processes. Use `await {trigger_list}({{}})?` to discover visible registrations, or filter with `{{ target: daily_digest }}`, `{{ name: \"daily_digest\" }}`, `{{ source_type: \"cron.Schedule\" }}`, and `{{ enabled: true }}`. Each listed registration includes registrant provenance plus `manifest_membership`: `present_in_current_artifact` or `orphaned` when the active compiled artifact no longer declares its key. Reconcile warnings never delete subscriptions. Remove reviewed orphans explicitly with `await triggers.prune({{ subscription_keys: [\"old-key\"] }})?`; prune is restricted to the acting owner namespace. Mutations are revision-checked: use `await {trigger_disable}({{ subscription_key: receipt.subscription_key, expected_revision: receipt.revision }})?`, `{trigger_enable}` to resume future deliveries, or `{trigger_delete}` to tombstone the subscription."
    )
}

fn operation_scheduling_language_bullet(processes: bool) -> String {
    let process_note = if processes {
        " Process parallelism still comes from starting all independent process handles before joining them with `await handles` or `await { key: handle }`."
    } else {
        ""
    };
    format!(
        "- Operation scheduling: consecutive `await module.op(...)` statements run one at a time. For independent module operations, use aggregate await: `results = await {{ first: module.a({{ ... }}), second: module.b({{ ... }}), label: \"kept\" }}`; direct receiver-call leaves in nested lists/records fan out through the host scheduler, pure value leaves are preserved, and results keep the same shape. Put `?` on each operation leaf you want unwrapped, e.g. `first: module.a({{ ... }})?`; all siblings finish before the first source-order failure is reported.{process_note}"
    )
}

fn base_tail_language_bullets() -> [String; 3] {
    [
        "- Control flow: statement `if`/`for`/`while`; `break` exits the nearest loop; `continue` skips to the nearest loop's next iteration; expression ternary `cond ? yes : no` (there is no expression-form `if`); boolean negation via `!cond` or `not cond`. Prefer bounded `while` loops where possible and bounded `for` loops over ranges/lists for fill or retry logic. `finish` is different from `break`: it ends the whole program/turn.".to_string(),
        "- Bare expressions are valid statements in normal blocks.".to_string(),
        "- Values already in scope are listed under **Bound Variables** (plus `history`) — use them directly in lashlang, don't recreate them.".to_string(),
    ]
}

fn render_builtins_section(images: bool) -> String {
    let builtins = lashlang::builtin_names()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let bullets = lashlang::builtin_names()
        .map(|name| builtin_prompt_bullet(name, images))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "### Builtins\n\nAvailable builtins (generated from the runtime registry): {builtins}.\n\nCall them as functions.\n\n{bullets}\n\nRegex and date/time operations are deliberately host tools, not builtins.\n\nExample using stable record sorting and membership:\n\n<lashlang>\nrows = [{{ name: \"first\", score: 2 }}, {{ name: \"second\", score: 1 }}]\nranked = sort_by(rows, \"score\")\nfinish ranked[0].name in [\"second\", \"third\"] ? ranked : reverse(ranked)\n</lashlang>\n"
    )
}

fn builtin_prompt_bullet(name: &str, images: bool) -> String {
    let detail = match name {
        "len" if images => {
            "`len(x)` — length of a string, tuple, list, or record (`0` for `null`); invalid for images, so use `image.size`"
        }
        "len" => "`len(x)` — length of a string, tuple, list, or record (`0` for `null`)",
        "empty" => "`empty(x)` — true when a string, tuple, list, record, or `null` has length `0`",
        "keys" => "`keys(record)` — record keys as a new list (`[]` for `null`)",
        "values" => "`values(record)` — record values as a new list (`[]` for `null`)",
        "trim" => "`trim(s)` — strip surrounding whitespace",
        "to_string" => "`to_string(x)` — convert a value to text",
        "to_int" => "`to_int(x)` — convert a numeric value or numeric string to an integer",
        "to_float" => "`to_float(x)` — convert a numeric value or numeric string to a float",
        "json_parse" => "`json_parse(s)` — parse a JSON string into a value",
        "contains" => {
            "`contains(haystack, needle)` — substring, tuple/list item, or record-key membership"
        }
        "grep_text" => {
            "`grep_text(s, needle)` — literal in-memory line search; `needle` must be non-empty; returns one record per matching line: `{ line: int, text: str, match: str, start: int, end: int }`, with 1-based lines and zero-based character offsets (`end` exclusive)"
        }
        "starts_with" => "`starts_with(s, prefix)` — true when text starts with the literal prefix",
        "ends_with" => "`ends_with(s, suffix)` — true when text ends with the literal suffix",
        "split" => "`split(s, sep)` — split text on a literal separator into a list",
        "join" => "`join(list, sep)` — join a tuple/list of values with a text separator",
        "validate" => {
            "`validate(value, Type { ... })` — return a value unchanged when it matches the type literal, otherwise abort with a typed validation error"
        }
        "ceil_div" => {
            "`ceil_div(a, b)` — ceiling integer division for chunk/count math; divisor must not be `0`"
        }
        "floor_div" => {
            "`floor_div(a, b)` — floor integer division for chunk/count math; divisor must not be `0`"
        }
        "push" => {
            "`push(list, item)` — return a new list with one item appended; the input is not mutated"
        }
        "slice" => {
            "`slice(s, start, end)` — substring or sublist; `null` opens either bound and negative bounds count from the end"
        }
        "find" => {
            "`find(s, needle, start?)` — zero-based character index of the first literal match, or `null`; `start` defaults to `0` and must be non-negative; an empty needle returns an in-bounds `start`"
        }
        "format" => {
            "`format(template, arg0, arg1, ...)` — positional interpolation: `{}` auto-numbers, `{0}` selects an arg, and `{{`/`}}` escape braces. Do not wrap args in a list: use `format(\"It is {}.\", trim(value))`, not `format(\"It is {}.\", [trim(value)])`"
        }
        "range" => {
            "`range(end)` / `range(start, end)` / `range(start, end, step)` — end-exclusive integer list; `step` may be positive or negative but never `0`"
        }
        "sort" => {
            "`sort(list)` — stable ascending sort of one comparable item type, returning a new list"
        }
        "sort_by" => {
            "`sort_by(list, \"field.path\")` — stable ascending record sort by a required non-empty dotted field path, returning a new list"
        }
        "sum" => "`sum(list)` — numeric total; `sum([])` is `0`",
        "min" => {
            "`min(list)` — least item from a non-empty list of one comparable type; an empty list is a typed runtime error"
        }
        "max" => {
            "`max(list)` — greatest item from a non-empty list of one comparable type; an empty list is a typed runtime error"
        }
        "replace" => {
            "`replace(s, from, to)` — literal Rust-style text replacement; an empty `from` inserts `to` at every UTF-8 boundary, including both ends"
        }
        "lower" => "`lower(s)` — Unicode lowercase conversion",
        "upper" => "`upper(s)` — Unicode uppercase conversion",
        "unique" => {
            "`unique(list)` — stable typed-equality deduplication that keeps first occurrences, returning a new list"
        }
        "reverse" => "`reverse(list)` — reverse a list or tuple into a new list",
        _ => panic!("builtin `{name}` is missing its prompt contract"),
    };
    format!("- {detail}")
}

fn render_decomposition_section(has_operations: bool, processes: bool) -> String {
    let mut section = String::from(
        "### Working with context\n\nYour turn's REPL trace is your working memory — keep it decision-sized and current. Large transient artifacts (files, search results, long pages, raw tool dumps) should stay in variables until you need a focused view; small durable state you consult each turn should stay visible.",
    );
    if has_operations {
        section.push_str(" Tool-specific lifecycle and output details live under **Tools**.");
    }
    section.push_str(
        "\n\nTruncation in the transcript is a display window, not lost state. A value shown truncated or summarized still has its full contents live — in the bound variable that holds it, and re-printable via its `history[N].output[M]` handle. A short preview never means the data is gone; `print` the variable or the slice you need and keep going. Never restart or abandon a task because earlier output looks truncated.",
    );
    section.push_str(
        "\n\nChoose the lightest mechanism that preserves progress:\n\n- Current variables already hold what you need -> reason inline in lashlang.",
    );
    if has_operations {
        if processes {
            section.push_str("\n- Several independent slow operations are needed -> use aggregate await over a record/list of direct module calls plus any pure values you want preserved, putting `?` on each operation leaf that should unwrap.");
        } else {
            section.push_str("\n- Several independent operations are needed -> use aggregate await over a record/list of direct module calls plus any pure values you want preserved, putting `?` on each operation leaf that should unwrap.");
        }
    }
    section.push_str("\n- The trace is bloated, stale, or failed attempts dominate -> use an available continuation tool to switch to a fresh AgentFrame with concrete state.");
    if has_operations && processes {
        section.push_str("\n- Anything tool-specific (parameters, return shapes, lifecycle) lives under **Tools**.\n\nExample parallel fan-out around an available operation (aggregate await preserves the record shape; use `?` on each leaf to unwrap it):\n\n    <lashlang>\n    results = await {\n      one: module.operation({ query: \"one\" })?,\n      two: module.operation({ query: \"two\" })?\n    }\n    finish format(\"First result: {}\\n\\nSecond result: {}\", slice(to_string(results.one), 0, 800), slice(to_string(results.two), 0, 800))\n    </lashlang>");
    } else if has_operations {
        section.push_str("\n- Anything tool-specific (parameters, return shapes, lifecycle) lives under **Tools**.");
    } else {
        section.push_str("\n- No module operations are available in this turn — don't infer one exists from generic lashlang syntax.");
    }
    section
}

fn join_words(words: &[&str]) -> String {
    match words {
        [] => String::new(),
        [one] => (*one).to_string(),
        [one, two] => format!("{one} or {two}"),
        _ => {
            let mut out = words[..words.len() - 1].join(", ");
            out.push_str(", or ");
            out.push_str(words[words.len() - 1]);
            out
        }
    }
}
