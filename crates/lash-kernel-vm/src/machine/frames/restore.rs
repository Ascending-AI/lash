//! Admission of a saved frame under the current executable.
use super::*;

/// Decodes the saved `record` of a frame the task comes to by `arrival`
/// into a resident frame, admitting each of its coordinates.
pub(super) fn frame(
    record: &FrameRecord,
    arrival: Arrival,
    access: &Access<'static>,
    exe: &Arc<Executable>,
    arena: &mut Arena,
) -> Result<Box<Frame>, Unrestored> {
    let site = record.statement.resolve(access)?;
    let code_id = (0..=site.path.len())
        .rev()
        .find_map(|length| exe.code_at(&Site::new(site.unit.clone(), &site.path[..length])))
        .ok_or_else(|| malformed("a frame has no admitted code body"))?;
    let code = exe.code(code_id);
    let pending = match &record.pending {
        Some(pending) => Some(Pending {
            site: pending.site.as_slice().into(),
            phase: pending.phase,
            roles: admit_roles(arena, access, pending.roles(access)?)?,
        }),
        None => None,
    };
    let mut depth = code.site.path.len();
    let mut block = code.body;
    let mut control = Vec::new();
    let mut loops = 0;
    let mut finally = 0;
    let mut iterated = 0;
    let mut departing = 0;
    let mut awaiting = None;
    let mut nested = None;
    let nowhere = || malformed(format!("{} is no statement of the admitted document", site));
    // A policy call has already left every source scope. Rebuilding the
    // path to its return would incorrectly enter those scopes again.
    let adopting = exe.stmt_at(code_id, &site).filter(|stmt| {
        arrival == Arrival::Returned
            && pending.is_none()
            && matches!(exe.stmt(*stmt).form, Form::ReturnVia { .. })
    });
    if let Some(stmt) = adopting {
        awaiting = Some(stmt);
    }
    // A run-root cleanup has left every source scope: the frame stands at
    // its body, holding the departure the cleanup returns to.
    let cleaning = if *site == code.site
        && arrival == Arrival::Returned
        && pending.is_none()
        && exe.root_cleanup(code_id).is_some()
    {
        let saved = row(&record.finally, 0, access)?;
        let value = admit(arena, access, &row(&record.departing, 0, access)?)?;
        (finally, departing) = (1, 1);
        let (cleaned, departure) = match saved.entered {
            EnteredRecord::Return => (Ok(value), Departure::Return),
            EnteredRecord::Throw => (Err(value), Departure::Throw),
            _ => return Err(malformed("a root cleanup holds no return or raise").into()),
        };
        admit_finally(&saved, &code.site, departure, access)?;
        Some(cleaned)
    } else {
        None
    };
    while adopting.is_none() && cleaning.is_none() {
        let index = *site.path.get(depth).ok_or_else(nowhere)? as usize;
        let stmts = &exe.block(block).stmts;
        if depth + 1 == site.path.len() {
            let next = if let Some(pending) = &pending {
                let stmt = *stmts.get(index).ok_or_else(nowhere)?;
                nested = Some(continuation(exe, stmt, &site, pending, arrival)?);
                awaiting = Some(stmt);
                control.push(Control::Block {
                    block,
                    next: index + 1,
                });
                // A `while` waits in its condition: it stands above its loop.
                if let Form::While { site: at, .. } = &exe.stmt(stmt).form {
                    let saved = admitted_loop(record, loops, at, false, false, access)?;
                    loops += 1;
                    control.push(Control::While {
                        stmt,
                        started: saved.started,
                    });
                }
                break;
            } else if arrival != Arrival::Fresh {
                let stmt = *stmts.get(index).ok_or_else(nowhere)?;
                awaiting = Some(stmt);
                match &exe.stmt(stmt).form {
                    // Only a `then yield` wait owes a turn once delivered.
                    Form::Action(action)
                        if arrival == Arrival::Delivered && !action.kind.then_yield() =>
                    {
                        let action = site_path(&site);
                        return Err(RestoreRefusal::TaskState { action }.into());
                    }
                    Form::Action(_) => {}
                    // A `for` waits in a protocol hook of its header: it
                    // stands above its loop.
                    Form::For { site: at, .. } if arrival == Arrival::Returned => {
                        let saved = admitted_loop(record, loops, at, true, true, access)?;
                        loops += 1;
                        control.push(Control::Block {
                            block,
                            next: index + 1,
                        });
                        control.push(saved_for(
                            stmt,
                            &saved,
                            record,
                            &mut iterated,
                            access,
                            arena,
                        )?);
                        break;
                    }
                    _ => return Err(malformed("a resuming frame has no action").into()),
                }
                index + 1
            } else if index <= stmts.len() {
                index
            } else {
                return Err(nowhere().into());
            };
            control.push(Control::Block { block, next });
            break;
        }
        let stmt = *stmts.get(index).ok_or_else(nowhere)?;
        control.push(Control::Block {
            block,
            next: index + 1,
        });
        let inner = Site::new(site.unit.clone(), &site.path[..depth + 2]);
        let at = |candidate| exe.block(candidate).site == inner;
        block = match &exe.stmt(stmt).form {
            Form::If {
                then_block,
                else_block,
            } => [*then_block, *else_block]
                .into_iter()
                .find(|candidate| at(*candidate))
                .ok_or_else(nowhere)?,
            Form::For { site, body, .. } if at(*body) => {
                let saved = admitted_loop(record, loops, site, true, false, access)?;
                loops += 1;
                control.push(saved_for(
                    stmt,
                    &saved,
                    record,
                    &mut iterated,
                    access,
                    arena,
                )?);
                *body
            }
            // A loop's `else` runs once the loop has left the control.
            Form::For {
                else_block: Some(else_block),
                ..
            }
            | Form::While {
                else_block: Some(else_block),
                ..
            } if at(*else_block) => *else_block,
            Form::While { site, body, .. } if at(*body) => {
                let saved = admitted_loop(record, loops, site, false, false, access)?;
                loops += 1;
                control.push(Control::While {
                    stmt,
                    started: saved.started,
                });
                *body
            }
            Form::Try {
                site: try_site,
                body,
                catch,
                finally: cleanup,
            } => {
                let (phase, inner) = if at(*body) {
                    (TryPhase::Body, *body)
                } else if let Some((_, catch)) = catch.filter(|(_, catch)| at(*catch)) {
                    (TryPhase::Catch, catch)
                } else if let Some(cleanup) = cleanup.filter(|cleanup| at(*cleanup)) {
                    let saved = row(&record.finally, finally, access)?;
                    finally += 1;
                    let mut value = || {
                        let value = row(&record.departing, departing, access)?;
                        departing += 1;
                        admit(arena, access, &value)
                    };
                    let (completion, departure) = match &saved.entered {
                        EnteredRecord::Normal => (Completion::Normal, Departure::Normal),
                        EnteredRecord::Break => (Completion::Break, Departure::Break),
                        EnteredRecord::Continue => (Completion::Continue, Departure::Continue),
                        EnteredRecord::Throw => {
                            (Completion::Throw(Box::new(value()?)), Departure::Throw)
                        }
                        EnteredRecord::Return => {
                            (Completion::Return(Box::new(value()?)), Departure::Return)
                        }
                        EnteredRecord::ReturnVia(via) => {
                            let via = via.resolve(access)?;
                            let stmt = exe
                                .stmt_at(code_id, &via)
                                .filter(|stmt| {
                                    matches!(exe.stmt(*stmt).form, Form::ReturnVia { .. })
                                })
                                .ok_or_else(|| RestoreRefusal::UnknownSite {
                                    action: site_path(&via),
                                })?;
                            let completion = Completion::ReturnVia {
                                value: Box::new(value()?),
                                stmt,
                            };
                            (completion, Departure::ReturnVia(site_path(&via)))
                        }
                    };
                    admit_finally(&saved, try_site, departure, access)?;
                    (TryPhase::Finally(completion), cleanup)
                } else {
                    return Err(nowhere().into());
                };
                control.push(Control::Try { stmt, phase });
                inner
            }
            _ => return Err(nowhere().into()),
        };
        depth += 2;
    }
    if loops != record.loops.len()
        || finally != record.finally.len()
        || iterated != record.iterated.len()
        || departing != record.departing.len()
    {
        return Err(malformed("a frame has excess saved control rows").into());
    }
    let library = match (&code.site.unit, code.site.path.is_empty()) {
        (Unit::Library(function), true) => {
            let lib = exe
                .lib_of(function)
                .ok_or_else(|| malformed("a frame's library is not admitted"))?;
            let arity = u32::try_from(exe.lib(lib).arity)
                .map_err(|_| malformed("a library function's arity overflows"))?;
            coord::admit_arguments(retained(record), Some(arity))?;
            let mut args = Vec::new();
            if let Some(saved) = &record.arguments {
                for at in 0..saved.len() {
                    args.push(admit(arena, access, &row(saved, at, access)?)?);
                }
            }
            Some(LibraryCall {
                lib,
                args: args.into(),
            })
        }
        _ => {
            coord::admit_arguments(retained(record), None)?;
            None
        }
    };
    let mut slots = Slots::default();
    slots.fresh(code.slots_by_at.len());
    for (at, value) in slots.values.iter_mut().enumerate() {
        let info = &code.slots[code.slots_by_at[at] as usize];
        *value = match record.bindings.get(&info.key, access)? {
            None => SlotState::Empty,
            Some(BindingRecord {
                value: BoundRecord::Value(value),
                ..
            }) => SlotState::Value(admit(arena, access, &value)?),
            Some(BindingRecord {
                value: BoundRecord::Shared(id),
                ..
            }) => SlotState::Cell(admit_cell(arena, id)),
        };
    }
    Ok(Box::new(Frame {
        slots,
        code: code_id,
        charging: code.charging,
        paced: code.paced,
        statement: record.statement.clone(),
        pending: Continuation {
            saved: if pending.is_some() {
                Saved::This
            } else {
                Saved::Nothing
            },
            pending,
        },
        control,
        awaiting,
        nested,
        library,
        cleaning,
        held: Vec::new(),
        owned: true,
    }))
}
