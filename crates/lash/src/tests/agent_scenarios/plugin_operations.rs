use super::super::*;
use crate::plugins::{
    PluginCommand, PluginOperation, PluginOperationFailure, PluginOperationOutcome, PluginQuery,
    PluginRuntimeEvent, PluginTask, SessionParam,
};

use std::time::Duration;

const SEED: u64 = 0x504c5547;

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
operation!(Query, PluginQuery, "accept.query");
operation!(Command, PluginCommand, "accept.command");
operation!(Task, PluginTask, "accept.task");

/// Q2/L03: a host observes the operation's terminal through its Run, including
/// after the command settlement has already been published.
#[test]
fn operation_run_follow_returns_the_task_terminal() -> Result<()> {
    run_async_test_on_stack_budget("operation-run-follow", || async {
        let spec = lash_core::facade_support::PluginSpec::new()
            .with_plugin_task_typed::<Task, _, _>(|_, args| async move {
                Ok(PluginOperationOutcome::new(args))
            });
        let double = restate_double(SEED).await;
        let core = explicit_ephemeral_facets(LashCore::standard_builder(double.lash_backend()))
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
        let wire = result.to_remote(
            &operation.session_id,
            &lash_core::InputId::from(&operation.run_id()),
        );
        wire.validate()?;
        let decoded: lash_remote_protocol::RemoteSendOutcome =
            serde_json::from_slice(&serde_json::to_vec(&wire)?)?;
        assert_eq!(decoded, wire);
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
    })
}

/// L03/L10: Run cancellation reaches the task, and a fresh operation with
/// identical input cannot adopt the cancelled owner's signal or result.
#[test]
fn operation_run_cancel_does_not_infect_a_fresh_operation() -> Result<()> {
    run_async_test_on_stack_budget("operation-run-cancel", || async {
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
        let double = restate_double(SEED).await;
        let core = explicit_ephemeral_facets(LashCore::standard_builder(double.lash_backend()))
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
        assert!(matches!(
            task.cancel().await?,
            crate::CancelReceipt::OperationRequested {
                request: lash_core::runtime::PluginTaskCancelRequest::Requested,
                ..
            }
        ));
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
            crate::CancelReceipt::AlreadySettled { .. }
        ));
        assert_eq!(
            session.run(old_run).outcome().await?.status(),
            crate::TurnStatus::Cancelled
        );
        Ok(())
    })
}

