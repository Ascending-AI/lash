//! What a worker may ask for, and how the parent decides whether it may.
//!
//! A worker's [`EffectRequest`](lash_vm_protocol::EffectRequest) is a
//! *request*. Its payload names a binding, an operation, arguments or a
//! handle, and none of that confers authority: the broker resolves every
//! request against the execution context the parent admitted (its frozen
//! bindings) and the handles the parent itself granted, in scopes that are
//! still live. A request that fails any check is refused before anything is
//! dispatched, and no tool is invoked for it. The worker's lease, its bytes
//! and anything it claims about itself are never consulted.
//!
//! This is lash's internal authority model: which of the parent's own
//! bindings a run was admitted with. Whether a caller may use a tool at all
//! is the host's policy, decided before the bindings are frozen.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_sansio::ToolCallId;
use lash_vm_protocol::{EffectKind, EncodedPayload, FrameEpoch, OwnerEpoch, VmOwner};
use serde::{Deserialize, Serialize};

use crate::identity::CodeCallIdentities;

/// One invocation a worker requests: `operation` of the resource bound as
/// `binding`, with JSON arguments.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Invocation {
    pub binding: String,
    pub operation: String,
    pub arguments: serde_json::Value,
}

/// A worker's request, as its effect request's payload carries it. The
/// payload's shape must match the request's [`EffectKind`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum OperationRequest {
    /// One resource operation ([`EffectKind::ResourceOperation`]).
    Invoke(Invocation),
    /// An aggregate of resource operations issued as one command
    /// ([`EffectKind::ResourceOperationBatch`]); each member is a leaf.
    Aggregate { members: Vec<Invocation> },
    /// An await of a handle the parent granted ([`EffectKind::Await`]).
    Await { handle: String },
    /// A durable sleep ([`EffectKind::Sleep`]).
    Sleep { millis: u64 },
}

impl OperationRequest {
    /// The effect kind a request of this shape travels under.
    pub fn kind(&self) -> EffectKind {
        match self {
            Self::Invoke(_) => EffectKind::ResourceOperation,
            Self::Aggregate { .. } => EffectKind::ResourceOperationBatch,
            Self::Await { .. } => EffectKind::Await,
            Self::Sleep { .. } => EffectKind::Sleep,
        }
    }

    /// Encodes the request as an effect request's payload.
    pub fn encode(&self) -> EncodedPayload {
        EncodedPayload(rmp_serde::to_vec_named(self).unwrap_or_default())
    }

    /// Decodes an effect request's payload. The frame codec already bounded
    /// the bytes; a payload that is not exactly one request is refused.
    pub fn decode(payload: &EncodedPayload) -> Result<Self, AuthorityRefusal> {
        rmp_serde::from_slice(&payload.0).map_err(|error| AuthorityRefusal::Malformed {
            reason: error.to_string(),
        })
    }
}

/// Encodes a JSON value as an effect value payload.
pub fn encode_value(value: &serde_json::Value) -> EncodedPayload {
    EncodedPayload(rmp_serde::to_vec_named(value).unwrap_or_default())
}

/// Decodes an effect value payload as JSON.
pub fn decode_value(payload: &EncodedPayload) -> Result<serde_json::Value, String> {
    rmp_serde::from_slice(&payload.0).map_err(|error| error.to_string())
}

/// The tool a bound operation routes to: parent-owned, never sent to the
/// worker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRoute {
    pub tool_id: String,
    pub tool_name: String,
}

/// What a bound operation accepts as arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArgumentContract {
    /// Any JSON value.
    Any,
    /// A JSON object holding every `required` property, and no property
    /// outside `properties` unless `additional` allows it.
    Object {
        required: BTreeSet<String>,
        properties: BTreeSet<String>,
        additional: bool,
    },
}

impl ArgumentContract {
    fn check(&self, arguments: &serde_json::Value) -> Result<(), String> {
        match self {
            Self::Any => Ok(()),
            Self::Object {
                required,
                properties,
                additional,
            } => {
                let object = arguments
                    .as_object()
                    .ok_or_else(|| "the arguments are not an object".to_string())?;
                if let Some(missing) = required.iter().find(|name| !object.contains_key(*name)) {
                    return Err(format!("the arguments lack `{missing}`"));
                }
                if !additional
                    && let Some(extra) = object
                        .keys()
                        .find(|name| !properties.contains(*name) && !required.contains(*name))
                {
                    return Err(format!("the arguments carry an undeclared `{extra}`"));
                }
                Ok(())
            }
        }
    }
}

/// One operation a binding exposes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundOperation {
    pub tool: ToolRoute,
    pub arguments: ArgumentContract,
}

/// The bindings a run was admitted with, frozen for the run: a binding added
/// or changed afterwards is not this run's.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FrozenBindings {
    bindings: BTreeMap<String, BTreeMap<String, BoundOperation>>,
}

