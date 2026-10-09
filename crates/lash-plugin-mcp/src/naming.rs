//! MCP names are a bounded projection of a server's current catalog.
//! Dispatch identity always uses the unchanged, length-framed raw names.

use lash_tool_support::ToolBinding;
use std::collections::{BTreeMap, BTreeSet};

/// Byte-length framing keeps delimiters in either component unambiguous.
pub(crate) fn durable_tool_id(server_name: &str, native_tool_name: &str) -> String {
    format!(
        "mcp:{}:{server_name}/{}:{native_tool_name}",
        server_name.len(),
        native_tool_name.len()
    )
}

const MODEL_NAME_LIMIT: usize = 64;
const MODEL_PREFIX_LEN: usize = "mcp__".len() + "__".len();
const DIGEST_BYTES: usize = 5;
const COLLISION_SUFFIX_LEN: usize = "__".len() + 8;
const SERVER_LIMIT: usize = MODEL_NAME_LIMIT - MODEL_PREFIX_LEN - COLLISION_SUFFIX_LEN - 1;

/// Normalize a configured server prefix to lowercase ASCII, collapsing
/// separators and trimming edge underscores. Empty prefixes become `tool`.
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

pub(crate) fn server_prefix(raw: &str) -> String {
    let mut prefix = normalize_identifier(raw);
    prefix.truncate(SERVER_LIMIT);
    prefix
}

fn clean_tool_name(raw: &str) -> String {
    let mut cleaned = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if cleaned.is_empty() {
        cleaned.push_str("tool");
    }
    if cleaned.starts_with(|ch: char| ch.is_ascii_digit()) {
        cleaned.insert(0, '_');
    }
    cleaned
}

fn identity_digest(tool_id: &str) -> [u8; DIGEST_BYTES] {
    let mut digest = [0; DIGEST_BYTES];
    digest.copy_from_slice(&blake3::hash(tool_id.as_bytes()).as_bytes()[..DIGEST_BYTES]);
    digest
}

fn digest_base32(digest: [u8; DIGEST_BYTES]) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut encoded = String::with_capacity(8);
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
    encoded
}

/// Assign bare cleaned names, preserving ASCII tool case. Every member of a
/// cleanup or truncation collision receives `__` and the first eight base32
/// characters of its durable-id digest. A bare name colliding with a generated
/// name joins the hashed group. Catalog order never decides a winner.
///
/// Refresh may rename operations; durable ids and recorded definitions retain
/// dispatch authority. No aliases or persistent allocation table are created.
/// A true 40-bit digest collision under the same truncated operation is the
/// sole tool-name collision that still receives the pool's typed config refusal.
/// Static server-prefix collisions remain a configuration refusal.
pub fn build_catalog_names(
    server_name: &str,
    names: &[&str],
) -> BTreeMap<String, (String, ToolBinding)> {
    build_catalog_names_with_digest(server_name, names, identity_digest)
}

pub(crate) fn build_catalog_names_with_digest(
    server_name: &str,
    names: &[&str],
    digest: impl Fn(&str) -> [u8; DIGEST_BYTES],
) -> BTreeMap<String, (String, ToolBinding)> {
    let server = server_prefix(server_name);
    let budget = MODEL_NAME_LIMIT - MODEL_PREFIX_LEN - server.len();
    let cleaned = names
        .iter()
        .map(|raw| ((*raw).to_owned(), clean_tool_name(raw)))
        .collect::<BTreeMap<_, _>>();
    let mut hashed = BTreeSet::new();
    loop {
        let names = cleaned
            .iter()
            .map(|(raw, tool)| {
                let operation = if hashed.contains(raw) {
                    let suffix = digest_base32(digest(&durable_tool_id(server_name, raw)));
                    format!(
                        "{}__{suffix}",
                        &tool[..tool.len().min(budget - COLLISION_SUFFIX_LEN)]
                    )
                } else {
                    tool[..tool.len().min(budget)].to_owned()
                };
                let name = format!("mcp__{server}__{operation}");
                (
                    raw.clone(),
                    (name, ToolBinding::new([server.as_str()], operation)),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut groups = BTreeMap::<&str, Vec<&String>>::new();
        for (raw, (name, _)) in &names {
            groups.entry(name).or_default().push(raw);
        }
        let mut changed = false;
        for group in groups.values().filter(|group| group.len() > 1) {
            for raw in group {
                changed |= hashed.insert((*raw).clone());
            }
        }
        if !changed {
            return names;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn durable_tool_id_length_frames_raw_server_and_native_names() {
        assert_eq!(
            durable_tool_id("My Server", "get/user:one"),
            "mcp:9:My Server/12:get/user:one"
        );
        assert_ne!(durable_tool_id("a/b", "c"), durable_tool_id("a", "b/c"));
    }

    #[test]
    fn cleanup_preserves_case_and_maps_each_unaddressable_character() {
        for (raw, cleaned) in [
            ("getUser", "getUser"),
            ("a-. b", "a___b"),
            ("文🔎", "__"),
            ("1lookup", "_1lookup"),
            ("", "tool"),
            ("_end_", "_end_"),
        ] {
            let names = build_catalog_names("docs", &[raw]);
            assert_eq!(names[raw].0, format!("mcp__docs__{cleaned}"));
        }
    }

    #[test]
    fn generated_suffix_collision_hashes_the_bare_occupant_too() {
        let names = build_catalog_names(
            "docs",
            &["search-docs", "search_docs", "search_docs__6rlrgooy"],
        );
        assert_eq!(
            names
                .values()
                .map(|entry| &entry.0)
                .collect::<BTreeSet<_>>()
                .len(),
            3
        );
        assert_eq!(names["search-docs"].0, "mcp__docs__search_docs__6rlrgooy");
        assert_ne!(
            names["search_docs__6rlrgooy"].0,
            "mcp__docs__search_docs__6rlrgooy"
        );

        let fits = "a".repeat(53);
        assert_eq!(build_catalog_names("docs", &[&fits])[&fits].0.len(), 64);
        for server in ["s".repeat(200), "服務器".repeat(40)] {
            for tool in ["🔎 documents".repeat(40), "a".repeat(200)] {
                let names = build_catalog_names(&server, &[&tool]);
                assert!(names[&tool].0.is_ascii());
                assert!(names[&tool].0.len() <= 64);
            }
        }
    }
}
