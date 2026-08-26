use super::InMemorySessionStore;
use lash_sansio::sync::MutexExt;

impl InMemorySessionStore {
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
        let mut bound = self.bound_session_id.lock_recover();
        if let Some(existing) = bound.as_ref() {
            if existing != &commit.session_id {
                return Err(crate::StoreError::SessionBindingMismatch {
                    bound_session_id: existing.clone(),
                    attempted_session_id: commit.session_id.clone(),
                });
            }
        } else {
            *bound = Some(commit.session_id.clone());
        }
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
        let mut bound = self.bound_session_id.lock_recover();
        if let Some(existing) = bound.as_ref() {
            if existing != &meta.session_id {
                return Err(crate::StoreError::SessionBindingMismatch {
                    bound_session_id: existing.clone(),
                    attempted_session_id: meta.session_id.clone(),
                });
            }
        } else {
            *bound = Some(meta.session_id.clone());
        }
        let mut durable = self.session_meta.lock_recover();
        if let Some(existing) = durable.as_ref()
            && existing.session_id != meta.session_id
        {
            return Err(crate::StoreError::SessionBindingMismatch {
                bound_session_id: existing.session_id.clone(),
                attempted_session_id: meta.session_id,
            });
        }
        *durable = Some(meta);
        let mut version = self.session_state_version.lock_recover();
        version.get_or_insert(crate::store::CURRENT_SESSION_STATE_VERSION);
        Ok(())
    }
}
