//! FIG-4731 at the facade: fork at any retained turn, and pin by input, turn
//! or revision at any time. Every law runs on lash-restate's engine over the
//! Restate server double, whose shift admits the next run as soon as one
//! commits.

use super::*;

use lash_core::Target;

/// What a session's head publishes: the state a fork of it must read back.
/// A fork copies the revision's history, checkpoint and recorded config in
/// full, at its own first config revision. The one exception is the plugin
/// admission the checkpoint records: it belongs to the session that admitted
/// it, so a fork's checkpoint omits it and the fork records its own
/// (FIG-4913).
#[derive(Clone, Debug, PartialEq)]
struct Published {
    leaf: Option<lash_core::NodeId>,
    checkpoint: Option<serde_json::Value>,
    config: lash_core::PersistedSessionConfig,
}

async fn published(fixture: &Fixture, session: &str) -> Result<Published> {
    published_by(&fixture.core, session).await
}

async fn published_by(core: &LashCore, session: &str) -> Result<Published> {
    let head = lash_core::SessionCommitStore::load_session_head_meta(
        core.store_factory.as_ref(),
        &SessionId::fixture(session),
    )
    .await?
    .expect("the session has a head");
    let checkpoint = lash_core::store::SessionHistoryStore::load_session_window(
        core.store_factory.as_ref(),
        &SessionId::fixture(session),
        lash_core::store::WindowSelector::Current,
    )
    .await?
    .expect("the session has a window")
    .checkpoint
    .map(|mut checkpoint| {
        checkpoint
            .components
            .remove(lash_core::store::PLUGIN_ADMISSION_CHECKPOINT_COMPONENT);
        serde_json::to_value(checkpoint).expect("a checkpoint serializes")
    });
    Ok(Published {
        leaf: head.leaf_node_id,
        checkpoint,
        config: lash_core::PersistedSessionConfig {
            config_revision: 0,
            ..head.config
        },
    })
}

/// Fork `target` of `source` into `branch`, declaring the lineage and leaving
/// the source node to the forked revision.
async fn fork(
    fixture: &Fixture,
    source: &str,
    target: Target,
    branch: &str,
) -> Result<lash_core::ForkSessionReceipt> {
    fork_by(&fixture.core, source, target, branch).await
}

async fn fork_by(
    core: &LashCore,
    source: &str,
    target: Target,
    branch: &str,
) -> Result<lash_core::ForkSessionReceipt> {
    core.fork_at(
        &SessionId::fixture(source),
        target,
        crate::ForkRequest {
            session_id: branch.parse().unwrap(),
            relation: lash_core::SessionRelation::Fork {
                source_session_id: source.parse().unwrap(),
                source_node_id: None,
            },
            observed_processes: Vec::new(),
        },
    )
    .await
}

/// A fork of `target` reads back exactly `turn`.
async fn assert_fork_is(
    fixture: &Fixture,
    source: &str,
    target: Target,
    branch: &str,
    turn: &Published,
) -> Result<()> {
    fork(fixture, source, target.clone(), branch).await?;
    assert_eq!(
        &published(fixture, branch).await?,
        turn,
        "a fork of {target} is the state its turn committed"
    );
    Ok(())
}

/// The host collection: the one place the default policy releases.
async fn collect(fixture: &Fixture) {
    fixture
        .core
        .store_factory
        .gc_unreachable()
        .await
        .expect("the host collection completes");
}

/// The refused fork created no session.
async fn assert_no_session(fixture: &Fixture, branch: &str) -> Result<()> {
    assert!(
        matches!(
            fixture
                .core
                .store_factory
                .lookup_session(&SessionId::fixture(branch))
                .await?,
            lash_core::store::SessionLookup::Absent
        ),
        "a refused fork creates no session"
    );
    Ok(())
}

