//! FIG-3586: a code cell's nested effects are keyed by issue ordinal, and a
//! redrive that cannot replay its journal refuses with nothing dispatched.
//!
//! Each law runs a cell against a file-backed SQLite effect journal, then
//! re-executes the same cell — the same `exec_code` replay key — against a
//! cold reopen of that journal, as a redrive after a crash before the turn
//! commit does. Changing the cell's source between the two runs stands in for
//! a changed build: a rewrite that keeps the order of the commands the cell
//! issues must replay, and one that changes it must refuse.

use super::*;
use lash_core::RuntimeEffectController as _;

const SESSION: &str = "replay-ordinals";
const TURN: &str = "turn-1";
const EXEC_KEY: &str = "exec-code:ordinals";

fn app_tool(id: &str, name: &str, operation: &str) -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        id,
        name,
        "Replay-ordinal fixture tool",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object" }),
    )
    .with_tool_binding(lash_lashlang_runtime::ToolBinding::new(["app"], operation))
}

/// `app.a` and `app.b`, echoing their arguments, and `app.d`, which defers
/// its outcome to an out-of-band completion. `revision` renames the tool
/// `app.a` resolves to, as a changed tool surface does; `app_a` removes that
/// tool, rewords its descriptor, or changes its retry policy under the same
/// name (FIG-3587); `app_d` does the same to `app.d`.
#[derive(Clone, Default)]
pub(super) struct AppTools {
    calls: Arc<Mutex<Vec<(String, Value)>>>,
    call_ids: Arc<Mutex<Vec<String>>>,
    revision: &'static str,
    app_a: AppA,
    app_d: AppA,
    /// The journal `app.d` resolves its completion on.
    completions: Option<std::path::PathBuf>,
}

/// What the registry holds for a tool.
#[derive(Clone, Copy, Debug, Default)]
pub(super) enum AppA {
    #[default]
    Registered,
    Removed,
    Described(&'static str),
    /// A dispatch-relevant change: another retry policy.
    Retried,
}

impl AppA {
    fn apply(self, definition: lash_core::ToolDefinition) -> Option<lash_core::ToolDefinition> {
        match self {
            Self::Registered => Some(definition),
            Self::Removed => None,
            Self::Described(description) => Some(lash_core::ToolDefinition {
                manifest: lash_core::ToolManifest {
                    description: description.to_string(),
                    ..definition.manifest
                },
                contract: definition.contract,
            }),
            Self::Retried => Some(lash_core::ToolDefinition {
                manifest: lash_core::ToolManifest {
                    retry_policy: lash_core::ToolRetryPolicy::Safe {
                        max_attempts: 3,
                        base_delay_ms: 10,
                        max_delay_ms: 100,
                    },
                    ..definition.manifest
                },
                contract: definition.contract,
            }),
        }
    }
}

impl AppTools {
    fn revised(&self, revision: &'static str) -> Self {
        Self {
            revision,
            ..self.clone()
        }
    }

    /// The same tools, sharing the dispatch log, with `app.a` as `app_a`.
    pub(super) fn with_app_a(&self, app_a: AppA) -> Self {
        Self {
            app_a,
            ..self.clone()
        }
    }

    /// The same tools, sharing the dispatch log, with `app.d` as `app_d`.
    pub(super) fn with_app_d(&self, app_d: AppA) -> Self {
        Self {
            app_d,
            ..self.clone()
        }
    }

    /// The same tools, resolving `app.d`'s completions on `journal`.
    pub(super) fn completing_on(&self, journal: &std::path::Path) -> Self {
        Self {
            completions: Some(journal.to_path_buf()),
            ..self.clone()
        }
    }

    fn definitions(&self) -> Vec<lash_core::ToolDefinition> {
        let a = app_tool(
            &format!("tool:app_a{}", self.revision),
            &format!("app_a{}", self.revision),
            "a",
        );
        self.app_a
            .apply(a)
            .into_iter()
            .chain([app_tool("tool:app_b", "app_b", "b")])
            .chain(self.app_d.apply(app_tool("tool:app_d", "app_d", "d")))
            .collect()
    }

    pub(super) fn dispatched(&self) -> usize {
        self.calls.lock_recover().len()
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for AppTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        self.definitions()
            .into_iter()
            .map(|definition| definition.manifest())
            .collect()
    }

