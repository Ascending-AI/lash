use crate::{ProcessEnv, ProcessEnvRef};

/// The env bytes an older build stored under its own family's reference:
/// one entry, read-only.
struct PriorFamilyEnv {
    env_ref: ProcessEnvRef,
    bytes: Vec<u8>,
}

lash_core_execution::impl_current_fleet_format!(PriorFamilyEnv);

#[async_trait::async_trait]
impl crate::ProcessEnvStore for PriorFamilyEnv {
    async fn publish_process_env(
        &self,
        _claim: &crate::ReferrerClaim,
        _env_ref: &ProcessEnvRef,
        _bytes: &[u8],
    ) -> Result<(), crate::ArtifactStoreError> {
        unreachable!("the prior-family entry is read-only")
    }

    async fn acquire_process_env(
        &self,
        _claim: &crate::ReferrerClaim,
        _env_ref: &ProcessEnvRef,
    ) -> Result<(), crate::ArtifactStoreError> {
        unreachable!("the prior-family entry is read-only")
    }

    async fn end_process_env_referrer(
        &self,
        _cleanup: &crate::ResolvedArtifactCleanup,
    ) -> Result<(), crate::ArtifactStoreError> {
        unreachable!("the prior-family entry is read-only")
    }

    async fn get_process_env(
        &self,
        env_ref: &ProcessEnvRef,
    ) -> Result<Option<Vec<u8>>, crate::ArtifactStoreError> {
        Ok((env_ref == &self.env_ref).then(|| self.bytes.clone()))
    }
}

#[tokio::test]
async fn runtime_feedback_process_environment_refuses_prior_family() {
    use crate::{ProcessEnvStore, load_process_env};
    let backend = crate::support::sqlite_memory_store_set().await;
    let store = backend.process_env_store();
    let profile = crate::LlmProfileConfig::new(crate::RecordedLlmProfile::mint(
        crate::LlmProfileKey::new("model"),
        crate::LlmProfileMetadata::builder("model")
            .cache_retention(lash_core_execution::provider::CacheRetention::Short)
            .context_window_tokens(100)
            .build()
            .unwrap()
            .with_capability(crate::LlmProfileCapability {
                instruction_role: crate::InstructionRole::Developer,
                native_mid_conversation_system: true,
                ..Default::default()
            }),
    ));
    let spec = ProcessEnv::builder(
        crate::SessionToolAccess::restricted(std::iter::empty()).unwrap(),
        crate::MaxToolCalls::new(1024),
    )
    .model(crate::ProcessModel::new(
        profile,
        Default::default(),
        Default::default(),
    ))
    .build()
    .unwrap();
    let claim = crate::ReferrerClaim::unguarded(crate::ArtifactReferrer::HostPin(
        crate::HostArtifactPin::mint(),
    ))
    .expect("host pin claim");
    let bytes = spec.to_store_bytes().unwrap();
    for (prefix, domain) in [
        ("process-env:v4:blake3:", "lash-process-env/v4"),
        ("process-env:v5:blake3:", "lash-process-env/v5"),
    ] {
        let old = ProcessEnvRef::new(format!(
            "{prefix}{}",
            crate::stable_hash::blake3_hex(domain, &bytes)
        ));
        assert!(matches!(
            store.publish_process_env(&claim, &old, &bytes).await,
            Err(crate::ArtifactStoreError::Immutable { .. })
        ));
        // A store only accepts bytes under their own reference, so the bytes
        // an older build stored under its family's reference come from a
        // store that answers them as-is.
        let prior = PriorFamilyEnv {
            env_ref: old.clone(),
            bytes: bytes.clone(),
        };
        assert!(
            load_process_env(&prior, &old)
                .await
                .unwrap_err()
                .to_string()
                .contains("recreate")
        );
    }
    let current = lash_core_execution::testing::publish_process_env_for_testing(
        store.as_ref(),
        &claim,
        &spec,
    )
    .await
    .unwrap();
    assert!(current.as_str().starts_with("process-env:v6:blake3:"));
    assert_eq!(
        load_process_env(store.as_ref(), &current).await.unwrap(),
        spec
    );
}
