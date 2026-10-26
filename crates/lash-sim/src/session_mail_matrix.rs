//! The session mail laws under the crash matrix (ADR 0132 §3, §12; L3s,
//! FIG-5196): two nodes serve one session actor whose activation drains its
//! mail with the production `drain_session_mail`, while a producer sends it
//! two inputs through the store; every labelled write is cut under every
//! fault, recovered on another node, and checked.
//!
//! - **One unfinished run:** however the claimers race, the session admits
//!   one run: the first input is bound to it and the second waits.
//! - **O1:** an input sent to a session hot on another node is seen within
//!   the claim-poll interval, and an owner that lost the actor keeps
//!   nothing of it: no commit lands under a stale epoch.
//! - **Tripwire:** draining mail re-runs nothing.

use lash_core::ActorContext;
use lash_core::AdmittedScope;
use lash_core::CancellationToken;
use lash_core::PendingTurnInputDraft;
use lash_core::PendingTurnInputReadStatus;
use lash_core::TurnInput;
use lash_core::TurnInputIngress;
use lash_core::durable_port::domain::SESSION_ACTOR_FORMATS;
use lash_core::durable_port::runner::{Activation, Exit, Owned};
use lash_core::durable_port::{
    ActorKey, ActorState, CommitLabel, DurableError, DurableStore, Epoch, FormatSet, Release,
    StateRevision,
};
use lash_core::runtime::durable::session_mail::{OneInputPerRun, drain_session_mail};
use lash_durable_test::{Cut, Fault, Scenario, SimClock, SimNodes, SimNodesConfig, Tripwire};
use lash_execution_contracts::backend::Backend;
use lash_execution_contracts::backend::StoreSet;
use lash_kernel_value::identity::SessionId;
use lash_protocol_types::sync::MutexExt as _;
use lash_vocabulary::identity::InputId;
use lash_vocabulary::identity::TurnId;
use std::sync::{Arc, Mutex, OnceLock};

const SESSION: &str = "session-mail-matrix";
const NODES: [&str; 2] = ["a", "b"];
const ACK: CommitLabel = CommitLabel::new("session.ack");
const RELEASE: CommitLabel = CommitLabel::new("session.release");
/// When the producer sends the second input, in virtual milliseconds.
const SECOND_AT_MS: u64 = 2_000;
/// How long an owner keeps the session hot after its last news.
const HOT_FOR_MS: u64 = 3_000;

/// What a run shares between the database, the activation and the
/// producer.
struct Shared {
    backend: Backend,
    clock: Arc<SimClock>,
}

/// One owner commit a node saw succeed.
#[derive(Clone, Debug)]
struct Committed {
    epoch: Epoch,
    revision: StateRevision,
    admitted: Option<TurnId>,
}

#[derive(Default)]
struct Ledger {
    committed: Mutex<Vec<Committed>>,
    /// The second input, and when it was sent.
    sent: Mutex<Option<(InputId, u64)>>,
    /// When an owner's drain first saw the second input.
    seen_ms: Mutex<Option<u64>>,
    producer_done: Mutex<bool>,
}

fn session() -> SessionId {
    SessionId::from(SESSION)
}

fn second_id() -> InputId {
    PendingTurnInputDraft::keyed_input_id(&session(), "second")
}

fn actor() -> ActorKey {
    ActorKey::session(SESSION).expect("the session names its actor")
}

/// Drains each claim's mail with the production drain, admits what it
/// returns, keeps the session hot for a while, then releases it; drops it
/// at once on a fence refusal.
struct SessionDrain {
    shared: Arc<OnceLock<Shared>>,
    ledger: Arc<Ledger>,
    tripwire: Arc<Tripwire>,
}

