//! A process's engine state as its execution root in the checkpoint graph
//! (FIG-5887 §3, §10): the engine's own bytes in a ProcessState node (kind
//! 38), under an ExecRoot (kind 36) whose owner is the process. The bytes are
//! stored as bytes, placed in shared leaves when large, never wrapped as hex
//! text; a state carried to a newer format is one more such root, which
//! reuses every leaf the carry left unchanged.

use lash_core_store::checkpoint_graph::bodies::{
    ExecRoot, ExecRootKind, ExecStatus, OwnerBinding, ProcessState, ProcessStateBody,
};
use lash_core_store::checkpoint_graph::{
    CommitId, DecodeLimits, EncodeBudget, ExplicitLimit, GraphDecoder, GraphRefusal, GraphStaging,
    NodeId, NodeKind,
};
use lash_durable::domain::graph::ExecRootWrite;
use lash_durable::domain::{ExecKey, ExecRootRow, RootOwner, SnapshotRev};
use lash_durable::{ActorTx, DomainWrite, DurableError, DurableReads};

use crate::ProcessId;
use crate::runtime::process::engine_state::{EngineState, EngineStateFormat};

fn mismatch(reason: &str) -> GraphRefusal {
    GraphRefusal::InvalidBody {
        kind: NodeKind::EXEC_ROOT.0,
        reason: reason.to_owned(),
    }
}

/// Stages `state`, the state `engine` holds for `process`, as the process's
/// complete execution root, and returns it with the row that publishes it
/// in place of `expected`: what an advance and a carry both commit.
///
/// # Errors
///
/// Any refusal of the staging area.
pub fn stage_engine_state(
    staging: &mut GraphStaging,
    process: &ProcessId,
    engine: &str,
    state: &EngineState,
    expected: (Option<SnapshotRev>, Option<NodeId>),
    commit_id: CommitId,
) -> Result<(NodeId, DomainWrite), GraphRefusal> {
    let owner = staging.encode_value(
        NodeKind::OWNER_BINDING,
        &OwnerBinding::Process {
            process: process.as_str().to_owned(),
        },
    )?;
    let body = staging.encode_value(
        NodeKind::PROCESS_STATE,
        &ProcessState {
            engine: engine.to_owned(),
            executable_identity: state.format.kind.clone(),
            state_format: state.format.version,
            state: match state.pages {
                Some(root) => ProcessStateBody::Kernel {
                    root,
                    bytes: state.bytes.clone(),
                },
                None => ProcessStateBody::EngineBytes(state.bytes.clone()),
            },
        },
    )?;
    let root = staging.encode_value(
        NodeKind::EXEC_ROOT,
        &ExecRoot {
            exec: ExecRootKind::Process,
            owner,
            executable_identity: state.format.kind.clone(),
            format: state.format.version,
            status: ExecStatus::Parked,
            state: body,
            effect_membership: None,
            host: None,
            envelope: None,
            owed: Vec::new(),
        },
    )?;
    let write = DomainWrite::ExecRoot(ExecRootWrite::Put {
        exec: ExecKey::Process(process.clone()),
        expected: expected.0,
        expected_root: expected.1,
        root,
        commit_id,
    });
    Ok((root, write))
}

/// Writes `state`, the state `engine` holds for `process`, into `tx` as the
/// process's execution root in place of `expected`, the root row it read:
/// its nodes staged, its row written, all in one owner commit.
///
/// # Errors
///
/// [`DurableError::Graph`] for any refusal of the staging area.
pub fn write_engine_state(
    tx: &mut ActorTx,
    process: &ProcessId,
    engine: &str,
    state: &EngineState,
    expected: Option<&ExecRootRow>,
) -> Result<(), DurableError> {
    let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    let (_, write) = stage_engine_state(
        &mut staging,
        process,
        engine,
        state,
        (expected.map(|row| row.rev), expected.map(|row| row.root)),
        CommitId::mint(),
    )?;
    tx.stage_all(staging);
    tx.write(write);
    Ok(())
}

/// `process`'s execution root row and the engine state it holds, with the
/// engine that wrote it, or `None` when the process has written none.
///
/// # Errors
///
/// A store failure, or [`DurableError::Graph`] for a root that does not
/// load as this process's state.
pub async fn read_engine_state(
    reads: &dyn DurableReads,
    process: &ProcessId,
    limits: DecodeLimits,
) -> Result<Option<(ExecRootRow, String, EngineState)>, DurableError> {
    let exec = ExecKey::Process(process.clone());
    let Some(row) = reads.exec_root(&exec).await? else {
        return Ok(None);
    };
    let graph = reads
        .hydrate(RootOwner::Exec(exec), Some(row.root), limits)
        .await?;
    let mut decoder = GraphDecoder::new(&graph, limits);
    let (engine, state) = load_engine_state(&mut decoder, row.root, process)?;
    Ok(Some((row, engine, state)))
}

/// The engine state `process`'s execution root `root` holds, with the
/// engine that wrote it: refused unless the root is a process root of this
/// process whose descriptor states the format its state holds.
///
/// # Errors
///
/// Any refusal of a node read, [`GraphRefusal::WrongChildKind`],
/// [`GraphRefusal::OwnerMismatch`], or [`GraphRefusal::InvalidBody`].
pub fn load_engine_state(
    decoder: &mut GraphDecoder<'_>,
    root: NodeId,
    process: &ProcessId,
) -> Result<(String, EngineState), GraphRefusal> {
    let exec: ExecRoot = decoder.decode(root, NodeKind::EXEC_ROOT)?;
    if exec.exec != ExecRootKind::Process {
        return Err(mismatch("a process's root is a cell's"));
    }
    exec.check_children(root, decoder)?;
    let owner: OwnerBinding = decoder.decode(exec.owner, NodeKind::OWNER_BINDING)?;
    if owner
        != (OwnerBinding::Process {
            process: process.as_str().to_owned(),
        })
    {
        return Err(GraphRefusal::OwnerMismatch);
    }
    let state: ProcessState = decoder.decode(exec.state, NodeKind::PROCESS_STATE)?;
    state.check_children(exec.state, decoder)?;
    let (bytes, pages) = match state.state {
        ProcessStateBody::EngineBytes(bytes) => (bytes, None),
        ProcessStateBody::Kernel { root, bytes } => (bytes, Some(root)),
    };
    if state.executable_identity != exec.executable_identity || state.state_format != exec.format {
        return Err(mismatch("the root states another format than its state"));
    }
    Ok((
        state.engine,
        EngineState {
            format: EngineStateFormat {
                kind: state.executable_identity,
                version: state.state_format,
            },
            bytes,
            pages,
        },
    ))
}

#[cfg(test)]
#[path = "state_graph_tests.rs"]
mod tests;
