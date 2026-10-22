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
//! never observed by them. The buffer is replaced only when it is full (by
//! one at least twice as long as its contents, amortized O(1)), or when the
//! appending handle is behind the buffer's tip and its value is not the one
//! already there (a fork).
//!
//! Every initialized slot a live handle other than the editing one can see is
//! immutable. The buffer counts its live handles by length, so an edit knows
//! exactly which slots nobody else can see: a reader that has been dropped
//! blocks nothing.

use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use crate::allocation::{AllocationBudget, AllocationError, AllocationHold, SourceCapacity};
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
    budget: AllocationBudget,
    _allocation: AllocationHold,
    _owns: PhantomData<T>,
}

struct BufferState {
    /// Slots `[0, initialized)` hold values; the rest are uninitialized.
    initialized: usize,
    /// Each subsequently claimed value's owned fields, alongside its slot.
    owned: Vec<AllocationHold>,
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
    fn from_vec(
        values: Vec<T>,
        capacity: usize,
        budget: AllocationBudget,
        allocation: AllocationHold,
    ) -> Self {
        let mut values = std::mem::ManuallyDrop::new(values);
        let initialized = values.len();
        let ptr = NonNull::new(values.as_mut_ptr()).unwrap_or(NonNull::dangling());
        let mut owned = Vec::with_capacity(capacity);
        owned.resize_with(initialized, AllocationHold::default);
        let mut state = BufferState {
            initialized,
            owned,
            handles: BTreeMap::new(),
        };
        state.register(initialized);
        Self {
            ptr,
            capacity,
            state: Mutex::new(state),
            budget,
            _allocation: allocation,
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
    /// The ledger that owns this buffer and every future detached buffer.
    pub fn allocation_budget(&self) -> &AllocationBudget {
        &self.buffer.budget
    }

    /// Allocate source slots and their handle registry under an owner budget.
    /// The caller separately owns any allocations reachable through `T`.
    pub fn try_with_capacity(
        capacity: usize,
        budget: &AllocationBudget,
    ) -> Result<Self, AllocationError> {
        let allocation = budget.reserve(Self::buffer_capacity_bytes(capacity))?;
        let values = Vec::with_capacity(capacity);
        Ok(Self::from_reserved(
            values,
            capacity,
            budget.clone(),
            allocation,
        ))
    }

    fn buffer_capacity_bytes(capacity: usize) -> u64 {
        // At most capacity + 1 distinct lengths can be live. Pre-admit a
        // conservative B-tree node per length, so clone, truncate and Drop
        // never need a fallible reservation while holding the registry lock.
        // A clone's length is already registered by its source handle.
        (capacity as u64)
            .saturating_mul(
                (std::mem::size_of::<T>() + std::mem::size_of::<AllocationHold>()) as u64,
            )
            .saturating_add((capacity as u64).saturating_add(1).saturating_mul(512))
            .saturating_add(
                (std::mem::size_of::<Buffer<T>>() + 2 * std::mem::size_of::<usize>()) as u64,
            )
    }

    fn from_reserved(
        values: Vec<T>,
        capacity: usize,
        budget: AllocationBudget,
        allocation: AllocationHold,
    ) -> Self {
        let len = values.len();
        Self {
            buffer: Arc::new(Buffer::from_vec(values, capacity, budget, allocation)),
            len,
        }
    }

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
        while state.initialized > len {
            state.initialized -= 1;
            // SAFETY: this buffer has no other handle, and moving the last
            // initialized value out withdraws it before any destructor runs.
            let value = unsafe { buffer.ptr.as_ptr().add(state.initialized).read() };
            let owned = state.owned.pop();
            drop(value);
            drop(owned);
        }
    }

    /// Writes `value` into the next slot when this handle is the buffer's
    /// tip and the buffer has room, answering it back otherwise.
    #[expect(
        unsafe_code,
        reason = "the slot at the initialized length is uninitialized and within capacity"
    )]
    fn push_in_place(
        &mut self,
        value: T,
        owned: AllocationHold,
    ) -> Result<(), (T, AllocationHold)> {
        self.reclaim_if_sole();
        let mut state = self.buffer.state.lock_recover();
        if state.initialized != self.len || self.len == self.buffer.capacity {
            return Err((value, owned));
        }
        // SAFETY: `len < capacity`, and the slot at `initialized` is
        // uninitialized; no handle reads it until `initialized` covers it,
        // which it does only under this lock, after the write.
        unsafe { self.buffer.ptr.as_ptr().add(self.len).write(value) };
        state.owned.push(owned);
        state.initialized += 1;
        state.relength(self.len, self.len + 1);
        self.len += 1;
        Ok(())
    }
}

