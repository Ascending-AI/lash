use crate::dialect::RlmDialect;
use std::sync::Arc;

/// Give the assembled RLM execution section its dialect identity. The shared
/// template owns the generic heading. If transport is its first subsection,
/// collapse both headings so those instructions become the direct body.
/// Other template sections and host-provided execution headings stay intact.
pub(crate) fn render_system_prompt(prompt: &str, dialect: &dyn RlmDialect) -> Option<Arc<str>> {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return None;
    }
    let mut language = dialect.prompt_vocabulary().language_name.to_owned();
    if let Some(initial) = language.get_mut(..1) {
        initial.make_ascii_uppercase();
    }
    let heading = format!("## {language} execution");
    if let Some(start) = prompt
        .match_indices("## Execution\n")
        .find_map(|(start, _)| {
            (start == 0 || prompt.as_bytes()[start - 1] == b'\n').then_some(start)
        })
    {
        let mut end = start + "## Execution".len();
        for transport in ["### Response shape", "### Tool transport"] {
            let opening = format!("\n\n{transport}\n");
            if prompt[end..].starts_with(&opening) {
                end += opening.len() - 1;
                break;
            }
        }
        let mut rendered = prompt.to_owned();
        rendered.replace_range(start..end, &heading);
        return Some(Arc::from(rendered));
    }
    Some(Arc::from(prompt))
}
