use super::*;
use lash_kernel_doc::heap::PageRef;
use lash_kernel_value::tokens::{TokenCodec, TokenError, TokenReader, TokenWriter};

fn slot_parts(slot: Slot) -> (u8, u32) {
    match slot {
        Slot::Binding(n) => (1, n),
        Slot::Operand(n) => (2, n),
        Slot::Control(n) => (3, n),
        Slot::Argument(n) => (4, n),
        Slot::Departure(n) => (5, n),
        Slot::Continuation(n) => (6, n),
        Slot::Scratch(n) => (7, n),
    }
}
pub(super) fn owner_key(owner: Owner) -> Vec<u8> {
    match owner {
        Owner::Frame { task, depth } | Owner::Value { task, depth, .. } => {
            let (kind, index) = match owner {
                Owner::Value { slot, .. } => slot_parts(slot),
                _ => (0, 0),
            };
            [
                vec![0],
                task.0.to_be_bytes().to_vec(),
                depth.to_be_bytes().to_vec(),
                vec![kind],
                index.to_be_bytes().to_vec(),
            ]
            .concat()
        }
        Owner::Session(key) => [vec![1], key.to_vec()].concat(),
        Owner::Incoming(task) => [vec![2], task.0.to_be_bytes().to_vec()].concat(),
        Owner::Ended(task) => [vec![3], task.0.to_be_bytes().to_vec()].concat(),
    }
}
impl TokenCodec for Slot {
    fn encode(&self, out: &mut TokenWriter) {
        let (tag, n) = slot_parts(*self);
        out.sum(u32::from(tag), 1);
        out.unsigned(u64::from(n));
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (tag, fields) = input.sum()?;
        TokenReader::expect_fields(fields, 1)?;
        let n = input.unsigned_u32()?;
        Ok(match tag {
            1 => Self::Binding(n),
            2 => Self::Operand(n),
            3 => Self::Control(n),
            4 => Self::Argument(n),
            5 => Self::Departure(n),
            6 => Self::Continuation(n),
            7 => Self::Scratch(n),
            _ => return Err(TokenError::UnsupportedVariant { variant: tag }),
        })
    }
}
impl TokenCodec for Owner {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Frame { task, depth } => {
                out.sum(0, 2);
                task.encode(out);
                depth.encode(out);
            }
            Self::Value { task, depth, slot } => {
                out.sum(1, 3);
                task.encode(out);
                depth.encode(out);
                slot.encode(out);
            }
            Self::Session(key) => {
                out.sum(2, 1);
                out.bytes(key);
            }
            Self::Incoming(task) => {
                out.sum(3, 1);
                task.encode(out);
            }
            Self::Ended(task) => {
                out.sum(4, 1);
                task.encode(out);
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (tag, fields) = input.sum()?;
        let expected = match tag {
            0 => 2,
            1 => 3,
            2..=4 => 1,
            _ => return Err(TokenError::UnsupportedVariant { variant: tag }),
        };
        TokenReader::expect_fields(fields, expected)?;
        Ok(match tag {
            0 => Self::Frame {
                task: TaskId::decode(input)?,
                depth: input.unsigned()?,
            },
            1 => Self::Value {
                task: TaskId::decode(input)?,
                depth: input.unsigned()?,
                slot: Slot::decode(input)?,
            },
            2 => {
                Self::Session(input.bytes()?.try_into().map_err(|_| {
                    TokenError::Invalid("source session key is not 32 bytes".into())
                })?)
            }
            3 => Self::Incoming(TaskId::decode(input)?),
            _ => Self::Ended(TaskId::decode(input)?),
        })
    }
}
impl TokenCodec for Edge {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.target.encode(out);
        self.count.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        let target = Identity::decode(input)?;
        let count = input.unsigned()?;
        if count == 0 {
            return Err(TokenError::Invalid(
                "logical edge has zero multiplicity".into(),
            ));
        }
        Ok(Self { target, count })
    }
}
impl Cell for Edge {
    fn cost(&self) -> u64 {
        0
    }
    fn task_edges(&self) -> u64 {
        if matches!(self.target, Identity::Task(_)) {
            self.count
        } else {
            0
        }
    }
}
impl TokenCodec for Holding {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.bytes.encode(out);
        self.links.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            bytes: input.unsigned()?,
            links: Tree::decode(input)?,
        })
    }
}
impl Cell for Holding {
    fn cost(&self) -> u64 {
        self.bytes
    }
    fn task_edges(&self) -> u64 {
        self.links.task_edges()
    }
    fn prepare(&mut self, access: &Access<'_>, work: &mut SealWork) -> Result<bool, HeapFault> {
        self.links.seal_nested(access, work)
    }
    fn children(&self, out: &mut Vec<PageRef>) {
        self.links.children(out);
    }
    fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
        self.links.grants(children)
    }
}
impl TokenCodec for RootHolding {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.owner.encode(out);
        self.value.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        let record = Self {
            owner: Owner::decode(input)?,
            value: Holding::decode(input)?,
        };
        validate_root(&record).map_err(|e| TokenError::Invalid(e.to_string()))?;
        Ok(record)
    }
}
pub(super) fn validate_root(record: &RootHolding) -> Result<(), HeapFault> {
    match record.owner {
        Owner::Frame { .. }
            if record.value.bytes != Cost::FRAME || !record.value.links.is_empty() =>
        {
            Err(invalid("frame owner has another price"))
        }
        Owner::Value {
            slot: Slot::Scratch(_),
            ..
        } if !record.value.links.is_empty() => Err(invalid("scratch is not a guest value owner")),
        Owner::Frame { .. }
        | Owner::Value {
            slot: Slot::Scratch(_),
            ..
        } => Ok(()),
        _ if record.value.bytes < Cost::REFERENCE => {
            Err(invalid("holding has no occurrence price"))
        }
        _ => Ok(()),
    }
}
impl Cell for RootHolding {
    fn cost(&self) -> u64 {
        self.value.bytes
    }
    fn live_members(&self) -> u64 {
        u64::from(self.owner.transient())
    }
    fn task_edges(&self) -> u64 {
        self.value.task_edges()
    }
    fn prepare(&mut self, access: &Access<'_>, work: &mut SealWork) -> Result<bool, HeapFault> {
        self.value.prepare(access, work)
    }
    fn children(&self, out: &mut Vec<PageRef>) {
        self.value.children(out);
    }
    fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
        self.value.grants(children)
    }
}
impl TokenCodec for EntityHolding {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.body.encode(out);
        self.incoming.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        let record = Self {
            body: Holding::decode(input)?,
            incoming: input.unsigned()?,
        };
        if record.body.bytes < Cost::OBJECT {
            return Err(TokenError::Invalid("entity has no logical header".into()));
        }
        Ok(record)
    }
}
impl Cell for EntityHolding {
    fn cost(&self) -> u64 {
        self.body.bytes
    }
    fn task_edges(&self) -> u64 {
        self.body.task_edges()
    }
    fn prepare(&mut self, access: &Access<'_>, work: &mut SealWork) -> Result<bool, HeapFault> {
        self.body.prepare(access, work)
    }
    fn children(&self, out: &mut Vec<PageRef>) {
        self.body.children(out);
    }
    fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
        self.body.grants(children)
    }
}
impl TokenCodec for LedgerRoot {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(4);
        self.owners.encode(out);
        self.ended.encode(out);
        self.entities.encode(out);
        self.refinements.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(4)?;
        Ok(Self {
            owners: KeyedRoot::decode(input)?,
            ended: KeyedRoot::decode(input)?,
            entities: KeyedRoot::decode(input)?,
            refinements: input.unsigned()?,
        })
    }
}
