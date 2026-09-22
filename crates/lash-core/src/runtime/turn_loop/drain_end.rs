//! The queue-drain end epilogue (ADR 0094, FIG-3419): a drain is an owner,
//! and an owner ends.
//!
//! A drain runs under `ExecutionScope::QueueDrain { session_id, drain_id }`,
//! and `drain_id` is the caller's idempotency identity: a retried drain under
//! the same `drain_id` is the same owner, so its end is written once and a
//! replay of this epilogue is a receipt no-op. The end fact is the session
//! store receipt `{drain scope}/final`; the parent-end ledger row in the
//! process registry is a second write because the two are separate stores,
//! and the crash window between them is closed by
//! `redrive_missing_opener_parent_end_rows`, which confirms the receipt
//! through `SessionCommitStore::drain_end_exists` before re-deriving the row.
//!
//! What is *not* a drain end: the worker dying (nothing is written — the
//! retry under the same `drain_id` ends it), a physical-turn commit inside
//! the drain (one drain can run several; its owner end is its own write), a
//! failed run (interrupted, not ended), and a fresh empty poll (nothing was
//! ever owned). Ordering is the §7 one this ticket fixes: protected
//! obligations drain first, then the outcome commit, then the receipt, then
//! the ledger row — the ledger row is what sweeps `OnParentEnd::Cancel`
//! children, so it must land only after the drain cannot owe more work.

use super::*;

