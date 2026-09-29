//! An append-only vector whose clones share one buffer.
//!
//! A session's read state is a sequence that only grows between frame
//! changes: the current frame's messages, its events, its rendered prompt,
//! the resident nodes. Readers hold snapshots of it (every observation the
//! live replay retains holds one) while the writer keeps appending. With an
//! `Arc<Vec<T>>` each append behind a held snapshot copied the whole
//! sequence, and each retained snapshot kept its own copy: O(history) work
//! per append and O(history) bytes per retained snapshot (FIG-4059,
//! FIG-4060).
//!
//! [`AppendVec`] is a view of a prefix of a shared buffer. A clone is an
//! `Arc` bump. An append from the handle whose length is the buffer's
//! initialized length writes the next slot in place, so a snapshot and every
//! later append share one allocation; a slot other handles cannot see is
//! never observed by them. The buffer is replaced only when it is full
//! (doubling, amortized O(1)), or when the appending handle is behind the
//! buffer's tip and its value is not the one already there (a fork).
//!
//! Every initialized slot a live handle other than the editing one can see is
//! immutable. The buffer counts its live handles by length, so an edit knows
//! exactly which slots nobody else can see: a reader that has been dropped
//! blocks nothing.

use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use crate::sync::MutexExt;

/// The smallest capacity a growing buffer allocates.
const MIN_GROWN_CAPACITY: usize = 4;

pub struct AppendVec<T> {
    buffer: Arc<Buffer<T>>,
    len: usize,
}

struct Buffer<T> {
    ptr: NonNull<T>,
    capacity: usize,
    /// Held while writing or rewriting a slot, while reading a slot past the
    /// reader's own length, and while a handle is made, dropped or changes
    /// length.
    state: Mutex<BufferState>,
    _owns: PhantomData<T>,
}

struct BufferState {
    /// Slots `[0, initialized)` hold values; the rest are uninitialized.
    initialized: usize,
    /// How many live handles have each length. A slot at `index` is visible
    /// exactly to the handles longer than `index`.
    handles: BTreeMap<usize, usize>,
}

impl BufferState {
    fn register(&mut self, len: usize) {
        *self.handles.entry(len).or_insert(0) += 1;
    }

    fn unregister(&mut self, len: usize) {
        if let Some(count) = self.handles.get_mut(&len) {
            *count -= 1;
            if *count == 0 {
                self.handles.remove(&len);
            }
        }
    }

    fn relength(&mut self, from: usize, to: usize) {
        self.unregister(from);
        self.register(to);
    }

    /// The longest live handle other than one handle of length `own`.
    fn longest_other(&self, own: usize) -> usize {
        let mut lengths = self.handles.iter().rev();
        match lengths.next() {
            Some((&len, &count)) if len == own && count == 1 => {
                lengths.next().map_or(0, |(&len, _)| len)
            }
            Some((&len, _)) => len,
            None => 0,
        }
    }
}

// SAFETY: a `Buffer<T>` owns its `T`s; moving it to another thread moves
// them (`T: Send`), and sharing it lets several threads read the same `T`s
// (`T: Sync`). Writes happen under `state`'s lock to slots no other live
// handle can read.
#[expect(
    unsafe_code,
    reason = "a raw buffer of `T` is `Send` exactly when `T` is `Send + Sync`"
)]
unsafe impl<T: Send + Sync> Send for Buffer<T> {}
// SAFETY: as for `Send` above.
#[expect(
    unsafe_code,
    reason = "a raw buffer of `T` is `Sync` exactly when `T` is `Send + Sync`"
)]
unsafe impl<T: Send + Sync> Sync for Buffer<T> {}

impl<T> Buffer<T> {
    /// Takes over `values`' allocation: its elements are the initialized
    /// slots and its spare capacity the rest. The one handle made from it
    /// is registered.
    fn from_vec(values: Vec<T>) -> Self {
        let mut values = std::mem::ManuallyDrop::new(values);
        let initialized = values.len();
        let capacity = values.capacity();
        let ptr = NonNull::new(values.as_mut_ptr()).unwrap_or(NonNull::dangling());
        let mut state = BufferState {
            initialized,
            handles: BTreeMap::new(),
        };
        state.register(initialized);
        Self {
            ptr,
            capacity,
            state: Mutex::new(state),
            _owns: PhantomData,
        }
    }
}

impl<T> Drop for Buffer<T> {
    #[expect(
        unsafe_code,
        reason = "the buffer is a `Vec` allocation taken apart in `from_vec`; reassembling it drops the initialized slots and frees it"
    )]
    fn drop(&mut self) {
        let initialized = self
            .state
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .initialized;
        // SAFETY: `ptr` and `capacity` came from a `Vec<T>` in `from_vec`,
        // slots `[0, initialized)` hold values, and no handle is left.
        drop(unsafe { Vec::from_raw_parts(self.ptr.as_ptr(), initialized, self.capacity) });
    }
}

