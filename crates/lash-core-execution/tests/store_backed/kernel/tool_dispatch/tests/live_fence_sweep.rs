use super::*;

/// A catalog tool whose body publishes live progress across two awaits and
/// hands its publisher to the law.
struct LiveYieldingTools {
    publishers: Arc<std::sync::Mutex<Vec<crate::ToolLivePublisher>>>,
}

#[async_trait::async_trait]
impl ToolProvider for LiveYieldingTools {
    fn tool_manifests(
        &self,
        _config: &crate::plugin::OwnerConfigView,
    ) -> std::result::Result<
        Vec<crate::ToolManifest>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(manifests(vec![named_beta_tool("live_tool")]))
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &crate::plugin::OwnerConfigView,
    ) -> Option<Arc<crate::ToolContract>> {
        (name == "tools.live_tool").then(|| Arc::new(named_beta_tool("live_tool").contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        let live = call.context.live();
        self.publishers
            .lock()
            .expect("the publisher witness")
            .push(live.clone());
        for step in 0..2 {
            live.publish(crate::ToolLiveFact::Progress {
                progress: f64::from(step),
                total: Some(2.0),
                message: None,
            });
            tokio::task::yield_now().await;
        }
        ToolOutcome::ok(json!("live")).into()
    }
}

/// Ownership at the claim (FIG-6854): an attempt's live slot is claimed with
/// a completion fence the attempt owns, so an attempt dropped at any await,
/// with its body mid-flight or not yet started, seals the call: the publisher
/// keeps no live call that a later fact could still be written to.
#[tokio::test]
async fn a_tool_attempt_dropped_at_each_await_seals_its_live_call() {
    let cut_mid_body = AtomicUsize::new(0);
    lash_core_ids::cancel_sweep::sweep_drops(
        async || {
            let publishers = Arc::default();
            let provider: Arc<dyn ToolProvider> = Arc::new(LiveYieldingTools {
                publishers: Arc::clone(&publishers),
            });
            let context = refusing_dispatch_context(provider_plugins(
                provider,
                crate::plugin::SessionAuthorityContext::fixture(),
            ))
            .await;
            let pending = crate::sansio::PendingToolCall {
                call_id: lash_core_execution::ToolCallId::fixture("live-call"),
                provider_call_id: None,
                tool_name: "tools.live_tool".to_string(),
                args: json!({ "value": "ok" }),
                replay: None,
            };
            let attempt = async move {
                let prepared =
                    match crate::tool_dispatch::prepare_tool_call_with_context(&context, pending)
                        .await
                    {
                        crate::tool_dispatch::ToolPreparationOutcome::Prepared(prepared) => {
                            *prepared
                        }
                        crate::tool_dispatch::ToolPreparationOutcome::Completed(outcome) => {
                            panic!("the call should prepare, got {:?}", outcome.record.output)
                        }
                    };
                let tool_context =
                    crate::testing::ToolCallFixture::from_dispatch(Arc::new(context.clone()))
                        .prepared_call(&prepared);
                let outcome = crate::execute_once(&context, &prepared, tool_context, None).await;
                assert!(
                    matches!(outcome, crate::ToolAttemptOutcome::Done { .. }),
                    "the attempt completes"
                );
            };
            (publishers, attempt)
        },
        async |publishers: Arc<std::sync::Mutex<Vec<crate::ToolLivePublisher>>>, cut| {
            let publishers = publishers.lock().expect("the publisher witness");
            if !cut.completed && !publishers.is_empty() {
                cut_mid_body.fetch_add(1, Ordering::SeqCst);
            }
            for publisher in publishers.iter() {
                assert_eq!(
                    publisher.diagnostics().live_calls,
                    0,
                    "the call is sealed once its attempt is {cut}"
                );
            }
        },
    )
    .await;
    assert!(
        cut_mid_body.load(Ordering::SeqCst) > 0,
        "the sweep dropped an attempt with its body mid-flight"
    );
}
