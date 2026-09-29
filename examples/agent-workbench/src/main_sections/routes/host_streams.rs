//! Host-owned HTTP observation streams.

use super::*;
use axum::http::HeaderMap;
use lash_remote_protocol::{Negotiated, Negotiation, REMOTE_PROTOCOL, answer};

const REMOTE_HELLO_HEADER: &str = "x-lash-protocol-hello";

fn negotiate_remote(headers: &HeaderMap) -> Result<(Negotiated, String), AppError> {
    let hello = headers
        .get(REMOTE_HELLO_HEADER)
        .ok_or_else(|| AppError::bad_request("missing remote protocol Hello"))?
        .to_str()
        .map_err(|_| AppError::bad_request("invalid remote protocol Hello header"))?;
    let hello: Negotiation = serde_json::from_str(hello)
        .map_err(|_| AppError::bad_request("invalid remote protocol Hello"))?;
    let accept = answer(REMOTE_PROTOCOL, &hello);
    let negotiated = Negotiated::from_accept(REMOTE_PROTOCOL, &accept)
        .map_err(|error| AppError::bad_request(error.to_string()))?;
    let accept_json = serde_json::to_string(&accept)
        .map_err(|error| AppError::internal(format!("encode protocol Accept: {error}")))?;
    Ok((negotiated, accept_json))
}

#[cfg(test)]
fn test_remote_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        REMOTE_HELLO_HEADER,
        serde_json::to_string(&Negotiation::Hello {
            supported: REMOTE_PROTOCOL,
        })
        .expect("encode test Hello")
        .parse()
        .expect("valid test Hello header"),
    );
    headers
}

#[cfg(test)]
pub(crate) async fn session_events(
    State(state): State<AppState>,
    Query(query): Query<ProductEventsQuery>,
) -> Result<Response, AppError> {
    session_events_with_shutdown(State(state), Query(query), None).await
}

