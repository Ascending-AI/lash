//! The durable formats this engine writes, as rows for the facade's
//! durable-format table.
//!
//! The facade names no engine (ADR 0104 §2): these formats used to be
//! `DurableFormat` variants spelled `Restate*`, which put the engine's own
//! name in kernel-facing vocabulary. The engine contributes them as rows
//! from [`durable_formats`] instead, and the facade's table chains them on
//! when the `restate` feature is on. A caller that needs to name one names
//! its [`EngineDurableFormat::id`]; no `Restate*` identifier is exported to
//! the kernel.

use lash_core::engine::UpgradePolicy;

use crate::controller::{EFFECT_JOURNAL_VERSION, PROCESS_COMMAND_JOURNAL_PAYLOAD_VERSION};
use crate::durable_wait::{DURABLE_WAIT_REGISTRY_FORMAT_VERSION, DURABLE_WAIT_REQUEST_VERSION};
use crate::effect_group::{
    EFFECT_GROUP_DISPATCH_JOURNAL_VERSION, EFFECT_GROUP_PAYLOAD_FORMAT_VERSION,
    EFFECT_GROUP_STATE_FORMAT_VERSION, EFFECT_GROUP_WIRE_VERSION,
};
use crate::process::RESTATE_PROCESS_JOURNAL_VERSION;
use crate::session_driver::LASH_SESSION_DRIVE_VERSION;

// The rows are comparable counters whose bytes live in the engine's
// deployment, so no bounded walk of lash's own store enumerates them.
const UNWALKABLE_REASON: &str = "no bounded surface: Restate journal and object state live in \
     the Restate deployment, outside lash's own store";

/// One durable format the engine writes, as it registers the format with
/// the facade's durable-format table (ADR 0104 §2).
///
/// The row is deliberately narrower than the facade's `DurableFormatEntry`:
/// every engine format is a comparable version counter whose bytes live in
/// the engine's deployment, so the probe shape is fixed and the row carries
/// only the facts the table cannot know — the id, the operator-facing
/// name, the version, the owning constant and the format's upgrade policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct EngineDurableFormat {
    /// The opaque, engine-neutral id the facade names this format by.
    /// Callers that need to name an engine format do it through this id,
    /// never an engine-spelled `DurableFormat` variant.
    pub id: &'static str,
    /// The operator-facing name used in preflight reports.
    pub name: &'static str,
    /// The version this build writes.
    pub version: u32,
    /// The owning constant's name, so a refusal can be traced to source.
    pub constant: &'static str,
    /// How this format's stored bytes move to a newer build (ADR 0106 §2).
    /// The engine declares it: they are the engine's bytes the policy
    /// describes, and the build's drain generation depends on the answer.
    pub upgrade_policy: UpgradePolicy,
    /// Why no bounded preflight surface enumerates this format: the
    /// journal and object state it gates live in the engine's deployment,
    /// outside lash's own store.
    pub unwalkable_reason: &'static str,
}

/// The engine's durable-format rows, in the order the facade's format
/// table reports them.
static DURABLE_FORMATS: &[EngineDurableFormat] = &[
    EngineDurableFormat {
        id: "restate.durable_wait_request",
        name: "Restate durable-wait request",
        version: DURABLE_WAIT_REQUEST_VERSION as u32,
        constant: "DURABLE_WAIT_REQUEST_VERSION",
        upgrade_policy: UpgradePolicy::Drain,
        unwalkable_reason: UNWALKABLE_REASON,
    },
    EngineDurableFormat {
        id: "restate.durable_wait_registry_format",
        name: "Restate durable-wait registry format",
        version: DURABLE_WAIT_REGISTRY_FORMAT_VERSION as u32,
        constant: "DURABLE_WAIT_REGISTRY_FORMAT_VERSION",
        upgrade_policy: UpgradePolicy::Migrate,
        unwalkable_reason: UNWALKABLE_REASON,
    },
    EngineDurableFormat {
        id: "restate.process_command_journal",
        name: "Restate process-command journal",
        version: PROCESS_COMMAND_JOURNAL_PAYLOAD_VERSION,
        constant: "PROCESS_COMMAND_JOURNAL_PAYLOAD_VERSION",
        upgrade_policy: UpgradePolicy::Drain,
        unwalkable_reason: UNWALKABLE_REASON,
    },
    EngineDurableFormat {
        id: "restate.effect_group_state_format",
        name: "Restate effect-group state format",
        version: EFFECT_GROUP_STATE_FORMAT_VERSION as u32,
        constant: "EFFECT_GROUP_STATE_FORMAT_VERSION",
        upgrade_policy: UpgradePolicy::Migrate,
        unwalkable_reason: UNWALKABLE_REASON,
    },
    EngineDurableFormat {
        id: "restate.effect_group_wire",
        name: "Restate effect-group wire",
        version: EFFECT_GROUP_WIRE_VERSION,
        constant: "EFFECT_GROUP_WIRE_VERSION",
        upgrade_policy: UpgradePolicy::Coexist,
        unwalkable_reason: UNWALKABLE_REASON,
    },
    EngineDurableFormat {
        id: "restate.effect_group_dispatch_journal",
        name: "Restate effect-group dispatch journal",
        version: EFFECT_GROUP_DISPATCH_JOURNAL_VERSION,
        constant: "EFFECT_GROUP_DISPATCH_JOURNAL_VERSION",
        upgrade_policy: UpgradePolicy::Drain,
        unwalkable_reason: UNWALKABLE_REASON,
    },
    EngineDurableFormat {
        id: "restate.effect_group_payload_format",
        name: "Restate effect-group payload format",
        version: EFFECT_GROUP_PAYLOAD_FORMAT_VERSION as u32,
        constant: "EFFECT_GROUP_PAYLOAD_FORMAT_VERSION",
        upgrade_policy: UpgradePolicy::Migrate,
        unwalkable_reason: UNWALKABLE_REASON,
    },
    EngineDurableFormat {
        id: "restate.process_journal",
        name: "Restate process journal prefix",
        version: RESTATE_PROCESS_JOURNAL_VERSION,
        constant: "RESTATE_PROCESS_JOURNAL_VERSION",
        upgrade_policy: UpgradePolicy::Drain,
        unwalkable_reason: UNWALKABLE_REASON,
    },
    EngineDurableFormat {
        id: "restate.effect_journal",
        name: "Restate effect journal",
        version: EFFECT_JOURNAL_VERSION,
        constant: "EFFECT_JOURNAL_VERSION",
        upgrade_policy: UpgradePolicy::Drain,
        unwalkable_reason: UNWALKABLE_REASON,
    },
    EngineDurableFormat {
        id: "restate.session_drive",
        name: "Restate session drive prefix",
        version: LASH_SESSION_DRIVE_VERSION,
        constant: "LASH_SESSION_DRIVE_VERSION",
        upgrade_policy: UpgradePolicy::Drain,
        unwalkable_reason: UNWALKABLE_REASON,
    },
];

/// Every durable format this engine writes, in the order the facade's
/// format table reports them.
pub fn durable_formats() -> impl Iterator<Item = &'static EngineDurableFormat> {
    DURABLE_FORMATS.iter()
}
