//! In-memory [`AttachmentManifest`](crate::AttachmentManifest) implementation
//! for [`InMemorySessionStore`], plus the crash-orphan reconciliation tests.
//!
//! Split from `runtime/in_memory_store.rs` to keep it under the file-size
//! budget. The impl is a trait impl on the parent module's type, so no public
//! path changes.

use super::InMemorySessionStore;
use crate::SessionId;
use lash_sansio::sync::MutexExt;

impl InMemorySessionStore {
    /// The caller holds the factory write transaction. Validate the whole batch
    /// for upload evidence before mutating anything, so a batch containing one
    /// unknown digest adopts none of it.
    pub(super) fn commit_attachment_refs_in_memory(
        &self,
        session_id: &SessionId,
        attachment_ids: &[crate::AttachmentId],
        committed_at_epoch_ms: u64,
    ) -> Result<(), crate::StoreError> {
        let mut condemnations = self.attachment_condemnations.lock_recover();
        let mut manifest = self.attachment_manifest.lock_recover();
        // Validation pass: a digest is adoptable iff some row anywhere records a
        // completed upload and no physical delete is in flight for it.
        let mut evidence = std::collections::BTreeMap::new();
        for id in attachment_ids {
            if matches!(
                condemnations.get(id),
                Some(super::AttachmentCondemnationPhase::Deleting)
            ) {
                return Err(crate::StoreError::UnknownAttachment { digest: id.clone() });
            }
            let written_at = manifest
                .values()
                .filter(|entry| &entry.attachment_id == id)
                .filter_map(|entry| entry.written_at_epoch_ms)
                .min();
            let Some(written_at) = written_at else {
                return Err(crate::StoreError::UnknownAttachment { digest: id.clone() });
            };
            evidence.insert(id.clone(), written_at);
        }
        for id in attachment_ids {
            let written_at_epoch_ms = evidence.get(id).copied();
            // The fresh committed root supersedes an unarmed, unclaimed
            // condemnation. A restoring writer's claim is left alone; its own
            // completion or abort settles it.
            if matches!(
                condemnations.get(id),
                Some(super::AttachmentCondemnationPhase::Condemned { write_claim: None })
            ) {
                condemnations.remove(id);
            }
            let entry = manifest
                .entry((session_id.clone(), id.clone()))
                .or_insert_with(|| crate::AttachmentManifestEntry {
                    attachment_id: id.clone(),
                    session_id: SessionId::from(session_id.to_string()),
                    canonical_uri: format!("lash-attachment://blake3/{id}"),
                    intent_at_epoch_ms: committed_at_epoch_ms,
                    written_at_epoch_ms: None,
                    committed_at_epoch_ms: None,
                    owner: None,
                });
            // Copy the evidence onto the adopter's row so it outlives the
            // uploader's intent being forgotten.
            if entry.written_at_epoch_ms.is_none() {
                entry.written_at_epoch_ms = written_at_epoch_ms;
            }
            entry
                .committed_at_epoch_ms
                .get_or_insert(committed_at_epoch_ms);
        }
        Ok(())
    }

    pub(super) fn commit_turn_attachment_intents(
        &self,
        session_id: &SessionId,
        completed: &crate::store::RuntimeTurnCommitStamp,
        committed_at_epoch_ms: u64,
    ) {
        for entry in self.attachment_manifest.lock_recover().values_mut() {
            let turn_id = completed.operation.turn_id();
            if entry.session_id == session_id
                && matches!(
                    &entry.owner,
                    Some(crate::AttachmentOwner::Turn { id })
                        if Some(id.as_str()) == turn_id.map(crate::TurnId::as_str)
                )
                && entry.committed_at_epoch_ms.is_none()
            {
                entry.committed_at_epoch_ms = Some(committed_at_epoch_ms);
            }
        }
    }

