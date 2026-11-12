//! A long text's resident bytes, owned or on a shared append buffer
//! (K-LIB-007, the shared-tip ruling of FIG-7688).
//!
//! A shared text is `buffer[..len]`. A buffer's bytes are written once,
//! before the text that reads them exists, and never change afterwards; a
//! buffer only grows its written length, within a capacity fixed when it is
//! allocated. So a text needs no exclusive ownership to be appended to:
//! concatenating a text that ends at its buffer's written tip writes the
//! right operand past that tip, and gives a new text one longer prefix of
//! the same buffer. Every other text on the buffer, and every borrow of
//! one, reads a prefix that no later write touches.
//!
//! Real memory stays bounded at twice the logical figure: a buffer is made
//! at exact capacity except for an append at the tip of a full one, which
//! doubles, and every text on a buffer has `len >= capacity / 2`. Only each
//! text's own `len` is admitted to the memory meter, exactly as for a
//! copied text. A frozen buffer takes no further append; a buffer is frozen
//! once any text on it gets a durable identity, a head or a page form.

use std::alloc::{self, Layout};
use std::cell::Cell;
use std::ptr::NonNull;

/// A long text's resident bytes: its own string, or a prefix of a shared
/// append buffer.
#[derive(Clone)]
pub enum TextBytes {
    Owned(String),
    Shared(SharedText),
}

impl TextBytes {
    #[inline]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Owned(text) => text,
            Self::Shared(text) => text.as_str(),
        }
    }

    /// The text's bytes as an owned string, copied when they are shared.
    pub fn into_string(self) -> String {
        match self {
            Self::Owned(text) => text,
            Self::Shared(text) => text.as_str().to_owned(),
        }
    }

    /// The shared text these bytes are, when they are one.
    #[inline]
    pub fn shared(&self) -> Option<&SharedText> {
        match self {
            Self::Owned(_) => None,
            Self::Shared(text) => Some(text),
        }
    }

    /// Lets no later append extend the buffer these bytes are on.
    #[inline]
    pub fn freeze(&self) {
        if let Self::Shared(text) = self {
            text.freeze();
        }
    }
}

impl std::ops::Deref for TextBytes {
    type Target = str;
    #[inline]
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for TextBytes {
    #[inline]
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<[u8]> for TextBytes {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.as_str().as_bytes()
    }
}

impl From<String> for TextBytes {
    #[inline]
    fn from(text: String) -> Self {
        Self::Owned(text)
    }
}

impl From<SharedText> for TextBytes {
    #[inline]
    fn from(text: SharedText) -> Self {
        Self::Shared(text)
    }
}

impl std::fmt::Debug for TextBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.as_str(), f)
    }
}

impl PartialEq for TextBytes {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for TextBytes {}

/// The head of a buffer's one allocation; its `capacity` bytes follow it.
struct Header {
    /// The texts naming the buffer.
    count: Cell<usize>,
    /// The bytes written, from the start: every text on the buffer reads a
    /// prefix of them.
    written: Cell<usize>,
    frozen: Cell<bool>,
    capacity: usize,
}

const BYTES: usize = std::mem::size_of::<Header>();

/// The first `len` bytes of a shared append buffer, valid UTF-8 ending on
/// a scalar boundary. Neither `Send` nor `Sync`, like the arena that holds
/// it: its count and tip are plain cells.
pub struct SharedText {
    buffer: NonNull<Header>,
    len: usize,
}

impl SharedText {
    /// The concatenation of `parts` on a new buffer of `capacity` bytes,
    /// which is at least their length and at most twice it; `None` when the
    /// host refuses the memory.
    pub fn new(parts: &[&str], capacity: usize) -> Option<Self> {
        let len = parts
            .iter()
            .try_fold(0usize, |len, part| len.checked_add(part.len()))?;
        assert!(
            len <= capacity && len >= capacity / 2,
            "a text on a shared buffer holds at least half of its capacity"
        );
        let layout = layout(capacity)?;
        // SAFETY: the layout is never zero-sized, since a header precedes
        // the bytes.
        #[expect(unsafe_code, reason = "allocate a shared text buffer")]
        let raw = unsafe { alloc::alloc(layout) };
        let buffer = NonNull::new(raw)?.cast::<Header>();
        // SAFETY: `buffer` is a fresh allocation of `layout`, aligned for a
        // header and followed by `capacity >= len` bytes; nothing else
        // names it yet. Each part is copied into its own range of
        // `[0, len)`, which no part overlaps, since the parts are borrowed
        // from other allocations.
        #[expect(unsafe_code, reason = "initialize a new buffer's head and bytes")]
        unsafe {
            buffer.as_ptr().write(Header {
                count: Cell::new(1),
                written: Cell::new(len),
                frozen: Cell::new(false),
                capacity,
            });
            let bytes = raw.add(BYTES);
            let mut at = 0;
            for part in parts {
                std::ptr::copy_nonoverlapping(part.as_ptr(), bytes.add(at), part.len());
                at += part.len();
            }
        }
        Some(Self { buffer, len })
    }

