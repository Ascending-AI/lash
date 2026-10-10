//! The native implementations.
//!
//! Each call does its work in one order: read the arguments, compile, run
//! the matcher, spend the result's text, then build the result. A guard
//! failure is therefore decided before anything is allocated, and by
//! counts that are functions of the arguments alone.
//!
//! The guard is work, not memory. What a call keeps follows the number of
//! matches times the number of groups, so each match kept, each text built
//! and each list of values is reserved against the run's memory bound
//! ([`NativeHeap::reserve`]) before it is allocated.

use std::ops::{ControlFlow, Range};
use std::sync::Arc;

use lash_kernel_doc::{
    ErrorValue, Integer, NativeCall, NativeError, NativeFunction, NativeHeap, Object, Value,
    WorkCounter,
};
use num_traits::ToPrimitive;

use crate::definitions::{BRAND, LONE_SURROGATE_ERROR, Operation, SYNTAX_ERROR};
use crate::matcher::{Found, Search};
use crate::pattern::{Engine, Flags, SyntaxError, text_from_units};

/// One of the six functions, bound to the engine that compiles for it.
pub(crate) struct Function {
    pub(crate) engine: Arc<Engine>,
    pub(crate) operation: Operation,
}

impl NativeFunction for Function {
    fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError> {
        let NativeCall {
            args,
            heap,
            counter,
        } = call;
        let arg = |index: usize| args.get(index).unwrap_or(&Value::Absent);
        if self.operation == Operation::CompileCheck {
            let pattern = text_arg(arg(0), "pattern")?;
            let flags = text_arg(arg(1), "flags")?;
            let flags = self.engine.check(pattern, flags).map_err(syntax_error)?;
            return regex_value(heap, pattern, flags);
        }
        let regex = Regex::read(heap, arg(0))?;
        let input = text_arg(arg(1), "input")?;
        let program = self
            .engine
            .program(&regex.pattern, regex.flags)
            .map_err(syntax_error)?;
        let units: Vec<u16> = input.encode_utf16().collect();
        let subject = Subject {
            search: Search {
                program: &program,
                units: &units,
                unicode: regex.flags.unicode,
            },
            regex: &regex,
        };
        match self.operation {
            Operation::CompileCheck => unreachable!("answered above"),
            Operation::Exec => {
                let (found, last_index) = subject.exec(counter)?;
                let found = match found {
                    Some(found) => {
                        spend_text(counter, match_units(&found))?;
                        subject.match_value(heap, &found)?
                    }
                    None => Value::Null,
                };
                record(
                    heap,
                    [("match", found), ("lastIndex", Value::Int(last_index))],
                )
            }
            Operation::Test => {
                let (found, last_index) = subject.exec(counter)?;
                record(
                    heap,
                    [
                        ("matched", Value::Bool(found.is_some())),
                        ("lastIndex", Value::Int(last_index)),
                    ],
                )
            }
            Operation::MatchAll => {
                let matches = subject.match_all(heap, counter)?;
                spend_text(counter, matches.iter().map(match_units).sum())?;
                reserve_values(heap, matches.len())?;
                let matches = matches
                    .iter()
                    .map(|found| subject.match_value(heap, found))
                    .collect::<Result<Vec<_>, _>>()?;
                heap.allocate(Object::List(matches)).map(Value::List)
            }
            Operation::Replace => {
                let replacement = text_arg(arg(2), "replacement")?;
                let (text, last_index) = subject.replace(replacement, heap, counter)?;
                record(
                    heap,
                    [("text", text), ("lastIndex", Value::Int(last_index))],
                )
            }
            Operation::Split => {
                let limit = match arg(2) {
                    Value::Absent => usize::MAX,
                    Value::Int(limit) if limit.is_negative() => {
                        return Err(raised(
                            "number_range",
                            "`limit` is negative: give the most pieces to return",
                        ));
                    }
                    Value::Int(limit) => limit.to_usize().unwrap_or(usize::MAX),
                    other => return Err(wrong_type("limit", "an integer", other)),
                };
                let pieces = subject.split(limit, heap, counter)?;
                spend_text(
                    counter,
                    pieces
                        .iter()
                        .flatten()
                        .map(|range| range.len() as u64)
                        .sum(),
                )?;
                reserve_values(heap, pieces.len())?;
                let pieces = pieces
                    .into_iter()
                    .map(|piece| piece.map_or(Ok(Value::Null), |range| subject.text(heap, range)))
                    .collect::<Result<Vec<_>, _>>()?;
                heap.allocate(Object::List(pieces)).map(Value::List)
            }
        }
    }
}

