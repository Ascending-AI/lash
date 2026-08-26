use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lash::persistence::{
    AttachmentReclamationPolicy, AttachmentReclamationReport, EmptyRootSetPolicy, GcReport,
    SessionRelation, SessionStoreCreateRequest, SessionStoreFactory, VacuumReport,
};
use lash::{TurnBudget, process::Processes, runtime::SessionPolicy};
use lash_sqlite_store::SqliteSessionStoreFactory;

use crate::state::AppStateData;

const RETENTION_WINDOW: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(60 * 60);

#[derive(Clone)]
pub(crate) struct StoreRetentionTargets {
    pub(crate) factory: Arc<SqliteSessionStoreFactory>,
    pub(crate) gc_store: Arc<dyn lash::persistence::StoreMaintenance>,
    pub(crate) attachment_store: Arc<lash::persistence::FileAttachmentStore>,
}

#[derive(Debug)]
pub(crate) struct SessionVacuumReport {
    pub(crate) session_id: String,
    pub(crate) report: VacuumReport,
}

#[derive(Debug)]
pub(crate) struct StoreRetentionReport {
    pub(crate) vacuumed: Vec<SessionVacuumReport>,
    pub(crate) gc: Option<GcReport>,
    pub(crate) attachments: Option<AttachmentReclamationReport>,
    pub(crate) failures: Vec<String>,
}

/// Run the store portion of one host-owned retention pass.
///
/// Every lever is attempted even if an earlier one stops. The returned report
/// makes partial progress and failures observable to the scheduler and to the
/// deterministic example test; none of these levers is a correctness path.
pub(crate) async fn run_store_retention_pass(
    targets: &StoreRetentionTargets,
    session_ids: &[String],
    attachment_policy: AttachmentReclamationPolicy,
) -> StoreRetentionReport {
    let mut report = StoreRetentionReport {
        vacuumed: Vec::with_capacity(session_ids.len()),
        gc: None,
        attachments: None,
        failures: Vec::new(),
    };

    // This factory-wide audit scans the whole catalog under `BEGIN IMMEDIATE`;
    // live SQLite writers wait at most the store's 15-second busy timeout. The
    // example runs it only hourly and reports contention. A deployment whose
    // catalog can outgrow that window should place it in a quiet maintenance
    // period rather than copy this cadence unchanged.
    match lash::persistence::StoreMaintenance::gc_unreachable(targets.gc_store.as_ref()).await {
        Ok(gc) => report.gc = Some(gc),
        Err(failure) => report.failures.push(format!(
            "store reachability audit stopped: {failure}; partial={:?}",
            failure.partial
        )),
    }

    for session_id in session_ids {
        let request = SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: session_id.clone(),
            relation: SessionRelation::Root,
            policy: SessionPolicy::new(TurnBudget::Unbounded),
        };
        match targets.factory.open_existing_store(&request).await {
            Ok(Some(store)) => {
                match lash::persistence::StoreMaintenance::vacuum(store.as_ref()).await {
                    Ok(vacuum) => report.vacuumed.push(SessionVacuumReport {
                        session_id: session_id.clone(),
                        report: vacuum,
                    }),
                    Err(failure) => report.failures.push(format!(
                        "vacuum session `{session_id}` stopped: {failure}; partial={:?}",
                        failure.partial
                    )),
                }
            }
            Ok(None) => report
                .failures
                .push(format!("vacuum session `{session_id}`: store not found")),
            Err(error) => report
                .failures
                .push(format!("vacuum session `{session_id}`: {error}")),
        }
    }

    match lash::persistence::reclaim_unreferenced_attachments(
        targets.factory.as_ref(),
        targets.attachment_store.as_ref(),
        attachment_policy,
    )
    .await
    {
        Ok(attachments) => report.attachments = Some(attachments),
        Err(failure) => report.failures.push(format!(
            "attachment reclamation stopped: {failure}; partial={:?}",
            failure.partial
        )),
    }

    report
}

