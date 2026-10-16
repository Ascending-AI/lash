//! A frame's pending continuation (`K-EVAL-008`): where a frame that
//! stands at an action nested in its statement's expression stands, and
//! the operand values its statement has already evaluated.
//!
//! Registers are never saved. The record names the action and each role by
//! its site, as the path suffix under the frame's `statement` site, and a
//! resume loads the roles through the resume entry of the layout it runs
//! under. The roles are flat in the frame's row, each a (site suffix,
//! value) pair in evaluation order; a list longer than [`ROLE_FLAT`] spills
//! to a sequence of its own, [`ROLE_CHUNK`] roles to a cell, instead of
//! growing the row.
use super::*;

/// How many roles a continuation keeps flat in its frame's row.
pub const ROLE_FLAT: usize = 16;

/// How many roles one cell of a spilled role sequence holds.
pub const ROLE_CHUNK: usize = 16;

/// Where a frame stands inside its statement, and what it keeps there.
#[derive(Clone)]
pub struct PendingRecord {
    /// The action's site, as the path suffix under the frame's statement.
    pub site: Vec<u32>,
    pub phase: PhaseRecord,
    pub roles: RolesRecord,
}

/// The roles of a continuation, in evaluation order.
#[derive(Clone)]
pub enum RolesRecord {
    /// At most [`ROLE_FLAT`] roles, in the frame's row.
    Flat(Arc<[RoleRecord]>),
    /// More, [`ROLE_CHUNK`] to a cell of a sequence of their own.
    Spilled(Tree<RoleChunk>),
}

/// The phase of the action a frame stands at (`K-EVAL-008`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhaseRecord {
    /// A call: the callee's frame is above.
    Called,
    /// A `perform` or a `sleep`: the task waits on its request.
    Waiting,
    /// A `spawn`: the child runs first, and the spawner goes on next with
    /// the handle (`K-TASK-002`).
    Spawned,
}

/// One operand value a continuation keeps: the path suffix, under the
/// frame's statement, of the expression that produced it.
#[derive(Clone, Debug)]
pub struct RoleRecord {
    pub site: Vec<u32>,
    pub value: Value,
}

/// A run of consecutive roles: one cell of a spilled role sequence.
#[derive(Clone)]
pub struct RoleChunk(pub Vec<RoleRecord>);

impl PendingRecord {
    /// The continuation at `site` in `phase`, keeping `roles` in order.
    pub fn new(
        site: Vec<u32>,
        phase: PhaseRecord,
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
        Ok(Self { site, phase, roles })
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
        visit: &mut dyn FnMut(&Value) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        self.visit_roles(access, &mut |role| visit(&role.value))
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
                site: <Vec<u32>>::decode(input)?,
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

impl Cell for PendingRecord {
    fn same_version(&self, other: &Self) -> bool {
        self.site == other.site
            && self.phase == other.phase
            && self.roles.same_version(&other.roles)
    }
    fn task_edges(&self) -> u64 {
        self.roles.task_edges()
    }
    fn cost(&self) -> u64 {
        self.roles.cost()
    }
    fn prepare(&mut self, access: &Access<'_>, work: &mut SealWork) -> Result<bool, HeapFault> {
        match &mut self.roles {
            RolesRecord::Flat(_) => Ok(true),
            RolesRecord::Spilled(tree) => tree.seal_nested(access, work),
        }
    }
    fn children(&self, out: &mut Vec<PageRef>) {
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

impl TokenCodec for PhaseRecord {
    fn encode(&self, out: &mut TokenWriter) {
        out.sum(
            match self {
                Self::Called => 0,
                Self::Waiting => 1,
                Self::Spawned => 2,
            },
            0,
        );
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (tag, fields) = input.sum()?;
        TokenReader::expect_fields(fields, 0)?;
        Ok(match tag {
            0 => Self::Called,
            1 => Self::Waiting,
            2 => Self::Spawned,
            _ => return Err(TokenError::UnsupportedVariant { variant: tag }),
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
        role.site.encode(out);
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

impl TokenCodec for PendingRecord {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        self.site.encode(out);
        self.phase.encode(out);
        self.roles.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(3)?;
        Ok(Self {
            site: <Vec<u32>>::decode(input)?,
            phase: <PhaseRecord>::decode(input)?,
            roles: <RolesRecord>::decode(input)?,
        })
    }
}
