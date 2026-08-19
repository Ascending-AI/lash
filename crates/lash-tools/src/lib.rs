//! Built-in tool suite for the lash agent runtime.
//!
//! Each module is a self-contained tool family sharing the
//! [`lash_tool_support`] utility layer:
//!
//! - [`files`] — `files.read` / `files.glob` / `files.edit` / `files.write`
//! - [`shell`] — `shell.exec` / `shell.start` / `shell.write`
//! - [`web`] — `web.fetch` / `web.search`
//!
//! Local grep is Host Application policy and lives outside this crate so
//! embedders do not inherit a native indexing dependency.

pub mod files;
pub mod shell;
pub mod web;

#[cfg(test)]
mod tests {
    use lash_core::ToolProvider;

    fn all_manifests() -> Vec<lash_core::ToolManifest> {
        let mut manifests = Vec::new();
        manifests.extend(crate::files::edit_provider().tool_manifests());
        manifests.extend(crate::files::write_provider().tool_manifests());
        manifests.extend(crate::files::read_file_provider().tool_manifests());
        manifests.extend(crate::files::glob_provider().tool_manifests());
        manifests.extend(
            crate::shell::shell_provider(crate::shell::StandardShell::new()).tool_manifests(),
        );
        manifests.extend(crate::web::fetch_url_provider("").tool_manifests());
        manifests.extend(crate::web::web_search_provider("").tool_manifests());
        manifests
    }

    #[cfg(not(feature = "lashlang"))]
    #[test]
    fn default_manifests_do_not_include_lashlang_bindings() {
        for manifest in all_manifests() {
            assert!(
                manifest.bindings.is_empty(),
                "{} unexpectedly had bindings: {:?}",
                manifest.name,
                manifest.bindings
            );
        }
    }

    #[cfg(feature = "lashlang")]
    #[test]
    fn lashlang_manifests_include_lashlang_bindings() {
        for manifest in all_manifests() {
            assert!(
                manifest
                    .bindings
                    .contains_key(lash_lashlang_runtime::LASHLANG_TOOL_BINDING_KEY),
                "{} did not include a lashlang binding",
                manifest.name
            );
        }
    }
}
