use super::*;
use std::collections::VecDeque;
use std::sync::Mutex;

struct RetainingSignalWaitProcesses {
    pages: tokio::sync::Mutex<
        VecDeque<lash_core::ProcessEventReadOutcome<lash_core::ProcessEventPage>>,
    >,
    written_waits: Mutex<Vec<lash_core::WaitState>>,
    terminal: bool,
}

impl RetainingSignalWaitProcesses {
    fn new(
        pages: impl IntoIterator<Item = lash_core::ProcessEventReadOutcome<lash_core::ProcessEventPage>>,
    ) -> Self {
        Self {
            pages: tokio::sync::Mutex::new(pages.into_iter().collect()),
            written_waits: Mutex::new(Vec::new()),
            terminal: false,
        }
    }

    /// A process whose terminal is stored: the registry refuses its waits.
    fn terminal(mut self) -> Self {
        self.terminal = true;
        self
    }

    fn written_waits(&self) -> Vec<lash_core::WaitState> {
        self.written_waits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

#[async_trait::async_trait]
impl SignalWaitProcesses for RetainingSignalWaitProcesses {
    async fn current_wait(&self) -> Result<Option<lash_core::WaitState>, lash_core::PluginError> {
        Ok(None)
    }

    async fn event_page(
        &self,
        _after_sequence: u64,
        _limit: std::num::NonZeroUsize,
    ) -> Result<
        lash_core::ProcessEventReadOutcome<lash_core::ProcessEventPage>,
        lash_core::PluginError,
    > {
        self.pages.lock().await.pop_front().ok_or_else(|| {
            lash_core::PluginError::Session("signal-wait fixture exhausted pages".to_string())
        })
    }

    async fn set_wait(&self, wait: lash_core::WaitState) -> Result<(), lash_core::PluginError> {
        if self.terminal {
            return Err(lash_core::PluginError::Session(
                "terminal process cannot enter a wait state".to_string(),
            ));
        }
        self.written_waits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(wait);
        Ok(())
    }

    async fn clear_wait(&self) -> Result<(), lash_core::PluginError> {
        if self.terminal {
            return Err(lash_core::PluginError::Session(
                "terminal process cannot clear a wait state".to_string(),
            ));
        }
        Ok(())
    }

    async fn is_terminal(&self) -> Result<bool, lash_core::PluginError> {
        Ok(self.terminal)
    }
}

/// A redrive that replays a body over its stored terminal reaches the signal
/// wait's writes again, and the registry refuses a terminal process's wait
/// state. The write is settled, not failed, so the body reissues the wait it
/// recorded instead of ending where its journal holds that wait (FIG-3673).
#[tokio::test]
async fn a_wait_write_refused_on_a_terminal_process_is_settled() {
    let processes = RetainingSignalWaitProcesses::new([]).terminal();
    let written = processes
        .set_wait(lash_core::WaitState {
            since_ms: 1,
            kind: lash_core::WaitKind::Signal {
                name: "go".to_string(),
                event_type: "process.signal.go".to_string(),
                key: "key".to_string(),
                ordinal: 1,
            },
        })
        .await;
    assert!(
        written.is_err(),
        "the registry refuses a terminal process's wait"
    );
    settle_wait_write(&processes, written)
        .await
        .expect("a refused wait on a terminal process is settled");
    let cleared = SignalWaitProcesses::clear_wait(&processes).await;
    settle_wait_write(&processes, cleared)
        .await
        .expect("a refused clear on a terminal process is settled");

    let live = RetainingSignalWaitProcesses::new([]);
    let refused = Err(lash_core::PluginError::Session("store fault".to_string()));
    assert!(
        settle_wait_write(&live, refused).await.is_err(),
        "a refusal on a live process stays a failure"
    );
}

fn process_event(
    process_id: &ProcessId,
    sequence: u64,
    event_type: &str,
    payload: serde_json::Value,
) -> lash_core::ProcessEvent {
    lash_core::ProcessEvent {
        process_id: process_id.clone(),
        sequence,
        event_type: event_type.to_string(),
        payload,
        invocation: lash_core::RuntimeInvocation::effect(
            lash_core::EffectAddress::new(
                lash_core::ExecutionScope::turn("signal-wait-session", "signal-wait-turn"),
                format!("signal-wait:{sequence}"),
            )
            .expect("valid signal-wait test effect address"),
            lash_core::RuntimeAttribution::for_turn(
                "signal-wait-session",
                "signal-wait-turn",
                sequence as usize,
                0,
            ),
            format!("signal-wait:{sequence}"),
        ),
        semantics: lash_core::runtime::ProcessEventSemantics::default(),
        occurred_at: sequence,
    }
}

async fn establish_ready_wait(
    processes: &RetainingSignalWaitProcesses,
) -> Result<(), SignalWaitSetupError> {
    establish_signal_wait(
        processes,
        "ready".to_string(),
        "signal.ready".to_string(),
        "process:signal-wait:signal.ready:1".to_string(),
        1,
    )
    .await
}

#[tokio::test]
async fn retention_before_a_wait_match_returns_the_typed_error_without_writing_a_wait() {
    let processes =
        RetainingSignalWaitProcesses::new([lash_core::ProcessEventReadOutcome::NoLongerRetained(
            lash_core::ProcessEventHistoryRetention::Pruned {
                terminal_label: "completed".to_string(),
                pruned_at_ms: 42,
            },
        )]);

    let error = establish_ready_wait(&processes)
        .await
        .expect_err("pruned history must refuse signal-wait setup");
    assert!(matches!(
        error,
        SignalWaitSetupError::Read(lash_core::PluginError::ProcessNoLongerRetained {
            terminal_label,
            pruned_at_ms: 42,
        }) if terminal_label == "completed"
    ));
    assert!(processes.written_waits().is_empty());
}

#[tokio::test]
async fn retention_after_an_earlier_wait_match_discards_the_timestamp_and_writes_no_wait() {
    let process_id = lash_core::process_id_for_test("signal-wait-pruned-later");
    let key = "process:signal-wait:signal.ready:1";
    let matched_wait = lash_core::WaitState {
        since_ms: 77,
        kind: lash_core::WaitKind::Signal {
            name: "ready".to_string(),
            event_type: "signal.ready".to_string(),
            key: key.to_string(),
            ordinal: 1,
        },
    };
    let mut rows = vec![process_event(
        &process_id,
        1,
        "process.waiting",
        serde_json::json!({ "wait": matched_wait }),
    )];
    rows.extend((2..=129).map(|sequence| {
        process_event(
            &process_id,
            sequence,
            "snapshot.filler",
            serde_json::Value::Null,
        )
    }));
    let first_page = lash_core::ProcessEventPage::from_full_rows(
        rows,
        std::num::NonZeroUsize::new(128).expect("non-zero page size"),
    );
    assert!(matches!(
        first_page.more,
        lash_core::ProcessEventPageMore::More { .. }
    ));
    let processes = RetainingSignalWaitProcesses::new([
        lash_core::ProcessEventReadOutcome::Retained(first_page),
        lash_core::ProcessEventReadOutcome::NoLongerRetained(
            lash_core::ProcessEventHistoryRetention::Pruned {
                terminal_label: "completed".to_string(),
                pruned_at_ms: 43,
            },
        ),
    ]);

    let error = establish_ready_wait(&processes)
        .await
        .expect_err("a pruned later page must discard an earlier matched timestamp");
    assert!(matches!(
        error,
        SignalWaitSetupError::Read(lash_core::PluginError::ProcessNoLongerRetained {
            pruned_at_ms: 43,
            ..
        })
    ));
    assert!(processes.written_waits().is_empty());
}
