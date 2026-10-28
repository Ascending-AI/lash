//! The claim-obligation reducer's laws over hand-built transactions
//! (FIG-6089): one token stays required until the last of its uses
//! settles, commits through one claim write their requirements in the
//! order they land (FIG-6075 R-11), and a commit that leaves the head in
//! place does not read it (FIG-6209).

use std::sync::Arc;
use std::time::Duration;

use lash_core_store::checkpoint_graph::DecodeLimits;
use lash_core_store::effect_opener::EffectOpener;
use lash_core_store::tool_run::{
    MaterialDigest, MaterialLocation, MaterialOwner, MaterialRef, MaterialRole,
};
use lash_durable::domain::{OwnerKey, RunSeq};
use lash_durable::{ActorKey, ActorTx, DurableInstant, Epoch, MailSeq, OpenedActor, StateRevision};
use lash_kernel_value::identity::SessionId;
use lash_kernel_value::tool_contract::ExecutionPolicy;
use lash_protocol_types::ExecutionLimit;
use lash_vocabulary::identity::TurnId;

use super::{ReadScope, RecordedWork, SessionObligations, Staged};
use crate::ToolCallId;
use crate::runtime::actor::round::{self, AdmittedExecution, ExecutionDraft};
use crate::tool_run::{AdmittedBinding, PresentationBinding};
use lash_core_store::tool_run::SettledOutput;
use lash_vocabulary::tool_contract::ToolId;

fn session() -> SessionId {
    SessionId::try_from("s".to_owned()).unwrap()
}

fn owner() -> OwnerKey {
    OwnerKey::Turn(session(), TurnId::try_from("t".to_owned()).unwrap())
}

fn opened() -> ActorTx {
    ActorTx::opened(OpenedActor {
        actor: ActorKey::session("s").unwrap(),
        epoch: Epoch(1),
        revision: StateRevision(0),
        acked: MailSeq(0),
        seen: MailSeq(0),
        mail: Vec::new(),
        at: DurableInstant(0),
        turn_cancel: None,
    })
}

fn limits() -> DecodeLimits {
    DecodeLimits::within_total(1 << 24)
}

/// The binding of a call to `owner`'s provider `tools`, with no
/// presentation callbacks.
fn bound_to(owner: &str) -> AdmittedBinding {
    let provider = lash_vocabulary::providers::PluginCallbackIdentity {
        owner: owner.to_owned(),
        key: "tool_provider:tools".to_owned(),
    };
    AdmittedBinding {
        executable: provider.clone(),
        preparation: provider,
        presentation: PresentationBinding::default(),
    }
}

/// A draft of call `name` to `owner`'s provider.
fn draft(name: &str, owner: &str) -> ExecutionDraft {
    ExecutionDraft::new(
        lash_durable::DurableInstant(0),
        ToolCallId::fixture(name),
        ToolId::new(name),
        MaterialRef {
            owner: MaterialOwner::Run {
                opener: EffectOpener::turn(session(), TurnId::try_from("t".to_owned()).unwrap()),
            },
            role: MaterialRole::PreparedRequest,
            location: MaterialLocation::JournalLocal,
            digest: MaterialDigest::parse(&format!("{:064x}", name.len())).unwrap(),
        },
        ExecutionPolicy::Once,
        ExecutionLimit {
            expires_at: 1_000_000,
            max_slice: Duration::from_secs(60),
        },
        None,
    )
    .with_binding(Some(bound_to(owner)))
}

/// A transaction admitting `drafts` as run `run`, and its members.
fn admitting(run: u64, drafts: Vec<ExecutionDraft>) -> (ActorTx, Vec<AdmittedExecution>) {
    let mut tx = opened();
    let members = round::admit(&mut tx, &owner(), RunSeq(run), drafts).unwrap();
    (tx, members)
}

/// A transaction settling `member`.
fn settling(member: &AdmittedExecution) -> ActorTx {
    let mut tx = opened();
    round::settle(
        &mut tx,
        member,
        SettledOutput::Interrupted,
        Vec::new(),
        None,
    )
    .unwrap();
    tx
}

fn token(owner: &str) -> String {
    lash_vocabulary::claims::ObligationToken::for_callback(
        lash_vocabulary::claims::RegistryKind::Session,
        &bound_to(owner).executable,
    )
    .to_string()
}

