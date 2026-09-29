//! The bounds a parent holds its workers to, as one explicit preset.

use std::time::Duration;

use crate::codec::DecodeLimits;

/// Every protocol bound a host states for its workers. There is no implicit
/// default: a host takes [`ProtocolBounds::standard`] or states its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProtocolBounds {
    /// Each frame's decode bounds.
    pub decode: DecodeLimits,
    /// The largest opaque VM state, a continuation or a guest snapshot, a
    /// worker may hand its parent or be started from.
    pub max_vm_state_bytes: u64,
    /// The largest encoded effect request or result value.
    pub max_effect_value_bytes: u64,
    /// The largest model program source a `Start` carries.
    pub max_source_bytes: u64,
    /// How long a worker may go without any frame before its parent treats
    /// it as unresponsive. Host waits pause it, and it is not a guest
    /// execution limit: permitted computation and serialization keep their
    /// own deadlines.
    pub no_response_watchdog: Duration,
}

impl ProtocolBounds {
    /// The FIG-4157 measured presets: the standard decode limits, 2 MiB of
    /// VM state (about twice the largest measured state), 1 MiB effect
    /// values, 64 KiB of source (the front end's own bound), and a 5-second
    /// no-response watchdog, about eleven times the worst measured decode.
    pub const fn standard() -> Self {
        Self {
            decode: DecodeLimits::standard(),
            max_vm_state_bytes: 2 * 1024 * 1024,
            max_effect_value_bytes: 1024 * 1024,
            max_source_bytes: 64 * 1024,
            no_response_watchdog: Duration::from_secs(5),
        }
    }
}
