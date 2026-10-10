//! The full-assembly prompt walker.
//!
//! Two narrower walkers already exist and both passed while the product was
//! broken: `every_diagnostic_code_named_in_the_prompt_exists` reads the
//! execution section's `TS_` codes, and
//! `no_diagnostic_from_a_prompt_primitive_names_a_lash_vm_identifier` reads
//! the diagnostics its primitives emit. Neither looks at the *rest* of the
//! prompt, and the rest of the prompt is where the leak was: a TypeScript
//! session was told to write `<typescript>` cells by its execution section and,
//! a few hundred tokens later, that its variables were "already bound in
//! lash_vm" and should be accessed "in `<lash_vm>` blocks". The judged
//! battery caught a model spending reasoning tokens trying to reconcile the
//! two.
//!
//! TypeScript is now the only contract (ADR 0096), so this walks every fragment
//! the crate contributes to an assembled prompt and fails on any word of the
//! retired surface. The markers are not vestigial: the host-surface inventory
//! is declared in Lash VM `TypeExpr`s and the tool examples are authored in
//! the retired spelling, so both are still rendered through a translator that
//! can regress.
//!
//! Tool descriptions and schema prose come from their owning crates and render
//! verbatim. The owning crate must test the prose it contributes to a prompt.
//!
//! Authored examples share the blind spot and sit outside even that gate's
//! reach: the prose sweep excludes them on purpose, and `authored_tool_examples`
//! below is a copied fixture rather than the live corpus — which is how
//! `agents.spawn` rendered `const Shape = Type { ... }` to TypeScript sessions
//! (FIG-1480). The rewriter stays pinned here over representative spellings;
//! each owning crate pins its own rendered catalog (see the rendered-catalog
//! test on `spawn_agent_tool_definition` in `examples/delegation`).

use super::*;
use lash_vm_runtime::ToolDefinitionBindingExt as _;

/// Text that names the retired surface, with the reason each token is a defect.
///
/// A TypeScript session must never see the retired cell tag, its language name
/// in prose, or its statement syntax.
const RETIRED_SURFACE_MARKERS: &[&str] = &[
    "<lash_vm>",
    "</lash_vm>",
    "lash_vm block",
    "lash_vm blocks",
    "bound in lash_vm",
    "`print ",
    "re-print",
    "finish <value>",
    // The retired surface's *type* syntax, which the host-surface inventory is
    // declared in and the prompt has to render. The bound-variable block
    // renders a whole type line in it, not one token: pinning only `list[`
    // described the leak as narrower than it is.
    "list[",
    // Written with the trailing punctuation so they cannot match TypeScript's
    // own `: string` / `: number`.
    ": str,",
    ": int,",
    "?: any |",
    "-> str",
    // The retired try-operator, in an authored tool example.
    ")?",
    "-> float",
    "trigger.register",
];

/// Substrate identifiers that may appear in a prompt in the retired surface's
/// spelling. ADR 0063 holds the rule and the whole list; this is the
/// executable half of it.
///
/// Exactly one qualifies, and it is a payload discriminant rather than prose:
/// the model-visible `history` variable really does contain
/// `kind: "lash_vm_step"` in both dialects, because `RlmHistoryItem` is one
/// serialized type and its event ids (`lash_vm_step_<turn>_<iteration>`,
/// `protocol/driver.rs`) are durable session-graph identifiers. Teaching a
/// TypeScript model to expect `typescript_step` would make the prompt
/// *disagree with the data the model receives*, which is the defect class this
/// layer exists to close. Renaming both sides together is a durable payload
/// change and is tracked separately; until then the honest prompt is the one
/// that matches the wire.
///
/// The `__lash_vm_runtime` module the TypeScript lowerer resolves
/// `Date.now()`/`Math.random()` through is deliberately **not** here: nothing
/// about it has to reach a model, so ADR 0063 hides it from the prompt instead
/// of carving it out.
/// The second carve-out is a durable process identity. A TypeScript session's
/// processes are compiled against the Lash VM substrate and their ids are
/// `process:lash_vm:v3:blake3:…` — journal identity, visible to a host through
/// `/api/work`. Renaming them would move a durable id for a cosmetic gain, so
/// the id stays and the *label* half of the same defect is what got fixed
/// (transcript badges read the recorded dialect).
const SUBSTRATE_CARVE_OUTS: &[&str] = &[
    "lash_vm_step",
    "process:lash_vm:",
    // Effect ids are `lash_vm:effect:<session>:<turn>:…` in every dialect:
    // the engine identity in a durable journal key. Same ruling as the process
    // id — durable, so carved out rather than renamed.
    "lash_vm:effect:",
];

