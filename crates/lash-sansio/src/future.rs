//! The erased future shape the drive's journaled boundaries share.
//!
//! `Pin<Box<dyn Future<Output = T> + Send + 'a>>` is the spelling every
//! journaled seam already uses: a recorded step's body handed to the engine,
//! a host ability call's reply through the run's command protocol, a
//! projected read's answer. The drive-determinism lint pins the tokens
//! wherever they appear spelled out, so each boundary names the shape through
//! here — a crate outside the scanned drive paths — rather than repeating
//! them (FIG-3672).

/// A `Send`-boxed future, lifetime-bound to its call site.
pub type SendBoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// Drive `future` to completion on the calling thread.
///
/// The VM's synchronous builtin surface — stringification, JSON projection,
/// list builtins — predates the async runtime and still answers through
/// `Future`-shaped helpers. This seam drives one such future: it must resolve
/// without an ambient executor (the projections it awaits are already in
/// flight or pure), and it never observes scheduling, a clock, or a store, so
/// the drive names the boundary rather than spelling a `block_on` primitive
/// inside scanned code (FIG-3672).
pub fn drive_sync<F: std::future::Future>(future: F) -> F::Output {
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};

    struct Parker(std::thread::Thread);

    impl Wake for Parker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }

    let waker = Waker::from(Arc::new(Parker(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::park(),
        }
    }
}
