#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::output::{
        MAX_OUTPUT, ReaderSignals, SPILL_OUTPUT_THRESHOLD, ShellOutputBuffer,
        clean_terminal_output, render_buffer_output, spawn_async_reader, take_buffer_output,
    };
    use lash_core::ProcessRegistry as _;
    use lash_sansio::sync::MutexExt;
    use serde_json::json;
    use std::fs;
    use std::sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::io::AsyncWriteExt;
    use tokio::sync::{Barrier, Notify};

    fn test_shell() -> StaticToolProvider<StandardShell> {
        shell_provider(StandardShell::new().with_cwd("/"))
    }

    async fn run(
        shell: &StaticToolProvider<StandardShell>,
        name: &str,
        args: &serde_json::Value,
    ) -> ToolOutcome {
        lash_core::testing::run_tool(shell, name, args).await
    }

    async fn run_with_context(
        shell: &StaticToolProvider<StandardShell>,
        name: &str,
        args: &serde_json::Value,
        context: &lash_core::ToolContext<'_>,
    ) -> ToolOutcome {
        if name == "start_command" && context.async_process_id().is_some() {
            let internal = lash_core::InternalProcessContext::__for_testing(context);
            return shell
                .execute_internal(lash_core::InternalProcessToolCall {
                    name: "run_start_command",
                    args,
                    context: &internal,
                })
                .await;
        }
        if matches!(name, "start_command" | "write_stdin") {
            let attempt = lash_core::AttemptContext::__for_testing(context, "shell-test-scope");
            let outcome = shell
                .execute_attempt(ToolCall {
                    name,
                    args,
                    context: &attempt,
                })
                .await;
            let lash_core::ToolAttemptOutcome::Done { result, intents } = outcome else {
                panic!("shell leaf test unexpectedly returned Pending");
            };
            let internal = lash_core::InternalProcessContext::__for_testing(context);
            for intent in intents.intents {
                match intent {
                    lash_core::ToolIntent::StartProcess(intent) => {
                        internal
                            .processes()
                            .start(intent.request)
                            .await
                            .expect("test drains StartProcess intent");
                    }
                    lash_core::ToolIntent::SignalProcess(intent) => {
                        let _ = internal
                            .processes()
                            .signal(&intent.process_id, &intent.signal_name, intent.payload)
                            .await;
                    }
                    other => panic!("unexpected shell intent: {:?}", other.kind()),
                }
            }
            return ToolOutcome::from_output(result.into_output());
        }
        shell
            .execute(ToolCall {
                name,
                args,
                context: &lash_core::AttemptContext::__for_testing(context, "shell-test-scope"),
            })
            .await
    }

    #[tokio::test]
    async fn missing_command_is_a_structured_invalid_request() {
        let shell = test_shell();
        let result = run(&shell, "exec_command", &json!({})).await;

        let lash_core::ToolCallOutcome::Failure(failure) = &result.as_output().outcome else {
            panic!("missing command must fail");
        };
        assert_eq!(failure.class, lash_core::ToolFailureClass::InvalidRequest);
        assert_eq!(failure.code, "invalid_tool_args");
        assert!(failure.message.contains("cmd"));
        assert_eq!(failure.retry, lash_core::ToolRetryStatus::Never);
    }

    #[tokio::test]
    async fn shell_spawn_failure_is_structured_io() {
        let shell = test_shell();
        let result = run(
            &shell,
            "exec_command",
            &json!({"cmd": "true", "shell": "/definitely/not/a/shell"}),
        )
        .await;

        let lash_core::ToolCallOutcome::Failure(failure) = &result.as_output().outcome else {
            panic!("missing shell executable must fail");
        };
        assert_eq!(failure.class, lash_core::ToolFailureClass::Io);
        assert_eq!(failure.code, "spawn_shell_command_failed");
        assert_eq!(failure.retry, lash_core::ToolRetryStatus::Never);
    }

    fn async_process_context(
        process_id: &str,
        cancel: CancellationToken,
    ) -> lash_core::ToolContext<'static> {
        lash_core::testing::mock_tool_context().with_async_process(process_id, cancel)
    }

    fn async_process_context_with_events(
        process_id: &str,
        registry: Arc<dyn lash_core::ProcessRegistry>,
        execution_write_authority: lash_core::ProcessExecutionWriteAuthority,
        cancel: CancellationToken,
    ) -> lash_core::ToolContext<'static> {
        let context = lash_core::testing::mock_tool_context().with_async_process(process_id, cancel);
        lash_core::ToolContext::with_process_events_for_testing(
            context,
            process_id,
            registry,
            execution_write_authority,
        )
    }

    #[derive(Clone, Default)]
    struct TestProcessService {
        registry: Arc<lash_core::TestLocalProcessRegistry>,
    }

    impl TestProcessService {
        fn registry(&self) -> Arc<lash_core::TestLocalProcessRegistry> {
            Arc::clone(&self.registry)
        }
    }

    #[async_trait::async_trait]
    impl lash_core::ProcessService for TestProcessService {
        async fn start_from_recorded_intent(
            &self,
            session_id: &str,
            request: lash_core::ProcessStartRequest,
            scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::ProcessHandleView, PluginError> {
            self.start_from_request(session_id, request, scope).await
        }

        async fn finish_recorded_intent_parent(
            &self,
            _session_id: &str,
            _identity: lash_core::ToolIntentIdentity,
            _process_id: String,
            _policy: lash_core::ProcessParentEndPolicy,
            _reason: String,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::ToolIntentParentEndOutcome, PluginError> {
            Err(PluginError::Session(
                "recorded parent end is unavailable in this shell fixture".to_string(),
            ))
        }

        async fn start_from_request(
            &self,
            session_id: &str,
            request: lash_core::ProcessStartRequest,
            scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::ProcessHandleView, PluginError> {
            let env_ref = request
                .env_spec
                .as_ref()
                .map(lash_core::ProcessExecutionEnvSpec::stable_ref)
                .transpose()
                .map_err(|err| {
                    PluginError::Session(format!("failed to hash test process env: {err}"))
                })?;
            let observers = request.observers.clone();
            let registration = request.into_registration(env_ref);
            let record = self
                .start(
                    session_id,
                    registration,
                    lash_core::ProcessStartOptions::new().with_initial_observers(observers),
                    scope,
                )
                .await?;
            let definition = record.identity.definition.clone();
            Ok(lash_core::ProcessHandleView::new(
                record.id,
                record.identity,
                record.status,
            )
            .with_definition(definition))
        }

        async fn start(
            &self,
            _session_id: &str,
            registration: lash_core::ProcessRegistration,
            options: lash_core::ProcessStartOptions,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::ProcessRecord, PluginError> {
            self.registry
                .register_process_with_observers(registration, &options.initial_observers)
                .await
        }

        async fn complete_external(
            &self,
            session_id: &str,
            process_id: &str,
            await_output: lash_core::ProcessAwaitOutput,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::ProcessCompletionOutcome, PluginError> {
            if !self
                .registry
                .is_observer(session_id, process_id)
                .await?
            {
                return Err(PluginError::Session(format!(
                    "process handle `{process_id}` is not visible in this session"
                )));
            }
            match self.registry.get_process(process_id).await? {
                Some(record)
                    if record.disposition != lash_core::RecoveryContract::ExternallyOwned =>
                {
                    return Err(PluginError::Session(format!(
                        "process `{process_id}` is not externally-owned"
                    )));
                }
                None => {
                    return Err(PluginError::Session(format!(
                        "unknown process `{process_id}`"
                    )));
                }
                Some(_) => {}
            }
            self.registry
                .complete_process(
                    process_id,
                    await_output,
                    lash_core::ProcessCompletionAuthority::external_owner(),
                )
                .await
        }

        async fn report_caller_departure(
            &self,
            session_id: &str,
            process_id: &str,
        ) -> Result<lash_core::ProcessRecord, PluginError> {
            if !self.registry.is_observer(session_id, process_id).await? {
                return Err(PluginError::Session(format!(
                    "process handle `{process_id}` is not visible in this session"
                )));
            }
            self.registry.record_caller_departure(process_id).await
        }

        async fn await_process(
            &self,
            process_id: &str,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::ProcessAwaitOutput, PluginError> {
            let registry: Arc<dyn lash_core::ProcessRegistry> = self.registry.clone();
            lash_core::facade_support::ProcessAwaiter::polling(registry)
                .await_terminal(process_id)
                .await
        }

        async fn list_visible(
            &self,
            session_id: &str,
            mode: lash_core::ProcessListMode,
            scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<Vec<lash_core::ProcessRecord>, PluginError> {
            let _ = scope;
            match mode {
                lash_core::ProcessListMode::Live => {
                    self.registry.list_live_observed_by(session_id).await
                }
                lash_core::ProcessListMode::All => {
                    self.registry.list_observed_by(session_id).await
                }
            }
        }

        async fn validate_visible(
            &self,
            session_id: &str,
            process_ids: &[String],
            scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<(), PluginError> {
            let _ = scope;
            for process_id in process_ids {
                if !self
                    .registry
                    .is_observer(session_id, process_id)
                    .await?
                {
                    return Err(PluginError::Session(format!(
                        "process handle `{process_id}` is not live or visible in this session"
                    )));
                }
            }
            Ok(())
        }

        async fn cancel(
            &self,
            _session_id: &str,
            process_id: &str,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::ProcessRecord, PluginError> {
            self.registry
                .append_event(
                    process_id,
                    lash_core::ProcessEventAppendRequest::cancel_requested(
                        process_id,
                        Some("requested by test".to_string()),
                    ),
                )
                .await?;
            self.registry
                .get_process(process_id)
                .await?
                .ok_or_else(|| PluginError::Session(format!("unknown process `{process_id}`")))
        }

        async fn cancel_recorded_intent(
            &self,
            _session_id: &str,
            process_id: &str,
            reason: Option<String>,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::ProcessRecord, PluginError> {
            self.registry
                .append_event(
                    process_id,
                    lash_core::ProcessEventAppendRequest::cancel_requested(process_id, reason),
                )
                .await?;
            self.registry
                .get_process(process_id)
                .await?
                .ok_or_else(|| PluginError::Session(format!("unknown process `{process_id}`")))
        }

        async fn signal_possessed(
            &self,
            session_id: &str,
            process_id: &str,
            signal_name: String,
            signal_id: String,
            payload: serde_json::Value,
            scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::ProcessEvent, PluginError> {
            // Mirror the real service: a signal only targets a live, visible
            // handle, so a terminal row (e.g. a detached command) is rejected.
            let _ = scope;
            let visible = self
                .registry
                .list_live_observed_by(session_id)
                .await?
                .into_iter()
                .any(|record| record.id == process_id);
            if !visible {
                return Err(PluginError::Session(format!(
                    "process handle `{process_id}` is not live or visible in this session"
                )));
            }
            let event_type = lash_core::facade_support::process_signal_event_type(&signal_name)?;
            self.registry
                .append_event(
                    process_id,
                    lash_core::ProcessEventAppendRequest::new(event_type, payload).with_replay_key(
                        format!("process:{process_id}:signal.{signal_name}:{signal_id}"),
                    ),
                )
                .await
                .map(|result| result.event)
        }

        async fn signal_recorded_intent(
            &self,
            _session_id: &str,
            process_id: &str,
            signal_name: String,
            signal_id: String,
            payload: serde_json::Value,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::ProcessEvent, PluginError> {
            if self.registry.get_process(process_id).await?.is_none() {
                return Err(PluginError::ProcessNotVisible {
                    process_id: process_id.to_string(),
                });
            }
            let event_type = lash_core::facade_support::process_signal_event_type(&signal_name)?;
            self.registry
                .append_event(
                    process_id,
                    lash_core::ProcessEventAppendRequest::new(event_type, payload).with_replay_key(
                        format!("process:{process_id}:signal.{signal_name}:{signal_id}"),
                    ),
                )
                .await
                .map(|result| result.event)
        }

        async fn emit_event_recorded_intent(
            &self,
            _session_id: &str,
            process_id: &str,
            event_type: String,
            replay_key: String,
            payload: serde_json::Value,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::ProcessEvent, PluginError> {
            self.registry
                .append_event(
                    process_id,
                    lash_core::ProcessEventAppendRequest::new(event_type, payload)
                        .with_replay_key(replay_key),
                )
                .await
                .map(|result| result.event)
        }

        async fn transfer(
            &self,
            _from_session_id: &str,
            _to_session_id: &str,
            _process_ids: Vec<String>,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<(), PluginError> {
            Ok(())
        }

    }

    fn context_with_processes(
        service: Arc<TestProcessService>,
        tool_call_id: &str,
    ) -> lash_core::ToolContext<'static> {
        let host = Arc::new(lash_core::testing::MockSessionManager::default());
        let processes: Arc<dyn lash_core::ProcessService> = service;
        lash_core::ToolContext::__for_testing(
            "test-session".to_string(),
            host.clone(),
            host.clone(),
            host,
            processes,
            Arc::new(lash_core::facade_support::SessionAttachmentStore::in_memory()),
            lash_core::facade_support::DirectCompletionClient::from_fn(|_, _| {
                Err(lash_core::PluginError::Session(
                    "direct completions are unavailable in shell tests".to_string(),
                ))
            }),
            Some(tool_call_id.to_string()),
        )
    }

    async fn register_signal_target(
        registry: &lash_core::TestLocalProcessRegistry,
        process_id: &str,
    ) {
        register_signal_target_with_disposition(
            registry,
            process_id,
            lash_core::RecoveryContract::ExternallyOwned,
        )
        .await;
    }

    async fn register_executable_signal_target(
        registry: &lash_core::TestLocalProcessRegistry,
        process_id: &str,
    ) {
        register_signal_target_with_disposition(
            registry,
            process_id,
            lash_core::RecoveryContract::OwnerBound,
        )
        .await;
    }

    async fn register_signal_target_with_disposition(
        registry: &lash_core::TestLocalProcessRegistry,
        process_id: &str,
        disposition: lash_core::RecoveryContract,
    ) {
        registry
            .register_process(
                lash_core::ProcessRegistration::new(
                    process_id,
                    lash_core::ProcessInput::External {
                        metadata: serde_json::json!({}),
                    },
                    disposition,
                    lash_core::ProcessProvenance::host(),
                )
                .with_extra_event_types([shell_signal_event_type()]),
            )
            .await
            .expect("register process");
        registry
            .add_observer(
                "test-session",
                process_id,
                lash_core::ProcessObserverBy::host("shell-test"),
            )
            .await
            .expect("observe process");
    }

    async fn claim_signal_target_execution(
        registry: &lash_core::TestLocalProcessRegistry,
        process_id: &str,
    ) -> lash_core::ProcessExecutionWriteAuthority {
        let lease = registry
            .claim_process_lease(
                process_id,
                &lash_core::LeaseOwnerIdentity::opaque(
                    format!("shell-test:{process_id}"),
                    "command-execution",
                ),
                60_000,
            )
            .await
            .expect("claim shell process lease")
            .acquired()
            .expect("shell process lease is available");
        let authority = lash_core::ProcessExecutionWriteAuthority::lease(lease.clone());
        registry
            .record_first_started_with_authority(
                process_id,
                lash_core::ProcessStarted {
                    owner: lease.owner.clone(),
                    fencing_token: lease.fencing_token,
                    attempt: 1,
                    started_at_ms: 1,
                },
                &authority,
            )
            .await
            .expect("record shell process start");
        authority
    }

    #[tokio::test]
    async fn exec_command_returns_exit_code_when_command_finishes() {
        let shell = test_shell();
        let result = run(&shell, "exec_command", &json!({"cmd": "echo hello"})).await;
        assert!(result.is_success());
        assert!(result.value_for_projection().get("session_id").is_none());
        assert_eq!(result.value_for_projection()["status"], "completed");
        assert_eq!(result.value_for_projection()["done"], true);
        assert_eq!(result.value_for_projection()["running"], false);
        assert_eq!(result.value_for_projection()["exit_code"], 0);
        assert!(
            result.value_for_projection()["wall_time_seconds"]
                .as_f64()
                .is_some()
        );
        assert!(
            result.value_for_projection()["output"]
                .as_str()
                .unwrap()
                .contains("hello")
        );
    }

    #[tokio::test]
    async fn exec_command_waits_for_process_exit() {
        let shell = shell_provider(StandardShell::new().with_cwd("/"));
        let result = run(
            &shell,
            "exec_command",
            &json!({"cmd": "sleep 0.05; echo done"}),
        )
        .await;
        assert!(result.is_success(), "{}", result.value_for_projection());
        assert!(result.value_for_projection().get("session_id").is_none());
        assert_eq!(result.value_for_projection()["status"], "completed");
        assert_eq!(result.value_for_projection()["done"], true);
        assert_eq!(result.value_for_projection()["exit_code"], 0);
        assert!(
            result.value_for_projection()["output"]
                .as_str()
                .unwrap()
                .contains("done")
        );
    }

    #[tokio::test]
    async fn exec_command_runs_without_a_tty() {
        let shell = test_shell();
        let result = run(
            &shell,
            "exec_command",
            &json!({"cmd": "if [ -t 0 ] || [ -t 1 ] || [ -t 2 ]; then echo tty; exit 1; else echo no-tty; fi"}),
        )
        .await;

        assert!(result.is_success(), "{}", result.value_for_projection());
        assert_eq!(result.value_for_projection()["exit_code"], 0);
        assert_eq!(
            result.value_for_projection()["output"]
                .as_str()
                .unwrap()
                .trim(),
            "no-tty"
        );
    }

    #[tokio::test]
    async fn exec_command_closes_stdin() {
        let shell = test_shell();
        let result = run(
            &shell,
            "exec_command",
            &json!({"cmd": "python3 -c 'import sys; print(sys.stdin.read() == \"\")'"}),
        )
        .await;

        assert!(result.is_success(), "{}", result.value_for_projection());
        assert_eq!(
            result.value_for_projection()["output"]
                .as_str()
                .unwrap()
                .trim(),
            "True"
        );
    }

    #[tokio::test]
    async fn exec_command_captures_stdout_and_stderr() {
        let shell = test_shell();
        let result = run(
            &shell,
            "exec_command",
            &json!({"cmd": "echo stdout-line; echo stderr-line >&2"}),
        )
        .await;

        assert!(result.is_success(), "{}", result.value_for_projection());
        let result_value = result.value_for_projection();
        let output = result_value["output"].as_str().unwrap();
        assert!(output.contains("stdout-line"), "{output}");
        assert!(output.contains("stderr-line"), "{output}");
    }

    #[test]
    fn shell_output_marks_truncation_when_start_offset_is_nonzero() {
        let buffer = Arc::new(StdMutex::new(ShellOutputBuffer::default()));
        buffer
            .lock_recover()
            .append("nonzero-start", &vec![b'x'; MAX_OUTPUT + 1]);

        let (output, _, full_output_path) =
            render_buffer_output("nonzero-start", &buffer, None);

        assert!(output.ends_with("\n[truncated]"));
        assert!(full_output_path.is_some());
    }

    #[tokio::test]
    async fn shell_output_drains_stdout_stderr_during_incremental_reads() {
        let buffer = Arc::new(StdMutex::new(ShellOutputBuffer::default()));
        let output_notify = Arc::new(Notify::new());
        let reader_died = Arc::new(AtomicBool::new(false));
        let (mut stdout_writer, stdout_reader) = tokio::io::duplex(64);
        let (mut stderr_writer, stderr_reader) = tokio::io::duplex(64);
        let stdout_reader = spawn_async_reader(
            "concurrent-drain".to_string(),
            stdout_reader,
            Arc::clone(&buffer),
            ReaderSignals::new(Arc::clone(&output_notify), Arc::clone(&reader_died)),
        );
        let stderr_reader = spawn_async_reader(
            "concurrent-drain".to_string(),
            stderr_reader,
            Arc::clone(&buffer),
            ReaderSignals::new(Arc::clone(&output_notify), Arc::clone(&reader_died)),
        );
        let first_chunks_drained = Arc::new(Barrier::new(3));
        let stdout_writer_task = {
            let first_chunks_drained = Arc::clone(&first_chunks_drained);
            tokio::spawn(async move {
                stdout_writer.write_all(b"o").await.unwrap();
                first_chunks_drained.wait().await;
                stdout_writer.write_all(&vec![b'o'; 4095]).await.unwrap();
                stdout_writer.shutdown().await.unwrap();
            })
        };
        let stderr_writer_task = {
            let first_chunks_drained = Arc::clone(&first_chunks_drained);
            tokio::spawn(async move {
                stderr_writer.write_all(b"e").await.unwrap();
                first_chunks_drained.wait().await;
                stderr_writer.write_all(&vec![b'e'; 4095]).await.unwrap();
                stderr_writer.shutdown().await.unwrap();
            })
        };

        let mut output = String::new();
        while !output.contains('o') || !output.contains('e') {
            output_notify.notified().await;
            output.push_str(&take_buffer_output("concurrent-drain", &buffer, None).0);
        }
        first_chunks_drained.wait().await;

        stdout_writer_task.await.unwrap();
        stderr_writer_task.await.unwrap();
        stdout_reader.await.unwrap();
        stderr_reader.await.unwrap();
        output.push_str(&take_buffer_output("concurrent-drain", &buffer, None).0);

        assert!(!reader_died.load(Ordering::SeqCst));
        assert_eq!(output.bytes().filter(|byte| *byte == b'o').count(), 4096);
        assert_eq!(output.bytes().filter(|byte| *byte == b'e').count(), 4096);
    }

    #[tokio::test]
    async fn start_command_runs_in_a_pty() {
        let shell = test_shell();
        let ctx = async_process_context("shell-pty", CancellationToken::new());
        let result = run_with_context(
            &shell,
            "start_command",
            &json!({"cmd": "if [ -t 0 ] && [ -t 1 ]; then echo tty; else echo no-tty; exit 1; fi"}),
            &ctx,
        )
        .await;

        assert!(result.is_success(), "{}", result.value_for_projection());
        assert_eq!(result.value_for_projection()["exit_code"], 0);
        assert_eq!(
            result.value_for_projection()["output"]
                .as_str()
                .unwrap()
                .trim(),
            "tty"
        );
    }

    #[tokio::test]
    async fn exec_command_timeout_kills_and_fails_running_process() {
        let shell = shell_provider(StandardShell::new().with_cwd("/"));
        let result = run(
            &shell,
            "exec_command",
            &json!({"cmd": "printf started; sleep 5", "timeout_ms": 50}),
        )
        .await;
        assert!(!result.is_success(), "{}", result.value_for_projection());
        assert_eq!(result.value_for_projection()["status"], "timed_out");
        assert_eq!(result.value_for_projection()["done"], true);
        assert_eq!(result.value_for_projection()["running"], false);
        assert!(result.value_for_projection().get("session_id").is_none());
        assert!(
            result.value_for_projection()["output"]
                .as_str()
                .unwrap_or("")
                .contains("started")
        );
    }

    #[tokio::test]
    async fn exec_command_reader_death_wins_timeout_classification() {
        let dir = tempfile::tempdir().expect("reader-death marker directory");
        let marker = dir.path().join("survived-reader-death");
        let command = format!("sleep 0.1; printf survived > {}", marker.display());
        let runtime = ShellRuntime::new().with_cwd("/").with_aborted_pipe_reader();
        let wait_handle_probe = runtime.clone();
        let shell = shell_provider(StandardShell { runtime });
        let result = run(
            &shell,
            "exec_command",
            &json!({"cmd": command, "timeout_ms": 50}),
        )
        .await;

        let lash_core::ToolCallOutcome::Failure(failure) = &result.as_output().outcome else {
            panic!(
                "reader death must beat the timeout record: {}",
                result.value_for_projection()
            );
        };
        assert_eq!(failure.code, "shell_reader_died");
        assert!(
            matches!(
                &result.as_output().outcome,
                lash_core::ToolCallOutcome::Failure(_)
            ),
            "reader death must be ReaderDied, not Cancelled: {}",
            result.value_for_projection()
        );
        assert_eq!(result.value_for_projection()["reader_died"], true);
        assert_ne!(result.value_for_projection()["status"], "timed_out");
        let wait_handle = wait_handle_probe
            .pipe_wait_handle_probe()
            .expect("pipe wait handle probe");
        tokio::time::timeout(Duration::from_secs(5), async {
            while !wait_handle.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("ReaderDied child must terminate");
        assert!(
            !marker.exists(),
            "ReaderDied must terminate the child before it writes the survival marker"
        );
    }

    #[tokio::test]
    async fn exec_command_exit_wins_expired_deadline_race() {
        let gate = Arc::new(tokio::sync::Barrier::new(2));
        let runtime = ShellRuntime::new()
            .with_cwd("/")
            .with_pipe_loop_gate(Arc::clone(&gate));
        let wait_handle_probe = runtime.clone();
        let shell = shell_provider(StandardShell { runtime });
        let run_task = tokio::spawn(async move {
            run(
                &shell,
                "exec_command",
                &json!({"cmd": "exit 23", "timeout_ms": 20}),
            )
            .await
        });

        gate.wait().await;
        let wait_handle = wait_handle_probe
            .pipe_wait_handle_probe()
            .expect("pipe wait handle probe");
        tokio::time::timeout(Duration::from_secs(5), async {
            while !wait_handle.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("pipe wait handle must finish before releasing the loop gate");
        gate.wait().await;
        let result = run_task.await.expect("deadline-race run task");

        assert!(result.is_success(), "{}", result.value_for_projection());
        assert_eq!(result.value_for_projection()["status"], "completed");
        assert_eq!(result.value_for_projection()["exit_code"], 23);
        assert_ne!(result.value_for_projection()["status"], "timed_out");
    }

    #[tokio::test]
    async fn exec_command_timeout_kills_process_group_children() {
        let shell = test_shell();
        let marker = std::env::temp_dir().join(format!(
            "lash-exec-timeout-child-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cmd = format!(
            "sh -c 'sleep 0.4; echo leaked > {}' & wait",
            marker.display()
        );

        let result = run(
            &shell,
            "exec_command",
            &json!({"cmd": cmd, "timeout_ms": 50}),
        )
        .await;

        assert!(!result.is_success(), "{}", result.value_for_projection());
        assert_eq!(result.value_for_projection()["status"], "timed_out");
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(!marker.exists(), "timed-out child process wrote marker");
        let _ = fs::remove_file(marker);
    }

    #[tokio::test]
    async fn start_command_registers_process_handle() {
        let shell = shell_provider(StandardShell::new().with_cwd("/"));
        let service = Arc::new(TestProcessService::default());
        let ctx = context_with_processes(Arc::clone(&service), "shell-call-1");
        let result = run_with_context(
            &shell,
            "start_command",
            &json!({"cmd": "sleep 1; echo done"}),
            &ctx,
        )
        .await;
        assert!(result.is_success(), "{}", result.value_for_projection());
        assert_eq!(result.value_for_projection()["status"], "running");
        assert_eq!(result.value_for_projection()["done"], false);
        assert_eq!(result.value_for_projection()["running"], true);
        assert_eq!(result.value_for_projection()["__handle__"], "process");
        let derived_id = result.value_for_projection()["process_id"]
            .as_str()
            .expect("derived process id")
            .to_string();
        assert!(derived_id.starts_with("tool-intent:v1:sha256:"));
        assert_eq!(result.value_for_projection()["id"], derived_id);

        let entries = service
            .registry()
            .list_live_observed_by("test-session")
            .await
            .expect("list live observed");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, derived_id);
        assert_eq!(entries[0].identity.kind, "shell");
        assert_eq!(
            entries[0].identity.label.as_deref(),
            Some("sleep 1; echo done")
        );
    }

    #[tokio::test]
    async fn shell_start_and_write_are_literal_leaf_intents() {
        let shell = test_shell();
        let context = context_with_processes(
            Arc::new(TestProcessService::default()),
            "shell-intent-call",
        );
        let attempt = lash_core::AttemptContext::__for_testing(&context, "shell-intent-scope");
        let start = shell
            .execute_attempt(ToolCall {
                name: "start_command",
                args: &json!({"cmd": "sleep 30", "detach": true}),
                context: &attempt,
            })
            .await;
        let lash_core::ToolAttemptOutcome::Done { result, intents } = start else {
            panic!("shell.start must complete with an intent")
        };
        let value = result.into_output().value_for_projection();
        assert_eq!(
            value["process_id"],
            "tool-intent:v1:sha256:bdfc6fa58690fb98375000a2ddf1fd5d9141819d6d7221415d8b614efd071a75:detached"
        );
        assert_eq!(intents.protocol_version, lash_core::TOOL_INTENT_PROTOCOL_V1);
        assert_eq!(intents.intents.len(), 1);
        let lash_core::ToolIntent::StartProcess(intent) = &intents.intents[0] else {
            panic!("shell.start must declare StartProcess")
        };
        let lash_core::ProcessInput::ToolCall { call } = &intent.request.input else {
            panic!("shell.start must record its internal process body")
        };
        assert_eq!(
            call.args.get("detach"),
            Some(&json!(true)),
            "the recorded process body must retain detach semantics"
        );
        assert_eq!(intent.session_id, "test-session");
        assert_eq!(intent.on_parent_end, lash_core::ProcessParentEndPolicy::Abandon);
        assert_eq!(
            intent.request.id,
            "tool-intent:v1:sha256:bdfc6fa58690fb98375000a2ddf1fd5d9141819d6d7221415d8b614efd071a75"
        );
        assert_eq!(call.args["detached_process_id"], value["process_id"]);
        assert!(intent.request.env_spec.is_some());
        assert!(intent.request.observers.is_empty());
        assert_eq!(intent.request.wake_session_id, None);

        let tracked = shell
            .execute_attempt(ToolCall {
                name: "start_command",
                args: &json!({"cmd": "cat"}),
                context: &attempt,
            })
            .await;
        let lash_core::ToolAttemptOutcome::Done { intents, .. } = tracked else {
            panic!("tracked shell.start must complete with an intent")
        };
        let lash_core::ToolIntent::StartProcess(tracked) = &intents.intents[0] else {
            panic!("tracked shell.start must declare StartProcess")
        };
        assert_eq!(
            tracked.request.originator,
            lash_core::ProcessOriginator::session(lash_core::SessionScope::new("test-session")),
        );
        assert_eq!(tracked.request.observers, vec!["test-session".to_string()]);
        assert_eq!(
            tracked.request.wake_session_id.as_deref(),
            Some("test-session")
        );

        let write = shell
            .execute_attempt(ToolCall {
                name: "write_stdin",
                args: &json!({
                    "process_id": "literal-shell-process",
                    "chars": "status\n",
                    "close_stdin": true,
                }),
                context: &attempt,
            })
            .await;
        let lash_core::ToolAttemptOutcome::Done { intents, .. } = write else {
            panic!("shell.write must complete with an intent")
        };
        let lash_core::ToolIntent::SignalProcess(intent) = &intents.intents[0] else {
            panic!("shell.write must declare SignalProcess")
        };
        assert_eq!(intent.session_id, "test-session");
        assert_eq!(intent.process_id, "literal-shell-process");
        assert_eq!(intent.signal_name, "stdin");
        assert_eq!(
            intent.payload,
            json!({"chars": "status\n", "close_stdin": true})
        );
    }

    #[tokio::test]
    async fn write_stdin_emits_process_signal() {
        let shell = test_shell();
        let service = Arc::new(TestProcessService::default());
        let registry = service.registry();
        register_signal_target(registry.as_ref(), "shell-call-1").await;
        let ctx = context_with_processes(Arc::clone(&service), "write-call-1");

        let result = run_with_context(
            &shell,
            "write_stdin",
            &json!({"process_id": "shell-call-1", "chars": "hello\n", "close_stdin": true}),
            &ctx,
        )
        .await;
        assert!(result.is_success(), "{}", result.value_for_projection());
        assert_eq!(result.value_for_projection()["status"], "signalled");
        assert_eq!(result.value_for_projection()["process_id"], "shell-call-1");

        let events = service
            .registry()
            .events_after("shell-call-1", 0)
            .await
            .expect("events");
        let signal_events = events
            .iter()
            .filter(|event| event.event_type == SHELL_STDIN_SIGNAL_EVENT)
            .collect::<Vec<_>>();
        assert_eq!(signal_events.len(), 1);
        assert_eq!(signal_events[0].payload["chars"], "hello\n");
        assert_eq!(signal_events[0].payload["close_stdin"], true);
    }

    #[cfg(unix)]
    fn process_alive(pid: u32) -> bool {
        // kill(pid, 0) probes existence/permission without delivering a signal.
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    #[cfg(unix)]
    fn process_parent_pid(pid: u32) -> Option<u32> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat.rsplit_once(") ")?
            .1
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    }

    #[cfg(unix)]
    async fn wait_until_dead(pid: u32) -> bool {
        for _ in 0..100 {
            if !process_alive(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        !process_alive(pid)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_runtime_teardown_kills_tracked_pty_children() {
        // Tool-layer RAII / lease-loss self-fence (ADR 0019): the worker rebuilds
        // a fresh runtime per process run that owns the shell plugin instance,
        // and hence this `ShellRuntime`. When that runtime is torn down — session
        // close, run end, or a run task dropped on LeaseLost before its
        // cooperative cancel ran — the plugin instance drops, the last table Arc
        // drops, and every still-tracked PTY group is SIGKILLed. This proves the
        // drop => kill link the worker relies on for self-fencing.
        let runtime = ShellRuntime::new().with_cwd("/");
        let id = runtime.allocate_handle_id();
        runtime
            .spawn_process(
                id.clone(),
                "sleep 300",
                std::path::Path::new("/"),
                false,
                "bash",
            )
            .expect("spawn pty process");
        let pid = runtime.tracked_pid(&id).expect("tracked pid");
        assert!(process_alive(pid), "child should be running after spawn");
        drop(runtime);
        assert!(
            wait_until_dead(pid).await,
            "PTY child {pid} should be dead after ShellRuntime teardown",
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_detached_is_untracked_and_survives_teardown() {
        let runtime = ShellRuntime::new().with_cwd("/");
        let launch = runtime
            .spawn_detached(
                "sleep 300".to_string(),
                std::path::PathBuf::from("/"),
                false,
                "bash".to_string(),
            )
            .await
            .expect("spawn detached");
        // A Detached Command is never inserted into the tracked map, so the
        // teardown group-kill can never reach it (ADR 0019).
        assert_eq!(
            runtime.tracked_len(),
            0,
            "detached command must not be tracked",
        );
        assert_eq!(
            launch.pgid, launch.pid,
            "setsid makes the child its own process-group leader",
        );
        assert!(process_alive(launch.pid), "detached child should be running");
        assert_ne!(
            process_parent_pid(launch.pid),
            Some(std::process::id()),
            "the double-forked child must be reparented away from the shell worker"
        );
        drop(runtime);
        assert!(
            process_alive(launch.pid),
            "detached child must survive host teardown",
        );
        // Reap the process group we intentionally detached.
        unsafe {
            libc::kill(-(launch.pgid as i32), libc::SIGKILL);
        }
    }

    #[tokio::test]
    async fn internal_detached_process_body_reports_launch_identity() {
        let shell = shell_provider(StandardShell::new().with_cwd("/"));
        let service = Arc::new(TestProcessService::default());
        let registry = service.registry();
        let ctx = context_with_processes(service, "detach-call-1")
            .with_async_process("detach-launcher-1", CancellationToken::new());
        let result = run_with_context(
            &shell,
            "start_command",
            &json!({
                "cmd": "sleep 300",
                "detach": true,
                "detached_process_id": "detach-call-1",
            }),
            &ctx,
        )
        .await;
        assert!(result.is_success(), "{}", result.value_for_projection());
        let value = result.value_for_projection();
        // Immediately-terminal audit fact — never a running claim.
        assert_eq!(value["status"], "detached");
        assert_eq!(value["done"], true);
        assert_eq!(value["running"], false);
        assert_eq!(value["__handle__"], "process");
        assert_eq!(value["command"], "sleep 300");
        assert!(value["pid"].as_u64().is_some(), "detach reports pid");
        assert!(value["pgid"].as_u64().is_some(), "detach reports pgid");
        assert!(
            value["started_at"].as_u64().is_some(),
            "detach reports started_at",
        );
        let record = registry
            .get_process("detach-call-1")
            .await
            .expect("read detached audit row")
            .expect("detached audit row exists");
        assert_eq!(
            record.disposition,
            lash_core::RecoveryContract::ExternallyOwned
        );
        assert!(record.is_terminal(), "detached audit row is terminal from birth");

        #[cfg(unix)]
        let pid = value["pid"].as_u64().expect("detached pid") as u32;
        #[cfg(unix)]
        assert_ne!(process_parent_pid(pid), Some(std::process::id()));
        drop(shell);
        #[cfg(unix)]
        assert!(process_alive(pid), "detached child survives shell teardown");

        // Reap the process group we launched.
        #[cfg(unix)]
        if let Some(pgid) = value["pgid"].as_u64() {
            unsafe {
                libc::kill(-(pgid as i32), libc::SIGKILL);
            }
        }
    }

    #[tokio::test]
    async fn write_stdin_projects_the_recorded_terminal_target_refusal() {
        let shell = StandardShell::new().with_cwd("/");
        let definition = shell
            .tool_definitions()
            .into_iter()
            .find(|definition| definition.name() == "write_stdin")
            .expect("shell.write definition");
        let provider = Arc::new(shell_provider(shell));
        let service = Arc::new(TestProcessService::default());
        let registry = service.registry();
        register_signal_target(registry.as_ref(), "detached-production").await;
        registry
            .complete_process(
                "detached-production",
                lash_core::ProcessAwaitOutput::from_tool_output(
                    lash_core::ToolCallOutput::success(json!({"pid": 1234})),
                ),
                lash_core::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("terminalize production target");
        let scope = lash_core::ScopedEffectController::shared(
            Arc::new(lash_core::facade_support::InlineRuntimeEffectController::default()),
            lash_core::ExecutionScope::turn("test-session", "write-terminal-turn"),
        )
        .expect("build production-shaped intent controller");
        let processes: Arc<dyn lash_core::ProcessService> = service;
        let completed = lash_core::testing::conformance::coordinate_tool_provider_with_services(
            scope,
            processes,
            "test-session",
            definition.clone(),
            provider,
            PreparedToolCall::from_parts(
                "write-terminal-call",
                definition.id().clone(),
                definition.name(),
                json!({"process_id": "detached-production", "chars": "hello\n"}),
                None,
                serde_json::Value::Null,
            ),
        )
        .await
        .expect("coordinate shell.write through production intent projection");

        let lash_core::ToolCallOutcome::Failure(failure) = &completed.output.outcome else {
            panic!(
                "terminal-target refusal must replace the optimistic signalled projection: {:?}",
                completed.output
            );
        };
        assert_eq!(failure.code, "process_already_terminal");
        assert!(matches!(
            completed.intent_outcomes.as_slice(),
            [lash_core::ToolIntentExecutionOutcome::Refused {
                refusal: lash_core::ToolIntentRefusalReason::CommandFailed { code, .. },
                ..
            }] if code == "process_already_terminal"
        ));
    }

    #[tokio::test]
    async fn write_stdin_projects_the_recorded_absent_target_discriminator() {
        let shell = StandardShell::new().with_cwd("/");
        let definition = shell
            .tool_definitions()
            .into_iter()
            .find(|definition| definition.name() == "write_stdin")
            .expect("shell.write definition");
        let provider = Arc::new(shell_provider(shell));
        let service = Arc::new(TestProcessService::default());
        let scope = lash_core::ScopedEffectController::shared(
            Arc::new(lash_core::facade_support::InlineRuntimeEffectController::default()),
            lash_core::ExecutionScope::turn("test-session", "write-absent-turn"),
        )
        .expect("build production-shaped intent controller");
        let processes: Arc<dyn lash_core::ProcessService> = service;
        let completed = lash_core::testing::conformance::coordinate_tool_provider_with_services(
            scope,
            processes,
            "test-session",
            definition.clone(),
            provider,
            PreparedToolCall::from_parts(
                "write-absent-call",
                definition.id().clone(),
                definition.name(),
                json!({"process_id": "absent-production", "chars": "hello\n"}),
                None,
                serde_json::Value::Null,
            ),
        )
        .await
        .expect("coordinate absent shell.write target");

        let lash_core::ToolCallOutcome::Failure(failure) = &completed.output.outcome else {
            panic!("absent-target refusal must replace the optimistic projection");
        };
        assert_eq!(failure.code, "process_not_visible");
        assert!(matches!(
            completed.intent_outcomes.as_slice(),
            [lash_core::ToolIntentExecutionOutcome::Refused {
                refusal: lash_core::ToolIntentRefusalReason::CommandFailed { code, .. },
                ..
            }] if code == "process_not_visible"
        ));
    }

    #[tokio::test]
    async fn write_stdin_projects_the_recorded_pruned_target_discriminator() {
        let shell = StandardShell::new().with_cwd("/");
        let definition = shell
            .tool_definitions()
            .into_iter()
            .find(|definition| definition.name() == "write_stdin")
            .expect("shell.write definition");
        let provider = Arc::new(shell_provider(shell));
        let service = Arc::new(TestProcessService::default());
        let registry = service.registry();
        register_signal_target(registry.as_ref(), "pruned-production").await;
        registry
            .complete_process(
                "pruned-production",
                lash_core::ProcessAwaitOutput::from_tool_output(
                    lash_core::ToolCallOutput::success(json!({"pid": 1234})),
                ),
                lash_core::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("terminalize target before pruning");
        let (_, cursor) = registry
            .processes_changed_since(lash_core::ProcessChangeCursor::initial(), 100)
            .await
            .expect("read terminal change cursor");
        let report = registry
            .prune_terminal_processes(
                u64::MAX,
                None,
                lash_core::ProjectionWatermark::UpTo(cursor),
            )
            .await
            .expect("prune terminal target");
        assert_eq!(report.pruned_processes, 1);

        let scope = lash_core::ScopedEffectController::shared(
            Arc::new(lash_core::facade_support::InlineRuntimeEffectController::default()),
            lash_core::ExecutionScope::turn("test-session", "write-pruned-turn"),
        )
        .expect("build production-shaped intent controller");
        let processes: Arc<dyn lash_core::ProcessService> = service;
        let completed = lash_core::testing::conformance::coordinate_tool_provider_with_services(
            scope,
            processes,
            "test-session",
            definition.clone(),
            provider,
            PreparedToolCall::from_parts(
                "write-pruned-call",
                definition.id().clone(),
                definition.name(),
                json!({"process_id": "pruned-production", "chars": "hello\n"}),
                None,
                serde_json::Value::Null,
            ),
        )
        .await
        .expect("coordinate pruned shell.write target");

        let lash_core::ToolCallOutcome::Failure(failure) = &completed.output.outcome else {
            panic!("pruned-target refusal must replace the optimistic projection");
        };
        assert_eq!(failure.code, "process_no_longer_retained");
        assert!(matches!(
            completed.intent_outcomes.as_slice(),
            [lash_core::ToolIntentExecutionOutcome::Refused {
                refusal: lash_core::ToolIntentRefusalReason::CommandFailed { code, .. },
                ..
            }] if code == "process_no_longer_retained"
        ));
    }

    #[tokio::test]
    async fn write_stdin_projects_the_recorded_signal_sequence() {
        let shell = StandardShell::new().with_cwd("/");
        let definition = shell
            .tool_definitions()
            .into_iter()
            .find(|definition| definition.name() == "write_stdin")
            .expect("shell.write definition");
        let provider = Arc::new(shell_provider(shell));
        let service = Arc::new(TestProcessService::default());
        let registry = service.registry();
        register_executable_signal_target(registry.as_ref(), "write-production").await;
        let scope = lash_core::ScopedEffectController::shared(
            Arc::new(lash_core::facade_support::InlineRuntimeEffectController::default()),
            lash_core::ExecutionScope::turn("test-session", "write-sequence-turn"),
        )
        .expect("build production-shaped intent controller");
        let processes: Arc<dyn lash_core::ProcessService> = service;
        let completed = lash_core::testing::conformance::coordinate_tool_provider_with_services(
            scope,
            processes,
            "test-session",
            definition.clone(),
            provider,
            PreparedToolCall::from_parts(
                "write-sequence-call",
                definition.id().clone(),
                definition.name(),
                json!({"process_id": "write-production", "chars": "hello\n"}),
                None,
                serde_json::Value::Null,
            ),
        )
        .await
        .expect("coordinate successful shell.write");

        assert!(completed.output.is_success());
        let sequence = completed.output.value_for_projection()["sequence"]
            .as_u64()
            .expect("shell.write projects the recorded event sequence");
        assert!(sequence > 0);
        assert!(matches!(
            completed.intent_outcomes.as_slice(),
            [lash_core::ToolIntentExecutionOutcome::Executed { result, .. }]
                if result["sequence"] == sequence
        ));
    }

    #[tokio::test]
    async fn internal_detached_process_body_refuses_before_host_launch_without_durable_context() {
        let dir = tempfile::tempdir().expect("detached ordering tempdir");
        let shell = shell_provider(StandardShell::new().with_cwd(dir.path()));
        let marker = dir.path().join("launched");
        let ctx = context_with_processes(
            Arc::new(TestProcessService::default()),
            "detach-ordering-call",
        );
        let args = json!({
                "cmd": "touch launched",
                "detach": true,
                "detached_process_id": "detach-ordering-audit",
            });
        let internal = lash_core::InternalProcessContext::__for_testing(&ctx);
        let result = shell
            .execute_internal(lash_core::InternalProcessToolCall {
                name: "run_start_command",
                args: &args,
                context: &internal,
            })
            .await;
        assert!(!result.is_success(), "missing durable context must refuse");
        assert_eq!(
            result.value_for_projection()["code"],
            "detached_process_runner_missing_process"
        );
        assert!(
            !marker.exists(),
            "host work must not launch before durable context validation"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancelled_detached_spawn_cannot_escape_without_a_durable_audit_row() {
        let dir = tempfile::tempdir().expect("detached cancellation tempdir");
        let marker = dir.path().join("launched-after-cancel");
        let gate = Arc::new(runtime::DetachedLaunchGate::new());
        let shell = shell_provider(
            StandardShell::new()
                .with_cwd(dir.path())
                .with_detached_launch_gate(Arc::clone(&gate)),
        );
        let service = Arc::new(TestProcessService::default());
        let registry = service.registry();
        let ctx = context_with_processes(service, "detach-cancel-audit")
            .with_async_process("detach-cancel-launcher", CancellationToken::new());
        let args = json!({
            "cmd": "touch launched-after-cancel",
            "detach": true,
            "detached_process_id": "detach-cancel-audit",
        });
        let task = tokio::spawn(async move {
            run_with_context(&shell, "start_command", &args, &ctx).await
        });

        let audit = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(record) = registry
                    .get_process("detach-cancel-audit")
                    .await
                    .expect("read cancellation audit row")
                {
                    break record;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("audit row must commit before the blocking spawn is released");
        assert_eq!(
            audit.disposition,
            lash_core::RecoveryContract::ExternallyOwned
        );
        assert!(!audit.is_terminal(), "the launch is still gated");

        gate.wait_until_entered();
        task.abort();
        gate.release();
        let cancelled = task.await.expect_err("the caller is cancelled mid-spawn");
        assert!(cancelled.is_cancelled());
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !marker.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the detached blocking task must demonstrate post-cancel launch");
        let departed = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let record = registry
                    .get_process("detach-cancel-audit")
                    .await
                    .expect("read post-cancel audit row")
                    .expect(
                        "a post-cancel host launch must retain its pre-spawn durable audit row",
                    );
                if record.status == lash_core::ProcessStatus::CallerDeparted {
                    break record;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect(
            "a caller cancelled mid-spawn must leave a durably distinguishable caller-departed row",
        );
        assert!(
            !departed.is_terminal(),
            "lash never observed whether the host launch happened, so the row must not claim an \
             outcome"
        );
        assert!(
            departed.outcome.is_none(),
            "a caller-departed row must carry no outcome claim"
        );
        assert!(
            departed.status.is_retired(),
            "retention must be able to reclaim a row nothing may ever terminalize"
        );

        // (d): an await on that row is typed-refused instead of parking forever.
        let refusal = lash_core::facade_support::ProcessAwaiter::polling(
            Arc::clone(&registry) as Arc<dyn lash_core::ProcessRegistry>
        )
        .await_terminal("detach-cancel-audit")
        .await
        .expect_err("awaiting a caller-departed row must be refused, not parked");
        assert!(
            matches!(
                refusal,
                PluginError::ProcessCallerDeparted { ref process_id }
                    if process_id == "detach-cancel-audit"
            ),
            "unexpected await refusal: {refusal:?}"
        );
    }

    #[tokio::test]
    async fn start_command_process_consumes_stdin_signals() {
        let shell = test_shell();
        let registry = Arc::new(lash_core::TestLocalProcessRegistry::default());
        register_executable_signal_target(registry.as_ref(), "shell-worker").await;
        let execution_write_authority =
            claim_signal_target_execution(registry.as_ref(), "shell-worker").await;
        let registry_dyn: Arc<dyn lash_core::ProcessRegistry> = registry.clone();
        let ctx = Arc::new(async_process_context_with_events(
            "shell-worker",
            registry_dyn,
            execution_write_authority,
            CancellationToken::new(),
        ));
        let args = Arc::new(json!({
            "cmd": "python3 -u -c 'import sys; line = sys.stdin.readline(); print(\"got:\" + line.strip())'",
            "login": false,
        }));
        let shell = Arc::new(shell);
        let worker = {
            let shell = Arc::clone(&shell);
            let ctx = Arc::clone(&ctx);
            let args = Arc::clone(&args);
            tokio::spawn(async move {
                run_with_context(&shell, "start_command", &args, &ctx).await
            })
        };

        tokio::time::sleep(Duration::from_millis(100)).await;
        registry
            .append_event(
                "shell-worker",
                lash_core::ProcessEventAppendRequest::new(
                    SHELL_STDIN_SIGNAL_EVENT,
                    json!({"chars": "hello\n", "close_stdin": false}),
                ),
            )
            .await
            .expect("signal");

        let result = worker.await.expect("worker task");
        assert!(result.is_success(), "{}", result.value_for_projection());
        assert_eq!(result.value_for_projection()["exit_code"], 0);
        assert!(
            result.value_for_projection()["output"]
                .as_str()
                .unwrap()
                .contains("got:hello")
        );
    }

    #[tokio::test]
    async fn start_command_process_can_close_stdin_from_signal() {
        let shell = test_shell();
        let registry = Arc::new(lash_core::TestLocalProcessRegistry::default());
        register_executable_signal_target(registry.as_ref(), "shell-close-stdin").await;
        let execution_write_authority =
            claim_signal_target_execution(registry.as_ref(), "shell-close-stdin").await;
        let registry_dyn: Arc<dyn lash_core::ProcessRegistry> = registry.clone();
        let ctx = Arc::new(async_process_context_with_events(
            "shell-close-stdin",
            registry_dyn,
            execution_write_authority,
            CancellationToken::new(),
        ));
        let args = Arc::new(json!({"cmd": "cat", "login": false}));
        let shell = Arc::new(shell);
        let worker = {
            let shell = Arc::clone(&shell);
            let ctx = Arc::clone(&ctx);
            let args = Arc::clone(&args);
            tokio::spawn(async move {
                run_with_context(&shell, "start_command", &args, &ctx).await
            })
        };

        tokio::time::sleep(Duration::from_millis(100)).await;
        registry
            .append_event(
                "shell-close-stdin",
                lash_core::ProcessEventAppendRequest::new(
                    SHELL_STDIN_SIGNAL_EVENT,
                    json!({"chars": "hello", "close_stdin": true}),
                ),
            )
            .await
            .expect("signal");

        let result = worker.await.expect("worker task");
        assert!(result.is_success(), "{}", result.value_for_projection());
        assert_eq!(result.value_for_projection()["exit_code"], 0);
        assert!(
            result.value_for_projection()["output"]
                .as_str()
                .unwrap()
                .contains("hello")
        );
    }

    #[tokio::test]
    async fn start_command_process_nonzero_exit_returns_result_data() {
        let shell = test_shell();
        let ctx = async_process_context("shell-exit-7", CancellationToken::new());
        let result = run_with_context(
            &shell,
            "start_command",
            &json!({"cmd": "exit 7", "login": false}),
            &ctx,
        )
        .await;

        assert!(result.is_success(), "{}", result.value_for_projection());
        assert_eq!(result.value_for_projection()["status"], "completed");
        assert_eq!(result.value_for_projection()["exit_code"], 7);
        assert!(result.value_for_projection()["error"].is_null());
    }

    #[tokio::test]
    async fn start_command_process_reports_full_output_path_when_token_truncated() {
        let shell = test_shell();
        let ctx = async_process_context("shell-token-truncated", CancellationToken::new());
        let result = run_with_context(
            &shell,
            "start_command",
            &json!({"cmd": "python3 -c 'print(\"segment \" * 5000)'", "login": false, "max_output_tokens": 24}),
            &ctx,
        )
        .await;

        assert!(result.is_success(), "{}", result.value_for_projection());
        let result_value = result.value_for_projection();
        let output = result_value["output"].as_str().unwrap();
        let full_output_path = result_value["full_output_path"].as_str().unwrap();
        let full_output = fs::read_to_string(full_output_path).expect("full output file");
        assert!(output.contains("[truncated]"));
        assert!(full_output.contains("segment segment"));
    }

    #[tokio::test]
    async fn start_command_process_completes_short_lived_commands() {
        let shell = test_shell();
        let cmd = "python3 -u -c 'import sys; line = sys.stdin.readline(); print(\"got:\" + line.strip())'";
        let registry = Arc::new(lash_core::TestLocalProcessRegistry::default());
        register_executable_signal_target(registry.as_ref(), "shell-short").await;
        let execution_write_authority =
            claim_signal_target_execution(registry.as_ref(), "shell-short").await;
        let registry_dyn: Arc<dyn lash_core::ProcessRegistry> = registry.clone();
        let ctx = Arc::new(async_process_context_with_events(
            "shell-short",
            registry_dyn,
            execution_write_authority,
            CancellationToken::new(),
        ));
        let args = Arc::new(json!({"cmd": cmd, "login": false}));
        let shell = Arc::new(shell);
        let worker = {
            let shell = Arc::clone(&shell);
            let ctx = Arc::clone(&ctx);
            let args = Arc::clone(&args);
            tokio::spawn(async move {
                run_with_context(&shell, "start_command", &args, &ctx).await
            })
        };

        tokio::time::sleep(Duration::from_millis(100)).await;
        registry
            .append_event(
                "shell-short",
                lash_core::ProcessEventAppendRequest::new(
                    SHELL_STDIN_SIGNAL_EVENT,
                    json!({"chars": "hello\n", "close_stdin": false}),
                ),
            )
            .await
            .expect("signal");

        let result = worker.await.expect("worker task");
        assert!(result.is_success());
        assert!(result.value_for_projection().get("session_id").is_none());
        assert_eq!(result.value_for_projection()["exit_code"], 0);
        assert!(
            result.value_for_projection()["output"]
                .as_str()
                .unwrap()
                .contains("got:hello")
        );
    }

    #[tokio::test]
    async fn exec_command_honors_workdir() {
        let shell = shell_provider(StandardShell::new().with_cwd("/"));
        let result = run(
            &shell,
            "exec_command",
            &json!({"cmd": "pwd", "workdir": "tmp"}),
        )
        .await;
        assert!(result.is_success());
        assert_eq!(
            result.value_for_projection()["output"]
                .as_str()
                .unwrap()
                .trim_end(),
            "/tmp"
        );
    }

    #[tokio::test]
    async fn exec_command_does_not_add_strict_pipeline_semantics() {
        let shell = test_shell();
        let result = run(&shell, "exec_command", &json!({"cmd": "false | cat"})).await;
        assert!(result.is_success(), "{}", result.value_for_projection());
        assert_eq!(result.value_for_projection()["exit_code"], 0);
        assert!(result.value_for_projection()["error"].is_null());
    }

    #[tokio::test]
    async fn exec_command_nonzero_exit_returns_result_data() {
        let shell = test_shell();
        let result = run(&shell, "exec_command", &json!({"cmd": "echo nope; exit 7"})).await;
        assert!(result.is_success(), "{}", result.value_for_projection());
        assert_eq!(result.value_for_projection()["exit_code"], 7);
        assert!(result.value_for_projection()["error"].is_null());
        assert!(
            result.value_for_projection()["output"]
                .as_str()
                .unwrap()
                .contains("nope")
        );
    }

    #[tokio::test]
    async fn exec_command_head_style_pipeline_is_not_failed_by_sigpipe() {
        let shell = test_shell();
        let result = run(
            &shell,
            "exec_command",
            &json!({"cmd": "yes line | head -n 3", "login": false}),
        )
        .await;

        assert!(result.is_success(), "{}", result.value_for_projection());
        assert_eq!(result.value_for_projection()["exit_code"], 0);
        assert_eq!(
            result.value_for_projection()["output"].as_str().unwrap(),
            "line\nline\nline\n"
        );
    }

    #[tokio::test]
    async fn exec_command_reports_full_output_path_when_token_truncated() {
        let shell = test_shell();
        let result = run(
            &shell,
            "exec_command",
            &json!({"cmd": "python3 -c 'print(\"hello \" * 4000)'", "max_output_tokens": 16, "login": false}),
        )
        .await;
        assert!(result.is_success(), "{}", result.value_for_projection());
        let result_value = result.value_for_projection();
        let output = result_value["output"].as_str().unwrap();
        let full_output_path = result_value["full_output_path"].as_str().unwrap();
        let full_output = fs::read_to_string(full_output_path).expect("full output file");
        assert!(output.contains("[truncated]"));
        assert!(full_output.contains("hello hello"));
    }

    #[tokio::test]
    async fn exec_command_spills_full_output_when_buffer_overflows() {
        let shell = test_shell();
        let result = run(
            &shell,
            "exec_command",
            &json!({"cmd": format!("python3 -c 'import sys; sys.stdout.write(\"x\" * {})'", MAX_OUTPUT + 8192), "login": false}),
        )
        .await;
        assert!(result.is_success(), "{}", result.value_for_projection());
        let result_value = result.value_for_projection();
        let output = result_value["output"].as_str().unwrap();
        let full_output_path = result_value["full_output_path"].as_str().unwrap();
        let full_output = fs::read_to_string(full_output_path).expect("full output file");
        assert!(output.contains("[truncated]"));
        assert!(full_output.len() >= MAX_OUTPUT + 8192);
    }

    #[tokio::test]
    async fn exec_command_reports_full_output_path_for_large_output() {
        let shell = test_shell();
        let result = run(
            &shell,
            "exec_command",
            &json!({"cmd": format!("python3 -c 'import sys; sys.stdout.write(\"x\" * {})'", SPILL_OUTPUT_THRESHOLD + 4096), "login": false}),
        )
        .await;
        assert!(result.is_success(), "{}", result.value_for_projection());
        let result_value = result.value_for_projection();
        assert!(result_value["output"].as_str().is_some());
        let full_output_path = result_value["full_output_path"].as_str().unwrap();
        let full_output = fs::read_to_string(full_output_path).expect("full output file");
        assert!(full_output.len() >= SPILL_OUTPUT_THRESHOLD + 4096);
    }

    #[test]
    fn shell_definitions_are_compact_and_non_empty() {
        let shell = StandardShell::default();
        let defs = shell.tool_definitions();
        assert_eq!(defs.len(), 4);
        assert_eq!(
            defs.iter()
                .filter(|definition| definition.manifest.activation == lash_core::ToolActivation::Internal)
                .map(|definition| definition.name())
                .collect::<Vec<_>>(),
            vec!["run_start_command"]
        );
        assert!(defs.iter().all(|def| !def.description().is_empty()));
    }

    #[test]
    fn shell_definitions_document_distinct_result_shapes() {
        let shell = StandardShell::default();
        let defs = shell.tool_definitions();
        let exec = defs
            .iter()
            .find(|definition| definition.name() == "exec_command")
            .expect("exec_command definition");
        let start = defs
            .iter()
            .find(|definition| definition.name() == "start_command")
            .expect("start_command definition");
        let write = defs
            .iter()
            .find(|definition| definition.name() == "write_stdin")
            .expect("write_stdin definition");

        assert!(
            exec.compact_contract()
                .render_signature()
                .contains("exit_code")
        );
        assert!(
            start
                .compact_contract()
                .render_signature()
                .contains("__handle__")
        );
        assert!(
            write
                .compact_contract()
                .render_signature()
                .contains("sequence")
        );
    }

    #[test]
    fn shell_exec_contract_documents_nonzero_exit_as_result_data() {
        let shell = StandardShell::default();
        let exec = shell
            .tool_definitions()
            .into_iter()
            .find(|definition| definition.name() == "exec_command")
            .expect("exec_command definition");
        let description = exec.description();

        assert!(description.contains("exit_code"));
        assert!(description.contains("Nonzero exit codes are returned as ordinary result data"));
        assert!(description.contains("does not abort your code"));
        // The same sentence used to say "in Lashlang, `await shell.exec(...)?`
        // does not abort": Lashlang's name and its try-operator, in a
        // description a TypeScript session reads verbatim (ADR 0063). The RLM
        // catalog now refuses to register prose that names a dialect, so the
        // wording is pinned here as dialect-neutral rather than merely present.
        assert!(!description.to_lowercase().contains("lashlang"), "{description}");
        assert!(!description.contains(")?"), "{description}");
        assert!(description.contains("Timed-out commands are killed and returned as a tool failure"));
    }

    #[test]
    fn start_command_contract_uses_process_handles() {
        let shell = StandardShell::default();
        let definition = shell
            .tool_definitions()
            .into_iter()
            .find(|definition| definition.name() == "start_command")
            .expect("start_command definition");
        let properties = definition
            .contract
            .input_schema
            .canonical
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .expect("properties");

        assert!(!properties.contains_key("poll_ms"));
        assert!(!properties.contains_key("timeout_ms"));
        assert!(definition.description().contains("processes.list"));
        assert!(definition.description().contains("processes.cancel"));
    }

    #[test]
    fn exec_command_defaults_to_non_login_shell() {
        let shell = StandardShell::default();
        let params = shell
            .parse_exec_command_params(&json!({"cmd": "echo hello"}))
            .expect("params");

        assert!(!params.login);
    }

    #[test]
    fn exec_command_defaults_to_generous_timeout() {
        let shell = StandardShell::default();
        let params = shell
            .parse_exec_command_params(&json!({"cmd": "echo hello"}))
            .expect("params");

        assert_eq!(params.timeout_ms, DEFAULT_EXEC_COMMAND_TIMEOUT_MS);
    }

    #[test]
    fn exec_command_timeout_schema_documents_default() {
        let shell = StandardShell::default();
        let definition = shell
            .tool_definitions()
            .into_iter()
            .find(|definition| definition.name() == "exec_command")
            .expect("exec_command definition");
        let properties = definition
            .contract
            .input_schema
            .canonical
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .expect("properties");

        assert_eq!(
            properties["timeout_ms"]["default"],
            DEFAULT_EXEC_COMMAND_TIMEOUT_MS
        );
        assert!(
            definition
                .description()
                .contains("Commands time out after 600000 ms by default")
        );
    }

    #[test]
    fn clean_terminal_output_strips_ansi_and_controls() {
        let raw = "\x1b[?2004h\x1b[31mred\x1b[0m\r\nab\x08c\x1b]0;title\x07\x00";

        assert_eq!(clean_terminal_output(raw), "red\nac");
    }

    #[tokio::test]
    async fn exec_command_cancel_token_kills_running_child() {
        use std::time::Instant;

        let shell = test_shell();
        let token = CancellationToken::new();
        let ctx = lash_core::testing::mock_tool_context().with_async_process("test", token.clone());

        // A long-running sleep that would otherwise hold the tool call for
        // 5s. The dispatcher must return promptly once the token fires, and
        // the pipe-backed process group must be killed rather than left to run.
        let args = json!({
            "cmd": "sleep 5",
            "login": false,
        });

        let cancel_handle = {
            let token = token.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                token.cancel();
            })
        };

        let started = Instant::now();
        let result = shell
            .execute(ToolCall {
                name: "exec_command",
                args: &args,
                context: &lash_core::AttemptContext::__for_testing(&ctx, "shell-test-scope"),
            })
            .await;
        let elapsed = started.elapsed();
        let _ = cancel_handle.await;

        assert!(
            elapsed < Duration::from_secs(1),
            "cancelled dispatch should return in under 1s (took {elapsed:?})"
        );
        assert!(!result.is_success(), "cancelled result should be an error");
        assert!(
            result
                .value_for_projection()
                .to_string()
                .contains("tool call cancelled")
        );
    }

    #[tokio::test]
    async fn start_command_cancel_token_kills_running_child() {
        use std::time::Instant;

        let shell = test_shell();
        let token = CancellationToken::new();
        let ctx = async_process_context("shell-cancel", token.clone());
        let args = json!({
            "cmd": "sleep 5",
            "login": false,
        });
        let cancel_handle = {
            let token = token.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                token.cancel();
            })
        };

        let started = Instant::now();
        let result = run_with_context(&shell, "start_command", &args, &ctx).await;
        let elapsed = started.elapsed();
        let _ = cancel_handle.await;

        assert!(
            elapsed < Duration::from_secs(1),
            "cancelled dispatch should return in under 1s (took {elapsed:?})"
        );
        assert!(!result.is_success(), "cancelled result should be an error");
        assert!(
            result
                .value_for_projection()
                .to_string()
                .contains("tool call cancelled")
        );
    }
}
