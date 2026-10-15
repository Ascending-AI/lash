//! The example's one law, over the HTTP surface a client uses.
#![expect(
    clippy::expect_used,
    reason = "test module: this law fails by panicking"
)]

use std::sync::Arc;

use lash::workflow::view::PathSite;
use serde_json::{Value, json};
use workflow_graph_roundtrip::{AppState, RunEvent, RunPath, RunStatus};

async fn serve() -> (String, lash::LashCore) {
    let stores = lash::sqlite::SqliteStoreSet::memory()
        .await
        .expect("SQLite memory stores");
    let backend = lash::durable::DurableBackendBuilder::new(Arc::new(stores))
        .build()
        .expect("the durable backend");
    let host = workflow_graph_roundtrip::workflow_core(backend).expect("workflow core");
    let core = host.core().clone();
    let state = AppState::new(host).await.expect("workflow state");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a local port");
    let base = format!("http://{}", listener.local_addr().expect("the address"));
    tokio::spawn(workflow_graph_roundtrip::serve(listener, state));
    (base, core)
}

async fn answer(response: reqwest::Response) -> Value {
    let status = response.status();
    let body: Value = response.json().await.expect("a JSON answer");
    assert!(
        status.is_success(),
        "the request is answered: {status} {body}"
    );
    body
}

/// Runs the saved workflow with `inputs` to its end, approving whatever it
/// asks for.
async fn run(client: &reqwest::Client, base: &str, inputs: Value) -> Vec<RunEvent> {
    let mut response = client
        .post(format!("{base}/run"))
        .json(&json!({ "inputs": inputs }))
        .send()
        .await
        .expect("POST /run");
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.expect("the start refusal");
        panic!("the run starts: {status} {body}");
    }
    let mut pending = String::new();
    let mut events = Vec::new();
    let mut resolved = std::collections::BTreeSet::new();
    while let Some(chunk) = response.chunk().await.expect("an SSE chunk") {
        pending.push_str(std::str::from_utf8(&chunk).expect("UTF-8 events"));
        while let Some(end) = pending.find("\n\n") {
            let frame = pending.drain(..end + 2).collect::<String>();
            assert!(!frame.contains("event: run_error"), "{frame}");
            for data in frame.lines().filter_map(|line| line.strip_prefix("data: ")) {
                let event: RunEvent = serde_json::from_str(data).expect("a run event");
                if let Some(key) = &event.approval_key
                    && resolved.insert(key.clone())
                {
                    answer(
                        client
                            .post(format!("{base}/approvals/{key}"))
                            .json(&json!({"approved": true}))
                            .send()
                            .await
                            .expect("the operator approves"),
                    )
                    .await;
                }
                events.push(event);
            }
        }
    }
    events
}

