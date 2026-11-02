//! Sans-IO turn machine and protocol driver contracts.

pub mod sansio;
pub mod turn;
pub mod turn_driver;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub use sansio::{
    ChatContextProjector, CheckpointContentPin, CheckpointContentRef, CheckpointDelivery,
    CheckpointDelta, CheckpointResumeAction, CompletedToolCall, ContextProjector, DriverAction,
    DriverContextView, Effect, EffectId, ExpandedRow, ExpandedWrapper, JournalRow, MachineInput,
    ModelToolCalls, PendingToolCall, PendingWork, ProjectorContext, ProtocolDriverHandle,
    ReplayDivergence, Response, ResponseToolCalls, SavedTurn, StagedRow, ToolControlOrigin,
    ToolExpansionPlan, ToolWave, TurnCheckpoint, TurnCheckpointContent, TurnCheckpointRestoreError,
    TurnMachine, TurnMachineConfig, TurnProtocol, TurnWindow, TurnWindowPin,
    UndecodableDriverState, UnitTurnProtocol,
};
pub use turn::{PreparedTurnMachine, SansIoTurnInput, build_turn};
pub use turn_driver::{
    BuildNewestWriterFormats, TurnDriverConfig, TurnDriverPreamble, WriterFormats,
    build_newest_writer_formats,
};
