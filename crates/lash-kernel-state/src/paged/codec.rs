//! Canonical semantic record bodies. Every field is present, including
//! empty roots; no runtime coordinate or transport grant is serialized.
use super::*;
use lash_kernel_doc::graph_tokens::{TokenCodec, TokenError, TokenReader, TokenWriter};
use lash_kernel_doc::heap::*;
use lash_kernel_doc::{DocumentId, JoinMode, TaskId};

macro_rules! record {
    ($name:ident { $($field:ident : $ty:ty),+ $(,)? }) => {
        impl TokenCodec for $name {
            fn encode(&self, out: &mut TokenWriter) {
                out.seq([$(stringify!($field)),+].len() as u64);
                $(self.$field.encode(out);)+
            }
            fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
                input.expect_struct([$(stringify!($field)),+].len() as u64)?;
                Ok(Self { $($field: <$ty>::decode(input)?),+ })
            }
        }
    };
}
macro_rules! units {
    ($name:ident { $($variant:ident = $tag:literal),+ $(,)? }) => {
        impl TokenCodec for $name {
            fn encode(&self, out: &mut TokenWriter) { out.sum(match self { $(Self::$variant => $tag),+ }, 0); }
            fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
                let (tag, fields) = input.sum()?; TokenReader::expect_fields(fields, 0)?;
                Ok(match tag { $($tag => Self::$variant),+, _ => return Err(TokenError::UnsupportedVariant { variant: tag }) })
            }
        }
    };
}
/// An empty tree root is tag 0 with no fields; a nonempty one tag 1 with
/// its page, at least one row and its cost. Nothing else decodes
/// (FIG-6197 F3), as a nested tree's root does not.
macro_rules! tree_root {
    ($name:ident) => {
        impl TokenCodec for $name {
            fn encode(&self, out: &mut TokenWriter) {
                match self {
                    Self::Empty => out.sum(0, 0),
                    Self::NonEmpty { page, len, cost } => {
                        out.sum(1, 3);
                        page.encode(out);
                        len.encode(out);
                        cost.encode(out);
                    }
                }
            }
            fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
                let (tag, fields) = input.sum()?;
                match tag {
                    0 => {
                        TokenReader::expect_fields(fields, 0)?;
                        Ok(Self::Empty)
                    }
                    1 => {
                        TokenReader::expect_fields(fields, 3)?;
                        let page = PageRef::decode(input)?;
                        let len = u64::decode(input)?;
                        let cost = LogicalBytes::decode(input)?;
                        if len == 0 {
                            return Err(TokenError::Invalid(
                                "a nonempty tree root holds no rows".into(),
                            ));
                        }
                        Ok(Self::NonEmpty { page, len, cost })
                    }
                    _ => Err(TokenError::UnsupportedVariant { variant: tag }),
                }
            }
        }
    };
}
tree_root!(KeyedRoot);
tree_root!(SequenceRoot);
record!(AccountingCertificate {
    metric: AccountingMetric,
    capacity: u64,
    entities: u64,
    refinements: u64
});
units!(AccountingMetric { Capacity = 0, Unmetered = 1 });
record!(HeapRoot {
    directory: KeyedRoot,
    families: KeyedRoot,
    next_ordinal: u64,
    next_family: u64,
    accounting: AccountingCertificate,
    population: DataOnlyCertificate,
    collection_entities: Option<u64>
});
record!(FrozenSummary {
    entities: u64,
    cost: LogicalBytes,
    data_only: bool
});
record!(FrozenRef {
    root: PageRef,
    summary: FrozenSummary
});
record!(MembershipRun {
    start: u64,
    len: u64
});
record!(FamilyRecord {
    family: FamilyId,
    base: FrozenRef,
    extent: u64,
    membership: KeyedRoot
});
record!(DataOnlyCertificate {
    closure_entities: u64,
    task_edges: u64
});
record!(SessionRoot {
    heap: PageRef,
    bindings: KeyedRoot,
    functions: KeyedRoot,
    certificate: DataOnlyCertificate
});
// The numeric token denotes Exact; none denotes Off. The token type is
// already a discriminant, retaining the bounded root's fixed footprint.
impl TokenCodec for SavedMeters {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(4);
        match self.charged {
            Some(charged) => charged.encode(out),
            None => out.none(),
        }
        self.waits_issued.encode(out);
        self.next_join.encode(out);
        self.live_tasks.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(4)?;
        let charged = if input.peek_tag()? == lash_kernel_doc::graph_tokens::Tag::None {
            input.option()?;
            None
        } else {
            Some(u64::decode(input)?)
        };
        Ok(Self {
            charged,
            waits_issued: u64::decode(input)?,
            next_join: u64::decode(input)?,
            live_tasks: u32::decode(input)?,
        })
    }
}
// None is an unmetered memory bound, as in [`SavedMeters`].
impl TokenCodec for SavedLimits {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        match self.memory {
            Some(memory) => memory.encode(out),
            None => out.none(),
        }
        self.charge.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        let memory = if input.peek_tag()? == lash_kernel_doc::graph_tokens::Tag::None {
            input.option()?;
            None
        } else {
            Some(u64::decode(input)?)
        };
        Ok(Self {
            memory,
            charge: u64::decode(input)?,
        })
    }
}
record!(CheckpointRoot {
    limits: SavedLimits,
    kernel: u32,
    document: DocumentId,
    closure: PageId,
    owner: PageId,
    target: CellTargetKind,
    functions: KeyedRoot,
    status: RootStatus,
    heap: PageRef,
    tasks: KeyedRoot,
    ready: SequenceRoot,
    waits: KeyedRoot,
    joins: KeyedRoot,
    withdrawn: KeyedRoot,
    unreported: SequenceRoot,
    reads: KeyedRoot,
    session: KeyedRoot,
    host: Vec<lash_kernel_doc::Name>,
    meters: SavedMeters
});
impl TokenCodec for RootStatus {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Parked => out.sum(0, 0),
            Self::Ended(value) => {
                out.sum(1, 1);
                value.encode(out);
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (tag, fields) = input.sum()?;
        Ok(match tag {
            0 => {
                TokenReader::expect_fields(fields, 0)?;
                Self::Parked
            }
            1 => {
                TokenReader::expect_fields(fields, 1)?;
                Self::Ended(<EndedRecord>::decode(input)?)
            }
            _ => return Err(TokenError::UnsupportedVariant { variant: tag }),
        })
    }
}
impl TokenCodec for EndedRecord {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Returned(value) => {
                out.sum(0, 1);
                value.encode(out);
            }
            Self::Finished(value) => {
                out.sum(1, 1);
                value.encode(out);
            }
            Self::Failed(value) => {
                out.sum(2, 1);
                value.encode(out);
            }
            Self::Stopped => out.sum(3, 0),
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (tag, fields) = input.sum()?;
        Ok(match tag {
            0 => {
                TokenReader::expect_fields(fields, 1)?;
                Self::Returned(<FrozenRef>::decode(input)?)
            }
            1 => {
                TokenReader::expect_fields(fields, 1)?;
                Self::Finished(<FrozenRef>::decode(input)?)
            }
            2 => {
                TokenReader::expect_fields(fields, 1)?;
                Self::Failed(<FrozenRef>::decode(input)?)
            }
            3 => {
                TokenReader::expect_fields(fields, 0)?;
                Self::Stopped
            }
            _ => return Err(TokenError::UnsupportedVariant { variant: tag }),
        })
    }
}

