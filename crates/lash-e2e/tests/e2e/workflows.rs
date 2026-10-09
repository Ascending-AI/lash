//! S38: one workflow through the whole workflow layer on a real host.
//!
//! The host is `examples/e2e-consumer` in its workflow mode: it is built on
//! `lash::workflow` and the facade, and holds no source lens. The case hands
//! it a generated workflow as a typed document, reads the published
//! definition back through the facade, edits inside the `try` region and
//! inside the inner loop with typed edits, publishes, and runs the old and
//! the new definition side by side. Both runs park inside the inner loop;
//! the node is killed and another takes them over. A follower that attaches
//! there has missed the start of the execution, and what it folds is checked
//! against a count of the loops made from the order the runs were given.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context as _, Result, bail, ensure};
use lash::vm::ir::{
    Expr, ExprSlot, ExprSlotVisitor, ProcessOrigin, WorkflowContainer, WorkflowDeclaration,
    WorkflowNode, WorkflowNodeKind, WorkflowSlotPath, WorkflowSubgraph, walk_expr_slots,
};
use lash::workflow::{WorkflowGraph, workflow_node_statement};
use lash_e2e::{Case, Host, NodeOptions};
use serde_json::{Value, json};

/// The checked-in workflow, as a model wrote it.
const FIXTURE: &str = "crates/lash-e2e/tests/e2e/workflows/order_review.ts";
/// The name the fixture binds its workflow to.
const WORKFLOW: &str = "order_review";

/// The order both runs are given: three groups of two lines.
fn order() -> Value {
    json!({"groups": [
        {"name": "a", "lines": [{"sku": "a1", "qty": 5}, {"sku": "a2", "qty": 1}]},
        {"name": "b", "lines": [{"sku": "b1", "qty": 3}, {"sku": "b2", "qty": 4}]},
        {"name": "c", "lines": [{"sku": "c1", "qty": 1}, {"sku": "c2", "qty": 6}]},
    ]})
}

/// One line of the order as a run meets it: the outer and inner body
/// iteration (from 1), and whether a definition reviewing above
/// `threshold` reviews it.
struct Line {
    outer: u64,
    inner: u64,
    item: String,
    sku: String,
    qty: u64,
}

fn lines() -> Vec<Line> {
    let order = order();
    let mut lines = Vec::new();
    for (outer, group) in order["groups"].as_array().into_iter().flatten().enumerate() {
        for (inner, line) in group["lines"].as_array().into_iter().flatten().enumerate() {
            let (name, sku) = (
                group["name"].as_str().unwrap_or_default(),
                line["sku"].as_str().unwrap_or_default(),
            );
            lines.push(Line {
                outer: outer as u64 + 1,
                inner: inner as u64 + 1,
                item: format!("{name}/{sku}"),
                sku: sku.to_owned(),
                qty: line["qty"].as_u64().unwrap_or_default(),
            });
        }
    }
    lines
}

async fn boot(case: &mut Case, node: &str) -> Result<()> {
    let options = NodeOptions {
        env: vec![
            ("E2E_CONSUMER_SCENARIO".to_owned(), "S38".to_owned()),
            (
                "E2E_CONSUMER_WORKFLOW_BODIES".to_owned(),
                case.dir.join("bodies.jsonl").display().to_string(),
            ),
        ],
        ..NodeOptions::default()
    };
    case.boot(Host::Consumer, node, options).await?;
    Ok(())
}

fn graph(value: &Value) -> Result<WorkflowGraph> {
    serde_json::from_value(value.clone()).context("the host's document is a typed workflow graph")
}

/// The path to the first expression of `statement` that `wanted` accepts.
fn slot_where(statement: &Expr, wanted: impl Fn(&Expr) -> bool) -> Option<Vec<ExprSlot>> {
    struct Find<F>(F, Option<Vec<ExprSlot>>);
    impl<F: Fn(&Expr) -> bool> ExprSlotVisitor for Find<F> {
        fn visit_slot(&mut self, path: &[ExprSlot], expr: &Expr) {
            if self.1.is_none() && (self.0)(expr) {
                self.1 = Some(path.to_vec());
            }
        }
    }
    let mut find = Find(wanted, None);
    walk_expr_slots(&mut find, statement);
    find.1
}

fn is_string(expr: &Expr, text: &str) -> bool {
    matches!(expr, Expr::String(value) if value.as_str() == text)
}

fn statement(node: &WorkflowNode) -> Expr {
    workflow_node_statement(node)
}

/// `statement` with the expression at `path` replaced.
fn replaced(mut statement: Expr, path: &[ExprSlot], with: Expr) -> Result<Expr> {
    let mut at = &mut statement;
    for slot in path {
        at = at
            .slot_mut(*slot)
            .context("the slot path reaches an expression")?;
    }
    *at = with;
    Ok(statement)
}

fn calls(node: &WorkflowNode, operation: &str) -> bool {
    matches!(&node.kind, WorkflowNodeKind::Call { operation: called, .. } if called == operation)
}

/// Every node of `body` and of the bodies under it, in document order.
fn all_nodes<'g>(body: &'g WorkflowSubgraph, nodes: &mut Vec<&'g WorkflowNode>) {
    for node in body.nodes() {
        nodes.push(node);
        if let WorkflowNodeKind::Container(container) = &node.kind {
            for (_, child) in container.child_subgraphs() {
                all_nodes(child, nodes);
            }
        }
    }
}

/// The nodes of the entry process the case edits, runs and reads sites of.
struct Shape<'g> {
    region: &'g WorkflowNode,
    try_body: &'g WorkflowSubgraph,
    catch_record: &'g WorkflowNode,
    outer: &'g WorkflowNode,
    inner: &'g WorkflowNode,
    inner_body: &'g WorkflowSubgraph,
    /// The `if` that decides whether a line is reviewed.
    threshold: &'g WorkflowNode,
    /// The `review.request` call a run parks on.
    review: &'g WorkflowNode,
    /// The `if` that reads the decision after the park.
    decision: &'g WorkflowNode,
    /// The `ledger.record` call that ends the inner loop body.
    record: &'g WorkflowNode,
    /// The assignment that ends the `try` body in the generated workflow.
    status: &'g WorkflowNode,
    /// The statement that binds the inline process.
    inline: &'g WorkflowNode,
    /// The `processes.start` of the inline process.
    start: &'g WorkflowNode,
}

