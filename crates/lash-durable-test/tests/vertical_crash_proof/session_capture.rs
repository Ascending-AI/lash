//! The session-capture law (FIG-6418 L3): the commit that publishes the
//! session's execution root is a candidate, and a lost acknowledgement of
//! it resolves against the row its owner write names, as a cell's resolves
//! against its execution row.

use super::*;

/// What the session-capture law saw of one store write or one candidate,
/// in order (FIG-6418 L3).
#[derive(Clone, Debug)]
enum Observed {
    /// An owner commit: the working-state root it publishes and whether it
    /// ends [`run`]'s turn.
    Commit {
        label: CommitLabel,
        /// Its position among the run's writes under `label`, from 1.
        nth: usize,
        working: Option<lash_core_store::checkpoint_graph::NodeId>,
        terminal: bool,
    },
    /// A candidate publication under `label`: its manifest's root.
    Candidate {
        label: CommitLabel,
        root: lash_core_store::checkpoint_graph::NodeId,
    },
}

impl Observed {
    fn commit(label: CommitLabel, tx: &lash_durable::ActorTx, nth: usize) -> Self {
        let mut working = None;
        let mut terminal = false;
        for write in tx.domain() {
            match write {
                lash_durable::DomainWrite::Turn(lash_durable::domain::TurnWrite::Working {
                    session: wrote,
                    working: staged,
                    ..
                }) if *wrote == session() => working = *staged,
                lash_durable::DomainWrite::Turn(lash_durable::domain::TurnWrite::Terminal {
                    run: ended,
                    ..
                }) if *ended == run() => terminal = true,
                _ => {}
            }
        }
        Self::Commit {
            label,
            nth,
            working,
            terminal,
        }
    }
}

/// One run of the session-capture law's world (FIG-6418 L3): a session
/// whose host binds [`HOST_DEFAULT`] through its command lane, a turn
/// binding it and a second turn's cell reading it, with every owner commit
/// and candidate publication observed in order, under `cut`'s armed rule.
async fn session_capture_world(cut: Option<(CommitLabel, usize)>) -> (V0, Vec<Observed>, SimNodes) {
    let scenario = V0::new(Protocol::Defaults, Dialect::SqliteMemory, None);
    let clock = SimClock::new();
    let database = scenario.database(Arc::clone(&clock)).await;
    let script = Script::new();
    if let Some((label, nth)) = cut {
        script.cut(label, nth, Fault::AckHidden);
    }
    let seen = Arc::new(Mutex::new((
        Vec::new(),
        BTreeMap::<CommitLabel, usize>::new(),
    )));
    script.observe_commits({
        let seen = Arc::clone(&seen);
        Arc::new(move |label, tx| {
            let mut seen = seen.lock_recover();
            let count = seen.1.entry(label).or_default();
            *count += 1;
            let nth = *count;
            seen.0.push(Observed::commit(label, tx, nth));
        })
    });
    script.observe_candidates({
        let seen = Arc::clone(&seen);
        Arc::new(move |label, summary, _| {
            seen.lock_recover().0.push(Observed::Candidate {
                label,
                root: summary.root,
            });
        })
    });
    let nodes = SimNodes::new(
        Arc::clone(&database),
        Arc::clone(&clock),
        script,
        scenario.config(),
        scenario.activation(),
    );

    let core = scenario.core();
    core.session(session())
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            MODEL,
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(64),
            lash_core::NoProgressBudget::bounded(12),
        )))
        .await
        .expect("create the session");
    let live = core
        .session(session())
        .open()
        .await
        .expect("the session opens");
    let pending = live
        .admin()
        .protocol()
        .apply_session_extension(
            lash_core::ProtocolSessionExtension::new(|fleet| {
                lash::rlm::rlm_seed_initial_nodes(
                    lash::rlm::RlmSeed {
                        globals: serde_json::Map::from_iter([(
                            HOST_DEFAULT.to_owned(),
                            serde_json::json!(41),
                        )]),
                        ..Default::default()
                    },
                    fleet,
                )
            }),
            "fig6418-default-global".to_owned(),
        )
        .await
        .expect("the host's default is accepted");
    nodes.start("a");
    pending
        .settle_with(
            &live.admin().commands(),
            lash::testing::admin_fixture_outcome,
        )
        .await
        .expect("the host's default settles");
    nodes.quiesce().await;

    let run_two = TurnId::try_from(RUN_TWO.to_owned()).unwrap();
    for (input, run) in [("bind the default", run()), ("read the default", run_two)] {
        live.send(lash::TurnInput::text(input))
            .id(run.clone())
            .await
            .expect("the turn is sent");
        let horizon = clock.logical_ms() + 600_000;
        while !matches!(
            nodes.database().turn_end(&session(), &run).await,
            Ok(Some(_))
        ) {
            assert!(
                clock.logical_ms() < horizon && nodes.step().await.is_some(),
                "the turn never ended:\n{}",
                nodes.script().rendered_trace()
            );
        }
        nodes.quiesce().await;
    }
    (scenario, seen.lock_recover().0.clone(), nodes)
}

