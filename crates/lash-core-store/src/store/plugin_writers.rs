//! The per-plugin writer ranges of the fleet record (FIG-4746).
//!
//! Beside the epoch `F`, the fleet record carries one writer range per
//! plugin id: the format versions the fleet permits that plugin's state and
//! config namespaces to be published in. A store reads the ranges under the
//! lock that guards `F` and admits every plugin publication against them
//! before it writes anything, so a build never publishes a namespace format
//! the rest of the fleet cannot read.
//!
//! - **Provisioning** comes from registrations, never a hand list
//!   ([`PluginWriterRanges::provisioned`]). A plugin the record does not name
//!   may publish its first format, which records `[1, 1]`; any other format
//!   is refused until the plugin is provisioned.
//! - **Publication** is described by what is being written: the stamps are
//!   read off the outgoing payload ([`PluginPublication`]), so no write path
//!   can omit them.
//! - **Finalize** is the only move of a recorded range
//!   ([`PluginWriterRanges::finalized`]), and it rides the move of `F`. A
//!   range never contracts: its floor stays where history was written.

use std::collections::BTreeMap;

use crate::compat::{CompatRefusal, VersionRange};
use crate::plugin_state::{FormatNamespace, FormatVersion, PluginStateMap};

use super::StoreError;

/// A recorded callback key the running build no longer registers.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct RecordedCallbackUnavailable {
    pub recorded: PluginCallbackIdentity,
    pub available: Vec<PluginCallbackIdentity>,
}

impl RecordedCallbackUnavailable {
    pub fn into_runtime_error(self) -> crate::RuntimeError {
        let mut error = crate::RuntimeError::new(
            crate::RuntimeErrorCode::RecordedCallbackUnavailable,
            format!(
                "recorded callback {}:{} is unavailable",
                self.recorded.owner, self.recorded.key
            ),
        );
        error.cause = Some(crate::RuntimeErrorCause::RecordedCallbackUnavailable {
            refusal: Box::new(self),
        });
        error
    }
}

/// Opaque stream-hook state whose format the receiving plugin cannot read.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct StreamStateFormatRefusal {
    pub callback: PluginCallbackIdentity,
    #[schemars(with = "std::num::NonZeroU32")]
    pub stored: FormatVersion,
    #[schemars(with = "Vec<std::num::NonZeroU32>")]
    pub readable: Vec<FormatVersion>,
}

impl StreamStateFormatRefusal {
    pub fn into_runtime_error(self) -> crate::RuntimeError {
        let mut error = crate::RuntimeError::new(
            crate::RuntimeErrorCode::StreamStateFormatUnreadable,
            format!(
                "stream state for {}:{} has unreadable format {}",
                self.callback.owner, self.callback.key, self.stored
            ),
        );
        error.cause = Some(crate::RuntimeErrorCause::StreamStateFormatUnreadable {
            refusal: Box::new(self),
        });
        error
    }
}

/// What one registered plugin declares about the formats it writes: the
/// store-side view of its declaration.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PluginWriterRegistration {
    /// The id the plugin registers under.
    pub plugin: String,
    /// The format the plugin reads and writes natively.
    pub native: FormatVersion,
    /// Every format the plugin can write. It contains `native`.
    pub writable: Vec<FormatVersion>,
}

impl PluginWriterRegistration {
    /// The oldest format the plugin can write: what a rollback window's
    /// older build still reads.
    pub fn floor(&self) -> FormatVersion {
        self.writable
            .iter()
            .copied()
            .min()
            .map_or(self.native, |oldest| oldest.min(self.native))
    }

    /// The range a provisioning records for a plugin the fleet record does
    /// not name. Inside a rollback window only the floor is permitted; a
    /// finalized fleet permits everything up to the native format.
    fn seed(&self, finalized: bool) -> VersionRange {
        let floor = self.floor().get();
        if finalized {
            VersionRange::between(floor, self.native.get().max(floor))
        } else {
            VersionRange::exactly(floor)
        }
    }
}

