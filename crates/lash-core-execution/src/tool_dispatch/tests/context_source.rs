//! Reads the fields of [`ToolDispatchContext`] out of its own source, so the
//! rebind checklist is proven against the declaration rather than a second
//! hand-maintained list.

/// The `pub <name>:` fields of `pub struct ToolDispatchContext`, in order.
pub(super) fn dispatch_context_fields() -> Vec<String> {
    const SOURCE: &str = include_str!("../context.rs");
    let start = SOURCE
        .find("pub struct ToolDispatchContext")
        .expect("ToolDispatchContext is declared in context.rs");
    let body_start = SOURCE[start..]
        .find('{')
        .map(|offset| start + offset)
        .expect("the struct has a body");
    // The struct body ends at the first `}` that begins a line.
    let tail = &SOURCE[body_start..];
    let end = tail
        .find("\n}")
        .map(|offset| body_start + offset)
        .expect("the struct body is closed");
    tail[..end - body_start]
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            line.strip_prefix("pub ")
                .and_then(|rest| rest.split(':').next())
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
        })
        .collect()
}
