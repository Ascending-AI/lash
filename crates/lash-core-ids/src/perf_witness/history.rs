//! Exclusive, poll-scoped CPU and allocation attribution for one dev probe.
//! No task-local scope crosses Pending: another task on the same worker must
//! never inherit its component. This entire module is behind `perf-witness`.
use std::cell::Cell;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::task::{Context, Poll};

use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Component {
    RequestBuild,
    ProviderEncodeSend,
    DurableCommitSql,
    OwnerRetention,
    PluginTools,
    TraceObserver,
    Other,
    Measurement,
}
impl Component {
    pub const ALL: [Self; 7] = [
        Self::RequestBuild,
        Self::ProviderEncodeSend,
        Self::DurableCommitSql,
        Self::OwnerRetention,
        Self::PluginTools,
        Self::TraceObserver,
        Self::Other,
    ];
    pub fn holds_history(self) -> bool {
        matches!(self, Self::RequestBuild | Self::ProviderEncodeSend)
    }
}
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Cost {
    pub cpu_ns: u64,
    pub allocations: u64,
    pub allocated_bytes: u64,
    pub statements: u64,
    pub vm_steps: u64,
    pub rows: u64,
}
impl Cost {
    pub fn since(self, old: Self) -> Self {
        Self {
            cpu_ns: self.cpu_ns.saturating_sub(old.cpu_ns),
            allocations: self.allocations.saturating_sub(old.allocations),
            allocated_bytes: self.allocated_bytes.saturating_sub(old.allocated_bytes),
            statements: self.statements.saturating_sub(old.statements),
            vm_steps: self.vm_steps.saturating_sub(old.vm_steps),
            rows: self.rows.saturating_sub(old.rows),
        }
    }
}
struct Counters {
    cpu: AtomicU64,
    allocations: AtomicU64,
    bytes: AtomicU64,
}
static COUNTERS: [Counters; 8] = [const {
    Counters {
        cpu: AtomicU64::new(0),
        allocations: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
    }
}; 8];
static INSTALLED: AtomicBool = AtomicBool::new(false);
static ENABLED: AtomicBool = AtomicBool::new(false);
static SQL: Mutex<BTreeMap<String, Cost>> = Mutex::new(BTreeMap::new());
static HELD: Mutex<(usize, usize, String)> = Mutex::new((0, 0, String::new()));
thread_local! {
    static CURRENT: Cell<Component> = const { Cell::new(Component::Other) };
    static START: Cell<u64> = const { Cell::new(0) };
}
fn clock(id: rustix::time::ClockId) -> u64 {
    let t = rustix::time::clock_gettime(id);
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
}
pub fn enabled() -> bool {
    ENABLED.load(Relaxed)
}
pub fn process_cpu_ns() -> u64 {
    clock(rustix::time::ClockId::ProcessCPUTime)
}
fn switch(next: Component) -> Component {
    let now = clock(rustix::time::ClockId::ThreadCPUTime);
    let old = CURRENT.with(|c| c.replace(next));
    let start = START.with(|s| s.replace(now));
    if start != 0 && old != Component::Other {
        COUNTERS[old as usize].cpu.fetch_add(now - start, Relaxed);
    }
    old
}
/// A scope cannot move to another thread. Futures enter it only during poll.
pub struct Scope(Option<Component>, std::marker::PhantomData<std::rc::Rc<()>>);
impl Scope {
    pub fn enter(component: Component) -> Self {
        Self(
            ENABLED.load(Relaxed).then(|| switch(component)),
            std::marker::PhantomData,
        )
    }
}
impl Drop for Scope {
    fn drop(&mut self) {
        if let Some(old) = self.0 {
            switch(old);
        }
    }
}
/// The original future is stored once, without an extra async state machine.
/// In particular, observing a large cold-load future must not double its size.
pub struct Measured<F> {
    component: Component,
    future: F,
}
impl<F: Future> Future for Measured<F> {
    type Output = F::Output;

