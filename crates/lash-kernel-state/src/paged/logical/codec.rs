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
        Slot::Capture(n) => (8, n),
    }
}
#[derive(Clone)]
pub(super) struct Key {
    bytes: [u8; 33],
    len: usize,
}
impl Key {
    pub(super) fn tagged(tag: u8, bytes: &[u8]) -> Self {
        let mut key = Self {
            bytes: [0; 33],
            len: 1,
        };
        key.bytes[0] = tag;
        key.append(bytes);
        key
    }
    fn append(&mut self, bytes: &[u8]) {
        self.bytes[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
    }
    pub(super) fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}
impl AsRef<[u8]> for Key {
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}
impl std::ops::Deref for Key {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}
pub(super) fn invocation_key(task: TaskId, invocation: u64) -> Key {
    let mut key = Key::tagged(0, &task.0.to_be_bytes());
    key.append(&invocation.to_be_bytes());
    key
}
pub(super) fn operand_prefix(task: TaskId, invocation: u64) -> Key {
    let mut key = invocation_key(task, invocation);
    key.append(&[slot_parts(Slot::Operand(0)).0]);
    key
}
pub(super) fn owner_key(owner: Owner) -> Key {
    match owner {
        Owner::Frame { task, invocation }
        | Owner::Value {
            task, invocation, ..
        } => {
            let (kind, index) = match owner {
                Owner::Value { slot, .. } => slot_parts(slot),
                _ => (0, 0),
            };
            let mut key = invocation_key(task, invocation);
            key.append(&[kind]);
            key.append(&index.to_be_bytes());
            key
        }
        Owner::Session(key) => Key::tagged(1, &key),
        Owner::Incoming(task) => Key::tagged(2, &task.0.to_be_bytes()),
        Owner::Ended(task) => Key::tagged(3, &task.0.to_be_bytes()),
        Owner::Saved(key) => Key::tagged(4, &key),
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
            8 => Self::Capture(n),
            _ => return Err(TokenError::UnsupportedVariant { variant: tag }),
        })
    }
}
impl TokenCodec for Owner {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Frame { task, invocation } => {
                out.sum(0, 2);
                task.encode(out);
                invocation.encode(out);
            }
            Self::Value {
                task,
                invocation,
                slot,
            } => {
                out.sum(1, 3);
                task.encode(out);
                invocation.encode(out);
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
            Self::Saved(key) => {
                out.sum(5, 1);
                out.bytes(key);
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (tag, fields) = input.sum()?;
        let expected = match tag {
            0 => 2,
            1 => 3,
            2..=5 => 1,
            _ => return Err(TokenError::UnsupportedVariant { variant: tag }),
        };
        TokenReader::expect_fields(fields, expected)?;
        Ok(match tag {
            0 => Self::Frame {
                task: TaskId::decode(input)?,
                invocation: input.unsigned()?,
            },
            1 => Self::Value {
                task: TaskId::decode(input)?,
                invocation: input.unsigned()?,
                slot: Slot::decode(input)?,
            },
            2 => {
                Self::Session(input.bytes()?.try_into().map_err(|_| {
                    TokenError::Invalid("source session key is not 32 bytes".into())
                })?)
            }
            3 => Self::Incoming(TaskId::decode(input)?),
            4 => Self::Ended(TaskId::decode(input)?),
            _ => Self::Saved(
                input
                    .bytes()?
                    .try_into()
                    .map_err(|_| TokenError::Invalid("source saved key is not 32 bytes".into()))?,
            ),
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
            links: Links::decode(input)?,
        })
    }
}
impl Cell for Holding {
    fn same_version(&self, other: &Self) -> bool {
        self.bytes == other.bytes && self.links.same_version(&other.links)
    }
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
        Owner::Value {
            slot: Slot::Capture(_),
            ..
        } => {
            if record.value.bytes != 0
                || record.value.links.len() != 1
                || record.value.links.task_edges() != 0
            {
                Err(invalid(
                    "capture must borrow one variable cell without an occurrence",
                ))
            } else {
                Ok(())
            }
        }
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
    fn same_version(&self, other: &Self) -> bool {
        self.owner == other.owner && self.value.same_version(&other.value)
    }
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
    fn same_version(&self, other: &Self) -> bool {
        self.incoming == other.incoming && self.body.same_version(&other.body)
    }
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
        out.seq(6);
        self.owners.encode(out);
        self.execution.encode(out);
        self.ended.encode(out);
        self.entities.encode(out);
        self.refinements.encode(out);
        self.next_invocation.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(6)?;
        Ok(Self {
            owners: KeyedRoot::decode(input)?,
            execution: KeyedRoot::decode(input)?,
            ended: KeyedRoot::decode(input)?,
            entities: KeyedRoot::decode(input)?,
            refinements: input.unsigned()?,
            next_invocation: input.unsigned()?,
        })
    }
}
