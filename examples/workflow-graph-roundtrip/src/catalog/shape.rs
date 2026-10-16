//! SHAPE: measure the real workflow importer, without changing lowering.
//! All document-owned units count, including entry adapters and closures;
//! library helper bodies do not. Source ownership is exclusive: the smallest
//! parser statement containing a statement's source-map range owns that kernel statement.
//! Unspelled adapters remain visible in `overhead`, never silently discarded.
mod source;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use lash_kernel_dialect::{Environment, Lowered};
use lash_kernel_doc::{Node, Site, Span, Unit};

#[derive(Debug)]
struct Measurement {
    path: String,
    // statements, scopes (including empty blocks), actions, author statements,
    // maximum statements owned by one author statement, unowned overhead.
    counts: [usize; 6],
    worst: Option<Span>,
    source: String,
}

fn corpus(root: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(root).expect("catalog directory") {
        let path = entry.expect("catalog entry").path();
        if path.is_dir() {
            corpus(&path, out);
        } else if matches!(path.extension().and_then(|s| s.to_str()), Some("ts" | "py")) {
            out.push(path);
        }
    }
}

fn measure(path: String, text: String, lowered: Lowered, authors: Vec<Span>) -> Measurement {
    let map = lowered
        .source_map
        .as_ref()
        .expect("the importer maps its document's source");
    map.validate(&lowered.document, &text)
        .expect("the importer's map names real document nodes");
    let notes: BTreeMap<_, _> = map
        .sites
        .iter()
        .map(|located| (located.site.clone(), located.ranges.first().copied()))
        .collect();
    let mut owned = vec![0; authors.len()];
    let mut counts = [0; 6];
    counts[3] = authors.len();
    let mut pending = vec![(
        Site::new(Unit::Main, vec![]),
        Node::Block(&lowered.document.main),
    )];
    pending.extend(lowered.document.functions.iter().map(|(name, function)| {
        (
            Site::new(Unit::Function(name.clone()), vec![]),
            Node::Block(&function.body),
        )
    }));
    while let Some((site, node)) = pending.pop() {
        match node {
            Node::Stmt(_) => {
                counts[0] += 1;
                let owner = notes.get(&site).copied().flatten().and_then(|span| {
                    authors
                        .iter()
                        .enumerate()
                        .filter(|(_, author)| author.start <= span.start && span.end <= author.end)
                        .min_by_key(|(_, author)| author.end - author.start)
                        .map(|(index, _)| index)
                });
                if let Some(owner) = owner {
                    owned[owner] += 1;
                } else {
                    counts[5] += 1;
                }
            }
            Node::Block(_) => counts[1] += 1,
            Node::Action(_) => counts[2] += 1,
            Node::Expr(_) => {}
        }
        pending.extend(
            node.children()
                .into_iter()
                .enumerate()
                .map(|(index, child)| (site.child(index as u32), child)),
        );
    }
    // Stable ties choose the first statement in source order.
    let worst = owned
        .iter()
        .enumerate()
        .max_by_key(|(index, count)| (**count, std::cmp::Reverse(*index)));
    let worst = worst.map(|(index, count)| {
        counts[4] = *count;
        authors[index]
    });
    Measurement {
        path,
        counts,
        worst,
        source: text,
    }
}

fn measurements() -> Vec<Measurement> {
    let embedding = lash_vm_worker::Embedder::kernel()
        .expect("kernel embedding")
        .dialect(lash_vm_worker::typescript(&Default::default()))
        .expect("TypeScript embedding")
        .dialect(lash_vm_worker::python())
        .expect("Python embedding")
        .finish()
        .expect("workflow embedding");
    let host = crate::runtime::environment().expect("catalog host environment");
    let boundary = lash::vm::HostBoundary::of_links(host.effects()).expect("catalog boundary");
    let effects = boundary.signatures();
    let controls = boundary.controls();
    let mut tool_roots = BTreeSet::new();
    for tool in crate::display::tool_definitions() {
        let binding =
            lash::vm::required_tool_executable(&tool.manifest).expect("catalog tool binding");
        if let Some(root) = binding.module_path.first() {
            tool_roots.insert(lash_kernel_doc::Name::new(root));
        }
    }
    let environment = Environment {
        library: embedding.library(),
        effects: &effects,
        tool_roots: &tool_roots,
        controls: &controls,
        bindings: &BTreeSet::new(),
        functions: &BTreeMap::new(),
    };
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("catalog");
    let mut paths = Vec::new();
    corpus(&root, &mut paths);
    paths.sort();
    assert!(!paths.is_empty(), "SHAPE corpus is empty");
    paths
        .into_iter()
        .map(|path| {
            let name = path
                .strip_prefix(&root)
                .expect("catalog relative path")
                .to_str()
                .expect("UTF-8 path")
                .to_owned();
            let text = std::fs::read_to_string(&path).expect("workflow source");
            let (lowered, authors) = if path.extension().is_some_and(|ext| ext == "ts") {
                (
                    lash_dialect_typescript::lower_workflow(&text, &environment)
                        .map_err(|error| error.to_string()),
                    source::typescript(&text),
                )
            } else {
                (
                    lash_dialect_python::lower(&text, &environment)
                        .map_err(|error| error.to_string()),
                    source::python(&text),
                )
            };
            let lowered = lowered.unwrap_or_else(|error| panic!("{name}: {error}"));
            measure(name, text, lowered, authors)
        })
        .collect()
}

/// FIG-6411 / SHAPE. Run with `--nocapture` to print the human table and
/// replacement ratchet rows; no recorder or alternate lowering path exists.
#[test]
fn every_catalog_and_probe_workflow_stays_within_its_shape_ratchet() {
    let measured = measurements();
    println!(
        "workflow | statements scopes actions author max overhead | ratio | worst author statement"
    );
    for row in &measured {
        let [statements, scopes, actions, author, max, overhead] = row.counts;
        let ratio = if author == 0 {
            "n/a".to_owned()
        } else {
            format!("{:.2}", statements as f64 / author as f64)
        };
        let worst = row
            .worst
            .map(|span| {
                let line = row.source[..span.start]
                    .bytes()
                    .filter(|b| *b == b'\n')
                    .count()
                    + 1;
                let excerpt = row.source[span.start..span.end]
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("L{line} bytes {}..{}: {excerpt}", span.start, span.end)
            })
            .unwrap_or_else(|| "none".to_owned());
        println!(
            "{} | {statements} {scopes} {actions} {author} {max} {overhead} | {ratio} | {worst}",
            row.path
        );
    }
    for row in &measured {
        assert!(
            row.counts[3] == 0 || row.counts[4] > 0,
            "{}: no kernel statement is attributed to an author statement; preserve importer source notes before ratcheting",
            row.path
        );
    }
    println!("\nRatchet rows (statements scopes actions author max overhead):");
    let actual = measured
        .iter()
        .map(|row| {
            format!(
                "{} {}",
                row.path,
                row.counts
                    .iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        })
        .collect::<Vec<_>>();
    for row in &actual {
        println!("{row}");
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("catalog/shape.ratchet");
    let baseline = std::fs::read_to_string(root).expect("committed shape ratchet");
    let expected = baseline
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .collect::<Vec<_>>();
    // Exact equality enforces both directions: increases require investigation;
    // decreases must lower the committed ceiling in the improving change.
    assert_eq!(
        actual, expected,
        "SHAPE changed: investigate count increases; lower shape.ratchet for decreases. Keep one sorted row per workflow, including probes; use the printed rows."
    );
}
