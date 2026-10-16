//! SM-COVER (FIG-6376): every statement and action a lowered cell's own
//! code spells, in `main`, function bodies and closures, has ranges in its
//! source map. What no source spells is named by kind.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_dialect::Lowered;
use lash_kernel_doc::{Document, Expr, Literal, Node, Rhs, Site, Stmt, TryStmt, Unit};

/// Every statement and action site of `document`'s code.
fn code_sites(document: &Document) -> Vec<(Site, String)> {
    fn walk(node: Node<'_>, site: Site, out: &mut Vec<(Site, String)>) {
        if matches!(
            node,
            Node::Stmt(_) | Node::Action(_) | Node::Expr(Expr::Act(_))
        ) {
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

/// The sites of `lowered`'s code its map gives no range, each named by the
/// kind of unspelled code it is.
fn unmapped(lowered: &Lowered) -> BTreeMap<String, &'static str> {
    let map = lowered.source_map.as_ref().expect("a lowered cell's map");
    let mapped: BTreeSet<&Site> = map
        .sites
        .iter()
        .filter(|located| !located.ranges.is_empty())
        .map(|located| &located.site)
        .collect();
    let main = &lowered.document.main;
    // The `try` that wraps a cell starting tasks, whose `finally` cancels
    // what is still running.
    let finalizer = match main.last() {
        Some(Stmt::Try(TryStmt {
            catch: None,
            finally: Some(_),
            ..
        })) => Some(u32::try_from(main.len() - 1).unwrap()),
        _ => None,
    };
    code_sites(&lowered.document)
        .into_iter()
        .filter(|(site, _)| !mapped.contains(site))
        .map(|(site, node)| {
            let kind = match (site.path.as_slice(), lowered.document.node(&site)) {
                (
                    [_],
                    Some(Node::Stmt(Stmt::Let {
                        value: Rhs::Expr(Expr::Literal(Literal::Absent)),
                        ..
                    })),
                ) if site.unit == Unit::Main => "absent-binding initialization",
                ([at], _) | ([at, 1, ..], _)
                    if site.unit == Unit::Main && Some(*at) == finalizer =>
                {
                    "task finalizer"
                }
                _ => panic!("{site} is unmapped: {node}"),
            };
            (site.to_string(), kind)
        })
        .collect()
}

/// A cell with async functions, a lambda, a loop, a try, a task and
/// non-ASCII text: every statement and action is mapped but the dialect's
/// unspelled prologue and epilogue.
#[test]
fn every_cell_statement_and_action_is_mapped() {
    let source = "größe = \"日本\"\nasync def scale(x: int, extra: int = 1) -> int:\n    async def inner(y):\n        return y * 2\n    return await inner(x) + extra\ntwice = lambda y: y * 2\ntotal = 0\nfor item in [1, 2]:\n    try:\n        total += await echo(item)\n    except Exception:\n        total -= 1\n    if total > 3:\n        break\ntask = asyncio.create_task(scale(total))\nprint(größe, twice(await task))\n";
    let lowered = super::machine::lower(source).unwrap();
    let mut expected: BTreeMap<String, &str> = (0..6)
        .map(|index| (format!("main/{index}"), "absent-binding initialization"))
        .collect();
    for site in ["main/6", "main/6/1/0", "main/6/1/0/0"] {
        expected.insert(site.to_owned(), "task finalizer");
    }
    assert_eq!(unmapped(&lowered), expected);
}
