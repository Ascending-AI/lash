//! FIG-4860: a physical segment transfers its logical opener's live groups.

use super::*;

#[derive(Default)]
struct HeldTools {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
    gate: tokio::sync::Notify,
    gate_release: tokio::sync::Notify,
    calls: std::sync::atomic::AtomicUsize,
}

fn held_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:segment_hold",
        "segment_hold",
        "A leaf held by the segment law.",
        serde_json::json!({ "type": "object", "properties": {
            "gate": { "type": "boolean" }
        }, "additionalProperties": false }),
        serde_json::json!({ "type": "string" }),
    )
    .expect("valid held tool")
    .with_tool_binding(lash_lashlang_runtime::ToolBinding::new(["law"], "hold"))
}

#[async_trait]
impl ToolProvider for HeldTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![held_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "segment_hold").then(|| Arc::new(held_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if call.args["gate"].as_bool() == Some(true) {
            self.gate.notify_one();
            self.gate_release.notified().await;
        } else {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            self.release.notified().await;
        }
        lash_core::ToolOutcome::ok(serde_json::json!("released")).into()
    }
}

#[derive(Clone, Copy, Debug)]
enum Boundary {
    InsideCell,
    EarlierCell,
    BetweenCells,
    JournalBudget,
}

#[derive(Clone, Copy, Debug)]
enum Disruption {
    Engine(Crash),
    CapturedBeforeCommit,
    CancelAtCapture,
    CancelSuccessor,
}

