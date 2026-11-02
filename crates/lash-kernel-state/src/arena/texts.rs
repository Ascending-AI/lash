//! A region's text accumulator (FIG-6795): the long text a linear
//! accumulator holds, which no older version of it observes, gives its
//! storage to the text the next concatenation of it makes. That text grows
//! into the storage's spare capacity, which doubles when it runs out, so a
//! text built by repeated concatenation copies each part once instead of
//! its whole prefix at every step.
//!
//! Every concatenation still makes its own immutable text. The one that
//! gave its storage keeps its summary, so what the growth estimate counts
//! and what a formula measures of it are what they were, and it is freed
//! as any garbage is; its contents are gone, and a read of them is
//! refused as malformed. A projection reads only the new text's bytes,
//! never its capacity, so it seals ordinary canonical text.

use lash_kernel_doc::Value;
use lash_kernel_doc::arena::{ArenaHandle, Body, Chunk, LongText, ShortText};
use lash_kernel_doc::heap::TextSummary;
use lash_kernel_doc::heap_values::text_summary;

use super::{Arena, BODY_SLOTS, Facts};

impl Arena {
    /// A resident text has no member handles, cold chunks or flat member
    /// summary. Install those known facts without walking a general body.
    fn insert_resident_text(&mut self, text: LongText) -> ArenaHandle {
        debug_assert!(matches!(&*text.chunks, [Chunk::Resident(_)]));
        self.resident = self
            .resident
            .saturating_add(BODY_SLOTS + text.summary.utf8_bytes / 16);
        let handle = self.objects.insert(Body::LongText(text));
        let index = handle.index() as usize;
        if index >= self.facts.len() {
            self.facts.resize_with(index + 1, Facts::default);
        }
        // Arena::free clears every released slot's facts. Fresh slots also
        // start empty, so only the new token and zero-list membership change.
        self.tokens += 1;
        self.facts[index].token = self.tokens;
        self.facts[index].zero = true;
        self.zero.push(handle);
        handle
    }

    /// Joins resident text members with an inline separator. Admission sees
    /// the output work and installation bounds before allocation begins.
    pub fn join_text(
        &mut self,
        list: ArenaHandle,
        separator: ShortText,
        admit: impl FnOnce(u64) -> bool,
    ) -> Option<(Value, u64)> {
        let values = self.sequence(list).ok()?;
        let separator = separator.as_str();
        let separators = values.len().saturating_sub(1) as u64;
        let mut bytes = (separator.len() as u64).checked_mul(separators)?;
        for value in values {
            bytes = bytes.checked_add(resident(self, value)?.len() as u64)?;
        }
        if !admit(self.text_install_bound(bytes).checked_add(bytes)?) {
            return None;
        }
        let joined = joined(self, values, separator, bytes)?;
        let value = match ShortText::new(&joined) {
            Some(short) => Value::Text(short),
            None => Value::LongText(self.insert_resident_text(LongText {
                summary: text_summary(&joined),
                chunks: Box::new([Chunk::Resident(joined)]),
            })),
        };
        Some((value, bytes))
    }

    /// Concatenates resident texts into their final buffer, retaining their
    /// certified character counts. The caller admits the native's work and
    /// installation bounds before calling; a decline changes no arena object.
    pub fn concat_text(&mut self, left: Value, right: Value) -> Option<Value> {
        let ((left, a), (right, b)) = (part(self, &left)?, part(self, &right)?);
        let len = left.len().checked_add(right.len())?;
        let mut joined = String::new();
        joined.try_reserve_exact(len).ok()?;
        joined.push_str(left);
        joined.push_str(right);
        if let Some(short) = ShortText::new(&joined) {
            return Some(Value::Text(short));
        }
        Some(Value::LongText(self.insert_resident_text(LongText {
            summary: TextSummary {
                utf8_bytes: len as u64,
                chars: a.chars.checked_add(b.chars)?,
                utf16_units: a.utf16_units.checked_add(b.utf16_units)?,
            },
            chunks: Box::new([Chunk::Resident(joined)]),
        })))
    }

