//! Cache each root's reachable callees for its exact parked stack edges.
//! Newly reached targets extend its binding without rebinding other callers.
use super::super::*;
use super::Code;
use crate::compile::room::region::calls::region_isa::CalleePlan;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub(super) struct Cache(BTreeMap<CodeId, Cached>);

struct Cached {
    root: Arc<Code>,
    reached: usize,
    parked: BTreeSet<(CodeId, CodeId)>,
    programs: Arc<Programs>,
}

impl Cached {
    fn matches(&self, parked: impl Iterator<Item = (CodeId, CodeId)> + Clone) -> bool {
        parked.clone().all(|pair| self.parked.contains(&pair))
            && self
                .parked
                .iter()
                .all(|known| parked.clone().any(|pair| pair == *known))
    }
}

impl Cache {
    pub(super) fn get(
        &mut self,
        entry: &Arc<Code>,
        parked: impl Iterator<Item = (CodeId, CodeId)> + Clone,
        bound: &super::bind::BoundCodes,
    ) -> Result<Arc<Programs>, RegionError> {
        let root = entry.program.code();
        let reached = entry.exe.dynamic_target_count().map_err(invalid)?;
        if let Some(cached) = self.0.get(&root)
            && Arc::ptr_eq(&cached.root, entry)
            && cached.reached == reached
            && cached.matches(parked.clone())
        {
            return Ok(Arc::clone(&cached.programs));
        }
        // A refused extension is never published as a complete binding.
        let mut cached = match self.0.remove(&root) {
            Some(cached) if Arc::ptr_eq(&cached.root, entry) && cached.matches(parked.clone()) => {
                cached
            }
            _ => Cached {
                root: Arc::clone(entry),
                reached: 0,
                parked: parked.collect(),
                programs: Arc::new(Programs::new(entry.exe.code_count())?),
            },
        };
        Arc::make_mut(&mut cached.programs).extend(entry, &cached.parked, bound)?;
        cached.reached = reached;
        let programs = Arc::clone(&cached.programs);
        self.0.insert(root, cached);
        Ok(programs)
    }
}

#[derive(Clone)]
pub(super) struct Programs {
    codes: Vec<Option<Arc<Code>>>,
    calls: Vec<Option<Arc<super::call_bind::Calls>>>,
    plans: Vec<Vec<Vec<Arc<CalleePlan>>>>,
    pub(super) arguments: usize,
    pub(super) transfer_width: usize,
    pub(super) quotes: usize,
    pub(super) loops: usize,
    pub(super) parameters: usize,
    pub(super) captures: usize,
    pub(super) sessions: Vec<Option<super::bind::SessionCell>>,
}

impl Programs {
    fn new(count: usize) -> Result<Self, RegionError> {
        fn table<T>(count: usize, empty: impl FnMut() -> T) -> Result<Vec<T>, RegionError> {
            let mut table = Vec::new();
            table
                .try_reserve_exact(count)
                .map_err(|_| invalid(Refused::Scratch))?;
            table.resize_with(count, empty);
            Ok(table)
        }
        Ok(Self {
            codes: table(count, || None)?,
            calls: table(count, || None)?,
            plans: table(count, Vec::new)?,
            arguments: 0,
            transfer_width: 0,
            quotes: 0,
            loops: 0,
            parameters: 0,
            captures: 0,
            sessions: Vec::new(),
        })
    }

    fn include(&mut self, code: Arc<Code>) -> Result<(), RegionError> {
        for (bank, cell) in &code.sessions {
            let bank = *bank as usize;
            if bank >= self.sessions.len() {
                self.sessions
                    .try_reserve(bank + 1 - self.sessions.len())
                    .map_err(|_| invalid(Refused::Scratch))?;
                self.sessions.resize(bank + 1, None);
            }
            match &self.sessions[bank] {
                Some(known) if known.digest != cell.digest => return Err(invalid(Refused::Facts)),
                Some(_) => {}
                None => self.sessions[bank] = Some(cell.clone()),
            }
        }
        self.arguments = self.arguments.max(code.arguments);
        self.captures = self.captures.max(code.captures);
        self.transfer_width = self.transfer_width.max(code.transfer_width);
        self.quotes = self.quotes.max(code.quotes);
        let mut loops = 0;
        while code.program.loop_info(loops).is_some() {
            loops = loops
                .checked_add(1)
                .ok_or_else(|| invalid(Refused::Facts))?;
        }
        self.loops = self.loops.max(loops as usize);
        let index = code.program.code().0 as usize;
        self.plans
            .get_mut(index)
            .ok_or_else(|| invalid(Refused::Source))?
            .resize_with(code.calls.len(), Vec::new);
        *self
            .codes
            .get_mut(index)
            .ok_or_else(|| invalid(Refused::Source))? = Some(code);
        Ok(())
    }

