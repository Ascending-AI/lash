//! FIG-5231, ADR 0113 §3.3: a start that is its own runtime operation — a
//! trigger delivery's start, or a local start with no causal effect — keeps
//! its staging until its registration or its abandonment decides it.
//!
//! No execution journals under such a starter, so its journal verdict never
//! says whether the start may still register. The artifact-cleanup relay
//! over the backend's own authorities must hold `Start(key)` while no record
//! holds the key and no abandonment ended it.

use pretty_assertions::assert_eq;
use std::sync::Arc;

use lash_core::runtime::artifact_cleanup::ArtifactCleanupRelay;
use lash_core::runtime::obligations::relay::relay_due;

/// A trigger delivery's start, staged, with the relay run before its
/// `trigger.start` commits: `Start(key)` keeps what it staged, and the start
/// then registers with its environment held by its record.
///
/// Red on the parent commit: the relay read the start's own operation as a
/// settled journal, fenced `Start(key)` and reclaimed the environment it
/// alone held, so the registration's adoption met a missing environment.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates each setup and transition"
)]
pub async fn a_trigger_start_keeps_its_staging_until_it_registers(backend: crate::Backend) {
    let world = StartWorld::over(&backend, "keeps-staging");
    let stores = world.stores();
    let prepared = crate::stage_process_start(&stores, world.registration().await, &[])
        .await
        .expect("stage the delivery's start");
    world.release_pin().await;

    // The relay runs before `trigger.start` commits.
    world.drain().await;
    assert_eq!(
        world.env().await,
        Some(world.bytes.clone()),
        "Start(key) holds its staged environment while the start may register"
    );

    let committed = world
        .registry
        .commit_process_registration(prepared.registration, prepared.staging.anchor())
        .await;
    let registered = prepared
        .staging
        .adopt(&stores, committed)
        .await
        .expect("the start registers");
    assert_eq!(
        registered.disposition,
        crate::ProcessRegistrationOutcome::Created
    );
    world.drain().await;
    assert_eq!(
        world.env().await,
        Some(world.bytes.clone()),
        "the registered record holds the environment"
    );
}

/// A trigger delivery's start, staged, whose `trigger.start` never commits:
/// its abandonment ends `Start(key)`, and the relay reclaims what it staged.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates each setup and transition"
)]
pub async fn an_abandoned_trigger_start_has_its_staging_reclaimed(backend: crate::Backend) {
    let world = StartWorld::over(&backend, "abandoned");
    let stores = world.stores();
    let prepared = crate::stage_process_start(&stores, world.registration().await, &[])
        .await
        .expect("stage the delivery's start");
    world.release_pin().await;
    prepared
        .staging
        .abandon(&stores)
        .await
        .expect("abandon the uncommitted start");

    world.drain().await;
    assert_eq!(
        world.env().await,
        None,
        "the abandoned start's staging is reclaimed"
    );
    let refusal = world
        .ports
        .env()
        .acquire_process_execution_env(&world.start_claim(), &world.env_ref)
        .await
        .expect_err("the abandoned Start(key) is fenced");
    assert!(
        matches!(
            &refusal,
            crate::ArtifactStoreError::ReferrerEnded { referrer }
                if *referrer == crate::ArtifactReferrer::Start(world.key.clone())
        ),
        "{refusal:?}"
    );
}

/// One trigger delivery's start over the backend under test: its key, its
/// own operation as starter, and an environment a host pin published, so
/// once the pin is released only the start holds it.
struct StartWorld {
    registry: Arc<dyn crate::ProcessRegistry>,
    ports: crate::ArtifactReferrerPorts,
    engines: crate::ProcessEngineRegistry,
    relay: ArtifactCleanupRelay,
    clock: Arc<dyn crate::Clock>,
    key: crate::StartKey,
    starter: crate::EffectJournalIdentity,
    pin: crate::ArtifactReferrer,
    env_ref: crate::ProcessExecutionEnvRef,
    bytes: Vec<u8>,
}

