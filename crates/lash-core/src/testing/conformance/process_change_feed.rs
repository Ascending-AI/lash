//! [`ProcessRegistry`] process change-feed conformance.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::process_registry::{plain_event_type, registration};
use super::*;

pub(super) async fn process_change_feed_never_misses_concurrent_terminal_writers(
    registry: Arc<dyn ProcessRegistry>,
) {
    const WRITER_COUNT: usize = 48;
    const PAGE_LIMIT: usize = 2;
    const MUTATION_EVENT_TYPE: &str = "producer.concurrent_mutation";

    let expected_ids = (0..WRITER_COUNT)
        .map(|index| format!("proc-change-concurrent-{index:02}"))
        .collect::<BTreeSet<_>>();
    let start_barrier = Arc::new(tokio::sync::Barrier::new(WRITER_COUNT + 1));
    let writers_done = Arc::new(AtomicBool::new(false));

    let reader_registry = Arc::clone(&registry);
    let reader_expected_ids = expected_ids.clone();
    let reader_start = Arc::clone(&start_barrier);
    let reader_done = Arc::clone(&writers_done);
    let reader = crate::task::spawn(async move {
        reader_start.wait().await;
        let mut cursor = ProcessChangeCursor::initial();
        let mut terminal_observations = BTreeMap::<String, usize>::new();

        loop {
            let (records, next_cursor) = reader_registry
                .processes_changed_since(cursor, PAGE_LIMIT)
                .await
                .expect("concurrent feed read");
            if records.is_empty() {
                if reader_done.load(Ordering::SeqCst)
                    && reader_expected_ids
                        .iter()
                        .all(|id| terminal_observations.get(id).copied() == Some(1))
                {
                    return terminal_observations;
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
            for change in records {
                let ProcessChange::Upsert { record } = change else {
                    continue;
                };
                if reader_expected_ids.contains(record.id.as_str()) && record.is_terminal() {
                    *terminal_observations.entry(record.id.clone()).or_default() += 1;
                }
            }
        }
    });

    let mut writer_handles = Vec::new();
    for writer_index in 0..WRITER_COUNT {
        let writer_registry = Arc::clone(&registry);
        let writer_start = Arc::clone(&start_barrier);
        let process_id = format!("proc-change-concurrent-{writer_index:02}");
        writer_handles.push(crate::task::spawn(async move {
            writer_start.wait().await;
            writer_registry
                .register_process(
                    registration(&process_id)
                        .with_extra_event_types([plain_event_type(MUTATION_EVENT_TYPE)]),
                )
                .await
                .expect("concurrent writer register");

            if writer_index % 3 == 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            } else {
                tokio::task::yield_now().await;
            }

            writer_registry
                .append_event(
                    &process_id,
                    ProcessEventAppendRequest::new(
                        MUTATION_EVENT_TYPE,
                        serde_json::json!({ "writer": writer_index }),
                    ),
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
                    ProcessAwaitOutput::Success {
                        value: serde_json::json!({ "writer": writer_index }),
                        control: None,
                    },
                    crate::ProcessCompletionAuthority::external_owner(),
                )
                .await
                .expect("concurrent writer complete");
        }));
    }

    for handle in writer_handles {
        handle.await.expect("concurrent writer task panicked");
    }
    writers_done.store(true, Ordering::SeqCst);

    let terminal_observations = tokio::time::timeout(Duration::from_secs(10), reader)
        .await
        .expect("reader timed out waiting for every terminal transition")
        .expect("reader task panicked");
    let missing = expected_ids
        .iter()
        .filter(|id| terminal_observations.get(*id).copied() != Some(1))
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
