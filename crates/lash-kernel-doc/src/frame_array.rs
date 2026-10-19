//! Process-lifetime frame indexes use private mappings, so they do not alter
//! the allocator's placement of each run's short-lived objects.
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;

#[derive(Debug)]
struct Region {
    pointer: NonNull<u8>,
    bytes: usize,
}
impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: this owner holds the exact mapping returned by mmap.
        #[expect(unsafe_code, reason = "release the private frame mapping")]
        unsafe {
            libc::munmap(self.pointer.as_ptr().cast(), self.bytes);
        }
    }
}
#[derive(Debug)]
struct Mapping<T> {
    region: Region,
    length: usize,
    element: PhantomData<T>,
}
impl<T> Drop for Mapping<T> {
    fn drop(&mut self) {
        // SAFETY: only this initialized prefix contains T. Region is a field
        // owner so its destructor also runs if dropping an element panics.
        #[expect(
            unsafe_code,
            reason = "drop initialized mapped elements before unmapping"
        )]
        unsafe {
            std::ptr::drop_in_place(std::ptr::slice_from_raw_parts_mut(
                self.region.pointer.as_ptr().cast::<T>(),
                self.length,
            ));
        }
    }
}
#[derive(Debug)]
enum Storage<T> {
    Heap(Vec<T>),
    Mapped(Mapping<T>),
}

/// An owned array whose frame form is allocated independently of the run heap.
#[derive(Debug)]
pub struct FrameArray<T> {
    pointer: NonNull<T>,
    length: usize,
    storage: Storage<T>,
}

impl<T> Default for FrameArray<T> {
    fn default() -> Self {
        Vec::new().into()
    }
}
impl<T> From<Vec<T>> for FrameArray<T> {
    fn from(mut values: Vec<T>) -> Self {
        Self {
            pointer: NonNull::new(values.as_mut_ptr()).unwrap_or_else(NonNull::dangling),
            length: values.len(),
            storage: Storage::Heap(values),
        }
    }
}
impl<T> FromIterator<T> for FrameArray<T> {
    fn from_iter<I: IntoIterator<Item = T>>(values: I) -> Self {
        values.into_iter().collect::<Vec<_>>().into()
    }
}

impl<T> FrameArray<T> {
    pub fn with_capacity(capacity: usize) -> Self {
        Vec::with_capacity(capacity).into()
    }

    /// Allocates a fixed mapping and initializes each element exactly once.
    pub fn mapped(length: usize, mut make: impl FnMut(usize) -> T) -> std::io::Result<Self> {
        let bytes = length
            .checked_mul(std::mem::size_of::<T>())
            .ok_or_else(|| std::io::Error::other("frame array size overflow"))?;
        if bytes == 0 {
            return Ok((0..length).map(make).collect());
        }
        // SAFETY: mmap returns a fresh writable region. Its owner is armed
        // before any initializer runs; no uninitialized element is read.
        #[expect(
            unsafe_code,
            reason = "frame indexes live outside the guest-run allocator"
        )]
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                bytes,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if address == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        let Some(pointer) = NonNull::new(address.cast::<u8>()) else {
            #[expect(unsafe_code, reason = "release an unusable mapping")]
            unsafe {
                libc::munmap(address, bytes);
            }
            return Err(std::io::Error::other("null frame mapping"));
        };
        let region = Region { pointer, bytes };
        if !(pointer.as_ptr() as usize).is_multiple_of(std::mem::align_of::<T>()) {
            return Err(std::io::Error::other(
                "frame mapping cannot align this element",
            ));
        }
        let mut mapping = Mapping {
            region,
            length: 0,
            element: PhantomData::<T>,
        };
        let pointer = pointer.cast::<T>();
        for at in 0..length {
            let value = make(at);
            #[expect(
                unsafe_code,
                reason = "initialize each mapped slot once before publication"
            )]
            unsafe {
                pointer.as_ptr().add(at).write(value);
            }
            mapping.length += 1;
        }
        Ok(Self {
            pointer,
            length,
            storage: Storage::Mapped(mapping),
        })
    }

    pub fn push(&mut self, value: T) {
        let Storage::Heap(values) = &mut self.storage else {
            unreachable!("fixed frame length")
        };
        values.push(value);
        self.pointer = NonNull::new(values.as_mut_ptr()).unwrap_or_else(NonNull::dangling);
        self.length = values.len();
    }

    pub fn resize(&mut self, length: usize, value: T)
    where
        T: Clone,
    {
        let Storage::Heap(values) = &mut self.storage else {
            unreachable!("fixed frame length")
        };
        // Publish the Vec's pointer and initialized length even if cloning T
        // unwinds after a reallocation or a partially initialized suffix.
        struct Refresh<'a, T> {
            values: &'a mut Vec<T>,
            pointer: &'a mut NonNull<T>,
            length: &'a mut usize,
        }
        impl<T> Drop for Refresh<'_, T> {
            fn drop(&mut self) {
                *self.pointer =
                    NonNull::new(self.values.as_mut_ptr()).unwrap_or_else(NonNull::dangling);
                *self.length = self.values.len();
            }
        }
        let guard = Refresh {
            values,
            pointer: &mut self.pointer,
            length: &mut self.length,
        };
        guard.values.resize(length, value);
    }
}
impl<T> Deref for FrameArray<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        debug_assert_eq!(
            self.length,
            match &self.storage {
                Storage::Heap(values) => values.len(),
                Storage::Mapped(mapping) => mapping.length,
            }
        );
        // SAFETY: storage owns this initialized prefix for the borrow's life.
        #[expect(
            unsafe_code,
            reason = "borrow either backing allocation without a hot-path branch"
        )]
        unsafe {
            std::slice::from_raw_parts(self.pointer.as_ptr(), self.length)
        }
    }
}
impl<T> DerefMut for FrameArray<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        // SAFETY: the exclusive owner lends its initialized prefix.
        #[expect(unsafe_code, reason = "exclusive access to either backing allocation")]
        unsafe {
            std::slice::from_raw_parts_mut(self.pointer.as_ptr(), self.length)
        }
    }
}
// SAFETY: the allocation is exclusively owned, with Vec's sharing rules.
#[expect(
    unsafe_code,
    reason = "owned elements have the same Send/Sync bounds as a Vec"
)]
unsafe impl<T: Send> Send for FrameArray<T> {}
// SAFETY: shared access only exposes shared references to initialized T.
#[expect(
    unsafe_code,
    reason = "owned elements have the same Send/Sync bounds as a Vec"
)]
unsafe impl<T: Sync> Sync for FrameArray<T> {}
impl<'a, T> IntoIterator for &'a FrameArray<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}
impl<'a, T> IntoIterator for &'a mut FrameArray<T> {
    type Item = &'a mut T;
    type IntoIter = std::slice::IterMut<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::FrameArray;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Ownership at the claim: unwinding an external initializer releases
    /// every constructed element even though no array was returned.
    #[test]
    fn a_frame_owns_its_initialized_prefix_before_the_next_callback() {
        struct Owned(Arc<AtomicUsize>);
        impl Drop for Owned {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicUsize::new(0));
        let result = std::panic::catch_unwind({
            let dropped = dropped.clone();
            move || {
                FrameArray::mapped(8, |at| {
                    assert_ne!(at, 3, "initializer unwinds");
                    Owned(dropped.clone())
                })
            }
        });
        assert!(result.is_err());
        assert_eq!(dropped.load(Ordering::SeqCst), 3);
    }
}
