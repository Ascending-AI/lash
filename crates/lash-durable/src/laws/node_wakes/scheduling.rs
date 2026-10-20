//! The scheduling laws (FIG-7044): a node claims when committed work is
//! due, from the deadlines and hints its commits hand it, and a quiet node
//! opens no claim write. Its reconciliation still finds work whose hint
//! never came, within one claim poll.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::mpsc;

use super::super::{LawBroken, LawOutcome, ensure};
use super::NodeWakeTier;
use super::harness::{Claims, CountingStore, actor, create, eventually, formats, owner_of, under};
use crate::runner::{Activation, Exit, Owned, Runner, RunnerConfig, Wake};
use crate::{
    ActorKey, CommitLabel, DurableInstant, DurableSettings, DurableStore, LeaseSettings, NodeId,
    NodeWakes, Release,
};

/// What a scheduling law's activation saw.
enum Seen {
    /// A timer's claim ran the actor: what woke the claim, and whether the
    /// store's time had not yet reached the due time it waited on.
    Fired { claimed_on: Wake, early: bool },
    /// An actor finished its chain.
    Finished,
    /// A claim that ran no timer: what woke it.
    Claimed(ActorKey, Wake),
    /// The activation could not do its part.
    Broken(String),
}

/// Runs each actor through a chain of `timers` waits of `step` each: every
/// activation acknowledges, then releases its actor `waiting` until `step`
/// past its open, until the chain is done.
struct Timers {
    timers: usize,
    step: Duration,
    /// Per actor, the timers fired so far and the due time it waits on.
    chains: Mutex<HashMap<ActorKey, (usize, Option<DurableInstant>)>>,
    seen: mpsc::UnboundedSender<Seen>,
}

#[async_trait::async_trait]
impl Activation for Timers {
    async fn activate(&self, owned: Owned) -> Exit {
        let Ok(mut tx) = owned.begin().await else {
            let _ = self
                .seen
                .send(Seen::Broken("an owner open failed".to_owned()));
            return Exit::Released;
        };
        let opened = tx.opened_at();
        let (fired, due) = {
            let mut chains = self.chains.lock().unwrap_or_else(PoisonError::into_inner);
            let (fired, due) = chains.entry(owned.actor().clone()).or_default();
            let seen = (*fired, *due);
            *fired += 1;
            seen
        };
        if let Some(due) = due {
            let _ = self.seen.send(Seen::Fired {
                claimed_on: owned.claimed_on(),
                early: opened < due,
            });
        }
        tx.ack_seen();
        let release = if fired < self.timers {
            let due = opened.after_millis(i64::try_from(self.step.as_millis()).unwrap_or(i64::MAX));
            self.chains
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(owned.actor().clone())
                .or_default()
                .1 = Some(due);
            Release::Waiting {
                next_due: Some(due),
            }
        } else {
            Release::Idle
        };
        tx.give_up(release);
        if let Err(error) = owned.commit(tx, CommitLabel::new("law.timer")).await {
            let _ = self
                .seen
                .send(Seen::Broken(format!("a timer release failed: {error}")));
            return Exit::Released;
        }
        if release == Release::Idle {
            let _ = self.seen.send(Seen::Finished);
        }
        Exit::Released
    }
}

/// Reports each claim that ran an actor, and what woke it, holding the
/// actor until the node stops.
struct Report(mpsc::UnboundedSender<Seen>);

#[async_trait::async_trait]
impl Activation for Report {
    async fn activate(&self, owned: Owned) -> Exit {
        let _ = self
            .0
            .send(Seen::Claimed(owned.actor().clone(), owned.claimed_on()));
        std::future::pending().await
    }
}

/// A node serving `activation` over `store`; dropping it stops the node as
/// a crash would.
struct Serving(tokio::task::JoinHandle<()>);

impl Serving {
    #[expect(clippy::expect_used, reason = "a law's own settings validate")]
    fn start(
        store: Arc<dyn DurableStore>,
        node_wakes: Option<Arc<dyn NodeWakes>>,
        name: &str,
        settings: DurableSettings,
        activation: Arc<dyn Activation>,
    ) -> Self {
        let config = settings.validate().expect("the law's settings validate");
        let mut runner = Runner::new(
            store,
            Arc::new(lash_core_ids::clock::SystemClock),
            RunnerConfig::new(NodeId::new(name), vec![formats()], &config),
            activation,
        );
        if let Some(node_wakes) = node_wakes {
            runner = runner.with_node_wakes(node_wakes);
        }
        Self(tokio::spawn(async move {
            let _ = runner.run(std::future::pending()).await;
        }))
    }

    async fn kill(self) {
        self.0.abort();
        let _ = self.0.await;
    }
}

