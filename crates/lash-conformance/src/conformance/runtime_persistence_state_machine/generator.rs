use super::*;

const GENERATED_PREFIX_OPS: usize = 63;

pub(super) struct ComponentSelection {
    pub(super) store_tool: bool,
    pub(super) store_plugin: bool,
    pub(super) store_execution: bool,
    pub(super) clear_execution: bool,
}

pub(super) fn component_selection(mode: u8) -> ComponentSelection {
    let (store_tool, store_plugin, store_execution, clear_execution) = match mode % 6 {
        0 => (false, false, false, false),
        1 => (true, true, true, false),
        2 => (true, false, false, false),
        3 => (false, true, false, false),
        4 => (false, false, true, false),
        _ => (false, false, false, true),
    };
    ComponentSelection {
        store_tool,
        store_plugin,
        store_execution,
        clear_execution,
    }
}

pub(super) fn plugin_state(value: u8) -> PluginState {
    PluginState {
        plugins: BTreeMap::from([(
            "property-plugin".to_string(),
            PluginNamespaceState {
                generation: u64::from(value),
                values: std::collections::BTreeMap::from([(
                    "state".into(),
                    serde_json::json!({"value": value}),
                )]),
            },
        )]),
    }
}

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
        SealFence { owner: 0 },
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
            turn_owned: false,
        },
        CommitWithAttachmentRefs {
            new_session: true,
            session_selection: 0,
            attachment_slot: 0,
            value: 0,
            turn_owned: false,
        },
        CommitWithAttachmentRefs {
            new_session: false,
            session_selection: 1,
            attachment_slot: 1,
            value: 1,
            turn_owned: true,
        },
        ReplayAttachmentCommit { selection: 1 },
        PutAttachmentWrite {
            owner_kind: 1,
            attachment_slot: 6,
            value: 254,
        },
        PutAttachmentWrite {
            owner_kind: 2,
            attachment_slot: 7,
            value: 255,
        },
        PutAttachmentWrite {
            owner_kind: 0,
            attachment_slot: 5,
            value: 253,
        },
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
        // A root takes the exclusive head alone and its commit settles it.
        AdmitWork,
        Commit {
            component_mode: 0,
            value: 0,
            settle_work: true,
            settle_inputs: false,
            stale_head: false,
        },
        AdmitWork,
        // The worker dies after its admission committed.
        Crash,
        SealFence { owner: 1 },
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
        // The successor resumes the recorded admission; its rows stay bound.
        AdmitWork,
        CancelAdmittedRow,
        SealFence { owner: 0 },
        AdmitWorkWithStaleFence,
        SettleUnderStaleFence,
        SettleForeignRow,
        Commit {
            component_mode: 0,
            value: 0,
            settle_work: true,
            settle_inputs: false,
            stale_head: false,
        },
        // A joined admission; a second root is refused while it is unfinished.
        AdmitWork,
        EnqueueTurnInput { slot: 0, value: 0 },
        EnqueueTurnInput { slot: 1, value: 1 },
        AdmitTurnInputs { max_inputs: 3 },
        // The root ends settling nothing: its terminal hands its rows back.
        Commit {
            component_mode: 0,
            value: 0,
            settle_work: false,
            settle_inputs: true,
            stale_head: false,
        },
        AdmitWork,
        Commit {
            component_mode: 0,
            value: 0,
            settle_work: true,
            settle_inputs: false,
            stale_head: false,
        },
        AdmitTurnInputs { max_inputs: 3 },
        Crash,
        SealFence { owner: 2 },
        AdmitTurnInputsWithStaleFence,
        AdmitTurnInputs { max_inputs: 3 },
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
        AdmitWork,
        Crash,
        SealFence { owner: 3 },
        SettleUnderStaleFence,
        Commit {
            component_mode: 5,
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
        3 => (0_u8..4).prop_map(|owner| SealFence { owner }),
        1 => Just(Crash),
        5 => (0_u8..8, any::<u8>(), any::<bool>()).prop_map(|(slot, value, coalesce)| EnqueueWork { slot, value, coalesce }),
        4 => Just(AdmitWork),
        1 => Just(AdmitWorkWithStaleFence),
        2 => any::<u8>().prop_map(|selection| CancelWork { selection }),
        4 => (0_u8..8, any::<u8>()).prop_map(|(slot, value)| EnqueueTurnInput { slot, value }),
        3 => (1_u8..5).prop_map(|max_inputs| AdmitTurnInputs { max_inputs }),
        1 => Just(AdmitTurnInputsWithStaleFence),
        1 => Just(CancelAdmittedRow),
        2 => any::<u8>().prop_map(|selection| CancelTurnInput { selection }),
        4 => (0_u8..8, any::<u8>()).prop_map(|(slot, value)| RecordUsage { slot, value }),
        2 => any::<bool>().prop_map(|replay_last_commit| StageUsage { replay_last_commit }),
        2 => any::<u8>().prop_map(|selection| ConfirmUsage { selection }),
        1 => Just(ReplayUsageReceipt),
        4 => (any::<bool>(), any::<u8>(), 0_u8..8, any::<u8>(), any::<bool>())
            .prop_map(|(new_session, session_selection, attachment_slot, value, turn_owned)| CommitWithAttachmentRefs {
                new_session, session_selection, attachment_slot, value, turn_owned,
            }),
        3 => (0_u8..3, 0_u8..8, any::<u8>())
            .prop_map(|(owner_kind, attachment_slot, value)| PutAttachmentWrite {
                owner_kind, attachment_slot, value,
            }),
        2 => any::<u8>().prop_map(|selection| ReplayAttachmentCommit { selection }),
        2 => any::<u8>().prop_map(|selection| ReclaimAttachmentSession { selection }),
        2 => Just(ProbeAttachmentGc),
        6 => (0_u8..6, any::<u8>(), any::<bool>(), any::<bool>(), any::<bool>())
            .prop_map(|(component_mode, value, settle_work, settle_inputs, stale_head)| Commit {
                component_mode, value, settle_work, settle_inputs, stale_head,
            }),
        2 => Just(SettleUnderStaleFence),
        2 => Just(SettleForeignRow),
    ]
}