    /// Whether long text `text` may give its storage to a concatenation of
    /// it: its bytes are one resident chunk, no member holds it, and no
    /// projection gave it a page form.
    pub fn extensible(&self, text: ArenaHandle) -> bool {
        let (Some(Body::LongText(long)), Some(facts)) = (
            self.objects.get(text),
            self.facts.get(text.index() as usize),
        ) else {
            return false;
        };
        matches!(&*long.chunks, [Chunk::Resident(_)])
            && facts.count == 0
            && facts.page.get().is_none()
            && !facts.cold
    }

    /// The new long text of extensible `from`'s bytes and then `part`'s,
    /// in the storage `from` gives it, and the length `from` had. Where
    /// the storage must grow, `admit` is asked for the capacity it grows
    /// to before it grows; `None`, having changed nothing, where `admit`
    /// refuses or the allocator does.
    pub fn extend_text(
        &mut self,
        from: ArenaHandle,
        part: &str,
        admit: impl FnOnce(u64) -> bool,
    ) -> Option<(ArenaHandle, usize)> {
        let Some(Body::LongText(text)) = self.objects.get_mut(from) else {
            return None;
        };
        let (made, len) = extended(text, part, || text_summary(part), admit)?;
        Some((self.insert_resident_text(made), len))
    }

    /// Extends with a resident text value, borrowing distinct long operands
    /// together. An aliased operand keeps the ordinary copying path.
    pub fn extend_text_from(
        &mut self,
        from: ArenaHandle,
        other: ArenaHandle,
        admit: impl FnOnce(u64) -> bool,
    ) -> Option<(ArenaHandle, usize)> {
        if from == other {
            let owned = resident(self, &Value::LongText(other))?.to_owned();
            return self.extend_text(from, &owned, admit);
        }
        let (Body::LongText(left), Body::LongText(right)) =
            self.objects.get_disjoint_mut(from, other)?
        else {
            return None;
        };
        let [Chunk::Resident(part)] = &*right.chunks else {
            return None;
        };
        let (made, len) = extended(left, part, || right.summary, admit)?;
        Some((self.insert_resident_text(made), len))
    }

    /// Gives `from` back the storage [`Self::extend_text`] gave `to`, cut
    /// back to `len`, the length `from` had: the statement that made `to`
    /// missed, and `to` is its garbage.
    pub fn unextend_text(&mut self, from: ArenaHandle, to: ArenaHandle, len: usize) {
        let Some(Body::LongText(made)) = self.objects.get_mut(to) else {
            return;
        };
        let mut chunks = std::mem::take(&mut made.chunks);
        if let [Chunk::Resident(held)] = &mut *chunks {
            held.truncate(len);
        }
        if let Some(Body::LongText(text)) = self.objects.get_mut(from) {
            text.chunks = chunks;
        }
    }
}

