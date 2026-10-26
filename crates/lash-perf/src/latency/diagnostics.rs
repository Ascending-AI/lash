//! Case-owned marker tasks. No lane waits for diagnostic completion.
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Mutex;

use anyhow::{Context, Result, ensure};
use lash_protocol_types::sync::MutexExt;
use serde::Serialize;
use tokio::task::JoinSet;

use super::{PollMarks, STORE_POLL_TIMEOUT, Sample};

pub(super) const PENDING_LIMIT: usize = 256;
type Key = (String, String);

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum DiagnosticDisposition {
    #[default]
    Pending,
    Complete,
    TimedOut,
    Failed(String),
    Refused,
}

pub(crate) struct CompletedSample {
    pub(crate) sample: Sample,
    pub(crate) pending: Key,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct Receipt {
    pub(crate) marker_waits_before_next_send: usize,
    pub(crate) pending_limit: usize,
    pub(crate) pending_peak: usize,
    pub(crate) joined: usize,
    pub(crate) refused: usize,
    pub(crate) timed_out: usize,
    pub(crate) failed: usize,
    pub(crate) pending_at_teardown: usize,
    pub(crate) join_wall_ms: f64,
}

#[derive(Default)]
struct State {
    tasks: JoinSet<(Key, PollMarks)>,
    // Finished rows are bounded by the case's declared sample population.
    marks: BTreeMap<Key, (DiagnosticDisposition, PollMarks)>,
    keys: BTreeMap<tokio::task::Id, Key>,
    receipt: Receipt,
    failure: Option<String>,
}

impl State {
    fn completed(
        &mut self,
        result: Result<(tokio::task::Id, (Key, PollMarks)), tokio::task::JoinError>,
    ) -> Result<()> {
        let (key, disposition, marks) = match result {
            Ok((id, (key, marks))) => {
                assert_eq!(self.keys.remove(&id).as_ref(), Some(&key));
                let disposition = if let Some(error) = &marks.error {
                    self.receipt.failed += 1;
                    DiagnosticDisposition::Failed(error.clone())
                } else if marks.timed_out {
                    self.receipt.timed_out += 1;
                    DiagnosticDisposition::TimedOut
                } else {
                    self.receipt.joined += 1;
                    DiagnosticDisposition::Complete
                };
                (key, disposition, marks)
            }
            Err(error) => {
                self.receipt.failed += 1;
                let key = self
                    .keys
                    .remove(&error.id())
                    .context("unowned diagnostic task")?;
                (
                    key,
                    DiagnosticDisposition::Failed(error.to_string()),
                    PollMarks::default(),
                )
            }
        };
        ensure!(
            self.marks.insert(key, (disposition, marks)).is_none(),
            "duplicate diagnostic identity"
        );
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct Diagnostics(Mutex<State>);
impl Diagnostics {
    pub(crate) fn start(
        &self,
        session: &lash_kernel_value::identity::SessionId,
        input: &lash_vocabulary::identity::InputId,
        future: impl Future<Output = PollMarks> + Send + 'static,
    ) -> Key {
        let key = (session.as_str().to_owned(), input.as_str().to_owned());
        let mut state = self.0.lock_recover();
        while let Some(result) = state.tasks.try_join_next_with_id() {
            if let Err(error) = state.completed(result) {
                state.failure = Some(error.to_string());
            }
        }
        if state.tasks.len() == PENDING_LIMIT {
            state.receipt.refused += 1;
            assert!(
                state
                    .marks
                    .insert(
                        key.clone(),
                        (DiagnosticDisposition::Refused, PollMarks::default())
                    )
                    .is_none()
            );
        } else {
            let task_key = key.clone();
            // JoinSet owns cancellation before the task can reach an await.
            let task = state.tasks.spawn(async move {
                let marks =
                    tokio::time::timeout(STORE_POLL_TIMEOUT + super::STORE_POLL_FLOOR, future)
                        .await
                        .unwrap_or_else(|_| PollMarks {
                            timed_out: true,
                            ..PollMarks::default()
                        });
                (task_key, marks)
            });
            state.keys.insert(task.id(), key.clone());
            state.receipt.pending_peak = state.receipt.pending_peak.max(state.tasks.len());
        }
        key
    }

    pub(crate) async fn finish(
        &self,
        completed: Vec<CompletedSample>,
    ) -> Result<(Vec<Sample>, Receipt)> {
        let started = std::time::Instant::now();
        let mut state = std::mem::take(&mut *self.0.lock_recover());
        while let Some(result) = state.tasks.join_next_with_id().await {
            state.completed(result)?;
        }
        ensure!(
            state.failure.is_none(),
            "diagnostic collection failed: {:?}",
            state.failure
        );
        let mut samples = Vec::with_capacity(completed.len());
        for CompletedSample {
            mut sample,
            pending,
        } in completed
        {
            ensure!(
                pending == (sample.session_id.clone(), sample.input_id.clone()),
                "diagnostic identity mismatch"
            );
            let (disposition, marks) = state
                .marks
                .remove(&pending)
                .context("missing diagnostic disposition")?;
            sample.clocks.admission_observed = marks.admission_observed;
            sample.clocks.terminal_observed = marks.terminal_observed;
            sample.poller_timed_out = marks.timed_out;
            if disposition != DiagnosticDisposition::Complete {
                let diagnostic_error = format!("diagnostic {disposition:?}");
                sample.error = Some(match sample.error {
                    Some(error) => format!("{error}; {diagnostic_error}"),
                    None => diagnostic_error,
                });
            }
            sample.diagnostic = disposition;
            samples.push(sample);
        }
        ensure!(
            state.marks.is_empty(),
            "unmatched diagnostics: {}",
            state.marks.len()
        );
        ensure!(state.keys.is_empty(), "orphan diagnostic tasks");
        state.receipt.pending_limit = PENDING_LIMIT;
        state.receipt.pending_at_teardown = state.tasks.len();
        state.receipt.join_wall_ms = started.elapsed().as_secs_f64() * 1000.0;
        Ok((samples, state.receipt))
    }
}
