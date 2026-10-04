use lash::plugins::{CompactionContext, ContextPressureContext, PluginCommandContext, TurnTransformContext};

fn command(ctx: &PluginCommandContext) {
    let _ = &ctx.scoped_effect_controller;
}

fn transform(ctx: &TurnTransformContext<'_>) {
    let _ = &ctx.tools;
}

fn compaction(ctx: &CompactionContext<'_>) {
    let _ = &ctx.tools;
}

fn pressure(ctx: &ContextPressureContext<'_>) {
    let _ = &ctx.tools;
}

fn main() {}
