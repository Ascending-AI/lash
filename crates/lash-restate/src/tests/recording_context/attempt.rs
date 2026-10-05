//! How a recording context ends an attempt.
//!
//! The engine ends an attempt at the step whose fault is retried
//! (`run_json_or_retry_send`): it journals nothing for the step, keeps only
//! the fault's text, and runs nothing after it. A recording context does the
//! same. A test that expects such a fault runs its body as an attempt
//! ([`AttemptEnd::run`]) and reads the failure the engine would keep.

use super::*;

/// What the engine keeps of an attempt a step's fault ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AttemptFailure {
    /// The step whose fault ended the attempt.
    pub(crate) effect: String,
    /// The failure text, as the step's closure returned it.
    pub(crate) failure: String,
}

/// The attempt a recording context is running, if a test opened one.
#[derive(Default)]
pub(crate) struct AttemptEnd {
    open: Mutex<Option<tokio::sync::oneshot::Sender<AttemptFailure>>>,
}

impl AttemptEnd {
    /// Runs `body` as one attempt: its value, or the failure of the step
    /// that ended it. The body is dropped where the attempt ends, as a
    /// handler's future is.
    pub(crate) async fn run<T>(&self, body: impl Future<Output = T>) -> Result<T, AttemptFailure> {
        let (end, ended) = tokio::sync::oneshot::channel();
        assert!(
            self.open.lock_recover().replace(end).is_none(),
            "a recording context runs one attempt at a time"
        );
        let outcome = tokio::select! {
            biased;
            failure = ended => Err(failure.expect("an open attempt ends with its failure")),
            value = body => Ok(value),
        };
        self.open.lock_recover().take();
        outcome
    }

    /// Ends the open attempt with the fault of `effect` and never returns.
    /// A fault with no attempt open fails the test: nothing may run past a
    /// step whose fault the engine retries.
    async fn end(&self, effect: String, failure: String) -> std::convert::Infallible {
        let Some(open) = self.open.lock_recover().take() else {
            panic!(
                "step `{effect}` ended its attempt with a retried fault, and the test opened no \
                 attempt to observe it: {failure}"
            );
        };
        // The attempt's observer may already be gone; the step still never
        // returns.
        let _ = open.send(AttemptFailure { effect, failure });
        std::future::pending().await
    }
}

/// `run_json_or_retry_send` for a recording context: an `Err` ends the
/// attempt before anything is journaled, and an `Ok` value is journaled
/// through the context's `run_json_send` as `{"Ok": value}`, the record
/// encoding the committed replay corpus pins for these contexts.
pub(crate) fn run_json_or_end_attempt<'ctx, 'run, C, S, Fut>(
    context: &'run C,
    attempt: &'run AttemptEnd,
    step: S,
    future: Fut,
) -> impl Future<Output = Result<Json<S::Output>, TerminalError>> + Send + 'run
where
    'ctx: 'run,
    C: RestateControllerContext<'ctx>,
    S: crate::JournalStep,
    Fut: Future<Output = Result<S::Output, String>> + Send + 'run,
{
    let effect = crate::journal_step_name(&step);
    let run = context.run_json_send(CommittedStep(step), None, async move {
        match future.await {
            Ok(value) => Ok(value),
            Err(failure) => match attempt.end(effect, failure).await {},
        }
    });
    async move {
        let Json(recorded) = run.await?;
        Ok(Json(recorded.unwrap_or_else(|fault| {
            panic!("a recording context never journals a step's fault: {fault}")
        })))
    }
}

/// A recording context's `run_json_or_retry_send`, over its `attempt` field.
macro_rules! run_json_or_retry_send_ends_the_attempt {
    () => {
        fn run_json_or_retry_send<'run, S, Fut>(
            &'run self,
            step: S,
            future: Fut,
        ) -> impl Future<Output = Result<Json<S::Output>, TerminalError>> + Send + 'run
        where
            'ctx: 'run,
            S: crate::JournalStep,
            Fut: Future<Output = Result<S::Output, String>> + Send + 'run,
        {
            run_json_or_end_attempt(self, &self.attempt, step, future)
        }
    };
}

struct CommittedStep<S>(S);
impl<S: crate::JournalStep> crate::JournalStep for CommittedStep<S> {
    type Output = Result<S::Output, String>;
    const SURFACE: lash_core::store::SurfaceFormat = S::SURFACE;
    const KIND: &'static str = S::KIND;
    fn instance(&self) -> String {
        self.0.instance()
    }
}
