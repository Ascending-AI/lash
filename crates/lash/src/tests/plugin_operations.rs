//! Plugin queries, commands and tasks through the facade, run by the core's
//! node over SQLite memory stores (FIG-5307; the scenarios FIG-5190 deleted
//! with the engine double).

use super::*;
use crate::plugins::{
    PluginCommand, PluginOperation, PluginOperationFailure, PluginOperationOutcome,
    PluginRuntimeEvent, PluginTask, SessionParam,
};
use crate::support::TurnInput;

use std::time::Duration;

macro_rules! operation {
    ($name:ident, $kind:ident, $wire:literal) => {
        struct $name;
        impl PluginOperation for $name {
            const NAME: &'static str = $wire;
            const DESCRIPTION: &'static str = "Plugin facade acceptance probe";
            const SESSION_PARAM: SessionParam = SessionParam::Required;
            type Args = String;
            type Output = String;
            type Error = String;
            const ERROR_TYPE: &'static str = Self::NAME;
            const ERROR_VERSION: lash_core::FormatVersion = lash_core::FormatVersion::ONE;
            fn error_class(_: &Self::Error) -> lash_sansio::PluginFailureClass {
                lash_sansio::PluginFailureClass::Terminal
            }
        }
        impl $kind for $name {}
    };
}
operation!(Command, PluginCommand, "accept.command");
operation!(Task, PluginTask, "accept.task");

/// Q2/L03: a host observes the operation's terminal through its Run, including
/// after the command settlement has already been published.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn operation_run_follow_returns_the_task_terminal() -> Result<()> {
    {
        let spec = lash_core::facade_support::PluginSpec::new()
            .with_plugin_task_typed::<Task, _, _>(|_, args| async move {
                Ok(PluginOperationOutcome::new(args))
            });
        let core = explicit_ephemeral_facets(LashCore::standard_builder(
            sqlite_memory_store_backend().await,
        ))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .plugin(Arc::new(StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial("accept"),
            spec,
        )))
        .build(crate::testing::runtime_lease_owner())?;
        let session = core
            .session(
                crate::SessionId::parse("operation-run-follow").expect("nonblank host identity"),
            )
            .created()
            .await
            .open()
            .await?;
        let receipt = session
            .admin()
            .commands()
            .submit(
                lash_core::facade_support::SessionCommand::RunPluginTask {
                    name: Task::NAME.into(),
                    args: serde_json::json!("terminal"),
                },
                "operation-run-follow",
            )
            .await?;
        session.admin().commands().settle(receipt.clone()).await?;
        let operation = lash_core::tool_run::OperationRun {
            session_id: receipt.session_id,
            operation_id: receipt.batch_id.to_string(),
        };
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            session.run(operation.run_id().into()).outcome(),
        )
        .await
        .expect("a completed operation Run must answer its follower")?;
        assert_eq!(result.run(), Some(&operation.run_id()));
        assert!(
            matches!(&result, crate::SendOutcome::OperationSettled { outcome, .. }
            if matches!(outcome.as_ref(), lash_core::runtime::PluginOperationCommandOutcome::Completed { output, .. }
                if output == &serde_json::json!("terminal")))
        );
        let durable = session.durable();
        drop(session);
        assert_eq!(
            durable
                .run(operation.run_id().into())
                .result()
                .await?
                .output,
            serde_json::json!("terminal")
        );
        Ok(())
    }
}