async fn held_loser_survives(
    storage: Storage,
    boundary: Boundary,
    disruption: Option<Disruption>,
) -> Result<()> {
    let case = match disruption {
        None => "steady".to_string(),
        Some(Disruption::Engine(crash)) => match crash {
            Crash::OldRunWhileParked => "old-park",
            Crash::OldRunAfterTheWake => "wake",
            Crash::OldRunAfterItsBoundaryCommit => "old-commit",
            Crash::ContinuationBeforeItsFirstStep => "next-start",
            Crash::ContinuationWhileParked => "next-park",
            Crash::ContinuationAfterItsCommit => "next-commit",
        }
        .to_string(),
        Some(Disruption::CapturedBeforeCommit) => "captured-before-commit".to_string(),
        Some(Disruption::CancelAtCapture) => "cancel-at-capture".to_string(),
        Some(Disruption::CancelSuccessor) => "cancel-successor".to_string(),
    };
    let session_name = format!("held-loser-{boundary:?}-{case}");
    let signal = format!("go-{session_name}");
    let World { engine, _keep, .. } = if matches!(boundary, Boundary::JournalBudget) {
        super::super::run_segment::budget_world(storage, 1).await
    } else {
        double_world(storage).await
    };
    let Engine::Double(double) = &engine else {
        unreachable!("the law runs on the double");
    };
    let crashes = lash_restate_test::CrashCount::new();
    assert!(double.server().on_crash(crashes.listener()));
    if matches!(boundary, Boundary::JournalBudget | Boundary::BetweenCells)
        && let Some(Disruption::Engine(crash)) = disruption
        && let Some(rule) = crash.rule(0)
    {
        double.server().crash_on(rule);
    }
    let script = Arc::new(lash_core::testing::Script::new());
    let held = Arc::new(HeldTools::default());
    let race = "await Promise.race([law.hold({}), sleep(20)]);";
    let code = cell(&signal);
    let cells = match boundary {
        Boundary::InsideCell => {
            vec![code.replacen("const worker", &format!("{race}\nconst worker"), 1)]
        }
        _ => vec![
            typescript_block(&format!(
                "{race}\nawait law.hold({{ gate: true }});\nprint(\"first cell\");"
            )),
            code,
        ],
    };
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let scripted = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from(
        cells,
    )));
    let provider = {
        let requests = Arc::clone(&requests);
        crate::testing::TestProvider::builder()
            .kind("held-loser")
            .complete(move |request| {
                requests.lock_recover().push(request);
                let code = scripted.lock_recover().pop_front().unwrap_or_else(|| {
                    typescript_block(r#"finish({ answer: "done", before: 42 });"#)
                });
                async move { Ok(text_response(&code)) }
            })
            .build()
            .into_handle()
    };
    let backend = lash_core::testing::runtime_helpers::LayeredBackend::over(engine.old_backend())
        .with_session_work(engine.old_work())
        .map_session_store_factory({
            let script = Arc::clone(&script);
            move |inner| script.wrap("deployment", inner)
        })
        .into_backend();
    let core = rlm_core_builder_over(backend)
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(
            crate::QueuedWorkBatchingConfig::new(1024).with_max_turn_input_admission(1),
        )
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .tools(Arc::clone(&held) as Arc<dyn ToolProvider>)
        .plugin(Arc::new(
            lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(
                lash_core::lifetime::session_or_starter,
            ),
        ))
        .plugin(lash_core::testing::process_engine_plugin_fixture())
        .plugin(Arc::new(StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial("loser-facts"),
            lash_core::facade_support::PluginSpec::new().with_tool_result_check(
                crate::hook_key!("loser-fact"),
                Arc::new(|input| {
                    let loser = input.prepared.tool_name() == "segment_hold"
                        && input.prepared.args()["gate"] != true;
                    Box::pin(async move {
                        Ok(lash_core::plugin::AfterToolContributions {
                            messages: if loser {
                                vec![lash_core::PluginMessage::text(
                                    lash_core::MessageRole::User,
                                    "loser-fact",
                                )]
                            } else {
                                Vec::new()
                            },
                            ..Default::default()
                        })
                    })
                }),
            ),
        )))
        .build(crate::testing::runtime_lease_owner())?;
    double.install_process_worker(
        lash_core_worker::DurableProcessWorker::new(core.durable_process_worker_config()?)
            .expect("the law's process worker"),
    );
    let session = lash_core::SessionId::fixture(&session_name);
    let handle = core
        .session(session.clone())
        .created()
        .await
        .open()
        .await?
        .send(TurnInput::text("race a timer, then wait for the process"))
        .id("run-run")
        .await?;
    tokio::time::timeout(WEDGE, held.started.notified())
        .await
        .unwrap_or_else(|error| {
            panic!(
                "the loser never starts: {error}; requests: {:?}; invocations: {:?}",
                requests.lock_recover().len(),
                double.server().invocations(),
            )
        });
    if !matches!(boundary, Boundary::InsideCell) {
        tokio::time::timeout(WEDGE, held.gate.notified())
            .await
            .expect("the first cell's gate starts");
    }
    let old = engine
        .old_backend()
        .build_generation()
        .expect("bound generation")
        .clone();
    let next = if matches!(boundary, Boundary::JournalBudget) {
        old.clone()
    } else {
        BuildGeneration::for_test("held-loser-next")
    };
    if matches!(boundary, Boundary::BetweenCells) {
        engine
            .roll(next.clone(), &Arc::new(Model::holding(0)))
            .await;
        engine
            .old_backend()
            .generation_drain()
            .mark_draining(&old, 1)
            .await?;
    }
    if !matches!(boundary, Boundary::InsideCell) {
        held.gate_release.notify_one();
    }
    let process = waiting_process(&core, &signal).await;
    let mut roll = CellRoll {
        engine,
        core,
        requests,
        session,
        handle: Some(handle),
        process,
        signal,
        old,
        next,
        _keep,
        script: Arc::clone(&script),
    };
    let continuation = matches!(boundary, Boundary::BetweenCells | Boundary::JournalBudget);
    let key = if continuation {
        CONTINUATION
    } else {
        "run-run"
    };
    let parked = roll.parked_run(key, 1).await;
    let groups = live_race_groups(roll.server());
    assert_eq!(groups.len(), 1, "one held race group: {groups:?}");
    assert_live_loser(roll.server(), &groups[0]);
    if !continuation {
        let commands = roll
            .server()
            .journal(&parked.id)
            .expect("the parked journal")
            .iter()
            .filter(|entry| entry.ty.is_command())
            .count();
        if let Some(Disruption::Engine(crash)) = disruption
            && let Some(rule) = crash.rule(commands)
        {
            roll.server().crash_on(rule);
        }
        roll.engine
            .roll(roll.next.clone(), &Arc::new(Model::holding(0)))
            .await;
        roll.engine
            .old_backend()
            .generation_drain()
            .mark_draining(&roll.old, 1)
            .await?;
        if matches!(
            disruption,
            Some(Disruption::CapturedBeforeCommit | Disruption::CancelAtCapture)
        ) {
            let op = lash_core::testing::StoreOp::authorize_turn_cancel_closure;
            let gate = script.on(op).nth(script.calls(op) + 1).before().pause();
            let mut handover = Box::pin(roll.hand_over());
            gate.reached_by(&mut handover, 1).await;
            assert_live_loser(roll.server(), &groups[0]);
            if matches!(disruption, Some(Disruption::CancelAtCapture)) {
                roll.handle
                    .as_ref()
                    .expect("the send handle")
                    .cancel()
                    .await?;
                gate.open_all();
                drop(handover);
                return assert_cancelled(roll, &groups[0], &held).await;
            }
            assert!(
                roll.server().crash(&parked.id),
                "the captured predecessor dies"
            );
            gate.open_all();
            handover.await?;
        } else {
            roll.hand_over().await?;
        }
        let resumed = roll.parked_run(CONTINUATION, 1).await;
        if matches!(
            disruption,
            Some(Disruption::Engine(Crash::ContinuationWhileParked))
        ) {
            assert!(roll.server().crash(&resumed.id));
            let replayed = roll.parked_run(CONTINUATION, 1).await;
            assert_eq!(
                replayed.attempts, 2,
                "the successor replays its existing subscription"
            );
        }
    }
    assert_live_loser(roll.server(), &groups[0]);
    assert_eq!(
        held.calls.load(Ordering::SeqCst),
        1,
        "the successor never dispatches the loser anew"
    );
    if matches!(disruption, Some(Disruption::CancelSuccessor)) {
        roll.handle
            .as_ref()
            .expect("the send handle")
            .cancel()
            .await?;
        return assert_cancelled(roll, &groups[0], &held).await;
    }
    held.release.notify_one();
    await_loser_seat(roll.server(), &groups[0]).await;
    roll.release_process().await?;
    let output = tokio::time::timeout(WEDGE, roll.sent().output())
        .await
        .expect("the Run ends")?;
    assert_eq!(
        output.result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::FinalValue {
            value: serde_json::json!({ "answer": "done", "before": 42 }),
        },)
    );
    let session = roll.core.session(roll.session.clone()).open().await?;
    assert_eq!(
        session
            .read_view()
            .messages()
            .iter()
            .filter(|message| message
                .parts
                .iter()
                .any(|part| part.content() == "loser-fact"))
            .count(),
        1,
        "the successor incorporates the loser's fact exactly once"
    );
    assert_eq!(held.calls.load(Ordering::SeqCst), 1);
    if matches!(
        disruption,
        Some(Disruption::Engine(_) | Disruption::CapturedBeforeCommit)
    ) {
        assert_eq!(crashes.get(), 1, "the named crash fired once");
    }
    let store = lash_core::runtime::live_session_view(&roll.core.store_factory, &roll.session)
        .await?
        .expect("the opened session's store");
    assert!(store.load_pending_follow_on().await?.is_none());
    let lifecycle = group_lifecycle(roll.server(), &groups[0]);
    assert_eq!(
        lifecycle["type"], "closed",
        "only the logical terminal closes the group"
    );
    Ok(())
}

