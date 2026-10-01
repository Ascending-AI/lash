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
    fn server_prefix_keeps_its_lowercase_normalization() {
        assert_eq!(
            normalize_identifier("Spotify-Search Songs"),
            "spotify_search_songs"
        );
        assert_eq!(normalize_identifier("___foo___bar___"), "foo_bar");
        assert_eq!(normalize_identifier("!!!"), "tool");
    }

    #[test]
    fn lone_tools_use_bare_cleaned_names_and_no_aliases() {
        for raw in ["search_docs", "search-docs"] {
            let names = build_catalog_names("Docs", &[raw]);
            let (name, binding) = &names[raw];
            assert_eq!(name, "mcp__docs__search_docs");
            assert_eq!(binding.module_path, ["docs"]);
            assert_eq!(binding.operation.as_deref(), Some("search_docs"));
            assert!(binding.aliases.is_empty());
        }
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
    fn collision_suffixes_match_independent_blake3_base32_vectors() {
        let names = build_catalog_names("docs", &["search-docs", "search_docs"]);
        // Python blake3 and base64.b32encode over the durable id, computed separately.
        assert_eq!(names["search-docs"].0, "mcp__docs__search_docs__6rlrgooy");
        assert_eq!(names["search_docs"].0, "mcp__docs__search_docs__ac5edv22");
        assert_ne!(names["search-docs"].0, names["search_docs"].0);
    }

    #[test]
    fn collision_members_rename_symmetrically_and_return_to_bare_on_removal() {
        let alone = build_catalog_names("directory", &["get_user", "unrelated"]);
        let pair = build_catalog_names("directory", &["get_user", "get-user", "unrelated"]);
        let reversed = build_catalog_names("directory", &["unrelated", "get-user", "get_user"]);
        assert_eq!(alone["get_user"].0, "mcp__directory__get_user");
        for raw in ["get_user", "get-user"] {
            assert!(pair[raw].0.starts_with("mcp__directory__get_user__"));
            assert_eq!(pair[raw].0.len(), "mcp__directory__get_user__".len() + 8);
            assert_eq!(pair[raw].0, reversed[raw].0);
        }
        assert_ne!(pair["get_user"].0, pair["get-user"].0);
        assert_eq!(alone["unrelated"].0, pair["unrelated"].0);
        assert_eq!(
            build_catalog_names("directory", &["get_user"])["get_user"].0,
            alone["get_user"].0
        );
    }

    #[test]
    fn names_are_bounded_ascii_including_truncation_collisions() {
        let fits = "a".repeat(53);
        let longer = format!("{fits}b");
        assert_eq!(build_catalog_names("docs", &[&fits])[&fits].0.len(), 64);
        assert_eq!(
            build_catalog_names("docs", &[&longer])[&longer].0,
            format!("mcp__docs__{fits}")
        );
        let collided = build_catalog_names("docs", &[&fits, &longer]);
        assert_ne!(collided[&fits].0, collided[&longer].0);
        for (name, binding) in collided.values() {
            assert_eq!(name.len(), 64);
            assert_eq!(binding.operation.as_ref().expect("operation").len(), 53);
            assert_eq!(name.rsplit_once("__").expect("suffix").1.len(), 8);
        }
        for server in ["s".repeat(200), "服務器".repeat(40)] {
            for tool in ["🔎 documents".repeat(40), "a".repeat(200)] {
                let names = build_catalog_names(&server, &[&tool]);
                assert!(names[&tool].0.is_ascii());
                assert!(names[&tool].0.len() <= 64);
            }
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
    }

    #[test]
    fn random_catalogs_have_unique_order_independent_names() {
        let mut rng = fastrand::Rng::with_seed(4552);
        let alphabet = ['a', 'A', '1', '_', '-', '.', ' ', '文'];
        for _ in 0..256 {
            let mut raw = BTreeSet::from([
                "control".to_string(),
                "get_user".to_string(),
                "get-user".to_string(),
            ]);
            for _ in 0..rng.usize(1..40) {
                raw.insert(
                    (0..rng.usize(0..100))
                        .map(|_| alphabet[rng.usize(..alphabet.len())])
                        .collect::<String>(),
                );
            }
            let mut raw = raw.iter().map(String::as_str).collect::<Vec<_>>();
            let names = build_catalog_names("docs", &raw);
            rng.shuffle(&mut raw);
            let reordered = build_catalog_names("docs", &raw);
            assert_eq!(names["control"].0, "mcp__docs__control");
            assert_eq!(
                names.len(),
                names
                    .values()
                    .map(|entry| &entry.0)
                    .collect::<BTreeSet<_>>()
                    .len()
            );
            for (raw, (name, binding)) in names {
                assert_eq!(name, reordered[&raw].0);
                assert_eq!(binding.operation, reordered[&raw].1.operation);
                assert!(name.is_ascii() && name.len() <= 64);
            }
        }
    }
}
