//! Standard-lock poison recovery policy.
//!
//! A panic while holding a standard-library lock marks the lock poisoned but
//! does not prove that its protected value is unusable. Lash therefore always
//! recovers the guard carried by [`std::sync::PoisonError`]. These extension
//! methods are the one workspace-wide acquisition idiom for `Mutex` and
//! `RwLock`; callers must repair any domain invariant that needs stronger
//! guarantees in the operation that owns that invariant. Unwinds are contained
//! only where host-supplied provider and tool implementations enter Lash; lock
//! acquisition is not an error boundary and never creates a typed poison tier.

use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError};

/// Recover the value carried by a standard-library poison result.
pub trait LockResultExt<T> {
    /// Return the successful value or the value retained by `PoisonError`.
    fn recover(self) -> T;
}

impl<T> LockResultExt<T> for std::sync::LockResult<T> {
    fn recover(self) -> T {
        self.unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Poison-recovering acquisition for [`Mutex`].
pub trait MutexExt<T: ?Sized> {
    /// Acquire the mutex, recovering the protected guard if it was poisoned.
    fn lock_recover(&self) -> MutexGuard<'_, T>;

    /// Try to acquire the mutex. Contention returns `None`; poison recovers the
    /// protected guard just like [`MutexExt::lock_recover`].
    fn try_lock_recover(&self) -> Option<MutexGuard<'_, T>>;
}

impl<T: ?Sized> MutexExt<T> for Mutex<T> {
    fn lock_recover(&self) -> MutexGuard<'_, T> {
        self.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn try_lock_recover(&self) -> Option<MutexGuard<'_, T>> {
        match self.try_lock() {
            Ok(guard) => Some(guard),
            Err(TryLockError::Poisoned(error)) => Some(error.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }
}

/// Poison-recovering acquisition for [`RwLock`].
pub trait RwLockExt<T: ?Sized> {
    /// Acquire a shared guard, recovering it if the lock was poisoned.
    fn read_recover(&self) -> RwLockReadGuard<'_, T>;

    /// Acquire an exclusive guard, recovering it if the lock was poisoned.
    fn write_recover(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T: ?Sized> RwLockExt<T> for RwLock<T> {
    fn read_recover(&self) -> RwLockReadGuard<'_, T> {
        self.read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write_recover(&self) -> RwLockWriteGuard<'_, T> {
        self.write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::{MutexExt as _, RwLockExt as _};
    use std::sync::{Arc, Mutex, RwLock};

    #[test]
    fn poisoned_mutex_recovers_the_protected_value() {
        let value = Arc::new(Mutex::new(1_u8));
        let poison_target = Arc::clone(&value);
        let join = std::thread::spawn(move || {
            let mut guard = poison_target.lock_recover();
            *guard = 2;
            panic!("poison mutex");
        });
        assert!(join.join().is_err());

        *value.lock_recover() += 1;
        assert_eq!(*value.lock_recover(), 3);
    }

    #[test]
    fn poisoned_rwlock_recovers_the_protected_value() {
        let value = Arc::new(RwLock::new(1_u8));
        let poison_target = Arc::clone(&value);
        let join = std::thread::spawn(move || {
            let mut guard = poison_target.write_recover();
            *guard = 2;
            panic!("poison rwlock");
        });
        assert!(join.join().is_err());

        *value.write_recover() += 1;
        assert_eq!(*value.read_recover(), 3);
    }
}
