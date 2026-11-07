//! Bind each reachable callee once: every static target, and each dynamic
//! target a run has reached or the lease's stack stands in. Recursive edges
//! retain code indexes.
use super::super::*;
use super::Code;
use crate::compile::room::region::calls::region_isa::CalleePlan;

pub(super) struct Programs {
    codes: Box<[Option<Arc<Code>>]>,
    calls: Box<[Option<super::call_bind::Calls>]>,
    pub(super) registers: usize,
    pub(super) arguments: usize,
    pub(super) transfer_width: usize,
    pub(super) quotes: usize,
    pub(super) loops: usize,
    pub(super) parameters: usize,
    pub(super) captures: usize,
    /// The session cells any bound code names, by lease-wide bank cell:
    /// the lease's one cell per digest.
    pub(super) sessions: Box<[Option<super::bind::SessionCell>]>,
}

impl Programs {
    /// `stack` holds the codes of the lease's frames, outermost first: a
    /// parked call enters the next frame's code.
    pub(super) fn bind(
        entry: &Arc<Code>,
        stack: &[CodeId],
        bound: &super::bind::BoundCodes,
    ) -> Result<Arc<Self>, RegionError> {
        let exe = &entry.exe;
        let mut codes = Vec::new();
        codes
            .try_reserve_exact(exe.code_count())
            .map_err(|_| invalid(Refused::Scratch))?;
        codes.resize_with(exe.code_count(), || None);
        let slot = codes
            .get_mut(entry.program.code().0 as usize)
            .ok_or_else(|| invalid(Refused::Source))?;
        *slot = Some(Arc::clone(entry));
        let mut pending = Vec::new();
        pending
            .try_reserve_exact(exe.code_count())
            .map_err(|_| invalid(Refused::Scratch))?;
        pending.push(Arc::clone(entry));
        let mut targets: Vec<Vec<Vec<Arc<CalleePlan>>>> = Vec::new();
        targets
            .try_reserve_exact(exe.code_count())
            .map_err(|_| invalid(Refused::Scratch))?;
        targets.resize_with(exe.code_count(), Vec::new);
        let mut result = Self {
            codes: Box::new([]),
            calls: Box::new([]),
            registers: 0,
            arguments: 0,
            transfer_width: 0,
            quotes: 0,
            loops: 0,
            parameters: 0,
            captures: 0,
            sessions: Box::new([]),
        };
        let mut sessions: Vec<Option<super::bind::SessionCell>> = Vec::new();
        while let Some(code) = pending.pop() {
            for (bank, cell) in &code.sessions {
                let bank = *bank as usize;
                if bank >= sessions.len() {
                    sessions
                        .try_reserve(bank + 1 - sessions.len())
                        .map_err(|_| invalid(Refused::Scratch))?;
                    sessions.resize(bank + 1, None);
                }
                match &sessions[bank] {
                    Some(known) if known.digest != cell.digest => {
                        return Err(invalid(Refused::Facts));
                    }
                    Some(_) => {}
                    None => sessions[bank] = Some(cell.clone()),
                }
            }
            result.registers = result.registers.max(code.program.registers().len());
            result.arguments = result.arguments.max(code.arguments);
            result.captures = result.captures.max(code.captures);
            result.transfer_width = result.transfer_width.max(code.transfer_width);
            result.quotes = result.quotes.max(code.quotes);
            let mut loops = 0;
            while code.program.loop_info(loops).is_some() {
                loops = loops
                    .checked_add(1)
                    .ok_or_else(|| invalid(Refused::Facts))?;
            }
            result.loops = result.loops.max(loops as usize);
            let caller = code.program.code();
            let mut sites = Vec::with_capacity(code.calls.len());
            for (site, call) in code.calls.iter().enumerate() {
                let Some(call) = call else {
                    sites.push(Vec::new());
                    continue;
                };
                let mut plans = Vec::new();
                if call.plan.dynamic() {
                    let site = u32::try_from(site).map_err(|_| invalid(Refused::Path))?;
                    let parked = stack
                        .windows(2)
                        .filter(|pair| pair[0] == caller)
                        .map(|pair| pair[1]);
                    let mut reached = exe.dynamic_targets(caller, site);
                    reached.extend(parked);
                    reached.sort_unstable();
                    reached.dedup();
                    plans.extend(
                        reached
                            .into_iter()
                            .filter_map(|target| call.plan.target(exe, target)),
                    );
                } else if let Some(plan) = call.plan.static_target() {
                    plans.push(Arc::clone(plan));
                }
                for plan in &plans {
                    let slot = codes
                        .get_mut(plan.code().0 as usize)
                        .ok_or_else(|| invalid(Refused::Source))?;
                    if slot.is_some() {
                        continue;
                    }
                    let program = exe.region(plan.code()).map_err(invalid)?;
                    let code = bound.get(exe, &program)?;
                    *slot = Some(Arc::clone(&code));
                    pending.push(code);
                }
                sites.push(plans);
            }
            *targets
                .get_mut(caller.0 as usize)
                .ok_or_else(|| invalid(Refused::Source))? = sites;
        }
        result.calls = codes
            .iter()
            .zip(&targets)
            .map(|(code, targets)| {
                code.as_ref()
                    .map(|code| super::call_bind::Calls::bind(code, targets, &codes))
                    .transpose()
            })
            .collect::<Result<_, RegionError>>()?;
        result.parameters = result
            .calls
            .iter()
            .flatten()
            .map(super::call_bind::Calls::width)
            .max()
            .unwrap_or(0);
        result.captures = result.captures.max(
            result
                .calls
                .iter()
                .flatten()
                .map(super::call_bind::Calls::captures)
                .max()
                .unwrap_or(0),
        );
        result.codes = codes.into();
        result.sessions = sessions.into();
        Ok(Arc::new(result))
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

    /// Whether the lease bound `target` at `caller`'s call `site`.
    pub(super) fn binds(&self, caller: CodeId, site: u32, target: CodeId) -> bool {
        self.calls
            .get(caller.0 as usize)
            .and_then(Option::as_ref)
            .is_some_and(|calls| calls.target(site, target).is_ok())
    }
}
