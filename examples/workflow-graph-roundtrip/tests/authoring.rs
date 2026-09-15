use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde_json::Value;
use workflow_graph_roundtrip::{
    AppState, EditableValue, FlowNode, NodeData, NodeName, RunEvent, RunStatus, RunTiming,
    SaveWorkflowResponse, WorkflowDocument,
};

#[tokio::test]
async fn blank_workflow_full_authoring_round_trip_rejects_malformed_then_runs() {
    let state = AppState::with_run_timing(RunTiming {
        sleep_cap: Duration::from_millis(2),
        signal_delay: Duration::from_millis(2),
    })
    .expect("default workflow");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("test listener address");
    let server = tokio::spawn(workflow_graph_roundtrip::serve(listener, state));
    let client = reqwest::Client::new();
    let base = format!("http://{addr}");

    let operations: Vec<Value> = client
        .get(format!("{base}/operations"))
        .send()
        .await
        .expect("GET /operations")
        .json()
        .await
        .expect("operation catalog JSON");
    let operation = |id: &str| {
        operations
            .iter()
            .find(|entry| entry["id"] == id)
            .unwrap_or_else(|| panic!("catalog entry {id}"))
    };

    let mut document = select_workflow(&client, &base, "blank").await;
    assert_eq!(
        document.source,
        "const blank = async () => {\n  return 0;\n};\n"
    );
    let baseline_version = document.version;
    let baseline_source = document.source.clone();

    // The Steps rail adds a catalog-shaped literal action directly to the
    // process body.
    let mut message = catalog_node(operation("display.show_message"), "new:steps-message");
    message.data.fields.insert(
        "text".to_string(),
        EditableValue::String("Built from blank".to_string()),
    );
    append_process_node(&mut document, message);

    // The Canvas top-level palette initially adds to `main`; moving the node
    // into the process and then reordering it models the editor journey.
    let mut progress = catalog_node(operation("display.set_progress"), "new:canvas-progress");
    progress
        .data
        .fields
        .insert("pct".to_string(), EditableValue::Number(73.0));
    document.roots.main.push(progress.id.clone());
    document.nodes.push(progress);
    // The module body also holds the `const blank = async (..) => ..`
    // binding, so the palette's node is the one the host just appended.
    assert_eq!(
        document.roots.main.last().map(String::as_str),
        Some("new:canvas-progress")
    );

    document.roots.main.retain(|id| id != "new:canvas-progress");
    let process_index = document
        .nodes
        .iter()
        .position(|node| node.node_type == "process")
        .expect("blank process");
    let process_id = document.nodes[process_index].id.clone();
    let body = document.nodes[process_index]
        .data
        .children
        .iter_mut()
        .find(|child| child.slot == "body")
        .expect("blank process body");
    let message_index = body
        .node_ids
        .iter()
        .position(|id| id == "new:steps-message")
        .expect("Steps message in process body");
    body.node_ids
        .insert(message_index, "new:canvas-progress".to_string());
    document
        .nodes
        .iter_mut()
        .find(|node| node.id == "new:canvas-progress")
        .expect("Canvas progress node")
        .parent_id = Some(process_id);

    // A malformed expression must be a typed rejection and must not advance
    // the saved version or discard the complete authored draft.
    let terminal = document
        .nodes
        .iter_mut()
        .find(|node| node.data.terminal_kind.as_deref() == Some("finish"))
        .expect("blank terminal");
    terminal.data.expression = Some("1 +".to_string());
    let terminal_id = terminal.id.clone();

    let response = client
        .post(format!("{base}/workflow"))
        .json(&document)
        .send()
        .await
        .expect("reject malformed authored workflow");
    assert_eq!(response.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    let error: Value = response.json().await.expect("typed render error");
    assert_eq!(error["error"]["code"], "invalid_expression");
    assert_eq!(error["error"]["details"]["nodeId"], terminal_id);
    assert_eq!(error["error"]["details"]["field"], "expression");

    let still_blank: WorkflowDocument = client
        .get(format!("{base}/workflow"))
        .send()
        .await
        .expect("GET workflow after rejected save")
        .json()
        .await
        .expect("saved workflow after rejected save");
    assert_eq!(still_blank.version, baseline_version);
    assert_eq!(still_blank.source, baseline_source);

    document
        .nodes
        .iter_mut()
        .find(|node| node.id == terminal_id)
        .expect("draft terminal retained")
        .data
        .expression = Some("\"done\"".to_string());
    let posted_ids = document
        .nodes
        .iter()
        .map(|node| node.id.clone())
        .collect::<BTreeSet<_>>();

    let response = client
        .post(format!("{base}/workflow"))
        .json(&document)
        .send()
        .await
        .expect("save authored blank workflow");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let saved: SaveWorkflowResponse = response.json().await.expect("saved workflow");
    assert_eq!(saved.document.version, baseline_version + 1);
    assert_eq!(saved.id_map.len(), posted_ids.len());
    assert!(posted_ids.iter().all(|posted_id| {
        saved
            .id_map
            .get(posted_id)
            .is_some_and(|new_id| saved.document.nodes.iter().any(|node| &node.id == new_id))
    }));
    for new_id in ["new:steps-message", "new:canvas-progress"] {
        assert_ne!(saved.id_map[new_id], new_id);
    }

    let progress_source_index = saved
        .document
        .source
        .find("display.set_progress({ pct: 73")
        .expect("canonical progress source");
    let message_source_index = saved
        .document
        .source
        .find("display.show_message({ text: \"Built from blank\" })")
        .expect("canonical message source");
    let terminal_source_index = saved
        .document
        .source
        .find("return \"done\";")
        .expect("canonical terminal source");
    assert!(progress_source_index < message_source_index);
    assert!(message_source_index < terminal_source_index);

    let saved_json = serde_json::to_value(&saved.document).expect("saved document JSON");
    let fetched_json: Value = client
        .get(format!("{base}/workflow"))
        .send()
        .await
        .expect("GET saved workflow")
        .json()
        .await
        .expect("saved workflow JSON");
    assert_eq!(fetched_json, saved_json);

    let projected: Value = client
        .post(format!("{base}/project"))
        .json(&serde_json::json!({ "source": saved.document.source }))
        .send()
        .await
        .expect("reproject saved source")
        .json()
        .await
        .expect("projected workflow JSON");
    assert_eq!(projected["document"]["source"], saved_json["source"]);
    assert_eq!(
        projected["document"]["nodes"]
            .as_array()
            .expect("projected nodes")
            .len(),
        saved.document.nodes.len()
    );

    let events = run_workflow(&client, &base).await;
    assert!(!events.is_empty());
    assert!(
        events
            .iter()
            .all(|event| event.workflow_version == saved.document.version)
    );
    assert!(!events.iter().any(|event| event.status == RunStatus::Failed));
    // FIG-3057: the process's closing `return` correlates no execution site
    // yet, so the last event is the last authored statement rather than the
    // terminal node; the authored terminal still reprojects and the run still
    // ends successfully.
    let final_event = events.last().expect("terminal event");
    assert_eq!(final_event.status, RunStatus::Succeeded);
    assert!(
        saved
            .document
            .nodes
            .iter()
            .any(|node| node.id == final_event.node_id)
    );
    assert_eq!(final_event.display.progress, 73.0);
    assert!(
        final_event
            .display
            .messages
            .iter()
            .any(|message| message == "Built from blank")
    );

    server.abort();
}

/// Renaming a node is an authoring act: the new title has to come back out of
/// the save path, and the only thing that decides whether it does is the tag
/// the client sends with it.
#[tokio::test]
async fn renaming_a_node_keeps_the_authored_title_through_save_and_reprojection() {
    let state = AppState::with_run_timing(RunTiming {
        sleep_cap: Duration::from_millis(2),
        signal_delay: Duration::from_millis(2),
    })
    .expect("default workflow");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("test listener address");
    let server = tokio::spawn(workflow_graph_roundtrip::serve(listener, state));
    let client = reqwest::Client::new();
    let base = format!("http://{addr}");

    let mut document = select_workflow(&client, &base, "blank").await;
    let terminal = document
        .nodes
        .iter_mut()
        .find(|node| node.data.terminal_kind.as_deref() == Some("finish"))
        .expect("blank terminal");
    assert_eq!(
        terminal.data.name,
        NodeName::Derived {
            title: "return".to_string()
        }
    );
    terminal.data.name = NodeName::Authored {
        title: "Hand back the result".to_string(),
        description: Some("What the operator sees when the run ends".to_string()),
    };

    let response = client
        .post(format!("{base}/workflow"))
        .json(&document)
        .send()
        .await
        .expect("save renamed terminal");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let saved: SaveWorkflowResponse = response.json().await.expect("saved workflow");

    // FIG-3047: an authored title is written back into the source as the
    // `@label` doc comment on the statement it names, so it survives the save
    // and comes back out of the reprojection rather than being recomputed.
    assert!(
        saved.document.source.contains(
            "/** @label Hand back the result — What the operator sees when the run ends */"
        ),
        "saved source:\n{}",
        saved.document.source
    );
    let saved_terminal = saved
        .document
        .nodes
        .iter()
        .find(|node| node.data.terminal_kind.as_deref() == Some("finish"))
        .expect("renamed terminal reprojected");
    assert_eq!(
        saved_terminal.data.name,
        NodeName::Authored {
            title: "Hand back the result".to_string(),
            description: Some("What the operator sees when the run ends".to_string()),
        }
    );

    // The control: the same title tagged as derived is a rendering, so it is
    // recomputed rather than written into the source.
    let mut document = saved.document;
    document
        .nodes
        .iter_mut()
        .find(|node| node.data.terminal_kind.as_deref() == Some("finish"))
        .expect("renamed terminal")
        .data
        .name = NodeName::Derived {
        title: "Hand back the result".to_string(),
    };
    let response = client
        .post(format!("{base}/workflow"))
        .json(&document)
        .send()
        .await
        .expect("save derived terminal");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let saved: SaveWorkflowResponse = response.json().await.expect("saved workflow");
    assert!(
        !saved.document.source.contains("@label"),
        "saved source:\n{}",
        saved.document.source
    );
    assert_eq!(
        saved
            .document
            .nodes
            .iter()
            .find(|node| node.data.terminal_kind.as_deref() == Some("finish"))
            .expect("terminal reprojected")
            .data
            .name,
        NodeName::Derived {
            title: "return".to_string()
        }
    );

    // A payload that carries a title without saying where the name came from
    // is refused outright: the host has no tolerant fallback to guess with.
    let mut untagged = serde_json::to_value(&saved.document).expect("saved document JSON");
    let node = untagged["nodes"]
        .as_array_mut()
        .expect("document nodes")
        .iter_mut()
        .find(|node| node["data"]["terminalKind"] == "finish")
        .expect("terminal node JSON");
    node["data"]["title"] = Value::String("Hand back the result".to_string());
    node["data"]
        .as_object_mut()
        .expect("node data object")
        .remove("nameSource");
    let response = client
        .post(format!("{base}/workflow"))
        .json(&untagged)
        .send()
        .await
        .expect("post untagged title");
    assert!(
        response.status().is_client_error(),
        "untagged title was accepted with {}",
        response.status()
    );

    server.abort();
}

/// FIG-3177: the editor posts a synthesized receiver call for every action it
/// inserts, and the `await` in it decides whether the workflow saves at all.
/// Without it the fragment lowers to a pending-tool value, the node has no
/// receiver operation, and the save the author just made is refused whole.
#[tokio::test]
async fn an_authored_action_saves_only_with_the_awaited_receiver_call_the_editor_emits() {
    let state = AppState::with_run_timing(RunTiming {
        sleep_cap: Duration::from_millis(2),
        signal_delay: Duration::from_millis(2),
    })
    .expect("default workflow");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("test listener address");
    let server = tokio::spawn(workflow_graph_roundtrip::serve(listener, state));
    let client = reqwest::Client::new();
    let base = format!("http://{addr}");

    let operations: Vec<Value> = client
        .get(format!("{base}/operations"))
        .send()
        .await
        .expect("GET /operations")
        .json()
        .await
        .expect("operation catalog JSON");
    let entry = operations
        .iter()
        .find(|entry| entry["id"] == "display.show_message")
        .expect("catalog entry display.show_message");

    // The literal the browser puts in `data.expression` for a zero-edit
    // palette insertion of Show message, pinned so the helper above cannot
    // drift away from `synthCallExpression` unnoticed.
    let authored = synth_call_expression(entry);
    assert_eq!(authored, r#"await display.show_message({ text: "" })"#);

    let baseline = select_workflow(&client, &base, "blank").await;

    // The unawaited form the editor used to emit is refused, and the refusal
    // names the node that carries it.
    let mut rejected = baseline.clone();
    let mut bare = catalog_node(entry, "new:bare-call");
    bare.data.expression = Some(authored.replace("await ", ""));
    append_process_node(&mut rejected, bare);
    let response = client
        .post(format!("{base}/workflow"))
        .json(&rejected)
        .send()
        .await
        .expect("post unawaited call node");
    assert_eq!(response.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    let error: Value = response.json().await.expect("typed render error");
    assert_eq!(error["error"]["code"], "invalid_expression");
    assert_eq!(error["error"]["details"]["nodeId"], "new:bare-call");
    assert_eq!(error["error"]["details"]["field"], "expression");

    let unchanged: WorkflowDocument = client
        .get(format!("{base}/workflow"))
        .send()
        .await
        .expect("GET workflow after rejected save")
        .json()
        .await
        .expect("saved workflow after rejected save");
    assert_eq!(unchanged.version, baseline.version);
    assert_eq!(unchanged.source, baseline.source);

    // The same node, byte-identical but for the `await`, saves and renders the
    // authored call into the canonical source.
    let mut accepted = baseline;
    let mut awaited = catalog_node(entry, "new:awaited-call");
    awaited.data.fields.insert(
        "text".to_string(),
        EditableValue::String("Authored".to_string()),
    );
    assert_eq!(awaited.data.expression.as_deref(), Some(authored.as_str()));
    append_process_node(&mut accepted, awaited);
    let response = client
        .post(format!("{base}/workflow"))
        .json(&accepted)
        .send()
        .await
        .expect("post awaited call node");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let saved: SaveWorkflowResponse = response.json().await.expect("saved workflow");
    assert!(
        saved
            .document
            .source
            .contains(r#"display.show_message({ text: "Authored" })"#),
        "authored call missing from canonical source: {}",
        saved.document.source
    );

    server.abort();
}

/// FIG-3178: a call node the editor inserts from a non-display catalog entry
/// must name that entry's receiver. Synthesizing `display.<operation>` for a
/// `gmail` or `llm` operation hands the lowerer a receiver that has no such
/// operation, and the `$expr` defaults those entries carry used to stringify
/// into `[object Object]` on the way into the argument record.
#[tokio::test]
async fn a_non_display_action_saves_against_its_own_receiver_and_expression_defaults() {
    let state = AppState::with_run_timing(RunTiming {
        sleep_cap: Duration::from_millis(2),
        signal_delay: Duration::from_millis(2),
    })
    .expect("default workflow");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("test listener address");
    let server = tokio::spawn(workflow_graph_roundtrip::serve(listener, state));
    let client = reqwest::Client::new();
    let base = format!("http://{addr}");

    let operations: Vec<Value> = client
        .get(format!("{base}/operations"))
        .send()
        .await
        .expect("GET /operations")
        .json()
        .await
        .expect("operation catalog JSON");
    // An entry whose defaults include `$expr` values: the synthesized call
    // names `llm`, and each expression default is its own raw source rather
    // than a stringified `{"$expr": ...}` object.
    let expression_defaults = operations
        .iter()
        .find(|entry| entry["id"] == "llm.query")
        .expect("catalog entry llm.query");
    assert_eq!(expression_defaults["receiver"], "llm");
    assert_eq!(
        synth_call_expression(expression_defaults),
        r#"await llm.query({ task: "Summarize the supplied input", inputs: {}, output: Type { result: str } })"#
    );

    // An entry the editor can actually insert and save. Posted with no
    // `expression`, so the save exercises the backend's own synthesis, which
    // has to reach the same receiver call the editor would have sent.
    let entry = operations
        .iter()
        .find(|entry| entry["id"] == "gmail.list_recent")
        .expect("catalog entry gmail.list_recent");
    assert_eq!(entry["receiver"], "gmail");
    assert_eq!(
        synth_call_expression(entry),
        "await gmail.list_recent({ count: 5 })"
    );

    let baseline = select_workflow(&client, &base, "blank").await;
    let mut document = baseline;
    let mut node = catalog_node(entry, "new:gmail-list-recent");
    node.data.expression = None;
    assert_eq!(node.data.receiver.as_deref(), Some("gmail"));
    append_process_node(&mut document, node);
    let response = client
        .post(format!("{base}/workflow"))
        .json(&document)
        .send()
        .await
        .expect("post receiverless call node");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let saved: SaveWorkflowResponse = response.json().await.expect("saved workflow");
    assert!(
        saved
            .document
            .source
            .contains("gmail.list_recent({ count: 5 })"),
        "synthesized call missing from canonical source: {}",
        saved.document.source
    );
    assert!(
        !saved.document.source.contains("display.list_recent"),
        "synthesized call named the wrong receiver: {}",
        saved.document.source
    );

    // Lowering resolved the receiver operation: the projected node the editor
    // reads back names `list_recent`, which it can only do if the synthesized
    // fragment parsed as a receiver call rather than as some other value.
    let projected = saved
        .document
        .nodes
        .iter()
        .find(|candidate| candidate.data.operation.as_deref() == Some("list_recent"))
        .expect("projected gmail.list_recent call node");
    assert_eq!(projected.data.kind, "call");

    server.abort();
}

/// Test-support helper outside `#[test]`, so clippy.toml's allow-in-tests does not reach it.
#[expect(
    clippy::expect_used,
    reason = "the call sites select a workflow id the served catalog produced, so the HTTP \
              round-trip succeeds"
)]
async fn select_workflow(client: &reqwest::Client, base: &str, id: &str) -> WorkflowDocument {
    let response = client
        .post(format!("{base}/workflow/select"))
        .json(&serde_json::json!({ "id": id }))
        .send()
        .await
        .expect("POST /workflow/select");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    response.json().await.expect("selected workflow document")
}

/// Test-support helper outside `#[test]`, so clippy.toml's allow-in-tests does not reach it.
#[expect(
    clippy::expect_used,
    reason = "POST /run and its SSE body exist for every served workflow, and each `data: ` \
              line is a RunEvent the server serialized"
)]
async fn run_workflow(client: &reqwest::Client, base: &str) -> Vec<RunEvent> {
    let response = client
        .post(format!("{base}/run"))
        .send()
        .await
        .expect("POST /run");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let stream = response.text().await.expect("complete SSE stream");
    stream
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str::<RunEvent>(data).expect("run event JSON"))
        .collect()
}

