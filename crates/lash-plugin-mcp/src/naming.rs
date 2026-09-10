//! MCP tool naming and identity helpers. Model-facing names are bounded,
//! readable projections of the durable raw server/tool identity.

use lash_tool_support::ToolBinding;

/// Build an unambiguous durable id from the configured server name and the
/// native tool name. Byte-length framing keeps arbitrary delimiters in either
/// component from aliasing another server/tool pair.
pub(crate) fn durable_tool_id(server_name: &str, native_tool_name: &str) -> String {
    format!(
        "mcp:{}:{server_name}/{}:{native_tool_name}",
        server_name.len(),
        native_tool_name.len()
    )
}

const MODEL_NAME_LIMIT: usize = 64;
const DIGEST_BYTES: usize = 16;
const DIGEST_BASE32_LEN: usize = 26;
const MODEL_NAME_FIXED_LEN: usize = "mcp__".len() + "__".len() + "_".len() + DIGEST_BASE32_LEN;
const READABLE_BUDGET: usize = MODEL_NAME_LIMIT - MODEL_NAME_FIXED_LEN;

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

fn identity_digest(tool_id: &str) -> [u8; DIGEST_BYTES] {
    let mut digest = [0; DIGEST_BYTES];
    digest.copy_from_slice(&blake3::hash(tool_id.as_bytes()).as_bytes()[..DIGEST_BYTES]);
    digest
}

fn truncate_readable_components<'a>(server: &'a str, tool: &'a str) -> (&'a str, &'a str) {
    if server.len() + tool.len() <= READABLE_BUDGET {
        return (server, tool);
    }

    let half = READABLE_BUDGET / 2;
    let mut server_len = server.len().min(half);
    let mut tool_len = tool.len().min(half);
    let spare = READABLE_BUDGET - server_len - tool_len;
    if server.len() > server_len {
        server_len += spare.min(server.len() - server_len);
    } else {
        tool_len += spare.min(tool.len() - tool_len);
    }
    (&server[..server_len], &tool[..tool_len])
}

fn digest_base32(digest: [u8; DIGEST_BYTES]) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut encoded = String::with_capacity(DIGEST_BASE32_LEN);
    let mut buffer = 0_u32;
    let mut bits = 0_u8;
    for byte in digest {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            encoded.push(ALPHABET[((buffer >> bits) & 0x1f) as usize] as char);
        }
    }
    if bits > 0 {
        encoded.push(ALPHABET[((buffer << (5 - bits)) & 0x1f) as usize] as char);
    }
    debug_assert_eq!(encoded.len(), DIGEST_BASE32_LEN);
    encoded
}

/// Build a provider-safe model name and Lashlang binding for one raw MCP tool.
///
/// The always-present 128-bit suffix hashes the complete durable tool id. The
/// readable server/tool prefix is ASCII-normalized and truncated so the final
/// name never exceeds 64 bytes. No catalog membership or ordering participates
/// in the result.
pub fn build_prefixed_name(server_name: &str, original_tool_name: &str) -> (String, ToolBinding) {
    let tool_id = durable_tool_id(server_name, original_tool_name);
    build_prefixed_name_with_digest(server_name, original_tool_name, identity_digest(&tool_id))
}

pub(crate) fn build_prefixed_name_with_digest(
    server_name: &str,
    original_tool_name: &str,
    digest: [u8; DIGEST_BYTES],
) -> (String, ToolBinding) {
    let server = normalize_identifier(server_name);
    let tool = normalize_identifier(original_tool_name);
    let (server, tool) = truncate_readable_components(&server, &tool);
    let operation = format!("{tool}_{}", digest_base32(digest));
    let prefixed = format!("mcp__{server}__{operation}");
    debug_assert!(prefixed.len() <= MODEL_NAME_LIMIT);
    let lashlang_binding = ToolBinding::new([server], operation);
    (prefixed, lashlang_binding)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_tool_id_length_frames_raw_server_and_native_names() {
        assert_eq!(
            durable_tool_id("My Server", "get/user:one"),
            "mcp:9:My Server/12:get/user:one"
        );
        assert_ne!(durable_tool_id("a/b", "c"), durable_tool_id("a", "b/c"));
    }

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
    fn build_prefixed_name_is_identity_stable_and_has_no_raw_alias() {
        let (name, meta) = build_prefixed_name("appworld", "spotify-search-songs");
        assert!(name.starts_with("mcp__appworld__spotify_"), "{name}");
        assert!(name.len() <= 64, "{name}");
        assert!(
            name.rsplit_once('_')
                .is_some_and(|(_, suffix)| suffix.len() == DIGEST_BASE32_LEN
                    && suffix
                        .bytes()
                        .all(|byte| byte.is_ascii_lowercase() || (b'2'..=b'7').contains(&byte))),
            "{name}"
        );
        assert_eq!(meta.module_path, vec!["appworld".to_string()]);
        assert_eq!(
            meta.operation.as_deref(),
            name.rsplit_once("__").map(|(_, operation)| operation)
        );
        assert!(meta.aliases.is_empty());
    }

    #[test]
    fn build_prefixed_name_matches_independent_blake3_base32_vector() {
        // Independently calculated with Python's blake3 package and
        // base64.b32encode over the first 16 digest bytes.
        let (name, _) = build_prefixed_name("docs", "search-docs");
        assert_eq!(name, "mcp__docs__search_docs_6rlrgooy6v2wymnh6or7j4q5ve");
    }

    #[test]
    fn build_prefixed_name_does_not_depend_on_catalog_neighbors() {
        let (alone_name, alone_binding) = build_prefixed_name("directory", "get_user");
        let _neighbor = build_prefixed_name("directory", "get-user");
        let (neighbor_name, neighbor_binding) = build_prefixed_name("directory", "get_user");

        assert_eq!(alone_name, neighbor_name);
        assert_eq!(alone_binding.operation, neighbor_binding.operation);
    }

    #[test]
    fn build_prefixed_name_bounds_ascii_and_unicode_inputs() {
        for (server, tool) in [
            ("a".repeat(15), "b".repeat(15)),
            ("a".repeat(15), "b".repeat(16)),
            ("服務器".repeat(40), "🔎 documents".repeat(40)),
        ] {
            let (name, _) = build_prefixed_name(&server, &tool);
            assert!(name.is_ascii(), "{name}");
            assert!(name.len() <= MODEL_NAME_LIMIT, "{}: {name}", name.len());
        }

        let (boundary_64, _) = build_prefixed_name(&"a".repeat(15), &"b".repeat(15));
        let (would_be_65, _) = build_prefixed_name(&"a".repeat(15), &"b".repeat(16));
        assert_eq!(boundary_64.len(), 64);
        assert_eq!(would_be_65.len(), 64);
        assert_ne!(boundary_64, would_be_65);
    }

    #[test]
    fn forced_digest_collision_produces_the_same_final_name() {
        let digest = [7; DIGEST_BYTES];
        let (hyphenated, _) = build_prefixed_name_with_digest("directory", "get-user", digest);
        let (underscored, _) = build_prefixed_name_with_digest("directory", "get_user", digest);
        assert_eq!(hyphenated, underscored);
    }
}
