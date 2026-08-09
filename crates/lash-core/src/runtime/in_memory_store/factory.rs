use super::*;
use crate::facade_support::SessionGraphFacadeOps;
use lash_sansio::sync::MutexExt;

/// Session-id-keyed factory: the same in-memory store is returned for a given
/// session across opens (so a worker rebuild sees the session's state), and a
/// fresh store is created on first use.
#[derive(Clone)]
pub struct InMemorySessionStoreFactory {
    pub(super) clock: Arc<dyn crate::Clock>,
    pub(super) stores: Arc<Mutex<HashMap<String, Arc<InMemorySessionStore>>>>,
    pub(super) write_transaction: Arc<Mutex<()>>,
    pub(super) global_session_graph: Arc<Mutex<crate::SessionGraph>>,
    pub(super) global_node_owners: Arc<Mutex<HashMap<String, String>>>,
    pub(super) global_session_heads: Arc<Mutex<HashMap<String, Option<String>>>>,
    pub(super) node_anchors: InMemoryNodeAnchors,
    pub(super) tombstoned_node_ids: Arc<Mutex<HashSet<String>>>,
    pub(super) deleted_session_ids: Arc<Mutex<HashSet<String>>>,
}

impl InMemorySessionStoreFactory {
    pub fn new() -> Self {
        Self::with_clock(Arc::new(crate::SystemClock))
    }

    pub fn with_clock(clock: Arc<dyn crate::Clock>) -> Self {
        Self {
            clock,
            stores: Arc::new(Mutex::new(HashMap::new())),
            write_transaction: Arc::new(Mutex::new(())),
            global_session_graph: Arc::new(Mutex::new(crate::SessionGraph::default())),
            global_node_owners: Arc::new(Mutex::new(HashMap::new())),
            global_session_heads: Arc::new(Mutex::new(HashMap::new())),
            node_anchors: Arc::new(Mutex::new(HashMap::new())),
            tombstoned_node_ids: Arc::new(Mutex::new(HashSet::new())),
            deleted_session_ids: Arc::new(Mutex::new(HashSet::new())),
        }
    }
}