/// FIG-5757: a catalog workflow is a kernel document end to end. The host
/// serves it with its sites and the TypeScript the printer spells; one
/// kernel edit transaction saves and publishes a new version and answers
/// where each node went; a refused transaction changes nothing; and a run
/// of the edited version reports every event at a site of that document,
/// shows the edited value, and ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_catalog_workflow_is_edited_republished_run_and_shown_as_a_kernel_document() {
    let (base, core) = serve().await;
    let client = reqwest::Client::new();
    let opened = answer(
        client
            .post(format!("{base}/workflow/select"))
            .json(&json!({"id": "onboarding"}))
            .send()
            .await
            .expect("select"),
    )
    .await;
    assert!(
        opened["definition"].is_string() && opened["notAdmitted"].is_null(),
        "the catalog workflow is admitted: {opened}"
    );
    let source = opened["source"].as_str().expect("the printer spells it");
    let original: lash::workflow::document::Document =
        serde_json::from_value(opened["document"].clone()).expect("the document is readable");
    let decoded = lash::typescript::read(source, &original, Default::default())
        .expect("the source reads back against its document without a library");
    assert_eq!(
        original.canonical_bytes().unwrap(),
        decoded.canonical_bytes().unwrap()
    );
    assert!(
        source.contains("Welcome to the workflow graph") && source.contains("set_status"),
        "the TypeScript lens shows the document: {source}"
    );

    // The host serves lash's view of the definition it published, verbatim.
    let served: lash::workflow::view::WorkflowView =
        serde_json::from_value(opened["view"].clone()).expect("lash's view");
    let definition = lash::process::ProcessDefinitionId::parse(
        opened["definition"].as_str().expect("the definition id"),
    )
    .expect("a definition id");
    let lash::workflow::WorkflowRead::Inspected(published) = core
        .host_artifacts()
        .definition_graph(&definition)
        .await
        .expect("the definition reads")
    else {
        panic!("lash holds the published definition");
    };
    assert_eq!(
        served,
        lash::workflow::view::derive(&published.document).expect("the view"),
        "the view is lash's read of the published document"
    );

    // The message the workflow shows first is the text literal of the record
    // lash binds before performing it: `let tN = {text: "Welcome ..."}`.
    let statements = opened["view"]["statements"].as_array().expect("statements");
    let bound = statements
        .iter()
        .find(|statement| {
            statement["statement"]["let"]["value"]["expr"]["record"][0]["value"]["literal"]["text"]
                == "Welcome to the workflow graph"
        })
        .expect("the statement binding the welcome message");
    let record = {
        let mut site = bound["site"].clone();
        site["path"].as_array_mut().expect("a path").push(json!(0));
        site
    };
    let literal = {
        let mut site = record.clone();
        site["path"].as_array_mut().expect("a path").push(json!(0));
        site
    };

    // A transaction the checker refuses changes nothing.
    let refused = client
        .post(format!("{base}/workflow/edits"))
        .json(&json!({"version": opened["version"], "edits": [
            {"replace_expression": {"expression": literal, "with": {"variable": "nowhere"}}},
        ]}))
        .send()
        .await
        .expect("edits");
    assert_eq!(refused.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    let refused: Value = refused.json().await.expect("an error body");
    assert!(
        refused["error"]["code"] == "edit_refused"
            && refused["error"]["details"]["diagnostics"][0]["site"] == literal,
        "the refusal names the site it is about: {refused}"
    );
    let unchanged = answer(
        client
            .get(format!("{base}/workflow"))
            .send()
            .await
            .expect("get"),
    )
    .await;
    assert_eq!(unchanged["identity"], opened["identity"]);

    // The host previews public authoring operations, then submits their
    // ordinary transaction through the same edit/admission route.
    let planned = answer(
        client
            .post(format!("{base}/workflow/plan"))
            .json(&json!({"version": opened["version"], "operations": [
                {"edit": {"replace_expression": {
                    "expression": literal,
                    "with": {"literal": {"text": "Edited as a kernel document"}},
                }}},
                {"insert_effect": {
                    "at": {"block": bound["block"], "before": bound["site"]},
                    "effect": "display.show_message",
                    "args": [{"record": [{"field": "text", "value": {"literal": {"text": "Added through public authoring"}}}]}],
                    "result": "null",
                    "binding": "extra_message",
                }},
            ]}))
            .send()
            .await
            .expect("preview authoring operations"),
    )
    .await;
    let still_saved = answer(
        client
            .get(format!("{base}/workflow"))
            .send()
            .await
            .expect("get after preview"),
    )
    .await;
    assert_eq!(still_saved["identity"], opened["identity"]);
    assert_eq!(still_saved["version"], opened["version"]);
    let edited = answer(
        client
            .post(format!("{base}/workflow/edits"))
            .json(&json!({"version": opened["version"], "edits": planned["transaction"]["edits"]}))
            .send()
            .await
            .expect("edits"),
    )
    .await;
    let workflow = &edited["workflow"];
    assert!(
        workflow["version"] == opened["version"].as_u64().expect("a version") + 1
            && workflow["definition"].is_string()
            && workflow["definition"] != opened["definition"]
            && workflow["identity"] != opened["identity"],
        "the edit saved and published a new version: {workflow}"
    );
    assert!(
        workflow["source"]
            .as_str()
            .is_some_and(|source| source.contains("Edited as a kernel document")),
        "the lens follows the edit"
    );
    let survivors = edited["correspondence"]["entries"]
        .as_array()
        .expect("survivors");
    assert!(
        survivors
            .iter()
            .filter(|survivor| survivor["edited"] == true)
            .map(|survivor| &survivor["from"])
            .eq([&literal]),
        "the correspondence marks the replaced literal alone as edited"
    );

    let events = run(&client, &base, json!({})).await;
    let sites: Vec<&Value> = workflow["view"]["execution_sites"]
        .as_array()
        .expect("execution sites")
        .iter()
        .map(|site| &site["site"])
        .collect();
    let last = events.last().expect("the run reports");
    assert!(
        last.site.is_none() && last.status == RunStatus::Succeeded,
        "the run ends completed: {last:?}"
    );
    for event in &events {
        assert_eq!(event.workflow_version, workflow["version"]);
        assert_eq!(json!(event.definition), workflow["identity"]);
        if let Some(site) = &event.site {
            let site = serde_json::to_value(site).expect("a site");
            assert!(
                sites.contains(&&site),
                "the run reports only sites of its document: {site}"
            );
        }
    }
    assert_eq!(
        last.display.messages,
        [
            "Added through public authoring",
            "Edited as a kernel document"
        ],
        "the run showed the helper's input and the directly edited message"
    );
    assert_eq!(last.display.progress, 100.0);
    assert_eq!(
        last.display.lists["steps"],
        ["Approved", "Loop item", "Loop item"]
    );
}

