use super::*;

impl InMemoryProcessExecutionEnvStore {
    #[cfg(test)]
    pub(crate) fn insert_raw_for_testing(&self, env_ref: ProcessExecutionEnvRef, bytes: Vec<u8>) {
        self.envs
            .lock_recover()
            .bytes
            .insert(env_ref.as_str().to_string(), bytes);
    }

    pub(crate) fn from_spec_for_testing(
        owner: ArtifactOwner,
        spec: &ProcessExecutionEnvSpec,
    ) -> Result<(Self, ProcessExecutionEnvRef), crate::PluginError> {
        let bytes = spec
            .to_store_bytes()
            .map_err(|error| crate::PluginError::Session(error.to_string()))?;
        let env_ref = process_execution_env_ref_for_bytes(&bytes);
        let mut state = InMemoryProcessExecutionEnvState::default();
        state.bytes.insert(env_ref.as_str().to_string(), bytes);
        state.owners.insert((env_ref.as_str().to_string(), owner));
        Ok((
            Self {
                envs: Mutex::new(state),
            },
            env_ref,
        ))
    }
}
