#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::output::{MAX_OUTPUT, SPILL_OUTPUT_THRESHOLD, clean_terminal_output};
    use lash_core::ProcessRegistry as _;
    use serde_json::json;
    use std::fs;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_shell() -> StaticToolProvider<StandardShell> {
        shell_provider(StandardShell::new().with_cwd("/"))
    }

    async fn run(
        shell: &StaticToolProvider<StandardShell>,
        name: &str,
        args: &serde_json::Value,
    ) -> ToolResult {
        lash_core::testing::run_tool(shell, name, args).await
    }

    async fn run_with_context(
        shell: &StaticToolProvider<StandardShell>,
        name: &str,
        args: &serde_json::Value,
        context: &lash_core::ToolContext<'_>,
    ) -> ToolResult {
        shell
            .execute(ToolCall {
                name,
                args,
                context,
                progress: None,
            })
            .await
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
        lash_core::testing::mock_tool_context()
            .with_async_process(process_id, cancel)
            .with_process_events_for_testing(process_id, registry, execution_write_authority)
    }

    #[derive(Clone, Default)]
    struct TestProcessService {
        registry: Arc<lash_core::TestLocalProcessRegistry>,
    }

    impl TestProcessService {
        fn registry(&self) -> Arc<lash_core::TestLocalProcessRegistry> {
            Arc::clone(&self.registry)
        }

        fn session_scope(
            session_id: &str,
            scope: &lash_core::ProcessOpScope<'_>,
        ) -> lash_core::SessionScope {
            scope
                .agent_frame_id()
                .filter(|frame_id| !frame_id.is_empty())
                .map(|frame_id| lash_core::SessionScope::for_agent_frame(session_id, frame_id))
                .unwrap_or_else(|| lash_core::SessionScope::new(session_id))
        }
    }

    #[async_trait::async_trait]
    impl lash_core::ProcessService for TestProcessService {
        async fn start_from_request(
            &self,
            session_id: &str,
            request: lash_core::ProcessStartRequest,
            scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::ProcessHandleSummary, PluginError> {
            let env_ref = request
                .env_spec
                .as_ref()
                .map(lash_core::ProcessExecutionEnvSpec::stable_ref)
                .transpose()
                .map_err(|err| {
                    PluginError::Session(format!("failed to hash test process env: {err}"))
                })?;
            let descriptor = request
                .grant
                .as_ref()
                .map(|grant| grant.descriptor.clone())
                .unwrap_or_default();
            let registration = request.into_registration(env_ref);
            let record = self
                .start(
                    session_id,
                    registration,
                    lash_core::ProcessStartOptions::new().with_descriptor(descriptor.clone()),
                    scope,
                )
                .await?;
            let definition = record.identity.definition.clone();
            Ok(lash_core::ProcessHandleSummary::new(
                record.id,
                descriptor,
                lash_core::ProcessLifecycleStatus::from(record.status),
            )
            .with_definition(definition))
        }

        async fn start(
            &self,
            session_id: &str,
            registration: lash_core::ProcessRegistration,
            options: lash_core::ProcessStartOptions,
            scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::ProcessRecord, PluginError> {
            let process_id = registration.id.clone();
            let record = self.registry.register_process(registration).await?;
            if let Some(descriptor) = options.descriptor {
                self.registry
                    .grant_handle(
                        &Self::session_scope(session_id, &scope),
                        &process_id,
                        descriptor,
                    )
                    .await?;
            }
            Ok(record)
        }

        async fn complete_external(
            &self,
            session_id: &str,
            process_id: &str,
            await_output: lash_core::ProcessAwaitOutput,
            scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::ProcessCompletionOutcome, PluginError> {
            let session_scope = Self::session_scope(session_id, &scope);
            if !self
                .registry
                .has_handle_grant(&session_scope, process_id)
                .await?
            {
                return Err(PluginError::Session(format!(
                    "process handle `{process_id}` is not visible in this session"
                )));
            }
            match self.registry.get_process(process_id).await {
                Some(record)
                    if record.disposition != lash_core::RecoveryDisposition::ExternallyOwned =>
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

        async fn await_process(
            &self,
            process_id: &str,
            _scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<lash_core::ProcessAwaitOutput, PluginError> {
            let registry: Arc<dyn lash_core::ProcessRegistry> = self.registry.clone();
            lash_core::ProcessAwaiter::polling(registry)
                .await_terminal(process_id)
                .await
        }

        async fn list_visible(
            &self,
            session_id: &str,
            mode: lash_core::ProcessListMode,
            scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<Vec<lash_core::runtime::ProcessHandleGrantEntry>, PluginError> {
            let session_scope = Self::session_scope(session_id, &scope);
            match mode {
                lash_core::ProcessListMode::Live => {
                    self.registry.list_live_handle_grants(&session_scope).await
                }
                lash_core::ProcessListMode::All => {
                    self.registry.list_handle_grants(&session_scope).await
                }
            }
        }

        async fn validate_visible(
            &self,
            session_id: &str,
            process_ids: &[String],
            scope: lash_core::ProcessOpScope<'_>,
        ) -> Result<(), PluginError> {
            let session_scope = Self::session_scope(session_id, &scope);
            for process_id in process_ids {
                if !self
                    .registry
                    .has_handle_grant(&session_scope, process_id)
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
                .await
                .ok_or_else(|| PluginError::Session(format!("unknown process `{process_id}`")))
        }

        async fn signal(
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
            let session_scope = Self::session_scope(session_id, &scope);
            let visible = self
                .registry
                .list_live_handle_grants(&session_scope)
                .await?
                .into_iter()
                .any(|(grant, _record)| grant.process_id == process_id);
            if !visible {
                return Err(PluginError::Session(format!(
                    "process handle `{process_id}` is not live or visible in this session"
                )));
            }
            let event_type = lash_core::process_signal_event_type(&signal_name)?;
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
            Arc::new(lash_core::SessionAttachmentStore::in_memory()),
            lash_core::DirectCompletionClient::from_fn(|_, _| {
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
            lash_core::RecoveryDisposition::ExternallyOwned,
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
            lash_core::RecoveryDisposition::OwnerBound,
        )
        .await;
    }

    async fn register_signal_target_with_disposition(
        registry: &lash_core::TestLocalProcessRegistry,
        process_id: &str,
        disposition: lash_core::RecoveryDisposition,
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
            .grant_handle(
                &lash_core::SessionScope::new("test-session"),
                process_id,
                lash_core::ProcessHandleDescriptor::new(Some("shell"), Some("test")),
            )
            .await
            .expect("grant handle");
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
        assert_eq!(result.value_for_projection()["id"], "shell-call-1");
        assert_eq!(result.value_for_projection()["process_id"], "shell-call-1");

        let entries = service
            .registry()
            .list_live_handle_grants(&lash_core::SessionScope::new("test-session"))
            .await
            .expect("list live handles");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0.process_id, "shell-call-1");
        assert_eq!(entries[0].0.descriptor.kind.as_deref(), Some("shell"));
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
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, SHELL_STDIN_SIGNAL_EVENT);
        assert_eq!(events[0].payload["chars"], "hello\n");
        assert_eq!(events[0].payload["close_stdin"], true);
    }

    #[cfg(unix)]
    fn process_alive(pid: u32) -> bool {
        // kill(pid, 0) probes existence/permission without delivering a signal.
        unsafe { libc::kill(pid as i32, 0) == 0 }
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
            .spawn_detached("sleep 300", std::path::Path::new("/"), false, "bash")
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
    async fn start_command_detach_records_terminal_audit_fact() {
        let shell = shell_provider(StandardShell::new().with_cwd("/"));
        let service = Arc::new(TestProcessService::default());
        let ctx = context_with_processes(Arc::clone(&service), "detach-call-1");
        let result = run_with_context(
            &shell,
            "start_command",
            &json!({"cmd": "sleep 300", "detach": true}),
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

        // The registry row is externally-owned and already terminal.
        let record = service
            .registry()
            .get_process("detach-call-1")
            .await
            .expect("registry row");
        assert!(
            record.is_terminal(),
            "detached row must be terminal from birth",
        );
        assert_eq!(
            record.disposition,
            lash_core::RecoveryDisposition::ExternallyOwned,
        );

        // Reap the process group we launched.
        #[cfg(unix)]
        if let Some(pgid) = value["pgid"].as_u64() {
            unsafe {
                libc::kill(-(pgid as i32), libc::SIGKILL);
            }
        }
    }

    #[tokio::test]
    async fn write_stdin_rejects_terminal_detached_target() {
        let shell = test_shell();
        let service = Arc::new(TestProcessService::default());
        let registry = service.registry();
        register_signal_target(registry.as_ref(), "detached-1").await;
        // A detached row is externally-owned and terminal from birth; completing
        // it here reproduces that state.
        registry
            .complete_process(
                "detached-1",
                lash_core::ProcessAwaitOutput::Success {
                    value: serde_json::json!({ "pid": 1234 }),
                    control: None,
                },
                lash_core::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete external row");
        let ctx = context_with_processes(Arc::clone(&service), "write-detached");
        let result = run_with_context(
            &shell,
            "write_stdin",
            &json!({"process_id": "detached-1", "chars": "hello\n"}),
            &ctx,
        )
        .await;
        assert!(
            !result.is_success(),
            "write_stdin to a terminal detached row must be rejected: {}",
            result.value_for_projection()
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
                shell
                    .execute(ToolCall {
                        name: "start_command",
                        args: &args,
                        context: &ctx,
                        progress: None,
                    })
                    .await
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
                shell
                    .execute(ToolCall {
                        name: "start_command",
                        args: &args,
                        context: &ctx,
                        progress: None,
                    })
                    .await
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
                shell
                    .execute(ToolCall {
                        name: "start_command",
                        args: &args,
                        context: &ctx,
                        progress: None,
                    })
                    .await
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
        assert_eq!(defs.len(), 3);
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
        assert!(description.contains("await shell.exec(...)?"));
        assert!(description.contains("does not abort just because the process exited nonzero"));
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
                context: &ctx,
                progress: None,
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
