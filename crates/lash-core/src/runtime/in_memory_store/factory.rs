use super::*;

impl Default for InMemorySessionStoreFactory {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl SessionStoreFactory for InMemorySessionStoreFactory {
    async fn create_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Arc<dyn RuntimePersistence>, String> {
        let _transaction = self
            .write_transaction
            .lock()
            .expect("lock in-memory write transaction");
        self.deleted_sessions
            .lock()
            .expect("lock deleted sessions")
            .remove(&request.session_id);
        let mut stores = self.stores.lock().expect("in-memory store factory");
        let store = stores
            .entry(request.session_id.clone())
            .or_insert_with(|| {
                let store = Arc::new(InMemorySessionStore::with_shared_history(
                    Arc::clone(&self.clock),
                    Arc::clone(&self.write_transaction),
                    Arc::clone(&self.global_session_graph),
                    Arc::clone(&self.global_node_owners),
                    Arc::clone(&self.global_session_heads),
                    Arc::clone(&self.node_anchors),
                    Arc::clone(&self.tombstoned_node_ids),
                    Arc::clone(&self.deleted_sessions),
                    Arc::clone(&self.session_incarnations),
                ));
                *store.session_meta.lock().expect("lock session meta") = Some(crate::SessionMeta {
                    session_id: request.session_id.clone(),
                    incarnation_id: crate::IncarnationId::mint_for_store(),
                    session_name: request.session_id.clone(),
                    created_at: self.clock.timestamp_rfc3339(),
                    model: request.policy.model.id.clone(),
                    cwd: None,
                    relation: request.relation.clone(),
                });
                store
            })
            .clone();
        let incarnation_id = store
            .session_meta
            .lock()
            .expect("lock session meta")
            .as_ref()
            .expect("factory-created store has session metadata")
            .incarnation_id
            .clone();
        self.session_incarnations
            .lock()
            .expect("lock session incarnations")
            .entry(request.session_id.clone())
            .or_insert(incarnation_id);
        Ok(store as Arc<dyn RuntimePersistence>)
    }

    async fn open_existing_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Option<Arc<dyn RuntimePersistence>>, String> {
        Ok(self
            .stores
            .lock()
            .expect("in-memory store factory")
            .get(&request.session_id)
            .cloned()
            .map(|store| store as Arc<dyn RuntimePersistence>))
    }

    async fn delete_session(&self, session_id: &str) -> Result<(), String> {
        let _transaction = self
            .write_transaction
            .lock()
            .expect("lock in-memory write transaction");
        self.deleted_sessions
            .lock()
            .expect("lock deleted sessions")
            .insert(session_id.to_string());
        self.session_incarnations
            .lock()
            .expect("lock session incarnations")
            .remove(session_id);
        let store = self
            .stores
            .lock()
            .expect("in-memory store factory")
            .get(session_id)
            .cloned();
        if let Some(store) = store {
            store
                .reclaim_history_for_delete(session_id)
                .map_err(|error| error.to_string())?;
            self.stores
                .lock()
                .expect("in-memory store factory")
                .remove(session_id);
        }
        Ok(())
    }

    async fn pin(&self, node_id: &str) -> Result<crate::ForkPoint, crate::StoreError> {
        let _transaction = self
            .write_transaction
            .lock()
            .expect("lock in-memory write transaction");
        if let Some((checkpoint_ref, _, source_session_id)) = self
            .node_anchors
            .lock()
            .expect("lock node anchors")
            .get(node_id)
            .cloned()
        {
            return Ok(crate::ForkPoint {
                node_id: node_id.to_string(),
                checkpoint_ref,
                source_session_id,
                pinned: true,
            });
        }
        let stores = self.stores.lock().expect("in-memory store factory");
        let retained = stores
            .iter()
            .filter_map(|(source_session_id, store)| {
                let head = store.session_head_meta.lock().expect("lock session head");
                (head.as_ref().and_then(|head| head.leaf_node_id.as_deref()) == Some(node_id))
                    .then(|| {
                        let checkpoint_ref = head.as_ref()?.checkpoint_ref.clone()?;
                        let checkpoint =
                            store.checkpoint.lock().expect("lock checkpoint").clone()?;
                        Some((checkpoint_ref, checkpoint, source_session_id.clone()))
                    })
                    .flatten()
            })
            .min_by(|left, right| left.2.cmp(&right.2));
        drop(stores);
        let Some((checkpoint_ref, checkpoint, source_session_id)) = retained else {
            return Err(crate::StoreError::ForkPointNotRetained {
                node_id: node_id.to_string(),
            });
        };
        let graph = self.global_session_graph.lock().expect("lock global graph");
        let tombstoned = self
            .tombstoned_node_ids
            .lock()
            .expect("lock tombstoned nodes");
        if graph.find_node(node_id).is_none() || tombstoned.contains(node_id) {
            return Err(crate::StoreError::ForkPointNotRetained {
                node_id: node_id.to_string(),
            });
        }
        drop(tombstoned);
        drop(graph);
        self.node_anchors.lock().expect("lock node anchors").insert(
            node_id.to_string(),
            (
                checkpoint_ref.clone(),
                checkpoint,
                source_session_id.clone(),
            ),
        );
        Ok(crate::ForkPoint {
            node_id: node_id.to_string(),
            checkpoint_ref,
            source_session_id,
            pinned: true,
        })
    }

