//! Reads the fields of [`ToolDispatchContext`] out of its own source, so the
//! rebind checklist is proven against the declaration rather than a second
//! hand-maintained list.

/// Every named field of `pub struct ToolDispatchContext`, in order — parsed
/// structurally, so visibility, attributes, and multiline declarations are
/// all collected. A private field escapes the checklist exactly as well as a
/// public one, which is why this exists.
pub(super) fn dispatch_context_fields() -> Vec<String> {
    const SOURCE: &str = include_str!("../context.rs");
    let file = syn::parse_file(SOURCE).expect("context.rs parses");
    let item = file
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Struct(item) if item.ident == "ToolDispatchContext" => Some(item),
            _ => None,
        })
        .expect("ToolDispatchContext is declared in context.rs");
    match &item.fields {
        syn::Fields::Named(fields) => fields
            .named
            .iter()
            .map(|field| {
                field
                    .ident
                    .as_ref()
                    .expect("a named field has an ident")
                    .to_string()
            })
            .collect(),
        _ => panic!("ToolDispatchContext must keep named fields"),
    }
}
