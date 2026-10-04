//! A FIFO rendezvous within the one polled program/owner frame.
//! Only the program can enqueue commands. Wakeups resume polling; they never
//! select a result or change command order. X/D alone select settlements.
use lash_sansio::sync::MutexExt;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

struct Queue<T> {
    values: VecDeque<T>,
    wake: Option<Waker>,
    open: bool,
}
pub(super) struct Sender<T>(Arc<Mutex<Queue<T>>>);
pub(super) struct Receiver<T>(Arc<Mutex<Queue<T>>>);

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

pub(super) fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Mutex::new(Queue {
        values: VecDeque::new(),
        wake: None,
        open: true,
    }));
    (Sender(shared.clone()), Receiver(shared))
}

impl<T> Sender<T> {
    pub(super) fn send(&self, value: T) -> Result<(), ()> {
        let wake = {
            let mut queue = self.0.lock_recover();
            if !queue.open {
                return Err(());
            }
            queue.values.push_back(value);
            queue.wake.take()
        };
        if let Some(wake) = wake {
            wake.wake();
        }
        Ok(())
    }
}

impl<T> Receiver<T> {
    pub(super) async fn recv(&mut self) -> Option<T> {
        std::future::poll_fn(|cx| {
            let mut queue = self.0.lock_recover();
            if let Some(value) = queue.values.pop_front() {
                Poll::Ready(Some(value))
            } else if !queue.open {
                Poll::Ready(None)
            } else {
                queue.wake = Some(cx.waker().clone());
                Poll::Pending
            }
        })
        .await
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let mut queue = self.0.lock_recover();
        queue.open = false;
        queue.values.clear();
    }
}

struct Answer<T> {
    value: Option<T>,
    closed: bool,
    wake: Option<Waker>,
}
pub(super) struct Reply<T>(Arc<Mutex<Answer<T>>>);
pub(super) struct Response<T>(Arc<Mutex<Answer<T>>>);

pub(super) fn reply<T>() -> (Reply<T>, Response<T>) {
    let shared = Arc::new(Mutex::new(Answer {
        value: None,
        closed: false,
        wake: None,
    }));
    (Reply(shared.clone()), Response(shared))
}

impl<T> Reply<T> {
    pub(super) fn send(self, value: T) -> Result<(), ()> {
        let wake = {
            let mut answer = self.0.lock_recover();
            answer.value = Some(value);
            answer.wake.take()
        };
        if let Some(wake) = wake {
            wake.wake();
        }
        Ok(())
    }
}

impl<T> Drop for Reply<T> {
    fn drop(&mut self) {
        let wake = {
            let mut answer = self.0.lock_recover();
            answer.closed = true;
            answer.wake.take()
        };
        if let Some(wake) = wake {
            wake.wake();
        }
    }
}

impl<T> Future for Response<T> {
    type Output = Result<T, ()>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut answer = self.0.lock_recover();
        if let Some(value) = answer.value.take() {
            Poll::Ready(Ok(value))
        } else if answer.closed {
            Poll::Ready(Err(()))
        } else {
            answer.wake = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}