    /// `self` followed by `right`, written past the buffer's tip: `None`
    /// unless `self` ends at the tip of an unfrozen buffer with room for
    /// `right`. No byte any text reads changes.
    pub fn append(&self, right: &str) -> Option<Self> {
        let header = self.header();
        if !self.at_tip() || header.capacity - self.len < right.len() {
            return None;
        }
        let len = self.len + right.len();
        // SAFETY: the destination `[self.len, len)` lies within the
        // buffer's capacity, as checked above, and past its written tip,
        // so no text reads it and no borrow covers it: every text on the
        // buffer reads a prefix `[0, n)` with `n <= written == self.len`.
        // `right` is valid UTF-8 and may be borrowed from this buffer's
        // written prefix, which the destination does not overlap.
        #[expect(unsafe_code, reason = "write past a shared buffer's tip")]
        unsafe {
            std::ptr::copy_nonoverlapping(right.as_ptr(), self.bytes().add(self.len), right.len());
        }
        header.written.set(len);
        Some(self.share(len))
    }

    /// Whether the text ends at its buffer's written tip, and the buffer
    /// takes appends.
    #[inline]
    pub fn at_tip(&self) -> bool {
        let header = self.header();
        !header.frozen.get() && header.written.get() == self.len
    }

    /// Lets no later append extend the text's buffer.
    #[inline]
    pub fn freeze(&self) {
        self.header().frozen.set(true);
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn as_str(&self) -> &str {
        // SAFETY: `[0, len)` was written, as valid UTF-8 ending on a scalar
        // boundary, before this text existed, and no write ever touches a
        // byte below the written tip, which is at least `len`. The buffer
        // lives as long as any text naming it, so as long as `self`.
        #[expect(unsafe_code, reason = "read a shared text's written prefix")]
        unsafe {
            std::str::from_utf8_unchecked(std::slice::from_raw_parts(self.bytes(), self.len))
        }
    }

    /// Another text on this buffer, of `len` written bytes.
    fn share(&self, len: usize) -> Self {
        let count = &self.header().count;
        // A text forgotten without its drop keeps its count, so the count
        // is not bounded by the texts alive. At `isize::MAX`, `Rc`'s limit,
        // a new text is refused before the count changes: a wrapped count
        // would free the buffer under its other texts. The refusal panics,
        // leaving every text and the count as they were.
        assert!(
            count.get() < isize::MAX as usize,
            "a shared text buffer's count is at its limit"
        );
        count.set(count.get() + 1);
        Self {
            buffer: self.buffer,
            len,
        }
    }

    #[inline]
    fn header(&self) -> &Header {
        // SAFETY: the allocation lives while any text names it, and its
        // head is initialized and only ever changed through its cells.
        #[expect(unsafe_code, reason = "read a live buffer's head")]
        unsafe {
            self.buffer.as_ref()
        }
    }

    #[inline]
    fn bytes(&self) -> *mut u8 {
        // SAFETY: the bytes follow the head in the same allocation, and the
        // pointer keeps the allocation's provenance: it is derived from the
        // allocator's pointer, never from a reference.
        #[expect(unsafe_code, reason = "address a buffer's bytes")]
        unsafe {
            self.buffer.as_ptr().cast::<u8>().add(BYTES)
        }
    }
}

/// A buffer's allocation: its head, then `capacity` bytes.
fn layout(capacity: usize) -> Option<Layout> {
    let (layout, at) = Layout::new::<Header>()
        .extend(Layout::array::<u8>(capacity).ok()?)
        .ok()?;
    debug_assert_eq!(at, BYTES);
    Some(layout)
}

impl Clone for SharedText {
    fn clone(&self) -> Self {
        self.share(self.len)
    }
}

impl Drop for SharedText {
    fn drop(&mut self) {
        let header = self.header();
        let count = header.count.get() - 1;
        header.count.set(count);
        // The layout was computed when the buffer was allocated, so it is
        // computed again here.
        if count == 0
            && let Some(layout) = layout(header.capacity)
        {
            // SAFETY: this was the last text naming the buffer, so nothing
            // reads it again; it was allocated with this layout.
            #[expect(unsafe_code, reason = "free a shared buffer with its last text")]
            unsafe {
                alloc::dealloc(self.buffer.as_ptr().cast(), layout);
            }
        }
    }
}

impl std::fmt::Debug for SharedText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.as_str(), f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_alias_reads_its_old_bytes_after_an_append() {
        let left = SharedText::new(&["ab", "c"], 6).unwrap();
        let alias = left.clone();
        let borrowed = alias.as_str();
        let appended = left.append("de").unwrap();
        assert_eq!(
            (borrowed, left.as_str(), appended.as_str()),
            ("abc", "abc", "abcde")
        );
    }

