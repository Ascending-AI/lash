//! A frame's canonical source continuation (`K-EVAL-008`), including calls,
//! Return, scope Leaves and caller completion with their retained values/debt.
//!
//! Registers and live authority are never saved. Coordinates are suffixes under
//! the frame's statement or defining body. Roles name expressions or logical
//! source slots; reopen validates their placement against admitted source.
//! The roles are flat in the frame's row; a list longer than [`ROLE_FLAT`] spills
//! to a sequence of its own, [`ROLE_CHUNK`] roles to a cell, instead of
//! growing the row.
use super::*;

/// How many roles a continuation keeps flat in its frame's row.
pub const ROLE_FLAT: usize = 16;

/// How many roles one cell of a spilled role sequence holds.
pub const ROLE_CHUNK: usize = 16;

/// Where a frame stands in its source, and what it keeps there.
#[derive(Clone)]
pub struct ContinuationRecord {
    /// Source site suffix under the frame's statement or defining body.
    pub site: Vec<u32>,
    pub phase: ContinuationPhase,
    /// The actual ordinal counted before this call entered its callee.
    pub occurrence: Option<u64>,
    pub roles: RolesRecord,
    /// Canonical frame identity, never an ISA activation or lease identity.
    pub invocation: u64,
    /// The next unpaid source event, relative to the frame's statement.
    pub unpaid: Option<(Vec<u32>, u32)>,
    /// A computed result, including the task Incoming before caller completion.
    pub pending: Option<PageValue>,
    /// Return origin retained while its scope Leaves run.
    pub departure: Option<(Vec<u32>, u32)>,
    /// Each exit a running cleanup suspended, outermost first: its actual
    /// departure origin and its quoted completion debt. Its value is the
    /// frame's `finally` record's.
    pub suspended: Vec<((Vec<u32>, u32), u64)>,
    /// Reached source occurrence and ordered charge progress.
    pub ordinal: u64,
    pub debt: ChargeDebtRecord,
    /// A reached source refusal: charge, memory, or call depth.
    pub refusal: Option<u32>,
}

/// The roles of a continuation, in evaluation order.
#[derive(Clone)]
pub enum RolesRecord {
    /// At most [`ROLE_FLAT`] roles, in the frame's row.
    Flat(Arc<[RoleRecord]>),
    /// More, [`ROLE_CHUNK`] to a cell of a sequence of their own.
    Spilled(Tree<RoleChunk>),
}

/// The source phase a frame stands at (`K-EVAL-008`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContinuationPhase {
    /// A `yield`: the task awaits its queued turn.
    Yielded,
    /// A call: the callee's frame is above.
    Call { phase: u32 },
    /// A `perform` or a `sleep`: the task waits on its request.
    Waiting,
    /// A `spawn`: the child runs first, and the spawner goes on next with
    /// the handle (`K-TASK-002`).
    Spawned,
    /// A `join` on one handle or a list: the task waits until the join is
    /// decided (`K-TASK-008`).
    Joining,
    /// A canonical semantic phase and its before/computed/accepted position.
    /// Both tags are checked against admitted source when the frame reopens.
    Source { phase: u32, progress: u32 },
    /// A returned call owes its quoted completion before caller Restore.
    Completion { phase: u32 },
}

/// One operand value a continuation keeps: the path suffix, under the
/// frame's statement, of the expression that produced it.
#[derive(Clone, Debug)]
pub struct RoleRecord {
    pub coordinate: RoleCoordinate,
    pub value: PageValue,
}

/// Source identity of a retained value. Logical slots precede ISA placement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoleCoordinate {
    Expression(Vec<u32>),
    Slot(crate::paged::logical::Slot),
}

/// Already paid source charges and quoted completion work whose input roles
/// may have been released. Reopen checks the cursor and owning source phase.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChargeDebtRecord {
    pub cursor: u64,
    pub paid: u64,
    pub completion: u64,
    pub completion_paid: bool,
}

/// A run of consecutive roles: one cell of a spilled role sequence.
#[derive(Clone)]
pub struct RoleChunk(pub Vec<RoleRecord>);

