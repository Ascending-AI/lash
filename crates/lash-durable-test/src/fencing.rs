//! The fencing matrix (design-opus §7.3, laws F1 and O1): two nodes serve
//! two actors while a producer on one of them mails both; every labelled
//! write is cut under every fault.
//!
//! - **F1, fencing:** no owner write ever commits under a stale epoch, a
//!   zombie's paused commit is refused once it resumes, a stale owner's
//!   later commits are refused, and a node paused past its lease stops itself.
//! - **O1, owner cache:** mail reaches its actor's owner within the hint or
//!   the poll, whether the actor is hot or idle and whether the wake is
//!   lost, and an owner that lost its actor keeps nothing of it.
//!
//! The scenario takes its database from the caller, so the same matrix runs
//! on every store that implements the port.

use crate::clock::SimClock;
use crate::matrix::Scenario;
use crate::nodes::{SimNodes, SimNodesConfig};
use crate::script::{Cut, Fault, Stored, WriteKind};
use lash_durable::runner::{Activation, Exit, Owned};
use lash_durable::{
    ActorKey, ActorState, CommitLabel, DurableError, DurableStore, Epoch, FormatSet, LeaseConfig,
    MailKind, MailSeq, MailTx, Release, StateRevision,
};
use lash_sansio::sync::MutexExt as _;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Builds a fresh, empty database whose clock is the given one.
pub type Database = Arc<
    dyn Fn(Arc<SimClock>) -> Pin<Box<dyn Future<Output = Arc<dyn DurableStore>> + Send>>
        + Send
        + Sync,
>;

pub const CREATE: CommitLabel = CommitLabel::new("fencing.create");
pub const MAIL: CommitLabel = CommitLabel::new("fencing.mail");
pub const ACK: CommitLabel = CommitLabel::new("fencing.ack");
pub const RELEASE: CommitLabel = CommitLabel::new("fencing.release");

const FORMATS: &str = "fencing-v1";
const PRODUCER: &str = "b";
const NODES: [&str; 2] = ["a", "b"];
const ROUNDS: usize = 3;
const ROUND_EVERY: Duration = Duration::from_secs(2);
/// How long an owner keeps a drained actor hot before releasing it.
const HOT_FOR: Duration = Duration::from_secs(3);

/// One mail an owner consumed with a commit it saw succeed.
#[derive(Clone, Debug)]
struct Consumed {
    actor: ActorKey,
    seq: MailSeq,
    appended_ms: i64,
    epoch: Epoch,
    revision: StateRevision,
    at_ms: i64,
}

#[derive(Default)]
struct Ledger {
    consumed: Mutex<Vec<Consumed>>,
    producer_done: Mutex<bool>,
}

/// Drains each claimed actor's mail, keeps it hot for a while, then
/// releases it; drops it at once on a fence refusal.
struct Drain {
    ledger: Arc<Ledger>,
}

#[async_trait::async_trait]
impl Activation for Drain {
    async fn activate(&self, owned: Owned) -> Exit {
        let clock = Arc::clone(owned.clock());
        let mut hot_until = clock.now() + HOT_FOR;
        loop {
            let mut tx = match owned.begin().await {
                Ok(tx) => tx,
                Err(DurableError::OwnershipLost(_)) => return Exit::Released,
                Err(_) => {
                    owned.wait_for_mail().await;
                    continue;
                }
            };
            if tx.mail().is_empty() {
                if clock.now() < hot_until {
                    owned.wait_for_mail().await;
                    continue;
                }
                tx.ack_seen().give_up(Release::Idle);
                match owned.commit(tx, RELEASE).await {
                    Ok(_) | Err(DurableError::OwnershipLost(_)) => return Exit::Released,
                    Err(_) => continue,
                }
            }
            let mail = tx.mail().to_vec();
            tx.ack_seen();
            match owned.commit(tx, ACK).await {
                Ok(commit) => {
                    let at_ms = SimClock::timestamp_ms_at(sim_ms(&clock)) as i64;
                    self.ledger
                        .consumed
                        .lock_recover()
                        .extend(mail.iter().map(|mail| Consumed {
                            actor: owned.actor().clone(),
                            seq: mail.seq,
                            appended_ms: mail.appended_at.0,
                            epoch: owned.epoch(),
                            revision: commit.revision,
                            at_ms,
                        }));
                    hot_until = clock.now() + HOT_FOR;
                }
                Err(DurableError::OwnershipLost(_)) => return Exit::Released,
                Err(_) => {}
            }
        }
    }
}

