use super::super::*;
use crate::plugins::{
    PluginCommand, PluginOperation, PluginOperationFailure, PluginOperationInvokeError,
    PluginOperationOutcome, PluginQuery, PluginRuntimeEvent, PluginTask, SessionParam,
};

use std::time::Duration;

macro_rules! operation {
    ($name:ident, $kind:ident, $wire:literal) => {
        struct $name;
        impl PluginOperation for $name {
            const NAME: &'static str = $wire;
            const DESCRIPTION: &'static str = "Plugin facade acceptance probe";
            const SESSION_PARAM: SessionParam = SessionParam::Required;
            type Args = String;
            type Output = String;
        }
        impl $kind for $name {}
    };
}
operation!(Query, PluginQuery, "accept.query");
operation!(Command, PluginCommand, "accept.command");
operation!(Task, PluginTask, "accept.task");

fn outcome(value: String, label: &str) -> PluginOperationOutcome<String> {
    PluginOperationOutcome::new(value.clone()).with_events(vec![PluginRuntimeEvent::Status {
        key: "accept-probe".into(),
        label: label.into(),
        detail: Some(value),
    }])
}

#[test]
pub(super) fn agent_scenario_plugin_task_query_command() -> Result<()> {
    run_async_test_on_stack_budget("plugin-operations", || async {
        let entered = Arc::new(tokio::sync::Notify::new());
        let task_entered = entered.clone();
        let spec = lash_core::facade_support::PluginSpec::new()
            .with_plugin_query_typed::<Query, _, _>(|ctx, args| async move {
                assert_eq!(ctx.session_id.as_deref(), Some("plugin-accept"));
                Ok(format!("query:{args}"))
            })
            .with_plugin_command_typed::<Command, _, _>(|_, args| async move {
                Ok(outcome(format!("command:{args}"), "recorded"))
            })
            .with_plugin_task_typed::<Task, _, _>(move |ctx, args| {
                let entered = task_entered.clone();
                async move {
                    if args == "cancel-739" {
                        entered.notify_one();
                        ctx.cancellation_token.cancelled().await;
                        return Err(PluginOperationFailure::new("cancelled:cancel-739"));
                    }
                    Ok(outcome(format!("task:{args}"), "completed"))
                }
            });
        let core =
            explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
                .provider(mock_provider())
                .model(mock_model_spec())
                .plugin(Arc::new(StaticPluginFactory::new("accept", spec)))
                .store_factory(Arc::new(
                    crate::persistence::InMemorySessionStoreFactory::new(),
                ))
                .process_registry(Arc::new(TestLocalProcessRegistry::default()))
                .build(crate::testing::runtime_lease_owner())?;
        let session = core.session("plugin-accept").open().await?;
        let ops = session.plugin_operations();
        let probe = || "cobalt-583".to_string();
        let before = session.admin().state().persist_current().await?;
        assert_eq!(ops.query::<Query>(probe()).await?, "query:cobalt-583");
        assert_eq!(
            session
                .admin()
                .state()
                .persist_current()
                .await?
                .session_graph
                .leaf_node_id,
            before.session_graph.leaf_node_id,
            "query must not append history"
        );
        let command = ops.run_command::<Command>(probe()).await?;
        let task = ops.run_task::<Task>(probe()).await?;
        for (receipt, output, label) in [
            (command, "command:cobalt-583", "recorded"),
            (task, "task:cobalt-583", "completed"),
        ] {
            assert_eq!(receipt.output, output);
            assert!(receipt.pending_turn_inputs.is_empty());
            assert_eq!(receipt.events.len(), 1);
            assert_eq!(receipt.events[0].plugin_id, "accept");
            assert!(matches!(&receipt.events[0].value,
                PluginRuntimeEvent::Status { key, label: actual, detail }
                if key == "accept-probe" && actual == label && detail.as_deref() == Some(output)));
        }
        let before_cancel = session.admin().state().persist_current().await?;
        let persisted_events: Vec<_> = before_cancel
            .session_graph
            .nodes
            .iter()
            .filter_map(|node| match node.event() {
                Some(lash_core::SessionHistoryRecord::Protocol(event))
                    if event.plugin_id == "lash.plugin_runtime" =>
                {
                    Some(event.payload.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            persisted_events,
            vec![
                serde_json::json!({"plugin_id": "accept", "event": {
                    "kind": "status", "key": "accept-probe", "label": "recorded", "detail": "command:cobalt-583"
                }}),
                serde_json::json!({"plugin_id": "accept", "event": {
                    "kind": "status", "key": "accept-probe", "label": "completed", "detail": "task:cobalt-583"
                }}),
            ],
            "both owned events persist in operation order"
        );
        let cancel = crate::CancellationToken::new();
        let task_cancel = cancel.clone();
        let running_ops = ops.clone();
        let running = tokio::spawn(async move {
            running_ops
                .run_task_with_cancel::<Task>("cancel-739".into(), task_cancel)
                .await
        });
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .expect("task entered");
        cancel.cancel();
        let error = tokio::time::timeout(Duration::from_secs(5), running)
            .await
            .expect("cancellation settles")
            .expect("task does not panic")
            .expect_err("cancelled task fails");
        assert!(
            matches!(error, EmbedError::Control(PluginOperationInvokeError::Failed(ref message))
            if message == "cancelled:cancel-739")
        );
        assert_eq!(
            session
                .admin()
                .state()
                .persist_current()
                .await?
                .session_graph
                .leaf_node_id,
            before_cancel.session_graph.leaf_node_id,
            "cancelled task emits no success event"
        );
        assert_eq!(
            ops.query::<Query>(probe()).await?,
            "query:cobalt-583",
            "writer released after cancellation"
        );
        Ok(())
    })
}