impl ContinuationRecord {
    /// The continuation at `site` in `phase`, keeping `roles` in order.
    pub fn new(
        site: Vec<u32>,
        phase: ContinuationPhase,
        roles: Vec<RoleRecord>,
        access: &Access<'_>,
    ) -> Result<Self, HeapFault> {
        let roles = if roles.len() <= ROLE_FLAT {
            RolesRecord::Flat(roles.into())
        } else {
            let mut tree = Tree::new(true);
            for (index, chunk) in roles.chunks(ROLE_CHUNK).enumerate() {
                tree.write(ordinal_key(index as u64), RoleChunk(chunk.to_vec()), access)?;
            }
            RolesRecord::Spilled(tree)
        };
        Ok(Self {
            site,
            phase,
            occurrence: None,
            roles,
            invocation: 0,
            unpaid: None,
            pending: None,
            departure: None,
            suspended: Vec::new(),
            ordinal: 0,
            debt: ChargeDebtRecord::default(),
            refusal: None,
        })
    }

    pub fn with_invocation(mut self, invocation: u64) -> Self {
        self.invocation = invocation;
        self
    }

    /// Retains the original action ordinal with its source continuation.
    pub fn with_occurrence(mut self, occurrence: Option<u64>) -> Self {
        self.occurrence = occurrence;
        self
    }

    /// The roles, in the order they were saved.
    pub fn roles(&self, access: &Access<'_>) -> Result<Vec<RoleRecord>, HeapFault> {
        let mut roles = Vec::new();
        self.visit_roles(access, &mut |role| {
            roles.push(role.clone());
            Ok(())
        })?;
        Ok(roles)
    }

    /// Visits each role's value.
    pub fn visit_values(
        &self,
        access: &Access<'_>,
        visit: &mut dyn FnMut(&PageValue) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        self.visit_roles(access, &mut |role| visit(&role.value))?;
        if let Some(value) = &self.pending {
            visit(value)?;
        }
        Ok(())
    }

    fn visit_roles(
        &self,
        access: &Access<'_>,
        visit: &mut dyn FnMut(&RoleRecord) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        match &self.roles {
            RolesRecord::Flat(roles) => roles.iter().try_for_each(visit),
            RolesRecord::Spilled(tree) => {
                let mut error = None;
                tree.visit(access, &mut |_, chunk| {
                    for role in &chunk.0 {
                        if let Err(fault) = visit(role) {
                            error = Some(fault);
                            return false;
                        }
                    }
                    true
                })?;
                error.map_or(Ok(()), Err)
            }
        }
    }
}

impl RolesRecord {
    /// Whether `other` is this version of the roles: the same flat list,
    /// or the same spilled sequence.
    pub fn same_version(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Flat(a), Self::Flat(b)) => Arc::ptr_eq(a, b),
            (Self::Spilled(a), Self::Spilled(b)) => a.same_version(b),
            _ => false,
        }
    }
    fn task_edges(&self) -> u64 {
        match self {
            Self::Flat(roles) => roles.iter().map(|role| role.value.task_edges()).sum(),
            Self::Spilled(tree) => tree.task_edges(),
        }
    }
    fn cost(&self) -> u64 {
        match self {
            Self::Flat(roles) => roles.iter().map(|role| value_cost(&role.value)).sum(),
            Self::Spilled(tree) => tree.cost(),
        }
    }
}

impl Cell for RoleChunk {
    fn task_edges(&self) -> u64 {
        self.0.iter().map(|role| role.value.task_edges()).sum()
    }
    fn cost(&self) -> u64 {
        self.0.iter().map(|role| value_cost(&role.value)).sum()
    }
    fn decode_cell(
        input: &mut TokenReader<'_>,
        family: Option<FamilyId>,
    ) -> Result<Self, TokenError> {
        let tokens = input.seq()?;
        if tokens % 2 != 0 {
            return Err(TokenError::Invalid("a role is a site and a value".into()));
        }
        let count = tokens / 2;
        let mut roles = Vec::with_capacity((count as usize).min(ROLE_CHUNK));
        for _ in 0..count {
            roles.push(RoleRecord {
                coordinate: RoleCoordinate::decode(input)?,
                value: lash_kernel_doc::heap_values::decode_value_with_family(input, family)?,
            });
        }
        Ok(Self(roles))
    }
    fn children(&self, out: &mut Vec<PageRef>) {
        for role in &self.0 {
            value_children(&role.value, out);
        }
    }
    fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
        for role in &mut self.0 {
            value_grants(&mut role.value, children)?;
        }
        Ok(())
    }
}