    #[expect(
        unsafe_code,
        reason = "structural pin projection to the future field; no Drop impl moves it"
    )]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let component = self.as_ref().get_ref().component;
        let _scope = Scope::enter(component);
        // SAFETY: pinning Measured pins its future field. This projection
        // never moves that field, and neither does Measured's implicit drop.
        unsafe { self.map_unchecked_mut(|this| &mut this.future) }.poll(cx)
    }
}
pub fn measure<F: Future>(component: Component, future: F) -> Measured<F> {
    Measured { component, future }
}
/// Called only by the developer binary's allocator; never allocates or locks.
pub fn allocation(count: u64, bytes: usize) {
    if ENABLED.load(Relaxed) {
        let component = CURRENT.try_with(Cell::get).unwrap_or(Component::Other);
        let counters = &COUNTERS[component as usize];
        counters.allocations.fetch_add(count, Relaxed);
        counters.bytes.fetch_add(bytes as u64, Relaxed);
    }
}
/// One collector owns one sequential session. Reporting happens while disabled.
pub struct Collector;
impl Collector {
    pub fn install() -> Result<Self, super::AlreadyInstalled> {
        if INSTALLED.swap(true, Relaxed) {
            return Err(super::AlreadyInstalled);
        }
        ENABLED.store(true, Relaxed);
        Ok(Self)
    }
    pub fn pause(&self) {
        ENABLED.store(false, Relaxed);
    }
    pub fn resume(&self) {
        ENABLED.store(true, Relaxed);
    }
    pub fn overhead_cpu_ns(&self) -> u64 {
        COUNTERS[Component::Measurement as usize].cpu.load(Relaxed)
    }
    pub fn snapshot(&self) -> BTreeMap<Component, Cost> {
        Component::ALL
            .into_iter()
            .map(|component| {
                let c = &COUNTERS[component as usize];
                (
                    component,
                    Cost {
                        cpu_ns: c.cpu.load(Relaxed),
                        allocations: c.allocations.load(Relaxed),
                        allocated_bytes: c.bytes.load(Relaxed),
                        ..Cost::default()
                    },
                )
            })
            .collect()
    }
    pub fn take_sql(&self) -> BTreeMap<String, Cost> {
        std::mem::take(
            &mut *SQL
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
    pub fn held(&self) -> (usize, usize, String) {
        HELD.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}
impl Drop for Collector {
    fn drop(&mut self) {
        self.pause();
        INSTALLED.store(false, Relaxed);
    }
}
pub fn held(bytes: usize, messages: usize, frame: String) {
    if ENABLED.load(Relaxed) {
        *HELD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = (bytes, messages, frame);
    }
}
/// Exact parameterized SQL is the statement family: no expanded bind values.
/// The callback supplies per-execution deltas, including cached statements.
pub fn sql(sql: &str, statements: u64, vm_steps: u64, rows: u64) {
    if ENABLED.load(Relaxed) {
        let mut families = SQL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cost = families.entry(sql.to_owned()).or_default();
        cost.statements += statements;
        cost.vm_steps += vm_steps;
        cost.rows += rows;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::poll_fn;
    #[test]
    fn pending_and_dropped_futures_return_the_threads_attribution() {
        let collector = Collector::install().unwrap();
        collector.pause();
        assert!(
            Collector::install().is_err(),
            "pausing keeps the collector claim"
        );
        collector.resume();
        let future = measure(
            Component::OwnerRetention,
            poll_fn(|_| {
                assert_eq!(CURRENT.with(Cell::get), Component::OwnerRetention);
                std::task::Poll::<()>::Pending
            }),
        );
        let mut future = Box::pin(future);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(future.as_mut().poll(&mut cx).is_pending());
        assert_eq!(CURRENT.with(Cell::get), Component::Other);
        // Cancellation releases the same obligation; another task must see Other.
        drop(future);
        assert_eq!(CURRENT.with(Cell::get), Component::Other);
        {
            let _outer = Scope::enter(Component::PluginTools);
            {
                let _inner = Scope::enter(Component::TraceObserver);
            }
            assert_eq!(CURRENT.with(Cell::get), Component::PluginTools);
        }
        assert_eq!(CURRENT.with(Cell::get), Component::Other);
    }
}