/// One namespace a publication writes, and the format it is stamped with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginWriterStamp {
    pub plugin: String,
    pub namespace: FormatNamespace,
    pub writer: FormatVersion,
}

/// Every plugin namespace one transaction publishes, read off the payload it
/// writes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PluginPublication {
    stamps: Vec<PluginWriterStamp>,
}

impl PluginPublication {
    /// Whether the transaction publishes no plugin namespace.
    pub fn is_empty(&self) -> bool {
        self.stamps.is_empty()
    }

    /// The published namespaces, each once.
    pub fn stamps(&self) -> &[PluginWriterStamp] {
        &self.stamps
    }

    /// The plugins the publication names, each once, in order.
    pub fn plugins(&self) -> Vec<&str> {
        let mut plugins: Vec<&str> = self
            .stamps
            .iter()
            .map(|stamp| stamp.plugin.as_str())
            .collect();
        plugins.sort_unstable();
        plugins.dedup();
        plugins
    }

    fn push(&mut self, plugin: &str, namespace: FormatNamespace, writer: FormatVersion) {
        let stamp = PluginWriterStamp {
            plugin: plugin.to_owned(),
            namespace,
            writer,
        };
        if !self.stamps.contains(&stamp) {
            self.stamps.push(stamp);
        }
    }

    /// Add every namespace of a recorded plugin configuration.
    pub fn add_config(&mut self, config: &crate::PluginConfig) {
        for (plugin, namespace) in config.namespaces() {
            self.push(plugin, FormatNamespace::Config, namespace.format_version);
        }
    }

    /// The namespaces a checkpoint publishes: its plugin namespace map when
    /// the commit carries a changed map. A map the commit only references
    /// was published, and admitted, by the commit that wrote it.
    pub fn add_checkpoint(
        &mut self,
        checkpoint: &super::HydratedSessionCheckpoint,
    ) -> Result<(), StoreError> {
        let Some(super::HydratedCheckpointComponent::Changed { body, .. }) =
            checkpoint.component(super::PLUGIN_STATE_CHECKPOINT_COMPONENT)
        else {
            return Ok(());
        };
        let map: PluginStateMap =
            rmp_serde::from_slice(body).map_err(|error| StoreError::StoredDataCorrupt {
                record_kind: "SessionCheckpoint component",
                message: format!(
                    "the published `{}` body carries no readable format stamps: {error}",
                    super::PLUGIN_STATE_CHECKPOINT_COMPONENT
                ),
            })?;
        for (plugin, entry) in &map.plugins {
            self.push(plugin, FormatNamespace::State, entry.format_version);
        }
        Ok(())
    }

    /// The namespaces a runtime commit publishes: its changed plugin-state
    /// component and the plugin configuration of the config it records.
    pub fn of_runtime_commit(commit: &super::RuntimeCommit) -> Result<Self, StoreError> {
        let mut publication = Self::default();
        publication.add_checkpoint(&commit.checkpoint)?;
        publication.add_config(&commit.config.plugin_config);
        if let Some(config) = &commit.execution_config {
            publication.add_config(&config.plugin_config);
        }
        Ok(publication)
    }

    /// The namespaces a recorded session config publishes.
    pub fn of_session_config(config: &crate::PersistedSessionConfig) -> Self {
        let mut publication = Self::default();
        publication.add_config(&config.plugin_config);
        publication
    }

    /// The config formats a process environment publishes, read off its
    /// store bytes: one stamp per named component, under the component's
    /// own writer key (`component:<id>`), so a component and a session
    /// plugin of the same id never share a format range.
    pub fn of_process_env(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        let env = crate::process_env::ProcessEnv::from_store_bytes(bytes)?;
        let mut publication = Self::default();
        for entry in env.components() {
            publication.push(
                &entry.id().writer_key(),
                FormatNamespace::Config,
                entry.config().format_version(),
            );
        }
        Ok(publication)
    }
}

/// The writer range the fleet record carries for each plugin id.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct PluginWriterRanges {
    ranges: BTreeMap<String, VersionRange>,
}

