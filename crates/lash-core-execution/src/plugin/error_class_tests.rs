//! One value of every [`PluginError`] variant, and the laws that hold of
//! each of them (FIG-4649).

use super::error::{PluginError, PluginErrorClass};
use crate::{RuntimeEffectControllerError, RuntimeError, RuntimeErrorCode, SessionId};

/// `plugin_error_samples!` names every `PluginError` variant with one value
/// of it. Its one input generates the list the laws iterate and an
/// exhaustive `match`, so a variant missing from it fails to compile.
macro_rules! plugin_error_samples {
    ($( $variant:ident $(($($tuple:tt)*))? $({$($fields:tt)*})? => $sample:expr, )*) => {
        fn one_of_every_variant() -> Vec<PluginError> {
            let samples = vec![$( $sample, )*];
            for sample in &samples {
                match sample {
                    $( PluginError::$variant $(($($tuple)*))? $({$($fields)*})? => {} )*
                }
            }
            samples
        }
    };
}

fn process() -> crate::ProcessId {
    crate::process_id_for_test("sampled-process")
}

fn session() -> SessionId {
    SessionId::from("sampled-session")
}

fn cancel(actor: &str) -> Box<crate::CancelRequest> {
    Box::new(crate::CancelRequest::new(
        crate::CancelOrigin::OperatorRequested,
        actor,
        1,
    ))
}

