//! Only active source loop counts survive; placement is rebuilt once at exit.
use super::super::*;
use super::{Running, scalar::ScalarAnswer};
use crate::compile::region_isa::{LoopPhase, Pc, RegionOp};
use crate::compile::{BlockId, Form};
use crate::machine::{Control, controls::Controls};

#[derive(Clone, Copy, Default)]
pub(super) struct LoopState {
    active: bool,
    started: u64,
}
#[derive(Clone, Copy)]
pub(super) struct Edit {
    index: usize,
    next: LoopState,
}

pub(super) fn bind(
    program: &RegionCode,
    canonical: &crate::machine::Frame,
) -> Result<Box<[LoopState]>, RegionError> {
    let mut states = Vec::new();
    while let Some(info) = program.loop_info(states.len() as u32) {
        let mut state = LoopState::default();
        if let Some(started) = canonical.control.iter().find_map(|control| match control {
            Control::While { stmt, started } | Control::For { stmt, started, .. }
                if *stmt == info.statement =>
            {
                Some(*started)
            }
            _ => None,
        }) {
            state = LoopState {
                active: true,
                started,
            };
        }
        states
            .try_reserve(1)
            .map_err(|_| invalid(Refused::Scratch))?;
        states.push(state);
    }
    Ok(states.into())
}

pub(super) fn branch(program: &RegionCode, pc: Pc) -> Option<u32> {
    if !matches!(program.op(pc), Some(RegionOp::Branch { .. })) {
        return None;
    }
    let step = program.step(pc)?;
    let mut index = 0;
    while let Some(info) = program.loop_info(index) {
        if info.while_loop
            && step.at.site == info.site
            && step.at.phase == crate::compile::room::Phase::Test
        {
            return Some(index);
        }
        index += 1;
    }
    None
}

impl Running<'_> {
    pub(super) fn prepare_control(
        &self,
        op: &RegionOp,
        answer: &ScalarAnswer,
        branch: Option<u32>,
    ) -> Result<Option<Edit>, RegionError> {
        let (index, next) = match op {
            RegionOp::Checked { kernel, .. } => match kernel.kind() {
                crate::compile::region_isa::CheckedKind::LoopControl { stage, .. } => {
                    let current = *self
                        .loops
                        .get(self.loop_index(stage.info)?)
                        .ok_or_else(|| invalid(Refused::Facts))?;
                    let next = match stage.phase {
                        LoopPhase::Enter => LoopState {
                            active: true,
                            started: 0,
                        },
                        LoopPhase::Test if current.active => current,
                        LoopPhase::Test => return Err(invalid(Refused::Facts)),
                    };
                    (stage.info, next)
                }
                _ => return Ok(None),
            },
            RegionOp::Branch { yes, .. } if branch.is_some() => {
                let index = branch.ok_or_else(|| invalid(Refused::Facts))?;
                let mut next = *self
                    .loops
                    .get(self.loop_index(index)?)
                    .filter(|state| state.active)
                    .ok_or_else(|| invalid(Refused::Facts))?;
                if answer.next == *yes {
                    next.started = next
                        .started
                        .checked_add(1)
                        .ok_or_else(|| invalid(Refused::Facts))?;
                } else {
                    next.active = false;
                }
                (index, next)
            }
            _ => return Ok(None),
        };
        Ok(Some(Edit {
            index: self.loop_index(index)?,
            next,
        }))
    }

    pub(super) fn accept_control(&mut self, edit: Option<Edit>) {
        if let Some(edit) = edit {
            self.loops[edit.index] = edit.next;
        }
    }

    pub(super) fn controls(&self) -> Result<Controls, RegionError> {
        self.controls_for(&self.code, self.pc, self.frame_index())
    }

    pub(super) fn controls_for(
        &self,
        code: &super::Code,
        pc: Pc,
        frame: usize,
    ) -> Result<Controls, RegionError> {
        let point = &code
            .program
            .step(pc)
            .ok_or_else(|| invalid(Refused::Path))?
            .at;
        let mut values = Vec::new();
        self.place(
            code,
            frame,
            code.exe.code(code.program.code()).body,
            point,
            &mut values,
        )?;
        Ok(values.into())
    }

    fn place(
        &self,
        code: &super::Code,
        frame: usize,
        block: BlockId,
        point: &Point,
        values: &mut Vec<Control>,
    ) -> Result<(), RegionError> {
        use crate::compile::room::Phase;
        let source = code.exe.block(block);
        if point.site == source.site {
            if point.phase != Phase::Enter {
                values.push(Control::Block {
                    block,
                    next: source.stmts.len(),
                });
            }
            return Ok(());
        }
        let (index, statement) = source
            .stmts
            .iter()
            .enumerate()
            .find_map(|(index, id)| {
                let statement = code.exe.stmt(*id);
                (point.site.unit == statement.site.unit
                    && point.site.path.starts_with(&statement.site.path))
                .then_some((index, statement))
            })
            .ok_or_else(|| invalid(Refused::Path))?;
        let nested = statement.form.blocks().into_iter().find(|child| {
            let site = &code.exe.block(*child).site;
            point.site.unit == site.unit && point.site.path.starts_with(&site.path)
        });
        let start = frame * self.programs.loops;
        let active = self.loops[start..start + self.programs.loops]
            .iter()
            .enumerate()
            .find_map(|(index, state)| {
                let info = code.program.loop_info(index as u32)?;
                (info.site == statement.site && state.active).then_some((info, state))
            });
        values.push(Control::Block {
            block,
            next: index + usize::from(nested.is_some() || active.is_some()),
        });
        match &statement.form {
            Form::While { body, .. } => {
                if let Some((info, state)) = active
                    && (nested.is_none() || nested == Some(*body))
                {
                    values.push(Control::While {
                        stmt: info.statement,
                        started: state.started,
                    });
                }
            }
            Form::For { .. } | Form::Try { .. } if nested.is_some() => {
                return Err(invalid(Refused::Facts));
            }
            _ => {}
        }
        if let Some(nested) = nested {
            self.place(code, frame, nested, point, values)?;
        }
        Ok(())
    }
}