impl<T> AppendVec<T> {
    #[must_use]
    pub fn new() -> Self {
        Self::from(Vec::new())
    }

    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self::from(Vec::with_capacity(capacity))
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[expect(
        unsafe_code,
        reason = "a handle's slots are initialized, and nothing rewrites a slot a live handle can see"
    )]
    pub fn as_slice(&self) -> &[T] {
        // SAFETY: slots `[0, len)` are initialized (`len <= initialized`
        // always). A slot is rewritten only when no live handle other than
        // the rewriting one is longer than it, and this handle is live and
        // longer than each of its slots; the rewriting handle itself needs
        // `&mut self`.
        unsafe { std::slice::from_raw_parts(self.buffer.ptr.as_ptr(), self.len) }
    }

    /// Whether `left` and `right` are the same view of the same buffer:
    /// equal by construction, without comparing elements.
    pub fn ptr_eq(left: &Self, right: &Self) -> bool {
        Arc::ptr_eq(&left.buffer, &right.buffer) && left.len == right.len
    }

    /// Shortens this view. The buffer keeps the dropped slots for the
    /// handles that still see them.
    pub fn truncate(&mut self, len: usize) {
        if len < self.len {
            self.buffer.state.lock_recover().relength(self.len, len);
            self.len = len;
        }
    }

    /// The buffer's slots past this view, which a handle that is the
    /// buffer's only one owns outright: dropped, so appends and edits write
    /// in place again.
    #[expect(
        unsafe_code,
        reason = "drops initialized slots of a buffer no other handle can reach"
    )]
    fn reclaim_if_sole(&mut self) {
        let len = self.len;
        let Some(buffer) = Arc::get_mut(&mut self.buffer) else {
            return;
        };
        let state = buffer
            .state
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.initialized <= len {
            return;
        }
        let stale = state.initialized - len;
        // Lowered first: a panicking drop below leaks, never double-drops.
        state.initialized = len;
        // SAFETY: slots `[len, len + stale)` are initialized, and this is
        // the buffer's only handle, so nothing else reads them.
        unsafe {
            std::ptr::drop_in_place(std::ptr::slice_from_raw_parts_mut(
                buffer.ptr.as_ptr().add(len),
                stale,
            ));
        }
    }

    /// Writes `value` into the next slot when this handle is the buffer's
    /// tip and the buffer has room, answering it back otherwise.
    #[expect(
        unsafe_code,
        reason = "the slot at the initialized length is uninitialized and within capacity"
    )]
    fn try_push_in_place(&mut self, value: T) -> Result<(), T> {
        self.reclaim_if_sole();
        let mut state = self.buffer.state.lock_recover();
        if state.initialized != self.len || self.len == self.buffer.capacity {
            return Err(value);
        }
        // SAFETY: `len < capacity`, and the slot at `initialized` is
        // uninitialized; no handle reads it until `initialized` covers it,
        // which it does only under this lock, after the write.
        unsafe { self.buffer.ptr.as_ptr().add(self.len).write(value) };
        state.initialized += 1;
        state.relength(self.len, self.len + 1);
        self.len += 1;
        Ok(())
    }
}

impl<T: Clone> AppendVec<T> {
    pub fn push(&mut self, value: T) {
        self.push_adopting(value, |_, _| false);
    }

    /// Appends `value`, adopting the buffer's next slot instead when another
    /// handle already wrote the same value there (`same(existing, value)`).
    ///
    /// Two handles that append the same values from the same point, such
    /// as a turn's read state and the graph that commits it, then share one
    /// buffer instead of forking it.
    pub fn push_adopting(&mut self, value: T, same: impl Fn(&T, &T) -> bool) {
        let value = match self.try_push_in_place(value) {
            Ok(()) => return,
            Err(value) => value,
        };
        if self.try_adopt(&value, &same) {
            return;
        }
        self.grow(1);
        if self.try_push_in_place(value).is_err() {
            unreachable!("a freshly grown buffer is this handle's, with room for one more");
        }
    }

    pub fn extend_adopting(
        &mut self,
        values: impl IntoIterator<Item = T>,
        same: impl Fn(&T, &T) -> bool,
    ) {
        let values = values.into_iter();
        self.reserve(values.size_hint().0);
        for value in values {
            self.push_adopting(value, &same);
        }
    }

    /// Ensures that `additional` appends from this handle write in place,
    /// unless another handle takes the tip first.
    pub fn reserve(&mut self, additional: usize) {
        if additional == 0 {
            return;
        }
        self.reclaim_if_sole();
        let at_tip = self.buffer.state.lock_recover().initialized == self.len;
        if at_tip && self.buffer.capacity - self.len < additional {
            self.grow(additional);
        }
        // Behind the tip, appends adopt or fork value by value.
    }