impl TokenCodec for JoinerRecord {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Single(value) => {
                out.sum(0, 1);
                value.encode(out);
            }
            Self::List(value) => {
                out.sum(1, 1);
                value.encode(out);
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (tag, fields) = input.sum()?;
        Ok(match tag {
            0 => {
                TokenReader::expect_fields(fields, 1)?;
                Self::Single(<TaskId>::decode(input)?)
            }
            1 => {
                TokenReader::expect_fields(fields, 1)?;
                Self::List(<u64>::decode(input)?)
            }
            _ => return Err(TokenError::UnsupportedVariant { variant: tag }),
        })
    }
}
impl TokenCodec for TaskStateRecord {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Ready => out.sum(0, 0),
            // A delivered task takes a tag of its own, so a ready one keeps
            // its bytes.
            Self::Resuming { incoming, requeue } => {
                out.sum(if *requeue { 6 } else { 1 }, 1);
                incoming.encode(out);
            }
            Self::Waiting(value) => {
                out.sum(2, 1);
                value.encode(out);
            }
            Self::Joining(value) => {
                out.sum(3, 1);
                value.encode(out);
            }
            Self::JoiningMany(value) => {
                out.sum(4, 1);
                value.encode(out);
            }
            Self::Ended(value) => {
                out.sum(5, 1);
                value.encode(out);
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (tag, fields) = input.sum()?;
        Ok(match tag {
            0 => {
                TokenReader::expect_fields(fields, 0)?;
                Self::Ready
            }
            1 | 6 => {
                TokenReader::expect_fields(fields, 1)?;
                Self::Resuming {
                    incoming: <IncomingRecord>::decode(input)?,
                    requeue: tag == 6,
                }
            }
            2 => {
                TokenReader::expect_fields(fields, 1)?;
                Self::Waiting(<u64>::decode(input)?)
            }
            3 => {
                TokenReader::expect_fields(fields, 1)?;
                Self::Joining(<TaskId>::decode(input)?)
            }
            4 => {
                TokenReader::expect_fields(fields, 1)?;
                Self::JoiningMany(<u64>::decode(input)?)
            }
            5 => {
                TokenReader::expect_fields(fields, 1)?;
                Self::Ended(<EndedTask>::decode(input)?)
            }
            _ => return Err(TokenError::UnsupportedVariant { variant: tag }),
        })
    }
}
impl TokenCodec for IncomingRecord {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Value(value) => {
                out.sum(0, 1);
                value.encode(out);
            }
            Self::Raise(value) => {
                out.sum(1, 1);
                value.encode(out);
            }
            Self::Joined(value) => {
                out.sum(2, 1);
                value.encode(out);
            }
            Self::Outcome(wait) => {
                out.sum(3, 1);
                wait.encode(out);
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (tag, fields) = input.sum()?;
        Ok(match tag {
            0 => {
                TokenReader::expect_fields(fields, 1)?;
                Self::Value(<PageValue>::decode(input)?)
            }
            1 => {
                TokenReader::expect_fields(fields, 1)?;
                Self::Raise(<PageValue>::decode(input)?)
            }
            2 => {
                TokenReader::expect_fields(fields, 1)?;
                Self::Joined(<TaskId>::decode(input)?)
            }
            3 => {
                TokenReader::expect_fields(fields, 1)?;
                Self::Outcome(crate::WaitId::decode(input)?)
            }
            _ => return Err(TokenError::UnsupportedVariant { variant: tag }),
        })
    }
}
impl TokenCodec for EndedTask {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Returned(value) => {
                out.sum(0, 1);
                value.encode(out);
            }
            Self::Raised(value) => {
                out.sum(1, 1);
                value.encode(out);
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (tag, fields) = input.sum()?;
        Ok(match tag {
            0 => {
                TokenReader::expect_fields(fields, 1)?;
                Self::Returned(<Option<PageValue>>::decode(input)?)
            }
            1 => {
                TokenReader::expect_fields(fields, 1)?;
                Self::Raised(<Option<PageValue>>::decode(input)?)
            }
            _ => return Err(TokenError::UnsupportedVariant { variant: tag }),
        })
    }
}