/// A regex record, read.
struct Regex {
    pattern: Arc<str>,
    flags: Flags,
    last_index: Integer,
}

impl Regex {
    /// Reads `{brand, pattern, flags, lastIndex}`: exactly those fields, a
    /// brand of [`BRAND`], and a `lastIndex` that is an integer of zero or
    /// more.
    fn read(heap: &dyn NativeHeap, value: &Value) -> Result<Self, NativeError> {
        let Value::Record(record) = value else {
            return Err(wrong_type("regex", "a regex record", value));
        };
        let field = |name: &str| heap.record_get(*record, name);
        let (Some(Value::Text(brand)), Some(Value::Text(pattern)), Some(Value::Text(flags))) =
            (field("brand"), field("pattern"), field("flags"))
        else {
            return Err(not_a_regex());
        };
        let Some(Value::Int(last_index)) = field("lastIndex") else {
            return Err(not_a_regex());
        };
        if brand.as_ref() != BRAND || heap.len(*record) != 4 {
            return Err(not_a_regex());
        }
        if last_index.is_negative() {
            return Err(raised(
                "number_range",
                "the regex's `lastIndex` is negative: it counts UTF-16 code units from the start of the input",
            ));
        }
        Ok(Self {
            pattern,
            flags: Flags::parse(&flags).map_err(syntax_error)?,
            last_index,
        })
    }
}

/// A regex's program over one input.
struct Subject<'a> {
    search: Search<'a>,
    regex: &'a Regex,
}

