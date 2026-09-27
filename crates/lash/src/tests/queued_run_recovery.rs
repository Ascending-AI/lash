use super::*;

struct StopQueuedTool;

#[cfg(feature = "rlm")]
#[async_trait]
impl ToolProvider for StopQueuedTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        AppTools.tool_manifests()
    }
    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        AppTools.resolve_contract(name)
    }
    async fn execute(&self, _: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(serde_json::json!({}))
            .with_control(lash_core::ToolControl::Fail {
                failure: lash_core::ToolFailure::tool(
                    lash_core::ToolFailureClass::Execution,
                    "stopped",
                    "stop this physical turn",
                ),
            })
            .into()
    }
}

#[cfg(feature = "rlm")]
#[tokio::test]
async fn a_stopped_turn_runs_withheld_input_in_a_follow_on() -> Result<()> {
    let durable = Arc::new(StdMutex::new(None::<crate::DurableSession>));
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let provider = crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete({
            let durable = Arc::clone(&durable);
            let requests = Arc::clone(&requests);
            move |request| {
                let durable = Arc::clone(&durable);
                let requests = Arc::clone(&requests);
                async move {
                    let call = {
                        let mut requests = requests.lock_recover();
                        requests.push(request);
                        requests.len()
                    };
                    let source = if call == 1 {
                        let session = durable.lock_recover().clone().unwrap();
                        session
                            .send(TurnInput::text("withheld after tool stop"))
                            .id("withheld-input")
                            .ingress(lash_core::TurnInputIngress::active_turn(
                                lash_core::TurnId::from("stopped-withheld"),
                                lash_core::TurnInputCheckpointBoundary::BeforeCompletion,
                            ))
                            .accepted()
                            .await
                            .unwrap();
                        "await tools.app_lookup({});"
                    } else {
                        assert_eq!(call, 2, "one follow-on consumes the withheld input");
                        "finish('withheld completed');"
                    };
                    Ok(text_response(&typescript_block(source)))
                }
            }
        })
        .build()
        .into_handle();
    // The engine's session drive runs the root and its follow-on (D5).
    let double = restate_double(0x0036_685d).await;
    let core = explicit_ephemeral_facets(rlm_core_builder_over(double.lash_backend()))
        .provider(provider)
        .model(mock_model_spec())
        .tools(Arc::new(StopQueuedTool))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("stopped-withheld").open().await?;
    *durable.lock_recover() = Some(session.durable());
    let output = session
        .send(TurnInput::text("start tool stop"))
        .id("stopped-withheld")
        .output()
        .await?;
    assert_eq!(
        requests.lock_recover().len(),
        2,
        "stopped physical turn must not cancel withheld work: {output:?}"
    );
    assert!(request_text(&requests.lock_recover()[1]).contains("withheld after tool stop"));
    assert_eq!(
        output.final_value(),
        Some(&serde_json::json!("withheld completed"))
    );
    assert!(
        session
            .durable()
            .turn_input_applications()
            .await?
            .iter()
            .any(|application| application.source_key.as_deref() == Some("withheld-input"))
    );
    assert!(session.durable().pending_turn_inputs().await?.is_empty());
    Ok(())
}