    fn resolve_manifest_by_id(&self, id: &lash_core::ToolId) -> Option<lash_core::ToolManifest> {
        self.definitions()
            .into_iter()
            .find(|definition| definition.manifest().id == *id)
            .map(|definition| definition.manifest())
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        self.definitions()
            .into_iter()
            .find(|definition| {
                let manifest = definition.manifest();
                manifest.name == name || manifest.id.as_str() == name
            })
            .map(|definition| Arc::new(definition.contract()))
    }

    fn attempt_may_defer(&self, tool_id: &lash_core::ToolId) -> bool {
        tool_id.as_str() == "tool:app_d"
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let name = call.name().to_string();
        let args = call.args.clone();
        self.calls.lock_recover().push((name.clone(), args.clone()));
        self.call_ids
            .lock_recover()
            .push(call.context.tool_call_id().unwrap_or_default().to_string());
        if name == "app_d" {
            let key = call
                .context
                .completion_key()
                .expect("a deferring tool is issued a completion key");
            let journal = self
                .completions
                .clone()
                .expect("a deferring tool knows the journal it completes on");
            let value = serde_json::json!({ "tool": name, "args": args });
            lash_core::task::spawn(async move {
                let host = lash_sqlite_store::SqliteEffectHost::open(&journal)
                    .await
                    .expect("open the completion journal");
                lash_core::AwaitEventResolver::resolve_await_event(
                    &host,
                    &key,
                    lash_core::Resolution::Ok(value),
                )
                .await
                .expect("resolve the deferred completion");
            });
            return lash_core::ToolAttemptOutcome::Pending(lash_core::PendingCompletion::new());
        }
        (async { lash_core::ToolOutcome::ok(serde_json::json!({ "tool": name, "args": args })) })
            .await
            .into()
    }
}

pub(super) struct Run {
    pub(super) response: ExecResponse,
    pub(super) nested: Option<lash_core::RuntimeEffectControllerError>,
}

impl Run {
    pub(super) fn refusal_code(&self) -> Option<lash_core::RuntimeErrorCode> {
        self.nested.as_ref().map(|error| error.code.clone())
    }

    pub(super) fn assert_clean(&self) {
        assert!(self.nested.is_none(), "{:?}", self.nested);
        assert!(self.response.error.is_none(), "{:?}", self.response.error);
    }

    pub(super) fn assert_diverged(&self) {
        assert_eq!(
            self.refusal_code(),
            Some(lash_core::RuntimeErrorCode::LashlangCellReplayDivergence),
            "{:?}",
            self.nested
        );
    }
}

/// Where a cell runs: its turn and its `exec_code` replay key.
#[derive(Clone, Copy)]
pub(super) struct CellAddress<'a> {
    pub(super) session: &'a str,
    pub(super) turn: &'a str,
    pub(super) exec_key: &'a str,
}

const CELL: CellAddress<'static> = CellAddress {
    session: SESSION,
    turn: TURN,
    exec_key: EXEC_KEY,
};

pub(super) struct Journal {
    _directory: tempfile::TempDir,
    pub(super) path: std::path::PathBuf,
}

impl Journal {
    pub(super) fn open() -> Self {
        let directory = tempfile::tempdir().expect("temporary effect journal");
        let path = directory.path().join("journal.sqlite");
        Self {
            _directory: directory,
            path,
        }
    }

    async fn controller_for(
        &self,
        session: &str,
        turn: &str,
    ) -> lash_sqlite_store::SqliteRuntimeEffectController {
        lash_sqlite_store::SqliteRuntimeEffectController::open(
            &self.path,
            lash_core::ExecutionScope::turn(session, turn),
        )
        .await
        .expect("open the SQLite effect journal")
    }

    /// A cold open of the journal as a durable effect host, its tool children
    /// wired as the turn driver wires them.
    pub(super) async fn host(&self) -> Arc<lash_sqlite_store::SqliteEffectHost> {
        Arc::new(
            lash_sqlite_store::SqliteEffectHost::open(&self.path)
                .await
                .expect("open the SQLite effect host"),
        )
    }

    pub(super) async fn run(&self, code: &str, tools: &AppTools) -> Run {
        self.run_under(code, tools, self.host().await).await
    }

    pub(super) async fn run_under(
        &self,
        code: &str,
        tools: &AppTools,
        host: Arc<lash_sqlite_store::SqliteEffectHost>,
    ) -> Run {
        run_cell(
            CELL,
            code,
            tools,
            host,
            &mut RlmExecutionState::for_engine("typescript"),
        )
        .await
    }

