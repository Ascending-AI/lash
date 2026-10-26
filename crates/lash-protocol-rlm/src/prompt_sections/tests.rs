//! Laws of the code-mode prompt sections (FIG-4588, FIG-5257): what each
//! section renders over the call's committed cut, where its default places
//! it, and how a host's plan and trusted wrappers move, replace or omit it.

use std::sync::Arc;

use lash_core::plugin::SessionPlugin;
use lash_core::plugin::prompt::{ComposedPrompt, PromptWrapSpec, PromptWrapTarget};
use lash_core::prompt_sections::{
    PromptPlacement, PromptPlan, PromptSectionPlacement, PromptWrapKey,
};
use lash_rlm_types::RlmCreateExtras;

use super::testing::{Call, RlmSections, compose, compose_rlm};
use super::*;

fn tool(
    name: &str,
    operation: &str,
    description: &str,
) -> lash_vocabulary::tool_contract::ToolDefinition {
    lash_vocabulary::tool_contract::ToolDefinition::raw(
        format!("tool:{name}"),
        operation,
        description,
        serde_json::json!({
            "type": "object",
            "properties": { "query": { "type": "string" } },
            "required": ["query"]
        }),
        serde_json::json!({ "type": "string" }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
}

/// Two tools of one MCP-style package, and one of another.
fn catalog() -> lash_core::ToolCatalog {
    lash_core::ToolCatalog::from_tool_definitions(vec![
        tool("grep", "files.grep", "Search file contents."),
        tool("tracker_search", "tracker.search", "Search issues."),
        tool("tracker_open", "tracker.open", "Open an issue."),
    ])
}

/// The sections of a TypeScript session whose host surface offers `catalog`.
fn sections(catalog: &lash_core::ToolCatalog) -> RlmSections {
    let _ = catalog;
    RlmSections::cell(SessionDialect::prompt_only(
        crate::dialect::CellDialect::typescript(),
    ))
}

fn facts() -> RlmPromptFacts {
    RlmPromptFacts {
        reserved_names: Vec::new(),
        bound_variables: Arc::from(""),
        packages: None,
        globals: Default::default(),
    }
}

fn turn(catalog: lash_core::ToolCatalog) -> Call {
    Call {
        catalog,
        ..Call::default()
    }
}

fn initial(composed: &ComposedPrompt) -> &str {
    composed.initial_instructions.as_deref().unwrap_or("")
}

fn current(composed: &ComposedPrompt) -> &str {
    composed.current_context.as_deref().unwrap_or("")
}

/// A host plugin whose wrappers replace or omit RLM sections.
struct HostWrappers(Vec<(&'static str, Option<&'static str>)>);

impl SessionPlugin for HostWrappers {
    fn id(&self) -> &'static str {
        "host"
    }
}
impl HostWrappers {
    fn recipe(self) -> lash_core::plugin::PluginRecipe {
        let mut reg = lash_core::plugin::PluginRegistrar::<Self>::new("host");
        for (target, text) in &self.0 {
            let text = *text;
            reg.prompt().wrap(
                PromptWrapSpec::new(
                    PromptWrapKey::new(*target).expect("valid wrap key"),
                    section_id(target),
                ),
                move |_, _| Ok(Arc::new(
                    move |_: &lash_core::plugin::prompt::PromptInput<'_>, _: PromptWrapTarget<'_>, _: lash_core::plugin::prompt::SectionText| {
                        Ok(text.map_or(lash_core::plugin::prompt::SectionText::Omit, lash_core::plugin::prompt::SectionText::text))
                    },
                )),
            ).expect("wrapper declares");
        }
        let owner = Arc::new(self);
        lash_core::plugin::PluginRecipe::new("host", reg, move |_, _| Ok(Arc::clone(&owner)))
    }
}

/// The declarations the fixture catalog generates.
const DECLARATIONS: &str = r##"### Tools

`files.grep({ query: string }): Promise<string>`
Search file contents.

`tracker.search({ query: string }): Promise<string>`
Search issues.

`tracker.open({ query: string }): Promise<string>`
Open an issue."##;
/// The TypeScript dialect's built-in execution prose through the native code tool.
const EXECUTION_PROSE: &str = r##"Use prose for conversation; use the `execute_code` program for action or computation. Call tools as `await module.operation({ ... })`, only those listed under **Tools**.

### Tool transport

For action or computation, make one `execute_code` call with `{"code": "<complete program>"}`. Call tools, including `control.finish`, inside the program; prose before the call is commentary.

### Example execute_code call

execute_code({"code":"const total = 1 + 2;\nawait control.finish(total);"})

Lexical and top-level bindings may shadow built-in names; top-level shadows persist across executions. Only an offered control capability ends the turn, never a user binding with the same name. Top-level bindings persist across executions as data. A function bound to a top-level name persists too, as a copy: what it reads from outside itself is frozen when its cell ends, so a later change to a top-level variable is not seen by it, and a change it makes to one is not kept. A pending promise does not outlive the cell that created it, nor does a function that holds one: a later cell that uses such a binding fails with `SESSION_BINDING_NOT_CARRIED`, so keep a promise's awaited result, not the promise. Return exactly the value and type the task asks for with `await control.finish(value)`; do not finish with an unexamined whole tool result. Putting an object into a string — with `+`, `` `${...}` `` or `String(...)` — gives the placeholder `[object Object]`, never its contents; read the value with `console.log(value)` or serialize it with `JSON.stringify(value)`.

`Math`, `Date` (UTC), `BigInt`, `String`, `Array`, `Object`, `JSON`, `Map`/`Set`, `RegExp` and `URL` are available; RegExp supports `d` capture indices in UTF-16 units, and Set algebra accepts set-like objects with `size`, `has` and `keys`; this is not Node or a browser, and classes, generators and `new Promise(...)` are not supported. Builtin constructors retain identity through aliases and computed reads. `globalThis` contains guest builtins only. Read-only own-property queries and descriptors, plus `Reflect.get`, `Reflect.has`, `Reflect.getOwnPropertyDescriptor` and `Reflect.ownKeys`, are supported; symbols and prototype/descriptor mutation are unavailable. `Object.assign` supports array indices/length and writable URL/RegExp fields, and regex match arrays support ordinary mutators. Builtin and bound function `toString()` returns native spelling; authored-source reflection remains unavailable. Builtin callbacks can cross function slots as forwarding wrappers with optional `arg0`, `arg1`, … parameters through their standard arity; data slots still reject functions.
Date local-named readers and date/time strings use UTC, with zero timezone offset. Date parsing accepts ISO and its own whole-second toString/toUTCString formats. Locale date/time formatting uses fixed en-US and UTC; locale case methods use pinned root/en-US Unicode mappings. Omit the locale or use "en-US", with no formatting options; other locales and localeCompare are refused. Dates are immutable; construct a new Date instead of using setters.

BigInt literals (`1n`) keep exact integer precision and have type `bigint`. Mixing BigInt and Number in arithmetic throws `TypeError`; use `Number(value)` to convert explicitly. `JSON.stringify` throws on BigInt values unless a replacer converts them to strings.

Labels, comma expressions, tagged templates and spread arguments to tools and `sleep` are supported. Top-level `this` is `undefined`; use `arguments` only inside a function. Ambient declarations and overload signatures erase and do not supply a runtime implementation. Discarding a tool promise is allowed, but outstanding work or an unobserved error can fail the cell at its end.

Type annotations are trusted and enforced where they are used: arithmetic, comparison, `!`, a condition, a template or an index on a value declared `number`, `string`, `boolean` or an array raises `type_error` when the value is not that type, and an index outside a declared array raises `index_out_of_range`, where JavaScript would convert or give `undefined`. Annotate only what is true, or leave the annotation off to keep JavaScript's conversions.

### Concurrency

An `async` function starts running when it is called, and async callbacks run concurrently: `items.map(async (item) => ...)` starts every call at once, and `await Promise.all(...)` collects them. Await every promise before the cell ends. A cell that ends while work it started is still running, or after a promise rejected with nothing awaiting it, fails with `CELL_TASKS_OUTSTANDING` and names the async code that is still running; `Promise.race([])` rejects instead of waiting forever.

### Host API

`console.log(value)` shows output in the next step; printing an image attachment shows the image there. `await control.finish(value)` ends the turn with `value`: nothing after it runs, so await it directly, as the last thing the program does, after every other promise has been awaited. A failed tool call throws; wrap it in `try`/`catch` to carry on.

`await sleep(ms)` pauses the program. For a timeout, race a call against a timer — `await Promise.race([call, sleep(ms)])` is `undefined` when the timer wins, and the losing call is cancelled."##;
/// Initial instructions contain execution and the offered declarations.
#[test]
fn the_initial_sections_render_the_protocol_prompt_around_the_declarations() {
    let catalog = catalog();
    let composed = compose_rlm(
        sections(&catalog),
        Call {
            facts: Some(facts()),
            ..turn(catalog)
        },
    );
    assert_eq!(
        initial(&composed),
        format!("## TypeScript execution\n\n{EXECUTION_PROSE}\n\n{DECLARATIONS}")
    );
}

/// OFFERED (FIG-5257): the declarations describe exactly the call's offered
/// callable surface. A tool the call does not offer is not declared; a
/// discovery session declares only its inline tools and says how to find
/// the rest, with or without the built-in prose; a trusted wrapper may
/// replace the declarations' text.
#[test]
fn the_declarations_describe_exactly_the_offered_callable_tools() {
    let full = catalog();
    let narrowed = lash_core::ToolCatalog::from_tool_definitions(vec![tool(
        "grep",
        "files.grep",
        "Search file contents.",
    )]);
    let rendered = compose_rlm(sections(&full), turn(narrowed));
    let rendered = initial(&rendered);
    assert!(rendered.contains("files.grep("), "{rendered}");
    assert!(!rendered.contains("tracker.search("), "{rendered}");

    let mut hidden = tool("hidden", "files.hidden", "A tool found by search.");
    hidden.manifest.inline = false;
    let discovering = lash_core::ToolCatalog::from_tool_definitions(vec![
        tool("search_tools", "tools.search", "Find tools."),
        hidden,
    ]);
    let discovery = RlmSections {
        discovery: Some(crate::plugin::RlmDiscovery {
            operation: "tools.search".to_string(),
            catalogue_bytes: usize::MAX,
        }),
        ..sections(&discovering)
    };
    for wrappers in [vec![], vec![(section_keys::EXECUTION, None)]] {
        let plugins: [lash_core::plugin::PluginRecipe; 2] =
            [discovery.clone().recipe(), HostWrappers(wrappers).recipe()];
        let composed = compose(&plugins, turn(discovering.clone()));
        let rendered = initial(&composed);
        assert!(rendered.contains(
            "Find tools.\n\nCatalogue: PARTIAL. 1 of 2 tools are listed above; find the other 1 with `await tools.search({ ... })`."
        ), "{rendered}");
        assert!(rendered.contains("Find tools."));
        assert!(!rendered.contains("A tool found by search."));
    }

    let plugins: [lash_core::plugin::PluginRecipe; 2] = [
        sections(&full).recipe(),
        HostWrappers(vec![(
            section_keys::DECLARATIONS,
            Some("### Tools\n\nThe host's own catalogue."),
        )])
        .recipe(),
    ];
    let replaced = compose(&plugins, turn(full.clone()));
    assert!(initial(&replaced).ends_with("### Tools\n\nThe host's own catalogue."));
    assert!(!initial(&replaced).contains("files.grep("));
}

/// The native channel's prompt carries no cell tag line in either part, and
/// the same declarations.
#[test]
fn the_native_channel_renders_the_same_declarations_without_cells() {
    let catalog = catalog();
    let composed = compose_rlm(
        RlmSections {
            ..sections(&catalog)
        },
        turn(catalog),
    );
    let rendered = initial(&composed);
    assert!(rendered.contains("### Tool transport"));
    assert!(rendered.contains(DECLARATIONS));
    assert!(rendered.contains("execute_code("));
}

/// A section with nothing to say is omitted: no heading, no separator.
#[test]
fn a_section_with_nothing_to_say_renders_nothing() {
    let catalog = catalog();
    let composed = compose_rlm(sections(&catalog), turn(catalog));
    let rendered = initial(&composed);
    assert!(!rendered.contains("Read-Only Variables"));
    assert!(!rendered.contains("\n\n\n"));
    // Late, only the finalization: no bound values, no budget.
    assert_eq!(
        current(&composed),
        format!(
            "\n=== FINALIZATION ===\n\n{}",
            sections(&lash_core::ToolCatalog::default())
                .dialect
                .finalization_copy(lash_core::TerminationMode::default(), &[])
        )
    );
}

/// The values a program has bound render late, in name order, from the
/// call's committed facts.
#[test]
fn bound_variables_render_late_in_name_order() {
    let mut cache = crate::rlm_support::BoundVariableRenderCache::default();
    let bound_variables = crate::driver::tests::rendered_bound_variables(
        &mut cache,
        serde_json::json!({ "zeta": 3, "scratch_note": "saved", "alpha": 1 }),
    );
    let composed = compose_rlm(
        RlmSections::typescript(),
        Call {
            facts: Some(RlmPromptFacts {
                reserved_names: Vec::new(),
                bound_variables,
                packages: None,
                globals: Default::default(),
            }),
            ..Call::default()
        },
    );
    let late = current(&composed);
    assert!(!initial(&composed).contains("scratch_note"));
    let alpha = late.find("- `alpha` = 1").expect("alpha row");
    let scratch = late
        .find(r#"- `scratch_note` = "saved""#)
        .expect("scratch row");
    let zeta = late.find("- `zeta` = 3").expect("zeta row");
    assert!(alpha < scratch && scratch < zeta, "{late}");
    assert!(late.find("=== FINALIZATION ===").expect("finalization") > zeta);
}

fn usage(tokens: i64) -> lash_core::LlmUsage {
    lash_core::LlmUsage {
        input_tokens: tokens,
        ..Default::default()
    }
}

fn budget(sections: RlmSections, call: Call) -> Option<String> {
    let composed = compose_rlm(sections, call);
    current(&composed)
        .split_once("=== CONTEXT BUDGET ===\n\n")
        .map(|(_, budget)| budget.to_string())
}

/// RLM-BUDGET (FIG-5257): the model-facing context budget is a keyed
/// section with the warning's rules. It is omitted without a configured
/// threshold or without committed usage; its threshold is clamped below the
/// context window; and it reads the previous turn's committed prompt usage,
/// never the current call's.
#[test]
fn the_context_budget_section_keeps_its_omit_clamp_and_prior_usage_rules() {
    let budgeted = |tokens| RlmSections {
        budget_tokens: tokens,
        ..RlmSections::typescript()
    };
    assert_eq!(
        budget(
            budgeted(None),
            Call {
                committed_usage: Some(usage(47_213)),
                ..Call::default()
            }
        ),
        None,
        "no configured threshold, no section"
    );
    for committed in [None, Some(usage(0))] {
        assert_eq!(
            budget(
                budgeted(Some(200_000)),
                Call {
                    committed_usage: committed,
                    ..Call::default()
                }
            ),
            None,
            "no committed usage, no section"
        );
    }
    let clamped = budget(
        budgeted(Some(100_000)),
        Call {
            committed_usage: Some(usage(40_999)),
            context_window_tokens: Some(41_000),
            ..Call::default()
        },
    )
    .expect("the section renders");
    assert!(
        clamped.contains("frame switch threshold: 40999"),
        "{clamped}"
    );
    assert!(!clamped.contains("100000"), "{clamped}");
    let prior = budget(
        budgeted(Some(200_000)),
        Call {
            committed_usage: Some(usage(47_213)),
            iteration: 2,
            ..Call::default()
        },
    )
    .expect("the section renders");
    assert!(
        prior.starts_with("Turn: 3 · Tokens: 47213 · frame switch threshold: 200000"),
        "{prior}"
    );
    // Without decomposition the budget asks for a concise finish, never a
    // frame switch.
    let undecomposed = budget(
        RlmSections {
            prompt_features: crate::protocol::RlmPromptFeatures {
                images: false,
                decomposition: false,
            },
            ..budgeted(Some(100))
        },
        Call {
            committed_usage: Some(usage(100)),
            ..Call::default()
        },
    )
    .expect("the section renders");
    assert!(undecomposed.contains("finish concisely"), "{undecomposed}");
    assert!(!undecomposed.contains("continue_as"), "{undecomposed}");
}

/// PLACEMENT (FIG-5257): the host's plan places a section exactly where it
/// says, with the same text either way. Left late (the default), a changing
/// budget leaves the initial instructions, the cached prefix, unchanged and
/// lands after the projected history; placed in the initial instructions,
/// it changes them.
#[test]
fn a_section_renders_the_same_text_wherever_the_host_places_it() {
    let budgeted = RlmSections {
        budget_tokens: Some(200_000),
        ..RlmSections::typescript()
    };
    let call = |plan: &PromptPlan, tokens| Call {
        committed_usage: Some(usage(tokens)),
        plan: plan.clone(),
        ..Call::default()
    };
    let late = PromptPlan::default();
    let early = PromptPlan {
        placements: vec![PromptSectionPlacement {
            section: section_id(section_keys::CONTEXT_BUDGET),
            placement: PromptPlacement::InitialInstructions,
        }],
        ..PromptPlan::default()
    };
    let block = |tokens: i64| {
        format!(
            "=== CONTEXT BUDGET ===\n\nTurn: 1 · Tokens: {tokens} · frame switch threshold: 200000 ({}%).",
            tokens * 100 / 200_000
        )
    };

    let late_first = compose_rlm(budgeted.clone(), call(&late, 10_000));
    let late_second = compose_rlm(budgeted.clone(), call(&late, 20_000));
    assert!(current(&late_first).ends_with(&block(10_000)));
    assert!(!initial(&late_first).contains("CONTEXT BUDGET"));
    assert_eq!(
        late_first.initial_instructions, late_second.initial_instructions,
        "a late section leaves the cached prefix unchanged"
    );

    let early_first = compose_rlm(budgeted.clone(), call(&early, 10_000));
    let early_second = compose_rlm(budgeted, call(&early, 20_000));
    assert!(initial(&early_first).ends_with(&block(10_000)));
    assert!(!current(&early_first).contains("CONTEXT BUDGET"));
    assert_ne!(
        early_first.initial_instructions, early_second.initial_instructions,
        "an early section changes the cached prefix"
    );

    // Placed on a request, the late text follows the projected history as
    // User context, outside it; the early text is the instructions'.
    let place = |composed: &ComposedPrompt| {
        let mut request = crate::driver::tests::projected_request();
        lash_core::sansio::place_prompt(
            &mut request,
            composed.initial_instructions.as_deref().map(Arc::from),
            composed.current_context.as_deref().map(Arc::from),
            true,
        );
        request
    };
    let late_request = place(&late_first);
    let tail = late_request.messages.last().expect("the late context");
    assert!(matches!(tail.role, lash_core::llm::types::LlmRole::User));
    assert!(crate::driver::tests::message_text(tail).ends_with(&block(10_000)));
    let early_request = place(&early_first);
    assert!(
        early_request
            .instructions
            .as_deref()
            .expect("instructions")
            .ends_with(&block(10_000))
    );
    assert!(
        !crate::driver::tests::message_text(early_request.messages.last().expect("tail"))
            .contains("CONTEXT BUDGET")
    );
}

/// RLM-LATE-LAYOUT (FIG-5271): sections continue the iteration in one User
/// message: the iteration, one headed variables block and finalization, with
/// no transcript surface (ADR 0147).
#[test]
fn the_late_sections_keep_one_user_message_with_the_expected_tail() {
    {
        for (structured, images) in [(false, true), (true, true), (true, false)] {
            let sections = RlmSections {
                ..RlmSections::typescript()
            };
            let options = RlmCreateExtras::default();
            let finalization = sections
                .dialect
                .finalization_copy(options.termination.unwrap_or_default(), &[]);
            let optional = if images {
                String::new()
            } else {
                "\n\n=== CONTEXT BUDGET ===\n\nTurn: 1 · Tokens: 10000 · frame switch threshold: 200000 (5%).".to_owned()
            };
            let (mut request, facts) =
                crate::driver::tests::projected_request_with_facts(structured);
            let history_messages = request.messages.len() - 1;
            let composed = compose_rlm(
                RlmSections {
                    budget_tokens: Some(200_000),
                    ..sections
                },
                Call {
                    facts: Some(facts),
                    options,
                    committed_usage: (!images).then(|| usage(10_000)),
                    ..Call::default()
                },
            );
            lash_core::sansio::place_prompt(
                &mut request,
                composed.initial_instructions.as_deref().map(Arc::from),
                composed.current_context.as_deref().map(Arc::from),
                true,
            );
            assert_eq!(
                request.messages.len(),
                history_messages + 1,
                "history plus one late message"
            );
            let tail = request.messages.last().expect("late message");
            assert!(matches!(tail.role, lash_core::llm::types::LlmRole::User));
            let text = crate::driver::tests::message_text(tail);
            assert_eq!(
                text,
                format!(
                    "\n\n\n=== CURRENT ITERATION: 1 ===\n\n\n=== BOUND VARIABLES ===\n\n\
                 - `scratch_note` = \"saved\"\n\n\n=== FINALIZATION ===\n\n{finalization}{optional}"
                )
            );
            assert_eq!(text.matches("=== BOUND VARIABLES ===").count(), 1);
            assert!(!tail.starts_user_segment, "the late context is synthetic");
            assert!(matches!(
                tail.blocks.as_slice(),
                [lash_core::llm::types::LlmContentBlock::Text {
                    cache_breakpoint: false,
                    ..
                }]
            ));
        }
    }
}

/// Tool names declare a callable surface, not host interaction policy (FIG-5432).
#[test]
fn an_ask_tool_declares_its_surface_without_interaction_policy() {
    let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![tool(
        "ask",
        "user.ask",
        "Collect a response.",
    )]);
    {
        let composed = compose_rlm(
            RlmSections {
                ..sections(&catalog)
            },
            turn(catalog.clone()),
        );
        let prompt = initial(&composed);
        assert!(prompt.contains("user.ask"), "{prompt}");
        assert!(
            !prompt.contains("Ask only when progress is blocked"),
            "{prompt}"
        );
        assert!(!prompt.contains("Be concise"), "{prompt}");
        assert!(!prompt.contains("You are an assistant"), "{prompt}");
    }
}

#[test]
fn the_initial_instructions_are_stable_while_globals_change() {
    let mut cache = crate::rlm_support::BoundVariableRenderCache::default();
    let previous =
        crate::driver::tests::rendered_bound_variables(&mut cache, serde_json::json!({}));
    let next = crate::driver::tests::rendered_bound_variables(
        &mut cache,
        serde_json::json!({ "scratch_note": "saved" }),
    );
    let at = |bound_variables, iteration| {
        compose_rlm(
            RlmSections::typescript(),
            Call {
                facts: Some(RlmPromptFacts {
                    reserved_names: Vec::new(),
                    bound_variables,

                    packages: None,
                    globals: Default::default(),
                }),
                iteration,
                ..Call::default()
            },
        )
    };
    let (previous, next) = (at(previous, 0), at(next, 1));
    assert_eq!(previous.initial_instructions, next.initial_instructions);
    assert!(!initial(&next).contains("scratch_note"));
    assert!(current(&next).contains("scratch_note"));
}
