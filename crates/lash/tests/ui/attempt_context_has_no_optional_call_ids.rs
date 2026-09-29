// ADR 0117 §5: an attempt names its call by the mandatory lash-minted
// `call_id()`; the optional provider-derived `tool_call_id()` and
// `replay_key()` are deleted.
//
// Held until FIG-4080: the accessors still exist, so this fixture has no
// `.stderr` pin and no registration in `tests/ui.rs`, and no gate compiles
// it. FIG-4080 deletes the accessors, blesses the pin and registers the
// fixture beside the other attempt-context contracts.
fn attempt_body(context: &lash::tools::AttemptContext<'_>) {
    let _ = context.tool_call_id();
    let _ = context.replay_key();
}

fn main() {}