#[expect(
    clippy::expect_used,
    reason = "catalog entries always carry string nodeKind/label and an array of string-named \
              fields (see the catalog module the server serves)"
)]
fn catalog_node(entry: &Value, id: &str) -> FlowNode {
    let text = |key: &str| entry[key].as_str();
    let kind = text("nodeKind").expect("catalog nodeKind");
    let mut node = FlowNode {
        id: id.to_string(),
        node_type: kind.to_string(),
        parent_id: None,
        data: NodeData {
            kind: kind.to_string(),
            subkind: text("subkind").map(str::to_string),
            name: NodeName::Derived {
                title: text("label").expect("catalog label").to_string(),
            },
            process_name: None,
            params: Vec::new(),
            signals: Vec::new(),
            operation: None,
            receiver: text("receiver").map(str::to_string),
            effect: None,
            terminal_kind: None,
            fields: BTreeMap::new(),
            binding: None,
            target: None,
            expression: None,
            condition: None,
            iterable: None,
            clauses: Vec::new(),
            source: None,
            children: Vec::new(),
            available_vars: Vec::new(),
            expected_arg_types: Vec::new(),
            diagnostics: Vec::new(),
        },
    };
    node.data.operation = text("operation").map(str::to_string);
    node.data.effect = text("effect").map(str::to_string);
    node.data.terminal_kind = text("terminalKind").map(str::to_string);
    for field in entry["fields"].as_array().expect("catalog fields") {
        let name = field["name"].as_str().expect("catalog field name");
        node.data.fields.insert(
            name.to_string(),
            serde_json::from_value(field["default"].clone())
                .unwrap_or_else(|error| panic!("catalog field {name} default: {error}")),
        );
    }
    // A palette insertion is not a bare `operation`: the editor seeds the
    // receiver call it will post in `data.expression`, so these tests post it
    // too (FIG-3177). Leaving it `None` exercised the backend's own fallback
    // and hid the frontend's unawaited call from every save test.
    if kind == "call" {
        node.data.expression = Some(synth_call_expression(entry));
    }
    node
}