impl Cell for ContinuationRecord {
    fn same_version(&self, other: &Self) -> bool {
        self.site == other.site
            && self.phase == other.phase
            && self.occurrence == other.occurrence
            && self.roles.same_version(&other.roles)
            && self.invocation == other.invocation
            && self.unpaid == other.unpaid
            && match (&self.pending, &other.pending) {
                (Some(a), Some(b)) => a.same_version(b),
                (None, None) => true,
                _ => false,
            }
            && self.departure == other.departure
            && self.suspended == other.suspended
            && self.ordinal == other.ordinal
            && self.debt == other.debt
            && self.refusal == other.refusal
    }
    fn task_edges(&self) -> u64 {
        self.roles.task_edges() + self.pending.as_ref().map_or(0, PageValue::task_edges)
    }
    fn cost(&self) -> u64 {
        self.roles.cost() + self.pending.as_ref().map_or(0, value_cost)
    }
    fn prepare(&mut self, access: &Access<'_>, work: &mut SealWork) -> Result<bool, HeapFault> {
        match &mut self.roles {
            RolesRecord::Flat(_) => Ok(true),
            RolesRecord::Spilled(tree) => tree.seal_nested(access, work),
        }
    }
    fn children(&self, out: &mut Vec<PageRef>) {
        if let Some(value) = &self.pending {
            value_children(value, out);
        }
        match &self.roles {
            RolesRecord::Flat(roles) => {
                for role in roles.iter() {
                    value_children(&role.value, out);
                }
            }
            RolesRecord::Spilled(tree) => tree.children(out),
        }
    }
    fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
        if let Some(value) = &mut self.pending {
            value_grants(value, children)?;
        }
        match &mut self.roles {
            RolesRecord::Flat(roles) => {
                let mut granted = roles.to_vec();
                for role in &mut granted {
                    value_grants(&mut role.value, children)?;
                }
                *roles = granted.into();
                Ok(())
            }
            RolesRecord::Spilled(tree) => tree.grants(children),
        }
    }
}

impl TokenCodec for ContinuationPhase {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Call { phase } | Self::Completion { phase } => {
                out.sum(
                    if matches!(self, Self::Call { .. }) {
                        0
                    } else {
                        6
                    },
                    1,
                );
                phase.encode(out);
            }
            Self::Source { phase, progress } => {
                out.sum(5, 2);
                phase.encode(out);
                progress.encode(out);
            }
            phase => out.sum(
                match phase {
                    Self::Waiting => 1,
                    Self::Spawned => 2,
                    Self::Joining => 3,
                    Self::Yielded => 4,
                    _ => unreachable!(),
                },
                0,
            ),
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (tag, fields) = input.sum()?;
        if matches!(tag, 0 | 5 | 6) {
            TokenReader::expect_fields(fields, if tag == 5 { 2 } else { 1 })?;
            let phase = u32::decode(input)?;
            if phase > 12 {
                return Err(TokenError::Invalid("invalid canonical source phase".into()));
            }
            return Ok(match tag {
                0 => Self::Call { phase },
                6 => Self::Completion { phase },
                _ => {
                    let progress = u32::decode(input)?;
                    if progress > 2 {
                        return Err(TokenError::Invalid(
                            "invalid canonical source progress".into(),
                        ));
                    }
                    Self::Source { phase, progress }
                }
            });
        }
        TokenReader::expect_fields(fields, 0)?;
        Ok(match tag {
            1 => Self::Waiting,
            2 => Self::Spawned,
            3 => Self::Joining,
            4 => Self::Yielded,
            _ => return Err(TokenError::UnsupportedVariant { variant: tag }),
        })
    }
}

