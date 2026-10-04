use super::*;

pub(super) fn continuation_refused(
    refusal: lash_core::tool_run::ContinuationRefusal,
) -> lash_core::ProcessInfraError {
    lash_core::ProcessInfraError::new(lash_core::PluginError::RuntimeEffectController(
        refusal.into(),
    ))
}

/// Version of the durable Lashlang segment-handover envelope.
///
/// v12 carries VM continuation v16, which counts aggregates in the occurrence
/// counters this envelope hands to the next segment. A segment parked by v11
/// holds counts for execution sites only, so the batches it re-derives after
/// handover would mint ordinals its own journal never recorded. The boundary is
/// a version rather than a decode failure for the same reason v9's was: the
/// bytes still parse.
/// v11 drops `signal_send_sequence`: its only producer was deleted with the
/// signal special forms (FIG-2999), and the ordinal had been round-tripping
/// dead since, so the envelope was version-gating a field that carried no
/// meaning. The remaining ordinals move into one `ReplayOrdinalsState`
/// group, so the envelope, the restore path and the boundary snapshot spell
/// them once.
/// v10 drops the parent-end action list: child lifecycle is settled from the
/// registry's scope-keyed parent-end ledger, so a parked segment no longer
/// carries per-child actions a replay would have to reconcile. v8 was reserved
/// for this change and went unused: the TypeScript cutover landed first and
/// took the next generation, so this one takes the one after it.
/// v9 carries VM continuation v14. TypeScript is the only RLM language
/// (ADR 0096), so the instruction set loses the deep-copy instructions the
/// retired surface compiled to: a segment parked before the cutover holds a
/// continuation over an instruction stream this reader cannot reproduce, so the
/// boundary is a version rather than a decode failure.
/// v7 pins the attempt bound this segment stamps onto the children it starts,
/// so a redrive after a host config change re-registers the recorded bound
/// instead of conflicting with the fingerprint the first attempt wrote.
/// v13 carries the once-only incorporation ledger (FIG-3411, ADR 0099 §6/§13):
/// which settlements the opener already applied and which usage deltas it
/// already charged. A segment parked by an older version has no ledger to
/// hand over, so the boundary is a version rather than a defaulted field — a
/// defaulted empty ledger would let the successor incorporate the same
/// settlement twice and double-charge its spend.
/// v14 replaces runtime occurrence-counter keys with the shared workflow node
/// identity and reserves the process root for the process declaration.
/// v15 carries sleep/signal call-site continuations and the closed workflow
/// execution-site kind. A parked v14 segment is refused before VM restore.
/// v16 carries the durable effect summary's per-node omission counts
/// (FIG-3464): a successor counting from zero would under-report omitted
/// occurrences in the run's terminal omission record.
/// v6 carries run-local child possession across execution segments. A segment
/// parked by another version is refused rather than decoded (ADR 0055).
/// v17 carries the effect groups the process still holds — each group's key,
/// child count and consumed cursor — so a successor segment reattaches losers
/// that are still running instead of declining the boundary (ADR 0099 §8, §9),
/// and embeds VM continuation v18.
/// v18 (FIG-3571) embeds VM continuation v19 over the carrier IR's node ids. A
/// v17 segment parked before the cutover is refused before continuation
/// restore; it is never redriven under the new node ids.
/// v19 (FIG-3586) carries the run's issue-ordinal state — the ordinal the
/// next command takes and the running digest of the commands it wrote — in
/// place of the per-kind sleep sequence, and embeds VM continuation state
/// whose aggregates no longer count occurrences. A segment parked by v18
/// resumes commands under keys a v19 run never mints, so it is refused.
/// v20 (FIG-3655) embeds VM continuation v22, whose closures carry their own
/// `name`/`length` metadata. A v19 segment holds continuations in the v21
/// shape, so it is refused rather than decoded.
/// v21 (FIG-3701) embeds VM continuation v24, whose heap may hold a built-in
/// method value (`'x'.includes`). A v20 segment holds v23 continuations, so it
/// is refused rather than decoded.
/// v22 (FIG-3707) embeds VM continuation v25, whose heap may hold a binding
/// cell. A v21 segment holds v24 continuations, so it is refused rather than
/// decoded.
/// v23 (FIG-3571) carries the run's pending effect-summary occurrences, which
/// the successor commits at its first boundary. A v22 segment committed each
/// occurrence as it was recorded and carries none, so it is refused rather
/// than decoded.
/// Re-exported by the facade's `formats` manifest so a host can read it before
/// wiring a store.
///
/// version_guard(
///     roots(LashlangSegmentState),
///     items(path = "crates/lashlang/src/workflow_graph.rs", workflow_node_id),
///     items(
///         path = "crates/lashlang/src/workflow_graph/execution_sites.rs",
///         path = "crates/lashlang/src/workflow_graph/ownership.rs",
///         path = "crates/lashlang/src/ast_roles.rs", path = "crates/lashlang/src/tracking.rs",
///         from_indices, indices, path_for_ast, for_main, for_process, ownership_map,
///         into_ownership_map, workflow_projection, statement_list, push_statement_list,
///         is_statement_list, collect_body, collect_statement, statement_value, map_node_subtree,
///         check_shape, process_wrapper_run_path, execution_sites, collect_execution_sites,
///         push_execution_site_descriptor, collect_child_execution_sites, workflow_owner,
///         node_site, branch_site, branch_edge_id,
///     ),
///     shapes(
///         path = "crates/lash-core-execution/src/runtime/process/engine.rs",
///         cover(SegmentHandover, PersistedSegmentHandover),
///     ),
/// )
/// version_surface = "drain"
/// format_manifest = "LashlangSegmentHandover"
pub const LASHLANG_SEGMENT_STATE_VERSION: u32 = 23;

