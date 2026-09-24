#[path = "support/catalog_span_oracles.rs"]
mod catalog_span_oracles;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use catalog_span_oracles::{expected_catalog_node_slices, expected_catalog_process_slice};
use serde_json::Value;
use workflow_graph_roundtrip::{
    AppState, ChildGroup, EditableProcessField, EditableValue, FlowNode, NodeData, NodeName,
    RunEvent, RunStatus, RunTiming, SaveWorkflowResponse, WorkflowCatalogEntry, WorkflowDocument,
};

/// The names a `counter-loop` fragment may reach. The lens parses editable
/// text through the TypeScript front-end, which rejects an unknown binding.
fn workflow_globals() -> std::collections::BTreeSet<String> {
    std::collections::BTreeSet::from(["state".to_string()])
}

/// The canonical TypeScript the lens stores for an editable expression: what
/// the front-end reads back out of `text`, printed by the lens's own printer.
fn canonical_expression(text: &str) -> String {
    let expression = lash::typescript::workflow_graph::parse_typescript_expression(
        text,
        &workflow_globals(),
        &std::collections::BTreeSet::new(),
    )
    .unwrap_or_else(|error| panic!("`{text}` must parse as an editable expression: {error}"));
    lash::typescript::workflow_graph::typescript_expression_source(&expression)
        .unwrap_or_else(|error| panic!("`{text}` must print back as TypeScript: {error}"))
}

#[tokio::test]
async fn operation_catalog_and_fragment_validation_match_the_editor_contract() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("test listener address");
    let server = tokio::spawn(workflow_graph_roundtrip::serve(
        listener,
        AppState::default(),
    ));
    let client = reqwest::Client::new();
    let base = format!("http://{addr}");

    let response = client
        .get(format!("{base}/operations"))
        .send()
        .await
        .expect("GET /operations");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let entries: Value = response.json().await.expect("operation catalog JSON");
    let entries = entries.as_array().expect("operation catalog array");
    assert_eq!(entries.len(), 23);
    assert_eq!(
        entries[0],
        serde_json::json!({
            "id": "display.show_message",
            "label": "Show message",
            "nodeKind": "call",
            "operation": "show_message",
            // FIG-3178: a call entry names the receiver its operation belongs
            // to, so the editor and the backend synthesize the same call
            // instead of both hardcoding `display`.
            "receiver": "display",
            "fields": [{ "name": "text", "type": "string", "default": "" }]
        })
    );
    assert!(entries.iter().any(|entry| {
        entry
            == &serde_json::json!({
                "id": "display.set_progress",
                "label": "Set progress",
                "nodeKind": "call",
                "operation": "set_progress",
                "receiver": "display",
                "fields": [{ "name": "pct", "type": "number", "default": 0 }]
            })
    }));
    for expected in [
        serde_json::json!({
            "id": "proc.process",
            "label": "Process",
            "nodeKind": "process",
            "fields": [
                { "name": "name", "type": "identifier", "default": "my_process" }
            ]
        }),
        serde_json::json!({
            "id": "control.for",
            "label": "For each",
            "nodeKind": "container",
            "subkind": "for",
            "fields": [
                { "name": "binding", "type": "identifier", "default": "item" },
                { "name": "iterable", "type": "expression", "default": "[1, 2, 3]" }
            ]
        }),
        serde_json::json!({
            "id": "stmt.opaque",
            "label": "Raw statement",
            "nodeKind": "opaque",
            "fields": [{
                "name": "source",
                "type": "expression",
                "default": "await display.show_message({ text: \"raw\" })"
            }]
        }),
        serde_json::json!({
            "id": "stmt.finish",
            "label": "Finish",
            "nodeKind": "terminal",
            "terminalKind": "finish",
            "fields": [{ "name": "expression", "type": "expression", "default": "0" }]
        }),
        serde_json::json!({
            "id": "effect.sleep",
            "label": "Sleep",
            "nodeKind": "effect",
            "effect": "sleep",
            "fields": [{ "name": "duration", "type": "expression", "default": "\"1s\"" }]
        }),
    ] {
        assert!(
            entries.contains(&expected),
            "missing catalog entry {expected}"
        );
    }

    for (request, expected) in [
        (
            serde_json::json!({
                "kind": "expression",
                "text": "state.count + 1",
                "availableVars": ["state"]
            }),
            serde_json::json!({ "ok": true }),
        ),
        (
            serde_json::json!({
                "kind": "assignment_target",
                "text": "state.items[0]",
                "availableVars": ["state"]
            }),
            serde_json::json!({ "ok": true }),
        ),
        (
            serde_json::json!({ "kind": "identifier", "text": "item" }),
            serde_json::json!({ "ok": true }),
        ),
    ] {
        let response = client
            .post(format!("{base}/validate"))
            .json(&request)
            .send()
            .await
            .expect("POST /validate valid fragment");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            response.json::<Value>().await.expect("validation JSON"),
            expected
        );
    }

    for (kind, text, code) in [
        ("expression", "1 +", "invalid_expression"),
        (
            "assignment_target",
            "state + count",
            "invalid_assignment_target",
        ),
        ("identifier", "state.count", "invalid_identifier"),
    ] {
        let response = client
            .post(format!("{base}/validate"))
            .json(&serde_json::json!({ "kind": kind, "text": text }))
            .send()
            .await
            .expect("POST /validate invalid fragment");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body: Value = response.json().await.expect("validation JSON");
        assert_eq!(body["ok"], false);
        assert_eq!(body["error"]["code"], code);
        assert!(
            body["error"]["message"]
                .as_str()
                .is_some_and(|m| !m.is_empty())
        );
        assert!(body["error"].get("details").is_none());
    }

    server.abort();
}

#[tokio::test]
async fn catalog_process_shape_adds_a_seeded_top_level_process_that_reprojects_and_runs() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("test listener address");
    let server = tokio::spawn(workflow_graph_roundtrip::serve(
        listener,
        AppState::default(),
    ));
    let client = reqwest::Client::new();
    let base = format!("http://{addr}");
    let entries: Vec<Value> = client
        .get(format!("{base}/operations"))
        .send()
        .await
        .expect("GET /operations")
        .json()
        .await
        .expect("operation catalog JSON");
    let process_entry = entries
        .iter()
        .find(|entry| entry["id"] == "proc.process")
        .expect("process catalog entry");

    let mut document = select_workflow(&client, &base, "blank").await;
    let process = new_flow_node_from_catalog(process_entry, "new:process");
    assert_eq!(process.data.process_name.as_deref(), Some("my_process"));
    document.roots.processes.insert(0, process.id.clone());
    document.nodes.push(process);

    let response = client
        .post(format!("{base}/workflow"))
        .json(&document)
        .send()
        .await
        .expect("save added process");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let saved: SaveWorkflowResponse = response.json().await.expect("saved workflow");
    assert!(saved.id_map.contains_key("new:process"));
    assert_eq!(saved.document.roots.processes.len(), 2);
    assert!(
        saved.document.source.starts_with(
            "const my_process = async () => {\n  return 0;\n};\nconst blank = async ("
        )
    );
    // FIG-3118: the added process renders as `const my_process = async () => ..`
    // and reprojects the way every top-level arrow now does — a statement node
    // in `main` carrying the authored binding, plus a process container whose
    // name is the lens's derived lift identity. `roots.processes` follows the
    // statement order, so the added process is the first container.
    let added = saved
        .document
        .nodes
        .iter()
        .find(|node| node.data.binding.as_deref() == Some("my_process"))
        .expect("reprojected added process binding");
    // In the admitted view the binding holds the reference to its lifted
    // process, which is pure data (FIG-3571).
    assert_eq!(added.data.kind, "data");
    let container_id = &saved.document.roots.processes[0];
    let added = saved
        .document
        .nodes
        .iter()
        .find(|node| &node.id == container_id)
        .expect("reprojected added process container");
    assert_eq!(added.data.kind, "process");
    assert!(
        added
            .data
            .process_name
            .as_deref()
            .is_some_and(|name| name.starts_with(lash::rlm::lang::LIFTED_PROCESS_NAME_PREFIX))
    );
    assert!(added.data.params.is_empty());
    assert!(added.data.signals.is_empty());
    let body = added
        .data
        .children
        .iter()
        .find(|child| child.slot == "body")
        .expect("seeded process body");
    assert_eq!(body.node_ids.len(), 1);
    assert!(saved.document.nodes.iter().any(|node| {
        node.id == body.node_ids[0]
            && node.data.terminal_kind.as_deref() == Some("finish")
            && node.data.expression.as_deref() == Some("0")
    }));
    let graph =
        lash::typescript::workflow_graph::workflow_graph_from_source(&saved.document.source)
            .expect("added process source reprojects");
    assert_eq!(
        lash::typescript::workflow_graph::workflow_graph_to_source(&graph)
            .expect("added process graph renders"),
        saved.document.source
    );

    // FIG-3057: the seeded process's only statement is the closing `return`,
    // which still sits inside the projected process wrapper and so
    // correlates no execution site. The run still completes without failing.
    let events = run_workflow(&client, &base).await;
    assert!(events.iter().all(|event| event.status != RunStatus::Failed));

    server.abort();
}

