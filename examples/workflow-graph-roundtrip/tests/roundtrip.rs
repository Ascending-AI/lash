//! The example's one law, over the HTTP surface a client uses.
#![expect(
    clippy::expect_used,
    reason = "test module: this law fails by panicking"
)]

use std::sync::Arc;

use serde_json::{Value, json};
use workflow_graph_roundtrip::{AppState, RunEvent, RunStatus};

async fn serve() -> String {
    let stores = lash::sqlite::SqliteStoreSet::memory()
        .await
        .expect("SQLite memory stores");
    let backend = lash::durable::DurableBackendBuilder::new(Arc::new(stores))
        .build()
        .expect("the durable backend");
    let host = workflow_graph_roundtrip::workflow_core(backend).expect("workflow core");
    let state = AppState::new(host).await.expect("workflow state");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a local port");
    let base = format!("http://{}", listener.local_addr().expect("the address"));
    tokio::spawn(workflow_graph_roundtrip::serve(listener, state));
    base
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
    assert!(response.status().is_success(), "the run starts");
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
    let base = serve().await;
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
    assert!(
        source.contains("Welcome to the workflow graph") && source.contains("set_status"),
        "the TypeScript lens shows the document: {source}"
    );

    // The message the workflow shows first is a text literal of its second
    // statement pair: `let shown_2 = {text: ...}`.
    let statements = opened["statements"].as_array().expect("statements");
    let bound = statements
        .iter()
        .find(|statement| statement["summary"] == "let shown_2 = a value")
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

    let edited = answer(
        client
            .post(format!("{base}/workflow/edits"))
            .json(&json!({"version": opened["version"], "edits": [
                {"replace_expression": {
                    "expression": literal,
                    "with": {"literal": {"text": "Edited as a kernel document"}},
                }},
            ]}))
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
            .map(|survivor| &survivor["to"])
            .eq([&literal]),
        "the correspondence marks the replaced literal alone as edited"
    );

    let events = run(&client, &base).await;
    let sites: Vec<&Value> = workflow["executionSites"]
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
        ["Edited as a kernel document"],
        "the run showed the edited message"
    );
    assert_eq!(last.display.progress, 100.0);
    assert_eq!(
        last.display.lists["steps"],
        ["Approved", "Loop item", "Loop item"]
    );
}