/// The exact string the frontend's `synthCallExpression`
/// (`frontend/src/lib/graph.js`) seeds into a palette-inserted call node, built
/// from the same catalog entry the browser reads from `GET /operations`.
///
/// The `await` is the whole point: an unawaited tool call lowers to a
/// pending-tool value with no receiver operation, so the backend cannot resolve
/// the node's operation and refuses the entire save.
#[expect(
    clippy::expect_used,
    reason = "catalog fields always carry a string name and a string type (see the catalog               module the server serves)"
)]
fn synth_call_expression(entry: &Value) -> String {
    let args = entry["fields"]
        .as_array()
        .expect("catalog fields")
        .iter()
        .map(|field| {
            let name = field["name"].as_str().expect("catalog field name");
            let default = &field["default"];
            // `recordArg` in the same frontend module: numbers and booleans
            // render bare, strings render JSON-quoted, and anything else is
            // raw slot text. Every `call` entry the display catalog serves
            // carries string and number fields only.
            let value = match field["type"].as_str().expect("catalog field type") {
                "number" => default.as_f64().unwrap_or(0.0).to_string(),
                "boolean" => default.as_bool().unwrap_or(false).to_string(),
                "string" => serde_json::to_string(default.as_str().unwrap_or_default())
                    .expect("JSON string literal"),
                _ => default_source(default),
            };
            format!("{name}: {value}")
        })
        .collect::<Vec<_>>()
        .join(", ");
    let operation = entry["operation"].as_str().expect("catalog operation");
    let receiver = entry["receiver"].as_str().expect("catalog receiver");
    format!("await {receiver}.{operation}({{ {args} }})")
}

