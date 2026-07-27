use super::InMemorySessionStore;

impl InMemorySessionStore {
    pub(super) fn durable_incarnation_for_commit(
        &self,
        commit: &crate::RuntimeCommit,
    ) -> crate::IncarnationId {
        let mut session_meta = self.session_meta.lock().expect("lock session meta");
        let meta = session_meta.get_or_insert_with(|| crate::SessionMeta {
            session_id: commit.session_id.clone(),
            incarnation_id: commit.incarnation_id.clone(),
            session_name: commit.session_id.clone(),
            created_at: self.clock.timestamp_rfc3339(),
            model: commit.config.model.id.clone(),
            cwd: None,
            relation: crate::SessionRelation::Root,
        });
        meta.incarnation_id.clone()
    }

    pub(super) fn replace_session_meta(
        &self,
        meta: crate::SessionMeta,
    ) -> Result<(), crate::StoreError> {
        let mut durable = self.session_meta.lock().expect("lock session meta");
        if let Some(existing) = durable.as_ref()
            && existing.incarnation_id != meta.incarnation_id
        {
            return Err(crate::StoreError::SessionIncarnationMismatch {
                session_id: meta.session_id,
                expected_incarnation_id: existing.incarnation_id.to_string(),
                actual_incarnation_id: meta.incarnation_id.to_string(),
            });
        }
        *durable = Some(meta);
        Ok(())
    }
}
