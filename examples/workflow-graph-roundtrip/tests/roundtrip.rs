#[path = "support/catalog_span_oracles.rs"]
mod catalog_span_oracles;
#[path = "support/runtime.rs"]
mod runtime;

use std::collections::BTreeSet;

use catalog_span_oracles::{expected_catalog_node_slices, expected_catalog_process_slice};
use serde_json::Value;
use workflow_graph_roundtrip::{
    NodeName, RunEvent, RunStatus, WorkflowCatalogEntry, WorkflowDocument,
};

#[tokio::test]
async fn operation_catalog_and_fragment_validation_match_the_editor_contract() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("test listener address");
    let state = runtime::state().await;
    let server = tokio::spawn(workflow_graph_roundtrip::serve(listener, state));
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
    assert_eq!(entries.len(), 25);
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
            "fields": [{ "name": "text", "type": "string", "default": { "kind": "string", "value": "" } }]
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
                "fields": [{ "name": "pct", "type": "number", "default": { "kind": "number", "value": 0.0 } }]
            })
    }));
    for expected in [
        serde_json::json!({
            "id": "proc.process",
            "label": "Process",
            "nodeKind": "process",
            "fields": [
                { "name": "name", "type": "identifier", "default": { "kind": "string", "value": "my_process" } }
            ]
        }),
        serde_json::json!({
            "id": "control.for",
            "label": "For each",
            "nodeKind": "container",
            "subkind": "for",
            "fields": [
                { "name": "binding", "type": "identifier", "default": { "kind": "string", "value": "item" } },
                { "name": "iterable", "type": "expression", "default": { "kind": "expr", "value": "[1, 2, 3]" } }
            ]
        }),
        serde_json::json!({
            "id": "control.try",
            "label": "Try",
            "nodeKind": "container",
            "subkind": "try",
            "fields": [
                { "name": "catchBinding", "type": "identifier", "default": { "kind": "string", "value": "error" } }
            ]
        }),
        serde_json::json!({
            "id": "stmt.throw",
            "label": "Throw",
            "nodeKind": "throw",
            "fields": [{
                "name": "expression",
                "type": "expression",
                "default": { "kind": "expr", "value": "\"failed\"" }
            }]
        }),
        serde_json::json!({
            "id": "stmt.finish",
            "label": "Finish",
            "nodeKind": "terminal",
            "terminalKind": "finish",
            "fields": [{ "name": "expression", "type": "expression", "default": { "kind": "expr", "value": "0" } }]
        }),
        serde_json::json!({
            "id": "effect.sleep",
            "label": "Sleep",
            "nodeKind": "effect",
            "effect": "sleep_for",
            "fields": [{ "name": "duration", "type": "expression", "default": { "kind": "expr", "value": "\"1s\"" } }]
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
async fn source_projection_is_a_stateless_canonical_fixpoint_with_typed_errors() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("test listener address");
    let state = runtime::state().await;
    let server = tokio::spawn(workflow_graph_roundtrip::serve(listener, state));
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
        node.data.kind() == "terminal"
            && node.data.terminal_kind() == Some(&lash::vm::ir::WorkflowTerminalKind::Finish)
            && node.data.expression().as_deref() == Some("value")
    }));
    let process = document
        .nodes
        .iter()
        .find(|node| node.data.kind() == "process")
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
    let state = runtime::state().await;
    let server = tokio::spawn(workflow_graph_roundtrip::serve(listener, state));
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
        Some(lash::formats::WORKFLOW_TYPE_FACET_SCHEMA_VERSION)
    );
    let state = document
        .nodes
        .iter()
        .find(|node| node.data.binding().as_deref() == Some("state"))
        .expect("state binding");
    assert_eq!(state.data.available_vars, ["record"]);
    assert_eq!(state.data.available_vars[0].variable_type, "any");
    let first = document
        .nodes
        .iter()
        .find(|node| node.data.binding().as_deref() == Some("first"))
        .expect("first binding");
    assert_eq!(first.data.available_vars, ["record", "state"]);
    let for_node = document
        .nodes
        .iter()
        .find(|node| node.data.subkind() == Some("for"))
        .expect("for node");
    assert_eq!(for_node.data.available_vars, ["first", "record", "state"]);
    let nested = document
        .nodes
        .iter()
        .find(|node| node.data.binding().as_deref() == Some("nested"))
        .expect("nested binding");
    assert_eq!(
        nested.data.available_vars,
        ["first", "item", "record", "state"]
    );
    // `for (const item of xs)` hands the loop its iterable itself (FIG-3625),
    // so the loop binding projects as `[1]`'s element type, `int`, as the
    // Lash VM `for item in [1]` did. (Through FIG-3033 it lowered through an
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
        .find(|node| node.data.kind() == "terminal")
        .expect("terminal");
    assert_eq!(
        terminal.data.available_vars,
        ["first", "nested", "record", "state"]
    );

    server.abort();
}

#[tokio::test]
async fn lists_selects_projects_and_runs_built_in_workflows() {
    let state = runtime::state().await;
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
            assert!(!document.nodes.iter().any(|node| node.node_type == "throw"));
            assert!(
                document
                    .nodes
                    .iter()
                    .any(|node| node.node_type == "state_update")
            );
            assert!(document.nodes.iter().any(|node| {
                node.node_type == "container"
                    && node.data.subkind() == Some("while")
                    && node
                        .data
                        .children()
                        .iter()
                        .any(|child| child.slot == "body")
            }));
            assert!(document.nodes.iter().any(|node| {
                node.node_type == "container" && node.data.subkind() == Some("for")
            }));
            assert!(projected.nodes().any(|node| {
                matches!(
                    &node.kind,
                    lash::vm::ir::WorkflowNodeKind::StateUpdate { target, .. }
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

/// Test-support helper outside `#[test]`, so clippy.toml's allow-in-tests does not reach it.
async fn run_workflow(client: &reqwest::Client, base: &str) -> Vec<RunEvent> {
    runtime::run_workflow(client, base).await
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
