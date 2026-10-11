//! Opt-in, thread-local whole-state phase receipts. Nothing is persisted.
//! Install on the thread that calls export/import (including a worker thread).
//! This unsupported diagnostic retains one raw sample per actual phase call;
//! its owner controls lifetime and retention. Phase durations do not overlap.
use std::cell::RefCell;
use std::time::Instant;

thread_local! {
    static CURRENT: RefCell<Option<Vec<Receipt>>> = const { RefCell::new(None) };
}

/// Input/output structure cardinalities, not decoded bytes or VM charge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub tasks: usize,
    pub active_calls: usize,
    pub objects: usize,
}
#[derive(Clone, Debug)]
pub struct Receipt {
    pub phase: &'static str,
    pub elapsed_nanos: u64,
    /// None when this phase refused before producing a complete structure.
    pub geometry: Option<Geometry>,
}
pub struct Collector(std::marker::PhantomData<std::rc::Rc<()>>);
impl Collector {
    /// Reject a nested collector rather than dropping its parent's samples.
    pub fn install() -> Option<Self> {
        CURRENT.with(|current| {
            let mut current = current.borrow_mut();
            if current.is_some() {
                return None;
            }
            *current = Some(Vec::new());
            Some(Self(std::marker::PhantomData))
        })
    }
    pub fn snapshot(&self) -> Vec<Receipt> {
        CURRENT.with(|current| current.borrow().as_ref().cloned().unwrap_or_default())
    }
}
impl Drop for Collector {
    fn drop(&mut self) {
        CURRENT.with(|current| *current.borrow_mut() = None);
    }
}
pub(crate) struct Phase {
    phase: &'static str,
    started: Instant,
    geometry: Option<Geometry>,
}
impl Phase {
    pub(crate) fn begin(phase: &'static str) -> Option<Self> {
        CURRENT.with(|current| {
            current.borrow().is_some().then(|| Self {
                phase,
                started: Instant::now(),
                geometry: None,
            })
        })
    }
    pub(crate) fn complete(&mut self, tasks: usize, active_calls: usize, objects: usize) {
        self.geometry = Some(Geometry {
            tasks,
            active_calls,
            objects,
        });
    }
}
impl Drop for Phase {
    fn drop(&mut self) {
        CURRENT.with(|current| {
            if let Some(receipts) = current.borrow_mut().as_mut() {
                receipts.push(Receipt {
                    phase: self.phase,
                    elapsed_nanos: self
                        .started
                        .elapsed()
                        .as_nanos()
                        .try_into()
                        .unwrap_or(u64::MAX),
                    geometry: self.geometry,
                });
            }
        });
    }
}