/// Five actors each run a chain of ten timers on one node. Each timer's
/// release hands the node its committed due time, so the node claims each
/// actor when its timer is due: the five first claims and the fifty timer
/// claims take work, at most five claims come back empty, and at most five
/// timers are first found by the claim poll rather than by their deadlines.
/// No timer runs before the store's time reaches its due time. Before,
/// every claim that took work was followed by empty claims backing off to
/// the poll, and every timer was found by one of them.
pub async fn timer_chains_are_claimed_when_due_without_empty_claims(
    tier: &dyn NodeWakeTier,
) -> LawOutcome {
    const CHAINS: usize = 5;
    const TIMERS: usize = 10;
    let (store, node_wakes) = tier.open().await;
    for chain in 0..CHAINS {
        create(store.as_ref(), &actor(&format!("chain-{chain}"))?).await?;
    }
    let claims = Claims::default();
    let (seen, mut reports) = mpsc::unbounded_channel();
    let node = Serving::start(
        Arc::new(CountingStore {
            inner: store,
            node: "timers".to_owned(),
            claims: claims.clone(),
        }),
        Some(node_wakes),
        "timers",
        under(LeaseSettings::default()),
        Arc::new(Timers {
            timers: TIMERS,
            step: Duration::from_millis(40),
            chains: Mutex::new(HashMap::new()),
            seen,
        }),
    );
    let (mut finished, mut fired, mut polled, mut early) = (0, 0, 0, 0);
    while finished < CHAINS {
        match reports.recv().await {
            Some(Seen::Fired {
                claimed_on,
                early: before,
            }) => {
                fired += 1;
                polled += usize::from(claimed_on == Wake::Poll);
                early += usize::from(before);
            }
            Some(Seen::Finished) => finished += 1,
            Some(Seen::Claimed(..)) => {}
            Some(Seen::Broken(why)) => return Err(LawBroken(why)),
            None => return Err(LawBroken("the timer node stopped".to_owned())),
        }
    }
    node.kill().await;
    let attempts = claims.take();
    let useful = attempts.iter().filter(|(_, taken)| *taken > 0).count();
    let empty = attempts.len() - useful;
    eprintln!(
        "{CHAINS} chains of {TIMERS} timers: {fired} fired, {useful} useful claims, \
         {empty} empty, {polled} timers first found by the claim poll, {early} early"
    );
    ensure!(
        fired == CHAINS * TIMERS && early == 0,
        "{fired} timers fired, {early} before their due time"
    );
    ensure!(
        useful <= CHAINS * (TIMERS + 1) && empty <= CHAINS && polled <= CHAINS,
        "{useful} useful claims, {empty} empty, {polled} timers found by the poll: {attempts:?}"
    );
    Ok(())
}

/// A healthy node with nothing to claim reconciles at its claim poll
/// without opening a claim: at most five claims in five seconds, all of
/// them empty ones only a startup race may cause. Work committed with no
/// hint the node can hear (it listens to no node wakes) is still claimed by
/// its reconciliation, as the claim poll found it before.
pub async fn a_quiet_node_claims_nothing_and_still_finds_unhinted_work(
    tier: &dyn NodeWakeTier,
) -> LawOutcome {
    const WINDOW: Duration = Duration::from_secs(5);
    let (store, _) = tier.open().await;
    let (inner, _) = tier.open().await;
    let claims = Claims::default();
    let (seen, mut reports) = mpsc::unbounded_channel();
    let node = Serving::start(
        Arc::new(CountingStore {
            inner,
            node: "quiet".to_owned(),
            claims: claims.clone(),
        }),
        None,
        "quiet",
        under(LeaseSettings::default()),
        Arc::new(Report(seen)),
    );
    tokio::time::sleep(WINDOW).await;
    let quiet = claims.take();
    eprintln!("a quiet node in {WINDOW:?}: {} claims", quiet.len());
    ensure!(
        quiet.len() <= 5 && quiet.iter().all(|(_, taken)| *taken == 0),
        "a quiet node claimed {} times in {WINDOW:?}: {quiet:?}",
        quiet.len()
    );
    let unhinted = actor("unhinted")?;
    create(store.as_ref(), &unhinted).await?;
    let claimed = match reports.recv().await {
        Some(Seen::Claimed(claimed, woke)) => (claimed, woke),
        _ => return Err(LawBroken("the quiet node stopped".to_owned())),
    };
    ensure!(
        claimed == (unhinted.clone(), Wake::Poll),
        "the reconciliation did not claim the unhinted actor: {claimed:?}"
    );
    eventually(|| async {
        Ok(owner_of(store.as_ref(), &unhinted).await?.as_deref() == Some("quiet"))
    })
    .await?;
    node.kill().await;
    Ok(())
}