/// L03/L10: Run cancellation reaches the task, and a fresh operation with
/// identical input cannot adopt the cancelled owner's signal or result.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "FIG-5343: a running task's cancel answers Withdrawn and never reaches its handler"]
async fn operation_run_cancel_does_not_infect_a_fresh_operation() -> Result<()> {
    {
        let entered = Arc::new(tokio::sync::Notify::new());
        let notify = entered.clone();
        let first = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let spec = lash_core::facade_support::PluginSpec::new()
            .with_plugin_task_typed::<Task, _, _>(move |ctx, args| {
                let entered = notify.clone();
                let first = first.clone();
                async move {
                    assert!(matches!(
                        ctx.scoped_effect_controller.execution_scope(),
                        lash_core::ExecutionScope::SessionOperation { .. }
                    ));
                    if first.swap(false, std::sync::atomic::Ordering::SeqCst) {
                        entered.notify_one();
                        ctx.cancellation_token.cancelled().await;
                        return Err(String::from("cancelled"));
                    }
                    assert!(!ctx.cancellation_token.is_cancelled());
                    Ok(PluginOperationOutcome::new(args))
                }
            });
        let core = explicit_ephemeral_facets(LashCore::standard_builder(
            sqlite_memory_store_backend().await,
        ))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .plugin(Arc::new(StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial("accept"),
            spec,
        )))
        .build(crate::testing::runtime_lease_owner())?;
        let session = core
            .session(
                crate::SessionId::parse("operation-run-cancel").expect("nonblank host identity"),
            )
            .created()
            .await
            .open()
            .await?;
        let task = session
            .plugin_operations()
            .start_task::<Task>("same input".into(), "first")
            .await?;
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .expect("the engine entered the task");
        let receipt = task.cancel().await?;
        assert!(
            matches!(receipt, crate::CancelReceipt::Cancelled { .. }),
            "a running task's cancel reaches it: {receipt:?}"
        );
        let old_run = task.run().clone();
        let terminal = tokio::time::timeout(Duration::from_secs(5), task.outcome())
            .await
            .expect("cancel settles the operation")?;
        assert!(
            matches!(terminal, crate::SendOutcome::OperationSettled { outcome, .. }
            if matches!(outcome.as_ref(), lash_core::runtime::PluginOperationCommandOutcome::Cancelled))
        );
        let fresh = session
            .plugin_operations()
            .start_task::<Task>("same input".into(), "second")
            .await?;
        assert_ne!(fresh.run(), &old_run);
        let fresh_run = fresh.run().clone();
        assert_eq!(
            fresh.result().await?.output,
            serde_json::json!("same input")
        );
        assert!(matches!(
            session.run(fresh_run).cancel().await?,
            crate::CancelReceipt::UnknownOrRevoked
        ));
        assert_eq!(
            session.run(old_run).outcome().await?.status(),
            crate::TurnStatus::Cancelled
        );
        Ok(())
    }
}

/// L08: operation close ends its own lifetime scope, while a process granted
/// the session lifetime retains its independent registration and StartKey.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "FIG-5345: an operation's completion never closes its lifetime scope"]
async fn a_session_lifetime_process_survives_operation_completion() -> Result<()> {
    {
        let spec = lash_core::facade_support::PluginSpec::new()
            .with_plugin_task_typed::<Task, _, _>(|ctx, _| async move {
                let session = ctx.session_id.clone().unwrap();
                let scope = lash_core::ProcessOpScope::new(ctx.scoped_effect_controller);
                let cx = scope
                    .start_cx()
                    .map_err(|error| error.to_string())?
                    .expect("an operation is an opener");
                let request = lash_core::ProcessStartRequest::new(
                    lash_core::testing::held_engine_input(serde_json::Value::Null),
                    lash_core::ProcessOriginator::session(lash_core::SessionScope::new(
                        session.clone(),
                    )),
                    lash_core::lifetime::session_or_starter(&cx),
                )
                .with_host_start_key("operation-session-process:child");
                let process = ctx
                    .processes
                    .start_from_request(&session, request, scope)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(PluginOperationOutcome::new(process.process_id.to_string()))
            });
        let backend = sqlite_memory_store_backend().await;
        let registry = backend.process_registry();
        let core = explicit_ephemeral_facets(LashCore::standard_builder(backend))
            .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
            .plugin(lash_core::testing::process_engine_plugin_fixture())
            .plugin(Arc::new(StaticPluginFactory::new(
                lash_core::plugin::PluginDeclaration::initial("accept"),
                spec,
            )))
            .build(crate::testing::runtime_lease_owner())?;
        let session = core
            .session(
                crate::SessionId::parse("operation-session-process")
                    .expect("nonblank host identity"),
            )
            .created()
            .await
            .open()
            .await?;
        let run = session
            .plugin_operations()
            .start_task_raw(Task::NAME, serde_json::json!(""), "start")
            .await?;
        let operation = lash_core::tool_run::OperationRun::for_run_id(
            crate::SessionId::from("operation-session-process"),
            &lash_core::TurnId::from(run.run().clone()),
        )
        .unwrap();
        let result = run.result().await?;
        let process_id = serde_json::from_value::<lash_core::ProcessId>(result.output.clone())?;
        let record = registry.get_process(&process_id).await?.unwrap();
        assert!(!record.status().is_terminal());
        assert_eq!(
            record.lifetime.scope(),
            Some(&lash_core::ScopeId::session("operation-session-process"))
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while registry
            .get_parent_end_plan(&lash_core::ScopeId::Opener(operation.opener()))
            .await?
            .is_none()
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "operation completion closes its own scope"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !registry
                .get_process(&process_id)
                .await?
                .unwrap()
                .status()
                .is_terminal()
        );
        Ok(())
    }
}