/// A commit at the claim's epoch that states no root's debts.
fn staged() -> Staged<'static> {
    Staged {
        epoch: Epoch(1),
        owing: None,
        limits: limits(),
        phase_limits: limits(),
    }
}

/// A seeded ledger reads nothing.
async fn unread(_: SessionId, _: ReadScope) -> Result<RecordedWork, lash_durable::DurableError> {
    panic!("the ledger knows the session's work")
}

/// OWED-IS-NOT-INSTALLED: two calls owe one provider; settling one keeps
/// it required, settling both drops it.
#[test]
fn a_token_stays_required_until_its_last_use_settles() {
    let mut work = RecordedWork::session();
    let (tx, members) = admitting(1, vec![draft("a", "k"), draft("b", "k")]);
    work.apply(&tx, Epoch(1), None, limits(), limits()).unwrap();
    assert_eq!(work.requires(), [token("k")]);
    assert_eq!(work.outstanding_obligations().len(), 2);
    work.apply(&settling(&members[0]), Epoch(1), None, limits(), limits())
        .unwrap();
    assert_eq!(work.requires(), [token("k")]);
    work.apply(&settling(&members[1]), Epoch(1), None, limits(), limits())
        .unwrap();
    assert!(work.requires().is_empty());
}

/// FIG-6075 R-11: a commit that settles K's call is in flight when another
/// admits a call to J. The second waits for the first, and writes its
/// requirements from the records the first left: the last commit to land
/// requires J and not K, whichever was staged first.
#[tokio::test]
async fn the_last_commit_to_land_writes_requirements_from_every_record_before_it() {
    let mut work = RecordedWork::session();
    let (tx, members) = admitting(1, vec![draft("x", "k")]);
    work.apply(&tx, Epoch(1), None, limits(), limits()).unwrap();
    let ledger = Arc::new(SessionObligations::seeded(
        session(),
        work,
        vec![token("k")],
    ));
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let (entered, entering) = tokio::sync::oneshot::channel::<()>();
    let settle = {
        let ledger = Arc::clone(&ledger);
        let tx = settling(&members[0]);
        crate::task::spawn(async move {
            ledger
                .commit(unread, staged(), tx, |tx| async move {
                    let _ = entered.send(());
                    let _ = released.await;
                    Ok(tx.requires().map(<[String]>::to_vec))
                })
                .await
                .unwrap()
        })
    };
    entering.await.unwrap();
    let admit = {
        let ledger = Arc::clone(&ledger);
        let (tx, _) = admitting(2, vec![draft("y", "j")]);
        crate::task::spawn(async move {
            ledger
                .commit(unread, staged(), tx, |tx| async move {
                    Ok(tx.requires().map(<[String]>::to_vec))
                })
                .await
                .unwrap()
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !admit.is_finished(),
        "the admission waits for the settlement in flight"
    );
    release.send(()).unwrap();
    assert_eq!(settle.await.unwrap(), Some(Vec::new()));
    assert_eq!(admit.await.unwrap(), Some(vec![token("j")]));
}

/// FIG-6209: an unstarted turn refused because its session's head does not
/// read ends without the head being read again. Its commit leaves the
/// requirements the claim found, all the head's, exactly as they were, and
/// the next commit still owes them.
#[tokio::test]
async fn a_refused_unstarted_turn_on_an_unreadable_head_keeps_the_heads_requirements() {
    let ledger = SessionObligations::claimed(session(), &[token("k")]);
    let scopes = std::sync::Mutex::new(Vec::new());
    let read = |_: SessionId, scope: ReadScope| {
        scopes.lock().unwrap().push(scope);
        async move {
            match scope {
                // The unstarted turn owes nothing.
                ReadScope::Turn => Ok(RecordedWork::session()),
                ReadScope::Head => Err(lash_durable::DurableError::StoreRefused(
                    crate::store::StoreRefusal::SessionStateVersionUnsupported {
                        found: 0,
                        current: 1,
                    },
                )),
            }
        }
    };
    let mut refusing = opened();
    refusing.write(lash_durable::DomainWrite::Turn(
        lash_durable::domain::TurnWrite::Terminal {
            session: session(),
            run: TurnId::try_from("t".to_owned()).unwrap(),
            cause: Box::new(crate::store::RunTerminalCause::Refused {
                refusal: crate::RuntimeError::new(
                    crate::RuntimeErrorCode::SessionStateVersionUnsupported,
                    "session state version 0 is unsupported",
                )
                .into(),
            }),
            head_revision: None,
            transcript: None,
        },
    ));
    let refused = ledger
        .commit(read, staged(), refusing, |tx| async move {
            Ok(tx.requires().map(<[String]>::to_vec))
        })
        .await
        .expect("the refusal commits");
    assert_eq!(refused, None, "the head's requirements stand unchanged");
    assert_eq!(
        *scopes.lock().unwrap(),
        [ReadScope::Turn],
        "the head is not read"
    );
    let (admit, _) = admitting(1, vec![draft("y", "j")]);
    let admitted = ledger
        .commit(unread, staged(), admit, |tx| async move {
            Ok(tx.requires().map(<[String]>::to_vec))
        })
        .await
        .unwrap();
    let mut owed = vec![token("j"), token("k")];
    owed.sort();
    assert_eq!(
        admitted,
        Some(owed),
        "the head's requirement is not dropped"
    );
}

/// What `requirements` is as claim tokens, in the order `requires` answers.
fn hard(requirements: &lash_vocabulary::claims::ClaimRequirements) -> Vec<String> {
    let mut tokens: Vec<String> =
        lash_vocabulary::claims::ObligationToken::for_requirements(requirements)
            .map(String::from)
            .collect();
    tokens.sort();
    tokens
}

/// A record naming one of each hard kind: a native, a kernel version, a
/// provider and a codec version.
fn record() -> lash_vocabulary::claims::ClaimRequirements {
    lash_vocabulary::claims::ClaimRequirements {
        machine: lash_kernel_value::claims::NodeRequirements {
            helpers: Default::default(),
            libraries: Default::default(),
            natives: [lash_kernel_value::FunctionId::from_bytes([3; 32])].into(),
            kernels: [1].into(),
        },
        providers: [lash_vocabulary::providers::ToolProviderHandle::new(
            lash_vocabulary::claims::RegistryKind::Session,
            "o",
            lash_vocabulary::providers::ToolProviderKey::new("p").unwrap(),
        )]
        .into(),
        callbacks: Default::default(),
        components: Default::default(),
        codecs: [(
            lash_vocabulary::providers::ToolProviderHandle::new(
                lash_vocabulary::claims::RegistryKind::Session,
                "o",
                lash_vocabulary::providers::ToolProviderKey::new("q").unwrap(),
            ),
            lash_kernel_value::providers::FormatVersion::new(2).unwrap(),
        )]
        .into(),
    }
}

/// D1 (FIG-6993): the commit that puts a cell's execution root states its
/// hard record, and the record is the session's claim requirement, one
/// token per requirement, for exactly as long as the root stands: the
/// commit that clears it requires nothing of it. The tokens read back as
/// the record they were written from.
#[test]
fn an_execution_roots_hard_record_is_required_while_the_root_stands() {
    use lash_durable::domain::graph::ExecRootWrite;
    let exec = lash_durable::domain::ExecKey::Cell(
        session(),
        TurnId::try_from("t".to_owned()).unwrap(),
        lash_durable::domain::CellId::new("c".to_owned()),
    );
    let mut work = RecordedWork::session();
    let mut put = opened();
    put.write(lash_durable::DomainWrite::ExecRoot(ExecRootWrite::Put {
        exec: exec.clone(),
        expected: None,
        expected_root: None,
        root: lash_core_store::checkpoint_graph::NodeId([1; 32]),
        commit_id: lash_core_store::checkpoint_graph::CommitId([2; 32]),
    }));
    let debts = super::ExecutionDebts {
        exec: exec.clone(),
        owed: Vec::new(),
        requirements: record(),
    };
    work.apply(&put, Epoch(1), Some(&debts), limits(), limits())
        .unwrap();
    assert_eq!(work.requires(), hard(&record()));
    assert_eq!(
        lash_vocabulary::claims::ObligationToken::requirements_of(
            work.requires().iter().map(String::as_str)
        ),
        record(),
        "the tokens read back as the record"
    );
    let mut cleared = opened();
    cleared.write(lash_durable::DomainWrite::ExecRoot(ExecRootWrite::Clear {
        exec,
    }));
    work.apply(&cleared, Epoch(1), None, limits(), limits())
        .unwrap();
    assert!(work.requires().is_empty(), "{:?}", work.requires());
}

/// STAGED-CLAIMS (package model §15.2): a resolved call owes the target's
/// provider and payload codec, including both registry-qualified callbacks,
/// alongside its admission binding until settlement retires that invocation.
#[test]
fn a_matched_call_owes_its_provider_and_codec_until_it_settles() {
    let provider = lash_vocabulary::providers::ToolProviderHandle::new(
        lash_vocabulary::claims::RegistryKind::Session,
        "o",
        crate::tool_provider_key!("p"),
    );
    let codec = lash_vocabulary::providers::ToolProviderHandle::new(
        lash_vocabulary::claims::RegistryKind::Session,
        "o",
        crate::tool_provider_key!("q"),
    );
    let callbacks = [
        lash_vocabulary::claims::CallbackRequirement {
            registry: provider.registry(),
            callback: provider.callback_identity(),
        },
        lash_vocabulary::claims::CallbackRequirement {
            registry: codec.registry(),
            callback: codec.callback_identity(),
        },
    ];
    let definition = crate::ToolDefinition::raw(
        "matched",
        "tools.matched",
        "",
        serde_json::json!({}),
        serde_json::json!({"type":"null"}),
    )
    .unwrap()
    .with_execution(Duration::from_secs(60));
    let link = lash_core_store::tool_surface::OperationRoute {
        tool: ToolId::new("matched"),
        provider,
        policy: ExecutionPolicy::Once,
        projection: Default::default(),
        payload: Some(lash_vocabulary::providers::ExecutionBinding {
            codec,
            format: lash_kernel_value::providers::FormatVersion::new(2).unwrap(),
            payload: serde_json::json!({}),
        }),
        source: lash_core_store::tool_surface::RouteSource::Resolved {
            definition: Box::new(definition),
        },
    };
    let call = ToolCallId::fixture("matched");
    let binding = bound_to("o");
    let mut work = RecordedWork::session();
    work.admit_linked(owner(), call.clone(), Some(&binding), Some(&link));
    let mut owed = hard(&lash_vocabulary::claims::ClaimRequirements {
        callbacks: callbacks.into(),
        providers: [lash_vocabulary::providers::ToolProviderHandle::new(
            lash_vocabulary::claims::RegistryKind::Session,
            "o",
            lash_vocabulary::providers::ToolProviderKey::new("p").unwrap(),
        )]
        .into(),
        codecs: [(
            lash_vocabulary::providers::ToolProviderHandle::new(
                lash_vocabulary::claims::RegistryKind::Session,
                "o",
                lash_vocabulary::providers::ToolProviderKey::new("q").unwrap(),
            ),
            lash_kernel_value::providers::FormatVersion::new(2).unwrap(),
        )]
        .into(),
        ..Default::default()
    });
    owed.push(token("o"));
    owed.sort();
    assert_eq!(work.requires(), owed);
    work.settle(&owner(), &call);
    assert!(work.requires().is_empty(), "{:?}", work.requires());
}

/// A detached commit must not reinstall claim work after activation exit.
#[tokio::test]
async fn a_contended_commit_cannot_restore_released_obligations() {
    let ledger = Arc::new(SessionObligations::seeded(
        session(),
        RecordedWork::session(),
        vec![],
    ));
    let (entered, entering) = tokio::sync::oneshot::channel();
    let (resume, resumed) = tokio::sync::oneshot::channel();
    let task_ledger = ledger.clone();
    let task = crate::task::spawn(async move {
        let mut tx = opened();
        tx.set_requires(vec![]);
        task_ledger
            .commit(unread, staged(), tx, |_| async move {
                entered.send(()).unwrap();
                resumed.await.unwrap();
                Ok(())
            })
            .await
    });
    entering.await.unwrap();
    ledger.invalidate_on_exit();
    resume.send(()).unwrap();
    task.await.unwrap().unwrap();
    assert!(
        ledger.state.lock().await.is_err(),
        "released obligations remain accessible"
    );
    assert!(
        !ledger.state.retains_value_for_testing().await,
        "released obligation storage remains retained"
    );
}
