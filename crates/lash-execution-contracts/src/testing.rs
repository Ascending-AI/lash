//! Pure record fixtures shared by contract and runtime laws.
#![expect(
    clippy::expect_used,
    reason = "test-support fixtures abort when setup is invalid"
)]

/// The fixed fixture process environment: no tools, no components, a
/// tool-call limit of 1024.
#[cfg(any(test, feature = "testing"))]
pub fn process_env_fixture() -> crate::ProcessEnv {
    process_env_with_limit(1024)
}

/// A component-free environment that admits no tool and holds at most
/// `max_tool_calls` calls at once: distinct limits are distinct environments.
#[cfg(any(test, feature = "testing"))]
#[expect(clippy::expect_used, reason = "fixture environment")]
pub fn process_env_with_limit(max_tool_calls: usize) -> crate::ProcessEnv {
    crate::ProcessEnv::builder(crate::MaxToolCalls::new(max_tool_calls))
        .build()
        .expect("the fixture process environment encodes")
}

/// The reference [`process_execution_env_fixture`] publishes under. An
/// environment reference is content-addressed, so it names the fixture
/// environment in every store: a subscription draft that records the
/// reference, and never loads it, takes this instead of publishing.
#[cfg(any(test, feature = "testing"))]
pub fn process_execution_env_fixture_ref() -> crate::ProcessEnvRef {
    process_env_fixture()
        .stable_ref()
        .expect("fixed process execution environment fixture encodes")
}

/// The launch of an engine start under the fixture environment, with no
/// engine settings.
#[cfg(any(test, feature = "testing"))]
pub fn fixture_engine_launch() -> crate::EngineLaunch {
    crate::process::EngineLaunch::new(process_execution_env_fixture_ref(), None)
}

/// A claim under a freshly minted host pin: an unguarded referrer a fixture
/// publishes under and never releases.
#[cfg(any(test, feature = "testing"))]
pub fn host_pin_claim_for_testing() -> crate::ReferrerClaim {
    crate::ReferrerClaim::unguarded(crate::ArtifactReferrer::HostPin(
        crate::HostArtifactPin::mint(),
    ))
    .expect("a host pin is an unguarded referrer")
}

/// An engine input of `HeldProcessEngine`'s kind: the trivial process a law
/// registers when it is about the row rather than about what runs it.
#[cfg(any(test, feature = "testing"))]
pub fn held_engine_input(payload: serde_json::Value) -> crate::ProcessInput {
    crate::process::ProcessInput::Engine {
        kind: HELD_PROCESS_ENGINE_KIND.to_string(),
        payload,
        launch: fixture_engine_launch(),
    }
}

/// `launch` as a declaration states it once its environment is held: what a
/// fixture declares when no start-policy round produced it.
#[cfg(any(test, feature = "testing"))]
pub fn declared_launch_for_testing(launch: crate::EngineLaunch) -> crate::DeclaredLaunch {
    crate::process::model::launch::held_launch(launch)
}

/// A host start of [`held_engine_input`] under the fixture environment,
/// which the host must have published (see [`process_execution_env_fixture`]).
#[cfg(any(test, feature = "testing"))]
pub fn held_engine_request(
    payload: serde_json::Value,
    originator: crate::ProcessOriginator,
    lifetime: impl Into<crate::LifetimeDecision>,
) -> crate::ProcessStartRequest {
    crate::process::ProcessStartRequest::engine(
        HELD_PROCESS_ENGINE_KIND,
        payload,
        fixture_engine_launch(),
        originator,
        lifetime,
    )
}

/// A declared start of [`held_engine_input`]: the declaration a recorded
/// start of the held engine journals, its launch the fixture environment.
#[cfg(any(test, feature = "testing"))]
pub fn held_engine_declaration(
    payload: serde_json::Value,
    originator: crate::ProcessOriginator,
    lifetime: impl Into<crate::LifetimeDecision>,
) -> crate::ProcessStartDeclaration {
    crate::process::ProcessStartDeclaration::engine(
        HELD_PROCESS_ENGINE_KIND,
        payload,
        crate::process::model::launch::held_launch(fixture_engine_launch()),
        originator,
        lifetime,
    )
}

/// A registration of [`held_engine_input`], whose launch names the fixture
/// environment. Publish it with [`process_execution_env_fixture`] in the
/// store receiving the registration.
#[cfg(any(test, feature = "testing"))]
pub fn held_engine_registration(
    payload: serde_json::Value,
    provenance: crate::ProcessProvenance,
    lifetime: impl Into<crate::LifetimeDecision>,
) -> crate::ProcessRegistration {
    crate::process::ProcessRegistration::new(held_engine_input(payload), provenance, lifetime)
}

/// The kind `HeldProcessEngine` runs.
#[cfg(any(test, feature = "testing"))]
pub const HELD_PROCESS_ENGINE_KIND: &str = "testing-held";
/// Publish the fixed component-free environment under an unguarded host pin.
pub async fn process_execution_env_fixture(
    env_store: &dyn crate::ProcessEnvStore,
) -> crate::ProcessEnvRef {
    let env = process_env_fixture();
    let bytes = env
        .to_store_bytes()
        .expect("fixed fixture environment encodes");
    let reference = env
        .stable_ref()
        .expect("fixed fixture environment has a reference");
    env_store
        .publish_process_env(&host_pin_claim_for_testing(), &reference, &bytes)
        .await
        .expect("fixed process execution environment fixture publishes");
    reference
}

mod registration_refusals;
pub use registration_refusals::{
    REFUSAL_FIXTURE_START_KEY as PROCESS_REFUSAL_FIXTURE_START_KEY, accepted_process_registration,
    refused_process_registrations,
};
