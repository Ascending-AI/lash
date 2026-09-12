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
//!
//! It cannot reach the *other* half of the prompt's tool docs. A tool's
//! description and schema prose are authored in the crate that owns the tool and
//! rendered verbatim, so this walker's synthetic fixture proves nothing about
//! what `lash-subagents` or a host plugin actually wrote — and three `lashlang`
//! strings reached TypeScript sessions through exactly that gap. The sibling
//! gate is `tool_catalog::validate_dialect_neutral_tool_prose`, which sweeps
//! whatever a session registers at registration time instead of a fixture here.

use super::*;
use crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY;
use crate::dialect::typescript::TYPESCRIPT_PROMPT_VOCABULARY;
use lash_lashlang_runtime::ToolDefinitionBindingExt as _;

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
        r#"await shell.exec({ cmd: "cargo test -p lash-protocol-rlm", timeout_ms: 600000 })?"#,
        "probe = await shell.exec({ cmd: \"test -f Cargo.lock\" })?\nfinish probe.exit_code == 0",
        r#"await shell.start({ cmd: "nohup ./daemon --serve", detach: true })?"#,
        r#"await shell.write({ process_id: "call-shell-1", chars: "", close_stdin: true })?"#,
        r#"await processes.list({ status: "any" })?"#,
        r#"await processes.cancel({ process_id: "tool:call-01JZK7G4QP9Q4J7W3Q2E1H6M9C" })?"#,
        r#"await tools.batch({ tool_calls: [{ tool: "read_file", parameters: { path: "src/main.rs" } }] })?"#,
        r#"await tools.search({ query: "text checksum", limit: 3 })?"#,
        r#"await workbench_deferred.stats({ /* matching arguments */ })?"#,
    ]
}

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
            // declared in and both dialects have to render. The bound-variable
            // block renders a whole type line in it, not one token: pinning
            // only `list[` described the leak as narrower than it is.
            "list[",
            // Written with the trailing punctuation so they cannot match
            // TypeScript's own `: string` / `: number`.
            ": str,",
            ": int,",
            "?: any |",
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
/// dialect's spelling. ADR 0063 holds the rule and the whole list; this is the
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
/// 0063 hides it from the prompt instead of carving it out.
/// The second carve-out is a durable process identity. A TypeScript session's
/// processes are compiled against the Lashlang VM substrate and their ids are
/// `process:lashlang:v2:blake3:…` — journal identity, visible to a host through
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
    assembled_prompt_fragments_with_projection(dialect, serde_json::json!("src/lib.rs"))
}

