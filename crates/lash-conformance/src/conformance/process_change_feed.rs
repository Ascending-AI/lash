//! [`ProcessRegistry`] process change-feed conformance.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::process_registry::registration;
use super::*;
use pretty_assertions::assert_eq;

/// Hang detector for the reader, not a latency expectation. Every terminal
/// transition is committed before the bound starts, and the reader does most
/// of its paging after that: a store that serializes its writers on the change
/// clock lets few reads through while they run. A loaded store only makes the
/// reader later.
const FEED_HANG_BOUND: Duration = Duration::from_secs(60);

/// How far the feed reader has got, shared so a hung reader can be reported.
#[derive(Clone, Default)]
struct ReaderProgress {
    cursor: ProcessChangeCursor,
    terminal_observations: BTreeMap<String, usize>,
}

/// The expected labels whose terminal record the feed serves after `cursor`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the read is established by the setup above"
)]
async fn terminal_labels_after(
    registry: &Arc<dyn ProcessRegistry>,
    mut cursor: ProcessChangeCursor,
    expected_ids: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut labels = BTreeSet::new();
    loop {
        let (records, next_cursor) = registry
            .processes_changed_since(cursor, 256)
            .await
            .expect("committed feed read");
        if records.is_empty() {
            return labels;
        }
        cursor = next_cursor;
        for change in records {
            if let ProcessChange::Upsert { record } = change
                && record.is_terminal()
                && let Some(label) = record.identity.label.as_deref()
                && expected_ids.contains(label)
            {
                labels.insert(label.to_string());
            }
        }
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn process_change_feed_never_misses_concurrent_terminal_writers(
    registry: Arc<dyn ProcessRegistry>,
) {
    const WRITER_COUNT: usize = 48;
    const PAGE_LIMIT: usize = 2;

    let expected_ids = (0..WRITER_COUNT)
        .map(|index| format!("proc-change-concurrent-{index:02}"))
        .collect::<BTreeSet<_>>();
    let start_barrier = Arc::new(tokio::sync::Barrier::new(WRITER_COUNT + 1));
    let writers_done = Arc::new(AtomicBool::new(false));
    let progress = Arc::new(std::sync::Mutex::new(ReaderProgress::default()));

    let reader_registry = Arc::clone(&registry);
    let reader_expected_ids = expected_ids.clone();
    let reader_start = Arc::clone(&start_barrier);
    let reader_done = Arc::clone(&writers_done);
    let reader_progress = Arc::clone(&progress);
    let reader = crate::task::spawn(async move {
        reader_start.wait().await;
        let mut cursor = ProcessChangeCursor::initial();

        loop {
            let (records, next_cursor) = reader_registry
                .processes_changed_since(cursor, PAGE_LIMIT)
                .await
                .expect("concurrent feed read");
            if records.is_empty() {
                // The feed is drained up to `cursor`. Once every writer has
                // returned and its terminal transition has been read, the
                // reader has reached the committed set; a repeat would have
                // been counted on the way here.
                let reached = reader_done.load(Ordering::SeqCst) && {
                    let progress = reader_progress.lock().expect("reader progress lock");
                    reader_expected_ids
                        .iter()
                        .all(|id| progress.terminal_observations.contains_key(id))
                };
                if reached {
                    return;
                }
                tokio::task::yield_now().await;
                tokio::time::sleep(Duration::from_millis(1)).await;
                continue;
            }

            assert!(
                next_cursor.store_sequence() > cursor.store_sequence(),
                "a non-empty process change page must advance the cursor"
            );
            cursor = next_cursor;
            let mut progress = reader_progress.lock().expect("reader progress lock");
            progress.cursor = cursor;
            for change in records {
                let ProcessChange::Upsert { record } = change else {
                    continue;
                };
                // Writers register under their own label; the feed carries
                // it on every record, whatever id the registrar minted.
                let Some(label) = record.identity.label.as_deref() else {
                    continue;
                };
                if reader_expected_ids.contains(label) && record.is_terminal() {
                    *progress
                        .terminal_observations
                        .entry(label.to_string())
                        .or_default() += 1;
                }
            }
        }
    });

    let mut writer_handles = Vec::new();
    for writer_index in 0..WRITER_COUNT {
        let writer_registry = Arc::clone(&registry);
        let writer_start = Arc::clone(&start_barrier);
        let label = format!("proc-change-concurrent-{writer_index:02}");
        writer_handles.push(crate::task::spawn(async move {
            writer_start.wait().await;
            let process_id = writer_registry
                .register_process(registration(&label))
                .await
                .expect("concurrent writer register")
                .id;

            let runner = start_runner(writer_registry.as_ref(), &process_id).await;

            if writer_index % 3 == 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            } else {
                tokio::task::yield_now().await;
            }

            writer_registry
                .append_event_with_authority(
                    &process_id,
                    call_wait_event(
                        &process_id,
                        "concurrent-mutation",
                        &writer_index.to_string(),
                        serde_json::json!({ "writer": writer_index }),
                    ),
                    &runner,
                )
                .await
                .expect("concurrent writer mutate");

            if writer_index % 2 == 0 {
                tokio::task::yield_now().await;
            } else {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }

            writer_registry
                .complete_process(
                    &process_id,
                    ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                        serde_json::json!({ "writer": writer_index }),
                    )),
                    crate::ProcessCompletionAuthority::workflow_key(&process_id),
                )
                .await
                .expect("concurrent writer complete");
        }));
    }

    for handle in writer_handles {
        handle.await.expect("concurrent writer task panicked");
    }
    writers_done.store(true, Ordering::SeqCst);

    // Every terminal transition is committed. The reader finishes when its
    // feed reaches that set, however long the store takes to serve it; the
    // bound only catches a reader that never gets there, and its report tells
    // a transition the reader's cursor passed from one still ahead of it.
    let reader_abort = reader.abort_handle();
    match tokio::time::timeout(FEED_HANG_BOUND, reader).await {
        Ok(joined) => joined.expect("reader task panicked"),
        Err(_) => {
            reader_abort.abort();
            let ReaderProgress {
                cursor,
                terminal_observations,
            } = progress.lock().expect("reader progress lock").clone();
            let committed =
                terminal_labels_after(&registry, ProcessChangeCursor::initial(), &expected_ids)
                    .await;
            let ahead = terminal_labels_after(&registry, cursor, &expected_ids).await;
            let passed = committed
                .iter()
                .filter(|id| !terminal_observations.contains_key(*id) && !ahead.contains(*id))
                .collect::<Vec<_>>();
            let unread = ahead
                .iter()
                .filter(|id| !terminal_observations.contains_key(*id))
                .collect::<Vec<_>>();
            panic!(
                "the reader did not reach the committed terminal set within \
                 {FEED_HANG_BOUND:?}: {} of {} terminal transitions committed, {} read; \
                 the reader's cursor {cursor:?} passed {passed:?} without reading them \
                 (missed) and had not yet read {unread:?} (still ahead of it)",
                committed.len(),
                expected_ids.len(),
                terminal_observations.len(),
            );
        }
    }
    let terminal_observations = progress
        .lock()
        .expect("reader progress lock")
        .terminal_observations
        .clone();
    let missing = expected_ids
        .iter()
        .filter(|id| !terminal_observations.contains_key(*id))
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "process change feed missed terminal transitions for {missing:?}; observed {terminal_observations:?}"
    );
    let repeated = terminal_observations
        .iter()
        .filter(|(_, count)| **count != 1)
        .collect::<Vec<_>>();
    assert!(
        repeated.is_empty(),
        "process change feed returned terminal transitions more than once: {repeated:?}"
    );
    assert_eq!(
        terminal_observations.len(),
        expected_ids.len(),
        "process change feed should observe exactly the expected terminal processes"
    );
}
