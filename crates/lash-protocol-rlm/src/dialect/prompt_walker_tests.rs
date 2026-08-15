//! The full-assembly prompt walker.
//!
//! Two narrower walkers already exist and both passed while the product was
//! broken: `every_diagnostic_code_named_in_the_prompt_exists` reads the
//! execution section's `TS_` codes, and
//! `no_diagnostic_from_a_prompt_primitive_names_a_lashlang_identifier` reads
//! the diagnostics its primitives emit. Neither looks at the *rest* of the
//! prompt, and the rest of the prompt is where the leak was: a TypeScript
//! session was told to write `<typescript>` cells by its execution section and,
//! a few hundred tokens later, that its variables were "already bound in
//! lashlang" and should be accessed "in `<lashlang>` blocks". The judged
//! battery caught a model spending reasoning tokens trying to reconcile the
//! two.
//!
//! This walks every fragment the crate contributes to an assembled prompt, for
//! both dialects, and fails on any word that belongs to the other one.

use super::*;
use crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY;
use crate::dialect::typescript::TYPESCRIPT_PROMPT_VOCABULARY;
use lash_lashlang_runtime::ToolDefinitionLashlangExt as _;

/// Text that names the *other* dialect, with the reason each token is a defect.
fn foreign_markers(language_id: &str) -> Vec<&'static str> {
    match language_id {
        // A TypeScript session must never see Lashlang's cell tag, its
        // language name in prose, or its statement syntax.
        "typescript" => vec![
            "<lashlang>",
            "</lashlang>",
            "lashlang block",
            "lashlang blocks",
            "bound in lashlang",
            "`print ",
            "re-print",
            "finish <value>",
            // Lashlang's *type* syntax, which the host-surface inventory is
            // declared in and both dialects have to render.
            "list[",
            "-> str",
            // The Lashlang try-operator, in an authored tool example.
            ")?",
            "-> float",
            "trigger.register",
        ],
        // And the reverse: a Lashlang session must not be handed TypeScript.
        // The last two are the *substrate* direction: internal identifiers the
        // TypeScript lowerer needs are bound into every Lashlang host, and the
        // host-environment section advertised them to a Lashlang reader.
        "lashlang" => vec![
            "<typescript>",
            "</typescript>",
            "typescript cell",
            "typescript cells",
            "console.log(",
            "finish(value)",
            "per cell",
            "__typescript_runtime",
            "typescript.runtime",
        ],
        other => panic!("unknown dialect `{other}`"),
    }
}

/// Substrate identifiers that may appear in a prompt written in the other
/// dialect's spelling. ADR 0060 holds the rule and the whole list; this is the
/// executable half of it.
///
/// Exactly one qualifies, and it is a payload discriminant rather than prose:
/// the model-visible `history` variable really does contain
/// `kind: "lashlang_step"` in both dialects, because `RlmHistoryItem` is one
/// serialized type and its event ids (`lashlang_step_<turn>_<iteration>`,
/// `protocol/driver.rs`) are durable session-graph identifiers. Teaching a
/// TypeScript model to expect `typescript_step` would make the prompt
/// *disagree with the data the model receives*, which is the defect class this
/// layer exists to close. Renaming both sides together is a durable payload
/// change and is tracked separately; until then the honest prompt is the one
/// that matches the wire.
///
/// The other durable cross-dialect identifier, the `__typescript_runtime`
/// module the TypeScript lowerer resolves `Date.now()`/`Math.random()` through,
/// is deliberately **not** here: nothing about it has to reach a model, so ADR
/// 0060 hides it from the prompt instead of carving it out.
/// The second carve-out is a durable process identity. A TypeScript session's
/// processes are compiled against the Lashlang VM substrate and their ids are
/// `process:lashlang:sha256:…` — journal identity, visible to a host through
/// `/api/work`. Renaming them would move a durable id for a cosmetic gain, so
/// the id stays and the *label* half of the same defect is what got fixed
/// (transcript badges read the recorded dialect).
const SUBSTRATE_CARVE_OUTS: &[&str] = &[
    "lashlang_step",
    "process:lashlang:",
    // Effect ids are `lashlang:effect:<session>:<turn>:…` in every dialect:
    // the engine identity in a durable journal key. Same ruling as the process
    // id — durable, so carved out rather than renamed.
    "lashlang:effect:",
];