    /// Every replay and group key the journal holds for `session`'s `turn`.
    pub(super) async fn keys_of(&self, session: &str, turn: &str) -> (Vec<String>, Vec<String>) {
        match self
            .controller_for(session, turn)
            .await
            .read_recorded_journal(&lash_core::RecordedKeyRange {
                lower: String::new(),
                upper: "\u{10FFFF}".to_string(),
                group_key_prefix: String::new(),
            })
            .await
            .expect("read the journal's keys")
        {
            lash_core::RecordedJournal::Keys(keys) => (keys.replay_keys, keys.group_keys),
            other => panic!("a SQLite journal answers with its keys: {other:?}"),
        }
    }

    pub(super) async fn keys(&self) -> (Vec<String>, Vec<String>) {
        self.keys_of(SESSION, TURN).await
    }
}

/// Runs one TypeScript cell at `cell` through `host`, as the turn driver runs
/// it: `state` carries the interpreter across the turn's cells.
pub(super) async fn run_cell(
    cell: CellAddress<'_>,
    code: &str,
    tools: &AppTools,
    host: Arc<lash_sqlite_store::SqliteEffectHost>,
    state: &mut RlmExecutionState,
) -> Run {
    // Triggers answer from a store of their own: a trigger operation is a
    // leaf a cell's aggregate may settle before its group head.
    let triggers = lash_core::testing::test_trigger_router(
        Arc::new(lash_core::facade_support::InMemoryTriggerStore::default()),
        crate::testing::memory_process_registry().await,
    );
    let ctx = lash_core::testing::TestExecutionContextBuilder::new(
        lash_core::testing::TestExecutionPorts::over_host(
            host as Arc<dyn lash_core::EffectHost>,
            Arc::new(lash_core::facade_support::InMemoryProcessExecutionEnvStore::new()),
        ),
    )
    .provider(Arc::new(tools.clone()))
    .tool_catalog(lash_core::ToolCatalog::from_tool_definitions(
        tools.definitions(),
    ))
    .trigger_router(Some(triggers))
    .runtime_parent_invocation(lash_core::testing::exec_code_invocation(
        cell.session,
        cell.turn,
        0,
        0,
        "replay exec",
        cell.exec_key,
    ))
    .build()
    .into_runtime();
    let probe = ctx.clone();
    let surface = LashlangSurface::new(
        lashlang::LashlangAbilities::default().with_sleep(),
        lashlang::LashlangLanguageFeatures::default(),
        lashlang::LashlangHostCatalog::new(),
    );
    let response = execute_code_unbounded_for_tests(
        state,
        ctx,
        ExecRequest {
            language: "typescript".to_string(),
            code: code.to_string(),
        },
        lashlang::global_in_memory_lashlang_artifact_store(),
        surface,
        None,
        RlmProjectedBindings::default(),
        Arc::new(ProjectionRegistry::new()),
        RlmLashlangExecutionTraceConfig::default(),
    )
    .await;
    Run {
        response,
        nested: probe.take_nested_effect_error(),
    }
}

/// T1: a rewrite that keeps the order of the commands a cell issues — new
/// statements, a wrapping block, so every node id and instruction pointer
/// moves — replays the whole journal: scalar calls, a journaled runtime
/// value, an all-results aggregate and a race against a timer. Nothing is
/// dispatched, the result is identical, and the journal gains no row: no
/// compiler byte reaches a key or an envelope.
#[test]
fn an_order_preserving_rewrite_replays_with_nothing_dispatched() {
    block_on(async {
        let journal = Journal::open();
        let tools = AppTools::default();
        let first = journal
            .run(
                r#"
                const a = await app.a({ n: 1 });
                const now = Date.now();
                const both = await Promise.all([app.a({ n: 2 }), app.b({ n: 3 })]);
                const raced = await Promise.race([app.b({ n: 4 }), sleep(60000)]);
                finish({ a: a, now: now, both: both, raced: raced });
                "#,
                &tools,
            )
            .await;
        first.assert_clean();
        let dispatched = tools.dispatched();
        assert_eq!(dispatched, 4);
        let recorded = journal.keys().await;

        let rewritten = journal
            .run(
                r#"
                let pad = 0;
                {
                    pad = pad + 1;
                    const a = await app.a({ n: 1 });
                    const label = "unrelated " + pad;
                    const now = Date.now();
                    const both = await Promise.all([app.a({ n: 2 }), app.b({ n: 3 })]);
                    const raced = await Promise.race([app.b({ n: 4 }), sleep(60000)]);
                    finish({ a: a, now: now, both: both, raced: raced });
                }
                "#,
                &tools,
            )
            .await;
        rewritten.assert_clean();
        assert_eq!(
            tools.dispatched(),
            dispatched,
            "a replay dispatches nothing"
        );
        assert_eq!(
            rewritten.response.terminal_finish, first.response.terminal_finish,
            "the replay reproduces the first run's result, `Date.now()` included"
        );
        assert_eq!(
            journal.keys().await,
            recorded,
            "a replay writes no new journal row"
        );
    });
}