/// The process the document's main body binds as the workflow: its name
/// and the id of its container. A generated workflow's processes are lifted
/// literals named by digest, so it is told from the inline process inside
/// it by where its literal sits: the workflow's is the outermost.
fn workflow_process(graph: &WorkflowGraph) -> Result<(String, String)> {
    ensure!(
        graph.main.nodes().into_iter().any(|node| matches!(
            &node.kind,
            WorkflowNodeKind::Data {
                binding: Some(binding),
                expression: Expr::ProcessRef { .. } | Expr::ProcessLiteral(_),
            } if binding.root.as_str() == WORKFLOW
        )),
        "the document's main body binds a process as `{WORKFLOW}`"
    );
    graph
        .declarations
        .iter()
        .filter_map(|declaration| match declaration {
            WorkflowDeclaration::Process(process) => match &process.origin {
                ProcessOrigin::Lifted { site, .. } => Some((site.steps.len(), process)),
                ProcessOrigin::Declared => None,
            },
            WorkflowDeclaration::Function(_) => None,
        })
        .min_by_key(|(depth, _)| *depth)
        .map(|(_, process)| (process.name.clone(), process.id.to_string()))
        .context("the document lifts the workflow's process")
}

/// The first `for` of `body`, with its own body.
fn loop_in(body: &WorkflowSubgraph) -> Option<(&WorkflowNode, &WorkflowSubgraph)> {
    body.nodes().into_iter().find_map(|node| match &node.kind {
        WorkflowNodeKind::Container(WorkflowContainer::For { body, .. }) => {
            Some((node, body.as_ref()))
        }
        _ => None,
    })
}

fn shape<'g>(graph: &'g WorkflowGraph, entry: &str) -> Result<Shape<'g>> {
    let process = graph.process(entry).context("the document has the entry")?;
    let top = process.body.nodes();
    top.iter()
        .find(|node| {
            slot_where(&statement(node), |expr| matches!(expr, Expr::Function(_))).is_some()
        })
        .context("the closure is a typed function value in a statement")?;
    let region = top
        .iter()
        .find(|node| {
            matches!(
                node.kind,
                WorkflowNodeKind::Container(WorkflowContainer::Try { .. })
            )
        })
        .context("the try region is a container")?;
    let WorkflowNodeKind::Container(WorkflowContainer::Try {
        body: try_body,
        catch: Some(catch),
        ..
    }) = &region.kind
    else {
        bail!("the try region has a catch clause");
    };
    let catch_record = catch
        .body
        .nodes()
        .into_iter()
        .find(|node| calls(node, "record"))
        .context("the catch body records")?;
    let (outer, outer_body) = loop_in(try_body).context("the outer loop is in the try body")?;
    let (inner, inner_body) = loop_in(outer_body).context("the inner loop is in the outer")?;
    let (threshold, then_graph) = inner_body
        .nodes()
        .into_iter()
        .find_map(|node| match &node.kind {
            WorkflowNodeKind::Container(WorkflowContainer::If { then_graph, .. }) => {
                Some((node, then_graph.as_ref()))
            }
            _ => None,
        })
        .context("the inner loop branches")?;
    let review = then_graph
        .nodes()
        .into_iter()
        .find(|node| calls(node, "request"))
        .context("the branch asks for a review")?;
    let decision = then_graph
        .nodes()
        .into_iter()
        .find(|node| {
            matches!(
                node.kind,
                WorkflowNodeKind::Container(WorkflowContainer::If { .. })
            )
        })
        .context("the branch reads the decision")?;
    let record = inner_body
        .nodes()
        .into_iter()
        .find(|node| calls(node, "record"))
        .context("the inner loop records")?;
    let status = try_body
        .nodes()
        .into_iter()
        .find(|node| matches!(node.kind, WorkflowNodeKind::StateUpdate(_)))
        .context("the try body ends by setting the status")?;
    let inline = top
        .iter()
        .find(|node| {
            slot_where(&statement(node), |expr| {
                matches!(expr, Expr::ProcessRef { .. })
            })
            .is_some()
        })
        .context("a statement holds the inline process")?;
    let start = top
        .iter()
        .find(|node| calls(node, "start"))
        .context("the workflow starts its inline process")?;
    Ok(Shape {
        region,
        try_body,
        catch_record,
        outer,
        inner,
        inner_body,
        threshold,
        review,
        decision,
        record,
        status,
        inline,
        start,
    })
}

/// Start a run of `definition` over the case's order under start key `key`.
async fn start(case: &mut Case, node: &str, definition: &Value, key: &str) -> Result<String> {
    let started = case
        .node(node)?
        .post(
            "/workflow/runs",
            &json!({"definition": definition["id"], "key": key, "args": {"order": order()}}),
        )
        .await?;
    case.record_barrier(
        json!({"barrier": "started", "node": node, "run": key, "receipt": started}),
    );
    started["process"]
        .as_str()
        .map(ToOwned::to_owned)
        .context("the start answered no process")
}

/// The review deliveries of `process`, in order.
fn reviews(case: &Case, process: &str) -> Result<Vec<Value>> {
    Ok(case
        .bodies()?
        .into_iter()
        .filter(|line| line["tool"] == "review_request" && line["process"] == process)
        .collect())
}