fn strip_carve_outs(text: &str) -> String {
    let mut text = text.to_string();
    for allowed in SUBSTRATE_CARVE_OUTS {
        text = text.replace(allowed, "«substrate carve-out»");
    }
    text
}

fn assembled_prompt_fragments(dialect: &dyn RlmDialect) -> Vec<(&'static str, String)> {
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
    .with_examples(vec![
        // Authored as Lashlang, like every example in the resident catalog.
        // Six of seven in the shipped catalog carry the try-operator, which is
        // a syntax error in TypeScript.
        r#"await web.fetch({ url: "https://example.test/" })?"#.to_string(),
        "page = await web.fetch({ url: \"https://example.test/\" })?\nfinish page".to_string(),
    ])
    .with_lashlang_binding(lash_lashlang_runtime::LashlangToolBinding::new(
        ["web"],
        "fetch",
    ));
    let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![tool]);

    let mut fragments = vec![(
        "execution section",
        dialect
            .render_execution_section(crate::protocol::RlmPromptFeatures::default(), &catalog)
            .expect("render execution section"),
    )];

    fragments.push((
        "tool docs",
        crate::tool_catalog::rlm_prompt_tool_docs(&catalog, dialect),
    ));

    // Bound variables, rendered through the dialect's **own session**, which is
    // the path a served turn uses. Calling `render_bound_variables` directly
    // with the right vocabulary would only prove the plumbing compiles: the
    // first version of this walker did exactly that and stayed green when the
    // TypeScript session was pointed back at Lashlang copy — the very bug it
    // exists to catch.
    let mut session = dialect
        .create_session()
        .expect("dialect session for the bound-variables path");
    session
        .patch_globals(
            &lash_rlm_types::RlmGlobalsPatchPluginBody {
                set_default: [(
                    "findings".to_string(),
                    serde_json::json!("summary of findings"),
                )]
                .into_iter()
                .collect(),
            },
            &std::collections::BTreeSet::new(),
        )
        .expect("seed one bound variable");
    fragments.push((
        "bound variables",
        session
            .prepare_bound_variables_prompt(&std::collections::BTreeSet::new())
            .expect("bound variables prompt")
            .render()
            .to_string(),
    ));

    // The budget escalation tails, at each of the three thresholds.
    for used in [600usize, 950, 1_200] {
        let usage = lash_core::PromptUsage {
            context_budget_tokens: used,
            ..Default::default()
        };
        if let Some(suffix) = crate::rlm_support::format_budget_suffix_with_vocabulary(
            1,
            Some(&usage),
            Some(1_000),
            vocabulary,
        ) {
            fragments.push(("budget suffix", suffix));
        }
    }

    // Every copy the dialect owns for turn boundaries.
    fragments.push((
        "finalization",
        dialect
            .finalization_copy(&lash_rlm_types::RlmTermination::FinishRequired { schema: None })
            .to_string(),
    ));
    fragments.push((
        "finalization (natural)",
        dialect
            .finalization_copy(&lash_rlm_types::RlmTermination::Natural)
            .to_string(),
    ));
    fragments.push(("turn limit", dialect.turn_limit_final_copy(8)));
    fragments.push(("finish required", dialect.finish_required_copy(false)));
    fragments.push(("finish schema", dialect.finish_required_copy(true)));
    fragments.push(("schema mismatch", dialect.finish_schema_mismatch_copy()));
    fragments.push((
        "invalid cell retry",
        dialect.invalid_cell_retry_copy("no closing tag"),
    ));
    fragments.push(("output limit", dialect.output_limit_cell_copy(Some(2_048))));

    // Copy the *driver* assembles around the dialect's own fragments. These
    // reach the model on ordinary turns — a truncated history entry and an
    // output-limit retry are not edge cases — and were written when Lashlang
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

