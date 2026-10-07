use super::*;
use lash_core::ToolCallOutcome;

async fn prepare(
    provider: &impl lash_core::ToolProvider,
    manifest: &lash_core::ToolManifest,
) -> lash_core::PreparedToolCall {
    let context = lash_core::ToolPrepareContext::for_testing(
        lash_core::RuntimeOwner::Session("mcp-admitted-payload".into()),
        Arc::new(lash_core::testing::MockSessionManager::default()),
        None,
    );
    provider
        .prepare_tool_call(lash_core::ToolPrepareCall {
            tool_id: manifest.id.clone(),
            pending: lash_core::sansio::PendingToolCall {
                call_id: context.call_id().clone(),
                provider_call_id: None,
                tool_name: manifest.name.clone(),
                args: json!({}),
                replay: None,
            },
            context: &context,
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn l12_mcp_cold_registry_cannot_replace_the_recorded_preparation_binding() {
    use lash_core::ToolProvider;
    let scratch = tempfile::tempdir().unwrap();
    let pool = connect_mock(
        scratch.path(),
        MockOptions {
            behavior: "success",
            ..Default::default()
        },
    )
    .await;
    let provider = crate::McpToolProvider::new(Arc::clone(&pool));
    let original = provider.tool_manifests().remove(0);
    let prepared = prepare(&provider, &original).await;
    let grant = crate::McpDeferredToolProvider::new(Arc::clone(&pool));
    let grant_fixture = lash_core::testing::ToolCallFixture::mock()
        .prepared_call(&prepared)
        .execution_binding(json!({"kind": "mcp", "server": "mock", "tool_id": original.id}));
    let grant_context = grant_fixture.attempt("mcp-canonical-grant-law");
    let lash_core::ToolAttemptOutcome::Done { result, .. } = grant
        .execute(lash_core::ToolCall::new(
            &original,
            &json!({}),
            &grant_context,
        ))
        .await
    else {
        panic!("a catalog grant finishes inline")
    };
    assert!(ToolOutcome::from_output(result.into_output()).is_success());
    let recorded = serde_json::to_value(&prepared).unwrap();
    let mut replacement = original.clone();
    replacement
        .bindings
        .get_mut(admission::MCP_BINDING_KEY)
        .unwrap()["tool_digest"] = json!("cold-registry-revision");
    pool.entries.read_recover()["mock"]
        .imported_tools
        .write_recover()
        .values_mut()
        .next()
        .unwrap()
        .definition
        .manifest = replacement;
    let rebuilt = provider.tool_manifests().remove(0);
    let restored: lash_core::PreparedToolCall = serde_json::from_value(recorded).unwrap();
    let fixture = lash_core::testing::ToolCallFixture::mock().prepared_call(&restored);
    let context = fixture.attempt("mcp-cold-binding-law");
    let lash_core::ToolAttemptOutcome::Done { result, .. } = provider
        .execute(lash_core::ToolCall::new(&rebuilt, &json!({}), &context))
        .await
    else {
        panic!("binding refusal finishes inline")
    };
    let result = ToolOutcome::from_output(result.into_output());
    assert_eq!(failure(&result).code, "mcp_execution_binding_changed");
    pool.shutdown_all().await;
    assert_eq!(
        received(scratch.path()).matches("tools/call").count(),
        1,
        "the cold refusal sends no fresh request"
    );
}

#[tokio::test]
async fn l02_mcp_redelivery_preserves_the_lash_call_identity_on_the_wire() {
    let scratch = tempfile::tempdir().unwrap();
    let pool = connect_mock(
        scratch.path(),
        MockOptions {
            behavior: "success",
            ..Default::default()
        },
    )
    .await;
    let call_id = lash_core::ToolCallId::fixture("mcp-interrupted-attempt");
    let fixture = lash_core::testing::ToolCallFixture::mock().call_id(call_id.clone());
    let context = fixture.attempt("mcp-wire-law");
    let name = mcp_name("mock", "work");
    for _ in 0..2 {
        assert!(
            pool.call_tool(&name, &json!({}), &context)
                .await
                .is_success()
        );
    }
    pool.shutdown_all().await;
    let requests: Vec<serde_json::Value> =
        std::fs::read_to_string(scratch.path().join("received.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .filter(|request: &serde_json::Value| request["method"] == "tools/call")
            .collect();
    assert_eq!(requests.len(), 2);
    assert_ne!(
        requests[0]["id"], requests[1]["id"],
        "JSON-RPC only correlates a delivery"
    );
    for request in requests {
        assert_eq!(
            request["params"]["_meta"]["lash.dev/tool-call-id"],
            call_id.to_string()
        );
        assert_eq!(request["params"]["_meta"]["lash.dev/tool-attempt"], 1);
    }
}

#[tokio::test]
async fn l12_mcp_refuses_changed_or_corrupt_admitted_bindings_before_send() {
    use lash_core::ToolProvider;
    let scratch = tempfile::tempdir().unwrap();
    let pool = connect_mock(
        scratch.path(),
        MockOptions {
            behavior: "success",
            ..Default::default()
        },
    )
    .await;
    let provider = crate::McpToolProvider::new(Arc::clone(&pool));
    let original = provider.tool_manifests().remove(0);
    let prepared = prepare(&provider, &original).await;
    let fixture = lash_core::testing::ToolCallFixture::mock().prepared_call(&prepared);
    let context = fixture.attempt("mcp-binding-refusal-law");
    let key = admission::MCP_BINDING_KEY;
    for payload in [Value::Null, json!({"server": "mock"})] {
        let missing = prepared.clone().with_prepared_payload(payload);
        let fixture = lash_core::testing::ToolCallFixture::mock().prepared_call(&missing);
        let context = fixture.attempt("mcp-canonical-refusal-law");
        let lash_core::ToolAttemptOutcome::Done { result, .. } = provider
            .execute(lash_core::ToolCall::new(&original, &json!({}), &context))
            .await
        else {
            panic!("a missing canonical binding refuses inline")
        };
        assert_eq!(
            failure(&ToolOutcome::from_output(result.into_output())).code,
            "mcp_invalid_execution_binding"
        );
    }
    for field in [
        "server",
        "native_tool",
        "transport_digest",
        "peer_digest",
        "tool_digest",
        "call_policy",
    ] {
        let mut admitted = original.clone();
        let mut binding = admitted.bindings[key].clone();
        if field == "call_policy" {
            binding[field]["call_timeout_ms"] = json!(987);
        } else {
            binding[field] = json!("unavailable");
        }
        admitted.bindings.insert(key.into(), binding);
        let attempt = provider
            .execute(lash_core::ToolCall::new(&admitted, &json!({}), &context))
            .await;
        let lash_core::ToolAttemptOutcome::Done { result, .. } = attempt else {
            panic!("binding refusal must finish inline")
        };
        let result = ToolOutcome::from_output(result.into_output());
        assert_eq!(
            failure(&result).code,
            "mcp_execution_binding_changed",
            "{field}"
        );
        assert_eq!(
            failure(&result).raw.as_ref().unwrap().to_json_value()["kind"],
            json!("execution_binding_changed")
        );
    }
    for binding in [Value::Null, json!({"server": "mock"})] {
        let mut admitted = original.clone();
        admitted.bindings.insert(key.into(), binding);
        let attempt = provider
            .execute(lash_core::ToolCall::new(&admitted, &json!({}), &context))
            .await;
        let lash_core::ToolAttemptOutcome::Done { result, .. } = attempt else {
            panic!("corrupt binding must finish inline")
        };
        let result = ToolOutcome::from_output(result.into_output());
        assert_eq!(failure(&result).code, "mcp_invalid_execution_binding");
    }
    let mut replacement = original.clone();
    replacement.bindings.get_mut(key).unwrap()["tool_digest"] = json!("new-server-revision");
    pool.entries.read_recover()["mock"]
        .imported_tools
        .write_recover()
        .values_mut()
        .next()
        .unwrap()
        .definition
        .manifest = replacement;
    let attempt = provider
        .execute(lash_core::ToolCall::new(&original, &json!({}), &context))
        .await;
    let lash_core::ToolAttemptOutcome::Done { result, .. } = attempt else {
        panic!("a refreshed tool cannot replace an admitted binding")
    };
    assert_eq!(
        failure(&ToolOutcome::from_output(result.into_output())).code,
        "mcp_execution_binding_changed"
    );
    pool.shutdown_all().await;
    assert!(
        !received(scratch.path())
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .any(|request| request["method"] == "tools/call")
    );
}

#[tokio::test]
async fn l07_l08_mcp_sockets_refuse_remote_defer_and_isolation_before_send() {
    use lash_core::ToolProvider;
    let scratch = tempfile::tempdir().unwrap();
    let pool = connect_mock(
        scratch.path(),
        MockOptions {
            behavior: "success",
            ..Default::default()
        },
    )
    .await;
    let provider = crate::McpToolProvider::new(Arc::clone(&pool));
    let original = provider.tool_manifests().remove(0);
    let prepared = prepare(&provider, &original).await;
    let fixture = lash_core::testing::ToolCallFixture::mock().prepared_call(&prepared);
    let context = fixture.attempt("mcp-unsupported-law");
    for mode in ["deferred", "task", "isolated"] {
        let mut manifest = original.clone();
        match mode {
            "deferred" => manifest.declaration.may_defer = true,
            "isolated" => manifest.declaration.isolated = true,
            _ => {
                manifest
                    .bindings
                    .get_mut(admission::MCP_BINDING_KEY)
                    .unwrap()["completion"] = json!("unsupported_task");
            }
        }
        let attempt = provider
            .execute(lash_core::ToolCall::new(&manifest, &json!({}), &context))
            .await;
        let lash_core::ToolAttemptOutcome::Done { result, .. } = attempt else {
            panic!("unsupported work never becomes Deferred")
        };
        let result = ToolOutcome::from_output(result.into_output());
        if mode == "isolated" {
            assert_eq!(
                failure(&result).cause.as_deref(),
                Some(&lash_core::ToolFailureCause::Admission {
                    refusal: lash_core::ToolAdmissionRefusal::UnsupportedIsolation
                })
            );
        } else {
            assert_eq!(failure(&result).code, "mcp_unsupported_remote_completion");
        }
    }
    let grant = crate::McpDeferredToolProvider::new(Arc::clone(&pool));
    let wrong_server = lash_core::testing::mock_attempt_context_with_execution_binding(
        json!({"kind": "mcp", "tool_id": original.id, "server": "other"}),
    );
    let attempt = grant
        .execute(lash_core::ToolCall::new(
            &original,
            &json!({}),
            &wrong_server,
        ))
        .await;
    let lash_core::ToolAttemptOutcome::Done { result, .. } = attempt else {
        panic!("a grant binds its server")
    };
    assert_eq!(
        failure(&ToolOutcome::from_output(result.into_output())).code,
        "mcp_invalid_execution_binding"
    );
    pool.shutdown_all().await;
    assert!(!received(scratch.path()).contains("tools/call"));
}

#[tokio::test]
async fn l03_mcp_cancellation_never_claims_remote_success_or_termination() {
    let scratch = tempfile::tempdir().unwrap();
    let clock = scripted::Clock::new().await;
    let (pool, mut mock) = scripted::Mock::connect(scratch.path(), MockOptions::default()).await;
    let token = tokio_util::sync::CancellationToken::new();
    let fixture =
        lash_core::testing::ToolCallFixture::mock().cancellation_token(Some(token.clone()));
    let context = fixture.attempt("mcp-cancel-law");
    let name = mcp_name("mock", "work");
    let args = json!({});
    let call = pool.call_tool(&name, &args, &context);
    tokio::pin!(call);
    assert!(futures_util::poll!(call.as_mut()).is_pending());
    mock.started(&pool).await;
    token.cancel();
    let outcome = call.await;
    mock.event("cancelled").await;
    mock.command("reply").await;
    peer(&pool)
        .await
        .send_request(ClientRequest::PingRequest(PingRequest::default()))
        .await
        .unwrap();
    assert!(matches!(
        outcome.as_done_output().unwrap().outcome,
        ToolCallOutcome::Cancelled(_)
    ));
    assert!(matches!(
        pool.call_tool(&name, &json!({}), &context)
            .await
            .as_done_output()
            .unwrap()
            .outcome,
        ToolCallOutcome::Cancelled(_)
    ));
    drop(clock);
    pool.shutdown_all().await;
    mock.event("eof").await;
}

#[tokio::test]
async fn l07_mcp_required_remote_tasks_refuse_during_admission() {
    use lash_core::ToolProvider;
    let scratch = tempfile::tempdir().unwrap();
    let pool = connect_mock(
        scratch.path(),
        MockOptions {
            behavior: "success",
            ..Default::default()
        },
    )
    .await;
    let entry = entry(&pool);
    let remote: rmcp::model::Tool = serde_json::from_value(json!({
        "name": "work", "inputSchema": {"type": "object"},
        "execution": {"taskSupport": "required"},
    }))
    .unwrap();
    let imported = import_tools("mock", vec![remote]).unwrap();
    let service = entry.service_snapshot().unwrap();
    let imported = admission::bind_imported_tools(imported, &entry, &service.peer).unwrap();
    entry.replace_imported_tools(imported).unwrap();
    let provider = crate::McpToolProvider::new(Arc::clone(&pool));
    let manifest = provider.tool_manifests().remove(0);
    let context = lash_core::ToolPrepareContext::for_testing(
        lash_core::RuntimeOwner::Session("mcp-task-refusal".into()),
        Arc::new(lash_core::testing::MockSessionManager::default()),
        None,
    );
    let result = provider
        .prepare_tool_call(lash_core::ToolPrepareCall {
            tool_id: manifest.id,
            pending: lash_core::sansio::PendingToolCall {
                call_id: context.call_id().clone(),
                provider_call_id: None,
                tool_name: manifest.name,
                args: json!({}),
                replay: None,
            },
            context: &context,
        })
        .await
        .unwrap_err();
    assert_eq!(failure(&result).code, "mcp_unsupported_remote_completion");
    pool.shutdown_all().await;
    assert!(!received(scratch.path()).contains("tools/call"));
}

#[tokio::test]
async fn l22_mcp_transport_timeout_is_an_inline_body_failure() {
    use lash_core::ToolProvider;
    let scratch = tempfile::tempdir().unwrap();
    let clock = scripted::Clock::new().await;
    let (pool, mut mock) = scripted::Mock::connect(scratch.path(), MockOptions::default()).await;
    let provider = crate::McpToolProvider::new(Arc::clone(&pool));
    let manifest = provider.tool_manifests().remove(0);
    let prepared = prepare(&provider, &manifest).await;
    let fixture = lash_core::testing::ToolCallFixture::mock().prepared_call(&prepared);
    let context = fixture.attempt("mcp-body-timeout-law");
    let args = json!({});
    let request = provider.execute(lash_core::ToolCall::new(&manifest, &args, &context));
    tokio::pin!(request);
    assert!(futures_util::poll!(request.as_mut()).is_pending());
    mock.started(&pool).await;
    assert!(futures_util::poll!(request.as_mut()).is_pending());
    tokio::time::advance(Duration::from_millis(150) + scripted::TIMER_TICK).await;
    let lash_core::ToolAttemptOutcome::Done { result, intents } = request.await else {
        panic!("a body transport timeout supplies no durable source or process");
    };
    assert!(intents.is_empty());
    let result = ToolOutcome::from_output(result.into_output());
    assert_eq!(failure(&result).class, ToolFailureClass::Timeout);
    assert_eq!(failure(&result).code, "mcp_call_timeout");
    assert_eq!(
        failure(&result).raw.as_ref().unwrap().to_json_value()["kind"],
        "call_timeout"
    );
    mock.event("cancelled").await;
    drop(clock);
    pool.shutdown_all().await;
}
