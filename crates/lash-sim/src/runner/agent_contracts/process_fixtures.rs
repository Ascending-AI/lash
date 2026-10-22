//! Fixed process programs use the immutable definition records introduced by
//! FIG-4177 (bf41d19ca5), then assert their existing Agent behavior contracts.

use super::*;

pub(super) async fn agent_started_process_tool_call_graph_execution()
-> Result<Value, FixedScriptRunnerError> {
    let expected = json!({ "ok": true });
    let result = facade_agent_process_execution(
        "lash_runtime agent started process tool",
        &SessionId::from("sim-agent-started-process-tool-contract"),
        "Start a process that calls the app lookup tool.",
        vec![
            r#"<typescript>
const definition = async (): Promise<{ ok: boolean }> => {
  const value = await tools.appLookup({});
  return value;
};
const handle = await processes.start({ definition });
const result = await handle;
await control.finish(result);
</typescript>"#,
        ],
        &expected,
        Some(Arc::new(ContractAppTools) as Arc<dyn lash_core::ToolProvider>),
    )
    .await?;
    Ok(result)
}

pub(super) async fn agent_durable_input_suspension_resolution_execution()
-> Result<Value, FixedScriptRunnerError> {
    let result = facade_agent_durable_input_execution(std::time::Duration::ZERO).await?;
    Ok(result)
}

pub(super) async fn agent_nested_process_start_await_execution()
-> Result<Value, FixedScriptRunnerError> {
    let expected = json!({ "parent": "done" });
    let result = facade_agent_process_execution(
        "lash_runtime agent nested process",
        &SessionId::from("sim-agent-nested-process-contract"),
        "Start a parent process that starts and awaits a child process.",
        vec![
            r#"<typescript>
const parent = async (): Promise<{ parent: string }> => {
  const handle = await processes.start({
    definition: async (): Promise<{ child: string }> => { return { child: 'done' }; }
  });
  const inner = await handle;
  return { parent: inner.child };
};
const handle = await processes.start({ definition: parent });
const result = await handle;
await control.finish(result);
</typescript>"#,
        ],
        &expected,
        None,
    )
    .await?;
    Ok(result)
}

pub(super) async fn agent_started_process_child_spawn_execution()
-> Result<Value, FixedScriptRunnerError> {
    let expected = json!({ "len": 2 });
    let result = facade_agent_process_execution_with_options(
        "lash_runtime agent started process subagent",
        &SessionId::from("sim-agent-started-process-subagent-contract"),
        "Run a Lash VM process that spawns a subagent and returns its value.",
        vec![
            r#"<typescript>
const spawnChild = async (): Promise<{ len: number }> => {
  const result = await agents.spawn({
    task: "Finish `{ len: chunk.length }` using the seeded `chunk` variable.",
    seed: { chunk: ["a", "b"] },
    output: { len: "int" }
  });
  return result;
};
const handle = await processes.start({ definition: spawnChild });
const result = await handle;
await control.finish(result);
</typescript>"#,
            r#"<typescript>
await control.finish({ len: chunk.length });
</typescript>"#,
        ],
        &expected,
        None,
        true,
        None,
    )
    .await?;
    Ok(result)
}

pub(super) async fn agent_session_turn_process_child_execution()
-> Result<Value, FixedScriptRunnerError> {
    let expected = json!({ "child": "done" });
    let result = facade_final_value_execution_with_process_surface(
        "lash_runtime agent session-turn process child",
        &SessionId::from("sim-agent-session-turn-process-child-contract"),
        "Start a child process and await its result.",
        r#"<typescript>
const child = async (): Promise<{ child: string }> => { return { child: 'done' }; };
const handle = await processes.start({ definition: child });
const result = await handle;
await control.finish(result);
</typescript>"#,
        &expected,
    )
    .await?;
    Ok(result)
}

pub(super) async fn agent_parallel_spawn_and_join_execution()
-> Result<Value, FixedScriptRunnerError> {
    let expected = json!({ "joined": ["left", "right"] });
    let result = facade_final_value_execution_with_process_surface(
        "lash_runtime agent parallel process join",
        &SessionId::from("sim-agent-parallel-spawn-join-contract"),
        "Start two processes, await both, and finish their joined result.",
        r#"<typescript>
const child = async (value: string): Promise<string> => { return value; };
const left = await processes.start({ definition: child, args: { value: "left" } });
const right = await processes.start({ definition: child, args: { value: "right" } });
const leftValue = await left;
const rightValue = await right;
await control.finish({ joined: [leftValue, rightValue] });
</typescript>"#,
        &expected,
    )
    .await?;
    Ok(result)
}

/// The contract host's session/process twins advertise the same operations.
pub(super) fn contract_tool_plugin(
    owner: &'static str,
    tools: Arc<dyn lash_core::ToolProvider>,
) -> Result<lash::plugins::StaticPluginFactory, FixedScriptRunnerError> {
    let roots = tools
        .tool_manifests(&lash_core::OwnerConfigView::default())
        .map_err(|error| FixedScriptRunnerError::Runtime(error.to_string()))?
        .into_iter()
        .map(|manifest| manifest.operation.package.as_slice()[0].clone())
        .collect();
    Ok(lash::plugins::StaticPluginFactory::new(
        lash::plugins::PluginDeclaration::initial(owner),
        lash::plugins::PluginSpec::new()
            .with_package_provider(
                lash::tool_provider_key!("tools"),
                Arc::new(lash::package::ToolPackageProvider::new(
                    owner,
                    lash::tool_provider_key!("tools"),
                    roots,
                    Arc::clone(&tools),
                )),
            )
            .with_tool_provider(lash::tool_provider_key!("tools"), tools),
    ))
}

/// Keep the actual cell refusal in a failed scripted contract's evidence.
pub(super) async fn contract_cell_failures(events: &RuntimeProofRecordingEvents) -> Vec<String> {
    events
        .snapshot()
        .await
        .iter()
        .filter_map(|activity| match &activity.event {
            lash::TurnEvent::CodeBlockCompleted {
                result: lash::transcript::CellOutcome::Failed(error),
                ..
            } => Some(format!("{error:?}")),
            _ => None,
        })
        .collect()
}