/// Law 1 (a): `send(..).pin()` pins the input in the transaction that accepts
/// it, before its run can start. The turn survives the next turn and a host
/// collection, and a fork of the input is that turn's committed state.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_pinned_at_acceptance_keeps_its_turn_through_collection() -> Result<()> {
    let fixture = fixture(1).await?;
    let session = fixture
        .core
        .session(crate::SessionId::parse("pin-at-send").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    session.send(TurnInput::text("one")).output().await?;
    let first = published(&fixture, "pin-at-send").await?;

    let second = session.send(TurnInput::text("two")).pin().await?;
    let input = second.input_id().clone();
    second.output().await?;
    let second = published(&fixture, "pin-at-send").await?;
    session.send(TurnInput::text("three")).output().await?;

    // No pin names the first turn: it forks until the host collects.
    let first_revision = session
        .revisions()
        .await?
        .into_iter()
        .rev()
        .find(|revision| revision.leaf_node_id == first.leaf)
        .expect("the first turn is retained before any collection")
        .head_revision;
    assert_fork_is(
        &fixture,
        "pin-at-send",
        Target::Revision(first_revision),
        "pin-at-send-first",
        &first,
    )
    .await?;

    collect(&fixture).await;
    let refused = fork(
        &fixture,
        "pin-at-send",
        Target::Revision(first_revision),
        "pin-at-send-collected",
    )
    .await
    .expect_err("the collection released the unpinned turn");
    assert!(
        matches!(
            &refused,
            EmbedError::Store(lash_core::StoreError::ForkTargetPruned { target, .. })
                if *target == Target::Revision(first_revision)
        ),
        "{refused:?}"
    );
    assert_no_session(&fixture, "pin-at-send-collected").await?;
    assert_fork_is(
        &fixture,
        "pin-at-send",
        Target::Input(input.clone()),
        "pin-at-send-second",
        &second,
    )
    .await?;

    session.unpin(Target::Input(input.clone())).await?;
    collect(&fixture).await;
    let released = fork(
        &fixture,
        "pin-at-send",
        Target::Input(input.clone()),
        "pin-at-send-released",
    )
    .await
    .expect_err("releasing the pin lets the collection take the turn");
    assert!(
        matches!(
            &released,
            EmbedError::Store(lash_core::StoreError::ForkTargetPruned { target, .. })
                if *target == Target::Input(input.clone())
        ),
        "{released:?}"
    );
    Ok(())
}

/// Law 1 (b) and law 5: a host pins the running turn out of band through
/// `current_turn()`. While the turn runs a fork of it refuses `Pending` and
/// creates nothing; once it commits, the pin holds it through the next turn
/// and a collection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn current_turn_pins_the_running_turn_out_of_band() -> Result<()> {
    let fixture = fixture(1).await?;
    let session = fixture
        .core
        .session(crate::SessionId::parse("pin-running").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    assert_eq!(session.current_turn().await?, None, "nothing runs yet");
    session.send(TurnInput::text("one")).output().await?;
    let before = published(&fixture, "pin-running").await?;

    let running = session
        .send(TurnInput::text(HELD))
        .id(crate::TurnId::parse("held-run").expect("nonblank host identity"))
        .await?;
    provider_called(&fixture, 2).await;
    let turn = session
        .current_turn()
        .await?
        .expect("the held turn is the unfinished run");
    assert_eq!(turn, lash_core::TurnId::from("held-run"));
    assert_eq!(running.run().await?, Some(turn.clone()));
    let target = Target::Turn(turn);
    session.pin(target.clone()).await?;
    session.pin(target.clone()).await?;

    let pending = fork(&fixture, "pin-running", target.clone(), "pin-running-early")
        .await
        .expect_err("a turn that has not finished names no state");
    assert!(
        matches!(
            &pending,
            EmbedError::Store(lash_core::StoreError::ForkTargetPending { target: refused, .. })
                if *refused == target
        ),
        "{pending:?}"
    );
    assert_no_session(&fixture, "pin-running-early").await?;
    assert_eq!(
        published(&fixture, "pin-running").await?,
        before,
        "the refusal never forks the head in the target's place"
    );

    fixture.release.notify_one();
    running.output().await?;
    let held = published(&fixture, "pin-running").await?;
    session.send(TurnInput::text("three")).output().await?;
    assert_eq!(session.current_turn().await?, None);

    collect(&fixture).await;
    let retained = session.revisions().await?;
    assert_eq!(
        retained
            .iter()
            .map(|revision| (revision.head, revision.pinned_by.clone()))
            .collect::<Vec<_>>(),
        vec![(false, vec![target.clone()]), (true, Vec::new())],
        "the collection keeps the pinned turn and the head; a pin written twice is one pin"
    );
    assert_fork_is(&fixture, "pin-running", target, "pin-running-held", &held).await
}

