//! Catalogue-preview prompt text for RLM deferred tool discovery.
//!
//! Resident catalog members render as full RLM tool docs. A host may also keep
//! a larger searchable catalogue outside the resident catalog and resolve
//! selected Lashlang call paths on demand. This formatter advertises that
//! searchable tail as a compact module index plus the instruction to use
//! `tools.search(...)` and then call the returned module path directly.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use lash_core::ToolManifest;
use serde_json::Value;

use crate::{ResolvedToolBinding, TOOL_BINDING_KEY, ToolBinding, ToolBindingResolutionExt};

pub const DEFAULT_CATALOGUE_PREVIEW_MODULE_LIMIT: usize = 100;
pub const DEFAULT_CATALOGUE_PREVIEW_CALL_NAME_LIMIT: usize = 50;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CataloguePreviewEntry {
    pub module_path: Vec<String>,
    pub call: String,
}

impl CataloguePreviewEntry {
    pub fn new(
        module_path: impl IntoIterator<Item = impl Into<String>>,
        call: impl Into<String>,
    ) -> Self {
        Self {
            module_path: module_path.into_iter().map(Into::into).collect(),
            call: call.into(),
        }
    }

    pub fn from_lashlang_executable(executable: ResolvedToolBinding) -> Self {
        let call = executable.call_path();
        Self {
            module_path: executable.module_path,
            call,
        }
    }

    pub fn module_path_string(&self) -> String {
        self.module_path.join(".")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CataloguePreviewOptions {
    pub title: String,
    pub search_call_path: String,
    pub module_limit: usize,
    pub call_name_limit: usize,
}

impl Default for CataloguePreviewOptions {
    fn default() -> Self {
        Self {
            title: "Catalogued Capabilities".to_string(),
            search_call_path: "tools.search".to_string(),
            module_limit: DEFAULT_CATALOGUE_PREVIEW_MODULE_LIMIT,
            call_name_limit: DEFAULT_CATALOGUE_PREVIEW_CALL_NAME_LIMIT,
        }
    }
}

/// The advertisement of a searchable catalogue, as prompt text under an
/// `### <title>` heading, or `None` when `entries` is empty.
///
/// The text is the host's to place: it states it in its protocol plugin's
/// prompt config (an instruction or a context entry), which records it with
/// the session. It names `options.search_call_path`, so a host states it only
/// for a session that has that search tool.
pub fn catalogue_preview(
    entries: impl IntoIterator<Item = CataloguePreviewEntry>,
    options: &CataloguePreviewOptions,
) -> Option<String> {
    let mut by_module: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut catalogued_count = 0usize;
    for entry in entries {
        catalogued_count += 1;
        by_module
            .entry(entry.module_path_string())
            .or_default()
            .push(entry.call);
    }
    if catalogued_count == 0 {
        return None;
    }
    for names in by_module.values_mut() {
        names.sort_unstable();
    }

    let search_call = options.search_call_path.trim().to_string();
    // No code snippet here. This crate has no dialect: the advertisement
    // goes into the prompt of whatever session holds the catalogue, and the Lashlang spelling this sentence used to carry
    // (`await {search_call}({ query: "..." })?`) put the try-operator — a
    // TypeScript syntax error — in front of every judged TypeScript session
    // with a deferred catalogue. The search tool's own doc block already shows
    // an example, respelled by the session's dialect (ADR 0063); naming the
    // argument in prose says the same thing in every dialect.
    let mut rendered = format!(
        "The capabilities below are callable directly by their module path; the listing is usually enough to call them. \
         Call `{search_call}(...)` with a `query` argument only if you need more detail than shown, or to find a \
         capability not listed here, then call the returned module path. \
         Results use the same compact contract shape as resident capabilities: call path, signature, description, and capped examples."
    );

    if by_module.len() <= options.module_limit {
        rendered.push_str("\n\nModules: ");
        for (index, (module, names)) in by_module.iter().enumerate() {
            if index > 0 {
                rendered.push_str(", ");
            }
            let _ = write!(rendered, "{module}({})", names.len());
        }
    } else {
        let _ = write!(
            rendered,
            "\n\nModules: {} total; use `{search_call}` to narrow them.",
            by_module.len()
        );
    }

    if catalogued_count <= options.call_name_limit {
        rendered.push_str("\n\nCatalogued calls:");
        for (module, names) in by_module {
            rendered.push('\n');
            let _ = write!(rendered, "{module}: {}", names.join(", "));
        }
    }

    Some(format!("### {}\n\n{rendered}", options.title.trim()))
}

pub fn catalogue_preview_entries_from_catalog_records(
    catalog: &[Value],
) -> Vec<CataloguePreviewEntry> {
    catalog
        .iter()
        .filter_map(catalogue_preview_entry_from_catalog_record)
        .collect()
}

pub fn catalogue_preview_entries_from_manifests<'a>(
    manifests: impl IntoIterator<Item = &'a ToolManifest>,
) -> Vec<CataloguePreviewEntry> {
    manifests
        .into_iter()
        .filter_map(catalogue_preview_entry_from_manifest)
        .collect()
}

pub fn catalogue_preview_entry_from_manifest(
    manifest: &ToolManifest,
) -> Option<CataloguePreviewEntry> {
    let binding = manifest
        .bindings
        .get(TOOL_BINDING_KEY)
        .cloned()
        .and_then(|value| serde_json::from_value::<ToolBinding>(value).ok())?;
    let executable = binding.executable_for(&manifest.name).ok()?;
    Some(CataloguePreviewEntry::from_lashlang_executable(executable))
}

pub fn catalogue_preview_entry_from_catalog_record(raw: &Value) -> Option<CataloguePreviewEntry> {
    let obj = raw.as_object()?;
    let name = obj.get("name")?.as_str()?;
    let binding: ToolBinding = obj
        .get("bindings")
        .and_then(|bindings| bindings.get(TOOL_BINDING_KEY))
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())?;
    let executable = binding.executable_for(name).ok()?;
    Some(CataloguePreviewEntry::from_lashlang_executable(executable))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolDefinitionBindingExt;
    use serde_json::json;

