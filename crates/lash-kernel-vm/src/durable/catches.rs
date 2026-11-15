//! Recovery observations retain error roots and document-keyed catch scopes.

use std::collections::HashSet;

use lash_kernel_doc::Value;
use lash_kernel_state::paged::run::continuation::{RunCatch, RunFailure, RunRecord};

use super::{Values, fault, number, origin};
use crate::WaitId;
use crate::bytecode::op::Op;
use crate::heap::slots::SlotHeap;
use crate::paged::SealError;
use crate::vm::Vm;
use crate::vm::catches::{Failure, Scope};
use crate::vm::frames::Context;

fn context(vm: &Vm<SlotHeap>, task: usize) -> Option<&Context> {
    if vm.tasks.installed && task == vm.tasks.running {
        Some(&vm.cx)
    } else {
        vm.tasks.all.get(task).map(|task| &task.cx)
    }
}

pub(super) fn save(vm: &Vm<SlotHeap>, values: &mut Values) -> Result<Vec<RunFailure>, SealError> {
    vm.catches
        .failures
        .iter()
        .map(|failure| {
            let catch = failure
                .catch
                .as_ref()
                .map(|scope| -> Result<_, SealError> {
                    let frame = context(vm, failure.task)
                        .and_then(|cx| cx.frames.get(scope.frame as usize))
                        .ok_or_else(|| fault("a recovery catch names no frame"))?;
                    let unit = vm.program.unit(frame.unit);
                    let at = scope.at as usize;
                    let site = origin(unit, at);
                    let copy = (0..at)
                        .filter(|&before| {
                            matches!(unit.ops[before], Op::CatchEnter(_))
                                && origin(unit, before) == site
                        })
                        .count();
                    Ok(RunCatch {
                        frame: scope.frame,
                        site: site.to_vec(),
                        copy: number(copy)?,
                    })
                })
                .transpose()?;
            Ok(RunFailure {
                task: number(failure.task)?,
                wait: failure.wait.0,
                error: values.put(failure.error),
                catch,
            })
        })
        .collect()
}

pub(super) fn open(
    vm: &mut Vm<SlotHeap>,
    record: &RunRecord,
    values: &[Value],
) -> Result<(), String> {
    let mut seen: HashSet<_> = record.waits.iter().map(|wait| wait.number).collect();
    for &wait in &record.caught {
        if wait >= record.next_wait || !seen.insert(wait) {
            return Err("a recovered wait is invalid or repeated".into());
        }
    }
    let failures = record
        .failures
        .iter()
        .map(|failure| {
            if failure.wait >= record.next_wait || !seen.insert(failure.wait) {
                return Err("a followed failure's wait is invalid or repeated".into());
            }
            let task = failure.task as usize;
            let cx = context(vm, task).ok_or("a followed failure names no task")?;
            let error = *values
                .get(failure.error as usize)
                .ok_or("a followed failure names no error root")?;
            if !matches!(error, Value::Error(_)) {
                return Err("a followed failure's root is not an error".into());
            }
            let catch = failure
                .catch
                .as_ref()
                .map(|saved| -> Result<_, String> {
                    let frame = cx
                        .frames
                        .get(saved.frame as usize)
                        .ok_or("a recovery catch names no frame")?;
                    let unit = vm.program.unit(frame.unit);
                    let (at, end) = unit
                        .ops
                        .iter()
                        .enumerate()
                        .filter_map(|(at, op)| match op {
                            Op::CatchEnter(end) if origin(unit, at) == saved.site.as_slice() => {
                                Some((at as u32, *end))
                            }
                            _ => None,
                        })
                        .nth(saved.copy as usize)
                        .ok_or("a recovery catch names no catch scope")?;
                    if frame.pc <= at || frame.pc >= end {
                        return Err("a recovery catch's frame is outside its scope".to_owned());
                    }
                    Ok(Scope {
                        frame: saved.frame,
                        at,
                        end,
                    })
                })
                .transpose()?;
            Ok(Failure {
                task,
                wait: WaitId(failure.wait),
                error,
                catch,
            })
        })
        .collect::<Result<_, String>>()?;
    vm.catches.failures = failures;
    vm.catches.caught = record.caught.iter().copied().map(WaitId).collect();
    Ok(())
}
