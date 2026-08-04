use super::*;

pub(super) fn generated_case() -> impl Strategy<Value = GeneratedCase> {
    (
        any::<u64>(),
        prop::collection::vec(operation(), 1..=(MAX_OPS - GENERATED_PREFIX_OPS)),
    )
        .prop_map(|(seed, random)| {
            let mut operations = generated_prefix();
            operations.extend(random);
            GeneratedCase { seed, operations }
        })
}

fn generated_prefix() -> Vec<RuntimePersistenceOp> {
    use RuntimePersistenceOp::*;
    let operations = vec![
        ClaimLease { owner: 0 },
        RecordUsage { slot: 0, value: 0 },
        RecordUsage {
            slot: 1,
            value: u8::MAX,
        },
        StageUsage {
            replay_last_commit: false,
        },
        Commit {
            component_mode: 0,
            value: 0,
            settle_work: false,
            settle_inputs: false,
            stale_head: false,
        },
        StageUsage {
            replay_last_commit: true,
        },
        RecordUsage { slot: 2, value: 3 },
        ConfirmUsage { selection: 0 },
        ReplayUsageReceipt,
        ConfirmUsage { selection: 0 },
        CommitWithAttachmentRefs {
            new_session: true,
            session_selection: 0,
            attachment_slot: 0,
            value: 0,
        },
        CommitWithAttachmentRefs {
            new_session: true,
            session_selection: 0,
            attachment_slot: 0,
            value: 0,
        },
        CommitWithAttachmentRefs {
            new_session: false,
            session_selection: 1,
            attachment_slot: 1,
            value: 1,
        },
        ReplayAttachmentCommit { selection: 1 },
        ReclaimAttachmentSession { selection: 0 },
        ProbeAttachmentGc,
        ReclaimAttachmentSession { selection: 0 },
        ProbeAttachmentGc,
        EnqueueWork {
            slot: 0,
            value: 0,
            coalesce: false,
        },
        EnqueueWork {
            slot: 1,
            value: 1,
            coalesce: false,
        },
        EnqueueWork {
            slot: 2,
            value: 2,
            coalesce: true,
        },
        EnqueueWork {
            slot: 3,
            value: 3,
            coalesce: true,
        },
        ClaimWork {
            selected: true,
            selection: 1,
        },
        Commit {
            component_mode: 0,
            value: 0,
            settle_work: true,
            settle_inputs: false,
            stale_head: false,
        },
        ClaimWork {
            selected: false,
            selection: 0,
        },
        Crash,
        ClaimLease { owner: 1 },
        StageUsage {
            replay_last_commit: false,
        },
        Commit {
            component_mode: 0,
            value: 0,
            settle_work: false,
            settle_inputs: false,
            stale_head: false,
        },
        ConfirmUsage { selection: 0 },
        ClaimLease { owner: 0 },
        RenewLease { stale: true },
        ClaimWorkWithStaleLease,
        ClaimWork {
            selected: false,
            selection: 0,
        },
        SettleStaleWork,
        Commit {
            component_mode: 0,
            value: 0,
            settle_work: true,
            settle_inputs: false,
            stale_head: false,
        },
        ClaimWork {
            selected: false,
            selection: 0,
        },
        Commit {
            component_mode: 0,
            value: 0,
            settle_work: true,
            settle_inputs: false,
            stale_head: false,
        },
        EnqueueTurnInput { slot: 0, value: 0 },
        EnqueueTurnInput { slot: 1, value: 1 },
        ClaimTurnInputs { max_inputs: 3 },
        Crash,
        ClaimLease { owner: 2 },
        ClaimTurnInputsWithStaleLease,
        ClaimTurnInputs { max_inputs: 3 },
        SettleStaleTurnInputs,
        Commit {
            component_mode: 1,
            value: 1,
            settle_work: false,
            settle_inputs: true,
            stale_head: false,
        },
        EnqueueWork {
            slot: 5,
            value: 5,
            coalesce: false,
        },
        ClaimWork {
            selected: false,
            selection: 0,
        },
        Crash,
        ClaimLease { owner: 3 },
        SettleStaleWork,
        Commit {
            component_mode: 0,
            value: 0,
            settle_work: false,
            settle_inputs: false,
            stale_head: false,
        },
        Commit {
            component_mode: 1,
            value: 2,
            settle_work: false,
            settle_inputs: false,
            stale_head: true,
        },
        Commit {
            component_mode: 1,
            value: 2,
            settle_work: false,
            settle_inputs: false,
            stale_head: false,
        },
        EnqueueWork {
            slot: 4,
            value: 4,
            coalesce: false,
        },
        CancelWork { selection: 0 },
        EnqueueTurnInput { slot: 2, value: 2 },
        CancelTurnInput { selection: 0 },
    ];
    debug_assert_eq!(operations.len(), GENERATED_PREFIX_OPS);
    operations
}

fn operation() -> impl Strategy<Value = RuntimePersistenceOp> {
    use RuntimePersistenceOp::*;
    prop_oneof![
        3 => (0_u8..4).prop_map(|owner| ClaimLease { owner }),
        2 => any::<bool>().prop_map(|stale| RenewLease { stale }),
        1 => Just(Crash),
        5 => (0_u8..8, any::<u8>(), any::<bool>()).prop_map(|(slot, value, coalesce)| EnqueueWork { slot, value, coalesce }),
        4 => (any::<bool>(), any::<u8>()).prop_map(|(selected, selection)| ClaimWork { selected, selection }),
        1 => Just(ClaimWorkWithStaleLease),
        2 => any::<u8>().prop_map(|selection| CancelWork { selection }),
        4 => (0_u8..8, any::<u8>()).prop_map(|(slot, value)| EnqueueTurnInput { slot, value }),
        3 => (1_u8..5).prop_map(|max_inputs| ClaimTurnInputs { max_inputs }),
        1 => Just(ClaimTurnInputsWithStaleLease),
        2 => any::<u8>().prop_map(|selection| CancelTurnInput { selection }),
        4 => (0_u8..8, any::<u8>()).prop_map(|(slot, value)| RecordUsage { slot, value }),
        2 => any::<bool>().prop_map(|replay_last_commit| StageUsage { replay_last_commit }),
        2 => any::<u8>().prop_map(|selection| ConfirmUsage { selection }),
        1 => Just(ReplayUsageReceipt),
        4 => (any::<bool>(), any::<u8>(), 0_u8..8, any::<u8>())
            .prop_map(|(new_session, session_selection, attachment_slot, value)| CommitWithAttachmentRefs {
                new_session, session_selection, attachment_slot, value,
            }),
        2 => any::<u8>().prop_map(|selection| ReplayAttachmentCommit { selection }),
        2 => any::<u8>().prop_map(|selection| ReclaimAttachmentSession { selection }),
        2 => Just(ProbeAttachmentGc),
        6 => (0_u8..5, any::<u8>(), any::<bool>(), any::<bool>(), any::<bool>())
            .prop_map(|(component_mode, value, settle_work, settle_inputs, stale_head)| Commit {
                component_mode, value, settle_work, settle_inputs, stale_head,
            }),
        2 => Just(SettleStaleWork),
        2 => Just(SettleStaleTurnInputs),
    ]
}