    #[expect(
        unsafe_code,
        reason = "reads the initialized slot at this handle's length under the buffer lock"
    )]
    fn try_adopt(&mut self, value: &T, same: impl Fn(&T, &T) -> bool) -> bool {
        let mut state = self.buffer.state.lock_recover();
        if self.len >= state.initialized {
            return false;
        }
        // SAFETY: the slot is initialized (`len < initialized`). A slot is
        // written or rewritten only under this lock, so nothing races this
        // read.
        let existing = unsafe { &*self.buffer.ptr.as_ptr().add(self.len) };
        if !same(existing, value) {
            return false;
        }
        state.relength(self.len, self.len + 1);
        self.len += 1;
        true
    }

    /// Moves this handle to a buffer of its own with room for `additional`
    /// more slots, copying its view. Other handles keep the old buffer.
    fn grow(&mut self, additional: usize) {
        let capacity = (self.len + additional)
            .max(self.len.saturating_mul(2))
            .max(MIN_GROWN_CAPACITY);
        let mut values = Vec::with_capacity(capacity);
        values.extend_from_slice(self.as_slice());
        *self = Self::from(values);
    }

    /// Replaces the slots from `start` on with `values`: in place when no
    /// other live handle sees any of them (a commit rewriting the records it
    /// just appended), and on a copy of this view otherwise. `start` past
    /// the end appends.
    #[expect(
        unsafe_code,
        reason = "rewrites and drops initialized slots no other live handle can see, under the buffer lock"
    )]
    pub fn replace_from(&mut self, start: usize, values: impl IntoIterator<Item = T>) {
        let start = start.min(self.len);
        let mut values = values.into_iter();
        self.reclaim_if_sole();
        let mut replaced = Vec::new();
        let in_place = {
            let mut state = self.buffer.state.lock_recover();
            let unseen = state.initialized == self.len && state.longest_other(self.len) <= start;
            if unseen {
                let mut index = start;
                while index < self.len {
                    let Some(value) = values.next() else {
                        break;
                    };
                    // SAFETY: the slot is initialized (`index < len ==
                    // initialized`) and no other live handle is longer than
                    // `start <= index`; the lock keeps adopters and new
                    // handles out while it is rewritten. The old value is
                    // dropped after the lock is released.
                    replaced.push(unsafe { self.buffer.ptr.as_ptr().add(index).replace(value) });
                    index += 1;
                }
                if index < self.len {
                    // Fewer values than slots: the view and the buffer end
                    // at the last slot written.
                    let stale = self.len - index;
                    state.initialized = index;
                    state.relength(self.len, index);
                    self.len = index;
                    // SAFETY: slots `[index, index + stale)` were initialized
                    // and no other live handle sees them; `initialized` no
                    // longer covers them, so they are dropped exactly once.
                    unsafe {
                        std::ptr::drop_in_place(std::ptr::slice_from_raw_parts_mut(
                            self.buffer.ptr.as_ptr().add(index),
                            stale,
                        ));
                    }
                }
            }
            unseen
        };
        drop(replaced);
        if !in_place {
            self.truncate(start);
            self.grow(0);
        }
        self.extend_adopting(values, |_, _| false);
    }

    /// The whole view, mutable: in place when this is the buffer's only
    /// handle, on a copy otherwise.
    #[expect(
        unsafe_code,
        reason = "the only handle of a buffer owns every slot of it"
    )]
    pub fn make_mut(&mut self) -> &mut [T] {
        self.reclaim_if_sole();
        if Arc::get_mut(&mut self.buffer).is_none() {
            self.grow(0);
        }
        // SAFETY: this is the buffer's only handle (`grow` made a fresh
        // one), so nothing else reads its slots while the borrow lives.
        unsafe { std::slice::from_raw_parts_mut(self.buffer.ptr.as_ptr(), self.len) }
    }

    pub fn to_vec(&self) -> Vec<T> {
        self.as_slice().to_vec()
    }
}

impl<T> Drop for AppendVec<T> {
    fn drop(&mut self) {
        self.buffer.state.lock_recover().unregister(self.len);
    }
}

impl<T> From<Vec<T>> for AppendVec<T> {
    fn from(values: Vec<T>) -> Self {
        let len = values.len();
        Self {
            buffer: Arc::new(Buffer::from_vec(values)),
            len,
        }
    }
}

impl<T: Clone> Extend<T> for AppendVec<T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, values: I) {
        self.extend_adopting(values, |_, _| false);
    }
}

impl<T> FromIterator<T> for AppendVec<T> {
    fn from_iter<I: IntoIterator<Item = T>>(values: I) -> Self {
        Self::from(values.into_iter().collect::<Vec<_>>())
    }
}

impl<T> Clone for AppendVec<T> {
    fn clone(&self) -> Self {
        self.buffer.state.lock_recover().register(self.len);
        Self {
            buffer: Arc::clone(&self.buffer),
            len: self.len,
        }
    }
}

impl<T> Default for AppendVec<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> std::ops::Deref for AppendVec<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T> AsRef<[T]> for AppendVec<T> {
    fn as_ref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<'a, T> IntoIterator for &'a AppendVec<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.as_slice().iter()
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for AppendVec<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.as_slice()).finish()
    }
}

impl<T: PartialEq> PartialEq for AppendVec<T> {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: Eq> Eq for AppendVec<T> {}

impl<T: serde::Serialize> serde::Serialize for AppendVec<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.as_slice().serialize(serializer)
    }
}

impl<'de, T: serde::Deserialize<'de>> serde::Deserialize<'de> for AppendVec<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Vec::<T>::deserialize(deserializer).map(Self::from)
    }
}

#[cfg(test)]
#[path = "append_vec_tests.rs"]
mod tests;
