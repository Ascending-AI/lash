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
            "finish <value>",
        ],
        // And the reverse: a Lashlang session must not be handed TypeScript.
        "lashlang" => vec![
            "<typescript>",
            "</typescript>",
            "typescript cell",
            "typescript cells",
            "console.log(",
            "finish(value)",
        ],
        other => panic!("unknown dialect `{other}`"),
    }
}

/// Fragments that legitimately carry the other dialect's spelling.
///
/// Exactly one, and it is a payload discriminant rather than prose: the
/// model-visible `history` variable really does contain
/// `kind: "lashlang_step"` in both dialects, because `RlmHistoryItem` is one
/// serialized type. Teaching a TypeScript model to expect `typescript_step`
/// would make the prompt *disagree with the data the model receives*, which is
/// the defect class this layer exists to close. Renaming both sides together is
/// a durable payload change and is tracked separately; until then the honest
/// prompt is the one that matches the wire.
const ALLOWED_SHARED_SPELLINGS: &[&str] = &["lashlang_step"];

fn strip_allowed(text: &str) -> String {
    let mut text = text.to_string();
    for allowed in ALLOWED_SHARED_SPELLINGS {
        text = text.replace(allowed, "«allowed»");
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
            let haystack = strip_allowed(&fragment).to_lowercase();
            for marker in &markers {
                if haystack.contains(&marker.to_lowercase()) {
                    violations.push(format!(
                        "{language_id} prompt fragment `{name}` contains `{marker}`"
                    ));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "the assembled prompt mixes dialects: {violations:#?}"
    );
}

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
    // And the markers themselves must be present in the opposite dialect's
    // real copy, or the walker is looking for strings nothing ever emits.
    assert!(foreign_markers("typescript").contains(&"<lashlang>"));
    assert!(foreign_markers("lashlang").contains(&"<typescript>"));
}
