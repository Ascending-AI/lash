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
    assert_eq!(document.source, "process blank() {\n  finish 0\n}\n");
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
    assert_eq!(document.roots.main, ["new:canvas-progress"]);

    document.roots.main.clear();
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
        .find("finish \"done\"")
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
    let final_event = events.last().expect("terminal event");
    assert_eq!(final_event.status, RunStatus::Succeeded);
    assert_eq!(final_event.node_id, saved.id_map[terminal_id.as_str()]);
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
            title: "finish".to_string()
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

    assert!(
        saved.document.source.contains(
            "@label(title: \"Hand back the result\", description: \"What the operator sees when the run ends\")"
        ),
        "authored title missing from rendered source: {}",
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
    assert!(!saved.document.source.contains("@label"));
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
            title: "finish".to_string()
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
    node
}

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
