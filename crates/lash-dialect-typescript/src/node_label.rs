//! The TypeScript spelling of a workflow-graph node label.
//!
//! A label is a name a *person* gave a step. The IR has carried one since the
//! retired surface spelled it `@label(title:)`, and the workflow lens still
//! serializes an editor rename through it, so TypeScript needs a form that (a)
//! a renderer can write back into source and (b) a parse reads at the same
//! statement position, without changing what the program does.
//!
//! That form is a one-line JSDoc comment on the statement:
//!
//! ```text
//! /** @label Traffic lights — Cycle a three-light signal twice */
//! await display.set_light({ name: "red", state: "on" });
//! ```
//!
//! An em-dash separates the title from an optional description. A comment is
//! inert by construction: everything that is not exactly this shape — a line
//! comment, a multi-line JSDoc, a `@label` that is not the first thing in the
//! comment, a comment in an expression position — is ordinary trivia and is
//! ignored rather than refused. Nothing about execution, journaling or retry
//! attaches to it; the label travels to the graph node and stops there.

/// The tag that makes a doc comment a label.
const TAG: &str = "@label";

/// Separates the title from the optional description.
const SEPARATOR: char = '—';

/// A label read off a statement's doc comment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NodeLabel {
    pub(crate) title: String,
    pub(crate) description: Option<String>,
}

/// `text` is what sits between `/*` and `*/`, so a JSDoc comment's text starts
/// with the extra `*`.
pub(crate) fn parse_label_comment(text: &str) -> Option<NodeLabel> {
    if text.contains('\n') || text.contains('\r') {
        return None;
    }
    let body = text.strip_prefix('*')?.trim();
    let rest = body.strip_prefix(TAG)?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let (title, description) = match rest.split_once(SEPARATOR) {
        Some((title, description)) => (title.trim(), description.trim()),
        None => (rest.trim(), ""),
    };
    if title.is_empty() {
        return None;
    }
    Some(NodeLabel {
        title: title.to_string(),
        description: (!description.is_empty()).then(|| description.to_string()),
    })
}

pub(crate) fn is_label_comment(text: &str) -> bool {
    !text.contains('\n')
        && !text.contains('\r')
        && text
            .strip_prefix('*')
            .map(str::trim)
            .and_then(|body| body.strip_prefix(TAG))
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_title_and_an_optional_description() {
        assert_eq!(
            parse_label_comment("* @label Traffic lights "),
            Some(NodeLabel {
                title: "Traffic lights".to_string(),
                description: None,
            })
        );
        assert_eq!(
            parse_label_comment("* @label Traffic lights — Cycle the signal twice "),
            Some(NodeLabel {
                title: "Traffic lights".to_string(),
                description: Some("Cycle the signal twice".to_string()),
            })
        );
    }

    #[test]
    fn ordinary_trivia_is_not_a_label() {
        // Not JSDoc.
        assert_eq!(parse_label_comment(" @label Title "), None);
        // Multi-line.
        assert_eq!(parse_label_comment("*\n * @label Title\n "), None);
        // Not the first thing in the comment.
        assert_eq!(parse_label_comment("* Notes @label Title "), None);
        // Tag prefix only.
        assert_eq!(parse_label_comment("* @labelled Title "), None);
        assert_eq!(parse_label_comment("* @label "), None);
        assert_eq!(parse_label_comment("* @label — only a description "), None);
    }
}