impl PluginWriterRanges {
    /// The ranges a store recorded as `(plugin, min, max)` rows. A row whose
    /// bounds are not a range is refused typed: the record is never repaired
    /// or skipped.
    pub fn from_rows(
        rows: impl IntoIterator<Item = (String, i64, i64)>,
    ) -> Result<Self, CompatRefusal> {
        let mut ranges = BTreeMap::new();
        for (plugin, min, max) in rows {
            let range = match (u32::try_from(min), u32::try_from(max)) {
                (Ok(min), Ok(max)) => VersionRange::new(min, max).map_err(|error| {
                    CompatRefusal::PluginWriterRangeMalformed {
                        plugin: plugin.clone(),
                        detail: error.to_string(),
                    }
                })?,
                _ => {
                    return Err(CompatRefusal::PluginWriterRangeMalformed {
                        plugin,
                        detail: format!("bounds [{min},{max}] are not format versions"),
                    });
                }
            };
            ranges.insert(plugin, range);
        }
        Ok(Self { ranges })
    }

    /// The recorded ranges, by plugin id.
    pub fn iter(&self) -> impl Iterator<Item = (&str, VersionRange)> {
        self.ranges
            .iter()
            .map(|(plugin, range)| (plugin.as_str(), *range))
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// The range the fleet permits `plugin` to write. A plugin the record
    /// does not name is refused typed.
    pub fn permitted_writer(&self, plugin: &str) -> Result<VersionRange, CompatRefusal> {
        self.ranges
            .get(plugin)
            .copied()
            .ok_or_else(|| CompatRefusal::PluginWriterUnprovisioned {
                plugin: plugin.to_owned(),
            })
    }

    /// Admit `publication` against the recorded ranges. It answers the
    /// entries the publication provisions: a plugin the record does not name
    /// that publishes its first format records `[1, 1]`. Any other stamp
    /// outside its plugin's range is refused, and the caller writes nothing.
    pub fn admit(
        &self,
        publication: &PluginPublication,
    ) -> Result<BTreeMap<String, VersionRange>, CompatRefusal> {
        let mut seeded = BTreeMap::new();
        for stamp in publication.stamps() {
            let permitted = match self
                .ranges
                .get(&stamp.plugin)
                .or_else(|| seeded.get(&stamp.plugin))
            {
                Some(range) => *range,
                None if stamp.writer == FormatVersion::ONE => {
                    let range = VersionRange::exactly(FormatVersion::ONE.get());
                    seeded.insert(stamp.plugin.clone(), range);
                    range
                }
                None => {
                    return Err(CompatRefusal::PluginWriterUnprovisioned {
                        plugin: stamp.plugin.clone(),
                    });
                }
            };
            if !permitted.contains(stamp.writer.get()) {
                return Err(CompatRefusal::PluginWriterOutsideRange {
                    plugin: stamp.plugin.clone(),
                    namespace: stamp.namespace,
                    writer: stamp.writer.get(),
                    permitted,
                });
            }
        }
        Ok(seeded)
    }

    /// The entries a provisioning from `registrations` adds: one for each
    /// registered plugin the record does not name. A recorded range is never
    /// changed here. `finalized` says the fleet epoch is the provisioning
    /// build's own, so no older build writes beside it.
    pub fn provisioned(
        &self,
        registrations: &[PluginWriterRegistration],
        finalized: bool,
    ) -> BTreeMap<String, VersionRange> {
        registrations
            .iter()
            .filter(|registration| !self.ranges.contains_key(&registration.plugin))
            .map(|registration| (registration.plugin.clone(), registration.seed(finalized)))
            .collect()
    }

    /// The ranges with `entries` recorded over them.
    pub fn with(mut self, entries: BTreeMap<String, VersionRange>) -> Self {
        self.ranges.extend(entries);
        self
    }
}

/// One plugin of an admission's composition: its place in hook order, the
/// format its namespaces are written in.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedPlugin {
    pub plugin: String,
    /// The format the admission chose for this plugin's state and config
    /// namespaces: the highest one the plugin writes that the fleet record
    /// permitted when the admission was recorded.
    pub writer: FormatVersion,
}

