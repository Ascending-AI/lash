//! S38: one workflow through the whole workflow layer on a real host.
//!
//! The host is `examples/e2e-consumer` in its workflow mode: it is built on
//! `lash::workflow` and the facade, and holds no source language. The case
//! hands it a generated workflow as a kernel document written against the
//! host's environment, reads the published definition back through the
//! facade, edits inside the `try` region and inside the inner loop with
//! kernel edits, publishes, and runs the old and the new definition side by
//! side. Both runs park inside the inner loop; the node is killed and
//! another takes them over. A follower that attaches there has missed the
//! start of the execution, and what it folds is checked against a count of
//! the loops made from the order the runs were given.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context as _, Result, bail, ensure};
use lash::workflow::document::{
    Action, Document, Expr, Literal, Name, Node, Rhs, Site, Stmt, Unit, parse_document,
};
use lash::workflow::edit::{Correspondence, Edit, Position};
use lash_e2e::{Case, Host, NodeOptions};
use serde_json::{Value, json};

/// The checked-in workflow, as a generator wrote it: a kernel document
/// whose library functions are named and resolved against the host.
const FIXTURE: &str = "examples/e2e-consumer/src/workflow/order_review.kernel";
/// The entry of the fixture a run starts.
const WORKFLOW: &str = "order_review";
/// The entry the workflow starts as a process of its own.
const AUDIT: &str = "audit";

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

fn document(value: &Value) -> Result<Document> {
    serde_json::from_value(value.clone()).context("the host's document is a kernel document")
}

/// Every site of the workflow entry's body whose node `wanted` accepts, in
/// document order.
fn sites(document: &Document, wanted: impl Fn(Node<'_>) -> bool) -> Result<Vec<Site>> {
    fn walk(node: Node<'_>, site: Site, wanted: &impl Fn(Node<'_>) -> bool, found: &mut Vec<Site>) {
        if wanted(node) {
            found.push(site.clone());
        }
        for (index, child) in (0u32..).zip(node.children()) {
            walk(child, site.child(index), wanted, found);
        }
    }
    let entry = Name::new(WORKFLOW);
    let function = document
        .functions
        .get(&entry)
        .context("the document declares the workflow")?;
    let mut found = Vec::new();
    walk(
        Node::Block(&function.body),
        Site::new(Unit::Function(entry), Vec::new()),
        &wanted,
        &mut found,
    );
    Ok(found)
}

