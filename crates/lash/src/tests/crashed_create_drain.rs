//! FIG-4492's crash window: a session whose create died between its catalog
//! row and its first head commit, then a drive that drains a queued command
//! (FIG-4553).
//!
//! The session manager's create admits the catalog row and commits the
//! session's first head a few awaits later (`session_init.rs`:
//! `bind_session_store`, then `commit_initialized_session`). A creator that
//! dies in between leaves a session no commit has written. Its admission
//! records the creation's complete config with the row, in the catalog's
//! transaction, so the session the engine opens runs exactly that config:
//! a drive that drains a command queued for it commits a head that records
//! it, and a later deployment opens that head.
//!
//! Each law has the real creator make its admission and never its commit:
//! the create states more initial nodes than the core's commit budget
//! admits, so its first commit is refused after the row is written. The law
//! then queues a host append on the command lane and has the engine drive
//! the session on the Restate server double.
//!
//! A catalog row with no head at all recorded no config. It is never opened
//! with defaults: the host's open and the engine's drive are both refused
//! with the typed creation-unrecorded refusal, and nothing is committed.
//!
//! Over SQLite memory, SQLite file and PostgreSQL. The PostgreSQL legs are
//! ignored outside a PostgreSQL gate, which selects them with
//! `--include-ignored`.

use super::*;

const SEED: u64 = 0x4553_0001;

/// The text the queued append carries.
const NOTE: &str = "a note appended after the crashed create";

/// An ancestor no head of the law's session ever had.
const ABSENT_ANCESTOR: &str = "an-ancestor-the-head-never-had";

/// The node budget of the core the create crashes on: a drained append fits
/// under it, the crashed create's initial nodes do not.
const NODE_BUDGET: usize = 16;

/// How many initial nodes the crashed create states.
const INITIAL_NODES: usize = 4 * NODE_BUDGET;

#[derive(Clone, Copy, Debug)]
enum Storage {
    SqliteMemory,
    SqliteFile,
    Postgres,
}

/// The double over `storage`, with what its stores need to outlive it.
async fn double_over(
    storage: Storage,
) -> Option<(
    lash_restate_test::RestateTestBackend,
    Box<dyn std::any::Any>,
)> {
    let config = lash_restate_test::ServerConfig::default;
    Some(match storage {
        Storage::SqliteMemory => (
            lash_restate_test::backend(SEED, config())
                .await
                .expect("the Restate double over SQLite memory"),
            Box::new(()),
        ),
        Storage::SqliteFile => {
            let files = tempfile::tempdir().expect("a SQLite store directory");
            let stores: Arc<dyn lash_core::StoreSet> = Arc::new(
                lash_sqlite_store::SqliteStoreSet::open(files.path())
                    .await
                    .expect("open the SQLite file stores"),
            );
            (
                lash_restate_test::backend_with(SEED, config(), move |_| stores)
                    .await
                    .expect("the Restate double over SQLite files"),
                Box::new(files),
            )
        }
        Storage::Postgres => {
            let (stores, held) = postgres_store_set().await?;
            (
                lash_restate_test::backend_with(SEED, config(), move |_| stores)
                    .await
                    .expect("the Restate double over PostgreSQL"),
                held,
            )
        }
    })
}

/// A core over `double` whose commits fit `nodes` nodes.
fn core_over(double: &lash_restate_test::RestateTestBackend, nodes: usize) -> Result<LashCore> {
    explicit_ephemeral_facets_with_budget(
        LashCore::standard_builder(double.lash_backend(), crate::TurnBudget::Unbounded),
        crate::CommitBudget::bounded(1024 * 1024, nodes),
    )
    .serve_test_model(mock_provider(), mock_model_spec())
    .build(crate::testing::runtime_lease_owner())
}

/// A session a crashed create left, and the config its creation resolved.
struct CrashedCreate {
    store: lash_core::store::SessionStore,
    /// What a create of the same request that finished recorded.
    creation: lash_core::PersistedSessionConfig,
}