    fn extend(
        &mut self,
        entry: &Arc<Code>,
        parked: &BTreeSet<(CodeId, CodeId)>,
        bound: &super::bind::BoundCodes,
    ) -> Result<(), RegionError> {
        let exe = &entry.exe;
        if self
            .codes
            .get(entry.program.code().0 as usize)
            .is_none_or(Option::is_none)
        {
            self.include(Arc::clone(entry))?;
        }
        let mut pending = Vec::new();
        pending
            .try_reserve_exact(exe.code_count())
            .map_err(|_| invalid(Refused::Scratch))?;
        pending.extend(self.codes.iter().flatten().cloned());
        let mut changed = BTreeSet::new();
        while let Some(code) = pending.pop() {
            let caller = code.program.code();
            let index = caller.0 as usize;
            if self.calls[index].is_none() {
                changed.insert(index);
            }
            for (site, call) in code.calls.iter().enumerate() {
                let Some(call) = call else { continue };
                let mut plans = Vec::new();
                if call.plan.dynamic() {
                    let site_id = u32::try_from(site).map_err(|_| invalid(Refused::Path))?;
                    let mut reached = exe.dynamic_targets(caller, site_id);
                    reached.extend(
                        parked
                            .range((caller, CodeId(0))..=(caller, CodeId(u32::MAX)))
                            .map(|&(_, target)| target),
                    );
                    reached.sort_unstable();
                    reached.dedup();
                    plans.extend(
                        reached
                            .into_iter()
                            .filter(|target| {
                                !self.plans[index][site]
                                    .iter()
                                    .any(|plan| plan.code() == *target)
                            })
                            .filter_map(|target| call.plan.target(exe, target)),
                    );
                } else if self.plans[index][site].is_empty()
                    && let Some(plan) = call.plan.static_target()
                {
                    plans.push(Arc::clone(plan));
                }
                for plan in plans {
                    let target = self
                        .codes
                        .get(plan.code().0 as usize)
                        .ok_or_else(|| invalid(Refused::Source))?;
                    if target.is_none() {
                        let program = exe.region(plan.code()).map_err(invalid)?;
                        let callee = bound.get(exe, &program)?;
                        self.include(Arc::clone(&callee))?;
                        pending.push(callee);
                    }
                    self.plans[index][site].push(plan);
                    changed.insert(index);
                }
            }
        }
        for index in changed {
            let code = self.codes[index]
                .as_ref()
                .ok_or_else(|| invalid(Refused::Source))?;
            let calls = super::call_bind::Calls::bind(code, &self.plans[index], &self.codes)?;
            self.parameters = self.parameters.max(calls.width());
            self.captures = self.captures.max(calls.captures());
            self.calls[index] = Some(Arc::new(calls));
        }
        Ok(())
    }

    pub(super) fn code(&self, id: CodeId) -> Result<&Arc<Code>, RegionError> {
        self.codes
            .get(id.0 as usize)
            .and_then(Option::as_ref)
            .ok_or_else(|| invalid(Refused::Source))
    }

    pub(super) fn target(
        &self,
        caller: CodeId,
        site: u32,
        target: CodeId,
    ) -> Result<&super::call_bind::Target, RegionError> {
        self.calls
            .get(caller.0 as usize)
            .and_then(Option::as_ref)
            .ok_or_else(|| invalid(Refused::Source))?
            .target(site, target)
    }

    pub(super) fn binds(&self, caller: CodeId, site: u32, target: CodeId) -> bool {
        self.calls
            .get(caller.0 as usize)
            .and_then(Option::as_ref)
            .is_some_and(|calls| calls.target(site, target).is_ok())
    }
}
