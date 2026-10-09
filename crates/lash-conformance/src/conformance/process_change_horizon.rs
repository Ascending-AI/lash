//! Process Change Feed recovery after Tombstone Compaction.

use super::process_registry::registration;
use super::*;
use crate::{PluginError, ProjectionWatermark};
use pretty_assertions::assert_eq;

/// Prove Tombstone Compaction records a read-side horizon even when the host
/// explicitly declares that no projector constrains deletion.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn process_change_cursor_below_tombstone_compaction_horizon_is_refused(
    registry: Arc<dyn ProcessRegistry>,
) {
    let process_id = "change-feed-prune-horizon";
    let change_feed_prune_horizon_record = registry
        .register_process(registration(process_id))
        .await
        .expect("register prune-horizon process");
    let process_id = change_feed_prune_horizon_record.id.clone();
    registry
        .complete_process(
            &process_id,
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::Value::Null,
            )),
            ProcessCompletionAuthority::workflow_key(&process_id),
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
            .compact_process_tombstones(u64::MAX, ProjectionWatermark::NoProjector)
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

/// FIG-5569: a filter can make a page empty without finishing enumeration.
/// Keyset work stays bounded and every page retains the pre-scan change fence.
#[expect(
    clippy::expect_used,
    reason = "conformance fixture and its named contract assertions"
)]
pub async fn process_roster_pages_preserve_the_fence_through_empty_filtered_pages(
    registry: Arc<dyn ProcessRegistry>,
) {
    for index in 0..3 {
        registry
            .register_process(registration(&format!("roster-{index}")))
            .await
            .expect("register roster");
    }
    let bound = std::num::NonZeroUsize::MIN;
    let filter = crate::ProcessListFilter {
        identity_label: Some("no-row-has-this-label".to_owned()),
        status: crate::ProcessStatusFilter::Any,
        ..crate::ProcessListFilter::default()
    };
    let first = registry
        .list_processes_page(&filter, bound, None)
        .await
        .expect("filtered page");
    let fence = first.change_cursor;
    assert!(first.records.is_empty());
    let cursor = first
        .continuation
        .expect("empty page still has candidates after it");
    assert!(matches!(
        registry
            .list_processes_page(
                &crate::ProcessListFilter::default(),
                bound,
                Some(cursor.clone())
            )
            .await,
        Err(crate::PluginError::ProcessRosterFilterMismatch {})
    ));
    let mut continuation = Some(cursor);
    let mut pages = 1;
    while let Some(cursor) = continuation {
        let page = registry
            .list_processes_page(&filter, bound, Some(cursor))
            .await
            .expect("continue filtered page");
        assert!(page.records.is_empty());
        assert_eq!(page.change_cursor, fence);
        continuation = page.continuation;
        pages += 1;
        assert!(
            pages <= 3,
            "a continuation advances past examined candidates"
        );
    }
    assert_eq!(pages, 3);
    let bounds = registry
        .process_change_bounds()
        .await
        .expect("verified high water");
    assert_eq!(bounds.current, fence);
    assert_eq!(bounds.retained_after, crate::ProcessChangeCursor::initial());
}