#[tokio::test]
async fn process_name_params_and_signals_add_remove_and_round_trip() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("test listener address");
    let server = tokio::spawn(workflow_graph_roundtrip::serve(
        listener,
        AppState::default(),
    ));
    let client = reqwest::Client::new();
    let base = format!("http://{addr}");

    // FIG-3118: a top-level arrow is a lifted process literal, so a process
    // container's signature splits in two. Its parameter list is authored —
    // it is the arrow's own parameter list, and the lens splices it back into
    // the literal. Its name and signal set are not: the name is the lens's
    // derived lift identity (the authored name is the `const` binding on the
    // statement node) and the signals are read back out of the body's
    // `waitSignal` calls. A client that echoes the derived fields back is
    // echoing a projection, and the echo is dropped on save.
    let mut document = select_workflow(&client, &base, "blank").await;
    let process = document
        .nodes
        .iter_mut()
        .find(|node| node.data.kind == "process")
        .expect("blank process");
    let derived_name = process
        .data
        .process_name
        .clone()
        .expect("projected process name");
    assert!(derived_name.starts_with(lash::rlm::lang::LIFTED_PROCESS_NAME_PREFIX));
    process.data.process_name = Some("renamed".to_string());
    process.data.params = vec![
        EditableProcessField {
            name: "input".to_string(),
            field_type: "float".to_string(),
        },
        EditableProcessField {
            name: "enabled".to_string(),
            field_type: "bool".to_string(),
        },
    ];
    process.data.signals = vec![EditableProcessField {
        name: "continue".to_string(),
        field_type: "any".to_string(),
    }];
    let statement = document
        .nodes
        .iter_mut()
        .find(|node| node.data.binding.as_deref() == Some("blank"))
        .expect("the statement that binds the process");
    statement.data.binding = Some("renamed".to_string());

    let response = client
        .post(format!("{base}/workflow"))
        .json(&document)
        .send()
        .await
        .expect("save edited process signature");
    let status = response.status();
    let body = response.bytes().await.expect("process signature response");
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let mut saved: SaveWorkflowResponse = serde_json::from_slice(&body).expect("saved workflow");
    // The parameters and the renamed binding land; the echoed process name
    // does not become a second module declaration.
    assert_eq!(
        saved.document.source,
        "const renamed = async (input: number, enabled: boolean) => {\n  return 0;\n};\n"
    );

    let process = saved
        .document
        .nodes
        .iter_mut()
        .find(|node| node.data.kind == "process")
        .expect("reprojected process");
    assert!(
        process
            .data
            .process_name
            .as_deref()
            .is_some_and(|name| name.starts_with(lash::rlm::lang::LIFTED_PROCESS_NAME_PREFIX)),
        "a lifted process keeps its derived name: {:?}",
        process.data.process_name
    );
    assert!(
        process.data.signals.is_empty(),
        "signals are read out of the body, not carried on the container: {:?}",
        process.data.signals
    );
    process.data.params.pop();

    let response = client
        .post(format!("{base}/workflow"))
        .json(&saved.document)
        .send()
        .await
        .expect("save reduced process signature");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let saved: SaveWorkflowResponse = response.json().await.expect("saved workflow");
    assert_eq!(
        saved.document.source,
        "const renamed = async (input: number) => {\n  return 0;\n};\n"
    );
    let process = saved
        .document
        .nodes
        .iter()
        .find(|node| node.data.kind == "process")
        .expect("reprojected reduced process");
    // FIG-3571: a canonical process arrow prints each parameter's type as
    // the annotation that lowers to it, so an authored parameter type
    // survives the round trip along with the parameter and its position.
    assert_eq!(
        process.data.params,
        [EditableProcessField {
            name: "input".to_string(),
            field_type: "float".to_string(),
        }]
    );
    assert_eq!(process.data.available_vars, ["input"]);
    let graph =
        lash::typescript::workflow_graph::workflow_graph_from_source(&saved.document.source)
            .expect("edited signature source reprojects");
    assert_eq!(
        lash::typescript::workflow_graph::workflow_graph_to_source(&graph)
            .expect("edited signature graph renders"),
        saved.document.source
    );

    server.abort();
}

#[tokio::test]
async fn newly_catalogued_nodes_save_reproject_and_run_from_their_catalog_shapes() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("test listener address");
    let server = tokio::spawn(workflow_graph_roundtrip::serve(
        listener,
        AppState::default(),
    ));
    let client = reqwest::Client::new();
    let base = format!("http://{addr}");
    let entries: Vec<Value> = client
        .get(format!("{base}/operations"))
        .send()
        .await
        .expect("GET /operations")
        .json()
        .await
        .expect("operation catalog JSON");
    let entry = |id: &str| {
        entries
            .iter()
            .find(|entry| entry["id"] == id)
            .unwrap_or_else(|| panic!("catalog entry {id}"))
    };

    let mut document = select_workflow(&client, &base, "blank").await;
    append_process_node(
        &mut document,
        new_flow_node_from_catalog(entry("stmt.opaque"), "new:opaque"),
    );

    let response = client
        .post(format!("{base}/workflow"))
        .json(&document)
        .send()
        .await
        .expect("save newly catalogued nodes");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let saved: SaveWorkflowResponse = response.json().await.expect("saved workflow");
    // FIG-3033: the `control.comprehension` and `stmt.fail` catalog shapes are
    // gone -- TypeScript has no list-comprehension expression and no authorable
    // `fail(...)` terminal, so neither shape can be rendered back to source.
    assert!(saved.id_map.contains_key("new:opaque"));
    assert!(saved.document.nodes.iter().any(|node| {
        node.data.operation.as_deref() == Some("show_message")
            && node
                .data
                .fields
                .get("text")
                .is_some_and(|value| value == &EditableValue::String("raw".to_string()))
    }));
    let graph =
        lash::typescript::workflow_graph::workflow_graph_from_source(&saved.document.source)
            .expect("saved catalog-created workflow reprojects");
    assert_eq!(
        lash::typescript::workflow_graph::workflow_graph_to_source(&graph)
            .expect("reprojected workflow renders"),
        saved.document.source
    );

    // The `fail` terminal that used to end this run is gone with its catalog
    // shape, so the catalogued nodes now run to a successful finish.
    let events = run_workflow(&client, &base).await;
    let last = events.last().expect("terminal run event");
    assert_eq!(last.status, RunStatus::Succeeded);
    assert!(last.display.messages.iter().any(|message| message == "raw"));

    server.abort();
}

#[tokio::test]
async fn source_projection_is_a_stateless_canonical_fixpoint_with_typed_errors() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("test listener address");
    let server = tokio::spawn(workflow_graph_roundtrip::serve(
        listener,
        AppState::default(),
    ));
    let client = reqwest::Client::new();
    let base = format!("http://{addr}");
    let before: WorkflowDocument = client
        .get(format!("{base}/workflow"))
        .send()
        .await
        .expect("GET /workflow before projection")
        .json()
        .await
        .expect("workflow before projection");

    let response = client
        .post(format!("{base}/project"))
        .json(&serde_json::json!({
            "source": "const drafted = async (input) => { let value = input; return value; };"
        }))
        .send()
        .await
        .expect("POST /project valid source");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.expect("project response JSON");
    let document: WorkflowDocument =
        serde_json::from_value(body["document"].clone()).expect("projected workflow document");
    assert_eq!(document.version, before.version);
    let graph = lash::typescript::workflow_graph::workflow_graph_from_source(&document.source)
        .expect("project response source projects");
    assert_eq!(
        lash::typescript::workflow_graph::workflow_graph_to_source(&graph)
            .expect("project response graph renders"),
        document.source
    );
    assert_eq!(
        lash::typescript::workflow_graph::workflow_graph_from_source(&document.source)
            .expect("project response source reprojects"),
        graph
    );
    assert!(document.nodes.iter().any(|node| {
        node.data.kind == "terminal"
            && node.data.terminal_kind.as_deref() == Some("finish")
            && node.data.expression.as_deref() == Some("value")
    }));
    let process = document
        .nodes
        .iter()
        .find(|node| node.data.kind == "process")
        .expect("projected process");
    assert_eq!(process.data.available_vars, ["input"]);

    let after: WorkflowDocument = client
        .get(format!("{base}/workflow"))
        .send()
        .await
        .expect("GET /workflow after projection")
        .json()
        .await
        .expect("workflow after projection");
    assert_eq!(after.version, before.version);
    assert_eq!(after.source, before.source);

    let response = client
        .post(format!("{base}/project"))
        .json(&serde_json::json!({ "source": "const broken = async (" }))
        .send()
        .await
        .expect("POST /project invalid source");
    assert_eq!(response.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = response.json().await.expect("project error JSON");
    assert_eq!(body["error"]["code"], "invalid_source");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty())
    );
    assert!(body["error"].get("details").is_none());

    server.abort();
}