impl LashRuntime {
    /// Run the drain-end epilogue for `drain_scope` after a successful drain
    /// (`ran` = a logical turn executed) or a nothing-to-do drain (`ran` =
    /// false). Only called with the session execution lease still held and
    /// only when the admitted scope is `QueueDrain`; a `turn_id`-scoped drain
    /// is a `Turn` owner and takes `record_turn_parent_end` instead.
    ///
    /// Never returns an error: the run the drain just committed is already
    /// the caller's result, and an epilogue failure is exactly the crash
    /// window recovery exists for — the sweep re-derives the ledger row from
    /// the receipt, and a missing receipt leaves the drain for its retry.
    /// Every decline therefore only traces.
    pub(super) async fn end_queue_drain(
        &self,
        drain_scope: &crate::ExecutionScope,
        session_execution_lease: &SessionExecutionLeaseGuard,
        store: &Arc<dyn crate::store::RuntimePersistence>,
        ran: bool,
    ) {
        let crate::ExecutionScope::QueueDrain {
            session_id,
            drain_id,
        } = drain_scope
        else {
            eprintln!("DRAIN_END bail: not a queue-drain scope");
            return;
        };
        let _phase = super::RuntimeNamedPhase::begin(
            self.turn_phase_probe.clone(),
            "queue_drain.parent_end",
        );

        // §7's ordering: the drain's protected obligations — closing groups
        // under its scope — finish before the end fact lands. A group still
        // `closing` with obligations this host cannot discharge reports
        // `Pending`, and anything still live under the scope (an in-progress
        // row, an open group short of a journaled child, an unresolved
        // promise) is the quiescence read the `WhenQuiescent` retirement gate
        // takes. Either answer withholds the end: the retried drain resumes
        // the same finalizations and asks again.
        let closing_seam = self.host.core.control.effect_host.effect_group_closing();
        eprintln!("DRAIN_END closing seam present: {}", closing_seam.is_some());
        if let Some(closing) = closing_seam {
            match closing
                .resume_closing_groups(drain_scope, &crate::GroupOnlyFinalization)
                .await
            {
                Ok(reports) => {
                    eprintln!("DRAIN_END resume reports: {}", reports.len());
                    let pending: usize = reports
                        .iter()
                        .map(|report| match report {
                            crate::GroupFinalizationReport::Pending { unsettled, .. } => *unsettled,
                            crate::GroupFinalizationReport::Settled { .. } => 0,
                        })
                        .sum();
                    if pending > 0 {
                        eprintln!("DRAIN_END bail: pending {pending}");
                        tracing::debug!(
                            session_id = %session_id,
                            drain_id = %drain_id,
                            pending,
                            "queue drain end withheld: closing groups under the scope still owe obligations",
                        );
                        return;
                    }
                }
                Err(error) => {
                    eprintln!("DRAIN_END bail: resume err {error}");
                    // A finalizer that errored leaves `closing` recorded and
                    // discoverable, which is the §7 contract; the drain stays
                    // unended and its retry re-runs the resume.
                    tracing::warn!(
                        session_id = %session_id,
                        drain_id = %drain_id,
                        error = %error,
                        "queue drain end withheld: closing-group resume failed",
                    );
                    return;
                }
            }
            match closing.scope_is_quiescent(drain_scope).await {
                Ok(true) => {}
                Ok(false) => {
                    eprintln!("DRAIN_END bail: not quiescent");
                    tracing::debug!(
                        session_id = %session_id,
                        drain_id = %drain_id,
                        "queue drain end withheld: scope is not quiescent",
                    );
                    return;
                }
                Err(error) => {
                    eprintln!("DRAIN_END bail: quiescence err {error}");
                    tracing::warn!(
                        session_id = %session_id,
                        drain_id = %drain_id,
                        error = %error,
                        "queue drain end withheld: quiescence read failed",
                    );
                    return;
                }
            }
        }

        // An empty poll ends nothing — unless the drain already owns
        // children, which means this is the retry of a drain that ran,
        // crashed before its end, and now finds the queue drained. `ran`
        // covers the common case; the registry read is what distinguishes a
        // first empty poll from a resumed one.
        if !ran {
            let Some(registry) = self.host.process_registry() else {
                return;
            };
            let parent = crate::ParentScope::queue_drain(session_id.clone(), drain_id.clone());
            let filter = crate::ProcessListFilter {
                parent_scope: Some(parent),
                status: crate::ProcessStatusFilter::Any,
                ..Default::default()
            };
            match registry.list_processes(&filter).await {
                Ok(children) if children.is_empty() => {
                    eprintln!("DRAIN_END bail: empty poll, no children");
                    return;
                }
                Ok(_) => {}
                Err(error) => {
                    eprintln!("DRAIN_END bail: list children err {error}");
                    tracing::warn!(
                        session_id = %session_id,
                        drain_id = %drain_id,
                        error = %error,
                        "queue drain end withheld: could not list the drain's children",
                    );
                    return;
                }
            }
        }

        // The end fact: a state-preserving commit receipted under the drain's
        // own scope at the reserved `final` key, borrowing the held lane so a
        // rotated lease vetoes the write. Receipt replay makes a retried
        // drain's epilogue a no-op.
        let operation = crate::OperationId::new(
            crate::ExecutionScope::queue_drain(session_id.clone(), drain_id.clone()),
            "final",
        );
        let mut commit = match crate::store::RuntimeCommit::persisted_state_with_graph_commit_and_operation_and_budget(
            &self.state,
            crate::GraphAppend::PreserveHead,
            &[],
            operation,
            self.host.core.durability.commit_budget,
        ) {
            Ok(commit) => commit,
            Err(error) => {
                eprintln!("DRAIN_END bail: commit build err {error}");
                tracing::warn!(
                    session_id = %session_id,
                    drain_id = %drain_id,
                    error = %error,
                    "queue drain end receipt commit could not be built",
                );
                return;
            }
        };
        // The commit's claimed frame must equal the committed graph's nearest
        // `FrameOpen` ancestor, and the runtime's live frame and leaf may be
        // ones it minted itself and never committed — a drain whose run never
        // opened a frame (an empty retry over a graph with no leaf) would
        // otherwise claim graph facts the store cannot derive. The head meta
        // already records the committed leaf and the derived frame, so claim
        // exactly those.
        match store.load_session_head_meta().await {
            Ok(meta) => {
                // A session whose head carries a minted-but-uncommitted frame
                // (a bound session that never ran a turn) has no `FrameOpen`
                // the store can derive — claim a frame only when the
                // committed leaf exists to derive it from, and claim no leaf
                // the head does not record.
                let (frame, leaf) = meta
                    .map(|meta| {
                        (
                            meta.current_frame_node_id
                                .filter(|_| meta.leaf_node_id.is_some()),
                            meta.leaf_node_id,
                        )
                    })
                    .unwrap_or_default();
                commit.current_frame_node_id = frame;
                commit.graph_base_leaf_node_id = leaf;
            }
            Err(error) => {
                tracing::warn!(
                    session_id = %session_id,
                    drain_id = %drain_id,
                    error = %error,
                    "queue drain end withheld: the session head could not be read",
                );
                return;
            }
        }
        let borrowed = session_execution_lease.borrowed_authority();
        let head_stale = self.resident_session.graph_head_stale_flag();
        if let Err(error) = crate::runtime::state::commit_in_lane_context(
            Some(&borrowed),
            Arc::clone(store),
            commit,
            &self.runtime_lease_owner,
            &self.runtime_lease_executor_id,
            self.host.core.control.lease_timings,
            Arc::clone(&self.host.core.clock),
            head_stale.as_ref(),
        )
        .await
        {
            eprintln!("DRAIN_END bail: receipt commit err {error}");
            tracing::warn!(
                session_id = %session_id,
                drain_id = %drain_id,
                error = %error,
                "queue drain end receipt commit failed; the drain stays unended for its retry",
            );
            return;
        }

        // The ledger row, after the end fact — the cross-store gap a crash
        // here leaves is exactly what the opener parent-end sweep closes.
        let Some(registry) = self.host.process_registry() else {
            eprintln!("DRAIN_END bail: no registry");
            return;
        };
        let parent = crate::ParentScope::queue_drain(session_id.clone(), drain_id.clone());
        if let Err(error) = registry.record_parent_end(&parent).await {
            tracing::warn!(
                session_id = %session_id,
                drain_id = %drain_id,
                error = %error,
                "queue drain parent-end ledger row not written; recovery re-derives it from the receipt",
            );
        }
    }
}
