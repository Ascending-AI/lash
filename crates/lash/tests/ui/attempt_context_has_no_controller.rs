// No method or field of an attempt's context yields an effect controller or
// the dispatch context: a body cannot issue a journaled effect of its own.
fn attempt_body(context: &lash::tools::AttemptContext<'_>) {
    let _ = context.effect_controller();
    let _ = context.controller();
    let _ = &context.effect_controller;
    let _ = &context.runtime_dispatch;
    let _ = context.runtime_dispatch();
}

fn main() {}
