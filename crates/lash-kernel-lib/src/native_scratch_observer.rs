//! Test-only observations at actual native buffer and table lifetimes.

thread_local! {
    static SCRATCH: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) };
}
pub(crate) fn reset() {
    SCRATCH.with(|state| state.set((0, 0)));
}
pub(crate) fn peak() -> u64 {
    SCRATCH.with(|state| state.get().1)
}
pub(crate) fn observe(bytes: u64) {
    let mut lease = hold(0);
    lease.resize(bytes);
}
pub(crate) struct Lease(u64);
pub(crate) fn hold(bytes: u64) -> Lease {
    SCRATCH.with(|state| {
        let (live, peak) = state.get();
        state.set((live + bytes, peak.max(live + bytes)));
    });
    Lease(bytes)
}
impl Lease {
    pub(crate) fn resize(&mut self, bytes: u64) {
        SCRATCH.with(|state| {
            let (live, peak) = state.get();
            let live = live - self.0 + bytes;
            state.set((live, peak.max(live)));
        });
        self.0 = bytes;
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        SCRATCH.with(|state| {
            let (live, peak) = state.get();
            state.set((live - self.0, peak));
        });
    }
}