    /// Insert or refresh one manifest intent row under a fresh attempt
    /// identity. The caller holds the store's write transaction.
    ///
    /// The new attempt has proven nothing, so it carries no upload stamp; any
    /// stamp or commitment already on the row is evidence a previous attempt
    /// earned and is preserved.
    fn record_intent_in_transaction(
        &self,
        intent: crate::AttachmentIntent,
        write_id: crate::AttachmentWriteToken,
    ) -> Result<(), crate::store::StoreError> {
        self.ensure_session_not_deleted(&intent.session_id)?;
        let key = (intent.session_id.clone(), intent.attachment_id.clone());
        let mut manifest = self.attachment_manifest.lock_recover();
        match manifest.get_mut(&key) {
            Some(existing) => {
                // Re-recording refreshes the timestamp, durable owner and
                // attempt identity as one manifest mutation. GC later composes
                // age with owner death.
                existing.canonical_uri = intent.canonical_uri;
                existing.intent_at_epoch_ms = intent.intent_at_epoch_ms;
                existing.owner = intent.owner;
                self.attachment_write_ids
                    .lock_recover()
                    .insert(key, write_id);
            }
            None => {
                manifest.insert(
                    key.clone(),
                    crate::AttachmentManifestEntry {
                        attachment_id: intent.attachment_id,
                        session_id: intent.session_id,
                        canonical_uri: intent.canonical_uri,
                        intent_at_epoch_ms: intent.intent_at_epoch_ms,
                        written_at_epoch_ms: None,
                        committed_at_epoch_ms: None,
                        owner: intent.owner,
                    },
                );
                self.attachment_write_ids
                    .lock_recover()
                    .insert(key, write_id);
            }
        }
        Ok(())
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn attachment_manifest_entries(&self) -> Vec<crate::AttachmentManifestEntry> {
        self.attachment_manifest
            .lock_recover()
            .values()
            .cloned()
            .collect()
    }
}

impl crate::AttachmentManifest for InMemorySessionStore {
    /// The writer half of the GC fence. The factory-global condemnation state
    /// and the manifest row are mutated under the store's one transaction lock —
    /// the same boundary the sweeper's condemn CAS takes — so claim-and-record
    /// is atomic against it.
    fn begin_attachment_write(
        &self,
        intent: crate::AttachmentIntent,
    ) -> Result<crate::AttachmentWriteFence, crate::store::StoreError> {
        let _transaction = self.write_transaction.lock_recover();
        self.ensure_session_not_deleted(&intent.session_id)?;
        let write_id = crate::AttachmentWriteToken::new();
        {
            let mut condemnations = self.attachment_condemnations.lock_recover();
            match condemnations.get(&intent.attachment_id).cloned() {
                // The delete is already in flight: record nothing, so the bytes
                // this writer is about to put cannot be swallowed by it.
                Some(super::AttachmentCondemnationPhase::Deleting)
                | Some(super::AttachmentCondemnationPhase::Condemned {
                    write_claim: Some(_),
                }) => {
                    return Ok(crate::AttachmentWriteFence::ReclamationInFlight);
                }
                // Own the condemnation until the backend put settles. Keeping
                // the phase present means no sweep can arm the delete.
                Some(super::AttachmentCondemnationPhase::Condemned { write_claim: None }) => {
                    condemnations.insert(
                        intent.attachment_id.clone(),
                        super::AttachmentCondemnationPhase::Condemned {
                            write_claim: Some(super::AttachmentWriteClaim {
                                write_id,
                                session_id: intent.session_id.clone(),
                            }),
                        },
                    );
                }
                None => {}
            }
        }
        self.record_intent_in_transaction(intent, write_id)?;
        Ok(crate::AttachmentWriteFence::Granted(
            crate::AttachmentWritePermit::new(write_id),
        ))
    }

    fn complete_attachment_write(
        &self,
        intent: &crate::AttachmentIntent,
        permit: crate::AttachmentWritePermit,
    ) -> Result<(), crate::store::StoreError> {
        let write_id = permit.write_id();
        let _transaction = self.write_transaction.lock_recover();
        let key = (intent.session_id.clone(), intent.attachment_id.clone());
        if self.attachment_write_ids.lock_recover().get(&key) != Some(&write_id) {
            return Err(crate::StoreError::StaleWritePermit {
                digest: intent.attachment_id.clone(),
            });
        }
        let written_at_epoch_ms = self.clock.timestamp_ms();
        {
            let mut manifest = self.attachment_manifest.lock_recover();
            let Some(entry) = manifest.get_mut(&key) else {
                return Err(crate::StoreError::StaleWritePermit {
                    digest: intent.attachment_id.clone(),
                });
            };
            // The first proven upload is the evidence; a repeat put keeps it.
            entry.written_at_epoch_ms.get_or_insert(written_at_epoch_ms);
        }
        let mut condemnations = self.attachment_condemnations.lock_recover();
        if matches!(
            condemnations.get(&intent.attachment_id),
            Some(super::AttachmentCondemnationPhase::Condemned {
                write_claim: Some(claim),
            }) if claim.write_id == write_id
        ) {
            condemnations.remove(&intent.attachment_id);
        }
        Ok(())
    }

