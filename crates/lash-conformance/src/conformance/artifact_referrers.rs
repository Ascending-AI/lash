//! Referrer laws shared by SQLite and PostgreSQL artifact stores.
//!
//! The fixture opens both artifact ports over one durable catalog. Each law
//! uses freshly minted pins so no test relies on another test's rows.

use crate::fused_artifact_store::ReopenableArtifactStore;
use crate::module_artifact_store::SampleArtifact;
use lash_core::ArtifactCarry;
use lash_core::ArtifactName;
use lash_core::ArtifactReferrer;
use lash_core::ArtifactReferrerKind;
use lash_core::ArtifactStoreId;
use lash_core::FrameEnvironmentId;
use lash_core::HostArtifactPin;
use lash_core::ReferrerClaim;
use lash_core::ReferrerGuard;
use lash_core::ResolvedArtifactCleanup;
use lash_core::testing::Gate;
use lash_execution_contracts::module_artifacts::ArtifactStoreError;
use lash_execution_contracts::module_artifacts::ModuleArtifactStore;
use pretty_assertions::assert_eq;
use std::sync::Arc;

fn module(name: &str) -> SampleArtifact {
    SampleArtifact::named(name)
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
    fn durability_tier(&self) -> lash_execution_contracts::module_artifacts::DurabilityTier {
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

    async fn hold_module_artifacts(
        &self,
        claim: &ReferrerClaim,
        artifacts: &[lash_execution_contracts::module_artifacts::ModuleArtifactHold<'_>],
    ) -> Result<(), ArtifactStoreError> {
        self.gate.pass().await;
        self.inner.hold_module_artifacts(claim, artifacts).await
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
    let key = artifact.key().to_owned();
    let bytes = artifact.to_store_bytes().expect("module bytes");
    let frame = ArtifactReferrer::FrameEnvironment(FrameEnvironmentId::new(
        lash_kernel_value::identity::SessionId::fixture(format!(
            "artifact-race-{}",
            HostArtifactPin::mint()
        )),
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
    let module_ref = artifact.key();
    let module_bytes = artifact.to_store_bytes().expect("module bytes");
    let env = lash_execution_contracts::testing::process_env_with_limit(1024);
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
        .publish_process_env(&claim, &env_ref, &env_bytes)
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
            .get_process_env(&env_ref)
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
            .get_process_env(&env_ref)
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
        .publish_process_env(&claim, &env_ref, &env_bytes)
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
        .publish_process_env(&fresh, &env_ref, &env_bytes)
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
            .get_process_env(&env_ref)
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
    let key = artifact.key();
    let bytes = artifact.to_store_bytes().expect("module bytes");
    let env = lash_execution_contracts::testing::process_env_with_limit(1024);
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
        .publish_process_env(&source_claim, &env_ref, &env_bytes)
        .await
        .expect("publish source environment");
    let module_carry = ResolvedArtifactCleanup {
        referrer: source.clone(),
        carries: vec![ArtifactCarry {
            artifact: ArtifactName {
                store: ArtifactStoreId::KernelDocument,
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
            .get_process_env(&env_ref)
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
                .get_process_env(&env_ref)
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
        ArtifactReferrer::EnvironmentVersion(
            lash_core_store::environment::VersionOwner::preparing(
                lash_core_store::environment::assemble(None, [])
                    .expect("canonical environment")
                    .id()
                    .clone(),
            )
            .id()
            .clone(),
        ),
        ArtifactReferrer::FrameEnvironment(FrameEnvironmentId::new(
            lash_kernel_value::identity::SessionId::from("canonical-session"),
            lash_core::FrameNodeId::new("canonical-frame").expect("frame id"),
        )),
        ArtifactReferrer::ProcessRecord(lash_vocabulary::identity::ProcessId::fixture(
            "canonical-process",
        )),
        ArtifactReferrer::Start(lash_core::StartKey::for_host("canonical-start")),
        ArtifactReferrer::StartInput {
            start_key: lash_core::StartKey::for_host("canonical-start"),
            starter: journal.clone(),
        },
        ArtifactReferrer::Execution(journal),
        ArtifactReferrer::HostPin(HostArtifactPin::mint()),
        ArtifactReferrer::Session(lash_kernel_value::identity::SessionId::from(
            "canonical-session",
        )),
        ArtifactReferrer::Upload(lash_core::UploadReferrerId::mint(
            lash_kernel_value::identity::SessionId::from("canonical-session"),
        )),
        ArtifactReferrer::Source(Box::new(lash_core::AwaitEventKey {
            scope: lash_core::ExecutionScope::turn("canonical-session", "canonical-turn"),
            wait: lash_core::AwaitEventWaitIdentity::tool_call(lash_core::ToolCallId::fixture(
                "canonical-call",
            )),
            key_id: "canonical-key".into(),
            signature: "canonical-signature".into(),
        })),
    ];
    assert_eq!(values.len(), ArtifactReferrerKind::ALL.len());
    assert_eq!(
        values.each_ref().map(ArtifactReferrer::kind),
        ArtifactReferrerKind::ALL,
    );
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
    // A stated environment as large as one with 128 KiB of component
    // config.
    let spec = lash_core::testing::process_env_of_size(128 * 1024);
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
            .publish_process_env(&claim, env_ref, &bytes)
            .await
            .expect("capture publishes");
    }
    let first = ArtifactReferrer::ProcessRecord(lash_vocabulary::identity::ProcessId::fixture(
        "captured-environment-first",
    ));
    let second = ArtifactReferrer::ProcessRecord(lash_vocabulary::identity::ProcessId::fixture(
        "captured-environment-second",
    ));
    for reader in [&first, &second] {
        store
            .acquire_process_env(
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
            .get_process_env(&first_ref)
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
            .get_process_env(&second_ref)
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
            .get_process_env(&first_ref)
            .await
            .expect("read after last reader"),
        None
    );
    store
        .end_process_env_referrer(&end(second.clone()))
        .await
        .expect("cleanup replay");
    assert!(
        matches!(store.acquire_process_env(&claim, &first_ref).await,
        Err(ArtifactStoreError::ReferrerEnded { referrer }) if referrer == declaration)
    );
    let ended_claim = ReferrerClaim::unguarded(second.clone()).expect("ended process claim");
    assert!(
        matches!(store.publish_process_env(&ended_claim, &first_ref, &spec.to_store_bytes().expect("bytes")).await,
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
    let env = lash_execution_contracts::testing::process_env_with_limit(1024);
    let env_ref = env.stable_ref().expect("environment reference");
    let env_bytes = env.to_store_bytes().expect("environment bytes");
    let claims = [
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
                .publish_process_env(&claim, &env_ref, &env_bytes)
                .await,
            handles
                .process_env
                .acquire_process_env(&claim, &lash_core::ProcessEnvRef::new("missing-env"))
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
            let plugin = lash_execution_contracts::plugin_error::PluginError::from(error);
            let refusal = lash_core::store::StoreRefusal::ReferrerKindRefused {
                kind,
                store: lash_core::ReferrerStore::Artifact,
            };
            assert!(
                matches!(&plugin, lash_execution_contracts::plugin_error::PluginError::StoreRefusal(found) if *found == refusal)
            );
            let plugin: lash_execution_contracts::plugin_error::PluginError =
                serde_json::from_value(
                    serde_json::to_value(plugin).expect("encode plugin refusal"),
                )
                .expect("decode plugin refusal");
            assert!(
                matches!(&plugin, lash_execution_contracts::plugin_error::PluginError::StoreRefusal(found) if *found == refusal)
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
            .get_process_env(&env_ref)
            .await
            .expect("read"),
        None
    );
}

/// An environment whose packages and authored roots reach a published
/// package, a Link and its Document, all first held by `configured`.
struct OriginEnv {
    configured: ArtifactReferrer,
    document: lash_kernel_value::names::DocumentId,
    package: lash_core_store::packages::PackageRecord,
    publication: lash_vocabulary::package::PublicationRecord,
    key: lash_vocabulary::package::PublicationKey,
    authored: lash_core_store::packages::LinkRecord,
    accepted: lash_core_store::tool_surface::RecordedPackages,
    env_ref: lash_core::ProcessEnvRef,
    bytes: Vec<u8>,
}

#[expect(
    clippy::expect_used,
    reason = "conformance fixture validates each store step"
)]
async fn origin_env(
    handles: &crate::fused_artifact_store::ArtifactStoreHandles,
    snapshot_only: bool,
) -> OriginEnv {
    use lash_core_store::packages::{LinkRecord, PackageRecord};
    use {
        lash_kernel_value::names::*, lash_kernel_value::package::*,
        lash_vocabulary::claims::RegistryKind, lash_vocabulary::package::*,
    };

    let (configured, configured_claim) = pin_claim();
    let document = DocumentId::from_bytes([73; 32]);
    handles
        .artifacts
        .publish_module_artifact(&configured_claim, &document.to_string(), b"origin document")
        .await
        .expect("document");
    let link = Link {
        helper_fills: Default::default(),
        document,
        fills: Default::default(),
    };
    let link = LinkRecord {
        id: link.identity(),
        link,
    };
    handles
        .packages
        .put_link(&configured_claim, &link, &[])
        .await
        .expect("Link");
    let package = Package {
        path: PackagePath::parse("law").expect("path"),
        members: [(
            Segment::new("run").expect("segment"),
            Member {
                visibility: Visibility::Public,
                kind: MemberKind::Export {
                    link: link.id,
                    export: ExportName {
                        name: Name::new("run"),
                        member: None,
                    },
                },
            },
        )]
        .into(),
    };
    let package = PackageRecord {
        id: package.identity(),
        package,
    };
    handles
        .packages
        .put_package(&configured_claim, &package)
        .await
        .expect("Package");
    let publication = PublicationRecord {
        context: PublicationContext {
            domain: PublicationDomain("law".into()),
            scope: PublicationScope::Host,
            publisher: PublisherInstance("configured".into()),
            root: Segment::new("law").expect("root"),
        },
        path: package.package.path.clone(),
        content: PublishedContent::Package(package.id),
        code_links: [link.id].into(),
        imported_publications: Vec::new(),
    };
    let key = handles
        .packages
        .publish(&configured_claim, &publication)
        .await
        .expect("publication");
    // An authored root is retained independently of the published package roots.
    let authored = Link {
        helper_fills: Default::default(),
        document,
        fills: [(
            ImportSlot(0),
            Fill::Export {
                link: link.id,
                export: ExportName {
                    name: Name::new("run"),
                    member: None,
                },
            },
        )]
        .into(),
    };
    let authored = LinkRecord {
        id: authored.identity(),
        link: authored,
    };
    handles
        .packages
        .put_link(&configured_claim, &authored, &[])
        .await
        .expect("authored root");
    let mut execution =
        lash_core_store::tool_surface::ExecutionEnvironment::empty(RegistryKind::Process);
    if !snapshot_only {
        execution.publications = vec![publication.clone()];
    }
    execution.authored.insert(authored.id);
    let accepted = lash_core_store::tool_surface::RecordedPackages {
        consumer: Some(publication.context.clone()),
        snapshot: NamespaceSnapshot {
            advertisements: vec![AcceptedAdvertisement {
                publication: publication.clone(),
                advertisement: PackageAdvertisement::Enumerated(package.package.clone()),
            }],
        },
        routes: Vec::new(),
    };
    let env = lash_core::ProcessEnv::builder(lash_core::MaxToolCalls::new(1024))
        .execution(execution)
        .packages(accepted.clone())
        .build()
        .expect("env");
    let env_ref = env.stable_ref().expect("env ref");
    let bytes = env.to_store_bytes().expect("env bytes");
    OriginEnv {
        configured,
        document,
        package,
        publication,
        key,
        authored,
        accepted,
        env_ref,
        bytes,
    }
}

/// L-ENV-ORIGIN: publishing/acquiring an env and carrying Start to ProcessRecord
/// retains the exact publication and authored Link closure under the same claim.
/// Source cleanup, cold acquisition and the final fence cannot lose or resurrect it.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store step"
)]
pub async fn process_env_carry_retains_exact_origin<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    for snapshot_only in [false, true] {
        let fixture = make();
        let handles = fixture.open;
        let OriginEnv {
            configured,
            document,
            publication,
            key,
            authored,
            accepted,
            env_ref,
            bytes,
            ..
        } = origin_env(&handles, snapshot_only).await;
        let start_key =
            lash_core::StartKey::for_host(format!("origin-{}", HostArtifactPin::mint()));
        let start = ArtifactReferrer::Start(start_key.clone());
        let start_claim = ReferrerClaim::guarded(ReferrerGuard::Start {
            start_key,
            starter: lash_core::ExecutionScope::runtime_operation("env-origin")
                .journal_identity()
                .expect("journal"),
        });
        handles
            .process_env
            .publish_process_env(&start_claim, &env_ref, &bytes)
            .await
            .expect("publish env and origin");
        handles
            .artifacts
            .end_module_referrer(&end(configured))
            .await
            .expect("configured end");
        assert_eq!(
            handles
                .packages
                .publication(&key)
                .await
                .expect("publication read"),
            Some(publication.clone())
        );
        let process = ArtifactReferrer::ProcessRecord(
            lash_vocabulary::identity::ProcessId::fixture("env-origin"),
        );
        handles
            .process_env
            .end_process_env_referrer(&ResolvedArtifactCleanup {
                referrer: start.clone(),
                carries: vec![ArtifactCarry {
                    artifact: ArtifactName {
                        store: ArtifactStoreId::ProcessEnv,
                        artifact_ref: env_ref.as_str().into(),
                    },
                    to: process.clone(),
                }],
            })
            .await
            .expect("carry env with origin");
        handles
            .artifacts
            .end_module_referrer(&end(start.clone()))
            .await
            .expect("start module end");
        drop(handles);
        let cold = (fixture.reopen)();
        let cold_bytes = cold
            .process_env
            .get_process_env(&env_ref)
            .await
            .expect("cold env read")
            .expect("retained env");
        let cold_env =
            lash_core::ProcessEnv::from_store_bytes(&cold_bytes).expect("decode cold env");
        assert_eq!(
            cold_env.packages(),
            &accepted,
            "trusted consumer and accepted advertisements survive cold carry"
        );
        assert_eq!(
            cold.packages
                .publication(&key)
                .await
                .expect("cold provenance"),
            Some(publication)
        );
        assert_eq!(
            cold.packages
                .link(&authored.id)
                .await
                .expect("cold authored root"),
            Some(authored.clone())
        );
        let (pin, pin_claim) = pin_claim();
        cold.process_env
            .acquire_process_env(&pin_claim, &env_ref)
            .await
            .expect("cold acquire env and origin");
        cold.process_env
            .end_process_env_referrer(&end(process.clone()))
            .await
            .expect("process env end");
        cold.artifacts
            .end_module_referrer(&end(process))
            .await
            .expect("process modules end");
        assert!(
            cold.packages
                .link(&authored.id)
                .await
                .expect("pin still owns authored")
                .is_some()
        );
        cold.process_env
            .end_process_env_referrer(&end(pin.clone()))
            .await
            .expect("last env end");
        cold.artifacts
            .end_module_referrer(&end(pin))
            .await
            .expect("last origin end");
        assert!(
            cold.packages
                .publication(&key)
                .await
                .expect("reclaimed publication")
                .is_none()
        );
        assert!(
            cold.packages
                .link(&authored.id)
                .await
                .expect("reclaimed authored")
                .is_none()
        );
        assert!(
            cold.artifacts
                .get_module_artifact(&document.to_string())
                .await
                .expect("reclaimed document")
                .is_none()
        );
        assert!(
            matches!(cold.process_env.publish_process_env(&start_claim, &env_ref, &bytes).await,
        Err(ArtifactStoreError::ReferrerEnded { referrer }) if referrer == start)
        );
    }
}

/// A store returns an environment's bytes exactly as published, whatever
/// spelling the publisher chose, and keeps a shared part alive while any
/// environment that contains it lives (FIG-7700). The oracle is the
/// published bytes themselves and their content address.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store step"
)]
pub async fn process_env_spellings_read_back_exactly<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    use lash_core_store::process_identity::process_env_ref_for_bytes;
    let fixture = make();
    let store = &fixture.open.process_env;
    let member = |env: &lash_core::ProcessEnv, key: &str| {
        let value = serde_json::to_value(env).expect("env value");
        serde_json::to_string(&value[key]).expect("member")
    };
    let first = lash_core::testing::process_env_of_size(64);
    let second = lash_core::testing::process_env_of_size(65);
    let [p, l, e, c] = ["packages", "limits", "execution", "components"].map(|k| member(&first, k));
    let c2 = member(&second, "components");
    let pretty = serde_json::to_vec_pretty(&serde_json::to_value(&first).expect("env value"))
        .expect("pretty env");
    let spellings: Vec<Vec<u8>> = vec![
        first.to_store_bytes().expect("canonical"),
        format!(" {{ \"packages\" :\t{p} ,\n\"limits\":{l},  \"execution\" : {e}\r\n, \"components\":{c} }}\n").into_bytes(),
        // The same members as the spelling above, with other components.
        format!(" {{ \"packages\" :\t{p} ,\n\"limits\":{l},  \"execution\" : {e}\r\n, \"components\":{c2} }}\n").into_bytes(),
        format!("{{\"components\":{c},\"execution\":{e},\"limits\":{l},\"packages\":{p}}}").into_bytes(),
        format!("{{\"packages\":{p},\"components\":{c},\"limits\":{l},\"execution\":{e}}}").into_bytes(),
        format!("{{\"p\\u0061ckages\":{p},\"limits\":{l},\"execution\":{e},\"components\":{c}}}").into_bytes(),
        pretty,
    ];
    for bytes in &spellings {
        lash_core::ProcessEnv::from_store_bytes(bytes).expect("each spelling is an environment");
    }
    let (publisher, claim) = pin_claim();
    let (survivor, survivor_claim) = pin_claim();
    let refs: Vec<_> = spellings
        .iter()
        .map(|bytes| process_env_ref_for_bytes(bytes))
        .collect();
    for (env_ref, bytes) in refs.iter().zip(&spellings) {
        store
            .publish_process_env(&claim, env_ref, bytes)
            .await
            .expect("spelling publishes");
    }
    store
        .acquire_process_env(&survivor_claim, &refs[1])
        .await
        .expect("survivor holds one spelling");
    let reopened = (fixture.reopen)();
    for (env_ref, bytes) in refs.iter().zip(&spellings) {
        let read = reopened
            .process_env
            .get_process_env(env_ref)
            .await
            .expect("read spelling")
            .expect("spelling stored");
        assert_eq!(
            String::from_utf8_lossy(&read),
            String::from_utf8_lossy(bytes),
            "a spelling reads back exactly"
        );
        assert_eq!(process_env_ref_for_bytes(&read), *env_ref);
    }
    reopened
        .process_env
        .end_process_env_referrer(&end(publisher))
        .await
        .expect("publisher ends");
    for (index, env_ref) in refs.iter().enumerate() {
        let read = store
            .get_process_env(env_ref)
            .await
            .expect("read after publisher");
        if index == 1 {
            assert_eq!(
                read.as_ref(),
                Some(&spellings[1]),
                "a held spelling outlives its siblings"
            );
        } else {
            assert_eq!(read, None, "an unheld spelling is reclaimed");
        }
    }
    store
        .end_process_env_referrer(&end(survivor))
        .await
        .expect("survivor ends");
    assert_eq!(
        store
            .get_process_env(&refs[1])
            .await
            .expect("read after survivor"),
        None
    );
}

/// What a referrer's package authority answers: the publications it does
/// not hold, and whether it may label the package.
async fn package_authority(
    packages: &dyn lash_durable::packages::PackageStore,
    claim: &ReferrerClaim,
    origin: &OriginEnv,
) -> (String, String) {
    let unheld = packages
        .unheld_publications(
            claim,
            &std::collections::BTreeSet::from([origin.key.clone()]),
        )
        .await;
    let labels = packages
        .put_labels(claim, &origin.package.id, "{\"law\":\"authority\"}")
        .await;
    (format!("{unheld:?}"), format!("{labels:?}"))
}

/// A referrer that holds an environment answers package authority as a
/// referrer holding the environment's closure itself does (FIG-7700): held
/// while it holds the environment, carried with a release, held again by
/// whoever republishes it, and refused once ended. A referrer holding
/// nothing is never answered as a holder.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store step"
)]
pub async fn environment_holders_answer_package_authority_as_direct_holders<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    let fixture = make();
    let handles = fixture.open;
    let origin = origin_env(&handles, false).await;
    let held = ("Ok({})".to_owned(), "Ok(())".to_owned());
    let start_key = lash_core::StartKey::for_host(format!("authority-{}", HostArtifactPin::mint()));
    let start = ArtifactReferrer::Start(start_key.clone());
    let start_claim = ReferrerClaim::guarded(ReferrerGuard::Start {
        start_key,
        starter: lash_core::ExecutionScope::runtime_operation("env-authority")
            .journal_identity()
            .expect("journal"),
    });
    handles
        .process_env
        .publish_process_env(&start_claim, &origin.env_ref, &origin.bytes)
        .await
        .expect("publish env");
    handles
        .artifacts
        .end_module_referrer(&end(origin.configured.clone()))
        .await
        .expect("configured ends");
    assert_eq!(
        package_authority(&*handles.packages, &start_claim, &origin).await,
        held,
        "the publisher holds its environment's closure"
    );
    let (_, bystander_claim) = pin_claim();
    let (unheld, labels) = package_authority(&*handles.packages, &bystander_claim, &origin).await;
    assert_eq!(
        unheld,
        format!("Ok({{{:?}}})", origin.key),
        "a bystander holds nothing"
    );
    assert_eq!(
        labels,
        format!(
            "Err(Package(Missing({:?})))",
            lash_durable::packages::package_reference(&origin.package.id)
        ),
        "a bystander may not label"
    );

    // Release as the cleanup relay applies it: the environment share carries
    // the environment to the process, then the module share carries the
    // publication to a pin.
    let (pin, carried_claim) = pin_claim();
    let process = ArtifactReferrer::ProcessRecord(lash_vocabulary::identity::ProcessId::fixture(
        "env-authority",
    ));
    let process_claim = ReferrerClaim::unguarded(process.clone()).expect("process claim");
    handles
        .process_env
        .end_process_env_referrer(&ResolvedArtifactCleanup {
            referrer: start.clone(),
            carries: vec![ArtifactCarry {
                artifact: ArtifactName {
                    store: ArtifactStoreId::ProcessEnv,
                    artifact_ref: origin.env_ref.as_str().into(),
                },
                to: process.clone(),
            }],
        })
        .await
        .expect("start's environment share carries the environment");
    handles
        .artifacts
        .end_module_referrer(&ResolvedArtifactCleanup {
            referrer: start.clone(),
            carries: vec![ArtifactCarry {
                artifact: ArtifactName {
                    store: ArtifactStoreId::module(),
                    artifact_ref: lash_durable::packages::publication_reference(&origin.key),
                },
                to: pin.clone(),
            }],
        })
        .await
        .expect("start's module share carries the publication");
    let reopened = (fixture.reopen)();
    let (unheld, labels) = package_authority(&*reopened.packages, &start_claim, &origin).await;
    assert!(
        unheld.contains("ReferrerEnded") && labels.contains("ReferrerEnded"),
        "an ended publisher is refused: {unheld} {labels}"
    );
    for (claim, who) in [(&process_claim, "the process"), (&carried_claim, "the pin")] {
        assert_eq!(
            package_authority(&*reopened.packages, claim, &origin).await,
            held,
            "{who} holds what the release carried to it"
        );
    }
    // Republish: a fresh referrer publishes the stored environment again.
    let (republisher, republisher_claim) = pin_claim();
    reopened
        .process_env
        .publish_process_env(&republisher_claim, &origin.env_ref, &origin.bytes)
        .await
        .expect("republish env");
    reopened
        .process_env
        .end_process_env_referrer(&end(process.clone()))
        .await
        .expect("process environment ends");
    reopened
        .artifacts
        .end_module_referrer(&end(process))
        .await
        .expect("process modules end");
    assert_eq!(
        package_authority(&*reopened.packages, &carried_claim, &origin).await,
        held,
        "the pin's carried publication outlives the process"
    );
    reopened
        .artifacts
        .end_module_referrer(&end(pin))
        .await
        .expect("pin ends");
    assert_eq!(
        package_authority(&*reopened.packages, &republisher_claim, &origin).await,
        held,
        "the republisher holds the closure"
    );
    reopened
        .process_env
        .end_process_env_referrer(&end(republisher.clone()))
        .await
        .expect("republisher environment ends");
    assert_eq!(
        reopened
            .packages
            .publication(&origin.key)
            .await
            .expect("read"),
        Some(origin.publication.clone()),
        "the republisher's module share outlives its environment share"
    );
    reopened
        .artifacts
        .end_module_referrer(&end(republisher))
        .await
        .expect("republisher modules end");
    assert_eq!(
        reopened
            .packages
            .publication(&origin.key)
            .await
            .expect("read"),
        None,
        "the last holder's end reclaims the publication"
    );
    assert_eq!(
        reopened
            .process_env
            .get_process_env(&origin.env_ref)
            .await
            .expect("read"),
        None
    );
}

