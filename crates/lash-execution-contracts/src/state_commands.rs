use lash_core_store::plugin_state::PluginStateError;
use lash_core_store::tool_run::StateCommand;
use serde::Serialize;
use serde_json::Value;
impl From<PluginStateError> for crate::PluginError {
    fn from(error: PluginStateError) -> Self {
        Self::State(error)
    }
}

/// The commands a callback or tool body returns with its result: an ordered
/// batch against its own plugin's namespace. Later commands see earlier
/// ones; [`set`](Self::set) and [`remove`](Self::remove) are last-writer-wins,
/// and a read-modify-write goes through a registered pure reducer with
/// [`apply`](Self::apply).
///
/// The batch is bounded and all-or-none: one refused command publishes
/// nothing of it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StateCommands(Vec<StateCommand>);

impl StateCommands {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set `key` to `value`.
    #[must_use]
    pub fn set(mut self, key: impl Into<String>, value: Value) -> Self {
        self.0.push(StateCommand::Set {
            key: key.into(),
            value,
        });
        self
    }

    /// Set `key` to `value`'s JSON encoding.
    ///
    /// # Errors
    ///
    /// [`PluginStateError::Encode`] when `value` does not encode.
    pub fn set_as<T: Serialize>(
        self,
        key: impl Into<String>,
        value: &T,
    ) -> Result<Self, PluginStateError> {
        let key = key.into();
        let value = serde_json::to_value(value).map_err(|source| PluginStateError::Encode {
            key: key.clone(),
            message: source.to_string(),
        })?;
        Ok(self.set(key, value))
    }

    /// Remove `key`.
    #[must_use]
    pub fn remove(mut self, key: impl Into<String>) -> Self {
        self.0.push(StateCommand::Remove { key: key.into() });
        self
    }

    /// Resolve `key` through the plugin's reducer `reducer` with `input`.
    #[must_use]
    pub fn apply(
        mut self,
        key: impl Into<String>,
        reducer: impl Into<String>,
        input: Value,
    ) -> Self {
        self.0.push(StateCommand::Apply {
            key: key.into(),
            name: reducer.into(),
            input,
        });
        self
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn commands(&self) -> &[StateCommand] {
        &self.0
    }

    #[must_use]
    pub fn into_commands(self) -> Vec<StateCommand> {
        self.0
    }
}

impl From<Vec<StateCommand>> for StateCommands {
    fn from(commands: Vec<StateCommand>) -> Self {
        Self(commands)
    }
}