#[inline]
fn part<'a>(arena: &'a Arena, value: &'a Value) -> Option<(&'a str, TextSummary)> {
    match value {
        Value::Text(text) => Some((text.as_str(), text_summary(text.as_str()))),
        Value::LongText(handle) => match arena.body(*handle).ok()? {
            Body::LongText(text) => match &*text.chunks {
                [Chunk::Resident(bytes)] => Some((bytes.as_str(), text.summary)),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

#[inline]
fn resident<'a>(arena: &'a Arena, value: &'a Value) -> Option<&'a str> {
    match value {
        Value::Text(text) => Some(text.as_str()),
        Value::LongText(handle) => match arena.body(*handle).ok()? {
            Body::LongText(text) => match &*text.chunks {
                [Chunk::Resident(bytes)] => Some(bytes.as_str()),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

#[inline(always)]
fn extended(
    text: &mut LongText,
    part: &str,
    added: impl FnOnce() -> TextSummary,
    admit: impl FnOnce(u64) -> bool,
) -> Option<(LongText, usize)> {
    let [Chunk::Resident(held)] = &mut *text.chunks else {
        return None;
    };
    let len = held.len();
    let wanted = len.checked_add(part.len())?;
    if wanted > held.capacity() {
        let grown = wanted.max(held.capacity().saturating_mul(2));
        if !admit(u64::try_from(grown).ok()?) || held.try_reserve_exact(grown - len).is_err() {
            return None;
        }
    }
    held.push_str(part);
    let added = added();
    let summary = TextSummary {
        utf8_bytes: text.summary.utf8_bytes + added.utf8_bytes,
        chars: text.summary.chars + added.chars,
        utf16_units: text.summary.utf16_units + added.utf16_units,
    };

    Some((
        LongText {
            summary,
            chunks: std::mem::take(&mut text.chunks),
        },
        len,
    ))
}

/// Dynamic facts of the canonical text body, before storage can move. The
/// resource compiler uses these sizes for its native formula and scratch phase;
/// neither the buffer capacity nor a guessed retained delta is a logical price.
#[derive(Clone, Copy, Debug)]
pub struct TextFacts {
    pub left: TextSummary,
    pub right: TextSummary,
    pub result: TextSummary,
    pub work: u64,
    pub install: u64,
}

const EMPTY_TEXT: TextSummary = TextSummary {
    utf8_bytes: 0,
    chars: 0,
    utf16_units: 0,
};

struct TextEdit {
    made: ArenaHandle,
    moved: Option<(ArenaHandle, usize)>,
}

/// Unpublished resident text storage for one certified source prefix. All
/// appends share this guard and one exit transaction. The caller proves that
/// `private` inputs have no observer in the prefix, including saved operands.
/// Drop restores moved buffers in reverse order and removes speculative texts.
/// No callback, seal, collection or other arena writer can overlap this borrow.
pub struct PreparedTexts<'a> {
    arena: &'a mut Arena,
    dependencies: Option<crate::paged::logical::Dependencies>,
    entries: std::collections::HashSet<(u32, u32)>,
    private: std::collections::HashSet<(u32, u32)>,
    made: std::collections::HashSet<(u32, u32)>,
    edits: Vec<TextEdit>,
    zero: usize,
    facts: usize,
    resident: u64,
    tokens: u64,
    published: bool,
}

impl Arena {
    /// Bind entry content once. Source activation, owner placement, legal exits,
    /// charge debt and the complete logical peak remain the common certificate's
    /// obligations. This method grants no admission or source advancement.
    pub fn prepare_text_region(
        &mut self,
        ledger: &crate::paged::logical::Ledger,
        entries: &[Value],
        private: &[ArenaHandle],
    ) -> Option<PreparedTexts<'_>> {
        if self.registration_active() {
            return None;
        }
        let handles: Vec<_> = entries.iter().filter_map(Value::handle).collect();
        let entries: std::collections::HashSet<_> =
            handles.iter().map(|handle| text_slot(*handle)).collect();
        if private
            .iter()
            .any(|handle| !entries.contains(&text_slot(*handle)) || !self.extensible(*handle))
        {
            return None;
        }
        let dependencies = crate::paged::logical::Dependencies::capture(ledger, self, &handles)?;
        Some(PreparedTexts {
            zero: self.zero.len(),
            facts: self.facts.len(),
            resident: self.resident,
            tokens: self.tokens,
            arena: self,
            dependencies: Some(dependencies),
            entries,
            private: private.iter().map(|handle| text_slot(*handle)).collect(),
            made: Default::default(),
            edits: Vec::new(),
            published: false,
        })
    }
}

impl PreparedTexts<'_> {
    /// Only entry values and this prefix's own outputs may be read. A moved
    /// predecessor has a summary but no readable contents, so it cannot become
    /// a live-out or be read as another operand after its storage was donated.
    fn readable(&self, value: &Value) -> bool {
        value.handle().is_none_or(|handle| {
            self.entries.contains(&text_slot(handle)) || self.made.contains(&text_slot(handle))
        }) && part(self.arena, value).is_some()
    }

    pub fn summary(&self, value: &Value) -> Option<TextSummary> {
        self.readable(value)
            .then(|| part(self.arena, value).map(|(_, summary)| summary))?
    }

    pub fn text<'a>(&'a self, value: &'a Value) -> Option<&'a str> {
        self.readable(value).then(|| resident(self.arena, value))?
    }

    /// Execute the canonical resident concatenation. Admission of the selected
    /// source prefix happens outside this body. `move_left` is the compiler's
    /// observer-exclusion fact, not an arena reference-count heuristic.
    pub fn concat(
        &mut self,
        left: Value,
        right: Value,
        move_left: bool,
    ) -> Option<(Value, TextFacts)> {
        let a = self.summary(&left)?;
        let b = self.summary(&right)?;
        let result = add_summary(a, b)?;
        let mut facts = TextFacts {
            left: a,
            right: b,
            result,
            work: result.utf8_bytes,
            install: if result.utf8_bytes <= ShortText::MAX as u64 {
                0
            } else {
                self.arena.text_install_bound(result.utf8_bytes)
            },
        };
        if let (Value::Text(a), Value::Text(b)) = (left, right)
            && let Some(text) = a.concat(&b)
        {
            facts.work = 0;
            return Some((Value::Text(text), facts));
        }
        self.edits.try_reserve(1).ok()?;
        self.made.try_reserve(1).ok()?;
        let moving = match left {
            Value::LongText(handle)
                if move_left
                    && left.handle() != right.handle()
                    && (self.private.contains(&text_slot(handle))
                        || self.made.contains(&text_slot(handle)))
                    && self.arena.extensible(handle) =>
            {
                Some(handle)
            }
            _ => None,
        };
        let (value, moved) = if let Some(from) = moving {
            let (made, length) = match right {
                Value::Text(text) => self.arena.extend_text(from, text.as_str(), |_| true)?,
                Value::LongText(other) => self.arena.extend_text_from(from, other, |_| true)?,
                _ => return None,
            };
            (Value::LongText(made), Some((from, length)))
        } else {
            (self.arena.concat_text(left, right)?, None)
        };
        if let Value::LongText(made) = value {
            self.edits.push(TextEdit { made, moved });
            self.made.insert(text_slot(made));
        }
        Some((value, facts))
    }

    /// Join already formatted text pieces. Numeric/string conversion remains
    /// the canonical library's job; this kernel never substitutes a formatter.
    /// The temporary source parts list remains a real allocation owned by the
    /// constructor consumer. Only its resident text pieces are consumed here.
    pub fn join(&mut self, pieces: &[Value], separator: Value) -> Option<(Value, TextFacts)> {
        let separator_summary = self.summary(&separator)?;
        let mut result = EMPTY_TEXT;
        for (index, piece) in pieces.iter().enumerate() {
            if index != 0 {
                result = add_summary(result, separator_summary)?;
            }
            result = add_summary(result, self.summary(piece)?)?;
        }
        self.edits.try_reserve(1).ok()?;
        self.made.try_reserve(1).ok()?;
        let joined = joined(
            self.arena,
            pieces,
            resident(self.arena, &separator)?,
            result.utf8_bytes,
        )?;
        let value = match ShortText::new(&joined) {
            Some(short) => Value::Text(short),
            None => {
                let made = self.arena.insert_resident_text(LongText {
                    summary: result,
                    chunks: Box::new([Chunk::Resident(joined)]),
                });
                self.edits.push(TextEdit { made, moved: None });
                self.made.insert(text_slot(made));
                Value::LongText(made)
            }
        };
        Some((
            value,
            TextFacts {
                left: EMPTY_TEXT,
                right: separator_summary,
                result,
                work: result.utf8_bytes,
                install: if result.utf8_bytes <= ShortText::MAX as u64 {
                    0
                } else {
                    self.arena.text_install_bound(result.utf8_bytes)
                },
            },
        ))
    }

    /// Stage actual live-outs once. The common source owner authenticates the
    /// selected exit and resource/charge facts in `finish`, then passes `apply`
    /// to AcceptedBoundary::publish inside SourceAccepted::commit. If it
    /// declines, rejects a dependency or returns an error, Drop restores storage.
    /// A callback returning success without publishing grants no storage commit.
    pub fn boundary<R>(
        mut self,
        ledger: &mut crate::paged::logical::Ledger,
        heap: &crate::paged::heap_impl::PagedHeap,
        inputs: &[crate::paged::logical::Owner],
        outputs: &[(crate::paged::logical::Owner, Value)],
        finish: impl FnOnce(
            crate::paged::logical::BoundaryTransaction<'_>,
            &mut crate::paged::logical::Ledger,
            &mut dyn FnMut(&mut Arena, Vec<crate::paged::logical::MemberEdit>),
        ) -> Result<R, lash_kernel_doc::heap::HeapFault>,
    ) -> Result<Option<R>, lash_kernel_doc::heap::HeapFault> {
        use crate::paged::logical::RootValue;
        // An interior predecessor's summary is sufficient for fees, but its
        // missing bytes must never be authenticated as a surviving root.
        for (_, value) in outputs {
            if matches!(value, Value::Text(_) | Value::LongText(_)) && !self.readable(value) {
                return Ok(None);
            }
        }
        let roots: Vec<_> = outputs
            .iter()
            .map(|(owner, value)| (*owner, RootValue::Value(value)))
            .collect();
        let dependencies = self.dependencies.take().ok_or_else(|| {
            lash_kernel_doc::heap::HeapFault::Malformed("text entry facts already consumed".into())
        })?;
        let arena_identity = std::ptr::from_ref(&*self.arena);
        let transaction = ledger
            .prepare_source_boundary(
                self.arena.registration().map_err(text_fault)?,
                heap,
                inputs,
                &roots,
            )?
            .bind_dependencies(dependencies)?;
        let published = &mut self.published;
        let result = finish(transaction, ledger, &mut |arena, edits| {
            assert!(
                std::ptr::eq(arena_identity, arena),
                "text storage belongs to this arena"
            );
            assert!(edits.is_empty(), "a text boundary has no member edits");
            *published = true;
        })?;
        Ok(self.published.then_some(result))
    }
}

impl Drop for PreparedTexts<'_> {
    fn drop(&mut self) {
        if self.published {
            return;
        }
        for edit in self.edits.iter().rev() {
            if let Some((from, length)) = edit.moved {
                self.arena.unextend_text(from, edit.made, length);
            }
            // These bodies have no member edges, identities or projected page
            // caches. No counter or zero-list entry may escape abandonment.
            self.arena.objects.release(edit.made);
            self.arena.facts[edit.made.index() as usize] = Facts::default();
        }
        self.arena.zero.truncate(self.zero);
        self.arena.facts.truncate(self.facts);
        self.arena.resident = self.resident;
        self.arena.tokens = self.tokens;
    }
}

fn add_summary(a: TextSummary, b: TextSummary) -> Option<TextSummary> {
    Some(TextSummary {
        utf8_bytes: a.utf8_bytes.checked_add(b.utf8_bytes)?,
        chars: a.chars.checked_add(b.chars)?,
        utf16_units: a.utf16_units.checked_add(b.utf16_units)?,
    })
}

fn text_fault(error: lash_kernel_doc::NativeError) -> lash_kernel_doc::heap::HeapFault {
    match error {
        lash_kernel_doc::NativeError::Host(error) => error,
        other => lash_kernel_doc::heap::HeapFault::Malformed(other.to_string()),
    }
}

fn text_slot(handle: ArenaHandle) -> (u32, u32) {
    (handle.index(), handle.generation())
}

fn joined(arena: &Arena, values: &[Value], separator: &str, bytes: u64) -> Option<String> {
    let mut joined = String::new();
    joined
        .try_reserve_exact(usize::try_from(bytes).ok()?)
        .ok()?;
    for (index, value) in values.iter().enumerate() {
        if !separator.is_empty() && index != 0 {
            joined.push_str(separator);
        }
        joined.push_str(resident(arena, value)?);
    }
    Some(joined)
}
