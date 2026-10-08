//! FIG-4492's crash window: a session whose create died between its catalog
//! row and its first head commit, then the drain of a queued command
//! (FIG-4553).
//!
//! The session manager's create admits the catalog row and commits the
//! session's first head a few awaits later (`session_init.rs`:
//! `bind_session_store`, then `commit_initialized_session`). A creator that
//! dies in between leaves a session no commit has written. Its admission
//! records the creation's complete config with the row, in the catalog's
//! transaction, so the session the engine opens runs exactly that config:
//! the drain of a command queued for it commits a head that records
//! it, and a later deployment opens that head.
//!
//! Each law has the real creator make its admission and never its commit:
//! the create states more initial nodes than the core's commit budget
//! admits, so its first commit is refused after the row is written. The law
//! then appends through the session's admin, a session command the
//! session's actor applies and commits on the core's own node.
//!
//! A catalog row with no head at all recorded no config. It is never opened
//! with defaults: the host's open and a send to it are both refused with the
//! typed creation-unrecorded refusal, and nothing is committed.
//!
//! Over SQLite memory, SQLite file and PostgreSQL. The PostgreSQL legs are
//! ignored outside a PostgreSQL gate, which selects them with
//! `--include-ignored`.

use super::*;
use crate::support::TurnOutcome;

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
pub(super) enum Storage {
    SqliteMemory,
    SqliteFile,
    Postgres,
}

/// The stores of `storage`, with what they need to outlive them and the
/// catalog's test seams.
pub(super) async fn stores_of(
    storage: Storage,
) -> (
    Arc<dyn lash_core::StoreSet>,
    Box<dyn std::any::Any>,
    Arc<dyn lash_core::store::StoreTestSupport>,
) {
    match storage {
        Storage::SqliteMemory => {
            let stores = sqlite_memory_store_set().await;
            let seams = stores.session_store_factory();
            (stores, Box::new(()), seams)
        }
        Storage::SqliteFile => {
            let files = tempfile::tempdir().expect("a SQLite store directory");
            let stores = lash_sqlite_store::SqliteStoreSet::open(
                files.path().join("lash.db"),
                lash_sqlite_store::SqliteSynchronous::Normal,
            )
            .await
            .expect("open the SQLite file stores");
            let seams = stores.session_store_factory();
            (Arc::new(stores), Box::new(files), seams)
        }
        Storage::Postgres => {
            let url = lash_postgres_store::testing::required_database_url();
            let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
            let storage = lash_postgres_store::testing::connect(database.url())
                .await
                .expect("connect to PostgreSQL");
            let attachments = tempfile::tempdir().expect("PostgreSQL attachment directory");
            let stores = lash_postgres_store::PostgresStoreSet::new(
                &storage,
                lash_sqlite_store::SqliteStoreSet::open(
                    (attachments.path()).join("attachments.db"),
                    lash_sqlite_store::SqliteSynchronous::Normal,
                )
                .await
                .expect("SQLite attachment store")
                .attachment_store(),
            );
            let seams = stores.session_store_factory();
            (
                Arc::new(stores),
                Box::new((database, attachments, storage)),
                seams,
            )
        }
    }
}

/// A core serving its own node over `stores`, whose commits fit `nodes`
/// nodes.
fn core_over(stores: &Arc<dyn lash_core::StoreSet>, nodes: usize) -> Result<LashCore> {
    explicit_ephemeral_facets_with_budget(
        LashCore::standard_builder(lash_conformance::backend_over(Arc::clone(stores))),
        crate::CommitBudget::bounded(1024 * 1024, nodes),
    )
    .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
    .build(crate::testing::runtime_lease_owner())
}

/// A session a crashed create left, and the config its creation resolved.
struct CrashedCreate {
    store: lash_core::store::SessionStore,
    /// What a create of the same request that finished recorded.
    creation: lash_core::PersistedSessionConfig,
}

