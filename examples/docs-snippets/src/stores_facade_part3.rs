//! Facade-only compile-time witnesses for durable-store host contracts.
//!
//! These probes type-check public contracts without constructing a live backend.

#![allow(dead_code, unreachable_code, unused_variables)]

fn type_witness<T>() {}
fn member_witness<T>(_: T) {}
fn field_witness<T>(_: impl FnOnce(&T)) {}
fn variant_witness<T>(_: impl FnOnce(&T) -> bool) {}

pub(crate) fn store_area_facade_witnesses() {
    // FIG-2105-WITNESS-0481: lash::persistence::TurnInputCheckpointBoundary::BeforeCompletion [variant]
    variant_witness(|value: &lash::persistence::TurnInputCheckpointBoundary| {
        matches!(
            value,
            lash::persistence::TurnInputCheckpointBoundary::BeforeCompletion
        )
    });
    // FIG-2105-WITNESS-0482: lash::persistence::TurnInputCheckpointBoundary::admits [function]
    member_witness(lash::persistence::TurnInputCheckpointBoundary::admits);
    // FIG-2105-WITNESS-0483: lash::persistence::TurnInputClaim [type_alias]
    type_witness::<lash::persistence::TurnInputClaim>();
    // FIG-2105-WITNESS-0484: lash::persistence::TurnInputClaimData [struct]
    type_witness::<lash::persistence::TurnInputClaimData>();
    // FIG-2105-WITNESS-0485: lash::persistence::TurnInputCompletion [struct]
    type_witness::<lash::persistence::TurnInputCompletion>();
    // FIG-2105-WITNESS-0486: lash::persistence::TurnInputCompletion::session_id [field]
    field_witness(|value: &lash::persistence::TurnInputCompletion| {
        let _ = &value.session_id;
    });
    // FIG-2105-WITNESS-0487: lash::persistence::TurnInputCompletion::claim [field]
    field_witness(|value: &lash::persistence::TurnInputCompletion| {
        let _ = &value.claim;
    });
    // FIG-2105-WITNESS-0488: lash::persistence::TurnInputCompletion::data [field]
    field_witness(|value: &lash::persistence::TurnInputCompletion| {
        let _ = &value.data;
    });
    // FIG-2105-WITNESS-0489: lash::persistence::TurnInputCompletion::claim_id [function]
    member_witness(lash::persistence::TurnInputCompletion::claim_id);
    // FIG-2105-WITNESS-0490: lash::persistence::TurnInputCompletion::lease_token [function]
    member_witness(lash::persistence::TurnInputCompletion::lease_token);
    // FIG-2105-WITNESS-0491: lash::persistence::TurnInputCompletion::settlement_identity [function]
    member_witness(lash::persistence::TurnInputCompletion::settlement_identity);
    // FIG-2105-WITNESS-0492: lash::persistence::TurnInputSettlementClaim [struct]
    type_witness::<lash::persistence::TurnInputSettlementClaim>();
    // FIG-2105-WITNESS-0493: lash::persistence::TurnInputSettlementClaim::claim_id [field]
    field_witness(|value: &lash::persistence::TurnInputSettlementClaim| {
        let _ = &value.claim_id;
    });
    // FIG-2105-WITNESS-0494: lash::persistence::TurnInputSettlementClaim::lease_token [field]
    field_witness(|value: &lash::persistence::TurnInputSettlementClaim| {
        let _ = &value.lease_token;
    });
    // FIG-2105-WITNESS-0495: lash::persistence::UnclaimedTurnInputs [struct]
    type_witness::<lash::persistence::UnclaimedTurnInputs>();
    // FIG-2105-WITNESS-0496: lash::persistence::UnclaimedTurnInputs::session_id [field]
    field_witness(|value: &lash::persistence::UnclaimedTurnInputs| {
        let _ = &value.session_id;
    });
    // FIG-2105-WITNESS-0497: lash::persistence::UnclaimedTurnInputs::inputs [field]
    field_witness(|value: &lash::persistence::UnclaimedTurnInputs| {
        let _ = &value.inputs;
    });
    // FIG-2105-WITNESS-0498: lash::persistence::UnclaimedTurnInputs::applications [field]
    field_witness(|value: &lash::persistence::UnclaimedTurnInputs| {
        let _ = &value.applications;
    });
    // FIG-2105-WITNESS-0499: lash::persistence::UnclaimedTurnInputs::completion [function]
    member_witness(lash::persistence::UnclaimedTurnInputs::completion);
    // FIG-2105-WITNESS-0500: lash::persistence::UnclaimedTurnInputs::record_initial_turn_application [function]
    member_witness(lash::persistence::UnclaimedTurnInputs::record_initial_turn_application);
    // FIG-2105-WITNESS-0501: lash::persistence::TurnInputCompletionData [struct]
    type_witness::<lash::persistence::TurnInputCompletionData>();
    // FIG-2105-WITNESS-0502: lash::persistence::TurnInputCompletionData::applications [field]
    field_witness(|value: &lash::persistence::TurnInputCompletionData| {
        let _ = &value.applications;
    });
    // FIG-2105-WITNESS-0503: lash::persistence::TurnInputCompletionData::input_ids [field]
    field_witness(|value: &lash::persistence::TurnInputCompletionData| {
        let _ = &value.input_ids;
    });
    // FIG-2105-WITNESS-0504: lash::persistence::TurnInputIngress::admits_checkpoint [function]
    member_witness(lash::persistence::TurnInputIngress::admits_checkpoint);
    // FIG-2105-WITNESS-0505: lash::persistence::TurnInputStore [trait]
    fn trait_witness_0505<T: lash::persistence::TurnInputStore>() {}
    // FIG-2105-WITNESS-0506: lash::persistence::TurnInputStore::abandon_turn_input_claim [function]
    fn method_witness_0506<T: lash::persistence::TurnInputStore>() {
        member_witness(T::abandon_turn_input_claim);
    }
    // FIG-2105-WITNESS-0507: lash::persistence::TurnInputStore::abandon_turn_input_claims [function]
    fn method_witness_0507<T: lash::persistence::TurnInputStore>() {
        member_witness(T::abandon_turn_input_claims);
    }
    // FIG-2105-WITNESS-0508: lash::persistence::TurnInputStore::cancel_pending_turn_input [function]
    fn method_witness_0508<T: lash::persistence::TurnInputStore>() {
        member_witness(T::cancel_pending_turn_input);
    }
    // FIG-2105-WITNESS-0509: lash::persistence::TurnInputStore::cancel_pending_turn_input_suffix [function]
    fn method_witness_0509<T: lash::persistence::TurnInputStore>() {
        member_witness(T::cancel_pending_turn_input_suffix);
    }
    // FIG-2105-WITNESS-0510: lash::persistence::TurnInputStore::cancel_pending_turn_inputs [function]
    fn method_witness_0510<T: lash::persistence::TurnInputStore>() {
        member_witness(T::cancel_pending_turn_inputs);
    }
    // FIG-2105-WITNESS-0511: lash::persistence::TurnInputStore::claim_active_turn_inputs [function]
    fn method_witness_0511<T: lash::persistence::TurnInputStore>() {
        member_witness(T::claim_active_turn_inputs);
    }
    // FIG-2105-WITNESS-0512: lash::persistence::TurnInputStore::claim_next_turn_inputs [function]
    fn method_witness_0512<T: lash::persistence::TurnInputStore>() {
        member_witness(T::claim_next_turn_inputs);
    }
    // FIG-2105-WITNESS-0513: lash::persistence::TurnInputStore::enqueue_pending_turn_input [function]
    fn method_witness_0513<T: lash::persistence::TurnInputStore>() {
        member_witness(T::enqueue_pending_turn_input);
    }
    // FIG-2105-WITNESS-0514: lash::persistence::TurnInputStore::list_pending_turn_inputs [function]
    fn method_witness_0514<T: lash::persistence::TurnInputStore>() {
        member_witness(T::list_pending_turn_inputs);
    }
    // FIG-2105-WITNESS-0515: lash::persistence::TurnInputStore::list_turn_input_applications [function]
    fn method_witness_0515<T: lash::persistence::TurnInputStore>() {
        member_witness(T::list_turn_input_applications);
    }
    // FIG-2105-WITNESS-0516: lash::persistence::WorkClaim [struct]
    type_witness::<lash::persistence::WorkClaim<lash::persistence::QueuedWorkClaimData>>();
    // FIG-2105-WITNESS-0517: lash::persistence::WorkClaim::claim_id [field]
    field_witness(
        |value: &lash::persistence::WorkClaim<lash::persistence::QueuedWorkClaimData>| {
            let _ = &value.claim_id;
        },
    );
    // FIG-2105-WITNESS-0518: lash::persistence::WorkClaim::completion [function]
    member_witness(
        <lash::persistence::WorkClaim<lash::persistence::QueuedWorkClaimData>>::completion,
    );
    // FIG-2105-WITNESS-0519: lash::persistence::WorkClaim::data [field]
    field_witness(
        |value: &lash::persistence::WorkClaim<lash::persistence::QueuedWorkClaimData>| {
            let _ = &value.data;
        },
    );
    // FIG-2105-WITNESS-0520: lash::persistence::WorkClaim::exclusive_session_command [function]
    member_witness(<lash::persistence::WorkClaim<lash::persistence::QueuedWorkClaimData>>::exclusive_session_command);
    // FIG-2105-WITNESS-0521: lash::persistence::WorkClaim::fencing_token [field]
    field_witness(
        |value: &lash::persistence::WorkClaim<lash::persistence::QueuedWorkClaimData>| {
            let _ = &value.fencing_token;
        },
    );
    // FIG-2105-WITNESS-0522: lash::persistence::WorkClaim::is_empty [function]
    member_witness(
        <lash::persistence::WorkClaim<lash::persistence::QueuedWorkClaimData>>::is_empty,
    );
    // FIG-2105-WITNESS-0523: lash::persistence::WorkClaim::lease_token [field]
    field_witness(
        |value: &lash::persistence::WorkClaim<lash::persistence::QueuedWorkClaimData>| {
            let _ = &value.lease_token;
        },
    );
    // FIG-2105-WITNESS-0524: lash::persistence::WorkClaim::materialize_checkpoint_turn_input [function]
    member_witness(<lash::persistence::WorkClaim<lash::persistence::TurnInputClaimData>>::materialize_checkpoint_turn_input);
    // FIG-2105-WITNESS-0525: lash::persistence::WorkClaim::materialize_queued_checkpoint_work [function]
    member_witness(<lash::persistence::WorkClaim<lash::persistence::QueuedWorkClaimData>>::materialize_queued_checkpoint_work);
    // FIG-2105-WITNESS-0526: lash::persistence::WorkClaim::materialize_queued_turn_work [function]
    member_witness(<lash::persistence::WorkClaim<lash::persistence::QueuedWorkClaimData>>::materialize_queued_turn_work);
    // FIG-2105-WITNESS-0527: lash::persistence::WorkClaim::materialize_turn_input [function]
    member_witness(<lash::persistence::WorkClaim<lash::persistence::TurnInputClaimData>>::materialize_turn_input);
    // FIG-2105-WITNESS-0528: lash::persistence::WorkClaim::owner [field]
    field_witness(
        |value: &lash::persistence::WorkClaim<lash::persistence::QueuedWorkClaimData>| {
            let _ = &value.owner;
        },
    );
    // FIG-2105-WITNESS-0529: lash::persistence::WorkClaim::record_checkpoint_applications [function]
    member_witness(<lash::persistence::WorkClaim<lash::persistence::TurnInputClaimData>>::record_checkpoint_applications);
    // FIG-2105-WITNESS-0530: lash::persistence::WorkClaim::session_commands [function]
    member_witness(
        <lash::persistence::WorkClaim<lash::persistence::QueuedWorkClaimData>>::session_commands,
    );
    // FIG-2105-WITNESS-0531: lash::persistence::WorkClaim::session_id [field]
    field_witness(
        |value: &lash::persistence::WorkClaim<lash::persistence::QueuedWorkClaimData>| {
            let _ = &value.session_id;
        },
    );
    // FIG-2105-WITNESS-0532: lash::persistence::WorkCompletion [struct]
    type_witness::<lash::persistence::WorkCompletion<lash::persistence::QueuedWorkCompletionData>>(
    );
    // FIG-2105-WITNESS-0533: lash::persistence::WorkCompletion::claim_id [field]
    field_witness(
        |value: &lash::persistence::WorkCompletion<lash::persistence::QueuedWorkCompletionData>| {
            let _ = &value.claim_id;
        },
    );
    // FIG-2105-WITNESS-0534: lash::persistence::WorkCompletion::data [field]
    field_witness(
        |value: &lash::persistence::WorkCompletion<lash::persistence::QueuedWorkCompletionData>| {
            let _ = &value.data;
        },
    );
    // FIG-2105-WITNESS-0535: lash::persistence::WorkCompletion::lease_token [field]
    field_witness(
        |value: &lash::persistence::WorkCompletion<lash::persistence::QueuedWorkCompletionData>| {
            let _ = &value.lease_token;
        },
    );
    // FIG-2105-WITNESS-0536: lash::persistence::WorkCompletion::session_id [field]
    field_witness(
        |value: &lash::persistence::WorkCompletion<lash::persistence::QueuedWorkCompletionData>| {
            let _ = &value.session_id;
        },
    );
    // FIG-2105-WITNESS-0537: lash::persistence::queued_work::QueuedWorkClass [enum]
    type_witness::<lash::persistence::queued_work::QueuedWorkClass>();
    // FIG-2105-WITNESS-0538: lash::persistence::queued_work::QueuedWorkClass::SessionCommand [variant]
    variant_witness(|value: &lash::persistence::queued_work::QueuedWorkClass| {
        matches!(
            value,
            lash::persistence::queued_work::QueuedWorkClass::SessionCommand
        )
    });
    // FIG-2105-WITNESS-0539: lash::persistence::queued_work::QueuedWorkClass::TurnWork [variant]
    variant_witness(|value: &lash::persistence::queued_work::QueuedWorkClass| {
        matches!(
            value,
            lash::persistence::queued_work::QueuedWorkClass::TurnWork
        )
    });
    // FIG-2105-WITNESS-0540: lash::plugins::TurnTransformContext::session_graph [field]
    field_witness(|value: &lash::plugins::TurnTransformContext| {
        let _ = &value.session_graph;
    });
    // FIG-2105-WITNESS-0541: lash::runtime::ExecutionScope::RuntimeOperation::operation_id [field]
    field_witness(|value: &lash::runtime::ExecutionScope| {
        if let lash::runtime::ExecutionScope::RuntimeOperation { operation_id, .. } = value {
            let _ = operation_id;
        }
    });
    // FIG-2105-WITNESS-0542: lash::runtime::RuntimeEffectCommand::AcceptTurnInput [variant]
    variant_witness(|value: &lash::runtime::RuntimeEffectCommand| {
        matches!(
            value,
            lash::runtime::RuntimeEffectCommand::AcceptTurnInput { .. }
        )
    });
    // FIG-2105-WITNESS-0543: lash::runtime::RuntimeEffectCommand::AcceptTurnInput::draft [field]
    field_witness(|value: &lash::runtime::RuntimeEffectCommand| {
        if let lash::runtime::RuntimeEffectCommand::AcceptTurnInput { draft, .. } = value {
            let _ = draft;
        }
    });
    // FIG-2105-WITNESS-0544: lash::runtime::RuntimeEffectCommand::Checkpoint [variant]
    variant_witness(|value: &lash::runtime::RuntimeEffectCommand| {
        matches!(
            value,
            lash::runtime::RuntimeEffectCommand::Checkpoint { .. }
        )
    });
    // FIG-2105-WITNESS-0545: lash::runtime::RuntimeEffectCommand::Checkpoint::checkpoint [field]
    field_witness(|value: &lash::runtime::RuntimeEffectCommand| {
        if let lash::runtime::RuntimeEffectCommand::Checkpoint { checkpoint, .. } = value {
            let _ = checkpoint;
        }
    });
    // FIG-2105-WITNESS-0546: lash::runtime::RuntimeEffectKind::AcceptTurnInput [variant]
    variant_witness(|value: &lash::runtime::RuntimeEffectKind| {
        matches!(value, lash::runtime::RuntimeEffectKind::AcceptTurnInput)
    });
    // FIG-2105-WITNESS-0547: lash::runtime::RuntimeEffectKind::Checkpoint [variant]
    variant_witness(|value: &lash::runtime::RuntimeEffectKind| {
        matches!(value, lash::runtime::RuntimeEffectKind::Checkpoint)
    });
    // FIG-2105-WITNESS-0548: lash::runtime::RuntimeEffectLocalExecutor::turn_acceptance [function]
    member_witness(lash::runtime::RuntimeEffectLocalExecutor::turn_acceptance);
    // FIG-2105-WITNESS-0549: lash::runtime::RuntimeEffectOutcome::AcceptTurnInput [variant]
    variant_witness(|value: &lash::runtime::RuntimeEffectOutcome| {
        matches!(
            value,
            lash::runtime::RuntimeEffectOutcome::AcceptTurnInput { .. }
        )
    });
    // FIG-2105-WITNESS-0550: lash::runtime::RuntimeEffectOutcome::AcceptTurnInput::accepted [field]
    field_witness(|value: &lash::runtime::RuntimeEffectOutcome| {
        if let lash::runtime::RuntimeEffectOutcome::AcceptTurnInput { accepted, .. } = value {
            let _ = accepted;
        }
    });
    // FIG-2105-WITNESS-0551: lash::runtime::RuntimeEffectOutcome::Checkpoint [variant]
    variant_witness(|value: &lash::runtime::RuntimeEffectOutcome| {
        matches!(
            value,
            lash::runtime::RuntimeEffectOutcome::Checkpoint { .. }
        )
    });
    // FIG-2105-WITNESS-0552: lash::runtime::RuntimeEffectOutcome::Checkpoint::claims [field]
    field_witness(|value: &lash::runtime::RuntimeEffectOutcome| {
        if let lash::runtime::RuntimeEffectOutcome::Checkpoint { claims, .. } = value {
            let _ = claims;
        }
    });
    // FIG-2105-WITNESS-0553: lash::runtime::RuntimeEffectOutcome::Checkpoint::result [field]
    field_witness(|value: &lash::runtime::RuntimeEffectOutcome| {
        if let lash::runtime::RuntimeEffectOutcome::Checkpoint { result, .. } = value {
            let _ = result;
        }
    });
    // FIG-2105-WITNESS-0554: lash::runtime::RuntimeErrorCode::CheckpointComponentEncodingVersionMismatch [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::CheckpointComponentEncodingVersionMismatch
        )
    });
    // FIG-2105-WITNESS-0555: lash::runtime::RuntimeErrorCode::ExecutionStateCaptureFailed [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::ExecutionStateCaptureFailed
        )
    });
    // FIG-2105-WITNESS-0556: lash::runtime::RuntimeErrorCode::RecordEncodingFailed [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(value, lash::runtime::RuntimeErrorCode::RecordEncodingFailed)
    });
    // FIG-2105-WITNESS-0557: lash::runtime::RuntimeErrorCode::RuntimeStoreCorrupt [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(value, lash::runtime::RuntimeErrorCode::RuntimeStoreCorrupt)
    });
    // FIG-2105-WITNESS-0558: lash::runtime::RuntimeErrorCode::StoreCommitByteBudgetExceeded [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::StoreCommitByteBudgetExceeded
        )
    });
    // FIG-2105-WITNESS-0559: lash::runtime::RuntimeErrorCode::StoreCommitNodeBudgetExceeded [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::StoreCommitNodeBudgetExceeded
        )
    });
    // FIG-2105-WITNESS-0560: lash::runtime::SessionSnapshot::checkpoint_ref [field]
    field_witness(|value: &lash::runtime::SessionSnapshot| {
        let _ = &value.checkpoint_ref;
    });
    // FIG-2105-WITNESS-0561: lash::runtime::SessionSnapshot::session_graph [field]
    field_witness(|value: &lash::runtime::SessionSnapshot| {
        let _ = &value.session_graph;
    });
    // FIG-2105-WITNESS-0562: lash::plugins::PersistedSegmentHandover [struct]
    type_witness::<lash::plugins::PersistedSegmentHandover>();
    // FIG-2105-WITNESS-0563: lash::plugins::PersistedSegmentHandover::handover [field]
    field_witness(|value: &lash::plugins::PersistedSegmentHandover| {
        let _ = &value.handover;
    });
    // FIG-2105-WITNESS-0564: lash::plugins::PersistedSegmentHandover::program_hash [function]
    member_witness(lash::plugins::PersistedSegmentHandover::program_hash);
    // FIG-2105-WITNESS-0565: lash::plugins::PersistedSegmentHandover::segment_ordinal [field]
    field_witness(|value: &lash::plugins::PersistedSegmentHandover| {
        let _ = &value.segment_ordinal;
    });
    // FIG-2105-WITNESS-0566: lash::persistence::TurnInputClaimMode::ActiveTurn::checkpoint [field]
    field_witness(|value: &lash::persistence::TurnInputClaimMode| {
        if let lash::persistence::TurnInputClaimMode::ActiveTurn { checkpoint, .. } = value {
            let _ = checkpoint;
        }
    });
    // FIG-2105-WITNESS-0567: lash::persistence::SessionNodeProjection [trait]
    fn trait_witness_0567<T: lash::persistence::SessionNodeProjection>() {}
    // FIG-2105-WITNESS-0568: lash::persistence::SessionNodeProjection::event [function]
    fn method_witness_0568<T: lash::persistence::SessionNodeProjection>() {
        member_witness(T::event);
    }
    // FIG-2105-WITNESS-0569: lash::persistence::SessionNodeProjection::message [function]
    fn method_witness_0569<T: lash::persistence::SessionNodeProjection>() {
        member_witness(T::message);
    }
    // FIG-2105-WITNESS-0570: lash::persistence::SessionNodeProjection::plugin [function]
    fn method_witness_0570<T: lash::persistence::SessionNodeProjection>() {
        member_witness(T::plugin);
    }
    // FIG-2105-WITNESS-0571: lash::persistence::QueuedWorkEnqueueOutcome::batch [function]
    member_witness(lash::persistence::QueuedWorkEnqueueOutcome::batch);
    // FIG-2105-WITNESS-0572: lash::persistence::QueuedWorkEnqueueOutcome::process_wake_was_absorbed [function]
    member_witness(lash::persistence::QueuedWorkEnqueueOutcome::process_wake_was_absorbed);
    // FIG-2105-WITNESS-0573: lash::persistence::GraphAppend::leaf_node_id [function]
    member_witness(lash::persistence::GraphAppend::leaf_node_id);
    // FIG-2105-WITNESS-0574: lash::persistence::RuntimeSessionState::execution_state_snapshot [function]
    member_witness(lash::persistence::RuntimeSessionState::execution_state_snapshot);
    // FIG-2105-WITNESS-0575: lash::persistence::RuntimeSessionState::policy [function]
    member_witness(lash::persistence::RuntimeSessionState::policy);
    // FIG-2105-WITNESS-0576: lash::persistence::RuntimeSessionState::session_graph [function]
    member_witness(lash::persistence::RuntimeSessionState::session_graph);
    // FIG-2105-WITNESS-0577: lash::persistence::RuntimeSessionState::execution_state_hydration [function]
    member_witness(lash::persistence::RuntimeSessionState::execution_state_hydration);
    // FIG-2105-WITNESS-0578: lash::persistence::PROCESS_WAKE_MERGE_KEY [constant]
    member_witness(lash::persistence::PROCESS_WAKE_MERGE_KEY);
    // FIG-2105-WITNESS-0579: lash::persistence::SelectedQueuedWorkClaimOutcome [struct]
    type_witness::<lash::persistence::SelectedQueuedWorkClaimOutcome>();
    // FIG-2105-WITNESS-0580: lash::persistence::SelectedQueuedWorkClaimOutcome::already_satisfied_batch_ids [field]
    field_witness(
        |value: &lash::persistence::SelectedQueuedWorkClaimOutcome| {
            let _ = &value.already_satisfied_batch_ids;
        },
    );
    // FIG-2105-WITNESS-0581: lash::persistence::SelectedQueuedWorkClaimOutcome::claim [field]
    field_witness(
        |value: &lash::persistence::SelectedQueuedWorkClaimOutcome| {
            let _ = &value.claim;
        },
    );
    // FIG-2105-WITNESS-0582: lash::persistence::SelectedQueuedWorkClaimOutcome::expect [function]
    member_witness(lash::persistence::SelectedQueuedWorkClaimOutcome::expect);
    // FIG-2105-WITNESS-0583: lash::persistence::SelectedQueuedWorkClaimOutcome::map [function]
    member_witness(
        |outcome: lash::persistence::SelectedQueuedWorkClaimOutcome| outcome.map(|_| ()),
    );
    // FIG-2105-WITNESS-0584: lash::persistence::SelectedQueuedWorkClaimOutcome::new [function]
    member_witness(lash::persistence::SelectedQueuedWorkClaimOutcome::new);
    // FIG-2105-WITNESS-0585: lash::persistence::SelectedQueuedWorkClaimOutcome::ok_or_else [function]
    member_witness(
        |outcome: lash::persistence::SelectedQueuedWorkClaimOutcome| outcome.ok_or_else(|| ()),
    );
    // FIG-2105-WITNESS-0586: lash::persistence::QueuedWorkAuthority [struct]
    type_witness::<lash::persistence::QueuedWorkAuthority>();
    // FIG-2105-WITNESS-0587: lash::persistence::QueuedWorkAuthority::elevation [field]
    field_witness(|value: &lash::persistence::QueuedWorkAuthority| {
        let _ = &value.elevation;
    });
    // FIG-2105-WITNESS-0588: lash::persistence::QueuedWorkAuthority::new [function]
    member_witness(|principal: String| lash::persistence::QueuedWorkAuthority::new(principal));
    // FIG-2105-WITNESS-0589: lash::persistence::QueuedWorkAuthority::principal [field]
    field_witness(|value: &lash::persistence::QueuedWorkAuthority| {
        let _ = &value.principal;
    });
    // FIG-2105-WITNESS-0590: lash::persistence::QueuedWorkAuthority::with_elevation [function]
    member_witness(
        |authority: lash::persistence::QueuedWorkAuthority, elevation: String| {
            authority.with_elevation(elevation)
        },
    );
    // FIG-2105-WITNESS-0591: lash::persistence::QueuedWorkBatch::authority [field]
    field_witness(|value: &lash::persistence::QueuedWorkBatch| {
        let _ = &value.authority;
    });
    // FIG-2105-WITNESS-0592: lash::persistence::QueuedWorkBatch::kind [field]
    field_witness(|value: &lash::persistence::QueuedWorkBatch| {
        let _ = &value.kind;
    });
    // FIG-2105-WITNESS-0593: lash::persistence::QueuedWorkBatchDraft::authority [field]
    field_witness(|value: &lash::persistence::QueuedWorkBatchDraft| {
        let _ = &value.authority;
    });
    // FIG-2105-WITNESS-0594: lash::persistence::QueuedWorkBatchDraft::kind [function]
    member_witness(lash::persistence::QueuedWorkBatchDraft::kind);
    // FIG-2105-WITNESS-0595: lash::persistence::QueuedWorkBatchDraft::with_authority [function]
    member_witness(lash::persistence::QueuedWorkBatchDraft::with_authority);
    // FIG-2105-WITNESS-0596: lash::persistence::QueuedWorkClaimPolicy [struct]
    type_witness::<lash::persistence::QueuedWorkClaimPolicy>();
    // FIG-2105-WITNESS-0597: lash::persistence::QueuedWorkClaimPolicy::action_token_reserve [field]
    field_witness(|value: &lash::persistence::QueuedWorkClaimPolicy| {
        let _ = &value.action_token_reserve;
    });
    // FIG-2105-WITNESS-0598: lash::persistence::QueuedWorkClaimPolicy::max_context_tokens [field]
    field_witness(|value: &lash::persistence::QueuedWorkClaimPolicy| {
        let _ = &value.max_context_tokens;
    });
    // FIG-2105-WITNESS-0599: lash::persistence::QueuedWorkClaimPolicy::max_pending_age_ms [field]
    field_witness(|value: &lash::persistence::QueuedWorkClaimPolicy| {
        let _ = &value.max_pending_age_ms;
    });
    // FIG-2105-WITNESS-0600: lash::persistence::QueuedWorkClaimPolicy::max_rows [field]
    field_witness(|value: &lash::persistence::QueuedWorkClaimPolicy| {
        let _ = &value.max_rows;
    });
    // FIG-2105-WITNESS-0601: lash::persistence::QueuedWorkKind [enum]
    type_witness::<lash::persistence::QueuedWorkKind>();
    // FIG-2105-WITNESS-0603: lash::persistence::QueuedWorkKind::Control [variant]
    variant_witness(|value: &lash::persistence::QueuedWorkKind| {
        matches!(value, lash::persistence::QueuedWorkKind::Control)
    });
    // FIG-2105-WITNESS-0604: lash::persistence::QueuedWorkKind::Turn [variant]
    variant_witness(|value: &lash::persistence::QueuedWorkKind| {
        matches!(value, lash::persistence::QueuedWorkKind::Turn)
    });
    // FIG-2105-WITNESS-0605: lash::persistence::QueuedWorkKind::as_str [function]
    member_witness(lash::persistence::QueuedWorkKind::as_str);
    // FIG-2105-WITNESS-0606: lash::persistence::QueuedWorkKind::from_wire_str [function]
    member_witness(lash::persistence::QueuedWorkKind::from_wire_str);
    // FIG-2105-WITNESS-0607: lash::persistence::QueuedWorkKind::is_batchable [function]
    member_witness(lash::persistence::QueuedWorkKind::is_batchable);
    // FIG-2105-WITNESS-0608: lash::persistence::StoreError::QueuedWorkActionReserveExhaustsContext [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::QueuedWorkActionReserveExhaustsContext { .. }
        )
    });
    // FIG-2105-WITNESS-0609: lash::persistence::StoreError::QueuedWorkActionReserveExhaustsContext::action_token_reserve [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::QueuedWorkActionReserveExhaustsContext {
            action_token_reserve,
            ..
        } = value
        {
            let _ = action_token_reserve;
        }
    });
    // FIG-2105-WITNESS-0610: lash::persistence::StoreError::QueuedWorkActionReserveExhaustsContext::max_context_tokens [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::QueuedWorkActionReserveExhaustsContext {
            max_context_tokens,
            ..
        } = value
        {
            let _ = max_context_tokens;
        }
    });
    // FIG-2105-WITNESS-0611: lash::persistence::StoreError::QueuedWorkRowExceedsContextWindow [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::QueuedWorkRowExceedsContextWindow { .. }
        )
    });
    // FIG-2105-WITNESS-0612: lash::persistence::StoreError::QueuedWorkRowExceedsContextWindow::batch_enqueue_seq [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::QueuedWorkRowExceedsContextWindow {
            batch_enqueue_seq,
            ..
        } = value
        {
            let _ = batch_enqueue_seq;
        }
    });
    // FIG-2105-WITNESS-0613: lash::persistence::StoreError::QueuedWorkRowExceedsContextWindow::max_context_tokens [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::QueuedWorkRowExceedsContextWindow {
            max_context_tokens,
            ..
        } = value
        {
            let _ = max_context_tokens;
        }
    });
    // FIG-2105-WITNESS-0614: lash::persistence::StoreError::QueuedWorkRowExceedsContextWindow::rendered_tokens [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::QueuedWorkRowExceedsContextWindow {
            rendered_tokens,
            ..
        } = value
        {
            let _ = rendered_tokens;
        }
    });
    // FIG-2105-WITNESS-0615: lash::persistence::SelectedQueuedWorkClaimOutcome::acquired_no_rows [function]
    member_witness(lash::persistence::SelectedQueuedWorkClaimOutcome::acquired_no_rows);
    // FIG-2105-WITNESS-0616: lash::persistence::QueuedWorkClaimPolicy::drain_policy [field]
    field_witness(|value: &lash::persistence::QueuedWorkClaimPolicy| {
        let _ = &value.drain_policy;
    });
    // FIG-2105-WITNESS-0617: lash::persistence::StoreError::QueuedWorkRowExceedsContextWindow::batch_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::QueuedWorkRowExceedsContextWindow {
            batch_id, ..
        } = value
        {
            let _ = batch_id;
        }
    });
    // FIG-2105-WITNESS-0618: lash::persistence::queued_work::select_exact_turn_work_claim_prefix [function]
    member_witness(lash::persistence::queued_work::select_exact_turn_work_claim_prefix);
    // FIG-2105-WITNESS-0619: lash::persistence::queued_work::PendingSessionWorkOrdering [struct]
    type_witness::<lash::persistence::queued_work::PendingSessionWorkOrdering>();
    // FIG-2105-WITNESS-0620: lash::persistence::queued_work::PendingSessionWorkOrdering::session_command [field]
    field_witness(
        |value: &lash::persistence::queued_work::PendingSessionWorkOrdering| {
            let _ = &value.session_command;
        },
    );
    // FIG-2105-WITNESS-0621: lash::persistence::queued_work::PendingSessionWorkOrdering::session_command_precedes_turn_input [function]
    member_witness(lash::persistence::queued_work::PendingSessionWorkOrdering::session_command_precedes_turn_input);
    // FIG-2105-WITNESS-0622: lash::persistence::queued_work::PendingSessionWorkOrdering::turn_input [field]
    field_witness(
        |value: &lash::persistence::queued_work::PendingSessionWorkOrdering| {
            let _ = &value.turn_input;
        },
    );
    // FIG-2105-WITNESS-0623: lash::persistence::queued_work::PendingWorkOrderingKey [struct]
    type_witness::<lash::persistence::queued_work::PendingWorkOrderingKey>();
    // FIG-2105-WITNESS-0624: lash::persistence::queued_work::PendingWorkOrderingKey::enqueue_seq [field]
    field_witness(
        |value: &lash::persistence::queued_work::PendingWorkOrderingKey| {
            let _ = &value.enqueue_seq;
        },
    );
    // FIG-2105-WITNESS-0625: lash::persistence::queued_work::PendingWorkOrderingKey::enqueued_at_ms [field]
    field_witness(
        |value: &lash::persistence::queued_work::PendingWorkOrderingKey| {
            let _ = &value.enqueued_at_ms;
        },
    );
    // FIG-2105-WITNESS-0626: lash::persistence::OrphanedTurnInputScope [enum]
    type_witness::<lash::persistence::OrphanedTurnInputScope>();
    // FIG-2105-WITNESS-0627: lash::persistence::OrphanedTurnInputScope::LaneGeneration [variant]
    variant_witness(|value: &lash::persistence::OrphanedTurnInputScope| {
        matches!(
            value,
            lash::persistence::OrphanedTurnInputScope::LaneGeneration { .. }
        )
    });
    // FIG-2105-WITNESS-0628: lash::persistence::OrphanedTurnInputScope::LaneGeneration::resumable_turn_id [field]
    field_witness(|value: &lash::persistence::OrphanedTurnInputScope| {
        if let lash::persistence::OrphanedTurnInputScope::LaneGeneration {
            resumable_turn_id, ..
        } = value
        {
            let _ = resumable_turn_id;
        }
    });
    // FIG-2105-WITNESS-0629: lash::persistence::OrphanedTurnInputScope::Turn [variant]
    variant_witness(|value: &lash::persistence::OrphanedTurnInputScope| {
        matches!(value, lash::persistence::OrphanedTurnInputScope::Turn(..))
    });
    // FIG-2105-WITNESS-0630: lash::persistence::OrphanedTurnInputScope::Turn::0 [field]
    field_witness(|value: &lash::persistence::OrphanedTurnInputScope| {
        if let lash::persistence::OrphanedTurnInputScope::Turn(field, ..) = value {
            let _ = field;
        }
    });
    // FIG-2105-WITNESS-0631: lash::persistence::TurnInputStore::defer_orphaned_active_turn_inputs [function]
    fn method_witness_0631<T: lash::persistence::TurnInputStore>() {
        member_witness(T::defer_orphaned_active_turn_inputs);
    }
    // FIG-2105-WITNESS-0632: lash::persistence::QueuedWorkClaimOutcome::claim [function]
    member_witness(lash::persistence::QueuedWorkClaimOutcome::claim);
    // FIG-2105-WITNESS-0633: lash::persistence::QueuedWorkClaimOutcome::refusal [function]
    member_witness(lash::persistence::QueuedWorkClaimOutcome::refusal);
    // FIG-2105-WITNESS-0634: lash::persistence::MaintenanceReport::reclaimed_count [function]
    fn method_witness_0634<T: lash::persistence::MaintenanceReport>() {
        member_witness(T::reclaimed_count);
    }
    // FIG-2105-WITNESS-0635: lash::persistence::MaintenanceFailure::refused [function]
    member_witness(
        lash::persistence::MaintenanceFailure::<
            lash::persistence::GcReport,
            lash::persistence::StoreError,
        >::refused,
    );
    // FIG-2105-WITNESS-0636: lash::persistence::MaintenanceFailure::refusal [function]
    member_witness(
        lash::persistence::MaintenanceFailure::<
            lash::persistence::GcReport,
            lash::persistence::StoreError,
        >::refusal,
    );
    // FIG-2105-WITNESS-0637: lash::persistence::SessionBlobReclaimReport [struct]
    type_witness::<lash::persistence::SessionBlobReclaimReport>();
    // FIG-2105-WITNESS-0638: lash::persistence::SessionBlobReclaimReport::enumerated_blob_count [field]
    field_witness(|value: &lash::persistence::SessionBlobReclaimReport| {
        let _ = &value.enumerated_blob_count;
    });
    // FIG-2105-WITNESS-0639: lash::persistence::SessionBlobReclaimReport::retained_blob_count [field]
    field_witness(|value: &lash::persistence::SessionBlobReclaimReport| {
        let _ = &value.retained_blob_count;
    });
    // FIG-2105-WITNESS-0640: lash::persistence::SessionBlobReclaimReport::deleted_blob_count [field]
    field_witness(|value: &lash::persistence::SessionBlobReclaimReport| {
        let _ = &value.deleted_blob_count;
    });
    // FIG-2105-WITNESS-0641: lash::persistence::RuntimeCommitReceipt::turn_cancel_input_outcome [field]
    field_witness(|value: &lash::persistence::RuntimeCommitReceipt| {
        let _ = &value.turn_cancel_input_outcome;
    });
    // FIG-2105-WITNESS-0642: lash::persistence::SessionStoreFactory::open_existing_store_by_id [function]
    fn method_witness_0642<T: lash::persistence::SessionStoreFactory>() {
        member_witness(T::open_existing_store_by_id);
    }
    // FIG-2105-WITNESS-0643: lash::persistence::TurnInputStore::record_turn_cancel_request [function]
    fn method_witness_0643<T: lash::persistence::TurnInputStore>() {
        member_witness(T::record_turn_cancel_request);
    }
    // FIG-2105-WITNESS-0644: lash::persistence::TurnInputStore::turn_cancel_request [function]
    fn method_witness_0644<T: lash::persistence::TurnInputStore>() {
        member_witness(T::turn_cancel_request);
    }
}
