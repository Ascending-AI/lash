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

/// Reads the label out of one block comment's inner text, or `None` when the
/// comment is ordinary trivia.
///
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

/// Whether the comment is a label comment at all, used to tell a second
/// `@label` on one statement from ordinary trivia.
pub(crate) fn is_label_comment(text: &str) -> bool {
    !text.contains('\n')
        && !text.contains('\r')
        && text
            .strip_prefix('*')
            .map(str::trim)
            .and_then(|body| body.strip_prefix(TAG))
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
}

/// Renders a label back into the comment a parse reads as that same label.
///
/// `None` means the text has no spelling: it would close the comment, span
/// lines, or — for a title — swallow the description separator, so a render
/// followed by a parse would not return what went in.
pub(crate) fn render_label_comment(title: &str, description: Option<&str>) -> Option<String> {
    let title = title.trim();
    let description = description.map(str::trim).filter(|text| !text.is_empty());
    if title.is_empty() || title.contains(SEPARATOR) || !is_renderable(title) {
        return None;
    }
    if description.is_some_and(|description| !is_renderable(description)) {
        return None;
    }
    Some(match description {
        Some(description) => format!("/** {TAG} {title} {SEPARATOR} {description} */"),
        None => format!("/** {TAG} {title} */"),
    })
}

fn is_renderable(text: &str) -> bool {
    !text.contains('\n') && !text.contains('\r') && !text.contains("*/")
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
        // No title.
        assert_eq!(parse_label_comment("* @label "), None);
        assert_eq!(parse_label_comment("* @label — only a description "), None);
    }

    #[test]
    fn rendering_round_trips_through_the_parse() {
        for (title, description) in [
            ("Traffic lights", None),
            ("Traffic lights", Some("Cycle the signal twice")),
            ("@label", Some("a title that looks like the tag")),
        ] {
            let rendered = render_label_comment(title, description).expect("renderable label");
            let text = rendered
                .strip_prefix("/*")
                .and_then(|text| text.strip_suffix("*/"))
                .expect("a block comment");
            assert_eq!(
                parse_label_comment(text),
                Some(NodeLabel {
                    title: title.to_string(),
                    description: description.map(str::to_string),
                })
            );
        }
    }

    #[test]
    fn text_with_no_spelling_is_refused_rather_than_mangled() {
        assert_eq!(render_label_comment("", None), None);
        assert_eq!(render_label_comment("   ", None), None);
        // The separator in a title would re-read as a description.
        assert_eq!(render_label_comment("Red — amber", None), None);
        // Closing the comment early would change the program.
        assert_eq!(render_label_comment("Close */ me", None), None);
        assert_eq!(render_label_comment("Title", Some("Close */ me")), None);
        assert_eq!(render_label_comment("Title", Some("two\nlines")), None);
    }
}
