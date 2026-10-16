//! SM-COVER (FIG-6376): every statement and action a lowered document's own
//! code spells, in `main`, entry bodies and closures, has ranges in the
//! document's source map. What no source spells is named by kind.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_dialect::Lowered;
use lash_kernel_doc::{Document, Node, Site, Unit};

/// Every statement and action site of `document`'s code.
fn code_sites(document: &Document) -> Vec<(Site, String)> {
    fn walk(node: Node<'_>, site: Site, out: &mut Vec<(Site, String)>) {
        if matches!(node, Node::Stmt(_) | Node::Action(_)) {
            out.push((site.clone(), format!("{node:?}")));
        }
        for (child, index) in node.children().into_iter().zip(0u32..) {
            walk(child, site.child(index), out);
        }
    }
    let mut out = Vec::new();
    walk(
        Node::Block(&document.main),
        Site::new(Unit::Main, Vec::new()),
        &mut out,
    );
    for (name, function) in &document.functions {
        walk(
            Node::Block(&function.body),
            Site::new(Unit::Function(name.clone()), Vec::new()),
            &mut out,
        );
    }
    out
}

/// The sites of `lowered`'s code its map gives no range, each with its node.
fn unmapped(lowered: &Lowered) -> BTreeMap<String, String> {
    let map = lowered.source_map.as_ref().expect("a lowered unit's map");
    let mapped: BTreeSet<&Site> = map
        .sites
        .iter()
        .filter(|located| !located.ranges.is_empty())
        .map(|located| &located.site)
        .collect();
    code_sites(&lowered.document)
        .into_iter()
        .filter(|(site, _)| !mapped.contains(site))
        .map(|(site, node)| (site.to_string(), node))
        .collect()
}

/// A workflow's entries, a declared async function and a const-bound
/// arrow, with closures, a branch and non-ASCII text: every statement of
/// their bodies is mapped.
#[test]
fn every_entry_body_statement_and_action_is_mapped() {
    let source = "export async function grüße(name: string): Promise<string> {\n  const said = await echo(`héllo ${name}`);\n  if (said === \"ß\") {\n    return \"ok\";\n  }\n  return said;\n}\nconst twice = async (n: number): Promise<number> => {\n  const helper = async (m: number) => m * 2;\n  return await helper(n);\n};\n";
    let effects = super::machine::effects();
    let lowered = crate::lower_workflow(
        source,
        &lash_kernel_dialect::Environment {
            library: super::library(),
            effects: &effects,
            controls: super::controls(),
            tool_roots: &BTreeSet::new(),
            bindings: &BTreeSet::new(),
            functions: &BTreeMap::new(),
        },
    )
    .unwrap();
    assert_eq!(lowered.document.entries.len(), 2);
    assert_eq!(unmapped(&lowered), BTreeMap::new());
}

/// A cell's closures, loop, try and a process arrow lifted to an entry:
/// every statement is mapped.
#[test]
fn every_cell_statement_and_action_is_mapped() {
    let source = "const ünïcode = \"日本\";\nlet total = 0;\nfor (const item of [1, 2]) {\n  try {\n    total += await echo(item);\n  } catch (error) {\n    total -= 1;\n  }\n}\nconst add = async (x: number) => x + total;\nawait finish(await add(1));\n";
    let lowered = super::lower_with_effects(source).unwrap();
    assert_eq!(unmapped(&lowered), BTreeMap::new());
}
