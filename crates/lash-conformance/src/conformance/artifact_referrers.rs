//! Referrer laws shared by SQLite and PostgreSQL artifact stores.
//!
//! The fixture opens both artifact ports over one durable catalog. Each law
//! uses freshly minted pins so no test relies on another test's rows.

use crate::fused_artifact_store::ReopenableArtifactStore;
use lash_core::testing::Gate;
use lash_core::{
    ArtifactCarry, ArtifactName, ArtifactReferrer, ArtifactReferrerKind, ArtifactStoreError,
    ArtifactStoreId, FrameEnvironmentId, HostArtifactPin, ModuleArtifactStore, ReferrerClaim,
    ReferrerGuard, ResolvedArtifactCleanup,
};
use lashlang::testing::ast_builders as b;
use lashlang::{ModuleArtifact, TypeExpr};
use pretty_assertions::assert_eq;
use std::sync::Arc;

#[expect(clippy::expect_used, reason = "test fixture constructs a valid module")]
fn module(name: &str) -> ModuleArtifact {
    ModuleArtifact::from_program(b::module(
        vec![b::process_returning(
            name,
            vec![b::param("root", TypeExpr::Str)],
            TypeExpr::Str,
            b::finish(b::var("root")),
        )],
        Vec::new(),
    ))
    .expect("module fixture")
}

#[expect(clippy::expect_used, reason = "test fixture constructs a valid claim")]
fn pin_claim() -> (ArtifactReferrer, ReferrerClaim) {
    let referrer = ArtifactReferrer::HostPin(HostArtifactPin::mint());
    let claim = ReferrerClaim::unguarded(referrer.clone()).expect("host pin claim");
    (referrer, claim)
}

fn end(referrer: ArtifactReferrer) -> ResolvedArtifactCleanup {
    ResolvedArtifactCleanup {
        referrer,
        carries: Vec::new(),
    }
}

/// An artifact store whose every publication waits at `gate` before it
/// enters the store (ADR 0044 §Simulation).
struct HeldPublication {
    inner: Arc<dyn ModuleArtifactStore>,
    gate: Arc<Gate>,
}

#[async_trait::async_trait]
impl ModuleArtifactStore for HeldPublication {
    fn durability_tier(&self) -> lash_core::DurabilityTier {
        self.inner.durability_tier()
    }

    async fn publish_module_artifact(
        &self,
        claim: &ReferrerClaim,
        module_ref: &str,
        bytes: &[u8],
    ) -> Result<(), ArtifactStoreError> {
        self.gate.pass().await;
        self.inner
            .publish_module_artifact(claim, module_ref, bytes)
            .await
    }

    async fn acquire_module_artifact(
        &self,
        claim: &ReferrerClaim,
        module_ref: &str,
    ) -> Result<(), ArtifactStoreError> {
        self.inner.acquire_module_artifact(claim, module_ref).await
    }

    async fn end_module_referrer(
        &self,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError> {
        self.inner.end_module_referrer(cleanup).await
    }

    async fn get_module_artifact(
        &self,
        module_ref: &str,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
        self.inner.get_module_artifact(module_ref).await
    }
}

/// ADR 0113 §7.5: a publication paused before its store transaction cannot
/// acquire a frame that was fenced while it waited. Its execution claim can
/// still publish the same immutable bytes.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store step"
)]
pub async fn publication_racing_frame_end_is_fenced<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    let handles = make().open;
    let artifact = module("racing");
    let key = artifact.module_ref().as_str().to_owned();
    let bytes = artifact.to_store_bytes().expect("module bytes");
    let frame = ArtifactReferrer::FrameEnvironment(FrameEnvironmentId::new(
        lash_core::SessionId::fixture(format!("artifact-race-{}", HostArtifactPin::mint())),
        lash_core::FrameNodeId::new("frame-1").expect("frame node id"),
    ));
    let frame_claim = ReferrerClaim::unguarded(frame.clone()).expect("frame claim");
    let held = Arc::new(Gate::new("module publication"));
    let writer_store = HeldPublication {
        inner: Arc::clone(&handles.artifacts),
        gate: Arc::clone(&held),
    };
    let writer_key = key.clone();
    let writer_bytes = bytes.clone();
    let writer = tokio::spawn(async move {
        writer_store
            .publish_module_artifact(&frame_claim, &writer_key, &writer_bytes)
            .await
    });
    held.reached(1).await;
    handles
        .artifacts
        .end_module_referrer(&end(frame.clone()))
        .await
        .expect("fence frame");
    held.open_all();
    let refusal = writer
        .await
        .expect("writer joins")
        .expect_err("ended frame refuses publication");
    assert!(matches!(refusal, ArtifactStoreError::ReferrerEnded { referrer } if referrer == frame));
    let journal = lash_core::ExecutionScope::runtime_operation("artifact-race")
        .journal_identity()
        .expect("execution journal");
    let execution = ReferrerClaim::guarded(ReferrerGuard::Journal(journal));
    handles
        .artifacts
        .publish_module_artifact(&execution, &key, &bytes)
        .await
        .expect("execution publication survives frame end");
    assert_eq!(
        handles
            .artifacts
            .get_module_artifact(&key)
            .await
            .expect("read"),
        Some(bytes)
    );
}