pub(crate) async fn session_events_with_shutdown(
    State(state): State<AppState>,
    Query(query): Query<ProductEventsQuery>,
    mut shutdown: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<Response, AppError> {
    let session_id = state
        .admit_session(
            &SessionQuery {
                session_id: query.session_id.clone(),
            },
            "api.events",
        )
        .await?;
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::Observe {
            session_id: session_id.clone(),
        })?;
    let (replay, mut product_events) = state
        .event_tx
        .subscribe_after(&session_id, query.cursor.unwrap_or(0));
    let event_registry = state.event_tx.clone();
    let (tx, rx) = mpsc::channel::<ProductStreamItem>(64);
    tokio::spawn(async move {
        for event in replay {
            if !send_until_shutdown(&tx, ProductStreamItem::Event { event }, &mut shutdown).await {
                return;
            }
        }
        loop {
            let next = match shutdown.as_mut() {
                Some(shutdown) => tokio::select! {
                    biased;
                    _ = host_shutdown_requested(shutdown) => return,
                    event = product_events.recv() => event,
                },
                None => product_events.recv().await,
            };
            match next {
                Ok(event) => {
                    if !send_until_shutdown(&tx, ProductStreamItem::Event { event }, &mut shutdown)
                        .await
                    {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_count)) => {
                    if !send_until_shutdown(
                        &tx,
                        ProductStreamItem::Resync {
                            snapshot: event_registry.snapshot(&session_id),
                        },
                        &mut shutdown,
                    )
                    .await
                    {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    Ok(ndjson_response(ReceiverStream::new(rx)))
}

#[cfg(test)]
pub(crate) async fn session_observations(
    State(state): State<AppState>,
    Query(query): Query<EventsQuery>,
) -> Result<Response, AppError> {
    session_observations_with_shutdown(State(state), Query(query), test_remote_headers(), None)
        .await
}

pub(crate) async fn session_observations_with_shutdown(
    State(state): State<AppState>,
    Query(query): Query<EventsQuery>,
    headers: HeaderMap,
    shutdown: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<Response, AppError> {
    let (negotiated, accept_json) = negotiate_remote(&headers)?;
    let session_id = state
        .admit_session(
            &SessionQuery {
                session_id: query.session_id.clone(),
            },
            "api.observations",
        )
        .await?;
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::Observe {
            session_id: session_id.clone(),
        })?;
    let session = Box::pin(state.open_session_for_observation(&session_id))
        .await
        .map_err(|error| state.session_admission_error(&session_id, "api.observations", error))?;
    let cursor = match query
        .cursor
        .as_deref()
        .filter(|cursor| !cursor.trim().is_empty())
    {
        Some(cursor) => serde_json::from_value::<SessionCursor>(json!(cursor))
            .map_err(|err| AppError::bad_request(format!("invalid session cursor: {err}")))?,
        None => session.observe().recoverable_chat_snapshot().cursor,
    };
    let (tx, rx) = mpsc::channel::<ObservationStreamItem>(64);
    tokio::spawn(async move {
        forward_session_observations_until_shutdown(session, cursor, tx, shutdown, negotiated)
            .await;
    });
    let mut response = ndjson_response(ReceiverStream::new(rx));
    response.headers_mut().insert(
        "x-lash-protocol-accept",
        accept_json
            .parse()
            .map_err(|error| AppError::internal(format!("protocol Accept header: {error}")))?,
    );
    Ok(response)
}

#[cfg(test)]
pub(crate) async fn forward_session_observations(
    session: lash::LashSession,
    cursor: SessionCursor,
    tx: mpsc::Sender<ObservationStreamItem>,
) {
    let negotiated = negotiate_remote(&test_remote_headers()).unwrap().0;
    forward_session_observations_until_shutdown(session, cursor, tx, None, negotiated).await;
}

async fn host_shutdown_requested(shutdown: &mut tokio::sync::watch::Receiver<bool>) {
    while !*shutdown.borrow_and_update() {
        if shutdown.changed().await.is_err() {
            break;
        }
    }
}

async fn send_until_shutdown<T>(
    tx: &mpsc::Sender<T>,
    item: T,
    shutdown: &mut Option<tokio::sync::watch::Receiver<bool>>,
) -> bool {
    match shutdown.as_mut() {
        Some(shutdown) => tokio::select! {
            biased;
            _ = host_shutdown_requested(shutdown) => false,
            result = tx.send(item) => result.is_ok(),
        },
        None => tx.send(item).await.is_ok(),
    }
}

async fn forward_session_observations_until_shutdown(
    session: lash::LashSession,
    cursor: SessionCursor,
    tx: mpsc::Sender<ObservationStreamItem>,
    mut shutdown: Option<tokio::sync::watch::Receiver<bool>>,
    negotiated: Negotiated,
) {
    use lash::recoverable_chat::RecoverableChatUpdate;

    if !send_until_shutdown(
        &tx,
        ObservationStreamItem::Cursor {
            cursor: cursor.to_string(),
        },
        &mut shutdown,
    )
    .await
    {
        return;
    }
    let mut stream = session.observe().subscribe_recoverable_chat(cursor);
    let mut sequence = 0;
    loop {
        let item = match shutdown.as_mut() {
            Some(shutdown) => tokio::select! {
                biased;
                _ = host_shutdown_requested(shutdown) => break,
                item = stream.next() => item,
            },
            None => stream.next().await,
        };
        let Some(item) = item else {
            break;
        };
        match item {
            Ok(RecoverableChatUpdate::Event { event, .. }) => {
                let event = match RemoteSessionObservationEvent::from_core(sequence, event) {
                    Ok(event) => event,
                    Err(err) => {
                        eprintln!("warning: workbench Lash observation stream stopped: {err}");
                        break;
                    }
                };
                sequence = sequence.saturating_add(1);
                if !send_until_shutdown(
                    &tx,
                    ObservationStreamItem::Observation {
                        event: Box::new(Envelope::at(&negotiated, event)),
                    },
                    &mut shutdown,
                )
                .await
                {
                    break;
                }
            }
            Ok(RecoverableChatUpdate::TerminalReplacement {
                event, snapshot, ..
            }) => {
                let event = match RemoteSessionObservationEvent::from_core(sequence, event) {
                    Ok(event) => event,
                    Err(err) => {
                        eprintln!("warning: workbench Lash observation stream stopped: {err}");
                        break;
                    }
                };
                sequence = sequence.saturating_add(1);
                if !send_until_shutdown(
                    &tx,
                    ObservationStreamItem::TerminalReplacement {
                        cursor: snapshot.cursor.to_string(),
                        event: Box::new(Envelope::at(&negotiated, event)),
                    },
                    &mut shutdown,
                )
                .await
                {
                    break;
                }
            }
            Ok(RecoverableChatUpdate::ResidentReplacement {
                event, snapshot, ..
            }) => {
                let event = match RemoteSessionObservationEvent::from_core(sequence, event) {
                    Ok(event) => event,
                    Err(err) => {
                        eprintln!("warning: workbench Lash observation stream stopped: {err}");
                        break;
                    }
                };
                sequence = sequence.saturating_add(1);
                if !send_until_shutdown(
                    &tx,
                    ObservationStreamItem::ResidentReplacement {
                        cursor: snapshot.cursor.to_string(),
                        event: Box::new(Envelope::at(&negotiated, event)),
                    },
                    &mut shutdown,
                )
                .await
                {
                    break;
                }
            }
            Ok(RecoverableChatUpdate::ReplayGap { snapshot, gap }) => {
                let observation =
                    RemoteSessionObservation::from_core(lash::observe::SessionObservation {
                        read_view: snapshot.read_view,
                        cursor: snapshot.cursor,
                    });
                let gap = RemoteLiveReplayGap::from(gap);
                if !send_until_shutdown(
                    &tx,
                    ObservationStreamItem::ReplayGap {
                        observation: Box::new(Envelope::at(&negotiated, observation)),
                        gap: Box::new(Envelope::at(&negotiated, gap)),
                    },
                    &mut shutdown,
                )
                .await
                {
                    break;
                }
            }
            Err(err) => {
                eprintln!("warning: workbench Lash observation stream stopped: {err}");
                break;
            }
        }
    }
}
