//! Issued X bodies progress beside every owner wait; their results do not.

use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Waker;

use lash_sansio::sync::MutexExt as _;

use super::super::singleton_run::RunAttemptBody;

/// The bodies a logical Run issued, shared by its owner's waits.
#[derive(Clone)]
pub struct RunBodies<'a>(Arc<Shared<'a>>);

struct Shared<'a> {
    bodies: Mutex<Vec<RunAttemptBody<'a>>>,
    waker: Mutex<Option<Waker>>,
    driving: AtomicBool,
}

/// Clear the shared driving mark however the poll leaves.
struct DriveGuard<'s>(&'s AtomicBool);

impl Drop for DriveGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl<'a> RunBodies<'a> {
    pub(super) fn new() -> Self {
        Self(Arc::new(Shared {
            bodies: Mutex::new(Vec::new()),
            waker: Mutex::new(None),
            driving: AtomicBool::new(false),
        }))
    }

    pub(super) fn issue(&self, body: RunAttemptBody<'a>) {
        self.0.bodies.lock_recover().push(body);
        let wake = self.0.waker.lock_recover().take();
        if let Some(wake) = wake {
            wake.wake();
        }
    }

    /// Drive issued bodies beside `future` until it returns. A call nested
    /// in another's future leaves driving to the outermost.
    pub async fn beside<F: Future>(&self, future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        std::future::poll_fn(|cx| {
            if self.0.driving.load(Ordering::SeqCst) {
                return future.as_mut().poll(cx);
            }
            self.0.driving.store(true, Ordering::SeqCst);
            let _drive = DriveGuard(&self.0.driving);
            if let std::task::Poll::Ready(output) = future.as_mut().poll(cx) {
                return std::task::Poll::Ready(output);
            }
            *self.0.waker.lock_recover() = Some(cx.waker().clone());
            let issued = std::mem::take(&mut *self.0.bodies.lock_recover());
            let mut surviving = Vec::with_capacity(issued.len());
            for mut body in issued {
                if body.as_mut().poll(cx).is_pending() {
                    surviving.push(body);
                }
            }
            let mut held = self.0.bodies.lock_recover();
            surviving.append(&mut held);
            *held = surviving;
            std::task::Poll::Pending
        })
        .await
    }
}
