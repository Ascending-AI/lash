//! In-memory [`AttachmentManifest`](crate::AttachmentManifest) implementation
//! for [`InMemorySessionStore`], plus the crash-orphan reconciliation tests.
//!
//! Split from `runtime/in_memory_store.rs` to keep it under the file-size
//! budget. The impl is a trait impl on the parent module's type, so no public
//! path changes.

use super::InMemorySessionStore;
use lash_sansio::sync::MutexExt;

impl InMemorySessionStore {
    pub(super) fn commit_attachment_refs_in_memory(
        &self,
        session_id: &str,
        attachment_ids: &[crate::AttachmentId],
        committed_at_epoch_ms: u64,
    ) {
        let mut manifest = self.attachment_manifest.lock_recover();
        for attachment_id in attachment_ids {
            if let Some(entry) = manifest.get_mut(&(session_id.to_string(), attachment_id.clone()))
            {
                entry
                    .committed_at_epoch_ms
                    .get_or_insert(committed_at_epoch_ms);
            }
        }
    }

    pub(super) fn commit_turn_attachment_intents(
        &self,
        session_id: &str,
        completed: &crate::store::RuntimeTurnCommitStamp,
        committed_at_epoch_ms: u64,
    ) {
        for entry in self.attachment_manifest.lock_recover().values_mut() {
            let turn_id = completed.operation.turn_id();
            if entry.session_id == session_id
                && entry.owner_kind == Some(crate::AttachmentOwnerKind::Turn)
                && entry.owner_id.as_deref() == turn_id
                && entry.committed_at_epoch_ms.is_none()
            {
                entry.committed_at_epoch_ms = Some(committed_at_epoch_ms);
            }
        }
    }

