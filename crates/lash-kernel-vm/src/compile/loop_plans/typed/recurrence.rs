//! A verified loop with one scalar recurrence and ordered copies of it.
//! The test, checked update, fees and iteration admission still run each time;
//! the private instruction decoder and register rollback are unnecessary.

use super::*;

#[derive(Clone, Copy)]
struct Operation<T> {
    kernel: usize,
    a: Arg<T>,
    b: Arg<T>,
}

#[derive(Clone)]
pub(super) struct Recurrence<T> {
    counter: u8,
    test: Operation<T>,
    update: Operation<T>,
    copies: Box<[(u8, bool)]>,
    fixed: u64,
    pub(super) written: u32,
}

impl<T: Domain> Recurrence<T> {
    /// Only the verified, linear body is projected. Every other operand
    /// remains invariant and every write copies the counter before or after
    /// its sole update. A branch, another update or an aliasing invariant
    /// keeps the ordinary typed program.
    pub(super) fn derive(program: &Program<T>) -> Option<Self> {
        if !program.tested {
            return None;
        }
        let [
            Instruction::Compare { kernel, a, b },
            Instruction::Commit { units, .. },
            Instruction::Branch { otherwise },
            body @ ..,
            Instruction::Exit { ended: false },
            Instruction::Exit { ended: true },
        ] = &*program.code
        else {
            return None;
        };
        if *otherwise != program.code.len() - 1 {
            return None;
        }
        let test = Operation {
            kernel: *kernel,
            a: *a,
            b: *b,
        };
        let mut fixed = *units;
        let mut copies = Vec::new();
        let mut update = None;
        let mut at = 0;
        while at < body.len() {
            match &body[at..] {
                [
                    Instruction::Set {
                        value: Arg::Reg(from),
                        dst,
                    },
                    Instruction::Commit { units, .. },
                    ..,
                ] => {
                    copies.push((*from, *dst, update.is_some()));
                    fixed = fixed.checked_add(*units)?;
                    at += 2;
                }
                [
                    Instruction::Number {
                        kernel,
                        a,
                        b,
                        dst: temp,
                    },
                    Instruction::Set {
                        value: Arg::Temp(from),
                        dst,
                    },
                    Instruction::Commit { units, .. },
                    ..,
                ] if temp == from && update.is_none() => {
                    update = Some((
                        *dst,
                        Operation {
                            kernel: *kernel,
                            a: *a,
                            b: *b,
                        },
                    ));
                    fixed = fixed.checked_add(*units)?;
                    at += 3;
                }
                _ => return None,
            }
        }
        let (counter, update) = update?;
        let mut written = 1_u32 << counter;
        for &(from, dst, _) in &copies {
            if from != counter || dst == counter {
                return None;
            }
            written |= 1 << dst;
        }
        let reads_counter = |op: Operation<T>| {
            [op.a, op.b]
                .iter()
                .any(|arg| matches!(arg, Arg::Reg(reg) if *reg == counter))
        };
        if !reads_counter(test) || !reads_counter(update) {
            return None;
        }
        for arg in [test.a, test.b, update.a, update.b] {
            match arg {
                Arg::Const(_) => {}
                Arg::Reg(reg) if reg == counter || written & (1 << reg) == 0 => {}
                _ => return None,
            }
        }
        Some(Self {
            counter,
            test,
            update,
            fixed,
            written,
            copies: copies
                .into_iter()
                .map(|(_, dst, after)| (dst, after))
                .collect(),
        })
    }

    pub(super) fn run<const COUNT: bool>(
        &self,
        program: &Program<T>,
        state: &mut State<'_, T>,
        headroom: Headroom,
        #[cfg(any(test, feature = "perf-witness"))] census: &execution::Recorder,
    ) -> (u64, u64, Ran) {
        let args = [self.test.a, self.test.b, self.update.a, self.update.b];
        let fixed = args.map(|arg| arg.get(state));
        let varies = args.map(|arg| matches!(arg, Arg::Reg(reg) if reg == self.counter));
        let test = &program.kernels[self.test.kernel];
        let update = &program.kernels[self.update.kernel];
        let mut counter = state.regs[usize::from(self.counter)];
        let mut before = counter;
        let mut iterations = 0_u64;
        let mut units = 0_u64;
        let budget = headroom.budget();
        let ended = loop {
            let Some(_admission) = budget.as_ref().and_then(|budget| budget.admit(units)) else {
                break Ran::Done(0);
            };
            let read = |at: usize| if varies[at] { counter } else { fixed[at] };
            let (a, b) = (read(0), read(1));
            match T::truth(test.op, a, b) {
                Some(true) => {}
                Some(false) => break Ran::Ended,
                None => break Ran::Missed,
            }
            let (x, y) = (read(2), read(3));
            let Some(next) = T::number(update.op, x, y) else {
                break Ran::Missed;
            };
            state.units = self.fixed;
            state.charge(&test.table, a, b, 0);
            state.charge(&update.table, x, y, next.sized());
            units += state.units + headroom.loop_test;
            iterations += 1;
            before = counter;
            counter = next;
        };
        if iterations != 0 {
            state.regs[usize::from(self.counter)] = counter;
            for &(dst, after) in &*self.copies {
                state.regs[usize::from(dst)] = if after { counter } else { before };
            }
            #[cfg(any(test, feature = "perf-witness"))]
            if COUNT {
                execution::statements_here(
                    census,
                    (self.copies.len() as u64 + 1).saturating_mul(iterations),
                    true,
                );
            }
        }
        (iterations, units, ended)
    }
}