/// L08: operation close ends its own lifetime scope, while a process granted
/// the session lifetime retains its independent registration and StartKey.
#[test]
fn a_session_lifetime_process_survives_operation_completion() -> Result<()> {
    run_async_test_on_stack_budget("operation-session-process", || async {
        let spec = lash_core::facade_support::PluginSpec::new()
            .with_plugin_task_typed::<Task, _, _>(|ctx, _| async move {
                let session = ctx.session_id.clone().unwrap();
                let scope = lash_core::ProcessOpScope::new(ctx.scoped_effect_controller);
                let cx = scope
                    .start_cx()
                    .map_err(|error| error.to_string())?
                    .expect("an operation is an opener");
                let request = lash_core::ProcessStartRequest::external(
                    lash_core::ProcessOriginator::session(lash_core::SessionScope::new(
                        session.clone(),
                    )),
                    serde_json::Value::Null,
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
        let double = restate_double(SEED).await;
        let backend = double.lash_backend();
        let registry = backend.process_registry();
        let core = explicit_ephemeral_facets(LashCore::standard_builder(backend))
            .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
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
            SessionId::from("operation-session-process"),
            run.run().stored(),
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
    })
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

#[test]
fn plugin_operation_failure_reaches_the_facade_as_a_typed_settlement() -> Result<()> {
    run_async_test_on_stack_budget("typed-plugin-settlement", || async {
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
        let journal_failure = Arc::new(std::sync::Mutex::new(None::<PluginOperationFailure>));
        let response_failure = Arc::clone(&journal_failure);
        let spec = lash_core::facade_support::PluginSpec::new()
            .with_assistant_response(
                crate::hook_key!("assistant-response-1"),
                None,
                Arc::new(move |_| {
                    let failure = response_failure
                        .lock()
                        .unwrap()
                        .clone()
                        .expect("capture the operation failure before the journal turn");
                    Box::pin(async move { Err(PluginError::Operation(Box::new(failure))) })
                }),
            )
            .with_plugin_command_typed::<FailureProbe, _, _>(move |_, _| {
                let error = returned.clone();
                async move { Err(error) }
            });
        let double = restate_double(SEED).await;
        let core = explicit_ephemeral_facets(LashCore::standard_builder(double.lash_backend()))
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
        let error = session
            .plugin_operations()
            .run_command::<FailureProbe>("reject".into())
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
        *journal_failure.lock().unwrap() = Some(*failure.clone());
        session
            .send(crate::TurnInput::text("journal the typed failure"))
            .id(crate::TurnId::fixture("typed-plugin-journal"))
            .output()
            .await?;
        let records: Vec<serde_json::Value> = double
            .server()
            .invocations()
            .into_iter()
            .flat_map(|invocation| double.server().journal(&invocation.id).unwrap_or_default())
            .filter_map(|entry| entry.run_completion()?.ok())
            .filter_map(|bytes| serde_json::from_slice(&bytes).ok())
            .collect();
        let journaled =
            records
                .iter()
                .find_map(|record| {
                    let error: lash_core::RuntimeEffectControllerError =
                        serde_json::from_value(record.get("outcome")?.get("Err")?.clone()).ok()?;
                    match error.cause {
                        Some(lash_core::RuntimeErrorCause::PluginOperation {
                            failure: recorded,
                        }) if recorded == failure => Some(recorded),
                        _ => None,
                    }
                })
                .expect("the engine journal retains the complete original typed failure");
        assert_eq!(journaled, failure);
        let batch_id = records
            .iter()
            .filter_map(|record| {
                serde_json::from_value::<lash_core::RuntimeEffectOutcome>(
                    record.get("outcome")?.get("Ok")?.clone(),
                )
                .ok()
            })
            .find_map(|outcome| match outcome {
                lash_core::RuntimeEffectOutcome::ReadSessionCommandRun { batches } => batches
                    .into_iter()
                    .find(|batch| match &batch.payload {
                        crate::persistence::QueuedWorkPayload::SessionCommand { command } => {
                            matches!(
                                command.as_ref(),
                                lash_core::runtime::SessionCommand::RunPluginCommand { name, .. }
                                    if name == FailureProbe::NAME
                            )
                        }
                        _ => false,
                    })
                    .map(|batch| batch.batch_id.as_str().to_owned()),
                _ => None,
            })
            .expect("the engine journal records this command's admission identity");
        let store = lash_core::runtime::live_session_view(
            &core.store_factory,
            &SessionId::from("typed-plugin-settlement"),
        )
        .await?
        .unwrap();
        let receipt = store
            .queued_work_batch_completion(&batch_id)
            .await?
            .expect("the completed command retains its SQLite receipt");
        let outcome = &receipt
            .command_outcomes
            .iter()
            .find(|(id, _)| id.as_str() == batch_id)
            .expect("the receipt names the journaled command")
            .1;
        let lash_core::runtime::SessionCommandOutcome::PluginOperation {
            outcome: lash_core::runtime::PluginOperationCommandOutcome::Failed { failure: stored },
        } = outcome
        else {
            panic!("stored failure outcome");
        };
        assert_eq!(stored, &failure);
        let issue = lash_core::facade_support::TurnIssue {
            severity: lash_core::facade_support::TurnIssueSeverity::Advisory,
            kind: lash_core::TurnFailureKind::Plugin,
            code: Some(failure.code.clone()),
            terminal_reason: None,
            message: failure.message.clone(),
            raw: None,
            retryable: Some(false),
            provider_failure_kind: None,
            plugin_failures: vec![*failure],
        };
        let wire: lash_remote_protocol::RemoteTurnIssue = issue.into();
        let decoded: lash_remote_protocol::RemoteTurnIssue =
            serde_json::from_slice(&serde_json::to_vec(&wire)?)?;
        assert_eq!(decoded, wire);
        assert_eq!(
            FailureProbe::decode_error(decoded.plugin_failures[0].clone()).unwrap(),
            expected
        );
        let unknown = decoded.plugin_failures[0].payload["sources"]
            .as_array()
            .unwrap()
            .last()
            .unwrap();
        assert_eq!(unknown["error_type"], "future.plugin.error");
        assert_eq!(unknown["payload"]["details"]["quota"], 17);
        Ok(())
    })
}

fn outcome(value: String, label: &str) -> PluginOperationOutcome<String> {
    PluginOperationOutcome::new(value.clone()).with_events(vec![PluginRuntimeEvent::Status {
        key: "accept-probe".into(),
        label: label.into(),
        detail: Some(value),
    }])
}

#[test]
fn agent_scenario_plugin_reserved_source_key_refusal_is_typed() -> Result<()> {
    run_async_test_on_stack_budget("reserved-plugin-keys", || async {
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
        let double = restate_double(SEED).await;
        let core = explicit_ephemeral_facets(LashCore::standard_builder(double.lash_backend()))
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
        for key in [
            "command:refresh_tool_catalog:foreign",
            "process:foreign:event:1:wake",
        ] {
            let refused = session
                .plugin_operations()
                .run_command::<Command>(key.into())
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
    })
}

#[test]
pub(super) fn agent_scenario_plugin_task_query_command() -> Result<()> {
    run_async_test_on_stack_budget("plugin-operations", || async {
        let entered = Arc::new(tokio::sync::Notify::new());
        let task_entered = entered.clone();
        let spec = lash_core::facade_support::PluginSpec::new()
            .with_plugin_query_typed::<Query, _, _>(|ctx, args| async move {
                assert_eq!(ctx.session_id.as_deref(), Some("plugin-accept"));
                Ok(format!("query:{args}"))
            })
            .with_plugin_command_typed::<Command, _, _>(|_, args| async move {
                Ok(outcome(format!("command:{args}"), "recorded"))
            })
            .with_plugin_task_typed::<Task, _, _>(move |ctx, args| {
                let entered = task_entered.clone();
                async move {
                    if args == "cancel-739" {
                        // This task ends only on its cancellation.
                        entered.notify_one();
                        ctx.cancellation_token.cancelled().await;
                        return Err(String::from("cancelled:cancel-739"));
                    }
                    Ok(outcome(format!("task:{args}"), "completed"))
                }
            });
        let double = restate_double(SEED).await;
        let writes = lash_core::testing::checkpoint_observer::CheckpointWriteCollector::default();
        let observed = writes.clone();
        let backend = DecoratedBackend::over(double.lash_backend())
            .session_store_factory(move |inner| {
                Arc::new(
                    lash_core::testing::checkpoint_observer::ObservedDeploymentStore::new(
                        inner, observed,
                    ),
                )
            })
            .into();
        let core = explicit_ephemeral_facets(LashCore::standard_builder(backend))
            .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
            .plugin(Arc::new(StaticPluginFactory::new(
                lash_core::plugin::PluginDeclaration::initial("accept"),
                spec,
            )))
            .build(crate::testing::runtime_lease_owner())?;
        // Queries read an already published plugin view; creation leaves it
        // cold until an engine admission publishes that view.
        let initializing = core
            .session(crate::SessionId::parse("plugin-accept").expect("nonblank host identity"))
            .created()
            .await
            .open()
            .await?;
        let admission = initializing
            .admin()
            .commands()
            .submit(
                lash_core::facade_support::SessionCommand::RefreshToolCatalog {
                    reason: "publish-query-view".into(),
                },
                "publish-query-view",
            )
            .await?;
        initializing.admin().commands().settle(admission).await?;
        drop(initializing);
        let session = core
            .session(crate::SessionId::parse("plugin-accept").expect("nonblank host identity"))
            .open()
            .await?;
        let ops = session.plugin_operations();
        let probe = || "cobalt-583".to_string();
        let before = session.admin().state().persist_current().await?;
        assert_eq!(
            ops.query::<Query>(probe())
                .await
                .expect("initial query services"),
            "query:cobalt-583"
        );
        assert_eq!(
            session
                .admin()
                .state()
                .persist_current()
                .await?
                .session_graph
                .leaf_node_id,
            before.session_graph.leaf_node_id,
            "query must not append history"
        );
        let command = ops.run_command::<Command>(probe()).await?;
        let task = ops.run_task::<Task>(probe()).await?;
        for (receipt, output, label) in [
            (command, "command:cobalt-583", "recorded"),
            (task, "task:cobalt-583", "completed"),
        ] {
            assert_eq!(receipt.output, output);
            assert!(receipt.pending_turn_inputs.is_empty());
            assert_eq!(receipt.events.len(), 1);
            assert_eq!(receipt.events[0].plugin_id, "accept");
            assert!(matches!(&receipt.events[0].value,
                PluginRuntimeEvent::Status { key, label: actual, detail }
                if key == "accept-probe" && actual == label && detail.as_deref() == Some(output)));
        }
        let before_cancel = session.admin().state().persist_current().await?;
        let persisted_events: Vec<_> = before_cancel
            .session_graph
            .nodes
            .iter()
            .filter_map(|node| match node.event() {
                Some(lash_core::SessionHistoryRecord::Protocol(event))
                    if event.plugin_id == "lash.plugin_runtime" =>
                {
                    Some(event.payload.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            persisted_events,
            vec![
                serde_json::json!({"plugin_id": "accept", "event": {
                    "kind": "status", "key": "accept-probe", "label": "recorded", "detail": "command:cobalt-583"
                }}),
                serde_json::json!({"plugin_id": "accept", "event": {
                    "kind": "status", "key": "accept-probe", "label": "completed", "detail": "task:cobalt-583"
                }}),
            ],
            "both owned events persist in operation order"
        );
        // The task is running in the shift, so the cancel no longer
        // withdraws it (FIG-4202): it reaches the task through its cancel
        // signal, and the shift settles the command cancelled (FIG-4391).
        let cancel = crate::CancellationToken::new();
        let task_cancel = cancel.clone();
        let running_ops = ops.clone();
        let running = tokio::spawn(async move {
            running_ops
                .run_task_with_cancel::<Task>("cancel-739".into(), task_cancel)
                .await
        });
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .expect("task entered");
        cancel.cancel();
        let error = tokio::time::timeout(Duration::from_secs(5), running)
            .await
            .expect("the cancelled task settles")
            .expect("task does not panic")
            .expect_err("the cancelled task settles cancelled");
        assert!(
            matches!(
                error,
                EmbedError::Send(ref error) if matches!(error.as_ref(), crate::SendError::NotSettled { status: crate::TurnStatus::Cancelled, .. })
            ),
            "a host cancel of an admitted task settles it with the typed cancel: {error:?}"
        );
        assert_eq!(
            session
                .admin()
                .state()
                .persist_current()
                .await?
                .session_graph
                .leaf_node_id,
            before_cancel.session_graph.leaf_node_id,
            "nothing of the cancelled task commits"
        );
        assert_eq!(
            ops.query::<Query>(probe())
                .await
                .expect("query services after operation cancellation"),
            "query:cobalt-583",
            "writer released after the cancelled settlement"
        );
        super::transcript::assert_typed_checkpoint_transcript(&writes.events());
        Ok(())
    })
}

/// FIG-5020 / Q2 / L03: start_task selects the host's output and error codecs;
/// its result retains receipt evidence and the complete typed failure cause.
#[test]
fn task_run_result_decodes_output_and_declared_error() -> Result<()> {
    run_async_test_on_stack_budget("typed-task-result", || async {
        let spec = lash_core::facade_support::PluginSpec::new()
            .with_plugin_task_typed::<Task, _, _>(|_, args| async move {
                if args == "fail" {
                    return Err(String::from("quota:17"));
                }
                Ok(outcome(args, "typed"))
            });
        let double = restate_double(SEED).await;
        let core = explicit_ephemeral_facets(LashCore::standard_builder(double.lash_backend()))
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
    })
}
