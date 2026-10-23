use super::*;

/// Sharing the history handed to an effect body costs the same number of
/// allocations and bytes at shallow and deep histories (FIG-5962).
/// Preparation and fixture allocation are outside the counted interval.
#[tokio::test]
async fn effect_executor_allocations_are_independent_of_history_depth() {
    let backend = crate::testing::sqlite_recording_backend().await;
    let session_id = lash_vocabulary::identity::SessionId::fixture("sequence-sharing");
    let store =
        crate::testing::runtime_helpers::recording_session_store(&backend, session_id.clone())
            .await;
    let mut runtime = crate::testing::runtime_helpers::TestRuntime::new(
        &backend,
        crate::testing::TestProvider::builder().build(),
    )
    .store(store)
    .with_session_id(session_id)
    .build()
    .await;
    let run = lash_vocabulary::identity::TurnId::from("sequence-sharing");
    let actor = lash_durable::ActorKey::session(runtime.state.session_id.as_str()).unwrap();
    let mut mail = lash_durable::MailTx::new();
    mail.append(
        actor.clone(),
        lash_durable::MailKind::new("law.sequence-sharing"),
        String::new(),
    );
    backend
        .durable()
        .commit_mail(mail, lash_durable::CommitLabel::MAIL_SESSION)
        .await
        .unwrap();
    let node = backend
        .durable()
        .register_node(&lash_durable::NodeSpec {
            node: lash_durable::NodeId::new("sequence-sharing"),
            decodes: backend.formats().decodes(),
            serves: Vec::new(),
            capabilities: Default::default(),
            ttl_millis: 15_000,
        })
        .await
        .unwrap();
    let claimed = backend.durable().claim(&node, 1).await.unwrap();
    let controller = crate::ActorContext::new(
        backend,
        actor,
        claimed[0].epoch,
        crate::AdmittedScope::turn(runtime.state.session_id.clone(), run.clone()),
        tokio_util::sync::CancellationToken::new(),
        std::sync::Arc::new(lash_durable::NoProbe),
    );
    let _claim = controller.session_claim_guard();
    crate::runtime::durable::head::SessionOwner::default()
        .head(&controller, &runtime.state.session_id)
        .await
        .expect("hydrate the claimed session projection");
    let (observer, _observations) = TurnObserver::unread();
    let turn = Box::pin(runtime.prepare_durable_turn(
        &controller,
        &run,
        crate::runtime::logical_turn::LogicalTurnAdmissions::new(Vec::new()),
        crate::runtime::turn_loop::TurnStart::Fresh(None),
        None,
        &observer,
    ))
    .await
    .expect("prepare the fixture's turn");
    let mut driver = turn.driver;
    let mut machine = driver.turn_machine(turn.window, turn.messages);
    // Warm the session projection and executor's fixed setup before either
    // cohort; their first-use work is unrelated to sequence history depth.
    drop(super::super::local_effects::turn_effect_executor(
        &mut driver,
        &machine,
        observer.clone(),
        controller.clone(),
        "sequence-sharing:body",
    ));
    let mut measured = Vec::new();
    for depth in [8, 512] {
        let history: crate::MessageSequence = (0..depth)
            .map(|index| crate::Message {
                id: format!("history-{index}"),
                role: crate::MessageRole::User,
                parts: crate::shared_parts(vec![crate::Part::text(
                    format!("history-{index}.p0"),
                    "retained input".to_string(),
                    None,
                )]),
                origin: Some(crate::MessageOrigin::TurnInput {
                    turn_id: lash_vocabulary::identity::TurnId::from("prior-turn"),
                    input_id: None,
                }),
                reply_marker: None,
            })
            .collect::<Vec<_>>()
            .into();
        // Owned history occurs after a cold restore or a plugin rewrite.
        // Both sequences and the recorded prelude are copied by the actual
        // executor constructor, including for effects that never read them.
        driver.prelude_base_messages = history.clone();
        driver.prepared_context.messages = history.clone();
        machine.adopt_committed_messages(history.clone());
        machine.adopt_prepared_messages(history);
        let measurement = allocation_counter::Measurement::start();
        let executor = super::super::local_effects::turn_effect_executor(
            &mut driver,
            &machine,
            observer.clone(),
            controller.clone(),
            "sequence-sharing:body",
        );
        std::hint::black_box(&executor);
        drop(executor);
        measured.push(measurement.finish());
    }
    eprintln!("effect executor at depths 8 and 512: {measured:?}");
    assert_eq!(
        measured[0].allocations, measured[1].allocations,
        "effect executor allocation count must not grow with retained history"
    );
    assert_eq!(
        measured[0].allocated, measured[1].allocated,
        "effect executor allocated bytes must not grow with retained history"
    );
}