#[tokio::test]
async fn projected_available_vars_follow_ssa_and_nested_lexical_scope() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("test listener address");
    let server = tokio::spawn(workflow_graph_roundtrip::serve(
        listener,
        AppState::default(),
    ));
    let client = reqwest::Client::new();
    let response = client
        .post(format!("http://{addr}/project"))
        .json(&serde_json::json!({
            "source": r#"
                const scoped = async (record) => {
                  let state = { count: 0 };
                  let first = 1;
                  for (const item of [1]) {
                    let nested = first + item;
                  }
                  return state;
                };
            "#
        }))
        .send()
        .await
        .expect("POST scoped source");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.expect("scoped project JSON");
    let document: WorkflowDocument =
        serde_json::from_value(body["document"].clone()).expect("scoped document");
    assert_eq!(
        document.facet_schema_version,
        Some(lash::rlm::lang::WORKFLOW_TYPE_FACET_SCHEMA_VERSION)
    );
    let state = document
        .nodes
        .iter()
        .find(|node| node.data.binding.as_deref() == Some("state"))
        .expect("state binding");
    assert_eq!(state.data.available_vars, ["record"]);
    assert_eq!(state.data.available_vars[0].variable_type, "any");
    let first = document
        .nodes
        .iter()
        .find(|node| node.data.binding.as_deref() == Some("first"))
        .expect("first binding");
    assert_eq!(first.data.available_vars, ["record", "state"]);
    let for_node = document
        .nodes
        .iter()
        .find(|node| node.data.subkind.as_deref() == Some("for"))
        .expect("for node");
    assert_eq!(for_node.data.available_vars, ["first", "record", "state"]);
    let nested = document
        .nodes
        .iter()
        .find(|node| node.data.binding.as_deref() == Some("nested"))
        .expect("nested binding");
    assert_eq!(
        nested.data.available_vars,
        ["first", "item", "record", "state"]
    );
    // `for (const item of xs)` hands the loop its iterable itself (FIG-3625),
    // so the loop binding projects as `[1]`'s element type, `int`, as the
    // Lashlang `for item in [1]` did. (Through FIG-3033 it lowered through an
    // opaque iterable copy and projected `any`.) The lash-typescript law
    // `a_for_of_body_sees_its_loop_binding_typed_by_the_iterable` holds the
    // same in PR CI.
    assert_eq!(
        nested
            .data
            .available_vars
            .iter()
            .find(|variable| variable.name == "item")
            .expect("typed loop binding")
            .variable_type,
        "int"
    );
    let terminal = document
        .nodes
        .iter()
        .find(|node| node.data.kind == "terminal")
        .expect("terminal");
    assert_eq!(
        terminal.data.available_vars,
        ["first", "nested", "record", "state"]
    );

    server.abort();
}

#[tokio::test]
async fn data_terminal_call_and_effect_edits_round_trip_without_raw_constructor_source() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("test listener address");
    let server = tokio::spawn(workflow_graph_roundtrip::serve(
        listener,
        AppState::default(),
    ));
    let client = reqwest::Client::new();
    let base = format!("http://{addr}");
    let mut document = select_workflow(&client, &base, "blank").await;

    let mut data = new_flow_node("new:data", "data", None, "value");
    data.data.binding = Some("value".to_string());
    data.data.expression = Some("1 + 1".to_string());
    append_process_node(&mut document, data);
    let mut computation = new_flow_node("new:computation", "computation", None, "Compute");
    computation.data.expression = Some("1 + 1".to_string());
    append_process_node(&mut document, computation);
    let mut call = new_flow_node("new:call-from-catalog", "call", None, "Set status");
    call.data.operation = Some("set_status".to_string());
    call.data.fields.insert(
        "key".to_string(),
        EditableValue::String("phase".to_string()),
    );
    call.data.fields.insert(
        "value".to_string(),
        EditableValue::String("ready".to_string()),
    );
    append_process_node(&mut document, call);
    let mut effect = new_flow_node("new:effect-from-catalog", "effect", None, "Sleep");
    effect.data.effect = Some("sleep".to_string());
    effect.data.fields.insert(
        "duration".to_string(),
        EditableValue::String("1ms".to_string()),
    );
    append_process_node(&mut document, effect);

    let response = client
        .post(format!("{base}/workflow"))
        .json(&document)
        .send()
        .await
        .expect("save catalog-constructed nodes");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let mut saved: WorkflowDocument = response.json().await.expect("first edited document");
    assert!(saved.source.contains("value = (1 + 1)"));
    assert!(saved.nodes.iter().any(|node| {
        node.data.kind == "computation" && node.data.expression.as_deref() == Some("(1 + 1)")
    }));
    assert!(
        saved
            .source
            .contains("await (display.set_status({ key: \"phase\", value: \"ready\" }))")
    );
    assert!(saved.source.contains("await sleep(\"1ms\")"));

    let data = saved
        .nodes
        .iter_mut()
        .find(|node| node.data.binding.as_deref() == Some("value"))
        .expect("saved data node");
    assert_eq!(data.data.expression.as_deref(), Some("(1 + 1)"));
    data.data.expression = Some("40 + 2".to_string());
    let terminal = saved
        .nodes
        .iter_mut()
        .find(|node| node.data.kind == "terminal")
        .expect("saved terminal");
    assert_eq!(terminal.data.terminal_kind.as_deref(), Some("finish"));
    assert_eq!(terminal.data.expression.as_deref(), Some("0"));
    // FIG-3033: a process ends with `return` in TypeScript and the dialect has
    // no authorable `fail(...)` terminal, so the terminal's editable value is
    // switched here instead of its kind.
    terminal.data.expression = Some("\"stopped\"".to_string());
    let call = saved
        .nodes
        .iter_mut()
        .find(|node| node.data.operation.as_deref() == Some("set_status"))
        .expect("saved call");
    call.data.operation = Some("show_message".to_string());
    call.data.fields.clear();
    call.data.fields.insert(
        "text".to_string(),
        EditableValue::String("switched".to_string()),
    );
    call.data.fields.insert(
        "tone".to_string(),
        EditableValue::String("warm".to_string()),
    );
    let effect = saved
        .nodes
        .iter_mut()
        .find(|node| node.data.effect.as_deref() == Some("sleep"))
        .expect("saved effect");
    effect.data.effect = Some("wait_signal".to_string());
    effect.data.fields.clear();
    effect.data.fields.insert(
        "signal".to_string(),
        EditableValue::String("continue".to_string()),
    );

    let response = client
        .post(format!("{base}/workflow"))
        .json(&saved)
        .send()
        .await
        .expect("save switched nodes");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let mut switched: WorkflowDocument = response.json().await.expect("switched document");
    assert!(switched.source.contains("let value = (40 + 2);"));
    assert!(switched.source.contains("return \"stopped\";"));
    assert!(
        switched
            .source
            .contains("await (display.show_message({ text: \"switched\", tone: \"warm\" }))")
    );
    assert!(!switched.source.contains("key: \"phase\""));
    assert!(!switched.source.contains("value: \"ready\""));
    assert!(switched.source.contains("await waitSignal(\"continue\")"));

    let call = switched
        .nodes
        .iter_mut()
        .find(|node| node.data.operation.as_deref() == Some("show_message"))
        .expect("switched call");
    call.data.fields.remove("tone");
    let terminal = switched
        .nodes
        .iter_mut()
        .find(|node| node.data.kind == "terminal")
        .expect("switched terminal");
    terminal.data.expression = Some("value".to_string());

    let response = client
        .post(format!("{base}/workflow"))
        .json(&switched)
        .send()
        .await
        .expect("save removed arg and restored finish");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let restored: WorkflowDocument = response.json().await.expect("restored document");
    assert!(restored.source.contains("return value;"));
    assert!(
        restored
            .source
            .contains("await (display.show_message({ text: \"switched\" }))")
    );
    assert!(!restored.source.contains("tone:"));
    assert!(restored.nodes.iter().any(|node| {
        node.data.kind == "terminal"
            && node.data.terminal_kind.as_deref() == Some("finish")
            && node.data.expression.as_deref() == Some("value")
    }));

    server.abort();
}