/// Which of a referrer's two shares ends first.
#[derive(Clone, Copy, Debug)]
enum ShareOrder {
    EnvironmentFirst,
    ModulesFirst,
}

/// One run of [`module_share_outlives_environment_share_in_either_order`]:
/// a publisher holds an environment through both its shares; `bystander`
/// holds nothing and carries the publication to `first`; the publisher's
/// own module end carries it to `second`.
struct ShareRun {
    origin: OriginEnv,
    publisher: ArtifactReferrer,
    publisher_claim: ReferrerClaim,
    bystander: ArtifactReferrer,
    first: (ArtifactReferrer, ReferrerClaim),
    second: (ArtifactReferrer, ReferrerClaim),
}

impl ShareRun {
    #[expect(
        clippy::expect_used,
        reason = "conformance fixture validates each store step"
    )]
    async fn new(handles: &crate::fused_artifact_store::ArtifactStoreHandles) -> Self {
        let origin = origin_env(handles, false).await;
        let (publisher, publisher_claim) = pin_claim();
        handles
            .process_env
            .publish_process_env(&publisher_claim, &origin.env_ref, &origin.bytes)
            .await
            .expect("publish env");
        handles
            .artifacts
            .end_module_referrer(&end(origin.configured.clone()))
            .await
            .expect("configured ends");
        Self {
            origin,
            publisher,
            publisher_claim,
            bystander: pin_claim().0,
            first: pin_claim(),
            second: pin_claim(),
        }
    }

    /// The publisher's environment share ends.
    async fn end_environment(
        &self,
        handles: &crate::fused_artifact_store::ArtifactStoreHandles,
    ) -> String {
        format!(
            "{:?}",
            handles
                .process_env
                .end_process_env_referrer(&end(self.publisher.clone()))
                .await
        )
    }

    /// The publisher's module share ends; as its second end it carries the
    /// publication to `second`.
    async fn end_modules(
        &self,
        handles: &crate::fused_artifact_store::ArtifactStoreHandles,
        carries: bool,
    ) -> String {
        let carries = if carries {
            vec![carry(
                ArtifactStoreId::module(),
                lash_durable::packages::publication_reference(&self.origin.key),
                &self.second.0,
            )]
        } else {
            Vec::new()
        };
        format!(
            "{:?}",
            handles
                .artifacts
                .end_module_referrer(&ResolvedArtifactCleanup {
                    referrer: self.publisher.clone(),
                    carries,
                })
                .await
        )
    }

    /// The publisher's environment share ends, or its module share.
    async fn end_share(
        &self,
        handles: &crate::fused_artifact_store::ArtifactStoreHandles,
        environment: bool,
        second: bool,
    ) -> String {
        if environment {
            self.end_environment(handles).await
        } else {
            self.end_modules(handles, second).await
        }
    }

    /// A referrer holding nothing carries the publication to `first`.
    async fn bystander_carries(
        &self,
        handles: &crate::fused_artifact_store::ArtifactStoreHandles,
    ) -> String {
        let answer = handles
            .artifacts
            .end_module_referrer(&ResolvedArtifactCleanup {
                referrer: self.bystander.clone(),
                carries: vec![carry(
                    ArtifactStoreId::module(),
                    lash_durable::packages::publication_reference(&self.origin.key),
                    &self.first.0,
                )],
            })
            .await;
        match answer {
            Ok(()) => "carried".to_owned(),
            Err(_) => "refused".to_owned(),
        }
    }

    /// The environment's and the publication's bytes, the publisher's
    /// authority, and whether each carry destination holds the publication.
    async fn observe(&self, handles: &crate::fused_artifact_store::ArtifactStoreHandles) -> String {
        let environment = match handles
            .process_env
            .get_process_env(&self.origin.env_ref)
            .await
        {
            Ok(Some(bytes)) if bytes == self.origin.bytes => "exact".to_owned(),
            other => format!("{other:?}"),
        };
        let publication = match handles.packages.publication(&self.origin.key).await {
            Ok(Some(record)) if record == self.origin.publication => "exact".to_owned(),
            other => format!("{other:?}"),
        };
        let (unheld, labels) =
            package_authority(&*handles.packages, &self.publisher_claim, &self.origin).await;
        let publisher = if unheld.contains("ReferrerEnded") && labels.contains("ReferrerEnded") {
            "ended".to_owned()
        } else {
            format!("{unheld} {labels}")
        };
        let mut holders = Vec::new();
        for (_, claim) in [&self.first, &self.second] {
            let (unheld, labels) = package_authority(&*handles.packages, claim, &self.origin).await;
            holders.push(if unheld == "Ok({})" && labels == "Ok(())" {
                "holds".to_owned()
            } else if unheld == format!("Ok({{{:?}}})", self.origin.key)
                && labels.contains("Missing")
            {
                "-".to_owned()
            } else {
                format!("{unheld} {labels}")
            });
        }
        // A referrer has ended for package authority once it is fenced
        // and reaches no module: its module share alone keeps it holding.
        let ended = match handles
            .package_storage
            .package_referrer_ended(&self.publisher)
            .await
        {
            Ok(ended) => ended.to_string(),
            other => format!("{other:?}"),
        };
        format!(
            "env {environment}, publication {publication}, publisher {publisher}, first {}, second {}, ended {ended}",
            holders[0], holders[1]
        )
    }
}