/// ADR 0113 §7.14: the same pin holds exact edges in both stores, then cannot
/// publish again after release. A newly minted pin remains usable.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store step"
)]
pub async fn host_pins_reclaim_and_fence<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    let handles = make().open;
    let artifact = module("pinned");
    let module_ref = artifact.module_ref().as_str();
    let module_bytes = artifact.to_store_bytes().expect("module bytes");
    let env = lash_core::ProcessExecutionEnvSpec::new(
        lash_core::AdmittedPluginConfig::default(),
        lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        ),
    );
    let env_ref = env.stable_ref().expect("env ref");
    let env_bytes = env.to_store_bytes().expect("env bytes");
    let (pin, claim) = pin_claim();
    handles
        .artifacts
        .publish_module_artifact(&claim, module_ref, &module_bytes)
        .await
        .expect("publish module");
    handles
        .process_env
        .publish_process_execution_env(&claim, &env_ref, &env_bytes)
        .await
        .expect("publish environment");
    assert_eq!(
        handles
            .artifacts
            .get_module_artifact(module_ref)
            .await
            .expect("read"),
        Some(module_bytes.clone())
    );
    assert_eq!(
        handles
            .process_env
            .get_process_execution_env(&env_ref)
            .await
            .expect("read"),
        Some(env_bytes.clone())
    );
    handles
        .artifacts
        .end_module_referrer(&end(pin.clone()))
        .await
        .expect("end module pin");
    handles
        .process_env
        .end_process_env_referrer(&end(pin.clone()))
        .await
        .expect("end env pin");
    assert_eq!(
        handles
            .artifacts
            .get_module_artifact(module_ref)
            .await
            .expect("read"),
        None
    );
    assert_eq!(
        handles
            .process_env
            .get_process_execution_env(&env_ref)
            .await
            .expect("read"),
        None
    );
    let module_error = handles
        .artifacts
        .publish_module_artifact(&claim, module_ref, &module_bytes)
        .await
        .expect_err("released pin refuses module");
    assert!(
        matches!(module_error, ArtifactStoreError::ReferrerEnded { referrer } if referrer == pin)
    );
    let env_error = handles
        .process_env
        .publish_process_execution_env(&claim, &env_ref, &env_bytes)
        .await
        .expect_err("released pin refuses environment");
    assert!(matches!(env_error, ArtifactStoreError::ReferrerEnded { referrer } if referrer == pin));
    let (_, fresh) = pin_claim();
    handles
        .artifacts
        .publish_module_artifact(&fresh, module_ref, &module_bytes)
        .await
        .expect("fresh pin publishes");
    handles
        .process_env
        .publish_process_execution_env(&fresh, &env_ref, &env_bytes)
        .await
        .expect("fresh pin publishes environment");
    assert_eq!(
        handles
            .artifacts
            .get_module_artifact(module_ref)
            .await
            .expect("read"),
        Some(module_bytes)
    );
    assert_eq!(
        handles
            .process_env
            .get_process_execution_env(&env_ref)
            .await
            .expect("read"),
        Some(env_bytes)
    );
}