impl<T: Clone + SourceCapacity> AppendVec<T> {
    /// Append after admitting any new buffer; refusal leaves this view intact.
    pub fn push(&mut self, value: T) -> Result<(), AllocationError> {
        self.push_adopting(value, |_, _| false)
    }

    /// Appends `value`, adopting the buffer's next slot instead when another
    /// handle already wrote the same value there (`same(existing, value)`).
    ///
    /// Two handles that append the same values from the same point, such
    /// as a turn's read state and the graph that commits it, then share one
    /// buffer instead of forking it.
    pub fn push_adopting(
        &mut self,
        value: T,
        same: impl Fn(&T, &T) -> bool,
    ) -> Result<(), AllocationError> {
        if self.try_adopt(&value, &same) {
            return Ok(());
        }
        let owned = self.reserve_owned(&value)?;
        self.push_prepared(value, owned)
    }

    /// Reserve before the source's owned fields are cloned into a new slot.
    pub fn push_cloned_adopting(
        &mut self,
        value: &T,
        same: impl Fn(&T, &T) -> bool,
    ) -> Result<(), AllocationError> {
        if self.try_adopt(value, &same) {
            return Ok(());
        }
        let owned = self.reserve_owned(value)?;
        self.reserve(1)?;
        self.push_prepared(value.clone(), owned)
    }

    fn reserve_owned(&self, value: &T) -> Result<AllocationHold, AllocationError> {
        if self.buffer.budget.identity() == 0 {
            return Ok(AllocationHold::default());
        }
        let bytes = value.owned_capacity();
        if bytes == 0 {
            return Ok(AllocationHold::default());
        }
        self.buffer.budget.reserve(bytes)
    }

    fn push_prepared(&mut self, value: T, owned: AllocationHold) -> Result<(), AllocationError> {
        // Keep the pair intact across a fallible grow: tuple fields drop in
        // order, so the value's destructor runs before its obligation ends.
        let prepared = match self.push_in_place(value, owned) {
            Ok(()) => return Ok(()),
            Err(value) => value,
        };
        self.try_grow(1)?;
        if self.push_in_place(prepared.0, prepared.1).is_err() {
            unreachable!("a freshly grown buffer is private and has room for one more");
        }
        Ok(())
    }

    /// Each admitted append is visible in this private view; a later refusal
    /// leaves the admitted prefix intact for the caller to discard or keep.
    pub fn extend_adopting(
        &mut self,
        values: impl IntoIterator<Item = T>,
        same: impl Fn(&T, &T) -> bool,
    ) -> Result<(), AllocationError> {
        let values = values.into_iter();
        self.reserve(values.size_hint().0)?;
        for value in values {
            self.push_adopting(value, &same)?;
        }
        Ok(())
    }

