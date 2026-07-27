use super::*;

#[async_trait::async_trait]
impl SessionStoreFactory for InMemorySessionStoreFactory {
    async fn create_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Arc<dyn RuntimePersistence>, String> {
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
                    Arc::clone(&self.incoming_node_refs),
                ));
                *store.session_meta.lock().expect("lock session meta") = Some(crate::SessionMeta {
                    session_id: request.session_id.clone(),
                    session_name: request.session_id.clone(),
                    created_at: self.clock.timestamp_rfc3339(),
                    model: request.policy.model.id.clone(),
                    cwd: None,
                    relation: request.relation.clone(),
                });
                store
            })
            .clone();
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
        if let Some((checkpoint_ref, _)) = self
            .node_anchors
            .lock()
            .expect("lock node anchors")
            .get(node_id)
            .cloned()
        {
            return Ok(crate::ForkPoint {
                node_id: node_id.to_string(),
                checkpoint_ref,
                pinned: true,
            });
        }
        let stores = self.stores.lock().expect("in-memory store factory");
        let retained = stores.values().find_map(|store| {
            let head = store.session_head_meta.lock().expect("lock session head");
            (head.as_ref().and_then(|head| head.leaf_node_id.as_deref()) == Some(node_id))
                .then(|| {
                    let checkpoint_ref = head.as_ref()?.checkpoint_ref.clone()?;
                    let checkpoint = store.checkpoint.lock().expect("lock checkpoint").clone()?;
                    Some((checkpoint_ref, checkpoint))
                })
                .flatten()
        });
        drop(stores);
        let Some((checkpoint_ref, checkpoint)) = retained else {
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
        *self
            .incoming_node_refs
            .lock()
            .expect("lock incoming node refs")
            .entry(node_id.to_string())
            .or_default() += 1;
        self.node_anchors
            .lock()
            .expect("lock node anchors")
            .insert(node_id.to_string(), (checkpoint_ref.clone(), checkpoint));
        Ok(crate::ForkPoint {
            node_id: node_id.to_string(),
            checkpoint_ref,
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
        let mut counts = self
            .incoming_node_refs
            .lock()
            .expect("lock incoming node refs")
            .clone();
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
        InMemorySessionStore::decrement_node_reference(
            &graph,
            &mut counts,
            &mut tombstoned,
            node_id,
            &heads,
            &anchored_node_ids,
        )?;
        drop(heads);
        drop(graph);
        *self.node_anchors.lock().expect("lock node anchors") = anchors;
        *self
            .incoming_node_refs
            .lock()
            .expect("lock incoming node refs") = counts;
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
            .map(|(node_id, (checkpoint_ref, _))| {
                (
                    node_id.clone(),
                    crate::ForkPoint {
                        node_id: node_id.clone(),
                        checkpoint_ref: checkpoint_ref.clone(),
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
            points
                .entry(node_id.clone())
                .or_insert_with(|| crate::ForkPoint {
                    node_id: node_id.clone(),
                    checkpoint_ref: checkpoint_ref.clone(),
                    pinned: false,
                });
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
                    .values()
                    .find_map(|store| {
                        let head = store.session_head_meta.lock().expect("lock session head");
                        (head.as_ref().and_then(|head| head.leaf_node_id.as_deref())
                            == Some(request.node_id.as_str()))
                        .then(|| {
                            Some((
                                head.as_ref()?.checkpoint_ref.clone()?,
                                store.checkpoint.lock().expect("lock checkpoint").clone()?,
                            ))
                        })
                        .flatten()
                    })
            });
        let Some((checkpoint_ref, checkpoint)) = retained else {
            return Err(crate::StoreError::ForkPointNotRetained {
                node_id: request.node_id.clone(),
            });
        };
        let graph = self.global_session_graph.lock().expect("lock global graph");
        let tombstoned = self
            .tombstoned_node_ids
            .lock()
            .expect("lock tombstoned nodes");
        let source_session_id = self
            .global_node_owners
            .lock()
            .expect("lock global node owners")
            .get(&request.node_id)
            .cloned()
            .filter(|_| {
                graph.find_node(&request.node_id).is_some()
                    && !tombstoned.contains(&request.node_id)
            })
            .ok_or_else(|| crate::StoreError::ForkPointNotRetained {
                node_id: request.node_id.clone(),
            })?;
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
        *self
            .incoming_node_refs
            .lock()
            .expect("lock incoming node refs")
            .entry(request.node_id.clone())
            .or_default() += 1;
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
            Arc::clone(&self.incoming_node_refs),
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
                graph_node_count: resident_graph.nodes.len(),
                token_ledger: Vec::new(),
            });
        *store.session_meta.lock().expect("lock session meta") = Some(crate::SessionMeta {
            session_id: request.session_id.clone(),
            session_name: request.session_id.clone(),
            created_at: self.clock.timestamp_rfc3339(),
            model: request.policy.model.id.clone(),
            cwd: None,
            relation: request.relation.clone(),
        });
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
