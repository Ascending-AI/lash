//! Fuse already verified scalar steps before their repeated execution.

use super::*;

/// Private execution positions, rebuilt with the executable, never durable PCs.
/// All branches go forward. A comparison's result is consumed by its branch.
#[derive(Clone)]
pub(super) enum Instruction<T> {
    Assigned(Box<Assignment<T>>),
    Test(Box<Test<T>>),
    Unary {
        kernel: usize,
        a: Arg<T>,
        dst: usize,
    },
    Number {
        kernel: usize,
        a: Arg<T>,
        b: Arg<T>,
        dst: usize,
    },
    Compare {
        kernel: usize,
        a: Arg<T>,
        b: Arg<T>,
    },
    Set {
        value: Arg<T>,
        dst: u8,
    },
    Commit {
        at: u32,
        units: u64,
        formulas: usize,
        program: Box<[Event]>,
    },
    Branch {
        otherwise: usize,
    },
    Jump {
        to: usize,
    },
    Exit {
        ended: bool,
    },
}

#[derive(Clone)]
pub(super) struct Commit {
    pub(super) units: u64,
    pub(super) formulas: usize,
    pub(super) program: Box<[Event]>,
}

#[derive(Clone)]
pub(super) struct Assignment<T> {
    pub(super) kernel: usize,
    pub(super) a: Arg<T>,
    pub(super) b: Arg<T>,
    pub(super) dst: u8,
    pub(super) commit: Commit,
}

#[derive(Clone)]
pub(super) struct Test<T> {
    pub(super) kernel: usize,
    pub(super) a: Arg<T>,
    pub(super) b: Arg<T>,
    pub(super) otherwise: usize,
    pub(super) commit: Commit,
}

impl<T: Domain> Program<T> {
    /// The verifier checks the source stream first. A target inside a suffix
    /// keeps that suffix separate; every surviving target is then relocated.
    pub(super) fn fused(mut self) -> Self {
        self.recurrence = Recurrence::derive(&self).map(Box::new);
        let len = self.code.len();
        let mut targets = vec![false; len + 1];
        for op in &self.code {
            if let Instruction::Branch { otherwise: to } | Instruction::Jump { to } = op {
                targets[*to] = true;
            }
        }
        let mut source: Vec<_> = std::mem::take(&mut self.code)
            .into_vec()
            .into_iter()
            .map(Some)
            .collect();
        let mut code = Vec::with_capacity(len);
        let mut relocation = vec![0; len + 1];
        for at in 0..len {
            relocation[at] = code.len();
            let Some(op) = source[at].take() else {
                continue;
            };
            let op = match op {
                Instruction::Number { kernel, a, b, dst }
                    if at + 2 < len
                        && !targets[at + 1]
                        && !targets[at + 2]
                        && matches!(&source[at + 1], Some(Instruction::Set { value: Arg::Temp(from), .. }) if *from == dst)
                        && matches!(&source[at + 2], Some(Instruction::Commit { .. })) =>
                {
                    let Some(Instruction::Set { dst, .. }) = source[at + 1].take() else {
                        unreachable!()
                    };
                    let commit = take_commit(&mut source[at + 2]);
                    Instruction::Assigned(Box::new(Assignment {
                        kernel,
                        a,
                        b,
                        dst,
                        commit,
                    }))
                }
                Instruction::Compare { kernel, a, b }
                    if at + 2 < len
                        && !targets[at + 1]
                        && !targets[at + 2]
                        && matches!(&source[at + 1], Some(Instruction::Commit { .. }))
                        && matches!(&source[at + 2], Some(Instruction::Branch { .. })) =>
                {
                    let commit = take_commit(&mut source[at + 1]);
                    let Some(Instruction::Branch { otherwise }) = source[at + 2].take() else {
                        unreachable!()
                    };
                    Instruction::Test(Box::new(Test {
                        kernel,
                        a,
                        b,
                        otherwise,
                        commit,
                    }))
                }
                other => other,
            };
            code.push(op);
        }
        relocation[len] = code.len();
        for op in &mut code {
            let to = match op {
                Instruction::Branch { otherwise } => otherwise,
                Instruction::Jump { to } => to,
                Instruction::Test(test) => &mut test.otherwise,
                _ => continue,
            };
            *to = relocation[*to];
        }
        self.code = code.into();
        self
    }
}

fn take_commit<T>(op: &mut Option<Instruction<T>>) -> Commit {
    let Some(Instruction::Commit {
        units,
        formulas,
        program,
        ..
    }) = op.take()
    else {
        unreachable!()
    };
    Commit {
        units,
        formulas,
        program,
    }
}