impl FrozenBindings {
    pub fn new() -> Self {
        Self::default()
    }

    /// Binds `operation` of `binding`.
    pub fn bind(
        mut self,
        binding: impl Into<String>,
        operation: impl Into<String>,
        bound: BoundOperation,
    ) -> Self {
        self.bindings
            .entry(binding.into())
            .or_default()
            .insert(operation.into(), bound);
        self
    }

    fn operation(
        &self,
        binding: &str,
        operation: &str,
    ) -> Result<&BoundOperation, AuthorityRefusal> {
        let operations =
            self.bindings
                .get(binding)
                .ok_or_else(|| AuthorityRefusal::UnknownBinding {
                    binding: binding.to_string(),
                })?;
        operations
            .get(operation)
            .ok_or_else(|| AuthorityRefusal::UnknownOperation {
                binding: binding.to_string(),
                operation: operation.to_string(),
            })
    }
}

/// The execution context a run was admitted under: whose it is, the
/// identities its commands take, and the bindings it may use. Built by the
/// parent from its own admission; nothing in it comes from a worker.
#[derive(Clone, Debug)]
pub struct AdmittedContext {
    pub owner: VmOwner,
    pub owner_epoch: OwnerEpoch,
    pub identities: CodeCallIdentities,
    pub bindings: Arc<FrozenBindings>,
}

/// A handle the parent granted when an operation it performed answered one:
/// which command granted it, and the frame it is scoped to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandleGrant {
    pub ordinal: u64,
    pub call_id: ToolCallId,
    pub frame_epoch: FrameEpoch,
}

/// Why a request was refused. A refused request dispatches nothing.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "refusal")]
pub enum AuthorityRefusal {
    #[error("the request is malformed: {reason}")]
    Malformed { reason: String },
    #[error("the request's payload is a {payload:?} request under a {kind:?} header")]
    KindMismatch {
        kind: EffectKind,
        payload: EffectKind,
    },
    #[error("{kind:?} requests are not brokered")]
    Unsupported { kind: EffectKind },
    #[error("no binding `{binding}` was admitted with this run")]
    UnknownBinding { binding: String },
    #[error("binding `{binding}` exposes no operation `{operation}`")]
    UnknownOperation { binding: String, operation: String },
    #[error("the arguments of `{binding}.{operation}` are refused: {reason}")]
    ArgumentsRefused {
        binding: String,
        operation: String,
        reason: String,
    },
    #[error("an aggregate names no member")]
    EmptyAggregate,
    #[error("handle `{handle}` was never granted to this run")]
    UnknownHandle { handle: String },
    #[error("handle `{handle}` belongs to a retired frame")]
    RetiredScope { handle: String },
    #[error(
        "the request differs from the one retained under its call {call_id} (retained {retained}, requested {requested})"
    )]
    RetainedRequestDrift {
        call_id: ToolCallId,
        retained: String,
        requested: String,
    },
}

impl AuthorityRefusal {
    /// The refusal as the failed outcome the guest may catch.
    pub fn as_payload(&self) -> EncodedPayload {
        encode_value(&serde_json::json!({
            "code": "lash_vm_request_refused",
            "message": self.to_string(),
            "refusal": serde_json::to_value(self).unwrap_or(serde_json::Value::Null),
        }))
    }
}

/// One call an admitted request resolved to.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedCall {
    pub binding: String,
    pub operation: String,
    pub tool: ToolRoute,
    pub arguments: serde_json::Value,
}

/// A request resolved against the admitted context, before the parent gives
/// it an ordinal.
#[derive(Clone, Debug, PartialEq)]
pub enum ResolvedRequest {
    Invoke(ResolvedCall),
    Aggregate(Vec<ResolvedCall>),
    Await { handle: String, grant: HandleGrant },
    Sleep { millis: u64 },
}