#[derive(
    Clone,
    Debug,
    PartialEq,
    serde::Serialize,
    serde::Deserialize,
    lash_core::facade_support::JsonSchema,
    thiserror::Error,
)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[error("quota {quota} refuses the operation")]
struct QuotaRefusal {
    quota: u64,
    sources: Vec<PluginOperationFailure>,
}

struct FailureProbe;
impl PluginOperation for FailureProbe {
    const NAME: &'static str = "accept.failure";
    const DESCRIPTION: &'static str = "Typed failure transport probe";
    const SESSION_PARAM: SessionParam = SessionParam::Required;
    type Args = String;
    type Output = String;
    type Error = QuotaRefusal;
    const ERROR_TYPE: &'static str = "accept.quota";
    const ERROR_VERSION: lash_core::FormatVersion = lash_core::FormatVersion::ONE;
    fn error_class(_: &Self::Error) -> crate::plugins::PluginFailureClass {
        crate::plugins::PluginFailureClass::Terminal
    }
}
impl PluginCommand for FailureProbe {}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plugin_operation_failure_reaches_the_facade_as_a_typed_settlement() -> Result<()> {
    {
        use crate::plugins::{PluginDeclarationError, PluginFailureClass};
        use lash_core::{PluginError, PluginStateError, RuntimeError, RuntimeErrorCode};
        let mut sources = vec![
            PluginOperationFailure::from(PluginError::StoredDataCorrupt {
                record_kind: "plugin checkpoint".into(),
                message: "invalid bytes".into(),
            }),
            PluginOperationFailure::from(PluginError::State(PluginStateError::PublicationFenced {
                plugin: "probe".into(),
            })),
        ];
        for (kind, code, retryable) in [
            (
                lash_core::ProviderFailureKind::Quota,
                "host_spend_cap",
                false,
            ),
            (lash_core::ProviderFailureKind::Transport, "transport", true),
        ] {
            let original = PluginError::ProviderFailure {
                kind,
                code: Some(lash_core::FailureCode::provider(code)),
                retryable,
                terminal_reason: lash_core::LlmTerminalReason::ProviderError,
                message: "recorded provider failure".into(),
            };
            let envelope = PluginOperationFailure::from(original.clone());
            assert_eq!(serde_json::to_value(original)?, envelope.payload);
            assert_eq!(envelope.class, PluginFailureClass::Terminal);
            sources.push(envelope);
        }
        for refusal in [
            PluginDeclarationError::IdMismatch {
                factory: "accept".into(),
                declared: "other".into(),
            },
            PluginDeclarationError::NativeFormatNotWritable {
                plugin: "accept".into(),
                format_version: lash_core::FormatVersion::new(2).unwrap(),
            },
        ] {
            let original = PluginError::Declaration(refusal);
            let envelope = PluginOperationFailure::from(original.clone());
            assert_eq!(serde_json::to_value(original)?, envelope.payload);
            assert_eq!(envelope.class, PluginFailureClass::Terminal);
            sources.push(envelope);
        }
        for namespace in [
            lash_core::FormatNamespace::State,
            lash_core::FormatNamespace::Config,
        ] {
            let original = PluginError::Format(lash_core::FormatRefusal {
                plugin: "accept".into(),
                namespace,
                stored: lash_core::FormatVersion::new(2).unwrap(),
                readable: lash_core::FormatVersion::ONE,
            });
            let envelope = PluginOperationFailure::from(original.clone());
            assert_eq!(serde_json::to_value(original)?, envelope.payload);
            assert_eq!(envelope.class, PluginFailureClass::Terminal);
            sources.push(envelope);
        }
        for (code, class) in [
            (
                RuntimeErrorCode::RuntimeStore,
                PluginFailureClass::Retryable,
            ),
            (
                RuntimeErrorCode::SessionExecutionLeaseLost,
                PluginFailureClass::Redrivable,
            ),
            (
                RuntimeErrorCode::EffectReplayDivergence,
                PluginFailureClass::Parked,
            ),
        ] {
            let source = PluginOperationFailure::from(RuntimeError::new(code, "same diagnostic"));
            assert_eq!(source.class, class);
            let runtime = PluginError::Operation(Box::new(source.clone()))
                .into_turn_failure(RuntimeErrorCode::Plugin);
            assert_eq!(
                runtime.is_retryable(),
                class == PluginFailureClass::Retryable
            );
            assert!(!runtime.is_terminal());
            if class == PluginFailureClass::Parked {
                let park =
                    lash_core::store::ParkReason::of_error(&runtime).expect("typed plugin park");
                let bytes = serde_json::to_vec(&park)?;
                assert_eq!(
                    serde_json::from_slice::<lash_core::store::ParkReason>(&bytes)?,
                    park
                );
            }
            sources.push(source);
        }
        let unknown = PluginOperationFailure {
            error_type: "future.plugin.error".into(),
            error_version: std::num::NonZeroU32::new(19).unwrap(),
            payload: serde_json::json!({"kind":"future_refusal", "details":{"quota":17}, "items":[1,2]}),
            class: PluginFailureClass::Parked,
            code: (&RuntimeErrorCode::Plugin).into(),
            message: "same diagnostic".into(),
            origin: None,
        };
        assert_eq!(
            *FailureProbe::decode_error(unknown.clone()).unwrap_err(),
            unknown
        );
        sources.push(unknown);
        let expected = QuotaRefusal { quota: 7, sources };
        let returned = expected.clone();
        let spec = lash_core::facade_support::PluginSpec::new()
            .with_plugin_command_typed::<FailureProbe, _, _>(move |_, _| {
                let error = returned.clone();
                async move { Err(error) }
            });
        let core = explicit_ephemeral_facets(LashCore::standard_builder(
            sqlite_memory_store_backend().await,
        ))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .plugin(Arc::new(StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial("accept"),
            spec,
        )))
        .build(crate::testing::runtime_lease_owner())?;
        let session = core
            .session(
                crate::SessionId::parse("typed-plugin-settlement").expect("nonblank host identity"),
            )
            .created()
            .await
            .open()
            .await?;
        let error = async {
            session
                .plugin_operations()
                .run_command::<FailureProbe>(
                    "reject".into(),
                    "host:plugin_operations:run_command:451".to_string(),
                )
                .await?
                .settle_with(
                    &session.admin().commands(),
                    crate::testing::admin_fixture_outcome,
                )
                .await
        }
        .await
        .unwrap_err();
        assert!(error.is_terminal());
        assert!(!error.is_retryable());
        let EmbedError::Control(lash_core::facade_support::PluginOperationInvokeError::Failed(
            failure,
        )) = error
        else {
            panic!(
                "a settled plugin failure must retain its typed cause through the facade: {error:?}"
            );
        };
        assert_eq!(
            FailureProbe::decode_error(*failure.clone()).unwrap(),
            expected
        );
        let origin = failure.origin.as_ref().unwrap();
        assert_eq!(
            (
                &*origin.plugin_id,
                &*origin.operation,
                origin.behavior_revision.get()
            ),
            ("accept", FailureProbe::NAME, 1)
        );
        assert_eq!(
            FailureProbe::decode_error((*failure).clone()).unwrap(),
            expected
        );
        let unknown = failure.payload["sources"]
            .as_array()
            .unwrap()
            .last()
            .unwrap();
        assert_eq!(unknown["error_type"], "future.plugin.error");
        assert_eq!(unknown["payload"]["details"]["quota"], 17);
        Ok(())
    }
}