pub(super) const SEGMENT_STATE_CUTOVER_REMEDY: &str = "drain in-flight sessions on the old build before deploying this build, or recreate development/test stores";

#[derive(Debug, thiserror::Error)]
pub(super) enum LashlangSegmentStateError {
    #[error(
        "lashlang segment handover format is incompatible: {details}; {SEGMENT_STATE_CUTOVER_REMEDY}"
    )]
    FormatMismatch { details: String },
    #[error(
        "lashlang segment handover version {found} is incompatible with version {expected}; {SEGMENT_STATE_CUTOVER_REMEDY}"
    )]
    VersionMismatch { expected: u32, found: u32 },
}

#[derive(serde::Deserialize)]
struct LashlangSegmentStateVersionProbe {
    pub(super) version: Option<u32>,
}

/// The replay ordinals a segment hands to the next execution of its run, as
/// they sit on the wire.
///
/// One group spelled once: the envelope embeds it flattened,
/// [`ReplayOrdinals::restore`] lifts it into the run's live counters and
/// [`ReplayOrdinals::snapshot`] writes it back. A counter spelled at fewer
/// than all three sites used to compile — `signal_send_sequence` kept
/// round-tripping for a day after FIG-2999 deleted its only producer.
#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct ReplayOrdinalsState {
    /// The run's issue-ordinal state (FIG-3586): every command's journal key.
    pub(super) commands: crate::LashlangRunOrdinals,
    /// Process-event sequence: the idempotency key of each event append,
    /// which observers address.
    pub(super) event_sequence: u64,
    /// Per-name signal-wait ordinals: the wait keys outside signallers
    /// address, so they stay named.
    pub(super) signal_wait_ordinals: BTreeMap<String, u64>,
}

/// The live counterpart of [`ReplayOrdinalsState`]: the ordinals the running
/// segment is consuming, held as the counters the host mutates in place. The
/// command ordinals live in the run itself.
pub(super) struct ReplayOrdinals {
    pub(super) event_sequence: AtomicU64,
    pub(super) signal_wait_ordinals: std::sync::Mutex<BTreeMap<String, u64>>,
}

impl ReplayOrdinals {
    pub(super) fn restore(state: Option<&LashlangSegmentState>) -> Self {
        let ordinals = state.map(|state| &state.ordinals);
        Self {
            event_sequence: AtomicU64::new(ordinals.map_or(0, |o| o.event_sequence)),
            signal_wait_ordinals: std::sync::Mutex::new(
                ordinals.map_or_else(BTreeMap::new, |o| o.signal_wait_ordinals.clone()),
            ),
        }
    }

    /// The command ordinals a resumed segment continues from, or a fresh
    /// run's.
    pub(super) fn restore_commands(
        state: Option<&LashlangSegmentState>,
    ) -> crate::LashlangRunOrdinals {
        state.map_or_else(crate::LashlangRunOrdinals::start, |state| {
            state.ordinals.commands.clone()
        })
    }

    pub(super) fn snapshot(&self, run: &crate::LashlangReplayRun) -> ReplayOrdinalsState {
        ReplayOrdinalsState {
            commands: run.ordinals(),
            event_sequence: self.event_sequence.load(Ordering::Relaxed),
            signal_wait_ordinals: self.signal_wait_ordinals.lock_recover().clone(),
        }
    }
}

/// The most continuation bytes a parked segment may carry: the preset the
/// measurement lane finalises.
pub(super) const MAX_SEGMENT_CONTINUATION_BYTES: u64 = 64 * 1024 * 1024;