    fn catalog_record(name: &str, module_path: &[&str], operation: &str) -> Value {
        let definition = lash_core::ToolDefinition::raw(
            format!("tool:{name}"),
            name,
            "Test tool",
            lash_core::ToolDefinition::default_input_schema(),
            json!({ "type": "object" }),
        )
        .with_tool_binding(ToolBinding::new(module_path.iter().copied(), operation));
        let manifest = definition.manifest();
        json!({
            "id": manifest.id,
            "name": manifest.name,
            "bindings": manifest.bindings,
            "contract": manifest.compact_contract,
        })
    }

    #[test]
    fn catalogue_preview_groups_catalog_records_by_module() {
        let catalog = vec![
            catalog_record("gmail_fetch_email", &["gmail"], "fetch_email"),
            catalog_record("figments_list", &["figments"], "list"),
        ];

        let preview = catalogue_preview(
            catalogue_preview_entries_from_catalog_records(&catalog),
            &CataloguePreviewOptions::default(),
        )
        .expect("catalogue preview");

        assert!(preview.starts_with("### Catalogued Capabilities\n\n"));
        assert!(preview.contains("callable directly by their module path"));
        assert!(preview.contains("only if you need more detail than shown"));
        assert!(preview.contains("Modules: figments(1), gmail(1)"));
        assert!(preview.contains("figments: figments.list"));
        assert!(preview.contains("gmail: gmail.fetch_email"));
    }

    #[test]
    fn catalogue_preview_renders_from_manifests() {
        let definition = lash_core::ToolDefinition::raw(
            "tool:calendar_work_create",
            "calendar_work_create",
            "Create a work calendar event",
            lash_core::ToolDefinition::default_input_schema(),
            json!({ "type": "object" }),
        )
        .with_tool_binding(ToolBinding::new(["calendar", "work"], "create"));
        let manifest = definition.manifest();

        let preview = catalogue_preview(
            catalogue_preview_entries_from_manifests([&manifest]),
            &CataloguePreviewOptions::default(),
        )
        .expect("catalogue preview");

        assert!(preview.contains("calendar.work(1)"));
        assert!(preview.contains("calendar.work: calendar.work.create"));
    }

    #[test]
    fn catalogue_preview_options_customize_search_call_and_limits() {
        let entries = vec![
            CataloguePreviewEntry::new(["one"], "one.call"),
            CataloguePreviewEntry::new(["two"], "two.call"),
        ];
        let preview = catalogue_preview(
            entries,
            &CataloguePreviewOptions {
                title: "Hidden Tools".to_string(),
                search_call_path: "tools.find".to_string(),
                module_limit: 1,
                call_name_limit: 1,
            },
        )
        .expect("catalogue preview");

        assert!(preview.starts_with("### Hidden Tools\n\n"));
        assert!(preview.contains("Modules: 2 total; use `tools.find` to narrow them."));
        assert!(!preview.contains("Catalogued calls:"));
    }

    #[test]
    fn an_empty_catalogue_has_no_preview() {
        assert_eq!(
            catalogue_preview(Vec::new(), &CataloguePreviewOptions::default()),
            None
        );
    }
}
