//! A region's text accumulator (FIG-6795): the long text a linear
//! accumulator holds, which no older version of it observes, gives its
//! storage to the text the next concatenation of it makes. That text grows
//! into the storage's spare capacity, which doubles when it runs out, so a
//! text built by repeated concatenation copies each part once instead of
//! its whole prefix at every step.
//!
//! Every concatenation still makes its own immutable text. The one that
//! gave its storage keeps its summary, so what the capacity meter counts
//! and what a formula measures of it are what they were, and it is freed
//! as any garbage is; its contents are gone, and a read of them is
//! refused as malformed. A projection reads only the new text's bytes,
//! never its capacity, so it seals ordinary canonical text.

use lash_kernel_doc::Value;
use lash_kernel_doc::arena::{ArenaHandle, Body, Chunk, LongText, ShortText};
use lash_kernel_doc::heap::TextSummary;
use lash_kernel_doc::heap_values::text_summary;

use super::{Arena, Facts};

impl Arena {
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
        let mut joined = String::new();
        joined
            .try_reserve_exact(usize::try_from(bytes).ok()?)
            .ok()?;
        for (index, value) in values.iter().enumerate() {
            if !separator.is_empty() && index != 0 {
                joined.push_str(separator);
            }
            joined.push_str(resident(self, value)?);
        }
        let value = match ShortText::new(&joined) {
            Some(short) => Value::Text(short),
            None => Value::LongText(self.insert(
                Body::LongText(LongText {
                    summary: text_summary(&joined),
                    chunks: Box::new([Chunk::Resident(joined)]),
                }),
                Facts::default(),
            )),
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
        Some(Value::LongText(self.insert(
            Body::LongText(LongText {
                summary: TextSummary {
                    utf8_bytes: len as u64,
                    chars: a.chars.checked_add(b.chars)?,
                    utf16_units: a.utf16_units.checked_add(b.utf16_units)?,
                },
                chunks: Box::new([Chunk::Resident(joined)]),
            }),
            Facts::default(),
        )))
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
        Some((self.insert(Body::LongText(made), Facts::default()), len))
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
        Some((self.insert(Body::LongText(made), Facts::default()), len))
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