/// ADR 0113 §7.16: replaying a cleanup, including a carry into a pin that
/// has since ended, cannot recreate an edge or bytes.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store step"
)]
pub async fn retry_idempotency_after_destination_ends<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    let handles = make().open;
    let artifact = module("idempotent");
    let key = artifact.module_ref().as_str();
    let bytes = artifact.to_store_bytes().expect("module bytes");
    let env = lash_core::ProcessExecutionEnvSpec::new(
        lash_core::AdmittedPluginConfig::default(),
        lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        ),
    );
    let env_ref = env.stable_ref().expect("env ref");
    let env_bytes = env.to_store_bytes().expect("env bytes");
    let (source, source_claim) = pin_claim();
    let (destination, _) = pin_claim();
    handles
        .artifacts
        .publish_module_artifact(&source_claim, key, &bytes)
        .await
        .expect("publish source");
    handles
        .process_env
        .publish_process_execution_env(&source_claim, &env_ref, &env_bytes)
        .await
        .expect("publish source environment");
    let module_carry = ResolvedArtifactCleanup {
        referrer: source.clone(),
        carries: vec![ArtifactCarry {
            artifact: ArtifactName {
                store: ArtifactStoreId::LashlangModule,
                artifact_ref: key.to_owned(),
            },
            to: destination.clone(),
        }],
    };
    let env_carry = ResolvedArtifactCleanup {
        referrer: source,
        carries: vec![ArtifactCarry {
            artifact: ArtifactName {
                store: ArtifactStoreId::ProcessEnv,
                artifact_ref: env_ref.as_str().to_owned(),
            },
            to: destination.clone(),
        }],
    };
    handles
        .artifacts
        .end_module_referrer(&module_carry)
        .await
        .expect("first delivery");
    handles
        .process_env
        .end_process_env_referrer(&env_carry)
        .await
        .expect("first environment delivery");
    assert_eq!(
        handles
            .artifacts
            .get_module_artifact(key)
            .await
            .expect("read"),
        Some(bytes)
    );
    assert_eq!(
        handles
            .process_env
            .get_process_execution_env(&env_ref)
            .await
            .expect("read carried environment"),
        Some(env_bytes)
    );
    handles
        .artifacts
        .end_module_referrer(&end(destination.clone()))
        .await
        .expect("end destination");
    handles
        .process_env
        .end_process_env_referrer(&end(destination))
        .await
        .expect("end environment destination");
    for _ in 0..2 {
        handles
            .artifacts
            .end_module_referrer(&module_carry)
            .await
            .expect("replay delivery");
        handles
            .process_env
            .end_process_env_referrer(&env_carry)
            .await
            .expect("replay environment delivery");
        assert_eq!(
            handles
                .artifacts
                .get_module_artifact(key)
                .await
                .expect("read"),
            None
        );
        assert_eq!(
            handles
                .process_env
                .get_process_execution_env(&env_ref)
                .await
                .expect("read reclaimed environment"),
            None
        );
    }
}

/// ADR 0113 §7.15: every kind has a canonical encoding. Stored corruption
/// checks additionally need each backend's raw SQL fixture.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates canonical fixtures"
)]
pub async fn every_referrer_kind_has_one_canonical_id<F>(_make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    let journal = lash_core::ExecutionScope::runtime_operation("canonical-referrer")
        .journal_identity()
        .expect("journal identity");
    let values = [
        ArtifactReferrer::FrameEnvironment(FrameEnvironmentId::new(
            lash_core::SessionId::from("canonical-session"),
            lash_core::FrameNodeId::new("canonical-frame").expect("frame id"),
        )),
        ArtifactReferrer::ProcessRecord(lash_core::ProcessId::fixture("canonical-process")),
        ArtifactReferrer::SubscriptionRevision(
            lash_core::SubscriptionRevisionId::new("sub".into(), "incarnation".into(), 1)
                .expect("subscription revision"),
        ),
        ArtifactReferrer::Start(lash_core::StartKey::for_host("canonical-start")),
        ArtifactReferrer::StartInput {
            start_key: lash_core::StartKey::for_host("canonical-start"),
            starter: journal.clone(),
        },
        ArtifactReferrer::Execution(journal),
        ArtifactReferrer::HostPin(HostArtifactPin::mint()),
        ArtifactReferrer::Session(lash_core::SessionId::from("canonical-session")),
        ArtifactReferrer::Upload(lash_core::UploadReferrerId::mint(
            lash_core::SessionId::from("canonical-session"),
        )),
        ArtifactReferrer::Source(Box::new(lash_core::AwaitEventKey {
            scope: lash_core::ExecutionScope::turn("canonical-session", "canonical-turn"),
            wait: lash_core::AwaitEventWaitIdentity::tool_completion(
                lash_core::ToolCallId::fixture("canonical-call"),
            ),
            key_id: "canonical-key".into(),
            signature: "canonical-signature".into(),
        })),
    ];
    assert_eq!(values.len(), ArtifactReferrerKind::ALL.len());
    for value in values {
        assert_eq!(
            ArtifactReferrer::decode(value.kind().as_str(), &value.canonical_id())
                .expect("decode canonical id"),
            value,
        );
    }
    for kind in ArtifactReferrerKind::ALL {
        assert!(
            ArtifactReferrer::decode(kind.as_str(), "").is_err(),
            "{kind} rejects empty id"
        );
    }
    assert!(ArtifactReferrer::decode("owner", "old").is_err());
    assert!(ArtifactReferrer::decode("host_pin", "host-pin:v1:INVALID").is_err());
}