/// Run every host-owned retention lever on a real operational cadence.
///
/// `vacuum` is session-scoped. This example's app chat catalog is its root
/// session catalog; process-owned sessions are not represented here and need
/// retention owned by their process host. Store GC is a verify/repair audit;
/// owner-delete transactions remain the correctness path.
pub(crate) fn spawn_retention(
    state: AppStateData,
    targets: StoreRetentionTargets,
    processes: Processes,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(MAINTENANCE_INTERVAL);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let session_ids = match state
                .with_db(|db| {
                    Ok(db
                        .list_chats()?
                        .into_iter()
                        .map(|chat| chat.id)
                        .collect::<Vec<_>>())
                })
                .await
            {
                Ok(session_ids) => session_ids,
                Err(error) => {
                    eprintln!("agent-service: retention could not list chats: {error}");
                    Vec::new()
                }
            };
            // The factory and attachment store must describe the same exclusive
            // deployment. Pairing this backend with the wrong factory can make
            // live content look unreachable and delete it after the grace window.
            let store_report =
                run_store_retention_pass(&targets, &session_ids, scheduled_attachment_policy())
                    .await;
            log_store_report(&store_report);
            prune_terminal_processes(&processes).await;
        }
    });
}

pub(crate) fn scheduled_attachment_policy() -> AttachmentReclamationPolicy {
    AttachmentReclamationPolicy {
        grace_period_ms: RETENTION_WINDOW.as_millis() as u64,
        empty_root_set: EmptyRootSetPolicy::Refuse,
    }
}

fn log_store_report(report: &StoreRetentionReport) {
    let vacuumed_nodes = report
        .vacuumed
        .iter()
        .map(|session| session.report.removed_node_count)
        .sum::<usize>();
    let vacuumed_inputs = report
        .vacuumed
        .iter()
        .map(|session| session.report.removed_pending_turn_input_tombstone_count)
        .sum::<usize>();
    let attachments = report
        .attachments
        .as_ref()
        .map_or(0, |attachments| attachments.reclaimed_count);
    if vacuumed_nodes > 0 || vacuumed_inputs > 0 || attachments > 0 {
        println!(
            "agent-service retention reclaimed {vacuumed_nodes} graph nodes, \
             {vacuumed_inputs} pending-input rows, and {attachments} attachments"
        );
    }
    let gc_blobs = report.gc.as_ref().map_or(0, |gc| gc.deleted_blob_count);
    if gc_blobs > 0 {
        eprintln!(
            "agent-service: verify/repair audit finding: gc_unreachable deleted {gc_blobs} \
             unreachable store blobs; investigate the owner-delete transaction that left them"
        );
    }
    for session in &report.vacuumed {
        if session.report.removed_node_count > 0
            || session.report.removed_pending_turn_input_tombstone_count > 0
        {
            println!(
                "agent-service retention vacuumed session `{}`",
                session.session_id
            );
        }
    }
    for failure in &report.failures {
        eprintln!("agent-service: {failure}");
    }
}

async fn prune_terminal_processes(processes: &Processes) {
    let now_ms = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(since) => since.as_millis() as u64,
        Err(error) => {
            eprintln!("agent-service: retention clock failed: {error}");
            return;
        }
    };
    let cutoff = now_ms.saturating_sub(RETENTION_WINDOW.as_millis() as u64);
    match processes
        .prune(
            cutoff,
            None,
            lash::process::ProjectionWatermark::NoProjector,
        )
        .await
    {
        Ok(report)
            if report.pruned_processes > 0
                || report.pruned_events > 0
                || report.pruned_trigger_deliveries > 0 =>
        {
            println!(
                "agent-service pruned {} terminal processes, {} events, and {} trigger \
                 deliveries (cutoff {cutoff}ms)",
                report.pruned_processes, report.pruned_events, report.pruned_trigger_deliveries
            );
        }
        Ok(_) => {}
        Err(error) => eprintln!("agent-service: process retention prune failed: {error}"),
    }
}
