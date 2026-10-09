//! Reading a cell with SWC: which goal symbol it parses under, and the one
//! lexer defect the front end repairs before the adapter sees the tree.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use swc_common::comments::SingleThreadedComments;
use swc_common::{BytePos, Spanned};
use swc_ecma_ast as swc;
use swc_ecma_parser::error::{Error, SyntaxError};
use swc_ecma_parser::input::Tokens;
use swc_ecma_parser::{Context, Parser, StringInput, Syntax, TsSyntax, lexer::Lexer};

/// The goal symbol a cell was read under.
///
/// A cell is a Script that may `await` at its top level (ADR 0062). ECMA-262
/// has no goal with exactly that shape, so a cell is read under the Module
/// goal first, whose `await` rule is the one top-level `await` needs; a cell
/// that does not parse there, because it uses `await` as an identifier, is
/// read again as a strict Script, where `await` is reserved only inside async
/// functions. A cell that parses under neither goal reports the Module goal's
/// error.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Goal {
    #[default]
    Module,
    Script,
}

pub(super) struct Parsed<'a> {
    pub(super) goal: Goal,
    /// The source the tree's spans index, byte for byte the submitted one
    /// except for the respellings [`respell_escaped_keyword`] and
    /// [`respell_script_await`] make.
    pub(super) text: Cow<'a, str>,
    /// Where an identifier spelled `await` was respelled for SWC to read it
    /// as one: the adapter names the identifier there `await` again.
    pub(super) respelled_awaits: BTreeSet<u32>,
    /// Every respelled word's position, with the error SWC reported there.
    pub(super) respellings: BTreeMap<u32, Error>,
    pub(super) items: Vec<swc::ModuleItem>,
    pub(super) comments: SingleThreadedComments,
}

/// Parses `source` under the first goal, from `first` on, that it satisfies.
/// The error is the first goal's, when none does.
pub(super) fn parse(source: &str, first: Goal) -> Result<Parsed<'_>, Error> {
    let mut text = Cow::Borrowed(source);
    let mut respellings = BTreeMap::new();
    let mut module_errors = None;
    if first == Goal::Module {
        loop {
            match parse_goal(&text, Goal::Module) {
                Ok((items, comments)) => {
                    return Ok(Parsed {
                        goal: Goal::Module,
                        text,
                        respelled_awaits: BTreeSet::new(),
                        respellings,
                        items,
                        comments,
                    });
                }
                Err(errors) if respell_escaped_keywords(&mut text, &errors, &mut respellings) => {}
                Err(errors) => {
                    module_errors = Some(errors);
                    break;
                }
            }
        }
    }
    let mut respelled_awaits = BTreeSet::new();
    loop {
        match parse_goal(&text, Goal::Script) {
            Ok((items, comments)) => {
                return Ok(Parsed {
                    goal: Goal::Script,
                    text,
                    respelled_awaits,
                    respellings,
                    items,
                    comments,
                });
            }
            Err(errors)
                if respell_escaped_keywords(&mut text, &errors, &mut respellings)
                    || respell_script_await(
                        &mut text,
                        &errors,
                        &mut respelled_awaits,
                        &mut respellings,
                    ) => {}
            Err(errors) => {
                return Err(module_errors
                    .unwrap_or(errors)
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| unreachable!("a failed parse reports an error")));
            }
        }
    }
}

type GoalResult = Result<(Vec<swc::ModuleItem>, SingleThreadedComments), Vec<Error>>;

fn parse_goal(text: &str, goal: Goal) -> GoalResult {
    let end = u32::try_from(text.len()).unwrap_or(u32::MAX);
    // Comments are trivia to the language and carry no semantics, but one
    // shape of doc comment names a graph node (see `crate::node_label`), so
    // the lexer has to keep them for the adapter to read back.
    let comments = SingleThreadedComments::default();
    let mut lexer = Lexer::new(
        Syntax::Typescript(TsSyntax {
            tsx: true,
            decorators: true,
            ..TsSyntax::default()
        }),
        Default::default(),
        StringInput::new(text, BytePos(0), BytePos(end)),
        Some(&comments),
    );
    if goal == Goal::Script {
        // The dialect has one mode: a cell is strict code under either goal.
        let strict = lexer.ctx() | Context::Strict;
        lexer.set_ctx(strict);
    }
    let mut parser = Parser::new_from(lexer);
    let body = match goal {
        Goal::Module => parser.parse_module().map(|module| module.body),
        Goal::Script => parser
            .parse_script()
            .map(|script| script.body.into_iter().map(swc::ModuleItem::Stmt).collect()),
    };
    let mut errors = parser.take_errors();
    match body {
        Ok(items) if errors.is_empty() => Ok((items, comments.clone())),
        Ok(_) => Err(errors),
        Err(error) => {
            errors.insert(0, error);
            Err(errors)
        }
    }
}