/// FIG-4256: declarations and retained process records share the captured
/// environment. Ending one reader cannot reclaim another reader's bytes.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates every store step"
)]
pub async fn captured_environments_are_shared_until_the_last_referrer_ends<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    let fixture = make();
    let store = &fixture.open.process_env;
    let spec = lash_core::ProcessExecutionEnvSpec::new(
        {
            // A captured environment as large as a session with 128 KiB of
            // recorded protocol prompt.
            let mut plugin_config =
                lash_core::PluginConfig::for_protocol(Some("protocol".to_string()));
            plugin_config.insert(
                "protocol",
                serde_json::json!({ "prompt": { "instructions": ["x".repeat(128 * 1024)] } }),
            );
            lash_core::AdmittedPluginConfig::new(plugin_config, 0)
        },
        lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        ),
    );
    let first_ref = spec.stable_ref().expect("first captured digest");
    let second_ref = spec.clone().stable_ref().expect("second captured digest");
    assert_eq!(
        first_ref, second_ref,
        "equal captures address one stored copy"
    );
    let journal = lash_core::ExecutionScope::runtime_operation(format!(
        "env-capture-{}",
        HostArtifactPin::mint()
    ))
    .journal_identity()
    .expect("journal");
    let declaration = ArtifactReferrer::Execution(journal.clone());
    let claim = ReferrerClaim::guarded(ReferrerGuard::Journal(journal.clone()));
    let bytes = spec.to_store_bytes().expect("environment bytes");
    for env_ref in [&first_ref, &second_ref] {
        store
            .publish_process_execution_env(&claim, env_ref, &bytes)
            .await
            .expect("capture publishes");
    }
    let first = ArtifactReferrer::ProcessRecord(lash_core::ProcessId::fixture(
        "captured-environment-first",
    ));
    let second = ArtifactReferrer::ProcessRecord(lash_core::ProcessId::fixture(
        "captured-environment-second",
    ));
    for reader in [&first, &second] {
        store
            .acquire_process_execution_env(
                &ReferrerClaim::unguarded(reader.clone()).expect("process claim"),
                &first_ref,
            )
            .await
            .expect("process holds captured environment");
    }
    store
        .end_process_env_referrer(&end(declaration.clone()))
        .await
        .expect("declaration ends");
    let reopened = (fixture.reopen)();
    assert_eq!(
        reopened
            .process_env
            .get_process_execution_env(&first_ref)
            .await
            .expect("read after declaration"),
        Some(bytes.clone())
    );
    reopened
        .process_env
        .end_process_env_referrer(&end(first))
        .await
        .expect("first process ends");
    assert_eq!(
        store
            .get_process_execution_env(&second_ref)
            .await
            .expect("read after first process"),
        Some(bytes)
    );
    reopened
        .process_env
        .end_process_env_referrer(&end(second.clone()))
        .await
        .expect("last process ends");
    assert_eq!(
        store
            .get_process_execution_env(&first_ref)
            .await
            .expect("read after last reader"),
        None
    );
    store
        .end_process_env_referrer(&end(second.clone()))
        .await
        .expect("cleanup replay");
    assert!(
        matches!(store.acquire_process_execution_env(&claim, &first_ref).await,
        Err(ArtifactStoreError::ReferrerEnded { referrer }) if referrer == declaration)
    );
    let ended_claim = ReferrerClaim::unguarded(second.clone()).expect("ended process claim");
    assert!(
        matches!(store.publish_process_execution_env(&ended_claim, &first_ref, &spec.to_store_bytes().expect("bytes")).await,
        Err(ArtifactStoreError::ReferrerEnded { referrer }) if referrer == second)
    );
}

