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
    render_execution_for_catalog(features, surface, &[])
}

pub(crate) fn render_execution_for_catalog(
    features: RlmPromptFeatures,
    surface: &lashlang::LashlangHostEnvironment,
    documented_tools: &[String],
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
    if let Some(section) = render_host_environment_section(surface, documented_tools) {
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
    let mut section = String::new();
    if has_operations {
        section.push_str("Use prose for conversation; use a paired `<lashlang>` block for action or computation. Call only documented tools: `await module.operation({ ... })?`. Use discovery if available.");
    } else {
        section.push_str("Use plain prose only for direct conversational replies that need no computation. Use Lashlang to compute values, inspect current variables, validate data, or return structured/computed results. No module operations are available in this turn, so do not invent tool calls.");
    }
    section.push_str(
        r#"

### `print` vs `finish`

- `print <expr>` inspects and continues; output appears next step. Print small useful values or selected fields/slices.
- `finish <expr>` ends the turn: strings pass through, other values render as pretty JSON. Follow final-answer guidance; omit bulky dumps unless requested.

Never `finish` a raw tool-result dump: `print` it first, then summarize.

Inspect and verify current-state results before finishing.

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
) -> String {
    let mut bullets = Vec::new();
    push_value_language_bullets(&mut bullets, images);
    bullets.push(strings_language_bullet());
    bullets.push(operator_language_bullet());
    bullets.push(assignment_language_bullet());
    bullets.push(list_comprehension_language_bullet());
    bullets.push(functions_language_bullet());
    if has_operations {
        bullets.push(module_operations_language_bullet());
    }
    if abilities.sleep {
        bullets.push(sleep_language_bullet().replace(
            "foreground code or process code",
            if abilities.processes {
                "foreground code or process code"
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
        bullets.push("- Images: image-producing tools may return an `Image` value. Read metadata with `.id`, `.label`, `.size`, `.width`, `.height`; fields are read-only. `print(image)` or `print` on a list/record containing images sends both descriptor text and the actual image attachment to the next model call. `finish image`, `to_string(image)`, and JSON-like serialization emit only `{ \"type\": \"image\", \"id\": ..., \"label\": ..., \"size\": ..., \"width\": ..., \"height\": ... }`. `len(image)` is invalid; use `.size`.".to_string());
    } else {
        bullets.push("- Values: null, booleans, numbers, strings, lists, and records. Literals: `[a, b]`, `{ a: 1, b: 2 }`.".to_string());
    }
}

fn strings_language_bullet() -> String {
    r#"- Strings: single/double quotes and triple-quoted multiline forms support `\n`, `\r`, `\t`, escaped quotes and `\\`. Prefix any with `r` for raw text (JSON, patches, shell payloads). Interpolate with `format`, not f-strings."#.to_string()
}

fn operator_language_bullet() -> String {
    r#"- Precedence: postfix calls/fields/indexing/result `?`; unary `-`/`!`/`not`; `* / %`; `+ -`; comparisons `== != < <= > >= in`; `and`/`&&`; `or`/`||`; ternary `? :`. `in` tests list/tuple membership, record keys or substrings; negate with `!(x in y)`, never `not in`. Unsupported pairs error; null haystacks return false."#.to_string()
}

fn assignment_language_bullet() -> String {
    r#"- `name = expr` persists across `<lashlang>` blocks. Update paths: `record.field = v`, `record[key] = v`, `list[i] = v`, and nested paths. Record writes insert/replace; lists require existing indices. Record keys stringify; missing reads return null, so `counts[g] = counts[g] + 1` works."#.to_string()
}

fn list_comprehension_language_bullet() -> String {
    r#"- Comprehensions: `[expr for x in xs if cond]`; multiple for/if clauses execute left-to-right. Bindings are local. Use loops for mutation, break or continue."#.to_string()
}

fn functions_language_bullet() -> String {
    r#"- Pure functions: `fn f(x: type) -> type { body }`; types required; last expression returns; parameters-only scope. Call `f(arg)`; recursion and forward calls work. Arithmetic yields float: use `-> float`, not int. Keep effects outside functions and pass results in."#.to_string()
}

fn module_operations_language_bullet() -> String {
    r#"- Tools: `await module.op({ field: value })?`; use only names under **Tools**. Bare calls are builtins; UpperCamel host constructors are pure (never await). `?` aborts on failure with sanitized operation metadata."#.to_string()
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
        "- Background processes: `process name(param: TYPE) {{ ... }}` declares a reusable process definition.{signal_declaration_note} `handle = start name(param: value)` creates one process run from that definition and returns its run handle;{trigger_process_note} For account-parametric work, pass typed module authorities explicitly, e.g. `process notify(mail: Gmail) {{ await mail.send({{ body: body }})? finish true }}` and `start notify(mail: gmail.work)`. For one-off concrete automations, a process body may reference concrete host paths such as `agents`, `web`, or `gmail.work` directly; params and locals shadow those captures. Inside a process use {}. `wake value` emits a `process.wake` event that notifies the agent/session with `value`; use it when process progress or other background work should re-enter the model as context. `finish value` completes the run and stores `value` as the process success value. `fail value` completes it as failed; falling off the end is `finish null`. `print` is foreground-only and invalid inside processes. Parallelism comes from starting all independent process handles before waiting for any of them; join a list or record of handles with `results = await handles`. `await handle` waits and returns a result wrapper like `{{ ok: true, value: ... }}`; when you need fields from the `finish` value, use `result = (await handle)?` and then read `result.field`. Cancel a live run with `cancel handle` (best-effort). If the Host Surface includes `processes.list`, use `await processes.list({{}})?` for running runs, `await processes.list({{ definition: name }})?` for runs of a definition, and `await processes.list({{ status: \"any\" }})?` for visible run history.",
        join_words(&forms),
    ));
    if abilities.process_signals {
        bullets.push("- Signalling processes: `signal_run(handle, \"name\", payload)` sends a typed `signal.name` event to a running process and may be used from the foreground turn as well as inside a process body, like `await handle` and `cancel handle`. The receiving side, `payload = wait_signal(\"name\")`, parks a process until that named signal arrives and is only valid inside a process body.".to_string());
    }
}

fn label_annotations_language_bullet(abilities: &lashlang::LashlangAbilities) -> String {
    let declaration = if abilities.processes {
        " or process declaration"
    } else {
        ""
    };
    let targets = if abilities.processes {
        "branches, loops, or process declarations"
    } else {
        "branches, and loops"
    };
    let mut process = String::new();
    if abilities.processes {
        let mut steps = vec!["awaited module calls", "`start`"];
        if abilities.sleep {
            steps.push("`sleep`");
        }
        if abilities.process_signals {
            steps.extend(["`wait_signal`", "`signal_run`"]);
        }
        steps.extend([
            "`wake`",
            "`yield`",
            "`finish`",
            "`fail`",
            "`if`",
            "loops",
            "and setup statements that explain the process",
        ]);
        process = format!(
            " Inside a `process` body, label durable steps such as {}.",
            steps.join(", ")
        );
    }
    format!(
        "- Execution labels: `@label(title: \"Label\")` or `@label(title: \"Label\", description: \"Details\")` names important Lashlang phases and graph steps. It is a prefix annotation, not a standalone statement; it must appear immediately before the one statement{declaration} it labels, e.g. `@label(title: \"Prepare query\")\\nquery = \"runtime architecture\"`. Do not emit `@label(...)` by itself or stack multiple labels before one statement. At top level, label meaningful setup, resource calls, submissions, {targets}.{process} Titles/descriptions must be string literals; do not use variables, interpolation, icons, colors, layout hints, or extra keys."
    )
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
        " Start independent process handles before joining with `await handles`."
    } else {
        ""
    };
    format!(
        "- Aggregate await: `results = await {{ a: module.a({{}})?, b: module.b({{}})?, label: \"kept\" }}` fans out direct operation leaves in nested lists/records, retaining shape and pure values. `?` unwraps each leaf; all siblings settle before the first source-order failure. Consecutive awaits serialize.{process_note}"
    )
}

fn base_tail_language_bullets() -> [String; 3] {
    [
        "- Statements: `if`, `for`, `while`; prefer bounded loops. `break` exits the nearest loop; `continue` skips an iteration; `finish` ends the turn. Expression conditional: `cond ? yes : no`, never expression-form if. Negate with `!cond` or `not cond`.".into(),
        "- Bare expressions are statements.".into(),
        "- Use Bound Variables and `history` directly; do not recreate them.".into(),
    ]
}

fn render_builtins_section(images: bool, type_literals: bool) -> String {
    let bullets = lashlang::builtin_names()
        .filter(|name| type_literals || *name != "validate")
        .map(|name| builtin_prompt_bullet(name, images))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "### Builtins\n\nCall as functions:\n\n{bullets}\n\nRegex/date operations require host tools."
    )
}

fn builtin_prompt_bullet(name: &str, images: bool) -> String {
    let detail = match name {
        "len" if images => {
            "`len(x)` — string/tuple/list/record length; null = 0; invalid for images, so use `image.size`"
        }
        "len" => "`len(x)` — string/tuple/list/record length; null = 0",
        "empty" => "`empty(x)` — len(x) == 0",
        "keys" => "`keys(record)` — new key list; null = []",
        "values" => "`values(record)` — new value list; null = []",
        "trim" => "`trim(s)` — trim whitespace",
        "to_string" => "`to_string(x)` — to text",
        "to_int" => "`to_int(x)` — number/numeric string to int",
        "to_float" => "`to_float(x)` — number/numeric string to float",
        "json_parse" => "`json_parse(s)` — parse JSON text",
        "contains" => "`contains(haystack, needle)` — substring, tuple/list item or record key",
        "grep_text" => {
            "`grep_text(s, needle)` — literal lines, nonempty needle; match rows: `{ line: int, text: str, match: str, start: int, end: int }`, 1-based lines, 0-based characters, exclusive end"
        }
        "starts_with" => "`starts_with(s, prefix)` — literal prefix test",
        "ends_with" => "`ends_with(s, suffix)` — literal suffix test",
        "split" => "`split(s, sep)` — literal split to list",
        "join" => "`join(list, sep)` — join tuple/list with separator",
        "validate" => {
            "`validate(value, Type { ... })` — return a value unchanged when it matches the type literal, otherwise abort with a typed validation error"
        }
        "ceil_div" => "`ceil_div(a, b)` — ceil(a/b); b != 0",
        "floor_div" => "`floor_div(a, b)` — floor(a/b); b != 0",
        "push" => "`push(list, item)` — new appended list",
        "slice" => {
            "`slice(s, start, end)` — string/list slice; null = open bound; negative = from end"
        }
        "find" => {
            "`find(s, needle, start?)` — first literal character index or null; start >= 0 (default 0); empty needle returns in-bounds start"
        }
        "format" => {
            "`format(template, arg0, arg1, ...)` — interpolate: `{}` auto, `{0}` indexed, `{{`/`}}` escaped braces; separate args, not a list"
        }
        "range" => {
            "`range(end)` / `range(start, end, step=1)` — end-exclusive integers; signed nonzero step"
        }
        "sort" => "`sort(list)` — stable ascending, one comparable type; new list",
        "sort_by" => {
            "`sort_by(list, \"field.path\")` — stable ascending by nonempty dotted path; new list"
        }
        "sum" => "`sum(list)` — numeric total; sum([]) = 0",
        "min" => "`min(list)` — least of one comparable type; empty errors",
        "max" => "`max(list)` — greatest of one comparable type; empty errors",
        "replace" => {
            "`replace(s, from, to)` — literal replace; empty from inserts at UTF-8 boundaries and ends"
        }
        "lower" => "`lower(s)` — Unicode lowercase",
        "upper" => "`upper(s)` — Unicode uppercase",
        "unique" => "`unique(list)` — new list, first occurrences, typed equality",
        "reverse" => "`reverse(list)` — new reversed list/tuple",
        _ => panic!("builtin `{name}` is missing its prompt contract"),
    };
    format!("- {detail}")
}

fn render_decomposition_section(
    has_operations: bool,
    processes: bool,
    decomposition: bool,
) -> String {
    let mut section = String::from(
        "### Working with context\n\nKeep large artifacts in variables; show small working state.",
    );

    section.push_str(
        "\n\nRead full prior output via `history[N].output[M]`; print the variable or slice you need.",
    );

    if !has_operations || processes || decomposition {
        section.push('\n');
    }
    if has_operations && processes {
        section.push_str("\n- Several independent slow operations are needed -> use aggregate await over a record/list of direct module calls plus any pure values you want preserved, putting `?` on each operation leaf that should unwrap.");
    }
    if decomposition {
        section.push_str("\n- The trace is bloated, stale, or failed attempts dominate -> use an available continuation tool to switch to a fresh AgentFrame with concrete state.");
    }
    if has_operations && processes {
        section.push_str("\n- See **Tools** for parameters and results.\n\nExample parallel fan-out around an available operation (aggregate await preserves the record shape; use `?` on each leaf to unwrap it):\n\n    <lashlang>\n    results = await {\n      one: module.operation({ query: \"one\" })?,\n      two: module.operation({ query: \"two\" })?\n    }\n    finish format(\"First result: {}\\n\\nSecond result: {}\", slice(to_string(results.one), 0, 800), slice(to_string(results.two), 0, 800))\n    </lashlang>");
    } else if !has_operations {
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