record!(FunctionRecord {
    name: NameRef,
    meta: PageValue,
    closure: PageRef,
    captures: std::collections::BTreeMap<lash_kernel_doc::Name, FrozenRef>,
    before: Vec<FrozenRef>,
    after: Vec<FrozenRef>
});
record!(BindingRecord {
    name: NameRef,
    declared: SiteRef,
    value: BoundRecord
});
impl TokenCodec for BoundRecord {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Value(value) => {
                out.sum(0, 1);
                value.encode(out);
            }
            Self::Shared(value) => {
                out.sum(1, 1);
                value.encode(out);
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (tag, fields) = input.sum()?;
        Ok(match tag {
            0 => {
                TokenReader::expect_fields(fields, 1)?;
                Self::Value(<PageValue>::decode(input)?)
            }
            1 => {
                TokenReader::expect_fields(fields, 1)?;
                Self::Shared(<EntityId>::decode(input)?)
            }
            _ => return Err(TokenError::UnsupportedVariant { variant: tag }),
        })
    }
}
record!(LoopRecord {
    site: SiteRef,
    started: u64,
    header: HeaderRecord,
    cursor: Option<CursorRecord>
});
units!(HeaderRecord { Body = 0, Open = 1, Pull = 2, Check = 3, Exhausted = 4, Item = 5 });
record!(CursorRecord {
    kind: CursorKind,
    position: u64,
    guard: Option<GuardRecord>
});
units!(CursorKind { List = 0, Tuple = 1, Table = 2, Text = 3, Driver = 4 });
record!(GuardRecord {
    size: u64,
    remaining: u64
});
record!(FinallyRecord {
    site: SiteRef,
    entered: EnteredRecord
});
impl TokenCodec for EnteredRecord {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::ReturnVia(site) => {
                out.sum(5, 1);
                site.encode(out);
            }
            _ => out.sum(
                match self {
                    Self::Normal => 0,
                    Self::Throw => 1,
                    Self::Return => 2,
                    Self::Break => 3,
                    Self::Continue => 4,
                    Self::ReturnVia(_) => unreachable!(),
                },
                0,
            ),
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (tag, fields) = input.sum()?;
        TokenReader::expect_fields(fields, if tag == 5 { 1 } else { 0 })?;
        Ok(match tag {
            0 => Self::Normal,
            1 => Self::Throw,
            2 => Self::Return,
            3 => Self::Break,
            4 => Self::Continue,
            5 => Self::ReturnVia(Box::new(SiteRef::decode(input)?)),
            _ => return Err(TokenError::UnsupportedVariant { variant: tag }),
        })
    }
}

