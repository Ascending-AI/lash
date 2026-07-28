use super::InMemorySessionStore;

impl InMemorySessionStore {
    pub(super) fn ensure_session_incarnation_in_memory(
        &self,
        session_id: &str,
        policy: &crate::SessionPolicy,
    ) -> Result<crate::IncarnationId, crate::StoreError> {
        let _transaction = self
            .write_transaction
            .lock()
            .expect("lock in-memory write transaction");
        if self
            .deleted_sessions
            .lock()
            .expect("lock deleted sessions")
            .contains(session_id)
        {
            return Err(crate::StoreError::SessionDeleted {
                session_id: session_id.to_string(),
            });
        }
        let mut durable = self.session_meta.lock().expect("lock session meta");
        if let Some(meta) = durable.as_ref() {
            if meta.session_id != session_id {
                return Err(crate::StoreError::SessionBindingMismatch {
                    bound_session_id: meta.session_id.clone(),
                    attempted_session_id: session_id.to_string(),
                });
            }
            return Ok(meta.incarnation_id.clone());
        }
        let incarnation_id = crate::IncarnationId::mint_for_store();
        self.session_incarnations
            .lock()
            .expect("lock session incarnations")
            .entry(session_id.to_string())
            .or_insert_with(|| incarnation_id.clone());
        *durable = Some(crate::SessionMeta {
            session_id: session_id.to_string(),
            incarnation_id: incarnation_id.clone(),
            session_name: session_id.to_string(),
            created_at: self.clock.timestamp_rfc3339(),
            model: policy.model.id.clone(),
            cwd: None,
            relation: crate::SessionRelation::Root,
        });
        Ok(incarnation_id)
    }

    pub(super) fn save_session_meta_in_memory(
        &self,
        meta: crate::SessionMeta,
    ) -> Result<(), crate::StoreError> {
        let _transaction = self
            .write_transaction
            .lock()
            .expect("lock in-memory write transaction");
        if self
            .deleted_sessions
            .lock()
            .expect("lock deleted sessions")
            .contains(&meta.session_id)
        {
            return Err(crate::StoreError::SessionDeleted {
                session_id: meta.session_id,
            });
        }
        let mut incarnations = self
            .session_incarnations
            .lock()
            .expect("lock session incarnations");
        if let Some(expected) = incarnations.get(&meta.session_id)
            && expected != &meta.incarnation_id
        {
            return Err(crate::StoreError::SessionIncarnationMismatch {
                session_id: meta.session_id,
                expected_incarnation_id: expected.to_string(),
                actual_incarnation_id: meta.incarnation_id.to_string(),
            });
        }
        incarnations
            .entry(meta.session_id.clone())
            .or_insert_with(|| meta.incarnation_id.clone());
        drop(incarnations);
        self.replace_session_meta(meta)
    }

    pub(super) fn durable_incarnation_for_commit(
        &self,
        commit: &crate::RuntimeCommit,
    ) -> Result<crate::IncarnationId, crate::StoreError> {
        let commit_incarnation_id = commit
            .durable_incarnation_id("in-memory runtime commit")?
            .clone();
        let mut session_meta = self.session_meta.lock().expect("lock session meta");
        let meta = session_meta.get_or_insert_with(|| crate::SessionMeta {
            session_id: commit.session_id.clone(),
            incarnation_id: commit_incarnation_id.clone(),
            session_name: commit.session_id.clone(),
            created_at: self.clock.timestamp_rfc3339(),
            model: commit.config.model.id.clone(),
            cwd: None,
            relation: crate::SessionRelation::Root,
        });
        let mut incarnations = self
            .session_incarnations
            .lock()
            .expect("lock session incarnations");
        if let Some(expected) = incarnations.get(&commit.session_id)
            && expected != &commit_incarnation_id
        {
            return Err(crate::StoreError::SessionIncarnationMismatch {
                session_id: commit.session_id.clone(),
                expected_incarnation_id: expected.to_string(),
                actual_incarnation_id: commit_incarnation_id.to_string(),
            });
        }
        incarnations
            .entry(commit.session_id.clone())
            .or_insert_with(|| meta.incarnation_id.clone());
        Ok(meta.incarnation_id.clone())
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
