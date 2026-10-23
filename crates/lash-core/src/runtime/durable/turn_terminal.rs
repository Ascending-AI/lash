//! A turn's terminal and its transcript (FIG-6601).
//!
//! # Contracts
//!
//! - **The transcript rides the terminal.** Every turn end writes its
//!   terminal through [`write_terminal`], last among the turn's own facts of
//!   its transaction: the transcript of the calls the turn answered, read
//!   from its ledger as stored and the facts `tx` appends, is staged in the
//!   same transaction, and the terminal publishes it as the run's root.
//! - **The ledger is the recovery log until then.** The store retires the
//!   turn's run facts in the terminal's commit, never before: a terminal
//!   that does not commit leaves the ledger whole, and the next pass
//!   recovers from it.
//! - **No copy.** The transcript references the material the ledger named
//!   ([`TurnTranscript`]); a settled turn's calls are read from it alone.

use lash_core_execution::runtime::actor::round::{TranscriptEntry, TurnTranscript};
use lash_core_store::checkpoint_graph::{EncodeBudget, ExplicitLimit, GraphStaging};
use lash_durable::domain::{OwnerKey, RunFacts, TurnWrite};
use lash_durable::{ActorTx, DomainWrite, DurableError};

use crate::ActorContext;
use lash_vocabulary::identity::SessionId;
use lash_vocabulary::identity::TurnId;

/// End `run` of `session` in `tx` with `cause` and the head revision its
/// commit published, if any, staging the turn's transcript beside it.
///
/// # Errors
///
/// A durable read's refusal, or a ledger whose facts do not decode.
pub(in crate::runtime) async fn write_terminal(
    cx: &ActorContext,
    tx: &mut ActorTx,
    session: &SessionId,
    run: &TurnId,
    cause: crate::store::RunTerminalCause,
    head_revision: Option<u64>,
) -> Result<(), DurableError> {
    let owner = OwnerKey::Turn(session.clone(), run.clone());
    let mut facts = cx.durable_reads()?.run_facts(&owner).await?;
    facts.extend(RunFacts::appended(tx.domain(), tx.graph(), tx.epoch()));
    facts.rows.retain(|row| row.owner == owner);
    let mut transcript = TurnTranscript::of(&facts)
        .map_err(DurableError::Graph)?
        .unwrap_or_default();
    let rounds = terminal_rounds(cx, tx, session, run).await?;
    for round in rounds {
        tx.require(round);
        transcript.entries.push(TranscriptEntry::Environment(round));
    }
    let transcript = if transcript.entries.is_empty() {
        None
    } else {
        let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
        let node = transcript
            .stage(&mut staging)
            .map_err(DurableError::Graph)?;
        tx.stage_all(staging);
        Some(node)
    };
    tx.write(DomainWrite::Turn(TurnWrite::Terminal {
        session: session.clone(),
        run: run.clone(),
        cause: Box::new(cause),
        head_revision,
        transcript,
    }));
    Ok(())
}

/// Prefer a phase staged by this transaction; otherwise use the run's retained
/// phase, before the terminal retires it. Never consult the session-current head.
async fn terminal_rounds(
    cx: &ActorContext,
    tx: &ActorTx,
    session: &SessionId,
    run: &TurnId,
) -> Result<Vec<lash_core_store::checkpoint_graph::NodeId>, DurableError> {
    use lash_core_store::checkpoint_graph::{GraphDecoder, NodeKind, bodies::PhaseRoot};
    use lash_durable::domain::{PhaseAdvance, RootOwner};
    let staged = tx.domain().iter().rev().find_map(|write| match write {
        DomainWrite::Turn(TurnWrite::Advance {
            session: s,
            run: r,
            phase,
            ..
        }) if s == session && r == run => match phase {
            PhaseAdvance::Model { checkpoint, .. } | PhaseAdvance::Tools { checkpoint, .. } => {
                Some(*checkpoint)
            }
        },
        _ => None,
    });
    if let Some(root) = staged {
        let mut decoder =
            GraphDecoder::new(tx.graph(), lash_durable::domain::graph::OWNER_READ_LIMITS);
        let body: PhaseRoot = decoder.decode(root, NodeKind::PHASE_ROOT)?;
        return Ok(body
            .environments
            .into_iter()
            .map(|(_, node)| node)
            .collect());
    }
    let graph = match cx
        .durable_reads()?
        .hydrate(
            RootOwner::TurnPhase {
                session: session.clone(),
                run: run.clone(),
            },
            None,
            lash_durable::domain::graph::OWNER_READ_LIMITS,
        )
        .await
    {
        Ok(graph) => graph,
        Err(DurableError::Graph(lash_core_store::checkpoint_graph::GraphRefusal::RootRetired)) => {
            return Ok(Vec::new());
        }
        Err(error) => return Err(error),
    };
    let mut decoder = GraphDecoder::new(&graph, lash_durable::domain::graph::OWNER_READ_LIMITS);
    let body: PhaseRoot = decoder.decode(graph.root, NodeKind::PHASE_ROOT)?;
    Ok(body
        .environments
        .into_iter()
        .map(|(_, node)| node)
        .collect())
}
