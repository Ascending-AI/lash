//! The dialect's caption syntax, which the front end and the reader of
//! generated source share: a run of `#:` comment lines, each alone on its
//! line and all at one indentation, directly above the line that begins
//! what they caption, at the same indentation. The first line is the title
//! and the rest the description; a bare `#:` is a blank line in it.
//!
//! A run placed otherwise, or whose text the caption rule refuses, is an
//! ordinary comment: a caption never refuses a source.

use std::collections::BTreeMap;
use std::ops::Range;

use lash_kernel_doc::Caption;

/// How a model writes captions in this dialect.
pub(crate) const GUIDE: &str = "\
Caption a step with `#:` comment lines directly above it, at its indentation \
and with no blank line between. The first `#:` line is the title, any further \
ones the description, and a bare `#:` a blank line in it:

    #: Fetch the open orders
    #: Only orders not yet shipped.
    orders = await tools.orders.list(status=\"open\")

A caption above an `async def` titles that workflow. Above `else:` (of an `if`, \
`for` or `while`), the first `except` or `finally:` it names that branch; above \
`elif` it names the nested check. A title is one line of at most 200 bytes and \
a description at most 2048 bytes. No line may start with `@` or contain `*/`. \
Docstrings and ordinary `#` comments are not captions.";

const MARKER: &str = "#:";

/// The captions a source spells.
#[derive(Debug, Default)]
pub(crate) struct Spelled {
    /// Each caption, by the byte offset of the first character of the line
    /// it captions.
    pub(crate) captions: BTreeMap<usize, Caption>,
    /// Every `#:` comment, attached or not: none of them is code.
    pub(crate) comments: Vec<Range<usize>>,
}

/// One run of `#:` lines, while it grows.
struct Run<'a> {
    indent: &'a str,
    /// Where the line after the run's last starts.
    next: usize,
    lines: Vec<&'a str>,
}

/// The captions `source` spells, among `comments`: the byte ranges of its
/// comments, ascending, each from its `#` to the end of its line.
pub(crate) fn spelled(source: &str, comments: impl IntoIterator<Item = Range<usize>>) -> Spelled {
    let mut spelled = Spelled::default();
    let mut run: Option<Run<'_>> = None;
    for comment in comments {
        let Some(content) = source[comment.clone()].strip_prefix(MARKER) else {
            continue;
        };
        spelled.comments.push(comment.clone());
        let start = source[..comment.start].rfind('\n').map_or(0, |at| at + 1);
        let indent = &source[start..comment.start];
        let rest = &source[comment.end..];
        let ending = if rest.starts_with("\r\n") {
            Some(2)
        } else if rest.starts_with('\n') {
            Some(1)
        } else {
            rest.is_empty().then_some(0)
        };
        let Some(ending) = ending.filter(|_| indent.bytes().all(|b| b == b' ' || b == b'\t'))
        else {
            // A `#:` after code, or one a lone `\r` ends, is no caption line.
            spelled.attach(source, run.take());
            continue;
        };
        let next = comment.end + ending;
        match &mut run {
            Some(open) if open.next == start && open.indent == indent => {
                open.lines.push(content.trim());
                open.next = next;
            }
            _ => {
                spelled.attach(source, run.take());
                run = Some(Run {
                    indent,
                    next,
                    lines: vec![content.trim()],
                });
            }
        }
    }
    spelled.attach(source, run);
    spelled
}

impl Spelled {
    /// Attaches `run` to the line after it when that line is code at the
    /// run's indentation and the text is a caption.
    fn attach(&mut self, source: &str, run: Option<Run<'_>>) {
        let Some(run) = run else {
            return;
        };
        let at = run.next + run.indent.len();
        let attached = source
            .get(run.next..)
            .and_then(|line| line.strip_prefix(run.indent))
            .is_some_and(|code| {
                !code.starts_with([' ', '\t', '\r', '\n', '#']) && !code.is_empty()
            });
        if !attached {
            return;
        }
        let Some((title, description)) = run.lines.split_first() else {
            return;
        };
        let description = (!description.is_empty()).then(|| description.join("\n"));
        if let Ok(caption) = Caption::new(*title, description) {
            self.captions.insert(at, caption);
        }
    }
}

/// The text of each `#:` line that spells `caption`, in order.
pub(crate) fn lines(caption: &Caption) -> impl Iterator<Item = &str> {
    std::iter::once(caption.title()).chain(
        caption
            .description()
            .into_iter()
            .flat_map(|text| text.split('\n')),
    )
}
