//! Independent semantic and event verification of the unboxed representation.

use super::*;

/// Checks the private program against the already verified semantic summaries.
/// The cursor consumes every instruction once, including both branch arms;
/// operands must name initialized registers or earlier statement temporaries.
struct Verify<'a, T> {
    program: &'a Program<T>,
    exe: &'a Executable,
    pc: usize,
    initialized: u32,
    temps: usize,
    formulas: Formulas,
}

impl<T: Domain> Program<T> {
    pub(super) fn verify(
        &self,
        exe: &Executable,
        test: Option<&Summary>,
        body: &Body,
        from: usize,
        covers: usize,
    ) -> bool {
        let locals = self
            .slots
            .iter()
            .take(REGISTERS)
            .enumerate()
            .filter(|(_, at)| body.declares.contains(at))
            .fold(0, |mask, (reg, _)| mask | (1 << reg));
        if self.slots.len() > REGISTERS
            || self.locals != locals
            || self.tested != test.is_some()
            || self
                .slots
                .iter()
                .enumerate()
                .any(|(i, slot)| self.slots[..i].contains(slot))
        {
            return false;
        }
        let mut initialized = 0;
        for &(slot, reg) in &*self.loads {
            if self.slots.get(usize::from(reg)) != Some(&slot) || initialized & (1 << reg) != 0 {
                return false;
            }
            initialized |= 1 << reg;
        }
        if self
            .held
            .iter()
            .any(|slot| !self.slots.contains(slot) || self.loads.iter().any(|(at, _)| at == slot))
        {
            return false;
        }
        let Some(steps) = body.steps.get(from..).and_then(|steps| steps.get(..covers)) else {
            return false;
        };
        let mut check = Verify {
            program: self,
            exe,
            pc: 0,
            initialized,
            temps: 0,
            formulas: Formulas::default(),
        };
        let valid = (|| {
            let branch = if let Some(test) = test {
                check.test(test)?;
                Some(check.branch()?)
            } else {
                None
            };
            for step in steps {
                check.step(step)?;
            }
            check.exit(false)?;
            if let Some(to) = branch {
                if to != check.pc {
                    return None;
                }
                check.exit(true)?;
            }
            (check.pc == self.code.len()).then_some(())
        })();
        valid.is_some()
    }
}