impl Subject<'_> {
    fn units(&self) -> &[u16] {
        self.search.units
    }

    /// ECMAScript's `RegExpBuiltinExec`, without the write: the match and
    /// the `lastIndex` the regex has afterwards.
    ///
    /// A regex that is neither global nor sticky searches from the start
    /// and keeps its `lastIndex`. Otherwise the search starts at
    /// `lastIndex`, which becomes the match's end, or zero when nothing
    /// matches or `lastIndex` lies past the input.
    fn exec(&self, counter: &mut WorkCounter) -> Result<(Option<Found>, Integer), NativeError> {
        let flags = self.regex.flags;
        if !flags.global && !flags.sticky {
            let found = self.search.first(0, false, counter)?;
            return Ok((found, self.regex.last_index.clone()));
        }
        let start = self
            .regex
            .last_index
            .to_usize()
            .filter(|start| *start <= self.units().len());
        let found = match start {
            Some(start) => self.search.first(start, flags.sticky, counter)?,
            None => None,
        };
        let last_index = found.as_ref().map_or(0, |found| found.range.end);
        Ok((found, Integer::new(last_index)))
    }

    /// Every match a global regex finds from its `lastIndex` on; for any
    /// other regex, the one match `exec` finds, or none.
    fn match_all(
        &self,
        heap: &mut dyn NativeHeap,
        counter: &mut WorkCounter,
    ) -> Result<Vec<Found>, NativeError> {
        let flags = self.regex.flags;
        if !flags.global {
            return Ok(self.exec(counter)?.0.into_iter().collect());
        }
        let start = self
            .regex
            .last_index
            .to_usize()
            .filter(|start| *start <= self.units().len());
        let mut matches = Kept::new(heap);
        if let Some(start) = start {
            self.search
                .each(start, flags.sticky, counter, |found| matches.keep(found))?;
        }
        matches.done()
    }

    /// ECMAScript's `RegExp.prototype[@@replace]` with a replacement text:
    /// a global regex replaces every match from the start and ends with a
    /// `lastIndex` of zero; any other replaces the one match `exec` finds.
    fn replace(
        &self,
        replacement: &str,
        heap: &mut dyn NativeHeap,
        counter: &mut WorkCounter,
    ) -> Result<(Value, Integer), NativeError> {
        let flags = self.regex.flags;
        let (matches, last_index) = if flags.global {
            let mut matches = Kept::new(heap);
            self.search
                .each(0, flags.sticky, counter, |found| matches.keep(found))?;
            (matches.done()?, Integer::new(0))
        } else {
            let (found, last_index) = self.exec(counter)?;
            (found.into_iter().collect(), last_index)
        };
        let units = self.units();
        let template: Vec<char> = replacement.chars().collect();
        let mut output = Output {
            units: Vec::new(),
            counter,
            heap,
        };
        let mut end = 0;
        for found in &matches {
            output.push(&units[end..found.range.start])?;
            output.expand(&template, units, found)?;
            end = found.range.end;
        }
        output.push(&units[end..])?;
        let Output { units, heap, .. } = output;
        Ok((text_value(heap, &units)?, last_index))
    }

    /// ECMAScript's `RegExp.prototype[@@split]`: the pieces of the input
    /// between matches, each followed by the match's captures, `None` for a
    /// group that took no part. It reads neither `lastIndex` nor the `g`
    /// and `y` flags.
    fn split(
        &self,
        limit: usize,
        heap: &mut dyn NativeHeap,
        counter: &mut WorkCounter,
    ) -> Result<Vec<Option<Range<usize>>>, NativeError> {
        let mut pieces = Vec::new();
        let mut refused = None;
        if limit == 0 {
            return Ok(pieces);
        }
        let size = self.units().len();
        if size == 0 {
            // The empty input is one piece unless the regex matches it.
            if self.search.first(0, false, counter)?.is_none() {
                pieces.push(Some(0..0));
            }
            return Ok(pieces);
        }
        let mut end = 0;
        self.search.each(0, false, counter, |found| {
            use std::ops::ControlFlow::{Break, Continue};
            if found.range.start == size {
                return Break(());
            }
            // An empty match where the last piece ended splits nothing.
            if found.range.is_empty() && found.range.start == end {
                return Continue(());
            }
            // A piece and the match's captures: reserved before they are kept.
            if let Err(error) = heap.reserve(found.captures.len() as u64 + 1, 0) {
                refused = Some(error);
                return Break(());
            }
            pieces.push(Some(end..found.range.start));
            pieces.extend(found.captures);
            end = found.range.end;
            if pieces.len() >= limit {
                Break(())
            } else {
                Continue(())
            }
        })?;
        if let Some(refused) = refused {
            return Err(refused);
        }
        if pieces.len() < limit {
            pieces.push(Some(end..size));
        }
        pieces.truncate(limit);
        Ok(pieces)
    }

    fn text(&self, heap: &mut dyn NativeHeap, range: Range<usize>) -> Result<Value, NativeError> {
        text_value(heap, &self.units()[range])
    }

    /// `{index, groups, named}`: where the match begins, the text of the
    /// whole match followed by each capturing group's (`null` for a group
    /// that took no part), and the named groups by name, or `null` when the
    /// pattern names none.
    fn match_value(&self, heap: &mut dyn NativeHeap, found: &Found) -> Result<Value, NativeError> {
        let group = |heap: &mut dyn NativeHeap, range: &Option<Range<usize>>| match range {
            Some(range) => self.text(heap, range.clone()),
            None => Ok(Value::Null),
        };
        reserve_values(heap, found.captures.len() + 1 + found.named.len())?;
        let mut groups = Vec::with_capacity(found.captures.len() + 1);
        groups.push(self.text(heap, found.range.clone())?);
        for capture in &found.captures {
            groups.push(group(heap, capture)?);
        }
        let named = if found.named.is_empty() {
            Value::Null
        } else {
            let fields = found
                .named
                .iter()
                .map(|(name, range)| Ok((name.clone(), group(heap, range)?)))
                .collect::<Result<Vec<_>, NativeError>>()?;
            heap.allocate(Object::Record(fields)).map(Value::Record)?
        };
        let groups = heap.allocate(Object::List(groups)).map(Value::List)?;
        record(
            heap,
            [
                ("index", Value::Int(Integer::new(found.range.start))),
                ("groups", groups),
                ("named", named),
            ],
        )
    }
}

