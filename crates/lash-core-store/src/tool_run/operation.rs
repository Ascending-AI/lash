//! K8: a tool-bearing host operation is a logical Run (binding Q2; FIG-4888
//! implements it).
//!
//! The operation uses the Run follow/cancel/result vocabulary with its own
//! input kind, driven by the session's existing keyed turn service: no new
//! Restate service. The host command returns once the operation Run is
//! admitted and its drive obligation is durable. The Run owns every call
//! and handle until explicit completion and Closing, and cannot settle while
//! owned work is live. Tool-free administration stays an administrative
//! scope outside any Run.
//!
//! The operation's opener is the existing session-operation opener, so its
//! call ids, start keys and stored scope ids keep their identity bytes.

use serde::{Deserialize, Serialize};

use crate::SessionId;
use crate::effect_opener::EffectOpener;

/// What a logical Run executes.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "input", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunInputKind {
    /// A turn of the session.
    Turn,
    /// A tool-bearing host operation.
    Operation { operation_id: String },
}

/// A tool-bearing host operation's Run.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationRun {
    pub session_id: SessionId,
    pub operation_id: String,
}

impl OperationRun {
    /// The opener that owns the operation's work: the session-operation
    /// opener, whose canonical encoding roots the operation's call ids.
    #[must_use]
    pub fn opener(&self) -> EffectOpener {
        EffectOpener::session_operation(self.session_id.clone(), self.operation_id.clone())
    }

    /// The input kind the session's turn service drives.
    #[must_use]
    pub fn input(&self) -> RunInputKind {
        RunInputKind::Operation {
            operation_id: self.operation_id.clone(),
        }
    }
}