/// Where a crash cuts a [`ShareRun`].
#[derive(Clone, Copy, Debug)]
enum ShareCut {
    /// The first end never commits; recovery replays it.
    FirstLost,
    /// The first end commits and its reply is lost; recovery replays it.
    FirstCommitted,
    /// The second end never commits; recovery replays it.
    SecondLost,
    /// The second end commits and its reply is lost; recovery replays it.
    SecondCommitted,
    /// A crash takes the store while the first end's transaction is open;
    /// recovery replays the end over what the crash left.
    FirstInFlight,
    /// A crash takes the store while the second end's transaction is open.
    SecondInFlight,
}

/// End the publisher's two shares in `order`, reopening the store between
/// the two ends and after a cut, and answer every observation.
async fn share_run<F>(make: &F, order: ShareOrder, cut: Option<ShareCut>) -> Vec<String>
where
    F: Fn() -> ReopenableArtifactStore,
{
    let fixture = make();
    let mut handles = fixture.open;
    let mut reopen = fixture.reopen;
    let run = ShareRun::new(&handles).await;
    let mut seen = Vec::new();
    for (index, environment) in match order {
        ShareOrder::EnvironmentFirst => [true, false],
        ShareOrder::ModulesFirst => [false, true],
    }
    .into_iter()
    .enumerate()
    {
        let lost = matches!(
            (index, cut),
            (0, Some(ShareCut::FirstLost)) | (1, Some(ShareCut::SecondLost))
        );
        let committed = matches!(
            (index, cut),
            (0, Some(ShareCut::FirstCommitted)) | (1, Some(ShareCut::SecondCommitted))
        );
        let in_flight = matches!(
            (index, cut),
            (0, Some(ShareCut::FirstInFlight)) | (1, Some(ShareCut::SecondInFlight))
        );
        if in_flight {
            #[expect(
                clippy::expect_used,
                reason = "the cut law runs in-flight cuts only where the fixture can cut"
            )]
            let interrupt = fixture
                .interrupt
                .as_ref()
                .expect("the fixture cuts a transaction in flight");
            // The end goes on over the original store; the crash's store
            // replaces it.
            let (_, crashed) = tokio::join!(
                run.end_share(&handles, environment, index == 1),
                interrupt()
            );
            reopen = crashed;
        } else if !lost {
            let answer = run.end_share(&handles, environment, index == 1).await;
            if !committed {
                seen.push(answer);
            }
        }
        // A reopen between the two ends, and after every cut.
        if index == 0 || lost || committed || in_flight {
            drop(handles);
            handles = reopen();
        }
        if lost || committed || in_flight {
            seen.push(run.end_share(&handles, environment, index == 1).await);
        }
        seen.push(run.observe(&handles).await);
        if index == 0 {
            seen.push(run.bystander_carries(&handles).await);
            seen.push(run.observe(&handles).await);
        }
    }
    seen
}

