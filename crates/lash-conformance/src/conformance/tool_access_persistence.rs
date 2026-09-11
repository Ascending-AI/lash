use super::*;

fn definition(id: &str, name: &str) -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        id,
        name,
        format!("{name} definition"),
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "string" }),
    )
}

struct ResidentProvider;

#[async_trait::async_trait]
impl crate::ToolProvider for ResidentProvider {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![definition("tool:recovery-resident", "recovery_resident").manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "recovery_resident")
            .then(|| Arc::new(definition("tool:recovery-resident", "recovery_resident").contract()))
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolOutcome {
        crate::ToolOutcome::ok(serde_json::json!("resident"))
    }
}

fn assert_restricted_empty_catalog(access: crate::SessionToolAccess, session_id: &str) {
    let mut factories = lash_core::testing::test_standard_protocol_factories();
    factories.push(Arc::new(lash_core::plugin::StaticPluginFactory::new(
        "tool_access_recovery_resident",
        lash_core::plugin::PluginSpec::new().with_tool_provider(Arc::new(ResidentProvider)),
    )));
    let session = lash_core::facade_support::PluginHost::new(factories)
        .build_session_with_parent(
            session_id,
            None,
            lash_core::plugin::SessionCreationConfig {
                authority: lash_core::plugin::SessionAuthorityContext {
                    tool_access: access,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .expect("build session from recovered authority");
    assert!(
        session
            .resolved_tool_catalog(&SessionId::from(session_id))
            .expect("resolve catalog from recovered authority")
            .tools
            .is_empty(),
        "recovered restricted-empty authority must not become ambient"
    );
}

/// Proves explicit resident-tool authority survives a real backend reopen and
/// that historical or invalid authority bytes refuse through production reads.
pub async fn session_tool_access_durable_recovery(
    factory: Arc<dyn crate::store::ConformanceSessionStoreFactory>,
) {
    let session_id = SessionId::from("explicit-tool-access-durable-recovery");
    let request = session_store_factory::session_store_request(
        &session_id,
        "tool-access-model",
        crate::SessionRelation::Root,
    );
    let open = factory
        .create_conformance_store(&request)
        .await
        .expect("create explicit-tool-access store");
    let mut state = crate::RuntimeSessionState {
        session_id: session_id.clone(),
        ..crate::RuntimeSessionState::new(request.policy.clone())
    };
    state.authority.tool_access =
        crate::SessionToolAccess::restricted([]).expect("restricted empty is valid");
    assert_restricted_empty_catalog(
        state.authority.tool_access.clone(),
        "restricted-empty-before-recovery",
    );
    open.commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("commit restricted-empty authority");
    drop(open);

    let reopened = factory
        .open_existing_conformance_store(&request)
        .await
        .expect("reopen explicit-tool-access store")
        .expect("committed session exists");
    let loaded = crate::store::load_persisted_session(reopened.as_ref())
        .await
        .expect("load restricted-empty authority after reopen")
        .expect("committed session state");
    assert_eq!(
        loaded
            .state
            .authority
            .tool_access
            .restricted_tools()
            .expect("restricted mode")
            .len(),
        0,
        "restricted-empty authority survives durable recovery"
    );
    assert_restricted_empty_catalog(
        loaded.state.authority.tool_access.clone(),
        "restricted-empty-after-recovery",
    );

    let predecessor = crate::store::SESSION_HEAD_META_SCHEMA_VERSION - 1;
    reopened
        .rewrite_session_tool_access_for_testing(predecessor, Some(serde_json::json!({})))
        .await
        .expect("write predecessor head bytes");
    let error = reopened
        .load_session()
        .await
        .expect_err("the predecessor session-head format must refuse");
    assert!(matches!(
        error,
        crate::StoreError::UnsupportedRecordSchemaVersion {
            record_kind: "SessionHeadMeta",
            actual,
            expected,
        } if actual == predecessor && expected == crate::store::SESSION_HEAD_META_SCHEMA_VERSION
    ));

    let duplicate_name = serde_json::json!({
        "mode": "restricted",
        "tools": [definition("tool:first", "same"), definition("tool:second", "same")]
    });
    let duplicate_id = serde_json::json!({
        "mode": "restricted",
        "tools": [definition("tool:same", "first"), definition("tool:same", "second")]
    });
    let empty_name = serde_json::json!({
        "mode": "restricted",
        "tools": [definition("tool:empty", " ")]
    });
    let empty_hidden_name = serde_json::json!({
        "mode": "ambient",
        "hidden_tools": [" "]
    });
    let duplicate_hidden_name = serde_json::json!({
        "mode": "ambient",
        "hidden_tools": ["same", "same"]
    });
    let invalid = [
        ("missing", None),
        ("null", Some(serde_json::Value::Null)),
        (
            "unknown-tag",
            Some(serde_json::json!({ "mode": "unknown" })),
        ),
        (
            "malformed-restricted",
            Some(serde_json::json!({ "mode": "restricted", "tools": "all" })),
        ),
        ("empty-name", Some(empty_name)),
        ("empty-hidden-name", Some(empty_hidden_name)),
        ("duplicate-name", Some(duplicate_name)),
        ("duplicate-id", Some(duplicate_id)),
        ("duplicate-hidden-name", Some(duplicate_hidden_name)),
    ];
    for (label, access) in invalid {
        reopened
            .rewrite_session_tool_access_for_testing(
                crate::store::SESSION_HEAD_META_SCHEMA_VERSION,
                access,
            )
            .await
            .unwrap_or_else(|error| panic!("write {label} authority bytes: {error}"));
        let error = reopened
            .load_session()
            .await
            .expect_err("invalid authority bytes must refuse");
        let expected_refusal = match &error {
            crate::StoreError::StoredDataCorrupt {
                record_kind: "SessionHeadMeta",
                ..
            } => true,
            crate::StoreError::Backend(message) => {
                message.contains("failed to decode SessionHeadMeta")
            }
            _ => false,
        };
        assert!(
            expected_refusal,
            "{label} authority returned the wrong refusal: {error:?}"
        );
    }
}