/// The UTF-16 code units of text a match's value holds.
fn match_units(found: &Found) -> u64 {
    let captures = found.captures.iter().flatten();
    let named = found.named.iter().filter_map(|(_, range)| range.as_ref());
    captures
        .chain(named)
        .fold(found.range.len() as u64, |units, range| {
            units.saturating_add(range.len() as u64)
        })
}

/// Spends the text a result is about to hold, before it is built.
fn spend_text(counter: &mut WorkCounter, units: u64) -> Result<(), NativeError> {
    Ok(counter.spend(units)?)
}

/// The matches a call keeps: each is reserved before it is kept.
struct Kept<'a> {
    heap: &'a mut dyn NativeHeap,
    matches: Vec<Found>,
    refused: Option<NativeError>,
}

impl<'a> Kept<'a> {
    fn new(heap: &'a mut dyn NativeHeap) -> Self {
        Self {
            heap,
            matches: Vec::new(),
            refused: None,
        }
    }

    /// Keeps a match, or ends the search at the one the heap refuses.
    fn keep(&mut self, found: Found) -> ControlFlow<()> {
        // The match, each capture and each named group is one range; a
        // named group also keeps its name.
        let ranges = 1 + found.captures.len() + found.named.len();
        let names: usize = found.named.iter().map(|(name, _)| name.len()).sum();
        match self.heap.reserve(ranges as u64, names as u64) {
            Ok(()) => {
                self.matches.push(found);
                ControlFlow::Continue(())
            }
            Err(refused) => {
                self.refused = Some(refused);
                ControlFlow::Break(())
            }
        }
    }

    fn done(self) -> Result<Vec<Found>, NativeError> {
        match self.refused {
            Some(refused) => Err(refused),
            None => Ok(self.matches),
        }
    }
}

/// Reserves a list of `count` values before the values are built.
fn reserve_values(heap: &mut dyn NativeHeap, count: usize) -> Result<(), NativeError> {
    heap.reserve(count as u64, 0)
}

/// The text of some of the input's code units. Its bytes are counted and
/// reserved, as the text and as the buffer it is decoded into, before it
/// is built.
fn text_value(heap: &mut dyn NativeHeap, units: &[u16]) -> Result<Value, NativeError> {
    let bytes: usize = char::decode_utf16(units.iter().copied())
        .map(|unit| unit.map_or(0, char::len_utf8))
        .sum();
    heap.reserve(0, 2 * bytes as u64)?;
    text_from_units(units)
        .map(Value::text)
        .ok_or_else(lone_surrogate)
}

/// A replacement's output: every unit written is spent first, as work, and
/// reserved, as memory.
struct Output<'a> {
    units: Vec<u16>,
    counter: &'a mut WorkCounter,
    heap: &'a mut dyn NativeHeap,
}

