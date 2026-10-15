//! Opt-in, thread-local whole-state phase receipts. Nothing is persisted.
//! Install on the thread that calls export/import (including a worker thread).
//! This unsupported diagnostic retains one raw sample per actual phase call;
//! its owner controls lifetime and retention. Phase durations do not overlap.
/// Enables a test/performance-only trace. This is no run observation level.
pub fn trace_statements(machine: &mut crate::KernelMachine) {
    machine.trace_statements();
}

/// Takes the diagnostic statement starts and empty-block entries.
pub fn take_statement_trace(machine: &mut crate::KernelMachine) -> Vec<lash_kernel_doc::Site> {
    machine.take_statement_trace()
}

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

/// Test/performance-only whole-state slowdown location.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    Checkpoint,
    Resume,
    Carry,
}
#[derive(Clone, Copy, Debug, Default)]
pub struct Work {
    pub vm_steps: u64,
    pub checkpoint_values: u64,
    pub resume_values: u64,
    pub carry_values: u64,
    pub probe_units: u64,
}
thread_local! {
    static WORK: std::cell::Cell<Option<Work>> = const { std::cell::Cell::new(None) };
    static PROBE: std::cell::Cell<Option<Probe>> = const { std::cell::Cell::new(None) };
}
/// Enables count-only diagnostics on the worker thread, without VM-loop locks
/// or allocations. Ordinary builds compile all calls out.
pub fn enable_work(probe: Option<Probe>) {
    WORK.set(Some(Work::default()));
    PROBE.set(probe);
}
pub fn take_work() -> Option<Work> {
    WORK.with(|work| work.replace(work.get().map(|_| Work::default())))
}
pub(crate) fn step() {
    WORK.with(|work| {
        if let Some(mut counts) = work.get() {
            counts.vm_steps += 1;
            work.set(Some(counts));
        }
    });
}
/// Counts the entries actually converted by the whole-state paths, using
/// collection cardinalities rather than another traversal.
pub(crate) fn object(phase: Probe, object: &lash_kernel_doc::Object) {
    use lash_kernel_doc::Object;
    let entries = 1 + match object {
        Object::List(items) | Object::Set(items) => items.len(),
        Object::Record(fields) => fields.len(),
        Object::Map(entries) => entries.len() * 2,
        Object::Variable(_) => 1,
        Object::Closure(closure) => closure.captures.len(),
    } as u64;
    WORK.with(|work| {
        if let Some(mut counts) = work.get() {
            match phase {
                Probe::Checkpoint => counts.checkpoint_values += entries,
                Probe::Resume => counts.resume_values += entries,
                Probe::Carry => counts.carry_values += entries,
            }
            work.set(Some(counts));
        }
    });
}
/// Deliberately visits every retained slot and payload byte. References are
/// visited as slots; the object table visits each body once, including cycles.
/// The hook is opt-in and changes no values, guest charge or admission.
pub(crate) fn probe<'a>(
    phase: Probe,
    objects: impl Iterator<Item = &'a lash_kernel_doc::Object>,
    roots: impl Iterator<Item = &'a lash_kernel_doc::Value>,
) {
    use lash_kernel_doc::{Object, Value};
    if PROBE.get() != Some(phase) {
        return;
    }
    fn value(item: &Value) -> u64 {
        std::hint::black_box(item);
        1 + match item {
            Value::Text(text) => text
                .bytes()
                .map(|byte| {
                    std::hint::black_box(byte);
                    1
                })
                .sum(),
            Value::Bytes(bytes) => bytes
                .as_slice()
                .iter()
                .map(|byte| {
                    std::hint::black_box(byte);
                    1
                })
                .sum(),
            Value::Tuple(items) => items.iter().map(value).sum(),
            Value::Error(error) => value(&error.data),
            _ => 0,
        }
    }
    let mut units = roots.map(value).sum::<u64>();
    for object in objects {
        std::hint::black_box(object);
        units += 1 + match object {
            Object::List(items) | Object::Set(items) => items.iter().map(value).sum::<u64>(),
            Object::Map(entries) => entries
                .iter()
                .map(|(key, item)| value(key) + value(item))
                .sum(),
            Object::Record(fields) => fields.iter().map(|(_, item)| value(item)).sum(),
            Object::Variable(item) => value(item),
            Object::Closure(closure) => closure.captures.len() as u64,
        };
    }
    WORK.with(|work| {
        if let Some(mut counts) = work.get() {
            counts.probe_units += units;
            work.set(Some(counts));
        }
    });
}