#[tokio::test]
async fn lists_selects_projects_and_runs_built_in_workflows() {
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

    let response = client
        .get(format!("{base}/workflows"))
        .send()
        .await
        .expect("GET /workflows");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let catalog: Vec<WorkflowCatalogEntry> = response.json().await.expect("workflow catalog JSON");
    assert_eq!(
        catalog
            .iter()
            .map(|entry| (entry.id.as_str(), entry.name.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("blank", "Blank workflow"),
            ("onboarding", "Onboarding"),
            ("summarize-emails", "Summarize my top 5 emails"),
            ("research-nvidia-stock", "Research NVIDIA stock"),
            ("team-standup-digest", "Team standup digest"),
            ("traffic-lights", "Traffic Lights"),
            ("branching-approval", "Branching Approval"),
            ("counter-loop", "Counter Loop"),
        ]
    );
    assert!(catalog.iter().all(|entry| !entry.description.is_empty()));

    let mut run_ids = BTreeSet::new();
    for (index, entry) in catalog.iter().enumerate() {
        let response = client
            .post(format!("{base}/workflow/select"))
            .json(&serde_json::json!({ "id": entry.id }))
            .send()
            .await
            .expect("POST /workflow/select");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let document: WorkflowDocument = response.json().await.expect("selected workflow JSON");
        assert_eq!(document.version, index as u64 + 2);
        assert!(!document.nodes.is_empty());
        assert!(!contains_key(
            &serde_json::to_value(&document).expect("workflow document value"),
            "position"
        ));

        let projected =
            lash::typescript::workflow_graph::workflow_graph_from_source(&document.source)
                .expect("catalog source should project");
        let nodes = projected.nodes().collect::<Vec<_>>();
        assert!(
            !nodes.is_empty(),
            "catalog workflow `{}` must project at least one textual node",
            entry.id
        );
        let slices = nodes
            .iter()
            .map(|node| {
                let span = node.source_span.unwrap_or_else(|| {
                    panic!("catalog node `{}` has no canonical span", node.name)
                });
                document
                    .source
                    .get(span.start..span.end)
                    .unwrap_or_else(|| {
                        panic!(
                            "catalog node `{}` span {span:?} is out of bounds for {} bytes",
                            node.name,
                            document.source.len()
                        )
                    })
            })
            .collect::<Vec<_>>();
        assert_eq!(
            slices[0],
            expected_catalog_process_slice(&entry.id, &document.source),
            "exact canonical process-root slice for catalog workflow `{}`",
            entry.id
        );
        let expected = expected_catalog_node_slices(&entry.id);
        assert!(
            !expected.is_empty(),
            "catalog oracle `{}` is empty",
            entry.id
        );
        assert_eq!(
            &slices[1..],
            expected,
            "exact canonical node slices for catalog workflow `{}`",
            entry.id
        );
        let rendered = lash::typescript::workflow_graph::workflow_graph_to_source(&projected)
            .expect("catalog graph should render");
        assert_eq!(rendered, document.source);
        assert_eq!(
            lash::typescript::parse(&rendered).expect("rendered catalog source"),
            lash::typescript::parse(&document.source).expect("canonical catalog source")
        );
        assert_eq!(
            lash::typescript::workflow_graph::workflow_graph_from_source(&rendered)
                .expect("reproject catalog source"),
            projected
        );

        if matches!(
            entry.id.as_str(),
            "summarize-emails" | "research-nvidia-stock" | "team-standup-digest"
        ) {
            assert!(!document.source.contains("display.set_status"));
            assert!(!document.source.contains("display.set_progress"));
            assert!(!document.source.contains("gmail.summarize"));
            assert!(!document.source.contains("research.deep"));
        }
        if entry.id == "summarize-emails" {
            assert_eq!(document.source.matches("llm.query").count(), 2);
        }

        // A label is the serialization target for an editor rename, so the
        // corpora that carry one prove the whole path: the doc comment
        // projects as an authored name, and the fixpoint checks above already
        // proved it is written back byte-identically.
        let authored = document
            .nodes
            .iter()
            .filter(|node| matches!(node.data.name, NodeName::Authored { .. }))
            .count();
        if matches!(
            entry.id.as_str(),
            "onboarding" | "traffic-lights" | "branching-approval" | "counter-loop"
        ) {
            assert!(
                authored > 0,
                "{} should carry authored `@label` names",
                entry.id
            );
            assert!(document.source.contains("/** @label "));
        } else {
            assert_eq!(authored, 0, "{} should carry only derived names", entry.id);
            assert!(!document.source.contains("@label"));
        }

        if entry.id == "counter-loop" {
            assert!(!document.nodes.iter().any(|node| node.node_type == "opaque"));
            assert!(
                document
                    .nodes
                    .iter()
                    .any(|node| node.node_type == "state_update")
            );
            assert!(document.nodes.iter().any(|node| {
                node.node_type == "container"
                    && node.data.subkind.as_deref() == Some("while")
                    && node.data.children.iter().any(|child| child.slot == "body")
            }));
            assert!(document.nodes.iter().any(|node| {
                node.node_type == "container" && node.data.subkind.as_deref() == Some("for")
            }));
            assert!(projected.nodes().any(|node| {
                matches!(
                    &node.kind,
                    lash::rlm::lang::WorkflowNodeKind::StateUpdate { target, .. }
                        if !target.steps.is_empty()
                )
            }));
        }

        let events = run_workflow(&client, &base).await;
        // FIG-3057: a TypeScript process body is still projected through the
        // process wrapper, so a workflow whose only statement is the
        // closing `return` correlates no execution site yet. Every corpus that
        // performs work still emits events, and the blank starting point runs
        // to completion without failing.
        if entry.id == "blank" {
            assert!(events.iter().all(|event| event.status != RunStatus::Failed));
            assert!(run_ids.insert(format!("blank-{index}")), "fresh blank run");
            continue;
        }
        assert!(!events.is_empty(), "{} should emit run events", entry.id);
        let run_id = events[0].run_id.clone();
        assert!(run_ids.insert(run_id.clone()), "each run id must be fresh");
        assert!(events.iter().all(|event| event.run_id == run_id));
        assert!(
            events
                .iter()
                .all(|event| event.workflow_version == document.version)
        );
        assert!(
            events
                .iter()
                .all(|event| { document.nodes.iter().any(|node| node.id == event.node_id) }),
            "{} emitted nodes outside its document: events={:?}, nodes={:?}",
            entry.id,
            events
                .iter()
                .map(|event| &event.node_id)
                .collect::<Vec<_>>(),
            document
                .nodes
                .iter()
                .map(|node| &node.id)
                .collect::<Vec<_>>(),
        );
        assert!(
            !events.iter().any(|event| event.status == RunStatus::Failed),
            "{} failed: {:?}",
            entry.id,
            events
                .iter()
                .filter_map(|event| event.error.as_deref())
                .collect::<Vec<_>>()
        );
        assert!(
            events
                .windows(2)
                .all(|pair| pair[0].sequence < pair[1].sequence)
        );
        // FIG-3057: the closing `return` of a TypeScript process still sits
        // inside the projected process wrapper and correlates no
        // execution site, so the run's last event is the workflow's last
        // correlated statement rather than its terminal node. The property kept
        // here is the one the assertion exists for: the run ends on a node the
        // document holds, and it ends successfully.
        let final_event = events.last().expect("final run event");
        assert!(
            document
                .nodes
                .iter()
                .any(|node| node.id == final_event.node_id),
            "{} final event node is missing from the document",
            entry.id
        );
        assert_eq!(final_event.status, RunStatus::Succeeded);

        if entry.id == "branching-approval" {
            assert!(
                events
                    .iter()
                    .any(|event| event.status == RunStatus::Waiting)
            );
            assert_eq!(
                final_event
                    .display
                    .statuses
                    .get("approval")
                    .map(String::as_str),
                Some("approved")
            );
            assert!(
                final_event
                    .display
                    .messages
                    .iter()
                    .any(|message| message == "Request approved")
            );
        }
    }
    assert_eq!(run_ids.len(), catalog.len());

    server.abort();
}

#[tokio::test]
async fn project_mutate_save_and_run_streams_correlated_events() {
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

    let response = client
        .get(format!("{base}/workflow"))
        .send()
        .await
        .expect("GET /workflow");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let projected_json: Value = response.json().await.expect("workflow JSON");
    assert!(!contains_key(&projected_json, "position"));
    let mut document: WorkflowDocument =
        serde_json::from_value(projected_json).expect("workflow contract");
    assert_eq!(document.version, 1);

    let message = document
        .nodes
        .iter_mut()
        .find(|node| node.data.operation.as_deref() == Some("show_message"))
        .expect("show_message node");
    assert!(
        matches!(message.data.name, NodeName::Derived { .. }),
        "unlabelled node projected as {:?}",
        message.data.name
    );
    message.data.fields.insert(
        "text".to_string(),
        EditableValue::String("Edited through the graph API".to_string()),
    );

    let response = client
        .post(format!("{base}/workflow"))
        .json(&document)
        .send()
        .await
        .expect("POST /workflow");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let saved: WorkflowDocument = response.json().await.expect("saved workflow JSON");
    assert_eq!(saved.version, 2);
    assert!(saved.source.contains("Edited through the graph API"));
    let edited_node_id = saved
        .nodes
        .iter()
        .find(|node| {
            node.data.fields.get("text")
                == Some(&EditableValue::String(
                    "Edited through the graph API".to_string(),
                ))
        })
        .expect("saved edited node")
        .id
        .clone();

    let events = run_workflow(&client, &base).await;

    assert!(!events.is_empty());
    let run_id = &events[0].run_id;
    assert!(events.iter().all(|event| event.run_id == *run_id));
    assert!(events.iter().all(|event| event.workflow_version == 2));
    assert!(
        events
            .iter()
            .all(|event| { saved.nodes.iter().any(|node| node.id == event.node_id) })
    );
    assert!(
        events
            .iter()
            .any(|event| event.status == RunStatus::Waiting)
    );
    assert!(!events.iter().any(|event| event.status == RunStatus::Failed));
    assert!(events.iter().any(|event| {
        event.node_id == edited_node_id
            && event.status == RunStatus::Succeeded
            && event
                .display_delta
                .messages_appended
                .iter()
                .any(|message| message == "Edited through the graph API")
    }));
    assert!(
        events
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence)
    );

    server.abort();
}