impl StartWorld {
    #[expect(
        clippy::expect_used,
        reason = "conformance law validates each setup and transition"
    )]
    fn over(backend: &crate::Backend, name: &str) -> Self {
        let ports = crate::ArtifactReferrerPorts::of_backend(backend);
        let engines = crate::testing::process_engine_fixture().with_artifact_ports(ports.clone());
        let key = crate::DERIVED_START_KEYS.for_trigger_delivery(
            &format!("start-operation-{name}-occurrence"),
            "start-operation-subscription",
            "start-operation-incarnation",
            1,
        );
        let starter = crate::start_operation_journal(&key).expect("the start's own operation");
        let spec = crate::ProcessExecutionEnvSpec::new(
            crate::AdmittedPluginConfig::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded, crate::MaxToolCalls::new(64)),
        );
        Self {
            registry: backend.process_registry(),
            relay: ArtifactCleanupRelay::over_backend(backend, engines.clone()),
            clock: backend.clock(),
            ports,
            engines,
            key,
            starter,
            pin: crate::ArtifactReferrer::HostPin(crate::HostArtifactPin::mint()),
            env_ref: spec.stable_ref().expect("environment ref"),
            bytes: spec.to_store_bytes().expect("environment bytes"),
        }
    }

    fn stores(&self) -> crate::ProcessStartStores<'_> {
        crate::ProcessStartStores {
            tracing: None,
            registry: self.registry.as_ref(),
            env_store: Some(self.ports.env()),
            engines: &self.engines,
            session_catalog: None,
            session_turn_admission: None,
            executor: "trigger delivery start",
            starter: &self.starter,
            trigger_route: None,
        }
    }

    fn start_claim(&self) -> crate::ReferrerClaim {
        crate::ReferrerClaim::guarded(crate::ReferrerGuard::Start {
            start_key: self.key.clone(),
            starter: self.starter.clone(),
        })
    }

    /// The delivery's registration, its environment published under the pin
    /// first.
    #[expect(
        clippy::expect_used,
        reason = "conformance law validates each setup and transition"
    )]
    async fn registration(&self) -> crate::ProcessRegistration {
        let pin = crate::ReferrerClaim::unguarded(self.pin.clone()).expect("pin claim");
        self.ports
            .env()
            .publish_process_execution_env(&pin, &self.env_ref, &self.bytes)
            .await
            .expect("publish the environment under the pin");
        crate::ProcessRegistration::new(
            crate::ProcessInput::Engine {
                kind: "testing-fixture".to_owned(),
                payload: serde_json::Value::Null,
            },
            crate::ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        )
        .with_start_key(Some(self.key.clone()))
        .with_execution_env_ref(Some(self.env_ref.clone()))
    }

    /// Release the pin and let the relay end it: `Start(key)` is then the
    /// environment's only referrer.
    #[expect(
        clippy::expect_used,
        reason = "conformance law validates each setup and transition"
    )]
    async fn release_pin(&self) {
        self.ports
            .end(self.pin.clone())
            .await
            .expect("release the pin");
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance law validates each setup and transition"
    )]
    async fn env(&self) -> Option<Vec<u8>> {
        self.ports
            .env()
            .get_process_execution_env(&self.env_ref)
            .await
            .expect("read the environment")
    }

    /// Run the relay until no cleanup is due: a guard whose referrer has not
    /// ended is deferred past this law's horizon.
    #[expect(
        clippy::expect_used,
        reason = "conformance law validates each setup and transition"
    )]
    async fn drain(&self) {
        for _ in 0..8 {
            let pass = relay_due(
                &self.relay,
                self.clock.as_ref(),
                std::num::NonZeroUsize::new(256).expect("a nonzero page"),
            )
            .await
            .expect("relay a due pass");
            assert_eq!(pass.stalled, 0, "no cleanup stalls: {pass:?}");
            if pass.claimed == 0 {
                return;
            }
        }
        panic!("the relay never drained");
    }
}