/// An admitted catalog row whose head has been removed through a test seam.
async fn headless_row(
    core: &LashCore,
    seams: &dyn lash_core::store::StoreTestSupport,
    session: &str,
) -> Result<lash_core::store::SessionStore> {
    let session_id = SessionId::fixture(session);
    core.session(session_id.clone())
        .create(crate::SessionCreation::root(
            crate::plugins::SessionToolAccess::ambient(),
            mock_session_spec(),
        ))
        .await?;
    seams
        .delete_session_head_for_testing(&session_id)
        .await
        .map_err(EmbedError::Store)?;
    lash_core::runtime::live_session_view(&core.store_factory, &session_id)
        .await
        .map_err(EmbedError::Store)?
        .ok_or_else(|| {
            EmbedError::Store(lash_core::StoreError::Backend(
                "the headless row the fixture wrote is absent".to_string(),
            ))
        })
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
        .session(SessionId::fixture(format!("{session}-creator")))
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
    // A session starts from exactly its create request (FIG-5296): the
    // request states the creator's policy.
    let policy = creator.policy_snapshot();
    let request = |id: &str| {
        let mut request = lash_core::SessionCreateRequest::root(
            crate::plugins::SessionToolAccess::ambient(),
            lash_core::SessionStartPoint::Empty,
            lash_core::PluginOptions::default(),
        )
        .with_session_id(lash_core::SessionId::fixture(id));
        request.policy = Some(policy.clone());
        request
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
        let id = SessionId::fixture(id);
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

/// The append of [`NOTE`], requiring `ancestor` when given.
fn an_append(ancestor: Option<&str>) -> lash_core::AppendSessionNodesRequest {
    lash_core::AppendSessionNodesRequest {
        operation_id: "host-append:after-crashed-create".to_string(),
        nodes: vec![lash_core::SessionAppendNode::message(
            lash_core::PluginMessage::text(lash_core::MessageRole::User, NOTE),
        )],
        requires_ancestor_node_id: ancestor.map(|id| lash_core::NodeId::fixture(id.to_string())),
    }
}

/// The append's settlement and the head it committed.
struct Drained {
    outcome: lash_core::AppendSessionNodesOutcome,
    /// How many nodes of the committed head carry [`NOTE`].
    notes_in_head: usize,
    /// The config the committed head records.
    recorded: lash_core::PersistedSessionConfig,
}

/// Append [`NOTE`] to `session`, requiring `ancestor` when given: a session
/// command the session's actor drains and settles.
async fn drain_an_append(
    core: &LashCore,
    store: &lash_core::store::SessionStore,
    session: &str,
    ancestor: Option<&str>,
) -> Result<Drained> {
    let host = core.session(SessionId::fixture(session)).open().await?;
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(90),
        host.admin()
            .state()
            .append_session_nodes(an_append(ancestor)),
    )
    .await
    .expect("the session's actor drains the queued command")?
    .settle_with(
        &host.admin().commands(),
        crate::testing::admin_fixture_outcome,
    )
    .await?;
    assert!(
        store
            .list_open_queued_work()
            .await
            .map_err(EmbedError::Store)?
            .is_empty(),
        "the drained command left nothing open"
    );
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

/// An append the store's ancestor check refuses, drained first
/// after a crashed create, settles `StaleBranch`. The head its settlement
/// commits holds nothing of it and records the creation's config.
async fn a_refused_append_drained_after_a_crashed_create_leaves_nothing_of_it(
    storage: Storage,
) -> Result<()> {
    const ID: &str = "crashed-create-refused-append";
    let (stores, _held, _seams) = stores_of(storage).await;
    let core = core_over(&stores, NODE_BUDGET)?;
    let crashed = crashed_create(&core, ID).await?;
    let drained = drain_an_append(&core, &crashed.store, ID, Append::Refused.ancestor()).await?;
    let lash_core::AppendSessionNodesOutcome::StaleBranch { required_node_id } = &drained.outcome
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

/// An accepted append drained first after a crashed create is
/// in the head its settlement commits exactly once, and that head records
/// the creation's config.
async fn an_append_drained_after_a_crashed_create_is_committed_once(
    storage: Storage,
) -> Result<()> {
    const ID: &str = "crashed-create-accepted-append";
    let (stores, _held, _seams) = stores_of(storage).await;
    let core = core_over(&stores, NODE_BUDGET)?;
    let crashed = crashed_create(&core, ID).await?;
    let drained = drain_an_append(&core, &crashed.store, ID, Append::Accepted.ancestor()).await?;
    assert!(
        matches!(
            &drained.outcome,
            lash_core::AppendSessionNodesOutcome::Appended { node_ids, .. } if node_ids.len() == 1
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

/// The head a drain commits after a crashed create is one a new deployment
/// opens: after the drain, a core restarted over the same stores runs a turn
/// of the session, which its actor opens from that head.
async fn a_session_drained_after_a_crashed_create_reopens_on_a_new_deployment(
    storage: Storage,
    append: Append,
) -> Result<()> {
    let id = format!("crashed-create-reopen-{append:?}").to_lowercase();
    let (stores, _held, _seams) = stores_of(storage).await;
    {
        let core = core_over(&stores, NODE_BUDGET)?;
        let crashed = crashed_create(&core, &id).await?;
        drain_an_append(&core, &crashed.store, &id, append.ancestor()).await?;
        core.shutdown().await?;
    }
    let core = core_over(&stores, 512)?;
    let turn = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        core.session(SessionId::fixture(id.clone()))
            .durable()
            .await?
            .send(TurnInput::text("answer after the drain"))
            .output()
            .await
    })
    .await;
    let Ok(output) = turn else {
        panic!("the restarted deployment runs a turn of the session: none finished in 60 s");
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
/// error, and a command queued for it settles nothing and commits no head.
async fn a_catalog_row_with_no_head_is_refused_and_never_opened_with_defaults(
    storage: Storage,
) -> Result<()> {
    const ID: &str = "catalog-row-with-no-head";
    let (stores, _held, seams) = stores_of(storage).await;
    let core = core_over(&stores, NODE_BUDGET)?;
    let store = headless_row(&core, seams.as_ref(), ID).await?;
    assert!(head_of(&store).await?.is_none(), "the row has no head");

    let opened = core
        .session(crate::SessionId::parse(ID).expect("nonblank host identity"))
        .open()
        .await;
    assert!(
        matches!(
            &opened,
            Err(EmbedError::SessionCreationUnrecorded { session_id }) if session_id.as_str() == ID
        ),
        "the host's open is refused as creation-unrecorded: {:?}",
        opened.err()
    );

    // The command is queued on the row as a host's earlier submission left
    // it, and its actor is asked to work the session.
    store
        .enqueue_queued_work(
            lash_core_store::queued_work_vocabulary::QueuedWorkBatchDraft::new(
                SessionId::fixture(ID),
                lash_core::DeliveryPolicy::AfterCurrentTurnCommit,
                lash_core_store::queued_work_vocabulary::SessionCommand::AppendSessionNodes {
                    request: Box::new(an_append(None)),
                },
            )
            .with_source_key("after-crashed-create".to_string()),
        )
        .await
        .map_err(EmbedError::Store)?;
    // A send wakes the session's actor, which drains the command first: it
    // cannot apply it to a session with no recorded config, and parks the
    // session with that refusal once its passes reach the activation-loop
    // budget, settling nothing over defaults.
    core.session(crate::SessionId::parse(ID).expect("nonblank host identity"))
        .durable()
        .await?
        .send(TurnInput::text("work the row"))
        .await?;
    let actor = lash_core::durable_port::ActorKey::session(ID).expect("a session actor key");
    let parked = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            if let Ok(Some(snapshot)) = core.backend.durable().actor(&actor).await
                && snapshot.state == lash_core::durable_port::ActorState::Parked
            {
                return snapshot;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the session parks instead of applying the command over defaults");
    let reason = parked
        .park
        .as_deref()
        .map(serde_json::from_str::<lash_core::runtime::durable::session::SessionParkReason>);
    assert!(
        matches!(
            &reason,
            Some(Ok(lash_core::runtime::durable::session::SessionParkReason::PassLoop { error, .. }))
                if error.contains("recorded no config")
        ),
        "the session parks for its unrecorded creation: {reason:?}"
    );
    assert!(
        head_of(&store).await?.is_none(),
        "nothing committed a head for the row"
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
/// the typed creation-unrecorded refusal of the actor that could not open
/// the session, and never waits on a turn that cannot run.
async fn a_send_to_a_catalog_row_with_no_head_answers_the_typed_refusal(
    storage: Storage,
) -> Result<()> {
    const ID: &str = "send-to-a-catalog-row-with-no-head";
    let (stores, _held, seams) = stores_of(storage).await;
    let core = core_over(&stores, NODE_BUDGET)?;
    let store = headless_row(&core, seams.as_ref(), ID).await?;

    let sent = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        core.session(crate::SessionId::parse(ID).expect("nonblank host identity"))
            .durable()
            .await?
            .send(TurnInput::text("a turn the row can never run"))
            .output()
            .await
    })
    .await;
    let Ok(output) = sent else {
        panic!("the sender's output answers the refusal: none in 60 s");
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
