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
            "edits": [{"insert_step": {
                "base": previous["identity"],
                "at": {"block": first["block"], "before": first["site"]},
                "effect": "display.show_message", "args": [{"record":[{"field":"text","value":{"literal":{"text":"probe"}}}]}], "result":"null", "binding": format!("correspondence_probe_{number}")
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

/// CANVAS-CLOSED: neither endpoint accepts the retired tree writes or passthrough.
#[tokio::test]
async fn canvas_closed_http() {
    let (base, _) = serve().await;
    let client = reqwest::Client::new();
    let opened = answer(client.get(format!("{base}/workflow")).send().await.unwrap()).await;
    for path in ["edits", "plan"] {
        let mut body = json!({"version":opened["version"]});
        body[if path == "edits" {
            "edits"
        } else {
            "operations"
        }] = json!([
            {"edit":{"replace_statement":{"statement":{"unit":"main","path":[0]},"with":{"finish":{"value":{"literal":"null"}}}}}}
        ]);
        let response = client
            .post(format!("{base}/workflow/{path}"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
        let error: Value = response.json().await.unwrap();
        assert_eq!(
            serde_json::from_value::<lash::workflow::edit::EditDiagnosticKind>(
                error["error"]["details"]["cause"].clone()
            )
            .unwrap(),
            lash::workflow::edit::EditDiagnosticKind::UnsupportedCanvasEdit
        );
    }
    let unchanged = answer(client.get(format!("{base}/workflow")).send().await.unwrap()).await;
    assert_eq!(unchanged["identity"], opened["identity"]);
}

/// EXAMPLE-SOURCE / EXAMPLE-DISPLAY: the authored pane reads the retained
/// map/source; printed reads preserve identity and authoring edits drop the map.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authored_source_is_read_from_its_published_map() {
    let (base, core) = serve().await;
    let client = reqwest::Client::new();
    for (dialect, source, format) in [
        (
            "typescript",
            "export async function demo() { return await corpus.echo(\"日本 🦀\"); }",
            "type_script_v1",
        ),
        (
            "python",
            "async def demo():\n    return await corpus.echo(\"日本 🦀\")\n",
            "python_v1",
        ),
    ] {
        let before = answer(client.get(format!("{base}/workflow")).send().await.unwrap()).await;
        let imported = answer(client.post(format!("{base}/workflow/source")).json(&json!({
            "version": before["version"], "entry": "demo", "dialect": dialect, "source": source
        })).send().await.unwrap()).await;
        assert!(
            imported["authored"]["sourceMapId"].is_string(),
            "the host holds the publication's map id"
        );
        let id = serde_json::from_value(imported["authored"]["sourceMapId"].clone()).unwrap();
        let lash::workflow::SourceMapRead::Read {
            map,
            source: stored_source,
        } = core.host_artifacts().read_source_map(&id).await.unwrap()
        else {
            panic!("the publication retains its source map");
        };
        assert_eq!(stored_source, source);
        assert_eq!(stored_source, imported["authored"]["text"]);
        assert_eq!(
            serde_json::to_value(map).unwrap(),
            imported["authored"]["sourceMap"]
        );
        let read = answer(
            client
                .post(format!("{base}/workflow/source"))
                .json(&json!({
                    "version": imported["version"], "entry": "demo", "dialect": dialect,
                    "format": format, "source": imported["source"]
                }))
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(
            read["identity"], imported["identity"],
            "printed reading preserves canonical bytes"
        );
        assert_eq!(read["document"], imported["document"]);
        assert_eq!(read["sourceDialect"], dialect);
        // Re-import authored text, then edit a tool argument through the closed API.
        let imported = answer(client.post(format!("{base}/workflow/source")).json(&json!({
            "version": read["version"], "entry": "demo", "dialect": dialect, "source": source
        })).send().await.unwrap()).await;
        let action = imported["view"]["dataflow"]["actions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|action| action["callee"]["effect"] == "corpus.echo")
            .unwrap();
        let operation = json!({"set_value": {"base": imported["identity"],
            "target": {"tool_argument": {"action": action["site"], "index": 0}},
            "value": {"literal": {"text": "edited λ"}}}});
        let preview = answer(
            client
                .post(format!("{base}/workflow/plan"))
                .json(&json!({
                    "version": imported["version"], "operations": [operation.clone()]
                }))
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_ne!(preview["preview"]["identity"], imported["identity"]);
        assert!(preview["preview"]["authored"]["sourceMapId"].is_null());
        let edited = answer(
            client
                .post(format!("{base}/workflow/edits"))
                .json(&json!({
                    "version": imported["version"], "edits": [operation]
                }))
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(
            edited["workflow"]["document"], preview["preview"]["document"],
            "preview and exact replay agree"
        );
        assert!(edited["workflow"]["authored"]["sourceMapId"].is_null());
        assert!(edited["workflow"]["authored"]["sourceMap"].is_null());
        assert_eq!(edited["correspondence"]["base"], imported["identity"]);
        let events = run(&client, &base, json!({})).await;
        let last = events.last().unwrap();
        assert_eq!(last.status, RunStatus::Succeeded);
        assert_eq!(last.output, Some(json!("edited λ")));
        assert_eq!(last.display.messages, ["edited λ"]);
    }
}

/// EXAMPLE-CORPUS / STORED-VIEW / HELP-LABEL: the picker follows the shared
/// catalog, twins keep their own results, and drawing consumes stored declarations.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corpus_catalog_imports_both_dialects_and_draws_the_stored_read() {
    let (base, core) = serve().await;
    let client = reqwest::Client::new();
    let catalog = answer(
        client
            .get(format!("{base}/workflows"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    let manifest: Value = serde_json::from_str(workflow_corpus::CATALOG_JSON).unwrap();
    assert_eq!(
        catalog.as_array().unwrap().len(),
        manifest.as_array().unwrap().len()
    );
    let before = answer(client.get(format!("{base}/workflow")).send().await.unwrap()).await;
    let refused = client
        .post(format!("{base}/workflow/select"))
        .json(&json!({"id":"straight-tool", "entry":"missing"}))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    let unchanged = answer(client.get(format!("{base}/workflow")).send().await.unwrap()).await;
    assert_eq!(unchanged["version"], before["version"]);
    let mut modules = 0;
    let mut refusals = 0;
    let mut cells = 0;
    for (case, declared) in catalog
        .as_array()
        .unwrap()
        .iter()
        .zip(manifest.as_array().unwrap())
    {
        assert_eq!(case["id"], declared["name"]);
        for dialect in ["typescript", "python"] {
            let source = &case[format!("{dialect}Source")];
            let before = answer(client.get(format!("{base}/workflow")).send().await.unwrap()).await;
            match case["group"].as_str().unwrap() {
                "cell" => {
                    assert!(
                        case[format!("{dialect}Witness")]
                            .as_str()
                            .is_some_and(
                                |text| text.contains("FunctionValue") || text.contains("finish")
                            )
                    );
                    cells += 1;
                }
                "refusal" => {
                    let response = client.post(format!("{base}/workflow/source")).json(&json!({
                        "version": before["version"], "entry": "demo", "dialect": dialect, "source": source
                    })).send().await.unwrap();
                    assert_eq!(response.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
                    let error: Value = response.json().await.unwrap();
                    assert_eq!(
                        error["error"]["details"]["diagnostics"][0]["code"],
                        declared["refusal"]
                    );
                    let after =
                        answer(client.get(format!("{base}/workflow")).send().await.unwrap()).await;
                    assert_eq!(after["identity"], before["identity"]);
                    assert_eq!(after["version"], before["version"]);
                    refusals += 1;
                }
                "module" => {
                    for entry in declared["entries"].as_array().unwrap() {
                        let opened = answer(
                            client
                                .post(format!("{base}/workflow/select"))
                                .json(&json!({
                                    "id": case["id"], "dialect": dialect, "entry": entry
                                }))
                                .send()
                                .await
                                .unwrap(),
                        )
                        .await;
                        assert_eq!(opened["authored"]["text"], *source);
                        assert_eq!(opened["sourceDialect"], dialect);
                        let definition = lash::process::ProcessDefinitionId::parse(
                            opened["definition"].as_str().unwrap(),
                        )
                        .unwrap();
                        let lash::workflow::WorkflowRead::Inspected(stored) = core
                            .host_artifacts()
                            .definition_graph(&definition)
                            .await
                            .unwrap()
                        else {
                            panic!("stored definition");
                        };
                        let mut expected = lash::workflow::view::derive(&stored.document).unwrap();
                        let map = serde_json::from_value(opened["sourceMap"].clone()).unwrap();
                        expected
                            .readable_ranges(
                                stored.document.document(),
                                if dialect == "python" {
                                    "generated_python"
                                } else {
                                    "generated_typescript"
                                }
                                .into(),
                                opened["source"].as_str().unwrap(),
                                &map,
                            )
                            .unwrap();
                        assert_eq!(serde_json::to_value(expected).unwrap(), opened["view"]);
                        for action in opened["view"]["dataflow"]["actions"].as_array().unwrap() {
                            if let Some(name) = action["callee"]["library"]["name"].as_str() {
                                assert!(
                                    opened["functionLabels"][name]
                                        .as_str()
                                        .is_some_and(|title| !title.is_empty())
                                );
                            }
                        }
                        modules += 1;
                    }
                }
                other => panic!("unknown corpus group {other}"),
            }
        }
    }
    println!(
        "EXAMPLE-CORPUS observed {modules} module entries, {refusals} refused imports, {cells} cell witnesses"
    );
}