/// Virtual milliseconds on a node's clock.
fn sim_ms(clock: &Arc<dyn lash_core_ids::clock::Clock>) -> u64 {
    let wall = clock.timestamp_datetime().timestamp_millis();
    u64::try_from(wall).unwrap_or_default() - SimClock::timestamp_ms_at(0)
}

/// The fencing scenario over one database.
pub struct Fencing {
    database: Database,
    ledger: Arc<Ledger>,
}

impl Fencing {
    pub fn new(database: Database) -> Self {
        Self {
            database,
            ledger: Arc::default(),
        }
    }
}

fn actors() -> Vec<ActorKey> {
    ["one", "two"]
        .into_iter()
        .filter_map(|id| ActorKey::session(id).ok())
        .collect()
}

#[async_trait::async_trait]
impl Scenario for Fencing {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        (self.database)(clock).await
    }

    fn config(&self) -> SimNodesConfig {
        SimNodesConfig {
            lease: LeaseConfig::default(),
            decodes: vec![FormatSet::new(FORMATS)],
            max_active: 8,
        }
    }

    fn activation(&self) -> Arc<dyn Activation> {
        Arc::new(Drain {
            ledger: Arc::clone(&self.ledger),
        })
    }

    async fn start(&self, nodes: &Arc<SimNodes>) -> Result<(), String> {
        for node in NODES {
            nodes.start(node);
        }
        let nodes = Arc::clone(nodes);
        let ledger = Arc::clone(&self.ledger);
        tokio::spawn(async move {
            produce(&nodes).await;
            *ledger.producer_done.lock_recover() = true;
        });
        Ok(())
    }

    fn actors(&self) -> Vec<ActorKey> {
        actors()
    }

    async fn done(&self, nodes: &SimNodes) -> bool {
        let producing =
            !*self.ledger.producer_done.lock_recover() && nodes.life(PRODUCER) != crate::Life::Dead;
        if producing {
            return false;
        }
        for actor in actors() {
            match nodes.database().actor(&actor).await {
                Ok(None) => {}
                Ok(Some(snapshot))
                    if snapshot.pending_mail == 0 && snapshot.state == ActorState::Idle => {}
                _ => return false,
            }
        }
        true
    }

    async fn check(&self, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let mut violations = Vec::new();
        let consumed = self.ledger.consumed.lock_recover().clone();
        no_stale_write(&consumed, &mut violations);
        let writes = nodes.script().trace();
        if let Some(cut) = cut {
            let cut_write = writes.iter().find(|write| write.cut.is_some());
            match (cut.fault, cut.kind, cut_write) {
                (Fault::Zombie, WriteKind::Actor, Some(write)) => {
                    if !matches!(
                        write.stored,
                        Stored::Refused(DurableError::OwnershipLost(_))
                    ) {
                        violations.push(format!(
                            "F1: the zombie's paused commit {write} was not refused for ownership"
                        ));
                    }
                }
                (Fault::StaleEpoch, WriteKind::Actor, Some(write)) => {
                    let later = writes
                        .iter()
                        .skip_while(|other| other.cut.is_none())
                        .skip(1)
                        .filter(|other| {
                            other.node == write.node
                                && other.kind == WriteKind::Actor
                                && other.actor == write.actor
                        });
                    for other in later {
                        if !matches!(
                            other.stored,
                            Stored::Refused(DurableError::OwnershipLost(_))
                        ) {
                            violations.push(format!(
                                "F1: the stale owner's later commit {other} was not refused"
                            ));
                        }
                    }
                }
                _ => {}
            }
            // A node paused before it registered held no lease to lose. Any
            // other was paused past `self_stop_after`, so it stops itself
            // unrenewed as soon as it resumes, before it can see its reap.
            if cut.fault.pauses() && cut.point.label != CommitLabel::NODE_REGISTER {
                match nodes.stopped(&cut.node).await {
                    Some(Ok(lash_durable::runner::Stopped::Unrenewed)) => {}
                    other => violations.push(format!(
                        "F1: paused node {} did not stop itself unrenewed on resume ({other:?})",
                        cut.node
                    )),
                }
            }
        }
        let undisturbed = cut.is_none_or(|cut| !cut.fault.pauses() && !cut.fault.kills());
        if undisturbed {
            let delay = match cut.map(|cut| cut.fault) {
                Some(Fault::DelayedAck(delay)) => delay,
                _ => Duration::ZERO,
            };
            let poll = self.config().lease.settings().claim_poll;
            let bound = (3 * poll + delay).as_millis() as i64;
            for mail in &consumed {
                if mail.at_ms - mail.appended_ms > bound {
                    violations.push(format!(
                        "O1: mail {:?} of {} waited {} ms for its owner (bound {bound} ms)",
                        mail.seq,
                        mail.actor,
                        mail.at_ms - mail.appended_ms
                    ));
                }
            }
        }
        violations
    }
}