plugin_error_samples! {
    ProviderFailure { .. } => PluginError::ProviderFailure {
        kind: crate::ProviderFailureKind::Quota,
        code: Some(crate::FailureCode::provider("host_spend_cap")),
        retryable: false,
        terminal_reason: crate::LlmTerminalReason::ProviderError,
        message: "host spend cap reached".into(),
    },
    Operation(_) => PluginError::Operation(Box::new(super::operation_protocol_failure("rejected"))),
    HookFailures { .. } => PluginError::HookFailures { causes: Vec::new() },
    UnusableSchema { .. } => PluginError::UnusableSchema {
        source: Box::new(crate::JsonSchema::admit(serde_json::Value::Null)
            .expect_err("null cannot be admitted as a schema")),
    },
    UnusableToolSchema { .. } => PluginError::UnusableToolSchema {
        source: Box::new(crate::ToolDefinition::raw(
            "sampled", "sampled", "sampled", serde_json::Value::Null, serde_json::json!({}),
        ).expect_err("a tool cannot publish an unusable schema")),
    },
    ValueMismatch { .. } => PluginError::ValueMismatch {
        context: "payload".into(),
        source: Box::new(crate::ValueMismatch {
            instance_path: "/count".into(),
            message: "integer required".into(),
        }),
    },
    MissingRecordedProcessConfig { .. } => PluginError::MissingRecordedProcessConfig {
        engine_kind: "sampled".to_string(),
    },
    ProcessCancelConflict { .. } => PluginError::ProcessCancelConflict {
        process_id: process(),
        existing: cancel("actor:first"),
        requested: cancel("actor:second"),
    },
    ParentEnded { .. } => PluginError::ParentEnded {
        start_key: None,
        parent: crate::ScopeId::process(process()),
    },
    StartKeyConflict { .. } => PluginError::StartKeyConflict {
        start_key: crate::StartKey::for_host("sampled"),
    },
    InvalidToolDiscovery { .. } => PluginError::InvalidToolDiscovery {
        operation: "sampled".to_string(),
    },
    InvalidBatchMaximum { .. } => PluginError::InvalidBatchMaximum { requested: 2, ceiling: 1 },
    ResidentToolContractUnavailable { .. } => PluginError::ResidentToolContractUnavailable {
        tool_id: crate::ToolId::from("tool:sampled"),
        name: "sampled".to_string(),
    },
    ResidentToolDuplicateId { .. } => PluginError::ResidentToolDuplicateId {
        tool_id: crate::ToolId::from("tool:sampled"),
    },
    ResidentToolDuplicateName { .. } => PluginError::ResidentToolDuplicateName {
        name: "sampled".to_string(),
    },
    ToolRegistrationRefused { .. } => PluginError::ToolRegistrationRefused {
        source: Box::new(crate::RegistrationRefused::MissingBound {
            tool: "sampled".to_string(),
            bound: crate::ToolBound::Execution,
        }),
    },
    ResidentToolRouteUnavailable { .. } => PluginError::ResidentToolRouteUnavailable {
        tool_id: crate::ToolId::from("tool:sampled"),
        name: "sampled".to_string(),
        reason: "sampled".to_string(),
    },
    SessionAlreadyExists { .. } => PluginError::SessionAlreadyExists { session_id: session() },
    SessionHeadOwned { .. } => PluginError::SessionHeadOwned {
        session_id: session(),
        owner: crate::store::SessionHeadOwner::CommandLane { enqueue_seq: 1 },
    },
    Registration(_) => PluginError::Registration("sampled".to_string()),
    ConfigRegistration(_) => PluginError::ConfigRegistration(
        crate::ConfigRegistrationError::ReservedOwner {
            owner: "sampled".to_string(),
        },
    ),
    Invoke(_) => PluginError::Invoke("sampled".to_string()),
    Session(_) => PluginError::Session("sampled".to_string()),
    State(_) => PluginError::State(crate::PluginStateError::StoreTooLarge { bytes: 2, limit: 1 }),
    Declaration(_) => PluginError::Declaration(
        crate::plugin::PluginDeclarationError::IdMismatch {
            factory: "sampled".to_string(),
            declared: "other".to_string(),
        },
    ),
    Format(_) => PluginError::Format(crate::FormatRefusal {
        plugin: "sampled".to_string(),
        namespace: crate::FormatNamespace::State,
        stored: crate::FormatVersion::new(2).unwrap(),
        readable: crate::FormatVersion::ONE,
    }),
    StoreRefusal(_) => PluginError::StoreRefusal(crate::store::StoreRefusal::WriterFenced {
        recorded: 2,
        writable: crate::compat::VersionRange::exactly(1),
    }),
    StoreUnavailable { .. } => PluginError::StoreUnavailable {
        fault: crate::store::StoreFault::Contended,
    },
    MissingRecordedSessionConfig { .. } => PluginError::MissingRecordedSessionConfig {
        plugin_id: "sampled".to_string(),
        field: "sampled".to_string(),
    },
    RecordedSessionConfigConflict { .. } => PluginError::RecordedSessionConfigConflict {
        plugin_id: "sampled".to_string(),
        field: "sampled".to_string(),
        recorded: "one".to_string(),
        requested: "two".to_string(),
    },
    Runtime(_) => PluginError::Runtime(RuntimeError::new(RuntimeErrorCode::Plugin, "sampled")),
    SessionExecutionLeaseLost { .. } => PluginError::SessionExecutionLeaseLost {
        session_id: session(),
    },
    AppendOperationIdentityConflict { .. } => PluginError::AppendOperationIdentityConflict {
        session_id: session(),
        operation_key: "sampled".to_string(),
    },
    AppendReceiptRequestedNodeCountCorrupt { .. } => {
        PluginError::AppendReceiptRequestedNodeCountCorrupt {
            session_id: session(),
            operation_key: "sampled".to_string(),
            stored: 1,
            attempted: 2,
        }
    },
    StoredDataCorrupt { .. } => PluginError::StoredDataCorrupt {
        record_kind: "Sampled".to_string(),
        message: "sampled".to_string(),
    },
    ClockBeforeUnixEpoch { .. } => PluginError::ClockBeforeUnixEpoch {
        clock: "sampled".to_string(),
        epoch_ms: -1,
    },
    ProcessNotVisible { .. } => PluginError::ProcessNotVisible { process_id: process() },
    NotASessionRuntime { .. } => PluginError::NotASessionRuntime {
        operation: "sampled".to_string(),
        process_id: process(),
    },
    ProcessOutputAttachmentUnavailable { .. } => PluginError::ProcessOutputAttachmentUnavailable {
        digest: crate::AttachmentId::parse("sampled-attachment").expect("a well-formed id"),
    },
    ProcessUnknown { .. } => PluginError::ProcessUnknown { process_id: process() },
    ProcessChangeCursorPruned { .. } => PluginError::ProcessChangeCursorPruned {
        requested_cursor: crate::ProcessChangeCursor::initial(),
        tombstone_compaction_horizon: crate::ProcessChangeCursor::from_store_sequence(2),
    },
    ProcessEventsReleased { .. } => PluginError::ProcessEventsReleased {
        process_id: process(),
        released_through: 3,
    },
    RuntimeEffectController(_) => PluginError::RuntimeEffectController(
        RuntimeEffectControllerError::new(RuntimeErrorCode::Plugin, "sampled"),
    ),
    ProcessExecutionSuperseded { .. } => PluginError::ProcessExecutionSuperseded {
        process_id: process(),
    },
    MonotonicCounterOverflow { .. } => PluginError::MonotonicCounterOverflow {
        counter: "sampled".to_string(),
        current: 1,
    },
    ProcessNoLongerRetained { .. } => PluginError::ProcessNoLongerRetained {
        terminal_label: crate::RetiredProcessStatus::Completed,
        pruned_at_ms: 1,
    },
    ProcessHandedOver { .. } => PluginError::ProcessHandedOver {
        process_id: process(),
        segment_ordinal: 1,
    },
    ProcessAlreadyTerminal { .. } => PluginError::ProcessAlreadyTerminal {
        process_id: process(),
        status: crate::ProcessStatus::Completed,
    },
    ProcessTerminalOutcomeMismatch { .. } => PluginError::ProcessTerminalOutcomeMismatch {
        declared_status: crate::ProcessStatus::Completed,
        outcome_status: None,
    },
    ReservedProcessEvent { .. } => PluginError::ReservedProcessEvent {
        event_type: "sampled".to_string(),
    },
    ProcessRegistryCursorBackendMismatch { .. } => {
        PluginError::ProcessRegistryCursorBackendMismatch {
            expected: "sqlite".to_string(),
            actual: "postgres".to_string(),
        }
    },
}