impl<T: Domain> Verify<'_, T> {
    fn next(&mut self) -> Option<&Instruction<T>> {
        let instruction = self.program.code.get(self.pc)?;
        self.pc += 1;
        Some(instruction)
    }

    fn same(&self, actual: Arg<T>, expected: Arg<T>) -> Option<()> {
        let valid = match (actual, expected) {
            (Arg::Reg(a), Arg::Reg(b)) => {
                a == b
                    && usize::from(a) < self.program.slots.len()
                    && self.initialized & (1 << a) != 0
            }
            (Arg::Const(a), Arg::Const(b)) => a.same(b),
            (Arg::Temp(a), Arg::Temp(b)) => a == b && a < self.temps,
            _ => false,
        };
        valid.then_some(())
    }

    fn kernel(&mut self, index: usize, lib: LibId, checked: bool, comparison: bool) -> Option<()> {
        let actual = self.program.kernels.get(index)?;
        let (op, table) =
            Builder::<T>::new(self.exe, &[], self.program.charged).kernel(lib, checked)?;
        if actual.op != op || actual.table != table || op.compares() != comparison {
            return None;
        }
        self.formulas.count += 1;
        self.formulas.units = self.formulas.units.checked_add(table[0])?;
        Some(())
    }

    fn operand(&mut self, expr: &Expr) -> Option<Arg<T>> {
        match &expr.kind {
            Kind::Local {
                local,
                scoped: false,
                ..
            } => Some(Arg::Reg(
                u8::try_from(self.program.slots.iter().position(|at| *at == local.at)?).ok()?,
            )),
            Kind::Const(value) => Some(Arg::Const(T::of(value)?)),
            Kind::Operator {
                lib, args, invoke, ..
            } if args.len() == 1 => {
                let a = self.operand(&args[0])?;
                let Instruction::Unary {
                    kernel,
                    a: actual,
                    dst,
                } = *self.next()?
                else {
                    return None;
                };
                self.same(actual, a)?;
                let expected = Builder::<T>::new(self.exe, &[], self.program.charged)
                    .unary(*lib, invoke.is_some_and(|invoke| invoke.checked))?;
                let actual = self.program.unaries.get(kernel)?;
                if actual.lib != expected.lib
                    || actual.table != expected.table
                    || dst != self.temps
                    || dst >= self.program.temps
                {
                    return None;
                }
                self.formulas.count += 1;
                self.formulas.units = self.formulas.units.checked_add(expected.table[0])?;
                self.temps += 1;
                Some(Arg::Temp(dst))
            }
            Kind::Operator {
                lib, args, invoke, ..
            } => {
                let [a, b] = &**args else {
                    return None;
                };
                let a = self.operand(a)?;
                let b = self.operand(b)?;
                let Instruction::Number {
                    kernel,
                    a: x,
                    b: y,
                    dst,
                } = *self.next()?
                else {
                    return None;
                };
                self.same(x, a)?;
                self.same(y, b)?;
                self.kernel(
                    kernel,
                    *lib,
                    invoke.is_some_and(|invoke| invoke.checked),
                    false,
                )?;
                if dst != self.temps || dst >= self.program.temps {
                    return None;
                }
                self.temps += 1;
                Some(Arg::Temp(dst))
            }
            _ => None,
        }
    }

    fn commit(&mut self, summary: &Summary) -> Option<()> {
        let builder = Builder::<T>::new(self.exe, &[], self.program.charged);
        let expected = program(summary)?;
        let units = builder.units(&expected, &self.formulas)?;
        let formulas = self.formulas.count;
        let Instruction::Commit {
            at,
            units: actual,
            formulas: count,
            program,
        } = self.next()?
        else {
            return None;
        };
        if *at != summary.ops.start
            || *actual != units
            || *count != formulas
            || **program != *expected
        {
            return None;
        }
        self.formulas = Formulas::default();
        self.temps = 0;
        Some(())
    }

    fn test(&mut self, summary: &Summary) -> Option<()> {
        let Kind::Operator {
            lib, args, invoke, ..
        } = &root(summary)?.kind
        else {
            return None;
        };
        let [a, b] = &**args else {
            return None;
        };
        let a = self.operand(a)?;
        let b = self.operand(b)?;
        let Instruction::Compare { kernel, a: x, b: y } = *self.next()? else {
            return None;
        };
        self.same(x, a)?;
        self.same(y, b)?;
        self.kernel(
            kernel,
            *lib,
            invoke.is_some_and(|invoke| invoke.checked),
            true,
        )?;
        self.commit(summary)
    }

    fn branch(&mut self) -> Option<usize> {
        let Instruction::Branch { otherwise } = *self.next()? else {
            return None;
        };
        (otherwise > self.pc - 1 && otherwise < self.program.code.len()).then_some(otherwise)
    }

    fn exit(&mut self, expected: bool) -> Option<()> {
        let Instruction::Exit { ended } = self.next()? else {
            return None;
        };
        (*ended == expected).then_some(())
    }

    fn step(&mut self, step: &Step) -> Option<()> {
        match step {
            Step::Write(summary) | Step::Bind(summary) => {
                let value = self.operand(root(summary)?)?;
                let Instruction::Set { value: actual, dst } = *self.next()? else {
                    return None;
                };
                self.same(actual, value)?;
                let Some(RegionSlot::Frame(at)) = summary.write else {
                    return None;
                };
                if self.program.slots.get(usize::from(dst)) != Some(&at) {
                    return None;
                }
                if matches!(step, Step::Write(_))
                    && !self.program.loads.iter().any(|(slot, _)| *slot == at)
                    && !self.program.held.contains(&at)
                {
                    return None;
                }
                self.initialized |= 1 << dst;
                self.commit(summary)
            }
            Step::If {
                test,
                then,
                otherwise,
            } if then.declares.is_empty() && otherwise.declares.is_empty() => {
                self.test(test)?;
                let target = self.branch()?;
                let before = self.initialized;
                for step in &then.steps {
                    self.step(step)?;
                }
                let assigned = self.initialized;
                let Instruction::Jump { to } = *self.next()? else {
                    return None;
                };
                if target != self.pc || to < self.pc || to >= self.program.code.len() {
                    return None;
                }
                self.initialized = before;
                for step in &otherwise.steps {
                    self.step(step)?;
                }
                self.initialized &= assigned;
                (to == self.pc).then_some(())
            }
            _ => None,
        }
    }
}

