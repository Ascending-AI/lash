/// One journaled process transition projected onto the live session stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionProcessEventKind {
    Started { sequence: u64 },
    Waiting { sequence: u64 },
    Resumed { sequence: u64 },
    CancelRequested { sequence: u64 },
    AbandonRequested { sequence: u64 },
    CallerDeparted { sequence: u64 },
    Completed { sequence: u64 },
    Failed { sequence: u64 },
    Cancelled { sequence: u64 },
    Abandoned { sequence: u64 },
}

impl SessionProcessEventKind {
    /// Project only journaled process lifecycle facts onto the session stream.
    pub fn from_durable_event(event_type: &str, sequence: u64) -> Option<Self> {
        Some(match event_type {
            "process.first_started" => Self::Started { sequence },
            "process.waiting" => Self::Waiting { sequence },
            "process.resumed" => Self::Resumed { sequence },
            "process.cancel_requested" => Self::CancelRequested { sequence },
            "process.abandon_requested" => Self::AbandonRequested { sequence },
            "process.caller_departed" => Self::CallerDeparted { sequence },
            "process.completed" => Self::Completed { sequence },
            "process.failed" => Self::Failed { sequence },
            "process.cancelled" => Self::Cancelled { sequence },
            "process.abandoned" => Self::Abandoned { sequence },
            _ => return None,
        })
    }
}