/// What the store answered before environments owned their modules
/// through a body (FIG-7700): a referrer's module share holds the
/// environment's closure until that share ends, whatever its environment
/// share does, and its environment share alone holds no module.
fn share_answers(order: ShareOrder) -> Vec<String> {
    let line = |environment: &str, publication: &str, first: &str, second: &str, ended: bool| {
        format!(
            "env {environment}, publication {publication}, publisher ended, first {first}, second {second}, ended {ended}"
        )
    };
    match order {
        ShareOrder::EnvironmentFirst => vec![
            "Ok(())".into(),
            line("Ok(None)", "exact", "-", "-", false),
            "carried".into(),
            line("Ok(None)", "exact", "holds", "-", false),
            "Ok(())".into(),
            line("Ok(None)", "exact", "holds", "holds", true),
        ],
        ShareOrder::ModulesFirst => vec![
            "Ok(())".into(),
            line("exact", "Ok(None)", "-", "-", true),
            "refused".into(),
            line("exact", "Ok(None)", "-", "-", true),
            "Ok(())".into(),
            line("Ok(None)", "Ok(None)", "-", "-", true),
        ],
    }
}

/// A referrer's module share keeps the environment's closure until that
/// share ends, in either end order and across a reopen between the two
/// ends: the publication reads back exactly, the referrer's package
/// authority, and a module carry to another referrer between the ends,
/// all answer as when the referrer held each module directly (FIG-7700).
pub async fn module_share_outlives_environment_share_in_either_order<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    for order in [ShareOrder::EnvironmentFirst, ShareOrder::ModulesFirst] {
        assert_eq!(
            share_run(&make, order, None).await,
            share_answers(order),
            "{order:?}"
        );
    }
}