#[tokio::test]
async fn each_edit_response_maps_its_immediate_base() {
    let (base, _) = serve().await;
    let client = reqwest::Client::new();
    let mut previous = answer(
        client
            .get(format!("{base}/workflow"))
            .send()
            .await
            .expect("read"),
    )
    .await;
    for number in 0..2 {
        let first = &previous["view"]["statements"][0];
        let response = answer(client.post(format!("{base}/workflow/edits")).json(&json!({
            "version": previous["version"],
            "edits": [{"insert_statement": {
                "at": {"block": first["block"], "before": first["site"]},
                "statement": {"let": {"name": format!("correspondence_probe_{number}"), "value": {"expr": {"literal": "null"}}}}
            }}]
        })).send().await.expect("edit")).await;
        assert_eq!(
            response["correspondence"]["base"], previous["identity"],
            "the response must map from this request's document, not the originally opened document"
        );
        assert_eq!(
            response["correspondence"]["result"],
            response["workflow"]["identity"]
        );
        previous = response["workflow"].clone();
    }
}

/// Named entry inputs are sent to the canonical process launch; the settled
/// result and tool call evidence must agree with that exact invocation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn named_inputs_reach_effects_and_the_final_workflow_output() {
    let (base, _) = serve().await;
    let client = reqwest::Client::new();
    let workflow = answer(
        client
            .post(format!("{base}/workflow/select"))
            .json(&json!({"id":"deep-research"}))
            .send()
            .await
            .expect("select"),
    )
    .await;
    assert_eq!(
        workflow["document"]["entries"]["deep_research"]["params"][0]["ty"],
        "text"
    );
    assert_eq!(workflow["view"]["dataflow"]["bindings"][0]["type"], "any");
    assert_eq!(workflow["view"]["dataflow"]["returns"][0]["type"], "any");
    let refused = client
        .post(format!("{base}/run"))
        .json(&json!({"version": 0, "inputs":{"topic":"Should not run"}}))
        .send()
        .await
        .expect("stale request");
    assert_eq!(refused.status(), reqwest::StatusCode::CONFLICT);
    let response = client
        .post(format!("{base}/run"))
        .json(&json!({"version":workflow["version"],"inputs":{"topic":"Typed dataflow research"}}))
        .send()
        .await
        .expect("run");
    assert!(response.status().is_success());
    let body = response.text().await.expect("SSE body");
    assert!(!body.contains("event: run_error"), "{body}");
    let events: Vec<RunEvent> = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|json| serde_json::from_str(json).expect("run event"))
        .collect();
    let last = events.last().expect("terminal event");
    assert_eq!(last.status, RunStatus::Succeeded);
    assert_eq!(
        last.output.as_ref().expect("returned report")["title"],
        "Typed dataflow research"
    );
    assert!(
        events
            .iter()
            .filter_map(|event| event.call.as_ref())
            .any(|call| call.input["topic"] == "Typed dataflow research")
    );
    assert_eq!(
        workflow["view"]["dataflow"]["parameters"]
            .as_array()
            .expect("parameters")
            .len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn imported_parameterized_entries_can_run_with_named_inputs() {
    let (base, _) = serve().await;
    let client = reqwest::Client::new();
    let before = answer(
        client
            .get(format!("{base}/workflow"))
            .send()
            .await
            .expect("workflow"),
    )
    .await;
    let workflow = answer(
        client
            .post(format!("{base}/workflow/source"))
            .json(&json!({
                "version": before["version"], "entry": "echo",
                "source": "export async function echo(message: string) { return message; }"
            }))
            .send()
            .await
            .expect("import parameterized source"),
    )
    .await;
    let response = client
        .post(format!("{base}/run"))
        .json(&json!({
            "version": workflow["version"], "inputs": {"message":"Named input from imported source"}
        }))
        .send()
        .await
        .expect("run imported entry");
    assert!(response.status().is_success());
    let body = response.text().await.expect("run events");
    let terminal: RunEvent = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .next_back()
        .map(|data| serde_json::from_str(data).expect("terminal event"))
        .expect("run event");
    assert_eq!(terminal.status, RunStatus::Succeeded);
    assert_eq!(
        terminal.output,
        Some(json!("Named input from imported source"))
    );
}

/// FIG-6401: every example is authored TypeScript and opens as exactly what
/// lash's workflow import lowers it to (its view is the size the catalog
/// lists, and its authored text is kept beside the printer's TypeScript and
/// map), then runs to its authored result against the mock tools.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_example_opens_as_its_imported_source_and_runs_with_the_mocks() {
    let (base, _) = serve().await;
    let client = reqwest::Client::new();
    let catalog = answer(
        client
            .get(format!("{base}/workflows"))
            .send()
            .await
            .expect("the catalog"),
    )
    .await;
    let topic = json!([{"title": "Release notes", "urgent": true}]);
    let runs = [
        ("deep-research", json!({"topic": "Loops"}), None),
        ("weekly-kpis", json!({"workspace": "Acme Cloud"}), None),
        ("blank", json!({}), Some(Value::Null)),
        ("onboarding", json!({}), Some(json!({"approved": true}))),
        ("summarize-emails", json!({}), None),
        ("research-nvidia-stock", json!({}), None),
        ("team-standup-digest", json!({}), None),
        ("traffic-lights", json!({}), Some(Value::Null)),
        (
            "branching-approval",
            json!({}),
            Some(json!({"approved": true})),
        ),
        ("counter-loop", json!({}), Some(json!(3))),
        ("flat", json!({}), None),
        ("branch", json!({}), Some(Value::Null)),
        ("digest", json!({"topics": topic}), Some(json!({"seen": 1}))),
        ("typed", json!({"topics": topic}), Some(json!({"seen": 1}))),
        (
            "inbox",
            json!({"items": [{"title": "Hello"}]}),
            Some(json!({"sent": 1})),
        ),
        ("countdown", json!({}), Some(json!({"count": 4}))),
        ("nested", json!({}), Some(json!({"filed": 6}))),
        ("retry", json!({}), Some(json!({"failures": 1}))),
        ("helper", json!({}), None),
        ("unread", json!({}), None),
    ];
    let entries = catalog.as_array().expect("a list");
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry["id"].as_str().expect("an id"))
            .collect::<Vec<_>>(),
        runs.iter().map(|(id, _, _)| *id).collect::<Vec<_>>(),
        "the catalog lists each example"
    );
    for ((id, inputs, output), entry) in runs.into_iter().zip(entries) {
        let opened = answer(
            client
                .post(format!("{base}/workflow/select"))
                .json(&json!({ "id": id }))
                .send()
                .await
                .expect("select"),
        )
        .await;
        assert!(
            opened["definition"].is_string(),
            "{id} is admitted: {}",
            opened["notAdmitted"]
        );
        assert_eq!(
            (
                opened["view"]["statements"].as_array().map(Vec::len),
                opened["view"]["scopes"].as_array().map(Vec::len),
            ),
            (
                entry["statements"].as_u64().map(|count| count as usize),
                entry["scopes"].as_u64().map(|count| count as usize),
            ),
            "{id} opens as the lowering the catalog counts"
        );
        assert_eq!(
            opened["authored"]["version"], opened["version"],
            "{id} keeps the source it was imported from"
        );
        assert!(
            opened["sourceMap"]["sites"]
                .as_array()
                .is_some_and(|sites| !sites.is_empty()),
            "{id} has lash's TypeScript source map"
        );
        let events = run(&client, &base, inputs).await;
        let last = events.last().expect("the run reports");
        assert!(
            last.site.is_none() && last.status == RunStatus::Succeeded,
            "{id} runs to its end: {last:?}"
        );
        if let Some(output) = output {
            // A `null` result reads back as an absent output.
            assert_eq!(
                last.output.clone().unwrap_or_default(),
                output,
                "{id} returns what it authored"
            );
        }
        println!(
            "{id}: {} statements, {} scopes",
            entry["statements"], entry["scopes"]
        );
    }
}