/// Wait until `process` is parked on its `nth` review (from 1): the body
/// entered, and the process's row waits on that call. Answers the delivery.
async fn parked(case: &Case, node: &str, process: &str, nth: usize) -> Result<Value> {
    let host = case.node(node)?;
    let delivery = case
        .until(&format!("{process} parked on review {nth}"), || async {
            let Some(delivery) = reviews(case, process)?.into_iter().nth(nth - 1) else {
                return Ok(None);
            };
            let run = host.get(&format!("/workflow/runs/{process}")).await?;
            let wait = json!({"kind": "call", "call_id": delivery["call_id"], "tool_id": "review_request"});
            let waits = run["waits"].as_array().cloned().unwrap_or_default();
            Ok((run["status"] == "Waiting" && waits == [wait]).then_some(delivery))
        })
        .await?;
    case.record_barrier(json!({"barrier": "parked", "node": node, "process": process, "review": nth, "delivery": delivery}));
    Ok(delivery)
}

/// Resolve the review `delivery` parked on as approved, through `node`.
async fn approve(case: &mut Case, node: &str, delivery: &Value) -> Result<()> {
    let key = delivery["completion"]
        .as_str()
        .context("a deferred body records its completion key")?;
    let answer = case
        .node(node)?
        .post(
            "/completions",
            &json!({"key": key, "value": {"approved": true}}),
        )
        .await?;
    case.evidence
        .effects
        .push(json!({"resolve": delivery["call_id"], "node": node, "answer": answer}));
    ensure!(
        answer["answer"] == "Resolved",
        "the review was already resolved: {answer}"
    );
    Ok(())
}

/// Whether two publications name different admitted modules.
fn new_graph_identity(edited: &Value, generated: &Value) -> bool {
    let identity = |publication: &Value| publication["graph"]["source_identity"].clone();
    identity(edited).is_string() && identity(edited) != identity(generated)
}

/// Every string anywhere in `value`.
fn strings<'v>(value: &'v Value, found: &mut Vec<&'v str>) {
    match value {
        Value::String(text) => found.push(text),
        Value::Array(items) => items.iter().for_each(|item| strings(item, found)),
        Value::Object(fields) => fields.values().for_each(|field| strings(field, found)),
        _ => {}
    }
}

/// The document carries no source: none of its strings is a line of the
/// generated text or holds its syntax, and no node is of an opaque kind or
/// has a field named for source text.
fn carries_no_source(document: &Value, source: &str) -> Result<()> {
    let mut found = Vec::new();
    strings(document, &mut found);
    let authored: BTreeSet<&str> = source
        .lines()
        .map(str::trim)
        .filter(|line| line.len() > 12)
        .collect();
    for text in found {
        ensure!(
            !authored.contains(text.trim()),
            "the document carries the source line `{text}`"
        );
        for syntax in ["=>", "await ", "const ", "${", "catch (", "for ("] {
            ensure!(
                !text.contains(syntax),
                "the document carries source text `{text}`"
            );
        }
    }
    fn fields(value: &Value, path: &str) -> Result<()> {
        match value {
            Value::Object(object) => {
                for (name, field) in object {
                    ensure!(
                        !matches!(name.as_str(), "source" | "source_text" | "text" | "code"),
                        "the document has a source field at {path}.{name}"
                    );
                    ensure!(
                        !(matches!(name.as_str(), "kind" | "container_kind")
                            && matches!(field.as_str(), Some("opaque" | "code" | "source"))),
                        "the document has an opaque node at {path}"
                    );
                    fields(field, &format!("{path}.{name}"))?;
                }
            }
            Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    fields(item, &format!("{path}[{index}]"))?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    fields(document, "graph")
}

/// What a correspondence says became of each node.
struct Correspondence {
    /// Where each surviving node of the base ended, by its base id.
    kept: BTreeMap<String, String>,
    /// Each node only the new document has: its id and where it came from.
    inserted: Vec<(String, Value)>,
}

fn correspondence(value: &Value) -> Result<Correspondence> {
    let mut read = Correspondence {
        kept: BTreeMap::new(),
        inserted: Vec::new(),
    };
    for entry in value["entries"].as_array().into_iter().flatten() {
        let id = |field: &str| entry[field].as_str().unwrap_or_default().to_owned();
        match entry["outcome"].as_str() {
            Some("retained") => {
                ensure!(
                    read.kept.insert(id("from"), id("to")).is_none(),
                    "a node has two outcomes: {entry}"
                );
            }
            Some("inserted") => read.inserted.push((id("to"), entry["source"].clone())),
            _ => bail!("no edit moved, removed, split, merged or lost a node: {entry}"),
        }
    }
    Ok(read)
}

fn ids(nodes: &[&WorkflowNode]) -> BTreeSet<String> {
    nodes.iter().map(|node| node.id.to_string()).collect()
}

fn entry_nodes<'g>(graph: &'g WorkflowGraph, entry: &str) -> Result<Vec<&'g WorkflowNode>> {
    let mut nodes = Vec::new();
    all_nodes(
        &graph.process(entry).context("the entry process")?.body,
        &mut nodes,
    );
    Ok(nodes)
}

fn node<'g>(graph: &'g WorkflowGraph, entry: &str, id: &str) -> Result<&'g WorkflowNode> {
    entry_nodes(graph, entry)?
        .into_iter()
        .find(|node| node.id.as_str() == id)
        .with_context(|| format!("the document has node {id}"))
}

/// The one execution site of `node` whose label names `label`, as the
/// overlay spells a site.
fn site(node: &WorkflowNode, label: &str) -> Result<Value> {
    let sites: Vec<_> = node
        .execution_sites
        .iter()
        .filter(|site| site.label.contains(label))
        .collect();
    let [site] = sites.as_slice() else {
        bail!(
            "node {} has one site labelled {label}: {:?}",
            node.id,
            node.execution_sites
        );
    };
    let mut reference = json!({"node_id": node.id});
    if !site.site_path.is_empty() {
        reference["site_path"] = serde_json::to_value(&site.site_path)?;
    }
    Ok(reference)
}

