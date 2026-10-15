//! The example's one law, over the HTTP surface a client uses.
#![expect(
    clippy::expect_used,
    reason = "test module: this law fails by panicking"
)]

use std::sync::Arc;

use serde_json::{Value, json};
use workflow_graph_roundtrip::{AppState, RunEvent, RunStatus};

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

/// Runs the saved workflow to its end, approving whatever it asks for.
async fn run(client: &reqwest::Client, base: &str) -> Vec<RunEvent> {
    let mut response = client
        .post(format!("{base}/run"))
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
    let decoded = lash::typescript::compile_generated(source, Default::default())
        .expect("the source reconstructs without a library");
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

    // The message the workflow shows first is a text literal of its second
    // statement pair: `let shown_2 = {text: ...}`.
    let statements = opened["view"]["statements"].as_array().expect("statements");
    let bound = statements
        .iter()
        .find(|statement| statement["statement"]["let"]["name"] == "shown_2")
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
            .json(&json!({"version": opened["version"], "steps": [
                {"direct": {"replace_expression": {
                    "expression": literal,
                    "with": {"literal": {"text": "Edited as a kernel document"}},
                }}},
                {"effect": {
                    "at": {"block": {"unit": {"function": "onboarding"}, "path": []}, "before": bound["site"]},
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

    let events = run(&client, &base).await;
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