#[tokio::test]
async fn edited_counter_loop_condition_saves_reprojects_and_runs() {
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

    let mut document: WorkflowDocument = client
        .post(format!("{base}/workflow/select"))
        .json(&serde_json::json!({ "id": "counter-loop" }))
        .send()
        .await
        .expect("select counter-loop")
        .json()
        .await
        .expect("counter-loop document");
    let while_node = document
        .nodes
        .iter_mut()
        .find(|node| node.node_type == "container" && node.data.subkind.as_deref() == Some("while"))
        .expect("counter-loop while node");
    assert_eq!(
        while_node.data.condition.as_deref(),
        Some("(state.count < 3)")
    );
    while_node.data.condition = Some("(state.count < 1)".to_string());

    let response = client
        .post(format!("{base}/workflow"))
        .json(&document)
        .send()
        .await
        .expect("save edited counter-loop");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let saved: WorkflowDocument = response.json().await.expect("saved counter-loop");
    assert!(saved.source.contains("while ((state.count < 1))"));
    assert!(saved.nodes.iter().any(|node| {
        node.node_type == "container" && node.data.condition.as_deref() == Some("(state.count < 1)")
    }));

    let events = run_workflow(&client, &base).await;
    assert!(!events.is_empty());
    assert!(!events.iter().any(|event| event.status == RunStatus::Failed));
    let final_event = events.last().expect("terminal event");
    assert_eq!(final_event.status, RunStatus::Succeeded);
    assert_eq!(
        final_event.display.lists.get("counts"),
        Some(&vec!["0".to_string()])
    );

    server.abort();
}

#[tokio::test]
async fn bare_counter_loop_condition_rewraps_canonically_and_runs() {
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

    let mut document = select_workflow(&client, &base, "counter-loop").await;
    let while_node = document
        .nodes
        .iter_mut()
        .find(|node| node.node_type == "container" && node.data.subkind.as_deref() == Some("while"))
        .expect("counter-loop while node");
    while_node.data.condition = Some("state.count < 1".to_string());

    let response = client
        .post(format!("{base}/workflow"))
        .json(&document)
        .send()
        .await
        .expect("save bare counter-loop condition");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let saved: WorkflowDocument = response.json().await.expect("saved counter-loop");
    assert!(saved.source.contains("while ((state.count < 1))"));

    let events = run_workflow(&client, &base).await;
    assert!(!events.is_empty());
    assert!(!events.iter().any(|event| event.status == RunStatus::Failed));
    assert_eq!(
        events.last().expect("terminal event").status,
        RunStatus::Succeeded
    );

    server.abort();
}