/// The frames a site inside the inner loop body runs under on `line`.
fn frames(observed: &Value) -> Vec<(Value, u64, Value)> {
    observed["loops"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|frame| {
            (
                frame["site"].clone(),
                frame["activation"].as_u64().unwrap_or_default(),
                frame["position"].clone(),
            )
        })
        .collect()
}

/// What one site of the document must show to a follower that attached
/// while its run was parked.
struct Expected {
    name: &'static str,
    site: Value,
    /// How many times the site runs over the whole order.
    total: u64,
    /// The first occurrence that ends after the follower attached.
    first_seen: u64,
    /// Whether the follower sees the start of `first_seen` too.
    start_seen: bool,
    /// Whether an occurrence reports a start at all (a branch does not).
    starts: bool,
    /// The line an occurrence runs on, or `None` outside the inner loop.
    line: Box<dyn Fn(u64) -> Option<usize>>,
    /// The arm a branch takes at an occurrence.
    arm: Option<Box<dyn Fn(u64) -> &'static str>>,
}

/// The run a late follower followed.
struct Followed<'a> {
    /// The run reviews lines above this quantity.
    threshold: u64,
    /// The review (from 1) the run was parked on when the follower attached.
    parked: usize,
    /// The delivery of the run's last review.
    last_review: &'a Value,
    /// The document reference the run's row names.
    reference: &'a Value,
}