async fn assert_cancelled(mut roll: CellRoll, group: &str, held: &HeldTools) -> Result<()> {
    let outcome = tokio::time::timeout(WEDGE, roll.sent().outcome())
        .await
        .expect("the cancelled Run answers")?;
    assert_eq!(outcome.status(), crate::TurnStatus::Cancelled);
    let lifecycle = group_lifecycle(roll.server(), group);
    assert_eq!(
        lifecycle["type"], "closed",
        "the logical cancellation closes its group: {lifecycle}"
    );
    assert_eq!(lifecycle["live"]["decisions"][1]["position"], 0);
    assert_eq!(
        lifecycle["live"]["decisions"][1]["seat"]["type"],
        "cancel_decided"
    );
    assert_eq!(held.calls.load(Ordering::SeqCst), 1);
    let store = lash_core::runtime::live_session_view(&roll.core.store_factory, &roll.session)
        .await?
        .expect("the opened session's store");
    assert!(store.load_pending_follow_on().await?.is_none());
    Ok(())
}

fn group_lifecycle(server: &lash_restate_test::RestateTestServer, key: &str) -> serde_json::Value {
    let state = server.object_state("EffectGroupIndex", key);
    let bytes = state
        .get("effect-group/v1/state")
        .expect("the group is retained");
    let value: serde_json::Value = serde_json::from_slice(bytes).expect("the stamped group record");
    value["body"]["lifecycle"].clone()
}