fn assembled_prompt_fragments_with_projection(
    dialect: &dyn RlmDialect,
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
    .with_examples(vec![
        // Authored as Lashlang, like every example in the resident catalog.
        // Six of seven in the shipped catalog carry the try-operator, which is
        // a syntax error in TypeScript.
        r#"await web.fetch({ url: "https://example.test/" })?"#.to_string(),
        "page = await web.fetch({ url: \"https://example.test/\" })?\nfinish page".to_string(),
    ])
    .with_tool_binding(lash_lashlang_runtime::ToolBinding::new(["web"], "fetch"));
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
    .with_examples(vec![
        r#"await processes.list({ status: "any" })?"#.to_string(),
    ])
    .with_tool_binding(lash_lashlang_runtime::ToolBinding::new(
        ["processes"],
        "list",
    ));
    let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![tool, listing]);

    let mut fragments = vec![(
        "execution section",
        dialect
            .render_execution_section(crate::protocol::RlmPromptFeatures::default(), &catalog)
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
    // (`lash_lashlang_runtime::catalogue_preview`) and a host contributes to the
    // execution section. It is model-facing on every turn of any session with a
    // deferred catalogue, it takes no vocabulary, and neither this walker nor
    // the tool-prose gate saw it: a judged TypeScript session was advertised
    // `await tools.search({ query: "..." })?`, try-operator included.
    fragments.push((
        "deferred-tool advertisement",
        lash_lashlang_runtime::catalogue_preview_contribution_for_entries([
            lash_lashlang_runtime::CataloguePreviewEntry {
                module_path: vec!["workbench_deferred".to_string()],
                call: "stats".to_string(),
            },
        ])
        .expect("one catalogued entry renders an advertisement")
        .content
        .to_string(),
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

    // Read-only variables use the protocol session's vocabulary. Per-turn live
    // extension handles are not part of the durable facade contract.
    let projected = crate::projection::RlmProjectedBindings::new()
        .bind_json("current_file", projected_value)
        .expect("seed one projected binding");
    fragments.push((
        "read-only variables",
        crate::projection::RlmProjectionExtension::prompt_contributions_for(&projected, vocabulary)
            .into_iter()
            .map(|contribution| contribution.content.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
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
            true,
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
    fragments.push((
        "finalization (schema)",
        dialect.finalization_copy(&lash_rlm_types::RlmTermination::FinishRequired {
            schema: Some(serde_json::json!({"type": "number"})),
        }),
    ));
    fragments.push(("finish required", dialect.finish_required_copy(false)));
    fragments.push(("finish schema", dialect.finish_required_copy(true)));
    fragments.push(("schema mismatch", dialect.finish_schema_mismatch_copy()));
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

// FIG-2750 closes the fixture's tool-signature and always-rendered history
// residuals. Structured history and large projected shapes still use the shared
// schema vocabulary; this simple-global fixture must have no dialect leaks.
const KNOWN_TYPE_SYNTAX_RESIDUALS: &[&str] = &[];

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
    // RV-3: a rendered example must be *parseable* TypeScript, not merely free
    // of foreign markers. The marker walk cannot tell "reads like TypeScript"
    // from "is a syntax error", and a syntax error in an example is exactly the
    // defect the examples fix exists to close. Parsed rather than linked: an
    // example names host modules and free identifiers that no isolated
    // environment has, so `TS_UNKNOWN_BINDING` is expected and a *syntax* error
    // is not.
    let typescript = crate::dialect::typescript_test_dialect();
    let mut unparseable = Vec::new();
    for example in authored_tool_examples() {
        let rendered = typescript.render_tool_example(example);
        if let Err(error) = lash_typescript::parse(&rendered) {
            let code = format!("{:?}", error.code);
            if code.contains("UnknownBinding") || code.contains("LinkError") {
                continue;
            }
            unparseable.push(format!("`{example}` → `{rendered}`: {error}"));
        }
    }
    assert!(
        unparseable.is_empty(),
        "rendered examples that are not TypeScript: {unparseable:#?}"
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

/// Every construct family the dialect accepts is mentioned somewhere in the
/// assembled TypeScript prompt.
///
/// The standard-library section is generated from the signature table, so a new
/// method reaches the prompt by construction. The *hand-written* sections are
/// where drift lives: async helpers, the `URL`/`URLSearchParams` constructors,
/// and the widened `instanceof` targets were all shipped and accepted while the
/// prose still described the surface without them — a model reading this prompt
/// would not have written any of the three. Nothing failed, because nothing was
/// looking.
///
/// The check is deliberately coarse: one family, a few tokens, at least one of
/// which must appear. It cannot verify the prose is *good*; it can only make
/// silent omission impossible. The list is explicit and maintained — widening
/// FIG-2750 supersedes exhaustive syntax teaching with a compact library list.
// FIG-2750: ordinary TypeScript syntax is learned from diagnostics; only the
// supported library families and host execution rules belong in the prompt.
#[test]
fn typescript_teaches_library_families_without_exhaustive_inventory() {
    let prompt = assembled_prompt_fragments(&crate::dialect::typescript_test_dialect())
        .into_iter()
        .map(|(_, text)| text)
        .collect::<Vec<_>>()
        .join("\n");
    for name in [
        "Math", "Date", "String", "Array", "Object", "JSON", "Map", "Set", "RegExp", "URL",
        "finish(",
    ] {
        assert!(prompt.contains(name), "{name}");
    }
    assert!(!prompt.contains("### v1 guardrails"));
    assert!(!prompt.contains("### Deterministic standard library"));
}

#[test]
fn composed_typescript_prompt_has_no_markdown_fences() {
    let mut resources = ::lashlang::LashlangHostCatalog::new();
    resources
        .add_trigger_source_constructor(
            ["cron", "Schedule"],
            ::lashlang::TypeExpr::Object(vec![::lashlang::TypeField {
                name: "expr".into(),
                ty: ::lashlang::TypeExpr::Str,
                optional: false,
            }]),
            ::lashlang::NamedDataType::object(
                "cron.Tick",
                vec![::lashlang::TypeField {
                    name: "fired_at".into(),
                    ty: ::lashlang::TypeExpr::Str,
                    optional: false,
                }],
            )
            .expect("tick type"),
        )
        .expect("trigger constructor");
    let dialect = super::TypescriptDialect::new(
        lash_lashlang_runtime::LashlangSurface {
            abilities: ::lashlang::LashlangAbilities::all(),
            language_features: Default::default(),
            resources,
        },
        super::test_dialect_services(),
    );
    // The full-assembly fixture includes tool signatures, contracts, examples,
    // host operations, and both natural and finish-required finalization.
    for (name, fragment) in assembled_prompt_fragments_with_projection(
        &dialect,
        serde_json::json!({"path": "src/lib.rs", "lines": [1, 2]}),
    ) {
        if name == "execution section" {
            assert!(fragment.contains("type cron_Tick ="));
            assert!(fragment.contains("cron.Schedule(input:"));
            // Response shape opens execution directly; no redundant language sentence.
            assert!(fragment.contains("### Response shape"));
        }
        assert!(
            !fragment.contains("```"),
            "TypeScript prompt fragment `{name}` contains a Markdown fence"
        );
    }
}