/// What the follower of `run` folded by the end, against the loops of the
/// order.
fn check_followed(
    observed: &Value,
    shape: &Shape<'_>,
    clone: Option<&WorkflowNode>,
    document_sites: &BTreeSet<String>,
    run: &Followed<'_>,
) -> Result<()> {
    let Followed {
        threshold,
        parked,
        last_review,
        reference,
    } = *run;
    let overlay = &observed["overlay"];
    ensure!(
        overlay["coverage"] == json!({"start_observed": false}),
        "the overlay says it missed the start: {}",
        overlay["coverage"]
    );
    ensure!(
        overlay["document"] == json!({"state": "loaded", "reference": reference}),
        "the overlay was held to the document the run names: {}",
        overlay["document"]
    );
    ensure!(
        overlay["status"] == "completed"
            && overlay["settlement"]["terminal"] == "completed"
            && overlay["mismatches"] == json!([])
            && overlay["conflicts"] == json!([]),
        "the overlay settled with the run and refused nothing: {} {} {}",
        overlay["settlement"],
        overlay["mismatches"],
        overlay["conflicts"]
    );
    let items = observed["items"].as_array().cloned().unwrap_or_default();
    ensure!(
        !items
            .iter()
            .any(|item| item["execution"]["kind"] == "execution_started"),
        "the follower never saw the execution start"
    );

    let lines = lines();
    let reviewed: Vec<usize> = (0..lines.len())
        .filter(|line| lines[*line].qty > threshold)
        .collect();
    let at = reviewed[parked - 1];
    let count = lines.len() as u64;
    let on_line = |from: u64| -> Box<dyn Fn(u64) -> Option<usize>> {
        Box::new(move |occurrence| Some((occurrence - from) as usize))
    };
    let reviewed_line = {
        let reviewed = reviewed.clone();
        move || -> Box<dyn Fn(u64) -> Option<usize>> {
            let reviewed = reviewed.clone();
            Box::new(move |occurrence| reviewed.get(occurrence as usize - 1).copied())
        }
    };
    let qty: Vec<u64> = lines.iter().map(|line| line.qty).collect();
    let mut expected = vec![
        Expected {
            name: "outer loop",
            site: site(shape.outer, "for")?,
            total: lines.last().map_or(0, |line| line.outer),
            first_seen: lines[at].outer + 1,
            start_seen: true,
            starts: true,
            line: Box::new(|_| None),
            arm: None,
        },
        Expected {
            name: "inner loop",
            site: site(shape.inner, "for")?,
            total: count,
            first_seen: at as u64 + 2,
            start_seen: true,
            starts: true,
            line: on_line(1),
            arm: None,
        },
        Expected {
            name: "threshold branch",
            site: site(shape.threshold, "if")?,
            total: count,
            first_seen: at as u64 + 2,
            start_seen: false,
            starts: false,
            line: on_line(1),
            arm: Some(Box::new(move |occurrence| {
                if qty[occurrence as usize - 1] > threshold {
                    "then"
                } else {
                    "else"
                }
            })),
        },
        Expected {
            name: "review",
            site: site(shape.review, "request")?,
            total: reviewed.len() as u64,
            first_seen: parked as u64,
            start_seen: false,
            starts: true,
            line: reviewed_line(),
            arm: None,
        },
        Expected {
            name: "decision branch",
            site: site(shape.decision, "if")?,
            total: reviewed.len() as u64,
            first_seen: parked as u64,
            start_seen: false,
            starts: false,
            line: reviewed_line(),
            arm: Some(Box::new(|_| "else")),
        },
        Expected {
            name: "record",
            site: site(shape.record, "record")?,
            total: count,
            first_seen: at as u64 + 1,
            start_seen: true,
            starts: true,
            line: on_line(1),
            arm: None,
        },
        Expected {
            name: "start of the inline process",
            site: site(shape.start, "start")?,
            total: 1,
            first_seen: 1,
            start_seen: true,
            starts: true,
            line: Box::new(|_| None),
            arm: None,
        },
    ];
    if let Some(clone) = clone {
        expected.push(Expected {
            name: "cloned record",
            site: site(clone, "record")?,
            total: count,
            first_seen: at as u64 + 1,
            start_seen: true,
            starts: true,
            line: on_line(1),
            arm: None,
        });
    }

    // The overlay lists only sites of the document, and never one of the
    // catch clause, which no run entered.
    let listed = overlay["sites"].as_array().cloned().unwrap_or_default();
    for entry in &listed {
        ensure!(
            document_sites.contains(&entry["site"].to_string()),
            "the overlay lists a site the document lacks: {}",
            entry["site"]
        );
    }
    let untouched = site(shape.catch_record, "record")?;
    ensure!(
        !listed.iter().any(|entry| entry["site"] == untouched
            || entry["site"]["node_id"] == json!(shape.catch_record.id)),
        "the overlay invents no execution of the catch clause"
    );

    let (outer_site, inner_site) = (site(shape.outer, "for")?, site(shape.inner, "for")?);
    let mut outer_activations = BTreeSet::new();
    let mut inner_activations = BTreeMap::<u64, BTreeSet<u64>>::new();
    for site in &expected {
        // Per-site occurrences: the latest is the last the order causes,
        // and the counts are the ones that ended, and started, after the
        // follower attached.
        let entry = listed
            .iter()
            .find(|entry| entry["site"] == site.site)
            .with_context(|| format!("the overlay lists the {} site", site.name))?;
        let seen = site.total + 1 - site.first_seen;
        let started = match (site.starts, site.start_seen) {
            (false, _) => 0,
            (true, true) => seen,
            (true, false) => seen - 1,
        };
        ensure!(
            entry["status"] == "completed"
                && entry["occurrence"] == site.total
                && entry["summary"]["terminal_count"] == seen
                && entry["summary"]["started_count"] == started,
            "the {} site ends at occurrence {} with {seen} ended and {started} started: {entry}",
            site.name,
            site.total
        );
        ensure!(
            entry["branch"]
                == site
                    .arm
                    .as_ref()
                    .map_or(Value::Null, |arm| json!(arm(site.total))),
            "the {} site shows the arm its last occurrence took: {entry}",
            site.name
        );

        // Every observation of the site: its occurrence is one the
        // follower could see, on the loop iterations of the line it ran on.
        let mut arms = BTreeMap::new();
        for item in &items {
            let fact = match item["item"].as_str() {
                Some("language") => &item["execution"],
                Some("step_body_started") => &item["step"],
                _ => continue,
            };
            let mut at_site = json!({"node_id": fact["node_id"]});
            if let Some(path) = fact["context"].get("site_path") {
                at_site["site_path"] = path.clone();
            }
            if at_site != site.site {
                continue;
            }
            let occurrence = fact["occurrence"].as_u64().context("an occurrence")?;
            ensure!(
                (site.first_seen..=site.total).contains(&occurrence),
                "the {} site reports occurrence {occurrence} after the attach: {fact}",
                site.name
            );
            let loops = frames(&fact["context"]);
            match (site.line)(occurrence) {
                Some(line) => {
                    let line = &lines[line];
                    let [
                        (outer, outer_activation, outer_at),
                        (inner, inner_activation, inner_at),
                    ] = loops.as_slice()
                    else {
                        bail!("the {} site runs inside both loops: {fact}", site.name);
                    };
                    ensure!(
                        *outer == outer_site
                            && *inner == inner_site
                            && *outer_at == json!({"body": line.outer})
                            && *inner_at == json!({"body": line.inner}),
                        "occurrence {occurrence} of the {} site runs on outer iteration {} and inner iteration {}: {fact}",
                        site.name,
                        line.outer,
                        line.inner
                    );
                    outer_activations.insert(*outer_activation);
                    inner_activations
                        .entry(line.outer)
                        .or_default()
                        .insert(*inner_activation);
                }
                None if site.site == outer_site => {
                    let [(outer, activation, position)] = loops.as_slice() else {
                        bail!("the outer loop runs inside itself alone: {fact}");
                    };
                    ensure!(
                        *outer == outer_site && *position == json!({"body": occurrence}),
                        "occurrence {occurrence} of the outer loop is its body iteration: {fact}"
                    );
                    outer_activations.insert(*activation);
                }
                None => ensure!(
                    loops.is_empty(),
                    "the {} site runs outside every loop: {fact}",
                    site.name
                ),
            }
            if fact["kind"] == "branch_selected" {
                arms.insert(occurrence, fact["selected"].clone());
            }
        }
        // Branch choices: each occurrence the follower could see took the
        // arm the line's quantity decides.
        if let Some(arm) = &site.arm {
            let chosen: BTreeMap<u64, Value> = (site.first_seen..=site.total)
                .map(|occurrence| (occurrence, json!(arm(occurrence))))
                .collect();
            ensure!(
                arms == chosen,
                "the {} site took {chosen:?}: {arms:?}",
                site.name
            );
        }
    }
    // One entry of the outer loop; one entry of the inner loop per outer
    // iteration, each its own activation, in order.
    let inner: Vec<u64> = inner_activations
        .values()
        .flat_map(|activations| activations.iter().copied())
        .collect();
    ensure!(
        outer_activations.len() == 1
            && inner_activations.values().all(|entered| entered.len() == 1)
            && inner.windows(2).all(|pair| pair[0] < pair[1]),
        "the loops are entered once per iteration around them: {outer_activations:?} {inner_activations:?}"
    );
    // The overlay binds the last review to the call its body was admitted as.
    let review = listed
        .iter()
        .find(|entry| entry["site"] == expected[3].site)
        .context("the review site")?;
    ensure!(
        review["call"]["call_id"] == last_review["call_id"]
            && review["call"]["occurrence"] == expected[3].total,
        "the review site is bound to the call that ran: {review}"
    );
    Ok(())
}

/// Every execution site of `body` and the bodies under it, as the overlay
/// spells a site.
fn sites_of(body: &WorkflowSubgraph) -> Result<BTreeSet<String>> {
    let mut nodes = Vec::new();
    all_nodes(body, &mut nodes);
    let mut sites = BTreeSet::new();
    for node in nodes {
        for site in &node.execution_sites {
            let mut reference = json!({"node_id": node.id});
            if !site.site_path.is_empty() {
                reference["site_path"] = serde_json::to_value(&site.site_path)?;
            }
            sites.insert(reference.to_string());
        }
    }
    Ok(sites)
}

