//! The session config-change laws (FIG-5333): a config transaction a host
//! commands on a durable session reaches the plugins that observe session
//! config changes, once, after its commit; and it still applies when the
//! session's plugins cannot build.
//!
//! Each law runs a lash core serving its own node over one tier's database.
//! The host applies a transaction through the session's config admin; the
//! core's node applies it in a command run, and a later `send()` runs a turn
//! after it in the same session actor, so the command run, delivery
//! included, has finished once the turn answers.
#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use lash::config::{ConfigTransaction, ConfigTransactionOutcome, ConfigWrite, SetMaxToolCalls};
use lash_sansio::sync::MutexExt as _;
use served::{Tier, World};

/// The observing plugin's id.
const PLUGIN: &str = "config-change-observer";
/// The tool-call limit a session is created with.
const BEFORE: usize = 8;
/// The tool-call limit the transaction sets.
const AFTER: usize = 16;

/// One `SessionConfigChanged` the plugin saw: the tool-call limits of its
/// previous and current policy, and the one the session's read service
/// answered while the observer ran.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Seen {
    session: String,
    previous: usize,
    current: usize,
    served: usize,
}

/// A plugin that records every session config change it observes. While
/// `failing` is set a session it is part of does not build.
#[derive(Clone, Default)]
struct Observer {
    seen: Arc<Mutex<Vec<Seen>>>,
    failing: Arc<AtomicBool>,
}

impl Observer {
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock_recover().clone()
    }
}

impl lash::plugins::PluginDefinition for Observer {
    fn declaration() -> lash::plugins::PluginDeclaration {
        lash::plugins::PluginDeclaration::initial(PLUGIN)
    }
}

impl lash::plugins::PluginFactory for Observer {
    fn id(&self) -> &'static str {
        PLUGIN
    }

    fn build(
        &self,
        _: &lash::plugins::PluginSessionContext,
    ) -> Result<Arc<dyn lash::plugins::SessionPlugin>, lash::plugins::PluginError> {
        Ok(Arc::new(self.clone()))
    }
}

impl lash::plugins::SessionPlugin for Observer {
    fn id(&self) -> &'static str {
        PLUGIN
    }

    /// A session's build ends here; inspecting the plugin's registrations,
    /// which a config transaction's resolution does, never reaches it.
    fn session_ready(
        &self,
        _: lash::plugins::SessionReadyContext,
    ) -> Result<(), lash::plugins::PluginError> {
        if self.failing.load(Ordering::SeqCst) {
            return Err(lash::plugins::PluginError::Registration(
                "the observer's session does not build".to_owned(),
            ));
        }
        Ok(())
    }

    fn register(
        &self,
        reg: &mut lash::plugins::PluginRegistrar,
    ) -> Result<(), lash::plugins::PluginError> {
        let seen = Arc::clone(&self.seen);
        reg.session().on_event(
            lash::hook_key!("observe"),
            Arc::new(move |event| {
                let seen = Arc::clone(&seen);
                Box::pin(async move {
                    if let lash::plugins::PluginLifecycleEvent::SessionConfigChanged(ctx) = event {
                        let snapshot = ctx.sessions.snapshot_current().await?;
                        seen.lock_recover().push(Seen {
                            session: ctx.session_id.to_string(),
                            previous: ctx.previous.max_tool_calls.get(),
                            current: ctx.current.max_tool_calls.get(),
                            served: snapshot.policy.max_tool_calls.get(),
                        });
                    }
                    Ok(())
                })
            }),
        )
    }
}

/// The live session `name` on `world`'s core, whose admin applies config.
async fn open(world: &World, name: &str) -> lash::LashSession {
    world
        .core
        .session(lash::SessionId::try_from(name.to_owned()).unwrap())
        .open()
        .await
        .expect("the session opens")
}