    /// Ensures that `additional` appends from this handle write in place,
    /// unless another handle takes the tip first.
    pub fn reserve(&mut self, additional: usize) -> Result<(), AllocationError> {
        if additional == 0 {
            return Ok(());
        }
        self.reclaim_if_sole();
        let at_tip = self.buffer.state.lock_recover().initialized == self.len;
        if at_tip && self.buffer.capacity - self.len < additional {
            self.try_grow(additional)?;
        }
        // Behind the tip, appends adopt or fork value by value.
        Ok(())
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
    ///
    /// The new buffer is at most half full, so a copy of `len` slots is
    /// followed by at least `len` appends in place. A batch larger than the
    /// view doubles what it needs: sized exactly, the buffer would be full
    /// again and the very next append would copy the whole batch (FIG-5673).
    fn try_grow(&mut self, additional: usize) -> Result<(), AllocationError> {
        let doubled = self.len.saturating_mul(2);
        let required = self.len.saturating_add(additional);
        let capacity = if required > doubled {
            required.saturating_mul(2)
        } else {
            doubled
        }
        .max(MIN_GROWN_CAPACITY);
        let budget = self.buffer.budget.clone();
        let owned = if budget.identity() == 0 {
            0
        } else {
            self.as_slice().iter().fold(0u64, |bytes, value| {
                bytes.saturating_add(value.owned_capacity())
            })
        };
        let allocation =
            budget.reserve(Self::buffer_capacity_bytes(capacity).saturating_add(owned))?;
        let mut values = Vec::with_capacity(capacity);
        values.extend_from_slice(self.as_slice());
        *self = Self::from_reserved(values, capacity, budget, allocation);
        Ok(())
    }

    /// Replaces the slots from `start` on with `values`: in place when no
    /// other live handle sees any of them (a commit rewriting the records it
    /// just appended), and on a copy of this view otherwise. `start` past
    /// the end appends.
    ///
    /// Fallible editing for private source drafts. A refused later append
    /// leaves its already admitted prefix; the caller owns draft publication.
    #[expect(
        unsafe_code,
        reason = "rewrites and drops initialized slots no other live handle can see, under the buffer lock"
    )]
    pub fn replace_from(
        &mut self,
        start: usize,
        values: impl IntoIterator<Item = T>,
    ) -> Result<(), AllocationError> {
        let start = start.min(self.len);
        let mut values = values.into_iter();
        let old_count = self.len - start;
        let capacity = old_count.max(values.size_hint().0);
        let bytes = |capacity: usize| {
            (capacity as u64)
                .saturating_mul(std::mem::size_of::<(T, AllocationHold)>() as u64)
                .saturating_add(128)
        };
        let mut scratch = self.buffer.budget.reserve(bytes(capacity))?;
        let mut prepared = Vec::with_capacity(capacity);
        // Materialize and admit the complete proposal before changing any
        // slot. A tuple destroys its value before its associated hold.
        for value in values.by_ref() {
            let owned = self.reserve_owned(&value)?;
            let value = (value, owned);
            if prepared.len() == prepared.capacity() {
                let capacity = prepared.len().saturating_mul(2).max(MIN_GROWN_CAPACITY);
                let next_scratch = self.buffer.budget.reserve(bytes(capacity))?;
                prepared.reserve_exact(capacity - prepared.len());
                scratch = next_scratch;
            }
            prepared.push(value);
        }
        let incoming = prepared.len();
        let end = start.saturating_add(incoming);
        let in_place = {
            let mut state = self.buffer.state.lock_recover();
            let unseen = state.initialized == self.len
                && state.longest_other(self.len) <= start
                && end <= self.buffer.capacity;
            if unseen {
                let overlap = incoming.min(old_count);
                for (offset, (value, owned)) in prepared[..overlap].iter_mut().enumerate() {
                    // SAFETY: these initialized slots are unseen by every
                    // other handle, and swaps run no destructor.
                    unsafe {
                        std::ptr::swap(self.buffer.ptr.as_ptr().add(start + offset), value);
                    }
                    std::mem::swap(&mut state.owned[start + offset], owned);
                }
                if incoming > old_count {
                    state.owned.resize_with(end, AllocationHold::default);
                    for offset in (old_count..incoming).rev() {
                        let (value, owned) = prepared
                            .pop()
                            .unwrap_or_else(|| unreachable!("the incoming suffix is prepared"));
                        // SAFETY: these slots are inside the allocation,
                        // uninitialized, and still invisible to readers.
                        unsafe {
                            self.buffer.ptr.as_ptr().add(start + offset).write(value);
                        }
                        state.owned[start + offset] = owned;
                    }
                } else {
                    for index in end..self.len {
                        // SAFETY: the removed slots are initialized and
                        // unseen. Move them to already-admitted scratch so
                        // their destructors run only after unlock.
                        let value = unsafe { self.buffer.ptr.as_ptr().add(index).read() };
                        let owned = std::mem::take(&mut state.owned[index]);
                        prepared.push((value, owned));
                    }
                    state.owned.truncate(end);
                }
                state.initialized = end;
                state.relength(self.len, end);
                self.len = end;
            }
            unseen
        };
        if !in_place {
            // Readers saw a changed slot, or this buffer has no room. Build
            // the replacement privately; refusal keeps the original view.
            let mut next = Self::try_with_capacity(end, &self.buffer.budget)?;
            for value in &self.as_slice()[..start] {
                next.push_cloned_adopting(value, |_, _| false)?;
            }
            for (value, owned) in prepared.drain(..) {
                next.push_prepared(value, owned)?;
            }
            *self = next;
        }
        drop(prepared);
        drop(scratch);
        Ok(())
    }

    /// The whole view, mutable: in place when this is the buffer's only
    /// handle, on a copy otherwise.
    #[expect(
        unsafe_code,
        reason = "the only handle of a buffer owns every slot of it"
    )]
    pub fn make_mut(&mut self) -> Result<&mut [T], AllocationError> {
        self.reclaim_if_sole();
        if Arc::get_mut(&mut self.buffer).is_none() {
            self.try_grow(0)?;
        }
        // SAFETY: this is the buffer's only handle (`grow` made a fresh
        // one), so nothing else reads its slots while the borrow lives.
        Ok(unsafe { std::slice::from_raw_parts_mut(self.buffer.ptr.as_ptr(), self.len) })
    }

    pub fn to_vec(&self) -> Vec<T> {
        self.as_slice().to_vec()
    }
}