case!(
    s38_generated_workflow_is_edited_published_run_and_shown_across_a_handover_resume,
    SqliteFile,
    Resume,
    s38
);
case!(
    s38_generated_workflow_is_edited_published_run_and_shown_across_a_handover_postgresql_resume,
    Postgresql,
    Resume,
    s38
);

async fn s38(case: &mut Case) -> Result<()> {
    let repo = std::env::var("LASH_E2E_REPO").context("LASH_E2E_REPO is required")?;
    let source = std::fs::read_to_string(std::path::Path::new(&repo).join(FIXTURE))?;
    boot(case, "node-a").await?;

    // (a) The generated workflow reaches the host as a typed document. The
    // lens that lowers the model's text runs here, outside the host.
    let generated = lash::typescript::workflow_graph::workflow_graph_from_source(&source)
        .context("the generated workflow lowers to a document")?;
    let host = case.node("node-a")?;
    let opened = host
        .post("/workflow/draft", &json!({"graph": generated}))
        .await?;
    let (_, entry) = workflow_process(&graph(&opened["graph"])?)?;
    let first = host
        .post("/workflow/draft/publish", &json!({"entry": entry}))
        .await?;
    ensure!(
        first["published"] == true,
        "the generated workflow is admitted: {first}"
    );
    let old_entry = first["entry"]
        .as_str()
        .context("the publication names its entry")?
        .to_owned();
    ensure!(
        workflow_process(&graph(&first["graph"])?)?.0 == old_entry,
        "the definition starts the process the document binds as the workflow"
    );
    case.write("published-generated.json", &first)?;

    // (b) The published definition, read back through the facade.
    let old = first["definition"].clone();
    let read = host
        .post("/workflow/definition", &json!({"definition": old["id"]}))
        .await?;
    ensure!(
        read["read"] == "inspected"
            && read["definition"] == old
            && read["entry"] == old_entry.as_str(),
        "the facade reads the definition that was published: {read}"
    );
    ensure!(
        read["graph"] == first["graph"],
        "the definition reads as the document its publication answered"
    );
    carries_no_source(&read["graph"], &source)?;
    let old_graph = graph(&read["graph"])?;
    let old_shape = shape(&old_graph, &old_entry)?;
    let lifted: Vec<_> = old_graph
        .declarations
        .iter()
        .filter_map(|declaration| match declaration {
            WorkflowDeclaration::Process(process) if process.name != old_entry => Some(process),
            _ => None,
        })
        .collect();
    ensure!(
        lifted.len() == 1
            && !lifted[0].origin.is_declared()
            && lifted[0]
                .body
                .nodes()
                .iter()
                .any(|node| calls(node, "record")),
        "the inline process is lifted into a declaration with a typed body"
    );
    // The inline process is a typed reference a statement binds, and the
    // start reads that binding.
    let WorkflowNodeKind::Data {
        binding: Some(audit),
        ..
    } = &old_shape.inline.kind
    else {
        bail!("a statement binds the inline process");
    };
    ensure!(
        slot_where(&statement(old_shape.inline), |expr| matches!(
            expr,
            Expr::ProcessRef { process } if process.as_str() == lifted[0].name
        ))
        .is_some()
            && slot_where(&statement(old_shape.start), |expr| matches!(
                expr,
                Expr::Variable(name) if name.as_str() == audit.root.as_str()
            ))
            .is_some(),
        "the start names the lifted process through its binding"
    );

    // (f, first half) A run of the generated definition, parked inside the
    // inner loop before anything is edited.
    let old_run = start(case, "node-a", &old, "generated").await?;
    let old_first = parked(case, "node-a", &old_run, 1).await?;
    ensure!(
        old_first["args"]["item"] == "a/a1",
        "the generated run parks on its first reviewed line: {old_first}"
    );

    // (c) Typed edits of the admitted document, inside the try region and
    // inside the inner loop, as one transaction.
    let host = case.node("node-a")?;
    let opened = host
        .post("/workflow/draft", &json!({"definition": old["id"]}))
        .await?;
    let draft_graph = graph(&opened["graph"])?;
    let (draft_entry, draft_entry_id) = workflow_process(&draft_graph)?;
    let draft = shape(&draft_graph, &draft_entry)?;
    let status = statement(draft.status);
    let status_slot =
        slot_where(&status, |expr| is_string(expr, "reviewed")).context("the status literal")?;
    let threshold = statement(draft.threshold);
    let threshold_slot = slot_where(
        &threshold,
        |expr| matches!(expr, Expr::Number(two) if *two == 2.0),
    )
    .context("the threshold literal")?;
    let rejected = statement(draft.catch_record);
    let rejected_slot = slot_where(&rejected, |expr| is_string(expr, "rejected"))
        .context("the recorded literal")?;
    let signed_off = Expr::String("signed-off".into());
    let edits = json!({"edits": [
        // In the try region: one more recorded entry, authored as IR.
        {
            "op": "insert_node",
            "body": {"node": draft.region.id, "slot": "try_body"},
            "statement": replaced(rejected, &rejected_slot, signed_off.clone())?,
        },
        // In the try region: the status a completed review ends with.
        {
            "op": "replace_expression",
            "node": draft.status.id,
            "slot": WorkflowSlotPath::new(status_slot),
            "expression": signed_off,
        },
        // In the inner loop: only larger lines are reviewed.
        {
            "op": "replace_expression",
            "node": draft.threshold.id,
            "slot": WorkflowSlotPath::new(threshold_slot),
            "expression": Expr::Number(3.0),
        },
        // In the inner loop: every line is recorded twice.
        {
            "op": "clone_node",
            "node": draft.record.id,
            "body": {"node": draft.inner.id, "slot": "loop_body"},
        },
    ]});
    case.write("edits.json", &edits)?;
    let edited = host.post("/workflow/draft/edits", &edits).await?;
    ensure!(edited["applied"] == true, "the edits apply: {edited}");
    let second = host
        .post("/workflow/draft/publish", &json!({"entry": draft_entry_id}))
        .await?;
    case.write("published-edited.json", &second)?;
    ensure!(
        second["published"] == true,
        "the edited workflow is admitted: {second}"
    );
    let new = second["definition"].clone();
    ensure!(
        new["id"] != old["id"] && new_graph_identity(&second, &first),
        "an edit publishes a new definition"
    );
    carries_no_source(&second["graph"], &source)?;
    let new_graph = graph(&second["graph"])?;
    let new_entry = second["entry"]
        .as_str()
        .context("the publication names its entry")?
        .to_owned();
    ensure!(
        workflow_process(&new_graph)?.0 == new_entry,
        "the edited definition starts the workflow, too"
    );
    let new_shape = shape(&new_graph, &new_entry)?;

    // Correspondence: every node of the generated definition's document is
    // retained and ends at a node of the edited one; the edited nodes end
    // at the nodes that hold the edits; the two new nodes say where they
    // came from.
    let moved = correspondence(&second["correspondence"])?;
    let (old_nodes, new_nodes) = (
        entry_nodes(&old_graph, &old_entry)?,
        entry_nodes(&new_graph, &new_entry)?,
    );
    for id in ids(&old_nodes) {
        let to = moved
            .kept
            .get(&id)
            .with_context(|| format!("node {id} of the generated document has an outcome"))?;
        node(&new_graph, &new_entry, to)?;
    }
    for (was, now) in [
        (old_shape.region, new_shape.region),
        (old_shape.outer, new_shape.outer),
        (old_shape.inner, new_shape.inner),
        (old_shape.threshold, new_shape.threshold),
        (old_shape.review, new_shape.review),
        (old_shape.decision, new_shape.decision),
        (old_shape.record, new_shape.record),
        (old_shape.status, new_shape.status),
        (old_shape.start, new_shape.start),
    ] {
        ensure!(
            moved.kept.get(was.id.as_str()).map(String::as_str) == Some(now.id.as_str()),
            "node {} corresponds to {}: {:?}",
            was.id,
            now.id,
            moved.kept.get(was.id.as_str())
        );
    }
    ensure!(
        slot_where(&statement(new_shape.status), |expr| is_string(
            expr,
            "signed-off"
        ))
        .is_some()
            && slot_where(&statement(new_shape.threshold), |expr| {
                matches!(expr, Expr::Number(three) if *three == 3.0)
            })
            .is_some(),
        "the corresponding nodes hold the edited expressions"
    );
    let authored = new_shape
        .try_body
        .nodes()
        .last()
        .copied()
        .context("the edited try body")?;
    let cloned = new_shape
        .inner_body
        .nodes()
        .last()
        .copied()
        .context("the edited inner loop body")?;
    ensure!(
        slot_where(&statement(authored), |expr| is_string(expr, "signed-off")).is_some()
            && calls(authored, "record")
            && calls(cloned, "record")
            && cloned.id != new_shape.record.id,
        "the inserted and the cloned statement are where the edits put them"
    );
    let inserted: BTreeMap<&str, &Value> = moved
        .inserted
        .iter()
        .map(|(id, source)| (id.as_str(), source))
        .collect();
    ensure!(
        inserted.len() == 2
            && inserted
                .get(authored.id.as_str())
                .map(|source| &source["kind"])
                == Some(&json!("authored"))
            && inserted.get(cloned.id.as_str()).copied()
                == Some(&json!({"kind": "clone", "of": draft.record.id})),
        "the new nodes are the authored statement and the clone of the record: {:?}",
        moved.inserted
    );
    ensure!(
        ids(&new_nodes).len() == ids(&old_nodes).len() + 2,
        "the edited document has exactly the two new nodes"
    );

    // (d) A run of the edited definition: it parks inside the inner loop,
    // is resolved once, and parks there again on a later iteration.
    let new_run = start(case, "node-a", &new, "edited").await?;
    let new_first = parked(case, "node-a", &new_run, 1).await?;
    approve(case, "node-a", &new_first).await?;
    let new_second = parked(case, "node-a", &new_run, 2).await?;
    ensure!(
        new_first["args"]["item"] == "a/a1" && new_second["args"]["item"] == "b/b2",
        "the edited run skips the line the edit excluded: {new_second}"
    );

    // Restart: the node dies with both runs parked inside the inner loop,
    // and another node takes them over.
    case.kill("node-a", "both runs parked inside the inner loop")
        .await?;
    boot(case, "node-b").await?;
    let host = case.node("node-b")?;
    let mut observers = BTreeMap::new();
    for (name, run, definition, document, entry) in [
        ("generated", &old_run, &old, &first["graph"], &old_entry),
        ("edited", &new_run, &new, &second["graph"], &new_entry),
    ] {
        let read = host.get(&format!("/workflow/runs/{run}")).await?;
        ensure!(
            read["status"] == "Waiting" && read["workflow"]["definition"] == *definition,
            "the {name} run is parked on its own definition after the restart: {read}"
        );
        // (e) A follower attaches now, mid-run, on a node that never saw
        // the execution start.
        let attached = host
            .post(&format!("/workflow/runs/{run}/observers"), &json!({}))
            .await?;
        case.write(&format!("attached-{name}.json"), &attached)?;
        let overlay = &attached["attached"]["overlay"];
        ensure!(
            attached["attached"]["document"]["graph"] == *document
                && attached["attached"]["document"]["reference"] == read["document"]
                && attached["attached"]["document"]["entry"] == entry.as_str(),
            "the {name} follower read the document its run was admitted under"
        );
        ensure!(
            overlay.is_null()
                || (overlay["document"]["state"] == "loaded"
                    && overlay["coverage"]["start_observed"] == false),
            "the {name} follower has not observed the start: {overlay}"
        );
        observers.insert(
            name,
            attached["observer"]
                .as_str()
                .context("an observer id")?
                .to_owned(),
        );
    }

    // Resume both runs to their ends, one review at a time, side by side.
    let threshold_of = |edited: bool| if edited { 3 } else { 2 };
    let mut last_review = BTreeMap::new();
    let mut pending = vec![
        ("generated", old_run.clone(), false, 1, old_first),
        ("edited", new_run.clone(), true, 2, new_second),
    ];
    while !pending.is_empty() {
        let mut next = Vec::new();
        for (name, run, edited, nth, delivery) in pending {
            approve(case, "node-b", &delivery).await?;
            let total = lines()
                .iter()
                .filter(|line| line.qty > threshold_of(edited))
                .count();
            if nth == total {
                last_review.insert(name, delivery);
                continue;
            }
            let delivery = parked(case, "node-b", &run, nth + 1).await?;
            // Mid-run, the follower shows the review the run is parked on
            // now as in flight (the park itself is the process's committed
            // wait, which the run's row answers), bound to its call, and
            // still says it missed the start.
            let shape = if edited { &new_shape } else { &old_shape };
            let review = site(shape.review, "request")?;
            let host = case.node("node-b")?;
            let observer = &observers[name];
            let waiting = case
                .until(&format!("the {name} follower shows the park"), || async {
                    let observed = host.get(&format!("/workflow/observers/{observer}")).await?;
                    let shown = observed["overlay"]["sites"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|entry| {
                            entry["site"] == review
                                && entry["status"] == "running"
                                && entry["occurrence"] == nth as u64 + 1
                                && entry["call"]["call_id"] == delivery["call_id"]
                        });
                    Ok(shown.then_some(observed))
                })
                .await?;
            ensure!(
                waiting["overlay"]["document"]["state"] == "loaded"
                    && waiting["overlay"]["coverage"] == json!({"start_observed": false})
                    && waiting["overlay"]["settlement"].is_null()
                    && waiting["terminal"].is_null(),
                "mid-run, the {name} overlay is unsettled and incomplete: {}",
                waiting["overlay"]["coverage"]
            );
            next.push((name, run, edited, nth + 1, delivery));
        }
        pending = next;
    }

    for (name, run, edited, parked_on) in [
        ("generated", &old_run, false, 1),
        ("edited", &new_run, true, 2),
    ] {
        let output = case
            .node("node-b")?
            .get(&format!("/workflow/runs/{run}/output"))
            .await?;
        case.evidence
            .outputs
            .push(json!({"run": name, "process": run, "output": output}));
        let reviewed = |line: &&Line| line.qty > threshold_of(edited);
        let status = if edited { "signed-off" } else { "reviewed" };
        let expected = json!({
            "status": status,
            "approved": lines().iter().filter(reviewed).map(|line| line.item.clone()).collect::<Vec<_>>(),
            "skipped": lines().iter().filter(|line| !reviewed(line)).map(|line| line.sku.clone()).collect::<Vec<_>>(),
            "audited": {"status": status},
        });
        ensure!(
            output["success"] == true && output["value"] == expected,
            "the {name} run finishes as its own definition says: {output}"
        );
        // (f) The finished run still reads as the definition it was
        // admitted under.
        let read = case
            .node("node-b")?
            .get(&format!("/workflow/runs/{run}"))
            .await?;
        let definition = if edited { &new } else { &old };
        ensure!(
            read["status"] == "Completed" && read["workflow"]["definition"] == *definition,
            "the {name} run ended on its own definition: {read}"
        );
        // (e) What the late follower folded by the end.
        let observed = case
            .node("node-b")?
            .get(&format!(
                "/workflow/observers/{}?until=terminal",
                observers[name]
            ))
            .await?;
        case.write(&format!("observed-{name}.json"), &observed)?;
        ensure!(
            observed["error"].is_null() && observed["terminal"] == "Completed",
            "the {name} follower followed its run to the end: {} {}",
            observed["error"],
            observed["terminal"]
        );
        let (graph, entry, shape) = if edited {
            (&new_graph, &new_entry, &new_shape)
        } else {
            (&old_graph, &old_entry, &old_shape)
        };
        check_followed(
            &observed,
            shape,
            edited.then_some(cloned),
            &sites_of(&graph.process(entry).context("the entry")?.body)?,
            &Followed {
                threshold: threshold_of(edited),
                parked: parked_on,
                last_review: &last_review[name],
                reference: &read["document"],
            },
        )
        .with_context(|| format!("the {name} run's late follower"))?;

        // Every body ran once: the handover re-ran nothing.
        let mine: Vec<Value> = case
            .bodies()?
            .into_iter()
            .filter(|line| line["process"] == run.as_str())
            .collect();
        let entries: Vec<Value> = mine
            .iter()
            .filter(|line| line["tool"] == "ledger_record")
            .map(|line| line["args"]["entry"].clone())
            .collect();
        let mut recorded = Vec::new();
        for line in lines() {
            recorded.extend(std::iter::repeat_n(
                json!(line.item),
                if edited { 2 } else { 1 },
            ));
        }
        if edited {
            recorded.push(json!("signed-off"));
        }
        let calls: BTreeSet<&str> = mine
            .iter()
            .filter_map(|line| line["call_id"].as_str())
            .collect();
        ensure!(
            entries == recorded
                && calls.len() == mine.len()
                && mine.iter().all(|line| line["attempt"] == 1)
                && mine.len() == recorded.len() + lines().iter().filter(reviewed).count(),
            "the {name} run's bodies each ran once, in order: {entries:?}"
        );
    }
    // The two inline processes each recorded once, under the status their
    // own workflow ended with.
    let mut audits: Vec<Value> = case
        .bodies()?
        .into_iter()
        .filter(|line| line["process"] != old_run.as_str() && line["process"] != new_run.as_str())
        .map(|line| line["args"]["entry"].clone())
        .collect();
    audits.sort_by_key(ToString::to_string);
    ensure!(
        audits == [json!("audit:reviewed"), json!("audit:signed-off")],
        "each run's inline process ran once: {audits:?}"
    );
    Ok(())
}