/// The segment envelope a boundary hands to the next segment.
///
/// It is assembled by the parent: `vm` is the worker's continuation, held as
/// opaque bytes the parent checks structurally and never decodes (ADR 0123),
/// and every other field is a ledger the parent owns — the ordinals, the
/// started children, incorporation, the pending summary and the groups. The
/// worker contributes the VM bytes and nothing else.
#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct LashlangSegmentState {
    pub(super) version: u32,
    pub(super) vm: lash_vm_protocol::OpaqueVmState,
    #[serde(flatten)]
    pub(super) ordinals: ReplayOrdinalsState,
    pub(super) started_process_ids: Vec<ProcessId>,
    /// The once-only settlement incorporation ledger (FIG-3411): a successor
    /// segment incorporates against the same set so a redrive cannot
    /// re-apply a settlement.
    pub(super) incorporation_ledger: lash_core::session::IncorporationLedger,
    /// Effect occurrences within the durable summary's per-node cap that no
    /// boundary has committed yet (FIG-3571). The successor segment commits
    /// them with its first boundary write; re-committing one a crashed
    /// successor already wrote is a replay-key no-op. Bounded by construction:
    /// at most [`lash_core::PROCESS_EFFECT_OCCURRENCE_CAP`] per execution site
    /// of the compiled program.
    pub(super) pending_summary: Vec<lash_core::ProcessEffectOccurrence>,
    /// Effect occurrences past the durable summary's per-node cap, counted by
    /// outcome class (FIG-3464). A successor segment keeps counting from here
    /// and the run's terminal omission record carries the total.
    pub(super) effect_omissions: BTreeMap<String, lash_core::ProcessEffectOmittedCounts>,
    /// The effect groups this process still holds after an aggregate stopped
    /// consuming early (ADR 0099 §8, §9): each group's key, child count and
    /// consumed cursor. A boundary is never declined because a loser is
    /// unsettled; the successor segment reattaches these cursors and the
    /// process terminal closes them.
    /// The tool calls each held group counts against the session's
    /// `max_tool_calls` (FIG-4546), by group key. The successor segment is
    /// the same process, so it holds the same calls: it reuses these
    /// reservations rather than counting the groups again or not at all.
    /// The complete tool Run, sealed after local durable acceptance.
    #[serde(deserialize_with = "deserialize_tool_run")]
    pub(super) tool_run: Option<Box<lash_core::tool_run::RunTransfer>>,
    /// The worker accounting the body carries across this boundary
    /// (ADR 0123).
    pub(super) worker_recovery: WorkerRecoveryLedger,
}

fn deserialize_tool_run<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Box<lash_core::tool_run::RunTransfer>>, D::Error> {
    serde::Deserialize::deserialize(deserializer)
}

pub(super) fn decode_lashlang_segment_state(
    data: &[u8],
) -> Result<LashlangSegmentState, LashlangSegmentStateError> {
    let probe: LashlangSegmentStateVersionProbe =
        serde_json::from_slice(data).map_err(|error| {
            LashlangSegmentStateError::FormatMismatch {
                details: error.to_string(),
            }
        })?;
    let found = probe.version.unwrap_or(0);
    if found != LASHLANG_SEGMENT_STATE_VERSION {
        return Err(LashlangSegmentStateError::VersionMismatch {
            expected: LASHLANG_SEGMENT_STATE_VERSION,
            found,
        });
    }
    serde_json::from_slice(data).map_err(|error| LashlangSegmentStateError::FormatMismatch {
        details: error.to_string(),
    })
}

/// The segment state a boundary hands over: the worker's continuation bytes,
/// sealed as opaque state, beside the parent's own ledgers the next segment
/// resumes with. An error names why the state could not be captured.
pub(super) fn capture_segment(
    vm: lash_vm_protocol::OpaqueVmState,
    host: &LashlangProcessHost<'_>,
    reason: lash_core::BoundaryReason,
    program_hash: &str,
) -> Result<lash_core::SegmentHandover, (String, &'static str)> {
    let mut tool_run = host
        .ctx
        .run_continuation_snapshot()
        .map_err(|error| (error.to_string(), "tool Run is not capturable; continuing"))?;
    if let Some(run) = &mut tool_run {
        run.vm_continuation = true;
    }
    let segment_state = LashlangSegmentState {
        version: LASHLANG_SEGMENT_STATE_VERSION,
        vm,
        ordinals: host.ordinals.snapshot(&host.run),
        started_process_ids: host.ctx.started_process_ids(),
        incorporation_ledger: host.ctx.incorporation_ledger_snapshot(),
        pending_summary: host.effect_summary.pending(),
        effect_omissions: host.effect_summary.omissions(),
        tool_run: tool_run.map(Box::new),
        // The worker released at this boundary settled its measured usage,
        // so the budget holds everything the body consumed so far.
        worker_recovery: host.worker_recovery.crossed(
            host.workers
                .execution_budget()
                .map_or(host.worker_recovery.totals, |budget| {
                    budget.recovery_totals()
                }),
        ),
    };
    let engine_state = serde_json::to_vec(&segment_state).map_err(|error| {
        (
            error.to_string(),
            "lashlang segment continuation was not serializable; continuing",
        )
    })?;
    Ok(lash_core::SegmentHandover {
        reason,
        program_hash: program_hash.to_owned(),
        engine_state,
    })
}
