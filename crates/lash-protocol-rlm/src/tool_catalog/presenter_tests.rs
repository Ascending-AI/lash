//! Laws of the dialect's tool presenter slot (FIG-6489): a session's tools
//! read the way its dialect's presenter writes them, and the presenter
//! decides nothing else.

use lash_core::{ToolCatalog, ToolContract, ToolDefinition, ToolManifest};
use lash_sansio::JsonSchema;
use lash_tool_presenter_api::{
    PresentationRefusal, PresentedToolSection, ToolPresentationEntry, ToolPresentationRequest,
    ToolPresenter, ToolPresenterSlot, WrappedToolPresenter,
};
use lash_tool_presenter_typescript::TypescriptToolPresenter;
use lash_vm_runtime::{ToolBinding, ToolDefinitionBindingExt};
use serde_json::{Value, json};

use crate::dialect::{CellDialect, SessionDialect};

fn lookup() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:test/lookup",
        "lookup",
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
    .with_tool_binding(ToolBinding::from_path("tools.lookup"))
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
    let contract = &catalog.tools[0].contract;
    assert!(lash_sansio::validate_tool_input(contract, &json!({ "query": 7 })).is_err());
    assert!(lash_sansio::validate_tool_input(contract, &json!({})).is_err());
    assert!(lash_sansio::validate_tool_input(contract, &json!({ "query": "a" })).is_ok());

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
