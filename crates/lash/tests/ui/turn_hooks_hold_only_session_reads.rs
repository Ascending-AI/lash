use lash::plugins::{CheckpointHookContext, TurnHookContext, TurnResultHookContext};

fn before(ctx: &TurnHookContext) {
    let _ = &ctx.session_lifecycle;
    let _ = &ctx.session_graph;
}
fn after(ctx: &TurnResultHookContext) {
    let _ = &ctx.session_lifecycle;
    let _ = &ctx.session_graph;
}
fn checkpoint(ctx: &CheckpointHookContext) {
    let _ = &ctx.session_lifecycle;
    let _ = &ctx.session_graph;
}
fn main() {}
