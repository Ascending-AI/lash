//! What a node reports on stdout, one JSON object per line, and what it
//! reads on stdin.
//!
//! The reports are diagnostics and timing: the laws read the witness ledger
//! and the store for what happened, and these lines for who attempted which
//! durable write and what the store answered. A line is written after the
//! store answered, so its order across nodes is the order the test read it.

use serde::{Deserialize, Serialize};

/// One line a node writes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// The node's boot registered.
    Registered {
        /// The boot id the store minted.
        boot: String,
    },
    /// A heartbeat answered.
    Heartbeat {
        /// `renewed`, `reaped`, or the error.
        outcome: String,
    },
    /// The node stopped serving.
    Stopped {
        /// `requested`, `lease_lost`, `unrenewed`, or the error.
        why: String,
    },
    /// A claim or an adoption took an actor.
    Claimed {
        /// The actor.
        actor: String,
        /// The epoch the node owns it under.
        epoch: i64,
    },
    /// A reap, by lease or by liveness lock, released an actor of another
    /// boot.
    Reaped {
        /// The actor.
        actor: String,
        /// The dead owner's node.
        from: String,
        /// The actor's new epoch.
        epoch: i64,
        /// `lease` or `lock`.
        by: String,
    },
    /// The liveness locks this node's probe saw changed.
    Liveness {
        /// The other boots whose lock was held, by node.
        held: Vec<String>,
        /// The other boots whose lock was free, by node.
        free: Vec<String>,
    },
    /// A reap by liveness lock was attempted.
    ReapAttempt {
        /// The boot's node.
        of: String,
        /// How many actors it released, or the error.
        outcome: String,
    },
    /// The node released its own actors as it stopped.
    Released {
        /// The actors.
        actors: Vec<String>,
    },
    /// An owner transaction's fenced open was refused: the actor's epoch
    /// moved, or the store failed. A granted open is not reported; its
    /// commit is.
    BeginRefused {
        /// The actor.
        actor: String,
        /// The epoch the owner held.
        epoch: i64,
        /// `ownership_lost` or `failed`.
        outcome: String,
        /// The refusal.
        error: String,
    },
    /// An owner commit answered.
    Commit {
        /// The actor.
        actor: String,
        /// The epoch it was fenced under.
        epoch: i64,
        /// Its label.
        label: String,
        /// `committed`, `ownership_lost`, or `failed`.
        outcome: String,
        /// The error, when it failed.
        error: Option<String>,
    },
    /// A model attempt started.
    ModelAttempt {
        /// Which call of the turn: 1 before the cell's result is in the
        /// transcript, 2 after.
        call: u32,
        /// Its attempt number.
        attempt: u32,
    },
    /// A body was entered, or returned.
    Body {
        /// Its call id.
        call: String,
        /// Its tool or step.
        tool: String,
        /// `entered` or `returned`.
        phase: String,
    },
    /// The test blocked or unblocked this node's heartbeat.
    HeartbeatBlocked {
        /// Whether it is blocked now.
        blocked: bool,
    },
}

/// A line a node reads on stdin. End of input stops the node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Stop serving cleanly: release every actor and exit.
    Stop,
    /// Hold every heartbeat from now on until [`Command::UnblockHeartbeat`]:
    /// the partition. Bodies and owner commits keep running.
    BlockHeartbeat,
    /// Let heartbeats through again.
    UnblockHeartbeat,
}

impl Command {
    /// The line that carries the command.
    #[must_use]
    pub fn line(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::BlockHeartbeat => "block-heartbeat",
            Self::UnblockHeartbeat => "unblock-heartbeat",
        }
    }

    /// The command a line carries.
    #[must_use]
    pub fn parse(line: &str) -> Option<Self> {
        match line.trim() {
            "stop" => Some(Self::Stop),
            "block-heartbeat" => Some(Self::BlockHeartbeat),
            "unblock-heartbeat" => Some(Self::UnblockHeartbeat),
            _ => None,
        }
    }
}

/// One report line, as a node prints it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// The reporting node.
    pub node: String,
    /// What happened.
    #[serde(flatten)]
    pub event: Event,
}

/// Print `event` as `node`'s report line.
pub fn report(node: &str, event: Event) {
    let line = Report {
        node: node.to_owned(),
        event,
    };
    match serde_json::to_string(&line) {
        Ok(line) => println!("{line}"),
        Err(error) => eprintln!("{node}: a report does not encode: {error}"),
    }
}
