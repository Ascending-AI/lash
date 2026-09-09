pub const LASHLANG_TYPE_LITERALS_SECTION: &str = r#"### Type literals

`Type { field: shape, ... }` describes a record; commas separate fields and a trailing comma is allowed.

- Scalars: `str`, `int`, `float`, `bool`, `dict`, `any`, `null`.
- Collections: `list[shape]`, `enum["a", "b"]`; nest records with `{ ... }` or `Type { ... }`.
- `email: str?` permits an absent field; a present value must be a string, never null.
- `email: str | null` requires the field and permits a string or null.
- Unions: `a | b | c`, for example `status: str | int`.

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
    render_execution_for_catalog(features, surface, &[])
}

pub(crate) fn render_execution_for_catalog(
    features: RlmPromptFeatures,
    surface: &lashlang::LashlangHostEnvironment,
    documented_tools: &[String],
) -> String {
    let has_operations = surface.resources.has_operations();
    let inventory = host_surface_inventory(surface);
    let mut sections = Vec::new();
    let mut intro = render_execution_intro(has_operations);
    let host_section = render_host_environment_section(surface, documented_tools);
    if host_section.is_some() {
        intro = intro.replace("under **Tools**.", "under **Tools** or **Host Surface**.");
    }
    sections.push(intro);
    sections.push(render_language_section(
        features.images,
        has_operations,
        &surface.abilities,
        &surface.language_features,
        !inventory.constructors.is_empty(),
    ));
    if let Some(section) = host_section {
        sections.push(section);
    }
    sections.push(render_builtins_section(
        features.images,
        features.type_literals,
    ));
    if features.type_literals {
        sections.push(LASHLANG_TYPE_LITERALS_SECTION.to_string());
    }
    sections.push(render_decomposition_section(
        has_operations,
        surface.abilities.processes,
        features.decomposition,
    ));
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

fn render_host_environment_section(
    surface: &lashlang::LashlangHostEnvironment,
    documented_tools: &[String],
) -> Option<String> {
    let mut inventory = host_surface_inventory(surface);
    inventory.operations.retain(|operation| {
        !documented_tools.contains(&format!("{}.{}", operation.alias, operation.operation))
    });
    if inventory.is_empty() {
        return None;
    }
    let operation_lines = inventory
        .operations
        .iter()
        .map(|operation| {
            format!(
                "- `await {}.{}({})? -> {}`\n{}",
                operation.alias,
                operation.operation,
                lashlang::format_type_expr(operation.input),
                lashlang::format_type_expr(operation.output),
                host_operation_description(&operation.alias, &operation.operation).unwrap_or("")
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
    let mut section = String::new();
    if has_operations {
        section.push_str("Use prose for conversation; use a paired `<lashlang>` block for action or computation. Call tools as `await module.operation({ ... })?`, only those listed under **Tools**.");
    } else {
        section.push_str("Use plain prose only for direct conversational replies that need no computation. Use Lashlang to compute values, inspect current variables, validate data, or return structured/computed results. No module operations are available in this turn, so do not invent tool calls.");
    }
    section.push_str(
        r#"

### `print` vs `finish`

- `print <expr>` shows the value in the next step and continues; print the field or slice you need, not whole results.
- `finish <expr>` ends the turn: strings pass through, other values render as JSON. Return exactly the value and type the task asks for; do not finish an unexamined whole tool result.

"#,
    );
    section.push_str(&crate::dialect::cell_response_shape(
        crate::dialect::CellTags {
            open: "<lashlang>",
            close: "</lashlang>",
        },
        crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY,
    ));
    section
}

fn render_language_section(
    images: bool,
    has_operations: bool,
    abilities: &lashlang::LashlangAbilities,
    language_features: &lashlang::LashlangLanguageFeatures,
    constructors: bool,
) -> String {
    let mut bullets = Vec::new();
    push_value_language_bullets(&mut bullets, images);
    bullets.push(strings_language_bullet());
    bullets.push(operator_language_bullet());
    bullets.push(assignment_language_bullet());
    bullets.push(list_comprehension_language_bullet());
    bullets.push(functions_language_bullet());
    if has_operations {
        bullets.push(module_operations_language_bullet(constructors));
    }
    if abilities.sleep {
        bullets.push(sleep_language_bullet().replace(
            "foreground or process code",
            if abilities.processes {
                "foreground or process code"
            } else {
                "foreground code"
            },
        ));
    }
    if abilities.processes {
        push_process_language_bullets(&mut bullets, abilities);
    }
    if language_features.label_annotations {
        bullets.push(label_annotations_language_bullet(abilities));
    }
    if abilities.triggers && abilities.processes {
        bullets.push(trigger_registry_language_bullet());
    }
    bullets.push(operation_scheduling_language_bullet(abilities.processes));
    bullets.extend(base_tail_language_bullets());
    format!("### Language\n\n{}", bullets.join("\n"))
}

fn push_value_language_bullets(bullets: &mut Vec<String>, images: bool) {
    if images {
        bullets.push("- Values: null, booleans, numbers, strings, lists, records, and immutable `Image` handles. Literals: `[a, b]`, `{ a: 1, b: 2 }`.".to_string());
        bullets.push("- Images: image-producing tools may return an `Image` value. Read metadata with `.id`, `.label`, `.size`, `.width`, `.height`; fields are read-only. `print(image)` or `print` on a list/record containing images sends both descriptor text and the actual image attachment to the next model call. `finish image`, `to_string(image)` and JSON serialize as `{ type: \"image\", id, label, size, width, height }`. `len(image)` is invalid; use `.size`.".to_string());
    } else {
        bullets.push("- Values: null, booleans, numbers, strings, lists, and records. Literals: `[a, b]`, `{ a: 1, b: 2 }`.".to_string());
    }
}

fn strings_language_bullet() -> String {
    r#"- Strings: single/double quotes and triple-quoted multiline forms support `\n`, `\r`, `\t`, escaped quotes and `\\`. Prefix any with `r` for raw text (JSON, patches, shell payloads). Interpolate with `format`, not f-strings."#.to_string()
}

fn operator_language_bullet() -> String {
    r#"- Precedence: postfix calls/fields/indexing/result `?`; unary `-`/`!`/`not`; `* / %`; `+ -`; comparisons `== != < <= > >= in`; `and`/`&&`; `or`/`||`; ternary `cond ? a : b`. Postfix `?` after a call unwraps a tool result; `? :` is the conditional. `in` tests list membership, record keys or substrings; negate with `!(x in y)`, never `not in`. Null haystacks return false."#.to_string()
}

fn assignment_language_bullet() -> String {
    r#"- `name = expr` persists across `<lashlang>` blocks. Update paths: `record.field = v`, `record[key] = v`, `list[i] = v`, and nested paths. Record writes insert/replace; lists require existing indices. Record keys stringify; missing reads return null, so `counts[g] = counts[g] + 1` works. Do not name variables `start` or `sleep` (they begin statements); use `start_idx`."#.to_string()
}

fn list_comprehension_language_bullet() -> String {
    r#"- Comprehensions: `[expr for x in xs if cond]`; multiple for/if clauses execute left-to-right. Bindings are local. Use loops for mutation, break or continue."#.to_string()
}

fn functions_language_bullet() -> String {
    r#"- Pure functions: `fn f(x: type) -> type { body }`; types required; last expression returns; parameters-only scope. Call `f(arg)`; recursion and forward calls work. Arithmetic yields float: use `-> float`, not int. Keep effects outside functions and pass results in."#.to_string()
}

fn module_operations_language_bullet(constructors: bool) -> String {
    let mut text = r#"- Tools: pass the documented argument record, `{}` when it is empty. `?` unwraps the result or aborts the execution on failure; omit it only to inspect the result wrapper. Builtins and declared `fn`s are called directly."#.to_string();
    if constructors {
        text.push_str(" UpperCamel host constructors are pure: never `await` them.");
    }
    text
}

fn sleep_language_bullet() -> String {
    r#"- Sleep: `sleep for "5s"` or `sleep until deadline` pauses foreground or process code. Durations: milliseconds or `ms`/`s`/`m`/`h`; deadlines: RFC3339 text or Unix epoch milliseconds."#.to_string()
}

fn push_process_language_bullets(
    bullets: &mut Vec<String>,
    abilities: &lashlang::LashlangAbilities,
) {
    bullets.push(r#"- Processes: `process name(p: T) { … }` declares a definition; `h = start name(p: v)` starts a run and returns its handle. Pass what the body needs as typed parameters, including module authorities: `process notify(mail: Gmail, body: str) { await mail.send({ body: body })? finish true }`, then `start notify(mail: gmail.work, body: "Hello")`."#.into());
    bullets.push(r#"- Inside a process: `yield value` reports progress, `wake value` re-enters the model with `value`, `finish value` / `fail value` complete the run (falling off the end is `finish null`). `print` is foreground-only. Start all independent runs first, then join: `results = await [h1, h2]`; `(await h)?` unwraps the `{ ok, value }` wrapper. `cancel h` is best-effort."#.into());
    if abilities.process_signals {
        bullets.push(r#"- Signals: declare inbound payloads with `process worker() signals { approve: { ok: bool } } { payload = wait_signal("approve") finish payload }`; `signal_run(h, "approve", { ok: true })` sends from foreground or process code; `wait_signal` is process-only."#.into());
    }
}

fn label_annotations_language_bullet(_abilities: &lashlang::LashlangAbilities) -> String {
    r#"- `@label(title: "…")` (optional `description: "…"`) goes on the line before the one top-level statement it names: setup, tool calls, submissions, branches, loops. String literals only; never standalone or stacked."#.to_string()
}

fn trigger_registry_language_bullet() -> String {
    r#"- Triggers: `receipt = await triggers.register({ source: source, target: daily_digest, inputs: { tick: trigger.event }, name: "daily_digest", subscription_key: "daily-digest" })?` connects a source value (built with a documented pure constructor) to a process definition. `inputs` supplies every process parameter exactly once; `trigger.event` passes the whole event. Registrations, filters, orphans, prune and revision-checked enable/disable/delete are documented on the `triggers.*` operations."#.to_string()
}

fn operation_scheduling_language_bullet(processes: bool) -> String {
    let process_note = if processes {
        " Start independent process handles before joining with `await handles`."
    } else {
        ""
    };
    format!(
        "- Aggregate await: `results = await {{ a: module.a({{}})?, b: module.b({{}})?, label: \"kept\" }}` fans out direct operation leaves in nested lists/records, retaining shape and pure values. `?` unwraps each leaf; all siblings settle before the first source-order failure. Consecutive awaits serialize.{process_note}"
    )
}

fn base_tail_language_bullets() -> [String; 1] {
    [r#"- Statements: `if cond { … }`, `for x in xs { … }`, `while cond { … }`; prefer bounded loops. `break` exits the nearest loop; `continue` skips an iteration."#.into()]
}

fn render_builtins_section(images: bool, type_literals: bool) -> String {
    let mut text = r#"### Builtins

Call as functions. Lists are immutable: list builtins return new lists, so write `xs = push(xs, item)`.

- Size/lookup: `len(x)` (null = 0), `empty(x)`, `keys(r)`, `values(r)`, `contains(x, needle)` (substring, list item or record key).
- Text: `trim`, `lower`, `upper`, `split(s, sep)`, `join(list, sep)`, `replace(s, from, to)`, `starts_with(s, p)`, `ends_with(s, p)`, `find(s, needle, start?)` → index or null, `slice(x, start, end)` (end exclusive; null = open; negative = from end), `format(template, a, b, …)` with `{}` / `{0}` / `{{ }}`, `grep_text(s, needle)` → `[{ line, text, match, start, end }]` (1-based lines).
- Convert: `to_string`, `to_int`, `to_float`, `json_parse`.
- Lists/numbers: `push(list, item)`, `sort(list)`, `sort_by(list, "a.b")`, `unique(list)`, `reverse(list)`, `range(end)` / `range(start, end)` / `range(start, end, step)` (end exclusive), `sum(list)` (`sum([]) = 0`), `min`, `max` (empty errors), `ceil_div(a, b)`, `floor_div(a, b)` (integer-valued `a`, `b`; `b != 0`).

No regex or date builtins."#.to_string();
    if type_literals {
        text = text.replace(
            "`json_parse`.",
            "`json_parse`, `validate(value, Type { … })`.",
        );
    }
    if images {
        text = text.replace(
            "`len(x)` (null = 0)",
            "`len(x)` (null = 0; invalid for images, use `.size`)",
        );
    }
    text
}

fn render_decomposition_section(
    _has_operations: bool,
    _processes: bool,
    decomposition: bool,
) -> String {
    let mut text = r#"### Working with context

Keep large results in variables and print only the slice you need. Reuse Bound Variables and `history` directly; do not recreate them. Earlier execution outputs are strings at `history[N].output[M]` (zero-based); message entries have `.content` instead."#.to_string();
    if decomposition {
        text.push_str("\n\nIf you switch to a continuation tool, pass the remaining task and all needed state explicitly; nothing is inherited, and the switch ends the current program.");
    }
    text
}

/// Operation-owned lifecycle and trigger guidance shared by both prompt dialects.
pub(crate) fn host_operation_description(module: &str, operation: &str) -> Option<&'static str> {
    match (module, operation) {
        ("processes", "list") => Some(
            "List visible process runs. Empty arguments select running runs; `definition` selects a definition and `status: \"any\"` includes visible run history.",
        ),
        ("triggers", "register") => Some(
            "Register a source value and process definition with every parameter supplied exactly once in inputs. `subscription_key` is stable within the caller's owner scope; supply it or use the linker-derived default. A different definition at an existing key conflicts. The source-owning host/plugin emits occurrences; constructors build source values.",
        ),
        ("triggers", "list") => Some(
            "List visible registrations; filter by target, name, source_type or enabled. Each row carries registrant provenance and `manifest_membership`: `present_in_current_artifact` or `orphaned`. Reconcile warnings never delete subscriptions.",
        ),
        ("triggers", "prune") => Some(
            "Remove reviewed orphan subscriptions by subscription_keys. Prune is restricted to the acting owner namespace.",
        ),
        ("triggers", "disable") => Some(
            "Pause future deliveries. Supply subscription_key and expected_revision from the current receipt; mutations are revision-checked.",
        ),
        ("triggers", "enable") => Some(
            "Resume future deliveries. Supply subscription_key and expected_revision from the current receipt; mutations are revision-checked.",
        ),
        ("triggers", "delete") => Some(
            "Tombstone the subscription. Supply subscription_key and expected_revision from the current receipt; mutations are revision-checked.",
        ),
        _ => None,
    }
}
