//! Tool name prefixing helpers. Moved verbatim from `crates/lash/src/mcp.rs` so the
//! `mcp__<server>__<tool>` naming scheme keeps producing identical names
//! (avoids breaking config files / saved trajectories that reference tools
//! by their full prefixed name).

use std::collections::BTreeSet;

use lash_tool_support::ToolBinding;

/// Normalise a server name or raw MCP tool name to lowercase ASCII
/// alphanumeric and underscore. Collapses runs of non-alphanumeric characters
/// into a single `_`, trims trailing underscores, and falls back to `"tool"`
/// if the input has no usable characters.
pub fn normalize_identifier(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_underscore = false;
    for ch in raw.chars() {
        let normalized = if ch.is_ascii_alphanumeric() { ch } else { '_' };
        if normalized == '_' {
            if !last_underscore && !out.is_empty() {
                out.push('_');
            }
            last_underscore = true;
        } else {
            out.push(normalized.to_ascii_lowercase());
            last_underscore = false;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    if out.is_empty() {
        "tool".to_string()
    } else {
        out
    }
}

/// Ensure `base` is unique against `used_names`. Appends `_2`, `_3`, ... until
/// a free name is found. The returned name is also recorded in `used_names`.
pub fn unique_prefixed_name(base: &str, used_names: &mut BTreeSet<String>) -> String {
    if used_names.insert(base.to_string()) {
        return base.to_string();
    }
    for idx in 2.. {
        let candidate = format!("{base}_{idx}");
        if used_names.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!("integer range exhausted while uniquifying tool name")
}

/// Build the prefixed tool name (`mcp__<server>__<tool>`) and discovery
/// metadata for one MCP tool. The original (unnormalised) tool name is kept
/// as an alias so users can search/find by the server's native name.
pub fn build_prefixed_name(
    server_name: &str,
    original_tool_name: &str,
    used_names: &mut BTreeSet<String>,
) -> (String, ToolBinding) {
    let server_prefix = normalize_identifier(server_name);
    let normalized_tool = normalize_identifier(original_tool_name);
    let prefixed = unique_prefixed_name(
        &format!("mcp__{server_prefix}__{normalized_tool}"),
        used_names,
    );
    let operation = prefixed
        .rsplit_once("__")
        .map(|(_, operation)| operation)
        .expect("prefixed MCP tool name contains the module separator");
    let lashlang_binding =
        ToolBinding::new([server_prefix], operation).with_aliases([original_tool_name.to_string()]);
    (prefixed, lashlang_binding)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_identifier_lowercases_and_dedups_underscores() {
        assert_eq!(
            normalize_identifier("Spotify-Search Songs"),
            "spotify_search_songs"
        );
        assert_eq!(normalize_identifier("___foo___bar___"), "foo_bar");
        assert_eq!(normalize_identifier("!!!"), "tool");
    }

    #[test]
    fn unique_prefixed_name_appends_index_on_collision() {
        let mut used = BTreeSet::new();
        assert_eq!(unique_prefixed_name("tool", &mut used), "tool");
        assert_eq!(unique_prefixed_name("tool", &mut used), "tool_2");
        assert_eq!(unique_prefixed_name("tool", &mut used), "tool_3");
    }

    #[cfg(feature = "lashlang")]
    #[test]
    fn build_prefixed_name_keeps_module_path_and_original_alias() {
        let mut used = BTreeSet::new();
        let (name, meta) = build_prefixed_name("appworld", "spotify-search-songs", &mut used);
        assert_eq!(name, "mcp__appworld__spotify_search_songs");
        assert_eq!(meta.module_path, vec!["appworld".to_string()]);
        assert_eq!(meta.operation.as_deref(), Some("spotify_search_songs"));
        assert_eq!(meta.aliases, vec!["spotify-search-songs".to_string()]);
    }

    #[cfg(feature = "lashlang")]
    #[test]
    fn build_prefixed_name_uniquifies_the_lashlang_operation() {
        let mut used = BTreeSet::new();
        let (_, first) = build_prefixed_name("directory", "get-user", &mut used);
        let (_, second) = build_prefixed_name("directory", "get_user", &mut used);

        assert_eq!(first.operation.as_deref(), Some("get_user"));
        assert_eq!(second.operation.as_deref(), Some("get_user_2"));
    }
}
