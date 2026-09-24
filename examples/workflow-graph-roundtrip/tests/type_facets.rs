use serde_json::Value;
use workflow_graph_roundtrip::{
    AppState, EditableValue, SaveWorkflowResponse, TypeDiagnostic, WorkflowDocument,
};

#[test]
fn facet_diagnostic_http_json_golden_is_exact() {
    let diagnostic = TypeDiagnostic {
        node_id: "node-1".to_string(),
        kind: "incompatible_expected_literal".to_string(),
        class: "definite".to_string(),
        slot: Some("arg[0][\"query\"]".to_string()),
        message: "expected enum, got incompatible literal \"bad\"".to_string(),
        span: Some(lash::rlm::lang::Span { start: 4, end: 9 }),
    };
    assert_eq!(
        serde_json::to_value(diagnostic).expect("diagnostic serializes"),
        serde_json::json!({
            "nodeId": "node-1",
            "kind": "incompatible_expected_literal",
            "class": "definite",
            "slot": "arg[0][\"query\"]",
            "message": "expected enum, got incompatible literal \"bad\"",
            "span": { "start": 4, "end": 9 }
        })
    );
}

#[tokio::test]
async fn type_facets_are_projected_and_client_echoes_are_ignored_on_save() {
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
        .post(format!("{base}/project"))
        .json(&serde_json::json!({
            "source": r#"
                const typed = async (name) => {
                  let result = await display.show_message({ text: name });
                  let shape = { ready: true };
                  for (const item of [shape.missing]) {
                    let seen = item;
                  }
                  return result;
                };
            "#
        }))
        .send()
        .await
        .expect("POST typed source");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.expect("typed project JSON");
    let mut document: WorkflowDocument =
        serde_json::from_value(body["document"].clone()).expect("typed document");
    assert_eq!(
        document.facet_schema_version,
        Some(lash::rlm::lang::WORKFLOW_TYPE_FACET_SCHEMA_VERSION)
    );
    let call = document
        .nodes
        .iter()
        .find(|node| node.data.operation.as_deref() == Some("show_message"))
        .expect("typed display call");
    assert!(call.data.expected_arg_types.iter().any(|argument| {
        argument.slot == "arg[0][\"text\"]" && argument.expected_type == "str"
    }));
    let loop_node = document
        .nodes
        .iter()
        .find(|node| node.data.subkind.as_deref() == Some("for"))
        .expect("non-list loop");
    assert!(
        loop_node
            .data
            .available_vars
            .iter()
            .any(|variable| { variable.name == "result" && variable.variable_type == "null" })
    );
    // FIG-3033: TypeScript's `for (const x of xs)` lowers through an iterable
    // copy, so a non-list target is never statically flagged the way Lashlang's
    // `for x in xs` was. The property kept here is the one the suite exists
    // for: a type diagnostic lands on the node whose expression caused it.
    assert!(loop_node.data.diagnostics.iter().any(|diagnostic| {
        diagnostic.kind == "unknown_object_field"
            && diagnostic.message.contains("no field `missing`")
    }));

    let canonical_source = document.source.clone();
    document.facet_schema_version = Some(999);
    for node in &mut document.nodes {
        for variable in &mut node.data.available_vars {
            variable.variable_type = "forged".to_string();
        }
        for argument in &mut node.data.expected_arg_types {
            argument.expected_type = "forged".to_string();
        }
        for diagnostic in &mut node.data.diagnostics {
            diagnostic.message = "forged".to_string();
        }
    }
    let response = client
        .post(format!("{base}/workflow"))
        .json(&document)
        .send()
        .await
        .expect("POST document with echoed facets");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let saved: SaveWorkflowResponse = response.json().await.expect("saved typed document");
    assert_eq!(saved.document.source, canonical_source);
    assert_eq!(
        saved.document.facet_schema_version,
        Some(lash::rlm::lang::WORKFLOW_TYPE_FACET_SCHEMA_VERSION)
    );
    assert!(saved.document.nodes.iter().all(|node| {
        node.data
            .available_vars
            .iter()
            .all(|variable| variable.variable_type != "forged")
            && node
                .data
                .expected_arg_types
                .iter()
                .all(|argument| argument.expected_type != "forged")
            && node
                .data
                .diagnostics
                .iter()
                .all(|diagnostic| diagnostic.message != "forged")
    }));

    server.abort();
}

