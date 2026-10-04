use super::*;

fn journal_handle() -> Value {
    let process_id = lash_sansio::ProcessId::fixture("container-replay-child");
    serde_json::json!({
        "__handle__":"lash",
        "id":lash_sansio::handle::HandleId::process(&process_id).as_str(),
        "process_id":process_id.as_str(),
    })
}

fn journal_definitions() -> Vec<lash_core::ToolDefinition> {
    let handle = serde_json::json!({"x-lash":{"kind":"process_unknown"}});
    vec![
        lash_core::ToolDefinition::raw(
            "tool:container_mint", "container_mint", "Return a process handle for the journal law",
            serde_json::json!({"type":"object"}), handle.clone(),
        ).expect("valid declared tool schemas").with_tool_binding(lash_lashlang_runtime::ToolBinding::new(["tools"], "mint")),
        lash_core::ToolDefinition::raw(
            "tool:container_check", "container_check", "Check the journaled process handle",
            serde_json::json!({"type":"object","additionalProperties":false,"properties":{"handle":handle},"required":["handle"]}), handle,
        ).expect("valid declared tool schemas").with_tool_binding(lash_lashlang_runtime::ToolBinding::new(["tools"], "check")),
    ]
}

struct JournalHandleProvider {
    mints: Arc<AtomicUsize>,
    checks: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for JournalHandleProvider {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        journal_definitions()
            .iter()
            .map(lash_core::ToolDefinition::manifest)
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        journal_definitions()
            .into_iter()
            .find(|definition| definition.manifest().name == name)
            .map(|definition| Arc::new(definition.contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        match call.name() {
            "container_mint" => {
                self.mints.fetch_add(1, Ordering::SeqCst);
            }
            "container_check" => {
                assert_eq!(call.args["handle"], journal_handle());
                self.checks.fetch_add(1, Ordering::SeqCst);
            }
            other => panic!("unexpected journal-law tool: {other}"),
        }
        lash_core::ToolOutcome::ok(journal_handle()).into()
    }
}

#[test]
fn process_handles_in_containers_replay_without_repeating_tool_effects() {
    block_on(async {
        for (storage, access) in [
            (
                "const handles=[]; handles.push(await tools.mint({}));",
                "handles[0]",
            ),
            (
                "const handles={child:await tools.mint({})};",
                "handles.child",
            ),
        ] {
            let mints = Arc::new(AtomicUsize::new(0));
            let checks = Arc::new(AtomicUsize::new(0));
            let run = CellAttemptRun {
                provider: Arc::new(JournalHandleProvider {
                    mints: Arc::clone(&mints),
                    checks: Arc::clone(&checks),
                }),
                catalog: lash_core::ToolCatalog::from_tool_definitions(journal_definitions()),
                invocation: lash_core::testing::exec_code_invocation(
                    "container-journal-replay",
                    "turn-1",
                    0,
                    0,
                    "container-replay",
                    "exec-code:container-replay",
                ),
                request: ExecRequest {
                    code: format!("{storage} finish(await tools.check({{handle:{access}}}));"),
                },
                resolver: None,
                layer: None,
            };
            let outcomes = run_cell_through_crashes(
                "container-journal-replay",
                "turn-1",
                vec![run.clone()],
                run,
                &[mints, checks],
            )
            .await;
            assert_eq!(outcomes.len(), 2, "a crashed attempt and its redrive");
            for outcome in outcomes {
                assert!(
                    outcome.response.error.is_none(),
                    "{:?}",
                    outcome.response.error
                );
                assert_eq!(outcome.response.terminal_finish, Some(journal_handle()));
                assert_eq!(
                    outcome.counts,
                    [1, 1],
                    "the journal replays the handle without re-running tools"
                );
            }
        }
    });
}