#[tokio::test]
async fn expression_valued_call_fields_save_reproject_and_reject_malformed_edits() {
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

    let mut document = select_workflow(&client, &base, "counter-loop").await;
    let expected_progress = canonical_expression("state.count * 20 + 20");
    let expected_item = canonical_expression("state.count");
    assert!(document.nodes.iter().any(|node| {
        node.data.operation.as_deref() == Some("add_item")
            && node.data.fields.get("item") == Some(&EditableValue::Expr(expected_item.clone()))
    }));
    let progress = document
        .nodes
        .iter_mut()
        .find(|node| {
            node.data.operation.as_deref() == Some("set_progress")
                && node.data.fields.get("pct")
                    == Some(&EditableValue::Expr(expected_progress.clone()))
        })
        .expect("expression-valued progress node");
    progress.data.fields.insert(
        "pct".to_string(),
        EditableValue::Expr("state.count * 10 + 10".to_string()),
    );

    let response = client
        .post(format!("{base}/workflow"))
        .json(&document)
        .send()
        .await
        .expect("save expression-valued field");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let mut saved: WorkflowDocument = response.json().await.expect("saved workflow");
    let edited_progress = canonical_expression("state.count * 10 + 10");
    assert!(saved.nodes.iter().any(|node| {
        node.data.operation.as_deref() == Some("set_progress")
            && node.data.fields.get("pct") == Some(&EditableValue::Expr(edited_progress.clone()))
    }));
    assert!(saved.source.contains(&edited_progress));

    let malformed = saved
        .nodes
        .iter_mut()
        .find(|node| {
            node.data.operation.as_deref() == Some("set_progress")
                && node.data.fields.get("pct")
                    == Some(&EditableValue::Expr(edited_progress.clone()))
        })
        .expect("edited progress node");
    malformed.data.fields.insert(
        "pct".to_string(),
        EditableValue::Expr("state.count +".to_string()),
    );
    let response = client
        .post(format!("{base}/workflow"))
        .json(&saved)
        .send()
        .await
        .expect("save malformed expression-valued field");
    assert_eq!(response.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = response.json().await.expect("typed expression error");
    assert_eq!(body["error"]["code"], "invalid_expression");
    assert_eq!(body["error"]["details"]["field"], "fields.pct");

    server.abort();
}

#[tokio::test]
async fn edited_if_condition_and_for_iterable_save_reproject_and_run() {
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

    let mut if_document: WorkflowDocument = client
        .post(format!("{base}/workflow/select"))
        .json(&serde_json::json!({ "id": "branching-approval" }))
        .send()
        .await
        .expect("select branching-approval")
        .json()
        .await
        .expect("branching-approval document");
    let if_node = if_document
        .nodes
        .iter_mut()
        .find(|node| {
            node.node_type == "container" && node.data.condition.as_deref() == Some("true")
        })
        .expect("branching-approval literal if node");
    if_node.data.condition = Some("false".to_string());

    let response = client
        .post(format!("{base}/workflow"))
        .json(&if_document)
        .send()
        .await
        .expect("save edited branching-approval");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let saved_if: WorkflowDocument = response.json().await.expect("saved branching-approval");
    assert!(saved_if.source.contains("if (false)"));
    assert!(saved_if.nodes.iter().any(|node| {
        node.node_type == "container" && node.data.condition.as_deref() == Some("false")
    }));
    let projected_if =
        lash::typescript::workflow_graph::workflow_graph_from_source(&saved_if.source)
            .expect("edited branching-approval source should reproject");
    assert_eq!(
        lash::typescript::workflow_graph::workflow_graph_to_source(&projected_if)
            .expect("edited branching-approval graph should render"),
        saved_if.source
    );

    let if_events = run_workflow(&client, &base).await;
    assert!(!if_events.is_empty());
    assert!(
        if_events
            .iter()
            .all(|event| event.workflow_version == saved_if.version)
    );
    assert!(
        if_events
            .iter()
            .all(|event| { saved_if.nodes.iter().any(|node| node.id == event.node_id) })
    );
    assert!(
        !if_events
            .iter()
            .any(|event| event.status == RunStatus::Failed)
    );
    let final_if_event = if_events.last().expect("branching terminal event");
    assert_eq!(final_if_event.status, RunStatus::Succeeded);
    assert!(
        final_if_event
            .display
            .messages
            .iter()
            .any(|message| message == "Approval needs review")
    );

    let mut for_document: WorkflowDocument = client
        .post(format!("{base}/workflow/select"))
        .json(&serde_json::json!({ "id": "counter-loop" }))
        .send()
        .await
        .expect("select counter-loop")
        .json()
        .await
        .expect("counter-loop document");
    let for_node = for_document
        .nodes
        .iter_mut()
        .find(|node| node.node_type == "container" && node.data.iterable.is_some())
        .expect("counter-loop for node");
    assert_eq!(for_node.data.iterable.as_deref(), Some("[70, 85, 100]"));
    for_node.data.iterable = Some("[15]".to_string());

    let response = client
        .post(format!("{base}/workflow"))
        .json(&for_document)
        .send()
        .await
        .expect("save edited counter-loop for iterable");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let saved_for: WorkflowDocument = response
        .json()
        .await
        .expect("saved counter-loop for iterable");
    assert!(saved_for.source.contains("for (const pct of [15])"));
    assert!(saved_for.nodes.iter().any(|node| {
        node.node_type == "container" && node.data.iterable.as_deref() == Some("[15]")
    }));
    let projected_for =
        lash::typescript::workflow_graph::workflow_graph_from_source(&saved_for.source)
            .expect("edited counter-loop source should reproject");
    assert_eq!(
        lash::typescript::workflow_graph::workflow_graph_to_source(&projected_for)
            .expect("edited counter-loop graph should render"),
        saved_for.source
    );

    let for_events = run_workflow(&client, &base).await;
    assert!(!for_events.is_empty());
    assert!(
        for_events
            .iter()
            .all(|event| event.workflow_version == saved_for.version)
    );
    assert!(
        for_events
            .iter()
            .all(|event| { saved_for.nodes.iter().any(|node| node.id == event.node_id) }),
        "edited for run emitted nodes outside its document: events={:?}, nodes={:?}",
        for_events
            .iter()
            .map(|event| &event.node_id)
            .collect::<Vec<_>>(),
        saved_for
            .nodes
            .iter()
            .map(|node| &node.id)
            .collect::<Vec<_>>(),
    );
    assert!(
        !for_events
            .iter()
            .any(|event| event.status == RunStatus::Failed)
    );
    let final_for_event = for_events.last().expect("counter-loop terminal event");
    assert_eq!(final_for_event.status, RunStatus::Succeeded);
    assert_eq!(final_for_event.display.progress, 15.0);

    server.abort();
}

#[tokio::test]
async fn delete_node_edit_round_trips_and_runs_the_saved_graph() {
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

    let mut document: WorkflowDocument = client
        .get(format!("{base}/workflow"))
        .send()
        .await
        .expect("GET /workflow")
        .json()
        .await
        .expect("workflow document");
    let original_node_count = document.nodes.len();
    let deleted_id = document
        .nodes
        .iter()
        .find(|node| {
            node.data.operation.as_deref() == Some("set_light")
                && node.data.fields.get("name")
                    == Some(&EditableValue::String("complete".to_string()))
        })
        .expect("complete light node")
        .id
        .clone();
    document.nodes.retain(|node| node.id != deleted_id);
    document
        .edges
        .retain(|edge| edge.source != deleted_id && edge.target != deleted_id);
    document.roots.main.retain(|node_id| node_id != &deleted_id);
    document
        .roots
        .processes
        .retain(|node_id| node_id != &deleted_id);
    for node in &mut document.nodes {
        for child in &mut node.data.children {
            child.node_ids.retain(|node_id| node_id != &deleted_id);
        }
    }

    let response = client
        .post(format!("{base}/workflow"))
        .json(&document)
        .send()
        .await
        .expect("POST deleted workflow node");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let saved: WorkflowDocument = response.json().await.expect("saved workflow JSON");
    assert_eq!(saved.version, 2);
    assert_eq!(saved.nodes.len(), original_node_count - 1);
    assert!(!saved.source.contains("name: \"complete\""));
    let projected = lash::typescript::workflow_graph::workflow_graph_from_source(&saved.source)
        .expect("deleted workflow source should reproject");
    assert_eq!(
        lash::typescript::workflow_graph::workflow_graph_to_source(&projected)
            .expect("deleted graph should render"),
        saved.source
    );

    let events = run_workflow(&client, &base).await;
    assert!(!events.is_empty());
    assert!(events.iter().all(|event| event.workflow_version == 2));
    assert!(
        events
            .iter()
            .all(|event| saved.nodes.iter().any(|node| node.id == event.node_id))
    );
    assert!(!events.iter().any(|event| event.status == RunStatus::Failed));
    assert_eq!(
        events.last().expect("terminal event").status,
        RunStatus::Succeeded
    );

    server.abort();
}

#[tokio::test]
async fn new_call_node_saves_reprojects_and_runs_with_canonical_correlation() {
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
    let mut call = new_flow_node("new:call", "call", None, "Set status");
    call.data.expression =
        Some("await display.set_status({ key: \"k\", value: \"v\" })".to_string());
    append_process_node(&mut document, call);
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
        .expect("save workflow with new call");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let saved_response: SaveWorkflowResponse = response.json().await.expect("saved workflow");
    let saved = saved_response.document;
    assert_eq!(saved_response.id_map.len(), posted_ids.len());
    assert!(posted_ids.iter().all(|posted_id| {
        saved_response
            .id_map
            .get(posted_id)
            .is_some_and(|reprojected_id| saved.nodes.iter().any(|node| &node.id == reprojected_id))
    }));
    assert!(
        saved_response
            .id_map
            .get("new:call")
            .is_some_and(|reprojected_id| {
                saved.nodes.iter().any(|node| {
                    &node.id == reprojected_id
                        && node.data.operation.as_deref() == Some("set_status")
                })
            })
    );
    assert!(
        saved
            .source
            .contains("await (display.set_status({ key: \"k\", value: \"v\" }))")
    );
    let call_id = saved
        .nodes
        .iter()
        .find(|node| node.data.operation.as_deref() == Some("set_status"))
        .expect("reprojected call node")
        .id
        .clone();
    assert_ne!(call_id, "new:call");

    let events = run_workflow(&client, &base).await;
    assert!(!events.iter().any(|event| event.status == RunStatus::Failed));
    assert!(events.iter().any(|event| {
        event.node_id == call_id
            && event.status == RunStatus::Succeeded
            && event.display.statuses.get("k").map(String::as_str) == Some("v")
    }));
    assert!(
        events
            .iter()
            .all(|event| saved.nodes.iter().any(|node| node.id == event.node_id))
    );

    server.abort();
}

#[tokio::test]
async fn new_if_while_and_for_containers_save_reproject_and_run() {
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

    let mut if_child = new_flow_node("new:if-call", "call", None, "If message");
    if_child.parent_id = Some("new:if".to_string());
    if_child.data.expression =
        Some("await display.show_message({ text: \"Created if body\" })".to_string());
    let mut if_node = new_flow_node("new:if", "container", Some("if"), "if");
    if_node.data.condition = Some("true".to_string());
    if_node.data.children = vec![
        ChildGroup {
            slot: "then".to_string(),
            scope: "container:new-if:then".to_string(),
            node_ids: vec![if_child.id.clone()],
        },
        ChildGroup {
            slot: "else".to_string(),
            scope: "container:new-if:else".to_string(),
            node_ids: Vec::new(),
        },
    ];
    append_process_node(&mut document, if_node);
    document.nodes.push(if_child);

    let mut while_child = new_flow_node("new:while-call", "call", None, "While message");
    while_child.parent_id = Some("new:while".to_string());
    while_child.data.expression =
        Some("await display.show_message({ text: \"Created while body\" })".to_string());
    let mut while_node = new_flow_node("new:while", "container", Some("while"), "while");
    while_node.data.condition = Some("false".to_string());
    while_node.data.children = vec![ChildGroup {
        slot: "body".to_string(),
        scope: "container:new-while:body".to_string(),
        node_ids: vec![while_child.id.clone()],
    }];
    append_process_node(&mut document, while_node);
    document.nodes.push(while_child);

    let mut for_child = new_flow_node("new:for-call", "call", None, "For status");
    for_child.parent_id = Some("new:for".to_string());
    for_child.data.expression =
        Some("await display.set_status({ key: \"loop\", value: \"ran\" })".to_string());
    let mut for_node = new_flow_node("new:for", "container", Some("for"), "for item");
    for_node.data.binding = Some("item".to_string());
    for_node.data.iterable = Some("[1]".to_string());
    for_node.data.children = vec![ChildGroup {
        slot: "body".to_string(),
        scope: "container:new-for:body".to_string(),
        node_ids: vec![for_child.id.clone()],
    }];
    append_process_node(&mut document, for_node);
    document.nodes.push(for_child);

    let response = client
        .post(format!("{base}/workflow"))
        .json(&document)
        .send()
        .await
        .expect("save workflow with new containers");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let saved: WorkflowDocument = response.json().await.expect("saved workflow");
    assert!(saved.source.contains("if (true) {"));
    assert!(saved.source.contains("while (false)"));
    assert!(saved.source.contains("for (const item of [1])"));
    for subkind in ["if", "while", "for"] {
        assert!(saved.nodes.iter().any(|node| {
            node.data.kind == "container" && node.data.subkind.as_deref() == Some(subkind)
        }));
    }

    let events = run_workflow(&client, &base).await;
    assert!(!events.iter().any(|event| event.status == RunStatus::Failed));
    let final_event = events.last().expect("terminal event");
    assert_eq!(final_event.status, RunStatus::Succeeded);
    assert_eq!(
        final_event.display.statuses.get("loop").map(String::as_str),
        Some("ran")
    );
    assert!(
        final_event
            .display
            .messages
            .iter()
            .any(|message| message == "Created if body")
    );

    server.abort();
}

#[tokio::test]
async fn new_statement_containers_allow_empty_bodies() {
    let state = AppState::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("test listener address");
    let server = tokio::spawn(workflow_graph_roundtrip::serve(listener, state));
    let client = reqwest::Client::new();
    let base = format!("http://{addr}");

    let mut document = select_workflow(&client, &base, "blank").await;
    let mut if_node = new_flow_node("new:empty-if", "container", Some("if"), "if");
    if_node.data.condition = Some("true".to_string());
    append_process_node(&mut document, if_node);
    let mut while_node = new_flow_node("new:empty-while", "container", Some("while"), "while");
    while_node.data.condition = Some("false".to_string());
    append_process_node(&mut document, while_node);
    let mut for_node = new_flow_node("new:empty-for", "container", Some("for"), "for item");
    for_node.data.binding = Some("item".to_string());
    for_node.data.iterable = Some("[]".to_string());
    append_process_node(&mut document, for_node);

    let response = client
        .post(format!("{base}/workflow"))
        .json(&document)
        .send()
        .await
        .expect("save empty statement containers");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let saved: WorkflowDocument = response.json().await.expect("saved workflow");
    assert!(saved.source.contains("if (true) {}"));
    assert!(saved.source.contains("while (false) {}"));
    assert!(saved.source.contains("for (const item of []) {}"));

    let events = run_workflow(&client, &base).await;
    assert_eq!(
        events.last().expect("terminal event").status,
        RunStatus::Succeeded
    );

    server.abort();
}

#[tokio::test]
async fn blank_workflow_grows_by_two_nodes_then_saves_and_runs() {
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
    assert_eq!(
        document.source,
        "const blank = async () => {\n  return 0;\n};\n"
    );

    let mut status = new_flow_node("new:blank-status", "call", None, "Blank status");
    status.data.expression =
        Some("await display.set_status({ key: \"blank\", value: \"built\" })".to_string());
    append_process_node(&mut document, status);
    let mut message = new_flow_node("new:blank-message", "call", None, "Blank message");
    message.data.expression =
        Some("await display.show_message({ text: \"Built from blank\" })".to_string());
    append_process_node(&mut document, message);

    let response = client
        .post(format!("{base}/workflow"))
        .json(&document)
        .send()
        .await
        .expect("save grown blank workflow");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let saved: WorkflowDocument = response.json().await.expect("saved workflow");
    assert!(saved.source.contains("display.set_status"));
    assert!(saved.source.contains("display.show_message"));

    let events = run_workflow(&client, &base).await;
    let final_event = events.last().expect("terminal event");
    assert_eq!(final_event.status, RunStatus::Succeeded);
    assert_eq!(
        final_event
            .display
            .statuses
            .get("blank")
            .map(String::as_str),
        Some("built")
    );
    assert!(
        final_event
            .display
            .messages
            .iter()
            .any(|message| message == "Built from blank")
    );

    server.abort();
}

#[tokio::test]
async fn reordered_process_statements_save_reproject_and_run_in_node_id_order() {
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
    let mut first = new_flow_node("new:first", "call", None, "First message");
    first.data.expression = Some("await display.show_message({ text: \"First\" })".to_string());
    append_process_node(&mut document, first);
    let mut second = new_flow_node("new:second", "call", None, "Second message");
    second.data.expression = Some("await display.show_message({ text: \"Second\" })".to_string());
    append_process_node(&mut document, second);

    let process = document
        .nodes
        .iter_mut()
        .find(|node| node.node_type == "process")
        .expect("process container");
    let body = process
        .data
        .children
        .iter_mut()
        .find(|child| child.slot == "body")
        .expect("process body");
    let first_index = body
        .node_ids
        .iter()
        .position(|id| id == "new:first")
        .expect("first node membership");
    let second_index = body
        .node_ids
        .iter()
        .position(|id| id == "new:second")
        .expect("second node membership");
    body.node_ids.swap(first_index, second_index);

    let response = client
        .post(format!("{base}/workflow"))
        .json(&document)
        .send()
        .await
        .expect("save reordered workflow");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let saved: SaveWorkflowResponse = response.json().await.expect("saved reordered workflow");
    let second_source_index = saved
        .document
        .source
        .find("text: \"Second\"")
        .expect("second statement source");
    let first_source_index = saved
        .document
        .source
        .find("text: \"First\"")
        .expect("first statement source");
    assert!(second_source_index < first_source_index);

    let events = run_workflow(&client, &base).await;
    assert!(!events.iter().any(|event| event.status == RunStatus::Failed));
    assert_eq!(
        events.last().expect("terminal event").display.messages,
        vec!["Second".to_string(), "First".to_string()]
    );

    server.abort();
}

#[tokio::test]
async fn moved_statement_between_scopes_saves_reprojects_and_runs_in_new_scope() {
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

    let mut document = select_workflow(&client, &base, "branching-approval").await;
    let process_id = document
        .nodes
        .iter()
        .find(|node| node.node_type == "process")
        .expect("process container")
        .id
        .clone();
    let moved_id = document
        .nodes
        .iter()
        .find(|node| {
            node.data.fields.get("text")
                == Some(&EditableValue::String("Approval needs review".to_string()))
        })
        .expect("inner else-branch message")
        .id
        .clone();
    let original_parent = document
        .nodes
        .iter()
        .find(|node| node.id == moved_id)
        .and_then(|node| node.parent_id.clone())
        .expect("nested message parent");
    assert_ne!(original_parent, process_id);

    for node in &mut document.nodes {
        for child in &mut node.data.children {
            child.node_ids.retain(|id| id != &moved_id);
        }
    }
    let terminal_id = document
        .nodes
        .iter()
        .find(|node| node.node_type == "terminal")
        .expect("terminal node")
        .id
        .clone();
    let process = document
        .nodes
        .iter_mut()
        .find(|node| node.id == process_id)
        .expect("process container");
    let body = process
        .data
        .children
        .iter_mut()
        .find(|child| child.slot == "body")
        .expect("process body");
    let terminal_index = body
        .node_ids
        .iter()
        .position(|id| id == &terminal_id)
        .expect("terminal membership");
    body.node_ids.insert(terminal_index, moved_id.clone());
    document
        .nodes
        .iter_mut()
        .find(|node| node.id == moved_id)
        .expect("moved message node")
        .parent_id = Some(process_id);

    let response = client
        .post(format!("{base}/workflow"))
        .json(&document)
        .send()
        .await
        .expect("save moved workflow");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let saved: SaveWorkflowResponse = response.json().await.expect("saved moved workflow");
    let canonical_moved_id = saved
        .id_map
        .get(&moved_id)
        .expect("moved node ID reconciliation");
    let canonical_process = saved
        .document
        .nodes
        .iter()
        .find(|node| node.node_type == "process")
        .expect("canonical process");
    assert_eq!(
        saved
            .document
            .nodes
            .iter()
            .find(|node| &node.id == canonical_moved_id)
            .and_then(|node| node.parent_id.as_ref()),
        Some(&canonical_process.id)
    );
    assert!(canonical_process.data.children.iter().any(|child| {
        child.slot == "body" && child.node_ids.iter().any(|id| id == canonical_moved_id)
    }));
    let highlight_index = saved
        .document
        .source
        .find("target: \"result\"")
        .expect("result highlight source");
    let moved_index = saved
        .document
        .source
        .find("text: \"Approval needs review\"")
        .expect("moved message source");
    assert!(highlight_index < moved_index);

    let events = run_workflow(&client, &base).await;
    assert!(!events.iter().any(|event| event.status == RunStatus::Failed));
    assert!(
        events
            .last()
            .expect("terminal event")
            .display
            .messages
            .iter()
            .any(|message| message == "Approval needs review")
    );

    server.abort();
}

#[tokio::test]
async fn new_call_without_receiver_expression_returns_typed_error() {
    let state = AppState::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("test listener address");
    let server = tokio::spawn(workflow_graph_roundtrip::serve(listener, state));
    let client = reqwest::Client::new();
    let base = format!("http://{addr}");

    let mut document = select_workflow(&client, &base, "blank").await;
    let mut call = new_flow_node("new:invalid-call", "call", None, "Invalid call");
    call.data.expression = Some("1 + 2".to_string());
    append_process_node(&mut document, call);

    let response = client
        .post(format!("{base}/workflow"))
        .json(&document)
        .send()
        .await
        .expect("save invalid new call");
    assert_eq!(response.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = response.json().await.expect("typed error response");
    assert_eq!(body["error"]["code"], "invalid_expression");
    assert_eq!(body["error"]["details"]["nodeId"], "new:invalid-call");
    assert_eq!(body["error"]["details"]["field"], "expression");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("needs a receiver call"))
    );

    let mut document = select_workflow(&client, &base, "blank").await;
    let comprehension = new_flow_node(
        "new:empty-comprehension",
        "container",
        Some("comprehension"),
        "list comprehension",
    );
    append_process_node(&mut document, comprehension);
    let response = client
        .post(format!("{base}/workflow"))
        .json(&document)
        .send()
        .await
        .expect("save empty comprehension");
    assert_eq!(response.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = response.json().await.expect("typed error response");
    assert_eq!(body["error"]["code"], "invalid_node_payload");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("container body cannot be empty"))
    );

    server.abort();
}

