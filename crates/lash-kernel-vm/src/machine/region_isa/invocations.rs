//! Numeric dispatch follows the actual affine state reservation. Its packet
//! stays private until one reached prefix consumes it at exit.
use super::*;
use lash_kernel_state::kernel_internals::logical::InvocationRange;

pub(super) struct Invocations {
    first: Option<u64>,
    capacity: u64,
    issued: u64,
    range: Option<InvocationRange>,
}
#[derive(Clone, Copy)]
pub(super) struct NextInvocation {
    pub(super) id: u64,
    issued: u64,
}
impl Invocations {
    pub(super) fn next_identity(
        &self,
        next: NextInvocation,
    ) -> Result<lash_kernel_state::kernel_internals::logical::Invocation, RegionError> {
        let expected = self.prepare()?;
        if next.id != expected.id || next.issued != expected.issued {
            return Err(invalid(Refused::Facts));
        }
        self.range
            .as_ref()
            .and_then(|range| range.invocation(self.issued))
            .filter(|identity| identity.id() == next.id)
            .ok_or_else(|| invalid(Refused::Facts))
    }
    pub(super) fn bind(range: InvocationRange) -> Result<Self, RegionError> {
        let first = range.invocation(0).map(|invocation| invocation.id());
        let capacity = range.len();
        if first.is_some() != (capacity != 0) || first == Some(0) {
            return Err(invalid(Refused::Facts));
        }
        Ok(Self {
            first,
            capacity,
            issued: 0,
            range: Some(range),
        })
    }
    pub(super) fn follows(&self, invocation: u64) -> bool {
        self.first.is_none_or(|first| first > invocation)
    }
    pub(super) fn prepare(&self) -> Result<NextInvocation, RegionError> {
        if self.issued >= self.capacity {
            return Err(invalid(Refused::Facts));
        }
        let id = self
            .first
            .and_then(|first| first.checked_add(self.issued))
            .ok_or_else(|| invalid(Refused::Facts))?;
        let issued = self
            .issued
            .checked_add(1)
            .ok_or_else(|| invalid(Refused::Facts))?;
        Ok(NextInvocation { id, issued })
    }
    pub(super) fn accept(&mut self, next: NextInvocation) {
        self.issued = next.issued;
    }
    pub(super) fn finish(self) -> Result<Option<InvocationRange>, RegionError> {
        self.range
            .map(|range| {
                range
                    .prefix(self.issued)
                    .ok_or_else(|| invalid(Refused::Facts))
            })
            .transpose()
    }
}