#[test]
fn no_assembled_prompt_fragment_carries_the_other_dialects_words() {
    let dialects: Vec<std::sync::Arc<dyn RlmDialect>> = vec![
        std::sync::Arc::new(crate::dialect::lashlang_test_dialect()),
        std::sync::Arc::new(crate::dialect::typescript_test_dialect()),
    ];

    let mut violations = Vec::new();
    for dialect in &dialects {
        let language_id = dialect.language_id();
        let markers = foreign_markers(language_id);
        for (name, fragment) in assembled_prompt_fragments(dialect.as_ref()) {
            let haystack = strip_carve_outs(&fragment).to_lowercase();
            for marker in &markers {
                if haystack.contains(&marker.to_lowercase()) {
                    violations.push(format!(
                        "{language_id} prompt fragment `{name}` contains `{marker}`"
                    ));
                }
            }
        }
    }
    violations.sort();
    let residuals = KNOWN_TYPE_SYNTAX_RESIDUALS
        .iter()
        .map(|residual| (*residual).to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        violations, residuals,
        "the assembled prompt mixes dialects, or a known residual changed. \
         Fixing one means deleting its row from KNOWN_TYPE_SYNTAX_RESIDUALS."
    );
}

/// The leaks this round measured but did not close, named rather than hidden.
///
/// Both are the *type* syntax of the host's declarations, not prose, and both
/// come from renderers that live outside this crate's dialect seam: tool docs
/// are rendered by `lash_core`'s contract renderer
/// (`compact_contract_with_signature_name(...).render_markdown()`), and the
/// bound-variable block builds its own shape language (`list[...]`,
/// `HistoryItem`) from a schema registry. Teaching either to speak a second
/// dialect is a typed-rendering change across crates, not a copy edit, so it is
/// recorded here instead of being half-done.
///
/// This list is asserted *exactly*: a third leak fails the walker, and closing
/// one of these fails it too until its row is deleted. That is the point — a
/// residual that cannot be forgotten is different in kind from one that is
/// merely known.
const KNOWN_TYPE_SYNTAX_RESIDUALS: &[&str] = &[
    "typescript prompt fragment `bound variables` contains `list[`",
    "typescript prompt fragment `tool docs` contains `-> str`",
];

/// The walker only measures if its marker list can fire. Both vocabularies are
/// asserted to be genuinely different so a future refactor cannot make the
/// check vacuous by collapsing them.
#[test]
fn the_two_vocabularies_are_actually_different() {
    assert_ne!(
        LASHLANG_PROMPT_VOCABULARY.cell_open_tag,
        TYPESCRIPT_PROMPT_VOCABULARY.cell_open_tag
    );
    assert_ne!(
        LASHLANG_PROMPT_VOCABULARY.print_call,
        TYPESCRIPT_PROMPT_VOCABULARY.print_call
    );
    assert_ne!(
        LASHLANG_PROMPT_VOCABULARY.finish_statement,
        TYPESCRIPT_PROMPT_VOCABULARY.finish_statement
    );
    // Non-vacuity for the example surface: the *authored* corpus really does
    // carry the try-operator, so the marker has something to catch. Reading it
    // from the Lashlang rendering proves the fixture, not the assertion.
    let example_fixture = assembled_prompt_fragments(&crate::dialect::lashlang_test_dialect())
        .into_iter()
        .find(|(name, _)| *name == "tool docs")
        .expect("tool docs fragment");
    assert!(
        example_fixture.1.contains(")?"),
        "the fixture's authored examples must carry the try-operator: {}",
        example_fixture.1
    );

    // The example surface specifically: a TypeScript reader must be shown the
    // examples rewritten, and the Lashlang reader must still get the original.
    let typescript = crate::dialect::typescript_test_dialect();
    assert_eq!(
        typescript.render_tool_example(r#"await web.fetch({ url: "https://example.test/" })?"#),
        r#"await web.fetch({ url: "https://example.test/" });"#
    );
    assert_eq!(
        typescript.render_tool_example("page = await web.fetch({ url: \"u\" })?\nfinish page"),
        "const page = await web.fetch({ url: \"u\" });\nfinish(page);"
    );
    let lashlang = crate::dialect::lashlang_test_dialect();
    assert_eq!(
        lashlang.render_tool_example("finish page"),
        "finish page",
        "the authored form is Lashlang's own and must pass through untouched"
    );

    // And the markers themselves must be present in the opposite dialect's
    // real copy, or the walker is looking for strings nothing ever emits.
    assert!(foreign_markers("typescript").contains(&"<lashlang>"));
    assert!(foreign_markers("lashlang").contains(&"<typescript>"));
}
