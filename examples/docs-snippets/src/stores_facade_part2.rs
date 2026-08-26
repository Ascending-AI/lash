//! Facade-only compile-time witnesses for durable-store host contracts.
//!
//! These probes type-check public contracts without constructing a live backend.

#![allow(dead_code, unreachable_code, unused_variables)]

fn type_witness<T>() {}
fn member_witness<T>(_: T) {}
fn field_witness<T>(_: impl FnOnce(&T)) {}
fn variant_witness<T>(_: impl FnOnce(&T) -> bool) {}

pub(crate) fn store_area_facade_witnesses() {
    // FIG-2105-WITNESS-0241: lash::persistence::SessionCommitStore::save_session_meta [function]
    fn method_witness_0241<T: lash::persistence::SessionCommitStore>() {
        member_witness(T::save_session_meta);
    }
    // FIG-2105-WITNESS-0242: lash::persistence::SessionExecutionLease [struct]
    type_witness::<lash::persistence::SessionExecutionLease>();
    // FIG-2105-WITNESS-0243: lash::persistence::SessionExecutionLease::authority [function]
    member_witness(lash::persistence::SessionExecutionLease::authority);
    // FIG-2105-WITNESS-0244: lash::persistence::SessionExecutionLease::claimed_at_epoch_ms [field]
    field_witness(|value: &lash::persistence::SessionExecutionLease| {
        let _ = &value.claimed_at_epoch_ms;
    });
    // FIG-2105-WITNESS-0245: lash::persistence::SessionExecutionLease::completion [function]
    member_witness(lash::persistence::SessionExecutionLease::completion);
    // FIG-2105-WITNESS-0246: lash::persistence::SessionExecutionLease::expires_at_epoch_ms [field]
    field_witness(|value: &lash::persistence::SessionExecutionLease| {
        let _ = &value.expires_at_epoch_ms;
    });
    // FIG-2105-WITNESS-0247: lash::persistence::SessionExecutionLease::executor_id [field]
    field_witness(|value: &lash::persistence::SessionExecutionLease| {
        let _ = &value.executor_id;
    });
    // FIG-2105-WITNESS-0248: lash::persistence::SessionExecutionLease::fence [function]
    member_witness(lash::persistence::SessionExecutionLease::fence);
    // FIG-2105-WITNESS-0249: lash::persistence::SessionExecutionLease::fencing_token [field]
    field_witness(|value: &lash::persistence::SessionExecutionLease| {
        let _ = &value.fencing_token;
    });
    // FIG-2105-WITNESS-0250: lash::persistence::SessionExecutionLease::lease_term_ms [field]
    field_witness(|value: &lash::persistence::SessionExecutionLease| {
        let _ = &value.lease_term_ms;
    });
    // FIG-2105-WITNESS-0251: lash::persistence::SessionExecutionLease::lease_token [field]
    field_witness(|value: &lash::persistence::SessionExecutionLease| {
        let _ = &value.lease_token;
    });
    // FIG-2105-WITNESS-0252: lash::persistence::SessionExecutionLease::owner [field]
    field_witness(|value: &lash::persistence::SessionExecutionLease| {
        let _ = &value.owner;
    });
    // FIG-2105-WITNESS-0253: lash::persistence::SessionExecutionLease::session_id [field]
    field_witness(|value: &lash::persistence::SessionExecutionLease| {
        let _ = &value.session_id;
    });
    // FIG-2105-WITNESS-0254: lash::persistence::SessionExecutionLeaseAcquisition [struct]
    type_witness::<lash::persistence::SessionExecutionLeaseAcquisition>();
    // FIG-2105-WITNESS-0255: lash::persistence::SessionExecutionLeaseAcquisition::displaced [field]
    field_witness(
        |value: &lash::persistence::SessionExecutionLeaseAcquisition| {
            let _ = &value.displaced;
        },
    );
    // FIG-2105-WITNESS-0256: lash::persistence::SessionExecutionLeaseAcquisition::displacing_observed [function]
    member_witness(lash::persistence::SessionExecutionLeaseAcquisition::displacing_observed);
    // FIG-2105-WITNESS-0257: lash::persistence::SessionExecutionLeaseAcquisition::fresh [function]
    member_witness(lash::persistence::SessionExecutionLeaseAcquisition::fresh);
    // FIG-2105-WITNESS-0258: lash::persistence::SessionExecutionLeaseAcquisition::lease [field]
    field_witness(
        |value: &lash::persistence::SessionExecutionLeaseAcquisition| {
            let _ = &value.lease;
        },
    );
    // FIG-2105-WITNESS-0259: lash::persistence::SessionExecutionLeaseAuthority [struct]
    type_witness::<lash::persistence::SessionExecutionLeaseAuthority>();
    // FIG-2105-WITNESS-0260: lash::persistence::SessionExecutionLeaseAuthority::fencing_token [field]
    field_witness(
        |value: &lash::persistence::SessionExecutionLeaseAuthority| {
            let _ = &value.fencing_token;
        },
    );
    // FIG-2105-WITNESS-0261: lash::persistence::SessionExecutionLeaseAuthority::executor_id [field]
    field_witness(
        |value: &lash::persistence::SessionExecutionLeaseAuthority| {
            let _ = &value.executor_id;
        },
    );
    // FIG-2105-WITNESS-0262: lash::persistence::SessionExecutionLeaseAuthority::lease_token [field]
    field_witness(
        |value: &lash::persistence::SessionExecutionLeaseAuthority| {
            let _ = &value.lease_token;
        },
    );
    // FIG-2105-WITNESS-0263: lash::persistence::SessionExecutionLeaseAuthority::owner [field]
    field_witness(
        |value: &lash::persistence::SessionExecutionLeaseAuthority| {
            let _ = &value.owner;
        },
    );
    // FIG-2105-WITNESS-0264: lash::persistence::SessionExecutionLeaseAuthority::session_id [field]
    field_witness(
        |value: &lash::persistence::SessionExecutionLeaseAuthority| {
            let _ = &value.session_id;
        },
    );
    // FIG-2105-WITNESS-0265: lash::persistence::SessionExecutionLeaseClaimOutcome [enum]
    type_witness::<lash::persistence::SessionExecutionLeaseClaimOutcome>();
    // FIG-2105-WITNESS-0266: lash::persistence::SessionExecutionLeaseClaimOutcome::Acquired [variant]
    variant_witness(
        |value: &lash::persistence::SessionExecutionLeaseClaimOutcome| {
            matches!(
                value,
                lash::persistence::SessionExecutionLeaseClaimOutcome::Acquired(..)
            )
        },
    );
    // FIG-2105-WITNESS-0267: lash::persistence::SessionExecutionLeaseClaimOutcome::Acquired::0 [field]
    field_witness(
        |value: &lash::persistence::SessionExecutionLeaseClaimOutcome| {
            if let lash::persistence::SessionExecutionLeaseClaimOutcome::Acquired(field, ..) = value
            {
                let _ = field;
            }
        },
    );
    // FIG-2105-WITNESS-0268: lash::persistence::SessionExecutionLeaseClaimOutcome::Busy [variant]
    variant_witness(
        |value: &lash::persistence::SessionExecutionLeaseClaimOutcome| {
            matches!(
                value,
                lash::persistence::SessionExecutionLeaseClaimOutcome::Busy { .. }
            )
        },
    );
    // FIG-2105-WITNESS-0269: lash::persistence::SessionExecutionLeaseClaimOutcome::Busy::holder [field]
    field_witness(
        |value: &lash::persistence::SessionExecutionLeaseClaimOutcome| {
            if let lash::persistence::SessionExecutionLeaseClaimOutcome::Busy { holder, .. } = value
            {
                let _ = holder;
            }
        },
    );
    // FIG-2105-WITNESS-0270: lash::persistence::SessionExecutionLeaseClaimOutcome::acquired [function]
    member_witness(lash::persistence::SessionExecutionLeaseClaimOutcome::acquired);
    // FIG-2105-WITNESS-0271: lash::persistence::SessionExecutionLeaseClaimOutcome::acquisition [function]
    member_witness(lash::persistence::SessionExecutionLeaseClaimOutcome::acquisition);
    // FIG-2105-WITNESS-0272: lash::persistence::SessionExecutionLeaseDisplacement [struct]
    type_witness::<lash::persistence::SessionExecutionLeaseDisplacement>();
    // FIG-2105-WITNESS-0273: lash::persistence::SessionExecutionLeaseDisplacement::expired_at_epoch_ms [field]
    field_witness(
        |value: &lash::persistence::SessionExecutionLeaseDisplacement| {
            let _ = &value.expired_at_epoch_ms;
        },
    );
    // FIG-2105-WITNESS-0274: lash::persistence::SessionExecutionLeaseDisplacement::executor_id [field]
    field_witness(
        |value: &lash::persistence::SessionExecutionLeaseDisplacement| {
            let _ = &value.executor_id;
        },
    );
    // FIG-2105-WITNESS-0275: lash::persistence::SessionExecutionLeaseDisplacement::fencing_token [field]
    field_witness(
        |value: &lash::persistence::SessionExecutionLeaseDisplacement| {
            let _ = &value.fencing_token;
        },
    );
    // FIG-2105-WITNESS-0276: lash::persistence::SessionExecutionLeaseDisplacement::owner [field]
    field_witness(
        |value: &lash::persistence::SessionExecutionLeaseDisplacement| {
            let _ = &value.owner;
        },
    );
    // FIG-2105-WITNESS-0277: lash::persistence::SessionExecutionLeaseRenewalInstallMismatch [enum]
    type_witness::<lash::persistence::SessionExecutionLeaseRenewalInstallMismatch>();
    // FIG-2105-WITNESS-0278: lash::persistence::SessionExecutionLeaseRenewalInstallMismatch::ExpiryRegressed [variant]
    variant_witness(
        |value: &lash::persistence::SessionExecutionLeaseRenewalInstallMismatch| {
            matches!(
                value,
                lash::persistence::SessionExecutionLeaseRenewalInstallMismatch::ExpiryRegressed
            )
        },
    );
    // FIG-2105-WITNESS-0279: lash::persistence::SessionExecutionLeaseRenewalInstallMismatch::Executor [variant]
    variant_witness(
        |value: &lash::persistence::SessionExecutionLeaseRenewalInstallMismatch| {
            matches!(
                value,
                lash::persistence::SessionExecutionLeaseRenewalInstallMismatch::Executor
            )
        },
    );
    // FIG-2105-WITNESS-0280: lash::persistence::SessionExecutionLeaseRenewalInstallMismatch::FencingToken [variant]
    variant_witness(
        |value: &lash::persistence::SessionExecutionLeaseRenewalInstallMismatch| {
            matches!(
                value,
                lash::persistence::SessionExecutionLeaseRenewalInstallMismatch::FencingToken
            )
        },
    );
    // FIG-2105-WITNESS-0281: lash::persistence::SessionExecutionLeaseRenewalInstallMismatch::LeaseToken [variant]
    variant_witness(
        |value: &lash::persistence::SessionExecutionLeaseRenewalInstallMismatch| {
            matches!(
                value,
                lash::persistence::SessionExecutionLeaseRenewalInstallMismatch::LeaseToken
            )
        },
    );
    // FIG-2105-WITNESS-0282: lash::persistence::SessionExecutionLeaseRenewalInstallMismatch::OwnerIncarnation [variant]
    variant_witness(
        |value: &lash::persistence::SessionExecutionLeaseRenewalInstallMismatch| {
            matches!(
                value,
                lash::persistence::SessionExecutionLeaseRenewalInstallMismatch::OwnerIncarnation
            )
        },
    );
    // FIG-2105-WITNESS-0283: lash::persistence::SessionExecutionLeaseRenewalInstallMismatch::Session [variant]
    variant_witness(
        |value: &lash::persistence::SessionExecutionLeaseRenewalInstallMismatch| {
            matches!(
                value,
                lash::persistence::SessionExecutionLeaseRenewalInstallMismatch::Session
            )
        },
    );
    // FIG-2105-WITNESS-0284: lash::persistence::SessionExecutionLeaseStore [trait]
    fn trait_witness_0284<T: lash::persistence::SessionExecutionLeaseStore>() {}
    // FIG-2105-WITNESS-0285: lash::persistence::SessionExecutionLeaseStore::release_session_execution_lease [function]
    fn method_witness_0285<T: lash::persistence::SessionExecutionLeaseStore>() {
        member_witness(T::release_session_execution_lease);
    }
    // FIG-2105-WITNESS-0286: lash::persistence::SessionExecutionLeaseStore::renew_session_execution_lease [function]
    fn method_witness_0286<T: lash::persistence::SessionExecutionLeaseStore>() {
        member_witness(T::renew_session_execution_lease);
    }
    // FIG-2105-WITNESS-0287: lash::persistence::SessionExecutionLeaseStore::try_claim_session_execution_lease [function]
    fn method_witness_0287<T: lash::persistence::SessionExecutionLeaseStore>() {
        member_witness(T::try_claim_session_execution_lease);
    }
    // FIG-2105-WITNESS-0288: lash::persistence::SessionExecutionLeaseStore::try_claim_session_execution_lease_with_token [function]
    fn method_witness_0288<T: lash::persistence::SessionExecutionLeaseStore>() {
        member_witness(T::try_claim_session_execution_lease_with_token);
    }
    // FIG-2105-WITNESS-0289: lash::persistence::SessionGraph [struct]
    type_witness::<lash::persistence::SessionGraph>();
    // FIG-2105-WITNESS-0290: lash::persistence::SessionGraph::append_active_read_delta [function]
    member_witness(lash::persistence::SessionGraph::append_active_read_delta);
    // FIG-2105-WITNESS-0291: lash::persistence::SessionGraph::append_message [function]
    member_witness(lash::persistence::SessionGraph::append_message);
    // FIG-2105-WITNESS-0292: lash::persistence::SessionGraph::from_active_read_state [function]
    member_witness(lash::persistence::SessionGraph::from_active_read_state);
    // FIG-2105-WITNESS-0293: lash::persistence::SessionGraph::from_nodes [function]
    member_witness(lash::persistence::SessionGraph::from_nodes);
    // FIG-2105-WITNESS-0294: lash::persistence::SessionGraph::message_tree [function]
    member_witness(lash::persistence::SessionGraph::message_tree);
    // FIG-2105-WITNESS-0295: lash::persistence::SessionGraph::push_node_record [function]
    member_witness(lash::persistence::SessionGraph::push_node_record);
    // FIG-2105-WITNESS-0296: lash::persistence::SessionGraph::rewrite_active_read_tail [function]
    member_witness(lash::persistence::SessionGraph::rewrite_active_read_tail);
    // FIG-2105-WITNESS-0297: lash::persistence::SessionGraph::set_leaf_node_id [function]
    member_witness(lash::persistence::SessionGraph::set_leaf_node_id);
    // FIG-2105-WITNESS-0298: lash::persistence::SessionGraph::trim_to_active_path [function]
    member_witness(lash::persistence::SessionGraph::trim_to_active_path);
    // FIG-2105-WITNESS-0299: lash::persistence::SessionHistoryRecord [type_alias]
    type_witness::<lash::persistence::SessionHistoryRecord>();
    // FIG-2105-WITNESS-0300: lash::persistence::SessionMeta [struct]
    type_witness::<lash::persistence::SessionMeta>();
    // FIG-2105-WITNESS-0301: lash::persistence::SessionMeta::parent_session_id [function]
    member_witness(lash::persistence::SessionMeta::parent_session_id);
    // FIG-2105-WITNESS-0302: lash::persistence::SessionMeta::relation [field]
    field_witness(|value: &lash::persistence::SessionMeta| {
        let _ = &value.relation;
    });
    // FIG-2105-WITNESS-0303: lash::persistence::SessionMeta::session_id [field]
    field_witness(|value: &lash::persistence::SessionMeta| {
        let _ = &value.session_id;
    });
    // FIG-2105-WITNESS-0304: lash::persistence::SessionNodeRecord [struct]
    type_witness::<lash::persistence::SessionNodeRecord>();
    // FIG-2105-WITNESS-0305: lash::persistence::SessionNodeRecord::decode_storage_body [function]
    member_witness(lash::persistence::SessionNodeRecord::decode_storage_body);
    // FIG-2105-WITNESS-0306: lash::persistence::SessionNodeRecord::encode_storage_body [function]
    member_witness(lash::persistence::SessionNodeRecord::encode_storage_body);
    // FIG-2105-WITNESS-0307: lash::persistence::SessionNodeRecord::frame_config [function]
    member_witness(lash::persistence::SessionNodeRecord::frame_config);
    // FIG-2105-WITNESS-0308: lash::persistence::SessionNodeRecord::frame_open [function]
    member_witness(lash::persistence::SessionNodeRecord::frame_open);
    // FIG-2105-WITNESS-0309: lash::persistence::SessionReadView::active_events [function]
    member_witness(lash::persistence::SessionReadView::active_events);
    // FIG-2105-WITNESS-0310: lash::persistence::SessionReadView::from_persisted_state [function]
    member_witness(lash::persistence::SessionReadView::from_persisted_state);
    // FIG-2105-WITNESS-0311: lash::persistence::SessionReadView::from_snapshot [function]
    member_witness(lash::persistence::SessionReadView::from_snapshot);
    // FIG-2105-WITNESS-0312: lash::persistence::SessionReadView::message_tree [function]
    member_witness(lash::persistence::SessionReadView::message_tree);
    // FIG-2105-WITNESS-0313: lash::persistence::SessionReadView::policy [function]
    member_witness(lash::persistence::SessionReadView::policy);
    // FIG-2105-WITNESS-0314: lash::persistence::SessionReadView::session_id [function]
    member_witness(lash::persistence::SessionReadView::session_id);
    // FIG-2105-WITNESS-0315: lash::persistence::SessionReadView::to_snapshot [function]
    member_witness(lash::persistence::SessionReadView::to_snapshot);
    // FIG-2105-WITNESS-0316: lash::persistence::SessionRelation::Child [variant]
    variant_witness(|value: &lash::persistence::SessionRelation| {
        matches!(value, lash::persistence::SessionRelation::Child { .. })
    });
    // FIG-2105-WITNESS-0317: lash::persistence::SessionRelation::Child::caused_by [field]
    field_witness(|value: &lash::persistence::SessionRelation| {
        if let lash::persistence::SessionRelation::Child { caused_by, .. } = value {
            let _ = caused_by;
        }
    });
    // FIG-2105-WITNESS-0318: lash::persistence::SessionRelation::Child::parent_session_id [field]
    field_witness(|value: &lash::persistence::SessionRelation| {
        if let lash::persistence::SessionRelation::Child {
            parent_session_id, ..
        } = value
        {
            let _ = parent_session_id;
        }
    });
    // FIG-2105-WITNESS-0319: lash::persistence::SessionRelation::Fork [variant]
    variant_witness(|value: &lash::persistence::SessionRelation| {
        matches!(value, lash::persistence::SessionRelation::Fork { .. })
    });
    // FIG-2105-WITNESS-0320: lash::persistence::SessionRelation::Fork::source_node_id [field]
    field_witness(|value: &lash::persistence::SessionRelation| {
        if let lash::persistence::SessionRelation::Fork { source_node_id, .. } = value {
            let _ = source_node_id;
        }
    });
    // FIG-2105-WITNESS-0321: lash::persistence::SessionRelation::Fork::source_session_id [field]
    field_witness(|value: &lash::persistence::SessionRelation| {
        if let lash::persistence::SessionRelation::Fork {
            source_session_id, ..
        } = value
        {
            let _ = source_session_id;
        }
    });
    // FIG-2105-WITNESS-0322: lash::persistence::SessionRelation::parent_session_id [function]
    member_witness(lash::persistence::SessionRelation::parent_session_id);
    // FIG-2105-WITNESS-0323: lash::persistence::SessionStoreCreateRequest::parent_session_id [function]
    member_witness(lash::persistence::SessionStoreCreateRequest::parent_session_id);
    // FIG-2105-WITNESS-0324: lash::persistence::SessionStoreFactory::create_store [function]
    fn method_witness_0324<T: lash::persistence::SessionStoreFactory>() {
        member_witness(T::create_store);
    }
    // FIG-2105-WITNESS-0325: lash::persistence::SessionStoreFactory::delete_session [function]
    fn method_witness_0325<T: lash::persistence::SessionStoreFactory>() {
        member_witness(T::delete_session);
    }
    // FIG-2105-WITNESS-0326: lash::persistence::SessionStoreFactory::fork_at [function]
    fn method_witness_0326<T: lash::persistence::SessionStoreFactory>() {
        member_witness(T::fork_at);
    }
    // FIG-2105-WITNESS-0327: lash::persistence::SessionStoreFactory::fork_points [function]
    fn method_witness_0327<T: lash::persistence::SessionStoreFactory>() {
        member_witness(T::fork_points);
    }
    // FIG-2105-WITNESS-0328: lash::persistence::SessionStoreFactory::open_existing_store [function]
    fn method_witness_0328<T: lash::persistence::SessionStoreFactory>() {
        member_witness(T::open_existing_store);
    }
    // FIG-2105-WITNESS-0329: lash::persistence::SessionStoreFactory::read_session [function]
    fn method_witness_0329<T: lash::persistence::SessionStoreFactory>() {
        member_witness(T::read_session);
    }
    // FIG-2105-WITNESS-0330: lash::persistence::SessionStoreFactory::pin [function]
    fn method_witness_0330<T: lash::persistence::SessionStoreFactory>() {
        member_witness(T::pin);
    }
    // FIG-2105-WITNESS-0331: lash::persistence::SessionStoreFactory::session_was_deleted [function]
    fn method_witness_0331<T: lash::persistence::SessionStoreFactory>() {
        member_witness(T::session_was_deleted);
    }
    // FIG-2105-WITNESS-0332: lash::persistence::SessionStoreFactory::unpin [function]
    fn method_witness_0332<T: lash::persistence::SessionStoreFactory>() {
        member_witness(T::unpin);
    }
    // FIG-2105-WITNESS-0333: lash::persistence::StoreError::AppendAncestorNotActive [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::AppendAncestorNotActive { .. }
        )
    });
    // FIG-2105-WITNESS-0334: lash::persistence::StoreError::AppendAncestorNotActive::required_node_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::AppendAncestorNotActive {
            required_node_id, ..
        } = value
        {
            let _ = required_node_id;
        }
    });
    // FIG-2105-WITNESS-0335: lash::persistence::StoreError::AppendOperationIdentityConflict [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::AppendOperationIdentityConflict { .. }
        )
    });
    // FIG-2105-WITNESS-0336: lash::persistence::StoreError::AppendOperationIdentityConflict::operation_key [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::AppendOperationIdentityConflict {
            operation_key,
            ..
        } = value
        {
            let _ = operation_key;
        }
    });
    // FIG-2105-WITNESS-0337: lash::persistence::StoreError::AppendOperationIdentityConflict::session_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::AppendOperationIdentityConflict {
            session_id, ..
        } = value
        {
            let _ = session_id;
        }
    });
    // FIG-2105-WITNESS-0338: lash::persistence::StoreError::AppendReceiptRequestedNodeCountCorrupt [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::AppendReceiptRequestedNodeCountCorrupt { .. }
        )
    });
    // FIG-2105-WITNESS-0339: lash::persistence::StoreError::AppendReceiptRequestedNodeCountCorrupt::attempted [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::AppendReceiptRequestedNodeCountCorrupt {
            attempted,
            ..
        } = value
        {
            let _ = attempted;
        }
    });
    // FIG-2105-WITNESS-0340: lash::persistence::StoreError::AppendReceiptRequestedNodeCountCorrupt::operation_key [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::AppendReceiptRequestedNodeCountCorrupt {
            operation_key,
            ..
        } = value
        {
            let _ = operation_key;
        }
    });
    // FIG-2105-WITNESS-0341: lash::persistence::StoreError::AppendReceiptRequestedNodeCountCorrupt::session_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::AppendReceiptRequestedNodeCountCorrupt {
            session_id,
            ..
        } = value
        {
            let _ = session_id;
        }
    });
    // FIG-2105-WITNESS-0342: lash::persistence::StoreError::AppendReceiptRequestedNodeCountCorrupt::stored [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::AppendReceiptRequestedNodeCountCorrupt {
            stored,
            ..
        } = value
        {
            let _ = stored;
        }
    });
    // FIG-2105-WITNESS-0343: lash::persistence::StoreError::Backend::0 [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::Backend(field, ..) = value {
            let _ = field;
        }
    });
    // FIG-2105-WITNESS-0344: lash::persistence::StoreError::CheckpointComponentMissing [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::CheckpointComponentMissing { .. }
        )
    });
    // FIG-2105-WITNESS-0345: lash::persistence::StoreError::CheckpointComponentMissing::key [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::CheckpointComponentMissing { key, .. } = value {
            let _ = key;
        }
    });
    // FIG-2105-WITNESS-0346: lash::persistence::StoreError::CheckpointRootMissing [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::CheckpointRootMissing { .. }
        )
    });
    // FIG-2105-WITNESS-0347: lash::persistence::StoreError::CheckpointRootMissing::blob_ref [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::CheckpointRootMissing { blob_ref, .. } = value {
            let _ = blob_ref;
        }
    });
    // FIG-2105-WITNESS-0348: lash::persistence::StoreError::CheckpointComponentEncodingVersionMismatch [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::CheckpointComponentEncodingVersionMismatch { .. }
        )
    });
    // FIG-2105-WITNESS-0349: lash::persistence::StoreError::CheckpointComponentEncodingVersionMismatch::actual [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::CheckpointComponentEncodingVersionMismatch {
            actual,
            ..
        } = value
        {
            let _ = actual;
        }
    });
    // FIG-2105-WITNESS-0350: lash::persistence::StoreError::CheckpointComponentEncodingVersionMismatch::expected [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::CheckpointComponentEncodingVersionMismatch {
            expected,
            ..
        } = value
        {
            let _ = expected;
        }
    });
    // FIG-2105-WITNESS-0351: lash::persistence::StoreError::CheckpointComponentEncodingVersionMismatch::key [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::CheckpointComponentEncodingVersionMismatch {
            key,
            ..
        } = value
        {
            let _ = key;
        }
    });
    // FIG-2105-WITNESS-0352: lash::persistence::StoreError::RecordEncodingFailed [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::RecordEncodingFailed { .. }
        )
    });
    // FIG-2105-WITNESS-0353: lash::persistence::StoreError::RecordEncodingFailed::message [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::RecordEncodingFailed { message, .. } = value {
            let _ = message;
        }
    });
    // FIG-2105-WITNESS-0354: lash::persistence::StoreError::RecordEncodingFailed::record_kind [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::RecordEncodingFailed { record_kind, .. } = value {
            let _ = record_kind;
        }
    });
    // FIG-2105-WITNESS-0355: lash::persistence::StoreError::CheckpointTokenUsageOutOfRange [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::CheckpointTokenUsageOutOfRange { .. }
        )
    });
    // FIG-2105-WITNESS-0356: lash::persistence::StoreError::CheckpointTokenUsageOutOfRange::counter [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::CheckpointTokenUsageOutOfRange { counter, .. } = value
        {
            let _ = counter;
        }
    });
    // FIG-2105-WITNESS-0357: lash::persistence::StoreError::CheckpointTurnIndexOutOfRange [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::CheckpointTurnIndexOutOfRange { .. }
        )
    });
    // FIG-2105-WITNESS-0358: lash::persistence::StoreError::CheckpointTurnIndexOutOfRange::max_exclusive [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::CheckpointTurnIndexOutOfRange {
            max_exclusive, ..
        } = value
        {
            let _ = max_exclusive;
        }
    });
    // FIG-2105-WITNESS-0359: lash::persistence::StoreError::CheckpointTurnIndexOutOfRange::turn_index [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::CheckpointTurnIndexOutOfRange { turn_index, .. } =
            value
        {
            let _ = turn_index;
        }
    });
    // FIG-2105-WITNESS-0360: lash::persistence::StoreError::ClaimSettlementCountMismatch [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::ClaimSettlementCountMismatch { .. }
        )
    });
    // FIG-2105-WITNESS-0361: lash::persistence::StoreError::ClaimSettlementCountMismatch::claim_kind [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::ClaimSettlementCountMismatch { claim_kind, .. } =
            value
        {
            let _ = claim_kind;
        }
    });
    // FIG-2105-WITNESS-0362: lash::persistence::StoreError::ClaimSettlementCountMismatch::completed_count [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::ClaimSettlementCountMismatch {
            completed_count, ..
        } = value
        {
            let _ = completed_count;
        }
    });
    // FIG-2105-WITNESS-0363: lash::persistence::StoreError::ClaimSettlementCountMismatch::originating_count [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::ClaimSettlementCountMismatch {
            originating_count,
            ..
        } = value
        {
            let _ = originating_count;
        }
    });
    // FIG-2105-WITNESS-0364: lash::persistence::StoreError::CommitByteBudgetExceeded [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::CommitByteBudgetExceeded { .. }
        )
    });
    // FIG-2105-WITNESS-0365: lash::persistence::StoreError::CommitByteBudgetExceeded::checkpoint_bytes [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::CommitByteBudgetExceeded {
            checkpoint_bytes, ..
        } = value
        {
            let _ = checkpoint_bytes;
        }
    });
    // FIG-2105-WITNESS-0366: lash::persistence::StoreError::CommitByteBudgetExceeded::graph_delta_bytes [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::CommitByteBudgetExceeded {
            graph_delta_bytes, ..
        } = value
        {
            let _ = graph_delta_bytes;
        }
    });
    // FIG-2105-WITNESS-0367: lash::persistence::StoreError::CommitByteBudgetExceeded::session_config_bytes [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::CommitByteBudgetExceeded {
            session_config_bytes,
            ..
        } = value
        {
            let _ = session_config_bytes;
        }
    });
    // FIG-2105-WITNESS-0368: lash::persistence::StoreError::CommitByteBudgetExceeded::max_bytes [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::CommitByteBudgetExceeded { max_bytes, .. } = value {
            let _ = max_bytes;
        }
    });
    // FIG-2105-WITNESS-0369: lash::persistence::StoreError::CommitByteBudgetExceeded::total_bytes [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::CommitByteBudgetExceeded { total_bytes, .. } = value {
            let _ = total_bytes;
        }
    });
    // FIG-2105-WITNESS-0370: lash::persistence::StoreError::CommitNodeBudgetExceeded [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::CommitNodeBudgetExceeded { .. }
        )
    });
    // FIG-2105-WITNESS-0371: lash::persistence::StoreError::CommitNodeBudgetExceeded::max_nodes [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::CommitNodeBudgetExceeded { max_nodes, .. } = value {
            let _ = max_nodes;
        }
    });
    // FIG-2105-WITNESS-0372: lash::persistence::StoreError::CommitNodeBudgetExceeded::node_count [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::CommitNodeBudgetExceeded { node_count, .. } = value {
            let _ = node_count;
        }
    });
    // FIG-2105-WITNESS-0373: lash::persistence::StoreError::Contended [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(value, lash::persistence::StoreError::Contended)
    });
    // FIG-2105-WITNESS-0374: lash::persistence::StoreError::CurrentFrameNodeMismatch [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::CurrentFrameNodeMismatch { .. }
        )
    });
    // FIG-2105-WITNESS-0375: lash::persistence::StoreError::CurrentFrameNodeMismatch::claimed [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::CurrentFrameNodeMismatch { claimed, .. } = value {
            let _ = claimed;
        }
    });
    // FIG-2105-WITNESS-0376: lash::persistence::StoreError::CurrentFrameNodeMismatch::derived [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::CurrentFrameNodeMismatch { derived, .. } = value {
            let _ = derived;
        }
    });
    // FIG-2105-WITNESS-0377: lash::persistence::StoreError::ExecutionStateCaptureFailed [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::ExecutionStateCaptureFailed { .. }
        )
    });
    // FIG-2105-WITNESS-0378: lash::persistence::StoreError::ExecutionStateCaptureFailed::message [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::ExecutionStateCaptureFailed { message, .. } = value {
            let _ = message;
        }
    });
    // FIG-2105-WITNESS-0379: lash::persistence::StoreError::ForeignQueuedWorkCompletion [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::ForeignQueuedWorkCompletion { .. }
        )
    });
    // FIG-2105-WITNESS-0380: lash::persistence::StoreError::ForeignQueuedWorkCompletion::claim_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::ForeignQueuedWorkCompletion { claim_id, .. } = value {
            let _ = claim_id;
        }
    });
    // FIG-2105-WITNESS-0381: lash::persistence::StoreError::ForeignQueuedWorkCompletion::session_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::ForeignQueuedWorkCompletion { session_id, .. } = value
        {
            let _ = session_id;
        }
    });
    // FIG-2105-WITNESS-0382: lash::persistence::StoreError::ForeignTurnInputCompletion [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::ForeignTurnInputCompletion { .. }
        )
    });
    // FIG-2105-WITNESS-0383: lash::persistence::StoreError::ForeignTurnInputCompletion::claim_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::ForeignTurnInputCompletion { claim_id, .. } = value {
            let _ = claim_id;
        }
    });
    // FIG-2105-WITNESS-0384: lash::persistence::StoreError::ForeignTurnInputCompletion::session_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::ForeignTurnInputCompletion { session_id, .. } = value
        {
            let _ = session_id;
        }
    });
    // FIG-2105-WITNESS-0385: lash::persistence::StoreError::ForkPointNotRetained::node_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::ForkPointNotRetained { node_id, .. } = value {
            let _ = node_id;
        }
    });
    // FIG-2105-WITNESS-0386: lash::persistence::StoreError::ForkSessionAlreadyExists [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::ForkSessionAlreadyExists { .. }
        )
    });
    // FIG-2105-WITNESS-0387: lash::persistence::StoreError::ForkSessionAlreadyExists::session_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::ForkSessionAlreadyExists { session_id, .. } = value {
            let _ = session_id;
        }
    });
    // FIG-2105-WITNESS-0388: lash::persistence::StoreError::HeadRevisionConflict [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::HeadRevisionConflict { .. }
        )
    });
    // FIG-2105-WITNESS-0389: lash::persistence::StoreError::HeadRevisionConflict::actual [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::HeadRevisionConflict { actual, .. } = value {
            let _ = actual;
        }
    });
    // FIG-2105-WITNESS-0390: lash::persistence::StoreError::HeadRevisionConflict::expected [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::HeadRevisionConflict { expected, .. } = value {
            let _ = expected;
        }
    });
    // FIG-2105-WITNESS-0391: lash::persistence::StoreError::IncompleteCheckpointComponentSet [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::IncompleteCheckpointComponentSet
        )
    });
    // FIG-2105-WITNESS-0392: lash::persistence::StoreError::InvalidGraphLeaf [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::InvalidGraphLeaf { .. }
        )
    });
    // FIG-2105-WITNESS-0393: lash::persistence::StoreError::InvalidGraphLeaf::leaf_node_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::InvalidGraphLeaf { leaf_node_id, .. } = value {
            let _ = leaf_node_id;
        }
    });
    // FIG-2105-WITNESS-0394: lash::persistence::StoreError::InvalidGraphParent [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::InvalidGraphParent { .. }
        )
    });
    // FIG-2105-WITNESS-0395: lash::persistence::StoreError::InvalidGraphParent::actual [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::InvalidGraphParent { actual, .. } = value {
            let _ = actual;
        }
    });
    // FIG-2105-WITNESS-0396: lash::persistence::StoreError::InvalidGraphParent::expected [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::InvalidGraphParent { expected, .. } = value {
            let _ = expected;
        }
    });
    // FIG-2105-WITNESS-0397: lash::persistence::StoreError::InvalidGraphParent::node_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::InvalidGraphParent { node_id, .. } = value {
            let _ = node_id;
        }
    });
    // FIG-2105-WITNESS-0398: lash::persistence::StoreError::InvalidSessionId [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::InvalidSessionId { .. }
        )
    });
    // FIG-2105-WITNESS-0399: lash::persistence::StoreError::InvalidSessionId::reason [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::InvalidSessionId { reason, .. } = value {
            let _ = reason;
        }
    });
    // FIG-2105-WITNESS-0400: lash::persistence::StoreError::MissingFrameOpenAncestor [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::MissingFrameOpenAncestor { .. }
        )
    });
    // FIG-2105-WITNESS-0401: lash::persistence::StoreError::MissingFrameOpenAncestor::leaf_node_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::MissingFrameOpenAncestor { leaf_node_id, .. } = value
        {
            let _ = leaf_node_id;
        }
    });
    // FIG-2105-WITNESS-0402: lash::persistence::StoreError::GraphGenerationCollision [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::GraphGenerationCollision { .. }
        )
    });
    // FIG-2105-WITNESS-0403: lash::persistence::StoreError::GraphGenerationCollision::generation [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::GraphGenerationCollision { generation, .. } = value {
            let _ = generation;
        }
    });
    // FIG-2105-WITNESS-0404: lash::persistence::StoreError::GraphGenerationCollision::session_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::GraphGenerationCollision { session_id, .. } = value {
            let _ = session_id;
        }
    });
    // FIG-2105-WITNESS-0405: lash::persistence::StoreError::MonotonicCounterOverflow [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::MonotonicCounterOverflow { .. }
        )
    });
    // FIG-2105-WITNESS-0406: lash::persistence::StoreError::MonotonicCounterOverflow::counter [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::MonotonicCounterOverflow { counter, .. } = value {
            let _ = counter;
        }
    });
    // FIG-2105-WITNESS-0407: lash::persistence::StoreError::MonotonicCounterOverflow::current [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::MonotonicCounterOverflow { current, .. } = value {
            let _ = current;
        }
    });
    // FIG-2105-WITNESS-0408: lash::persistence::StoreError::NodeIdCollision [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(value, lash::persistence::StoreError::NodeIdCollision { .. })
    });
    // FIG-2105-WITNESS-0409: lash::persistence::StoreError::NodeIdCollision::node_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::NodeIdCollision { node_id, .. } = value {
            let _ = node_id;
        }
    });
    // FIG-2105-WITNESS-0410: lash::persistence::StoreError::NodeIdDerivationMismatch [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::NodeIdDerivationMismatch { .. }
        )
    });
    // FIG-2105-WITNESS-0411: lash::persistence::StoreError::NodeIdDerivationMismatch::expected_node_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::NodeIdDerivationMismatch {
            expected_node_id, ..
        } = value
        {
            let _ = expected_node_id;
        }
    });
    // FIG-2105-WITNESS-0412: lash::persistence::StoreError::NodeIdDerivationMismatch::node_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::NodeIdDerivationMismatch { node_id, .. } = value {
            let _ = node_id;
        }
    });
    // FIG-2105-WITNESS-0413: lash::persistence::StoreError::PendingTurnInputSourceKeyConflict [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::PendingTurnInputSourceKeyConflict { .. }
        )
    });
    // FIG-2105-WITNESS-0414: lash::persistence::StoreError::PendingTurnInputSourceKeyConflict::existing_input_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::PendingTurnInputSourceKeyConflict {
            existing_input_id,
            ..
        } = value
        {
            let _ = existing_input_id;
        }
    });
    // FIG-2105-WITNESS-0415: lash::persistence::StoreError::PendingTurnInputSourceKeyConflict::session_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::PendingTurnInputSourceKeyConflict {
            session_id, ..
        } = value
        {
            let _ = session_id;
        }
    });
    // FIG-2105-WITNESS-0416: lash::persistence::StoreError::PendingTurnInputSourceKeyConflict::source_key [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::PendingTurnInputSourceKeyConflict {
            source_key, ..
        } = value
        {
            let _ = source_key;
        }
    });
    // FIG-2105-WITNESS-0417: lash::persistence::StoreError::QueuedWorkClaimSuperseded [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::QueuedWorkClaimSuperseded { .. }
        )
    });
    // FIG-2105-WITNESS-0418: lash::persistence::StoreError::QueuedWorkClaimSuperseded::claim_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::QueuedWorkClaimSuperseded { claim_id, .. } = value {
            let _ = claim_id;
        }
    });
    // FIG-2105-WITNESS-0419: lash::persistence::StoreError::QueuedWorkClaimSuperseded::row_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::QueuedWorkClaimSuperseded { row_id, .. } = value {
            let _ = row_id;
        }
    });
    // FIG-2105-WITNESS-0420: lash::persistence::StoreError::QueuedWorkClaimSuperseded::session_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::QueuedWorkClaimSuperseded { session_id, .. } = value {
            let _ = session_id;
        }
    });
    // FIG-2105-WITNESS-0421: lash::persistence::StoreError::QueuedWorkClaimSuperseded::superseding_claim_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::QueuedWorkClaimSuperseded {
            superseding_claim_id,
            ..
        } = value
        {
            let _ = superseding_claim_id;
        }
    });
    // FIG-2105-WITNESS-0422: lash::persistence::StoreError::QueuedWorkClaimSuperseded::superseding_session_lease_generation [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::QueuedWorkClaimSuperseded {
            superseding_session_lease_generation,
            ..
        } = value
        {
            let _ = superseding_session_lease_generation;
        }
    });
    // FIG-2105-WITNESS-0423: lash::persistence::StoreError::RuntimeCommitLeaseAuthorityConflict [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::RuntimeCommitLeaseAuthorityConflict { .. }
        )
    });
    // FIG-2105-WITNESS-0424: lash::persistence::StoreError::RuntimeCommitLeaseAuthorityConflict::session_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::RuntimeCommitLeaseAuthorityConflict {
            session_id,
            ..
        } = value
        {
            let _ = session_id;
        }
    });
    // FIG-2105-WITNESS-0425: lash::persistence::StoreError::RuntimeTurnCommitConflict [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::RuntimeTurnCommitConflict { .. }
        )
    });
    // FIG-2105-WITNESS-0426: lash::persistence::StoreError::RuntimeTurnCommitConflict::session_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::RuntimeTurnCommitConflict { session_id, .. } = value {
            let _ = session_id;
        }
    });
    // FIG-2105-WITNESS-0427: lash::persistence::StoreError::RuntimeTurnCommitConflict::turn_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::RuntimeTurnCommitConflict { turn_id, .. } = value {
            let _ = turn_id;
        }
    });
    // FIG-2105-WITNESS-0428: lash::persistence::StoreError::SessionBindingMismatch [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::SessionBindingMismatch { .. }
        )
    });
    // FIG-2105-WITNESS-0429: lash::persistence::StoreError::SessionBindingMismatch::attempted_session_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::SessionBindingMismatch {
            attempted_session_id,
            ..
        } = value
        {
            let _ = attempted_session_id;
        }
    });
    // FIG-2105-WITNESS-0430: lash::persistence::StoreError::SessionBindingMismatch::bound_session_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::SessionBindingMismatch {
            bound_session_id, ..
        } = value
        {
            let _ = bound_session_id;
        }
    });
    // FIG-2105-WITNESS-0431: lash::persistence::StoreError::SessionBindingNotMaterialized [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::SessionBindingNotMaterialized { .. }
        )
    });
    // FIG-2105-WITNESS-0432: lash::persistence::StoreError::SessionBindingNotMaterialized::session_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::SessionBindingNotMaterialized { session_id, .. } =
            value
        {
            let _ = session_id;
        }
    });
    // FIG-2105-WITNESS-0433: lash::persistence::StoreError::SessionExecutionLeaseExpired [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::SessionExecutionLeaseExpired { .. }
        )
    });
    // FIG-2105-WITNESS-0434: lash::persistence::StoreError::SessionExecutionLeaseExpired::session_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::SessionExecutionLeaseExpired { session_id, .. } =
            value
        {
            let _ = session_id;
        }
    });
    // FIG-2105-WITNESS-0435: lash::persistence::StoreError::SessionExecutionLeaseReleaseRefused [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::SessionExecutionLeaseReleaseRefused { .. }
        )
    });
    // FIG-2105-WITNESS-0436: lash::persistence::StoreError::SessionExecutionLeaseReleaseRefused::session_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::SessionExecutionLeaseReleaseRefused {
            session_id,
            ..
        } = value
        {
            let _ = session_id;
        }
    });
    // FIG-2105-WITNESS-0437: lash::persistence::StoreError::SessionExecutionLeaseRenewalInstallRefused [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::SessionExecutionLeaseRenewalInstallRefused { .. }
        )
    });
    // FIG-2105-WITNESS-0438: lash::persistence::StoreError::SessionExecutionLeaseRenewalInstallRefused::mismatch [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::SessionExecutionLeaseRenewalInstallRefused {
            mismatch,
            ..
        } = value
        {
            let _ = mismatch;
        }
    });
    // FIG-2105-WITNESS-0439: lash::persistence::StoreError::SessionExecutionLeaseRenewalInstallRefused::session_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::SessionExecutionLeaseRenewalInstallRefused {
            session_id,
            ..
        } = value
        {
            let _ = session_id;
        }
    });
    // FIG-2105-WITNESS-0440: lash::persistence::StoreError::SessionExecutionLeaseRenewalRefused [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::SessionExecutionLeaseRenewalRefused { .. }
        )
    });
    // FIG-2105-WITNESS-0441: lash::persistence::StoreError::SessionExecutionLeaseRenewalRefused::session_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::SessionExecutionLeaseRenewalRefused {
            session_id,
            ..
        } = value
        {
            let _ = session_id;
        }
    });
    // FIG-2105-WITNESS-0442: lash::persistence::StoreError::SessionNotBound [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(value, lash::persistence::StoreError::SessionNotBound)
    });
    // FIG-2105-WITNESS-0443: lash::persistence::StoreError::SessionResolutionAmbiguous [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::SessionResolutionAmbiguous { .. }
        )
    });
    // FIG-2105-WITNESS-0444: lash::persistence::StoreError::SessionResolutionAmbiguous::session_count [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::SessionResolutionAmbiguous { session_count, .. } =
            value
        {
            let _ = session_count;
        }
    });
    // FIG-2105-WITNESS-0445: lash::persistence::StoreError::StorageFailure [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(value, lash::persistence::StoreError::StorageFailure { .. })
    });
    // FIG-2105-WITNESS-0446: lash::persistence::StoreError::StorageFailure::backend [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::StorageFailure { backend, .. } = value {
            let _ = backend;
        }
    });
    // FIG-2105-WITNESS-0447: lash::persistence::StoreError::StorageFailure::message [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::StorageFailure { message, .. } = value {
            let _ = message;
        }
    });
    // FIG-2105-WITNESS-0448: lash::persistence::StoreError::StoredDataCorrupt [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::StoredDataCorrupt { .. }
        )
    });
    // FIG-2105-WITNESS-0449: lash::persistence::StoreError::StoredDataCorrupt::message [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::StoredDataCorrupt { message, .. } = value {
            let _ = message;
        }
    });
    // FIG-2105-WITNESS-0450: lash::persistence::StoreError::StoredDataCorrupt::record_kind [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::StoredDataCorrupt { record_kind, .. } = value {
            let _ = record_kind;
        }
    });
    // FIG-2105-WITNESS-0451: lash::persistence::StoreError::TokenUsageAccountingOverflow [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::TokenUsageAccountingOverflow { .. }
        )
    });
    // FIG-2105-WITNESS-0452: lash::persistence::StoreError::TokenUsageAccountingOverflow::counter [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::TokenUsageAccountingOverflow { counter, .. } = value {
            let _ = counter;
        }
    });
    // FIG-2105-WITNESS-0453: lash::persistence::StoreError::TokenUsageAccountingOverflow::model [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::TokenUsageAccountingOverflow { model, .. } = value {
            let _ = model;
        }
    });
    // FIG-2105-WITNESS-0454: lash::persistence::StoreError::TokenUsageAccountingOverflow::usage_source [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::TokenUsageAccountingOverflow {
            usage_source, ..
        } = value
        {
            let _ = usage_source;
        }
    });
    // FIG-2105-WITNESS-0455: lash::persistence::StoreError::TurnInputClaimSuperseded [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::TurnInputClaimSuperseded { .. }
        )
    });
    // FIG-2105-WITNESS-0456: lash::persistence::StoreError::TurnInputClaimSuperseded::claim_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::TurnInputClaimSuperseded { claim_id, .. } = value {
            let _ = claim_id;
        }
    });
    // FIG-2105-WITNESS-0457: lash::persistence::StoreError::TurnInputClaimSuperseded::row_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::TurnInputClaimSuperseded { row_id, .. } = value {
            let _ = row_id;
        }
    });
    // FIG-2105-WITNESS-0458: lash::persistence::StoreError::TurnInputClaimSuperseded::session_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::TurnInputClaimSuperseded { session_id, .. } = value {
            let _ = session_id;
        }
    });
    // FIG-2105-WITNESS-0459: lash::persistence::StoreError::TurnInputClaimSuperseded::superseding_claim_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::TurnInputClaimSuperseded {
            superseding_claim_id,
            ..
        } = value
        {
            let _ = superseding_claim_id;
        }
    });
    // FIG-2105-WITNESS-0460: lash::persistence::StoreError::TurnInputClaimSuperseded::superseding_session_lease_generation [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::TurnInputClaimSuperseded {
            superseding_session_lease_generation,
            ..
        } = value
        {
            let _ = superseding_session_lease_generation;
        }
    });
    // FIG-2105-WITNESS-0461: lash::persistence::StoreError::UnsettledQueuedWorkClaim [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::UnsettledQueuedWorkClaim { .. }
        )
    });
    // FIG-2105-WITNESS-0462: lash::persistence::StoreError::UnsettledQueuedWorkClaim::claim_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::UnsettledQueuedWorkClaim { claim_id, .. } = value {
            let _ = claim_id;
        }
    });
    // FIG-2105-WITNESS-0463: lash::persistence::StoreError::UnsettledQueuedWorkClaim::session_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::UnsettledQueuedWorkClaim { session_id, .. } = value {
            let _ = session_id;
        }
    });
    // FIG-2105-WITNESS-0464: lash::persistence::StoreError::UnclaimedTurnInputSettlementSuperseded [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::UnclaimedTurnInputSettlementSuperseded { .. }
        )
    });
    // FIG-2105-WITNESS-0465: lash::persistence::StoreError::UnclaimedTurnInputSettlementSuperseded::session_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::UnclaimedTurnInputSettlementSuperseded {
            session_id,
            ..
        } = value
        {
            let _ = session_id;
        }
    });
    // FIG-2105-WITNESS-0466: lash::persistence::StoreError::UnclaimedTurnInputSettlementSuperseded::input_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::UnclaimedTurnInputSettlementSuperseded {
            input_id,
            ..
        } = value
        {
            let _ = input_id;
        }
    });
    // FIG-2105-WITNESS-0467: lash::persistence::StoreError::UnclaimedTurnInputSettlementSuperseded::observed_state [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::UnclaimedTurnInputSettlementSuperseded {
            observed_state,
            ..
        } = value
        {
            let _ = observed_state;
        }
    });
    // FIG-2105-WITNESS-0468: lash::persistence::StoreError::UnclaimedTurnInputSettlementSuperseded::superseding_claim_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::UnclaimedTurnInputSettlementSuperseded {
            superseding_claim_id,
            ..
        } = value
        {
            let _ = superseding_claim_id;
        }
    });
    // FIG-2105-WITNESS-0469: lash::persistence::StoreError::UnsettledTurnInputClaim [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::UnsettledTurnInputClaim { .. }
        )
    });
    // FIG-2105-WITNESS-0470: lash::persistence::StoreError::UnsettledTurnInputClaim::claim_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::UnsettledTurnInputClaim { claim_id, .. } = value {
            let _ = claim_id;
        }
    });
    // FIG-2105-WITNESS-0471: lash::persistence::StoreError::UnsettledTurnInputClaim::session_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::UnsettledTurnInputClaim { session_id, .. } = value {
            let _ = session_id;
        }
    });
    // FIG-2105-WITNESS-0472: lash::persistence::StoreError::UnstagedUsageConfirmation [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::UnstagedUsageConfirmation { .. }
        )
    });
    // FIG-2105-WITNESS-0473: lash::persistence::StoreError::UnstagedUsageConfirmation::confirmed_count [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::UnstagedUsageConfirmation {
            confirmed_count, ..
        } = value
        {
            let _ = confirmed_count;
        }
    });
    // FIG-2105-WITNESS-0474: lash::persistence::StoreError::UnstagedUsageConfirmation::staged_count [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::UnstagedUsageConfirmation { staged_count, .. } = value
        {
            let _ = staged_count;
        }
    });
    // FIG-2105-WITNESS-0475: lash::persistence::StoreError::UnsupportedStoreOperation [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::UnsupportedStoreOperation { .. }
        )
    });
    // FIG-2105-WITNESS-0476: lash::persistence::StoreError::UnsupportedStoreOperation::operation [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::UnsupportedStoreOperation { operation, .. } = value {
            let _ = operation;
        }
    });
    // FIG-2105-WITNESS-0477: lash::persistence::StoreError::variant_name [function]
    member_witness(lash::persistence::StoreError::variant_name);
    // FIG-2105-WITNESS-0478: lash::persistence::TurnId [struct]
    type_witness::<lash::persistence::TurnId>();
    // FIG-2105-WITNESS-0479: lash::persistence::TurnId::as_str [function]
    member_witness(lash::persistence::TurnId::as_str);
    // FIG-2105-WITNESS-0480: lash::persistence::TurnId::into_inner [function]
    member_witness(lash::persistence::TurnId::into_inner);
}