impl Output<'_> {
    fn push(&mut self, piece: &[u16]) -> Result<(), NativeError> {
        self.counter.spend(piece.len() as u64)?;
        self.heap.reserve(0, 2 * piece.len() as u64)?;
        self.units.extend_from_slice(piece);
        Ok(())
    }

    fn push_char(&mut self, character: char) -> Result<(), NativeError> {
        self.push(character.encode_utf16(&mut [0; 2]))
    }

    /// ECMAScript's `GetSubstitution`: `$$`, `$&`, `` $` ``, `$'`, `$n`,
    /// `$nn` and `$<name>`; any other `$` is itself.
    fn expand(
        &mut self,
        template: &[char],
        input: &[u16],
        found: &Found,
    ) -> Result<(), NativeError> {
        let mut index = 0;
        while let Some(&character) = template.get(index) {
            let Some(&next) = template.get(index + 1).filter(|_| character == '$') else {
                self.push_char(character)?;
                index += 1;
                continue;
            };
            match next {
                '$' => {
                    self.push_char('$')?;
                    index += 2;
                }
                '&' => {
                    self.push(&input[found.range.clone()])?;
                    index += 2;
                }
                '`' => {
                    self.push(&input[..found.range.start])?;
                    index += 2;
                }
                '\'' => {
                    self.push(&input[found.range.end..])?;
                    index += 2;
                }
                '<' if !found.named.is_empty() => {
                    let Some(length) = template[index + 2..].iter().position(|c| *c == '>') else {
                        self.push_char('$')?;
                        index += 1;
                        continue;
                    };
                    let close = index + 2 + length;
                    let name: String = template[index + 2..close].iter().collect();
                    let group = found.named.iter().find(|(candidate, _)| *candidate == name);
                    if let Some((_, Some(range))) = group {
                        self.push(&input[range.clone()])?;
                    }
                    index = close + 1;
                }
                digit if digit.is_ascii_digit() => {
                    // Up to two digits name a group. A number that names
                    // none falls back to its first digit; if that names
                    // none either, the `$` is itself.
                    let digit_at = |at: usize| {
                        template
                            .get(at)
                            .and_then(|digit| digit.to_digit(10))
                            .map(|digit| digit as usize)
                    };
                    let groups = 1..=found.captures.len();
                    let first = digit_at(index + 1).filter(|group| groups.contains(group));
                    let both = digit_at(index + 1)
                        .zip(digit_at(index + 2))
                        .map(|(first, second)| first * 10 + second)
                        .filter(|group| groups.contains(group));
                    let Some((group, length)) = both
                        .map(|group| (group, 3))
                        .or(first.map(|group| (group, 2)))
                    else {
                        self.push_char('$')?;
                        index += 1;
                        continue;
                    };
                    if let Some(range) = &found.captures[group - 1] {
                        self.push(&input[range.clone()])?;
                    }
                    index += length;
                }
                _ => {
                    self.push_char('$')?;
                    index += 1;
                }
            }
        }
        Ok(())
    }
}

/// The record `compile_check` returns: the pattern, the flags in canonical
/// order and a `lastIndex` of zero.
fn regex_value(
    heap: &mut dyn NativeHeap,
    pattern: &Arc<str>,
    flags: Flags,
) -> Result<Value, NativeError> {
    record(
        heap,
        [
            ("brand", Value::text(BRAND)),
            ("pattern", Value::Text(pattern.clone())),
            ("flags", Value::text(flags.canonical())),
            ("lastIndex", Value::Int(Integer::new(0))),
        ],
    )
}

fn record<const N: usize>(
    heap: &mut dyn NativeHeap,
    fields: [(&str, Value); N],
) -> Result<Value, NativeError> {
    let fields = fields
        .into_iter()
        .map(|(name, value)| (name.to_string(), value))
        .collect();
    heap.allocate(Object::Record(fields)).map(Value::Record)
}

fn text_arg<'a>(value: &'a Value, name: &str) -> Result<&'a Arc<str>, NativeError> {
    match value {
        Value::Text(text) => Ok(text),
        other => Err(wrong_type(name, "a text", other)),
    }
}

fn raised(kind: &str, message: impl Into<String>) -> NativeError {
    NativeError::Raised(ErrorValue::new(kind, message))
}

fn wrong_type(name: &str, wanted: &str, found: &Value) -> NativeError {
    raised(
        "type_error",
        format!("`{name}` must be {wanted}, not {:?}", found.kind()),
    )
}

fn not_a_regex() -> NativeError {
    raised(
        "type_error",
        format!(
            "`regex` must be a regex record: exactly the fields `brand` (\"{BRAND}\"), `pattern`, `flags` and an integer `lastIndex`"
        ),
    )
}

fn syntax_error(error: SyntaxError) -> NativeError {
    raised(SYNTAX_ERROR, error.to_string())
}

fn lone_surrogate() -> NativeError {
    raised(
        LONE_SURROGATE_ERROR,
        "the result would hold half of a surrogate pair, which text cannot: match by code point with the `u` flag",
    )
}
