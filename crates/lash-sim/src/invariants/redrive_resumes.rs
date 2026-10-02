//! A redrive is asked of a park that names its root, and a redriven root
//! resumes.
//!
//! The turn park feed is the store's own record of every park transition;
//! the acknowledgements and resumes are what the deployment's store and
//! engine answered at the trait seams the soak decorates. The checker folds
//! the two against each other:
//!
//! - an acknowledged redrive (the store's [`Fact::IntentAck`], or the host's
//!   own `Known` [`HostOp::Redrive`]) has a `redrive_requested` feed event
//!   for its root, on a park a `parked` event opened for that root before it;
//! - a root the engine was asked to resume ([`Fact::Resume`]) has a
//!   `redrive_requested` event: nothing resumes a root nobody redrove;
//! - once the final recovery pass ran, every `redrive_requested` event is
//!   followed by the park's end, and, unless the park ended cancelled, by
//!   the intent's acknowledgement and the root's resume.

use std::num::NonZeroUsize;

use serde::Serialize;

use super::{Fact, History, HistoryChecker, HostOp, HostOutcome, Violation};

pub(super) struct RedriveResumes;
const INVARIANT: &str = "redrive-resumes";

/// One turn park feed event, as the store committed it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ParkEventRow {
    /// The feed's own sequence: events are in commit order by it.
    pub seq: u64,
    pub at_ms: u64,
    pub session: String,
    pub root: String,
    /// The park the transition applies to.
    pub park: u64,
    /// `parked`, `unparked`, `cancelled` or `redrive_requested`.
    pub kind: String,
    /// The redrive's intent, on a `redrive_requested` event.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
    /// What ended the park, on a closing event.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
}

impl ParkEventRow {
    fn names(&self, session: &str, root: &str) -> bool {
        self.session == session && self.root == root
    }

    fn requested(&self) -> bool {
        self.kind == "redrive_requested"
    }

    fn ends(&self) -> bool {
        matches!(self.kind.as_str(), "unparked" | "cancelled")
    }

    fn render(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| format!("{self:?}"))
    }
}

/// The feed page a read takes.
const PAGE: NonZeroUsize = NonZeroUsize::MIN.saturating_add(255);

/// `world`'s whole turn park feed, in commit order.
pub(super) async fn read_park_feed(
    world: &crate::crash_matrix::world::CrashWorld,
) -> Result<Vec<ParkEventRow>, String> {
    let store = world.backend().session_store_factory();
    let mut rows = Vec::new();
    let mut cursor = lash_core::store::ParkFeedCursor::initial();
    loop {
        let page = store
            .turn_park_feed(cursor, PAGE)
            .await
            .map_err(|error| format!("read the turn park feed: {error}"))?;
        let read = page.events.len();
        rows.extend(page.events.into_iter().map(|event| {
            let (intent, cause) = match &event.kind {
                lash_core::store::ParkEventKind::RedriveRequested { intent } => {
                    (Some(intent.to_string()), None)
                }
                lash_core::store::ParkEventKind::Unparked { cause } => (None, Some(cause.encode())),
                lash_core::store::ParkEventKind::Cancelled { cause } => {
                    (None, Some(cause.encode()))
                }
                _ => (None, None),
            };
            ParkEventRow {
                seq: event.seq,
                at_ms: event.at_ms,
                session: event.target.session_id.to_string(),
                root: event.target.turn_id.to_string(),
                park: event.park_id.feed_sequence(),
                kind: event.kind.kind_code().to_owned(),
                intent,
                cause,
            }
        }));
        if read < PAGE.get() {
            return Ok(rows);
        }
        cursor = page.next;
    }
}

/// The `redrive_requested` events of `session`'s `root` on a park a `parked`
/// event opened for that root before them.
fn requests<'a>(
    history: &'a History,
    session: &'a str,
    root: &'a str,
) -> impl Iterator<Item = &'a ParkEventRow> {
    history.park_events.iter().filter(move |event| {
        event.requested()
            && event.names(session, root)
            && history.park_events.iter().any(|opened| {
                opened.kind == "parked"
                    && opened.park == event.park
                    && opened.names(session, root)
                    && opened.seq < event.seq
            })
    })
}

/// The feed events of `session`, rendered, for a violation's rows.
fn timeline(history: &History, session: &str) -> Vec<String> {
    history
        .park_events
        .iter()
        .filter(|event| event.session == session)
        .map(ParkEventRow::render)
        .collect()
}

