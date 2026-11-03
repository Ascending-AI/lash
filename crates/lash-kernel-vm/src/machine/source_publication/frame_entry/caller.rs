//! Caller authority names evaluated arguments and the live continuation roles.
use super::*;
use crate::compile::Nested;
use crate::machine::frames::{Pending, Role};
use lash_kernel_exec::{ActIx, Reg};
use lash_kernel_state::paged::PhaseRecord;

pub(in crate::machine) struct CallerArguments {
    pub(in crate::machine) args: usize,
    pub(in crate::machine) owners: ArgumentOwners,
    pub(in crate::machine) given: usize,
    pub(in crate::machine) awaited_by: Option<crate::compile::StmtId>,
    pub(in crate::machine) capture: Option<ActIx>,
}
impl From<&EntryCall> for CallerArguments {
    fn from(call: &EntryCall) -> Self {
        Self {
            args: call.args,
            owners: call.owners,
            given: call.given,
            awaited_by: call.awaited_by,
            capture: call.capture,
        }
    }
}

pub(super) struct Caller<'a> {
    pub(super) callee: Option<Reg>,
    pub(super) capture: Option<(ActIx, &'a Nested)>,
    expected: Option<Vec<Owner>>,
}
impl Caller<'_> {
    pub(super) fn roles(&self) -> &[crate::compile::NestedRole] {
        self.capture.map_or(&[], |(_, entry)| entry.roles.as_ref())
    }
    pub(super) fn authenticate(
        &self,
        bank: &crate::machine::registers::PreparedEntryBank<'_>,
    ) -> Result<(), HeapFault> {
        if let Some(expected) = &self.expected {
            let mut actual = buffer(bank.call_owners().count())?;
            actual.extend(bank.call_owners());
            actual.sort_unstable();
            if &actual != expected || actual.windows(2).any(|pair| pair[0] == pair[1]) {
                return Err(malformed(
                    "frame entry names other evaluated source arguments",
                ));
            }
        }
        Ok(())
    }
    pub(super) fn pending(
        &self,
        bank: &crate::machine::registers::PreparedEntryBank<'_>,
        ordinal: Option<u64>,
    ) -> Result<Option<(ActIx, Pending)>, HeapFault> {
        let Some((act, entry)) = self.capture else {
            return Ok(None);
        };
        let mut roles = buffer(entry.roles.len())?;
        for (site, value, holding, _, _) in bank.saved() {
            roles.push(Role {
                site: Arc::clone(site),
                value,
                holding: holding.clone(),
            });
        }
        Ok(Some((
            act,
            Pending {
                site: Arc::clone(&entry.suffix),
                phase: PhaseRecord::Called,
                occurrence: ordinal,
                roles,
            },
        )))
    }
}
impl KernelMachine {
    pub(in crate::machine) fn prepare_export_arguments(
        &mut self,
        task: TaskId,
        exe: &Executable,
        call: CallerArguments,
    ) -> Eval<()> {
        let mut encoding = TokenWriter::new();
        let Some((caller, _)) = self.prepare_frame_caller(task, exe, &call, &mut encoding)? else {
            return Err(malformed("export has no authenticated public source call").into());
        };
        let control = matches!(call.owners, ArgumentOwners::Control);
        let Some(bank) = self.regs.prepare_frame_entry(
            call.args,
            call.given,
            caller.callee,
            control,
            caller.roles(),
        )?
        else {
            return Err(malformed("export has no evaluated public argument bank").into());
        };
        caller.authenticate(&bank)?;
        bank.stage_export(call.args, caller.callee, control)?;
        Ok(())
    }

