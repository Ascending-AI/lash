//! Shared types and helpers used by protocol drivers. Concrete drivers and
//! their prompts live in protocol plugin crates; this module exposes the common
//! turn-driver surface:
//!
//! - [`TurnDriverConfig`], [`TurnDriverPreamble`] — the per-turn configuration
//!   driver-plugins populate.

use std::sync::Arc;

use crate::sansio::{
    ChatContextProjector, ContextProjector, ProtocolDriverHandle, TurnProtocol, UnitTurnProtocol,
};
use lash_protocol_types::llm::types::LlmToolSpec;

#[derive(Clone)]
pub struct TurnDriverConfig<M: TurnProtocol = UnitTurnProtocol> {
    pub protocol: Arc<dyn ProtocolDriverHandle<M>>,
    pub projector: Arc<dyn ContextProjector<M>>,
}

impl<M: TurnProtocol> TurnDriverConfig<M> {
    pub fn chat(protocol: Arc<dyn ProtocolDriverHandle<M>>) -> Self {
        Self {
            protocol,
            projector: Arc::new(ChatContextProjector::default()),
        }
    }
}

/// The writer versions a fleet assigns registered durable-format surfaces
/// (FIG-3796): while the fleet writes generation `F`, every durable stamp a
/// protocol driver emits comes from this table rather than the build's own
/// constants, so a newer build in a mixed deployment never writes a format an
/// older fleet member cannot read. The host resolves the table from the
/// fleet-format row the session's store recorded; a driver asks it through
/// `DriverContextView::writer_version` (usually via the
/// `driver_writer_version!` macro).
pub trait WriterFormats: Send + Sync {
    /// The version writers emit for the surface registered under `constant`,
    /// whose build-newest version is `build_newest`.
    fn writer_version(&self, constant: &'static str, build_newest: u32) -> u32;
}

/// The writer versions of a context that never consulted a store: every
/// surface emits its build-newest version.
#[derive(Clone, Copy, Debug, Default)]
pub struct BuildNewestWriterFormats;

impl WriterFormats for BuildNewestWriterFormats {
    fn writer_version(&self, _constant: &'static str, build_newest: u32) -> u32 {
        build_newest
    }
}

/// The `WriterFormats` a host without a recorded fleet format installs.
pub fn build_newest_writer_formats() -> Arc<dyn WriterFormats> {
    Arc::new(BuildNewestWriterFormats)
}

/// Resolves a registered surface's writer version through a
/// `DriverContextView`: `driver_writer_version!(ctx, MY_FORMAT_VERSION)` names
/// the surface and carries the constant's own value as its build-newest
/// version, so the name and the version can never come from different
/// constants.
#[macro_export]
macro_rules! driver_writer_version {
    ($view:expr, $constant:expr) => {
        $view.writer_version(::core::stringify!($constant), $constant as u32)
    };
}

#[derive(Clone)]
pub struct TurnDriverPreamble<M: TurnProtocol = UnitTurnProtocol> {
    pub config: TurnDriverConfig<M>,
    pub tool_specs: Arc<Vec<LlmToolSpec>>,
    pub tool_names: Arc<Vec<String>>,
    /// The fleet's writer-version table (FIG-3796): the turn machine hands it
    /// to `DriverContextView` so drivers stamp durable envelopes at the
    /// versions the fleet writes.
    pub writer_formats: Arc<dyn WriterFormats>,
}
