//! Laws of the dialect's tool presenter slot (FIG-6489): a session's tools
//! read the way its dialect's presenter writes them, and the presenter
//! decides nothing else.

use lash_core::ToolCatalog;
use lash_tool_presenter_api::{
    PresentationRefusal, PresentedToolSection, ToolPresentationEntry, ToolPresentationRequest,
    ToolPresenter, ToolPresenterSlot, WrappedToolPresenter,
};
use lash_tool_presenter_typescript::TypescriptToolPresenter;
use lash_vocabulary::json_schema::JsonSchema;
use lash_vocabulary::tool_contract::ToolContract;
use lash_vocabulary::tool_contract::ToolDefinition;
use lash_vocabulary::tool_contract::ToolManifest;
use serde_json::{Value, json};

use crate::dialect::{CellDialect, SessionDialect};

fn lookup() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:test/lookup",
        "tools.lookup",
        "Find a record.",
        json!({
            "type": "object",
            "properties": { "query": { "type": "string" } },
            "required": ["query"],
            "additionalProperties": false
        }),
        json!({ "type": "string" }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(30))
}

fn declarations(
    dialect: CellDialect,
    catalog: &ToolCatalog,
) -> Result<String, lash_core::SessionError> {
    SessionDialect::prompt_only(dialect)
        .execution_section(
            Default::default(),
            catalog,
            crate::plugin::RlmChannel::Cell,
            None,
        )
        .map(|section| section.declarations)
}

/// A host presenter that writes its own words for every tool, and answers
/// `listed` as the tools it lists.
struct HostPresenter {
    text: &'static str,
    lists: bool,
}

impl ToolPresenter for HostPresenter {
    fn id(&self) -> &str {
        "host.tools.v1"
    }

    fn admit_config(&self, raw: &Value) -> Result<Value, PresentationRefusal> {
        Ok(raw.clone())
    }

    fn present(
        &self,
        request: ToolPresentationRequest<'_>,
    ) -> Result<PresentedToolSection, PresentationRefusal> {
        Ok(PresentedToolSection {
            text: self.text.to_owned(),
            listed: if self.lists {
                request
                    .tools
                    .iter()
                    .map(|tool| tool.manifest.id.clone())
                    .collect()
            } else {
                Vec::new()
            },
            omitted: Vec::new(),
        })
    }

    fn tool_doc(
        &self,
        _tool: &ToolPresentationEntry<'_>,
        _config: &Value,
    ) -> Result<String, PresentationRefusal> {
        Ok(self.text.to_owned())
    }

    fn tool_signature(
        &self,
        _call_path: &str,
        _input: &JsonSchema,
        _output: &JsonSchema,
        _config: &Value,
    ) -> Result<String, PresentationRefusal> {
        Ok(self.text.to_owned())
    }

    fn discovery_text(
        &self,
        _manifest: &ToolManifest,
        _contract: &ToolContract,
        _config: &Value,
    ) -> Result<String, PresentationRefusal> {
        Ok(self.text.to_owned())
    }
}

/// Replacing or wrapping a dialect's presenter changes its tools section;
/// it never changes what a call accepts. An input the contract refuses is
/// refused however the presenter spelled the tool, and a section that does
/// not account for the offered tools is refused rather than shown.
#[test]
fn a_replaced_or_wrapped_presenter_changes_the_section_and_never_admission() {
    let catalog = ToolCatalog::from_tool_definitions(vec![lookup()]);
    let shipped = declarations(CellDialect::typescript(), &catalog).expect("the shipped presenter");
    let doc = "`tools.lookup({ query: string }): Promise<string>`\nFind a record.";
    assert_eq!(shipped, format!("### Tools\n\n{doc}"));

    let wrapped = declarations(
        CellDialect::typescript().with_tool_presenter(ToolPresenterSlot::new(
            WrappedToolPresenter::new("host.tools.v1", TypescriptToolPresenter)
                .with_heading("Prefer `tools.lookup` before asking the user.")
                .with_appended("Report every failed call."),
        )),
        &catalog,
    )
    .expect("the wrapped presenter");
    assert_eq!(
        wrapped,
        format!(
            "### Tools\n\nPrefer `tools.lookup` before asking the user.\n\n{doc}\n\nReport every failed call."
        )
    );

    let replaced = declarations(
        CellDialect::typescript().with_tool_presenter(ToolPresenterSlot::new(HostPresenter {
            text: "`tools.lookup(query: number)` finds a record by number.",
            lists: true,
        })),
        &catalog,
    )
    .expect("the host presenter");
    assert_eq!(
        replaced,
        "### Tools\n\n`tools.lookup(query: number)` finds a record by number."
    );
    let contract = &catalog.entries()[0].contract;
    assert!(
        lash_vocabulary::tool_contract::validate_tool_input(contract, &json!({ "query": 7 }))
            .is_err()
    );
    assert!(lash_vocabulary::tool_contract::validate_tool_input(contract, &json!({})).is_err());
    assert!(
        lash_vocabulary::tool_contract::validate_tool_input(contract, &json!({ "query": "a" }))
            .is_ok()
    );

    let unaccounted = declarations(
        CellDialect::typescript().with_tool_presenter(ToolPresenterSlot::new(HostPresenter {
            text: "No tools.",
            lists: false,
        })),
        &catalog,
    )
    .expect_err("a section that drops an offered tool");
    assert!(
        unaccounted
            .to_string()
            .contains("does not account for the offered tools"),
        "{unaccounted}"
    );
}

