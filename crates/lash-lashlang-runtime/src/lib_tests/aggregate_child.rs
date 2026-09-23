use super::*;

#[tokio::test(flavor = "current_thread")]
async fn real_aggregate_child_await_names_both_without_fold_conflict() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct ChildHost(AtomicUsize);

    impl lashlang::ExecutionHost for ChildHost {
        async fn perform(
            &self,
            op: lashlang::AbilityOp,
        ) -> Result<lashlang::AbilityResult, lashlang::ExecutionHostError> {
            match op {
                lashlang::AbilityOp::ResourceOperation(operation)
                    if operation.operation == "start" =>
                {
                    let ordinal = self.0.fetch_add(1, Ordering::Relaxed) + 1;
                    let mut record = lashlang::Record::default();
                    record.insert(
                        lash_sansio::handle::HANDLE_FIELD.to_string(),
                        lashlang::Value::String(lash_sansio::handle::HANDLE_KIND.into()),
                    );
                    record.insert(
                        "id".to_string(),
                        lashlang::Value::String(
                            lash_sansio::handle::HandleId::process(&format!("child-{ordinal}"), 1)
                                .as_str()
                                .into(),
                        ),
                    );
                    Ok(lashlang::AbilityResult::Value(lashlang::Value::Record(
                        Arc::new(record),
                    )))
                }
                lashlang::AbilityOp::Await(_) => {
                    Ok(lashlang::AbilityResult::Value(lashlang::Value::Null))
                }
                lashlang::AbilityOp::Finish(value) | lashlang::AbilityOp::Fail(value) => {
                    Ok(lashlang::AbilityResult::Value(value))
                }
                _ => Err(lashlang::ExecutionHostError::new(
                    "unexpected child-host operation",
                )),
            }
        }
    }

    let program = b::module(
        vec![b::process(
            "echo",
            Vec::new(),
            b::block(vec![b::finish(b::null())]),
        )],
        vec![
            b::assign("first", b::start("echo", Vec::new())),
            b::assign("second", b::start("echo", Vec::new())),
            b::assign(
                "both",
                b::await_expr(b::tuple(vec![b::var("first"), b::var("second")])),
            ),
            b::finish(b::var("both")),
        ],
    );
    let store = Arc::new(TraceLashlangGraphStore::default());
    let identity = TraceLanguageExecutionIdentity {
        scope: lash_trace::TraceRuntimeScope::none(),
        subject: lash_trace::TraceRuntimeSubject::Process {
            process_id: lash_core::ProcessId::from("parent"),
        },
        source_identity: "source".to_string(),
        module_ref: "module".to_string(),
        entry_kind: "main".to_string(),
        entry_ref: None,
        entry_name: "main".to_string(),
        restate_invocation_id: None,
        generation: None,
    };
    let observer_store = Arc::clone(&store);
    let traced = LanguageTraceHost::new(ChildHost::default(), move |_: &ChildHost, payload| {
        let event = TraceLanguageExecution {
            event_key: "real-aggregate".to_string(),
            identity: identity.clone(),
            payload,
        };
        lash_trace::TraceSink::append(
            &*observer_store,
            &lash_trace::TraceRecord::new(
                lash_trace::TraceContext::default(),
                lash_trace::TraceEvent::LanguageExecution {
                    language: "lashlang".to_string(),
                    event,
                },
            ),
        )
        .expect("trace append");
    });
    let compiled = lashlang::testing::harness::compile_labeled_program(program);
    lashlang::execute(&compiled, &mut lashlang::State::new(), &traced)
        .await
        .expect("aggregate await executes");
    let graph = store.graphs().into_iter().next().expect("aggregate graph");
    assert!(
        graph.conflicts.is_empty(),
        "one await occurrence must have one wait and resume"
    );
    assert!(graph.history.iter().any(|item| matches!(
        &item.event.payload,
        TraceLanguageExecutionPayload::NodeWaiting {
            awaited: TraceNodeAwaited::ChildProcesses { process_ids },
            ..
        } if process_ids == &vec![
            lash_core::ProcessId::from("child-1"),
            lash_core::ProcessId::from("child-2"),
        ]
    )));
}