/// Every variant, the carried runtime errors in each of their classes, and
/// every store error as the plugin boundary carries it.
fn samples() -> Vec<PluginError> {
    let mut samples = one_of_every_variant();
    samples.push(PluginError::ProviderFailure {
        kind: crate::ProviderFailureKind::Transport,
        code: Some(crate::FailureCode::provider("transport")),
        retryable: true,
        terminal_reason: crate::LlmTerminalReason::ProviderError,
        message: "recorded transport failure".into(),
    });
    for code in [
        RuntimeErrorCode::RuntimeStore,
        RuntimeErrorCode::StoreCommitSuperseded,
        RuntimeErrorCode::EffectReplayDivergence,
    ] {
        samples.push(PluginError::Runtime(RuntimeError::new(
            code.clone(),
            "sampled",
        )));
        samples.push(PluginError::RuntimeEffectController(
            RuntimeEffectControllerError::new(code, "sampled"),
        ));
    }
    samples.push(PluginError::State(
        crate::PluginStateError::PublicationFenced {
            plugin: "sample".into(),
        },
    ));
    for class in [
        super::PluginFailureClass::Retryable,
        super::PluginFailureClass::Redrivable,
        super::PluginFailureClass::Terminal,
        super::PluginFailureClass::Parked,
    ] {
        let mut failure = super::operation_protocol_failure("same diagnostic");
        failure.class = class;
        samples.push(PluginError::Operation(Box::new(failure.clone())));
        samples.push(PluginError::HookFailures {
            causes: vec![super::PluginHookFailure {
                origin: super::PluginFailureOrigin {
                    plugin_id: "sample".into(),
                    behavior_revision: std::num::NonZeroU32::MIN,
                    operation: "hook".into(),
                },
                failure,
            }],
        });
    }
    samples.extend(
        crate::StoreError::samples_for_testing()
            .into_iter()
            .map(PluginError::from),
    );
    samples
}

