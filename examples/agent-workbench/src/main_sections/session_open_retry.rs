const SESSION_OPEN_MAX_ATTEMPTS: usize = 6;
const SESSION_OPEN_RETRY_BUDGET: Duration = Duration::from_millis(75);

async fn open_session_with_bounded_retry(
    state: &AppState,
    session_id: &str,
) -> Result<lash::LashSession, lash::EmbedError> {
    retry_session_open(
        || state.session_builder(session_id.to_string()).open(),
        |event, payload| state.trace_for_session(session_id, event, payload),
    )
    .await
}

async fn retry_session_open<T, Open, OpenFuture, Trace>(
    mut open: Open,
    mut trace: Trace,
) -> Result<T, lash::EmbedError>
where
    Open: FnMut() -> OpenFuture,
    OpenFuture: std::future::Future<Output = Result<T, lash::EmbedError>>,
    Trace: FnMut(&str, Value),
{
    static RETRY_SEQUENCE: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);

    let started = tokio::time::Instant::now();
    let mut last_contended = None;
    for attempt in 1..=SESSION_OPEN_MAX_ATTEMPTS {
        if attempt > 1 && started.elapsed() >= SESSION_OPEN_RETRY_BUDGET {
            break;
        }
        match open().await {
            Ok(session) => {
                if attempt > 1 {
                    trace(
                        "session.open.retried",
                        json!({
                            "attempt": attempt,
                            "elapsed_ms": started.elapsed().as_millis(),
                            "outcome": "opened",
                        }),
                    );
                }
                return Ok(session);
            }
            Err(error) if session_open_is_contended(&error) => {
                trace(
                    "session.open.contended",
                    json!({
                        "attempt": attempt,
                        "attempt_cap": SESSION_OPEN_MAX_ATTEMPTS,
                        "elapsed_ms": started.elapsed().as_millis(),
                        "latency_budget_ms": SESSION_OPEN_RETRY_BUDGET.as_millis(),
                        "outcome": "retrying",
                    }),
                );
                last_contended = Some(error);
                if attempt == SESSION_OPEN_MAX_ATTEMPTS {
                    break;
                }
                let base_ms = 1_u64 << (attempt - 1).min(4);
                let sequence =
                    RETRY_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let delay = Duration::from_millis(base_ms + sequence % (base_ms + 1));
                let remaining = SESSION_OPEN_RETRY_BUDGET.saturating_sub(started.elapsed());
                if delay > remaining {
                    break;
                }
                tokio::time::sleep(delay).await;
            }
            Err(error) => return Err(error),
        }
    }
    trace(
        "session.open.retry_exhausted",
        json!({
            "attempt_cap": SESSION_OPEN_MAX_ATTEMPTS,
            "elapsed_ms": started.elapsed().as_millis(),
            "latency_budget_ms": SESSION_OPEN_RETRY_BUDGET.as_millis(),
            "outcome": "temporarily_unavailable",
        }),
    );
    Err(last_contended.expect("a retry budget exhausts only after typed contention"))
}

fn session_open_is_contended(error: &lash::EmbedError) -> bool {
    matches!(
        error,
        lash::EmbedError::Store(lash::persistence::StoreError::Contended)
            | lash::EmbedError::Session(lash::SessionError::Store {
                source: lash::persistence::StoreError::Contended,
                ..
            })
    )
}

fn temporarily_unavailable_session_open() -> AppError {
    AppError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        message: "session is temporarily busy; retry the request".to_string(),
        verdict: AppErrorVerdict::Retryable,
    }
}