fn strip_carve_outs(text: &str) -> String {
    let mut text = text.to_string();
    for allowed in SUBSTRATE_CARVE_OUTS {
        text = text.replace(allowed, "«substrate carve-out»");
    }
    text
}

async fn assembled_prompt_fragments(dialect: &SessionDialect) -> Vec<(&'static str, String)> {
    assembled_prompt_fragments_with_projection(dialect, serde_json::json!("src/lib.rs")).await
}

#[tokio::test]
async fn no_assembled_prompt_fragment_carries_the_retired_surfaces_words() {
    let dialect = crate::dialect::typescript_test_dialect();
    let mut violations = Vec::new();
    for (name, fragment) in assembled_prompt_fragments(&dialect).await {
        let haystack = strip_carve_outs(&fragment).to_lowercase();
        for marker in RETIRED_SURFACE_MARKERS {
            if haystack.contains(&marker.to_lowercase()) {
                violations.push(format!("prompt fragment `{name}` contains `{marker}`"));
            }
        }
        for word in retired_type_words(&fragment) {
            violations.push(format!(
                "prompt fragment `{name}` spells a type as `{word}`"
            ));
        }
    }
    violations.sort();
    let residuals = KNOWN_TYPE_SYNTAX_RESIDUALS
        .iter()
        .map(|residual| (*residual).to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        violations, residuals,
        "the assembled prompt carries retired-surface words, or a known residual changed. \
         Fixing one means deleting its row from KNOWN_TYPE_SYNTAX_RESIDUALS."
    );
}

// FIG-4544 closes the last of them: tool rows, the required-output block, the
// history definition and inferred value shapes are all spelled by the dialect.
const KNOWN_TYPE_SYNTAX_RESIDUALS: &[&str] = &[];

/// The retired surface's type words, where a fragment uses one as a type.
///
/// `RETIRED_SURFACE_MARKERS` pins a few whole tokens (`list[`, `: str,`). A
/// type can follow any of `:`, `|`, `<`, `[` or `=`, and end at any
/// punctuation, so this reads the word after each of those instead. None of
/// these words is a TypeScript type, so a match is the retired spelling.
fn retired_type_words(text: &str) -> Vec<String> {
    const WORDS: &[&str] = &[
        "str", "int", "float", "bool", "record", "list", "enum", "dict",
    ];
    let mut found = Vec::new();
    for (index, opener) in text.char_indices() {
        if !matches!(opener, ':' | '|' | '<' | '[' | '=') {
            continue;
        }
        let rest = text[index + opener.len_utf8()..].trim_start_matches(' ');
        let word = rest
            .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
            .next()
            .unwrap_or_default();
        if WORDS.contains(&word) && !found.iter().any(|seen| seen == word) {
            found.push(word.to_string());
        }
    }
    found
}