#[async_trait::async_trait]
impl Activation for SessionDrain {
    async fn activate(&self, owned: Owned) -> Exit {
        let Some(shared) = self.shared.get() else {
            return Exit::Released;
        };
        let cx = ActorContext::new(
            shared.backend.clone(),
            owned.actor().clone(),
            owned.epoch(),
            AdmittedScope::runtime_operation("session-mail-matrix"),
            CancellationToken::new(),
            Arc::clone(&self.tripwire) as Arc<dyn lash_core::durable_port::DurableProbe>,
        );
        let mut hot_until = shared.clock.logical_ms() + HOT_FOR_MS;
        loop {
            let mut tx = match owned.begin().await {
                Ok(tx) => tx,
                Err(DurableError::OwnershipLost(_)) => return Exit::Released,
                Err(_) => {
                    owned.wait_for_mail().await;
                    continue;
                }
            };
            let woken = tx.woken();
            let Ok(drain) = drain_session_mail(&cx, &mut tx, &OneInputPerRun).await else {
                owned.wait_for_mail().await;
                continue;
            };
            if woken {
                self.observe(shared).await;
            }
            let admitted = drain.admit.map(|admitted| admitted.run);
            let label = if admitted.is_some() {
                CommitLabel::TURN_ADMIT
            } else if woken {
                ACK
            } else if shared.clock.logical_ms() < hot_until {
                owned.wait_for_mail().await;
                continue;
            } else {
                tx.give_up(Release::Idle);
                RELEASE
            };
            match owned.commit(tx, label).await {
                Ok(commit) => {
                    self.ledger.committed.lock_recover().push(Committed {
                        epoch: owned.epoch(),
                        revision: commit.revision,
                        admitted,
                    });
                    if label == RELEASE {
                        return Exit::Released;
                    }
                    hot_until = shared.clock.logical_ms() + HOT_FOR_MS;
                }
                Err(DurableError::OwnershipLost(_)) => return Exit::Released,
                Err(_) => {}
            }
        }
    }
}

impl SessionDrain {
    /// Note when an owner first sees the second input in the mailbox.
    async fn observe(&self, shared: &Shared) {
        // Identity is fixed before admission: an owner may read the committed
        // input before enqueue returns to its producer.
        let second = second_id();
        let _hold = shared.clock.hold();
        let Ok(mailbox) = shared.backend.durable().session_mailbox(&session()).await else {
            return;
        };
        if mailbox.inputs.iter().any(|input| input.input == second) {
            self.ledger
                .seen_ms
                .lock_recover()
                .get_or_insert(shared.clock.logical_ms());
        }
    }
}

/// The session mail scenario over a fresh SQLite memory store set.
#[derive(Default)]
struct SessionMail {
    shared: Arc<OnceLock<Shared>>,
    ledger: Arc<Ledger>,
    tripwire: Arc<Tripwire>,
}

async fn send(stores: &Arc<dyn StoreSet>, draft: PendingTurnInputDraft) -> Result<InputId, String> {
    stores
        .session_store_factory()
        .enqueue_pending_turn_input(draft)
        .await
        .map(|input| input.input_id)
        .map_err(|error| error.to_string())
}

