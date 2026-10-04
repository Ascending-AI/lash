use lash::plugins::{CheckpointHookContext, TurnHookContext, TurnResultHookContext};

fn before(ctx: &TurnHookContext) {
    let _ = ctx.sessions.set_tool_membership(&ctx.session_id, &[], false);
    let _ = ctx.sessions.apply_tool_state(&ctx.session_id, lash::tools::ToolState::default());
}
fn after(ctx: &TurnResultHookContext) {
    let _ = ctx.sessions.set_tool_membership(&ctx.session_id, &[], false);
    let _ = &ctx.session_graph;
}
fn checkpoint(ctx: &CheckpointHookContext) {
    let _ = ctx.sessions.set_tool_membership(&ctx.session_id, &[], false);
    let _ = &ctx.session_lifecycle;
    let _ = &ctx.session_graph;
}
fn main() {}
