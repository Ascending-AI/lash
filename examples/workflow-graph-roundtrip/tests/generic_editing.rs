//! FIG-5577: the host opens a generated workflow that holds a `try`/`catch`
//! and a closure, edits inside the `try` region through the generic
//! structured editor, publishes, runs and reads the execution overlay. No
//! request carries source and no step converts to or from TypeScript.

#[path = "support/runtime.rs"]
mod runtime;

use lash::vm::testing::ast_builders as b;
use serde_json::{Value, json};
use workflow_graph_roundtrip::{RunStatus, SaveWorkflowResponse, WorkflowDocument};

/// `guarded()`: a closure, then a `try` whose body waits and calls the
/// closure, with a `catch`. Built as IR; no source exists for it.
fn generated() -> lash::workflow::WorkflowGraph {
    lash::vm::ir::workflow_graph_from_program(&b::module(
        vec![b::process(
            "guarded",
            Vec::new(),
            b::block(vec![
                b::assign(
                    "shout",
                    b::closure(
                        None,
                        &["text"],
                        &[],
                        b::concat(b::var("text"), b::string("!")),
                    ),
                ),
                b::assign("out", b::string("start")),
                b::try_expr(
                    b::block(vec![
                        b::sleep_for(b::string("10ms")),
                        b::assign("out", b::call(b::var("shout"), vec![b::string("guarded")])),
                    ]),
                    Some(b::catch(
                        "error",
                        b::block(vec![b::assign("out", b::string("caught"))]),
                    )),
                    None,
                ),
                b::finish(b::var("out")),
            ]),
        )],
        Vec::new(),
    ))
}

#[expect(
    clippy::expect_used,
    reason = "test helper: IR always serializes, and a failure should abort the test"
)]
fn ir(expression: &lash::vm::ir::Expr) -> Value {
    serde_json::to_value(expression).expect("IR serializes")
}

/// The slot of `node` whose expression is `expression`.
#[expect(
    clippy::expect_used,
    reason = "test helper: a node without the slot should abort the test"
)]
fn slot_of(workflow: &Value, node: &str, expression: &Value) -> Value {
    workflow["nodes"][node]["slots"]
        .as_array()
        .expect("the node lists its slots")
        .iter()
        .find(|slot| &slot["expression"] == expression)
        .unwrap_or_else(|| panic!("node `{node}` has a slot holding {expression}"))["path"]
        .clone()
}