fn one(document: &Document, what: &str, wanted: impl Fn(Node<'_>) -> bool) -> Result<Site> {
    let found = sites(document, wanted)?;
    let [site] = found.as_slice() else {
        bail!("the workflow has one {what}: {found:?}");
    };
    Ok(site.clone())
}

fn is_text(node: Node<'_>, text: &str) -> bool {
    matches!(node, Node::Expr(Expr::Literal(Literal::Text(value))) if value == text)
}

fn performs(node: Node<'_>, effect: &str) -> bool {
    matches!(node, Node::Action(Action::Perform { effect: performed, .. })
        if performed.to_string() == effect)
}

/// Whether `site` is `under` or a node beneath it.
fn within(site: &Site, under: &Site) -> bool {
    site.unit == under.unit && site.path.starts_with(&under.path)
}

/// The statement an action is the right-hand side of.
fn statement_of(action: &Site) -> Site {
    let mut statement = action.clone();
    statement.path.pop();
    statement
}

/// The statements of `main { <text> }`, as kernel text spells them.
fn statements(text: &str) -> Result<Vec<Stmt>> {
    Ok(parse_document(&format!(
        "kernel 1\nnumbers float\neffect ledger.record(input: Any) -> Any\n\nmain {{\n{text}\n}}\n"
    ))
    .with_context(|| format!("`{text}` parses"))?
    .main)
}

fn expression(text: &str) -> Result<Expr> {
    let Ok([Stmt::Finish { value }]) =
        <[Stmt; 1]>::try_from(statements(&format!("finish {text}"))?)
    else {
        bail!("`finish {text}` is one statement");
    };
    Ok(value)
}

/// The sites of the workflow the case edits, runs and reads the overlay at.
struct Shape {
    /// The `try` statement and its two blocks.
    region: Site,
    try_body: Site,
    catch_body: Site,
    /// The two `for` statements, and the inner one's body block.
    outer: Site,
    inner: Site,
    inner_body: Site,
    /// The literal the threshold condition compares a quantity with.
    threshold: Site,
    /// The `review.request` action a run parks on.
    review: Site,
    /// The `ledger.record` actions of the inner loop body, in order: one in
    /// the generated workflow, two once it is cloned.
    records: Vec<Site>,
    /// The `ledger.record` action an edit authored at the end of the `try`
    /// body; the generated workflow has none.
    authored: Option<Site>,
    /// The literal the `try` body's last assignment sets the status to.
    status: Site,
    /// The `processes.start` and `processes.await` of the audit process.
    start: Site,
    wait: Site,
}

fn shape(document: &Document, status: &str) -> Result<Shape> {
    let region = one(document, "try", |node| {
        matches!(node, Node::Stmt(Stmt::Try(_)))
    })?;
    let (try_body, catch_body) = (region.child(0), region.child(1));
    let loops = sites(document, |node| {
        matches!(node, Node::Stmt(Stmt::For { .. }))
    })?;
    let [outer, inner] = loops.as_slice() else {
        bail!("the workflow has an outer and an inner loop: {loops:?}");
    };
    ensure!(
        within(outer, &try_body) && within(inner, &outer.child(1)),
        "the loops nest inside the try body"
    );
    let inner_body = inner.child(1);
    let threshold = one(document, "threshold literal", |node| {
        matches!(node, Node::Expr(Expr::Literal(Literal::Int(_))))
    })?;
    let review = one(document, "review", |node| performs(node, "review.request"))?;
    ensure!(
        within(&threshold, &inner_body) && within(&review, &inner_body),
        "the threshold and the review are in the inner loop"
    );
    let records: Vec<Site> = sites(document, |node| performs(node, "ledger.record"))?
        .into_iter()
        .filter(|site| within(site, &inner_body))
        .collect();
    ensure!(!records.is_empty(), "the inner loop records");
    let authored = sites(document, |node| performs(node, "ledger.record"))?
        .into_iter()
        .find(|site| within(site, &try_body) && !within(site, &inner_body));
    ensure!(
        sites(document, |node| performs(node, "ledger.record"))?
            .iter()
            .any(|site| within(site, &catch_body)),
        "the catch body records"
    );
    // An assignment to a variable has one child: its value.
    let status = one(document, "assignment of the status", |node| {
        matches!(
            node,
            Node::Stmt(Stmt::Assign { value: Rhs::Expr(Expr::Literal(Literal::Text(value))), .. })
                if value == status
        )
    })?
    .child(0);
    ensure!(within(&status, &try_body), "the try body sets the status");
    Ok(Shape {
        region,
        try_body,
        catch_body,
        outer: outer.clone(),
        inner: inner.clone(),
        inner_body,
        threshold,
        review,
        records,
        authored,
        status,
        start: one(document, "start", |node| performs(node, "processes.start"))?,
        wait: one(document, "await", |node| performs(node, "processes.await"))?,
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
            let wait = json!({"kind": "call", "call_id": delivery["call_id"], "tool_id": "tool:review_request"});
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

/// A site as the overlay and the feed spell it.
fn spelled(site: &Site) -> Result<Value> {
    Ok(serde_json::to_value(site)?)
}

/// What one site of the document must show to a follower that attached
/// while its run was parked.
struct Expected {
    name: &'static str,
    site: Site,
    /// How many times the site runs over the whole order.
    total: u64,
    /// The first run of the site (from 1) that ends after the follower
    /// attached.
    first_seen: u64,
    /// Whether the follower sees the start of `first_seen` too.
    start_seen: bool,
    /// The line a run of the site (from 1) is on, or `None` outside the
    /// loops.
    line: Box<dyn Fn(u64) -> Option<usize>>,
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
    /// The execution sites of the run's document, as the host lists them.
    document_sites: &'a Value,
}

/// What the follower of `run` folded by the end, against the loops of the
/// order.
fn check_followed(observed: &Value, shape: &Shape, run: &Followed<'_>) -> Result<()> {
    let Followed {
        threshold,
        parked,
        last_review,
        reference,
        document_sites,
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
    let on_line =
        || -> Box<dyn Fn(u64) -> Option<usize>> { Box::new(|run| Some(run as usize - 1)) };
    let outside = || -> Box<dyn Fn(u64) -> Option<usize>> { Box::new(|_| None) };
    let mut expected = vec![Expected {
        name: "review",
        site: shape.review.clone(),
        total: reviewed.len() as u64,
        first_seen: parked as u64,
        start_seen: false,
        line: {
            let reviewed = reviewed.clone();
            Box::new(move |run| reviewed.get(run as usize - 1).copied())
        },
    }];
    for record in &shape.records {
        expected.push(Expected {
            name: "record",
            site: record.clone(),
            total: count,
            first_seen: at as u64 + 1,
            start_seen: true,
            line: on_line(),
        });
    }
    for (name, site) in [
        ("authored record", shape.authored.as_ref()),
        ("start of the audit process", Some(&shape.start)),
        ("await of the audit process", Some(&shape.wait)),
    ] {
        let Some(site) = site else {
            continue;
        };
        expected.push(Expected {
            name,
            site: site.clone(),
            total: 1,
            first_seen: 1,
            start_seen: true,
            line: outside(),
        });
    }

    // The overlay lists only execution sites of the document, each in the
    // one task of the run, and never one of the catch clause, which no run
    // entered.
    let listed = overlay["sites"].as_array().cloned().unwrap_or_default();
    let known: BTreeSet<String> = document_sites
        .as_array()
        .into_iter()
        .flatten()
        .map(|site| site["site"].to_string())
        .collect();
    for entry in &listed {
        ensure!(
            known.contains(&entry["site"]["site"].to_string()) && entry["site"]["task"] == "main",
            "the overlay lists a site the document lacks: {}",
            entry["site"]
        );
        let site: Site = serde_json::from_value(entry["site"]["site"].clone())?;
        ensure!(
            !within(&site, &shape.catch_body),
            "the overlay invents no execution of the catch clause: {entry}"
        );
    }
    ensure!(
        listed.len() == expected.len(),
        "the overlay lists the sites the run reached after the attach: {listed:?}"
    );

    let (outer, inner) = (spelled(&shape.outer)?, spelled(&shape.inner)?);
    for site in &expected {
        // Per-site occurrences, counted from 0: the latest is the last the
        // order causes, and the counts are the ones that ended, and
        // started, after the follower attached.
        let at_site = spelled(&site.site)?;
        let entry = listed
            .iter()
            .find(|entry| entry["site"]["site"] == at_site)
            .with_context(|| format!("the overlay lists the {} site", site.name))?;
        let seen = site.total + 1 - site.first_seen;
        let started = if site.start_seen { seen } else { seen - 1 };
        ensure!(
            entry["status"] == "completed"
                && entry["occurrence"] == site.total - 1
                && entry["summary"]["terminal_count"] == seen
                && entry["summary"]["started_count"] == started,
            "the {} site ends at occurrence {} with {seen} ended and {started} started: {entry}",
            site.name,
            site.total - 1
        );

        // Every observation of the site: its occurrence is one the follower
        // could see, on the loop iterations of the line it ran on.
        for item in &items {
            let fact = match item["item"].as_str() {
                Some("language") => &item["execution"]["at"],
                Some("step_body_started") => &item["step"]["at"],
                _ => continue,
            };
            if fact["site"] != at_site {
                continue;
            }
            ensure!(fact["task"] == "main", "the run has one task: {fact}");
            let run = fact["occurrence"].as_u64().context("an occurrence")? + 1;
            ensure!(
                (site.first_seen..=site.total).contains(&run),
                "the {} site reports run {run} after the attach: {fact}",
                site.name
            );
            let loops = fact["loops"].as_array().cloned().unwrap_or_default();
            match (site.line)(run) {
                Some(line) => {
                    let line = &lines[line];
                    ensure!(
                        loops
                            == [
                                json!({"site": outer, "iteration": line.outer - 1}),
                                json!({"site": inner, "iteration": line.inner - 1}),
                            ],
                        "run {run} of the {} site is on outer iteration {} and inner iteration {}: {fact}",
                        site.name,
                        line.outer - 1,
                        line.inner - 1
                    );
                }
                None => ensure!(
                    loops.is_empty(),
                    "the {} site runs outside every loop: {fact}",
                    site.name
                ),
            }
        }
    }
    // The overlay binds the last review to the call its body was admitted as.
    let review = listed
        .iter()
        .find(|entry| entry["site"]["site"] == spelled(&shape.review).unwrap_or_default())
        .context("the review site")?;
    ensure!(
        review["call"]["call_id"] == last_review["call_id"]
            && review["call"]["occurrence"] == expected[0].total - 1,
        "the review site is bound to the call that ran: {review}"
    );
    Ok(())
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

/// The fixture as a document `host` admits: each `@{name}` is the identity
/// the host's environment gives that library function, each effect carries
/// the signature the host offers it under, and the manifest lists every
/// function the host says the code reaches.
async fn generated(host: &lash_e2e::Node, text: &str) -> Result<Document> {
    let environment = host.get("/workflow/environment").await?;
    let mut text = text.to_owned();
    for (name, id) in environment["functions"]
        .as_object()
        .context("the environment names its functions")?
    {
        text = text.replace(
            &format!("@{{{name}}}"),
            &format!("@{}", id.as_str().context("an identity")?),
        );
    }
    let mut document = parse_document(&text).context("the generated workflow parses")?;
    for (effect, signature) in &mut document.manifest.effects {
        *signature = serde_json::from_value(environment["effects"][effect.to_string()].clone())
            .with_context(|| format!("the host offers `{effect}`"))?;
    }
    let required = host
        .post("/workflow/requirements", &serde_json::to_value(&document)?)
        .await?;
    document.manifest.functions = serde_json::from_value(required["functions"].clone())
        .context("the functions the document reaches")?;
    Ok(document)
}

async fn s38(case: &mut Case) -> Result<()> {
    let repo = std::env::var("LASH_E2E_REPO").context("LASH_E2E_REPO is required")?;
    let text = std::fs::read_to_string(std::path::Path::new(&repo).join(FIXTURE))?;
    boot(case, "node-a").await?;
    let entry = json!({"kind": "entry", "function": WORKFLOW});

    // (a) The generated workflow reaches the host as a kernel document. It
    // is written here, outside the host, against what the host offers.
    let host = case.node("node-a")?;
    let generated = generated(host, &text).await?;
    host.post(
        "/workflow/draft",
        &json!({"document": serde_json::to_value(&generated)?}),
    )
    .await?;
    let first = host
        .post("/workflow/draft/publish", &json!({"entry": WORKFLOW}))
        .await?;
    ensure!(
        first["published"] == true && first["workflow"]["reference"]["entry"] == entry,
        "the generated workflow is admitted as a definition of its entry: {first}"
    );
    case.write("published-generated.json", &first)?;

    // (b) The published definition, read back through the facade.
    let old = first["definition"].clone();
    let read = host
        .post("/workflow/definition", &json!({"definition": old["id"]}))
        .await?;
    ensure!(
        read["read"] == "inspected" && read["definition"] == old,
        "the facade reads the definition that was published: {read}"
    );
    ensure!(
        read["workflow"] == first["workflow"],
        "the definition reads as the document its publication answered"
    );
    let old_document = document(&read["workflow"]["document"])?;
    ensure!(
        old_document == generated,
        "the admitted document is the one the case wrote"
    );
    let old_shape = shape(&old_document, "reviewed")?;
    // The audit process is an entry of the same document, and the start
    // names it by function reference.
    ensure!(
        old_document.entries.contains_key(&Name::new(AUDIT))
            && old_document.entries.contains_key(&Name::new(WORKFLOW)),
        "the document lists the workflow and its audit process as entries"
    );
    one(
        &old_document,
        "reference to the audit entry",
        |node| matches!(node, Node::Expr(Expr::Literal(Literal::Function(name))) if name.as_str() == AUDIT),
    )?;

    // (f, first half) A run of the generated definition, parked inside the
    // inner loop before anything is edited.
    let old_run = start(case, "node-a", &old, "generated").await?;
    let old_first = parked(case, "node-a", &old_run, 1).await?;
    ensure!(
        old_first["args"]["item"] == "a/a1",
        "the generated run parks on its first reviewed line: {old_first}"
    );

    // (c) Kernel edits of the admitted document, inside the try region and
    // inside the inner loop, as one transaction.
    let host = case.node("node-a")?;
    let opened = host
        .post("/workflow/draft", &json!({"definition": old["id"]}))
        .await?;
    let draft = shape(&document(&opened["document"])?, "reviewed")?;
    let Ok([bind, record]) = <[Stmt; 2]>::try_from(statements(
        "let signed = {entry: \"signed-off\"}\ndo perform ledger.record(signed) as Any",
    )?) else {
        bail!("the authored statements are two");
    };
    let edits = json!({"edits": [
        // In the try region: one more recorded entry, authored as kernel
        // statements.
        Edit::InsertStatement { at: Position::end(draft.try_body.clone()), statement: bind },
        Edit::InsertStatement { at: Position::end(draft.try_body.clone()), statement: record },
        // In the try region: the status a completed review ends with.
        Edit::ReplaceExpression {
            expression: draft.status.clone(),
            with: expression("\"signed-off\"")?,
        },
        // In the inner loop: only larger lines are reviewed.
        Edit::ReplaceExpression { expression: draft.threshold.clone(), with: expression("3")? },
        // In the inner loop: every line is recorded twice.
        Edit::CloneStatement {
            statement: statement_of(&draft.records[0]),
            to: Position::end(draft.inner_body.clone()),
        },
    ]});
    case.write("edits.json", &edits)?;
    let edited = host.post("/workflow/draft/edits", &edits).await?;
    ensure!(edited["applied"] == true, "the edits apply: {edited}");
    let second = host
        .post("/workflow/draft/publish", &json!({"entry": WORKFLOW}))
        .await?;
    case.write("published-edited.json", &second)?;
    ensure!(
        second["published"] == true && second["workflow"]["reference"]["entry"] == entry,
        "the edited workflow is admitted: {second}"
    );
    let new = second["definition"].clone();
    ensure!(
        new["id"] != old["id"]
            && second["workflow"]["reference"]["document"].is_string()
            && second["workflow"]["reference"]["document"]
                != first["workflow"]["reference"]["document"],
        "an edit publishes a new definition of a new document"
    );
    let new_document = document(&second["workflow"]["document"])?;
    let new_shape = shape(&new_document, "signed-off")?;

    // Correspondence: every node of the generated document survives into
    // the edited one; the two replaced expressions are the only nodes an
    // edit wrote; the nodes the case edits around end where the edited
    // document holds them; and the only new nodes are the two authored
    // statements and the clone.
    let moved: Correspondence = serde_json::from_value(second["correspondence"].clone())
        .context("the publication answers a correspondence")?;
    let old_sites = sites(&old_document, |_| true)?;
    for site in &old_sites {
        let survivor = moved
            .survivor(site)
            .with_context(|| format!("node {site} of the generated document survives"))?;
        ensure!(
            survivor.edited == (*site == old_shape.status || *site == old_shape.threshold),
            "only the replaced expressions were edited: {survivor:?}"
        );
    }
    for (was, now) in [
        (&old_shape.region, &new_shape.region),
        (&old_shape.outer, &new_shape.outer),
        (&old_shape.inner, &new_shape.inner),
        (&old_shape.threshold, &new_shape.threshold),
        (&old_shape.review, &new_shape.review),
        (&old_shape.records[0], &new_shape.records[0]),
        (&old_shape.status, &new_shape.status),
        (&old_shape.start, &new_shape.start),
        (&old_shape.wait, &new_shape.wait),
    ] {
        ensure!(
            moved.successor(was) == Some(now),
            "node {was} corresponds to {now}: {:?}",
            moved.successor(was)
        );
    }
    ensure!(
        old_shape.authored.is_none() && new_shape.authored.is_some(),
        "only the edited workflow records the authored entry"
    );
    let [_, cloned] = new_shape.records.as_slice() else {
        bail!(
            "the edited inner loop records twice: {:?}",
            new_shape.records
        );
    };
    let signed = sites(&new_document, |node| is_text(node, "signed-off"))?;
    ensure!(
        signed.len() == 2 && signed.iter().all(|site| within(site, &new_shape.try_body)),
        "the status and the authored entry are in the try body: {signed:?}"
    );
    let new_sites: Vec<Site> = sites(&new_document, |_| true)?
        .into_iter()
        .filter(|site| moved.predecessor(site).is_none())
        .collect();
    let try_statements = sites(&new_document, |node| matches!(node, Node::Stmt(_)))?
        .into_iter()
        .filter(|site| site.path.len() == new_shape.try_body.path.len() + 1)
        .filter(|site| within(site, &new_shape.try_body))
        .collect::<Vec<_>>();
    let [.., bound, recorded] = try_statements.as_slice() else {
        bail!("the edited try body ends with the authored statements");
    };
    let cloned_statement = statement_of(cloned);
    ensure!(
        new_sites.len() == 7
            && new_sites.iter().all(|site| {
                within(site, bound) || within(site, recorded) || within(site, &cloned_statement)
            })
            && sites(&new_document, |_| true)?.len() == old_sites.len() + 7,
        "the only new nodes are the authored statements and the clone: {new_sites:?}"
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
    for (name, run, definition, publication) in [
        ("generated", &old_run, &old, &first),
        ("edited", &new_run, &new, &second),
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
            attached["attached"]["document"] == publication["workflow"]
                && attached["attached"]["document"]["reference"] == read["document"],
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
            let review = json!({"site": spelled(&shape.review)?, "task": "main"});
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
                                && entry["occurrence"] == nth as u64
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
        let (publication, shape) = if edited {
            (&second, &new_shape)
        } else {
            (&first, &old_shape)
        };
        check_followed(
            &observed,
            shape,
            &Followed {
                threshold: threshold_of(edited),
                parked: parked_on,
                last_review: &last_review[name],
                reference: &read["document"],
                document_sites: &publication["workflow"]["execution_sites"],
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