fn outcome(value: String, label: &str) -> PluginOperationOutcome<String> {
    PluginOperationOutcome::new(value.clone()).with_events(vec![PluginRuntimeEvent::Status {
        key: "accept-probe".into(),
        label: label.into(),
        detail: Some(value),
    }])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_scenario_plugin_reserved_source_key_refusal_is_typed() -> Result<()> {
    {
        let spec = lash_core::facade_support::PluginSpec::new()
            .with_plugin_command_typed::<Command, _, _>(|_, key| async move {
                Ok(
                    PluginOperationOutcome::new(String::new()).with_directives(vec![
                        crate::plugins::PluginRuntimeDirective::QueueTurn {
                            input: TurnInput::text("plugin input"),
                            source_key: Some(key),
                        },
                    ]),
                )
            });
        let core = explicit_ephemeral_facets(LashCore::standard_builder(
            sqlite_memory_store_backend().await,
        ))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .plugin(Arc::new(StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial("accept"),
            spec,
        )))
        .build(crate::testing::runtime_lease_owner())?;
        let session = core
            .session(
                crate::SessionId::parse("reserved-plugin-keys").expect("nonblank host identity"),
            )
            .created()
            .await
            .open()
            .await?;
        for key in ["command:refresh_tool_catalog:foreign"] {
            let refused = async {
                session
                    .plugin_operations()
                    .run_command::<Command>(key.into(), format!("reserved-source-command:{key}"))
                    .await?
                    .settle_with(
                        &session.admin().commands(),
                        crate::testing::admin_fixture_outcome,
                    )
                    .await
            }
            .await;
            let Err(EmbedError::Runtime(error)) = refused else {
                panic!("plugin refusal must reach the host typed");
            };
            assert_eq!(error.code.as_str(), "ingress_reserved_source_key");
            let recorded = serde_json::to_value(&error)?;
            assert_eq!(recorded["cause"]["source_key"], key);
            assert!(session.durable().pending_turn_inputs().await?.is_empty());
        }
        Ok(())
    }
}