/// The session's head, when any admission or commit wrote one.
async fn head_of(
    store: &lash_core::store::SessionStore,
) -> Result<Option<lash_core::store::SessionHeadMeta>> {
    store
        .load_session_head_meta()
        .await
        .map_err(EmbedError::Store)
}

/// Have the session manager create `session` and die between its admission
/// and its first commit: the create states more initial nodes than `core`'s
/// commit budget admits, so the commit is refused after the catalog row is
/// written. A twin create of the same request without the nodes finishes,
/// and records what the creation resolved.
async fn crashed_create(core: &LashCore, session: &str) -> Result<CrashedCreate> {
    let creator = core
        .session(format!("{session}-creator"))
        .created()
        .await
        .open()
        .await?;
    let lifecycle = {
        let writer = creator.runtime.writer();
        let runtime = writer.lock().await;
        runtime
            .session_lifecycle_service()
            .expect("the creator's session lifecycle service")
    };
    let request = |id: &str| {
        lash_core::SessionCreateRequest::root(
            lash_core::SessionStartPoint::Empty,
            lash_core::PluginOptions::default(),
        )
        .with_session_id(id)
    };
    let twin = format!("{session}-twin");
    lifecycle
        .create_session(request(&twin))
        .await
        .expect("the twin create finishes");
    let crashed = lifecycle
        .create_session(
            request(session).with_initial_nodes(
                (0..INITIAL_NODES)
                    .map(|index| {
                        lash_core::SessionAppendNode::message(lash_core::PluginMessage::text(
                            lash_core::MessageRole::User,
                            format!("initial node {index}"),
                        ))
                    })
                    .collect(),
            ),
        )
        .await
        .expect_err("the create's first commit exceeds the node budget");
    assert!(
        crashed.to_string().contains("budget"),
        "the create died at its first commit: {crashed}"
    );
    drop(lifecycle);
    drop(creator);

    let view = |id: &str| {
        let id = SessionId::from(id);
        async move {
            lash_core::runtime::live_session_view(&core.store_factory, &id)
                .await
                .map_err(EmbedError::Store)
        }
    };
    let store = view(session)
        .await?
        .expect("the crashed create wrote the session's catalog row");
    assert!(
        head_of(&store)
            .await?
            .is_none_or(|head| head.head_revision == 0),
        "the crashed create committed no head"
    );
    let creation = head_of(&view(&twin).await?.expect("the twin's catalog row"))
        .await?
        .expect("the twin create committed its head")
        .config;
    Ok(CrashedCreate { store, creation })
}

/// Queue an append of [`NOTE`] on `session`'s command lane, requiring
/// `ancestor` when given.
async fn queue_an_append(
    store: &lash_core::store::SessionStore,
    session: &str,
    ancestor: Option<&str>,
) -> lash_core::BatchId {
    store
        .enqueue_queued_work(
            lash_core_store::queued_work_vocabulary::QueuedWorkBatchDraft::new(
                SessionId::from(session),
                lash_core::DeliveryPolicy::AfterCurrentTurnCommit,
                lash_core_store::queued_work_vocabulary::SessionCommand::AppendSessionNodes {
                    request: Box::new(lash_core::AppendSessionNodesRequest {
                        operation_id: "host-append:after-crashed-create".to_string(),
                        nodes: vec![lash_core::SessionAppendNode::message(
                            lash_core::PluginMessage::text(lash_core::MessageRole::User, NOTE),
                        )],
                        requires_ancestor_node_id: ancestor.map(|id| id.to_string().into()),
                    }),
                },
            )
            .with_source_key("after-crashed-create".to_string()),
        )
        .await
        .expect("queue the host append")
        .batch_id
}

/// Ask the engine to drive `session`. No host runtime is open: the engine
/// opens the session itself.
async fn schedule_a_drive(core: &LashCore, session: &str) {
    let engine_port = core.substrate_slot.ports().await.queued;
    engine_port.schedule_drive(
        &SessionId::from(session),
        lash_core::engine::DriveRequestId::new("after-crashed-create"),
    );
}

