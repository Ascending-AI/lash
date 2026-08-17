use lash_core::plugin::{PluginError, ToolCatalogContext};
use lash_core::{ToolActivation, ToolCatalog, facade_support::ToolCatalogContribution};
use lash_lashlang_runtime::{
    required_tool_lashlang_executable, required_tool_typescript_executable,
};

use crate::dialect::{RlmDialectRegistry, TOOL_PROSE_TOKENS, dialect_identity_markers};

/// RLM catalog assembly. The catalog is a flat callable set: every member is
/// rendered as a full prompt doc under its Lashlang call-path. RLM contributes
/// no removals; it validates that each member carries explicit `lashlang.tool`
/// and `typescript.tool` bindings so either dialect can call it by module path,
/// and that no member's model-facing prose spells a dialect out literally.
pub(crate) fn rlm_tool_catalog(
    ctx: ToolCatalogContext,
    dialects: &RlmDialectRegistry,
) -> Result<ToolCatalogContribution, PluginError> {
    validate_rlm_language_bindings(&ctx)?;
    validate_dialect_neutral_tool_prose(&ctx, dialects)?;
    Ok(ToolCatalogContribution::default())
}

/// Render every catalog member as a full prompt doc under **this dialect's**
/// call path. Being a member *is* being presented.
///
/// This used to render every doc under the Lashlang path unconditionally, so a
/// TypeScript session was handed a tool list it could not call: the typed
/// declarations in its execution section said one thing and the doc block said
/// another. Registration already requires both bindings on every non-internal
/// tool, so the dialect's path is always available.
pub(crate) fn rlm_prompt_tool_docs(
    tool_catalog: &ToolCatalog,
    dialect: &dyn crate::dialect::RlmDialect,
) -> String {
    let vocabulary = dialect.prompt_vocabulary();
    tool_catalog
        .tools
        .iter()
        .filter(|tool| tool.manifest.activation != ToolActivation::Internal)
        .filter_map(|tool| {
            let contract = tool_catalog.resolve_contract(&tool.manifest.name)?;
            let call_path = dialect
                .tool_call_path(&tool.manifest)
                .expect("RLM tool catalog registration validates both dialects' bindings");
            let mut compact =
                contract.compact_contract_with_signature_name(&tool.manifest, &call_path);
            // Authored examples are Lashlang source; the dialect spells them.
            compact.examples = compact
                .examples
                .iter()
                .map(|example| dialect.render_tool_example(example))
                .collect();
            // And authored prose resolves its dialect tokens against the
            // session's own vocabulary, the same way. The doc block is the
            // measured leak site: a TypeScript session's saved system prompt
            // carried `agents.spawn`'s Lashlang wording on three lines.
            compact.description = vocabulary.render_tool_prose(&compact.description);
            render_doc_field_prose(vocabulary, &mut compact.parameters);
            render_doc_field_prose(vocabulary, &mut compact.return_fields);
            Some(compact.render_markdown())
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Resolve the dialect tokens in one rendered doc row's `description`.
fn render_doc_field_prose(
    vocabulary: crate::dialect::DialectPromptVocabulary,
    rows: &mut [serde_json::Value],
) {
    for row in rows {
        let Some(description) = row.get("description").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let rendered = vocabulary.render_tool_prose(description);
        if let Some(object) = row.as_object_mut() {
            object.insert("description".to_string(), serde_json::json!(rendered));
        }
    }
}

/// One authored string a tool contributes to a prompt doc, and whether the doc
/// renderer will resolve a prose token in it.
struct AuthoredProse {
    site: &'static str,
    text: String,
    /// True when this exact string is one the renderer copies into a doc row and
    /// therefore token-resolves. A dialect word is a defect either way; a *token*
    /// is only meaningful where it gets resolved.
    token_resolved: bool,
}

/// Every model-facing string one tool contributes to a prompt doc.
///
/// Descriptions are swept at any depth of either schema, and so are the authored
/// literals the doc lines render (`default`, `enum`): those reach the model as
/// `in "a"|"b"` / `= "x"` fragments, which makes them prose whatever the schema
/// calls them.
///
/// Examples are excluded on purpose: they are code, they are already respelled
/// by [`crate::dialect::RlmDialect::render_tool_example`], and the Lashlang
/// try-operator every authored example carries would make a dialect-word sweep
/// fire on all of them.
///
/// Resolvability is answered by the renderer itself rather than by a rule
/// restating it: the strings a doc row carries are read out of the compact
/// contract this tool would render, so the guard cannot drift from
/// `schema_docs.rs`. It has to be asked, because the renderer's reach is
/// uneven — input rows come from the schema's *top-level* `properties` only,
/// while return fields are collected recursively. A token in the deep input
/// position is a trap either way: today it renders nowhere at all, and the
/// author who wrote it believes the model reads a hint.
fn model_facing_tool_prose(
    manifest: &lash_core::ToolManifest,
    contract: Option<&lash_core::ToolContract>,
) -> Vec<AuthoredProse> {
    let mut prose = Vec::new();
    if !manifest.description.trim().is_empty() {
        prose.push(AuthoredProse {
            site: "description",
            text: manifest.description.clone(),
            token_resolved: true,
        });
    }
    let Some(contract) = contract else {
        return prose;
    };
    let rendered = rendered_doc_strings(manifest, contract);
    let mut authored = Vec::new();
    collect_schema_prose(
        "input schema",
        contract.input_schema.canonical(),
        &mut authored,
    );
    collect_schema_prose(
        "output schema",
        contract.output_schema.canonical(),
        &mut authored,
    );
    for (site, kind, text) in authored {
        prose.push(AuthoredProse {
            site,
            token_resolved: kind == SchemaProseKind::Description && rendered.contains(&text),
            text,
        });
    }
    prose
}

/// The strings this tool's rendered doc rows carry, before token resolution.
fn rendered_doc_strings(
    manifest: &lash_core::ToolManifest,
    contract: &lash_core::ToolContract,
) -> std::collections::BTreeSet<String> {
    let compact = contract.compact_contract(manifest);
    compact
        .parameters
        .iter()
        .chain(compact.return_fields.iter())
        .filter_map(|row| row.get("description").and_then(serde_json::Value::as_str))
        .map(str::to_string)
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SchemaProseKind {
    Description,
    /// An authored literal (`default`, `enum`) that doc lines render inline. A
    /// token here is never resolved, so it may only ever be dialect-neutral.
    Literal,
}

/// Every authored string anywhere in one JSON Schema.
fn collect_schema_prose(
    site: &'static str,
    schema: &serde_json::Value,
    out: &mut Vec<(&'static str, SchemaProseKind, String)>,
) {
    match schema {
        serde_json::Value::Object(object) => {
            for (key, value) in object {
                match (key.as_str(), value) {
                    ("description", serde_json::Value::String(text)) if !text.trim().is_empty() => {
                        out.push((site, SchemaProseKind::Description, text.clone()));
                    }
                    ("default" | "enum" | "const", value) => {
                        collect_literal_strings(site, value, out);
                    }
                    _ => collect_schema_prose(site, value, out),
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_schema_prose(site, item, out);
            }
        }
        _ => {}
    }
}

/// Every string inside one authored literal value.
fn collect_literal_strings(
    site: &'static str,
    value: &serde_json::Value,
    out: &mut Vec<(&'static str, SchemaProseKind, String)>,
) {
    match value {
        serde_json::Value::String(text) if !text.trim().is_empty() => {
            out.push((site, SchemaProseKind::Literal, text.clone()));
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_literal_strings(site, item, out);
            }
        }
        serde_json::Value::Object(object) => {
            for item in object.values() {
                collect_literal_strings(site, item, out);
            }
        }
        _ => {}
    }
}

/// Refuse a catalog whose members spell a dialect out in model-facing prose.
///
/// The narrower dialect-leak guards are all *renderer*-side: the prompt walker
/// sweeps the fragments this crate renders, and every one of them reads its
/// words from a [`crate::dialect::DialectPromptVocabulary`]. Tool prose does
/// not: it is authored in the crate that owns the tool — `lash-subagents`,
/// `lash-plugin-process-controls`, any host plugin — and rendered verbatim into
/// the doc block, so no vocabulary sits between the author and the model. That
/// is how `agents.spawn` told TypeScript sessions their seeds had a "lashlang
/// source root".
///
/// The rule is *neutrality*, not foreignness: one authored string is served to
/// sessions of every registered dialect, so naming any dialect is wrong even in
/// that dialect's own session. Prose that genuinely needs a dialect word writes
/// a [`TOOL_PROSE_TOKENS`] token and lets the session's vocabulary spell it,
/// which is also why an unrecognized token is rejected here — a misspelled one
/// would otherwise reach the model raw.
pub(crate) fn validate_dialect_neutral_tool_prose(
    ctx: &ToolCatalogContext,
    dialects: &RlmDialectRegistry,
) -> Result<(), PluginError> {
    let markers: Vec<(&'static str, Vec<String>)> = dialects
        .dialects()
        .map(|dialect| {
            (
                dialect.language_id(),
                dialect_identity_markers(dialect.as_ref()),
            )
        })
        .collect();
    // Every violation, not the first: a host fixing its plugin should see the
    // whole list once instead of rediscovering it one failed session at a time.
    let mut violations = Vec::new();
    for tool in &ctx.tools {
        if tool.activation == ToolActivation::Internal {
            continue;
        }
        let contract = ctx
            .resolve_contract
            .as_ref()
            .and_then(|resolve| resolve(&tool.name));
        for prose in model_facing_tool_prose(tool, contract.as_deref()) {
            let AuthoredProse {
                site,
                text,
                token_resolved,
            } = prose;
            let haystack = text.to_lowercase();
            for (language_id, markers) in &markers {
                for marker in markers {
                    if haystack.contains(marker) {
                        violations.push(format!(
                            "tool `{name}` names the `{language_id}` dialect in its {site} \
                             (`{marker}`)",
                            name = tool.name,
                        ));
                    }
                }
            }
            for token in prose_token_occurrences(&text) {
                match token {
                    ProseTokenOccurrence::Known(_) if token_resolved => {}
                    ProseTokenOccurrence::Known(token) => violations.push(format!(
                        "tool `{name}` writes `{token}` in its {site}, in a position the tool-doc \
                         renderer never resolves (input rows are read from the schema's top-level \
                         `properties`; literals and nested input fields are not), so the token \
                         cannot reach the model as words",
                        name = tool.name,
                    )),
                    ProseTokenOccurrence::Unknown(token) => violations.push(format!(
                        "tool `{name}` writes `{token}` in its {site}, which is not an RLM prose \
                         token",
                        name = tool.name,
                    )),
                    ProseTokenOccurrence::Unclosed(snippet) => violations.push(format!(
                        "tool `{name}` writes an unclosed `{{{{` token in its {site} \
                         (`{snippet}`); nothing resolves it and it reaches the model verbatim",
                        name = tool.name,
                    )),
                }
            }
        }
    }
    if violations.is_empty() {
        return Ok(());
    }
    Err(PluginError::Registration(format!(
        "model-facing tool prose is served to every registered dialect, so it must be \
         dialect-neutral: drop the word, or write one of the RLM prose tokens ({tokens}) and let \
         the session's dialect spell it. {count} violation(s): {list}",
        tokens = prose_token_list(),
        count = violations.len(),
        list = violations.join("; "),
    )))
}

fn prose_token_list() -> String {
    TOOL_PROSE_TOKENS
        .iter()
        .map(|(token, _)| *token)
        .collect::<Vec<_>>()
        .join(", ")
}

/// One `{{…}}` occurrence in authored prose.
enum ProseTokenOccurrence {
    /// A token [`TOOL_PROSE_TOKENS`] resolves. Whether that is *allowed* depends
    /// on the position, which the caller knows and this scanner does not.
    Known(String),
    Unknown(String),
    /// A `{{` with no `}}` after it — the typo that motivated scanning every
    /// occurrence instead of stopping at the first unresolvable one. It renders
    /// verbatim, and an earlier version of this scanner answered `None` for the
    /// whole string when it saw one, abandoning everything written after it.
    Unclosed(String),
}

/// Every `{{…}}` occurrence in `text`, in order.
///
/// An unclosed open brace ends the token but not the scan: whatever follows it
/// is still authored prose, and the dialect word a host writes three sentences
/// later is not excused by a typo three sentences earlier.
fn prose_token_occurrences(text: &str) -> Vec<ProseTokenOccurrence> {
    const UNCLOSED_SNIPPET_CHARS: usize = 32;
    let mut occurrences = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        let tail = &rest[start..];
        match tail.find("}}") {
            Some(close) => {
                let end = close + 2;
                let token = &tail[..end];
                occurrences.push(
                    if TOOL_PROSE_TOKENS.iter().any(|(known, _)| *known == token) {
                        ProseTokenOccurrence::Known(token.to_string())
                    } else {
                        ProseTokenOccurrence::Unknown(token.to_string())
                    },
                );
                rest = &tail[end..];
            }
            None => {
                let snippet = tail
                    .chars()
                    .take(UNCLOSED_SNIPPET_CHARS)
                    .collect::<String>();
                occurrences.push(ProseTokenOccurrence::Unclosed(snippet));
                rest = &tail["{{".len()..];
            }
        }
    }
    occurrences
}

fn validate_rlm_language_bindings(ctx: &ToolCatalogContext) -> Result<(), PluginError> {
    for tool in &ctx.tools {
        if tool.activation == ToolActivation::Internal {
            continue;
        }
        required_tool_lashlang_executable(tool)
            .map_err(|err| PluginError::Registration(err.to_string()))?;
        let typescript = required_tool_typescript_executable(tool)
            .map_err(|err| PluginError::Registration(err.to_string()))?;
        // Being a catalog member is being advertised, and the TypeScript
        // execution section advertises the binding's call path as a typed
        // declaration the model calls verbatim. A path the dialect resolves to
        // anything but a tool call — a module segment no cell can write, an ECMA
        // global namespace, a refused method name — can only be advertised as a
        // callable nothing, so it is refused here instead (FIG-1444).
        let call_path = typescript.call_path();
        lash_typescript::ensure_tool_call_path_addressable(&call_path).map_err(|err| {
            PluginError::Registration(format!(
                "tool `{}` has a `typescript.tool` binding no TypeScript cell can call as `{call_path}`: {err}",
                tool.name
            ))
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::lashlang_test_dialect;
    use lash_core::{
        ToolContract, ToolDefinition, facade_support::ToolCatalogBuildInput,
        facade_support::build_tool_catalog,
    };
    use lash_lashlang_runtime::{LashlangSurface, LashlangToolBinding, ToolDefinitionLashlangExt};
    use serde_json::json;
    use std::sync::Arc;

    /// Both shipped dialects, the way a session registers them.
    ///
    /// The prose guard is registry-driven on purpose: it has to reject every
    /// registered dialect's words, not just the inactive one's.
    fn test_dialect_registry() -> RlmDialectRegistry {
        RlmDialectRegistry::new([
            Arc::new(lashlang_test_dialect()) as Arc<dyn crate::dialect::RlmDialect>,
            Arc::new(crate::dialect::typescript_test_dialect()),
        ])
    }

    #[test]
    fn rlm_catalog_renders_all_members_under_call_path() {
        let tools = [
            ToolDefinition::raw(
                "tool:test/fetch_url",
                "fetch_url",
                "Fetch URL",
                ToolContract::default_input_schema(),
                json!({ "type": "string" }),
            )
            .with_lashlang_binding(LashlangToolBinding::new(["web"], "fetch")),
            ToolDefinition::raw(
                "tool:test/read_file",
                "read_file",
                "Read a file",
                ToolContract::default_input_schema(),
                json!({ "type": "string" }),
            )
            .with_lashlang_binding(LashlangToolBinding::new(["files"], "read")),
        ];
        let contracts: std::collections::BTreeMap<_, _> = tools
            .iter()
            .map(|tool| (tool.name().to_string(), Arc::new(tool.contract())))
            .collect();
        let manifests = tools.iter().map(|tool| tool.manifest()).collect::<Vec<_>>();
        let contribution = rlm_tool_catalog(
            ToolCatalogContext {
                session_id: "session".to_string(),
                tools: manifests.clone(),
                resolve_contract: Some(Arc::new({
                    let contracts = contracts.clone();
                    move |name| contracts.get(name).cloned()
                })),
                tool_access: lash_core::SessionToolAccess::default(),
                subagent: None,
                extensions: Default::default(),
            },
            &test_dialect_registry(),
        )
        .unwrap();
        assert!(contribution.is_empty(), "RLM contributes no removals");
        let catalog = build_tool_catalog(ToolCatalogBuildInput {
            tools: manifests,
            resolve_contract: Some(Arc::new(move |name| contracts.get(name).cloned())),
            contributions: vec![contribution],
        });

        assert!(catalog.has_callable_tool("fetch_url"));
        assert!(catalog.has_callable_tool("read_file"));
        let docs = rlm_prompt_tool_docs(&catalog, &lashlang_test_dialect());
        assert!(docs.contains("web.fetch"), "{docs}");
        assert!(docs.contains("files.read"), "{docs}");
        // No legacy catalogue notes or tier filtering.
        assert!(!docs.contains("Catalogued capabilities:"), "{docs}");
    }

    #[test]
    fn rlm_catalog_rejects_members_without_lashlang_binding() {
        let missing = ToolDefinition::raw(
            "tool:test/update_plan",
            "update_plan",
            "Update plan",
            ToolContract::default_input_schema(),
            json!({ "type": "string" }),
        );

        let err = rlm_tool_catalog(
            ToolCatalogContext {
                session_id: "session".to_string(),
                tools: vec![missing.manifest()],
                resolve_contract: None,
                tool_access: lash_core::SessionToolAccess::default(),
                subagent: None,
                extensions: Default::default(),
            },
            &test_dialect_registry(),
        )
        .expect_err("missing binding should fail RLM registration");

        assert!(
            err.to_string()
                .contains("missing an explicit `lashlang.tool` binding"),
            "{err}"
        );
    }

    #[test]
    fn rlm_catalog_ignores_internal_members_without_lashlang_bindings() {
        let internal = ToolDefinition::raw(
            "tool:test/internal_runner",
            "internal_runner",
            "Runtime-owned process body",
            ToolContract::default_input_schema(),
            json!({ "type": "string" }),
        )
        .with_activation(ToolActivation::Internal);

        rlm_tool_catalog(
            ToolCatalogContext {
                session_id: "session".to_string(),
                tools: vec![internal.manifest()],
                resolve_contract: None,
                tool_access: lash_core::SessionToolAccess::default(),
                subagent: None,
                extensions: Default::default(),
            },
            &test_dialect_registry(),
        )
        .expect("internal process bodies are not Lashlang-callable catalog members");
    }

    /// Membership is advertisement, so a binding whose call path a TypeScript
    /// cell cannot write is refused at registration rather than rendered as a
    /// declaration nothing can call (FIG-1444). `delete` is a module root no
    /// cell can spell; `Math` is a root the lowerer resolves as an ECMA global
    /// namespace.
    #[test]
    fn rlm_catalog_rejects_typescript_call_paths_no_cell_can_address() {
        for module in ["delete", "Math"] {
            let unaddressable = ToolDefinition::raw(
                "tool:test/purge",
                "purge",
                "Purge",
                ToolContract::default_input_schema(),
                json!({ "type": "string" }),
            )
            .with_lashlang_binding(LashlangToolBinding::new([module], "run"));

            let err = rlm_tool_catalog(
                ToolCatalogContext {
                    session_id: "session".to_string(),
                    tools: vec![unaddressable.manifest()],
                    resolve_contract: None,
                    tool_access: lash_core::SessionToolAccess::default(),
                    subagent: None,
                    extensions: Default::default(),
                },
                &test_dialect_registry(),
            )
            .expect_err("an unaddressable TypeScript call path must fail registration");

            assert!(
                err.to_string().contains("no TypeScript cell can call"),
                "{err}"
            );
            assert!(err.to_string().contains(&format!("{module}.run")), "{err}");
            // The refusal must lead with why the path is unadvertisable. The
            // probe's own diagnostic answers a different question — `Math.*`
            // fails it as `TS_AWAIT_UNSUPPORTED`, which reads as "drop the
            // await" — so it belongs after the reason, never in place of it.
            assert!(
                err.to_string().contains("does not dispatch a tool"),
                "{err}"
            );
        }
    }

    #[test]
    fn rlm_catalog_rejects_members_without_typescript_binding() {
        let mut missing = ToolDefinition::raw(
            "tool:test/update_plan",
            "update_plan",
            "Update plan",
            ToolContract::default_input_schema(),
            json!({ "type": "string" }),
        )
        .with_lashlang_binding(LashlangToolBinding::new(["plan"], "update"));
        missing
            .manifest
            .bindings
            .remove(lash_lashlang_runtime::TYPESCRIPT_TOOL_BINDING_KEY);

        let err = rlm_tool_catalog(
            ToolCatalogContext {
                session_id: "session".to_string(),
                tools: vec![missing.manifest()],
                resolve_contract: None,
                tool_access: lash_core::SessionToolAccess::default(),
                subagent: None,
                extensions: Default::default(),
            },
            &test_dialect_registry(),
        )
        .expect_err("missing TypeScript binding should fail RLM registration");

        assert!(
            err.to_string()
                .contains("missing an explicit `typescript.tool` binding"),
            "{err}"
        );
    }

    #[test]
    fn member_rlm_tool_docs_render_and_link_module_call() {
        let update_plan = ToolDefinition::raw(
            "tool:test/update_plan",
            "update_plan",
            "Update the visible plan",
            json!({
                "type": "object",
                "properties": {
                    "plan": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "step": { "type": "string" },
                                "status": { "type": "string" }
                            },
                            "required": ["step", "status"]
                        }
                    }
                },
                "required": ["plan"],
                "additionalProperties": false
            }),
            json!({ "type": "string" }),
        )
        .with_lashlang_binding(LashlangToolBinding::new(["plan"], "update"));

        let contracts: std::collections::BTreeMap<_, _> = [update_plan.clone()]
            .iter()
            .map(|tool| (tool.name().to_string(), Arc::new(tool.contract())))
            .collect();
        let manifests = vec![update_plan.manifest()];
        let contribution = rlm_tool_catalog(
            ToolCatalogContext {
                session_id: "session".to_string(),
                tools: manifests.clone(),
                resolve_contract: Some(Arc::new({
                    let contracts = contracts.clone();
                    move |name| contracts.get(name).cloned()
                })),
                tool_access: lash_core::SessionToolAccess::default(),
                subagent: None,
                extensions: Default::default(),
            },
            &test_dialect_registry(),
        )
        .expect("RLM catalog validates explicit binding");
        let catalog = build_tool_catalog(ToolCatalogBuildInput {
            tools: manifests,
            resolve_contract: Some(Arc::new(move |name| contracts.get(name).cloned())),
            contributions: vec![contribution],
        });

        let docs = rlm_prompt_tool_docs(&catalog, &lashlang_test_dialect());
        assert!(docs.len() <= 768, "plan.update docs exceeded budget");
        assert!(docs.contains("plan.update("), "{docs}");
        assert!(
            docs.contains("plan: list[record{step: str, status: str}]"),
            "{docs}"
        );
        assert!(!docs.contains("update_plan("), "{docs}");

        let host_environment = LashlangSurface::default()
            .host_environment(&catalog)
            .expect("explicit binding builds host environment");
        let program = lashlang::parse(
            r#"await plan.update({ plan: [{ step: "Patch", status: "pending" }] })?"#,
        )
        .expect("module call parses");
        lashlang::LinkedModule::link(program, host_environment).expect("module call links");
    }
    /// The three strings this guard was built from, exactly as `main` shipped
    /// them.
    ///
    /// Copied rather than referenced: `lash-protocol-rlm` cannot depend on
    /// `lash-subagents` or `lash-plugin-process-controls` (they depend on it),
    /// and a guard whose red side is only "some string with the word in it"
    /// would not prove it catches *these*. Each was measured in a judged
    /// TypeScript session's saved system prompt.
    const LEAKED_HOST_PROSE: &[&str] = &[
        "A Lashlang process definition value, for example `on_button`.",
        "Optional typed result shape. Use string descriptors for record fields, e.g. \
         `{ queries: \"list[str]\" }`, or pass a Lashlang `Type { ... }` literal for nested \
         shapes.",
        "Optional record of state to seed into the child. Each entry's kind is preserved \
         automatically: if its lashlang source root is a host-projected binding (e.g. \
         `seed: { problem: input.prompt }`), the child receives it as a read-only projected \
         binding; otherwise it lands as a regular RLM global.",
    ];

    /// One tool whose only interesting property is the prose at `site`.
    fn tool_with_prose(site: ProseSite, prose: &str) -> ToolDefinition {
        let (description, input_schema) = match site {
            ProseSite::Description => (prose.to_string(), ToolContract::default_input_schema()),
            ProseSite::Schema => (
                "Run a subagent".to_string(),
                json!({
                    "type": "object",
                    "properties": { "output": { "type": "object", "description": prose } },
                    "additionalProperties": false
                }),
            ),
        };
        ToolDefinition::raw(
            "tool:test/spawn_agent",
            "spawn_agent",
            description,
            input_schema,
            json!({ "type": "string" }),
        )
        .with_lashlang_binding(LashlangToolBinding::new(["agents"], "spawn"))
    }

    #[derive(Clone, Copy)]
    enum ProseSite {
        Description,
        Schema,
    }

    fn catalog_registration(tool: ToolDefinition) -> Result<(), PluginError> {
        let contract = Arc::new(tool.contract());
        let name = tool.name().to_string();
        rlm_tool_catalog(
            ToolCatalogContext {
                session_id: "session".to_string(),
                tools: vec![tool.manifest()],
                resolve_contract: Some(Arc::new(move |requested| {
                    (requested == name).then(|| Arc::clone(&contract))
                })),
                tool_access: lash_core::SessionToolAccess::default(),
                subagent: None,
                extensions: Default::default(),
            },
            &test_dialect_registry(),
        )
        .map(|_| ())
    }

    /// The class, at both prose sites and for both dialects' words.
    #[test]
    fn host_tool_prose_that_names_a_dialect_fails_registration() {
        for prose in LEAKED_HOST_PROSE {
            for site in [ProseSite::Description, ProseSite::Schema] {
                let err = catalog_registration(tool_with_prose(site, prose))
                    .expect_err("dialect-named prose must not register");
                let message = err.to_string();
                assert!(
                    message.contains("names the `lashlang` dialect"),
                    "{message}"
                );
                assert!(message.contains("{{type_literal_hint}}"), "{message}");
            }
        }

        // The rule is neutrality, not foreignness: the *other* dialect's words
        // are rejected by the same gate, in the same Lashlang-default session.
        for prose in [
            "Write the result into a <typescript> cell.",
            "Call finish(value) when done.",
        ] {
            let err = catalog_registration(tool_with_prose(ProseSite::Description, prose))
                .expect_err("TypeScript wording must not register either");
            assert!(
                err.to_string().contains("names the `typescript` dialect"),
                "{err}"
            );
        }
    }

    /// Neutral prose registers, so the guard is a rule and not a wall.
    #[test]
    fn dialect_neutral_host_tool_prose_registers() {
        catalog_registration(tool_with_prose(
            ProseSite::Schema,
            "A process definition value, for example `on_button`.",
        ))
        .expect("neutral prose registers");
        catalog_registration(tool_with_prose(
            ProseSite::Schema,
            "Optional typed result shape. Use string descriptors for record fields, \
             e.g. `{ queries: \"list[str]\" }`{{type_literal_hint}}.",
        ))
        .expect("token-carrying prose registers");
    }

    /// A misspelled token would otherwise reach the model verbatim.
    #[test]
    fn unrecognized_prose_token_fails_registration() {
        let err = catalog_registration(tool_with_prose(
            ProseSite::Schema,
            "Optional typed result shape{{type_literal}}.",
        ))
        .expect_err("an unresolved token must not register");
        let message = err.to_string();
        assert!(message.contains("`{{type_literal}}`"), "{message}");
        assert!(message.contains("not an RLM prose token"), "{message}");
    }

    /// The token resolves to each dialect's own answer in the rendered doc.
    ///
    /// Rendered through `rlm_prompt_tool_docs`, the path a served turn uses, so
    /// this cannot pass while the doc block skips the substitution.
    #[test]
    fn prose_tokens_are_spelled_by_the_session_dialect() {
        let authored = "Optional typed result shape. Use string descriptors for record fields, \
             e.g. `{ queries: \"list[str]\" }`{{type_literal_hint}}.";
        let tool = tool_with_prose(ProseSite::Schema, authored);
        let contracts: std::collections::BTreeMap<_, _> =
            [(tool.name().to_string(), Arc::new(tool.contract()))]
                .into_iter()
                .collect();
        let manifests = vec![tool.manifest()];
        let catalog = build_tool_catalog(ToolCatalogBuildInput {
            tools: manifests,
            resolve_contract: Some(Arc::new(move |name| contracts.get(name).cloned())),
            contributions: vec![ToolCatalogContribution::default()],
        });

        let lashlang = rlm_prompt_tool_docs(&catalog, &lashlang_test_dialect());
        assert!(
            lashlang.contains("or pass a `Type { ... }` literal for nested shapes"),
            "{lashlang}"
        );
        let typescript = rlm_prompt_tool_docs(&catalog, &crate::dialect::typescript_test_dialect());
        assert!(
            typescript.contains("e.g. `{ queries: \"list[str]\" }`."),
            "{typescript}"
        );
        assert!(!typescript.contains("Type {"), "{typescript}");
        for rendered in [&lashlang, &typescript] {
            assert!(!rendered.contains("{{"), "unresolved token: {rendered}");
        }
    }

    /// A leak nested deep in a schema, and one in an *output* schema.
    ///
    /// The measured three all sat one level into an input schema. A result
    /// schema's field docs are rendered into the same doc block ("Return
    /// fields:"), and a nested `items`/`properties` chain is where a listing
    /// tool's rows live — `processes.list` returns an array of records — so the
    /// sweep walks whole schemas rather than reading their top level.
    #[test]
    fn a_dialect_word_anywhere_in_a_schema_fails_registration() {
        let tool = ToolDefinition::raw(
            "tool:test/list_process_handles",
            "list_process_handles",
            "List process runs visible to this session",
            json!({
                "type": "object",
                "properties": {
                    "filter": {
                        "type": "object",
                        "properties": {
                            "definition": {
                                "type": "object",
                                "description": "A Lashlang process definition value."
                            }
                        }
                    }
                }
            }),
            json!({
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "string",
                            "description": "Handle id; print it with `finish <value>` when done."
                        }
                    }
                }
            }),
        )
        .with_lashlang_binding(LashlangToolBinding::new(["processes"], "list"));

        let err = catalog_registration(tool).expect_err("both leaks must be reported");
        let message = err.to_string();
        assert!(message.contains("in its input schema"), "{message}");
        assert!(message.contains("in its output schema"), "{message}");
        assert!(message.contains("2 violation(s)"), "{message}");
    }

    /// A typo'd token: `{{` with no `}}`.
    ///
    /// Nothing resolves it, it renders verbatim, and the scanner that answered
    /// "no unresolvable token" for the whole string when it met one also stopped
    /// reading there — so a dialect word written after the typo was invisible
    /// too. Both halves are asserted.
    #[test]
    fn an_unclosed_prose_token_fails_registration() {
        let err = catalog_registration(tool_with_prose(
            ProseSite::Schema,
            "Optional typed result shape{{type_literal_hint.",
        ))
        .expect_err("an unclosed token must not register");
        let message = err.to_string();
        assert!(message.contains("unclosed"), "{message}");
        assert!(message.contains("{{type_literal_hint."), "{message}");

        // The scan continues past it: the dialect word later in the same string
        // is reported alongside the typo, not swallowed by it.
        let err = catalog_registration(tool_with_prose(
            ProseSite::Schema,
            "Optional typed result shape{{type_literal_hint. Pass a Lashlang type literal for \
             nested shapes.",
        ))
        .expect_err("both defects must be reported");
        let message = err.to_string();
        assert!(message.contains("unclosed"), "{message}");
        assert!(
            message.contains("names the `lashlang` dialect"),
            "{message}"
        );
        assert!(message.contains("2 violation(s)"), "{message}");
    }

    /// A known token where the renderer will never resolve it.
    ///
    /// The doc renderer's reach is uneven: input rows are built from the
    /// schema's *top-level* `properties`, while return fields are collected
    /// recursively. Accepting a token in the deep input position would leave an
    /// author believing a hint reaches the model when nothing renders it at all,
    /// so the guard rejects the token exactly where it cannot be spelled — and
    /// still accepts it one level up, and at depth in an output schema.
    #[test]
    fn a_prose_token_the_renderer_cannot_reach_fails_registration() {
        let hint = "Nested shape support{{type_literal_hint}}.";
        let deep_input = ToolDefinition::raw(
            "tool:test/spawn_agent",
            "spawn_agent",
            "Run a subagent",
            json!({
                "type": "object",
                "properties": {
                    "output": {
                        "type": "object",
                        "properties": { "shape": { "type": "string", "description": hint } }
                    }
                }
            }),
            json!({ "type": "string" }),
        )
        .with_lashlang_binding(LashlangToolBinding::new(["agents"], "spawn"));
        let err = catalog_registration(deep_input).expect_err("a token nowhere is a defect");
        let message = err.to_string();
        assert!(
            message.contains("a position the tool-doc renderer never resolves"),
            "{message}"
        );

        // A deep *output* description does render — return fields are collected
        // recursively — so the same token is accepted there.
        let deep_output = ToolDefinition::raw(
            "tool:test/spawn_agent",
            "spawn_agent",
            "Run a subagent",
            ToolContract::default_input_schema(),
            json!({
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": { "shape": { "type": "string", "description": hint } }
                }
            }),
        )
        .with_lashlang_binding(LashlangToolBinding::new(["agents"], "spawn"));
        catalog_registration(deep_output).expect("a rendered output field resolves its token");
    }

    /// `default` / `enum` / `const` values are authored prose too.
    ///
    /// They are not descriptions, but the doc line renders them inline (`in
    /// "a"|"b"`), so a dialect word in one reaches the model exactly like a
    /// description does — while a *token* in one never resolves, because nothing
    /// substitutes literals.
    #[test]
    fn authored_schema_literals_are_swept_but_never_token_resolved() {
        let leaked = ToolDefinition::raw(
            "tool:test/list_process_handles",
            "list_process_handles",
            "List process runs",
            json!({
                "type": "object",
                "properties": {
                    "shape": {
                        "type": "string",
                        "enum": ["plain", "lashlang record"],
                        "default": "plain"
                    }
                }
            }),
            json!({ "type": "string" }),
        )
        .with_lashlang_binding(LashlangToolBinding::new(["processes"], "list"));
        let err = catalog_registration(leaked).expect_err("an enum value names a dialect");
        assert!(
            err.to_string().contains("names the `lashlang` dialect"),
            "{err}"
        );

        let tokenized = ToolDefinition::raw(
            "tool:test/list_process_handles",
            "list_process_handles",
            "List process runs",
            json!({
                "type": "object",
                "properties": {
                    "shape": { "type": "string", "default": "{{type_literal_hint}}" }
                }
            }),
            json!({ "type": "string" }),
        )
        .with_lashlang_binding(LashlangToolBinding::new(["processes"], "list"));
        let err = catalog_registration(tokenized).expect_err("a literal never resolves a token");
        assert!(
            err.to_string()
                .contains("a position the tool-doc renderer never resolves"),
            "{err}"
        );
    }

    /// The guard only measures if each dialect's marker list can fire, and if
    /// the two lists are not the same list.
    #[test]
    fn every_registered_dialect_contributes_distinct_markers() {
        let registry = test_dialect_registry();
        let mut all = Vec::new();
        for dialect in registry.dialects() {
            let markers = crate::dialect::dialect_identity_markers(dialect.as_ref());
            assert!(
                markers.contains(&dialect.language_id().to_lowercase()),
                "{markers:?}"
            );
            assert!(markers.len() >= 3, "{markers:?}");
            all.push(markers);
        }
        assert_eq!(all.len(), 2, "both shipped dialects are registered");
        assert_ne!(
            all[0], all[1],
            "collapsed marker lists make the guard vacuous"
        );
    }
}