/// The public trace host observes cancellation through its wrapped host. A
/// child await that is parked when the host cancels resolves as cancelled and
/// its occurrence ends `Cancelled` instead of reporting a completion; the
/// start that completed before it stays `Completed`.
#[tokio::test(flavor = "current_thread")]
async fn public_trace_host_reports_a_parked_await_cancelled_after_partial_completion() {
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Default)]
    struct CancellingHost(AtomicBool);

    impl lashlang::ExecutionHost for CancellingHost {
        async fn perform(
            &self,
            op: lashlang::AbilityOp,
        ) -> Result<lashlang::AbilityResult, lashlang::ExecutionHostError> {
            match op {
                lashlang::AbilityOp::ResourceOperation(operation)
                    if operation.operation == "start" =>
                {
                    let mut record = lashlang::Record::default();
                    record.insert(
                        lash_sansio::handle::HANDLE_FIELD.to_string(),
                        lashlang::Value::String(lash_sansio::handle::HANDLE_KIND.into()),
                    );
                    record.insert(
                        "id".to_string(),
                        lashlang::Value::String(
                            lash_sansio::handle::HandleId::process("child", 1)
                                .as_str()
                                .into(),
                        ),
                    );
                    Ok(lashlang::AbilityResult::Value(lashlang::Value::Record(
                        Arc::new(record),
                    )))
                }
                lashlang::AbilityOp::Await(_) => {
                    self.0.store(true, Ordering::SeqCst);
                    Err(lashlang::ExecutionHostError::new("cancelled while parked"))
                }
                lashlang::AbilityOp::Finish(value) | lashlang::AbilityOp::Fail(value) => {
                    Ok(lashlang::AbilityResult::Value(value))
                }
                _ => Err(lashlang::ExecutionHostError::new(
                    "unexpected cancelling-host operation",
                )),
            }
        }

        fn is_cancelled(&self) -> bool {
            self.0.load(Ordering::SeqCst)
        }
    }

    let program = b::module(
        vec![b::process(
            "echo",
            Vec::new(),
            b::block(vec![b::finish(b::null())]),
        )],
        vec![
            b::assign("child", b::start("echo", Vec::new())),
            b::assign("value", b::await_expr(b::var("child"))),
            b::finish(b::var("value")),
        ],
    );
    let store = Arc::new(TraceLashlangGraphStore::default());
    let identity = TraceLanguageExecutionIdentity {
        scope: lash_trace::TraceRuntimeScope::none(),
        subject: lash_trace::TraceRuntimeSubject::Process {
            process_id: lash_core::ProcessId::from("parent"),
        },
        source_identity: "source".to_string(),
        module_ref: "module".to_string(),
        entry_kind: "main".to_string(),
        entry_ref: None,
        entry_name: "main".to_string(),
        restate_invocation_id: None,
        generation: None,
    };
    let payloads = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = Arc::clone(&payloads);
    let observer_store = Arc::clone(&store);
    let traced = LanguageTraceHost::new(
        CancellingHost::default(),
        move |_: &CancellingHost, payload: TraceLanguageExecutionPayload| {
            observed.lock().expect("payload log").push(payload.clone());
            lash_trace::TraceSink::append(
                &*observer_store,
                &lash_trace::TraceRecord::new(
                    lash_trace::TraceContext::default(),
                    lash_trace::TraceEvent::LanguageExecution {
                        language: "lashlang".to_string(),
                        event: TraceLanguageExecution {
                            event_key: "public-cancel".to_string(),
                            identity: identity.clone(),
                            payload,
                        },
                    },
                ),
            )
            .expect("trace append");
        },
    );
    let compiled = lashlang::testing::harness::compile_labeled_program(program);
    let _ = lashlang::execute(&compiled, &mut lashlang::State::new(), &traced).await;
    let payloads = payloads.lock().expect("payload log").clone();
    assert!(
        payloads.iter().any(|payload| matches!(
            payload,
            TraceLanguageExecutionPayload::NodeResumed {
                resolution: lash_trace::TraceNodeWaitResolution::Cancelled,
                ..
            }
        )),
        "the parked await must resolve as cancelled: {payloads:#?}"
    );
    let cancelled = payloads
        .iter()
        .filter_map(|payload| match payload {
            TraceLanguageExecutionPayload::NodeCancelled { node_id, .. } => Some(node_id.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(cancelled.len(), 1, "{payloads:#?}");
    assert!(
        !payloads.iter().any(|payload| matches!(
            payload,
            TraceLanguageExecutionPayload::NodeFailed { node_id, .. } if node_id == &cancelled[0]
        )),
        "a cancelled occurrence must not also report a failure: {payloads:#?}"
    );
    let graph = store.graphs().into_iter().next().expect("cancelled graph");
    assert!(graph.conflicts.is_empty(), "{:?}", graph.conflicts);
    let observation = |id: &str| {
        &graph
            .nodes
            .iter()
            .find(|node| node.id == id)
            .expect("observed node")
            .observation
    };
    assert!(matches!(
        observation(&cancelled[0]),
        TraceLashlangNodeObservation::Cancelled { .. }
    ));
    assert!(
        graph.nodes.iter().any(|node| matches!(
            node.observation,
            TraceLashlangNodeObservation::Completed { .. }
        )),
        "the start before the await completed: {:#?}",
        graph.nodes
    );
}

/// A branch inside a loop takes `then` in iteration 1 and `else` in
/// iteration 2. The run is real VM execution; the static map is the module
/// artifact's own. Each iteration's untaken arm folds to `Skipped` from the
/// observed `BranchSelected` alone.
#[tokio::test(flavor = "current_thread")]
async fn real_loop_branch_skips_the_untaken_arm_in_each_iteration() {
    #[derive(Default)]
    struct PrintHost;

    impl lashlang::ExecutionHost for PrintHost {
        async fn perform(
            &self,
            op: lashlang::AbilityOp,
        ) -> Result<lashlang::AbilityResult, lashlang::ExecutionHostError> {
            match op {
                lashlang::AbilityOp::Print(_) => Ok(lashlang::AbilityResult::Unit),
                lashlang::AbilityOp::Finish(value) | lashlang::AbilityOp::Fail(value) => {
                    Ok(lashlang::AbilityResult::Value(value))
                }
                _ => Err(lashlang::ExecutionHostError::new(
                    "unexpected print-host operation",
                )),
            }
        }
    }

    let program = b::program(vec![
        b::for_in(
            "flag",
            b::list(vec![b::bool_lit(true), b::bool_lit(false)]),
            b::block(vec![b::if_else(
                b::var("flag"),
                b::block(vec![b::labelled(
                    b::label("Then print", None),
                    b::print(b::num(1.0)),
                )]),
                b::block(vec![b::labelled(
                    b::label("Else print", None),
                    b::print(b::num(0.0)),
                )]),
            )]),
        ),
        b::finish(b::null()),
    ]);
    let source = r#"
        for flag in [true, false] {
          if flag {
            @label(title: "Then print")
            print 1
          } else {
            @label(title: "Else print")
            print 0
          }
        }
        finish null
    "#;
    let environment = LashlangHostEnvironment::new(
        lashlang::LashlangHostCatalog::new(),
        LashlangAbilities::all(),
    )
    .with_language_features(lashlang::LashlangLanguageFeatures::default().with_label_annotations());
    let output = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source,
        program: program.clone(),
        environment: &environment,
    })
    .expect("loop branch compiles");
    let execution_map = trace_lashlang_main_map(&output.artifact);
    let arm = |title: &str| {
        execution_map
            .nodes
            .iter()
            .find(|node| node.label == title)
            .map(|node| node.id.clone())
            .unwrap_or_else(|| panic!("`{title}` is mapped: {execution_map:#?}"))
    };
    let (then_arm, else_arm) = (arm("Then print"), arm("Else print"));

    let identity = TraceLanguageExecutionIdentity {
        scope: lash_trace::TraceRuntimeScope::none(),
        subject: lash_trace::TraceRuntimeSubject::Process {
            process_id: lash_core::ProcessId::from("loop-branch"),
        },
        source_identity: trace_lashlang_source_identity(&output.artifact),
        module_ref: output.module_ref.to_string(),
        entry_kind: "main".to_string(),
        entry_ref: None,
        entry_name: "main".to_string(),
        restate_invocation_id: None,
        generation: None,
    };
    let record = |payload: TraceLanguageExecutionPayload| {
        lash_trace::TraceRecord::new(
            lash_trace::TraceContext::default(),
            lash_trace::TraceEvent::LanguageExecution {
                language: "lashlang".to_string(),
                event: TraceLanguageExecution {
                    event_key: "loop-branch".to_string(),
                    identity: identity.clone(),
                    payload,
                },
            },
        )
    };
    let records = Arc::new(std::sync::Mutex::new(vec![record(
        TraceLanguageExecutionPayload::ExecutionStarted {
            execution_map: execution_map.clone(),
        },
    )]));
    let observed = Arc::clone(&records);
    let observed_record = record;
    let traced = LanguageTraceHost::new(PrintHost, move |_: &PrintHost, payload| {
        observed
            .lock()
            .expect("record log")
            .push(observed_record(payload));
    });
    let compiled = lashlang::testing::harness::compile_labeled_program(program);
    lashlang::execute(&compiled, &mut lashlang::State::new(), &traced)
        .await
        .expect("loop branch executes");
    let records = records.lock().expect("record log").clone();

    let is_selection = |record: &lash_trace::TraceRecord| {
        matches!(
            &record.event,
            lash_trace::TraceEvent::LanguageExecution { event, .. }
                if matches!(event.payload, TraceLanguageExecutionPayload::BranchSelected { .. })
        )
    };
    let second_selection = records
        .iter()
        .enumerate()
        .filter(|(_, record)| is_selection(record))
        .map(|(index, _)| index)
        .nth(1)
        .expect("the branch is selected in both iterations");
    let fold = |records: &[lash_trace::TraceRecord]| {
        let store = TraceLashlangGraphStore::default();
        for record in records {
            lash_trace::TraceSink::append(&store, record).expect("fold loop branch");
        }
        store
            .graphs()
            .into_iter()
            .next()
            .expect("loop branch graph")
    };
    let observation = |graph: &lash_trace::TraceLashlangGraph, id: &str| {
        graph
            .nodes
            .iter()
            .find(|node| node.id == id)
            .map(|node| node.observation.clone())
            .unwrap_or_else(|| panic!("`{id}` in graph: {:#?}", graph.nodes))
    };

    let first = fold(&records[..second_selection]);
    assert!(
        matches!(
            observation(&first, &then_arm),
            TraceLashlangNodeObservation::Completed { .. }
        ),
        "{:#?}",
        first.nodes
    );
    assert!(
        matches!(
            observation(&first, &else_arm),
            TraceLashlangNodeObservation::Skipped {
                branch_occurrence: 1,
                ..
            }
        ),
        "{:#?}",
        first.nodes
    );

    let last = fold(&records);
    assert!(last.conflicts.is_empty(), "{:?}", last.conflicts);
    assert!(
        matches!(
            observation(&last, &then_arm),
            TraceLashlangNodeObservation::Skipped {
                branch_occurrence: 2,
                ..
            }
        ),
        "{:#?}",
        last.nodes
    );
    assert!(
        matches!(
            observation(&last, &else_arm),
            TraceLashlangNodeObservation::Completed { .. }
        ),
        "{:#?}",
        last.nodes
    );
}
