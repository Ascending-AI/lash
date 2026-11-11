//! Canonical semantic record bodies. Every field is present, including
//! empty roots; no runtime coordinate or transport grant is serialized.
use super::*;
use lash_kernel_doc::heap::*;

use lash_kernel_value::names::DocumentId;
use {
    lash_kernel_value::tokens::Tag, lash_kernel_value::tokens::TokenCodec,
    lash_kernel_value::tokens::TokenError, lash_kernel_value::tokens::TokenReader,
    lash_kernel_value::tokens::TokenWriter,
};

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
record!(FrozenSummary {
    entities: u64,
    cost: LogicalBytes,
    data_only: bool
});
record!(FrozenRef {
    root: PageRef,
    summary: FrozenSummary
});
// The paged engine's session was a record; the run form is a one-field
// sum, which no record's token begins with. A record is refused typed.
impl TokenCodec for SessionRoot {
    fn encode(&self, out: &mut TokenWriter) {
        out.sum(0, 1);
        self.0.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        if input.peek_tag()? != Tag::Sum {
            return Err(TokenError::Retired(
                lash_kernel_value::tokens::RetiredState::Session,
            ));
        }
        match input.sum()? {
            (0, 1) => Ok(Self(PageRef::decode(input)?)),
            (variant, _) => Err(TokenError::UnsupportedVariant { variant }),
        }
    }
}
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
        let charged = if input.peek_tag()? == lash_kernel_value::tokens::Tag::None {
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
        let memory = if input.peek_tag()? == lash_kernel_value::tokens::Tag::None {
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
    link: lash_kernel_value::content_ids::LinkId,
    closure: PageId,
    helper_library: lash_kernel_value::HelperLibraryId,
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
    host: Vec<lash_kernel_value::names::Name>,
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

units!(CellTargetKind { Main = 0, Entry = 1 });
