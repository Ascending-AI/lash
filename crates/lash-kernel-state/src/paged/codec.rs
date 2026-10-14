//! Canonical semantic record bodies. Every field is present, including
//! empty roots; no runtime coordinate or transport grant is serialized.
use super::*;
use lash_kernel_doc::graph_tokens::{TokenCodec, TokenError, TokenReader, TokenWriter};
use lash_kernel_doc::heap::*;
use lash_kernel_doc::{DocumentId, EffectIdentity, JoinMode, Name, Site, TaskId, TaskIdentity};

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
record!(KeyedRoot { page: Option<PageRef>, len: u64, cost: LogicalBytes });
record!(SequenceRoot { page: Option<PageRef>, len: u64, cost: LogicalBytes });
record!(AccountingCertificate {
    metric: AccountingMetric,
    upper: LogicalBytes,
    entities: u64,
    refinements: u64
});
units!(AccountingMetric { LocalCostV1 = 0 });
record!(HeapRoot {
    directory: KeyedRoot,
    families: KeyedRoot,
    next_ordinal: u64,
    next_family: u64,
    accounting: AccountingCertificate,
    population: DataOnlyCertificate
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
    len: u64,
    live: bool
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
record!(SavedMeters {
    charged: u64,
    waits_issued: u64,
    next_join: u64,
    live_tasks: u32
});
record!(CheckpointRoot {
    kernel: u32,
    document: DocumentId,
    release: PageId,
    owner: PageId,
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
record!(TaskRecord { identity: TaskIdentity, state: TaskStateRecord, joiners: Vec<JoinerRecord>, observed: bool, passed: bool,
    failed: bool, occurrences: KeyedRoot, frames: SequenceRoot });
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
            Self::Resuming(value) => {
                out.sum(1, 1);
                value.encode(out);
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
            1 => {
                TokenReader::expect_fields(fields, 1)?;
                Self::Resuming(<IncomingRecord>::decode(input)?)
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
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (tag, fields) = input.sum()?;
        Ok(match tag {
            0 => {
                TokenReader::expect_fields(fields, 1)?;
                Self::Value(<Value>::decode(input)?)
            }
            1 => {
                TokenReader::expect_fields(fields, 1)?;
                Self::Raise(<Value>::decode(input)?)
            }
            2 => {
                TokenReader::expect_fields(fields, 1)?;
                Self::Joined(<TaskId>::decode(input)?)
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
                Self::Returned(<Value>::decode(input)?)
            }
            1 => {
                TokenReader::expect_fields(fields, 1)?;
                Self::Raised(<Value>::decode(input)?)
            }
            _ => return Err(TokenError::UnsupportedVariant { variant: tag }),
        })
    }
}
record!(FrameRecord { statement: Site, bindings: Vec<BindingRecord>, loops: Vec<LoopRecord>, finally: Vec<FinallyRecord>, held: HeldRecord });
record!(BindingRecord {
    name: Name,
    declared: Site,
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
                Self::Value(<Value>::decode(input)?)
            }
            1 => {
                TokenReader::expect_fields(fields, 1)?;
                Self::Shared(<EntityId>::decode(input)?)
            }
            _ => return Err(TokenError::UnsupportedVariant { variant: tag }),
        })
    }
}
record!(LoopRecord { site: Site, started: u64, position: Option<u64> });
record!(FinallyRecord {
    site: Site,
    entered: EnteredRecord
});
units!(EnteredRecord { Normal = 0, Throw = 1, Return = 2, Break = 3, Continue = 4 });
record!(HeldRecord { iterated: Vec<Value>, departing: Vec<Value>, arguments: Option<Vec<Value>> });
record!(WaitRecord {
    wait: u64,
    task: TaskId,
    identity: EffectIdentity,
    kind: WaitKindRecord,
    state: WaitStateRecord
});
units!(WaitKindRecord { Effect = 0, Sleep = 1 });
impl TokenCodec for WaitStateRecord {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Admitted => out.sum(0, 0),
            Self::Committed(value) => {
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
                Self::Admitted
            }
            1 => {
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
            _ => return Err(TokenError::UnsupportedVariant { variant: tag }),
        })
    }
}
record!(ListJoinRecord { joiner: TaskId, mode: JoinMode, members: Vec<TaskId> });