/// FIG-6418 L3: a session capture is published as a candidate; a lost
/// acknowledgement of that commit resolves against the turn's working row,
/// the turn commits once and a later cell reads the host's default.
async fn session_capture_candidate() {
    let (_scenario, seen, _nodes) = session_capture_world(None).await;

    // A candidate's owner commit is observed right after the candidate.
    // Every commit that publishes the session's execution root (a Working
    // write naming it) must pair with a candidate whose root is that root.
    let mut publications = Vec::new();
    for (first, second) in seen.iter().zip(seen.iter().skip(1)) {
        let (
            Observed::Candidate {
                label: candidate_label,
                root: candidate_root,
            },
            Observed::Commit {
                label,
                nth,
                working,
                ..
            },
        ) = (first, second)
        else {
            continue;
        };
        if candidate_label == label && Some(*candidate_root) == *working {
            publications.push((*label, *nth));
        }
    }
    let publishers: Vec<_> = seen
        .iter()
        .filter(|event| {
            matches!(
                event,
                Observed::Commit {
                    working: Some(_),
                    ..
                }
            )
        })
        .collect();
    assert!(
        !publishers.is_empty(),
        "the turn never published a working execution root: {seen:?}"
    );
    assert_eq!(
        publications.len(),
        publishers.len(),
        "not every commit publishing the session's execution root was a candidate: {seen:?}"
    );
    // The first commit that publishes the session's execution root.
    let (label, nth) = publishers
        .iter()
        .find_map(|event| match event {
            Observed::Commit { label, nth, .. } => Some((*label, *nth)),
            _ => None,
        })
        .expect("a publishing commit");

    // The same run with the publication's ack hidden: the commit lands,
    // the caller is answered AckLost, and the resolution reads the turn's
    // working row — the turn still commits exactly once and a later cell
    // reads the default.
    let (scenario, seen, nodes) = session_capture_world(Some((label, nth))).await;
    let terminals: Vec<_> = seen
        .iter()
        .filter(|event| {
            matches!(
                event,
                Observed::Commit {
                    label: CommitLabel::TURN_COMMIT,
                    terminal: true,
                    ..
                }
            )
        })
        .collect();
    assert_eq!(
        terminals.len(),
        1,
        "the turn commits exactly once under a hidden ack: {seen:?}"
    );
    let Observed::Commit {
        label: terminal_label,
        nth: terminal_nth,
        ..
    } = terminals[0]
    else {
        unreachable!()
    };
    assert!(
        nodes
            .script()
            .trace()
            .iter()
            .any(|write| write.point.label == *terminal_label
                && write.point.nth == *terminal_nth
                && write.committed()),
        "the turn's commit committed: {}",
        nodes.script().rendered_trace()
    );
    assert!(
        scenario
            .world
            .writes()
            .values()
            .flatten()
            .any(|args| args.get("x") == Some(&serde_json::json!(41))),
        "the later cell read the host's default"
    );
}

/// The session-capture law on SQLite in memory.
#[tokio::test]
async fn a_session_capture_is_published_as_a_candidate_and_a_lost_ack_resolves_against_its_owner_row_on_sqlite_memory()
 {
    session_capture_candidate().await;
}
