//! Read-only itemization for independent execution comparisons.
use super::*;

pub use super::{
    AcceptedProgram, EvaluatedProgram, JournalEvent, Materializations, PreparedProgram, Program,
};

/// The actual canonical holding, including repeated references and zero-byte
/// capture roots. Physical arena residency does not enter this observation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HoldingSnapshot {
    pub bytes: u64,
    pub edges: Vec<(Identity, u64)>,
}

/// A complete ledger observation. Retained cycles appear even without a root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedgerSnapshot {
    pub roots: Vec<(Owner, HoldingSnapshot)>,
    pub entities: Vec<(EntityId, HoldingSnapshot, u64)>,
    pub balance: Balance,
    pub next_invocation: u64,
}

fn holding(value: &Holding, access: &Access<'_>) -> Result<HoldingSnapshot, HeapFault> {
    let mut edges = Vec::new();
    value.visit_edges(access, &mut |target, count| {
        edges.push((target, count));
        Ok(())
    })?;
    edges.sort();
    Ok(HoldingSnapshot {
        bytes: value.bytes,
        edges,
    })
}

impl Ledger {
    /// Observe the committed ledger through its ordinary cold-page reader.
    /// This neither changes prices nor creates holding or publication authority.
    pub fn testing_snapshot(&self, access: &Access<'_>) -> Result<LedgerSnapshot, HeapFault> {
        self.require_unleased()?;
        let mut roots = Vec::new();
        for tree in [&self.state.owners, &self.state.execution, &self.state.ended] {
            visit_tree(tree, access, &mut |root| {
                roots.push((root.owner, holding(&root.value, access)?));
                Ok(())
            })?;
        }
        roots.sort_by_key(|(owner, _)| *owner);
        let mut entities = Vec::new();
        let mut error = None;
        self.state.entities.visit(access, &mut |key, value| {
            let result = (|| {
                let entity = entity_of_key(key)
                    .ok_or_else(|| invalid("itemized entity key is malformed"))?;
                entities.push((entity, holding(&value.body, access)?, value.incoming));
                Ok(())
            })();
            if let Err(fault) = result {
                error = Some(fault);
                return false;
            }
            true
        })?;
        if let Some(error) = error {
            return Err(error);
        }
        entities.sort_by_key(|(entity, _, _)| *entity);
        Ok(LedgerSnapshot {
            roots,
            entities,
            balance: self.balance()?,
            next_invocation: self.state.next_invocation,
        })
    }
}
