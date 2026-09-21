//! Regression gate for the facade "one home" invariant.
//!
//! `lib.rs` states the contract: "Every public name has exactly one home."
//! This test parses the crate's own public re-export map (`pub use` statements)
//! and asserts no public name is re-exported from two module homes.
//!
//! One exemption, intentional: `prelude` duplicates a curated subset of root
//! names for ergonomic glob imports.

// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

fn read(rel: &str) -> String {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// Extract the leaf identifiers introduced by the body of a `pub use ...;`
/// statement. Glob (`::*`) imports introduce no nameable leaf and are skipped.
fn leaves_of(body: &str) -> Vec<String> {
    let s: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.contains("::*") || s.trim_end().ends_with('*') {
        return Vec::new();
    }
    let items: Vec<String> = if let Some(open) = s.find('{') {
        let close = s.rfind('}').unwrap_or(s.len());
        s[open + 1..close]
            .split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect()
    } else {
        vec![s.trim().to_string()]
    };
    items
        .into_iter()
        .filter_map(|it| {
            let leaf = if let Some(idx) = it.find(" as ") {
                it[idx + 4..].trim().to_string()
            } else {
                it.rsplit("::").next().unwrap_or(&it).trim().to_string()
            };
            let is_ident = leaf
                .chars()
                .next()
                .map(|c| c.is_ascii_alphabetic() || c == '_')
                .unwrap_or(false)
                && leaf.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            is_ident.then_some(leaf)
        })
        .collect()
}

/// Collect `(leaf_name, module_home)` pairs for every `pub use` in `src`.
///
/// `forced_module` names the home for a file module whose whole body is one
/// module (`usage`, `admin`, ...). For `lib.rs` (`forced_module == None`) the
/// home is the full path of enclosing inline `pub mod`s joined with `::`
/// (so nested sub-namespaces like `remote::llm` are distinct homes from a
/// top-level `llm`), or `root` at file scope.
fn collect(src: &str, forced_module: Option<&str>) -> Vec<(String, String)> {
    let bytes = src.as_bytes();

    // Inline `pub mod NAME { .. }` ranges (byte spans), only relevant for lib.rs.
    let mut ranges: Vec<(String, usize, usize)> = Vec::new();
    if forced_module.is_none() {
        let mut search = 0;
        while let Some(rel) = src[search..].find("pub mod ") {
            let start = search + rel;
            let after = start + "pub mod ".len();
            let name: String = src[after..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            let mut j = after + name.len();
            while j < bytes.len() && bytes[j] != b'{' && bytes[j] != b';' {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'{' {
                let mut depth = 0usize;
                let mut k = j;
                while k < bytes.len() {
                    match bytes[k] {
                        b'{' => depth += 1,
                        b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    k += 1;
                }
                ranges.push((name.clone(), start, k));
            }
            search = after;
        }
    }

    let module_for = |pos: usize| -> String {
        if let Some(m) = forced_module {
            return m.to_string();
        }
        // Every inline module enclosing `pos`, outermost first (ranges are
        // discovered in source order, so enclosing modules sort by start).
        let mut chain: Vec<&(String, usize, usize)> =
            ranges.iter().filter(|r| r.1 <= pos && pos <= r.2).collect();
        chain.sort_by_key(|r| r.1);
        if chain.is_empty() {
            "root".to_string()
        } else {
            chain
                .iter()
                .map(|r| r.0.as_str())
                .collect::<Vec<_>>()
                .join("::")
        }
    };

    let mut out = Vec::new();
    let mut search = 0;
    while let Some(rel) = src[search..].find("pub use ") {
        let start = search + rel;
        let line_start = src[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
        if src[line_start..start].trim_start().starts_with("//") {
            search = start + "pub use ".len();
            continue;
        }
        let mut j = start;
        while j < bytes.len() && bytes[j] != b';' {
            j += 1;
        }
        let body = &src[start + "pub use ".len()..j];
        let module = module_for(start);
        for leaf in leaves_of(body) {
            out.push((leaf, module.clone()));
        }
        search = j + 1;
    }
    out
}

#[test]
fn every_public_name_has_exactly_one_home() {
    let mut pairs = Vec::new();
    pairs.extend(collect(&read("src/lib.rs"), None));
    // File modules that are `pub mod` and carry their own `pub use` re-exports.
    pairs.extend(collect(&read("src/usage.rs"), Some("usage")));
    pairs.extend(collect(&read("src/admin.rs"), Some("admin")));
    pairs.extend(collect(&read("src/turn.rs"), Some("turn")));
    pairs.extend(collect(
        &read("src/scenario_contracts.rs"),
        Some("scenario_contracts"),
    ));
    pairs.extend(collect(&read("src/testing.rs"), Some("testing")));
    pairs.extend(collect(&read("src/rlm.rs"), Some("rlm")));

    let mut homes: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (name, module) in pairs {
        // Prelude names deliberately alias their canonical root home.
        let module = if module == "prelude" {
            "root".to_string()
        } else {
            module
        };
        homes.entry(name).or_default().insert(module);
    }

    // Sanity check: the parser found a meaningful surface. If this drops to a
    // handful, the parser broke and the gate would silently pass.
    assert!(
        homes.len() > 100,
        "parsed only {} public names — the export-map parser likely broke",
        homes.len()
    );

    let violations: Vec<(String, Vec<String>)> = homes
        .into_iter()
        .filter(|(_, mods)| mods.len() > 1)
        .map(|(name, mods)| (name, mods.into_iter().collect()))
        .collect();

    assert!(
        violations.is_empty(),
        "facade public names re-exported from more than one module home \
         (violates the \"every public name has exactly one home\" contract): {violations:#?}",
    );
}