/// What a segment admission records about its plugins (FIG-4747): the
/// ordered composition it runs and the writer format chosen for each plugin.
///
/// A plugin transition records this choice from the fleet record's ranges.
/// A replay encodes with that recorded choice: widening a range changes a
/// later admission's writer, never an already-recorded publication's format.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct PluginAdmission {
    plugins: Vec<AdmittedPlugin>,
}

impl PluginAdmission {
    /// Choose each plugin's writer from `ranges`: the highest format in both
    /// its writable set and the range the fleet record permits it.
    /// `composition` is the admitting build's plugins in hook order, each
    /// with its namespace formats. A plugin that writes no permitted format
    /// is refused typed, and so is one the record does not name.
    pub fn choose<'a>(
        composition: impl IntoIterator<Item = &'a PluginWriterRegistration>,
        ranges: &PluginWriterRanges,
    ) -> Result<Self, CompatRefusal> {
        let mut plugins = Vec::new();
        for registration in composition {
            let permitted = ranges.permitted_writer(&registration.plugin)?;
            let writer = registration
                .writable
                .iter()
                .copied()
                .filter(|writable| permitted.contains(writable.get()))
                .max()
                .ok_or_else(|| CompatRefusal::PluginWriterUnwritable {
                    plugin: registration.plugin.clone(),
                    writable: registration
                        .writable
                        .iter()
                        .map(|writable| writable.get())
                        .collect(),
                    permitted,
                })?;
            plugins.push(AdmittedPlugin {
                plugin: registration.plugin.clone(),
                writer,
            });
        }
        Ok(Self { plugins })
    }

    /// An admission read back from its recorded plugins, in hook order.
    pub fn from_plugins(plugins: Vec<AdmittedPlugin>) -> Self {
        Self { plugins }
    }

    /// The admitted plugins, in hook order.
    pub fn plugins(&self) -> &[AdmittedPlugin] {
        &self.plugins
    }

    /// Whether the admission names no plugin.
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// The writer the admission recorded for `plugin`, if it names it.
    pub fn writer(&self, plugin: &str) -> Option<FormatVersion> {
        self.plugins
            .iter()
            .find(|admitted| admitted.plugin == plugin)
            .map(|admitted| admitted.writer)
    }

    /// The recorded writer of every admitted plugin, by plugin id.
    pub fn writers(&self) -> BTreeMap<String, FormatVersion> {
        self.plugins
            .iter()
            .map(|admitted| (admitted.plugin.clone(), admitted.writer))
            .collect()
    }
}