#[tokio::test]
async fn invalid_graph_post_returns_typed_unprocessable_entity() {
    let state = AppState::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("test listener address");
    let server = tokio::spawn(workflow_graph_roundtrip::serve(listener, state));
    let client = reqwest::Client::new();
    let base = format!("http://{addr}");

    let mut document: WorkflowDocument = client
        .get(format!("{base}/workflow"))
        .send()
        .await
        .expect("GET /workflow")
        .json()
        .await
        .expect("workflow document");
    document.schema_version -= 1;

    let response = client
        .post(format!("{base}/workflow"))
        .json(&document)
        .send()
        .await
        .expect("POST invalid workflow graph");
    assert_eq!(response.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = response.json().await.expect("typed error response");
    assert_eq!(body["error"]["code"], "unsupported_schema_version");
    assert_eq!(
        body["error"]["details"]["found"],
        lash::rlm::lang::WORKFLOW_GRAPH_SCHEMA_VERSION - 1
    );
    assert_eq!(
        body["error"]["details"]["expected"],
        lash::rlm::lang::WORKFLOW_GRAPH_SCHEMA_VERSION
    );

    document.schema_version = lash::rlm::lang::WORKFLOW_GRAPH_SCHEMA_VERSION;
    let while_node = document
        .nodes
        .iter_mut()
        .find(|node| node.node_type == "container" && node.data.subkind.as_deref() == Some("while"))
        .expect("default workflow while container");
    let original_condition = while_node.data.condition.clone();
    while_node.data.condition = Some("count <".to_string());
    let response = client
        .post(format!("{base}/workflow"))
        .json(&document)
        .send()
        .await
        .expect("POST invalid while condition");
    assert_eq!(response.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = response.json().await.expect("typed expression response");
    assert_eq!(body["error"]["code"], "invalid_expression");
    assert_eq!(body["error"]["details"]["field"], "condition");

    let while_node = document
        .nodes
        .iter_mut()
        .find(|node| node.node_type == "container" && node.data.subkind.as_deref() == Some("while"))
        .expect("default workflow while container");
    while_node.data.condition = original_condition;
    while_node
        .data
        .children
        .retain(|child| child.slot != "body");
    let response = client
        .post(format!("{base}/workflow"))
        .json(&document)
        .send()
        .await
        .expect("POST while without body");
    assert_eq!(response.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = response.json().await.expect("typed missing-child response");
    assert_eq!(body["error"]["code"], "invalid_graph_document");
    assert_eq!(body["error"]["details"]["child"], "body");

    let mut document: WorkflowDocument = client
        .post(format!("{base}/workflow/select"))
        .json(&serde_json::json!({ "id": "counter-loop" }))
        .send()
        .await
        .expect("select counter-loop")
        .json()
        .await
        .expect("counter-loop document");
    let invalid_target = "state.count + 1";
    assert!(
        lash::typescript::workflow_graph::parse_typescript_expression(
            invalid_target,
            &workflow_globals(),
            &std::collections::BTreeSet::new(),
        )
        .is_ok(),
        "invalid assignment target should remain a valid expression"
    );
    let state_update = document
        .nodes
        .iter_mut()
        .find(|node| node.node_type == "state_update")
        .expect("counter-loop state update");
    state_update.data.target = Some(invalid_target.to_string());
    let response = client
        .post(format!("{base}/workflow"))
        .json(&document)
        .send()
        .await
        .expect("POST invalid state update target");
    assert_eq!(response.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = response
        .json()
        .await
        .expect("typed assignment-target response");
    assert_eq!(body["error"]["code"], "invalid_assignment_target");
    assert_eq!(body["error"]["details"]["field"], "target");

    server.abort();
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
    assert_eq!(
        response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
    let stream = response.text().await.expect("complete SSE stream");
    stream
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str::<RunEvent>(data).expect("run event JSON"))
        .collect()
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

fn new_flow_node(id: &str, kind: &str, subkind: Option<&str>, title: &str) -> FlowNode {
    FlowNode {
        id: id.to_string(),
        node_type: kind.to_string(),
        parent_id: None,
        data: NodeData {
            kind: kind.to_string(),
            subkind: subkind.map(str::to_string),
            name: NodeName::Derived {
                title: title.to_string(),
            },
            process_name: None,
            params: Vec::new(),
            signals: Vec::new(),
            operation: None,
            receiver: None,
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
    }
}

#[expect(
    clippy::expect_used,
    reason = "catalog entries always carry string nodeKind/label and an array of string-named \
              fields (see the catalog module the server serves)"
)]
fn new_flow_node_from_catalog(entry: &Value, id: &str) -> FlowNode {
    let text = |key: &str| entry[key].as_str();
    let kind = text("nodeKind").expect("catalog nodeKind");
    let mut node = new_flow_node(
        id,
        kind,
        text("subkind"),
        text("label").expect("catalog label"),
    );
    node.data.operation = text("operation").map(str::to_string);
    node.data.effect = text("effect").map(str::to_string);
    node.data.terminal_kind = text("terminalKind").map(str::to_string);
    for field in entry["fields"].as_array().expect("catalog fields") {
        let name = field["name"].as_str().expect("catalog field name");
        let default = field["default"]
            .as_str()
            .unwrap_or_else(|| panic!("catalog field {name} string default"))
            .to_string();
        match name {
            "name" => node.data.process_name = Some(default),
            "binding" => node.data.binding = Some(default),
            "target" => node.data.target = Some(default),
            "expression" => node.data.expression = Some(default),
            "condition" => node.data.condition = Some(default),
            "iterable" => node.data.iterable = Some(default),
            "source" => node.data.source = Some(default),
            _ => {
                node.data
                    .fields
                    .insert(name.to_string(), EditableValue::String(default));
            }
        }
    }
    node
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

fn contains_key(value: &Value, key: &str) -> bool {
    match value {
        Value::Array(values) => values.iter().any(|value| contains_key(value, key)),
        Value::Object(map) => {
            map.contains_key(key) || map.values().any(|value| contains_key(value, key))
        }
        _ => false,
    }
}