/// Law 2: an input a merging drain answers inside another input's run pins,
/// and forks, the run that applied it. `SendHandle::run()` names that run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_merged_inputs_pin_resolves_to_the_run_that_applied_it() -> Result<()> {
    let fixture = composing_fixture().await?;
    let session = fixture
        .core
        .session(crate::SessionId::parse("pin-merged").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;

    let running = session
        .send(TurnInput::text(HELD))
        .id(crate::TurnId::parse("held-run").expect("nonblank host identity"))
        .await?;
    provider_called(&fixture, 1).await;
    let second = session
        .send(TurnInput::text("second"))
        .id(crate::TurnId::parse("second-run").expect("nonblank host identity"))
        .await?;
    let third = session
        .send(TurnInput::text("third"))
        .id(crate::TurnId::parse("third-run").expect("nonblank host identity"))
        .await?;
    assert_eq!(third.run().await?, None, "no run has taken the input yet");
    third.pin().await?;
    third.pin().await?;
    let third_input = third.input_id().clone();
    fixture.release.notify_one();
    running.output().await?;
    second.output().await?;
    assert_eq!(
        session.attach(third_input.clone()).run().await?,
        Some(lash_core::TurnId::from("second-run")),
        "the merged input is bound to the run that applied it"
    );
    let merged = published(&fixture, "pin-merged").await?;
    session.send(TurnInput::text("after")).output().await?;

    collect(&fixture).await;
    let by_input = fork(
        &fixture,
        "pin-merged",
        Target::Input(third_input),
        "pin-merged-by-input",
    )
    .await?;
    assert_eq!(published(&fixture, "pin-merged-by-input").await?, merged);
    let by_turn = fixture
        .core
        .store_factory
        .resolve_target(
            &SessionId::from("pin-merged"),
            &Target::Turn(lash_core::TurnId::from("second-run")),
        )
        .await?;
    assert_eq!(
        by_input.head_revision, by_turn.head_revision,
        "the merged input and the applying run name one revision"
    );
    Ok(())
}

/// D4: a session that never ran a turn forks at its creation revision, and
/// one that took a config command before any turn forks at the revision the
/// command published. Each fork copies that revision's recorded configuration
/// and records its lineage.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_empty_session_forks_before_and_after_a_config_command() -> Result<()> {
    let fixture = fixture(1).await?;
    let session = fixture
        .core
        .session(crate::SessionId::parse("fork-empty").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let created = published(&fixture, "fork-empty").await?;
    assert_eq!(created.leaf, None, "nothing has run");
    assert_eq!(
        session
            .revisions()
            .await?
            .iter()
            .map(|revision| (revision.head_revision, revision.head))
            .collect::<Vec<_>>(),
        vec![(0, true)],
        "creation recorded the head as revision 0"
    );
    assert_fork_is(
        &fixture,
        "fork-empty",
        Target::Revision(0),
        "fork-empty-created",
        &created,
    )
    .await?;
    assert_eq!(
        fixture
            .core
            .store_factory
            .load_session_meta(&SessionId::from("fork-empty-created"))
            .await?
            .expect("the fork has metadata")
            .relation,
        lash_core::SessionRelation::Fork {
            source_session_id: "fork-empty".into(),
            source_node_id: None,
        },
        "the fork records its lineage; an empty session has no node to name"
    );

    session
        .admin()
        .config()
        .configure(crate::config::ConfigTransaction::of(
            crate::config::SetAttachmentAcceptance {
                acceptance: lash_core::AttachmentCapabilitySnapshot {
                    revision: "configured-before-any-turn".to_string(),
                    acceptors: Vec::new(),
                },
            },
        ))
        .await?;
    let configured = published(&fixture, "fork-empty").await?;
    assert_ne!(
        configured.config.attachment_acceptance, created.config.attachment_acceptance,
        "the config command changed what the session records"
    );
    let head = session
        .revisions()
        .await?
        .pop()
        .expect("the config command published a head");
    assert!(head.head && head.head_revision > 0);
    assert_fork_is(
        &fixture,
        "fork-empty",
        Target::Revision(head.head_revision),
        "fork-empty-configured",
        &configured,
    )
    .await?;
    // The creation revision is still its own state, not the head's.
    assert_fork_is(
        &fixture,
        "fork-empty",
        Target::Revision(0),
        "fork-empty-created-again",
        &created,
    )
    .await?;

    // Both forks are ordinary sessions: each runs its own first turn.
    let branch = fixture
        .core
        .session(crate::SessionId::parse("fork-empty-configured").expect("nonblank host identity"))
        .open()
        .await?;
    assert_eq!(
        branch
            .send(TurnInput::text("hello"))
            .output()
            .await?
            .assistant_message(),
        Some("echo: hello")
    );
    Ok(())
}

