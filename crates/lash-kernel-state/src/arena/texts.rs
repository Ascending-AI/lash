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

use lash_kernel_doc::arena::{ArenaHandle, Body, Chunk, LongText};
use lash_kernel_doc::heap::TextSummary;
use lash_kernel_doc::heap_values::text_summary;

use super::{Arena, Facts};

impl Arena {
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
        let added = text_summary(part);
        let summary = TextSummary {
            utf8_bytes: text.summary.utf8_bytes + added.utf8_bytes,
            chars: text.summary.chars + added.chars,
            utf16_units: text.summary.utf16_units + added.utf16_units,
        };
        let chunks = std::mem::take(&mut text.chunks);
        let made = self.insert(
            Body::LongText(LongText { summary, chunks }),
            Facts::default(),
        );
        Some((made, len))
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