impl TokenCodec for RoleCoordinate {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Expression(site) => {
                out.sum(0, 1);
                site.encode(out);
            }
            Self::Slot(slot) => {
                out.sum(1, 1);
                slot.encode(out);
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (tag, fields) = input.sum()?;
        TokenReader::expect_fields(fields, 1)?;
        match tag {
            0 => Ok(Self::Expression(Vec::<u32>::decode(input)?)),
            1 => Ok(Self::Slot(crate::paged::logical::Slot::decode(input)?)),
            _ => Err(TokenError::UnsupportedVariant { variant: tag }),
        }
    }
}

impl TokenCodec for ChargeDebtRecord {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(4);
        self.cursor.encode(out);
        self.paid.encode(out);
        self.completion.encode(out);
        self.completion_paid.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(4)?;
        Ok(Self {
            cursor: u64::decode(input)?,
            paid: u64::decode(input)?,
            completion: u64::decode(input)?,
            completion_paid: bool::decode(input)?,
        })
    }
}

impl TokenCodec for RoleChunk {
    fn encode(&self, out: &mut TokenWriter) {
        encode_roles(&self.0, out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Self::decode_cell(input, None)
    }
}

/// Each role as its site suffix, then its value: two tokens of one flat
/// sequence, no header of its own.
fn encode_roles(roles: &[RoleRecord], out: &mut TokenWriter) {
    out.seq(2 * roles.len() as u64);
    for role in roles {
        role.coordinate.encode(out);
        role.value.encode(out);
    }
}

impl TokenCodec for RolesRecord {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Flat(roles) => {
                out.sum(0, 1);
                encode_roles(roles, out);
            }
            Self::Spilled(tree) => {
                out.sum(1, 1);
                tree.encode(out);
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (tag, fields) = input.sum()?;
        TokenReader::expect_fields(fields, 1)?;
        match tag {
            0 => {
                let RoleChunk(roles) = RoleChunk::decode_cell(input, None)?;
                if roles.len() > ROLE_FLAT {
                    return Err(TokenError::Invalid(
                        "a flat role list exceeds its bound".into(),
                    ));
                }
                Ok(Self::Flat(roles.into()))
            }
            1 => {
                let tree = <Tree<RoleChunk>>::decode(input)?;
                if tree.len() <= (ROLE_FLAT / ROLE_CHUNK) as u64 {
                    return Err(TokenError::Invalid(
                        "a spilled role list fits its frame".into(),
                    ));
                }
                Ok(Self::Spilled(tree))
            }
            _ => Err(TokenError::UnsupportedVariant { variant: tag }),
        }
    }
}

impl TokenCodec for ContinuationRecord {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(12);
        self.site.encode(out);
        self.phase.encode(out);
        self.occurrence.encode(out);
        self.roles.encode(out);
        self.invocation.encode(out);
        self.unpaid.encode(out);
        self.pending.encode(out);
        self.departure.encode(out);
        self.suspended.encode(out);
        self.ordinal.encode(out);
        self.debt.encode(out);
        self.refusal.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(12)?;
        Ok(Self {
            site: <Vec<u32>>::decode(input)?,
            phase: <ContinuationPhase>::decode(input)?,
            occurrence: <Option<u64>>::decode(input)?,
            roles: <RolesRecord>::decode(input)?,
            invocation: u64::decode(input)?,
            unpaid: Option::<(Vec<u32>, u32)>::decode(input)?,
            pending: Option::<PageValue>::decode(input)?,
            departure: Option::<(Vec<u32>, u32)>::decode(input)?,
            suspended: <Vec<((Vec<u32>, u32), u64)>>::decode(input)?,
            ordinal: u64::decode(input)?,
            debt: ChargeDebtRecord::decode(input)?,
            refusal: Option::<u32>::decode(input)?,
        })
    }
}