impl<T> Drop for AppendVec<T> {
    #[expect(
        unsafe_code,
        reason = "moves an initialized tail slot out under the registry lock after its last reader unregisters"
    )]
    fn drop(&mut self) {
        let mut unregister = true;
        loop {
            let value = {
                let mut state = self.buffer.state.lock_recover();
                if unregister {
                    state.unregister(self.len);
                    unregister = false;
                }
                if state.handles.is_empty() {
                    // The final buffer owner drops its remaining slots in one
                    // pass through Buffer::drop, without taking the lock per slot.
                    break;
                }
                let visible = state.handles.last_key_value().map_or(0, |(len, _)| *len);
                if state.initialized <= visible {
                    break;
                }
                state.initialized -= 1;
                // SAFETY: no registered handle sees this slot. Moving it out
                // and shortening the initialized range happen under the same
                // lock as appends and adoption. Another writer may reuse the
                // slot only after the value has been moved out.
                let value = unsafe { self.buffer.ptr.as_ptr().add(state.initialized).read() };
                let owned = state
                    .owned
                    .pop()
                    .unwrap_or_else(|| unreachable!("every initialized slot has its obligation"));
                (value, owned)
            };
            // A value's destructor may call back into a holder of this buffer.
            // Never run it under the registry lock or allocate cleanup storage.
            drop(value);
        }
    }
}

impl<T> From<Vec<T>> for AppendVec<T> {
    fn from(values: Vec<T>) -> Self {
        let capacity = if std::mem::size_of::<T>() == 0 {
            values.len().max(MIN_GROWN_CAPACITY)
        } else {
            values.capacity()
        };
        Self::from_reserved(
            values,
            capacity,
            AllocationBudget::unowned(),
            AllocationHold::default(),
        )
    }
}

impl<T: Clone + SourceCapacity> AppendVec<T> {
    pub fn extend(&mut self, values: impl IntoIterator<Item = T>) -> Result<(), AllocationError> {
        self.extend_adopting(values, |_, _| false)
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
