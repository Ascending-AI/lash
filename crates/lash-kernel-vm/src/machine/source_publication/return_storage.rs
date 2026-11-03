//! Source-derived departure exits retain only their surviving exact transfer.
use super::*;
use crate::compile::room::{
    Phase, Point,
    publication::{DepartureExit, Occurrence},
};
use lash_kernel_state::kernel_internals::logical::{Ledger, Owner};
use lash_kernel_value::{
    sites::Site,
    tokens::{TokenCodec, TokenWriter, logical_digest},
};

pub(in crate::machine) struct ReturnInput<'a> {
    pub(in crate::machine) source: &'a Arc<room::Source>,
    pub(in crate::machine) invocation: &'a Invocation,
    pub(in crate::machine) task: doc::TaskId,
    pub(in crate::machine) at: &'a Site,
    pub(in crate::machine) carried: Option<Carried<'a>>,
    pub(in crate::machine) safe_point: u64,
    pub(in crate::machine) loops: Vec<(Site, u64)>,
    pub(in crate::machine) cursor: Option<(Point, Option<ArenaHandle>)>,
}
pub(in crate::machine) struct Carried<'a> {
    pub(in crate::machine) rhs: &'a Site,
    pub(in crate::machine) input: Owner,
    pub(in crate::machine) value: doc::Value,
}
pub(in crate::machine) struct ReturnRecipe {
    pub(in crate::machine) start: Position,
    pub(in crate::machine) next: Position,
    pub(in crate::machine) allowance: Allowance,
    pub(in crate::machine) exit: Arc<DepartureExit>,
    pub(in crate::machine) input: Option<Owner>,
    pub(in crate::machine) dependencies: Vec<ArenaHandle>,
    pub(in crate::machine) debt: Option<room::charges::ReturnDebt>,
}
impl ReturnInput<'_> {
    pub(in crate::machine) fn prepare(
        self,
        ledger: &Ledger,
        _arena: &mut Arena,
        heap: &PagedHeap,
    ) -> Result<Option<ReturnRecipe>, HeapFault> {
        let at = Point {
            site: self.at.clone(),
            phase: Phase::Commit,
        };
        let Ok(exit) = self.source.departure_exit(&at) else {
            return Ok(None);
        };
        if exit
            .interval()
            .return_loops()
            .is_none_or(|loops| !loops.iter().eq(self.loops.iter().map(|(site, _)| site)))
        {
            return Err(malformed(
                "return loop owners differ from its authored controls",
            ));
        }
        let slots = self
            .source
            .owner_slots()
            .map_err(|_| malformed("return source owners failed verification"))?;
        if let Some(carried) = &self.carried {
            let expected = slots.root(
                &room::Owner::Operand(carried.rhs.clone()),
                self.task,
                self.invocation.id(),
            );
            if expected != Some(carried.input) {
                return Err(malformed("return has another source operand"));
            }
            if ledger.holding(carried.input, heap.pages())?.is_none() {
                return Ok(None);
            }
        }
        let mut encoding = TokenWriter::new();
        encoding.seq(2);
        encoding.text("lash.source.compiled-departure");
        at.site.encode(&mut encoding);
        let start = Position {
            at: at.clone(),
            unpaid: Some(at),
            recovery: logical_digest(&encoding.into_bytes()),
            occurrence: Occurrence {
                task: self.task,
                invocation: self.invocation.id(),
                loops: self.loops,
                action: None,
            },
        };
        let Ok(next) = exit.next(
            self.source,
            &start,
            self.cursor.as_ref().map(|(point, _)| point),
        ) else {
            return Ok(None);
        };
        let dependencies = self
            .carried
            .as_ref()
            .and_then(|carried| carried.value.handle())
            .into_iter()
            .chain(self.cursor.and_then(|(_, handle)| handle))
            .collect();
        Ok(Some(ReturnRecipe {
            start,
            next,
            allowance: Allowance {
                occurrences: vec![],
                before: 0,
                units: 0,
                close: None,
                strict: false,
                depth: 0,
                safe_point: self.safe_point,
            },
            exit,
            input: self.carried.map(|carried| carried.input),
            dependencies,
            debt: None,
        }))
    }
}
