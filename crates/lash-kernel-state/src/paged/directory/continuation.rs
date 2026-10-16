//! A frame's pending continuation (FIG-6417 P0): where a frame parked at
//! an action nested in its statement's expression waits, and the operand
//! values it keeps.
use super::*;

/// Where a frame parked at an action nested in its statement's
/// expression waits (`K-EVAL-006`): the action's site, its phase, and
/// the operand values the statement already evaluated, each named by
/// the site of the expression that produced it. Registers are never
/// saved; a resume loads the roles through the layout's resume entry.
/// Sites are the path suffix under the frame's `statement` site (same
/// unit): a role and the action are still named by site, compactly.
#[derive(Clone)]
pub struct PendingRecord {
    pub site: Vec<u32>,
    pub phase: PhaseRecord,
    /// The roles, in `live_roles` order, keyed by ordinal.
    pub roles: Tree<RoleRecord>,
}
/// What a parked nested action is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhaseRecord {
    /// The action called a function whose frame is running or returned.
    Called,
    /// The action requested an effect or a sleep and waits on it.
    Waiting,
}
/// One operand value a pending continuation holds.
#[derive(Clone)]
pub struct RoleRecord {
    /// The path suffix of the expression that produced the value,
    /// under the frame's `statement` site.
    pub site: Vec<u32>,
    pub value: Value,
}
impl Cell for PhaseRecord {
    fn cost(&self) -> u64 {
        0
    }
}
impl Cell for RoleRecord {
    fn task_edges(&self) -> u64 {
        self.value.task_edges()
    }
    fn cost(&self) -> u64 {
        value_cost(&self.value)
    }
    fn decode_cell(
        input: &mut TokenReader<'_>,
        family: Option<FamilyId>,
    ) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            site: <Vec<u32>>::decode(input)?,
            value: lash_kernel_doc::heap_values::decode_value_with_family(input, family)?,
        })
    }
    fn children(&self, out: &mut Vec<PageRef>) {
        value_children(&self.value, out);
    }
    fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
        value_grants(&mut self.value, children)
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
        16 + self.roles.cost()
    }
    fn prepare(&mut self, access: &Access<'_>, work: &mut SealWork) -> Result<bool, HeapFault> {
        self.roles.seal_nested(access, work)
    }
    fn children(&self, out: &mut Vec<PageRef>) {
        self.roles.children(out);
    }
    fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
        self.roles.grants(children)
    }
}
impl TokenCodec for PhaseRecord {
    fn encode(&self, out: &mut TokenWriter) {
        out.sum(
            match self {
                Self::Called => 0,
                Self::Waiting => 1,
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
            _ => {
                return Err(TokenError::UnsupportedVariant { variant: tag });
            }
        })
    }
}
impl TokenCodec for RoleRecord {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.site.encode(out);
        self.value.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            site: <Vec<u32>>::decode(input)?,
            value: <Value>::decode(input)?,
        })
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
            roles: <Tree<RoleRecord>>::decode(input)?,
        })
    }
}