#[tokio::test]
async fn a_generated_workflow_is_edited_inside_its_try_region_published_and_run() {
    let (state, core) = runtime::state_and_core().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let base = format!("http://{}", listener.local_addr().expect("address"));
    let server = tokio::spawn(workflow_graph_roundtrip::serve(listener, state));
    let client = reqwest::Client::new();

    // Open: the typed document is the whole request.
    let opened: WorkflowDocument = client
        .post(format!("{base}/workflow/ir"))
        .json(&json!({ "graph": generated() }))
        .send()
        .await
        .expect("open")
        .json()
        .await
        .expect("the opened document");
    assert!(
        opened.not_admitted.is_none(),
        "the generated workflow is admitted: {:?}",
        opened.not_admitted
    );
    let region = opened
        .nodes
        .iter()
        .find(|node| node.data.subkind() == Some("try"))
        .expect("the try region is a container of the document");
    let body = &region
        .data
        .children()
        .iter()
        .find(|group| group.slot == "body")
        .expect("the try body")
        .node_ids;
    let [wait, call] = body.as_slice() else {
        panic!("the try body holds its two statements: {body:?}");
    };
    assert!(
        region
            .data
            .children()
            .iter()
            .any(|group| group.slot == "catch" && group.node_ids.len() == 1),
        "the catch clause is a child body of the region"
    );

    let workflow: Value = client
        .get(format!("{base}/workflow/ir"))
        .send()
        .await
        .expect("read the typed document")
        .json()
        .await
        .expect("the typed document");
    let closure = opened
        .nodes
        .iter()
        .find(|node| {
            workflow["nodes"][&node.id]["slots"]
                .as_array()
                .is_some_and(|slots| slots.iter().any(|slot| slot["variant"] == "Function"))
        })
        .expect("the closure is reachable as a slot of its statement");

    // Edit inside the try region, and inside the closure, by slot path.
    let edited: SaveWorkflowResponse = client
        .post(format!("{base}/workflow/edits"))
        .json(&json!({
            "version": opened.version,
            "edits": [
                {
                    "op": "replaceExpression",
                    "node": wait,
                    "slot": slot_of(&workflow, wait, &ir(&b::string("10ms"))),
                    "expression": ir(&b::string("20ms")),
                },
                {
                    "op": "replaceExpression",
                    "node": call,
                    "slot": slot_of(&workflow, call, &ir(&b::string("guarded"))),
                    "expression": ir(&b::string("edited")),
                },
                {
                    "op": "replaceExpression",
                    "node": closure.id,
                    "slot": slot_of(&workflow, &closure.id, &ir(&b::string("!"))),
                    "expression": ir(&b::string("?")),
                },
                {
                    "op": "insertNode",
                    "body": { "kind": "child", "node": region.id, "slot": "body" },
                    "statement": ir(&b::sleep_for(b::string("5ms"))),
                },
            ],
        }))
        .send()
        .await
        .expect("edit")
        .json()
        .await
        .expect("the edited document");
    assert_eq!(edited.document.version, opened.version + 1);
    assert!(
        edited.document.not_admitted.is_none(),
        "the edited workflow is published: {:?}",
        edited.document.not_admitted
    );
    assert_ne!(
        edited.document.definition, opened.definition,
        "an edit publishes a new definition"
    );
    let wait_now = edited.id_map.get(wait).expect("the wait survives the edit");
    let region_now = edited
        .id_map
        .get(&region.id)
        .expect("the region survives the edit");
    let body_now = &edited
        .document
        .nodes
        .iter()
        .find(|node| &node.id == region_now)
        .expect("the region")
        .data
        .children()
        .iter()
        .find(|group| group.slot == "body")
        .expect("the try body")
        .node_ids;
    assert_eq!(
        body_now.len(),
        3,
        "the inserted statement joined the region"
    );
    let inserted = &body_now[2];

    let workflow: Value = client
        .get(format!("{base}/workflow/ir"))
        .send()
        .await
        .expect("read the edited document")
        .json()
        .await
        .expect("the edited typed document");
    assert_eq!(
        workflow["nodes"][wait_now]["statement"],
        ir(&b::sleep_for(b::string("20ms"))),
        "the statement in the region is the edited IR"
    );

    // Run, and read the overlay of the edited definition.
    let events = runtime::run_workflow(&client, &base).await;
    let last = events.last().expect("the run ends");
    assert_eq!(last.status, RunStatus::Succeeded, "{last:?}");
    assert!(
        events
            .iter()
            .all(|event| Some(&event.definition) == edited.document.definition.as_ref()),
        "the overlay is of the definition the edit published"
    );
    for node in [wait_now, inserted] {
        for status in [RunStatus::Started, RunStatus::Succeeded] {
            assert!(
                events
                    .iter()
                    .any(|event| &event.node_id == node && event.status == status),
                "the overlay shows `{node}` inside the try region {status:?}: {events:#?}"
            );
        }
    }

    // What ran is what was edited: the closure's new body and its new
    // argument reach the process's output.
    let process = events[0]
        .run_id
        .parse::<lash::ProcessId>()
        .expect("the overlay names its process");
    let lash::process::ProcessAwaitOutput::Settled { output } = core
        .processes()
        .await_output(&process)
        .await
        .expect("the output reads")
    else {
        panic!("the process settled");
    };
    assert_eq!(output.value_for_projection(), json!("edited?"));
    server.abort();
}