fn defaults() -> [Box<dyn ToolPresenter>; 3] {
    [
        Box::new(TypescriptToolPresenter),
        Box::new(lash_tool_presenter_python::PythonToolPresenter),
        Box::new(lash_tool_presenter_standard::StandardToolPresenter),
    ]
}

fn referenced(name: &str, kind: &str) -> ToolDefinition {
    ToolDefinition::raw(
        name,
        format!("tools.{name}"),
        format!("Call {name}."),
        json!({"$defs":{"Field":{"type":kind,"description":"Shared field notes."}},
            "type":"object","properties":{"value":{"$ref":"#/$defs/Field"}},
            "required":["value"],"additionalProperties":false}),
        json!({"type":"boolean"}),
    )
    .expect("admitted schema")
    .with_execution(std::time::Duration::from_secs(30))
}

fn paths(catalog: &ToolCatalog) -> Vec<String> {
    catalog
        .entries()
        .iter()
        .map(|tool| tool.manifest.tool())
        .collect()
}

fn entries<'a>(catalog: &'a ToolCatalog, paths: &'a [String]) -> Vec<ToolPresentationEntry<'a>> {
    catalog
        .entries()
        .iter()
        .zip(paths)
        .map(|(tool, call_path)| ToolPresentationEntry {
            manifest: &tool.manifest,
            contract: &tool.contract,
            call_path,
            pinned: false,
        })
        .collect()
}

/// FIG-6495: a selected tool owns its complete closure, an omitted tool adds
/// nothing, and discovery does not borrow definitions from an earlier listing.
#[test]
fn selected_closures_are_shared_and_discovery_is_standalone_in_every_dialect() {
    let mut hidden = referenced("hidden", "string");
    hidden.manifest.inline = false;
    let mut unique = referenced("unique", "integer");
    unique.manifest.inline = false;
    let catalog = ToolCatalog::from_tool_definitions(vec![
        referenced("first", "string"),
        referenced("second", "string"),
        hidden,
        unique,
    ]);
    let paths = paths(&catalog);
    let entries = entries(&catalog, &paths);
    for presenter in defaults() {
        let config = presenter.admit_config(&json!({})).expect("config");
        let full = presenter
            .present(ToolPresentationRequest {
                tools: &entries[..2],
                config: &config,
                disclosure: lash_tool_presenter_api::ToolDisclosure::Complete,
                reserved_names: &[],
                max_section_bytes: None,
            })
            .expect("complete listing");
        let request = ToolPresentationRequest {
            tools: &entries,
            config: &config,
            disclosure: lash_tool_presenter_api::ToolDisclosure::Budget {
                catalogue_bytes: full.text.len() - 2,
            },
            reserved_names: &[],
            max_section_bytes: None,
        };
        let listing = presenter.present(request).expect("budget listing");
        assert!(listing.accounts_for(&request));
        assert_eq!(
            listing.listed.len(),
            2,
            "{}: {}",
            presenter.id(),
            listing.text
        );
        assert_eq!(listing.text, full.text);
        assert_eq!(
            listing.text.matches("Field = ").count(),
            1,
            "{}",
            listing.text
        );
        assert_eq!(
            listing.text.matches("Shared field notes.").count(),
            1,
            "{}",
            listing.text
        );
        assert!(!listing.text.contains("Field_2"), "{}", listing.text);
        let found = presenter
            .tool_doc(&entries[2], &config)
            .expect("standalone discovery");
        assert_eq!(found.matches("Field = ").count(), 1, "{found}");
        assert_eq!(found.matches("Shared field notes.").count(), 1, "{found}");
        let signature = presenter
            .tool_signature(
                "hidden",
                &entries[2].contract.input_schema.canonical,
                &entries[2].contract.output_schema.canonical,
                &config,
            )
            .expect("signature bundle");
        assert_eq!(signature.matches("Field = ").count(), 1, "{signature}");
    }
}

/// FIG-6495 append-prefix and deterministic replay, including collisions.
#[test]
fn later_collisions_append_without_rewriting_earlier_declarations() {
    let catalog = ToolCatalog::from_tool_definitions(vec![
        referenced("first", "string"),
        referenced("second", "integer"),
        referenced("third", "boolean"),
    ]);
    let paths = paths(&catalog);
    let entries = entries(&catalog, &paths);
    for presenter in defaults() {
        let config = presenter.admit_config(&json!({})).expect("config");
        let request = ToolPresentationRequest {
            tools: &entries[..2],
            config: &config,
            disclosure: lash_tool_presenter_api::ToolDisclosure::Complete,
            reserved_names: &[],
            max_section_bytes: None,
        };
        let before = presenter.present(request).expect("prefix");
        let request = ToolPresentationRequest {
            tools: &entries,
            ..request
        };
        let after = presenter.present(request).expect("appended");
        assert!(
            after.text.starts_with(&before.text),
            "{}\n{}",
            before.text,
            after.text
        );
        assert!(after.text.contains("Field_2 = "), "{}", after.text);
        assert!(after.text.contains("Field_3 = "), "{}", after.text);
        assert_eq!(after, presenter.present(request).expect("replay"));
    }
}