/// A crash at either end of [`module_share_outlives_environment_share_in_either_order`],
/// recovered by replaying that end, answers as the uncut run (FIG-7700):
/// before the end, after it commits, and, where the fixture can cut one,
/// while its transaction is open.
pub async fn module_share_survives_a_cut_between_its_ends<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    let in_flight = make().interrupt.is_some();
    for order in [ShareOrder::EnvironmentFirst, ShareOrder::ModulesFirst] {
        for cut in [
            ShareCut::FirstLost,
            ShareCut::FirstCommitted,
            ShareCut::SecondLost,
            ShareCut::SecondCommitted,
            ShareCut::FirstInFlight,
            ShareCut::SecondInFlight,
        ]
        .into_iter()
        .filter(|cut| {
            in_flight || !matches!(cut, ShareCut::FirstInFlight | ShareCut::SecondInFlight)
        }) {
            assert_eq!(
                share_run(&make, order, Some(cut)).await,
                share_answers(order),
                "{order:?} cut {cut:?}"
            );
        }
    }
}

/// An environment over its own document and the link that authors it.
struct LinkedEnv {
    document: lash_kernel_value::names::DocumentId,
    link: lash_core_store::packages::LinkRecord,
    env_ref: lash_core::ProcessEnvRef,
    bytes: Vec<u8>,
    /// The environment's body reference: its closure's key.
    body: String,
    /// The document's and the link's module references.
    modules: std::collections::BTreeSet<String>,
}

