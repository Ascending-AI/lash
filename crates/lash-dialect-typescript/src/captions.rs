//! Captions in TypeScript spelling, shared by the front end, the reader of
//! exported source and the printer.
//!
//! A caption is a JSDoc block, `/** … */`, with only whitespace and no blank
//! line between its end and what it names: a statement, a function
//! declaration, or the `else`, `catch` or `finally` keyword of a clause arm.
//! Its first line is the title; the lines after it, each without its ` * `
//! prefix, are the description. The block's tag section, from its first line
//! that starts with `@`, is not caption text, so a JSDoc of tags alone is no
//! caption. Any other comment, and a JSDoc whose text the caption rule
//! refuses, is ordinary trivia: a caption never refuses a source.

use std::collections::BTreeMap;
use std::ops::Range;

use lash_kernel_doc::{Caption, CaptionKind};
use swc_common::comments::{CommentKind, SingleThreadedComments};

/// The caption syntax the dialect spells, for a host's model prompts.
pub(crate) const GUIDE: &str = "\
Caption a step with a JSDoc block written immediately before it, with no blank line between:
  /** Send the weekly report */
  await mail.send(report);
The first line is the title: one line of at most 200 bytes. Further lines, each starting with ` * `, \
are an optional description of at most 2 KiB:
  /**
   * Send the weekly report
   *
   * Only the team leads receive it.
   */
A caption before a statement names that step; before a top-level function it names the function, \
and the entry function's caption is the workflow's title. Write one before `else`, `catch` or \
`finally` to caption that clause; one before `else if` names the inner `if`. Lines starting with \
`@` (JSDoc tags) are not caption text, `//` and `/* */` comments are ordinary comments, and a \
caption never contains `*/`.";

/// Every kind a TypeScript source spells.
pub(crate) const KINDS: &[CaptionKind] = &CaptionKind::ALL;

/// The comments of one source: where each is, and the caption each JSDoc
/// spells, by the offset of the token it stands immediately before.
#[derive(Clone, Debug, Default)]
pub(crate) struct SourceComments {
    /// Each comment's bytes, ascending and disjoint.
    ranges: Vec<Range<usize>>,
    /// Each caption by the token it names, with where the whitespace and
    /// comments before that token begin.
    captions: BTreeMap<usize, (usize, Caption)>,
}

impl SourceComments {
    /// The comments SWC collected reading `text`, whose offsets their spans
    /// index.
    pub(crate) fn collect(text: &str, comments: SingleThreadedComments) -> Self {
        let (leading, trailing) = comments.take_all();
        let mut all: Vec<_> = leading
            .take()
            .into_values()
            .chain(trailing.take().into_values())
            .flatten()
            .collect();
        all.sort_by_key(|comment| comment.span.lo);
        all.dedup_by_key(|comment| comment.span.lo);
        let ranges: Vec<_> = all
            .iter()
            .map(|comment| comment.span.lo.0 as usize..comment.span.hi.0 as usize)
            .collect();
        let captions = all
            .iter()
            .zip(&ranges)
            .filter(|(comment, _)| comment.kind == CommentKind::Block)
            .filter_map(|(comment, range)| {
                let target = adjacent_token(text, range.end)?;
                let caption = jsdoc(&comment.text)?;
                Some((target, (trivia_start(text, &ranges, range.start), caption)))
            })
            .collect();
        Self { ranges, captions }
    }

    /// Each comment's bytes, ascending.
    pub(crate) fn ranges(&self) -> &[Range<usize>] {
        &self.ranges
    }

    /// The caption standing immediately before the token at `offset`.
    pub(crate) fn before(&self, offset: usize) -> Option<&Caption> {
        self.captions.get(&offset).map(|(_, caption)| caption)
    }

    /// The caption standing immediately before the first token after
    /// `offset`, where code ends: that of the keyword that follows it, when
    /// only whitespace and comments lie between.
    pub(crate) fn before_next(&self, offset: usize) -> Option<&Caption> {
        let (_, (trivia, caption)) = self.captions.range(offset..).next()?;
        (*trivia <= offset).then_some(caption)
    }
}

/// Where the run of whitespace and comments that ends with the comment
/// starting at `start` begins.
fn trivia_start(text: &str, ranges: &[Range<usize>], mut start: usize) -> usize {
    loop {
        let code = text[..start].trim_end_matches(is_whitespace);
        start = code.len();
        match ranges.binary_search_by_key(&start, |range| range.end) {
            Ok(index) => start = ranges[index].start,
            Err(_) => return start,
        }
    }
}

fn is_line_terminator(character: char) -> bool {
    matches!(character, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

fn is_whitespace(character: char) -> bool {
    character.is_whitespace() || character == '\u{feff}'
}

fn whitespace_len(text: &str) -> usize {
    text.find(|character| !is_whitespace(character))
        .unwrap_or(text.len())
}

/// Where the token after a comment ending at `end` starts, when only
/// whitespace with no blank line lies between.
fn adjacent_token(text: &str, end: usize) -> Option<usize> {
    let gap = &text[end.min(text.len())..];
    let gap = &gap[..whitespace_len(gap)];
    let mut terminators = 0;
    let mut characters = gap.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\r' && characters.peek() == Some(&'\n') {
            continue;
        }
        terminators += usize::from(is_line_terminator(character));
    }
    (terminators <= 1 && end + gap.len() < text.len()).then_some(end + gap.len())
}

/// The caption a block comment's text, between `/*` and `*/`, spells: none
/// unless it is a JSDoc whose title and description the caption rule
/// accepts.
fn jsdoc(text: &str) -> Option<Caption> {
    let body = text.strip_prefix('*')?.replace("\r\n", "\n");
    let mut lines = Vec::new();
    for (index, line) in body.split(is_line_terminator).enumerate() {
        let line = line.trim();
        let line = if index == 0 {
            line
        } else {
            line.strip_prefix('*').unwrap_or(line).trim()
        };
        if line.starts_with('@') {
            break;
        }
        lines.push(line);
    }
    let first = lines.iter().position(|line| !line.is_empty())?;
    let title = lines[first];
    let mut description: Vec<&str> = Vec::new();
    for &line in &lines[first + 1..] {
        let blank_run = line.is_empty() && description.last().is_none_or(|last| last.is_empty());
        if !blank_run {
            description.push(line);
        }
    }
    while description.last().is_some_and(|line| line.is_empty()) {
        description.pop();
    }
    let description = (!description.is_empty()).then(|| description.join("\n"));
    Caption::new(title, description).ok()
}

/// `caption` as the JSDoc block that names the construct after it, each
/// line indented by `indent`, ending with the indentation of that construct.
pub(crate) fn spell(caption: &Caption, indent: &str) -> String {
    let Some(description) = caption.description() else {
        return format!("/** {} */\n{indent}", caption.title());
    };
    let mut text = format!("/**\n{indent} * {}\n{indent} *\n", caption.title());
    for line in description.split('\n') {
        text.push_str(indent);
        text.push_str(if line.is_empty() { " *" } else { " * " });
        text.push_str(line);
        text.push('\n');
    }
    text.push_str(indent);
    text.push_str(" */\n");
    text.push_str(indent);
    text
}

/// Which clause arm a caption before its keyword names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Arm {
    Else,
    Catch,
    Finally,
}

/// The captions an authored module spells, by the start of the statement
/// each names, or of the `if` or `try` whose arm it names.
#[derive(Clone, Debug, Default)]
pub(crate) struct Spelled {
    pub(crate) statements: BTreeMap<usize, Caption>,
    pub(crate) arms: BTreeMap<(usize, Arm), Caption>,
}