/// Artifact kinds are refused before byte lookup, without changing the catalog,
/// and retain their typed cause through the plugin and host boundaries.
#[expect(clippy::expect_used, reason = "law validates each store operation")]
pub async fn attachment_only_referrers_cannot_acquire_artifacts<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    let handles = make().open;
    let env = lash_core::ProcessExecutionEnvSpec::new(
        lash_core::AdmittedPluginConfig::default(),
        lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        ),
    );
    let env_ref = env.stable_ref().expect("environment reference");
    let env_bytes = env.to_store_bytes().expect("environment bytes");
    let claims = [
        ReferrerClaim::unguarded(ArtifactReferrer::Session("artifact-kind-refusal".into()))
            .expect("session claim"),
        ReferrerClaim::guarded(ReferrerGuard::Upload {
            upload: lash_core::UploadReferrerId::mint("artifact-kind-refusal".into()),
            expires_at_ms: 10,
        }),
        ReferrerClaim::guarded(ReferrerGuard::StartInput {
            start_key: lash_core::StartKey::for_host("artifact-kind-refusal"),
            starter: lash_core::ExecutionScope::runtime_operation("artifact-kind-refusal")
                .journal_identity()
                .expect("starter"),
        }),
    ];
    for claim in claims {
        let kind = claim.referrer().kind();
        for result in [
            handles
                .artifacts
                .publish_module_artifact(&claim, "refused-module", b"bytes")
                .await,
            handles
                .artifacts
                .acquire_module_artifact(&claim, "missing-module")
                .await,
            handles
                .process_env
                .publish_process_execution_env(&claim, &env_ref, &env_bytes)
                .await,
            handles
                .process_env
                .acquire_process_execution_env(
                    &claim,
                    &lash_core::ProcessExecutionEnvRef::new("missing-env"),
                )
                .await,
            handles
                .artifacts
                .end_module_referrer(&ResolvedArtifactCleanup {
                    referrer: ArtifactReferrer::HostPin(HostArtifactPin::mint()),
                    carries: vec![ArtifactCarry {
                        artifact: ArtifactName {
                            store: ArtifactStoreId::module(),
                            artifact_ref: "missing-module".into(),
                        },
                        to: claim.referrer(),
                    }],
                })
                .await,
            handles
                .process_env
                .end_process_env_referrer(&ResolvedArtifactCleanup {
                    referrer: ArtifactReferrer::HostPin(HostArtifactPin::mint()),
                    carries: vec![ArtifactCarry {
                        artifact: ArtifactName {
                            store: ArtifactStoreId::ProcessEnv,
                            artifact_ref: "missing-env".into(),
                        },
                        to: claim.referrer(),
                    }],
                })
                .await,
        ] {
            let error = result
                .expect_err("attachment-only referrer must be refused before looking up bytes");
            assert!(
                matches!(error, ArtifactStoreError::ReferrerKindRefused { kind: found } if found == kind),
                "{kind} refusal must be typed: {error:?}"
            );
            let plugin = lash_core::PluginError::from(error);
            let refusal = lash_core::store::StoreRefusal::ReferrerKindRefused {
                kind,
                store: lash_core::ReferrerStore::Artifact,
            };
            assert!(
                matches!(&plugin, lash_core::PluginError::StoreRefusal(found) if *found == refusal)
            );
            let plugin: lash_core::PluginError = serde_json::from_value(
                serde_json::to_value(plugin).expect("encode plugin refusal"),
            )
            .expect("decode plugin refusal");
            assert!(
                matches!(&plugin, lash_core::PluginError::StoreRefusal(found) if *found == refusal)
            );
            let host = lash_core::RuntimeEffectControllerError::from(plugin).into_runtime_error();
            assert!(host.is_terminal());
            let decoded: lash_core::RuntimeError =
                serde_json::from_value(serde_json::to_value(host).expect("encode host refusal"))
                    .expect("decode host refusal");
            assert_eq!(decoded.store_refusal(), Some(&refusal));
        }
    }
    assert_eq!(
        handles
            .artifacts
            .get_module_artifact("refused-module")
            .await
            .expect("read"),
        None
    );
    assert_eq!(
        handles
            .process_env
            .get_process_execution_env(&env_ref)
            .await
            .expect("read"),
        None
    );
}
