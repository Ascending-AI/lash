//! Sans-IO turn machine and protocol driver contracts.

pub mod sansio;
#[doc(hidden)]
pub mod testing;
pub mod turn;
pub mod turn_driver;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub use sansio::{
    ChatContextProjector, CheckpointDelivery, CheckpointResumeAction, CompletedToolCall,
    ContextProjector, DriverAction, DriverContextView, Effect, EffectId, ExpandedRow,
    ExpandedWrapper, JournalRow, MachineInput, ModelToolCalls, PendingToolCall, PendingWork,
    ProjectorContext, ProtocolDriverHandle, RecordedSyncs, ReplayDivergence, Response,
    ResponseToolCalls, StagedRow, ToolControlOrigin, ToolExpansionPlan, ToolWave, TurnMachine,
    TurnMachineConfig, TurnProtocol, TurnWindow, TurnWindowPin, UndecodableDriverState,
    UnitTurnProtocol,
};
pub use turn::{PreparedTurnMachine, SansIoTurnInput, build_turn};
pub use turn_driver::{
    BuildNewestWriterFormats, TurnDriverConfig, TurnDriverPreamble, WriterFormats,
    build_newest_writer_formats,
};