    pub(super) fn prepare_frame_caller<'a>(
        &mut self,
        task_id: TaskId,
        exe: &'a Executable,
        call: &CallerArguments,
        encoding: &mut TokenWriter,
    ) -> Eval<Option<(Caller<'a>, Option<u64>)>> {
        let given = self.regs.public_entry_count(call.args, call.given)?;
        let mut result = Caller {
            callee: None,
            capture: None,
            expected: None,
        };
        let mut ordinal = None;
        let mut issued = None;
        if let Some(statement) = call.awaited_by {
            let caller = self.frame(task_id)?;
            encoding.unsigned(caller.invocation);
            if matches!(call.owners, ArgumentOwners::Control) {
                encoding.seq(0);
                return Ok(Some((result, None)));
            }
            if let Some(act) = call.capture.or(caller.nested) {
                let (stream, stmt, _) = exe.statement(statement);
                let entry = stream
                    .nested
                    .get(act.ix())
                    .ok_or_else(|| fault("entry caller has no nested source"))?;
                if call.capture.is_some()
                    && (caller.pending.peek().is_some() || caller.nested.is_some())
                {
                    return Err(malformed(
                        "prepared call capture would replace a live continuation",
                    )
                    .into());
                }
                // A library body's call counts no occurrence: its capture
                // is checked against the same source continuation and
                // carries no action ordinal.
                if !entry.guest && call.capture.is_some() {
                    if !captured(&exe.code(caller.code).source_room, entry, stmt)? {
                        return Ok(None);
                    }
                    result.capture = Some((act, entry));
                }
                let pending = if call.capture.is_none() {
                    let pending = caller
                        .pending
                        .peek()
                        .ok_or_else(|| fault("entry caller has no continuation"))?;
                    if pending.site != entry.suffix {
                        return Err(
                            malformed("entry caller names another source continuation").into()
                        );
                    }
                    Some(pending)
                } else {
                    None
                };
                if entry.guest && matches!(entry.kind, NestedKind::Call(_)) {
                    ordinal = if call.capture.is_some() {
                        caller
                            .source_action
                            .filter(|(index, _)| *index == entry.occurrence)
                            .map(|(_, ordinal)| ordinal)
                    } else {
                        pending.and_then(|pending| pending.occurrence)
                    };
                    let Some(actual) = ordinal else {
                        return Ok(None);
                    };
                    let Some(frontier) = &caller.source_frontier else {
                        return Ok(None);
                    };
                    let at = &frontier.position().at;
                    if at.site != entry.site || at.phase != Phase::Enter {
                        return Ok(None);
                    }
                    if frontier.position().occurrence.invocation != caller.invocation
                        || frontier.position().occurrence.action.as_ref()
                            != Some(&(at.clone(), actual))
                        || pending.is_some_and(|pending| pending.phase != PhaseRecord::Called)
                    {
                        return Err(
                            malformed("entry caller lost its accepted source action").into()
                        );
                    }
                    if matches!(entry.kind, NestedKind::Call(NestedCallee::Read)) {
                        result.callee = Some(entry.dst)
                    }
                    let source = &exe.code(caller.code).source_room;
                    let event = source
                        .compile_event(at, room::ExitKind::Action)
                        .map_err(|_| malformed("entry caller has no canonical Enter"))?;
                    let slots = source
                        .owner_slots()
                        .map_err(|_| malformed("entry caller owners failed verification"))?;
                    let mut expected = buffer(given + usize::from(result.callee.is_some()))?;
                    for selector in &event
                        .events()
                        .next()
                        .ok_or_else(|| malformed("entry event disappeared"))?
                        .inputs
                    {
                        expected.push(
                            slots
                                .root(selector, task_id, caller.invocation)
                                .ok_or_else(|| {
                                    malformed("entry caller input has no source owner")
                                })?,
                        );
                    }
                    if expected.len() != given + usize::from(result.callee.is_some())
                        || call.args != entry.dst.ix() + usize::from(result.callee.is_some())
                    {
                        return Err(malformed(
                            "entry arguments differ from the actual source call",
                        )
                        .into());
                    }
                    expected.sort_unstable();
                    result.expected = Some(expected);
                    if call.capture.is_some() {
                        if !captured(source, entry, stmt)? {
                            return Ok(None);
                        }
                        result.capture = Some((act, entry));
                    }
                    issued = Some((entry.occurrence, actual));
                    encoding.seq(2);
                    entry.site.encode(encoding);
                    encoding.unsigned(actual);
                } else {
                    encoding.seq(0)
                }
            } else {
                encoding.seq(0)
            }
        } else {
            encoding.unsigned(0);
            encoding.seq(0);
        }
        if let Some((index, ordinal)) = issued
            && self
                .task(task_id)?
                .counters
                .current(index)
                .is_none_or(|count| ordinal >= count)
        {
            return Err(malformed("frame entry action was not admitted by its task").into());
        }
        Ok(Some((result, ordinal)))
    }
}

/// Whether a call's capture names exactly its statement's source
/// continuation: the Restore event's roles, under the statement's site. A
/// source without that Restore event misses; another continuation is
/// malformed.
fn captured(
    source: &room::Source,
    entry: &Nested,
    stmt: &crate::compile::Stmt,
) -> Result<bool, HeapFault> {
    let Ok(restore) = source.compile_event(
        &Point {
            site: entry.site.clone(),
            phase: Phase::Restore,
        },
        room::ExitKind::Normal,
    ) else {
        return Ok(false);
    };
    let roles = restore
        .restored_roles()
        .ok_or_else(|| malformed("entry capture source roles are absent"))?;
    if !roles.iter().eq(entry.roles.iter().map(|role| &role.site))
        || !entry.site.path.starts_with(&stmt.site.path)
        || entry.suffix.as_ref() != &entry.site.path[stmt.site.path.len()..]
        || entry.roles.iter().any(|role| {
            role.site.unit != stmt.site.unit
                || !role.site.path.starts_with(&stmt.site.path)
                || role.suffix.as_ref() != &role.site.path[stmt.site.path.len()..]
        })
    {
        return Err(malformed(
            "entry capture differs from its source continuation",
        ));
    }
    Ok(true)
}
