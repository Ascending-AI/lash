// Copyright (c) 2020 ridiculous_fish; https://github.com/ridiculousfish/regress @ 7e64ad5e6807b5503e5cc97a79e0f129b23c556b; MIT licensed; modified: fuel/step-budget instrumentation.
use core::cmp::Eq;
use core::fmt;
use core::marker::PhantomData;
use core::ops;

/// A trait which references a position in an input string.
/// The intent is that this may be satisfied via indexes or pointers.
/// Positions must be subtractable, producing usize; they also obey other "pointer arithmetic" ideas.
pub trait PositionType:
    core::fmt::Debug + Copy + Clone + PartialEq + Eq + PartialOrd + Ord
where
    Self: ops::Add<usize, Output = Self>,
    Self: ops::Sub<usize, Output = Self>,
    Self: ops::Sub<Self, Output = usize>,
    Self: ops::AddAssign<usize>,
    Self: ops::SubAssign<usize>,
{
}

/// Choose the preferred position type with this alias.
#[cfg(feature = "index-positions")]
pub type DefPosition<'a> = IndexPosition<'a>;

#[cfg(not(feature = "index-positions"))]
pub type DefPosition<'a> = RefPosition<'a>;

/// A simple index-based position.
/// It remembers the lifetime of the slice it is tied to.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct IndexPosition<'a>(usize, PhantomData<&'a ()>);

impl<'a> fmt::Debug for IndexPosition<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("IndexPosition").field(&self.0).finish()
    }
}

#[allow(dead_code)]
impl IndexPosition<'_> {
    /// IndexPosition does not enforce its size.
    #[inline(always)]
    pub fn check_size() {}

    #[inline(always)]
    pub fn new(pos: usize) -> Self {
        Self(pos, PhantomData)
    }
}

impl ops::Add<usize> for IndexPosition<'_> {
    type Output = Self;

    #[inline(always)]
    fn add(self, rhs: usize) -> Self::Output {
        debug_assert!(self.0 + rhs >= self.0, "Overflow");
        IndexPosition(self.0 + rhs, PhantomData)
    }
}

impl ops::AddAssign<usize> for IndexPosition<'_> {
    #[inline(always)]
    fn add_assign(&mut self, rhs: usize) {
        *self = *self + rhs;
    }
}

impl ops::SubAssign<usize> for IndexPosition<'_> {
    #[inline(always)]
    fn sub_assign(&mut self, rhs: usize) {
        *self = *self - rhs;
    }
}

impl<'a> ops::Sub<IndexPosition<'a>> for IndexPosition<'a> {
    type Output = usize;

    #[inline(always)]
    fn sub(self, rhs: Self) -> Self::Output {
        debug_assert!(self.0 >= rhs.0, "Underflow");
        self.0 - rhs.0
    }
}

impl ops::Sub<usize> for IndexPosition<'_> {
    type Output = Self;

    #[inline(always)]
    fn sub(self, rhs: usize) -> Self::Output {
        debug_assert!(self.0 >= rhs, "Underflow");
        IndexPosition(self.0 - rhs, PhantomData)
    }
}

impl PositionType for IndexPosition<'_> {}

/// A reference position holds a reference to a byte and uses pointer arithmetic.
/// This must use raw pointers because it must be capable of representing the one-past-the-end value.
/// TODO: thread lifetimes through this.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RefPosition<'a>(core::ptr::NonNull<u8>, PhantomData<&'a ()>);

impl<'a> fmt::Debug for RefPosition<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let addr = self.0.as_ptr() as usize;
        f.debug_tuple("RefPosition").field(&addr).finish()
    }
}

#[allow(dead_code)]
impl RefPosition<'_> {
    /// The big idea of RefPosition is that `Option<RefPosition>` becomes pointer-sized, by using nullptr as the None value.
    /// Good candidate for const-panics when stabilized.
    #[inline(always)]
    pub fn check_size() {
        if core::mem::size_of::<Option<Self>>() > core::mem::size_of::<*const u8>() {
            panic!("Option<RefPosition> should be pointer sized")
        }
    }

    /// Access the underlying pointer.
    #[inline(always)]
    pub fn ptr(self) -> *const u8 {
        self.0.as_ptr()
    }

    /// Construct from a pointer, which must never be null.
    #[inline(always)]
    #[expect(
        unsafe_code,
        reason = "all internal constructors provide a non-null slice-derived pointer"
    )]
    pub fn new(ptr: *const u8) -> Self {
        debug_assert!(!ptr.is_null(), "Pointer cannot be null");
        // Annoyingly there's no *const NonNull.
        let mutp = ptr as *mut u8;
        let nonnullp = if cfg!(feature = "prohibit-unsafe") {
            core::ptr::NonNull::new(mutp).expect("Pointer was null")
        } else {
            // SAFETY: every caller passes either a slice-derived pointer or an
            // in-bounds offset from one, both of which are non-null.
            unsafe { core::ptr::NonNull::new_unchecked(mutp) }
        };
        Self(nonnullp, PhantomData)
    }
}

impl PositionType for RefPosition<'_> {}

impl ops::Add<usize> for RefPosition<'_> {
    type Output = Self;

    #[inline(always)]
    #[expect(
        unsafe_code,
        reason = "position arithmetic remains within its originating input allocation"
    )]
    fn add(self, rhs: usize) -> Self::Output {
        // SAFETY: matcher positions and offsets are maintained within the
        // originating input allocation, including its one-past-the-end value.
        Self::new(unsafe { self.ptr().add(rhs) })
    }
}

impl<'a> ops::Sub<RefPosition<'a>> for RefPosition<'a> {
    type Output = usize;

    #[inline(always)]
    #[expect(
        unsafe_code,
        reason = "positions being subtracted belong to the same input allocation"
    )]
    fn sub(self, rhs: Self) -> Self::Output {
        debug_assert!(self.0 >= rhs.0, "Underflow");
        // Note Rust has backwards naming here. The "origin" is self, not the param; the rhs is the offset value.
        // SAFETY: both positions originate from the same input slice and
        // matcher state preserves their order.
        unsafe { self.ptr().offset_from(rhs.ptr()) as usize }
    }
}

impl ops::Sub<usize> for RefPosition<'_> {
    type Output = Self;

    #[inline(always)]
    #[expect(
        unsafe_code,
        reason = "position arithmetic remains within its originating input allocation"
    )]
    fn sub(self, rhs: usize) -> Self::Output {
        debug_assert!(self.ptr() as usize >= rhs, "Underflow");
        // SAFETY: matcher offsets never move a position before the originating
        // input allocation.
        Self::new(unsafe { self.ptr().sub(rhs) })
    }
}

impl ops::AddAssign<usize> for RefPosition<'_> {
    #[inline(always)]
    fn add_assign(&mut self, rhs: usize) {
        *self = *self + rhs;
    }
}

impl ops::SubAssign<usize> for RefPosition<'_> {
    #[inline(always)]
    fn sub_assign(&mut self, rhs: usize) {
        *self = *self - rhs;
    }
}
