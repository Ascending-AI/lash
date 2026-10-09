//! SQL-WORK fixture: five real store operations at two stored-history sizes.
//! Setup has no active window and is excluded. No node/background polling runs.
use crate::facade_support::sql::{self, Receipt};
use crate::store::{HistoryAnchor, HistoryBudget, WindowSelector};
use crate::{
    MaxToolCalls, MessageRole, NoProgressBudget, PluginMessage, RuntimeSessionState,
    SessionAppendNode, SessionId, SessionPolicy, StoreSet, TurnBudget,
};
use std::num::{NonZeroU32, NonZeroU64};

/// Functional physical receipts; every returned number is an operation-window
/// sum. The configured history page holds two nodes at either history size.
pub async fn receipts(stores: &dyn StoreSet, backend: &'static str) -> Vec<Receipt> {
    let factory = stores.session_store_factory();
    let registry = stores.process_registry();
    let durable = stores.durable_store();
    let clock = stores.clock();
    super::process_execution_env_fixture(stores.process_env_store().as_ref()).await;
    let mut receipts = Vec::new();
    for size in [2, 8] {
        let session = SessionId::fixture(format!("sql-work-{size}"));
        factory
            .admit_session(&super::store_fixtures::root_session_request(&session))
            .await
            .expect("admit SQL work session");
        let mut state = RuntimeSessionState {
            session_id: session.clone(),
            ..RuntimeSessionState::ambient_fixture(SessionPolicy::new(
                TurnBudget::Unbounded,
                MaxToolCalls::new(1024),
                NoProgressBudget::bounded(12),
            ))
        };
        let nodes = (0..size)
            .map(|_| {
                SessionAppendNode::message(PluginMessage::text(
                    MessageRole::User,
                    "SQL work fixture",
                ))
            })
            .collect::<Vec<_>>();
        let seed = crate::store::append_request_commit_with_clock_for_testing(
            &mut state,
            "seed",
            &nodes,
            None,
            clock.as_ref(),
        )
        .expect("seed commit");
        factory
            .commit_runtime_state(seed)
            .await
            .expect("seed history");
        state = crate::store::window_state(
            factory
                .load_session_window(&session, WindowSelector::Current)
                .await
                .expect("seed window")
                .expect("seed head"),
            crate::FleetFormat::current(),
        )
        .expect("seed state")
        .state;
        let append = crate::store::append_request_commit_with_clock_for_testing(
            &mut state,
            "measured",
            &nodes[..1],
            None,
            clock.as_ref(),
        )
        .expect("measured commit");
        let (answer, receipt) = sql::collect(
            format!("{session}/commit"),
            backend,
            factory.commit_runtime_state(append),
        )
        .await;
        answer.expect("commit");
        receipts.push(receipt);
        let (answer, receipt) = sql::collect(
            format!("{session}/history-load"),
            backend,
            factory.load_ancestors(
                &session,
                HistoryAnchor::Head,
                HistoryBudget {
                    max_nodes: NonZeroU32::new(2).expect("page"),
                    max_bytes: NonZeroU64::new(1024 * 1024).expect("bytes"),
                },
            ),
        )
        .await;
        assert_eq!(answer.expect("history").nodes.len(), 2);
        receipts.push(receipt);
        let (answer, receipt) = sql::collect(
            format!("{session}/window-load"),
            backend,
            factory.load_session_window(&session, WindowSelector::Current),
        )
        .await;
        assert!(answer.expect("window").is_some());
        receipts.push(receipt);
        let registration = super::held_engine_registration(
            serde_json::json!({"size": size}),
            crate::ProcessProvenance::host(),
            crate::Lifetime::Detached,
        );
        let (answer, receipt) = sql::collect(
            format!("{session}/process-register"),
            backend,
            registry.register_process(registration),
        )
        .await;
        answer.expect("register");
        receipts.push(receipt);
        let mut create = lash_durable::MailTx::new();
        create.create_actor(
            lash_durable::ActorKey::session(session.as_str()).expect("actor"),
            lash_durable::FormatSet::new("sql-work"),
        );
        durable
            .commit_mail(create, lash_durable::CommitLabel::new("sql-work.setup"))
            .await
            .expect("create wake actor");
        let mut mail = lash_durable::MailTx::new();
        mail.wake(lash_durable::ActorKey::session(session.as_str()).expect("actor"));
        let (answer, receipt) = sql::collect(
            format!("{session}/wake"),
            backend,
            durable.commit_mail(mail, lash_durable::CommitLabel::new("sql-work.wake")),
        )
        .await;
        answer.expect("wake");
        receipts.push(receipt);
    }
    for receipt in &receipts {
        assert!(
            receipt.statements > 0,
            "{} was not observed",
            receipt.operation
        );
        assert!(!receipt.shapes.is_empty());
        assert_eq!(
            receipt
                .shapes
                .values()
                .map(|shape| shape.statements)
                .sum::<u64>()
                + receipt.unretained_shape_statements,
            receipt.statements
        );
        assert_eq!(
            receipt.unretained_shape_statements, 0,
            "fixture exceeds configured map"
        );
        assert_eq!(
            receipt
                .shapes
                .values()
                .map(|shape| shape.rows_returned)
                .sum::<u64>(),
            receipt.rows_returned
        );
    }
    receipts
}

/// One owner commit through the actual durable producer, for the SQL summary's
/// join to the retained actor epoch and state revision.
pub async fn owner_commit(stores: &dyn StoreSet) -> crate::facade_support::sql::OwnerIdentity {
    use lash_durable::{ActorKey, CommitLabel, FormatSet, MailTx, NodeId, NodeSpec};
    let durable = stores.durable_store();
    let formats = FormatSet::new("sql-work-owner");
    let node = durable
        .register_node(&NodeSpec {
            node: NodeId::new("sql-work-owner"),
            decodes: vec![formats.clone()],
            ttl_millis: 15_000,
        })
        .await
        .expect("owner node");
    let actor = ActorKey::session("sql-work-owner").expect("actor");
    let mut create = MailTx::new();
    create.create_actor(actor.clone(), formats);
    durable
        .commit_mail(create, CommitLabel::new("sql-work.owner-create"))
        .await
        .expect("create owner");
    let claimed = durable.claim(&node, 1).await.expect("claim");
    assert_eq!(claimed[0].actor, actor);
    let tx = durable
        .begin(&actor, claimed[0].epoch)
        .await
        .expect("begin owner");
    let identity = crate::facade_support::sql::OwnerIdentity {
        actor: actor.as_str().to_owned(),
        epoch: tx.epoch().0,
        revision: tx.revision().0,
    };
    durable
        .commit(tx, CommitLabel::new("sql-work.owner"))
        .await
        .expect("commit owner");
    identity
}
