//! A claim's state is cleared by the last participant in release/unlock.
use lash_durable::{DurableError, StoreFailure, StoreFailureKind};
use lash_protocol_types::sync::MutexExt as _;
use std::ops::{Deref, DerefMut};

#[derive(Debug)]
pub(super) struct ClaimCell<T> {
    live: std::sync::Mutex<bool>,
    value: tokio::sync::Mutex<Option<T>>,
}

impl<T> ClaimCell<T> {
    pub(super) fn new(value: T) -> Self {
        Self {
            live: std::sync::Mutex::new(true),
            value: tokio::sync::Mutex::new(Some(value)),
        }
    }

    pub(super) async fn lock(&self) -> Result<ClaimGuard<'_, T>, DurableError> {
        // Own cancellation cleanup before the semaphore can assign us a permit.
        let acquisition = ClaimAcquisition(self);
        let mut value = self.value.lock().await;
        drop(acquisition);
        if !*self.live.lock_recover() {
            let released = value.take();
            drop(value);
            drop(released);
            return Err(DurableError::Store(StoreFailure {
                code: None,
                kind: StoreFailureKind::Unavailable,
                message: "session claim has ended".into(),
            }));
        }
        Ok(ClaimGuard {
            cell: self,
            value: Some(value),
        })
    }

    #[cfg(test)]
    pub(super) async fn retains_value_for_testing(&self) -> bool {
        self.value.lock().await.is_some()
    }

    pub(super) fn release(&self) {
        // Serialize with the guard's *unlock*, not just its liveness check.
        // Either this takes the value, or the current holder must take it.
        *self.live.lock_recover() = false;
        self.clear_if_released();
    }

    fn clear_if_released(&self) {
        let released = {
            let live = self.live.lock_recover();
            if *live {
                None
            } else {
                self.value
                    .try_lock()
                    .ok()
                    .and_then(|mut value| value.take())
            }
        };
        drop(released);
    }
}

struct ClaimAcquisition<'a, T>(&'a ClaimCell<T>);
impl<T> Drop for ClaimAcquisition<'_, T> {
    fn drop(&mut self) {
        self.0.clear_if_released();
    }
}

pub(super) struct ClaimGuard<'a, T> {
    cell: &'a ClaimCell<T>,
    value: Option<tokio::sync::MutexGuard<'a, Option<T>>>,
}

impl<T> Drop for ClaimGuard<'_, T> {
    fn drop(&mut self) {
        let released = {
            let live = self.cell.live.lock_recover();
            self.value.take().and_then(|mut value| {
                let released = if *live { None } else { value.take() };
                // Release cannot miss us between checking `live` and unlocking.
                drop(value);
                released
            })
        };
        drop(released);
    }
}

#[expect(
    clippy::expect_used,
    reason = "a live guard owns the populated slot until its Drop"
)]
impl<T> Deref for ClaimGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.value
            .as_ref()
            .and_then(|value| value.as_ref())
            .expect("live claim guard")
    }
}
#[expect(
    clippy::expect_used,
    reason = "a live guard owns the populated slot until its Drop"
)]
impl<T> DerefMut for ClaimGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.value
            .as_mut()
            .and_then(|value| value.as_mut())
            .expect("live claim guard")
    }
}

#[cfg(test)]
mod tests {
    use super::ClaimCell;
    use std::{
        future::Future,
        sync::Arc,
        task::{Context, Poll, Waker},
    };

    #[tokio::test]
    async fn a_cancelled_queued_reader_releases_the_claims_last_reference() {
        let value = Arc::new(());
        let witness = Arc::downgrade(&value);
        let cell = ClaimCell::new(value);
        let holder = cell.lock().await.unwrap();
        let mut waiter = Box::pin(cell.lock());
        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(waiter.as_mut().poll(&mut cx), Poll::Pending));
        // Unlock assigns the mutex permit to the queued future, which has not
        // resumed to construct a ClaimGuard. Release must cover this state too.
        drop(holder);
        cell.release();
        drop(waiter);
        assert!(
            witness.upgrade().is_none(),
            "a cancelled waiter kept the released value"
        );
    }
}