/// T2: swapping two calls refuses at ordinal 0 with nothing dispatched, and a
/// second redrive refuses again.
#[test]
fn a_reordering_rewrite_refuses_at_the_first_ordinal() {
    block_on(async {
        let journal = Journal::open();
        let tools = AppTools::default();
        journal
            .run(
                "await app.a({ n: 1 }); await app.b({ n: 2 }); finish(1);",
                &tools,
            )
            .await
            .assert_clean();
        assert_eq!(tools.dispatched(), 2);
        let recorded = journal.keys().await;
        for _ in 0..2 {
            let swapped = journal
                .run(
                    "await app.b({ n: 2 }); await app.a({ n: 1 }); finish(1);",
                    &tools,
                )
                .await;
            swapped.assert_diverged();
            let message = &swapped.nested.as_ref().expect("refusal").message;
            assert!(message.contains(":lk2:0000000000"), "{message}");
            assert_eq!(tools.dispatched(), 2, "a refusal dispatches nothing");
            assert_eq!(journal.keys().await, recorded, "a refusal writes nothing");
        }
    });
}

/// T3: a journaled effect inserted before a recorded call refuses at the
/// insertion ordinal.
#[test]
fn an_inserted_journaled_effect_refuses_at_its_ordinal() {
    block_on(async {
        let journal = Journal::open();
        let tools = AppTools::default();
        journal
            .run("await app.a({ n: 1 }); finish(1);", &tools)
            .await
            .assert_clean();
        let inserted = journal
            .run(
                "const now = Date.now(); await app.a({ n: 1 }); finish(now);",
                &tools,
            )
            .await;
        inserted.assert_diverged();
        let message = &inserted.nested.as_ref().expect("refusal").message;
        assert!(message.contains("issue ordinal 0"), "{message}");
        assert_eq!(tools.dispatched(), 1);
    });
}

/// T4: a recorded scalar call replayed as a one-leaf aggregate refuses at its
/// ordinal: the range read finds the scalar row where the aggregate would
/// open its group.
#[test]
fn a_kind_change_at_an_ordinal_refuses() {
    block_on(async {
        let journal = Journal::open();
        let tools = AppTools::default();
        journal
            .run("await app.a({ n: 1 }); finish(1);", &tools)
            .await
            .assert_clean();
        let recorded = journal.keys().await;
        journal
            .run("await Promise.all([app.a({ n: 1 })]); finish(1);", &tools)
            .await
            .assert_diverged();
        assert_eq!(tools.dispatched(), 1);
        assert_eq!(
            journal.keys().await,
            recorded,
            "the refused aggregate opens no group"
        );
    });
}

/// T5: the seal of a completed cell. An extra trailing command refuses when
/// the range read finds the seal; a dropped trailing command refuses at the
/// seal.
#[test]
fn a_completed_cells_seal_refuses_an_extra_or_a_dropped_command() {
    block_on(async {
        // Both runs link the same surface, so only the commands differ.
        let journal = Journal::open();
        let tools = AppTools::default();
        journal
            .run(
                "const more = false; await app.a({ n: 1 }); if (more) { await app.b({ n: 2 }); } finish(1);",
                &tools,
            )
            .await
            .assert_clean();
        let extra = journal
            .run(
                "const more = true; await app.a({ n: 1 }); if (more) { await app.b({ n: 2 }); } finish(1);",
                &tools,
            )
            .await;
        extra.assert_diverged();
        let message = &extra.nested.as_ref().expect("refusal").message;
        assert!(message.contains("issue ordinal 1"), "{message}");
        assert_eq!(
            tools.dispatched(),
            1,
            "the extra command is never dispatched"
        );

        let journal = Journal::open();
        let tools = AppTools::default();
        journal
            .run(
                "await app.a({ n: 1 }); await app.b({ n: 2 }); finish(1);",
                &tools,
            )
            .await
            .assert_clean();
        let dropped = journal
            .run("await app.a({ n: 1 }); finish(1);", &tools)
            .await;
        dropped.assert_diverged();
        let message = &dropped.nested.as_ref().expect("refusal").message;
        assert!(message.contains("seal"), "{message}");
        assert_eq!(tools.dispatched(), 2);
    });
}

