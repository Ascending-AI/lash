//! The one canonical encoding of a `Value::Projected` shared by both durable
//! writers.
//!
//! The VM continuation wire and the `State` snapshot wire encode a projection
//! the same way, so a value that one writer accepts the other accepts
//! (FIG-2865).
//!
//! A projection is plain data (ADR 0132 §9): a resource projection is its
//! `name`, its declared `type_name` and its [`ResourceRef`], which is all the
//! VM ever holds of it, so it decodes into the same projection on any node and
//! reads through the provider registered for its type there. A scalar
//! projection is its value: the writers encode the value itself, and the
//! host's binding re-occupies its named slot on resume.

use serde::{Deserialize, Serialize};

use super::{ContinuationError, ProjectedValue, ResourceRef};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CanonicalProjectedValue {
    pub(crate) name: String,
    pub(crate) type_name: String,
    pub(crate) resource: ResourceRef,
}

impl CanonicalProjectedValue {
    /// The canonical form of a resource projection.
    ///
    /// # Errors
    ///
    /// A scalar projection has none: its writer encodes its value instead.
    pub(crate) fn from_projected(
        projected: &ProjectedValue,
        location: &str,
    ) -> Result<Self, ContinuationError> {
        let resource =
            projected
                .resource_ref()
                .ok_or_else(|| ContinuationError::UnserializableValue {
                    location: location.to_string(),
                    variant: "scalar projection, which is encoded by its value",
                })?;
        Ok(Self {
            name: projected.name().to_string(),
            type_name: projected.value_type_name().to_string(),
            resource: resource.clone(),
        })
    }

    pub(crate) fn into_projected(self) -> ProjectedValue {
        ProjectedValue::resource(self.name, self.type_name, self.resource)
    }
}