/// The command's settlement and the head it committed.
struct Drained {
    outcome: lash_core_store::queued_work_vocabulary::SessionCommandOutcome,
    /// How many nodes of the committed head carry [`NOTE`].
    notes_in_head: usize,
    /// The config the committed head records.
    recorded: lash_core::PersistedSessionConfig,
}

/// Queue an append of [`NOTE`] on `session`'s command lane, requiring
/// `ancestor` when given, and have the engine drive the session until the
/// lane drains.
async fn drain_an_append(
    core: &LashCore,
    store: &lash_core::store::SessionStore,
    session: &str,
    ancestor: Option<&str>,
) -> Result<Drained> {
    let command = queue_an_append(store, session, ancestor).await;
    schedule_a_drive(core, session).await;
    let drained = tokio::time::timeout(std::time::Duration::from_secs(90), async {
        loop {
            match store.list_open_queued_work().await {
                Ok(open) if open.is_empty() => return,
                Ok(_) | Err(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
            }
        }
    })
    .await;
    assert!(
        drained.is_ok(),
        "the drive drains the queued command: still open {:?}",
        store.list_open_queued_work().await,
    );

    let completion = store
        .queued_work_batch_completion(command.as_str())
        .await
        .expect("read the command's settlement")
        .expect("the drained command wrote its settlement");
    let outcome = completion
        .command_outcomes
        .get(&command)
        .cloned()
        .expect("the settlement carries the command's outcome");
    let head = lash_core::store::load_session_window_state(
        store,
        lash_core::store::WindowSelector::Current,
    )
    .await
    .map_err(EmbedError::Store)?
    .expect("the settlement committed the session's first head");
    assert!(
        head.state.head_revision > 0,
        "the settlement committed the session's first head"
    );
    let notes_in_head = head
        .state
        .session_graph
        .nodes
        .iter()
        .filter(|node| format!("{:?}", node.payload).contains(NOTE))
        .count();
    Ok(Drained {
        outcome,
        notes_in_head,
        recorded: head.config,
    })
}

/// How the drained append settled.
#[derive(Clone, Copy, Debug)]
enum Append {
    /// The store's ancestor check refuses it.
    Refused,
    Accepted,
}

impl Append {
    fn ancestor(self) -> Option<&'static str> {
        match self {
            Self::Refused => Some(ABSENT_ANCESTOR),
            Self::Accepted => None,
        }
    }
}

/// An append the store's ancestor check refuses, drained by the first drive
/// after a crashed create, settles `StaleBranch`. The head its settlement
/// commits holds nothing of it and records the creation's config.
async fn a_refused_append_drained_after_a_crashed_create_leaves_nothing_of_it(
    storage: Storage,
) -> Result<()> {
    const ID: &str = "crashed-create-refused-append";
    let Some((double, _held)) = double_over(storage).await else {
        return Ok(());
    };
    let core = core_over(&double, NODE_BUDGET)?;
    let crashed = crashed_create(&core, ID).await?;
    let drained = drain_an_append(&core, &crashed.store, ID, Append::Refused.ancestor()).await?;
    let lash_core_store::queued_work_vocabulary::SessionCommandOutcome::AppendSessionNodes {
        outcome: lash_core::AppendSessionNodesOutcome::StaleBranch { required_node_id },
    } = &drained.outcome
    else {
        panic!(
            "the ancestor-refused append settles stale: {:?}",
            drained.outcome
        );
    };
    assert_eq!(required_node_id.to_string(), ABSENT_ANCESTOR);
    assert_eq!(
        drained.notes_in_head, 0,
        "nothing of the refused append reached the head"
    );
    assert_eq!(
        drained.recorded, crashed.creation,
        "the drained head records the creation's config"
    );
    Ok(())
}