    #[test]
    fn two_appends_from_one_left_give_independent_texts() {
        let left = SharedText::new(&["abcd"], 8).unwrap();
        let first = left.append("x").unwrap();
        assert!(left.append("y").is_none(), "only the tip appends in place");
        let second = SharedText::new(&[left.as_str(), "y"], 5).unwrap();
        assert_eq!((first.as_str(), second.as_str()), ("abcdx", "abcdy"));
        // The right operand borrowed from the buffer it is written to.
        let third = first.append(&left.as_str()[..2]).unwrap();
        assert_eq!((left.as_str(), third.as_str()), ("abcd", "abcdxab"));
    }

    #[test]
    fn a_full_or_frozen_buffer_takes_no_append() {
        let full = SharedText::new(&["abc"], 3).unwrap();
        assert!(full.append("d").is_none());
        let roomy = SharedText::new(&["abc"], 5).unwrap();
        roomy.freeze();
        assert!(roomy.append("d").is_none());
        assert!(roomy.append("").is_none());
    }

    /// The count is refused at its limit, before it changes, so that no
    /// sequence of forgotten clones wraps it and frees the buffer under
    /// another text.
    #[test]
    fn a_count_at_its_limit_refuses_the_next_text() {
        let left = SharedText::new(&["abc"], 3).unwrap();
        let limit = isize::MAX as usize;
        left.header().count.set(limit - 1);
        std::mem::forget(left.clone());
        let refused = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            std::mem::forget(left.clone());
        }));
        assert!(refused.is_err(), "a text past the limit");
        assert_eq!((left.header().count.get(), left.as_str()), (limit, "abc"));
        // Dropping `left` leaves the forgotten texts' counts: the buffer
        // leaks, as a forgotten `Rc` does, and is never freed.
    }

    #[test]
    fn the_last_text_frees_the_buffer() {
        let left = SharedText::new(&["é"], 4).unwrap();
        let right = left.append("ü").unwrap();
        drop(left);
        assert_eq!(right.clone().as_str(), "éü");
    }
}
