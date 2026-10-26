//! Catalogue-preview prompt text for RLM deferred tool discovery.
//!
//! Resident catalog members render as full RLM tool docs. A host may also keep
//! a larger searchable catalogue outside the resident catalog and resolve
//! selected operations on demand. This formatter advertises that searchable
//! tail as a compact package index plus the instruction to use
//! `tools.search(...)` and then call the returned path directly, each name
//! in the session dialect's spelling.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use lash_kernel_dialect::{SpellKind, Spelling};
use lash_kernel_value::names::QualifiedOperation;
use lash_vocabulary::tool_contract::ToolManifest;
use serde_json::Value;

pub const DEFAULT_CATALOGUE_PREVIEW_MODULE_LIMIT: usize = 100;
pub const DEFAULT_CATALOGUE_PREVIEW_CALL_NAME_LIMIT: usize = 50;

/// One catalogued operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CataloguePreviewEntry {
    pub operation: QualifiedOperation,
}

impl CataloguePreviewEntry {
    pub fn new(operation: QualifiedOperation) -> Self {
        Self { operation }
    }

    /// The operation's package path, in `spelling`.
    pub fn module_path_string(&self, spelling: Spelling) -> String {
        self.operation
            .package
            .iter()
            .map(|segment| spelling.spell(SpellKind::PathSegment, segment))
            .collect::<Vec<_>>()
            .join(".")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CataloguePreviewOptions {
    pub title: String,
    pub search_call_path: String,
    /// The spelling of the session dialect the preview is placed in.
    pub spelling: Spelling,
    pub module_limit: usize,
    pub call_name_limit: usize,
}

impl Default for CataloguePreviewOptions {
    fn default() -> Self {
        Self::standard()
    }
}

impl CataloguePreviewOptions {
    /// Standard preset: "Catalogued Capabilities", search via `tools.search`,
    /// in TypeScript's spelling, at most 100 modules and 50 call names.
    /// Historical, unmeasured cuts.
    pub fn standard() -> Self {
        Self {
            title: "Catalogued Capabilities".to_string(),
            search_call_path: "tools.search".to_string(),
            spelling: Spelling::TypeScript,
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
            .entry(entry.module_path_string(options.spelling))
            .or_default()
            .push(lash_kernel_dialect::spell_operation(
                options.spelling,
                &entry.operation,
            ));
    }
    if catalogued_count == 0 {
        return None;
    }
    for names in by_module.values_mut() {
        names.sort_unstable();
    }

    let search_call = options.search_call_path.trim().to_string();
    // Naming the query argument in prose works in every session dialect.
    let mut rendered = format!(
        "The capabilities below are callable directly by their module path; the listing is usually enough to call them. \
         Call `{search_call}(...)` with a `query` argument only if you need more detail than shown, or to find a \
         capability not listed here, then call the returned module path. \
         Results use the same compact contract shape as resident capabilities: call path, signature, and description."
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
    Some(CataloguePreviewEntry::new(manifest.operation.clone()))
}

/// The entry of a serialized tool manifest: its `operation`.
pub fn catalogue_preview_entry_from_catalog_record(raw: &Value) -> Option<CataloguePreviewEntry> {
    let operation = raw.as_object()?.get("operation")?.as_str()?;
    lash_kernel_value::operation_text::parse(operation)
        .ok()
        .map(CataloguePreviewEntry::new)
}