#[expect(
    clippy::expect_used,
    reason = "conformance fixture builds a valid environment"
)]
fn linked_env(round: u8) -> LinkedEnv {
    use lash_kernel_value::package::Link;
    let document = lash_kernel_value::names::DocumentId::from_bytes([round; 32]);
    let link = Link {
        helper_fills: Default::default(),
        document,
        fills: Default::default(),
    };
    let link = lash_core_store::packages::LinkRecord {
        id: link.identity(),
        link,
    };
    let mut execution = lash_core_store::tool_surface::ExecutionEnvironment::empty(
        lash_vocabulary::claims::RegistryKind::Process,
    );
    execution.authored.insert(link.id);
    let env = lash_core::ProcessEnv::builder(lash_core::MaxToolCalls::new(1024))
        .execution(execution)
        .build()
        .expect("env");
    let env_ref = env.stable_ref().expect("env ref");
    let bytes = env.to_store_bytes().expect("env bytes");
    let body = lash_core_store::process_env::stored::split(&bytes).body_ref;
    let modules = [
        document.to_string(),
        lash_durable::packages::link_reference(&link.id),
    ]
    .into();
    LinkedEnv {
        document,
        link,
        env_ref,
        bytes,
        body,
        modules,
    }
}

impl LinkedEnv {
    /// `modules` publishes the document and the link; `holder` publishes
    /// the environment over them.
    #[expect(
        clippy::expect_used,
        reason = "conformance fixture validates each store step"
    )]
    async fn publish(
        &self,
        handles: &crate::fused_artifact_store::ArtifactStoreHandles,
        modules: &ReferrerClaim,
        holder: &ReferrerClaim,
    ) {
        handles
            .artifacts
            .publish_module_artifact(modules, &self.document.to_string(), b"carry order document")
            .await
            .expect("document");
        handles
            .packages
            .put_link(modules, &self.link, &[])
            .await
            .expect("link");
        handles
            .process_env
            .publish_process_env(holder, &self.env_ref, &self.bytes)
            .await
            .expect("publish env");
    }
}

