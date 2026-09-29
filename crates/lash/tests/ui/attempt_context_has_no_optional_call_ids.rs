// ADR 0117 §5: an attempt names its call by the mandatory lash-minted
// `call_id()`; the optional provider-derived `tool_call_id()` and
// `replay_key()` are deleted.
fn attempt_body(context: &lash::tools::AttemptContext<'_>) {
    let _ = context.tool_call_id();
    let _ = context.replay_key();
}

fn main() {}
