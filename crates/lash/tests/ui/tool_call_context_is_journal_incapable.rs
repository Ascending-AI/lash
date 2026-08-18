// A recorded leaf attempt always receives the sealed `AttemptContext`: there is
// no provider opt-in that swaps `ToolCall::context` back to a journal-capable
// `ToolContext`, so none of these routes exist on a tool body.
async fn leaf_tool_body(call: lash::tools::ToolCall<'_>) {
    let _ = call.context.triggers();
    let _ = call.context.process_events();
    let _ = call.context.dispatch();
    let _ = call.context.process_admin();
    let _ = call.context.sessions().start_turn();
}

fn main() {}
