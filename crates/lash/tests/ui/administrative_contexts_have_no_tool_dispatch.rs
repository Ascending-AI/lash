use lash::plugins::{AttachmentOmissionContext, CompactionContext, ContextPressureContext, PluginCommandContext};

fn command(ctx: &PluginCommandContext) {
    let _ = &ctx.scoped_effect_controller;
}

fn omission(ctx: &AttachmentOmissionContext) {
    let _ = &ctx.tools;
}

fn compaction(ctx: &CompactionContext<'_>) {
    let _ = &ctx.tools;
}

fn pressure(ctx: &ContextPressureContext<'_>) {
    let _ = &ctx.tools;
}

fn main() {}
