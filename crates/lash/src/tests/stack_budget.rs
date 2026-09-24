use super::*;

#[test]
fn stack_budget_rlm_lashlang_process_turn() -> Result<()> {
    run_async_test_on_stack_budget("stack-budget-rlm-lashlang-process-turn", || async {
        let core = explicit_ephemeral_facets(rlm_core_builder().await)
            .provider(queued_text_provider(vec![typescript_block(
                r#"
const child = async (value) => {
    const lookup = await tools.app_lookup({});
    return { value: value, ok: lookup.ok };
  };

const left = await processes.start({ definition: child, args: { value: "left" } });
const right = await processes.start({ definition: child, args: { value: "right" } });
const joined = { left: await left, right: await right };
finish({
  left: joined.left,
  right: joined.right,
  ok: joined.left.ok && joined.right.ok
});"#,
            )]))
            .model(mock_model_spec())
            .tools(Arc::new(AppTools))
            // ADR 0095: `processes` is catalogue presence, so a scripted cell
            // that authors `processes.start` needs this factory installed.
            .plugin(Arc::new(
                lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(),
            ))
            .build(crate::testing::runtime_lease_owner())?;
        let session = core.session("stack-budget-rlm-lashlang").open().await?;
        let events = RecordingEvents::default();

        let turn = session
            .turn(TurnInput::text("run stack budget process fanout"))
            .stream_to(&events)
            .await?;
        session.refresh_background_graph().await?;

        assert_eq!(
            turn.final_value(),
            Some(&serde_json::json!({
                "left": {
                    "ok": true,
                    "value": "left",
                },
                "right": {
                    "ok": true,
                    "value": "right",
                },
                "ok": true,
            }))
        );
        assert!(
            events
                .snapshot()
                .await
                .iter()
                .any(|activity| matches!(activity.event, TurnEvent::FinalValue { .. }))
        );
        Ok(())
    })
}
