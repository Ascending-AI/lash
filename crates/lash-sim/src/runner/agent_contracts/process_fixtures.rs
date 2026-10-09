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
const definition = async () => {
  const value = await tools.app_lookup({});
  return value;
};
const handle = await processes.start({ definition });
const result = await handle;
finish(result);
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
const parent = async () => {
  const child = async () => { return { child: 'done' }; };
  const inner = await (await processes.start({ definition: child }));
  return { parent: inner.child };
};
const handle = await processes.start({ definition: parent });
const result = await handle;
finish(result);
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
const spawnChild = async () => {
  const result = await agents.spawn({
    task: "Finish `{ len: chunk.length }` using the seeded `chunk` variable.",
    seed: { chunk: ["a", "b"] },
    output: { len: "int" }
  });
  return result;
};
const handle = await processes.start({ definition: spawnChild });
const result = await handle;
finish(result);
</typescript>"#,
            r#"<typescript>
finish({ len: chunk.length });
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
const child = async () => { return { child: 'done' }; };
const handle = await processes.start({ definition: child });
const result = await handle;
finish(result);
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
const child = async (value: string) => { return value; };
const left = await processes.start({ definition: child, args: { value: "left" } });
const right = await processes.start({ definition: child, args: { value: "right" } });
const leftValue = await left;
const rightValue = await right;
finish({ joined: [leftValue, rightValue] });
</typescript>"#,
        &expected,
    )
    .await?;
    Ok(result)
}
