//! The one canonical encoding of a `Value::Projected` shared by both durable
//! writers.
//!
//! The VM continuation wire and the `State` snapshot wire encode a projection
//! the same way, so a value that one writer accepts the other accepts
//! (FIG-2865).
//!
//! A projection is plain data (ADR 0132 §9) and one leaf of the encoding, a
//! scalar member wherever it sits: a heap object, an array or a closure
//! capture holds it as it holds a number. A resource projection is its
//! `name`, its declared `type_name` and its [`ResourceRef`], which is all the
//! VM ever holds of it, so it decodes into the same projection on any node and
//! reads through the provider registered for its type there. A scalar
//! projection is its `name` and the host value it stands for, written in the
//! writer's own value encoding `V`. That value is host data and holds no heap
//! reference: each writer refuses one on both sides, because heap tracing
//! never looks inside a projection.

use serde::{Deserialize, Serialize};

use super::value::ProjectedForm;
use super::{ProjectedValue, ResourceRef, Value};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum CanonicalProjectedValue<V> {
    Scalar {
        name: String,
        value: Box<V>,
    },
    Resource {
        name: String,
        type_name: String,
        resource: ResourceRef,
    },
}

impl<V> CanonicalProjectedValue<V> {
    /// The canonical form of `projected`, writing a scalar projection's value
    /// with `encode`.
    pub(crate) fn from_projected<E>(
        projected: &ProjectedValue,
        encode: impl FnOnce(&Value) -> Result<V, E>,
    ) -> Result<Self, E> {
        Ok(match projected.form() {
            ProjectedForm::Scalar(value) => Self::Scalar {
                name: projected.name().to_string(),
                value: Box::new(encode(value)?),
            },
            ProjectedForm::Resource {
                type_name,
                resource,
            } => Self::Resource {
                name: projected.name().to_string(),
                type_name: type_name.to_string(),
                resource: resource.clone(),
            },
        })
    }

    /// The projection this form encodes, reading a scalar projection's value
    /// with `decode`.
    pub(crate) fn into_projected<E>(
        self,
        decode: impl FnOnce(V) -> Result<Value, E>,
    ) -> Result<ProjectedValue, E> {
        Ok(match self {
            Self::Scalar { name, value } => ProjectedValue::scalar(name, decode(*value)?),
            Self::Resource {
                name,
                type_name,
                resource,
            } => ProjectedValue::resource(name, type_name, resource),
        })
    }
}