/// The walker only measures if its marker list can fire, and only proves
/// anything if a rendered example is *parseable* TypeScript.
#[test]
fn the_marker_list_and_the_example_rewriter_are_not_vacuous() {
    // RV-3: a rendered example must be *parseable* TypeScript, not merely free
    // of retired markers. The marker walk cannot tell "reads like TypeScript"
    // from "is a syntax error", and a syntax error in an example is exactly the
    // defect the examples fix exists to close. Parsed rather than linked: an
    // example names host modules and free identifiers that no isolated
    // environment has, so `TS_UNKNOWN_BINDING` is expected and a *syntax* error
    // is not.
    let typescript = crate::dialect::TypescriptPrompts;
    let embedding = lash_vm_worker::standard(&lash_vm_client::WorkerTuning::standard())
        .expect("the standard worker embedding");
    let (effects, bindings) = (
        std::collections::BTreeMap::new(),
        std::collections::BTreeSet::new(),
    );
    let mut unparseable = Vec::new();
    for example in authored_tool_examples() {
        let rendered = typescript
            .render_tool_example(example)
            .expect("TypeScript spells every authored example");
        if let Err(error) = lash_dialect_typescript::lower(
            &rendered,
            &lash_kernel_dialect::Environment {
                library: embedding.library(),
                effects: &effects,
                controls: &std::collections::BTreeMap::new(),
                tool_roots: &std::collections::BTreeSet::new(),
                bindings: &bindings,
                functions: &std::collections::BTreeMap::new(),
            },
        ) {
            let code = format!("{:?}", error.code);
            if code.contains("UnknownBinding") || code.contains("MethodUnsupported") {
                continue;
            }
            unparseable.push(format!("`{example}` → `{rendered}`: {}", error.message));
        }
    }
    assert!(
        unparseable.is_empty(),
        "rendered examples that are not TypeScript: {unparseable:#?}"
    );

    // Non-vacuity for the example surface: the *authored* corpus really does
    // carry the try-operator, so the marker has something to catch.
    assert!(
        authored_tool_examples()
            .iter()
            .any(|example| example.contains(")?")),
        "the authored corpus must carry the try-operator"
    );

    // The rewriter itself: a reader must be shown the examples rewritten.
    assert_eq!(
        typescript
            .render_tool_example(r#"await web.fetch({ url: "https://example.test/" })?"#)
            .as_deref(),
        Some(r#"await web.fetch({ url: "https://example.test/" });"#)
    );
    assert_eq!(
        typescript
            .render_tool_example("page = await web.fetch({ url: \"u\" })?")
            .as_deref(),
        Some("const page = await web.fetch({ url: \"u\" });")
    );

    // And the markers themselves must be present in the retired surface's real
    // copy, or the walker is looking for strings nothing ever emitted.
    assert!(RETIRED_SURFACE_MARKERS.contains(&"<lash_vm>"));
    assert!(RETIRED_SURFACE_MARKERS.contains(&"finish <value>"));
    assert_ne!(
        crate::dialect::TypescriptPrompts
            .prompt_vocabulary()
            .cell_tags
            .open,
        "<lash_vm>",
        "the sole vocabulary must not be the retired one"
    );
}

/// The authored example corpus, as tool authors spell it.
///
/// Copied deliberately rather than read from a live catalog: this is the shape
/// authors write, and the rewriter has to survive every one of them. The set
/// spans the current first-party plugins (process controls, the standard
/// protocol, the MCP web tools) plus retired tool families the rewriter must
/// still accept verbatim.
fn authored_tool_examples() -> Vec<&'static str> {
    vec![
        r#"await parallel.web_search_57jmhsdk2uvtc7o55qwq73syq({ query: "latest Rust release notes", limit: 5 })?"#,
        r#"await files.read({ path: "src/main.rs", offset: 1, limit: 120 })?"#,
        r#"await files.edit({ path: "src/main.rs", edits: [{ oldText: "old();", newText: "new();" }] })?"#,
        r#"await files.glob({ pattern: "**/*.rs", path: "crates/lash/src", limit: 50 })?"#,
        r#"await files.write({ path: "hello.txt", content: "hello\n" })?"#,
        r#"await jobs.run({ target: "//crates/lash-protocol-rlm:protocol_drivers__test", timeout_ms: 600000 })?"#,
        r#"probe = await files.stat({ path: "Cargo.lock" })?"#,
        r#"await jobs.start({ name: "daemon", args: ["--serve"], detach: true })?"#,
        r#"await jobs.send({ process_id: "call-job-1", message: "", close: true })?"#,
        r#"await processes.list({ status: "any" })?"#,
        r#"await processes.cancel({ process_id: "tool:call-01JZK7G4QP9Q4J7W3Q2E1H6M9C" })?"#,
        r#"await tools.batch({ tool_calls: [{ tool: "read_file", parameters: { path: "src/main.rs" } }] })?"#,
        r#"await tools.search({ query: "text checksum", limit: 3 })?"#,
        r#"await workbench_deferred.stats({ /* matching arguments */ })?"#,
    ]
}

async fn assembled_prompt_fragments_with_projection(
    dialect: &SessionDialect,
    projected_value: serde_json::Value,
) -> Vec<(&'static str, String)> {
    let vocabulary = dialect.prompt_vocabulary();
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
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_examples(vec![
        // Authored as LashVm, like every example in the resident catalog.
        // Six of seven in the shipped catalog carry the try-operator, which is
        // a syntax error in TypeScript.
        r#"await web.fetch({ url: "https://example.test/" })?"#.to_string(),
        "page = await web.fetch({ url: \"https://example.test/\" })?\nfinish page".to_string(),
    ])
    .with_tool_binding(lash_vm_runtime::ToolBinding::new(["web"], "fetch"));
    // A second member whose *shapes* are collections of records, like
    // `processes.list`. The first fixture's schema is one string field, so the
    // tool-docs fragment never rendered a collection or record type label and
    // the walker could not see what a TypeScript reader is shown for them.
    let listing = lash_core::ToolDefinition::raw(
        "tool:test/list_process_handles",
        "list_process_handles",
        "List process runs visible to this session",
        serde_json::json!({
            "type": "object",
            "properties": {
                "status": { "type": "string", "enum": ["running", "any"] },
                "definition": {
                    "type": "object",
                    "description": "A process definition value, for example `on_button`."
                }
            },
            "additionalProperties": false
        }),
        serde_json::json!({
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Process handle id." },
                    "descriptor": {
                        "type": "object",
                        "properties": { "kind": { "type": "string" } },
                        "additionalProperties": false
                    }
                },
                "required": ["id", "descriptor"],
                "additionalProperties": false
            }
        }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_examples(vec![
        r#"await processes.list({ status: "any" })?"#.to_string(),
    ])
    .with_tool_binding(lash_vm_runtime::ToolBinding::new(["processes"], "list"));
    // A third member shaped like an MCP import (FIG-4544): no
    // `additionalProperties`, nested object and array properties, constraints
    // and a field without a description. Every fixture above is a closed
    // object, which is the one shape the retired signature rows and the
    // widened `Record<string, unknown>` never showed up in.
    let imported = lash_core::ToolDefinition::raw(
        "tool:test/issues_search",
        "mcp__tracker__issues_search",
        "[MCP tracker] Search issues.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "minLength": 1, "description": "Search text." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 50, "default": 10 },
                "filter": {
                    "type": "object",
                    "properties": {
                        "state": { "enum": ["open", "closed"] },
                        "labels": { "type": "array", "items": { "type": "string" } }
                    }
                }
            },
            "required": ["query"]
        }),
        serde_json::json!({
            "type": "object",
            "properties": {
                "issues": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "integer", "description": "Stable issue id." },
                            "score": { "type": "number" }
                        },
                        "required": ["id"]
                    }
                }
            },
            "required": ["issues"]
        }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_tool_binding(lash_vm_runtime::ToolBinding::new(
        ["tracker"],
        "issues_search",
    ));
    let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![tool, listing, imported]);

    let mut fragments = vec![(
        "execution section",
        dialect
            .render_execution_section(
                crate::protocol::RlmPromptFeatures::default(),
                &catalog,
                crate::plugin::RlmChannel::Cell,
                None,
            )
            .expect("render execution section"),
    )];

    fragments.push((
        "tool docs",
        crate::tool_catalog::rlm_prompt_tool_docs(
            &catalog,
            dialect,
            crate::protocol::RlmPromptFeatures::default(),
        ),
    ));

    // The deferred-tool advertisement, which is prose a *lower* crate composes
    // (`catalogue_preview`) and a host states in its
    // prompt config. It is model-facing on every turn of any session with a
    // deferred catalogue, it takes no vocabulary, and neither this walker nor
    // the tool-prose gate saw it: a judged TypeScript session was advertised
    // `await tools.search({ query: "..." })?`, try-operator included.
    fragments.push((
        "deferred-tool advertisement",
        crate::catalogue_preview(
            [crate::CataloguePreviewEntry {
                module_path: vec!["workbench_deferred".to_string()],
                call: "stats".to_string(),
            }],
            &crate::CataloguePreviewOptions::default(),
        )
        .expect("one catalogued entry renders an advertisement"),
    ));

    // Bound variables, rendered through the dialect's **own session**, which is
    // the path a served turn uses. Calling `render_bound_variables` directly
    // with the right vocabulary would only prove the plumbing compiles: the
    // first version of this walker did exactly that and stayed green when the
    // TypeScript session was pointed back at Lash VM copy — the very bug it
    // exists to catch.
    let mut session = dialect.create_session();
    session
        .patch_globals(
            &lash_rlm_types::RlmGlobalsPatchPluginBody {
                // The second variable is too large to show inline, so its
                // row and its `Schema:` block render inferred shapes: a list,
                // a record and each scalar.
                set_default: [
                    (
                        "findings".to_string(),
                        serde_json::json!("summary of findings"),
                    ),
                    (
                        "samples".to_string(),
                        serde_json::Value::Array(
                            (0..40)
                                .map(|index| {
                                    serde_json::json!({
                                        "name": format!("sample-{index}"),
                                        "count": index,
                                        "ratio": 0.5,
                                        "ok": true,
                                        "tags": ["a"]
                                    })
                                })
                                .collect(),
                        ),
                    ),
                ]
                .into_iter()
                .collect(),
            },
            &std::collections::BTreeSet::new(),
        )
        .await
        .expect("seed one bound variable");
    fragments.push((
        "bound variables",
        session
            .prepare_bound_variables_prompt(
                &std::collections::BTreeSet::new(),
                lash_render::RenderParams::preview(),
            )
            .await
            .expect("bound variables prompt")
            .render()
            .to_string(),
    ));

    // Per-turn live extension handles are not part of the durable facade contract.
    let projected = crate::projection::CodeModeProjectedBindings::new()
        .bind_json("current_file", projected_value)
        .expect("seed one projected binding");
    fragments.push((
        "read-only variables",
        dialect
            .read_only_variables_prompt(&projected)
            .unwrap_or_default(),
    ));

    // The budget escalation tails, at each of the three thresholds.
    for used in [600usize, 950, 1_200] {
        let usage = lash_core::LlmUsage {
            input_tokens: used as i64,
            ..Default::default()
        };
        if let Some(suffix) = crate::rlm_support::format_budget_suffix_with_vocabulary(
            1,
            Some(&usage),
            Some(1_000),
            vocabulary,
            true,
        ) {
            fragments.push(("budget suffix", suffix));
        }
    }

    // Every copy the dialect owns for turn boundaries.
    // A host's own finish tool, named in place of `control.finish`.
    let host_finish = ["await tools.answer(…)".to_owned()];
    for (name, termination, finishing) in [
        (
            "finalization",
            lash_core::TerminationMode::TerminalRequired,
            &[][..],
        ),
        (
            "finalization (natural)",
            lash_core::TerminationMode::Natural,
            &[][..],
        ),
        (
            "finalization (host finish)",
            lash_core::TerminationMode::TerminalRequired,
            &host_finish[..],
        ),
    ] {
        fragments.push((
            name,
            dialect.finalization_copy(termination, finishing, crate::plugin::RlmChannel::Cell),
        ));
    }
    fragments.push(("history definition", dialect.history_item_definition(true)));
    fragments.push((
        "finish required",
        dialect.finish_required_copy(&[], crate::plugin::RlmChannel::Cell),
    ));
    fragments.push((
        "finish required (host finish)",
        dialect.finish_required_copy(&host_finish, crate::plugin::RlmChannel::Cell),
    ));
    fragments.push((
        "invalid cell retry",
        dialect.invalid_cell_retry_copy("no closing tag"),
    ));
    fragments.push(("output limit", dialect.output_limit_cell_copy(Some(2_048))));
    fragments.push((
        "malformed cell fence retry",
        dialect.malformed_cell_fence_retry_copy(),
    ));
    // `foreign_cell_retry_copy` is deliberately absent: its whole job is to name
    // the *other* dialect's tag, which is the marker this walker hunts.

    // Copy the *driver* assembles around the dialect's own fragments. These
    // reach the model on ordinary turns — a truncated history entry and an
    // output-limit retry are not edge cases — and were written when Lash VM
    // was the only dialect.
    fragments.push((
        "history preview notice",
        crate::driver::history::preview_retained_copy(vocabulary, "history[0].content"),
    ));
    fragments.push((
        "history output reference",
        crate::driver::history::preview_retained_copy(vocabulary, "history[0].output[0]"),
    ));
    fragments.push((
        "output limit retry",
        crate::protocol::finish::output_limit_retry_copy(vocabulary, Some(2_048)),
    ));
    fragments
}