/// Resolves a worker's request against the admitted context, the handles
/// the parent granted and the frame the run is in. Invokes nothing.
pub fn resolve(
    context: &AdmittedContext,
    grants: &BTreeMap<String, HandleGrant>,
    frame_epoch: FrameEpoch,
    kind: EffectKind,
    payload: &EncodedPayload,
) -> Result<ResolvedRequest, AuthorityRefusal> {
    if !matches!(
        kind,
        EffectKind::ResourceOperation
            | EffectKind::ResourceOperationBatch
            | EffectKind::Await
            | EffectKind::Sleep
    ) {
        return Err(AuthorityRefusal::Unsupported { kind });
    }
    let request = OperationRequest::decode(payload)?;
    if request.kind() != kind {
        return Err(AuthorityRefusal::KindMismatch {
            kind,
            payload: request.kind(),
        });
    }
    let call = |invocation: Invocation| -> Result<ResolvedCall, AuthorityRefusal> {
        let bound = context
            .bindings
            .operation(&invocation.binding, &invocation.operation)?;
        bound
            .arguments
            .check(&invocation.arguments)
            .map_err(|reason| AuthorityRefusal::ArgumentsRefused {
                binding: invocation.binding.clone(),
                operation: invocation.operation.clone(),
                reason,
            })?;
        Ok(ResolvedCall {
            tool: bound.tool.clone(),
            binding: invocation.binding,
            operation: invocation.operation,
            arguments: invocation.arguments,
        })
    };
    match request {
        OperationRequest::Invoke(invocation) => Ok(ResolvedRequest::Invoke(call(invocation)?)),
        OperationRequest::Aggregate { members } => {
            if members.is_empty() {
                return Err(AuthorityRefusal::EmptyAggregate);
            }
            Ok(ResolvedRequest::Aggregate(
                members.into_iter().map(call).collect::<Result<_, _>>()?,
            ))
        }
        OperationRequest::Await { handle } => {
            let grant = grants
                .get(&handle)
                .ok_or_else(|| AuthorityRefusal::UnknownHandle {
                    handle: handle.clone(),
                })?;
            if grant.frame_epoch != frame_epoch {
                return Err(AuthorityRefusal::RetiredScope { handle });
            }
            Ok(ResolvedRequest::Await {
                grant: grant.clone(),
                handle,
            })
        }
        OperationRequest::Sleep { millis } => Ok(ResolvedRequest::Sleep { millis }),
    }
}

/// The digest of a request's canonical content: the tools, operations and
/// arguments it names, or the handle it awaits. Keyed by the domain, so no
/// other digest collides with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RequestFingerprint(#[serde(with = "hex_digest")] [u8; 32]);

impl RequestFingerprint {
    pub fn of(request: &ResolvedRequest) -> Self {
        let mut canonical = String::new();
        let call = |canonical: &mut String, call: &ResolvedCall| {
            canonical.push_str(&format!(
                "call:{}:{}:{}:{}:{}:{}:{}:{}:",
                call.tool.tool_id.len(),
                call.tool.tool_id,
                call.binding.len(),
                call.binding,
                call.operation.len(),
                call.operation,
                call.tool.tool_name.len(),
                call.tool.tool_name,
            ));
            canonical_json(&call.arguments, canonical);
        };
        match request {
            ResolvedRequest::Invoke(invocation) => call(&mut canonical, invocation),
            ResolvedRequest::Aggregate(members) => {
                canonical.push_str(&format!("aggregate:{}:", members.len()));
                for member in members {
                    call(&mut canonical, member);
                }
            }
            ResolvedRequest::Await { handle, .. } => {
                canonical.push_str(&format!("await:{}:{handle}", handle.len()));
            }
            ResolvedRequest::Sleep { millis } => canonical.push_str(&format!("sleep:{millis}")),
        }
        Self(
            *blake3::Hasher::new_derive_key("lash-vm-broker request fingerprint v1")
                .update(canonical.as_bytes())
                .finalize()
                .as_bytes(),
        )
    }

    pub fn to_hex(&self) -> String {
        hex_digest::encode(&self.0)
    }
}

impl std::fmt::Display for RequestFingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// JSON in one spelling whatever the map order: object keys sorted, every
/// string length-prefixed.
fn canonical_json(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::Null => out.push('n'),
        serde_json::Value::Bool(value) => out.push(if *value { 't' } else { 'f' }),
        serde_json::Value::Number(number) => {
            let text = number.to_string();
            out.push_str(&format!("#{}:{text}", text.len()));
        }
        serde_json::Value::String(text) => out.push_str(&format!("s{}:{text}", text.len())),
        serde_json::Value::Array(items) => {
            out.push_str(&format!("[{}:", items.len()));
            for item in items {
                canonical_json(item, out);
            }
            out.push(']');
        }
        serde_json::Value::Object(object) => {
            let sorted = object.iter().collect::<BTreeMap<_, _>>();
            out.push_str(&format!("{{{}:", sorted.len()));
            for (key, value) in sorted {
                out.push_str(&format!("k{}:{key}", key.len()));
                canonical_json(value, out);
            }
            out.push('}');
        }
    }
}

mod hex_digest {
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn encode(bytes: &[u8; 32]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    pub(super) fn serialize<S: Serializer>(
        bytes: &[u8; 32],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&encode(bytes))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<[u8; 32], D::Error> {
        let text = String::deserialize(deserializer)?;
        let mut digest = [0_u8; 32];
        if text.len() != 64 {
            return Err(serde::de::Error::custom("a fingerprint is 64 hex digits"));
        }
        for (index, slot) in digest.iter_mut().enumerate() {
            *slot = text
                .get(index * 2..index * 2 + 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                .ok_or_else(|| serde::de::Error::custom("a fingerprint is 64 hex digits"))?;
        }
        Ok(digest)
    }
}

#[cfg(test)]
mod tests;
