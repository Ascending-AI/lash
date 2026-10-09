#[path = "support/runtime.rs"]
mod runtime;
use serde_json::Value;
use workflow_graph_roundtrip::{EditableValue, WorkflowDocument};

#[tokio::test]
async fn mocked_tool_schemas_project_into_seed_workflow_facets() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("test listener address");
    let state = runtime::state().await;
    let server = tokio::spawn(workflow_graph_roundtrip::serve(listener, state));
    let client = reqwest::Client::new();
    let base = format!("http://{addr}");

    let emails = select_workflow(&client, &base, "summarize-emails").await;
    assert_clean_facets(&emails);
    let summarize = call_with_task(&emails, "Summarize this email");
    assert!(summarize.data.expected_arg_types.iter().any(|argument| {
        argument.slot == "arg[0][\"task\"]" && argument.expected_type == "str"
    }));
    // The digest call shares the list with a tool that may replace its items.
    // FIG-4238 therefore opens the element type, including inside the loop.
    assert_eq!(
        summarize
            .data
            .available_vars
            .iter()
            .find(|variable| variable.name == "email")
            .map(|variable| variable.variable_type.as_str()),
        Some("any"),
        "shared-list loop facets: {:?}",
        summarize.data.available_vars
    );
    let unshared_source = emails.source.replace("summaries: emails", "summaries: []");
    assert_ne!(
        unshared_source, emails.source,
        "the list escape must be removed"
    );
    let response = client
        .post(format!("{base}/project"))
        .json(&serde_json::json!({ "source": unshared_source }))
        .send()
        .await
        .expect("POST workflow without a list escape");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.expect("unshared-list projection");
    let unshared: WorkflowDocument =
        serde_json::from_value(body["document"].clone()).expect("unshared-list document");
    assert_clean_facets(&unshared);
    let unshared_summarize = call_with_task(&unshared, "Summarize this email");
    assert_eq!(
        unshared_summarize
            .data
            .available_vars
            .iter()
            .find(|variable| variable.name == "email")
            .map(|variable| variable.variable_type.as_str()),
        Some("dict"),
        "unshared-list loop facets: {:?}",
        unshared_summarize.data.available_vars
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
        Some(lash::formats::WORKFLOW_TYPE_FACET_SCHEMA_VERSION)
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
        .find(|node| node.data.kind() == "call" && node.data.fields().contains_key(field))
        .unwrap_or_else(|| panic!("call with `{field}` field"))
}

fn call_with_operation<'a>(
    document: &'a WorkflowDocument,
    operation: &str,
) -> &'a workflow_graph_roundtrip::FlowNode {
    document
        .nodes
        .iter()
        .find(|node| {
            node.data.kind() == "call" && node.data.operation().as_deref() == Some(operation)
        })
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
            node.data.kind() == "call"
                && node
                    .data
                    .fields()
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