/// T6: a cell that crashed after two of its four calls replays the two and
/// runs the rest live, each exactly once; a redrive whose second call changed
/// its arguments refuses instead.
#[test]
fn a_mid_cell_crash_replays_its_prefix_and_runs_the_rest_live() {
    const CELL: &str = r#"
        await app.a({ n: 1 });
        await app.a({ n: 2 });
        await app.b({ n: 3 });
        await app.b({ n: 4 });
        finish(1);
    "#;
    block_on(async {
        let journal = Journal::open();
        let tools = AppTools::default();
        let crashing = journal.host().await;
        let faults = crashing.effect_journal_faults();
        faults.fail_next(
            lash_core::runtime::effect::effect_replay_driver::EffectJournalFaultPoint::Claim,
            &format!("{EXEC_KEY}:lk2:0000000002:attempt:1"),
        );
        let crashed = journal.run_under(CELL, &tools, crashing).await;
        assert!(faults.fired(), "the injected crash fired at the third call");
        assert!(
            crashed.nested.is_some(),
            "the injected crash aborts the cell"
        );
        assert_eq!(tools.dispatched(), 2);

        let perturbed = journal.run(&CELL.replace("n: 2", "n: 20"), &tools).await;
        perturbed.assert_diverged();
        assert_eq!(tools.dispatched(), 2);

        journal.run(CELL, &tools).await.assert_clean();
        let calls = tools.calls.lock_recover().clone();
        assert_eq!(
            calls
                .iter()
                .map(|(_, args)| args["n"].clone())
                .collect::<Vec<_>>(),
            vec![
                serde_json::json!(1),
                serde_json::json!(2),
                serde_json::json!(3),
                serde_json::json!(4)
            ],
            "the prefix is served from the journal and the rest runs once each"
        );
    });
}

/// T7: an aggregate whose arguments drifted refuses at its group head and
/// opens no new group.
#[test]
fn an_aggregate_whose_arguments_drifted_refuses_at_its_group_head() {
    block_on(async {
        let journal = Journal::open();
        let tools = AppTools::default();
        journal
            .run(
                "await Promise.all([app.a({ n: 1 }), app.b({ n: 2 })]); finish(1);",
                &tools,
            )
            .await
            .assert_clean();
        let recorded = journal.keys().await;
        assert_eq!(recorded.1.len(), 1, "the aggregate recorded one group");
        journal
            .run(
                "await Promise.all([app.a({ n: 1 }), app.b({ n: 3 })]); finish(1);",
                &tools,
            )
            .await
            .assert_diverged();
        assert_eq!(tools.dispatched(), 2);
        assert_eq!(journal.keys().await, recorded, "no new group row");
    });
}

/// T8: a guest `catch` cannot swallow a divergence. The refusal stops the
/// cell, so the call after the `try` is never dispatched.
#[test]
fn a_divergence_cannot_be_caught_by_the_cell() {
    block_on(async {
        let journal = Journal::open();
        let tools = AppTools::default();
        journal
            .run(
                "const later = false; await app.a({ n: 1 }); if (later) { await app.b({ n: 3 }); } finish(1);",
                &tools,
            )
            .await
            .assert_clean();
        let caught = journal
            .run(
                r#"
                const later = true;
                try { await app.a({ n: 2 }); } catch (error) {}
                if (later) { await app.b({ n: 3 }); }
                finish(1);
                "#,
                &tools,
            )
            .await;
        caught.assert_diverged();
        let message = &caught.nested.as_ref().expect("refusal").message;
        assert!(message.contains(":lk2:0000000000"), "{message}");
        assert_eq!(
            tools.dispatched(),
            1,
            "nothing after the divergence leaves the cell"
        );
    });
}

