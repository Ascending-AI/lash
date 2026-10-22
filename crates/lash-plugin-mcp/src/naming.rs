//! MCP names are a bounded projection of a server's current catalog: one
//! canonical server segment per server and one canonical operation name per
//! tool (NAME-CANON). Dispatch identity always uses the unchanged,
//! length-framed raw names.

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
/// `_h` and eight base32 characters of a durable-id digest.
const COLLISION_SUFFIX_LEN: usize = "_h".len() + 8;
const SERVER_LIMIT: usize = MODEL_NAME_LIMIT - MODEL_PREFIX_LEN - COLLISION_SUFFIX_LEN - 1;

/// What a canonical name is allocated for: the two differ only in the
/// prefix a digit-led first segment takes.
#[derive(Clone, Copy)]
enum Kind {
    Server,
    Tool,
}

/// `raw` as one canonical segment, unbounded: lowercase with camel humps
/// split (`getUser` is `get_user`), every other non-alphanumeric run one
/// `_`, edge underscores trimmed; each digit-led part folded into the part
/// before it (`get_2fa_code` is `get2fa_code`); a digit-led first part
/// prefixed (`mcp123`, `tool2fa`). An empty name is `tool`.
fn canonical(raw: &str, kind: Kind) -> String {
    let chars = raw.chars().collect::<Vec<_>>();
    let mut words = String::with_capacity(raw.len());
    for (index, &ch) in chars.iter().enumerate() {
        if !ch.is_ascii_alphanumeric() {
            words.push('_');
            continue;
        }
        if ch.is_ascii_uppercase() && index > 0 {
            let previous = chars[index - 1];
            let next = chars.get(index + 1).copied();
            let hump = previous.is_ascii_lowercase()
                || previous.is_ascii_digit()
                || (previous.is_ascii_uppercase() && next.is_some_and(|c| c.is_ascii_lowercase()));
            if hump {
                words.push('_');
            }
        }
        words.push(ch.to_ascii_lowercase());
    }
    let mut parts: Vec<String> = Vec::new();
    for part in words.split('_').filter(|part| !part.is_empty()) {
        match parts.last_mut() {
            Some(last) if part.starts_with(|ch: char| ch.is_ascii_digit()) => last.push_str(part),
            _ => parts.push(part.to_owned()),
        }
    }
    let Some(first) = parts.first_mut() else {
        return "tool".to_owned();
    };
    if first.starts_with(|ch: char| ch.is_ascii_digit()) {
        first.insert_str(
            0,
            match kind {
                Kind::Server => "mcp",
                Kind::Tool => "tool",
            },
        );
    }
    parts.join("_")
}

/// `name` cut to `budget` bytes, with no trailing `_`: still canonical,
/// since every part of it starts with a letter.
fn truncated(name: &str, budget: usize) -> String {
    let mut name = name[..name.len().min(budget)].to_owned();
    while name.ends_with('_') {
        name.pop();
    }
    name
}

/// The canonical server segment of the configured server `raw`.
pub fn server_segment(raw: &str) -> String {
    truncated(&canonical(raw, Kind::Server), SERVER_LIMIT)
}

/// Each raw tool name of `server_name`'s catalog as its canonical operation
/// name. Raw names that allocate one name (`getUser` and `get_user`) each
/// take a final `_h` segment of eight base32 characters of their durable-id
/// digest. A suffixed name colliding with a bare one joins the hashed group.
/// Catalog order never decides a winner.
///
/// Refresh may rename operations; durable ids and recorded definitions retain
/// dispatch authority. No aliases or persistent allocation table are created.
/// A true 40-bit digest collision under the same truncated name is the sole
/// tool-name collision that still receives the pool's typed config refusal.
/// Static server-segment collisions remain a configuration refusal.
pub fn build_catalog_names(server_name: &str, names: &[&str]) -> BTreeMap<String, String> {
    build_catalog_names_with_digest(
        server_name,
        names,
        lash_vocabulary::tool_contract::digest_tag,
    )
}