/// Law 5: a withdrawn input refuses `Unavailable`, an accepted input no run
/// has taken refuses `Pending`, and neither forks the head in its place.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_withdrawn_or_waiting_input_refuses_typed() -> Result<()> {
    let fixture = fixture(1).await?;
    let session = fixture
        .core
        .session(crate::SessionId::parse("fork-refused").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let running = session
        .send(TurnInput::text(HELD))
        .id(crate::TurnId::parse("held-run").expect("nonblank host identity"))
        .await?;
    provider_called(&fixture, 1).await;
    let waiting = session
        .send(TurnInput::text("waits"))
        .id(crate::TurnId::parse("waiting-run").expect("nonblank host identity"))
        .pin()
        .await?;
    let waiting_target = Target::Input(waiting.input_id().clone());
    let withdrawn = session
        .send(TurnInput::text("withdraw me"))
        .id(crate::TurnId::parse("withdrawn-run").expect("nonblank host identity"))
        .pin()
        .await?;
    let withdrawn_target = Target::Input(withdrawn.input_id().clone());
    assert!(matches!(
        withdrawn.cancel().await?,
        crate::CancelReceipt::Withdrawn(_)
    ));

    let pending = fork(
        &fixture,
        "fork-refused",
        waiting_target.clone(),
        "fork-refused-waiting",
    )
    .await
    .expect_err("no run has taken the waiting input");
    assert!(
        matches!(
            &pending,
            EmbedError::Store(lash_core::StoreError::ForkTargetPending { target, .. })
                if *target == waiting_target
        ),
        "{pending:?}"
    );
    let unavailable = fork(
        &fixture,
        "fork-refused",
        withdrawn_target.clone(),
        "fork-refused-withdrawn",
    )
    .await
    .expect_err("a withdrawn input names no state");
    assert!(
        matches!(
            &unavailable,
            EmbedError::Store(lash_core::StoreError::ForkTargetUnavailable { target, .. })
                if *target == withdrawn_target
        ),
        "{unavailable:?}"
    );
    for branch in ["fork-refused-waiting", "fork-refused-withdrawn"] {
        assert_no_session(&fixture, branch).await?;
    }

    fixture.release.notify_one();
    running.output().await?;
    waiting.output().await?;
    let waited = published(&fixture, "fork-refused").await?;
    assert_fork_is(
        &fixture,
        "fork-refused",
        waiting_target,
        "fork-refused-answered",
        &waited,
    )
    .await
}