/// T9: the tool an alias resolves to was renamed between the runs. The
/// redrive links the call against the cell's recorded binding (FIG-3587) and
/// replays its recorded result instead of dispatching the new tool live.
#[test]
fn a_renamed_alias_binding_replays_its_recorded_result() {
    block_on(async {
        let journal = Journal::open();
        let tools = AppTools::default();
        let first = journal
            .run("const a = await app.a({ n: 1 }); finish(a);", &tools)
            .await;
        first.assert_clean();
        let revised = tools.revised("_v2");
        let redriven = journal
            .run("const a = await app.a({ n: 1 }); finish(a);", &revised)
            .await;
        redriven.assert_clean();
        assert_eq!(
            redriven.response.terminal_finish, first.response.terminal_finish,
            "the redrive answers from the recorded result"
        );
        assert_eq!(tools.dispatched(), 1, "the revised tool never runs");
    });
}

/// T10: the id a call is minted — the id a spawned subagent's process id and
/// a signal's id derive from — is its issue ordinal under the cell, so a
/// rewrite that moves every node id and instruction pointer mints the same
/// ids, and a redrive re-mints no child.
#[test]
fn a_rewrite_mints_the_same_call_ids() {
    block_on(async {
        let original = AppTools::default();
        Journal::open()
            .run(
                "await app.a({ n: 1 }); await Promise.all([app.b({ n: 2 }), app.b({ n: 3 })]); finish(1);",
                &original,
            )
            .await
            .assert_clean();
        let rewritten = AppTools::default();
        Journal::open()
            .run(
                "let pad = 1; { pad = pad + 1; await app.a({ n: 1 }); const x = [pad]; await Promise.all([app.b({ n: 2 }), app.b({ n: 3 })]); finish(1); }",
                &rewritten,
            )
            .await
            .assert_clean();
        let minted = original.call_ids.lock_recover().clone();
        assert_eq!(minted.len(), 3);
        for (id, suffix) in
            minted
                .iter()
                .zip([":0000000000", ":0000000001:child:0", ":0000000001:child:1"])
        {
            assert!(
                id.starts_with("lashlang:v2:") && id.ends_with(suffix),
                "call ids are issue ordinals under the cell: {minted:?}"
            );
        }
        assert_eq!(minted, rewritten.call_ids.lock_recover().clone());
    });
}

/// T7b: an aggregate whose shape drifted at a recorded ordinal — another
/// leaf, or another consumer — refuses at its group head under the replay
/// divergence, so the turn parks instead of failing.
#[test]
fn an_aggregate_whose_shape_drifted_refuses_as_a_divergence() {
    const RECORDED: &str = "await Promise.all([app.a({ n: 1 }), app.b({ n: 2 })]); finish(1);";
    for drifted in [
        "await Promise.all([app.a({ n: 1 }), app.b({ n: 2 }), app.b({ n: 3 })]); finish(1);",
        "await Promise.race([app.a({ n: 1 }), app.b({ n: 2 })]); finish(1);",
    ] {
        block_on(async {
            let journal = Journal::open();
            let tools = AppTools::default();
            journal.run(RECORDED, &tools).await.assert_clean();
            let recorded = journal.keys().await;
            journal.run(drifted, &tools).await.assert_diverged();
            assert_eq!(tools.dispatched(), 2, "{drifted}: nothing is dispatched");
            assert_eq!(
                journal.keys().await,
                recorded,
                "{drifted}: nothing is journaled"
            );
        });
    }
}

/// FIG-3586 (per-key frontier): a leaf the recorded aggregate did not have —
/// here a trigger operation, settled before the group head is checked, as an
/// alias that moved to a trigger on redrive would be —
/// is refused at its key while the journal holds entries beyond, so nothing
/// is journaled live inside the recorded run.
#[test]
fn a_leaf_settled_before_the_group_head_cannot_write_an_unrecorded_key() {
    block_on(async {
        let journal = Journal::open();
        let tools = AppTools::default();
        journal
            .run(
                "await Promise.all([app.a({ n: 1 })]); const later = await triggers.list({}); finish(later);",
                &tools,
            )
            .await
            .assert_clean();
        let recorded = journal.keys().await;
        let drifted = journal
            .run(
                "await Promise.all([app.a({ n: 1 }), triggers.list({})]); const later = await triggers.list({}); finish(later);",
                &tools,
            )
            .await;
        drifted.assert_diverged();
        let message = &drifted.nested.as_ref().expect("refusal").message;
        assert!(
            message.contains(":lk2:0000000000:child:1"),
            "the leaf is refused at its own unrecorded key: {message}"
        );
        assert_eq!(tools.dispatched(), 1, "nothing is dispatched");
        assert_eq!(
            journal.keys().await,
            recorded,
            "the unrecorded leaf journals nothing"
        );
    });
}