/// F1 in the ledger: per actor, the commits owners saw succeed never go
/// back in epoch as the state revision advances, and no revision is
/// committed twice.
fn no_stale_write(consumed: &[Consumed], violations: &mut Vec<String>) {
    for actor in actors() {
        let mut commits: Vec<(StateRevision, Epoch)> = consumed
            .iter()
            .filter(|mail| mail.actor == actor)
            .map(|mail| (mail.revision, mail.epoch))
            .collect();
        commits.sort();
        commits.dedup();
        for pair in commits.windows(2) {
            let [(revision, epoch), (next_revision, next_epoch)] = pair else {
                continue;
            };
            if revision == next_revision {
                violations.push(format!(
                    "F1: {actor} revision {revision:?} committed under epochs {epoch} and {next_epoch}"
                ));
            } else if next_epoch < epoch {
                violations.push(format!(
                    "F1: {actor} revision {next_revision:?} committed under stale epoch {next_epoch} after epoch {epoch}"
                ));
            }
        }
        let mut seqs: Vec<MailSeq> = consumed
            .iter()
            .filter(|mail| mail.actor == actor)
            .map(|mail| mail.seq)
            .collect();
        let all = seqs.len();
        seqs.sort();
        seqs.dedup();
        if seqs.len() != all {
            violations.push(format!("F1: {actor} consumed some mail twice"));
        }
    }
}

/// The producer on node `b`: create both actors, then mail each of them
/// every two seconds. It sends each write once; a write whose answer is
/// lost is not retried, so no mail is ever appended twice.
async fn produce(nodes: &SimNodes) {
    let mut create = MailTx::new();
    for actor in actors() {
        create.create_actor(actor, FormatSet::new(FORMATS));
    }
    if nodes.mail(PRODUCER, create, CREATE).await.is_err() {
        return;
    }
    for round in 0..ROUNDS {
        lash_core_ids::clock::Clock::sleep(&**nodes.clock(), ROUND_EVERY).await;
        let mut mail = MailTx::new();
        for actor in actors() {
            mail.append(actor, MailKind::new("tick"), round.to_string());
        }
        let _ = nodes.mail(PRODUCER, mail, MAIL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix::Matrix;
    use crate::testing::{sqlite, sqlite_file};

    /// F1 and O1 hold on SQLite in memory with every labelled write of the
    /// scenario cut under every fault that applies to it.
    #[tokio::test]
    async fn the_fencing_laws_hold_at_every_cut_on_sqlite_memory() {
        let report = Matrix::new()
            .run(|| Fencing::new(Arc::new(|clock| Box::pin(sqlite(clock)))))
            .await;
        assert_every_label_held(&report);
    }

    /// The same matrix on a SQLite file store set.
    #[tokio::test]
    async fn the_fencing_laws_hold_at_every_cut_on_sqlite_file() {
        let dirs = Arc::new(Mutex::new(Vec::new()));
        let database: Database = Arc::new(move |clock| {
            let dirs = Arc::clone(&dirs);
            Box::pin(async move { sqlite_file(clock, &dirs).await })
        });
        let report = Matrix::new()
            .run(|| Fencing::new(Arc::clone(&database)))
            .await;
        assert_every_label_held(&report);
    }

    fn assert_every_label_held(report: &crate::MatrixReport) {
        report.assert_held();
        for label in [
            CREATE,
            MAIL,
            ACK,
            RELEASE,
            CommitLabel::CLAIM,
            CommitLabel::HEARTBEAT,
            CommitLabel::NODE_REGISTER,
        ] {
            assert!(
                report.labels().contains(&label),
                "the matrix never cut `{label}`"
            );
        }
    }
}
