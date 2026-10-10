//! How a run ended, as one whole binary structure (FIG-5887 §4): the body
//! of node kind 27, TerminalWhole.
//!
//! An end keeps every detail the machine reported and no parked scheduler.
//! A finished run's carried session bindings and the not-carried bindings
//! that reach a closure are two distinct sets of roots over **one** object
//! table, by run-local identity: an object both reach is written once. The
//! run's result, a failure's reason and an uncaught error are a frozen
//! datum, a FrozenValue reference: a tree with no identity, distinct from
//! every mutable binding, whose large text and bytes share leaves.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::graph_tokens::{ChildRef, TokenCodec, TokenError, TokenReader, TokenWriter};
use lash_kernel_doc::{FunctionId, FunctionName, Name, Object, ObjectId, TaskIdentity, Value};

use crate::whole::variant;

/// Kind 27's body.
#[derive(Clone, Debug, PartialEq)]
pub struct TerminalWhole {
    pub end: TerminalEnd,
    /// The FrozenValue of a finished run's result, a failure's reason or an
    /// uncaught error; `None` for every other end.
    pub result: Option<ChildRef>,
    /// Whether a `finish` ended a finished run.
    pub finish: bool,
    /// The session bindings carried to the next run.
    pub carried: BTreeMap<Name, Value>,
    /// The not-carried bindings that reach no task, closures among what
    /// they reach.
    pub closures: BTreeMap<Name, Value>,
    /// Every object either set of bindings reaches, once.
    pub objects: BTreeMap<ObjectId, Object>,
    /// The bindings not carried because they reach a closure or a task.
    pub not_carried: Vec<Name>,
}

/// Which end, with its detail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalEnd {
    Finished,
    Failed,
    Uncaught,
    TasksOutstanding {
        unfinished: Vec<TaskIdentity>,
        unobserved: Vec<TaskIdentity>,
    },
    Deadlock {
        waiting: Vec<TaskIdentity>,
    },
    Bound {
        bound: TerminalBound,
        limit: u64,
        function: Option<FunctionName>,
    },
}

/// Which bound an end passed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalBound {
    Charge,
    Memory,
    CallDepth,
    LiveTasks,
    RequestsPerPark,
    JoinMembers,
    Guard { function: FunctionId },
}

impl TerminalEnd {
    /// Whether the end carries a frozen datum.
    #[must_use]
    pub fn has_result(&self) -> bool {
        matches!(self, Self::Finished | Self::Failed | Self::Uncaught)
    }
}

impl TokenCodec for TerminalBound {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Charge => out.sum(0, 0),
            Self::Memory => out.sum(1, 0),
            Self::CallDepth => out.sum(2, 0),
            Self::LiveTasks => out.sum(3, 0),
            Self::RequestsPerPark => out.sum(4, 0),
            Self::JoinMembers => out.sum(5, 0),
            Self::Guard { function } => {
                out.sum(6, 1);
                function.encode(out);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(
            match variant(input, |variant| match variant {
                0..=5 => Some(0),
                6 => Some(1),
                _ => None,
            })? {
                0 => Self::Charge,
                1 => Self::Memory,
                2 => Self::CallDepth,
                3 => Self::LiveTasks,
                4 => Self::RequestsPerPark,
                5 => Self::JoinMembers,
                _ => Self::Guard {
                    function: FunctionId::decode(input)?,
                },
            },
        )
    }
}

impl TokenCodec for TerminalEnd {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Finished => out.sum(0, 0),
            Self::Failed => out.sum(1, 0),
            Self::Uncaught => out.sum(2, 0),
            Self::TasksOutstanding {
                unfinished,
                unobserved,
            } => {
                out.sum(3, 2);
                unfinished.encode(out);
                unobserved.encode(out);
            }
            Self::Deadlock { waiting } => {
                out.sum(4, 1);
                waiting.encode(out);
            }
            Self::Bound {
                bound,
                limit,
                function,
            } => {
                out.sum(5, 3);
                bound.encode(out);
                out.unsigned(*limit);
                function.encode(out);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(
            match variant(input, |variant| match variant {
                0..=2 => Some(0),
                3 => Some(2),
                4 => Some(1),
                5 => Some(3),
                _ => None,
            })? {
                0 => Self::Finished,
                1 => Self::Failed,
                2 => Self::Uncaught,
                3 => Self::TasksOutstanding {
                    unfinished: Vec::decode(input)?,
                    unobserved: Vec::decode(input)?,
                },
                4 => Self::Deadlock {
                    waiting: Vec::decode(input)?,
                },
                _ => Self::Bound {
                    bound: TerminalBound::decode(input)?,
                    limit: input.unsigned()?,
                    function: Option::decode(input)?,
                },
            },
        )
    }
}

impl TokenCodec for TerminalWhole {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(7);
        self.end.encode(out);
        self.result.encode(out);
        out.bool(self.finish);
        self.carried.encode(out);
        self.closures.encode(out);
        self.objects.encode(out);
        self.not_carried.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(7)?;
        let whole = Self {
            end: TerminalEnd::decode(input)?,
            result: Option::decode(input)?,
            finish: input.bool()?,
            carried: BTreeMap::decode(input)?,
            closures: BTreeMap::decode(input)?,
            objects: BTreeMap::decode(input)?,
            not_carried: Vec::decode(input)?,
        };
        let finished = whole.end == TerminalEnd::Finished;
        let not_carried: BTreeSet<&Name> = whole.not_carried.iter().collect();
        let consistent = whole.result.is_some() == whole.end.has_result()
            && (finished || !whole.finish)
            && (finished
                || (whole.carried.is_empty()
                    && whole.closures.is_empty()
                    && whole.objects.is_empty()
                    && whole.not_carried.is_empty()))
            && not_carried.len() == whole.not_carried.len()
            && whole.carried.keys().all(|name| !not_carried.contains(name))
            && whole.closures.keys().all(|name| not_carried.contains(name));
        if consistent {
            Ok(whole)
        } else {
            Err(TokenError::Invalid(
                "a terminal state's result, finish flag or bindings do not fit its end".to_owned(),
            ))
        }
    }
}
