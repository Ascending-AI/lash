//! Process Change Feed recovery after Tombstone Compaction.

use super::process_registry::registration;
use super::*;
use crate::{PluginError, ProjectionWatermark};

/// Test-consumer model for the typed Process Change Feed recovery contract:
/// a pruned cursor requires a complete relist before resuming at the reported
/// Tombstone Compaction horizon.
pub(super) async fn changes_after_full_relist_if_required(
    registry: &Arc<dyn ProcessRegistry>,
    limit: usize,
) -> (Vec<ProcessChange>, ProcessChangeCursor) {
    match registry
        .processes_changed_since(ProcessChangeCursor::initial(), limit)
        .await
    {
        Ok(result) => result,
        Err(PluginError::ProcessChangeCursorPruned {
            tombstone_compaction_horizon,
            ..
        }) => {
            registry
                .list_processes(&crate::ProcessListFilter::default())
                .await
                .expect("full relist after a pruned Process Change Feed cursor");
            registry
                .processes_changed_since(tombstone_compaction_horizon, limit)
                .await
                .expect("resume Process Change Feed at the compaction horizon")
        }
        Err(error) => panic!("read Process Change Feed from its oldest retained cursor: {error}"),
    }
}

/// Prove Tombstone Compaction records a read-side horizon even when the host
/// explicitly declares that no projector constrains deletion.
pub async fn process_change_cursor_below_tombstone_compaction_horizon_is_refused(
    registry: Arc<dyn ProcessRegistry>,
) {
    let process_id = "change-feed-prune-horizon";
    registry
        .register_process(registration(process_id))
        .await
        .expect("register prune-horizon process");
    registry
        .complete_process(
            process_id,
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::Value::Null,
            )),
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete prune-horizon process");
    let (_, terminal_cursor) = registry
        .processes_changed_since(ProcessChangeCursor::initial(), 100)
        .await
        .expect("project terminal process");
    registry
        .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::UpTo(terminal_cursor))
        .await
        .expect("prune terminal process");
    let (changes, deletion_cursor) = registry
        .processes_changed_since(terminal_cursor, 100)
        .await
        .expect("project deletion tombstone");
    assert!(changes.into_iter().any(|change| matches!(
        change,
        ProcessChange::Deleted { tombstone } if tombstone.process_id == process_id
    )));
    assert_eq!(
        registry
            .compact_process_tombstones(u64::MAX, ProjectionWatermark::NoProjector, None)
            .await
            .expect("compact tombstone without a configured projector"),
        1
    );

    let error = registry
        .processes_changed_since(ProcessChangeCursor::initial(), 100)
        .await
        .expect_err("a cursor below the Tombstone Compaction horizon must be refused");
    assert!(matches!(
        error,
        PluginError::ProcessChangeCursorPruned {
            requested_cursor,
            tombstone_compaction_horizon,
        } if requested_cursor == ProcessChangeCursor::initial()
            && tombstone_compaction_horizon == deletion_cursor
    ));
    registry
        .processes_changed_since(deletion_cursor, 100)
        .await
        .expect("the horizon cursor itself remains resumable");
}