/// Carried environments refuse in the caller's order, as each carry's
/// environment was checked in turn before environments owned their modules
/// through a closure (FIG-7700); the order a store takes its locks in does
/// not choose the refusal. A source's module share has ended and taken its
/// environments' modules while its environment share keeps their heads.
/// Carrying [A, B] from that share names A's missing module both when B's
/// head is absent and when B's closure is missing too.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store step"
)]
pub async fn carried_environments_refuse_in_the_callers_order<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    let envs: Vec<LinkedEnv> = (90..=121).map(linked_env).collect();
    // A sorts before B by reference, so a store that checks its carries in
    // reference order checks them in the caller's order; B's closure sorts
    // before A's, so a store that checks closures in key order names B.
    let (a, b) = envs
        .iter()
        .flat_map(|a| envs.iter().map(move |b| (a, b)))
        .find(|(a, b)| a.env_ref.as_str() < b.env_ref.as_str() && b.body < a.body)
        .expect("two environments whose reference and closure orders differ");
    for b_published in [false, true] {
        let fixture = make();
        let handles = fixture.open;
        let (configured, configured_claim) = pin_claim();
        let (source, source_claim) = pin_claim();
        let (destination, _destination_claim) = pin_claim();
        a.publish(&handles, &configured_claim, &source_claim).await;
        if b_published {
            b.publish(&handles, &configured_claim, &source_claim).await;
        }
        handles
            .artifacts
            .end_module_referrer(&end(configured))
            .await
            .expect("configured ends");
        handles
            .artifacts
            .end_module_referrer(&end(source.clone()))
            .await
            .expect("source's module share ends");
        let answer = handles
            .process_env
            .end_process_env_referrer(&ResolvedArtifactCleanup {
                referrer: source,
                carries: [a, b]
                    .into_iter()
                    .map(|env| {
                        carry(
                            ArtifactStoreId::ProcessEnv,
                            env.env_ref.as_str().to_owned(),
                            &destination,
                        )
                    })
                    .collect(),
            })
            .await;
        let named = match &answer {
            Err(ArtifactStoreError::ArtifactMissing { artifact_ref })
                if a.modules.contains(artifact_ref) =>
            {
                "a module of A".to_owned()
            }
            other => format!("{other:?}"),
        };
        assert_eq!(
            named,
            "a module of A",
            "B {}",
            if b_published {
                "published, both closures missing"
            } else {
                "absent"
            }
        );
    }
}

/// The referrers of one cut run, minted fresh so runs never share a fence.
struct CutRun {
    origin: OriginEnv,
    start: ArtifactReferrer,
    start_claim: ReferrerClaim,
    process: ArtifactReferrer,
    process_claim: ReferrerClaim,
    pin: ArtifactReferrer,
    pin_claim: ReferrerClaim,
    republisher: ArtifactReferrer,
    republisher_claim: ReferrerClaim,
}

/// The steps of [`environment_retention_survives_a_cut_at_every_step`].
/// A step names the first step of the delivery it belongs to: a recovered
/// delivery replays every share from there, as the cleanup relay does.
const CUT_STEPS: [(&str, usize); 10] = [
    ("start publishes the environment", 0),
    ("configured ends", 1),
    (
        "start's environment share carries the environment to the process",
        2,
    ),
    ("start's module share carries the publication to the pin", 2),
    ("republisher publishes the environment again", 4),
    ("process's environment share ends", 5),
    ("process's module share ends", 5),
    ("pin ends", 7),
    ("republisher's environment share ends", 8),
    ("republisher's module share ends", 8),
];

fn carry(store: ArtifactStoreId, artifact_ref: String, to: &ArtifactReferrer) -> ArtifactCarry {
    ArtifactCarry {
        artifact: ArtifactName {
            store,
            artifact_ref,
        },
        to: to.clone(),
    }
}