/// FIG-7080: anchoring and recognizing one protocol reply must not copy
/// the assistant ids of every preceding turn.
#[test]
fn protocol_reply_tracking_allocates_independently_of_history_depth() {
    let message = |id: String| crate::Message {
        parts: crate::shared_parts(vec![crate::Part::text(
            format!("{id}.p0"),
            "reply".to_owned(),
            None,
        )]),
        id,
        role: crate::MessageRole::Assistant,
        origin: None,
        reply_marker: None,
    };
    let work = [1, 10, 1000].map(|turns| {
        let history = (0..turns)
            .map(|turn| message(format!("old-{turn:04}")))
            .collect::<Vec<_>>();
        let mut suffix = vec![
            message("current-reply".to_owned()),
            message("old-0000".to_owned()),
        ];
        let mut user = message("after-turn-input".to_owned());
        user.role = crate::MessageRole::User;
        suffix.push(user);
        // The commit draft may already have folded the terminal delta. The
        // boundary's base is a shorter immutable view of that same buffer.
        let mut all = history;
        all.extend(suffix.iter().cloned());
        let graph = crate::SessionGraph::from_active_read_state(&all);
        let mut prefix = graph.read_model().messages;
        prefix.truncate(turns);
        let before = crate::MessageSequence::from_base(prefix);
        let mut after = before.clone();
        after.extend(suffix);
        let measurement = allocation_counter::Measurement::start();
        let mut tracker = super::machine::ProtocolReplyTracker::default();
        tracker.mark_run_start(&before);
        tracker.mark_model_call(&before);
        assert!(tracker.terminal_output(&before, &graph).is_empty());
        let reply = tracker.terminal_output(&after, &graph);
        let counts = measurement.finish();
        assert_eq!(reply, ["current-reply"]);
        // A continuation run keeps the last model call's identity boundary.
        tracker.mark_run_start(&after);
        assert_eq!(tracker.terminal_output(&after, &graph), ["current-reply"]);
        // A new model call excludes that reply from its successor's output.
        tracker.mark_model_call(&after);
        assert!(tracker.terminal_output(&after, &graph).is_empty());
        after.push(message("following-reply".to_owned()));
        assert_eq!(tracker.terminal_output(&after, &graph), ["following-reply"]);
        // Rebuilt/edited histories have no prefix witness: preserve the same
        // id semantics, including an old assistant moved after the reply.
        let mut rewritten = after.iter().cloned().collect::<Vec<_>>();
        rewritten.rotate_left(1);
        let rewritten = crate::MessageSequence::from_owned(rewritten);
        assert_eq!(
            tracker.terminal_output(&rewritten, &graph),
            ["following-reply"]
        );
        counts
    });
    eprintln!("reply tracking at 1, 10 and 1000 turns: {work:?}");
    for counts in &work[1..] {
        assert_eq!(work[0].allocations, counts.allocations);
        assert_eq!(work[0].allocated, counts.allocated);
    }
}

mod allocation_counter {
    //! Allocation requests on the thread constructing an effect executor.
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    #[derive(Clone, Copy, Debug, Default)]
    pub(crate) struct Counts {
        pub(crate) allocated: usize,
        pub(crate) allocations: usize,
    }

    thread_local! {
        static COUNTS: Cell<Option<Counts>> = const { Cell::new(None) };
    }

    fn charge(bytes: usize) {
        let _ = COUNTS.try_with(|cell| {
            if let Some(mut counts) = cell.get() {
                counts.allocated += bytes;
                counts.allocations += 1;
                cell.set(Some(counts));
            }
        });
    }

    struct Counter;

    #[global_allocator]
    static ALLOCATOR: Counter = Counter;

    #[expect(
        unsafe_code,
        reason = "test allocator forwards the unchanged GlobalAlloc contract to System"
    )]
    unsafe impl GlobalAlloc for Counter {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            charge(layout.size());
            // SAFETY: The caller supplies a valid layout; System receives it unchanged.
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            charge(layout.size());
            // SAFETY: The caller supplies a valid layout; System receives it unchanged.
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // SAFETY: Allocations came from System with the same layout.
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            charge(new_size);
            // SAFETY: The pointer, layout and new size keep the caller's contract.
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    pub(crate) struct Measurement;

    impl Measurement {
        pub(crate) fn start() -> Self {
            COUNTS.with(|cell| {
                assert!(cell.get().is_none(), "allocation measurements cannot nest");
                cell.set(Some(Counts::default()));
            });
            Self
        }

        pub(crate) fn finish(self) -> Counts {
            COUNTS.with(|cell| cell.take().expect("active allocation measurement"))
        }
    }

    impl Drop for Measurement {
        fn drop(&mut self) {
            COUNTS.with(|cell| cell.set(None));
        }
    }
}
