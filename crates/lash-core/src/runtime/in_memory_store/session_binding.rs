use super::InMemorySessionStore;
use lash_sansio::sync::MutexExt;

impl InMemorySessionStore {
    /// The binding is the sole authority; metadata and head rows carry payload,
    /// not independent session-binding decisions. Call under the write transaction.
    pub(super) fn bind_or_verify(&self, session_id: &str) -> Result<(), crate::StoreError> {
        let mut bound = self.bound_session_id.lock_recover();
        match bound.as_ref() {
            Some(existing) if existing != session_id => {
                Err(crate::StoreError::SessionBindingMismatch {
                    bound_session_id: existing.clone(),
                    attempted_session_id: session_id.to_string(),
                })
            }
            Some(_) => Ok(()),
            None => {
                *bound = Some(session_id.to_string());
                Ok(())
            }
        }
    }

    pub(super) fn ensure_session_not_deleted(
        &self,
        session_id: &str,
    ) -> Result<(), crate::StoreError> {
        if self.deleted_session_ids.lock_recover().contains(session_id) {
            Err(crate::StoreError::SessionDeleted {
                session_id: session_id.to_string(),
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