/// Law 1 (a) and (b) on any backend, and the refusal of law 5: a held turn is
/// pinned through its input at acceptance and, out of band, through
/// `current_turn()`. It refuses `Pending` while it runs; once it commits, the
/// next turn and a host collection leave it forkable, and both pins name the
/// one revision it published. The store may outlive a run, so every session
/// is named under `prefix`. Answers the core, which serves the session's
/// shift until the caller's deployment has finished.
async fn a_held_turn_pinned_both_ways_forks_after_collection(
    backend: lash_core::Backend,
    prefix: &str,
) -> Result<LashCore> {
    let release = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let core = LashCore::standard_builder(backend)
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
        .serve_test_llm_profile(
            scripted_provider(Arc::clone(&release), Arc::clone(&calls)),
            mock_llm_profile_spec(),
        )
        .build(crate::testing::runtime_lease_owner())?;
    let source = format!("{prefix}-source");
    let session = core
        .session(SessionId::fixture(source.clone()))
        .created()
        .await
        .open()
        .await?;
    session.send(TurnInput::text("one")).output().await?;

    let held_run = format!("{prefix}-held");
    let running = session
        .send(TurnInput::text(HELD))
        .id(lash_core::TurnId::fixture(held_run.as_str()))
        .pin()
        .await?;
    reaches(&calls, 2, "the held turn's model call starts").await;
    let turn = session
        .current_turn()
        .await?
        .expect("the held turn is the unfinished run");
    assert_eq!(turn, lash_core::TurnId::fixture(held_run.as_str()));
    let by_turn = Target::Turn(turn);
    let by_input = Target::Input(running.input_id().clone());
    session.pin(by_turn.clone()).await?;

    let early = format!("{prefix}-early");
    let pending = fork_by(&core, &source, by_turn.clone(), &early)
        .await
        .expect_err("a turn that has not finished names no state");
    assert!(
        matches!(
            &pending,
            EmbedError::Store(lash_core::StoreError::ForkTargetPending { target, .. })
                if *target == by_turn
        ),
        "{pending:?}"
    );
    assert!(matches!(
        core.store_factory
            .lookup_session(&SessionId::fixture(early.as_str()))
            .await?,
        lash_core::store::SessionLookup::Absent
    ));

    release.notify_one();
    running.output().await?;
    let held = published_by(&core, &source).await?;
    session.send(TurnInput::text("three")).output().await?;

    core.store_factory
        .gc_unreachable()
        .await
        .expect("the host collection completes");
    let retained = session.revisions().await?;
    let [kept, head] = retained.as_slice() else {
        panic!("the collection keeps the pinned turn and the head: {retained:?}");
    };
    assert!(head.head && head.pinned_by.is_empty());
    assert!(
        !kept.head
            && kept.pinned_by.len() == 2
            && kept.pinned_by.contains(&by_input)
            && kept.pinned_by.contains(&by_turn),
        "both pins name the revision the held turn published: {kept:?}"
    );

    let by_input_branch = format!("{prefix}-by-input");
    let forked = fork_by(&core, &source, by_input, &by_input_branch).await?;
    assert_eq!(forked.head_revision, kept.head_revision);
    assert_eq!(published_by(&core, &by_input_branch).await?, held);
    let by_turn_branch = format!("{prefix}-by-turn");
    let forked = fork_by(&core, &source, by_turn, &by_turn_branch).await?;
    assert_eq!(forked.head_revision, kept.head_revision);
    assert_eq!(published_by(&core, &by_turn_branch).await?, held);
    Ok(core)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_turn_pinned_both_ways_forks_after_collection_on_sqlite() -> Result<()> {
    let double = restate_double(SEED).await;
    a_held_turn_pinned_both_ways_forks_after_collection(double.lash_backend(), "pin-both").await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
async fn a_held_turn_pinned_both_ways_forks_after_collection_on_postgres() -> Result<()> {
    let Some((stores, _held)) = postgres_store_set().await else {
        return Ok(());
    };
    let double = lash_restate_test::backend_with(
        SEED,
        lash_restate_test::ServerConfig::default(),
        move |_| stores,
    )
    .await
    .expect("build the deployment over PostgreSQL");
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("wall clock after the epoch")
        .as_nanos();
    a_held_turn_pinned_both_ways_forks_after_collection(
        double.lash_backend(),
        &format!("pin-both-{nonce}"),
    )
    .await?;
    Ok(())
}

/// The law on a live `restate-server` (the `recorded-runs` suite of
/// `scripts/restate-suites.toml`). The server's state outlives a run, so each
/// run names its own sessions.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated Restate server; run by the recorded-runs suite"]
#[allow(
    clippy::disallowed_methods,
    reason = "the live law reads the suite's server and endpoint addresses"
)]
async fn live_a_held_turn_pinned_both_ways_forks_after_collection() -> Result<()> {
    let env = |name: &str| {
        std::env::var(name).unwrap_or_else(|_| panic!("the live suite's environment sets {name}"))
    };
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("wall clock after the epoch")
        .as_nanos();
    let prefix = format!("pin-both-{nonce}");
    let live =
        lash_restate_test::live::LiveRestateBackend::start(lash_restate_test::live::LiveConfig {
            ingress_url: env("RESTATE_INGRESS_URL"),
            admin_url: env("RESTATE_ADMIN_URL"),
            endpoint_bind: env("RR_BIND").parse().expect("a socket address"),
            endpoint_url: env("RR_URL"),
            run_tag: prefix.clone(),
            namespace: lash_restate::RestateNamespace::default(),
        })
        .await
        .expect("serve the live deployment");
    // The core outlives the census: the session's shift finishes on it.
    let result =
        a_held_turn_pinned_both_ways_forks_after_collection(live.lash_backend(), &prefix).await;
    live.finish().await;
    result.map(drop)
}