    fn abort_attachment_write(
        &self,
        intent: &crate::AttachmentIntent,
        permit: crate::AttachmentWritePermit,
    ) -> Result<(), crate::store::StoreError> {
        let write_id = permit.write_id();
        let _transaction = self.write_transaction.lock_recover();
        let key = (intent.session_id.clone(), intent.attachment_id.clone());
        // A superseded attempt owns nothing: it must not delete a newer
        // attempt's row, nor release a claim it no longer holds.
        if self.attachment_write_ids.lock_recover().get(&key) != Some(&write_id) {
            return Ok(());
        }
        let mut condemnations = self.attachment_condemnations.lock_recover();
        let mut manifest = self.attachment_manifest.lock_recover();
        // Only this attempt's unstamped, uncommitted row goes.
        let committed = match manifest.get(&key) {
            Some(entry)
                if entry.written_at_epoch_ms.is_none() && entry.committed_at_epoch_ms.is_none() =>
            {
                manifest.remove(&key);
                self.attachment_write_ids.lock_recover().remove(&key);
                false
            }
            Some(entry) => entry.committed_at_epoch_ms.is_some(),
            None => false,
        };
        if matches!(
            condemnations.get(&intent.attachment_id),
            Some(super::AttachmentCondemnationPhase::Condemned {
                write_claim: Some(claim),
            }) if claim.write_id == write_id
        ) {
            if committed {
                // The same intent became a committed root while the claim was
                // held: that newer root supersedes the unarmed condemnation.
                condemnations.remove(&intent.attachment_id);
            } else {
                condemnations.insert(
                    intent.attachment_id.clone(),
                    super::AttachmentCondemnationPhase::Condemned { write_claim: None },
                );
            }
        }
        Ok(())
    }

    fn commit_refs(
        &self,
        session_id: &SessionId,
        attachment_ids: &[crate::AttachmentId],
    ) -> Result<(), crate::store::StoreError> {
        let committed_at_epoch_ms = self.clock.timestamp_ms();
        let _transaction = self.write_transaction.lock_recover();
        self.ensure_session_not_deleted(session_id)?;
        self.commit_attachment_refs_in_memory(session_id, attachment_ids, committed_at_epoch_ms)
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
        let deleted = self.deleted_session_ids.lock_recover().clone();
        let committed_turns = self
            .runtime_turn_commits
            .lock_recover()
            .iter()
            .map(|((session_id, turn_id), record)| {
                (session_id.clone(), turn_id.clone(), record.committed_at_ms)
            })
            .collect::<Vec<_>>();
        // Age, owner death, and removal happen under the same transaction/lock
        // boundary. Process owners are conservatively live in the in-memory store;
        // durable factories evaluate process-row existence in their database.
        self.attachment_manifest.lock_recover().retain(|_, entry| {
            let owner_is_dead = deleted.contains(&entry.session_id)
                || match &entry.owner {
                    None => true,
                    Some(crate::AttachmentOwner::Turn { id: owner_id }) => committed_turns
                        .iter()
                        .any(|(session_id, turn_id, committed_at_ms)| {
                            session_id == entry.session_id
                                && turn_id != owner_id
                                && *committed_at_ms > entry.intent_at_epoch_ms
                        }),
                    Some(crate::AttachmentOwner::Process { .. }) => false,
                };
            !(entry.committed_at_epoch_ms.is_none()
                && entry.intent_at_epoch_ms <= intent_grace_cutoff_epoch_ms
                && owner_is_dead)
        });
        Ok(())
    }