/// `defaultSource` in `frontend/src/lib/operations.js`: an expression-valued
/// catalog default arrives as `{"$expr": "<source>"}` and is emitted as that
/// raw source, not as the JSON object stringified (FIG-3178).
fn default_source(default: &Value) -> String {
    default["$expr"]
        .as_str()
        .or_else(|| default.as_str())
        .unwrap_or_default()
        .to_string()
}

#[expect(
    clippy::expect_used,
    reason = "the blank workflow template always contains a process container with a body slot"
)]
fn append_process_node(document: &mut WorkflowDocument, mut node: FlowNode) {
    let process_index = document
        .nodes
        .iter()
        .position(|candidate| candidate.node_type == "process")
        .expect("process container");
    let process_id = document.nodes[process_index].id.clone();
    let terminal_id = document
        .nodes
        .iter()
        .find_map(|candidate| (candidate.node_type == "terminal").then(|| candidate.id.clone()));
    let body = document.nodes[process_index]
        .data
        .children
        .iter_mut()
        .find(|child| child.slot == "body")
        .expect("process body");
    let insert_at = terminal_id
        .as_ref()
        .and_then(|terminal_id| {
            body.node_ids
                .iter()
                .position(|node_id| node_id == terminal_id)
        })
        .unwrap_or(body.node_ids.len());
    body.node_ids.insert(insert_at, node.id.clone());
    node.parent_id = Some(process_id);
    document.nodes.push(node);
}
