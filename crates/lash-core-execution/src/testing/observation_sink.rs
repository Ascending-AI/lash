//! Observation sinks a fixture installs to capture a drive's output.

use std::sync::Arc;

/// An [`ObservationSink`](crate::engine::ObservationSink) that delivers
/// observations onto unbounded channels, for hosts and fixtures that consume
/// [`SessionStreamEvent`](crate::SessionStreamEvent)s and
/// [`TurnActivity`](crate::TurnActivity)s on `mpsc` receivers. The channels
/// are unbounded on purpose: observation never waits on a receiver. An
/// activity's id derives from `(key, ordinal)`, the same derivation the turn
/// observer applies.
pub struct ChannelObservationSink {
    session_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::SessionStreamEvent>>,
    activity_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::TurnActivity>>,
}

impl ChannelObservationSink {
    pub fn new(
        session_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::SessionStreamEvent>>,
        activity_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::TurnActivity>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            session_tx,
            activity_tx,
        })
    }
}

impl crate::engine::ObservationSink for ChannelObservationSink {
    fn observe(&self, observation: crate::engine::DriveObservation) {
        let crate::engine::DriveObservation {
            key,
            ordinal,
            event,
        } = observation;
        match event {
            crate::engine::ObservedEvent::Session(event) => {
                if let Some(tx) = &self.session_tx {
                    let _ = tx.send(event);
                }
            }
            crate::engine::ObservedEvent::Activity {
                correlation_id,
                event,
            } => {
                if let Some(tx) = &self.activity_tx {
                    let id = crate::TurnActivityId::new(format!("{key}#{ordinal}"));
                    let _ = tx.send(crate::TurnActivity {
                        correlation_id: correlation_id.unwrap_or_else(|| id.clone()),
                        id,
                        event,
                    });
                }
            }
            crate::engine::ObservedEvent::RecordedSession(event) => {
                if let Some(tx) = &self.session_tx {
                    let _ = tx.send(event);
                }
            }
            crate::engine::ObservedEvent::RecordedActivity(activity) => {
                if let Some(tx) = &self.activity_tx {
                    let _ = tx.send(activity);
                }
            }
        }
    }
}