#[async_trait::async_trait]
impl Scenario for SessionMail {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        let stores = lash_sqlite_store::SqliteStoreSet::memory_with_clock(
            Arc::clone(&clock) as Arc<dyn lash_core::Clock>
        )
        .await
        .expect("an in-memory store set opens");
        let durable: Arc<dyn DurableStore> = Arc::new(stores.durable_store());
        let backend = Backend::for_testing(
            Arc::new(stores),
            lash_durable::OwnerRetentionSettings::default(),
        );
        let _ = self.shared.set(Shared { backend, clock });
        durable
    }

    fn config(&self) -> SimNodesConfig {
        SimNodesConfig {
            lease: lash_durable_test::Matrix::test_lease(),
            decodes: vec![FormatSet::new(SESSION_ACTOR_FORMATS)],
            serves: Vec::new(),
            max_active: 8,
        }
    }

    fn activation(&self) -> Arc<dyn Activation> {
        Arc::new(SessionDrain {
            shared: Arc::clone(&self.shared),
            ledger: Arc::clone(&self.ledger),
            tripwire: Arc::clone(&self.tripwire),
        })
    }

    async fn start(&self, nodes: &Arc<SimNodes>) -> Result<(), String> {
        let shared = self.shared.get().ok_or("the database was not built")?;
        let stores = shared.backend.stores();
        stores
            .session_store_factory()
            .admit_session(&lash_core::testing::store_fixtures::root_session_request(
                &session(),
            ))
            .await
            .map_err(|error| error.to_string())?;
        send(
            &stores,
            PendingTurnInputDraft::new(
                session(),
                TurnInputIngress::NextTurn,
                TurnInput::text("first"),
            ),
        )
        .await?;
        for node in NODES {
            nodes.start(node);
        }
        let clock = Arc::clone(&shared.clock);
        let ledger = Arc::clone(&self.ledger);
        tokio::spawn(async move {
            lash_core::Clock::sleep(&*clock, std::time::Duration::from_millis(SECOND_AT_MS)).await;
            // The external SQLite work and its producer receipt belong to
            // one virtual instant, even if the owner reads before the reply.
            let _hold = clock.hold();
            let draft = PendingTurnInputDraft::new(
                session(),
                TurnInputIngress::NextTurn,
                TurnInput::text("second"),
            )
            .with_input_id(second_id());
            if let Ok(second) = send(&stores, draft).await {
                *ledger.sent.lock_recover() = Some((second, clock.logical_ms()));
            }
            *ledger.producer_done.lock_recover() = true;
        });
        Ok(())
    }

    fn actors(&self) -> Vec<ActorKey> {
        vec![actor()]
    }

    async fn done(&self, nodes: &SimNodes) -> bool {
        if !*self.ledger.producer_done.lock_recover() {
            return false;
        }
        let bound = match nodes.database().session_mailbox(&session()).await {
            Ok(mailbox) => mailbox.bound_run.is_some(),
            Err(_) => false,
        };
        let idle = matches!(
            nodes.database().actor(&actor()).await,
            Ok(Some(snapshot)) if !snapshot.has_mail && snapshot.state == ActorState::Idle
        );
        bound && idle
    }

    async fn check(&self, _nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let mut violations = Vec::new();
        let committed = self.ledger.committed.lock_recover().clone();
        let Some(shared) = self.shared.get() else {
            return vec!["the database was not built".into()];
        };
        self.one_unfinished_run(shared, &committed, &mut violations)
            .await;
        no_stale_write(&committed, &mut violations);
        let settings = self.config().lease.settings();
        let delay = match cut.map(|cut| cut.fault) {
            Some(Fault::DelayedAck(delay)) => delay,
            _ => std::time::Duration::ZERO,
        };
        let renewal_loss = cut.is_some_and(|cut| {
            cut.point.label == CommitLabel::HEARTBEAT
                && settings.heartbeat_every + delay >= settings.self_stop_after
        });
        let failover =
            renewal_loss || cut.is_some_and(|cut| cut.fault.pauses() || cut.fault.kills());
        // A lost renewal stops serving before expiry. The next owner must
        // wait for that lease, its reap and its takeover; healthy owners
        // retain the O1 mail-poll bound.
        let bound = 3 * settings.claim_poll
            + delay
            + if failover {
                settings.ttl + settings.reap_every
            } else {
                std::time::Duration::ZERO
            };
        let bound_ms = bound.as_millis() as u64;
        let sent = self.ledger.sent.lock_recover().clone();
        let seen = *self.ledger.seen_ms.lock_recover();
        match (sent, seen) {
            (Some((_, sent_ms)), Some(seen_ms))
                if seen_ms >= sent_ms && seen_ms - sent_ms <= bound_ms => {}
            (sent, seen) => violations.push(format!(
                "{}: the second input (sent {sent:?}) was seen at {seen:?}, not within \
                 {bound_ms} ms",
                if failover { "failover recovery" } else { "O1" },
            )),
        }
        let replayed = self.tripwire.counts();
        if replayed != lash_durable_test::TripwireCounts::default() {
            violations.push(format!("Tripwire: draining mail re-ran work: {replayed:?}"));
        }
        violations
    }
}