/// FIG-5020 / Q2 / L03: start_task selects the host's output and error codecs;
/// its result retains receipt evidence and the complete typed failure cause.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_run_result_decodes_output_and_declared_error() -> Result<()> {
    {
        let spec = lash_core::facade_support::PluginSpec::new()
            .with_plugin_task_typed::<Task, _, _>(|_, args| async move {
                if args == "fail" {
                    return Err(String::from("quota:17"));
                }
                Ok(outcome(args, "typed"))
            });
        let core = explicit_ephemeral_facets(LashCore::standard_builder(
            sqlite_memory_store_backend().await,
        ))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .plugin(Arc::new(StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial("accept"),
            spec,
        )))
        .build(crate::testing::runtime_lease_owner())?;
        let session = core
            .session(crate::SessionId::parse("typed-task-result").expect("nonblank host identity"))
            .created()
            .await
            .open()
            .await?;
        let task = session
            .plugin_operations()
            .start_task::<Task>("value".into(), "typed-success")
            .await?;
        let result: crate::plugins::PluginOperationReceipt<String> = task.result().await?;
        assert_eq!(result.output, "value");
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].plugin_id, "accept");
        assert!(
            matches!(&result.events[0].value, PluginRuntimeEvent::Status { detail: Some(detail), .. } if detail == "value")
        );
        assert!(result.pending_turn_inputs.is_empty());
        // A host with an unrecognized codec keeps the registered operation's
        // original envelope; it never guesses at its type or version.
        struct UnknownCodec;
        impl PluginOperation for UnknownCodec {
            const NAME: &'static str = Task::NAME;
            const DESCRIPTION: &'static str = "Unknown host error codec";
            const SESSION_PARAM: SessionParam = SessionParam::Required;
            type Args = String;
            type Output = String;
            type Error = String;
            const ERROR_TYPE: &'static str = "future.accept.task";
            const ERROR_VERSION: lash_core::FormatVersion = lash_core::FormatVersion::ONE;
            fn error_class(_: &Self::Error) -> crate::plugins::PluginFailureClass {
                crate::plugins::PluginFailureClass::Terminal
            }
        }
        impl PluginTask for UnknownCodec {}
        let unknown = session
            .plugin_operations()
            .start_task::<UnknownCodec>("fail".into(), "unknown-codec")
            .await?;
        match unknown.result().await.unwrap_err() {
            crate::admin::PluginTaskResultError::Host(error) => match *error {
                EmbedError::Control(crate::plugins::PluginOperationInvokeError::Failed(
                    failure,
                )) => {
                    assert_eq!(failure.error_type, Task::ERROR_TYPE);
                    assert_eq!(failure.payload, serde_json::json!("quota:17"));
                    assert_eq!(Task::decode_error(*failure).unwrap(), "quota:17");
                }
                other => panic!("unknown codecs retain the original failure: {other:?}"),
            },
            other => panic!("an unknown error codec cannot decode: {other:?}"),
        }
        let task = session
            .plugin_operations()
            .start_task::<Task>("fail".into(), "typed-failure")
            .await?;
        let run = task.run().clone();
        match task.result().await.unwrap_err() {
            crate::admin::PluginTaskResultError::Failed { error, failure } => {
                assert_eq!(error, "quota:17");
                assert_eq!(failure.class, crate::plugins::PluginFailureClass::Terminal);
                assert_eq!(Task::decode_error(*failure.clone()).unwrap(), error);
                assert_eq!(failure.origin.as_ref().unwrap().operation, Task::NAME);
                let durable = session.durable();
                drop(session);
                // A raw reattachment retains the complete envelope and its codec
                // identity, including after the live session has gone away.
                match durable.run(run).result().await.unwrap_err() {
                    crate::admin::PluginTaskResultError::Failed {
                        error: raw,
                        failure: retained,
                    } => {
                        assert_eq!(raw, *failure);
                        assert_eq!(retained, failure);
                    }
                    other => panic!("raw task failure must retain its envelope: {other:?}"),
                }
            }
            other => panic!("a typed task must decode its declared error: {other:?}"),
        }
        Ok(())
    }
}
