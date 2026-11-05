//! Arena ownership for a region, with one transferable registration rollback.
use super::versions::RegistrationRecord;
use super::*;
use crate::paged::logical::MemberEdit;
use crate::paged::tree::Access;
use lash_kernel_doc::heap::HeapFault;
use std::cell::RefMut;

pub(crate) enum ArenaAuthority<'a> {
    Borrowed(&'a mut Arena),
    Shared(RefMut<'a, Arena>),
}
impl ArenaAuthority<'_> {
    pub(crate) fn read(&self) -> &Arena {
        match self {
            Self::Borrowed(arena) => arena,
            Self::Shared(arena) => arena,
        }
    }
    pub(crate) fn write(&mut self) -> &mut Arena {
        match self {
            Self::Borrowed(arena) => arena,
            Self::Shared(arena) => arena,
        }
    }
}
pub(crate) struct RegionBuild<T> {
    pub bodies: Vec<Body>,
    pub values: Vec<Value>,
    pub ordinals: Vec<(ArenaHandle, u64)>,
    pub metadata: Vec<RegionBodyMetadata>,
    pub data: T,
}
impl<'a> ArenaAuthority<'a> {
    pub(crate) fn stage_region<T>(
        mut self,
        pages: &Access<'_>,
        count: usize,
        allocations: u64,
        build: impl FnOnce(&[ArenaHandle]) -> Result<RegionBuild<T>, HeapFault>,
    ) -> Result<(RegionRegistration<'a>, PreparedBatch, PreparedGraphProof, T), (Self, HeapFault)>
    {
        let staged: Result<_, HeapFault> = (|| {
            let mut registration = self.write().registration().map_err(fault)?;
            let handles = registration.reserve_region_headers(count).map_err(fault)?;
            let RegionBuild {
                bodies,
                values,
                ordinals,
                metadata,
                data,
            } = build(&handles)?;
            registration.fill_region_headers(bodies).map_err(fault)?;
            let mut installation = Arena::prepare_registered(registration).map_err(fault)?;
            installation.region_metadata(metadata)?;
            let proof = installation.prove_region_values(
                pages,
                &values,
                usize::MAX,
                false,
                Some((&ordinals, allocations)),
            )?;
            let (registration, batch) = installation.into_parts();
            Ok((registration.detach(), batch, proof, data))
        })();
        match staged {
            Ok((record, batch, proof, data)) => Ok((
                RegionRegistration {
                    authority: self,
                    record: Some(record),
                },
                batch,
                proof,
                data,
            )),
            Err(error) => Err((self, error)),
        }
    }
}
/// Keeps the arena port and its rollback obligation together without a
/// reference into an owned RefMut guard. Abandonment uses Registration's
/// original rollback, including every identity cursor and reserved slot.
pub(crate) struct RegionRegistration<'a> {
    authority: ArenaAuthority<'a>,
    record: Option<RegistrationRecord>,
}
impl RegionRegistration<'_> {
    pub(crate) fn prepare_local_members(
        &mut self,
        batch: PreparedBatch,
        edits: &[MemberEdit],
        tables: Vec<(ArenaHandle, Vec<RegionTablePatch>, u64)>,
        lists: Vec<(ArenaHandle, Vec<Value>, Vec<u64>)>,
    ) -> Result<(PreparedBatch, Vec<RegionMemberBody>), HeapFault> {
        let record = self
            .record
            .take()
            .unwrap_or_else(|| unreachable!("region rollback owner"));
        let registration = record.attach(self.authority.write());
        let mut installation = PreparedInstallation::from_region_parts(registration, batch);
        let mut members = installation.prepare_region_members(edits)?;
        for (handle, values, cards) in lists {
            members.push(installation.prepare_lease_list(handle, values, cards)?);
        }
        for (handle, edits, next) in tables {
            members.push(installation.prepare_lease_table(handle, edits, next)?);
        }
        let (registration, batch) = installation.into_parts();
        self.record = Some(registration.detach());
        Ok((batch, members))
    }
    pub(crate) fn publish_revisions(&mut self, revisions: Vec<RegionBodyMetadata>) {
        for revision in revisions {
            let arena = self.authority.write();
            let facts = &mut arena.facts[revision.handle.index() as usize];
            Arena::mark_resident(facts, &mut arena.dirty, revision.handle, None);
            facts.content_revision = revision.content;
            facts.revision = revision.structural;
        }
    }
    pub(crate) fn arena(&mut self) -> &mut Arena {
        self.authority.write()
    }
    pub(crate) fn read(&self) -> &Arena {
        self.authority.read()
    }
    /// Close the registration while retaining arena exclusion through the VM
    /// swaps. No allocation, validation, or external callback remains.
    pub(crate) fn commit(&mut self) {
        let record = self
            .record
            .take()
            .unwrap_or_else(|| unreachable!("region registration commits once"));
        record.attach(self.authority.write()).commit();
    }
}
impl Drop for RegionRegistration<'_> {
    fn drop(&mut self) {
        if let Some(record) = self.record.take() {
            drop(record.attach(self.authority.write()));
        }
    }
}
fn fault(error: NativeError) -> HeapFault {
    match error {
        NativeError::Host(error) => error,
        error => HeapFault::Malformed(error.to_string()),
    }
}