/// A core over `tier` with `observer` installed.
async fn world(tier: Tier, observer: &Observer) -> Option<World> {
    let factory: Arc<dyn lash::plugins::PluginFactory> = Arc::new(observer.clone());
    World::new(tier, |backend| {
        lash::LashCore::standard_builder(backend.clone()).plugin(factory)
    })
    .await
}

/// Apply a transaction setting the tool-call limit to `limit` on `session`,
/// written against `revision`.
async fn set_limit(
    session: &lash::LashSession,
    id: &str,
    revision: u64,
    limit: usize,
) -> ConfigTransactionOutcome {
    session
        .admin()
        .config()
        .apply(
            ConfigWrite::new(id, revision),
            ConfigTransaction::of(SetMaxToolCalls {
                max_tool_calls: lash::MaxToolCalls::new(limit),
            }),
        )
        .await
        .expect("the transaction settles")
}

async fn revision(session: &lash::LashSession) -> u64 {
    session
        .admin()
        .config()
        .revision()
        .await
        .expect("read the config revision")
}

/// A committed config transaction reaches the session's config-change
/// observers once, after its commit: they see the previous and the current
/// policy, and the session they read already serves the new one. A later
/// turn does not deliver it again, and a transaction that does not apply
/// delivers nothing (FIG-5333).
async fn a_committed_config_transaction_reaches_its_observers_once(tier: Tier) {
    let observer = Observer::default();
    let Some(world) = world(tier, &observer).await else {
        return;
    };
    let name = "config-change-observed";
    let durable = world.session(name, served::spec(BEFORE)).await;
    world.send(&durable, "before the change").await;
    let session = open(&world, name).await;
    assert_eq!(observer.seen(), Vec::new(), "no config changed yet");

    let base = revision(&session).await;
    let outcome = set_limit(&session, "raise-the-limit", base, AFTER).await;
    assert!(
        matches!(outcome, ConfigTransactionOutcome::Applied { .. }),
        "{outcome:?}"
    );
    // The session actor runs this turn after the command run that applied
    // the transaction: its answer means the command run has ended.
    world.send(&durable, "after the change").await;
    let once = vec![Seen {
        session: name.to_owned(),
        previous: BEFORE,
        current: AFTER,
        served: AFTER,
    }];
    assert_eq!(
        observer.seen(),
        once,
        "one delivery of the committed change"
    );

    let stale = set_limit(&session, "stale-write", base, BEFORE).await;
    assert!(
        matches!(stale, ConfigTransactionOutcome::Stale { .. }),
        "{stale:?}"
    );
    world.send(&durable, "after the stale write").await;
    assert_eq!(
        observer.seen(),
        once,
        "neither a later turn nor a transaction that did not apply delivers"
    );
    assert_eq!(session.policy_snapshot().max_tool_calls.get(), AFTER);
    world.shutdown().await;
}

/// A config transaction applies to a session whose plugins cannot build:
/// it answers `Applied` and the session records the new policy; its
/// observers, never built, see nothing (FIG-5245, FIG-5333).
async fn a_config_transaction_applies_when_the_sessions_plugins_cannot_build(tier: Tier) {
    let observer = Observer::default();
    let Some(world) = world(tier, &observer).await else {
        return;
    };
    let name = "config-change-unbuilt";
    world.session(name, served::spec(BEFORE)).await;
    let session = open(&world, name).await;
    let base = revision(&session).await;
    observer.failing.store(true, Ordering::SeqCst);
    let outcome = set_limit(&session, "raise-unbuilt", base, AFTER).await;
    assert!(
        matches!(outcome, ConfigTransactionOutcome::Applied { .. }),
        "{outcome:?}"
    );
    assert_eq!(session.policy_snapshot().max_tool_calls.get(), AFTER);
    assert_eq!(revision(&session).await, base + 1);
    assert_eq!(observer.seen(), Vec::new());
    world.shutdown().await;
}

tiered_laws!(
    a_committed_config_transaction_reaches_its_observers_once,
    a_config_transaction_applies_when_the_sessions_plugins_cannot_build,
);
