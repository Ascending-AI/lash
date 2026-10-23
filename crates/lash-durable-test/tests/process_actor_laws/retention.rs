//! LO-EVICT includes quiet process owners in the strict resident cap and LRU.
use super::*;
use lash_durable::runner::{Activation, Exit, Owned};
use std::collections::BTreeMap;
use std::sync::{
    Weak,
    atomic::{AtomicUsize, Ordering},
};

type Claims = Arc<Mutex<BTreeMap<String, (lash_durable::Epoch, Weak<()>)>>>;
struct Observed {
    inner: Arc<ProcessActivation>,
    claims: Claims,
    live: Arc<AtomicUsize>,
}
struct Resident(Arc<AtomicUsize>);
impl Drop for Resident {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
#[async_trait::async_trait]
impl Activation for Observed {
    async fn capabilities(
        &self,
    ) -> Result<lash_durable::NodeCapabilities, lash_durable::DurableError> {
        self.inner.capabilities().await
    }
    async fn activate(&self, owned: Owned) -> Exit {
        let _resident = Resident(self.live.clone());
        assert!(
            self.live.fetch_add(1, Ordering::SeqCst) < 2,
            "process resident cap exceeded"
        );
        let lifetime = Arc::new(());
        self.claims.lock_recover().insert(
            owned.actor().id().to_owned(),
            (owned.epoch(), Arc::downgrade(&lifetime)),
        );
        let exit = self.inner.activate(owned).await;
        drop(lifetime);
        exit
    }
}
fn claim(claims: &Claims, process: &ProcessId) -> (lash_durable::Epoch, Weak<()>) {
    claims.lock_recover().get(process.as_str()).unwrap().clone()
}

#[tokio::test]
async fn lo_evict_process_owners_on_sqlite_memory() {
    let claims = Claims::default();
    let live = Arc::new(AtomicUsize::new(0));
    let observed = claims.clone();
    let running = live.clone();
    let world = World::retained(
        Script::new(),
        lash_durable::OwnerRetentionSettings {
            idle: Duration::from_secs(60),
            max_resident_owners: std::num::NonZeroUsize::new(2).unwrap(),
        },
        move |inner| {
            Arc::new(Observed {
                inner,
                claims: observed,
                live: running,
            })
        },
    )
    .await;
    let register = |tag| {
        registration(
            json!({"tag": tag, "act": "sleep", "until_ms": SimClock::timestamp_ms_at(3_600_000)}),
        )
    };
    let a = world.register(register("a")).await;
    world.nodes.start("owner");
    let _wakes = world
        .backend
        .stores()
        .wake_sink()
        .attach(&world.nodes.hints("owner").unwrap());
    world
        .until("the first process is resident", || async {
            world.actor_state(&a).await == Some(ActorState::Owned)
        })
        .await;
    let (epoch, old) = claim(&claims, &a);
    let b = world.register(register("b")).await;
    world
        .until("the second process is resident", || async {
            world.actor_state(&b).await == Some(ActorState::Owned)
        })
        .await;
    let (_, held_b) = claim(&claims, &b);
    let c = world.register(register("c")).await;
    world
        .until("the third process is resident", || async {
            world.actor_state(&c).await == Some(ActorState::Owned)
        })
        .await;
    assert!(
        old.upgrade().is_none(),
        "LRU process activation did not release"
    );
    assert!(held_b.upgrade().is_some(), "newer process was evicted");
    assert_eq!(live.load(Ordering::SeqCst), 2);
    assert_eq!(world.actor_state(&a).await, Some(ActorState::Waiting));
    // A wake reclaims the same durable continuation, without starting it again.
    let mut wake = lash_durable::MailTx::new();
    wake.wake(actor(&a));
    world
        .nodes
        .producer("reclaim")
        .commit_mail(wake, CommitLabel::new("law.reclaim"))
        .await
        .unwrap();
    world
        .until("the first process is reclaimed", || async {
            world.actor_state(&a).await == Some(ActorState::Owned)
        })
        .await;
    let (next, reloaded) = claim(&claims, &a);
    assert!(next > epoch);
    assert!(reloaded.upgrade().is_some());
    assert_eq!(world.advances("a"), ["started"]);
    assert!(
        held_b.upgrade().is_none(),
        "reclaim must evict the oldest remaining process"
    );
    // The host's idle deadline releases every remaining resident as well.
    let idle_by = world.clock.logical_ms() + 60_001;
    while world.clock.logical_ms() < idle_by {
        world
            .nodes
            .step()
            .await
            .expect("a serving node has another timer");
    }
    world.nodes.quiesce().await;
    assert!(
        world.nodes.stopped("owner").await.is_none(),
        "idle release must preserve the serving lease"
    );
    assert!(reloaded.upgrade().is_none());
    assert_eq!(live.load(Ordering::SeqCst), 0);
    world.nodes.kill("owner");
    world.nodes.quiesce().await;
}
