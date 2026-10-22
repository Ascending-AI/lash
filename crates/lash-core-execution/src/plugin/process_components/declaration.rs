//! Component formats and the claim requirements of a captured environment.
use super::*;

/// What a component declares about itself: its id and formats for stored state and config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessComponentDeclaration {
    pub id: ProcessComponentId,
    /// The format of stored state and config the component reads natively.
    pub format_version: FormatVersion,
    /// Every state and config format it reads. Contains `format_version`.
    pub readable_formats: Vec<FormatVersion>,
    /// Every state and config format it writes. Contains `format_version`.
    pub writable_formats: Vec<FormatVersion>,
}

impl ProcessComponentDeclaration {
    /// The declaration of a component that reads and writes only `format_version`.
    ///
    /// # Errors
    ///
    /// The id is refused.
    pub fn initial(id: &str, format_version: FormatVersion) -> Result<Self, PluginError> {
        Ok(Self {
            id: ProcessComponentId::new(id)
                .map_err(|error| PluginError::Registration(error.to_string()))?,
            format_version,
            readable_formats: vec![format_version],
            writable_formats: vec![format_version],
        })
    }

    /// The claim requirements a process under `env` carries from its
    /// start: one format token for every component `env` names, and the
    /// provider and codec version of every tool it links (FIG-6993), which
    /// a node must register to run a call through the link.
    pub fn env_requirements(env: &ProcessEnv) -> BTreeSet<crate::ObligationToken> {
        env.components()
            .iter()
            .map(|entry| {
                lash_vocabulary::claims::ObligationToken::for_component(
                    entry.id(),
                    entry.config().format_version(),
                )
            })
            .chain(crate::ObligationToken::for_requirements(
                &env.execution().requirements,
            ))
            .collect()
    }
}
