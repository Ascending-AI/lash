use lash_sansio::sync::MutexExt;

use super::InMemorySessionStore;

impl InMemorySessionStore {
    pub(super) fn read_session_state_version_in_memory(&self) -> Result<u32, crate::StoreError> {
        crate::store::resolve_session_state_version(*self.session_state_version.lock_recover())
    }

    pub(super) fn admit_session_state_in_memory(
        &self,
        lease: &crate::SessionExecutionLeaseAuthority,
    ) -> Result<crate::store::SessionStateAdmission, crate::StoreError> {
        let now = self.clock.timestamp_ms();
        let _transaction = self.write_transaction.lock_recover();
        self.verify_session_execution_lease(&lease.session_id, lease, now)?;
        let version = self.read_session_state_version_in_memory()?;
        Ok(crate::store::SessionStateAdmission {
            session_id: lease.session_id.clone(),
            version,
            lease_fencing_token: lease.fencing_token,
        })
    }

    pub(super) fn stamp_session_state_version_and_corrupt_payload_in_memory(&self, version: u32) {
        *self.session_state_version.lock_recover() = Some(version);
        self.corrupt_session_payload_for_testing
            .store(true, std::sync::atomic::Ordering::Release);
    }

    pub(super) fn guard_session_payload_in_memory(&self) -> Result<(), crate::StoreError> {
        self.read_session_state_version_in_memory()?;
        if self
            .corrupt_session_payload_for_testing
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(crate::StoreError::StoredDataCorrupt {
                record_kind: "SessionHead",
                message: "injected payload is not decodable by the current codec".to_string(),
            });
        }
        Ok(())
    }

    pub(super) fn admit_and_bind_session_in_memory(
        &self,
        binding: &crate::SessionBinding,
    ) -> Result<crate::SessionAdmission, crate::StoreError> {
        #[cfg(test)]
        self.session_admission_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        binding.validate()?;
        let _transaction = self.write_transaction.lock_recover();
        if self
            .deleted_session_ids
            .lock_recover()
            .contains(&binding.session_id)
        {
            return Err(crate::StoreError::SessionDeleted {
                session_id: binding.session_id.clone(),
            });
        }
        let mut bound = self.bound_session_id.lock_recover();
        if let Some(existing) = bound.as_ref() {
            if existing != &binding.session_id {
                return Err(crate::StoreError::SessionBindingMismatch {
                    bound_session_id: existing.clone(),
                    attempted_session_id: binding.session_id.clone(),
                });
            }
        } else {
            *bound = Some(binding.session_id.clone());
        }
        let mut durable = self.session_meta.lock_recover();
        if let Some(meta) = durable.as_ref() {
            if meta.session_id != binding.session_id {
                return Err(crate::StoreError::SessionBindingMismatch {
                    bound_session_id: meta.session_id.clone(),
                    attempted_session_id: binding.session_id.clone(),
                });
            }
            return Ok(crate::SessionAdmission::Rebound);
        }
        *durable = Some(crate::SessionMeta {
            session_id: binding.session_id.clone(),
            relation: binding.relation.clone(),
            pending_observer_intents: Vec::new(),
        });
        *self.session_state_version.lock_recover() =
            Some(crate::store::CURRENT_SESSION_STATE_VERSION);
        Ok(crate::SessionAdmission::Created)
    }
}