/// An accepted append drained by the first drive after a crashed create is
/// in the head its settlement commits exactly once, and that head records
/// the creation's config.
async fn an_append_drained_after_a_crashed_create_is_committed_once(
    storage: Storage,
) -> Result<()> {
    const ID: &str = "crashed-create-accepted-append";
    let Some((double, _held)) = double_over(storage).await else {
        return Ok(());
    };
    let core = core_over(&double, NODE_BUDGET)?;
    let crashed = crashed_create(&core, ID).await?;
    let drained = drain_an_append(&core, &crashed.store, ID, Append::Accepted.ancestor()).await?;
    assert!(
        matches!(
            &drained.outcome,
            lash_core_store::queued_work_vocabulary::SessionCommandOutcome::AppendSessionNodes {
                outcome: lash_core::AppendSessionNodesOutcome::Appended { node_ids, .. },
            } if node_ids.len() == 1
        ),
        "the append settles appended: {:?}",
        drained.outcome
    );
    assert_eq!(
        drained.notes_in_head, 1,
        "the accepted append is in the head once"
    );
    assert_eq!(
        drained.recorded, crashed.creation,
        "the drained head records the creation's config"
    );
    Ok(())
}

/// The head a drive commits after a crashed create is one a new deployment
/// opens: after the drain, a deployment restarted over the same stores runs
/// a turn of the session, which the engine opens from that head.
async fn a_session_drained_after_a_crashed_create_reopens_on_a_new_deployment(
    storage: Storage,
    append: Append,
) -> Result<()> {
    let id = format!("crashed-create-reopen-{append:?}").to_lowercase();
    let Some((double, _held)) = double_over(storage).await else {
        return Ok(());
    };
    {
        let core = core_over(&double, NODE_BUDGET)?;
        let crashed = crashed_create(&core, &id).await?;
        drain_an_append(&core, &crashed.store, &id, append.ancestor()).await?;
    }
    let restarted = redeploy(double).await;
    let core = core_over(&restarted, 512)?;
    let turn = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        core.session(id.as_str())
            .durable()
            .await?
            .send(TurnInput::text("answer after the drain"))
            .output()
            .await
    })
    .await;
    let Ok(output) = turn else {
        panic!(
            "the restarted deployment runs a turn of the session: none finished in 60 s, \
             invocations {:?}",
            invocations(&restarted)
        );
    };
    let output = output?;
    assert!(
        matches!(output.result.outcome, TurnOutcome::Finished(_)),
        "the restarted deployment runs a turn of the session: {:?}",
        output.result.outcome
    );
    Ok(())
}

/// A catalog row with no head recorded no config, and nothing opens it with
/// defaults: the host's open is refused with the typed creation-unrecorded
/// error, the engine's drive of a command queued for it is refused with the
/// typed code, and neither commits a head.
async fn a_catalog_row_with_no_head_is_refused_and_never_opened_with_defaults(
    storage: Storage,
) -> Result<()> {
    const ID: &str = "catalog-row-with-no-head";
    let Some((double, _held)) = double_over(storage).await else {
        return Ok(());
    };
    let core = core_over(&double, NODE_BUDGET)?;
    let store = lash_core::runtime::admit_session_view(
        &core.store_factory,
        &lash_core::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from(ID),
            relation: lash_core::SessionRelation::Root,
            config: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded).into(),
            head: lash_core::SessionCreationHead::CommittedByCreator,
        },
    )
    .await
    .map_err(EmbedError::Store)?;
    assert!(head_of(&store).await?.is_none(), "the row has no head");

    let opened = core.session(ID).open().await;
    assert!(
        matches!(
            &opened,
            Err(EmbedError::SessionCreationUnrecorded { session_id }) if session_id.as_str() == ID
        ),
        "the host's open is refused as creation-unrecorded: {:?}",
        opened.err()
    );

    queue_an_append(&store, ID, None).await;
    schedule_a_drive(&core, ID).await;
    let code = lash_core::RuntimeErrorCode::SessionCreationUnrecorded.as_str();
    let refused = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            if invocations(&double)
                .iter()
                .any(|invocation| invocation.contains(code))
            {
                return;
            }
            assert!(
                matches!(head_of(&store).await, Ok(None)),
                "no drive commits a head for the row"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        refused.is_ok(),
        "the engine's drive is refused as {code}: invocations {:?}",
        invocations(&double)
    );
    assert!(
        head_of(&store).await?.is_none(),
        "the refused drive committed no head"
    );
    assert_eq!(
        store
            .list_open_queued_work()
            .await
            .map_err(EmbedError::Store)?
            .len(),
        1,
        "the queued command is not settled over defaults"
    );
    Ok(())
}

