//! Runtime visitation over every DurableReads method (FIG-6687).
#![allow(clippy::expect_used)]
use super::surface_sweep::SURFACE_RUN_ID;
use super::*;
use lash_core_store::checkpoint_graph::{DecodeLimits, GraphReadPolicy, GraphRefusal};
use lash_durable::domain::{
    CellId, ExecKey, ModelCallId, OwnerKey, PromptCallKey, RootOwner, RunSeq, ScopeKey, WaitId,
};
use lash_durable::{ActorKey, DurableStore};

#[derive(Clone, Copy, Debug)]
pub(super) enum DurableReadMethod {
    Turn,
    TurnNamespaces,
    TurnEnd,
    RunFacts,
    RunFactsOf,
    RunFactsWithin,
    ExecRoot,
    CellRoots,
    CellRootsWithin,
    PendingWaits,
    Wait,
    Process,
    LiveUntilDescendants,
    UntilChildren,
    SessionClose,
    EndingScopes,
    SessionMailbox,
    PromptSnapshot,
    PromptTexts,
    Hydrate,
}

impl DurableReadMethod {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Turn => "turn",
            Self::TurnNamespaces => "turn_namespaces",
            Self::TurnEnd => "turn_end",
            Self::RunFacts => "run_facts",
            Self::RunFactsOf => "run_facts_of",
            Self::RunFactsWithin => "run_facts_within",
            Self::ExecRoot => "exec_root",
            Self::CellRoots => "cell_roots",
            Self::CellRootsWithin => "cell_roots_within",
            Self::PendingWaits => "pending_waits",
            Self::Wait => "wait",
            Self::Process => "process",
            Self::LiveUntilDescendants => "live_until_descendants",
            Self::UntilChildren => "until_children",
            Self::SessionClose => "session_close",
            Self::EndingScopes => "ending_scopes",
            Self::SessionMailbox => "session_mailbox",
            Self::PromptSnapshot => "prompt_snapshot",
            Self::PromptTexts => "prompt_texts",
            Self::Hydrate => "hydrate",
        }
    }
}

/// Survey reads use absent addresses beside the nonempty call ledger; every
/// scheduled method awaits its actual adapter before recording visitation.
pub(super) async fn drive(
    store: &dyn DurableStore,
    session: &SessionId,
    method: DurableReadMethod,
    visited: &mut std::collections::BTreeSet<&'static str>,
) -> String {
    // Keep these diagnostic reads on absent addresses even when the sweep's
    // real session has ingress with backend-minted batch identities.
    let absent_session = SessionId::fixture(format!("{session}:absent-reads"));
    let session = &absent_session;
    let run: lash_protocol_types::TurnId = SURFACE_RUN_ID.into();
    let owner = OwnerKey::Turn(session.clone(), run.clone());
    let actor = ActorKey::session(session.as_str()).expect("session actor");
    let exec = ExecKey::Cell(
        session.clone(),
        run.clone(),
        CellId::new("coverage-absent-cell"),
    );
    let scope = ScopeKey::Session(session.clone());
    let process = lash_core::ProcessId::fixture("coverage-absent-process");
    let call = PromptCallKey {
        session: session.clone(),
        call: ModelCallId::Turn {
            run: run.clone(),
            ordinal: 99,
        },
    };
    let answer = match method {
        DurableReadMethod::Turn => {
            format!("{:?}", store.turn(session).await.expect("execute turn"))
        }
        DurableReadMethod::TurnNamespaces => format!(
            "{:?}",
            store
                .turn_namespaces(session, &run)
                .await
                .expect("execute turn_namespaces")
        ),
        DurableReadMethod::TurnEnd => format!(
            "{:?}",
            store
                .turn_end(session, &run)
                .await
                .expect("execute turn_end")
        ),
        DurableReadMethod::RunFacts => format!(
            "{:?}",
            store.run_facts(&owner).await.expect("execute run_facts")
        ),
        DurableReadMethod::RunFactsOf => format!(
            "{:?}",
            store
                .run_facts_of(&owner, RunSeq(1))
                .await
                .expect("execute run_facts_of")
        ),
        DurableReadMethod::RunFactsWithin => format!(
            "{:?}",
            store
                .run_facts_within(
                    &owner,
                    Some(RunSeq(1)),
                    4096,
                    DecodeLimits::within_total(16 * 1024 * 1024)
                )
                .await
                .expect("execute run_facts_within")
        ),
        DurableReadMethod::ExecRoot => format!(
            "{:?}",
            store.exec_root(&exec).await.expect("execute exec_root")
        ),
        DurableReadMethod::CellRoots => format!(
            "{:?}",
            store
                .cell_roots(session, &run)
                .await
                .expect("execute cell_roots")
        ),
        DurableReadMethod::CellRootsWithin => format!(
            "{:?}",
            store
                .cell_roots_within(session, &run, 4096)
                .await
                .expect("execute cell_roots_within")
        ),
        DurableReadMethod::PendingWaits => format!(
            "{:?}",
            store
                .pending_waits(&actor)
                .await
                .expect("execute pending_waits")
        ),
        DurableReadMethod::Wait => format!(
            "{:?}",
            store.wait(&WaitId([0x67; 16])).await.expect("execute wait")
        ),
        DurableReadMethod::Process => format!(
            "{:?}",
            store.process(&process).await.expect("execute process")
        ),
        DurableReadMethod::LiveUntilDescendants => format!(
            "{:?}",
            store
                .live_until_descendants(&scope, 4096)
                .await
                .expect("execute live_until_descendants")
        ),
        DurableReadMethod::UntilChildren => format!(
            "{:?}",
            store
                .until_children(&scope, None, 4096)
                .await
                .expect("execute until_children")
        ),
        DurableReadMethod::SessionClose => format!(
            "{:?}",
            store
                .session_close(session)
                .await
                .expect("execute session_close")
        ),
        DurableReadMethod::EndingScopes => format!(
            "{:?}",
            store
                .ending_scopes(session)
                .await
                .expect("execute ending_scopes")
        ),
        DurableReadMethod::SessionMailbox => format!(
            "{:?}",
            store
                .session_mailbox(session)
                .await
                .expect("execute session_mailbox")
        ),
        DurableReadMethod::PromptSnapshot => format!(
            "{:?}",
            store
                .prompt_snapshot(&call)
                .await
                .expect("execute prompt_snapshot")
        ),
        DurableReadMethod::PromptTexts => format!(
            "{:?}",
            store
                .prompt_texts(&["coverage-absent-prompt".to_owned()])
                .await
                .expect("execute prompt_texts")
        ),
        DurableReadMethod::Hydrate => {
            let error = store
                .hydrate(
                    RootOwner::Exec(exec),
                    None,
                    GraphReadPolicy::standard().execution(),
                )
                .await
                .expect_err("an absent root is refused");
            assert!(matches!(
                error,
                lash_durable::DurableError::Graph(GraphRefusal::RootRetired)
            ));
            "root_retired".to_owned()
        }
    };
    visited.insert(method.name());
    answer
}