    fn forget(
        &self,
        session_id: &SessionId,
        attachment_id: &crate::AttachmentId,
    ) -> Result<(), crate::store::StoreError> {
        let _transaction = self.write_transaction.lock_recover();
        let owners = self.global_node_owners.lock_recover();
        let tombstoned = self.tombstoned_node_ids.lock_recover();
        let retained = owners
            .iter()
            .any(|(node, owner)| owner == session_id && !tombstoned.contains(node));
        self.attachment_manifest
            .lock_recover()
            .retain(|(owner, id), entry| {
                owner != session_id
                    || id != attachment_id
                    || (retained && entry.committed_at_epoch_ms.is_some())
            });
        Ok(())
    }

    fn has_live_ref_for_id(
        &self,
        attachment_id: &crate::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, crate::store::StoreError> {
        let deleted = self.deleted_session_ids.lock_recover().clone();
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
                if deleted.contains(&entry.session_id) {
                    return false;
                }
                match &entry.owner {
                    None => false,
                    Some(crate::AttachmentOwner::Turn { id: owner_id }) => !committed_turns
                        .iter()
                        .any(|(session_id, turn_id, committed_at_ms)| {
                            session_id == entry.session_id
                                && turn_id != owner_id
                                && *committed_at_ms > entry.intent_at_epoch_ms
                        }),
                    // The in-memory store has no durable process registry, so it
                    // cannot prove process death and must retain the root.
                    Some(crate::AttachmentOwner::Process { .. }) => true,
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
    use crate::SessionId;

    fn intent_at(session: &str, id: &str, at_ms: u64) -> crate::AttachmentIntent {
        crate::AttachmentIntent {
            attachment_id: crate::AttachmentId::parse(id).expect("valid attachment id"),
            session_id: SessionId::from(session.to_string()),
            canonical_uri: format!("lash-attachment://blake3/{id}"),
            intent_at_epoch_ms: at_ms,
            owner: None,
        }
    }

    /// One completed write: acquire the fence, then stamp upload evidence —
    /// the only way a manifest row is created.
    fn put(store: &InMemorySessionStore, intent: crate::AttachmentIntent) {
        let fence = store
            .begin_attachment_write(intent.clone())
            .expect("begin attachment write");
        let crate::AttachmentWriteFence::Granted(permit) = fence else {
            panic!("expected a granted write fence");
        };
        store
            .complete_attachment_write(&intent, permit)
            .expect("complete attachment write");
    }

    // Blocker 2 (in-memory): a fresh write that refreshes an aged intent's
    // timestamp past the reconciliation cutoff must survive reconciliation — the
    // age check and the removal happen under one lock, so the refreshed timestamp
    // is what the conditional delete sees. Without a refresh the aged intent is
    // reconciled away.
    #[test]
    fn reconciliation_spares_refreshed_intent_and_collects_stale() {
        let store = InMemorySessionStore::new();
        let cutoff = 200;

        // Refreshed case: recorded old, then re-recorded (refreshed) young.
        put(&store, intent_at("s", "kept", 100));
        put(&store, intent_at("s", "kept", 300));

        // Stale case: recorded old and never refreshed.
        put(&store, intent_at("s", "collected", 100));

        store
            .forget_aged_uncommitted_intents(cutoff)
            .expect("reconcile");

        assert!(
            store
                .list_all_refs()
                .map(|refs| refs
                    .contains(&crate::AttachmentId::parse("kept").expect("valid attachment id")))
                .unwrap(),
            "a refreshed intent (timestamp past the cutoff) must survive reconciliation"
        );
        assert!(
            !store
                .list_all_refs()
                .map(|refs| refs.contains(
                    &crate::AttachmentId::parse("collected").expect("valid attachment id")
                ))
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

        put(&store, intent_at("s", "committed", 100));
        store
            .commit_refs(&SessionId::from("s"), std::slice::from_ref(&committed))
            .unwrap();
        put(&store, intent_at("s", "orphan", 100));

        assert!(store.has_live_ref_for_id(&committed, cutoff).unwrap());
        assert!(
            !store.has_live_ref_for_id(&orphan, cutoff).unwrap(),
            "an aged uncommitted intent is not a live root"
        );

        store.forget_aged_uncommitted_intents(cutoff).unwrap();
        assert!(
            store
                .list_all_refs()
                .map(|refs| refs.contains(&committed))
                .unwrap(),
            "a committed ref survives reconciliation regardless of its intent age"
        );
        assert!(
            !store
                .list_all_refs()
                .map(|refs| refs.contains(&orphan))
                .unwrap()
        );
    }
}