/// The posture each projection of `error` gives it: (retryable, terminal).
fn postures(error: &PluginError) -> [(&'static str, (bool, bool)); 4] {
    let controller = RuntimeEffectControllerError::from(error.clone()).into_runtime_error();
    let turn = error.clone().into_turn_failure(RuntimeErrorCode::Plugin);
    let class = error.class();
    [
        (
            "class",
            (
                class == PluginErrorClass::Retryable,
                class == PluginErrorClass::Terminal,
            ),
        ),
        ("plugin", (error.is_retryable(), error.is_terminal())),
        (
            "effect controller",
            (controller.is_retryable(), controller.is_terminal()),
        ),
        ("turn failure", (turn.is_retryable(), turn.is_terminal())),
    ]
}

/// A plugin error has one class: retried, redriven or terminal the same way
/// on the plugin port, through the effect controller and as a turn's
/// failure, before and after it is journaled.
#[test]
fn every_plugin_error_has_one_class_on_every_boundary() {
    let mut disagreements = Vec::new();
    for error in samples() {
        let envelope = super::PluginOperationFailure::from(error.clone());
        let decoded: super::PluginOperationFailure = serde_json::from_slice(
            &serde_json::to_vec(&envelope).expect("encode failure envelope"),
        )
        .expect("decode failure envelope");
        assert_eq!(decoded, envelope);
        if !matches!(error, PluginError::Operation(_)) {
            assert_eq!(
                decoded.payload,
                serde_json::to_value(&error).expect("encode original cause")
            );
        }
        let replayed: PluginError =
            serde_json::from_slice(&serde_json::to_vec(&error).expect("encode plugin journal"))
                .expect("replay plugin journal");
        for error in [error, replayed] {
            if let PluginError::ProviderFailure {
                kind,
                code,
                retryable,
                terminal_reason,
                ..
            } = &error
            {
                let runtime =
                    RuntimeEffectControllerError::from(error.clone()).into_runtime_error();
                let recorded: crate::RuntimeError = serde_json::from_slice(
                    &serde_json::to_vec(&runtime).expect("encode provider cause"),
                )
                .expect("replay provider cause");
                assert_eq!(
                    recorded.cause,
                    Some(crate::RuntimeErrorCause::ProviderFailure {
                        failure_kind: *kind,
                        code: code.clone(),
                        retryable: *retryable,
                        terminal_reason: *terminal_reason,
                    })
                );
            }
            let postures = postures(&error);
            if postures
                .iter()
                .any(|(_, posture)| *posture != postures[0].1)
            {
                disagreements.push(format!("{error:?}: {postures:?}"));
            }
        }
    }
    assert!(
        disagreements.is_empty(),
        "{} disagreements:\n{}",
        disagreements.len(),
        disagreements.join("\n")
    );
}

/// A store error reaching a plugin port keeps its class: retryable exactly
/// when the identical operation may succeed again, never both, and terminal
/// exactly when the store's own classification is.
#[test]
fn a_store_error_keeps_its_class_across_the_plugin_boundary() {
    let mut disagreements = Vec::new();
    for index in 0..crate::StoreError::samples_for_testing().len() {
        let sample = || crate::StoreError::samples_for_testing().swap_remove(index);
        let name = sample().variant_name();
        let transient = sample().is_transient();
        let terminal = sample().runtime_error().is_terminal();
        let plugin = PluginError::from(sample());
        let replayed: PluginError =
            serde_json::from_slice(&serde_json::to_vec(&plugin).expect("encode plugin journal"))
                .expect("replay plugin journal");
        for plugin in [plugin, replayed] {
            if plugin.is_retryable() != transient || plugin.is_terminal() != terminal {
                disagreements.push(format!(
                    "{name}: store transient={transient} terminal={terminal}, plugin \
                     retryable={} terminal={} as {plugin:?}",
                    plugin.is_retryable(),
                    plugin.is_terminal(),
                ));
            }
        }
    }
    assert!(
        disagreements.is_empty(),
        "{} disagreements:\n{}",
        disagreements.len(),
        disagreements.join("\n")
    );
}
