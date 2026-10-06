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

#[test]
fn every_public_name_has_exactly_one_home() {
    let exports = super::facade_inventory::whole_module_coverage::facade_exports();
    let pairs = exports.into_iter().flat_map(|(module, (leaves, _))| {
        leaves.into_iter().map(move |name| (name, module.clone()))
    });

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