/// FIG-6404: a run is observed at `counts`, so the canvas can play it out.
/// Over two topics, one urgent, the digest's loop body is entered twice and
/// each of its statements runs twice, each branch runs once, the counts
/// arrive while the run is live, and the path lash's replay verifies starts
/// exactly the statements the counts say ran.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_counted_run_reports_each_statement_as_often_as_its_loop_ran_it() {
    let (base, _) = serve().await;
    let client = reqwest::Client::new();
    let opened = answer(
        client
            .post(format!("{base}/workflow/select"))
            .json(&json!({ "id": "digest" }))
            .send()
            .await
            .expect("select"),
    )
    .await;
    let topics = json!([
        {"title": "Release notes", "urgent": true},
        {"title": "Team offsite", "urgent": false},
    ]);
    let events = run(&client, &base, json!({ "topics": topics })).await;
    let last = events.last().expect("the run reports");
    assert_eq!(
        (last.status, last.output.clone()),
        (RunStatus::Succeeded, Some(json!({"seen": 2}))),
        "the digest runs to its end"
    );
    assert!(
        events[..events.len() - 1]
            .iter()
            .any(|event| event.coverage.is_some()),
        "counts arrive while the run is live"
    );
    let coverage = serde_json::to_value(last.coverage.as_ref().expect("the end carries coverage"))
        .expect("coverage encodes");
    assert_eq!(coverage["observation"], "counts");
    let view = &opened["view"];
    let list = |value: &Value| value.as_array().expect("a list").clone();
    let (statements, scopes) = (list(&view["statements"]), list(&view["scopes"]));
    let (runs, entries) = (list(&coverage["statements"]), list(&coverage["scopes"]));
    let count = |value: &Value| value.as_u64().expect("observed at counts");
    let entered = |scope: &Value| {
        count(&entries[scopes.iter().position(|s| s == scope).expect("a scope")]["entries"])
    };
    let ran_in = |scope: &Value| {
        statements
            .iter()
            .zip(&runs)
            .filter(|(statement, _)| statement["block"] == scope["block"])
            .map(|(_, ran)| count(&ran["runs"]))
            .collect::<Vec<_>>()
    };
    // The lowering iterates a list in one loop and other iterables in
    // another: the list loop ran once per topic, the other never did.
    let mut loops = scopes
        .iter()
        .filter(|scope| scope["kind"] == "for")
        .map(|scope| (entered(scope), ran_in(scope)))
        .collect::<Vec<_>>();
    loops.sort();
    assert_eq!(loops.len(), 2, "the lowering has two loops: {loops:?}");
    let (other, list_loop) = (&loops[0], &loops[1]);
    assert_eq!(list_loop.0, 2, "the loop ran once per topic");
    assert!(
        !list_loop.1.is_empty() && list_loop.1.iter().all(|runs| *runs == 2),
        "each statement in the loop ran once per topic: {list_loop:?}"
    );
    assert!(
        other.0 == 0 && other.1.iter().all(|runs| *runs == 0),
        "the other loop never ran: {other:?}"
    );
    // One topic is urgent and one is not: some `if` took each branch once.
    assert!(
        scopes
            .iter()
            .filter(|scope| scope["kind"] == "then")
            .any(|then| {
                entered(then) == 1
                    && scopes.iter().any(|other| {
                        other["kind"] == "else"
                            && other["statement"] == then["statement"]
                            && entered(other) == 1
                    })
            }),
        "the urgent test took each branch once"
    );
    let Some(RunPath::Verified { steps }) = &last.path else {
        panic!("the run replays to a verified path: {:?}", last.path);
    };
    let started = steps
        .iter()
        .filter(|step| matches!(step.site, PathSite::Statement { .. }))
        .count() as u64;
    let ran: u64 = runs
        .iter()
        .map(|ran| ran["runs"].as_u64().expect("observed"))
        .sum();
    assert_eq!(started, ran, "the path starts what the counts say ran");
}
