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
        created_at: &str,
    ) -> Result<(), crate::StoreError> {
        let mut session_meta = self.session_meta.lock_recover();
        session_meta.get_or_insert_with(|| crate::SessionMeta {
            session_id: commit.session_id.clone(),
            session_name: commit.session_id.clone(),
            created_at: created_at.to_string(),
            model: commit.config.model.id.clone(),
            cwd: None,
            relation: crate::SessionRelation::Root,
        });
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
        Ok(())
    }
}
