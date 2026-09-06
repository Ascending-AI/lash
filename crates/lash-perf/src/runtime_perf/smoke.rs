//! Completion-driven smoke execution, separate from benchmark deadlines.

use std::future::Future;

#[derive(Clone, Copy)]
enum ExecutionMode {
    Smoke,
    Measurement,
}

tokio::task_local! {
    static MODE: ExecutionMode;
}

pub(crate) fn is_smoke() -> bool {
    MODE.try_with(|mode| matches!(mode, ExecutionMode::Smoke))
        .unwrap_or(false)
}

pub(crate) fn with_budget<F: Future>(
    budget: std::time::Duration,
    future: F,
) -> impl Future<Output = Result<F::Output, tokio::time::error::Elapsed>> {
    if is_smoke() {
        futures_util::future::Either::Left(futures_util::FutureExt::map(future, Ok))
    } else {
        futures_util::future::Either::Right(tokio::time::timeout(budget, future))
    }
}

pub(crate) async fn execute<T>(
    smoke: bool,
    scenario: super::scenarios::RuntimePerfScenario,
    turns: usize,
    future: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    use super::scenarios::RuntimePerfScenario;
    let mode = if smoke {
        ExecutionMode::Smoke
    } else {
        ExecutionMode::Measurement
    };
    let expected = match scenario {
        RuntimePerfScenario::RlmProcessAsyncToolCompletion
        | RuntimePerfScenario::RlmAsyncToolCompletion => Some(2 * turns),
        // The standard provider reuses the first tool result from session
        // history on subsequent turns; RLM executes its two calls every turn.
        RuntimePerfScenario::StandardAsyncToolCompletion => Some(1),
        _ => None,
    };
    let (tasks, receiver) = mpsc::unbounded_channel();
    let witness = Arc::new(CompletionWitness { tasks });
    MODE.scope(
        mode,
        COMPLETIONS.scope(witness, observe(future, receiver, expected)),
    )
    .await
}

use futures_util::{StreamExt, stream::FuturesUnordered};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::task::AbortOnDropHandle;

type Completion = AbortOnDropHandle<anyhow::Result<()>>;

pub(crate) struct CompletionWitness {
    tasks: mpsc::UnboundedSender<Completion>,
}

tokio::task_local! {
    static COMPLETIONS: Arc<CompletionWitness>;
}

pub(crate) fn completion_witness() -> Option<Arc<CompletionWitness>> {
    COMPLETIONS.try_with(Arc::clone).ok()
}

impl CompletionWitness {
    pub(crate) fn spawn(&self, future: impl Future<Output = anyhow::Result<()>> + Send + 'static) {
        let task = AbortOnDropHandle::new(tokio::spawn(future));
        // A closed observer means the scenario already failed. Dropping the
        // returned handle cancels its completion task.
        let _ = self.tasks.send(task);
    }
}

async fn observe<T>(
    future: impl Future<Output = anyhow::Result<T>>,
    mut tasks: mpsc::UnboundedReceiver<Completion>,
    expected: Option<usize>,
) -> anyhow::Result<T> {
    tokio::pin!(future);
    let mut pending = FuturesUnordered::new();
    let mut delivered = 0;
    let result = loop {
        tokio::select! {
            result = &mut future => break result?,
            Some(task) = tasks.recv() => pending.push(task),
            Some(result) = pending.next(), if !pending.is_empty() => {
                result??;
                delivered += 1;
            }
        }
    };
    while let Ok(task) = tasks.try_recv() {
        pending.push(task);
    }
    while let Some(result) = pending.next().await {
        result??;
        delivered += 1;
    }
    if let Some(expected) = expected {
        anyhow::ensure!(
            delivered == expected,
            "completion witness expected {expected} deliveries, observed {delivered}"
        );
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::super::scenarios::RuntimePerfScenario;
    use super::*;

    #[tokio::test]
    async fn completion_failure_interrupts_a_parked_scenario() {
        let result = execute(true, RuntimePerfScenario::Standard, 1, async {
            completion_witness()
                .unwrap()
                .spawn(async { anyhow::bail!("rejected delivery witness") });
            std::future::pending::<anyhow::Result<()>>().await
        })
        .await;
        assert_eq!(result.unwrap_err().to_string(), "rejected delivery witness");
    }

    #[tokio::test]
    async fn process_completion_witness_rejects_a_missing_delivery() {
        let result = execute(
            true,
            RuntimePerfScenario::RlmProcessAsyncToolCompletion,
            1,
            async {
                completion_witness().unwrap().spawn(async { Ok(()) });
                Ok(())
            },
        )
        .await;
        assert_eq!(
            result.unwrap_err().to_string(),
            "completion witness expected 2 deliveries, observed 1"
        );
    }

    #[tokio::test]
    async fn completion_witness_waits_for_every_registered_task() {
        let (release, released) = tokio::sync::oneshot::channel();
        let result = execute(
            true,
            RuntimePerfScenario::RlmProcessAsyncToolCompletion,
            1,
            async {
                let witness = completion_witness().unwrap();
                witness.spawn(async move {
                    released.await?;
                    Ok(())
                });
                witness.spawn(async move {
                    release.send(()).unwrap();
                    Ok(())
                });
                Ok(())
            },
        )
        .await;
        result.unwrap();
    }
}
