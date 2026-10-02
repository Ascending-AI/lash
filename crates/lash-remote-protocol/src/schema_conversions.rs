//! Wire conversions for the schema contract vocabulary. The types live in lash-sansio, so
//! these hold in every feature variant, not only with `core-conversions`.

use super::*;

impl From<lash_sansio::ProjectionMode> for RemoteProjectionMode {
    fn from(value: lash_sansio::ProjectionMode) -> Self {
        match value {
            lash_sansio::ProjectionMode::Auto => Self::Auto,
            lash_sansio::ProjectionMode::ExplicitOnly => Self::ExplicitOnly,
            lash_sansio::ProjectionMode::Exact => Self::Exact,
        }
    }
}

impl From<RemoteProjectionMode> for lash_sansio::ProjectionMode {
    fn from(value: RemoteProjectionMode) -> Self {
        match value {
            RemoteProjectionMode::Auto => Self::Auto,
            RemoteProjectionMode::ExplicitOnly => Self::ExplicitOnly,
            RemoteProjectionMode::Exact => Self::Exact,
        }
    }
}

impl From<lash_sansio::SchemaProjectionOverride> for RemoteSchemaProjectionOverride {
    fn from(value: lash_sansio::SchemaProjectionOverride) -> Self {
        let lash_sansio::SchemaProjectionOverride { dialect, schema } = value;
        Self { dialect, schema }
    }
}

impl From<RemoteSchemaProjectionOverride> for lash_sansio::SchemaProjectionOverride {
    fn from(value: RemoteSchemaProjectionOverride) -> Self {
        let RemoteSchemaProjectionOverride { dialect, schema } = value;
        Self { dialect, schema }
    }
}

impl From<lash_sansio::SchemaProjectionPolicy> for RemoteSchemaProjectionPolicy {
    fn from(value: lash_sansio::SchemaProjectionPolicy) -> Self {
        Self {
            mode: value.mode.into(),
            overrides: value.overrides.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<RemoteSchemaProjectionPolicy> for lash_sansio::SchemaProjectionPolicy {
    fn from(value: RemoteSchemaProjectionPolicy) -> Self {
        Self {
            mode: value.mode.into(),
            overrides: value.overrides.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<lash_sansio::SchemaContract> for RemoteSchemaContract {
    fn from(value: lash_sansio::SchemaContract) -> Self {
        Self {
            canonical: value.canonical,
            projection: value.projection.into(),
        }
    }
}

impl From<RemoteSchemaContract> for lash_sansio::SchemaContract {
    fn from(value: RemoteSchemaContract) -> Self {
        lash_sansio::SchemaContract {
            canonical: value.canonical,
            projection: value.projection.into(),
        }
    }
}
