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
    let document = exe
        .instances
        .get(&record.link)
        .ok_or_else(|| malformed("frame Link is not loaded"))?;
    match &site.unit {
        Unit::Main if document.main.is_some() => {}
        Unit::Library(function)
            if document.functions.contains_key(function)
                || exe.admits_callable(&lash_kernel_value::CallableId {
                    link: record.link,
                    function: *function,
                }) => {}
        _ => return Err(malformed("frame code is not in its defining Link").into()),
    }
    let code_id = code_of(record, exe, access)?;
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
        && (record.convention.as_ref().is_some_and(|convention| {
            !matches!(convention, lash_kernel_doc::StartConvention::Direct)
        }) || exe.root_cleanup(code_id).is_some())
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
    let library = match (&code.site.unit, code.site.path.is_empty() && !code.charged) {
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
        let required = code
            .captures
            .iter()
            .any(|capture| capture.inner_at as usize == at);
        let borrowed = code.borrows_binding(at as u32);
        let bound = record.bindings.get(&info.key, access)?.map(|row| row.value);
        *value = match bound {
            None if !required => SlotState::Empty,
            Some(BoundRecord::Value(value)) if !borrowed && !info.shared => {
                SlotState::Value(admit(arena, access, &value)?)
            }
            Some(BoundRecord::Shared(id)) if !borrowed && info.shared => {
                SlotState::Cell(admit_cell(arena, id))
            }
            Some(BoundRecord::Captured(id)) if borrowed && info.shared => {
                SlotState::Cell(admit_cell(arena, id))
            }
            _ => {
                return Err(
                    malformed("binding capture ownership disagrees with its source").into(),
                );
            }
        };
    }
    Ok(Box::new(Frame {
        invocation: record.invocation,
        logical: None,
        link: record.link,
        convention: record.convention.clone(),
        public_result: record.public_result.clone(),
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
        control: control.into(),
        awaiting,
        nested,
        library,
        cleaning,
        held: Vec::new(),
        owned: true,
    }))
}

#[cfg(test)]
mod laws {
    use super::*;
    use crate::Step;
    use crate::laws::embedder::Embedder;
    use lash_kernel_doc::TaskId;