/// Respells every reserved word SWC's lexer refused for being written with a
/// Unicode escape, and reports whether it made any change to read again.
///
/// ECMA-262 allows an escaped reserved word as an IdentifierName, so
/// `obj.bre\u0061k` and `{ bre\u0061k: 1 }` are valid; only as an identifier
/// is it an early error, which the adapter reports. SWC's lexer refuses the
/// word wherever it reads one whose first character is a plain letter, and
/// reads the same word as an ordinary identifier with its cooked name when the
/// first character is itself escaped. So each refused word is respelled with
/// its first character escaped and one escaped letter written plainly: the
/// same cooked name in the same number of bytes, so every span still indexes
/// the submitted source. Each respelled word starts with `\`, which the lexer
/// never refuses again, so the reading loop ends.
fn respell_escaped_keywords(
    text: &mut Cow<'_, str>,
    errors: &[Error],
    respellings: &mut BTreeMap<u32, Error>,
) -> bool {
    let refused = errors
        .iter()
        .filter(|error| matches!(error.kind(), SyntaxError::EscapeInReservedWord { .. }))
        .collect::<Vec<_>>();
    if refused.is_empty() {
        return false;
    }
    let text = text.to_mut();
    refused.into_iter().all(|error| {
        let start = error.span().lo.0;
        respellings.insert(start, error.clone());
        respell_escaped_keyword(text, start as usize)
    })
}

fn respell_escaped_keyword(text: &mut String, start: usize) -> bool {
    let bytes = text.as_bytes();
    let Some(&first) = bytes.get(start) else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    let mut escape = start + 1;
    while bytes.get(escape).is_some_and(u8::is_ascii_lowercase) {
        escape += 1;
    }
    let Some((letter, length)) = ascii_letter_escape(&bytes[escape..]) else {
        return false;
    };
    // A reserved word is ASCII, so its escaped letter needs two hex digits and
    // the escape is at least `\u{61}` long: room for the first letter's.
    let digits = length - 4;
    let mut respelled = String::with_capacity(length + escape - start);
    respelled.push('\\');
    respelled.push_str(&format!("u{{{first:0digits$x}}}"));
    respelled.push_str(&text[start + 1..escape]);
    respelled.push(char::from(letter));
    text.replace_range(start..escape + length, &respelled);
    true
}

/// Respells, as a placeholder identifier of the same length, each plain
/// `await` a Script-goal parse failed at, and reports whether it respelled
/// any.
///
/// Outside an async function a Script reads `await` as an identifier
/// (§13.1), but SWC's Script parser reads it as the start of an `await`
/// expression wherever an expression may begin, and refuses one as a class or
/// label name. Read as a placeholder, the word parses as the identifier
/// ECMA-262 makes it; the adapter names it `await` again and applies the
/// reserved-word rule of the function it stands in, so an `await` that a
/// Script does reserve is still refused.
fn respell_script_await(
    text: &mut Cow<'_, str>,
    errors: &[Error],
    respelled: &mut BTreeSet<u32>,
    respellings: &mut BTreeMap<u32, Error>,
) -> bool {
    let refused = errors
        .iter()
        .filter_map(|error| {
            let start = await_start(text, error)?;
            (!respelled.contains(&start)).then_some((start, error))
        })
        .collect::<Vec<_>>();
    if refused.is_empty() {
        return false;
    }
    let text = text.to_mut();
    for (start, error) in refused {
        let start_byte = start as usize;
        text.replace_range(
            start_byte..start_byte + AWAIT_PLACEHOLDER.len(),
            AWAIT_PLACEHOLDER,
        );
        respelled.insert(start);
        respellings.entry(start).or_insert_with(|| error.clone());
    }
    true
}

/// Where the plain `await` word an error is about starts: at the error
/// itself, or as the word just before it, where SWC read `await` as an
/// operator and failed on what follows (`await * 3`).
fn await_start(text: &str, error: &Error) -> Option<u32> {
    let at = error.span().lo.0 as usize;
    let before = text
        .get(..at)
        .and_then(|head| head.trim_end().len().checked_sub("await".len()));
    [Some(at), before]
        .into_iter()
        .flatten()
        .find(|&start| is_await_word(text, start))
        .and_then(|start| u32::try_from(start).ok())
}

fn is_await_word(text: &str, start: usize) -> bool {
    let Some(rest) = text
        .get(start..)
        .and_then(|rest| rest.strip_prefix("await"))
    else {
        return false;
    };
    let continues = rest
        .chars()
        .next()
        .is_some_and(swc::Ident::is_valid_continue);
    let preceded = text
        .get(..start)
        .and_then(|head| head.chars().next_back())
        .is_some_and(|previous| previous == '\\' || swc::Ident::is_valid_continue(previous));
    !continues && !preceded
}

/// Five bytes, like `await`, and an identifier in every context `await` is.
const AWAIT_PLACEHOLDER: &str = "_wait";

/// The letter a `\uXXXX` or `\u{X…}` escape at the head of `bytes` spells, and
/// the escape's length in bytes, when it spells a lowercase ASCII letter.
fn ascii_letter_escape(bytes: &[u8]) -> Option<(u8, usize)> {
    let rest = bytes.strip_prefix(b"\\u")?;
    let (digits, length) = match rest.strip_prefix(b"{") {
        Some(braced) => {
            let close = braced.iter().position(|&byte| byte == b'}')?;
            (&braced[..close], close + 4)
        }
        None => (rest.get(..4)?, 6),
    };
    let text = std::str::from_utf8(digits).ok()?;
    let value = u32::from_str_radix(text, 16).ok()?;
    let letter = u8::try_from(value).ok()?;
    (letter.is_ascii_lowercase() && length >= 6).then_some((letter, length))
}
