//! The RLM authority split (FIG-4158, ADR 0123).
//!
//! The durable RLM root carries two kinds of state, and only one of them may
//! reach the worker that runs model code:
//!
//! - **Guest state**, the worker's: the Lashlang durable header and one
//!   fragment per binding — the guest heap and its roots. It crosses as
//!   [`RlmWorkerEnvelope`] (parent to worker) and comes back as
//!   [`RlmWorkerCapture`] (worker to parent).
//! - **Authority**, the parent's: the deferred tool resolutions with their
//!   [`ToolGrant`](lash_lashlang_runtime::ToolGrant)s and host-owned
//!   `execution_binding`s, and the deferred trigger resolutions with their
//!   routes. They never cross, in either direction.
//!
//! Both envelope types deny unknown fields, so bytes a worker returns cannot
//! carry authority in: a capture naming a grant is refused, and the root the
//! parent assembles takes its resolutions from its own state only.
//!
//! Until execution moves out of process (tsvm-d), the parent calls the
//! `worker_side` functions in process; they are the only code here that
//! decodes guest state semantically.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;

/// The guest state the parent hands a worker: the durable header and each
/// binding's fragment body, and nothing else.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RlmWorkerEnvelope {
    pub(crate) state_header: ByteBuf,
    pub(crate) globals: BTreeMap<String, ByteBuf>,
}

/// The guest state a worker hands back after a run: the durable header, the
/// fragments that changed since the parent's last capture, and the names
/// whose fragments did not.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RlmWorkerCapture {
    pub(crate) state_header: ByteBuf,
    pub(crate) changed: BTreeMap<String, ByteBuf>,
    pub(crate) unchanged: BTreeSet<String>,
}

/// A worker-bound or worker-returned envelope that failed to decode.
#[derive(Debug, thiserror::Error)]
#[error("RLM worker envelope is malformed: {details}")]
pub(crate) struct RlmWorkerEnvelopeRefusal {
    pub(crate) details: String,
}

impl RlmWorkerEnvelope {
    pub(crate) fn encode(&self) -> Vec<u8> {
        encode(self)
    }
}

impl RlmWorkerCapture {
    pub(crate) fn encode(&self) -> Vec<u8> {
        encode(self)
    }

    /// The parent's decode of what a worker returned: structural only, and
    /// refusing any field the capture does not define.
    pub(crate) fn accept(bytes: &[u8]) -> Result<Self, RlmWorkerEnvelopeRefusal> {
        decode(bytes)
    }
}

#[expect(
    clippy::expect_used,
    reason = "a map of byte strings always encodes as named MessagePack"
)]
fn encode<T: Serialize>(envelope: &T) -> Vec<u8> {
    rmp_serde::to_vec_named(envelope).expect("an RLM worker envelope encodes")
}

fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, RlmWorkerEnvelopeRefusal> {
    rmp_serde::from_slice(bytes).map_err(|error| RlmWorkerEnvelopeRefusal {
        details: error.to_string(),
    })
}

/// What the worker does with guest state: install it on its instance, and
/// capture it back as fragments.
pub(super) mod worker_side {
    use super::{RlmWorkerCapture, RlmWorkerEnvelope, RlmWorkerEnvelopeRefusal, decode};
    use lashlang::{DurableBaseline, DurableFragment};
    use serde_bytes::ByteBuf;

    /// Installs a worker envelope on `vm`: the semantic decode of every
    /// fragment, regular expressions included, happens here.
    pub(in crate::executor) fn install(
        vm: &mut lashlang::VmInstance,
        envelope: &[u8],
        fleet_format: lash_core::FleetFormat,
    ) -> Result<DurableBaseline, InstallError> {
        let envelope: RlmWorkerEnvelope = decode(envelope).map_err(InstallError::Envelope)?;
        let fragments = envelope
            .globals
            .iter()
            .map(|(name, body)| (name.as_str(), body.as_slice()));
        vm.restore_durable_parts(&envelope.state_header, fragments, fleet_format)
            .map_err(InstallError::Snapshot)
    }

    #[derive(Debug)]
    pub(in crate::executor) enum InstallError {
        Envelope(RlmWorkerEnvelopeRefusal),
        Snapshot(lashlang::SnapshotDecodeError),
    }

    /// The worker's capture of its guest state relative to `since`: the
    /// encoded capture, and the baseline the next capture diffs against.
    pub(in crate::executor) fn capture(
        vm: &lashlang::VmInstance,
        since: &DurableBaseline,
        fleet_format: lash_core::FleetFormat,
    ) -> Result<(Vec<u8>, DurableBaseline), lashlang::ContinuationError> {
        let parts = vm.state().durable_parts(since, fleet_format)?;
        let mut capture = RlmWorkerCapture {
            state_header: ByteBuf::from(parts.header),
            changed: Default::default(),
            unchanged: Default::default(),
        };
        for (name, fragment) in parts.fragments {
            match fragment {
                DurableFragment::Changed(body) => {
                    capture.changed.insert(name, ByteBuf::from(body));
                }
                DurableFragment::Unchanged => {
                    capture.unchanged.insert(name);
                }
            }
        }
        Ok((capture.encode(), parts.baseline))
    }
}
