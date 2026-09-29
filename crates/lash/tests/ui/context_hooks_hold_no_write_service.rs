// ADR 0001, ADR 0105 §6: a Prompt View transform, a compactor and a
// context-pressure hook hold read-only services. None of their contexts
// reaches a graph append, a session create, a tool-state write or a frame
// switch: a durable context change is a decision core writes.
use lash::plugins::{
    CompactionContext, ContextPressureContext, SessionGraphService, TurnTransformContext,
};

fn transform(ctx: &TurnTransformContext<'_>) {
    let _ = &ctx.session_graph;
    let _ = &ctx.session_lifecycle;
    let _ = &ctx.sessions;
}

fn compactor(ctx: &CompactionContext<'_>) {
    let _ = &ctx.session_graph;
    let _ = &ctx.session_lifecycle;
    let _ = &ctx.sessions;
}

fn pressure_hook(ctx: &ContextPressureContext<'_>) {
    let _ = &ctx.session_graph;
    let _ = &ctx.session_lifecycle;
    let _ = &ctx.sessions;
}

// No service switches frames on a plugin's behalf.
fn graph(graph: &dyn SessionGraphService) {
    let _ = graph.switch_agent_frame();
}

fn main() {}