#[tokio::test]
async fn mocked_tool_schemas_project_into_seed_workflow_facets() {
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

    let emails = select_workflow(&client, &base, "summarize-emails").await;
    assert_clean_facets(&emails);
    let summarize = call_with_task(&emails, "Summarize this email");
    assert!(summarize.data.expected_arg_types.iter().any(|argument| {
        argument.slot == "arg[0][\"task\"]" && argument.expected_type == "str"
    }));
    // `for (const email of emails)` hands the loop its iterable itself
    // (FIG-3625), so the loop binding projects as the list's element: an
    // object, open (`dict`) because the body passes it to a tool, which may
    // hold on to it (the closed-shape field guard, FIG-3626). Through
    // FIG-3033 it lowered through an opaque iterable copy and projected `any`.
    assert!(
        summarize
            .data
            .available_vars
            .iter()
            .any(|variable| { variable.name == "email" && variable.variable_type == "dict" })
    );
    assert!(summarize.data.available_vars.iter().any(|variable| {
        variable.name == "emails"
            && variable.variable_type
                == "list[{ from: str, snippet: str, subject: str, unread: bool }]"
    }));
    // FIG-3033: TypeScript has no renderable list-accumulation form, so the
    // corpus carries the mocked list straight into the digest call. The
    // property under test is unchanged: a list-typed local reaches a later
    // node with its element type intact.
    let format_digest = call_with_task(&emails, "Format these five summaries");
    assert!(format_digest.data.available_vars.iter().any(|variable| {
        variable.name == "emails"
            && variable.variable_type
                == "list[{ from: str, snippet: str, subject: str, unread: bool }]"
    }));

    let nvidia = select_workflow(&client, &base, "research-nvidia-stock").await;
    assert_clean_facets(&nvidia);
    let search = call_with_field(&nvidia, "query");
    assert!(search.data.expected_arg_types.iter().any(|argument| {
        argument.slot == "arg[0][\"query\"]" && argument.expected_type == "str"
    }));
    let research = call_with_operation(&nvidia, "spawn");
    assert!(research.data.available_vars.iter().any(|variable| {
        variable.name == "search"
            && variable.variable_type == "{ results: list[{ content: str, title: str, url: str }] }"
    }));
    // FIG-3033: the declared `output:` type literal has no TypeScript spelling
    // (`Expr::TypeLiteral` is unrepresentable), so the corpus cannot narrow a
    // subagent result and the binding projects as `any`. Its visibility in the
    // later node's scope -- what this assertion exists for -- is unchanged.
    let research_message = call_with_operation(&nvidia, "show_message");
    assert!(
        research_message
            .data
            .available_vars
            .iter()
            .any(|variable| { variable.name == "research" && variable.variable_type == "any" })
    );
    assert!(
        research_message
            .data
            .expected_arg_types
            .iter()
            .any(|argument| {
                argument.slot == "arg[0][\"text\"]" && argument.expected_type == "str"
            })
    );

    let response = client
        .post(format!("{base}/project"))
        .json(&serde_json::json!({
            // The subagent result is untyped in TypeScript (see above), so the
            // bad field is read off the typed web-search result instead.
            "source": nvidia.source.replace("search.results", "search.missing")
        }))
        .send()
        .await
        .expect("POST invalid typed subagent field");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response
        .json()
        .await
        .expect("invalid typed field projection");
    let invalid: WorkflowDocument =
        serde_json::from_value(body["document"].clone()).expect("invalid typed document");
    let invalid_message = call_with_operation(&invalid, "spawn");
    assert!(invalid_message.data.diagnostics.iter().any(|diagnostic| {
        diagnostic.kind == "unknown_object_field" && diagnostic.message.contains("missing")
    }));

    let standup = select_workflow(&client, &base, "team-standup-digest").await;
    assert_clean_facets(&standup);
    let slack = call_with_field(&standup, "channel");
    assert!(slack.data.expected_arg_types.iter().any(|argument| {
        argument.slot == "arg[0][\"channel\"]" && argument.expected_type == "str"
    }));
    let github = call_with_field(&standup, "repo");
    assert!(github.data.available_vars.iter().any(|variable| {
        variable.name == "messages"
            && variable.variable_type == "list[{ text: str, ts: str, user: str }]"
    }));
    assert!(github.data.expected_arg_types.iter().any(|argument| {
        argument.slot == "arg[0][\"repo\"]" && argument.expected_type == "str"
    }));
    let digest = call_with_operation(&standup, "spawn");
    assert!(digest.data.available_vars.iter().any(|variable| {
        variable.name == "activity"
            && variable.variable_type == "list[{ author: str, kind: str, title: str }]"
    }));
    let standup_message = call_with_operation(&standup, "show_message");
    assert!(standup_message.data.available_vars.iter().any(|variable| {
        // FIG-3033: same `output:` type-literal gap as the research corpus.
        variable.name == "standup" && variable.variable_type == "any"
    }));

    server.abort();
}

fn assert_clean_facets(document: &WorkflowDocument) {
    assert_eq!(
        document.facet_schema_version,
        Some(lash::rlm::lang::WORKFLOW_TYPE_FACET_SCHEMA_VERSION)
    );
    assert!(
        document
            .nodes
            .iter()
            .all(|node| node.data.diagnostics.is_empty())
    );
}

fn call_with_field<'a>(
    document: &'a WorkflowDocument,
    field: &str,
) -> &'a workflow_graph_roundtrip::FlowNode {
    document
        .nodes
        .iter()
        .find(|node| node.data.kind == "call" && node.data.fields.contains_key(field))
        .unwrap_or_else(|| panic!("call with `{field}` field"))
}

fn call_with_operation<'a>(
    document: &'a WorkflowDocument,
    operation: &str,
) -> &'a workflow_graph_roundtrip::FlowNode {
    document
        .nodes
        .iter()
        .find(|node| node.data.kind == "call" && node.data.operation.as_deref() == Some(operation))
        .unwrap_or_else(|| panic!("call to `{operation}`"))
}

fn call_with_task<'a>(
    document: &'a WorkflowDocument,
    task_prefix: &str,
) -> &'a workflow_graph_roundtrip::FlowNode {
    document
        .nodes
        .iter()
        .find(|node| {
            node.data.kind == "call"
                && node
                    .data
                    .fields
                    .get("task")
                    .is_some_and(|value| {
                        matches!(value, EditableValue::String(task) if task.starts_with(task_prefix))
                    })
        })
        .unwrap_or_else(|| panic!("call with task starting `{task_prefix}`"))
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