    async fn unpin(&self, node_id: &str) -> Result<(), crate::StoreError> {
        let _transaction = self
            .write_transaction
            .lock()
            .expect("lock in-memory write transaction");
        let mut anchors = self.node_anchors.lock().expect("lock node anchors").clone();
        if anchors.remove(node_id).is_none() {
            return Ok(());
        }
        let graph = self.global_session_graph.lock().expect("lock global graph");
        let mut tombstoned = self
            .tombstoned_node_ids
            .lock()
            .expect("lock tombstoned nodes")
            .clone();
        let heads = self
            .global_session_heads
            .lock()
            .expect("lock global session heads");
        let anchored_node_ids = anchors.keys().cloned().collect();
        let mut live_child_counts = InMemorySessionStore::live_child_counts(&graph, &tombstoned);
        InMemorySessionStore::reclaim_unreachable_ancestry(
            &graph,
            &mut live_child_counts,
            &mut tombstoned,
            node_id,
            &heads,
            &anchored_node_ids,
        );
        drop(heads);
        drop(graph);
        *self.node_anchors.lock().expect("lock node anchors") = anchors;
        *self
            .tombstoned_node_ids
            .lock()
            .expect("lock tombstoned nodes") = tombstoned;
        Ok(())
    }

    async fn fork_points(&self) -> Result<Vec<crate::ForkPoint>, crate::StoreError> {
        let _transaction = self
            .write_transaction
            .lock()
            .expect("lock in-memory read transaction");
        let anchors = self.node_anchors.lock().expect("lock node anchors");
        let mut points = anchors
            .iter()
            .map(|(node_id, (checkpoint_ref, _, source_session_id))| {
                (
                    node_id.clone(),
                    crate::ForkPoint {
                        node_id: node_id.clone(),
                        checkpoint_ref: checkpoint_ref.clone(),
                        source_session_id: source_session_id.clone(),
                        pinned: true,
                    },
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        let stores = self.stores.lock().expect("in-memory store factory");
        for store in stores.values() {
            let head = store.session_head_meta.lock().expect("lock session head");
            let Some(head) = head.as_ref() else {
                continue;
            };
            let (Some(node_id), Some(checkpoint_ref)) =
                (head.leaf_node_id.as_ref(), head.checkpoint_ref.as_ref())
            else {
                continue;
            };
            let candidate = crate::ForkPoint {
                node_id: node_id.clone(),
                checkpoint_ref: checkpoint_ref.clone(),
                source_session_id: head.session_id.clone(),
                pinned: false,
            };
            match points.entry(node_id.clone()) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(candidate);
                }
                std::collections::btree_map::Entry::Occupied(mut entry)
                    if !entry.get().pinned
                        && candidate.source_session_id < entry.get().source_session_id =>
                {
                    entry.insert(candidate);
                }
                std::collections::btree_map::Entry::Occupied(_) => {}
            }
        }
        Ok(points.into_values().collect())
    }

    async fn fork_at(
        &self,
        request: &crate::ForkSessionRequest,
    ) -> Result<crate::ForkSessionResult, crate::StoreError> {
        let _transaction = self
            .write_transaction
            .lock()
            .expect("lock in-memory write transaction");
        if self
            .stores
            .lock()
            .expect("in-memory store factory")
            .contains_key(&request.session_id)
        {
            return Err(crate::StoreError::ForkSessionAlreadyExists {
                session_id: request.session_id.clone(),
            });
        }
        let retained = self
            .node_anchors
            .lock()
            .expect("lock node anchors")
            .get(&request.node_id)
            .cloned()
            .or_else(|| {
                self.stores
                    .lock()
                    .expect("in-memory store factory")
                    .iter()
                    .filter_map(|(source_session_id, store)| {
                        let head = store.session_head_meta.lock().expect("lock session head");
                        (head.as_ref().and_then(|head| head.leaf_node_id.as_deref())
                            == Some(request.node_id.as_str()))
                        .then(|| {
                            Some((
                                head.as_ref()?.checkpoint_ref.clone()?,
                                store.checkpoint.lock().expect("lock checkpoint").clone()?,
                                source_session_id.clone(),
                            ))
                        })
                        .flatten()
                    })
                    .min_by(|left, right| left.2.cmp(&right.2))
            });
        let Some((checkpoint_ref, checkpoint, source_session_id)) = retained else {
            return Err(crate::StoreError::ForkPointNotRetained {
                node_id: request.node_id.clone(),
            });
        };
        if let crate::SessionRelation::Fork {
            source_session_id: expected,
            ..
        } = &request.relation
            && expected != &source_session_id
        {
            return Err(crate::StoreError::ForkPointNotRetained {
                node_id: request.node_id.clone(),
            });
        }
        let graph = self.global_session_graph.lock().expect("lock global graph");
        let tombstoned = self
            .tombstoned_node_ids
            .lock()
            .expect("lock tombstoned nodes");
        if graph.find_node(&request.node_id).is_none() || tombstoned.contains(&request.node_id) {
            return Err(crate::StoreError::ForkPointNotRetained {
                node_id: request.node_id.clone(),
            });
        }
        let current_frame_node_id = graph
            .nearest_frame_node_id(Some(&request.node_id))
            .map(ToOwned::to_owned)
            .ok_or_else(|| crate::StoreError::MissingFrameOpenAncestor {
                leaf_node_id: request.node_id.clone(),
            })?;
        let mut resident_graph = graph.clone();
        resident_graph.set_leaf_node_id(Some(request.node_id.clone()));
        resident_graph = resident_graph.trim_to_active_path();
        drop(tombstoned);
        drop(graph);
        self.global_session_heads
            .lock()
            .expect("lock global session heads")
            .insert(request.session_id.clone(), Some(request.node_id.clone()));
        let store = Arc::new(InMemorySessionStore::with_shared_history(
            Arc::clone(&self.clock),
            Arc::clone(&self.write_transaction),
            Arc::clone(&self.global_session_graph),
            Arc::clone(&self.global_node_owners),
            Arc::clone(&self.global_session_heads),
            Arc::clone(&self.node_anchors),
            Arc::clone(&self.tombstoned_node_ids),
            Arc::clone(&self.deleted_sessions),
            Arc::clone(&self.session_incarnations),
        ));
        *store.session_graph.lock().expect("lock graph") = resident_graph.clone();
        *store.checkpoint.lock().expect("lock checkpoint") = Some(checkpoint);
        *store.session_head_meta.lock().expect("lock session head") =
            Some(crate::store::SessionHeadMeta {
                schema_version: crate::store::SESSION_HEAD_META_SCHEMA_VERSION,
                session_id: request.session_id.clone(),
                head_revision: 0,
                config: crate::PersistedSessionConfig {
                    provider_id: request.policy.recorded_provider_id().to_string(),
                    model: request.policy.model.clone(),
                },
                current_frame_node_id: Some(current_frame_node_id),
                checkpoint_ref: Some(checkpoint_ref),
                leaf_node_id: Some(request.node_id.clone()),
            });
        let incarnation_id = crate::IncarnationId::mint_for_store();
        *store.session_meta.lock().expect("lock session meta") = Some(crate::SessionMeta {
            session_id: request.session_id.clone(),
            incarnation_id: incarnation_id.clone(),
            session_name: request.session_id.clone(),
            created_at: self.clock.timestamp_rfc3339(),
            model: request.policy.model.id.clone(),
            cwd: None,
            relation: request.relation.clone(),
        });
        self.deleted_sessions
            .lock()
            .expect("lock deleted sessions")
            .remove(&request.session_id);
        self.session_incarnations
            .lock()
            .expect("lock session incarnations")
            .insert(request.session_id.clone(), incarnation_id);
        self.stores
            .lock()
            .expect("in-memory store factory")
            .insert(request.session_id.clone(), store);
        Ok(crate::ForkSessionResult {
            session_id: request.session_id.clone(),
            node_id: request.node_id.clone(),
            source_session_id,
        })
    }

    async fn live_attachment_refs(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<std::collections::BTreeSet<crate::AttachmentId>, crate::store::StoreError> {
        let stores = {
            self.stores
                .lock()
                .expect("in-memory store factory")
                .values()
                .cloned()
                .collect::<Vec<_>>()
        };
        let mut refs = std::collections::BTreeSet::new();
        for store in stores {
            // Apply age and durable owner-death in one conditional pass (no
            // list-then-forget race against a concurrent intent refresh), then
            // union the surviving roots.
            crate::AttachmentManifest::forget_aged_uncommitted_intents(
                &*store,
                intent_grace_cutoff_epoch_ms,
            )?;
            refs.extend(crate::AttachmentManifest::list_all_refs(&*store)?);
        }
        Ok(refs)
    }

    async fn has_live_attachment_ref(
        &self,
        id: &crate::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, crate::store::StoreError> {
        let stores = {
            self.stores
                .lock()
                .expect("in-memory store factory")
                .values()
                .cloned()
                .collect::<Vec<_>>()
        };
        for store in stores {
            if crate::AttachmentManifest::has_live_ref_for_id(
                &*store,
                id,
                intent_grace_cutoff_epoch_ms,
            )? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}
