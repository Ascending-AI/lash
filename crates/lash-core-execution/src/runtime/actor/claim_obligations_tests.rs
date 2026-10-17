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
use lash_sansio::{ExecutionLimit, ExecutionPolicy, SessionId, TurnId};

use super::{ReadScope, RecordedWork, SessionObligations, Staged};
use crate::runtime::actor::round::{self, AdmittedExecution, ExecutionDraft};
use crate::tool_run::{AdmittedBinding, PresentationBinding};
use crate::{ToolCallId, ToolId};
use lash_core_store::tool_run::SettledOutput;

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
    let provider = crate::plugin::PluginCallbackIdentity {
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
    crate::ObligationToken::for_callback(crate::RegistryKind::Session, &bound_to(owner).executable)
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