impl SessionMail {
    /// One unfinished run: the first input is bound to the one run every
    /// admission that landed names, and the second input stays open.
    async fn one_unfinished_run(
        &self,
        shared: &Shared,
        committed: &[Committed],
        violations: &mut Vec<String>,
    ) {
        let admitted: Vec<&TurnId> = committed
            .iter()
            .filter_map(|commit| commit.admitted.as_ref())
            .collect();
        if admitted.len() > 1 {
            violations.push(format!(
                "one unfinished run: {} admissions landed: {admitted:?}",
                admitted.len()
            ));
        }
        let mailbox = match shared.backend.durable().session_mailbox(&session()).await {
            Ok(mailbox) => mailbox,
            Err(error) => {
                violations.push(format!("the mailbox does not read: {error}"));
                return;
            }
        };
        let Some(run) = mailbox.bound_run.clone() else {
            violations.push("one unfinished run: no run is bound".into());
            return;
        };
        if admitted.iter().any(|landed| **landed != run) {
            violations.push(format!(
                "one unfinished run: an admission landed for a run other than the bound {run}: \
                 {admitted:?}"
            ));
        }
        let statuses = shared
            .backend
            .stores()
            .session_store_factory()
            .list_pending_turn_inputs(&session())
            .await
            .map(|reads| {
                reads
                    .into_iter()
                    .map(|read| read.status)
                    .collect::<Vec<_>>()
            });
        let expected = vec![
            PendingTurnInputReadStatus::Admitted { run: run.clone() },
            PendingTurnInputReadStatus::Open,
        ];
        match statuses {
            Ok(statuses) if statuses == expected => {}
            other => violations.push(format!(
                "one unfinished run: the inputs read {other:?}, not the first bound to {run} \
                 and the second open"
            )),
        }
    }
}

/// F1 over the commits owners saw succeed: none goes back in epoch as the
/// state revision advances, and no revision is committed twice.
fn no_stale_write(committed: &[Committed], violations: &mut Vec<String>) {
    let mut commits: Vec<(StateRevision, Epoch)> = committed
        .iter()
        .map(|commit| (commit.revision, commit.epoch))
        .collect();
    commits.sort();
    for pair in commits.windows(2) {
        let [(revision, epoch), (next_revision, next_epoch)] = pair else {
            continue;
        };
        if revision == next_revision {
            violations.push(format!(
                "O1: revision {revision:?} committed under epochs {epoch} and {next_epoch}"
            ));
        } else if next_epoch < epoch {
            violations.push(format!(
                "O1: revision {next_revision:?} committed under stale epoch {next_epoch} after \
                 epoch {epoch}"
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIG-7132: the owner may observe committed mail before enqueue
    /// returns to the producer. Its sole woken pass must still record it.
    #[tokio::test]
    async fn committed_mail_is_observed_before_the_producer_records_its_receipt() {
        let scenario = SessionMail::default();
        let clock = SimClock::new();
        scenario.database(Arc::clone(&clock)).await;
        let shared = scenario.shared.get().unwrap();
        let stores = shared.backend.stores();
        stores
            .session_store_factory()
            .admit_session(&lash_core::testing::store_fixtures::root_session_request(
                &session(),
            ))
            .await
            .unwrap();
        let second = second_id();
        let input = stores
            .session_store_factory()
            .enqueue_pending_turn_input(
                PendingTurnInputDraft::new(
                    session(),
                    TurnInputIngress::NextTurn,
                    TurnInput::text("second"),
                )
                .with_input_id(second.clone()),
            )
            .await
            .unwrap();
        eprintln!("mail-order: second committed; producer receipt not recorded");
        let drain = SessionDrain {
            shared: Arc::clone(&scenario.shared),
            ledger: Arc::clone(&scenario.ledger),
            tripwire: Arc::clone(&scenario.tripwire),
        };
        drain.observe(shared).await;
        eprintln!(
            "mail-order: owner observed on its woken pass; seen={:?}",
            *scenario.ledger.seen_ms.lock_recover()
        );
        *scenario.ledger.sent.lock_recover() = Some((input.input_id, clock.logical_ms()));
        eprintln!("mail-order: producer receipt recorded");
        assert_eq!(*scenario.ledger.seen_ms.lock_recover(), Some(0));
    }

    /// One unfinished run, O1 and the Tripwire hold on SQLite in memory with
    /// every labelled write of the scenario cut under every fault that
    /// applies to it.
    #[tokio::test]
    async fn the_session_mail_laws_hold_at_every_cut_on_sqlite_memory() {
        let matrix = lash_durable_test::Matrix::new();
        let report = if std::env::var("LASH_MATRIX_VERIFY_LEASE").as_deref() == Ok("1") {
            matrix.run_lease_equivalence(SessionMail::default).await
        } else {
            matrix.run(SessionMail::default).await
        };
        report.assert_held();
        for label in [CommitLabel::TURN_ADMIT, ACK, RELEASE, CommitLabel::CLAIM] {
            assert!(
                report.labels().contains(&label),
                "the matrix never cut `{label}`"
            );
        }
    }
}