#[cfg(test)]
impl crate::compile::LoopPlan {
    /// Semantic adversaries for the private verifier, called by the body-tree law.
    pub(crate) fn assert_flat_verified(&self, exe: &Executable) {
        let crate::compile::Head::While(test) = &self.head else {
            panic!("a while plan")
        };
        let island = exe
            .islands
            .plans
            .iter()
            .find(|plan| plan.body.owns_loop(self.stmt))
            .expect("loop island");
        let mut registers = crate::machine::islands::compiled::source_registers(island, exe)
            .expect("register program");
        let (program, covers) = Builder::<i64>::new(exe, &registers, true)
            .program(Some(test), &self.body, 0)
            .expect("the scalar plan forms a typed program");
        assert_eq!(covers, self.body.steps.len());
        let verify =
            |program: &Program<i64>| program.verify(exe, Some(test), &self.body, 0, covers);
        assert!(verify(&program));
        let mut bad = program.clone();
        bad.locals ^= 1;
        assert!(!verify(&bad), "local ownership must match the block");
        for at in 0..program.code.len() {
            let mut bad = program.clone();
            match &mut bad.code[at] {
                Instruction::Assigned(_) | Instruction::Test(_) => {
                    unreachable!("verification precedes fusion")
                }
                Instruction::Unary { a, .. } | Instruction::Number { a, .. } => {
                    *a = Arg::Temp(program.temps)
                }
                Instruction::Compare { b, .. } => *b = Arg::Reg(REGISTERS as u8),
                Instruction::Set { dst, .. } => *dst = REGISTERS as u8,
                Instruction::Branch { otherwise } => *otherwise = at,
                Instruction::Jump { to } => *to = program.code.len(),
                Instruction::Commit { at, .. } => *at = at.saturating_add(1),
                Instruction::Exit { ended } => *ended = !*ended,
            }
            assert!(!verify(&bad), "malformed instruction {at}");
        }
        let mut bad = program.clone();
        bad.loads = Box::default();
        assert!(!verify(&bad), "reads must be initialized");
        let mut bad = program.clone();
        let commit = bad
            .code
            .iter_mut()
            .find_map(|op| match op {
                Instruction::Commit { program, .. } if program.len() > 1 => Some(program),
                _ => None,
            })
            .unwrap();
        commit.swap(0, commit.len() - 1);
        assert!(
            !verify(&bad),
            "event order must match the lowered statement"
        );
        // REGISTER-SOURCE: withdrawing the physical operator's numeric fact
        // must withdraw its typed representation, even with an unchanged summary.
        let site = registers
            .iter_mut()
            .find_map(|instruction| match &mut instruction.op {
                RegisterOp::DirectOperator { site, .. } | RegisterOp::Operator { site, .. } => {
                    Some(site)
                }
                _ => None,
            })
            .expect("numeric register site");
        site.pair = None;
        assert!(
            compile(exe, &registers, true, Some(test), &self.body, 0).is_none(),
            "changed register facts withdraw typed repeats"
        );
        assert!(!program.verify(exe, Some(test), &self.body, 0, covers - 1));
    }
}