/// FIG-5645: form saves name their operations, and source import is explicit.
#[tokio::test]
async fn form_saves_apply_operations_and_import_only_when_requested() {
    let (state, core) = runtime::state_and_core().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let base = format!("http://{}", listener.local_addr().expect("address"));
    let server = tokio::spawn(workflow_graph_roundtrip::serve(listener, state));
    let client = reqwest::Client::new();
    let opened: WorkflowDocument = client
        .post(format!("{base}/workflow/ir"))
        .json(&json!({"graph": generated()}))
        .send()
        .await
        .expect("open")
        .json()
        .await
        .expect("document");
    let before: Value = client
        .get(format!("{base}/workflow/ir"))
        .send()
        .await
        .expect("read")
        .json()
        .await
        .expect("IR");
    let terminal = opened
        .nodes
        .iter()
        .find(|node| node.data.kind() == "terminal")
        .expect("terminal");
    let mut data = serde_json::to_value(&terminal.data).expect("form");
    data["expression"] = json!("\"edited by form\"");
    let response = client
        .post(format!("{base}/workflow"))
        .json(&json!({"kind":"edit", "version":opened.version,
            "edits":[{"op":"setForm", "node":terminal.id, "data":data}]}))
        .send()
        .await
        .expect("save");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::OK,
        "explicit operations save: {}",
        response.text().await.expect("body")
    );
    let saved: SaveWorkflowResponse = response.json().await.expect("saved operations");
    let edited = &saved.document;
    assert_eq!(edited.version, opened.version + 1);
    assert!(edited.not_admitted.is_none(), "{:?}", edited.not_admitted);
    let after: Value = client
        .get(format!("{base}/workflow/ir"))
        .send()
        .await
        .expect("read")
        .json()
        .await
        .expect("IR");
    assert!(before["graph"]["declarations"][0].is_object());
    assert_eq!(
        before["graph"]["declarations"][0]["wrapper"],
        after["graph"]["declarations"][0]["wrapper"]
    );
    for node in opened
        .nodes
        .iter()
        .filter(|node| node.id != terminal.id && node.data.kind() != "process")
    {
        let mapped = saved
            .id_map
            .get(&node.id)
            .expect("untouched handle survives");
        assert_eq!(
            before["nodes"][&node.id]["statement"],
            after["nodes"][mapped]["statement"]
        );
    }
    // New form nodes are addressed by aliases, then moved and removed in the
    // same save. The host never reconstructs their enclosing graph.
    let structural = client.post(format!("{base}/workflow"))
        .json(&json!({"kind":"edit", "version":edited.version, "edits":[
            {"op":"insertForm", "id":"new:loop", "body":{"kind":"main"},
                "data":{"kind":"container", "subkind":"while", "title":"Loop", "nameSource":"derived", "condition":"false"}},
            {"op":"insertForm", "id":"new:wait", "body":{"kind":"main"},
                "data":{"kind":"effect", "effect":"sleep_for", "title":"Wait", "nameSource":"derived", "expression":"await sleep(\"1ms\")"}},
            {"op":"moveNode", "node":"new:wait", "body":{"kind":"child", "node":"new:loop", "slot":"body"}},
            {"op":"removeNode", "node":"new:wait"}
        ]})).send().await.expect("structural save");
    assert_eq!(
        structural.status(),
        reqwest::StatusCode::OK,
        "{}",
        structural.text().await.expect("body")
    );
    let structural: SaveWorkflowResponse = structural.json().await.expect("structural document");
    assert!(structural.id_map.contains_key("new:loop"));
    assert!(!structural.id_map.contains_key("new:wait"));
    assert!(structural.document.nodes.iter().any(|node| {
        node.data.subkind() == Some("while")
            && node
                .data
                .children()
                .iter()
                .all(|body| body.node_ids.is_empty())
    }));
    // A late refusal cannot publish an earlier operation of the save.
    let refused = client
        .post(format!("{base}/workflow"))
        .json(
            &json!({"kind":"edit", "version":structural.document.version, "edits":[
                {"op":"removeNode", "node":structural.id_map["new:loop"]},
                {"op":"removeNode", "node":"missing"}
            ]}),
        )
        .send()
        .await
        .expect("refusal");
    assert_eq!(refused.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    let unchanged: WorkflowDocument = client
        .get(format!("{base}/workflow"))
        .send()
        .await
        .expect("read")
        .json()
        .await
        .expect("unchanged document");
    assert_eq!(unchanged.version, structural.document.version);
    let events = runtime::run_workflow(&client, &base).await;
    assert!(events.iter().all(|event| event.status != RunStatus::Failed));
    let imported = client
        .post(format!("{base}/workflow"))
        .json(
            &json!({"kind":"importSource", "version":structural.document.version,
            "source":"const imported = async () => { return 17; };", "edits":[]}),
        )
        .send()
        .await
        .expect("import");
    assert_eq!(imported.status(), reqwest::StatusCode::OK);
    let imported: SaveWorkflowResponse = imported.json().await.expect("imported document");
    assert!(imported.document.source.contains("imported"));
    assert!(
        imported
            .id_map
            .keys()
            .all(|id| !opened.nodes.iter().any(|node| &node.id == id))
    );
    server.abort();
    core.shutdown().await.expect("shutdown");
}