record!(WaitRecord {
    task: TaskId,
    kind: WaitKindRecord,
    phase: WaitPhase
});
units!(WaitKindRecord { Effect = 0, Sleep = 1 });
impl TokenCodec for WaitPhase {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Issued { request } => {
                out.sum(0, 1);
                request.encode(out);
            }
            Self::Admitted => out.sum(1, 0),
            Self::Committed(value) => {
                out.sum(2, 1);
                value.encode(out);
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (tag, fields) = input.sum()?;
        Ok(match tag {
            0 => {
                TokenReader::expect_fields(fields, 1)?;
                Self::Issued {
                    request: FrozenRef::decode(input)?,
                }
            }
            1 => {
                TokenReader::expect_fields(fields, 0)?;
                Self::Admitted
            }
            2 => {
                TokenReader::expect_fields(fields, 1)?;
                Self::Committed(<CommittedOutcomeRecord>::decode(input)?)
            }
            _ => return Err(TokenError::UnsupportedVariant { variant: tag }),
        })
    }
}
impl TokenCodec for CommittedOutcomeRecord {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Completed(value) => {
                out.sum(0, 1);
                value.encode(out);
            }
            Self::Failed(value) => {
                out.sum(1, 1);
                value.encode(out);
            }
            Self::Elapsed => out.sum(2, 0),
            Self::Rejected(value) => {
                out.sum(3, 1);
                value.encode(out);
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (tag, fields) = input.sum()?;
        Ok(match tag {
            0 => {
                TokenReader::expect_fields(fields, 1)?;
                Self::Completed(<FrozenRef>::decode(input)?)
            }
            1 => {
                TokenReader::expect_fields(fields, 1)?;
                Self::Failed(<FrozenRef>::decode(input)?)
            }
            2 => {
                TokenReader::expect_fields(fields, 0)?;
                Self::Elapsed
            }
            3 => {
                TokenReader::expect_fields(fields, 1)?;
                Self::Rejected(FrozenRef::decode(input)?)
            }
            _ => return Err(TokenError::UnsupportedVariant { variant: tag }),
        })
    }
}
record!(ListJoinRecord { joiner: TaskId, mode: JoinMode, members: tree::Tree<TaskId> });

units!(CellTargetKind { Main = 0, Entry = 1 });