fn live_race_groups(server: &lash_restate_test::RestateTestServer) -> Vec<String> {
    server
        .invocations()
        .into_iter()
        .filter_map(|view| {
            view.target
                .strip_prefix("EffectGroupIndex/")
                .and_then(|target| target.rsplit_once('/').map(|(key, _)| key.to_string()))
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter(|key| {
            let lifecycle = group_lifecycle(server, key);
            lifecycle["live"]["shape"]["replay_keys"]
                .as_array()
                .is_some_and(|children| children.len() == 2)
        })
        .collect()
}

fn assert_live_loser(server: &lash_restate_test::RestateTestServer, key: &str) {
    let lifecycle = group_lifecycle(server, key);
    assert_eq!(
        lifecycle["type"], "ready",
        "a segment boundary must leave the opener live: {lifecycle}"
    );
    let decisions = lifecycle["live"]["decisions"]
        .as_array()
        .expect("the group decisions");
    assert_eq!(
        decisions.len(),
        1,
        "the timer won; the held loser has no final or cancel decision"
    );
    assert_eq!(decisions[0]["position"], 1);
}

async fn await_loser_seat(server: &lash_restate_test::RestateTestServer, key: &str) {
    let deadline = tokio::time::Instant::now() + WEDGE;
    loop {
        let lifecycle = group_lifecycle(server, key);
        if lifecycle["live"]["decisions"]
            .as_array()
            .is_some_and(|decisions| {
                decisions.len() == 2 && decisions[1]["seat"]["type"] == "seated"
            })
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the released loser never seated: {lifecycle}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn held_loser_survives_mid_cell_handover_sqlite_memory() -> Result<()> {
    held_loser_survives(Storage::SqliteMemory, Boundary::InsideCell, None).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn held_loser_survives_earlier_cell_handover_sqlite_memory() -> Result<()> {
    held_loser_survives(Storage::SqliteMemory, Boundary::EarlierCell, None).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn held_loser_survives_between_cell_handover_sqlite_memory() -> Result<()> {
    held_loser_survives(Storage::SqliteMemory, Boundary::BetweenCells, None).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn held_loser_survives_journal_budget_sqlite_memory() -> Result<()> {
    held_loser_survives(Storage::SqliteMemory, Boundary::JournalBudget, None).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn held_loser_survives_crash_before_capture_sqlite_memory() -> Result<()> {
    held_loser_survives(
        Storage::SqliteMemory,
        Boundary::InsideCell,
        Some(Disruption::Engine(Crash::OldRunAfterTheWake)),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn held_loser_survives_crash_after_boundary_commit_sqlite_memory() -> Result<()> {
    held_loser_survives(
        Storage::SqliteMemory,
        Boundary::InsideCell,
        Some(Disruption::Engine(Crash::OldRunAfterItsBoundaryCommit)),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn held_loser_survives_earlier_cell_crash_before_capture_sqlite_memory() -> Result<()> {
    held_loser_survives(
        Storage::SqliteMemory,
        Boundary::EarlierCell,
        Some(Disruption::Engine(Crash::OldRunAfterTheWake)),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn held_loser_survives_earlier_cell_crash_after_boundary_commit_sqlite_memory() -> Result<()>
{
    held_loser_survives(
        Storage::SqliteMemory,
        Boundary::EarlierCell,
        Some(Disruption::Engine(Crash::OldRunAfterItsBoundaryCommit)),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn held_loser_survives_crash_after_capture_before_commit_sqlite_memory() -> Result<()> {
    held_loser_survives(
        Storage::SqliteMemory,
        Boundary::InsideCell,
        Some(Disruption::CapturedBeforeCommit),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn held_loser_survives_earlier_cell_crash_after_capture_before_commit_sqlite_memory()
-> Result<()> {
    held_loser_survives(
        Storage::SqliteMemory,
        Boundary::EarlierCell,
        Some(Disruption::CapturedBeforeCommit),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn held_loser_survives_journal_budget_crash_after_commit_sqlite_memory() -> Result<()> {
    held_loser_survives(
        Storage::SqliteMemory,
        Boundary::JournalBudget,
        Some(Disruption::Engine(Crash::OldRunAfterItsBoundaryCommit)),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn held_loser_survives_successor_crash_sqlite_memory() -> Result<()> {
    held_loser_survives(
        Storage::SqliteMemory,
        Boundary::InsideCell,
        Some(Disruption::Engine(Crash::ContinuationWhileParked)),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn logical_cancel_closes_held_loser_after_handover_sqlite_memory() -> Result<()> {
    held_loser_survives(
        Storage::SqliteMemory,
        Boundary::InsideCell,
        Some(Disruption::CancelSuccessor),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn late_cancel_closes_captured_held_loser_sqlite_memory() -> Result<()> {
    held_loser_survives(
        Storage::SqliteMemory,
        Boundary::InsideCell,
        Some(Disruption::CancelAtCapture),
    )
    .await
}