    /// Insert or refresh one manifest intent row. The caller holds the store's
    /// write transaction.
    fn record_intent_in_transaction(
        &self,
        intent: crate::AttachmentIntent,
    ) -> Result<(), crate::store::StoreError> {
        self.ensure_session_not_deleted(&intent.session_id)?;
        let key = (intent.session_id.clone(), intent.attachment_id.clone());
        let mut manifest = self.attachment_manifest.lock_recover();
        match manifest.get_mut(&key) {
            Some(existing) => {
                // Re-recording refreshes the timestamp and durable owner as one
                // manifest mutation. GC later composes age with owner death.
                existing.canonical_uri = intent.canonical_uri;
                existing.intent_at_epoch_ms = intent.intent_at_epoch_ms;
                existing.owner_kind = intent.owner_kind;
                existing.owner_id = intent.owner_id;
            }
            None => {
                manifest.insert(
                    key,
                    crate::AttachmentManifestEntry {
                        attachment_id: intent.attachment_id,
                        session_id: intent.session_id,
                        canonical_uri: intent.canonical_uri,
                        intent_at_epoch_ms: intent.intent_at_epoch_ms,
                        committed_at_epoch_ms: None,
                        owner_kind: intent.owner_kind,
                        owner_id: intent.owner_id,
                    },
                );
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn attachment_manifest_entries(&self) -> Vec<crate::AttachmentManifestEntry> {
        self.attachment_manifest
            .lock_recover()
            .values()
            .cloned()
            .collect()
    }
}

impl crate::AttachmentManifest for InMemorySessionStore {
    fn record_intent(
        &self,
        intent: crate::AttachmentIntent,
    ) -> Result<(), crate::store::StoreError> {
        let _transaction = self.write_transaction.lock_recover();
        self.record_intent_in_transaction(intent)
    }

    /// The writer half of the GC fence. The factory-global condemnation state
    /// and the manifest row are mutated under the store's one transaction lock —
    /// the same boundary the sweeper's condemn CAS takes — so revoke-and-record
    /// is atomic against it.
    fn begin_attachment_write(
        &self,
        intent: crate::AttachmentIntent,
    ) -> Result<crate::AttachmentWriteFence, crate::store::StoreError> {
        let _transaction = self.write_transaction.lock_recover();
        {
            let mut condemnations = self.attachment_condemnations.lock_recover();
            match condemnations.get(&intent.attachment_id) {
                // The delete is already in flight: record nothing, so the bytes
                // this writer is about to put cannot be swallowed by it.
                Some(super::AttachmentCondemnationPhase::Deleting) => {
                    return Ok(crate::AttachmentWriteFence::ReclamationInFlight);
                }
                // Take the digest back before the sweeper can arm its delete.
                Some(super::AttachmentCondemnationPhase::Condemned) => {
                    condemnations.remove(&intent.attachment_id);
                }
                None => {}
            }
        }
        self.record_intent_in_transaction(intent)
            .map(|()| crate::AttachmentWriteFence::Granted)
    }

    fn commit_refs(
        &self,
        session_id: &str,
        attachment_ids: &[crate::AttachmentId],
    ) -> Result<(), crate::store::StoreError> {
        let committed_at_epoch_ms = self.clock.timestamp_ms();
        let _transaction = self.write_transaction.lock_recover();
        self.ensure_session_not_deleted(session_id)?;
        self.commit_attachment_refs_in_memory(session_id, attachment_ids, committed_at_epoch_ms);
        Ok(())
    }

    fn list_uncommitted(
        &self,
        older_than_epoch_ms: u64,
    ) -> Result<Vec<crate::AttachmentManifestEntry>, crate::store::StoreError> {
        let mut entries = self
            .attachment_manifest
            .lock_recover()
            .values()
            .filter(|entry| {
                entry.committed_at_epoch_ms.is_none()
                    && entry.intent_at_epoch_ms <= older_than_epoch_ms
            })
            .cloned()
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            left.intent_at_epoch_ms
                .cmp(&right.intent_at_epoch_ms)
                .then_with(|| left.session_id.cmp(&right.session_id))
                .then_with(|| left.attachment_id.cmp(&right.attachment_id))
        });
        Ok(entries)
    }

    fn forget_aged_uncommitted_intents(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<(), crate::store::StoreError> {
        let _transaction = self.write_transaction.lock_recover();
        let committed_turns = self
            .runtime_turn_commits
            .lock_recover()
            .iter()
            .map(|((session_id, turn_id), record)| {
                (session_id.clone(), turn_id.clone(), record.committed_at_ms)
            })
            .collect::<Vec<_>>();
        // Age, owner death, and removal happen under the same transaction/lock
        // boundary. Process owners are conservatively live in the inline store;
        // durable factories evaluate process-row existence in their database.
        self.attachment_manifest.lock_recover().retain(|_, entry| {
            let owner_is_dead = match (entry.owner_kind, entry.owner_id.as_deref()) {
                (None, None) => true,
                (Some(crate::AttachmentOwnerKind::Turn), Some(owner_id)) => committed_turns
                    .iter()
                    .any(|(session_id, turn_id, committed_at_ms)| {
                        session_id == &entry.session_id
                            && turn_id != owner_id
                            && *committed_at_ms > entry.intent_at_epoch_ms
                    }),
                (Some(crate::AttachmentOwnerKind::Process), Some(_)) => false,
                _ => false,
            };
            !(entry.committed_at_epoch_ms.is_none()
                && entry.intent_at_epoch_ms <= intent_grace_cutoff_epoch_ms
                && owner_is_dead)
        });
        Ok(())
    }

    fn forget(
        &self,
        session_id: &str,
        attachment_id: &crate::AttachmentId,
    ) -> Result<(), crate::store::StoreError> {
        self.attachment_manifest
            .lock_recover()
            .remove(&(session_id.to_string(), attachment_id.clone()));
        Ok(())
    }

    fn holds_ref(
        &self,
        session_id: &str,
        attachment_id: &crate::AttachmentId,
    ) -> Result<bool, crate::store::StoreError> {
        Ok(self
            .attachment_manifest
            .lock_recover()
            .contains_key(&(session_id.to_string(), attachment_id.clone())))
    }

    fn has_live_ref_for_id(
        &self,
        attachment_id: &crate::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, crate::store::StoreError> {
        let committed_turns = self
            .runtime_turn_commits
            .lock_recover()
            .iter()
            .map(|((session_id, turn_id), record)| {
                (session_id.clone(), turn_id.clone(), record.committed_at_ms)
            })
            .collect::<Vec<_>>();
        Ok(self
            .attachment_manifest
            .lock_recover()
            .values()
            .filter(|entry| &entry.attachment_id == attachment_id)
            .any(|entry| {
                if entry.committed_at_epoch_ms.is_some()
                    || entry.intent_at_epoch_ms > intent_grace_cutoff_epoch_ms
                {
                    return true;
                }
                match (entry.owner_kind, entry.owner_id.as_deref()) {
                    (None, None) => false,
                    (Some(crate::AttachmentOwnerKind::Turn), Some(owner_id)) => !committed_turns
                        .iter()
                        .any(|(session_id, turn_id, committed_at_ms)| {
                            session_id == &entry.session_id
                                && turn_id != owner_id
                                && *committed_at_ms > entry.intent_at_epoch_ms
                        }),
                    // The inline store has no durable process registry, so it
                    // cannot prove process death and must retain the root.
                    (Some(crate::AttachmentOwnerKind::Process), Some(_)) => true,
                    // Invalid owner pairs are conservatively retained.
                    _ => true,
                }
            }))
    }

    fn list_all_refs(&self) -> Result<Vec<crate::AttachmentId>, crate::store::StoreError> {
        let mut refs = self
            .attachment_manifest
            .lock_recover()
            .keys()
            .map(|(_, attachment_id)| attachment_id.clone())
            .collect::<Vec<_>>();
        refs.sort();
        refs.dedup();
        Ok(refs)
    }
}

#[cfg(test)]
mod attachment_reconciliation_tests {
    use super::InMemorySessionStore;
    use crate::AttachmentManifest;

    fn intent_at(session: &str, id: &str, at_ms: u64) -> crate::AttachmentIntent {
        crate::AttachmentIntent {
            attachment_id: crate::AttachmentId::parse(id).expect("valid attachment id"),
            session_id: session.to_string(),
            canonical_uri: format!("lash-attachment://blake3/{id}"),
            intent_at_epoch_ms: at_ms,
            owner_kind: None,
            owner_id: None,
        }
    }

    // Blocker 2 (in-memory): a `record_intent` that refreshes an aged intent's
    // timestamp past the reconciliation cutoff must survive reconciliation — the
    // age check and the removal happen under one lock, so the refreshed timestamp
    // is what the conditional delete sees. Without a refresh the aged intent is
    // reconciled away.
    #[test]
    fn reconciliation_spares_refreshed_intent_and_collects_stale() {
        let store = InMemorySessionStore::new();
        let cutoff = 200;

        // Refreshed case: recorded old, then re-recorded (refreshed) young.
        store
            .record_intent(intent_at("s", "kept", 100))
            .expect("record aged intent");
        store
            .record_intent(intent_at("s", "kept", 300))
            .expect("refresh intent past cutoff");

        // Stale case: recorded old and never refreshed.
        store
            .record_intent(intent_at("s", "collected", 100))
            .expect("record stale intent");

        store
            .forget_aged_uncommitted_intents(cutoff)
            .expect("reconcile");

        assert!(
            store
                .holds_ref(
                    "s",
                    &crate::AttachmentId::parse("kept").expect("valid attachment id")
                )
                .unwrap(),
            "a refreshed intent (timestamp past the cutoff) must survive reconciliation"
        );
        assert!(
            !store
                .holds_ref(
                    "s",
                    &crate::AttachmentId::parse("collected").expect("valid attachment id")
                )
                .unwrap(),
            "a stale aged intent must be reconciled away"
        );
    }

    // A committed ref is never reconciled, even if its intent timestamp predates
    // the cutoff; and `has_live_ref_for_id` reflects committed vs aged-orphan.
    #[test]
    fn has_live_ref_distinguishes_committed_from_aged_orphan() {
        let store = InMemorySessionStore::new();
        let cutoff = 200;
        let committed = crate::AttachmentId::parse("committed").expect("valid attachment id");
        let orphan = crate::AttachmentId::parse("orphan").expect("valid attachment id");

        store
            .record_intent(intent_at("s", "committed", 100))
            .unwrap();
        store
            .commit_refs("s", std::slice::from_ref(&committed))
            .unwrap();
        store.record_intent(intent_at("s", "orphan", 100)).unwrap();

        assert!(store.has_live_ref_for_id(&committed, cutoff).unwrap());
        assert!(
            !store.has_live_ref_for_id(&orphan, cutoff).unwrap(),
            "an aged uncommitted intent is not a live root"
        );

        store.forget_aged_uncommitted_intents(cutoff).unwrap();
        assert!(
            store.holds_ref("s", &committed).unwrap(),
            "a committed ref survives reconciliation regardless of its intent age"
        );
        assert!(!store.holds_ref("s", &orphan).unwrap());
    }
}