/// A send to a catalog row with no head ends: the sender's output answers
/// the typed creation-unrecorded refusal of the drive that could not open
/// the session, and never waits on a turn that cannot run.
async fn a_send_to_a_catalog_row_with_no_head_answers_the_typed_refusal(
    storage: Storage,
) -> Result<()> {
    const ID: &str = "send-to-a-catalog-row-with-no-head";
    let Some((double, _held)) = double_over(storage).await else {
        return Ok(());
    };
    let core = core_over(&double, NODE_BUDGET)?;
    let store = lash_core::runtime::admit_session_view(
        &core.store_factory,
        &lash_core::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from(ID),
            relation: lash_core::SessionRelation::Root,
            config: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded).into(),
            head: lash_core::SessionCreationHead::CommittedByCreator,
        },
    )
    .await
    .map_err(EmbedError::Store)?;

    let sent = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        core.session(ID)
            .durable()
            .await?
            .send(TurnInput::text("a turn the row can never run"))
            .output()
            .await
    })
    .await;
    let Ok(output) = sent else {
        panic!(
            "the sender's output answers the refusal: none in 60 s, invocations {:?}",
            invocations(&double)
        );
    };
    let refusal = match output {
        Ok(output) => panic!("the row runs no turn: {:?}", output.result.outcome),
        Err(refusal) => refusal,
    };
    let code = lash_core::RuntimeErrorCode::SessionCreationUnrecorded;
    assert!(
        format!("{refusal:?}").contains(&format!("{code:?}")),
        "the sender's output answers the typed creation-unrecorded refusal: {refusal:?}"
    );
    assert!(
        head_of(&store).await?.is_none(),
        "the refused send committed no head"
    );
    Ok(())
}

macro_rules! crashed_create_drain_laws {
    ($($(#[$service:meta])* $storage:ident: $kind:expr;)*) => {
        $(
            mod $storage {
                use super::*;

                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                $(#[$service])*
                async fn a_refused_append_drained_after_a_crashed_create_leaves_nothing_of_it()
                -> Result<()> {
                    super::a_refused_append_drained_after_a_crashed_create_leaves_nothing_of_it(
                        $kind,
                    )
                    .await
                }

                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                $(#[$service])*
                async fn an_append_drained_after_a_crashed_create_is_committed_once() -> Result<()>
                {
                    super::an_append_drained_after_a_crashed_create_is_committed_once($kind).await
                }

                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                $(#[$service])*
                async fn a_refused_append_drain_after_a_crashed_create_reopens_on_a_new_deployment()
                -> Result<()> {
                    super::a_session_drained_after_a_crashed_create_reopens_on_a_new_deployment(
                        $kind,
                        Append::Refused,
                    )
                    .await
                }

                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                $(#[$service])*
                async fn an_accepted_append_drain_after_a_crashed_create_reopens_on_a_new_deployment()
                -> Result<()> {
                    super::a_session_drained_after_a_crashed_create_reopens_on_a_new_deployment(
                        $kind,
                        Append::Accepted,
                    )
                    .await
                }

                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                $(#[$service])*
                async fn a_send_to_a_catalog_row_with_no_head_answers_the_typed_refusal()
                -> Result<()> {
                    super::a_send_to_a_catalog_row_with_no_head_answers_the_typed_refusal($kind)
                        .await
                }

                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                $(#[$service])*
                async fn a_catalog_row_with_no_head_is_refused_and_never_opened_with_defaults()
                -> Result<()> {
                    super::a_catalog_row_with_no_head_is_refused_and_never_opened_with_defaults(
                        $kind,
                    )
                    .await
                }
            }
        )*
    };
}

crashed_create_drain_laws! {
    sqlite_memory: Storage::SqliteMemory;
    sqlite_file: Storage::SqliteFile;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    postgres: Storage::Postgres;
}
