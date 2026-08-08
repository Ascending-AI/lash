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

fn render_host_environment_section(surface: &lashlang::LashlangHostEnvironment) -> Option<String> {
    // Operations with real Lashlang types (trigger and other host primitives)
    // are listed here. Tool-catalog operations are bridged with placeholder
    // `any` types and documented in full under **Tools**, so they are skipped to
    // avoid an uninformative `any -> any` duplicate of that section.
    let mut operation_lines = Vec::new();
    for (_, module) in surface.resources.module_instances() {
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
                operation_lines.push(format!(
                    "- `await {}.{}({})? -> {}`",
                    module.alias,
                    operation,
                    lashlang::format_type_expr(&binding.input_ty),
                    lashlang::format_type_expr(&binding.output_ty)
                ));
            }
        }
    }
    let mut data_type_lines = Vec::new();
    for (_, data_type) in surface.resources.named_data_types() {
        data_type_lines.push(format!(
            "- `type {} = {}`",
            data_type.name(),
            lashlang::format_type_expr(data_type.ty())
        ));
    }
    let mut constructor_lines = Vec::new();
    for (_, constructor) in surface.resources.value_constructors() {
        let output_ty = match &constructor.output_ty {
            lashlang::TypeExpr::Ref(name) => surface
                .resources
                .resolve_trigger_source(name.as_str())
                .map(|binding| format!("TriggerSource<{}>", binding.event_type_name()))
                .unwrap_or_else(|| lashlang::format_type_expr(&constructor.output_ty)),
            _ => lashlang::format_type_expr(&constructor.output_ty),
        };
        constructor_lines.push(format!(
            "- `{}({}) -> {}`",
            constructor.path.join("."),
            lashlang::format_type_expr(&constructor.input_ty),
            output_ty
        ));
    }
    let mut protocol_lines = Vec::new();
    let trigger_register = lashlang::TriggerHostOperation::Register.host_operation();
    for (source_ty, binding) in surface.resources.trigger_sources() {
        protocol_lines.push(format!(
            "- `{}` can be passed to `{}` and emits `{}`",
            source_ty,
            trigger_register,
            binding.event_type_name()
        ));
    }
    if operation_lines.is_empty()
        && data_type_lines.is_empty()
        && constructor_lines.is_empty()
        && protocol_lines.is_empty()
    {
        return None;
    }
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

Executable code must be inside paired `<lashlang>` and `</lashlang>` tags. The start and close tag lines must be standalone after trimming. When action is needed, place the Lashlang block after any visible prose or omit prose. Any turn-ending rules for prose-only responses versus `finish` are listed in the current **FINALIZATION** section.
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
    let len_detail = if images {
        "length of string/list/record (0 for null); use `image.size` for images"
    } else {
        "length of string/list/record (0 for null)"
    };
    let bullets = [
        format!("- `len(x)` — {len_detail}"),
        "- `empty(x)` — true if length is 0".to_string(),
        "- `slice(s, start, end)` — substring or sublist".to_string(),
        "- `range(end)` / `range(start, end)` / `range(start, end, step)` — integer list, end-exclusive; positive or negative `step`, never `0`".to_string(),
        "- `ceil_div(a, b)` / `floor_div(a, b)` — integer division helpers for chunk/count math; divisor must not be `0`".to_string(),
        "- `push(list, item)` — new list with one item appended".to_string(),
        "- `sort(list)` — stable ascending sort; items must share one comparable type. `sort_by(list, \"field.path\")` stably sorts records by a required nested field".to_string(),
        "- `sum(list)` / `min(list)` / `max(list)` — numeric total or comparable extrema; `sum([])` is `0`, while extrema of an empty list are `null`".to_string(),
        "- `unique(list)` / `reverse(list)` — stable deduplication or reversal, returning a new list".to_string(),
        "- `split(s, sep)` / `join(list, sep)` — string split/join".to_string(),
        "- `replace(s, from, to)` / `lower(s)` / `upper(s)` — literal text rewriting and Unicode case conversion".to_string(),
        "- `find(s, needle, start?)` — zero-based character index of the first literal match, or `null`; `start` defaults to `0` and is a non-negative character index; an empty `needle` returns `start` when it is in bounds".to_string(),
        "- `grep_text(s, needle)` — literal in-memory line search; `needle` must be non-empty; returns one record per matching line: `{ line: int, text: str, match: str, start: int, end: int }`, where `line` is 1-based, `text` is the line without its line ending, and `start`/`end` are zero-based character offsets within that line's `text` with `end` exclusive".to_string(),
        "- `trim(s)` — strip whitespace".to_string(),
        "- `starts_with(s, prefix)` / `ends_with(s, suffix)` / `contains(haystack, needle)`".to_string(),
        "- `keys(record)` / `values(record)`".to_string(),
        "- `to_string(x)` / `to_int(x)` / `to_float(x)`".to_string(),
        "- `json_parse(s)` — parse a JSON string into a value".to_string(),
        "- `format(template, arg0, arg1, ...)` — positional interpolation: `{}` auto-numbers, `{0}` / `{1}` pick a specific arg, `{{` / `}}` escape literal braces. Do not wrap args in a list: use `format(\"It is {}.\", trim(now.output))`, not `format(\"It is {}.\", [trim(now.output)])`.".to_string(),
        "- `validate(value, Type { ... })` — check an intermediate value against a Type literal and return it unchanged, or abort with a validation error".to_string(),
        "- Regex and date/time operations are deliberately host tools, not builtins".to_string(),
    ];

    format!(
        "### Builtins\n\nCall as functions (e.g. `len(x)`, `slice(s, 0, 200)`). For `slice`, `null` bounds mean start/end; negative bounds count from the end.\n\n{}\n",
        bullets.join("\n")
    )
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
