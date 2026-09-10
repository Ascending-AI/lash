use super::InMemorySessionStore;
use crate::SessionId;
use lash_sansio::sync::MutexExt;

impl InMemorySessionStore {
    /// The binding field is the sole authority once set; metadata and head
    /// rows carry payload, not independent session-binding decisions. A fresh
    /// bind adopts the durable identity already in the store — the head row
    /// first, then session metadata — and refuses a session that disagrees
    /// with it: the same commit-time authority the SQL backends read from
    /// their stored head row. Takes the binding and row locks itself; callers
    /// hold the coarse write transaction but no row locks.
    pub(super) fn bind_or_verify(&self, session_id: &SessionId) -> Result<(), crate::StoreError> {
        let mut bound = self.bound_session_id.lock_recover();
        let authority = bound
            .clone()
            .or_else(|| self.durable_head_identity())
            .or_else(|| self.durable_meta_identity());
        Self::refuse_binding_mismatch(authority, session_id)?;
        if bound.is_none() {
            *bound = Some(session_id.clone());
        }
        Ok(())
    }

    /// Admission-time half of [`Self::bind_or_verify`]: report the same
    /// mismatch without installing a fresh bind and without consulting the
    /// head row. Admission materializes metadata; head-row identity is
    /// commit-time authority, so the fresh bind lands at the first durable
    /// write, where [`Self::bind_or_verify`] adjudicates it.
    pub(super) fn verify_binding_for_admission(
        &self,
        session_id: &SessionId,
    ) -> Result<(), crate::StoreError> {
        let authority = self
            .bound_session_id
            .lock_recover()
            .clone()
            .or_else(|| self.durable_meta_identity());
        Self::refuse_binding_mismatch(authority, session_id)
    }

    fn durable_head_identity(&self) -> Option<SessionId> {
        self.session_head_meta
            .lock_recover()
            .as_ref()
            .map(|head| head.session_id.clone())
    }

    fn durable_meta_identity(&self) -> Option<SessionId> {
        self.session_meta
            .lock_recover()
            .as_ref()
            .map(|meta| meta.session_id.clone())
    }

    fn refuse_binding_mismatch(
        authority: Option<SessionId>,
        session_id: &SessionId,
    ) -> Result<(), crate::StoreError> {
        match authority {
            Some(existing) if &existing != session_id => {
                Err(crate::StoreError::SessionBindingMismatch {
                    bound_session_id: existing,
                    attempted_session_id: session_id.clone(),
                })
            }
            _ => Ok(()),
        }
    }

    pub(super) fn ensure_session_not_deleted(
        &self,
        session_id: &SessionId,
    ) -> Result<(), crate::StoreError> {
        if self.deleted_session_ids.lock_recover().contains(session_id) {
            Err(crate::StoreError::SessionDeleted {
                session_id: SessionId::from(session_id.to_string()),
            })
        } else {
            Ok(())
        }
    }

    pub(super) fn ensure_session_metadata_for_commit(
        &self,
        commit: &crate::RuntimeCommit,
    ) -> Result<(), crate::StoreError> {
        self.bind_or_verify(&commit.session_id)?;
        let mut session_meta = self.session_meta.lock_recover();
        session_meta.get_or_insert_with(|| crate::SessionMeta {
            session_id: commit.session_id.clone(),
            relation: crate::SessionRelation::Root,
            pending_observer_intents: Vec::new(),
        });
        let mut version = self.session_state_version.lock_recover();
        version.get_or_insert(crate::store::CURRENT_SESSION_STATE_VERSION);
        Ok(())
    }

    pub(super) fn replace_session_meta(
        &self,
        meta: crate::SessionMeta,
    ) -> Result<(), crate::StoreError> {
        if self
            .deleted_session_ids
            .lock_recover()
            .contains(&meta.session_id)
        {
            return Err(crate::StoreError::SessionDeleted {
                session_id: meta.session_id,
            });
        }
        self.bind_or_verify(&meta.session_id)?;
        let mut durable = self.session_meta.lock_recover();
        *durable = Some(meta);
        let mut version = self.session_state_version.lock_recover();
        version.get_or_insert(crate::store::CURRENT_SESSION_STATE_VERSION);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