fn violation(history: &History, session: &str, detail: String) -> Violation {
    let mut violation = Violation::new(INVARIANT, detail).session(session);
    violation.rows = timeline(history, session);
    violation
}

impl HistoryChecker for RedriveResumes {
    fn invariant(&self) -> &'static str {
        INVARIANT
    }

    /// The redrives the store recorded: each is judged against the facts.
    fn observed(&self, history: &History) -> usize {
        history
            .park_events
            .iter()
            .filter(|event| event.requested())
            .count()
    }

    fn check(&self, history: &History) -> Vec<Violation> {
        let mut violations = Vec::new();
        for record in &history.records {
            match &record.fact {
                Fact::IntentAck {
                    intent,
                    session,
                    verb,
                    root: Some(root),
                    park: Some(park),
                    ..
                } if verb == "redrive" => {
                    if !requests(history, session, root).any(|event| {
                        event.park == *park && event.intent.as_deref() == Some(intent.as_str())
                    }) {
                        violations.push(
                            violation(
                                history,
                                session,
                                format!(
                                    "redrive intent {intent} of `{root}` was acknowledged, but the feed holds no redrive request of it on park {park} opened for that root"
                                ),
                            )
                            .records([record.at]),
                        );
                    }
                }
                Fact::HostOp {
                    op: HostOp::Redrive,
                    session,
                    roots,
                    outcome: HostOutcome::Known,
                } => {
                    for root in roots {
                        if requests(history, session, root).next().is_none() {
                            violations.push(
                                violation(
                                    history,
                                    session,
                                    format!(
                                        "the host heard its redrive of `{root}` accepted, but the feed holds no redrive request on a park opened for that root"
                                    ),
                                )
                                .records([record.at]),
                            );
                        }
                    }
                }
                Fact::Resume { session, root, .. }
                    if requests(history, session, root).next().is_none() =>
                {
                    violations.push(
                        violation(
                            history,
                            session,
                            format!(
                                "the engine was asked to resume `{root}`, which no redrive request on a park opened for it names"
                            ),
                        )
                        .records([record.at]),
                    );
                }
                _ => {}
            }
        }
        for event in history.park_events.iter().filter(|event| event.requested()) {
            let (session, root) = (event.session.as_str(), event.root.as_str());
            if requests(history, session, root).all(|named| named.seq != event.seq) {
                violations.push(violation(
                    history,
                    session,
                    format!(
                        "feed event {} requests a redrive of `{root}` on park {}, which no earlier `parked` event opened for that root",
                        event.seq, event.park
                    ),
                ));
            }
            if !history.relay_ran {
                // The redrive may still be owed to the recovery pass.
                continue;
            }
            let end = history
                .park_events
                .iter()
                .find(|end| end.ends() && end.park == event.park && end.seq > event.seq);
            let Some(end) = end else {
                violations.push(violation(
                    history,
                    session,
                    format!(
                        "park {} of `{root}` never ended after feed event {} requested its redrive",
                        event.park, event.seq
                    ),
                ));
                continue;
            };
            if end.kind == "cancelled" {
                // A cancel, fork or deletion took the root: nothing resumes.
                continue;
            }
            let acknowledged: Vec<usize> = history
                .records
                .iter()
                .filter(|record| {
                    matches!(&record.fact, Fact::IntentAck { intent, verb, .. }
                        if verb == "redrive" && Some(intent.as_str()) == event.intent.as_deref())
                })
                .map(|record| record.at)
                .collect();
            let resumed: Vec<usize> = history
                .records
                .iter()
                .filter(|record| {
                    matches!(&record.fact, Fact::Resume { session: resumed, root: of, .. }
                        if resumed == session && of == root)
                })
                .map(|record| record.at)
                .collect();
            if acknowledged.is_empty() {
                violations.push(
                    violation(
                        history,
                        session,
                        format!(
                            "the redrive of `{root}` feed event {} requested was never acknowledged, though park {} ended `{}`",
                            event.seq,
                            event.park,
                            end.cause.as_deref().unwrap_or(&end.kind)
                        ),
                    )
                    .records(resumed.iter().copied()),
                );
            }
            if resumed.is_empty() {
                violations.push(
                    violation(
                        history,
                        session,
                        format!(
                            "`{root}` was never resumed after feed event {} requested its redrive, though park {} ended `{}`",
                            event.seq,
                            event.park,
                            end.cause.as_deref().unwrap_or(&end.kind)
                        ),
                    )
                    .records(acknowledged.iter().copied()),
                );
            }
        }
        violations
    }
}