    /// K-BND-003 / K-EVAL-008: a stored borrowed capture must still be the
    /// source's cell owner. Ordinary values and missing captures cannot stand
    /// in for it and change either its price or its identity on restore.
    #[test]
    fn restored_capture_ownership_is_checked_against_the_source() {
        let definition = "function outer(held: Any) -> Any\nkernel 1\nmetering statements\nbody { let f = fn() { let w = perform 0(null) as Any return held } return apply f() }";
        let mut run = Embedder::new(&format!(
            "definition {}\nmain {{ let answer = call outer(7) return answer }}",
            serde_json::to_string(definition).unwrap(),
        ));
        assert!(matches!(run.run(), Step::Parked(_)));
        run.checkpoint();
        let machine = &mut run.machine;
        let access = machine.heap.pages().clone();
        let exe = Arc::clone(&machine.exe);
        let rows = machine.task(TaskId::MAIN).unwrap().frames.rows.clone();
        let original = (0..rows.len())
            .map(|at| row::<FrameRecord>(&rows, at, &access).unwrap())
            .find(|record| !record.capture_names.is_empty())
            .expect("the called closure has a captured variable");
        let mut captured = None;
        original
            .bindings
            .visit(&access, &mut |key, binding| {
                if let BoundRecord::Captured(id) = binding.value {
                    captured = Some((key.to_vec(), binding.clone(), id));
                }
                true
            })
            .unwrap();
        let (key, binding, id) = captured.expect("the source capture has a borrowed row");
        assert!(
            original.created.is_some(),
            "a lexical frame retains its creation site"
        );
        let statement = original.statement.resolve(&access).unwrap();
        let Unit::Library(function) = statement.unit else {
            panic!("the captured body is a lifted function")
        };
        let program = &machine.program;
        let selection = lash_kernel_doc::Selection::Restored(lash_kernel_doc::RestoreSelection {
            live: [lash_kernel_value::CallableId {
                link: original.link,
                function,
            }]
            .into(),
            pins: program
                .helpers()
                .contains(&function)
                .then_some(function)
                .into_iter()
                .collect(),
        });
        let admission = program
            .library
            .admit(
                &program.document,
                lash_kernel_value::data_limits::KernelDataLimits::standard(),
                &selection,
            )
            .unwrap_or_else(|_| panic!("the retained child is admitted"));
        let narrowed = admission
            .executable(
                &program.document,
                &program.library,
                &program.documents,
                &[function].into(),
                &program.instances,
            )
            .unwrap_or_else(|_| panic!("the retained child is compiled"));
        assert!(
            frame(
                &original,
                Arrival::Requested,
                &access,
                &narrowed,
                &mut Arena::new(),
            )
            .is_ok(),
            "the live child keeps its exact lexical source after selection narrows"
        );
        let created = original.created.as_ref().unwrap().resolve(&access).unwrap();
        let creator = Site::new(created.unit.clone(), Vec::new());
        let Unit::Library(creator_function) = creator.unit else {
            panic!("the lexical creator is an ordinary function")
        };
        assert!(
            !narrowed.declared.contains_key(&creator_function)
                && narrowed.code_at(&creator, &[]).is_none()
                && narrowed
                    .closure_at(&creator_function, original.link, &creator, &[])
                    .is_none(),
            "retaining the lexical source admitted its unselected creator"
        );
        let mut foreign = program.document.as_ref().clone();
        foreign.main = None;
        foreign.functions.remove(&creator_function);
        foreign.exports.clear();
        let foreign_id = foreign.identity();
        let foreign_link = lash_kernel_value::Link {
            document: foreign_id,
            fills: Default::default(),
            helper_fills: Default::default(),
        }
        .identity();
        let mut documents = program.documents.clone();
        documents.insert(foreign_id, Arc::new(foreign));
        let mut instances = program.instances.clone();
        let mut instance = instances[&original.link].clone();
        instance.link = foreign_link;
        instance.document = foreign_id;
        instances.insert(foreign_link, instance);
        let linked = admission
            .executable(
                &program.document,
                &program.library,
                &documents,
                &[function].into(),
                &instances,
            )
            .unwrap_or_else(|_| panic!("the retained body has two admitted Links"));
        let mut forged = original.clone();
        forged.link = foreign_link;
        assert!(
            frame(
                &forged,
                Arrival::Requested,
                &access,
                &linked,
                &mut Arena::new(),
            )
            .is_err(),
            "a lexical frame borrowed its creation site from another Link"
        );
        for created in [
            None,
            Some(SiteRef::new(&Site::new(Unit::Main, vec![999]), &access).unwrap()),
        ] {
            let mut forged = original.clone();
            forged.created = created;
            assert!(
                frame(
                    &forged,
                    Arrival::Requested,
                    &access,
                    &exe,
                    &mut Arena::new()
                )
                .is_err(),
                "a substituted lexical source was accepted"
            );
        }
        assert!(
            frame(
                &original,
                Arrival::Requested,
                &access,
                &exe,
                &mut Arena::new()
            )
            .is_ok()
        );
        for replacement in [
            Some(BoundRecord::Value(PageValue::Null)),
            Some(BoundRecord::Shared(id)),
            None,
        ] {
            let mut forged = original.clone();
            match replacement {
                Some(value) => {
                    let mut binding = binding.clone();
                    binding.value = value;
                    forged
                        .bindings
                        .write(key.clone(), binding, &access)
                        .unwrap();
                }
                None => {
                    forged.bindings.remove(&key, &access).unwrap();
                }
            }
            assert!(
                matches!(frame(&forged, Arrival::Requested, &access, &exe, &mut Arena::new()),
                    Err(Unrestored::Fault(HeapFault::Malformed(problem)))
                        if problem == "binding capture ownership disagrees with its source"),
                "a substituted capture was accepted"
            );
        }
    }
}