/// FIG-6495: hard section limits include complete pinned definitions and notes.
#[test]
fn pinned_sections_refuse_with_the_complete_required_size() {
    let catalog = ToolCatalog::from_tool_definitions(vec![referenced("first", "string")]);
    let paths = paths(&catalog);
    let mut entries = entries(&catalog, &paths);
    entries[0].pinned = true;
    for presenter in defaults() {
        let config = presenter.admit_config(&json!({})).expect("config");
        let request = ToolPresentationRequest {
            tools: &entries,
            config: &config,
            disclosure: lash_tool_presenter_api::ToolDisclosure::Budget { catalogue_bytes: 0 },
            reserved_names: &[],
            max_section_bytes: None,
        };
        let complete = presenter.present(request).expect("pinned listing");
        let required = complete.text.len();
        assert_eq!(
            presenter
                .present(ToolPresentationRequest {
                    max_section_bytes: Some(required),
                    ..request
                })
                .expect("exact fit"),
            complete
        );
        assert_eq!(
            presenter.present(ToolPresentationRequest {
                max_section_bytes: Some(required - 1),
                ..request
            }),
            Err(PresentationRefusal::SectionLimit {
                required,
                limit: required - 1
            })
        );
    }
}

/// This is the short custom presenter documented on ToolPresenterSlot: only
/// public helpers, sharing definitions after filtering and retaining budgets.
#[test]
fn a_public_helper_wrapper_filters_reheads_and_appends_budgeted_tools() {
    let catalog = ToolCatalog::from_tool_definitions(vec![
        referenced("first", "string"),
        referenced("drop", "integer"),
        referenced("second", "string"),
    ]);
    let paths = paths(&catalog);
    let entries = entries(&catalog, &paths);
    let presenter = WrappedToolPresenter::new("host.tools.v1", TypescriptToolPresenter)
        .with_heading("Host tools")
        .with_filter(|tool| tool.call_path != "tools.drop")
        .with_appended("Host note.");
    let config = presenter.admit_config(&json!({})).expect("config");
    let request = ToolPresentationRequest {
        tools: &entries,
        config: &config,
        disclosure: lash_tool_presenter_api::ToolDisclosure::Budget {
            catalogue_bytes: 4096,
        },
        reserved_names: &[],
        max_section_bytes: None,
    };
    let full = presenter.present(request).expect("custom presenter");
    let listing = presenter
        .present(ToolPresentationRequest {
            max_section_bytes: Some(full.text.len()),
            ..request
        })
        .expect("budgeted wrapper");
    assert!(listing.accounts_for(&request));
    assert_eq!(listing.listed.len(), 2);
    assert_eq!(listing.omitted, [catalog.entries()[1].manifest.id.clone()]);
    assert!(listing.text.starts_with("Host tools\n\n"));
    assert!(listing.text.ends_with("\n\nHost note."));
    assert_eq!(listing.text.matches("Field = ").count(), 1);
    assert_eq!(listing.text.matches("Shared field notes.").count(), 1);
    assert!(!listing.text.contains("Field_2"));
}

/// FIG-6495: reserve the actual inferred registry's symbols in every dialect.
#[test]
fn inferred_binding_names_reserve_tool_definition_symbols_in_every_dialect() {
    let mut cache = crate::rlm_support::BoundVariableRenderCache::default();
    let (bound, _) = crate::rlm_support::render_bound_variables(
        &mut cache,
        vec![crate::session_globals::SessionGlobal {
            name: "field".into(),
            kind: crate::session_globals::SessionGlobalKind::Data(json!({"body":"x".repeat(2000)})),
        }],
        &crate::dialect::TypescriptPrompts,
        &crate::render::BuiltinCodeRenderer,
        &lash_render::RenderParams::preview(),
        12,
        24 * 1024,
    )
    .expect("inferred binding");
    assert!(bound.contains("type Field = "), "{bound}");
    assert!(cache.reserved_names.iter().any(|name| name == "Field"));
    let catalog = ToolCatalog::from_tool_definitions(vec![referenced("lookup", "string")]);
    let paths = paths(&catalog);
    let entries = entries(&catalog, &paths);
    for presenter in defaults() {
        let config = presenter.admit_config(&json!({})).expect("config");
        let text = presenter
            .present(ToolPresentationRequest {
                tools: &entries,
                config: &config,
                disclosure: lash_tool_presenter_api::ToolDisclosure::Complete,
                reserved_names: &cache.reserved_names,
                max_section_bytes: None,
            })
            .expect("reserved inferred symbols")
            .text;
        assert!(text.contains("Field_2 = "), "{}: {text}", presenter.id());
        assert!(!text.contains("Field = "), "{text}");
    }
}