impl CutRun {
    #[expect(
        clippy::expect_used,
        reason = "conformance fixture builds valid claims"
    )]
    async fn new(handles: &crate::fused_artifact_store::ArtifactStoreHandles) -> Self {
        let origin = origin_env(handles, false).await;
        let start_key = lash_core::StartKey::for_host(format!("cut-{}", HostArtifactPin::mint()));
        let start = ArtifactReferrer::Start(start_key.clone());
        let start_claim = ReferrerClaim::guarded(ReferrerGuard::Start {
            start_key,
            starter: lash_core::ExecutionScope::runtime_operation("env-cut")
                .journal_identity()
                .expect("journal"),
        });
        let process =
            ArtifactReferrer::ProcessRecord(lash_vocabulary::identity::ProcessId::fixture(
                &format!("env-cut-{}", HostArtifactPin::mint()),
            ));
        let process_claim = ReferrerClaim::unguarded(process.clone()).expect("process claim");
        let (pin, carried_claim) = pin_claim();
        let (republisher, republisher_claim) = pin_claim();
        Self {
            origin,
            start,
            start_claim,
            process,
            process_claim,
            pin,
            pin_claim: carried_claim,
            republisher,
            republisher_claim,
        }
    }

    async fn step(
        &self,
        handles: &crate::fused_artifact_store::ArtifactStoreHandles,
        step: usize,
    ) -> Result<(), String> {
        let env_ref = self.origin.env_ref.as_str().to_owned();
        let publication = lash_durable::packages::publication_reference(&self.origin.key);
        let answer = match step {
            0 => {
                handles
                    .process_env
                    .publish_process_env(
                        &self.start_claim,
                        &self.origin.env_ref,
                        &self.origin.bytes,
                    )
                    .await
            }
            1 => {
                handles
                    .artifacts
                    .end_module_referrer(&end(self.origin.configured.clone()))
                    .await
            }
            2 => {
                handles
                    .process_env
                    .end_process_env_referrer(&ResolvedArtifactCleanup {
                        referrer: self.start.clone(),
                        carries: vec![carry(ArtifactStoreId::ProcessEnv, env_ref, &self.process)],
                    })
                    .await
            }
            3 => {
                handles
                    .artifacts
                    .end_module_referrer(&ResolvedArtifactCleanup {
                        referrer: self.start.clone(),
                        carries: vec![carry(ArtifactStoreId::module(), publication, &self.pin)],
                    })
                    .await
            }
            4 => {
                handles
                    .process_env
                    .publish_process_env(
                        &self.republisher_claim,
                        &self.origin.env_ref,
                        &self.origin.bytes,
                    )
                    .await
            }
            5 => {
                handles
                    .process_env
                    .end_process_env_referrer(&end(self.process.clone()))
                    .await
            }
            6 => {
                handles
                    .artifacts
                    .end_module_referrer(&end(self.process.clone()))
                    .await
            }
            7 => {
                handles
                    .artifacts
                    .end_module_referrer(&end(self.pin.clone()))
                    .await
            }
            8 => {
                handles
                    .process_env
                    .end_process_env_referrer(&end(self.republisher.clone()))
                    .await
            }
            _ => {
                handles
                    .artifacts
                    .end_module_referrer(&end(self.republisher.clone()))
                    .await
            }
        };
        answer.map_err(|error| format!("{}: {error:?}", CUT_STEPS[step].0))
    }

    /// What every reader answers: the environment's bytes, the publication,
    /// and each referrer's package authority, with refusals by kind only.
    async fn observe(
        &self,
        handles: &crate::fused_artifact_store::ArtifactStoreHandles,
    ) -> Vec<String> {
        let kind = |error: String| -> String {
            error
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '(')
                .collect()
        };
        let mut seen = vec![
            match handles
                .process_env
                .get_process_env(&self.origin.env_ref)
                .await
            {
                Ok(Some(bytes)) if bytes == self.origin.bytes => "environment exact".to_owned(),
                other => format!("environment {other:?}"),
            },
            format!(
                "publication {:?}",
                handles
                    .packages
                    .publication(&self.origin.key)
                    .await
                    .map(|found| found.is_some())
            ),
        ];
        for claim in [
            &self.start_claim,
            &self.process_claim,
            &self.pin_claim,
            &self.republisher_claim,
        ] {
            let unheld = handles
                .packages
                .unheld_publications(
                    claim,
                    &std::collections::BTreeSet::from([self.origin.key.clone()]),
                )
                .await;
            seen.push(match unheld {
                Ok(unheld) => format!("unheld {}", unheld.len()),
                Err(error) => kind(format!("{error:?}")),
            });
        }
        seen
    }
}

#[derive(Clone, Copy, Debug)]
enum EnvCut {
    /// The step's transaction never commits.
    Lost,
    /// The step commits and its reply is lost.
    Committed,
}

/// Run every step, cutting `cut` if given. A cut reopens the store and
/// replays the interrupted delivery from its first share. Answers each
/// step's observations.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store step"
)]
async fn env_cut_run<F>(make: &F, cut: Option<(usize, EnvCut)>) -> Vec<Vec<String>>
where
    F: Fn() -> ReopenableArtifactStore,
{
    let fixture = make();
    let mut handles = fixture.open;
    let run = CutRun::new(&handles).await;
    let mut seen = Vec::new();
    for (step, (_, delivery)) in CUT_STEPS.iter().enumerate() {
        match cut {
            Some((at, fault)) if at == step => {
                if matches!(fault, EnvCut::Committed) {
                    run.step(&handles, step).await.expect("step before the cut");
                }
                drop(handles);
                handles = (fixture.reopen)();
                for replay in *delivery..=step {
                    run.step(&handles, replay)
                        .await
                        .expect("recovery replays the delivery");
                }
            }
            _ => run.step(&handles, step).await.expect("step"),
        }
        seen.push(run.observe(&handles).await);
    }
    seen
}

/// A process environment's retention and the package authority it carries
/// survive a crash at every step of publish, release and republish: a lost
/// or a committed-but-unanswered step, recovered by replaying its delivery,
/// leaves every reader answering as the uncut run does (FIG-7700).
pub async fn environment_retention_survives_a_cut_at_every_step<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    let uncut = env_cut_run(&make, None).await;
    assert_eq!(
        uncut.last().map(|last| &last[..2]),
        Some(
            &[
                "environment Ok(None)".to_owned(),
                "publication Ok(false)".to_owned()
            ][..]
        ),
        "the last end reclaims everything"
    );
    for (step, (name, _)) in CUT_STEPS.iter().enumerate() {
        for fault in [EnvCut::Lost, EnvCut::Committed] {
            assert_eq!(
                env_cut_run(&make, Some((step, fault))).await,
                uncut,
                "cut {fault:?} at `{name}`"
            );
        }
    }
}