pub use lash_vocabulary::providers::{
    PluginCallbackIdentity, declared_tool_provider, is_declared_tool_provider_key,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn version(value: u32) -> FormatVersion {
        FormatVersion::new(value).expect("a format version")
    }

    fn registration(plugin: &str, native: u32, writable: &[u32]) -> PluginWriterRegistration {
        PluginWriterRegistration {
            plugin: plugin.to_owned(),
            native: version(native),
            writable: writable.iter().copied().map(version).collect(),
        }
    }

    fn publication(stamps: &[(&str, FormatNamespace, u32)]) -> PluginPublication {
        let mut publication = PluginPublication::default();
        for (plugin, namespace, writer) in stamps {
            publication.push(plugin, *namespace, version(*writer));
        }
        publication
    }

    fn ranges(rows: &[(&str, u32, u32)]) -> PluginWriterRanges {
        PluginWriterRanges::from_rows(
            rows.iter()
                .map(|(plugin, min, max)| ((*plugin).to_owned(), i64::from(*min), i64::from(*max))),
        )
        .expect("well-formed ranges")
    }

    #[test]
    fn a_malformed_range_is_refused() {
        for (min, max) in [(0, 1), (2, 1), (-1, 1), (1, i64::from(u32::MAX) + 1)] {
            assert!(matches!(
                PluginWriterRanges::from_rows([("probe".to_owned(), min, max)]),
                Err(CompatRefusal::PluginWriterRangeMalformed { plugin, .. }) if plugin == "probe"
            ));
        }
    }

    #[test]
    fn provisioning_seeds_the_floor_inside_a_window_and_never_moves_a_range() {
        let recorded = ranges(&[("old", 1, 1)]);
        let registrations = [
            registration("old", 2, &[1, 2]),
            registration("new", 2, &[1, 2]),
        ];
        assert_eq!(
            recorded.provisioned(&registrations, false),
            BTreeMap::from([("new".to_owned(), VersionRange::exactly(1))])
        );
        assert_eq!(
            recorded.provisioned(&registrations, true),
            BTreeMap::from([("new".to_owned(), VersionRange::between(1, 2))])
        );
    }

    #[test]
    fn a_runtime_commit_publication_reads_its_stamps_off_the_payload() {
        let mut state = PluginStateMap::default();
        state.plugins.insert(
            "probe".into(),
            crate::plugin_state::PluginNamespaceState {
                format_version: version(2),
                generation: 1,
                ..Default::default()
            }
            .entry(crate::plugin_state::NamespaceBody::encode(&BTreeMap::new()).values),
        );
        let mut checkpoint = super::super::HydratedSessionCheckpoint::default();
        checkpoint.components.insert(
            super::super::PLUGIN_STATE_CHECKPOINT_COMPONENT.to_owned(),
            super::super::HydratedCheckpointComponent::changed(
                rmp_serde::to_vec_named(&state).expect("encode plugin state"),
            ),
        );
        let mut found = PluginPublication::default();
        found.add_checkpoint(&checkpoint).expect("readable stamps");
        assert_eq!(found, publication(&[("probe", FormatNamespace::State, 2)]));

        checkpoint.components.insert(
            super::super::PLUGIN_STATE_CHECKPOINT_COMPONENT.to_owned(),
            super::super::HydratedCheckpointComponent::changed(b"not a plugin state".to_vec()),
        );
        assert!(matches!(
            PluginPublication::default().add_checkpoint(&checkpoint),
            Err(StoreError::StoredDataCorrupt { .. })
        ));

        let mut config = crate::PluginConfig::default();
        config.insert_versioned("probe", version(3), serde_json::json!({}));
        let mut found = PluginPublication::default();
        found.add_config(&config);
        assert_eq!(found, publication(&[("probe", FormatNamespace::Config, 3)]));
    }

    #[test]
    fn an_admission_chooses_the_highest_permitted_writable_format() {
        let probe = registration("probe", 3, &[1, 2, 3]);
        let other = registration("other", 1, &[1]);
        let composition = [&probe, &other];
        let window = ranges(&[("probe", 1, 2), ("other", 1, 1)]);
        let admitted = PluginAdmission::choose(composition, &window).expect("admitted");
        assert_eq!(
            admitted
                .plugins()
                .iter()
                .map(|plugin| (plugin.plugin.as_str(), plugin.writer.get()))
                .collect::<Vec<_>>(),
            vec![("probe", 2), ("other", 1)]
        );
        assert_eq!(admitted.writer("probe"), Some(version(2)));
        assert_eq!(admitted.writer("absent"), None);

        let finalized = ranges(&[("probe", 1, 3), ("other", 1, 1)]);
        assert_eq!(
            PluginAdmission::choose(composition, &finalized)
                .expect("admitted")
                .writer("probe"),
            Some(version(3))
        );
    }

    #[test]
    fn an_admission_refuses_a_plugin_with_no_permitted_writer() {
        let probe = registration("probe", 3, &[3]);
        let composition = [&probe];
        assert_eq!(
            PluginAdmission::choose(composition, &ranges(&[("probe", 1, 2)])),
            Err(CompatRefusal::PluginWriterUnwritable {
                plugin: "probe".into(),
                writable: vec![3],
                permitted: VersionRange::between(1, 2),
            })
        );
        assert_eq!(
            PluginAdmission::choose(composition, &PluginWriterRanges::default()),
            Err(CompatRefusal::PluginWriterUnprovisioned {
                plugin: "probe".into()
            })
        );
    }
}