fn retained_fork_config(
    graph: &crate::SessionGraph,
    node_id: &str,
) -> Result<crate::PersistedSessionConfig, crate::StoreError> {
    let frame_node_id = graph.nearest_frame_node_id(Some(node_id)).ok_or_else(|| {
        crate::StoreError::MissingFrameOpenAncestor {
            leaf_node_id: node_id.to_string(),
        }
    })?;
    graph
        .find_node(frame_node_id)
        .and_then(crate::SessionNodeRecord::frame_config)
        .ok_or_else(|| {
            crate::StoreError::Backend(format!(
                "retained frame node `{frame_node_id}` has no frame assignment"
            ))
        })
}

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
    ) -> Result<Arc<dyn RuntimePersistence>, crate::StoreError> {
        let binding = crate::SessionBinding::from_create_request(request);
        binding.validate()?;
        let created_at = self.clock.timestamp_rfc3339();
        let _transaction = self.write_transaction.lock_recover();
        if self
            .deleted_session_ids
            .lock_recover()
            .contains(&request.session_id)
        {
            return Err(crate::StoreError::SessionDeleted {
                session_id: request.session_id.clone(),
            });
        }
        let mut stores = self.stores.lock_recover();
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
                    Arc::clone(&self.deleted_session_ids),
                ));
                *store.session_meta.lock_recover() = Some(crate::SessionMeta {
                    session_id: request.session_id.clone(),
                    session_name: request.session_id.clone(),
                    created_at: created_at.clone(),
                    model: binding.model_id.clone(),
                    cwd: binding.cwd.clone(),
                    relation: binding.relation.clone(),
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
            .lock_recover()
            .get(&request.session_id)
            .cloned()
            .map(|store| store as Arc<dyn RuntimePersistence>))
    }

    async fn has_claimable_queued_work(
        &self,
        request: &SessionStoreCreateRequest,
        now_epoch_ms: u64,
    ) -> Result<Option<bool>, crate::StoreError> {
        let store = self.stores.lock_recover().get(&request.session_id).cloned();
        let Some(store) = store else {
            return Ok(Some(false));
        };
        // This is a conservative readiness peek, not a claim. A due row with a
        // same-generation claim may belong to a crashed/live lease holder; it
        // must keep the driver's bounded contention recheck armed until the
        // generation becomes reclaimable.
        if store.queued_work.lock_recover().iter().any(|entry| {
            entry.batch.session_id == request.session_id
                && entry.batch.available_at_ms <= now_epoch_ms
        }) {
            return Ok(Some(true));
        }
        Ok(Some(store.pending_turn_inputs.lock_recover().iter().any(
            |entry| {
                entry.input.session_id == request.session_id
                    && entry.input.state == crate::TurnInputState::DeferredNextTurn
            },
        )))
    }

    async fn session_was_deleted(&self, session_id: &str) -> Result<bool, String> {
        Ok(self.deleted_session_ids.lock_recover().contains(session_id))
    }

    async fn delete_session(&self, session_id: &str) -> Result<(), String> {
        let _transaction = self.write_transaction.lock_recover();
        let store = self.stores.lock_recover().get(session_id).cloned();
        if let Some(store) = store {
            self.deleted_session_ids
                .lock_recover()
                .insert(session_id.to_string());
            store
                .reclaim_history_for_delete(session_id)
                .map_err(|error| error.to_string())?;
            self.stores.lock_recover().remove(session_id);
        }
        Ok(())
    }

    async fn pin(&self, node_id: &str) -> Result<crate::ForkPoint, crate::StoreError> {
        let _transaction = self.write_transaction.lock_recover();
        if let Some((checkpoint_ref, _, source_session_id)) =
            self.node_anchors.lock_recover().get(node_id).cloned()
        {
            let graph = self.global_session_graph.lock_recover();
            return Ok(crate::ForkPoint {
                node_id: node_id.to_string(),
                checkpoint_ref,
                source_session_id,
                config: retained_fork_config(&graph, node_id)?,
                pinned: true,
            });
        }
        let stores = self.stores.lock_recover();
        let retained = stores
            .iter()
            .filter_map(|(source_session_id, store)| {
                let head = store.session_head_meta.lock_recover();
                (head.as_ref().and_then(|head| head.leaf_node_id.as_deref()) == Some(node_id))
                    .then(|| {
                        let checkpoint_ref = head.as_ref()?.checkpoint_ref.clone()?;
                        let checkpoint = store.checkpoint.lock_recover().clone()?;
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
        let graph = self.global_session_graph.lock_recover().clone();
        let tombstoned = self.tombstoned_node_ids.lock_recover();
        if graph.find_node(node_id).is_none() || tombstoned.contains(node_id) {
            return Err(crate::StoreError::ForkPointNotRetained {
                node_id: node_id.to_string(),
            });
        }
        let config = retained_fork_config(&graph, node_id)?;
        drop(tombstoned);
        drop(graph);
        self.node_anchors.lock_recover().insert(
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
            config,
            pinned: true,
        })
    }

    async fn unpin(&self, node_id: &str) -> Result<(), crate::StoreError> {
        let _transaction = self.write_transaction.lock_recover();
        let mut anchors = self.node_anchors.lock_recover().clone();
        if anchors.remove(node_id).is_none() {
            return Ok(());
        }
        let graph = self.global_session_graph.lock_recover();
        let mut tombstoned = self.tombstoned_node_ids.lock_recover().clone();
        let heads = self.global_session_heads.lock_recover();
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
        *self.node_anchors.lock_recover() = anchors;
        *self.tombstoned_node_ids.lock_recover() = tombstoned;
        Ok(())
    }

    async fn fork_points(&self) -> Result<Vec<crate::ForkPoint>, crate::StoreError> {
        let _transaction = self.write_transaction.lock_recover();
        let graph = self.global_session_graph.lock_recover().clone();
        let anchors = self.node_anchors.lock_recover();
        let mut points = std::collections::BTreeMap::new();
        for (node_id, (checkpoint_ref, _, source_session_id)) in anchors.iter() {
            points.insert(
                node_id.clone(),
                crate::ForkPoint {
                    node_id: node_id.clone(),
                    checkpoint_ref: checkpoint_ref.clone(),
                    source_session_id: source_session_id.clone(),
                    config: retained_fork_config(&graph, node_id)?,
                    pinned: true,
                },
            );
        }
        let stores = self.stores.lock_recover();
        for store in stores.values() {
            let head = store.session_head_meta.lock_recover();
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
                config: retained_fork_config(&graph, node_id)?,
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
        let created_at = self.clock.timestamp_rfc3339();
        let _transaction = self.write_transaction.lock_recover();
        if self.stores.lock_recover().contains_key(&request.session_id) {
            return Err(crate::StoreError::ForkSessionAlreadyExists {
                session_id: request.session_id.clone(),
            });
        }
        if self
            .deleted_session_ids
            .lock_recover()
            .contains(&request.session_id)
        {
            return Err(crate::StoreError::SessionDeleted {
                session_id: request.session_id.clone(),
            });
        }
        let retained = self
            .node_anchors
            .lock_recover()
            .get(&request.node_id)
            .cloned()
            .or_else(|| {
                self.stores
                    .lock_recover()
                    .iter()
                    .filter_map(|(source_session_id, store)| {
                        let head = store.session_head_meta.lock_recover();
                        (head.as_ref().and_then(|head| head.leaf_node_id.as_deref())
                            == Some(request.node_id.as_str()))
                        .then(|| {
                            Some((
                                head.as_ref()?.checkpoint_ref.clone()?,
                                store.checkpoint.lock_recover().clone()?,
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
        let graph = self.global_session_graph.lock_recover();
        let tombstoned = self.tombstoned_node_ids.lock_recover();
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
            .lock_recover()
            .insert(request.session_id.clone(), Some(request.node_id.clone()));
        let store = Arc::new(InMemorySessionStore::with_shared_history(
            Arc::clone(&self.clock),
            Arc::clone(&self.write_transaction),
            Arc::clone(&self.global_session_graph),
            Arc::clone(&self.global_node_owners),
            Arc::clone(&self.global_session_heads),
            Arc::clone(&self.node_anchors),
            Arc::clone(&self.tombstoned_node_ids),
            Arc::clone(&self.deleted_session_ids),
        ));
        *store.session_graph.lock_recover() = resident_graph.clone();
        *store.checkpoint.lock_recover() = Some(checkpoint);
        *store.session_head_meta.lock_recover() = Some(crate::store::SessionHeadMeta::assemble(
            crate::store::SessionHeadPayload {
                schema_version: crate::store::SESSION_HEAD_META_SCHEMA_VERSION,
                session_id: request.session_id.clone(),
                config: crate::PersistedSessionConfig {
                    provider_id: request.policy.recorded_provider_id().to_string(),
                    model: request.policy.model.clone(),
                    turn_budget: request.policy.turn_budget,
                },
                current_frame_node_id: Some(current_frame_node_id),
            },
            0,
            Some(checkpoint_ref),
            Some(request.node_id.clone()),
        ));
        *store.session_meta.lock_recover() = Some(crate::SessionMeta {
            session_id: request.session_id.clone(),
            session_name: request.session_id.clone(),
            created_at,
            model: request.policy.model.id.clone(),
            cwd: None,
            relation: request.relation.clone(),
        });
        self.stores
            .lock_recover()
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
                .lock_recover()
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
                .lock_recover()
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
