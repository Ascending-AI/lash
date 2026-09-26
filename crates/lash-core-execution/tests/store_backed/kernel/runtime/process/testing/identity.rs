use crate::{ProcessExecutionEnvRef, ProcessExecutionEnvSpec};

/// The env bytes an older build stored under its own family's reference:
/// one entry, read-only.
struct PriorFamilyEnv {
    env_ref: ProcessExecutionEnvRef,
    bytes: Vec<u8>,
}

#[async_trait::async_trait]
impl crate::ProcessExecutionEnvStore for PriorFamilyEnv {
    async fn publish_process_execution_env(
        &self,
        _owner: &crate::ArtifactOwner,
        _env_ref: &ProcessExecutionEnvRef,
        _bytes: &[u8],
    ) -> Result<(), crate::PluginError> {
        unreachable!("the prior-family entry is read-only")
    }

    async fn transfer_process_execution_env(
        &self,
        _from: &crate::ArtifactOwner,
        _to: &crate::ArtifactOwner,
        _env_ref: &ProcessExecutionEnvRef,
    ) -> Result<(), crate::PluginError> {
        unreachable!("the prior-family entry is read-only")
    }

    async fn release_process_execution_env(
        &self,
        _owner: &crate::ArtifactOwner,
        _env_ref: &ProcessExecutionEnvRef,
    ) -> Result<(), crate::PluginError> {
        unreachable!("the prior-family entry is read-only")
    }

    async fn retire_process_execution_env_owner(
        &self,
        _owner: &crate::ArtifactOwner,
    ) -> Result<(), crate::PluginError> {
        unreachable!("the prior-family entry is read-only")
    }

    async fn get_process_execution_env(
        &self,
        env_ref: &ProcessExecutionEnvRef,
    ) -> Result<Option<Vec<u8>>, crate::PluginError> {
        Ok((env_ref == &self.env_ref).then(|| self.bytes.clone()))
    }
}

#[tokio::test]
async fn runtime_feedback_process_environment_refuses_prior_family() {
    use crate::{
        ArtifactOwner, ProcessExecutionEnvStore, load_process_execution_env,
        publish_process_execution_env,
    };
    let backend = crate::support::memory_store_set().await;
    let store = backend.process_env_store();
    let mut policy = crate::SessionPolicy::new(crate::TurnBudget::Unbounded);
    policy.model = crate::ModelSpec::builder("model")
        .context_window_tokens(100)
        .build()
        .unwrap()
        .with_capability(crate::ModelCapability {
            instruction_role: crate::InstructionRole::Developer,
            native_mid_conversation_system: true,
            ..Default::default()
        });
    let spec = ProcessExecutionEnvSpec::new(crate::PluginOptions::default(), policy);
    let bytes = spec.to_store_bytes().unwrap();
    for (prefix, domain) in [
        ("process-env:v4:blake3:", "lash-process-env/v4"),
        ("process-env:v5:blake3:", "lash-process-env/v5"),
    ] {
        let old = ProcessExecutionEnvRef::new(format!(
            "{prefix}{}",
            crate::stable_hash::blake3_hex(domain, &bytes)
        ));
        assert!(
            store
                .publish_process_execution_env(&ArtifactOwner::host("version-test"), &old, &bytes)
                .await
                .unwrap_err()
                .to_string()
                .contains("do not match")
        );
        // A store only accepts bytes under their own reference, so the bytes
        // an older build stored under its family's reference come from a
        // store that answers them as-is.
        let prior = PriorFamilyEnv {
            env_ref: old.clone(),
            bytes: bytes.clone(),
        };
        assert!(
            load_process_execution_env(&prior, &old)
                .await
                .unwrap_err()
                .to_string()
                .contains("recreate")
        );
    }
    let current =
        publish_process_execution_env(store.as_ref(), &ArtifactOwner::host("version-test"), &spec)
            .await
            .unwrap();
    assert!(current.as_str().starts_with("process-env:v6:blake3:"));
    assert_eq!(
        load_process_execution_env(store.as_ref(), &current)
            .await
            .unwrap(),
        spec
    );
}