pub(crate) fn build_catalog_names_with_digest(
    server_name: &str,
    names: &[&str],
    digest: impl Fn(&str) -> String,
) -> BTreeMap<String, String> {
    let server = server_segment(server_name);
    let budget = MODEL_NAME_LIMIT - MODEL_PREFIX_LEN - server.len();
    let cleaned = names
        .iter()
        .map(|raw| ((*raw).to_owned(), canonical(raw, Kind::Tool)))
        .collect::<BTreeMap<_, _>>();
    let mut hashed = BTreeSet::new();
    loop {
        let names = cleaned
            .iter()
            .map(|(raw, tool)| {
                let name = if hashed.contains(raw) {
                    let suffix = digest(&durable_tool_id(server_name, raw));
                    format!(
                        "{}_h{suffix}",
                        truncated(tool, budget - COLLISION_SUFFIX_LEN)
                    )
                } else {
                    truncated(tool, budget)
                };
                (raw.clone(), name)
            })
            .collect::<BTreeMap<_, _>>();
        let mut groups = BTreeMap::<&str, Vec<&String>>::new();
        for (raw, name) in &names {
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

    fn canonical_segment(text: &str) -> bool {
        let mut parts = text.split('_');
        parts.all(|part| {
            part.starts_with(|ch: char| ch.is_ascii_lowercase())
                && part
                    .chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit())
        })
    }

    #[test]
    fn durable_tool_id_length_frames_raw_server_and_native_names() {
        assert_eq!(
            durable_tool_id("My Server", "get/user:one"),
            "mcp:9:My Server/12:get/user:one"
        );
        assert_ne!(durable_tool_id("a/b", "c"), durable_tool_id("a", "b/c"));
    }

    /// MCP-CANON: every server segment and tool name the allocator outputs
    /// satisfies NAME-CANON, distinct raw names get distinct names, and each
    /// durable tool id keeps its raw name.
    #[test]
    fn the_allocator_gives_distinct_canonical_names() {
        for (raw, canonical) in [
            ("get_2fa_code", "get2fa_code"),
            ("getUser", "get_user"),
            ("HTTPServer", "http_server"),
            ("a-. b", "a_b"),
            ("文🔎", "tool"),
            ("2fa", "tool2fa"),
            ("", "tool"),
            ("_end_", "end"),
        ] {
            assert_eq!(build_catalog_names("docs", &[raw])[raw], canonical, "{raw}");
        }
        assert_eq!(server_segment("123"), "mcp123");
        assert_eq!(server_segment("My Server"), "my_server");

        let names = build_catalog_names("directory", &["getUser", "get_user"]);
        assert_ne!(names["getUser"], names["get_user"]);
        for (raw, name) in &names {
            let suffix =
                lash_vocabulary::tool_contract::digest_tag(&durable_tool_id("directory", raw));
            assert_eq!(name, &format!("get_user_h{suffix}"));
        }

        let tool = format!("{}_{}", "a".repeat(40), "b".repeat(40));
        let server = "s".repeat(10);
        let name = &build_catalog_names(&server, &[tool.as_str()])[&tool];
        assert!(!name.ends_with('_'), "{name}");
        for server in ["s".repeat(200), "服務器".repeat(40), "9 lives".to_owned()] {
            let segment = server_segment(&server);
            assert!(canonical_segment(&segment), "{segment}");
            for tool in [
                "🔎 documents".repeat(40),
                "a".repeat(200),
                "x_".repeat(60),
                "Get2FA-Code".to_owned(),
            ] {
                let names = build_catalog_names(&server, &[&tool, "get_user", "getUser"]);
                let distinct = names.values().collect::<BTreeSet<_>>();
                assert_eq!(distinct.len(), names.len());
                for name in names.values() {
                    assert!(canonical_segment(name), "{name}");
                    assert!(segment.len() + 2 + name.len() <= 64 - 5, "{name}");
                }
            }
        }
    }

    #[test]
    fn generated_suffix_collision_hashes_the_bare_occupant_too() {
        let suffixed = format!(
            "search_docs_h{}",
            lash_vocabulary::tool_contract::digest_tag(&durable_tool_id("docs", "search-docs"))
        );
        let names = build_catalog_names("docs", &["search-docs", "search_docs", &suffixed]);
        assert_eq!(names.values().collect::<BTreeSet<_>>().len(), 3);
        assert_eq!(names["search-docs"], suffixed);
        assert_ne!(names[&suffixed], suffixed);
    }
}
